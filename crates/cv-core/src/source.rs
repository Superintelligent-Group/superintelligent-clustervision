//! Auxiliary source dependencies, distinct from a transcript's primary `(mtime, size)`.
//!
//! Stamps retain canonical individual path records, including absence, rather than a lossy
//! max-mtime/total-size aggregate. Each consumer owns its baseline: refreshing counts cannot
//! make an older search index fresh. Missing baselines cause an incremental reread once.
//!
//! These are metadata freshness guards, not content verification. Primary transcripts retain
//! the size/mtime tuple; timestamp-restored rewrites are outside that guarantee. Inventories
//! cover known adapter-local files. Claude's conditional fork inference reads siblings and an
//! adopted transcript may point to tool outputs outside its local tool-results directory; those
//! require a future consumer-recorded read set and are not covered by the local inventory.

use crate::ir::{Harness, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Entry {
    path: PathBuf,
    state: Option<FileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileState {
    len: u64,
    modified_ns: u64,
    identity: Option<(u64, u64, i64, i64)>,
}

/// Exact serialized metadata records for an auxiliary dependency set, not a content digest.
#[derive(Serialize)]
struct Records {
    version: u32,
    entries: Vec<Entry>,
}

/// A canonical record string. Loaded baselines remain strings instead of allocating each path
/// three times for counts, events and search; equality compares the actual records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp(String);

impl Stamp {
    fn empty(&self) -> bool {
        self.0 == r#"{"version":1,"entries":[]}"#
    }
}

/// A source pass's captured primary signature and auxiliary dependencies.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub primary: (i64, i64),
    pub dependencies: Option<Stamp>,
}

/// File and immediate-directory probes shared within one operation. It never descends recursively.
#[derive(Default)]
pub struct Probe {
    entries: HashMap<PathBuf, Option<Entry>>,
    directories: HashMap<PathBuf, Option<Vec<PathBuf>>>,
}

impl Probe {
    fn entry(&mut self, path: &Path) -> Option<Entry> {
        self.entries
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let state = match std::fs::metadata(path) {
                    Ok(md) => {
                        let modified = md
                            .modified()
                            .ok()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()?
                            .as_nanos() as u64;
                        #[cfg(unix)]
                        let identity = {
                            use std::os::unix::fs::MetadataExt;
                            Some((md.dev(), md.ino(), md.ctime(), md.ctime_nsec()))
                        };
                        #[cfg(not(unix))]
                        let identity = None;
                        Some(FileState {
                            len: md.len(),
                            modified_ns: modified,
                            identity,
                        })
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => return None,
                };
                Some(Entry {
                    path: path.to_path_buf(),
                    state,
                })
            })
            .clone()
    }

    fn directory(&mut self, path: &Path) -> Option<Vec<PathBuf>> {
        self.directories
            .entry(path.to_path_buf())
            .or_insert_with(|| match std::fs::read_dir(path) {
                Ok(rd) => rd.map(|e| e.ok().map(|e| e.path())).collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
                Err(_) => None,
            })
            .clone()
    }

    /// Collect the currently declared session-local dependencies. Plain file transcripts have
    /// an empty set; shared databases depend on WAL even when it is absent.
    pub fn dependencies(&mut self, r: &SessionRef) -> Option<Stamp> {
        let mut files = Vec::new();
        let mut directories = Vec::new();
        let ext = r.path.extension().and_then(|e| e.to_str());
        if ext.is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3" | "vscdb")) {
            let mut wal = r.path.as_os_str().to_os_string();
            wal.push("-wal");
            files.push(PathBuf::from(wal));
        }
        match r.harness {
            Harness::Claude => {
                if r.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"))
                {
                    files.push(r.path.with_extension("meta.json"));
                }
                directories.push(r.path.with_extension("").join("tool-results"));
                if let Some(session) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "subagents" || n == "workflows"))
                    .and_then(Path::parent)
                {
                    directories.push(session.join("tool-results"));
                }
                if let Some(config) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "projects"))
                    .and_then(Path::parent)
                {
                    directories.push(config.join("tasks").join(&r.id));
                }
            }
            Harness::Grok => {
                files.extend(
                    [
                        "summary.json",
                        "chat_history.jsonl",
                        "updates.jsonl",
                        "system_prompt.txt",
                        "plan.json",
                        "rewind_points.jsonl",
                    ]
                    .map(|name| r.path.join(name)),
                );
                // Only subagents consult sibling parent metadata. Ordinary sessions never scan
                // a cwd group's siblings merely to establish freshness.
                let summary = std::fs::read(r.path.join("summary.json")).ok()?;
                let summary: serde_json::Value = serde_json::from_slice(&summary).ok()?;
                if summary.get("session_kind").and_then(serde_json::Value::as_str) == Some("subagent") {
                    let group = r.path.parent()?;
                    files.push(group.to_path_buf());
                    for sibling in self.directory(group)? {
                        files.push(sibling.join("subagents").join(&r.id).join("meta.json"));
                    }
                }
            }
            Harness::Cline
                if r.path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".messages.json")) =>
            {
                let sid = r.id.split('/').next().unwrap_or(&r.id);
                if let Some(dir) = r.path.parent() {
                    files.push(dir.join(format!("{sid}.json")));
                    if !r.id.contains('/') {
                        if let Some(data) = dir.parent().and_then(Path::parent) {
                            for name in ["sessions.db", "cron.db", "teams.db"] {
                                let db = data.join("db").join(name);
                                let mut wal = db.as_os_str().to_os_string();
                                wal.push("-wal");
                                files.push(db);
                                files.push(PathBuf::from(wal));
                            }
                        }
                    }
                }
            }
            Harness::Gemini => {
                if let Some(project) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "chats"))
                    .and_then(Path::parent)
                {
                    directories.push(project.join(&r.id).join("tracker"));
                    directories.push(project.join(&r.id).join("plans"));
                }
            }
            Harness::Qwen => {
                files.push(r.path.with_extension("meta.json"));
                if let Some(chats) = r.path.parent().filter(|p| p.file_name().is_some_and(|n| n == "chats")) {
                    if let (Some(root), Some(cwd)) = (
                        chats.parent().and_then(Path::parent).and_then(Path::parent),
                        r.cwd.as_ref().and_then(|p| p.to_str()),
                    ) {
                        files.push(
                            root.join("tmp")
                                .join(crate::digest::sha256_hex(cwd.as_bytes()))
                                .join("scheduled_tasks.json"),
                        );
                    }
                }
            }
            Harness::Cline | Harness::Roo => files.extend(
                [
                    "api_conversation_history.json",
                    "ui_messages.json",
                    "task_metadata.json",
                    "history_item.json",
                ]
                .map(|name| r.path.join(name)),
            ),
            Harness::Kimi => {
                if let Some(root) = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
                    .and_then(Path::parent)
                {
                    files.push(root.join("config.toml"));
                }
                files.push(r.path.join("meta.json"));
                if self.entry(&r.path.join("context.jsonl"))?.state.is_some() {
                    // Includes rotated segments and their membership; no subagent/background recursion.
                    directories.push(r.path.clone());
                } else {
                    files.push(r.path.join(format!("{}.jsonl", r.id)));
                }
            }
            Harness::KimiCode => {
                let (session, agent) = match (r.path.parent(), r.path.file_name().and_then(|n| n.to_str())) {
                    (Some(agents), Some(agent))
                        if agents.file_name().is_some_and(|n| n == "agents") && agent.starts_with("agent-") =>
                    {
                        (agents.parent().unwrap_or(&r.path), agent)
                    }
                    _ => (r.path.as_path(), "main"),
                };
                files.push(session.join("state.json"));
                files.push(session.join("agents").join(agent).join("wire.jsonl"));
            }
            Harness::OpenClaw => {
                if let Some(state) = r.path.ancestors().nth(4) {
                    if ext.is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3")) {
                        let db = state.join("state/openclaw.sqlite");
                        files.push(db.clone());
                        let mut wal = db.into_os_string();
                        wal.push("-wal");
                        files.push(PathBuf::from(wal));
                    } else {
                        if let Some(dir) = r.path.parent() {
                            files.push(dir.join("sessions.json"));
                        }
                        files.push(state.join("cron/jobs.json"));
                        files.push(state.join("subagents/runs.json"));
                    }
                }
            }
            Harness::OpenCode if !ext.is_some_and(|e| matches!(e, "db" | "sqlite" | "sqlite3")) => {
                let storage = r
                    .path
                    .ancestors()
                    .find(|p| p.file_name().is_some_and(|n| n == "session"))
                    .and_then(Path::parent)?;
                {
                    let messages = storage.join("message").join(&r.id);
                    // Parts are keyed by the parsed message ID. Read only the ID (other fields are
                    // ignored), and enumerate only this session's own message/part directories.
                    for message in self.directory(&messages)? {
                        #[derive(Deserialize)]
                        struct MessageId {
                            #[serde(default)]
                            id: String,
                        }
                        let file = std::fs::File::open(&message).ok()?;
                        let id: MessageId = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
                        directories.push(storage.join("part").join(id.id));
                    }
                    directories.push(messages);
                }
            }
            _ => {}
        }
        let mut entries = BTreeMap::new();
        for dir in directories {
            entries.insert(dir.clone(), self.entry(&dir)?);
            files.extend(self.directory(&dir)?);
        }
        for file in files {
            entries.insert(file.clone(), self.entry(&file)?);
        }
        Some(Stamp(
            serde_json::to_string(&Records {
                version: 1,
                entries: entries.into_values().collect(),
            })
            .ok()?,
        ))
    }

    pub fn snapshot(&mut self, r: &SessionRef) -> Snapshot {
        Snapshot {
            primary: self
                .entry(&r.path)
                .and_then(|entry| entry.state)
                .map_or((0, 0), |state| (state.modified_ns as i64, state.len as i64)),
            dependencies: self.dependencies(r),
        }
    }
}

/// A captured source still matches at publication time. Errors never establish freshness.
pub fn unchanged(r: &SessionRef, snapshot: &Snapshot) -> bool {
    let now = Probe::default().snapshot(r);
    snapshot.dependencies.is_some() && now.primary == snapshot.primary && now.dependencies == snapshot.dependencies
}

pub type Update = (SessionRef, Snapshot);

type Key = (String, String, String);
fn key(r: &SessionRef) -> Key {
    (
        r.harness.as_str().into(),
        r.id.clone(),
        r.path.to_string_lossy().into_owned(),
    )
}

/// A consumer's baselines, loaded once. Empty sets need no stored row.
#[derive(Default)]
pub struct Table {
    rows: HashMap<Key, ((i64, i64), Stamp)>,
}

impl Table {
    pub fn fresh(&self, r: &SessionRef, primary: (i64, i64), dependencies: &Option<Stamp>) -> bool {
        dependencies.as_ref().is_some_and(|stamp| {
            stamp.empty()
                || self
                    .rows
                    .get(&key(r))
                    .is_some_and(|(old_primary, old)| *old_primary == primary && old == stamp)
        })
    }

    #[cfg(feature = "sqlite")]
    pub fn load(consumer: &str) -> Self {
        open().map_or_else(Self::default, |conn| Self::load_in(&conn, consumer))
    }

    /// Load alongside SQL consumer rows in the caller's read transaction.
    #[cfg(feature = "sqlite")]
    pub(crate) fn load_in(conn: &rusqlite::Connection, consumer: &str) -> Self {
        let load = || -> Option<_> {
            let mut stmt = conn
                .prepare("SELECT harness,id,path,mtime,size,stamp FROM source_dependencies WHERE consumer=?1")
                .ok()?;
            let rows = stmt
                .query_map([consumer], |row| {
                    Ok((
                        (row.get(0)?, row.get(1)?, row.get(2)?),
                        ((row.get(3)?, row.get(4)?), row.get::<_, String>(5)?),
                    ))
                })
                .ok()?;
            Some(
                rows.flatten()
                    .map(|(key, (primary, text))| (key, (primary, Stamp(text))))
                    .collect(),
            )
        };
        Self {
            rows: load().unwrap_or_default(),
        }
    }

    #[cfg(not(feature = "sqlite"))]
    pub fn load(_consumer: &str) -> Self {
        Self { rows: HashMap::new() }
    }
}

#[cfg(feature = "sqlite")]
fn open() -> Option<rusqlite::Connection> {
    let conn = crate::catalog::open_db()?;
    ensure_table(&conn)?;
    Some(conn)
}

#[cfg(feature = "sqlite")]
fn ensure_table(conn: &rusqlite::Connection) -> Option<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_dependencies(
        consumer TEXT NOT NULL,harness TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,
        mtime INTEGER NOT NULL,size INTEGER NOT NULL,stamp TEXT NOT NULL,
        PRIMARY KEY(consumer,harness,id,path));",
    )
    .ok()
}

/// Check one previously recorded baseline without loading every session's records during writes.
#[cfg(feature = "sqlite")]
pub(crate) fn fresh_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    primary: (i64, i64),
    dependencies: &Option<Stamp>,
) -> bool {
    dependencies.as_ref().is_some_and(|stamp| {
        if stamp.empty() {
            return true;
        }
        let (h, id, path) = key(r);
        conn.query_row(
            "SELECT mtime,size,stamp FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
            rusqlite::params![consumer, h, id, path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
        )
        .ok()
        .is_some_and(|(mtime, size, old)| (mtime, size) == primary && old == stamp.0)
    })
}

/// Publish in the same transaction as SQL consumer output. A failure must roll back both.
#[cfg(feature = "sqlite")]
pub(crate) fn publish_in(
    conn: &rusqlite::Connection,
    consumer: &str,
    r: &SessionRef,
    snapshot: &Snapshot,
) -> Option<()> {
    ensure_table(conn)?;
    let (h, id, path) = key(r);
    conn.execute(
        "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
        rusqlite::params![consumer, h, id, path],
    )
    .ok()?;
    if let Some(stamp) = snapshot
        .dependencies
        .as_ref()
        .filter(|stamp| !stamp.empty() && unchanged(r, snapshot))
    {
        conn.execute(
            "INSERT INTO source_dependencies(consumer,harness,id,path,mtime,size,stamp)
            VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![consumer, h, id, path, snapshot.primary.0, snapshot.primary.1, stamp.0],
        )
        .ok()?;
    }
    Some(())
}

/// Invalidate old baselines before changing consumer output. Failure must prevent that output
/// write: otherwise an older stamp could certify newer output after dependencies revert.
#[cfg(feature = "sqlite")]
pub fn invalidate(consumer: &str, refs: &[SessionRef]) -> bool {
    if refs.is_empty() {
        return true;
    }
    let clear = || -> Option<()> {
        let mut conn = open()?;
        let tx = conn.transaction().ok()?;
        for r in refs {
            let (h, id, path) = key(r);
            tx.execute(
                "DELETE FROM source_dependencies WHERE consumer=?1 AND harness=?2 AND id=?3 AND path=?4",
                rusqlite::params![consumer, h, id, path],
            )
            .ok()?;
        }
        tx.commit().ok()
    };
    clear().is_some()
}

#[cfg(not(feature = "sqlite"))]
pub fn invalidate(_consumer: &str, _refs: &[SessionRef]) -> bool {
    true
}

/// Publish after the consumer commits its output. Revalidation makes concurrent edits stale;
/// old baselines must already have been invalidated with [`invalidate`]. SQL consumers instead
/// publish atomically with their output through `publish_in`.
#[cfg(feature = "sqlite")]
pub fn update(consumer: &str, updates: &[Update]) {
    if updates.is_empty() {
        return;
    }
    let Some(mut conn) = open() else { return };
    let Ok(tx) = conn.transaction() else { return };
    for (r, snapshot) in updates {
        if publish_in(&tx, consumer, r, snapshot).is_none() {
            return;
        }
    }
    let _ = tx.commit();
}

#[cfg(not(feature = "sqlite"))]
pub fn update(_consumer: &str, _updates: &[Update]) {}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    #[test]
    fn dependency_baselines_are_per_consumer_and_revalidated_at_publication() {
        let w = cv_testkit::World::in_process("source-baselines");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s1".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let mut probe = Probe::default();
        let before = probe.snapshot(&r);
        let other = SessionRef {
            id: "s2".into(),
            ..r.clone()
        };
        assert_eq!(probe.snapshot(&other).dependencies, before.dependencies);
        assert_eq!(
            probe.entries.len(),
            2,
            "one main DB and one absent WAL probe shared by both sessions"
        );
        assert!(!Table::load("counts").fresh(&r, before.primary, &before.dependencies));
        update("counts", &[(r.clone(), before.clone())]);
        assert!(Table::load("counts").fresh(&r, before.primary, &before.dependencies));
        assert!(!Table::load("events").fresh(&r, before.primary, &before.dependencies));

        std::fs::write(w.base.join("sessions.db-wal"), "new WAL").unwrap();
        let after = Probe::default().snapshot(&r);
        assert_eq!(after.primary, before.primary);
        assert_ne!(after.dependencies, before.dependencies);
        assert!(!unchanged(&r, &before));
        update("counts", &[(r.clone(), before)]);
        assert!(
            !Table::load("counts").fresh(&r, after.primary, &after.dependencies),
            "a racing change cannot publish freshness"
        );
        update("counts", &[(r.clone(), after.clone())]);
        assert!(Table::load("counts").fresh(&r, after.primary, &after.dependencies));
        assert!(!Table::load("events").fresh(&r, after.primary, &after.dependencies));
    }
    #[test]
    fn scoped_children_track_wire_state_and_opencode_parts() {
        let w = cv_testkit::World::in_process("source-children");
        let session = w.base.join("session_test");
        std::fs::create_dir_all(session.join("agents/main")).unwrap();
        std::fs::write(session.join("state.json"), "before").unwrap();
        let wire = session.join("agents/main/wire.jsonl");
        std::fs::write(&wire, "line1\n").unwrap();
        let r = SessionRef {
            harness: Harness::KimiCode,
            id: "test".into(),
            path: session.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let before = Probe::default().snapshot(&r);
        std::fs::write(&wire, "line1\nline2\n").unwrap();
        let after = Probe::default().snapshot(&r);
        assert_eq!(before.primary, after.primary);
        assert_ne!(before.dependencies, after.dependencies);
        std::fs::write(session.join("state.json"), "after!").unwrap();
        assert_ne!(after.dependencies, Probe::default().snapshot(&r).dependencies);

        let storage = w.base.join("storage");
        for dir in ["session/project", "message/sid", "part/mid"] {
            std::fs::create_dir_all(storage.join(dir)).unwrap();
        }
        let path = storage.join("session/project/sid.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::write(storage.join("message/sid/noncanonical.json"), r#"{"id":"mid"}"#).unwrap();
        let part = storage.join("part/mid/pid.json");
        std::fs::write(&part, "before").unwrap();
        let r = SessionRef {
            harness: Harness::OpenCode,
            id: "sid".into(),
            path,
            ..r
        };
        let before = Probe::default().snapshot(&r);
        std::fs::write(&part, "after!").unwrap();
        let after = Probe::default().snapshot(&r);
        assert_eq!(before.primary, after.primary);
        assert_ne!(before.dependencies, after.dependencies, "part-only same-size edit");
        std::fs::write(storage.join("part/mid/new.json"), "new").unwrap();
        assert_ne!(
            after.dependencies,
            Probe::default().snapshot(&r).dependencies,
            "part membership"
        );
    }

    #[test]
    #[allow(deprecated)]
    fn counts_publish_captured_primary_and_legacy_record_stays_stale() {
        let w = cv_testkit::World::in_process("source-counts-publish");
        let path = w.base.join("plain.jsonl");
        std::fs::write(&path, "before").unwrap();
        let r = SessionRef {
            harness: Harness::Codex,
            id: "test".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        crate::catalog::open_db().unwrap();
        let source = Probe::default().snapshot(&r);
        crate::catalog::note_affordances_with_source(&r, &source, || {
            // An edit while producing side-store counts cannot publish a newer primary tuple.
            std::fs::write(&path, "after!").unwrap();
            crate::affordance::Counts::new()
        });
        assert!(crate::catalog::AffordanceTable::load().stale(&r));
        crate::stream::Counted {
            turns: 1,
            counts: crate::affordance::Counts::new(),
        }
        .record(&r);
        assert!(
            crate::catalog::AffordanceTable::load().stale(&r),
            "no before-read snapshot"
        );
    }
    #[test]
    fn failed_stamp_publication_cannot_reuse_an_older_baseline() {
        let w = cv_testkit::World::in_process("source-publication-failure");
        let path = w.base.join("sessions.db");
        std::fs::write(&path, "database").unwrap();
        let r = SessionRef {
            harness: Harness::Hermes,
            id: "s".into(),
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let conn = crate::catalog::open_db().unwrap();
        let before = Probe::default().snapshot(&r);
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        assert!(!crate::catalog::AffordanceTable::load().stale(&r));
        let wal = w.base.join("sessions.db-wal");
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r);
        conn.execute_batch(
            "CREATE TRIGGER reject_stamp BEFORE INSERT ON source_dependencies
            WHEN NEW.consumer='counts' BEGIN SELECT RAISE(FAIL,'stamp rejected'); END;",
        )
        .unwrap();
        crate::catalog::note_affordances_with_source(&r, &after, || {
            [(crate::affordance::Kind::Plan, 1)].into_iter().collect()
        });
        std::fs::remove_file(&wal).unwrap();
        assert_eq!(before.dependencies, Probe::default().snapshot(&r).dependencies);
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed stamp insert rolls back new counts too"
        );
        conn.execute_batch("DROP TRIGGER reject_stamp;").unwrap();
        crate::catalog::note_affordances_with_source(&r, &before, crate::affordance::Counts::new);
        conn.execute_batch(
            "CREATE TRIGGER reject_invalidation BEFORE DELETE ON source_dependencies
            WHEN OLD.consumer='counts' BEGIN SELECT RAISE(FAIL,'delete rejected'); END;",
        )
        .unwrap();
        std::fs::write(&wal, "new").unwrap();
        let after = Probe::default().snapshot(&r);
        let mut called = false;
        crate::catalog::note_affordances_with_source(&r, &after, || {
            called = true;
            crate::affordance::Counts::new()
        });
        assert!(called);
        std::fs::remove_file(&wal).unwrap();
        assert!(
            crate::catalog::AffordanceTable::load().counts(&r).unwrap().is_empty(),
            "failed invalidation rolls back consumer output mutation"
        );
    }
}
