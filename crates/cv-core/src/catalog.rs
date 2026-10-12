//! A persistent SQLite catalog of discovered sessions: `id → (harness, path, metadata)` — and,
//! since v0.9.12, the **hot read path** for listing the fleet.
//!
//! [`crate::find`] used to resolve one session id by calling `discover()` on **every** adapter and
//! scanning the whole fleet — O(all sessions across all harnesses) for a single `cv show <id>`. The
//! catalog turns that into an indexed lookup: [`crate::discover_all`] replaces every harness's rows
//! here, and `find` consults it first (O(1)/O(log n)), falling back to a full scan only on a miss
//! (which then re-warms the catalog).
//!
//! The same rows now also serve [`crate::sessions`], which answers `cv ls`/`stats`
//! without touching the fleet at all. Correctness rests on the **freshness probe**
//! ([`probe_stale`]): every full discovery records, per harness, the directories that directly
//! contain session files (plus all ancestors up to the harness root) and — for the SQLite-backed
//! harnesses — the database files and their WAL sidecars, each with its mtime at scan time (the `watched`
//! table). The probe re-stats those few hundred paths in milliseconds; any new/changed/vanished
//! entry marks its harness for re-discovery.
//!
//! Sibling tables: `scan_cache` is [`crate::discover_cache`]'s per-file `(mtime, size) → refs`
//! store (it owns the reads/writes; the DDL lives here so every opener sees one schema), and
//! `catalog_meta` holds the `last_full_sync` stamp behind the bounded-staleness backstop.
//!
//! Best-effort throughout: any sqlite error is swallowed and the caller falls back to scanning, so
//! correctness never depends on the catalog (a stale row is filtered by `path.exists()` at lookup).
//! Behind the `sqlite` feature (default-on); no-op fallbacks keep the scan path working without it.

use crate::ir::{Harness, SessionRef};
#[cfg(feature = "sqlite")]
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection, Row};
    use std::collections::{BTreeSet, HashSet};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// How many of the most-recently-updated session files the probe re-stats to catch in-place
    /// appends (POSIX directory mtimes don't change when a file inside merely grows).
    const TOP_K: usize = 50;

    /// A directory (or db file) whose recorded mtime is younger than this at record time is stamped
    /// `mtime_ns = 0` ("always stale"): a session file may have landed *between* discovery
    /// enumerating the directory and us stat'ing it, so the next probe re-discovers that harness
    /// once and re-records a trustworthy mtime. Self-quiescing: an actively-written harness pays
    /// one cheap re-discovery per probe until it goes idle for this long.
    const WATCH_FUDGE: Duration = Duration::from_secs(2);

    /// An optional WAL was absent at discovery. Unlike `0`, stable absence is fresh; its
    /// appearance must invalidate discovery even if the database itself has not changed.
    const WATCH_ABSENT: i64 = -1;

    fn db_path() -> Option<PathBuf> {
        // Back-compat: also honor the legacy `$CLAURDVOYANT_HOME` / `claurdvoyant` cache dir so a
        // catalog built before the rename keeps being used.
        let dir = std::env::var_os("CLUSTERVISION_HOME")
            .or_else(|| std::env::var_os("CLAURDVOYANT_HOME"))
            .map(PathBuf::from)
            .or_else(|| {
                let cache = dirs::cache_dir()?;
                let new = cache.join("clustervision");
                let legacy = cache.join("claurdvoyant");
                Some(if !new.exists() && legacy.exists() { legacy } else { new })
            })?;
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join("catalog.db"))
    }

    /// An open connection to the shared catalog db (WAL + busy timeout, `sessions` DDL applied),
    /// for sibling catalog tables — the event catalog ([`crate::events`]) adds its own DDL on top.
    /// `None` on any error; callers degrade.
    pub(crate) fn open_db() -> Option<Connection> {
        open()
    }

    fn open() -> Option<Connection> {
        let mut conn = Connection::open(db_path()?).ok()?;
        // WAL + a busy timeout so concurrent cv processes don't error each other out.
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(
                 harness       TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 cwd           TEXT,
                 title         TEXT,
                 created_at    INTEGER,
                 updated_at    INTEGER,
                 message_count INTEGER,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);
             CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
             CREATE TABLE IF NOT EXISTS watched(
                 harness  TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 PRIMARY KEY(harness, path)
             );
             CREATE TABLE IF NOT EXISTS scan_cache(
                 path     TEXT PRIMARY KEY,
                 mtime_ns INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 refs     TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS affordance_counts(
                 harness TEXT NOT NULL,
                 id      TEXT NOT NULL,
                 path    TEXT NOT NULL,
                 sig_a   INTEGER NOT NULL,
                 sig_b   INTEGER NOT NULL,
                 counts  TEXT NOT NULL,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE TABLE IF NOT EXISTS catalog_meta(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- The seek store (0.14 and earlier) is gone; see `crate::offsets`.
             DROP TABLE IF EXISTS msg_offsets;
             DROP TABLE IF EXISTS msg_offsets_schema;",
        )
        .ok()?;
        // `sessions` predates the catalog read path, which needs sub-second timestamps to
        // reproduce discovery's ordering exactly. The original second-precision columns stay (the
        // event catalog joins them); these ride alongside. Errors ignored: a concurrent open may
        // have just added them (and a pre-migration reader simply sees NULL → second fallback).
        let has_ns: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name='updated_ns')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if !has_ns {
            let _ = conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN created_ns INTEGER;
                 ALTER TABLE sessions ADD COLUMN updated_ns INTEGER;",
            );
        }
        let has_version: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scan_cache') WHERE name='interpretation_version')",
                [],
                |row| row.get(0),
            )
            .ok()?;
        if !has_version {
            // Nullable: an older binary's INSERT OR REPLACE omits it and produces a stale row.
            let _ = conn.execute_batch("ALTER TABLE scan_cache ADD COLUMN interpretation_version INTEGER;");
        }
        let version = crate::discover_cache::INTERPRETATION_VERSION.to_string();
        let old: Option<String> = conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                [],
                |row| row.get(0),
            )
            .ok();
        if old.as_deref() != Some(version.as_str()) {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .ok()?;
            // Recheck under the writer lock: a concurrent new process may already have upgraded.
            let old: Option<String> = tx
                .query_row(
                    "SELECT value FROM catalog_meta WHERE key='discovery_version'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if old.as_deref() != Some(version.as_str()) {
                tx.execute("DELETE FROM watched", []).ok()?;
                tx.execute("DELETE FROM catalog_meta WHERE key='last_full_sync'", [])
                    .ok()?;
                tx.execute(
                    "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('discovery_version',?1)",
                    [&version],
                )
                .ok()?;
                tx.commit().ok()?;
                crate::discover_cache::invalidate_interpretation();
            }
        }
        Some(conn)
    }

    fn watch_is_fresh(path: &Path, recorded: i64) -> bool {
        match std::fs::metadata(path) {
            Ok(md) => {
                recorded != 0
                    && recorded != WATCH_ABSENT
                    && md.modified().ok().is_some_and(|t| {
                        t.duration_since(UNIX_EPOCH)
                            .ok()
                            .is_some_and(|d| d.as_nanos() as i64 == recorded)
                    })
            }
            Err(e) => recorded == WATCH_ABSENT && e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// The SELECT column list [`row_to_ref`] decodes — keep the two in lockstep.
    const REF_COLUMNS: &str = "harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns";

    /// A timestamp from the nanosecond column at `ns_idx`, falling back to the second-precision
    /// column at `s_idx` (rows written before the `_ns` migration, or dates outside the i64-ns
    /// range — ±~292 years of 1970, where `timestamp_nanos_opt` stores NULL).
    fn ts_col(row: &Row, ns_idx: usize, s_idx: usize) -> rusqlite::Result<Option<chrono::DateTime<chrono::Utc>>> {
        if let Some(ns) = row.get::<_, Option<i64>>(ns_idx)? {
            return Ok(Some(chrono::DateTime::from_timestamp_nanos(ns)));
        }
        Ok(row
            .get::<_, Option<i64>>(s_idx)?
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)))
    }

    /// One row (in [`REF_COLUMNS`] order) → one `SessionRef`. `Ok(None)` for rows whose harness
    /// this binary doesn't know (a newer binary's rows — skipped, never deleted by us).
    fn row_to_ref(row: &Row) -> rusqlite::Result<Option<SessionRef>> {
        let harness: String = row.get(0)?;
        let Some(harness) = Harness::parse(&harness) else {
            return Ok(None);
        };
        Ok(Some(SessionRef {
            harness,
            id: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            cwd: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            title: row.get(4)?,
            created_at: ts_col(row, 8, 5)?,
            updated_at: ts_col(row, 9, 6)?,
            message_count: row.get::<_, i64>(7)? as usize,
        }))
    }

    /// Insert (or replace) every ref through `stmt` — shared by [`sync`] and [`replace_harness`].
    fn insert_refs(tx: &rusqlite::Transaction, refs: &[SessionRef]) {
        let Ok(mut stmt) = tx.prepare(
            "INSERT OR REPLACE INTO sessions
             (harness,id,path,cwd,title,created_at,updated_at,message_count,created_ns,updated_ns)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        ) else {
            return;
        };
        for r in refs {
            let _ = stmt.execute(params![
                r.harness.as_str(),
                r.id,
                r.path.to_string_lossy(),
                r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                r.title,
                r.created_at.map(|t| t.timestamp()),
                r.updated_at.map(|t| t.timestamp()),
                r.message_count as i64,
                r.created_at.and_then(|t| t.timestamp_nanos_opt()),
                r.updated_at.and_then(|t| t.timestamp_nanos_opt()),
            ]);
        }
    }

    /// Upsert every ref in one transaction, deleting nothing — the *additive* sync for callers
    /// that know about one session (event ingest, tests). Full discovery uses [`replace_harness`]
    /// instead so vanished sessions actually disappear.
    pub fn sync(refs: &[SessionRef]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        insert_refs(&tx, refs);
        let _ = tx.commit();
    }

    /// Replace one harness's catalog rows *and* its watch entries in a single transaction — the
    /// post-discovery sync. Deleting first is what makes vanished sessions vanish from
    /// [`all_sessions`]; the transaction keeps concurrent readers from seeing the gap.
    pub(crate) fn replace_harness(h: Harness, refs: &[SessionRef], watches: &[(PathBuf, i64)]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        let _ = tx.execute("DELETE FROM sessions WHERE harness=?1", params![h.as_str()]);
        let _ = tx.execute("DELETE FROM watched WHERE harness=?1", params![h.as_str()]);
        insert_refs(&tx, refs);
        {
            let Ok(mut stmt) = tx.prepare("INSERT OR REPLACE INTO watched(harness,path,mtime_ns) VALUES(?1,?2,?3)")
            else {
                return;
            };
            for (path, mtime_ns) in watches {
                let _ = stmt.execute(params![h.as_str(), path.to_string_lossy(), mtime_ns]);
            }
        }
        let _ = tx.commit();
    }

    /// Compute the watch set for one harness from a finished discovery: the harness roots, every
    /// ancestor directory between each session file and its root (so a new project/day directory
    /// anywhere in the tree bumps a watched mtime), and — for the SQLite-backed harnesses — each
    /// session's backing file and its WAL (in-place writes change neither directory mtimes nor
    /// necessarily the main database). Absent WALs remain watched. Other paths that can't be
    /// stat'd record `0`, as do recently-modified paths (see [`WATCH_FUDGE`]).
    pub(crate) fn watches_for(h: Harness, roots: &[PathBuf], refs: &[SessionRef]) -> Vec<(PathBuf, i64)> {
        let mut paths: BTreeSet<PathBuf> = roots.iter().cloned().collect();
        let mut optional_wals = BTreeSet::new();
        let file_watch = matches!(
            h,
            Harness::Cursor | Harness::Goose | Harness::Hermes | Harness::Zed | Harness::Devin | Harness::OpenCode
        );
        for r in refs {
            let sqlite_watch =
                file_watch || (h == Harness::OpenClaw && r.path.extension().and_then(|e| e.to_str()) == Some("sqlite"));
            if sqlite_watch {
                paths.insert(r.path.clone());
            }
            if sqlite_watch
                && r.path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb"))
            {
                let mut wal = r.path.as_os_str().to_os_string();
                wal.push("-wal");
                let wal = PathBuf::from(wal);
                optional_wals.insert(wal.clone());
                paths.insert(wal);
            }
            let root = roots.iter().map(PathBuf::as_path).find(|rt| r.path.starts_with(rt));
            let mut d = r.path.parent();
            while let Some(dir) = d {
                paths.insert(dir.to_path_buf());
                match root {
                    // Still strictly below the root: keep walking up.
                    Some(rt) if dir != rt && dir.starts_with(rt) => d = dir.parent(),
                    // Reached the root, or the path lives outside it: stop.
                    _ => break,
                }
            }
        }
        let now = SystemTime::now();
        paths
            .into_iter()
            .filter_map(|p| {
                let md = match std::fs::metadata(&p) {
                    Ok(md) => md,
                    Err(e) if optional_wals.contains(&p) && e.kind() == std::io::ErrorKind::NotFound => {
                        return Some((p, WATCH_ABSENT));
                    }
                    Err(_) => return Some((p, 0)), // unknown: never record as fresh
                };
                let Ok(mtime) = md.modified() else { return Some((p, 0)) };
                let ns = mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64;
                let ns = match now.duration_since(mtime) {
                    Ok(age) if age >= WATCH_FUDGE => ns,
                    _ => 0, // too fresh (or clock skew): force one re-discovery next probe
                };
                Some((p, ns))
            })
            .collect()
    }

    /// The freshness probe: which harnesses need re-discovery before the catalog can be trusted?
    ///
    /// Three checks, all stat-only (a few hundred syscalls, ~ms):
    /// 1. every `watched` path (dirs + db files) — new/changed/vanished mtime ⇒ stale harness;
    /// 2. the [`TOP_K`] most-recently-updated session files that have a `scan_cache` entry —
    ///    an in-place append changes `(mtime, size)` there (active sessions are precisely the
    ///    recently-updated ones);
    /// 3. each registered adapter's `roots()` (any at all) vs. whether we hold watch rows for
    ///    it — a harness installed (or wholly removed) since the last sync is stale — and each of
    ///    its existing `roots()` vs. a watch row for that path: a root that appeared since
    ///    the last sync (a new agent seat under a `*` Claude root, a line added to the roots file,
    ///    or another cv process that syncs with different roots) is stale too.
    ///
    /// `None` means the catalog is unusable (sqlite error) and the caller must fall back to a full
    /// discovery. An empty Vec means "fresh, read away".
    ///
    /// Documented gaps (accepted; bounded by the [`crate::sessions`] staleness backstop):
    /// * an append to a session **outside** the top-K window, or to a session of a harness that
    ///   doesn't use the scan cache and isn't db-file-watched (e.g. grok/opencode), goes unseen
    ///   until the backstop — unless the writer rename-installs the file, which bumps the dir;
    /// * a first session appearing in a previously *empty* pre-existing subdirectory deeper than
    ///   any watched dir (no watched mtime changes);
    /// * filesystems with coarse mtime granularity can miss a same-tick change (the classic
    ///   make(1) race) — APFS/ext4 nanosecond stamps make this vanishingly rare.
    pub(crate) fn probe_stale() -> Option<Vec<Harness>> {
        let conn = open()?;
        let mut stale: Vec<Harness> = Vec::new();
        fn mark(stale: &mut Vec<Harness>, h: Harness) {
            if !stale.contains(&h) {
                stale.push(h);
            }
        }

        // 1. watched dirs + db files.
        let mut watched_harnesses: HashSet<Harness> = HashSet::new();
        let mut watched_paths: HashSet<(Harness, String)> = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT harness, path, mtime_ns FROM watched").ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })
                .ok()?;
            for (h, path, recorded) in rows.flatten() {
                let Some(h) = Harness::parse(&h) else { continue };
                watched_harnesses.insert(h);
                watched_paths.insert((h, path.clone()));
                if stale.contains(&h) {
                    continue; // already condemned; skip the stat
                }
                if !watch_is_fresh(Path::new(&path), recorded) {
                    mark(&mut stale, h); // changed, vanished, or fudge-stamped
                }
            }
        }

        // 2. top-K recently-updated session files with scan-cache stats (append detection).
        {
            let mut stmt = conn
                .prepare(
                    "SELECT s.harness, s.path, c.mtime_ns, c.size
                     FROM sessions s JOIN scan_cache c ON c.path = s.path
                     ORDER BY s.updated_at DESC LIMIT ?1",
                )
                .ok()?;
            let rows = stmt
                .query_map(params![TOP_K as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .ok()?;
            let mut seen: HashSet<String> = HashSet::new();
            for (h, path, mtime_ns, size) in rows.flatten() {
                let Some(h) = Harness::parse(&h) else { continue };
                if stale.contains(&h) || !seen.insert(path.clone()) {
                    continue;
                }
                let live = std::fs::metadata(Path::new(&path)).ok().and_then(|m| {
                    let t = m.modified().ok()?;
                    Some((t.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64, m.len() as i64))
                });
                if live != Some((mtime_ns, size)) {
                    mark(&mut stale, h);
                }
            }
        }

        // 3. harnesses that appeared (root now exists, no watch rows) or vanished outright, and
        //    existing roots with no watch row of their own (appeared since the last sync).
        for a in crate::harness::all() {
            let h = a.harness();
            if stale.contains(&h) {
                continue;
            }
            let unwatched_root = || {
                a.roots()
                    .iter()
                    .any(|r| r.exists() && !watched_paths.contains(&(h, r.to_string_lossy().into_owned())))
            };
            if !a.roots().is_empty() != watched_harnesses.contains(&h) || unwatched_root() {
                mark(&mut stale, h);
            }
        }

        Some(stale)
    }

    /// Every catalog row, in insertion (= discovery) order: [`replace_harness`] deletes + inserts
    /// each harness's refs in the order discovery produced them, so after a full sync `rowid`
    /// order reproduces [`crate::discover_all`]'s output order exactly — which keeps `cv ls`'s
    /// stable sorts byte-identical even across rows whose timestamps tie. `None` on any sqlite
    /// error — callers fall back to a full discovery. Note `Some(vec![])` is a valid answer (a
    /// machine with no sessions); cold-vs-empty is [`last_full_sync`]'s job.
    pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
        let conn = open()?;
        let mut stmt = conn
            .prepare(&format!("SELECT {REF_COLUMNS} FROM sessions ORDER BY rowid"))
            .ok()?;
        let rows = stmt.query_map([], row_to_ref).ok()?;
        Some(rows.flatten().flatten().collect())
    }

    /// Seconds-since-epoch of the last completed full discovery, if any. Absent = cold catalog.
    pub(crate) fn last_full_sync() -> Option<i64> {
        let conn = open()?;
        conn.query_row("SELECT value FROM catalog_meta WHERE key='last_full_sync'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()?
        .parse()
        .ok()
    }

    /// Stamp "a full discovery just completed" (the [`last_full_sync`] the staleness backstop reads).
    pub(crate) fn stamp_full_sync() {
        let Some(conn) = open() else { return };
        let _ = conn.execute(
            "INSERT OR REPLACE INTO catalog_meta(key,value) VALUES('last_full_sync',?1)",
            params![chrono::Utc::now().timestamp().to_string()],
        );
    }

    /// A full parse counted `n` turns in `r`: write it into `r`'s catalog row and its scan-cache
    /// entry where they hold `0` ("not counted"), so listings show it and the next discovery (a
    /// scan-cache hit while the file is unchanged) keeps it. A row with a count is never touched:
    /// discovery's number stays the authority wherever it has one. Never creates the catalog.
    pub fn fill_count(r: &SessionRef, n: usize) {
        if !db_path().is_some_and(|p| p.exists()) {
            return;
        }
        let Some(conn) = open() else { return };
        let path = r.path.to_string_lossy();
        let _ = conn.execute(
            "UPDATE sessions SET message_count=?1
             WHERE harness=?2 AND id=?3 AND path=?4 AND message_count=0",
            params![n as i64, r.harness.as_str(), r.id, path],
        );
        crate::discover_cache::fill_count(&conn, &r.path, r.harness, &r.id, n);
    }

    /// `(harness, id, path)` → the captured primary source tuple and counts a full read recorded.
    type AffordanceRows = std::collections::HashMap<(String, String, String), (i64, i64, String)>;

    /// Legacy aggregate signature retained for API compatibility. Fresh affordance counts use
    /// the captured primary tuple plus exact auxiliary dependencies instead: directory totals
    /// and a shared database session's catalog timestamp cannot establish those dependencies.
    pub fn affordance_sig(r: &SessionRef) -> (i64, i64) {
        let Ok(md) = std::fs::metadata(&r.path) else {
            return (0, 0);
        };
        let shared_db = r
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb"));
        if shared_db {
            return (r.updated_at.and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0), -1);
        }
        if md.is_dir() {
            let (mut newest, mut total) = (0i64, 0i64);
            for e in std::fs::read_dir(&r.path).into_iter().flatten().flatten() {
                let Ok(m) = e.metadata() else { continue };
                if m.is_file() {
                    let t = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos() as i64);
                    newest = newest.max(t);
                    total += m.len() as i64;
                }
            }
            return (newest, total);
        }
        crate::offsets::file_sig(&r.path)
    }

    /// The key under which a counts row records the [`crate::affordance::VIEW_VERSION`] that
    /// counted it.
    const VERSION_KEY: &str = "_v";

    fn counts_text(c: &crate::affordance::Counts) -> String {
        let mut obj: serde_json::Map<String, serde_json::Value> = c
            .iter()
            .map(|(k, n)| (k.as_str().to_string(), serde_json::json!(n)))
            .collect();
        obj.insert(VERSION_KEY.into(), crate::affordance::VIEW_VERSION.into());
        serde_json::Value::Object(obj).to_string()
    }

    /// The counts a row holds, or `None` when an older view counted them (a new kind or a new
    /// reader could have found more): such a row is "not counted", and the next full read
    /// rewrites it.
    fn counts_of(text: &str) -> Option<crate::affordance::Counts> {
        let Ok(serde_json::Value::Object(o)) = serde_json::from_str(text) else {
            return None;
        };
        if o.get(VERSION_KEY).and_then(serde_json::Value::as_u64) != Some(crate::affordance::VIEW_VERSION.into()) {
            return None;
        }
        Some(
            o.iter()
                .filter_map(|(k, v)| Some((crate::affordance::Kind::parse(k)?, v.as_u64()? as u32)))
                .collect(),
        )
    }

    /// Compatibility entry point without a before-read source snapshot. It cannot publish fresh
    /// counts safely: the closure may contain counts from an earlier read. Use
    /// [`note_affordances_with_source`] with a snapshot captured before counting instead.
    #[deprecated(note = "no-op without a before-read source snapshot; use note_affordances_with_source")]
    pub fn note_affordances(_r: &SessionRef, _counts: impl FnOnce() -> crate::affordance::Counts) {}

    /// Record counts against the dependencies captured before their complete source pass.
    pub fn note_affordances_with_source(
        r: &SessionRef,
        source: &crate::source::Snapshot,
        counts: impl FnOnce() -> crate::affordance::Counts,
    ) {
        if !db_path().is_some_and(|p| p.exists()) {
            return;
        }
        if !crate::source::unchanged(r, source) {
            return;
        }
        let (a, b) = source.primary;
        if a == 0 {
            return;
        }
        let Some(mut conn) = open() else { return };
        let path = r.path.to_string_lossy();
        let stored: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT sig_a, sig_b, counts FROM affordance_counts WHERE harness=?1 AND id=?2 AND path=?3",
                params![r.harness.as_str(), r.id, path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        if stored.is_some_and(|(sa, sb, text)| (sa, sb) == (a, b) && counts_of(&text).is_some())
            && crate::source::fresh_in(&conn, "counts", r, source.primary, &source.dependencies)
        {
            return;
        }
        let Ok(tx) = conn.transaction() else { return };
        let written = tx.execute(
            "INSERT OR REPLACE INTO affordance_counts(harness,id,path,sig_a,sig_b,counts) VALUES(?1,?2,?3,?4,?5,?6)",
            params![r.harness.as_str(), r.id, path, a, b, counts_text(&counts())],
        );
        if written.is_ok() && crate::source::publish_in(&tx, "counts", r, source).is_some() {
            let _ = tx.commit();
        }
    }

    /// Every recorded affordance count, loaded once for a sweep (`cv ls -q has:plan`, `cv index`).
    pub struct AffordanceTable {
        rows: AffordanceRows,
        dependencies: crate::source::Table,
    }

    impl AffordanceTable {
        pub fn load() -> AffordanceTable {
            let load = || -> Option<AffordanceTable> {
                let mut conn = open()?;
                let tx = conn.transaction().ok()?;
                let mut stmt = tx
                    .prepare("SELECT harness, id, path, sig_a, sig_b, counts FROM affordance_counts")
                    .ok()?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            (row.get(0)?, row.get(1)?, row.get(2)?),
                            (row.get(3)?, row.get(4)?, row.get(5)?),
                        ))
                    })
                    .ok()?;
                Some(AffordanceTable {
                    rows: rows.flatten().collect(),
                    dependencies: crate::source::Table::load_in(&tx, "counts"),
                })
            };
            load().unwrap_or_else(|| AffordanceTable {
                rows: Default::default(),
                dependencies: Default::default(),
            })
        }

        /// `r`'s counts when a full read by this view recorded them and they are still fresh;
        /// `None` ("not counted") otherwise. Stats the session once.
        pub fn counts(&self, r: &SessionRef) -> Option<crate::affordance::Counts> {
            self.counts_with_source(r, &crate::source::Probe::default().snapshot(r))
        }

        /// Use a shared operation's dependency probe rather than stat'ing shared files again.
        pub fn counts_with_source(
            &self,
            r: &SessionRef,
            source: &crate::source::Snapshot,
        ) -> Option<crate::affordance::Counts> {
            self.counts_with_probe(r, source, &mut crate::source::Probe::default())
        }

        pub fn counts_with_probe(
            &self,
            r: &SessionRef,
            source: &crate::source::Snapshot,
            probe: &mut crate::source::Probe,
        ) -> Option<crate::affordance::Counts> {
            let key = (
                r.harness.as_str().to_string(),
                r.id.clone(),
                r.path.to_string_lossy().into_owned(),
            );
            let (a, b, text) = self.rows.get(&key)?;
            if (*a, *b) == source.primary
                && *a != 0
                && self
                    .dependencies
                    .fresh_with_probe(r, source.primary, &source.dependencies, probe)
            {
                counts_of(text)
            } else {
                None
            }
        }

        /// Whether `r` has no fresh counts (a sweep re-reads it to fill them).
        pub fn stale(&self, r: &SessionRef) -> bool {
            self.counts(r).is_none()
        }
    }

    /// All catalog rows whose id equals `id` or starts with it, optionally constrained to one
    /// harness. Order: exact-id matches first. Empty on any error or a cold catalog.
    pub fn lookup(id: &str, harness: Option<Harness>) -> Vec<SessionRef> {
        let Some(conn) = open() else { return Vec::new() };
        // Escape LIKE metacharacters so ids containing `%`/`_` match literally (same treatment as
        // `events::sessions_touching`) — `_` is common in real session ids.
        let escaped = id.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let like = format!("{escaped}%");
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {REF_COLUMNS} FROM sessions WHERE id=?1 OR id LIKE ?2 ESCAPE '\\'
             ORDER BY (id=?1) DESC",
        )) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![id, like], row_to_ref);
        let Ok(rows) = rows else { return Vec::new() };
        rows.flatten()
            .flatten()
            .filter(|r| harness.is_none_or(|want| r.harness == want))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn db_ref(harness: Harness, path: PathBuf) -> SessionRef {
            SessionRef {
                harness,
                id: "session".into(),
                path,
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
            }
        }

        #[test]
        fn watches_track_absent_wal_and_its_appearance_and_removal() {
            let w = cv_testkit::World::in_process("catalog-absent-wal");
            let path = w.base.join("openclaw-agent.sqlite");
            std::fs::write(&path, []).unwrap();
            let wal = w.base.join("openclaw-agent.sqlite-wal");
            let r = db_ref(Harness::OpenClaw, path.clone());
            let watches = watches_for(Harness::OpenClaw, std::slice::from_ref(&w.base), &[r]);
            assert!(
                watches.iter().any(|(p, _)| *p == path),
                "OpenClaw SQLite is file-watched"
            );
            let absent = watches.iter().find(|(p, _)| *p == wal).unwrap().1;
            assert_eq!(absent, WATCH_ABSENT);
            assert!(watch_is_fresh(&wal, absent));

            std::fs::write(&wal, "wal").unwrap();
            assert!(
                !watch_is_fresh(&wal, absent),
                "WAL appearance invalidates its absent watch"
            );
            let present = crate::offsets::file_sig(&wal).0;
            assert!(watch_is_fresh(&wal, present));
            std::fs::remove_file(&wal).unwrap();
            assert!(
                !watch_is_fresh(&wal, present),
                "WAL removal invalidates its present watch"
            );
            assert!(!watch_is_fresh(&wal, 0), "unknown is not stable absence");

            let jsonl = db_ref(Harness::OpenClaw, w.base.join("legacy.jsonl"));
            let watches = watches_for(Harness::OpenClaw, &[], std::slice::from_ref(&jsonl));
            assert!(
                !watches.iter().any(|(p, _)| *p == jsonl.path),
                "legacy JSONL keeps the bounded file probe"
            );
        }

        #[test]
        fn wal_only_write_invalidates_database_watch_set() {
            let w = cv_testkit::World::in_process("catalog-live-wal");
            let path = w.base.join("sessions.db");
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE messages(text TEXT);",
            )
            .unwrap();
            let wal = w.base.join("sessions.db-wal");
            // Age the files so the test exercises actual WAL freshness, not WATCH_FUDGE.
            let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for p in [&path, &wal, &w.base] {
                std::fs::File::open(p)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
            let db_sig = crate::offsets::file_sig(&path);
            let r = db_ref(Harness::OpenCode, path.clone());
            let watches = watches_for(Harness::OpenCode, std::slice::from_ref(&w.base), &[r]);
            assert!(watches.iter().all(|(p, stamp)| watch_is_fresh(p, *stamp)));
            conn.execute("INSERT INTO messages VALUES ('new message')", []).unwrap();
            assert_eq!(crate::offsets::file_sig(&path), db_sig, "write stayed in WAL");
            let changed: Vec<_> = watches.iter().filter(|(p, stamp)| !watch_is_fresh(p, *stamp)).collect();
            assert_eq!(changed.len(), 1);
            assert_eq!(
                changed[0].0, wal,
                "only WAL invalidates the otherwise unchanged watch set"
            );
        }

        /// A counts row an older view wrote (no version, or another one) is "not counted": a kind
        /// that view did not know must not read as absent.
        #[test]
        fn counts_an_older_view_wrote_are_not_counted() {
            let mut c = crate::affordance::Counts::new();
            c.insert(crate::affordance::Kind::Approval, 2);
            assert_eq!(counts_of(&counts_text(&c)), Some(c));
            assert_eq!(counts_of(r#"{"plan": 1}"#), None);
            assert_eq!(counts_of(r#"{"plan": 1, "_v": 1}"#), None);
            // Classifier changes can turn a previous view's zero into a positive count.
            // Reject that row so both has:approval and its negation trigger a recount.
            let previous = serde_json::json!({
                "approval": 0,
                "_v": crate::affordance::VIEW_VERSION - 1,
            });
            assert_eq!(counts_of(&previous.to_string()), None);
        }
    }
}

#[cfg(feature = "sqlite")]
#[allow(deprecated)]
pub use imp::{
    affordance_sig, fill_count, lookup, note_affordances, note_affordances_with_source, sync, AffordanceTable,
};
#[cfg(feature = "sqlite")]
pub(crate) use imp::{
    all_sessions, last_full_sync, open_db, probe_stale, replace_harness, stamp_full_sync, watches_for,
};

/// No-op fallbacks when sqlite is disabled — `find`/`sessions` simply always take the scan path.
#[cfg(not(feature = "sqlite"))]
pub fn sync(_refs: &[SessionRef]) {}
#[cfg(not(feature = "sqlite"))]
pub fn fill_count(_r: &SessionRef, _n: usize) {}
#[cfg(not(feature = "sqlite"))]
#[deprecated(note = "no-op without a before-read source snapshot; use note_affordances_with_source")]
pub fn note_affordances(_r: &SessionRef, _counts: impl FnOnce() -> crate::affordance::Counts) {}
#[cfg(not(feature = "sqlite"))]
pub fn note_affordances_with_source(
    _r: &SessionRef,
    _source: &crate::source::Snapshot,
    _counts: impl FnOnce() -> crate::affordance::Counts,
) {
}
/// Without a catalog nothing is ever counted: every `has:<kind>` parses.
#[cfg(not(feature = "sqlite"))]
pub struct AffordanceTable;
#[cfg(not(feature = "sqlite"))]
impl AffordanceTable {
    pub fn load() -> AffordanceTable {
        AffordanceTable
    }
    pub fn counts(&self, _r: &SessionRef) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_source(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Snapshot,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn counts_with_probe(
        &self,
        _r: &SessionRef,
        _source: &crate::source::Snapshot,
        _probe: &mut crate::source::Probe,
    ) -> Option<crate::affordance::Counts> {
        None
    }
    pub fn stale(&self, _r: &SessionRef) -> bool {
        false
    }
}
#[cfg(not(feature = "sqlite"))]
pub fn lookup(_id: &str, _harness: Option<Harness>) -> Vec<SessionRef> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn replace_harness(_h: Harness, _refs: &[SessionRef], _watches: &[(std::path::PathBuf, i64)]) {}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn watches_for(
    _h: Harness,
    _roots: &[std::path::PathBuf],
    _refs: &[SessionRef],
) -> Vec<(std::path::PathBuf, i64)> {
    Vec::new()
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn probe_stale() -> Option<Vec<Harness>> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn all_sessions() -> Option<Vec<SessionRef>> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn last_full_sync() -> Option<i64> {
    None
}
#[cfg(not(feature = "sqlite"))]
pub(crate) fn stamp_full_sync() {}
