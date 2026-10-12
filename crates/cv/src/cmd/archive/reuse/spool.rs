//! Message frames and canonical artifact prefix ranges. JSON is serialized by serde; framing
//! only uses ASCII RS, which cannot occur unescaped in valid JSON. Backing storage is shared.
use super::*;
use cv_core::bytes::{ByteSource, ByteStage};
use serde_json::ser::{Formatter, PrettyFormatter};
use std::cell::{Cell, RefCell};
use std::io::{self, Seek, SeekFrom};
use std::rc::Rc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Prefix {
    pub start: u64,
    pub end: u64,
    pub messages: usize,
}
impl Prefix {
    pub fn valid(&self, bytes: u64, messages: usize) -> bool {
        self.messages == messages && messages > 0 && self.start < self.end && self.end <= bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FirstUser {
    pub ordinal: usize,
    pub head: Option<String>,
}

pub(super) struct Spool {
    stage: Option<Counted<BufWriter<ByteStage>>>,
    resolver: cv_core::Resolver,
    messages: usize,
    error: Option<anyhow::Error>,
    first_user: Option<FirstUser>,
    written: Rc<Cell<u64>>,
}
impl Spool {
    pub fn new(r: &SessionRef) -> Self {
        let written = Rc::new(Cell::new(0));
        Self {
            stage: Some(Counted {
                inner: BufWriter::with_capacity(64 * 1024, ByteStage::default()),
                position: written.clone(),
            }),
            resolver: cv_core::Resolver::new(Some(r.path.clone())),
            messages: 0,
            error: None,
            first_user: None,
            written,
        }
    }
    pub fn finish(mut self) -> Result<(ByteSource, usize, Option<FirstUser>, u64)> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        let stage = self
            .stage
            .take()
            .unwrap()
            .inner
            .into_inner()
            .map_err(|error| error.into_error())?;
        Ok((stage.finish()?, self.messages, self.first_user, self.written.get()))
    }
}
impl cv_core::MessageSink for Spool {
    fn meta(&mut self, session: &Session) {
        self.resolver = session.resolver();
    }
    fn message(&mut self, message: cv_core::Message) -> cv_core::Flow {
        if self.error.is_some() {
            return cv_core::Flow::Stop;
        }
        let result = (|| -> Result<()> {
            if self.first_user.is_none() && message.role == cv_core::Role::User {
                let mut head = String::new();
                let mut chars = 0;
                let has_text = Cell::new(false);
                let nonwhite = Cell::new(false);
                let mut add = |text: &str| {
                    has_text.set(has_text.get() || !text.is_empty());
                    nonwhite.set(nonwhite.get() || text.chars().any(|c| !c.is_whitespace()));
                    if chars < 73 {
                        for c in text.chars().take(73 - chars) {
                            head.push(c);
                            chars += 1;
                        }
                    }
                };
                for block in &message.content {
                    if let cv_core::Block::Text { text } = block {
                        // Message::text inserts a separator after any preceding nonempty block,
                        // including before a later empty block. Preserve that exact label rule.
                        let separator = match text {
                            cv_core::Text::Inline(_) | cv_core::Text::Span(_) => has_text.get(),
                        };
                        if separator {
                            add("\n");
                        }
                        match text {
                            cv_core::Text::Inline(text) => add(text),
                            cv_core::Text::Span(span) => self.resolver.try_for_each_chunk(span, 64 * 1024, |text| {
                                add(text);
                                Ok(())
                            })?,
                        }
                    }
                }
                self.first_user = Some(FirstUser {
                    ordinal: self.messages + 1,
                    head: nonwhite.get().then_some(head),
                });
            }
            let stage = self.stage.as_mut().unwrap();
            stage.write_all(&[0x1e])?;
            message.write_json_pretty(stage, &self.resolver)?;
            self.messages += 1;
            Ok(())
        })();
        match result {
            Ok(()) => cv_core::Flow::Continue,
            Err(error) => {
                self.error = Some(error);
                cv_core::Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }
}

struct Counted<W> {
    inner: W,
    position: Rc<Cell<u64>>,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.position.set(
            self.position
                .get()
                .checked_add(n as u64)
                .ok_or_else(|| io::Error::other("archive output length overflow"))?,
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
                        return Err(io::Error::other("archive spool has no frame boundary"));
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
            return Err(io::Error::other("archive spool message count mismatch"));
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
                return Err(io::Error::other("duplicate archive messages array"));
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

pub(super) fn write_session(
    writer: &mut impl Write,
    session: &Session,
    spool: &ByteSource,
    old: Option<(&Path, &Prefix)>,
    stable: Option<usize>,
    total: usize,
) -> Result<Option<Prefix>> {
    anyhow::ensure!(
        session.messages.is_empty(),
        "archive header unexpectedly retained messages"
    );
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
        .context("archive Session serializer omitted messages")?;
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::{Block, Flow, Message, MessageSink, Role};

    fn stage(session: &Session) -> (ByteSource, Option<FirstUser>) {
        let r = SessionRef::new(&session.id, session.harness, PathBuf::from("unused"));
        let mut spool = Spool::new(&r);
        spool.meta(session);
        for message in &session.messages {
            assert_eq!(spool.message(message.clone()), Flow::Continue);
        }
        let (bytes, count, first, _) = spool.finish().unwrap();
        assert_eq!(count, session.messages.len());
        (bytes, first)
    }
    fn message(text: &str) -> Message {
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text { text: text.into() });
        message
    }

    #[test]
    fn derived_pretty_output_and_relocated_prefix_are_exact() {
        let root = std::env::temp_dir().join(format!("cv-archive-frame-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let mut session = Session::new("framing", Harness::Claude);
        session.title = Some("短い\n\"title\"".into());
        session
            .extra
            .insert("messages".into(), serde_json::json!({"messages":[null,{},["x"]]}));
        session.messages = vec![
            message(""),
            message("Unicode é🦀\n\u{1e}\\\""),
            message(&"wide\n".repeat(20_000)),
        ];
        for stable in 0..=session.messages.len() {
            let (bytes, first) = stage(&session);
            assert!(
                first.unwrap().head.is_none(),
                "first empty user remains the label source"
            );
            let mut header = session.clone();
            header.messages.clear();
            let mut full = Vec::new();
            let prefix = write_session(&mut full, &header, &bytes, None, Some(stable), session.messages.len()).unwrap();
            assert_eq!(full, serde_json::to_vec_pretty(&session).unwrap());
            if stable == 0 {
                assert!(prefix.is_none());
                continue;
            }
            let path = root.join("old.json");
            fs::write(&path, &full).unwrap();
            let mut suffix = session.clone();
            suffix.messages = session.messages[stable..].to_vec();
            suffix.messages.push(message("appended"));
            let (bytes, _) = stage(&suffix);
            let mut expected = session.clone();
            expected.title = Some("a much wider final header changes the archive prefix byte offset".into());
            expected.messages.push(message("appended"));
            let mut header = expected.clone();
            header.messages.clear();
            let mut output = Vec::new();
            let moved = write_session(
                &mut output,
                &header,
                &bytes,
                Some((&path, &prefix.unwrap())),
                Some(expected.messages.len()),
                expected.messages.len(),
            )
            .unwrap()
            .unwrap();
            assert!(moved.valid(output.len() as u64, expected.messages.len()));
            assert_eq!(output, serde_json::to_vec_pretty(&expected).unwrap());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frame_counts_and_header_retention_fail_closed() {
        let mut session = Session::new("framing", Harness::Claude);
        session.messages.push(message("one"));
        let (bytes, _) = stage(&session);
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, None, 1).is_err());
        session.messages.clear();
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, None, 2).is_err());
        assert!(write_session(&mut Vec::new(), &session, &bytes, None, Some(2), 1).is_err());
        let (empty, _) = stage(&session);
        let mut output = Vec::new();
        assert!(write_session(&mut output, &session, &empty, None, None, 0)
            .unwrap()
            .is_none());
        assert_eq!(output, serde_json::to_vec_pretty(&session).unwrap());
    }

    #[test]
    fn bounded_label_matches_first_user_text_and_unicode_truncation() {
        for text in [
            "".into(),
            " \n\t".into(),
            "🦀".repeat(80),
            format!("{}x", " ".repeat(100)),
        ] {
            let mut session = Session::new("label", Harness::Claude);
            session.messages = vec![message(&text), message("later user")];
            session.messages[0].content.push(Block::Text { text: "".into() });
            let (_, first) = stage(&session);
            assert_eq!(
                cv_core::label_from(None, first.unwrap().head.as_deref()),
                session.label()
            );
        }
    }
}
