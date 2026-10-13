//! Bounded positioned JSONL framing. Producers retain/check source identity and choose their own
//! JSON/UTF-8 acceptance policy; these helpers preserve raw ranges, Unicode trim and partial EOF.
use anyhow::{Context, Result};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
};

/// Read one captured extent without changing a shared descriptor's seek position.
pub(crate) fn reader(file: &fs::File, range: Range) -> Result<BufReader<Reader<'_>>> {
    let end = range.offset.checked_add(range.len).context("JSONL range overflow")?;
    Ok(BufReader::with_capacity(
        64 * 1024,
        Reader {
            file,
            at: range.offset,
            end,
        },
    ))
}
/// Each completed range excludes LF; CR and Unicode whitespace remain until trim. A partial final
/// row is emitted with complete=false. On read error the consumer may stop or retry from offset().
pub(crate) fn records(file: &fs::File, range: Range) -> Result<Records<'_>> {
    Ok(Records {
        reader: reader(file, range)?,
        at: range.offset,
        small: Vec::new(),
        small_limit: 0,
        small_complete: false,
    })
}

/// Match the buffered driver's Unicode trim. Only boundary windows are decoded, even when
/// a record is a giant string or consists entirely of whitespace.
pub(crate) fn trim(file: &fs::File, mut range: Range) -> Result<Range> {
    range.offset.checked_add(range.len).context("JSONL range overflow")?;
    let mut window = [0; 64 * 1024];
    while range.len > 0 {
        let n = range.len.min(window.len() as u64) as usize;
        read_exact_at(file, &mut window[..n], range.offset)?;
        let end = match std::str::from_utf8(&window[..n]) {
            Ok(_) => n,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            // Invalid UTF-8 cannot be trimmed into an accepted record.
            Err(_) => return Ok(range),
        };
        let text = std::str::from_utf8(&window[..end]).unwrap();
        let leading = text.len() - text.trim_start().len();
        range.offset += leading as u64;
        range.len -= leading as u64;
        if leading != end || end == 0 {
            break;
        }
    }
    while range.len > 0 {
        let n = range.len.min(window.len() as u64) as usize;
        read_exact_at(file, &mut window[..n], range.offset + range.len - n as u64)?;
        // The tail window can start inside a UTF-8 scalar; its trailing end cannot.
        let start = window[..n].iter().take_while(|byte| **byte & 0xc0 == 0x80).count();
        let Ok(text) = std::str::from_utf8(&window[start..n]) else {
            return Ok(range);
        };
        let trailing = text.len() - text.trim_end().len();
        range.len -= trailing as u64;
        if trailing != text.len() || text.is_empty() {
            break;
        }
    }
    Ok(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positioned_frames_match_buffered_lines_and_trim_boundary_windows() {
        let world = cv_testkit::World::new("positioned-jsonl-frames");
        let text = format!(
            "{{\"n\":1}}\r\n\n{}{{\"body\":\"{}\"}}{}\n{}\n final 🌙 ",
            "\u{2003}".repeat(30000),
            "x\\n".repeat(30000),
            "\u{2002}".repeat(30000),
            "\u{2003}".repeat(30000),
        );
        let path = world.base.join("rows.jsonl");
        std::fs::write(&path, &text).unwrap();
        let file = fs::File::open(path).unwrap();
        let range = Range {
            offset: 0,
            len: text.len() as u64,
        };
        let mut framed = records(&file, range).unwrap();
        let mut at = 0;
        for row in text.split_inclusive('\n') {
            let (raw, complete) = framed.next().unwrap().unwrap();
            let complete_expected = row.ends_with('\n');
            assert_eq!(
                (raw.offset, raw.len, complete),
                (
                    at,
                    (row.len() - usize::from(complete_expected)) as u64,
                    complete_expected
                )
            );
            at += row.len() as u64;
            assert_eq!(framed.offset(), at);
            let trimmed = trim(&file, raw).unwrap();
            let mut actual = String::new();
            reader(&file, trimmed).unwrap().read_to_string(&mut actual).unwrap();
            assert_eq!(actual, row.trim());
        }
        assert!(framed.next().is_none());
        assert!(reader(
            &file,
            Range {
                offset: u64::MAX,
                len: 1
            }
        )
        .is_err());
        assert!(trim(
            &file,
            Range {
                offset: u64::MAX,
                len: 1
            }
        )
        .is_err());
        let suffix = Range {
            offset: 10,
            len: range.len - 10,
        };
        assert_eq!(records(&file, suffix).unwrap().next().unwrap().unwrap().0.offset, 10);
    }

    #[test]
    fn framing_leaves_utf8_and_json_rejection_to_the_consumer() {
        let world = cv_testkit::World::new("positioned-jsonl-invalid");
        let path = world.base.join("rows.jsonl");
        let bytes = b"\xff\nnot json\n{\"ok\":1}";
        std::fs::write(&path, bytes).unwrap();
        let file = fs::File::open(path).unwrap();
        let rows = records(
            &file,
            Range {
                offset: 0,
                len: bytes.len() as u64,
            },
        )
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].0.offset, rows[0].0.len, rows[0].1), (0, 1, true));
        assert_eq!((rows[2].0.offset, rows[2].0.len, rows[2].1), (11, 8, false));
        let invalid = trim(&file, rows[0].0).unwrap();
        assert_eq!((invalid.offset, invalid.len), (0, 1));
    }
    #[cfg(not(feature = "mmap"))]
    #[test]
    fn optional_small_records_drop_giant_bytes_and_resume_after_window_boundaries() {
        let world = cv_testkit::World::in_process("positioned-small-record-cache");
        let path = world.base.join("rows.jsonl");
        let bytes = format!("{}\n{}\nsmall\r\npartial", "a".repeat(65535), "b".repeat(65537));
        fs::write(&path, &bytes).unwrap();
        let file = fs::File::open(path).unwrap();
        let mut rows = records(
            &file,
            Range {
                offset: 0,
                len: bytes.len() as u64,
            },
        )
        .unwrap();
        rows.keep_small_records(65536);
        for expected in bytes.split_inclusive('\n') {
            let complete = expected.ends_with('\n');
            let body = expected.strip_suffix('\n').unwrap_or(expected);
            let (range, got_complete) = rows.next().unwrap().unwrap();
            assert_eq!(range.len, body.len() as u64);
            assert_eq!(got_complete, complete);
            assert_eq!(rows.small_record(), (body.len() <= 65536).then_some(body.as_bytes()));
            assert!(rows.small.capacity() <= 65536);
        }
        assert!(rows.next().is_none());
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Range {
    pub offset: u64,
    pub len: u64,
}
pub(crate) struct Reader<'a> {
    file: &'a fs::File,
    at: u64,
    end: u64,
}
impl Read for Reader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = ((self.end - self.at).min(bytes.len() as u64)) as usize;
        let n = loop {
            match read_at(self.file, &mut bytes[..n], self.at) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        self.at += n as u64;
        Ok(n)
    }
}
fn read_at(file: &fs::File, bytes: &mut [u8], at: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(bytes, at)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(bytes, at)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut file = file.try_clone()?;
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(at))?;
        file.read(bytes)
    }
}
pub(crate) fn read_exact_at(file: &fs::File, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        match read_at(file, bytes, at) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                at += n as u64;
                bytes = &mut bytes[n..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
pub(crate) struct Records<'a> {
    reader: BufReader<Reader<'a>>,
    at: u64,
    small: Vec<u8>,
    small_limit: usize,
    small_complete: bool,
}
impl Records<'_> {
    /// Optionally retain small physical records during framing, avoiding a second positioned
    /// read. Above this fixed cap the scratch is cleared; giant records remain only ranges.
    #[cfg(not(feature = "mmap"))]
    pub(crate) fn keep_small_records(&mut self, limit: usize) {
        self.small = Vec::with_capacity(limit);
        self.small_limit = limit;
    }
    #[cfg(not(feature = "mmap"))]
    pub(crate) fn small_record(&self) -> Option<&[u8]> {
        self.small_complete.then_some(&self.small)
    }
    pub(crate) fn offset(&self) -> u64 {
        self.at
    }
}
impl Iterator for Records<'_> {
    type Item = Result<(Range, bool)>;
    fn next(&mut self) -> Option<Self::Item> {
        let start = self.at;
        self.small.clear();
        self.small_complete = self.small_limit > 0;
        loop {
            let bytes = match self.reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) => return Some(Err(error.into())),
            };
            if bytes.is_empty() {
                return (self.at > start).then_some(Ok((
                    Range {
                        offset: start,
                        len: self.at - start,
                    },
                    false,
                )));
            }
            let newline = memchr::memchr(b'\n', bytes);
            let n = newline.map_or(bytes.len(), |i| i + 1);
            if self.small_complete {
                let payload = n - usize::from(newline.is_some());
                if payload <= self.small_limit - self.small.len() {
                    self.small.extend_from_slice(&bytes[..payload]);
                } else {
                    self.small.clear();
                    self.small_complete = false;
                }
            }
            self.at += n as u64;
            self.reader.consume(n);
            if newline.is_some() {
                return Some(Ok((
                    Range {
                        offset: start,
                        len: self.at - start - 1,
                    },
                    true,
                )));
            }
        }
    }
}
