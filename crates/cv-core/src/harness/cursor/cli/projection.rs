//! Discovery's count/title projection validates complete JSON without retaining tool arguments,
//! tool results, provider metadata or conversational bodies beyond a title/classification prefix.
use super::*;
use crate::harness::store::DiscardValue;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use std::fmt;

#[derive(Clone, Copy)]
enum Shape {
    Message,
    Content,
    Part,
    Provider,
    Cursor,
    Presence,
}
#[derive(Clone, Copy)]
struct Project(Shape);
impl<'de> DeserializeSeed<'de> for Project {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> std::result::Result<Value, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Project {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Cursor CLI message metadata")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let nested = match (self.0, key.as_str()) {
                (Shape::Message, "content") => Some(Shape::Content),
                (Shape::Message, "providerOptions") => Some(Shape::Provider),
                (Shape::Provider, "cursor") => Some(Shape::Cursor),
                (Shape::Cursor, "requestContextCompleteness") => Some(Shape::Presence),
                _ => None,
            };
            let keep = match self.0 {
                Shape::Message => matches!(key.as_str(), "role"),
                Shape::Part => matches!(key.as_str(), "type" | "text"),
                Shape::Cursor => key == "isSummary",
                _ => false,
            };
            if let Some(shape) = nested {
                out.insert(key, map.next_value_seed(Project(shape))?);
            } else if keep && key == "text" {
                out.insert(key, map.next_value_seed(Project(Shape::Content))?);
            } else if keep {
                out.insert(key, map.next_value()?);
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(if matches!(self.0, Shape::Presence) {
            true.into()
        } else {
            Value::Object(out)
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut parts = Vec::new();
        if matches!(self.0, Shape::Content) {
            while let Some(part) = seq.next_element_seed(Project(Shape::Part))? {
                parts.push(part);
            }
        } else {
            while seq.next_element::<DiscardValue>()?.is_some() {}
        }
        Ok(match self.0 {
            Shape::Content => Value::Array(parts),
            Shape::Presence => true.into(),
            _ => Value::Null,
        })
    }
    fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<Value, E> {
        Ok(match self.0 {
            Shape::Content | Shape::Part => {
                let prefix = text.trim_start().chars().take(96).collect::<String>();
                if prefix.is_empty() && !text.is_empty() {
                    " ".into()
                } else {
                    prefix.into()
                }
            }
            Shape::Presence => true.into(),
            _ => Value::Null,
        })
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Value, E> {
        Ok(if matches!(self.0, Shape::Presence) {
            true.into()
        } else {
            Value::Null
        })
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.0, Shape::Presence) {
            true.into()
        } else {
            Value::Null
        })
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.0, Shape::Presence) {
            true.into()
        } else {
            Value::Null
        })
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Value, E> {
        Ok(if matches!(self.0, Shape::Presence) {
            true.into()
        } else {
            Value::Null
        })
    }
}
pub(super) fn read(input: impl Read) -> Result<Value> {
    let mut de = serde_json::Deserializer::from_reader(input);
    let value = Project(Shape::Message).deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

pub(super) fn source(source: &ByteSource) -> Result<Value> {
    if let Some(result) = source.with_bytes(|bytes| -> Result<Value> {
        let mut de = serde_json::Deserializer::from_slice(bytes);
        let value = Project(Shape::Message).deserialize(&mut de)?;
        de.end()?;
        Ok(value)
    })? {
        result
    } else {
        read(source.reader()?)
    }
}
