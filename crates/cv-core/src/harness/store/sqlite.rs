//! SQLite plumbing for the database-backed stores (Cursor, Devin, Goose, Hermes, OpenClaw,
//! OpenCode, Zed). One invariant lives here so no adapter can get it wrong: **cv opens a harness's
//! database read-only** (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`) and never takes a write
//! lock on a live store — a WAL-mode database being written by its harness stays readable.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::Path;

/// Open `path` read-only, never locking the harness's live database.
pub(crate) fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))
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
