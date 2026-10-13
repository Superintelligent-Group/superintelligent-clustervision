//! One transcript line parsed for the passes that read neither `extra` nor every record (lazy,
//! bulk), in one scan: the record as a `Value`, minus the two sidecars those passes never open.
//!
//! `toolUseResult` (Claude's structured copy of a tool's output: patches, file bodies, stdout)
//! keeps only `agentId` and `status`, which tell a sub-agent's return from a launch receipt; the
//! `attachment` object (hook output, file snippets, skill listings — up to 40% of a real
//! transcript) keeps only `type` and the `systemPrompt` sections. A lean reader treats any other
//! shape of either as absent, which is what this builds for it. Any parse error falls back to the
//! caller's plain `Value` parse.

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;

/// Parse one record line (`slice`, already trimmed).
pub(super) fn parse(slice: &[u8]) -> serde_json::Result<Value> {
    let mut de = serde_json::Deserializer::from_slice(slice);
    let v = Top.deserialize(&mut de)?;
    de.end()?;
    Ok(v)
}

/// The positioned transport does not retain a physical line beside the owned fallback payload.
#[cfg(not(feature = "mmap"))]
pub(super) fn parse_reader(reader: impl std::io::Read) -> serde_json::Result<Value> {
    let mut de = serde_json::Deserializer::from_reader(reader);
    let v = Top.deserialize(&mut de)?;
    de.end()?;
    Ok(v)
}

struct Top;

impl<'de> DeserializeSeed<'de> for Top {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Top {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a transcript record object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let val = match key.as_str() {
                "toolUseResult" => map.next_value_seed(Keep(&["agentId", "status"]))?,
                "attachment" => map.next_value_seed(Keep(&["type", "systemPrompt"]))?,
                _ => map.next_value::<Value>()?,
            };
            out.insert(key, val);
        }
        Ok(Value::Object(out))
    }
}

/// An object with only these keys built; anything else as `Null`.
struct Keep(&'static [&'static str]);

impl<'de> DeserializeSeed<'de> for Keep {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Keep {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if self.0.contains(&key.as_str()) {
                let v = map.next_value::<Value>()?;
                out.insert(key, v);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Value::Object(out))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E>(self, _: &str) -> Result<Value, E> {
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidecars_keep_what_a_lean_reader_opens() {
        let line = br#"{"type":"user","toolUseResult":{"stdout":"big","agentId":"a1","status":"completed","structuredPatch":[1]},"attachment":{"type":"hook_success","stdout":"x","systemPrompt":["s"]},"message":{"content":"hi"}}"#;
        assert_eq!(
            parse(line).unwrap(),
            serde_json::json!({"type":"user","toolUseResult":{"agentId":"a1","status":"completed"},"attachment":{"type":"hook_success","systemPrompt":["s"]},"message":{"content":"hi"}})
        );
        let bare = br#"{"type":"user","toolUseResult":"Error: Exit code 64"}"#;
        assert_eq!(parse(bare).unwrap()["toolUseResult"], Value::Null);
    }
}
