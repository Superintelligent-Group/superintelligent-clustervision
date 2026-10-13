//! Bounded text projections shared by search and API consumers.
mod json_frames;
mod json_string;
mod string;
use crate::{Resolver, Text};
pub use json_frames::write_json_array;
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
    pub fn push_text(&mut self, text: &Text, resolver: &Resolver) {
        if matches!(text, Text::Source(_)) {
            let _ = self.try_push_text(text, resolver);
            return;
        }
        if self.full() {
            return;
        }
        // Preserve the existing raw-prefix selection for spans. Decoded malformed UTF-8 may
        // expand beyond this raw budget, so still apply the final decoded-byte bound.
        let prefix = text.resolve_prefix(resolver, self.remaining() as u64);
        self.push(&prefix);
    }
    /// Checked text demand with the same raw-prefix selection as push_text. Decoded UTF-8
    /// shares the remaining byte budget; selected raw prefixes keep their existing behavior.
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

/// Exact decoded byte prefix of the searchable message projection, with a truncation witness.
/// Demanded text uses checked chunks; JSON uses the canonical bounded writer. No later block
/// is demanded after the prefix is established.
pub fn searchable_prefix(
    message: &crate::Message,
    resolver: &Resolver,
    limit: usize,
) -> anyhow::Result<(String, bool)> {
    #[derive(Debug)]
    struct PrefixComplete;
    impl std::fmt::Display for PrefixComplete {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("searchable prefix complete")
        }
    }
    impl std::error::Error for PrefixComplete {}
    // Four witness bytes distinguish an exact fit from the next UTF8 scalar not fitting.
    let mut head = Head {
        text: String::with_capacity(limit.saturating_add(4).min(64 * 1024)),
        limit: limit.saturating_add(4),
        stopped: false,
    };
    for block in &message.content {
        if head.full() {
            break;
        }
        match block {
            crate::Block::Text { text }
            | crate::Block::Thinking { text, .. }
            | crate::Block::ToolResult { content: text, .. } => {
                let result = text.try_for_each_chunk(resolver, 4096, |part| {
                    head.push(part);
                    if head.full() {
                        Err(PrefixComplete.into())
                    } else {
                        Ok(())
                    }
                });
                if let Err(error) = result {
                    if !error.is::<PrefixComplete>() {
                        return Err(error);
                    }
                }
                head.push("\n");
            }
            crate::Block::ToolUse { name, input, .. } => {
                head.push(name);
                head.push(" ");
                head.push_json(input)?;
                head.push("\n");
            }
            crate::Block::File { path, source, .. } => {
                if let Some(path) = path.as_deref().or(source.as_deref()) {
                    head.push(path);
                    head.push("\n");
                }
            }
            crate::Block::Image { .. } => {}
        }
    }
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
        for checked in [true, false] {
            let mut head = Head::new(3);
            if checked {
                head.try_push_text(&text, &Resolver::new(None)).unwrap();
            } else {
                head.push_text(&text, &Resolver::new(None));
            }
            head.push("\n");
            assert_eq!(head.as_str(), "ab");
            assert!(head.full());
        }
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
