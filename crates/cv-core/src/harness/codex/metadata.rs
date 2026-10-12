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
}
#[derive(Clone, Copy)]
struct Project {
    shape: Shape,
    title_needed: bool,
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
                (Shape::Payload, "content") if self.title_needed => Some(Shape::Content),
                _ => None,
            };
            let keep = match self.shape {
                Shape::Record => matches!(key.as_str(), "type" | "timestamp" | "id" | "ordinal"),
                Shape::Payload => {
                    matches!(
                        key.as_str(),
                        "type" | "id" | "cwd" | "agent_path" | "subagent_history_start_ordinal" | "role" | "author"
                    ) || (self.title_needed && key == "message")
                }
                Shape::Part => matches!(key.as_str(), "type" | "text"),
                Shape::Content => false,
            };
            if let Some(shape) = nested {
                let value = map.next_value_seed(Self { shape, ..self })?;
                out.insert(key, value);
            } else if keep {
                out.insert(key, map.next_value()?);
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(Value::Object(out))
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
            Ok(Value::Null)
        }
    }
    fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<Value, E> {
        Ok(if matches!(self.shape, Shape::Content) {
            text.into()
        } else {
            Value::Null
        })
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
}

pub(super) fn project(line: &str, scan: &CodexScan) -> Option<Value> {
    let mut de = serde_json::Deserializer::from_str(line.trim());
    let value = Project {
        shape: Shape::Record,
        title_needed: scan.title.is_none(),
    }
    .deserialize(&mut de)
    .ok()?;
    de.end().ok()?;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare<'a>(lines: impl IntoIterator<Item = &'a str>) {
        let mut owned = CodexScan::default();
        let mut projected = CodexScan::default();
        for line in lines {
            let full = serde_json::from_str::<Value>(line.trim()).ok();
            let slim = project(line, &projected);
            assert_eq!(full.is_some(), slim.is_some(), "acceptance differs: {line}");
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
