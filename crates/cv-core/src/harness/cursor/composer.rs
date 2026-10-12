//! Strict serde projection and selected array replay for immutable Cursor composer cells.
use super::*;
use crate::harness::store::{DiscardValue, bytes::ByteSource};
use serde::Deserialize;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

#[derive(Default)]
pub(super) struct Composer {
    pub metadata: Value,
    headers: Option<(usize, usize)>,
    conversation: Option<(usize, usize)>,
}
impl Composer {
    pub fn count(&self) -> usize {
        self.headers.or(self.conversation).map_or(0, |(_, count)| count)
    }
    pub fn replay(&self, source: &ByteSource, f: &mut dyn FnMut(Value, bool) -> Flow) {
        let Some((target, _)) = self.headers.or(self.conversation) else {
            return;
        };
        let headers = self.headers.is_some();
        // The first pass validated the whole immutable cell; intentional Stop unwinds replay.
        let _ = pass(source, Some(target), &mut |value| f(value, headers));
    }
}
pub(super) fn header(source: &ByteSource) -> Option<Composer> {
    pass(source, None, &mut |_| Flow::Continue)
}
fn pass(source: &ByteSource, target: Option<usize>, f: &mut dyn FnMut(Value) -> Flow) -> Option<Composer> {
    if let Some(out) = source
        .with_bytes(|bytes| {
            let mut de = serde_json::Deserializer::from_slice(bytes);
            let out = Root { target, f }.deserialize(&mut de).ok()?;
            de.end().ok()?;
            Some(out)
        })
        .ok()?
    {
        return out;
    }
    let mut de = serde_json::Deserializer::from_reader(source.reader().ok()?);
    let out = Root { target, f }.deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(out)
}
macro_rules! scalars {
    () => {
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
}
pub(super) use scalars;

struct Root<'a> {
    target: Option<usize>,
    f: &'a mut dyn FnMut(Value) -> Flow,
}
impl<'de> DeserializeSeed<'de> for Root<'_> {
    type Value = Composer;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<Composer, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Root<'_> {
    type Value = Composer;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("composer JSON")
    }
    scalars!();
    fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> std::result::Result<Composer, S::Error> {
        DiscardValue::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        Ok(Composer::default())
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<Composer, M::Error> {
        let mut out = Composer {
            metadata: Value::Object(Default::default()),
            ..Default::default()
        };
        let mut member = 0;
        while let Some(key) = map.next_key::<String>()? {
            if matches!(key.as_str(), "fullConversationHeadersOnly" | "conversation") {
                let count = map.next_value_seed(Array {
                    f: (self.target == Some(member)).then_some(&mut *self.f),
                })?;
                let array = count.map(|count| (member, count));
                if key == "fullConversationHeadersOnly" {
                    out.headers = array;
                } else {
                    out.conversation = array;
                }
                member += 1;
            } else if self.target.is_none()
                && matches!(
                    key.as_str(),
                    "name" | "createdAt" | "lastUpdatedAt" | "subagentInfo" | "unifiedMode" | "modelConfig"
                )
            {
                out.metadata
                    .as_object_mut()
                    .unwrap()
                    .insert(key, map.next_value::<Value>()?);
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(out)
    }
}
pub(super) struct Array<'a> {
    pub(super) f: Option<&'a mut dyn FnMut(Value) -> Flow>,
}
impl<'de> DeserializeSeed<'de> for Array<'_> {
    type Value = Option<usize>;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<Self::Value, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Array<'_> {
    type Value = Option<usize>;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("conversation array")
    }
    scalars!();
    fn visit_map<M: MapAccess<'de>>(self, map: M) -> std::result::Result<Self::Value, M::Error> {
        DiscardValue::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(None)
    }
    fn visit_seq<S: SeqAccess<'de>>(mut self, mut seq: S) -> std::result::Result<Self::Value, S::Error> {
        let mut count = 0;
        if let Some(f) = &mut self.f {
            while let Some(value) = seq.next_element::<Value>()? {
                count += 1;
                if f(value) == Flow::Stop {
                    return Err(serde::de::Error::custom("message sink stopped"));
                }
            }
        } else {
            while seq.next_element::<DiscardValue>()?.is_some() {
                count += 1;
            }
        }
        Ok(Some(count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_and_replay_match_whole_value_strictly() {
        let mut cases = vec![
            r#"{"name":"n","conversation":[{"type":1,"text":"first"},{"type":2,"text":"last"}]}"#.to_string(),
            r#"{"conversation":[1,{}],"fullConversationHeadersOnly":[]}"#.into(),
            r#"{"conversation":[1,{}],"fullConversationHeadersOnly":[],"fullConversationHeadersOnly":false}"#.into(),
            r#"{"conversation":[1],"conversation":[2],"fullConversationHeadersOnly":[3],"fullConversationHeadersOnly":[4]}"#.into(),
            r#"{"conversation":[1],"conversation":false,"name":"old","na\u006de":"last"}"#.into(),
            r#"{"conversation":[{"type":1,"text":"ok"}],"ignored":"\ud800"}"#.into(),
            r#"{"conversation":[{"type":1,"text":"ok"}],"ignored":1e400}"#.into(),
            r#"{"conversation":[{"type":1,"text":"ok"}],"ignored":{"a":"\ud800","a":null}}"#.into(),
            r#"{"conversation":[]} trailing"#.into(), "null".into(), "[]".into(), "42".into(), "\"string\"".into(),
        ];
        for depth in 125..=129 {
            cases.push(format!(
                "{{\"conversation\":[],\"ignored\":{}0{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for text in cases {
            let old = serde_json::from_str::<Value>(&text).ok();
            let source = ByteSource::read(text.as_bytes()).unwrap();
            let header = header(&source);
            assert_eq!(header.is_some(), old.is_some(), "{text}");
            let Some(header) = header else { continue };
            let old = old.unwrap();
            let headers = old.get("fullConversationHeadersOnly").and_then(Value::as_array);
            let selected = headers.or_else(|| old.get("conversation").and_then(Value::as_array));
            let expected = selected.cloned().unwrap_or_default();
            assert_eq!(header.count(), super::super::conversation_len(&old));
            let mut actual = Vec::new();
            header.replay(&source, &mut |value, external| {
                assert_eq!(external, headers.is_some());
                actual.push(value);
                Flow::Continue
            });
            assert_eq!(actual, expected);
            let mut stopped = 0;
            header.replay(&source, &mut |_, _| {
                stopped += 1;
                Flow::Stop
            });
            assert_eq!(stopped, usize::from(!expected.is_empty()));
            for key in [
                "name",
                "createdAt",
                "lastUpdatedAt",
                "subagentInfo",
                "unifiedMode",
                "modelConfig",
            ] {
                assert_eq!(header.metadata.get(key), old.get(key));
            }
        }
    }
}
