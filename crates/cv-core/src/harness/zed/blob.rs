//! Strict metadata/validation before selected messages. Stream operations decode once into
//! bounded memory or a private spill; compressed offsets never become transcript spans.

mod count;

use super::{legacy_message, modern_message};
use crate::harness::store::{bytes::ByteSource, DiscardValue};
use crate::ir::Message;
#[cfg(test)]
use crate::ir::Role;
use crate::stream::Flow;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Header {
    pub metadata: Value,
    pub last_messages: Option<usize>,
    pub message_count: usize,
    decoded: Option<ByteSource>,
    pub(super) checked: Option<crate::json::Json>,
}

pub(super) fn header(data_type: Option<&str>, data: &[u8], count: bool) -> Option<Header> {
    pass(data_type, data, count, None, &mut |_| Flow::Continue)
}

pub(super) fn replay(data_type: Option<&str>, data: &[u8], header: &Header, f: &mut dyn FnMut(Message) -> Flow) {
    if let Some(target) = header.last_messages {
        // The first pass validated the entire immutable SQLite cell. Stop is an intentional
        // unwind of the second decoder, so a window consumer need not replay the suffix.
        let _ = pass(data_type, data, false, Some(target), f);
    }
}

fn pass(
    data_type: Option<&str>,
    data: &[u8],
    count: bool,
    target: Option<usize>,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Option<Header> {
    let seed = Root { count, target, f };
    let compressed = match data_type {
        Some("zstd") => true,
        Some("json") => false,
        _ => data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
    };
    if compressed {
        let decoder = crate::harness::store::zst::decoder(data).ok()?;
        let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::new(decoder));
        let out = seed.deserialize(&mut de).ok()?;
        de.end().ok()?;
        Some(out)
    } else {
        let mut de = serde_json::Deserializer::from_slice(data);
        let out = seed.deserialize(&mut de).ok()?;
        de.end().ok()?;
        Some(out)
    }
}

/// Incremental SQLite blobs do not expose contiguous source bytes. Both passes reopen the
/// same cell inside the caller's read transaction, retaining only decoder/message scratch.
pub(super) fn header_reader(data_type: Option<&str>, data: impl std::io::Read, count: bool) -> Option<Header> {
    pass_reader(data_type, data, count, None, &mut |_| Flow::Continue)
}

pub(super) fn replay_reader(
    data_type: Option<&str>,
    data: impl std::io::Read,
    header: &Header,
    f: &mut dyn FnMut(Message) -> Flow,
) {
    if let Some(target) = header.last_messages {
        let _ = pass_reader(data_type, data, false, Some(target), f);
    }
}

fn pass_reader(
    data_type: Option<&str>,
    data: impl std::io::Read,
    count: bool,
    target: Option<usize>,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Option<Header> {
    use std::io::BufRead;
    let mut data = std::io::BufReader::new(data);
    let compressed = match data_type {
        Some("zstd") => true,
        Some("json") => false,
        _ => data.fill_buf().ok()?.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
    };
    let seed = Root { count, target, f };
    if compressed {
        let decoder = crate::harness::store::zst::decoder(data).ok()?;
        let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::new(decoder));
        let out = seed.deserialize(&mut de).ok()?;
        de.end().ok()?;
        Some(out)
    } else {
        let mut de = serde_json::Deserializer::from_reader(data);
        let out = seed.deserialize(&mut de).ok()?;
        de.end().ok()?;
        Some(out)
    }
}

fn parse_decoded(
    decoded: &ByteSource,
    count: bool,
    target: Option<usize>,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Option<Header> {
    if let Some(out) = decoded
        .with_bytes(|bytes| pass(Some("json"), bytes, count, target, f))
        .ok()?
    {
        return out;
    }
    pass_reader(Some("json"), decoded.reader().ok()?, count, target, f)
}

const METADATA: &[&str] = &[
    "title",
    "summary",
    "updated_at",
    "initial_project_snapshot",
    "model",
    "version",
    "profile",
    "completion_mode",
    "detailed_summary_state",
    "detailed_summary",
    "cumulative_token_usage",
    "subagent_context",
    "imported",
];

fn decode(data_type: Option<&str>, data: impl std::io::Read) -> Option<ByteSource> {
    use std::io::BufRead;
    let mut data = std::io::BufReader::new(data);
    let compressed = match data_type {
        Some("zstd") => true,
        Some("json") => false,
        _ => data.fill_buf().ok()?.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
    };
    if compressed {
        ByteSource::read(crate::harness::store::zst::decoder(data).ok()?).ok()
    } else {
        ByteSource::read(data).ok()
    }
}

pub(super) fn prepare_reader(data_type: Option<&str>, data: impl std::io::Read) -> Option<Header> {
    let decoded = decode(data_type, data)?;
    let mut header = parse_decoded(&decoded, false, None, &mut |_| Flow::Continue)?;
    header.decoded = Some(decoded);
    Some(header)
}

/// Validate the entire decoded document before metadata/callbacks, then retain its checked
/// messages child. Replaying selected children does not deserialize a whole message tree.
pub(super) fn prepare_checked_reader(
    data_type: Option<&str>,
    data: impl std::io::Read,
    source: &crate::json::JsonSource,
) -> Option<Header> {
    let decoded = std::sync::Arc::new(decode(data_type, data)?);
    let len = decoded.len().ok()?;
    source
        .read_backing_value(decoded, 0, len, |view| {
            let mut keys = METADATA.to_vec();
            keys.push("messages");
            let fields = crate::harness::store::document::Fields::parse(view, &keys)?;
            let metadata = fields
                .as_ref()
                .map(|fields| fields.project(METADATA))
                .transpose()?
                .unwrap_or_default();
            let messages = fields
                .as_ref()
                .and_then(|fields| fields.get("messages"))
                .map(|view| view.to_json())
                .transpose()?
                .unwrap_or_default();
            Ok(Header {
                metadata: Value::Object(metadata),
                checked: Some(messages),
                ..Header::default()
            })
        })
        .ok()
}

pub(super) fn replay_prepared(header: &Header, f: &mut dyn FnMut(Message) -> Flow) -> bool {
    let Some(decoded) = &header.decoded else { return false };
    if let Some(target) = header.last_messages {
        let _ = parse_decoded(decoded, false, Some(target), f);
    }
    true
}

// Both root and messages accept non-container scalars, just as Value followed by get/as_array
// did. Serde still checks finite numbers, UTF-16 and container depth on every discarded value.
macro_rules! scalars {
    () => {
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
}

// Keep only recognized static keys. Unknown keys still receive Serde's strict string
// validation, but never acquire a second owned copy beyond its reader scratch.
struct RootKey;
impl<'de> DeserializeSeed<'de> for RootKey {
    type Value = Option<&'static str>;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_str(self)
    }
}
impl<'de> Visitor<'de> for RootKey {
    type Value = Option<&'static str>;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("a thread field name")
    }
    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(if key == "messages" {
            Some("messages")
        } else {
            METADATA.iter().copied().find(|name| *name == key)
        })
    }
}

struct Root<'a> {
    count: bool,
    target: Option<usize>,
    f: &'a mut dyn FnMut(Message) -> Flow,
}

impl<'de> DeserializeSeed<'de> for Root<'_> {
    type Value = Header;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Header, D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Root<'_> {
    type Value = Header;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("a thread")
    }
    scalars!();
    fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> Result<Header, S::Error> {
        DiscardValue::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        Ok(Header::default())
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Header, M::Error> {
        let mut out = Header {
            metadata: Value::Object(Default::default()),
            ..Header::default()
        };
        let mut member = 0;
        while let Some(key) = map.next_key_seed(RootKey)? {
            if key == Some("messages") {
                out.last_messages = Some(member);
                let mut count = 0;
                if self.count {
                    count = map.next_value_seed(count::Messages)?;
                } else if self.target == Some(member) {
                    map.next_value_seed(Messages(self.f))?;
                } else {
                    map.next_value::<DiscardValue>()?;
                }
                out.message_count = count;
                member += 1;
            } else if let Some(key) = key.filter(|_| self.target.is_none()) {
                let value = map.next_value::<Value>()?;
                out.metadata.as_object_mut().unwrap().insert(key.into(), value);
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(out)
    }
}

struct Messages<'a>(&'a mut dyn FnMut(Message) -> Flow);
impl<'de> DeserializeSeed<'de> for Messages<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<(), D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Messages<'_> {
    type Value = ();
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("thread messages")
    }
    scalars!();
    fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<(), M::Error> {
        DiscardValue::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(())
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let mut tool_names = HashMap::new();
        while let Some(message) = seq.next_element::<Value>()? {
            if legacy_message(&message, &mut tool_names, self.0) == Flow::Stop
                || modern_message(&message, self.0) == Flow::Stop
            {
                return Err(serde::de::Error::custom("message sink stopped"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_spill_is_owned_and_reader_fallback_is_bounded() {
        let invalid = format!(
            "{{\"messages\":[],\"ignored\":\"{}\"}} trailing",
            "x".repeat(crate::harness::store::bytes::MEMORY_LIMIT)
        );
        assert!(prepare_reader(Some("json"), invalid.as_bytes()).is_none());
    }

    #[test]
    fn invalid_utf8_or_compressed_tail_never_emits_a_prefix() {
        let json = b"{\"messages\":[{\"role\":\"user\",\"text\":\"ok\"}],\"ignored\":\"\xff\"}";
        assert!(serde_json::from_slice::<Value>(json).is_err());
        let compressed = zstd::encode_all(json.as_slice(), 1).unwrap();
        assert!(header(Some("json"), json, false).is_none());
        assert!(header(Some("zstd"), &compressed, false).is_none());
        let valid = br#"{"messages":[{"role":"user","text":"ok"}]}"#;
        let compressed = zstd::encode_all(valid.as_slice(), 1).unwrap();
        assert!(header(Some("zstd"), &compressed[..compressed.len() - 1], false).is_none());
    }

    #[test]
    fn projection_and_replay_match_whole_value_oracle() {
        let mut cases = vec![
            serde_json::to_string(&super::super::tests::legacy_thread()).unwrap(),
            serde_json::to_string(&super::super::tests::modern_thread()).unwrap(),
            r#"{"messages":[{"role":"user","text":"old"}],"title":"old","messages":[{"role":"assistant","text":"last"},{"User":{"id":"u","content":[{"Text":"modern"}]}},{"role":"user","text":"legacy","User":{"id":"mixed","content":[{"Text":"both"}]}}],"ti\u0074le":"last"}"#.into(),
            r#"{"messages":[{"role":"user","text":"obsolete"}],"messages":false,"title":"last"}"#.into(),
            r#"{"messages":null,"messages":[]}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":"\ud800"}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":1e400}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":{"a":"\ud800","a":null}}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}]} trailing"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}]} { }"#.into(),
            r#"{"messages":{"ignored":"\ud800"}}"#.into(),
            "null".into(), "42".into(), "\"text\"".into(), "[]".into(),
        ];
        cases.push(format!(
            "{{\"{}\":null,\"messages\":[{{\"role\":\"user\",\"text\":\"ok\"}}],\"ti\\u0074le\":\"last\"}}",
            "unknown".repeat(1024)
        ));
        cases.push(r#"{"\ud800":null,"messages":[]}"#.into());
        for depth in 125..=129 {
            cases.push(format!(
                "{{\"messages\":[],\"ignored\":{}0{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
            cases.push(format!(
                "{{\"messages\":[{{\"role\":\"user\",\"text\":\"ok\",\"ignored\":{}0{}}}]}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for text in cases {
            let old = serde_json::from_str::<Value>(&text).ok();
            let mut expected = Vec::new();
            if let Some(t) = &old {
                super::super::each_ir_message(t, &mut |m| {
                    expected.push(m);
                    Flow::Continue
                });
            }
            for compressed in [false, true] {
                let bytes = if compressed {
                    zstd::encode_all(text.as_bytes(), 1).unwrap()
                } else {
                    text.as_bytes().to_vec()
                };
                for count in [false, true] {
                    let projection = header(Some(if compressed { "zstd" } else { "json" }), &bytes, count);
                    assert_eq!(
                        projection.is_some(),
                        old.is_some(),
                        "compressed={compressed} count={count}: {text}"
                    );
                    let streamed = header_reader(
                        Some(if compressed { "zstd" } else { "json" }),
                        std::io::Cursor::new(&bytes),
                        count,
                    );
                    assert_eq!(streamed.is_some(), old.is_some());
                    if let (Some(a), Some(b)) = (&projection, &streamed) {
                        assert_eq!(a.metadata, b.metadata);
                        assert_eq!(a.message_count, b.message_count);
                        assert_eq!(a.last_messages, b.last_messages);
                    }
                    let prepared = prepare_reader(None, std::io::Cursor::new(&bytes));
                    assert_eq!(prepared.is_some(), old.is_some());
                    if let Some(prepared) = &prepared {
                        let mut actual = Vec::new();
                        assert!(replay_prepared(prepared, &mut |m| {
                            actual.push(m);
                            Flow::Continue
                        }));
                        assert_eq!(
                            serde_json::to_value(actual).unwrap(),
                            serde_json::to_value(&expected).unwrap()
                        );
                        let mut stopped = Vec::new();
                        replay_prepared(prepared, &mut |m| {
                            stopped.push(m);
                            Flow::Stop
                        });
                        assert_eq!(stopped.len(), usize::from(!expected.is_empty()));
                        assert_eq!(prepared.metadata, projection.as_ref().unwrap().metadata);
                    }
                    let Some(h) = projection else { continue };
                    if count {
                        assert_eq!(
                            h.message_count,
                            expected
                                .iter()
                                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                                .count()
                        );
                    }
                    if let Some(t) = &old {
                        let mut old_extra = serde_json::Map::new();
                        let mut new_extra = serde_json::Map::new();
                        let mut old_parent = None;
                        let mut new_parent = None;
                        super::super::thread_extras(t, &mut old_extra, &mut old_parent);
                        super::super::thread_extras(&h.metadata, &mut new_extra, &mut new_parent);
                        assert_eq!(old_extra, new_extra);
                        assert_eq!(old_parent, new_parent);
                        assert_eq!(super::super::snapshot_cwd(t), super::super::snapshot_cwd(&h.metadata));
                        assert_eq!(
                            serde_json::to_value(super::super::snapshot_git(t)).unwrap(),
                            serde_json::to_value(super::super::snapshot_git(&h.metadata)).unwrap()
                        );
                        for key in ["title", "summary", "updated_at", "model"] {
                            assert_eq!(t.get(key), h.metadata.get(key));
                        }
                    }
                    let mut actual = Vec::new();
                    replay(None, &bytes, &h, &mut |m| {
                        actual.push(m);
                        Flow::Continue
                    });
                    assert_eq!(
                        serde_json::to_value(&actual).unwrap(),
                        serde_json::to_value(&expected).unwrap()
                    );
                    let mut streamed = Vec::new();
                    replay_reader(None, std::io::Cursor::new(&bytes), &h, &mut |m| {
                        streamed.push(m);
                        Flow::Continue
                    });
                    assert_eq!(
                        serde_json::to_value(&streamed).unwrap(),
                        serde_json::to_value(&expected).unwrap()
                    );
                    let mut stopped = Vec::new();
                    replay(None, &bytes, &h, &mut |m| {
                        stopped.push(m);
                        Flow::Stop
                    });
                    assert_eq!(stopped.len(), usize::from(!expected.is_empty()));
                    if let Some(m) = stopped.first() {
                        assert_eq!(
                            serde_json::to_value(m).unwrap(),
                            serde_json::to_value(&expected[0]).unwrap()
                        );
                    }
                }
            }
        }
    }
}
