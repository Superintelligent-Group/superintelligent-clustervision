//! Count the same visible user/assistant turns as the owned mapping, retaining schema facts
//! rather than a message tree or IR blocks. Serde still validates every ignored value; its
//! reader string scratch can grow to the largest decoded string, so this is not a full bound.
use crate::harness::store::DiscardValue;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

pub(super) struct Messages;
impl<'de> DeserializeSeed<'de> for Messages {
    type Value = usize;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<usize, D::Error> {
        Ok(Probe(Shape::Messages).deserialize(de)?.count)
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Ignore,
    String,
    NonNull,
    Messages,
    Message,
    Segments,
    Segment,
    Uses,
    User,
    UserContent,
    UserItem,
    Mention,
    Agent,
    AgentContent,
    AgentItem,
}
impl Shape {
    fn fields(self) -> &'static [(&'static str, Self)] {
        use Shape::*;
        match self {
            Message => &[
                ("role", String),
                ("segments", Segments),
                ("text", String),
                ("tool_uses", Uses),
                ("context", String),
                ("User", User),
                ("Agent", Agent),
            ],
            Segment => &[("type", String), ("text", String)],
            User => &[("content", UserContent)],
            UserItem => &[("Text", String), ("Mention", Mention), ("Image", Ignore)],
            Mention => &[("uri", Ignore)],
            Agent => &[("content", AgentContent), ("reasoning_details", NonNull)],
            AgentItem => &[
                ("Text", String),
                ("Thinking", Ignore),
                ("RedactedThinking", String),
                ("ToolUse", Ignore),
            ],
            _ => &[],
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Empty,
    System,
    Text,
    Segment,
    Other,
}
#[derive(Clone, Copy, Default)]
struct Fact {
    string: Option<StringKind>,
    array: bool,
    content: bool,
    count: usize,
}
impl Fact {
    fn nonempty(self) -> bool {
        self.string.is_some_and(|s| s != StringKind::Empty)
    }
}
struct Probe(Shape);
impl Probe {
    fn scalar(self, string: Option<StringKind>, null: bool) -> Fact {
        Fact {
            string,
            content: matches!(self.0, Shape::NonNull) && !null,
            ..Fact::default()
        }
    }
}
impl<'de> DeserializeSeed<'de> for Probe {
    type Value = Fact;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Fact, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Probe {
    type Value = Fact;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("a counted message fact")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Fact, E> {
        Ok(self.scalar(None, true))
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Fact, E> {
        Ok(self.scalar(None, false))
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Fact, E> {
        Ok(self.scalar(None, false))
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Fact, E> {
        Ok(self.scalar(None, false))
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Fact, E> {
        Ok(self.scalar(None, false))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Fact, E> {
        let kind = match value {
            "" => StringKind::Empty,
            "system" => StringKind::System,
            "text" => StringKind::Text,
            "thinking" | "RedactedThinking" | "tool_use" | "tool_result" | "image" | "document" => StringKind::Segment,
            _ => StringKind::Other,
        };
        Ok(self.scalar(Some(kind), false))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Fact, S::Error> {
        let item = match self.0 {
            Shape::Messages => Some(Shape::Message),
            Shape::Segments => Some(Shape::Segment),
            Shape::UserContent => Some(Shape::UserItem),
            Shape::AgentContent => Some(Shape::AgentItem),
            _ => None,
        };
        let mut out = Fact {
            array: true,
            ..Fact::default()
        };
        if let Some(item) = item {
            while let Some(value) = seq.next_element_seed(Probe(item))? {
                out.content |= value.content;
                out.count += value.count;
            }
        } else {
            while seq.next_element::<DiscardValue>()?.is_some() {
                out.content |= matches!(self.0, Shape::Uses);
            }
            out.content |= matches!(self.0, Shape::NonNull);
        }
        Ok(out)
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Fact, M::Error> {
        let schema = self.0.fields();
        // Fixed schema slots: duplicate fields replace their facts, including non-container
        // winners. Unknown keys/bodies are validated without a roster or retained payload.
        let mut fields = [Fact::default(); 7];
        let mut present = [false; 7];
        while let Some(index) = map.next_key_seed(Key(schema))? {
            if let Some(index) = index {
                fields[index] = map.next_value_seed(Probe(schema[index].1))?;
                present[index] = true;
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        let mut count = 0;
        let content = match self.0 {
            Shape::NonNull => true,
            Shape::Segment => match fields[0].string {
                Some(StringKind::Text) => fields[1].nonempty(),
                Some(StringKind::Segment) => true,
                _ => false,
            },
            Shape::User => fields[0].content,
            Shape::UserItem => {
                if fields[0].string.is_some() {
                    fields[0].nonempty()
                } else if present[1] {
                    fields[1].content
                } else {
                    present[2]
                }
            }
            Shape::Mention => present[0],
            Shape::Agent => fields[0].content || fields[1].content,
            Shape::AgentItem => {
                if fields[0].string.is_some() {
                    fields[0].nonempty()
                } else {
                    present[1] || fields[2].string.is_some() || present[3]
                }
            }
            Shape::Message => {
                let legacy_content = if fields[1].array {
                    fields[1].content
                } else {
                    fields[2].nonempty()
                };
                let legacy = fields[0].string.is_some_and(|role| role != StringKind::System)
                    && (legacy_content || fields[3].content || fields[4].nonempty());
                // Modern enum presence is authoritative before type/content checks. A present
                // scalar/empty User suppresses Agent, exactly like the owned converter.
                let modern = if present[5] {
                    fields[5].content
                } else {
                    fields[6].content
                };
                count = usize::from(legacy) + usize::from(modern);
                false
            }
            _ => false,
        };
        Ok(Fact {
            content,
            count,
            ..Fact::default()
        })
    }
}

/// Match schema keys without copying even an unexpectedly huge key. Serde validates/decodes
/// the original string before this visitor; unknown strings retain no owned payload.
struct Key(&'static [(&'static str, Shape)]);
impl<'de> DeserializeSeed<'de> for Key {
    type Value = Option<usize>;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_str(self)
    }
}
impl<'de> Visitor<'de> for Key {
    type Value = Option<usize>;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("a message field name")
    }
    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(self.0.iter().position(|(name, _)| *name == key))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{header, header_reader};
    use crate::{ir::Role, stream::Flow};
    use serde_json::{json, Value};

    fn compare(raw: &str) {
        let owned = serde_json::from_str::<Value>(raw).ok();
        let mut count = 0;
        if let Some(value) = &owned {
            super::super::super::each_ir_message(value, &mut |message| {
                count += usize::from(matches!(message.role, Role::User | Role::Assistant));
                Flow::Continue
            });
        }
        for compressed in [false, true] {
            let bytes = if compressed {
                ruzstd::encoding::compress_to_vec(raw.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
            } else {
                raw.as_bytes().to_vec()
            };
            for actual in [header(None, &bytes, true), header_reader(None, bytes.as_slice(), true)] {
                assert_eq!(actual.is_some(), owned.is_some());
                if let Some(actual) = actual {
                    assert_eq!(actual.message_count, count);
                    if let Some(value) = owned.as_ref() {
                        for key in ["title", "summary", "updated_at", "model", "version"] {
                            assert_eq!(actual.metadata.get(key), value.get(key));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn count_facts_match_owned_turn_visibility_and_duplicate_precedence() {
        compare(&super::super::super::tests::legacy_thread().to_string());
        compare(&super::super::super::tests::modern_thread().to_string());
        let mut messages = Vec::new();
        for role in [
            Value::Null,
            json!("user"),
            json!("assistant"),
            json!("system"),
            json!("unknown"),
        ] {
            for segments in [
                Value::Null,
                json!([]),
                json!([null, true, {}]),
                json!([{"type":"text","text":""}]),
                json!([{"type":"text","text":"body"}]),
                json!([{"type":"thinking"}]),
                json!([{"type":"RedactedThinking"}]),
                json!([{"type":"tool_use"}]),
                json!([{"type":"tool_result"}]),
                json!([{"type":"image"}]),
                json!([{"type":"document"}]),
                json!([{"type":"unknown","text":"body"}]),
            ] {
                messages.push(json!({"role":role,"segments":segments,"text":"fallback"}));
                messages.push(json!({"role":role,"segments":segments,"context":"attached","tool_uses":[null]}));
            }
        }
        for content in [
            json!([]),
            json!([null]),
            json!([{"Text":""}]),
            json!([{"Text":"body"}]),
            json!([{"Text":null,"Mention":{"uri":null,"content":"discard"}}]),
            json!([{"Text":"","Mention":{"uri":"suppressed"},"Image":null}]),
            json!([{"Mention":null,"Image":{}}]),
            json!([{"Image":null}]),
            json!([{"Thinking":null}]),
            json!([{"RedactedThinking":""}]),
            json!([{"ToolUse":null}]),
        ] {
            messages.push(json!({"User":{"content":content}}));
            messages.push(json!({"Agent":{"content":content,"reasoning_details":null}}));
            messages.push(json!({"Agent":{"content":content,"reasoning_details":{}}}));
            messages.push(json!({"User":null,"Agent":{"content":content,"reasoning_details":{}}}));
        }
        compare(&json!({"title":"facts","messages":messages}).to_string());
        for raw in [
            r#"{"messages":[{"role":"user","text":"obsolete"}],"messages":false}"#,
            r#"{"messages":[{"role":"user","role":null,"text":"hidden"},{"role":"system","ro\u006ce":"assistant","text":"kept"},{"role":"user","segments":[{"type":"text","text":"kept"}],"segments":null,"text":"fallback"},{"User":{"content":[{"Text":"kept"}]},"User":false,"Agent":{"reasoning_details":true}},{"Agent":{"content":[{"Text":"obsolete"}],"content":[{"Text":"","Thinking":{}}],"reasoning_details":true,"reasoning_details":null}}]}"#,
            r#"{"messages":[{"role":"user","text":"legacy","User":{"content":[{"Mention":{"uri":1}}]}}]}"#,
        ] {
            compare(raw);
        }
    }

    #[test]
    fn giant_discarded_count_payloads_keep_strict_value_acceptance() {
        let giant = "雪\n\"\\".repeat(crate::harness::store::bytes::MEMORY_LIMIT / 4);
        let raw = json!({"title":"giant count","messages":[
            {"role":"user","text":giant,"ignored":{"input":giant}},
            {"role":"assistant","tool_uses":[{"input":{"body":giant}}]},
            {"User":{"content":[{"Mention":{"uri":"file:///ref","content":giant}}]}},
            {"Agent":{"content":[{"ToolUse":{"input":{"body":giant}}}],"tool_results":{"t":{"content":giant}}}}
        ]})
        .to_string();
        compare(&raw);
        // Full validation includes giant ignored payloads and invalid duplicate losers. Count
        // facts never authorize ignoring nonfinite numbers, lone surrogates or UTF-8 errors.
        for ignored in [r#""\ud800""#, "1e400", r#"{"a":"\ud800","a":null}"#] {
            let invalid = format!(
                "{{\"messages\":[{{\"role\":\"user\",\"text\":{},\"ignored\":{ignored}}}]}}",
                serde_json::to_string(&giant).unwrap()
            );
            compare(&invalid);
        }
        let invalid_utf8 = b"{\"messages\":[{\"role\":\"user\",\"text\":\"ok\",\"ignored\":\"\xff\"}]}";
        assert!(header(None, invalid_utf8, true).is_none());
        assert!(header_reader(None, invalid_utf8.as_slice(), true).is_none());
    }
}
