//! Event-authority projection. Whole-record eligibility matches the owned Value pass; only the
//! type/role discriminants are inspected, and giant output strings remain borrowed raw bytes.

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::fmt;

/// None means a blank/unreadable line, which must not advance the lookahead counter. Every valid
/// JSON value, including a scalar or non-object payload, returns a classification pair.
pub(super) fn line(bytes: &[u8]) -> Option<(bool, bool)> {
    // Match the existing Value reader's Unicode trim, not the emission path's ASCII byte trim.
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str::<&RawValue>(text).ok()?;
    if !super::super::store::json::value_compatible(text.as_bytes()) {
        return None;
    }
    if !text.starts_with('{') {
        return Some((false, false));
    }
    let top: Fields<'_> = serde_json::from_str(text).ok()?;
    let payload = match top.payload.filter(|raw| raw.get().starts_with('{')) {
        Some(raw) => serde_json::from_str::<Fields<'_>>(raw.get()).ok()?,
        None => return Some((false, false)),
    };
    let event = equals(top.kind, b"event_msg")
        && (equals(payload.kind, b"user_message") || equals(payload.kind, b"agent_message"));
    let response = equals(top.kind, b"response_item")
        && equals(payload.kind, b"message")
        && (equals(payload.role, b"user") || equals(payload.role, b"assistant"));
    Some((event, response))
}

fn equals(raw: Option<&RawValue>, expected: &[u8]) -> bool {
    let Some(body) = raw.and_then(|raw| raw.get().strip_prefix('"')?.strip_suffix('"')) else {
        return false;
    };
    super::json_ascii_prefix(body.as_bytes(), expected) == Some(body.len())
}

#[derive(Default)]
struct Fields<'a> {
    kind: Option<&'a RawValue>,
    payload: Option<&'a RawValue>,
    role: Option<&'a RawValue>,
}

// Derived struct deserialization rejects duplicates; Value instead keeps the last field, even
// when its value is null, non-string or non-object. Keep that exact overwrite behavior here.
impl<'de> Deserialize<'de> for Fields<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Fields<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a record object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = Fields::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Type => fields.kind = Some(map.next_value()?),
                        Key::Payload => fields.payload = Some(map.next_value()?),
                        Key::Role => fields.role = Some(map.next_value()?),
                        Key::Other => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(fields)
            }
        }
        d.deserialize_map(V)
    }
}

enum Key {
    Type,
    Payload,
    Role,
    Other,
}
impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Key;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object key")
            }
            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Key, E> {
                Ok(match key {
                    "type" => Key::Type,
                    "payload" => Key::Payload,
                    "role" => Key::Role,
                    _ => Key::Other,
                })
            }
        }
        d.deserialize_identifier(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::codex::{is_nl_event, is_nl_response_message, EventDetector};
    use crate::stream::Flow;
    use serde_json::Value;

    fn oracle(text: &str) -> Option<(bool, bool)> {
        serde_json::from_str::<Value>(text.trim())
            .ok()
            .map(|v| (is_nl_event(&v), is_nl_response_message(&v)))
    }

    #[test]
    fn event_projection_matches_value_acceptance_and_last_wins() {
        let mut cases: Vec<String> = [
            "",
            "  \t\r",
            "null",
            "true",
            "42",
            "\"scalar\"",
            "[]",
            "{}",
            "{} {}",
            r#"{"type":"event_msg","payload":{"type":"user_message"}}"#,
            r#"{"payload":{"role":"assistant","type":"message"},"type":"response_item"}"#,
            r#"{"ty\u0070e":"event_\u006dsg","payl\u006fad":{"type":"user_\u006dessage"}}"#,
            r#"{"type":"event_msg","type":false,"payload":{"type":"user_message"}}"#,
            r#"{"type":false,"type":"event_msg","payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message"},"payload":[]}"#,
            r#"{"type":"event_msg","payload":null,"payload":{"type":"agent_message"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","type":null}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","role":false}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":null,"role":"user"}}"#,
            r#"{"type":"event_msg","ignored":1e400,"payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","ignored":1e400,"ignored":0,"payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","ignored":"\ud800","payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","ignored":"\\ud800","payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","ignored":"\ud83c\udf19","payload":{"type":"user_message"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message"}"#,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        cases.push(format!(
            "\u{a0}{}\u{a0}",
            r#"{"type":"event_msg","payload":{"type":"user_message"}}"#
        ));
        for depth in [125, 126, 127, 128, 129] {
            cases.push(format!(
                r#"{{"type":"event_msg","ignored":{}0{},"payload":{{"type":"user_message"}}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for text in cases {
            assert_eq!(line(text.as_bytes()), oracle(&text), "{text}");
        }
        assert_eq!(
            line(b"{\"type\":\"event_msg\",\"ignored\":\"\xff\",\"payload\":{\"type\":\"user_message\"}}"),
            None
        );
    }

    #[test]
    fn event_projection_preserves_lookahead_order_and_malformed_skips() {
        let response = r#"{"type":"response_item","payload":{"type":"message","role":"user"}}"#;
        let event = r#"{"type":"event_msg","payload":{"type":"user_message"}}"#;
        for count in [
            EventDetector::LOOKAHEAD - 1,
            EventDetector::LOOKAHEAD,
            EventDetector::LOOKAHEAD + 1,
        ] {
            let mut records = vec![response.to_owned()];
            for i in 0..count {
                records.push(["null", "true", "42", "[]", "{}", "\"scalar\""][i as usize % 6].to_owned());
                records.push(r#"{"ignored":1e400}"#.to_owned());
                records.push("".to_owned());
            }
            records.push(event.to_owned());
            let mut owned = EventDetector::default();
            let mut projected = EventDetector::default();
            for text in records {
                if let Some(flags) = oracle(&text) {
                    let value = serde_json::from_str::<Value>(&text).unwrap();
                    let expected = owned.feed(&value);
                    let actual_flags = line(text.as_bytes()).unwrap();
                    assert_eq!(actual_flags, flags);
                    let actual = projected.feed_flags(actual_flags.0, actual_flags.1);
                    assert_eq!(actual, expected);
                    if actual == Flow::Stop {
                        break;
                    }
                } else {
                    assert!(line(text.as_bytes()).is_none());
                }
            }
            assert_eq!(projected.found, owned.found);
            assert_eq!(projected.found, count <= EventDetector::LOOKAHEAD);
        }
    }
    #[test]
    fn event_projection_reader_and_mapped_authority_match_owned_corpus() {
        fn check(bytes: &[u8]) {
            let mut owned = EventDetector::default();
            crate::harness::for_each_json_line(std::io::Cursor::new(bytes), |v| owned.feed(&v));
            assert_eq!(
                super::super::detect_has_events(std::io::Cursor::new(bytes)),
                owned.found
            );
            #[cfg(feature = "mmap")]
            assert_eq!(super::super::detect_has_events_bytes(bytes), owned.found);
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/codex");
        let mut tested = 0;
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                check(&std::fs::read(entry.path()).unwrap());
                tested += 1;
            }
        }
        assert!(tested > 0);
        let response = r#"{"type":"response_item","payload":{"type":"message","role":"user"}}"#;
        let event = r#"{"type":"event_msg","payload":{"type":"user_message"}}"#;
        for count in [EventDetector::LOOKAHEAD, EventDetector::LOOKAHEAD + 1] {
            let mut text = format!("{response}\r\n");
            for _ in 0..count {
                text.push_str("42\n{\"ignored\":1e400}\n\n");
            }
            text.push_str(&format!("\u{a0}{event}\u{a0}"));
            check(text.as_bytes());
        }
        check(b"{\"type\":\"event_msg\",\"ignored\":\"\xff\",\"payload\":{\"type\":\"user_message\"}}\nnull\n");
    }
}
