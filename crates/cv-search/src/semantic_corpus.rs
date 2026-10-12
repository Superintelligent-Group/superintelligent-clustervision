//! Bounded semantic body and label projections; no embedding model is needed to test them.
use cv_core::{Block, Flow, Message, MessageSink, Resolver, Role, Text};

const EMBED_HEAD_BYTES: usize = 64 * 1024;
const FIRST_USER_RAW_HEAD: u64 = 512;
const LABEL_CHARS: usize = 73; // label_from reads 72 characters plus one for its ellipsis.

use cv_core::projection::Head;

/// Message::text's joining and non-whitespace test, retaining only the label's required chars.
/// Inline whitespace beyond the head still counts when deciding which user message is first.
#[derive(Default)]
struct UserLabel {
    head: String,
    chars: usize,
    joined_nonempty: bool,
    meaningful: bool,
}
impl UserLabel {
    fn push(&mut self, text: &str) {
        if self.joined_nonempty {
            self.keep("\n");
        }
        self.keep(text);
        self.joined_nonempty |= !text.is_empty();
        self.meaningful |= !text.trim().is_empty();
    }
    fn keep(&mut self, text: &str) {
        for ch in text.chars().take(LABEL_CHARS - self.chars) {
            self.head.push(ch);
            self.chars += 1;
        }
    }
}

pub(super) struct DocSink {
    pub(super) body: Head,
    pub(super) first_user_text: Option<String>,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}
impl DocSink {
    pub(super) fn new(resolver: Resolver) -> Self {
        Self {
            body: Head::new(EMBED_HEAD_BYTES),
            first_user_text: None,
            resolver,
            error: None,
            failed: false,
        }
    }
    /// Message::append_searchable's projection, plus persisted-output heads, within one budget.
    fn append_searchable(&mut self, message: &Message) {
        for block in &message.content {
            if self.body.full() {
                break;
            }
            match block {
                Block::Text { text } | Block::Thinking { text, .. } => {
                    self.body.push_text(text, &self.resolver);
                    self.body.push("\n");
                }
                Block::ToolUse { name, input, .. } => {
                    self.body.push(name);
                    self.body.push(" ");
                    if !self.body.full() {
                        if let Err(error) = self.body.push_json(input) {
                            self.error = Some(error);
                            return;
                        }
                    }
                    self.body.push("\n");
                }
                Block::ToolResult { content, .. } => {
                    self.body.push_text(content, &self.resolver);
                    self.body.push("\n");
                    if !self.body.full() {
                        if let Some(path) = block.persisted_output_path() {
                            if let Some(text) =
                                cv_core::lazy::read_head(std::path::Path::new(path), self.body.remaining())
                            {
                                self.body.push(&text);
                                self.body.push("\n");
                            }
                        }
                    }
                }
                Block::File { path, source, .. } => {
                    if let Some(path) = path.as_deref().or(source.as_deref()) {
                        self.body.push(path);
                        self.body.push("\n");
                    }
                }
                Block::Image { .. } => {}
            }
        }
    }
}
impl MessageSink for DocSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.failed |= self.error.is_some();
        self.error.take().or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("semantic projection previously failed"))
        })
    }

    fn message(&mut self, message: Message) -> Flow {
        if self.failed || self.error.is_some() {
            return Flow::Stop;
        }
        if self.first_user_text.is_none() && message.role == Role::User {
            let mut label = UserLabel::default();
            for block in &message.content {
                if let Block::Text { text } = block {
                    label.push(&text.resolve_prefix(&self.resolver, FIRST_USER_RAW_HEAD));
                }
            }
            if label.meaningful {
                self.first_user_text = Some(label.head);
            }
        }
        self.append_searchable(&message);
        if self.error.is_some() || (self.body.full() && self.first_user_text.is_some()) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(value: impl Into<String>) -> Block {
        Block::Text {
            text: Text::Inline(value.into()),
        }
    }
    fn tool(value: serde_json::Value) -> Block {
        Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            input: value.into(),
            namespace: None,
        }
    }
    fn message(role: Role, content: Vec<Block>) -> Message {
        let mut message = Message::new(role);
        message.content = content;
        message
    }
    fn sink() -> DocSink {
        DocSink::new(Resolver::new(None))
    }
    fn prefix(text: &str, limit: usize) -> &str {
        let mut end = limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    #[test]
    fn semantic_head_matches_materialized_projection_and_labels() {
        // Independent owned projection; fake embedding consumer reads the same resulting head.
        let messages = [
            message(
                Role::Assistant,
                vec![
                    tool(json!({"a": [1, true, "é\n\""], "z": null})),
                    Block::Thinking {
                        text: "thought".into(),
                        signature: None,
                        encrypted: None,
                        redacted: false,
                    },
                    Block::File {
                        path: Some("a.rs".into()),
                        source: None,
                        mime: None,
                    },
                ],
            ),
            message(Role::User, vec![text(""), text("first\nuser"), text("second")]),
        ];
        let mut expected = String::new();
        let mut actual = sink();
        for m in &messages {
            m.append_searchable(&mut expected).unwrap();
            actual.message(m.clone());
        }
        assert_eq!(actual.body.as_str(), expected);
        assert_eq!(
            cv_core::label_from(None, actual.first_user_text.as_deref()),
            cv_core::label_from(None, messages[1].text().as_deref())
        );
        let fake_embed = |body: &str| {
            body.bytes()
                .fold(0u64, |sum, byte| sum.wrapping_mul(31).wrapping_add(byte as u64))
        };
        assert_eq!(fake_embed(actual.body.as_str()), fake_embed(&expected));
    }

    #[test]
    fn semantic_head_source_json_preserves_prefix_and_reports_changed_source() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-json-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("input.json");
        let raw = "{\"arg\":\"\\u00e9\\n\\\"\",\"tail\":true}";
        std::fs::write(&path, raw).unwrap();
        let json = cv_core::json::Json::source(&path, 0, raw.len() as u64).unwrap();
        let mut message = message(Role::Assistant, vec![tool(json!(null))]);
        if let Block::ToolUse { input, .. } = &mut message.content[0] {
            *input = json;
        }
        let mut expected = String::new();
        message.append_searchable(&mut expected).unwrap();
        for limit in 0..expected.len() {
            let mut actual = sink();
            actual.body = Head::new(limit);
            actual.message(message.clone());
            assert!(actual.take_error().is_none());
            assert_eq!(actual.body.as_str(), prefix(&expected, limit), "limit {limit}");
        }
        std::fs::write(&path, "null").unwrap();
        let mut actual = sink();
        actual.message(message);
        assert!(actual.take_error().unwrap().to_string().contains("JSON source changed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_head_giant_inline_arguments_names_and_paths_stay_bounded() {
        for size in [16 * 1024 * 1024, 64 * 1024 * 1024] {
            for content in [
                text("x".repeat(size)),
                tool(json!({"arg": "é\\\n".repeat(size / 4)})),
                Block::File {
                    path: Some("p".repeat(size)),
                    source: None,
                    mime: None,
                },
                Block::ToolUse {
                    id: "call".into(),
                    name: "n".repeat(size),
                    input: json!(null).into(),
                    namespace: None,
                },
            ] {
                let mut actual = sink();
                actual.message(message(Role::User, vec![content]));
                assert!(actual.body.len() <= EMBED_HEAD_BYTES);
                assert_eq!(actual.body.capacity(), EMBED_HEAD_BYTES);
                assert!(actual
                    .first_user_text
                    .as_ref()
                    .is_none_or(|text| text.len() <= LABEL_CHARS * 4));
            }
        }
    }

    #[test]
    fn semantic_head_caps_json_utf8_and_punctuation_as_a_single_prefix() {
        let m = message(
            Role::Assistant,
            vec![
                text("αβ"),
                tool(json!({"arg": "é\"\n".repeat(100)})),
                text("must not skip ahead"),
            ],
        );
        let mut expected = String::new();
        m.append_searchable(&mut expected).unwrap();
        for limit in 0..expected.len() {
            let mut actual = sink();
            actual.body = Head::new(limit);
            actual.message(m.clone());
            assert_eq!(actual.body.as_str(), prefix(&expected, limit), "limit {limit}");
        }
    }

    #[test]
    fn semantic_head_first_user_is_bounded_across_blocks_and_late_whitespace() {
        for blocks in [
            vec![text(" ".repeat(10000)), text("late")],
            vec![text("α".repeat(10000)), text("tail")],
            (0..1000).map(|_| text("a")).collect(),
            vec![text(""), text(""), text("hello")],
        ] {
            let m = message(Role::User, blocks);
            let expected = cv_core::label_from(None, m.text().as_deref());
            let mut actual = sink();
            actual.message(m);
            assert_eq!(cv_core::label_from(None, actual.first_user_text.as_deref()), expected);
            assert!(actual.first_user_text.unwrap().len() <= LABEL_CHARS * 4);
        }
        let mut actual = sink();
        actual.message(message(Role::User, vec![text(" ".repeat(10000))]));
        assert!(actual.first_user_text.is_none());
        actual.message(message(Role::User, vec![text("chosen")]));
        assert_eq!(actual.first_user_text.as_deref(), Some("chosen"));
    }

    #[test]
    fn semantic_head_sidecar_and_stop_share_the_body_budget() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("output");
        let output = "é".repeat(100000);
        std::fs::write(&path, &output).unwrap();
        let block = Block::ToolResult {
            tool_use_id: "call".into(),
            content: "stub".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(json!({"persistedOutput": {"path": path}})),
        };
        let mut actual = sink();
        assert!(matches!(
            actual.message(message(Role::Assistant, vec![block])),
            Flow::Continue
        ));
        let projected = format!("stub\n{output}\n");
        assert_eq!(actual.body.as_str(), prefix(&projected, EMBED_HEAD_BYTES));
        assert!(actual.body.full());
        let saved = actual.body.as_str().to_owned();
        assert!(matches!(
            actual.message(message(Role::User, vec![text("label")])),
            Flow::Stop
        ));
        assert_eq!(actual.body.as_str(), saved);
        assert_eq!(actual.first_user_text.as_deref(), Some("label"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn semantic_head_lazy_spans_keep_raw_prefix_and_bound_lossy_expansion() {
        let dir = std::env::temp_dir().join(format!("cv-semantic-head-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        for (escaped, bytes) in [(true, b"\\u0061".repeat(20000)), (false, vec![0xff; 100000])] {
            let path = dir.join("source");
            std::fs::write(&path, &bytes).unwrap();
            let resolver = Resolver::new(Some(path));
            let span = cv_core::Span {
                source: None,
                offset: 0,
                len: bytes.len() as u64,
                escaped,
            };
            let expected = resolver.resolve_prefix(&span, EMBED_HEAD_BYTES as u64);
            let mut actual = DocSink::new(Resolver::new(Some(dir.join("source"))));
            let expected_label = cv_core::label_from(None, Some(&resolver.resolve_prefix(&span, FIRST_USER_RAW_HEAD)));
            actual.message(message(Role::User, vec![Block::Text { text: Text::Span(span) }]));
            let mut projected = prefix(&expected, EMBED_HEAD_BYTES).to_owned();
            if projected.len() < EMBED_HEAD_BYTES && expected.len() <= EMBED_HEAD_BYTES {
                projected.push('\n');
            }
            assert_eq!(actual.body.as_str(), projected);
            assert!(actual.body.len() <= EMBED_HEAD_BYTES);
            assert_eq!(
                cv_core::label_from(None, actual.first_user_text.as_deref()),
                expected_label
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
