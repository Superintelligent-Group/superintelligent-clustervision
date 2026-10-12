//! One rollout line parsed for the lean (not `complete`) passes in one scan: the record as a
//! `Value`, minus what no lean reader looks at.
//!
//! A `compacted` record re-records the whole conversation so far (`replacement_history`, plus
//! `guardian_history` and friends): 43% of the codex bytes on a real fleet, of which the parser
//! reads one number, the history's length. Here that payload keeps `message`, the window fields
//! and `replacement_history` as that many nulls; the rest is skipped without being built. On a
//! giant line a `response_item`'s `output` comes back raw (a `Null` holds its place in the
//! payload) so the span path can point at it instead of materializing hundreds of MB.
//!
//! The pruning keys off the record `type` and payload `type` having been read first, which is how
//! Codex writes them; a line in any other order is built whole. Any parse error falls back to the
//! caller's plain `Value` parse, so a line the lean parser cannot read reads exactly as before.

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::fmt;

/// The fields of a `compacted` payload [`super::handle_compacted`] reads.
const COMPACTED_KEEP: &[&str] = &["message", "window_number", "window_id", "compaction_response_id"];

/// A parsed record line, and (giant lines only) the raw `output` of its `response_item` payload.
pub(super) struct LeanLine<'a> {
    pub v: Value,
    pub output: Option<&'a RawValue>,
}

/// Parse one record line (`slice`, already trimmed). `giant` turns on the raw `output` capture.
pub(super) fn parse(slice: &[u8], giant: bool) -> serde_json::Result<LeanLine<'_>> {
    let mut de = serde_json::Deserializer::from_slice(slice);
    let mut output = None;
    let v = Top {
        giant,
        output: &mut output,
    }
    .deserialize(&mut de)?;
    de.end()?;
    Ok(LeanLine { v, output })
}

struct Top<'s, 'a> {
    giant: bool,
    output: &'s mut Option<&'a RawValue>,
}

impl<'de, 's> DeserializeSeed<'de> for Top<'s, 'de> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de, 's> Visitor<'de> for Top<'s, 'de> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a rollout record object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        let mut ty: Option<String> = None;
        while let Some(key) = map.next_key::<String>()? {
            let val = match (key.as_str(), ty.as_deref()) {
                ("payload", Some("compacted")) => map.next_value_seed(Compacted)?,
                ("payload", Some("response_item")) if self.giant => map.next_value_seed(Item {
                    output: &mut *self.output,
                })?,
                _ => map.next_value::<Value>()?,
            };
            if key == "type" {
                ty = val.as_str().map(str::to_string);
            }
            out.insert(key, val);
        }
        Ok(Value::Object(out))
    }
}

/// A `compacted` payload: [`COMPACTED_KEEP`] built, `replacement_history` as its length in nulls.
struct Compacted;

impl<'de> DeserializeSeed<'de> for Compacted {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Compacted {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a compacted payload")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "replacement_history" {
                let v = map.next_value_seed(Count)?;
                out.insert(key, v);
            } else if COMPACTED_KEEP.contains(&key.as_str()) {
                let v = map.next_value::<Value>()?;
                out.insert(key, v);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Value::Object(out))
    }
    // Anything but an object reads as nothing to [`super::handle_compacted`] either way.
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
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Value::Null)
    }
}

/// An array as that many nulls (its length is all a reader takes); anything else as `Null`.
struct Count;

impl<'de> DeserializeSeed<'de> for Count {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Count {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut n = 0usize;
        while seq.next_element::<IgnoredAny>()?.is_some() {
            n += 1;
        }
        Ok(Value::Array(vec![Value::Null; n]))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
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

/// A giant line's `response_item` payload: built, except `output`, captured raw.
struct Item<'s, 'a> {
    output: &'s mut Option<&'a RawValue>,
}

impl<'de, 's> DeserializeSeed<'de> for Item<'s, 'de> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de, 's> Visitor<'de> for Item<'s, 'de> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a response_item payload")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "output" {
                *self.output = Some(map.next_value::<&'de RawValue>()?);
                out.insert(key, Value::Null);
            } else {
                let v = map.next_value::<Value>()?;
                out.insert(key, v);
            }
        }
        Ok(Value::Object(out))
    }
    // A non-object payload: build it as it is (the giant path wants an object).
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
        Ok(Value::Bool(b))
    }
    fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
        Ok(Value::from(n))
    }
    fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
        Ok(Value::from(n))
    }
    fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
    }
    fn visit_str<E>(self, s: &str) -> Result<Value, E> {
        Ok(Value::String(s.to_string()))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element::<Value>()? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the lean parse builds of a record is what a plain parse builds, minus exactly the
    /// pruned fields.
    #[test]
    fn a_compacted_payload_keeps_what_the_parser_reads() {
        let line = br#"{"timestamp":"t","ordinal":3,"type":"compacted","payload":{"message":"m","replacement_history":[{"a":[1,2]},{"b":"x"},3],"guardian_history":[{"x":1}],"window_number":2,"window_id":"w","retained_context":{"y":2}}}"#;
        let got = parse(line, false).unwrap();
        assert!(got.output.is_none());
        assert_eq!(
            got.v,
            serde_json::json!({"timestamp":"t","ordinal":3,"type":"compacted","payload":{"message":"m","replacement_history":[null,null,null],"window_number":2,"window_id":"w"}})
        );
        // Any other record, and a payload read before its type, are built whole.
        for line in [
            &br#"{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Plan","text":"x"}}}"#[..],
            br#"{"payload":{"replacement_history":[1]},"type":"compacted"}"#,
        ] {
            let plain: Value = serde_json::from_slice(line).unwrap();
            assert_eq!(parse(line, true).unwrap().v, plain);
        }
    }

    #[test]
    fn a_giant_lines_output_comes_back_raw_in_place() {
        let line = br#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"big\nstring"}}"#;
        let got = parse(line, true).unwrap();
        assert_eq!(got.output.map(RawValue::get), Some(r#""big\nstring""#));
        let keys: Vec<&str> = got.v["payload"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["type", "call_id", "output"]);
        assert!(parse(line, false).unwrap().output.is_none());
    }
}
