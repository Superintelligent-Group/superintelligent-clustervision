//! A persistent cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript to extract metadata.
//! Cache hits require unchanged `(mtime, size)` and captured file identity, rejecting appends,
//! timestamp-preserving replacements and rewrites. A hit returns stored [`SessionRef`]s without
//! reading transcript content; full discovery still freshly enumerates each harness's inventory.
//!
//! It's process-global and shared by `cv` and the desktop app via the `scan_cache` table in
//! the shared catalog db (see [`crate::catalog`], which owns the DDL; the rows also feed the
//! freshness probe's append detection). Earlier releases kept a whole-loaded `discover.json`
//! instead — a second, redundant representation of the same facts that every command paid to parse;
//! [`load`] leaves a "safe to delete" note if one is still lying around. Reads are tolerant — a
//! missing or unreadable cache simply means everything is re-scanned (and the cache rebuilt).
//! Entries are reused only when interpretation, tuple, and captured file identity match.
//! Captured source identities also reject replacement or rewrites with restored timestamps.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

struct Entry {
    mtime_ns: u64,
    size: u64,
    refs: Vec<SessionRef>,
    reads: crate::source::ReadSet,
    /// Scanned this run and not yet written back to sqlite.
    dirty: bool,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    any_dirty: AtomicBool,
}

/// Bump when discovery changes the interpretation of unchanged source bytes.
#[cfg(feature = "sqlite")]
pub(crate) const INTERPRETATION_VERSION: u32 = 4;
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
        if let Ok(mut stmt) = conn
            .prepare("SELECT path, mtime_ns, size, refs, source_reads FROM scan_cache WHERE interpretation_version=?1")
        {
            let rows = stmt.query_map([INTERPRETATION_VERSION], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            });
            if let Ok(rows) = rows {
                for (path, mtime_ns, size, refs, reads) in rows.flatten() {
                    let Ok(refs) = serde_json::from_str::<Vec<SessionRef>>(&refs) else {
                        continue;
                    };
                    let Ok(reads) = serde_json::from_str(&reads) else {
                        continue;
                    };
                    entries.insert(
                        PathBuf::from(path),
                        Entry {
                            mtime_ns: mtime_ns as u64,
                            size: size as u64,
                            refs,
                            reads,
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
    let mut reads = crate::source::ReadSet::default();
    reads.record(path);
    let Some((mtime_ns, size)) = stat_key(path) else {
        return scan();
    };
    let cache = CACHE.get_or_init(load);
    clear_invalidated(cache);
    if let Some(e) = cache.entries.read().unwrap().get(path) {
        if e.mtime_ns == mtime_ns
            && e.size == size
            && e.reads == reads
            && reads.fresh(&mut crate::source::Probe::default())
        {
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
                reads,
                dirty: true,
            },
        );
        cache.any_dirty.store(true, Ordering::Relaxed);
    }
    refs
}

/// Original before-read metadata, never a fresh post-discovery stamp.
#[cfg(feature = "sqlite")]
pub(crate) fn captured_reads(path: &Path) -> Option<crate::source::ReadSet> {
    CACHE.get()?.entries.read().ok()?.get(path).map(|e| e.reads.clone())
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

fn persistence_cache(prune: bool) -> Option<&'static Cache> {
    let cache = CACHE.get()?;
    (cache.any_dirty.load(Ordering::Relaxed) || prune).then_some(cache)
}

/// Flush freshly-scanned entries to the catalog db in one transaction. With `prune`, also drop
/// rows whose files no longer exist so the table doesn't grow without bound — pass it after a
/// *full* [`crate::discover_all`] (the stat-per-row cost belongs on the slow path; per-harness
/// probe refreshes skip it). Best-effort: a failed flush just means a re-scan next run.
pub(crate) fn persist(prune: bool) {
    if persistence_cache(prune).is_none() {
        return;
    }
    #[cfg(feature = "sqlite")]
    if let Some(mut conn) = crate::catalog::open_db() {
        persist_in(&mut conn, prune);
    }
}

#[cfg(feature = "sqlite")]
pub(crate) fn persist_in(conn: &mut rusqlite::Connection, prune: bool) {
    let Some(cache) = persistence_cache(prune) else { return };
    clear_invalidated(cache);
    // Full discovery still checks every persisted path, including malformed cache rows
    // omitted by load. Pruning therefore keeps its existing completeness semantics.
    let mut prune_paths: std::collections::HashSet<String> = if prune {
        let Ok(mut stmt) = conn.prepare("SELECT path FROM scan_cache") else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return;
        };
        let Ok(paths) = rows.collect::<rusqlite::Result<_>>() else {
            return;
        };
        paths
    } else {
        Default::default()
    };
    let mut g = cache.entries.write().unwrap();
    if prune {
        // Match the previous flush-then-prune behavior for newly scanned files that
        // vanished before publication, checking each persisted/dirty path only once.
        prune_paths.extend(
            g.iter()
                .filter(|(_, e)| e.dirty)
                .map(|(path, _)| path.to_string_lossy().into_owned()),
        );
    }
    let vanished: Vec<_> = prune_paths.into_iter().filter(|p| !Path::new(p).exists()).collect();
    if vanished.is_empty() && !g.values().any(|entry| entry.dirty) {
        return;
    }
    let Ok(tx) = conn.transaction() else { return };
    let mut written = Vec::new();
    {
        let Ok(mut stmt) = tx.prepare("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs,interpretation_version,source_reads) VALUES(?1,?2,?3,?4,?5,?6)") else { return };
        for (path, e) in g.iter().filter(|(_, e)| e.dirty) {
            let Ok(refs) = serde_json::to_string(&e.refs) else {
                continue;
            };
            let Ok(reads) = serde_json::to_string(&e.reads) else {
                continue;
            };
            if stmt
                .execute(rusqlite::params![
                    path.to_string_lossy(),
                    e.mtime_ns as i64,
                    e.size as i64,
                    refs,
                    INTERPRETATION_VERSION,
                    reads
                ])
                .is_err()
            {
                return;
            }
            written.push(path.clone());
        }
    }
    for path in &vanished {
        if tx.execute("DELETE FROM scan_cache WHERE path=?1", [path]).is_err() {
            return;
        }
    }
    if tx.commit().is_err() {
        return;
    }
    // A failed transaction retains dirty flags for retry rather than losing its writes.
    for path in written {
        if let Some(entry) = g.get_mut(&path) {
            entry.dirty = false;
        }
    }
    for path in vanished {
        g.remove(Path::new(&path));
    }
    cache
        .any_dirty
        .store(g.values().any(|entry| entry.dirty), Ordering::Relaxed);
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
        conn.execute(
            "UPDATE scan_cache SET interpretation_version=?1",
            [INTERPRETATION_VERSION - 1],
        )
        .unwrap();
        conn.execute(
            "UPDATE catalog_meta SET value=?1 WHERE key='discovery_version'",
            [(INTERPRETATION_VERSION - 1).to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync','1');
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
    #[test]
    fn failed_flush_retries_and_prunes_newly_vanished_entries() {
        let w = cv_testkit::World::in_process("scan-persist-retry");
        let path = w.base.join("retry.jsonl");
        std::fs::write(&path, "data").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "retry".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        let conn = crate::catalog::open_db().unwrap();
        cached_scan(&path, || Some(r.clone()));
        conn.execute_batch(
            "CREATE TRIGGER reject_cache BEFORE INSERT ON scan_cache BEGIN SELECT RAISE(ABORT,'retry'); END;",
        )
        .unwrap();
        persist(false);
        assert!(CACHE.get().unwrap().entries.read().unwrap().get(&path).unwrap().dirty);
        conn.execute_batch("DROP TRIGGER reject_cache;").unwrap();
        persist(false);
        assert!(!CACHE.get().unwrap().entries.read().unwrap().get(&path).unwrap().dirty);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let vanished = w.base.join("newly-vanished.jsonl");
        std::fs::write(&vanished, "data").unwrap();
        cached_scan(&vanished, || {
            Some(SessionRef {
                path: vanished.clone(),
                ..r
            })
        });
        std::fs::remove_file(&vanished).unwrap();
        // Include a malformed persisted row: pruning must not rely only on loaded entries.
        conn.execute("INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads) VALUES('missing-malformed',1,1,'broken','broken')", []).unwrap();
        persist(true);
        assert!(!CACHE.get().unwrap().entries.read().unwrap().contains_key(&vanished));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM scan_cache WHERE path=?1 OR path='missing-malformed'",
                [vanished.to_string_lossy()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn discovery_mutation_keeps_original_identity_and_forces_rescan() {
        let w = cv_testkit::World::in_process("scan-mutation");
        let path = w.base.join("raced.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            path: path.clone(),
            id: "old".into(),
            harness: crate::Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        cached_scan(&path, || {
            std::fs::write(&path, "after!").unwrap();
            Some(r.clone())
        })
        .unwrap();
        let reads = captured_reads(&path).unwrap();
        crate::catalog::replace_harness(
            crate::Harness::Claude,
            std::slice::from_ref(&r),
            &[],
            &Default::default(),
        );
        let conn = crate::catalog::open_db().unwrap();
        let json: String = conn
            .query_row(
                "SELECT reads FROM primary_sources WHERE path=?1",
                [path.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        let published: crate::source::ReadSet = serde_json::from_str(&json).unwrap();
        assert_eq!(published, reads, "catalog must publish the original captured identity");
        assert!(
            !reads.fresh(&mut crate::source::Probe::default()),
            "publication must retain the before-read identity"
        );
        let fresh = cached_scan(&path, || Some(SessionRef { id: "new".into(), ..r })).unwrap();
        assert_eq!(fresh.id, "new");
        persist(false);
        let loaded = load();
        let entries = loaded.entries.read().unwrap();
        assert!(entries
            .get(&path)
            .unwrap()
            .reads
            .fresh(&mut crate::source::Probe::default()));
    }
}
