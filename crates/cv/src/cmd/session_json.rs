//! Shared bounded pretty Session framing for archive and CLI output.
use anyhow::{Context, Result};
use cv_core::{bytes::ByteSource, Session};
use serde::{Deserialize, Serialize};
use serde_json::ser::{Formatter, PrettyFormatter};
use std::{
    cell::{Cell, RefCell},
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    rc::Rc,
};
/// RS cannot occur unescaped in JSON; each independently pretty message is one frame.
pub(crate) fn write_message_frame(
    writer: &mut impl Write,
    message: &cv_core::Message,
    resolver: &cv_core::Resolver,
) -> Result<()> {
    writer.write_all(&[0x1e])?;
    message.write_json_pretty(writer, resolver)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Prefix {
    pub start: u64,
    pub end: u64,
    pub messages: usize,
}
impl Prefix {
    pub fn valid(&self, bytes: u64, messages: usize) -> bool {
        self.messages == messages && messages > 0 && self.start < self.end && self.end <= bytes
    }
}

pub(crate) struct Counted<W> {
    pub(crate) inner: W,
    pub(crate) position: Rc<Cell<u64>>,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.position.set(
            self.position
                .get()
                .checked_add(n as u64)
                .ok_or_else(|| io::Error::other("JSON output length overflow"))?,
        );
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Preserve Session's derived field order/omission rules and serde's pretty output. Only the
/// known root messages array is replaced with already serialized values; no JSON schema parser.
struct Messages<'a> {
    pretty: PrettyFormatter<'static>,
    depth: usize,
    key: Vec<u8>,
    reading_key: bool,
    messages_key: bool,
    injected: bool,
    spool: Box<dyn Read + 'a>,
    old: Option<(&'a Path, &'a Prefix)>,
    position: Rc<Cell<u64>>,
    count: usize,
    total: usize,
    stable: Option<usize>,
    start: Option<u64>,
    prefix: Option<Prefix>,
    result: Rc<RefCell<Option<Option<Prefix>>>>,
}
impl Messages<'_> {
    fn record_end(&mut self) {
        if self.stable == Some(self.count) {
            self.prefix = self.start.map(|start| Prefix {
                start,
                end: self.position.get(),
                messages: self.count,
            });
        }
    }
    fn begin_message<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.pretty.begin_array_value(writer, self.count == 0)?;
        self.start.get_or_insert(self.position.get());
        Ok(())
    }
    fn end_message<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.count += 1;
        self.record_end();
        self.pretty.end_array_value(writer)
    }
    fn inject<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if let Some((path, prefix)) = self.old {
            self.begin_message(writer)?;
            let mut old = fs::File::open(path)?;
            old.seek(SeekFrom::Start(prefix.start))?;
            let expected = prefix.end - prefix.start;
            let copied = io::copy(&mut old.take(expected), writer)?;
            if copied != expected {
                return Err(io::Error::other("archive stable prefix truncated"));
            }
            self.count = prefix.messages;
            self.record_end();
            self.pretty.end_array_value(writer)?;
        }
        let mut inside = false;
        let mut bytes = [0u8; 64 * 1024];
        loop {
            let n = self.spool.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            for piece in bytes[..n].split_inclusive(|byte| *byte == 0x1e) {
                let delimiter = piece.last() == Some(&0x1e);
                let body = &piece[..piece.len() - usize::from(delimiter)];
                if !body.is_empty() {
                    if !inside {
                        return Err(io::Error::other("JSON spool has no frame boundary"));
                    }
                    // Standalone Message pretty indentation gains the enclosing array's four
                    // spaces at every physical newline. JSON strings contain escaped newlines.
                    for line in body.split_inclusive(|byte| *byte == b'\n') {
                        writer.write_all(line)?;
                        if line.last() == Some(&b'\n') {
                            writer.write_all(b"    ")?;
                        }
                    }
                }
                if delimiter {
                    if inside {
                        self.end_message(writer)?;
                    }
                    self.begin_message(writer)?;
                    inside = true;
                }
            }
        }
        if inside {
            self.end_message(writer)?;
        }
        if self.count != self.total {
            return Err(io::Error::other("JSON spool message count mismatch"));
        }
        if self.stable.is_some_and(|stable| stable > 0 && self.prefix.is_none()) {
            return Err(io::Error::other("archive checkpoint has no serialized prefix"));
        }
        *self.result.borrow_mut() = Some(self.prefix.clone());
        Ok(())
    }
}
impl Formatter for Messages<'_> {
    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        let inject = self.depth == 1 && self.messages_key;
        self.depth += 1;
        self.pretty.begin_array(w)?;
        if inject {
            if self.injected {
                return Err(io::Error::other("duplicate Session messages array"));
            }
            self.injected = true;
            self.inject(w)?;
        }
        Ok(())
    }
    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        self.pretty.end_array(w)
    }
    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.pretty.begin_object(w)
    }
    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        self.pretty.end_object(w)
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.reading_key = self.depth == 1;
        self.key.clear();
        self.pretty.begin_object_key(w, first)
    }
    fn write_string_fragment<W: ?Sized + Write>(&mut self, w: &mut W, s: &str) -> io::Result<()> {
        if self.reading_key {
            if self.key.len().saturating_add(s.len()) <= 8 {
                self.key.extend_from_slice(s.as_bytes());
            } else {
                self.reading_key = false;
            }
        }
        self.pretty.write_string_fragment(w, s)
    }
    fn end_object_key<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.messages_key = self.reading_key && self.key == b"messages";
        self.reading_key = false;
        self.pretty.end_object_key(w)
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_object_value(w)
    }
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.pretty.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.pretty.end_array_value(w)
    }
}

pub(crate) fn write_session(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    write_framed(writer, session, spool, old, stable, total)
}

pub(crate) fn write_document(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    total: usize,
    document: bool,
) -> Result<()> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "JSON header unexpectedly retained messages"
    );
    if document {
        write_framed(
            writer,
            &cv_core::harness::opensession::document(session),
            spool,
            None,
            None,
            total,
        )?;
    } else {
        write_framed(writer, session, spool, None, None, total)?;
    }
    Ok(())
}

fn write_framed(
    writer: &mut impl Write,
    session: &impl Serialize,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    let position = Rc::new(Cell::new(0));
    let result = Rc::new(RefCell::new(None));
    let formatter = Messages {
        pretty: PrettyFormatter::new(),
        depth: 0,
        key: Vec::with_capacity(8),
        reading_key: false,
        messages_key: false,
        injected: false,
        spool: spool.reader()?,
        old,
        position: position.clone(),
        count: 0,
        total,
        stable,
        start: None,
        prefix: None,
        result: result.clone(),
    };
    let mut counted = Counted {
        inner: writer,
        position,
    };
    let mut serializer = serde_json::Serializer::with_formatter(&mut counted, formatter);
    session.serialize(&mut serializer)?;
    drop(serializer);
    let prefix = result
        .borrow_mut()
        .take()
        .context("Session serializer omitted messages")?;
    Ok(prefix)
}
