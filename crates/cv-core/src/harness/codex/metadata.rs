//! One-pass metadata projection. DiscardValue validates unneeded subtrees with Value's
//! scalar, surrogate and recursion rules without retaining their bodies or container rosters.

use super::*;
use crate::harness::store::DiscardValue;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use std::fmt;

#[derive(Clone, Copy)]
enum Shape {
    Record,
    Payload,
    Content,
    Part,
    Text,
    Settings,
    Collaboration,
    Info,
    Item,
    Presence,
}
#[derive(Clone, Copy)]
struct Project {
    shape: Shape,
    title_needed: bool,
    counting: bool,
}
impl<'de> DeserializeSeed<'de> for Project {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Project {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a metadata value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let nested = match (self.shape, key.as_str()) {
                (Shape::Record, "payload") => Some(Shape::Payload),
                (Shape::Payload, "content") if self.title_needed || self.counting => Some(Shape::Content),
                (Shape::Payload, "summary") if self.counting => Some(Shape::Content),
                (Shape::Payload, "message" | "encrypted_content") if self.counting => Some(Shape::Text),
                (Shape::Part, "text") if self.counting => Some(Shape::Text),
                (Shape::Payload, "thread_settings") if self.counting => Some(Shape::Settings),
                (Shape::Payload, "collaboration_mode") if self.counting => Some(Shape::Collaboration),
                (Shape::Collaboration, "settings") => Some(Shape::Settings),
                (Shape::Payload, "info") if self.counting => Some(Shape::Info),
                (Shape::Payload, "item") if self.counting => Some(Shape::Item),
                (Shape::Payload, "rate_limits") if self.counting => Some(Shape::Presence),
                (Shape::Info, "model_context_window") => Some(Shape::Presence),
                (Shape::Item, "text") => Some(Shape::Text),
                _ => None,
            };
            let keep = match self.shape {
                Shape::Record => matches!(key.as_str(), "type" | "timestamp" | "id" | "ordinal"),
                Shape::Payload => {
                    matches!(
                        key.as_str(),
                        "type" | "id" | "cwd" | "agent_path" | "subagent_history_start_ordinal" | "role" | "author"
                    ) || (self.title_needed && key == "message")
                        || (self.counting && matches!(key.as_str(), "usage" | "model" | "effort"))
                }
                Shape::Part => matches!(key.as_str(), "type" | "text"),
                Shape::Settings => matches!(key.as_str(), "model" | "reasoning_effort"),
                Shape::Info => matches!(key.as_str(), "last_token_usage" | "total_token_usage"),
                Shape::Item => matches!(key.as_str(), "type" | "kind"),
                Shape::Content | Shape::Text | Shape::Collaboration | Shape::Presence => false,
            };
            if let Some(shape) = nested {
                let value = map.next_value_seed(Self {
                    shape,
                    // Summary/encrypted blobs and UI plan text never supply the listing title.
                    title_needed: self.title_needed
                        && !matches!(key.as_str(), "summary" | "encrypted_content")
                        && !matches!(self.shape, Shape::Item),
                    ..self
                })?;
                out.insert(key, value);
            } else if keep {
                out.insert(key, map.next_value()?);
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Object(out)
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        if matches!(self.shape, Shape::Content) {
            let mut parts = Vec::new();
            while let Some(value) = seq.next_element_seed(Self {
                shape: Shape::Part,
                ..self
            })? {
                parts.push(value);
            }
            Ok(parts.into())
        } else {
            while seq.next_element::<DiscardValue>()?.is_some() {}
            Ok(if matches!(self.shape, Shape::Presence) {
                Value::Bool(true)
            } else {
                Value::Null
            })
        }
    }
    fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Content | Shape::Text) {
            if self.title_needed {
                text.into()
            } else {
                count_text(text).into()
            }
        } else if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Null
        })
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Null
        })
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Null
        })
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Null
        })
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Presence) {
            Value::Bool(true)
        } else {
            Value::Null
        })
    }
}

pub(super) fn project(line: &str, scan: &CodexScan) -> Option<Value> {
    project_with_counts(line, scan, false)
}

fn project_with_counts(line: &str, scan: &CodexScan, counting: bool) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_str(line.trim());
    let value = Project {
        shape: Shape::Record,
        title_needed: scan.title.is_none(),
        counting,
    }
    .deserialize(&mut de)
    .ok()?;
    de.end().ok()?;
    Some(value)
}

fn project_reader(reader: impl std::io::Read, scan: &CodexScan) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_reader(reader);
    let value = Project {
        shape: Shape::Record,
        title_needed: scan.title.is_none(),
        counting: true,
    }
    .deserialize(&mut de)
    .ok()?;
    de.end().ok()?;
    Some(value)
}

/// Counts depend on text presence and injected-prefix classification, not message bodies.
/// Preserve empty versus whitespace-only strings and enough leading characters for every
/// injected prefix. The original first-title projection is reduced after the title is captured.
fn count_text(text: &str) -> String {
    let trimmed = text.trim_start();
    if trimmed.is_empty() && !text.is_empty() {
        " ".into()
    } else {
        trimmed.chars().take(96).collect()
    }
}

fn reduce_bodies(value: &mut Value) {
    let Some(payload) = value.get_mut("payload") else {
        return;
    };
    for key in ["message", "encrypted_content", "content", "summary"] {
        let Some(body) = payload.get_mut(key) else { continue };
        match body {
            Value::String(text) => *text = count_text(text),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(Value::String(text)) = part.get_mut("text") {
                        *text = count_text(text);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Legacy documents are already parsed, but dispatch must not clone giant tool arguments
/// merely to count the turns. Reuse the same projection over borrowed values.
#[cfg(test)]
pub(super) fn count_item(item: &Value, ctx: &mut CodexCtx) -> usize {
    let Ok(value) = (Project {
        shape: Shape::Payload,
        title_needed: false,
        counting: true,
    })
    .deserialize(item) else {
        return 0;
    };
    let mut out = Vec::new();
    handle_item(Some(&value), false, None, &mut out, ctx);
    out.iter().filter(|m| m.is_turn()).count()
}

struct TurnPass {
    session: Session,
    ctx: CodexCtx,
    trailing: Vec<Message>,
    count: usize,
}
impl Default for TurnPass {
    fn default() -> Self {
        Self {
            session: Session::new(String::new(), Harness::Codex),
            ctx: CodexCtx::default(),
            trailing: Vec::new(),
            count: 0,
        }
    }
}
impl TurnPass {
    fn feed(&mut self, value: &Value, events: bool) {
        let before = self.trailing.len();
        dispatch_line(&mut self.session, &mut self.trailing, value, events, &mut self.ctx);
        self.count += self.trailing[before..].iter().filter(|m| m.is_turn()).count();
        // Usage records only consult the final message. Keep its role/usage alongside the
        // parser context, not a growing transcript. Metadata/system notes matter too: they can
        // prevent a later usage snapshot from attaching to an earlier assistant.
        if self.trailing.len() > 1 {
            let last = self.trailing.pop().unwrap();
            self.trailing.clear();
            self.trailing.push(last);
        }
    }
}

/// Until the shared bounded detector decides, both grammars consume the same small projection.
/// This avoids a second file read or buffering a potentially giant detection prefix. Once the
/// verdict is fixed, the other candidate is dropped, including for mixed-era late events.
#[derive(Default)]
pub(super) struct Turns {
    detector: EventDetector,
    decided: bool,
    responses: Option<TurnPass>,
    events: Option<TurnPass>,
}
impl Turns {
    pub(super) fn feed_line(&mut self, line: &str, scan: &mut CodexScan) {
        let Some(value) = project_with_counts(line, scan, true) else {
            return;
        };
        self.feed_value(value, scan);
    }
    pub(super) fn feed_reader(&mut self, reader: impl std::io::Read, scan: &mut CodexScan) {
        let Some(value) = project_reader(reader, scan) else {
            return;
        };
        self.feed_value(value, scan);
    }
    pub(super) fn feed_value(&mut self, value: Value, scan: &mut CodexScan) {
        scan.feed(&value);
        self.feed_counts(value);
    }
    pub(super) fn feed_value_with_title(&mut self, value: Value, title: Value, scan: &mut CodexScan) {
        scan.feed(&title);
        self.feed_counts(value);
    }
    fn feed_counts(&mut self, mut value: Value) {
        reduce_bodies(&mut value);
        if !self.decided {
            self.responses.get_or_insert_with(TurnPass::default);
            self.events.get_or_insert_with(TurnPass::default);
            if self.detector.feed(&value) == Flow::Stop {
                self.decided = true;
                if self.detector.found {
                    self.responses = None;
                } else {
                    self.events = None;
                }
            }
        }
        if let Some(pass) = &mut self.responses {
            pass.feed(&value, false);
        }
        if let Some(pass) = &mut self.events {
            pass.feed(&value, true);
        }
    }
    pub(super) fn count(&self) -> usize {
        if self.detector.found {
            self.events.as_ref().map_or(0, |pass| pass.count)
        } else {
            self.responses.as_ref().map_or(0, |pass| pass.count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical_count(text: &str) -> usize {
        let mut turns = Turns::default();
        let mut scan = CodexScan::default();
        for line in text.lines() {
            turns.feed_line(line, &mut scan);
        }
        turns.count()
    }

    fn assert_logical_count(text: &str) {
        let parsed = parse_str("fixture", text, true, None);
        let expected = parsed.messages.iter().filter(|m| m.is_turn()).count();
        assert_eq!(logical_count(text), expected, "logical count differs: {text}");
        let mut sink = crate::stream::CollectSink::default();
        stream_jsonl(
            "fixture",
            std::io::Cursor::new(text.as_bytes()),
            None,
            detect_has_events(std::io::Cursor::new(text.as_bytes())),
            &ParseOptions::lazy(),
            &mut sink,
        );
        assert_eq!(
            logical_count(text),
            sink.messages.iter().filter(|m| m.is_turn()).count(),
            "streamed logical count differs: {text}"
        );
    }

    #[test]
    fn discovery_logical_counts_match_parsed_and_streamed_codex_fixtures() {
        let _world = cv_testkit::World::in_process("codex-discovery-logical-counts");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        let adapter = Codex { roots: Vec::new() };
        let mut checked = 0;
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let path = entry.unwrap().path();
            let ext = path.extension().and_then(|s| s.to_str());
            if !matches!(ext, Some("jsonl" | "json")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed = parse_str("fixture", &text, ext == Some("jsonl"), Some(path.clone()));
            let expected = parsed.messages.iter().filter(|m| m.is_turn()).count();
            let reference = scan(&path).unwrap();
            assert_eq!(reference.message_count, expected, "{} parsed", path.display());
            let mut sink = crate::stream::CollectSink::default();
            adapter
                .try_stream(&reference, &ParseOptions::lazy(), &mut sink)
                .unwrap();
            assert_eq!(
                reference.message_count,
                sink.messages.iter().filter(|m| m.is_turn()).count(),
                "{} streamed",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 20);
        // Event echoes replace response-item text; they are not two additional turns.
        let text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codex/event_msg_dedups_response_item_text.jsonl"),
        )
        .unwrap();
        assert_eq!(logical_count(&text), 2);
    }

    #[test]
    fn legacy_array_discovery_counts_tool_reasoning_and_subagent_turns() {
        let world = cv_testkit::World::in_process("codex-legacy-array-counts");
        let path = world.base.join("rollout-array.json");
        let items = serde_json::json!([
            {"type":"message","role":"developer","content":"instructions"},
            {"type":"message","content":"# AGENTS.md instructions for /scratch"},
            {"type":"message","content":"prompt"},
            {"type":"message","role":"assistant","content":[]},
            {"type":"function_call","arguments":"giant arguments omitted"},
            {"type":"function_call_output","output":"result"},
            {"type":"reasoning","summary":"summary"},
            {"type":"agent_message","author":"/root/child","content":[]}
        ])
        .to_string();
        std::fs::write(&path, &items).unwrap();
        let expected = parse_str("fixture", &items, false, None)
            .messages
            .iter()
            .filter(|m| m.is_turn())
            .count();
        assert_eq!(expected, 4);
        let reference = scan(&path).unwrap();
        assert_eq!(reference.message_count, expected);
        let mut sink = crate::stream::CollectSink::default();
        Codex { roots: Vec::new() }
            .try_stream(&reference, &ParseOptions::lazy(), &mut sink)
            .unwrap();
        assert_eq!(sink.messages.iter().filter(|m| m.is_turn()).count(), expected);
    }

    #[test]
    fn discovery_logical_counts_preserve_mixed_eras_shapes_and_usage_attachment() {
        let records = [
            r#"{"type":"session_meta","payload":{"id":"s","agent_path":"/root/child","subagent_history_start_ordinal":4}}"#,
            r#"{"type":"event_msg","ordinal":1,"payload":{"type":"user_message","message":"inherited echo still decides the grammar"}}"#,
            r#"{"type":"response_item","ordinal":4,"payload":{"type":"message","role":"user","content":"response twin"}}"#,
            r#"{"type":"event_msg","ordinal":4,"payload":{"type":"user_message","message":"own prompt"}}"#,
            r##"{"type":"event_msg","ordinal":5,"payload":{"type":"user_message","message":"  # AGENTS.md instructions for /scratch"}}"##,
            r#"{"type":"response_item","ordinal":6,"payload":{"type":"function_call","arguments":"ignored"}}"#,
            r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":10,"output_tokens":2}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Plan","text":" nonempty "}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":2}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":3},"total_token_usage":{"x":1}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"second"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":3},"total_token_usage":{"x":1}}}}"#,
            r#"{"type":"turn_context","payload":{"collaboration_mode":{"settings":{"model":"first","reasoning_effort":"low"}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"model":"second","reasoning_effort":"high"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":false,"info":{"model_context_window":[]}}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"text":""},{"text":""}]}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","encrypted_content":""}}"#,
            r#"{"type":"response_item","payload":{"type":"agent_message","author":"/root/child","content":[]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"other","content":[{"type":"input_image"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"other","content":[{"text":" "},{"text":"<environment_context>"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":null}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","content":[],"summary":"fallback"}}"#,
        ];
        assert_logical_count(&records.join("\n"));
        // The detector's old-format verdict is final; a late modern record cannot change it.
        for gap in [EventDetector::LOOKAHEAD as usize, EventDetector::LOOKAHEAD as usize + 1] {
            let mut lines = vec![
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"first"}}"#.to_string(),
            ];
            lines.extend(std::iter::repeat_n("null".to_string(), gap));
            lines.push(r#"{"type":"event_msg","payload":{"type":"user_message","message":"late echo"}}"#.into());
            assert_logical_count(&lines.join("\n"));
        }
        for shape in ["null", "true", "0", "0.0", "[]", "{}", r#""string""#] {
            assert_logical_count(&format!(
                r#"{{"type":"response_item","payload":{{"type":"message","content":{shape}}}}}
{{"type":"event_msg","payload":{{"type":"token_count","rate_limits":{shape}}}}}"#
            ));
        }
        assert_logical_count(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"old","content":[]}}
{"type":"event_msg","payload":{"type":"user_message","message":"old","message":null}}
{"type":"response_item","payload":{"type":"reasoning","content":"old","content":{"ignored":1e400}}}
{"type":"response_item","payload":{"type":"function_call"}}
{"type":"response_item","payload":{"type":"function_call_output","output":"ignored"}}"#,
        );
    }

    #[test]
    fn logical_count_projection_retains_no_conversational_bodies_after_title() {
        let body = "ignored escaped \\\" 🌙\n".repeat(100_000);
        let scan = CodexScan {
            title: Some("known".into()),
            ..Default::default()
        };
        for kind in ["message", "reasoning", "agent_message", "function_call_output"] {
            let line = serde_json::json!({"type":"response_item", "payload":{
                "type":kind,"role":"assistant","content":[{"type":"output_text","text":body}],
                "encrypted_content":body,"output":body,"arguments":body,"summary":body}})
            .to_string();
            let legacy_item = serde_json::from_str::<Value>(&line).unwrap()["payload"].clone();
            assert_eq!(
                count_item(&legacy_item, &mut CodexCtx::default()),
                usize::from(kind != "function_call_output")
            );
            let projected = project_with_counts(&line, &scan, true).unwrap();
            assert!(projected.to_string().len() < 1000, "{kind} retains body");
            let mut turns = Turns::default();
            let mut scan = CodexScan::default();
            turns.feed_line(&line, &mut scan);
            assert_eq!(turns.count(), usize::from(kind != "function_call_output"));
            for pass in [turns.responses.as_ref(), turns.events.as_ref()].into_iter().flatten() {
                assert!(serde_json::to_vec(&pass.trailing).unwrap().len() < 2000);
            }
        }
    }

    fn compare<'a>(lines: impl IntoIterator<Item = &'a str>) {
        let mut owned = CodexScan::default();
        let mut projected = CodexScan::default();
        for line in lines {
            let full = serde_json::from_str::<Value>(line.trim()).ok();
            let slim = project(line, &projected);
            assert_eq!(full.is_some(), slim.is_some(), "acceptance differs: {line}");
            assert_eq!(
                full.is_some(),
                project_with_counts(line, &projected, true).is_some(),
                "count projection acceptance differs: {line}"
            );
            if let Some(full) = full {
                owned.feed(&full);
            }
            if let Some(slim) = slim {
                projected.feed(&slim);
            }
            assert_eq!(owned, projected, "scan differs: {line}");
        }
    }

    #[test]
    fn discovery_projection_matches_owned_codec_on_every_codex_fixture() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        let mut checked = 0;
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                let text = std::fs::read_to_string(path).unwrap();
                compare(text.lines());
                checked += 1;
            }
        }
        assert!(checked > 20);
    }

    #[test]
    fn discovery_projection_preserves_last_wins_shapes_and_strict_discarded_values() {
        compare([
            "null",
            "0",
            "[]",
            "true",
            r#"{"type":null,"id":"bare"}"#,
            r#"{"type":"session_meta","timestamp":"2026-10-01T00:00:00Z","payload":{"id":"ignored","id":"s","cwd":null,"cwd":"/scratch","agent_path":"root/child","subagent_history_start_ordinal":4}}"#,
            r#"{"type":"event_msg","ordinal":1,"timestamp":"2026-10-01T00:00:01Z","payload":{"type":"user_message","message":"inherited"}}"#,
            r##"{"payload":{"type":"user_message","message":"# AGENTS.md instructions for /scratch"},"type":"event_msg","ordinal":4}"##,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"discard","message":null}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"text":"injected"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":null,"content":[{"text":"  "},{"text":null},{"text":"🌙\n prompt"},{"text":" tail"},{"type":"image","data":"ignored"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":42}}"#,
            r#"{"type":"response_item","type":"event_msg","payload":{"type":"user_message"},"payload":false}"#,
            r#"{"type":"response_item","payload":{"type":"agent_message","author":"root","content":[{"type":"input_text","text":"ignored later title"},{"type":"output_text","text":"ignored"}]}}"#,
            r#"{"timestamp":"2026-10-01T00:00:03Z","type":"compacted","payload":{"history":1e400}}"#,
            r#"{"timestamp":"2026-10-01T00:00:04Z","type":"compacted","payload":{"history":"\ud800"}}"#,
            r#"{"timestamp":"2026-10-01T00:00:05Z","type":"compacted","payload":{"history":1e400,"history":0}}"#,
            r#"{"timestamp":"2026-10-01T00:00:06Z","type":"compacted","payload":{"history":"\\ud800"}}"#,
            r#"{"ordinal":-0,"type":"event_msg","payload":{"type":"agent_message"}}"#,
            r#"{"ordinal":18446744073709551616,"type":"event_msg","payload":{"type":"agent_message"}}"#,
        ]);
        // Every shape can replace fields of an earlier duplicate, including ignored containers.
        for shape in ["null", "true", "0", "0.0", "[]", "{}", r#""string""#] {
            compare([
                format!(r#"{{"type":"session_meta","payload":{{"id":"s","cwd":"/old"}},"payload":{shape}}}"#).as_str(),
            ]);
            compare([format!(
                r#"{{"type":"event_msg","type":{shape},"payload":{{"type":"user_message","message":"prompt"}}}}"#
            )
            .as_str()]);
        }
        for depth in 125..=130 {
            let line = format!(
                r#"{{"type":"compacted","payload":{}0{}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            );
            compare([line.as_str()]);
        }
    }

    #[test]
    fn discovery_projection_file_scan_rejects_non_utf8_suffix() {
        let world = cv_testkit::World::new("codex-discovery-invalid-utf8");
        let path = world.base.join("rollout-session.jsonl");
        let valid = br#"{"type":"event_msg","payload":{"type":"user_message","message":"valid prefix"}}"#;
        std::fs::write(&path, valid).unwrap();
        assert_eq!(scan(&path).unwrap().title.as_deref(), Some("valid prefix"));
        let mut bad = valid.to_vec();
        bad.extend_from_slice(b"\n\xff");
        std::fs::write(&path, bad).unwrap();
        assert!(
            scan(&path).is_err(),
            "partial metadata cannot certify a failed whole-file read"
        );
    }

    #[test]
    fn discovery_projection_does_not_retain_discarded_payloads() {
        let body = "ignored escaped \\\" 🌙\n".repeat(100_000);
        for kind in ["compacted", "response_item", "event_msg"] {
            let line = serde_json::json!({"type":kind,"timestamp":"2026-10-01T00:00:00Z",
                "payload":{"type":"function_call_output","output":body,"replacement_history":[{"text":body}],"guardian_history":body}}).to_string();
            let metadata = project(&line, &CodexScan::default()).unwrap();
            assert!(metadata.to_string().len() < 128, "{kind} retains body");
            compare([line.as_str()]);
        }
    }
}
