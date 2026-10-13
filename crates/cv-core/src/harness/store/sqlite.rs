//! SQLite plumbing for the database-backed stores (Cursor, Devin, Goose, Hermes, OpenClaw,
//! OpenCode, Zed). One invariant lives here so no adapter can get it wrong: **cv opens a harness's
//! database read-only** (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`) and never takes a write
//! lock on a live store — a WAL-mode database being written by its harness stays readable.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::Path;

/// A reader retains both its private backing (when needed) and the original provenance.
/// Use `read` around an adapter operation, and carry `reads()` into lazy source captures.
pub(crate) struct ReadConnection {
    connection: Connection,
    reads: crate::source::ReadSet,
    _snapshot: Option<std::sync::Arc<ColdSnapshot>>,
    _scope: ReadScope,
}

impl std::ops::Deref for ReadConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl ReadConnection {
    pub(crate) fn reads(&self) -> &crate::source::ReadSet {
        &self.reads
    }

    pub(crate) fn read<T>(&self, operation: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        check_reads(&self.reads)?;
        let result = operation(self);
        // A mutation takes precedence over a body/SQL failure too.
        check_reads(&self.reads)?;
        result
    }
}

/// Retain at an operation boundary that reads multiple sessions. Dropping the final scope or
/// reader releases the one cached backing, including its directory; no process-global files
/// survive idle operations. Concurrent/nested scopes share this bounded cache.
pub(crate) struct ReadScope {
    cache: std::sync::Arc<SnapshotCache>,
}

struct SnapshotCache(std::sync::Mutex<Option<std::sync::Arc<ColdSnapshot>>>);

pub(crate) fn read_scope() -> ReadScope {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static ACTIVE: OnceLock<Mutex<Weak<SnapshotCache>>> = OnceLock::new();
    let mut active = ACTIVE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = active.upgrade().unwrap_or_else(|| {
        let cache = Arc::new(SnapshotCache(Mutex::new(None)));
        *active = Arc::downgrade(&cache);
        cache
    });
    ReadScope { cache }
}

struct ColdSnapshot {
    original: std::path::PathBuf,
    reads: crate::source::ReadSet,
    directory: std::path::PathBuf,
    database: std::path::PathBuf,
}

impl Drop for ColdSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn check_reads(reads: &crate::source::ReadSet) -> Result<()> {
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "SQLite source dependencies changed or unavailable"
    );
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

/// SQLite may create WAL/SHM even with READ_ONLY. Missing WAL auxiliaries therefore use a
/// checked private copy; existing live auxiliaries keep ordinary SQLite locking/read semantics.
/// One cached snapshot avoids copying a cold multi-session database once per session. This is
/// bounded to one database per active operation, with readers retaining snapshot lifetime.
pub(crate) fn open_ro(path: &Path) -> Result<ReadConnection> {
    open_ro_with_copy_check(path, |_| Ok(()))
}

fn open_ro_with_copy_check(path: &Path, copied: impl FnOnce(&Path) -> Result<()>) -> Result<ReadConnection> {
    use std::io::Read;
    use std::sync::Arc;
    let scope = read_scope();
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut reads = crate::source::ReadSet::default();
    reads.record(&path);
    reads.record(&sidecar(&path, "-wal"));
    reads.record(&sidecar(&path, "-journal"));
    let mut header = [0; 20];
    match std::fs::File::open(&path)?.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => header.fill(0),
        Err(error) => return Err(error.into()),
    }
    check_reads(&reads)?;
    let cold = &header[..16] == b"SQLite format 3\0"
        && (header[18] == 2 || header[19] == 2)
        && (!sidecar(&path, "-wal").exists() || !sidecar(&path, "-shm").exists());
    let snapshot = if cold {
        let mut cache = scope
            .cache
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot cache poisoned"))?;
        if let Some(snapshot) = cache
            .as_ref()
            .filter(|s| s.original == path && s.reads.fresh(&mut crate::source::Probe::default()))
        {
            Some(Arc::clone(snapshot))
        } else {
            let directory = std::env::temp_dir().join(format!("cv-sqlite-snapshot-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory)?;
            let snapshot = ColdSnapshot {
                original: path.clone(),
                reads: reads.clone(),
                database: directory.join("store.db"),
                directory,
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&snapshot.directory, std::fs::Permissions::from_mode(0o700))?;
            }
            // std::fs::copy uses clonefile on APFS and bounded OS copying elsewhere. Never copy
            // SHM: it contains live coordination state; SQLite rebuilds it only in our directory.
            std::fs::copy(&path, &snapshot.database)?;
            for suffix in ["-wal", "-journal"] {
                let original = sidecar(&path, suffix);
                if original.exists() {
                    std::fs::copy(original, sidecar(&snapshot.database, suffix))?;
                }
            }
            copied(&snapshot.database)?;
            check_reads(&reads)?;
            let snapshot = Arc::new(snapshot);
            *cache = Some(Arc::clone(&snapshot));
            Some(snapshot)
        }
    } else {
        None
    };
    let backing = snapshot.as_ref().map_or(path.as_path(), |s| s.database.as_path());
    let connection = Connection::open_with_flags(
        backing,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    check_reads(&reads)?;
    Ok(ReadConnection {
        connection,
        reads,
        _snapshot: snapshot,
        _scope: scope,
    })
}

/// Copy a snapshot's immutable TEXT/BLOB cell into an operation-owned arena using bounded
/// scratch. Table and column are adapter-owned identifiers, never user selectors. Blob access
/// may be unavailable for virtual tables; the fallback retains one encoded cell in SQLite.
pub(crate) fn append_cell(
    conn: &Connection,
    table: &str,
    column: &str,
    rowid: i64,
    arena: &super::bytes::ByteSource,
) -> Result<(u64, u64)> {
    if let Ok(mut blob) = conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, true) {
        return arena.append(|writer| {
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = std::io::Read::read(&mut blob, &mut buffer)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buffer[..read])?;
            }
            Ok(())
        });
    }
    conn.query_row(
        &format!("SELECT {column} FROM {table} WHERE rowid = ?1"),
        [rowid],
        |row| {
            Ok(arena.append(|writer| {
                match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                        writer.write_all(bytes)?
                    }
                    _ => anyhow::bail!("SQLite source cell is not TEXT or BLOB"),
                }
                Ok(())
            }))
        },
    )?
}

/// The columns present on `table` (`PRAGMA table_info`). Every SQLite-backed harness grows its
/// schema by `ALTER TABLE` across versions, so an adapter probes before it selects; an unknown
/// table yields the empty set.
pub(crate) fn columns(conn: &Connection, table: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            set.extend(rows.flatten());
        }
    }
    set
}

/// Whether the database has a table named `name`.
pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    static SNAPSHOT_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fixture() -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!("cv-cold-wal-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("sessions.db");
        (directory, database)
    }

    #[test]
    fn cold_wal_is_private_and_reused() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        {
            let writer = Connection::open(&database).unwrap();
            writer
                .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(7);")
                .unwrap();
        }
        assert!(!sidecar(&database, "-wal").exists());
        assert!(!sidecar(&database, "-shm").exists());
        let first = open_ro(&database).unwrap();
        assert_eq!(
            first
                .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            7
        );
        let second = open_ro(&database).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            first._snapshot.as_ref().unwrap(),
            second._snapshot.as_ref().unwrap()
        ));
        assert!(!sidecar(&database, "-wal").exists());
        assert!(!sidecar(&database, "-shm").exists());
        assert!(second.execute("INSERT INTO t VALUES(8)", []).is_err());
        let private_directory = first._snapshot.as_ref().unwrap().directory.clone();
        let scope = read_scope();
        drop(first);
        drop(second);
        for _ in 0..32 {
            let reader = open_ro(&database).unwrap();
            assert_eq!(reader._snapshot.as_ref().unwrap().directory, private_directory);
            reader
                .read(|r| Ok(r.query_row("SELECT x FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap();
        }
        assert!(private_directory.exists());
        drop(scope);
        assert!(!private_directory.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copy_race_rejects_source_change_and_removes_private_backing() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        {
            let writer = Connection::open(&database).unwrap();
            writer
                .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x);")
                .unwrap();
        }
        let mut private = None;
        let result = open_ro_with_copy_check(&database, |copied| {
            private = copied.parent().map(Path::to_path_buf);
            let writer = Connection::open(&database)?;
            writer.execute("INSERT INTO t VALUES(9)", [])?;
            Ok(())
        });
        assert!(result.err().unwrap().to_string().contains("dependencies changed"));
        assert!(!private.unwrap().exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_shm_snapshot_preserves_wal_only_rows_and_rejects_mutation() {
        let _serial = SNAPSHOT_TEST.lock().unwrap();
        let (directory, database) = fixture();
        let writer = Connection::open(&database).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x); PRAGMA wal_checkpoint(TRUNCATE); INSERT INTO t VALUES(7);").unwrap();
        // Simulate a retained/crashed WAL without its coordination sidecar. The writer remains
        // alive solely to demonstrate why immutable=1 would incorrectly omit this committed row.
        std::fs::remove_file(sidecar(&database, "-shm")).unwrap();
        let reader = open_ro(&database).unwrap();
        assert_eq!(
            reader
                .read(|r| Ok(r.query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))?))
                .unwrap(),
            1
        );
        assert!(!sidecar(&database, "-shm").exists());
        let immutable = Connection::open_with_flags(
            format!("file:{}?immutable=1", database.display()),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert_eq!(
            immutable
                .query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(immutable);
        let failure = reader
            .read::<()>(|_| {
                writer.execute("INSERT INTO t VALUES(8)", [])?;
                anyhow::bail!("body failure")
            })
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"));
        assert!(crate::json::JsonSource::capture_with_reads(&database, reader.reads()).is_err());
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn append_text_and_blob_cells_share_one_backing() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE payload(data); INSERT INTO payload VALUES('text'); INSERT INTO payload VALUES(X'626C6F62'); INSERT INTO payload VALUES(NULL);").unwrap();
        let arena = super::super::bytes::ByteSource::arena().unwrap();
        assert_eq!(append_cell(&connection, "payload", "data", 1, &arena).unwrap(), (0, 4));
        assert_eq!(append_cell(&connection, "payload", "data", 2, &arena).unwrap(), (4, 4));
        assert!(append_cell(&connection, "payload", "data", 3, &arena).is_err());
        let mut output = Vec::new();
        arena.copy_to(&mut output).unwrap();
        assert_eq!(output, b"textblob");
    }

    #[test]
    fn probes_and_read_only_open() {
        let dir = std::env::temp_dir().join(format!("cv-store-sqlite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE sessions (id TEXT, title TEXT); INSERT INTO sessions VALUES ('a', 't');")
                .unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert!(table_exists(&ro, "sessions"));
        assert!(!table_exists(&ro, "messages"));
        let cols = columns(&ro, "sessions");
        assert!(cols.contains("id") && cols.contains("title") && cols.len() == 2);
        assert!(columns(&ro, "nope").is_empty());
        // Read-only means a write is refused, not performed.
        assert!(ro.execute("INSERT INTO sessions VALUES ('b', 'u')", []).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
