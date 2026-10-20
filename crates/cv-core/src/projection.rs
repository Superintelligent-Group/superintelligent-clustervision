//! Bounded text projections shared by search and API consumers.
mod json_frames;
mod json_string;
mod string;
use crate::{Resolver, Text};
pub use json_frames::{write_frame, write_frames, write_json_array, Counted, Framing, Prefix};
pub use json_string::{for_each_lossy_utf8_chunk, write_json_string};
pub(crate) use string::{string_prefix, string_until};

/// Append complete canonical JSON into an already demanded string without an intermediate copy.
/// A failure can leave a prefix appended; callers must discard their staged output on error.
pub fn append_json(out: &mut String, json: &crate::json::Json) -> anyhow::Result<()> {
    struct StringWriter<'a>(&'a mut String);
    impl std::io::Write for StringWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .push_str(std::str::from_utf8(bytes).map_err(std::io::Error::other)?);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    json.write_canonical(&mut StringWriter(out))
}

/// A decoded UTF-8 byte prefix. Once a scalar does not fit, later writes cannot skip it.
/// All text, punctuation, paths, names and streamed JSON formatting share this one budget.
pub struct Head {
    text: String,
    limit: usize,
    stopped: bool,
}
impl Head {
    pub fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit),
            limit,
            stopped: false,
        }
    }
    pub fn remaining(&self) -> usize {
        if self.stopped {
            0
        } else {
            self.limit - self.text.len()
        }
    }
    pub fn full(&self) -> bool {
        self.remaining() == 0
    }
    pub fn push(&mut self, text: &str) {
        if self.full() {
            return;
        }
        let mut end = text.len().min(self.remaining());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.stopped |= end < text.len();
    }
    /// Checked text demand: a span's raw prefix of the remaining budget, then the decoded-byte
    /// bound (decoded malformed UTF-8 may expand beyond the raw budget).
    pub fn try_push_text(&mut self, text: &Text, resolver: &Resolver) -> anyhow::Result<()> {
        if !self.full() {
            if let Text::Source(source) = text {
                let (prefix, truncated) = source.try_prefix_with_truncation(self.remaining())?;
                self.push(&prefix);
                self.stopped |= truncated;
                return Ok(());
            }
            let prefix = text.try_resolve_prefix(resolver, self.remaining() as u64)?;
            self.push(&prefix);
        }
        Ok(())
    }
    pub fn push_json(&mut self, json: &crate::json::Json) -> anyhow::Result<()> {
        if !self.full() {
            let (prefix, truncated) = json.canonical_prefix(self.remaining())?;
            self.push(&prefix);
            self.stopped |= truncated;
        }
        Ok(())
    }
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn len(&self) -> usize {
        self.text.len()
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.text.capacity()
    }
    pub fn into_string(self) -> String {
        self.text
    }
}

/// Where [`searchable`] writes: the complete projection, or a bounded head of it.
trait Searchable {
    fn full(&self) -> bool {
        false
    }
    fn str(&mut self, s: &str);
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
    fn json(&mut self, json: &crate::json::Json, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;
}

/// The searchable projection of one message, spelled once: text, thinking and tool-result text
/// each followed by a newline; a tool call as `name input` (canonical JSON) and a newline; a
/// file's path (else its source) and a newline; an image nothing. `check` runs before each
/// block, at each decoded text chunk and JSON write, and once at the end.
fn searchable(
    message: &crate::Message,
    resolver: &Resolver,
    out: &mut impl Searchable,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for block in &message.content {
        check()?;
        if out.full() {
            break;
        }
        match block {
            crate::Block::Text { text }
            | crate::Block::Thinking { text, .. }
            | crate::Block::ToolResult { content: text, .. } => {
                out.text(text, resolver, check)?;
                out.str("\n");
            }
            crate::Block::ToolUse { name, input, .. } => {
                out.str(name);
                out.str(" ");
                out.json(input, check)?;
                out.str("\n");
            }
            crate::Block::File { path, source, .. } => {
                if let Some(path) = path.as_deref().or(source.as_deref()) {
                    out.str(path);
                    out.str("\n");
                }
            }
            crate::Block::Image { .. } => {}
        }
    }
    check()
}

/// The complete projection, as bytes (a JSON write may end inside a scalar; the whole is UTF-8).
impl Searchable for Vec<u8> {
    fn str(&mut self, s: &str) {
        self.extend_from_slice(s.as_bytes());
    }
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        text.try_for_each_chunk(resolver, 4096, |part| {
            check()?;
            self.extend_from_slice(part.as_bytes());
            Ok(())
        })
    }
    fn json(&mut self, json: &crate::json::Json, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        struct Checked<'a, 'b> {
            bytes: &'a mut Vec<u8>,
            check: &'a mut (dyn FnMut() -> anyhow::Result<()> + 'b),
        }
        impl std::io::Write for Checked<'_, '_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                (self.check)().map_err(std::io::Error::other)?;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        json.write_canonical(&mut Checked { bytes: self, check })
    }
}

/// A bounded head: decoding stops once the budget is spent.
impl Searchable for Head {
    fn full(&self) -> bool {
        Head::full(self)
    }
    fn str(&mut self, s: &str) {
        self.push(s);
    }
    fn text(
        &mut self,
        text: &Text,
        resolver: &Resolver,
        check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        #[derive(Debug)]
        struct PrefixComplete;
        impl std::fmt::Display for PrefixComplete {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("searchable prefix complete")
            }
        }
        impl std::error::Error for PrefixComplete {}
        match text.try_for_each_chunk(resolver, 4096, |part| {
            check()?;
            self.push(part);
            if Head::full(self) {
                Err(PrefixComplete.into())
            } else {
                Ok(())
            }
        }) {
            Err(error) if !error.is::<PrefixComplete>() => Err(error),
            _ => Ok(()),
        }
    }
    fn json(&mut self, json: &crate::json::Json, _: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.push_json(json)
    }
}

/// Complete searchable projection with cooperative cancellation between text chunks and
/// canonical JSON writes. The joined output is demanded one message at a time.
pub fn searchable_text_checked(
    message: &crate::Message,
    resolver: &Resolver,
    mut check: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    searchable(message, resolver, &mut bytes, &mut check)?;
    Ok(String::from_utf8(bytes)?)
}

/// Exact decoded byte prefix of the searchable message projection, with a truncation witness.
/// Demanded text uses checked chunks; JSON uses the canonical bounded writer. No later block
/// is demanded after the prefix is established.
pub fn searchable_prefix(
    message: &crate::Message,
    resolver: &Resolver,
    limit: usize,
) -> anyhow::Result<(String, bool)> {
    searchable_prefix_checked(message, resolver, limit, || Ok(()))
}

/// Bounded searchable projection with cooperative checks at block and decoded text chunks.
pub fn searchable_prefix_checked(
    message: &crate::Message,
    resolver: &Resolver,
    limit: usize,
    mut check: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<(String, bool)> {
    // Four witness bytes distinguish an exact fit from the next UTF8 scalar not fitting.
    let mut head = Head {
        text: String::with_capacity(limit.saturating_add(4).min(64 * 1024)),
        limit: limit.saturating_add(4),
        stopped: false,
    };
    searchable(message, resolver, &mut head, &mut check)?;
    let mut text = head.into_string();
    let truncated = text.len() > limit;
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    Ok((text, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_searchable_projection_matches_owned_text_and_interrupts_chunks() {
        let mut message = crate::Message::new(crate::Role::Assistant);
        message.content = vec![
            crate::Block::Text {
                text: "λ ordinary\n".into(),
            },
            crate::Block::Thinking {
                text: "thinking".into(),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            crate::Block::ToolUse {
                id: "call".into(),
                name: "tool".into(),
                input: serde_json::json!({"z":"λ", "a":2}).into(),
                namespace: None,
            },
            crate::Block::ToolResult {
                tool_use_id: "call".into(),
                content: "result".into(),
                is_error: false,
                tool_name: None,
                details: None,
                status: None,
            },
        ];
        let mut expected = String::new();
        message.append_searchable(&mut expected).unwrap();
        assert_eq!(
            searchable_text_checked(&message, &Resolver::new(None), || Ok(())).unwrap(),
            expected
        );
        message.content = vec![crate::Block::Text {
            text: "λ".repeat(32 * 1024).into(),
        }];
        let mut checks = 0;
        let error = searchable_text_checked(&message, &Resolver::new(None), || {
            checks += 1;
            anyhow::ensure!(checks < 4, "cancelled inside payload");
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled inside payload");
        assert_eq!(checks, 4);
    }

    #[test]
    fn searchable_prefix_checks_escaped_span_and_retained_text() {
        let dir = cv_testkit::temp_dir("searchable-escaped-prefix");
        let path = dir.join("text.json");
        let text = "月🌙é\n\"\\".repeat(4000);
        let raw = serde_json::to_vec(&text).unwrap();
        std::fs::write(&path, &raw).unwrap();
        let json = crate::Json::source(&path, 0, raw.len() as u64).unwrap();
        let retained = json.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
        let span = Text::Span(crate::lazy::Span {
            source: Some(path.clone()),
            offset: 1,
            len: raw.len() as u64 - 2,
            escaped: true,
        });
        let resolver = Resolver::new(None);
        for body in [span, retained] {
            let mut message = crate::Message::new(crate::Role::Assistant);
            message.content.push(crate::Block::Text { text: body });
            let oracle = format!("{text}\n");
            for cap in [0, 1, 2, 3, 4, 7, 16000, oracle.len(), oracle.len() + 4] {
                let (actual, truncated) = searchable_prefix(&message, &resolver, cap).unwrap();
                let mut end = oracle.len().min(cap);
                while !oracle.is_char_boundary(end) {
                    end -= 1;
                }
                assert_eq!(actual, oracle[..end]);
                assert_eq!(truncated, oracle.len() > cap);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn searchable_prefix_matches_owned_projection_at_every_unicode_boundary() {
        let mut message = crate::Message::new(crate::Role::Assistant);
        message.content = vec![
            crate::Block::Text {
                text: "月🌙é\n".into()
            },
            crate::Block::ToolUse {
                id: "c".into(),
                name: "tool".into(),
                input: serde_json::json!({"z":"🌙\n\"\\", "a":2}).into(),
                namespace: None,
            },
            crate::Block::ToolResult {
                tool_use_id: "c".into(),
                tool_name: None,
                content: "done月".into(),
                is_error: false,
                details: None,
                status: None,
            },
        ];
        let mut oracle = String::new();
        message.append_searchable(&mut oracle).unwrap();
        for cap in 0..=oracle.len() + 4 {
            let (actual, truncated) = searchable_prefix(&message, &Resolver::new(None), cap).unwrap();
            assert_eq!(
                searchable_prefix_checked(&message, &Resolver::new(None), cap, || Ok(())).unwrap(),
                (actual.clone(), truncated)
            );
            let mut end = oracle.len().min(cap);
            while !oracle.is_char_boundary(end) {
                end -= 1;
            }
            assert_eq!(actual, oracle[..end], "cap={cap}");
            assert_eq!(truncated, oracle.len() > cap);
        }
    }

    #[test]
    fn source_text_prefix_preserves_unicode_cutoff_and_zero_budget() {
        let dir = cv_testkit::temp_dir("source-text-head");
        let path = dir.join("source.json");
        let decoded = format!("abé{}", "x".repeat(20_000));
        std::fs::write(&path, serde_json::to_vec(&decoded).unwrap()).unwrap();
        let value = crate::json::Json::source(&path, 0, std::fs::metadata(&path).unwrap().len()).unwrap();
        let text = value.with_view(|view| Ok(view.source_text()?.unwrap())).unwrap();
        let Text::Source(source) = &text else {
            panic!("retained source")
        };
        assert_eq!(source.try_prefix_with_truncation(0).unwrap(), (String::new(), true));
        assert_eq!(source.try_prefix_with_truncation(3).unwrap(), ("ab".into(), true));
        assert_eq!(
            source.try_prefix_with_truncation(decoded.len()).unwrap(),
            (decoded.clone(), false)
        );
        let mut head = Head::new(3);
        head.try_push_text(&text, &Resolver::new(None)).unwrap();
        head.push("\n");
        assert_eq!(head.as_str(), "ab");
        assert!(head.full());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checked_text_head_propagates_selected_source_failure() {
        let resolver = Resolver::new(None);
        let text = Text::Span(crate::lazy::Span {
            source: None,
            offset: 0,
            len: 20,
            escaped: false,
        });
        let mut head = Head::new(8);
        assert!(head.try_push_text(&text, &resolver).is_err());
        assert!(head.is_empty());
        let mut full = Head::new(0);
        full.try_push_text(&text, &resolver).unwrap();
        let mut inline = Head::new(3);
        inline.try_push_text(&Text::from("abé"), &resolver).unwrap();
        inline.push("z");
        assert_eq!(inline.as_str(), "ab");
    }

    #[test]
    fn prefix_cannot_skip_a_scalar_that_does_not_fit() {
        let mut head = Head::new(3);
        head.push("ab");
        head.push("é");
        head.push("x");
        assert_eq!(head.as_str(), "ab");
        assert!(head.full());
        assert_eq!(head.capacity(), 3);
    }

    #[test]
    fn canonical_json_and_following_text_share_one_prefix_budget() {
        let json = crate::json::Json::from(serde_json::json!({"arg":"é\n\""}));
        let expected = format!("tool {}\ntail", json.resolve().unwrap());
        for cap in 0..=expected.len() {
            let mut head = Head::new(cap);
            head.push("tool ");
            head.push_json(&json).unwrap();
            head.push("\ntail");
            let mut end = cap.min(expected.len());
            while !expected.is_char_boundary(end) {
                end -= 1;
            }
            assert_eq!(head.as_str(), &expected[..end], "cap {cap}");
            assert_eq!(head.capacity(), cap);
        }
    }
}
