//! A persistent, content-addressed cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript just to extract a handful
//! of metadata fields. But transcripts only change by being appended to, so a file whose
//! `(mtime, size)` is unchanged since last time has unchanged metadata. This cache keys on exactly
//! that: a hit returns the stored [`SessionRef`]s without touching the file, so steady-state
//! discovery costs one `stat` per file instead of a full read+parse.
//!
//! It's process-global and shared by `cv` and the desktop app via the `scan_cache` table in
//! the shared catalog db (see [`crate::catalog`], which owns the DDL; the rows also feed the
//! freshness probe's append detection). Earlier releases kept a whole-loaded `discover.json`
//! instead — a second, redundant representation of the same facts that every command paid to parse;
//! [`load`] leaves a "safe to delete" note if one is still lying around. Reads are tolerant — a
//! missing or unreadable cache simply means everything is re-scanned (and the cache rebuilt).
//! Entries are reused only when the interpretation version and `(mtime, size)` match.
//! Metadata probes do not verify content rewritten with restored timestamps.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

struct Entry {
    mtime_ns: u64,
    size: u64,
    refs: Vec<SessionRef>,
    /// Scanned this run and not yet written back to sqlite.
    dirty: bool,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    any_dirty: AtomicBool,
}

/// Bump when discovery changes the interpretation of unchanged source bytes.
#[cfg(feature = "sqlite")]
pub(crate) const INTERPRETATION_VERSION: u32 = 2;
static CACHE: OnceLock<Cache> = OnceLock::new();
static INVALIDATE: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "sqlite")]
pub(crate) fn invalidate_interpretation() {
    INVALIDATE.store(true, Ordering::Release);
}

fn clear_invalidated(cache: &Cache) {
    if INVALIDATE.load(Ordering::Acquire) {
        let mut entries = cache.entries.write().unwrap();
        if INVALIDATE.swap(false, Ordering::AcqRel) {
            entries.clear();
            cache.any_dirty.store(false, Ordering::Release);
        }
    }
}

/// One-time nudge about the pre-0.9.12 JSON cache this module replaced.
fn note_legacy_json() {
    let Some(path) = dirs::cache_dir().map(|d| d.join("clustervision").join("discover.json")) else {
        return;
    };
    if path.exists() {
        eprintln!(
            "cv: note: stale discover.json no longer used (the scan cache lives in catalog.db now); safe to delete {}",
            path.display()
        );
    }
}

/// Bulk-load the persisted scan cache. Only ever runs on the discovery (slow) path — probe-only
/// commands never touch this module. Unparseable rows are simply dropped (re-scanned, re-written).
fn load() -> Cache {
    note_legacy_json();
    #[allow(unused_mut)]
    let mut entries: HashMap<PathBuf, Entry> = HashMap::new();
    #[cfg(feature = "sqlite")]
    if let Some(conn) = crate::catalog::open_db() {
        if let Ok(mut stmt) =
            conn.prepare("SELECT path, mtime_ns, size, refs FROM scan_cache WHERE interpretation_version=?1")
        {
            let rows = stmt.query_map([INTERPRETATION_VERSION], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            });
            if let Ok(rows) = rows {
                for (path, mtime_ns, size, refs) in rows.flatten() {
                    let Ok(refs) = serde_json::from_str::<Vec<SessionRef>>(&refs) else {
                        continue;
                    };
                    entries.insert(
                        PathBuf::from(path),
                        Entry {
                            mtime_ns: mtime_ns as u64,
                            size: size as u64,
                            refs,
                            dirty: false,
                        },
                    );
                }
            }
        }
    }
    Cache {
        entries: RwLock::new(entries),
        any_dirty: AtomicBool::new(false),
    }
}

/// `(mtime_ns, size)` for `path`, or `None` if it can't be stat'd (then callers just scan).
fn stat_key(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, m.len()))
}

/// Return cached refs for `path` when its `(mtime, size)` is unchanged; otherwise run `scan`, cache
/// any non-empty result, and return it. One file may map to several sessions (e.g. Gemini logs).
///
/// The stat happens **before** the scan, so a write racing the scan leaves a too-old `(mtime, size)`
/// in the cache — which misses next time and re-scans. The safe direction.
pub(crate) fn cached_scan_many<F>(path: &Path, scan: F) -> Vec<SessionRef>
where
    F: FnOnce() -> Vec<SessionRef>,
{
    let Some((mtime_ns, size)) = stat_key(path) else {
        return scan();
    };
    let cache = CACHE.get_or_init(load);
    clear_invalidated(cache);
    if let Some(e) = cache.entries.read().unwrap().get(path) {
        if e.mtime_ns == mtime_ns && e.size == size {
            return e.refs.clone();
        }
    }
    let refs = scan();
    // Only cache hits — empty results are cheap to recompute (a failed name/content check that never
    // read the file) and caching them would bloat the table with every non-transcript path.
    if !refs.is_empty() {
        cache.entries.write().unwrap().insert(
            path.to_path_buf(),
            Entry {
                mtime_ns,
                size,
                refs: refs.clone(),
                dirty: true,
            },
        );
        cache.any_dirty.store(true, Ordering::Relaxed);
    }
    refs
}

/// Single-result convenience over [`cached_scan_many`].
pub(crate) fn cached_scan<F>(path: &Path, scan: F) -> Option<SessionRef>
where
    F: FnOnce() -> Option<SessionRef>,
{
    cached_scan_many(path, || scan().into_iter().collect())
        .into_iter()
        .next()
}

/// Flush freshly-scanned entries to the catalog db in one transaction. With `prune`, also drop
/// rows whose files no longer exist so the table doesn't grow without bound — pass it after a
/// *full* [`crate::discover_all`] (the stat-per-row cost belongs on the slow path; per-harness
/// probe refreshes skip it). Best-effort: a failed flush just means a re-scan next run.
pub(crate) fn persist(prune: bool) {
    let Some(cache) = CACHE.get() else {
        return;
    };
    if !cache.any_dirty.swap(false, Ordering::Relaxed) && !prune {
        return;
    }
    #[cfg(feature = "sqlite")]
    {
        let Some(mut conn) = crate::catalog::open_db() else {
            return;
        };
        clear_invalidated(cache);
        let Ok(tx) = conn.transaction() else { return };
        let mut g = cache.entries.write().unwrap();
        {
            let Ok(mut stmt) =
                tx.prepare("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs,interpretation_version) VALUES(?1,?2,?3,?4,?5)")
            else {
                return;
            };
            for (path, e) in g.iter_mut().filter(|(_, e)| e.dirty) {
                let Ok(refs) = serde_json::to_string(&e.refs) else {
                    continue;
                };
                if stmt
                    .execute(rusqlite::params![
                        path.to_string_lossy(),
                        e.mtime_ns as i64,
                        e.size as i64,
                        refs,
                        INTERPRETATION_VERSION
                    ])
                    .is_ok()
                {
                    e.dirty = false;
                }
            }
        }
        if prune {
            let vanished: Vec<String> = tx
                .prepare("SELECT path FROM scan_cache")
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.flatten().filter(|p| !Path::new(p).exists()).collect())
                })
                .unwrap_or_default();
            if let Ok(mut del) = tx.prepare("DELETE FROM scan_cache WHERE path=?1") {
                for p in &vanished {
                    let _ = del.execute(rusqlite::params![p]);
                    g.remove(Path::new(p));
                }
            }
        }
        drop(g);
        let _ = tx.commit();
    }
}

/// [`crate::catalog::fill_count`]'s scan-cache half: set `message_count = n` on the cached ref for
/// `(path, harness, id)` where it is `0`, both in the persisted row (left at its recorded
/// `(mtime, size)`, so a changed file still misses and re-scans) and in this process's copy.
#[cfg(feature = "sqlite")]
pub(crate) fn fill_count(conn: &rusqlite::Connection, path: &Path, harness: crate::ir::Harness, id: &str, n: usize) {
    let fill = |refs: &mut [SessionRef]| {
        let mut hit = false;
        for r in refs
            .iter_mut()
            .filter(|r| r.harness == harness && r.id == id && r.message_count == 0)
        {
            r.message_count = n;
            hit = true;
        }
        hit
    };
    if let Some(cache) = CACHE.get() {
        clear_invalidated(cache);
        if let Some(e) = cache.entries.write().unwrap().get_mut(path) {
            fill(&mut e.refs);
        }
    }
    let key = path.to_string_lossy();
    let Ok(json) = conn.query_row(
        "SELECT refs FROM scan_cache WHERE path=?1 AND interpretation_version=?2",
        rusqlite::params![key, INTERPRETATION_VERSION],
        |row| row.get::<_, String>(0),
    ) else {
        return;
    };
    let Ok(mut refs) = serde_json::from_str::<Vec<SessionRef>>(&json) else {
        return;
    };
    if fill(&mut refs) {
        if let Ok(json) = serde_json::to_string(&refs) {
            let _ = conn.execute(
                "UPDATE scan_cache SET refs=?1 WHERE path=?2 AND interpretation_version=?3",
                rusqlite::params![json, key, INTERPRETATION_VERSION],
            );
        }
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[test]
    fn interpretation_upgrade_rejects_persisted_and_process_scan_rows() {
        let w = cv_testkit::World::in_process("scan-interpretation");
        let path = w.base.join("unchanged.jsonl");
        std::fs::write(&path, "unchanged").unwrap();
        let old = SessionRef {
            harness: crate::Harness::Codex,
            id: "old".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 4,
        };
        assert_eq!(cached_scan(&path, || Some(old.clone())).unwrap().id, "old");
        persist(false);
        let conn = crate::catalog::open_db().unwrap();
        conn.execute_batch(
            "UPDATE scan_cache SET interpretation_version=1;
            UPDATE catalog_meta SET value='1' WHERE key='discovery_version';
            INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync','1');
            INSERT OR REPLACE INTO watched(harness,path,mtime_ns) VALUES('gemini','old-watch',1);",
        )
        .unwrap();
        let conn = crate::catalog::open_db().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM watched", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(crate::catalog::last_full_sync().is_none());
        assert!(
            load().entries.read().unwrap().is_empty(),
            "old persisted interpretation ignored"
        );
        let current = SessionRef {
            id: "current".into(),
            message_count: 2,
            ..old
        };
        assert_eq!(
            cached_scan(&path, || Some(current)).unwrap().id,
            "current",
            "process entry invalidated despite unchanged bytes"
        );
        assert_eq!(
            cached_scan(&path, || panic!("current scan must stay cached"))
                .unwrap()
                .message_count,
            2
        );
        persist(false);
        assert_eq!(
            conn.query_row(
                "SELECT interpretation_version FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
            INTERPRETATION_VERSION
        );
        // Old binaries omit the added nullable field on replacement: their row remains stale.
        conn.execute(
            "INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs) VALUES(?1,1,1,'[]')",
            [path.to_string_lossy()],
        )
        .unwrap();
        assert!(load().entries.read().unwrap().is_empty());
    }
}
