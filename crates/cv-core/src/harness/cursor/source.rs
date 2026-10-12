//! SQLite TEXT/BLOB cells are read incrementally under the caller's read snapshot.
use super::*;
use crate::harness::store::bytes::ByteSource;
use std::io::{self, Read};

pub(super) fn cell(conn: &Connection, table: &str, key: &str) -> Result<ByteSource> {
    let row = conn.query_row(
        &format!("SELECT _rowid_, typeof(value) FROM {table} WHERE key = ?1"),
        [key],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    );
    if let Ok((rowid, kind)) = row {
        if let Ok(blob) = conn.blob_open(rusqlite::DatabaseName::Main, table, "value", rowid, true) {
            return if kind == "blob" {
                Ok(ByteSource::read(LossyUtf8::new(blob))?)
            } else {
                Ok(ByteSource::read(blob)?)
            };
        }
    }
    // WITHOUT ROWID/indexed value schemas cannot expose incremental cells. Preserve the old
    // TEXT/BLOB tolerance while bounding the JSON tree; this still retains one native cell.
    let raw = conn
        .query_row(&format!("SELECT value FROM {table} WHERE key = ?1"), [key], |row| {
            row.get::<_, String>(0).or_else(|_| {
                row.get::<_, Vec<u8>>(0)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            })
        })
        .with_context(|| format!("{table} cell {key} not found"))?;
    Ok(ByteSource::read(raw.as_bytes())?)
}

/// Cursor historically replaces invalid UTF-8 in BLOB values before JSON parsing. Chunked
/// conversion preserves exactly String::from_utf8_lossy's replacement groups, including tails.
struct LossyUtf8<R> {
    input: R,
    pending: Vec<u8>,
    output: Vec<u8>,
    position: usize,
    done: bool,
}
impl<R: Read> LossyUtf8<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            pending: Vec::new(),
            output: Vec::new(),
            position: 0,
            done: false,
        }
    }
    fn fill(&mut self) -> io::Result<()> {
        self.output.clear();
        self.position = 0;
        let mut chunk = [0; 64 * 1024];
        let n = self.input.read(&mut chunk)?;
        self.done = n == 0;
        self.pending.extend_from_slice(&chunk[..n]);
        let mut cursor = 0;
        while cursor < self.pending.len() {
            match std::str::from_utf8(&self.pending[cursor..]) {
                Ok(_) => {
                    self.output.extend_from_slice(&self.pending[cursor..]);
                    cursor = self.pending.len();
                }
                Err(error) => {
                    let valid = cursor + error.valid_up_to();
                    self.output.extend_from_slice(&self.pending[cursor..valid]);
                    if let Some(len) = error.error_len() {
                        self.output.extend_from_slice("\u{fffd}".as_bytes());
                        cursor = valid + len;
                    } else if self.done {
                        self.output.extend_from_slice("\u{fffd}".as_bytes());
                        cursor = self.pending.len();
                    } else {
                        cursor = valid;
                        break;
                    }
                }
            }
        }
        self.pending.drain(..cursor);
        Ok(())
    }
}
impl<R: Read> Read for LossyUtf8<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position == self.output.len() && !self.done {
            self.fill()?;
        }
        let n = out.len().min(self.output.len() - self.position);
        out[..n].copy_from_slice(&self.output[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lossy_blob_conversion_matches_whole_string_across_boundaries() {
        for suffix in [
            vec![0xff],
            vec![0xe0, 0xa0],
            vec![0xf0, 0x9f, 0x80, 0x80],
            vec![0xed, 0xa0, 0x80],
            vec![0xf0, 0x80, 0x80, 0x80],
        ] {
            for boundary in 65530..=65538 {
                let mut input = vec![b'x'; boundary];
                input.extend(&suffix);
                input.extend(b"tail");
                let expected = String::from_utf8_lossy(&input);
                let mut actual = String::new();
                LossyUtf8::new(input.as_slice()).read_to_string(&mut actual).unwrap();
                assert_eq!(actual, expected);
            }
        }
    }
    #[test]
    fn native_text_and_lossy_blob_cells_and_without_rowid_fallback() {
        for rowid in [true, false] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value){}",
                if rowid { "" } else { " WITHOUT ROWID" }
            ))
            .unwrap();
            let invalid = b"{\"conversation\":[{\"type\":1,\"text\":\"\xff\"}]}";
            conn.execute("INSERT INTO cursorDiskKV VALUES('blob',?1)", [invalid.as_slice()])
                .unwrap();
            conn.execute("INSERT INTO cursorDiskKV VALUES('text',?1)", ["{\"conversation\":[]}"])
                .unwrap();
            if rowid {
                assert!(
                    conn.blob_open(rusqlite::DatabaseName::Main, "cursorDiskKV", "value", 2, true)
                        .is_ok()
                );
            }
            for key in ["blob", "text"] {
                let source = cell(&conn, "cursorDiskKV", key).unwrap();
                assert!(super::super::composer::header(&source).is_some());
                let mut actual = String::new();
                source.reader().unwrap().read_to_string(&mut actual).unwrap();
                if key == "blob" {
                    assert_eq!(actual, String::from_utf8_lossy(invalid));
                }
            }
        }
    }
}
