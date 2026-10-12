//! Workspace chat tabs share the same strict array projection and immutable staged bytes.
use super::composer::{scalars, Array};
use super::*;
use crate::harness::store::{bytes::ByteSource, DiscardValue};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

#[derive(Default)]
pub(super) struct Legacy {
    pub tabs: Option<(usize, Vec<Tab>)>,
}
#[derive(Default)]
pub(super) struct Tab {
    pub metadata: Value,
    pub bubbles: Option<(usize, usize)>,
}
impl Legacy {
    pub fn replay(&self, source: &ByteSource, tab: usize, f: &mut dyn FnMut(Value) -> Flow) -> Result<()> {
        let Some((root, tabs)) = &self.tabs else { return Ok(()) };
        let Some((bubbles, _)) = tabs[tab].bubbles else {
            return Ok(());
        };
        pass(source, Some((*root, tab, bubbles)), f)?;
        Ok(())
    }
}
pub(super) fn header(source: &ByteSource) -> Option<Legacy> {
    pass(source, None, &mut |_| Flow::Continue).ok()
}
fn pass(
    source: &ByteSource,
    target: Option<(usize, usize, usize)>,
    f: &mut dyn FnMut(Value) -> Flow,
) -> Result<Legacy> {
    let mut stopped = false;
    let mut callback = |value| {
        let flow = f(value);
        stopped = flow == Flow::Stop;
        flow
    };
    let result = if let Some(result) = source.with_bytes(|bytes| -> Result<Legacy> {
        let mut de = serde_json::Deserializer::from_slice(bytes);
        let out = Root {
            target,
            f: &mut callback,
        }
        .deserialize(&mut de)?;
        de.end()?;
        Ok(out)
    })? {
        result
    } else {
        parse_reader(source.reader()?, target, &mut callback)
    };
    // Only a callback's explicit Stop turns serde's intentional unwind into success.
    if stopped {
        Ok(Legacy::default())
    } else {
        result
    }
}
fn parse_reader(
    input: impl std::io::Read,
    target: Option<(usize, usize, usize)>,
    f: &mut dyn FnMut(Value) -> Flow,
) -> Result<Legacy> {
    let mut de = serde_json::Deserializer::from_reader(input);
    let out = Root { target, f }.deserialize(&mut de)?;
    de.end()?;
    Ok(out)
}
struct Root<'a> {
    target: Option<(usize, usize, usize)>,
    f: &'a mut dyn FnMut(Value) -> Flow,
}
impl<'de> DeserializeSeed<'de> for Root<'_> {
    type Value = Legacy;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<Legacy, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Root<'_> {
    type Value = Legacy;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("legacy chatdata")
    }
    scalars!();
    fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> std::result::Result<Legacy, S::Error> {
        DiscardValue::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        Ok(Legacy::default())
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<Legacy, M::Error> {
        let mut out = Legacy::default();
        let mut member = 0;
        while let Some(key) = map.next_key::<String>()? {
            if key == "tabs" {
                if self.target.is_none() || self.target.is_some_and(|target| target.0 == member) {
                    out.tabs = map
                        .next_value_seed(Tabs {
                            target: self.target.map(|(_, tab, bubbles)| (tab, bubbles)),
                            f: self.f,
                        })?
                        .map(|tabs| (member, tabs));
                } else {
                    map.next_value::<DiscardValue>()?;
                }
                member += 1;
            } else {
                map.next_value::<DiscardValue>()?;
            }
        }
        Ok(out)
    }
}
struct Tabs<'a> {
    target: Option<(usize, usize)>,
    f: &'a mut dyn FnMut(Value) -> Flow,
}
impl<'de> DeserializeSeed<'de> for Tabs<'_> {
    type Value = Option<Vec<Tab>>;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<Self::Value, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Tabs<'_> {
    type Value = Option<Vec<Tab>>;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("chat tabs array")
    }
    scalars!();
    fn visit_map<M: MapAccess<'de>>(self, map: M) -> std::result::Result<Self::Value, M::Error> {
        DiscardValue::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(None)
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> std::result::Result<Self::Value, S::Error> {
        let mut tabs = Vec::new();
        let mut index = 0;
        loop {
            if let Some((tab, bubbles)) = self.target {
                if index == tab {
                    if seq
                        .next_element_seed(TabSeed {
                            target: Some(bubbles),
                            f: self.f,
                        })?
                        .is_none()
                    {
                        break;
                    }
                } else if seq.next_element::<DiscardValue>()?.is_none() {
                    break;
                }
            } else {
                let Some(tab) = seq.next_element_seed(TabSeed {
                    target: None,
                    f: self.f,
                })?
                else {
                    break;
                };
                tabs.push(tab);
            }
            index += 1;
        }
        Ok(Some(tabs))
    }
}
struct TabSeed<'a> {
    target: Option<usize>,
    f: &'a mut dyn FnMut(Value) -> Flow,
}
impl<'de> DeserializeSeed<'de> for TabSeed<'_> {
    type Value = Tab;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<Tab, D::Error> {
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for TabSeed<'_> {
    type Value = Tab;
    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("chat tab")
    }
    scalars!();
    fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> std::result::Result<Tab, S::Error> {
        DiscardValue::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))?;
        Ok(Tab::default())
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<Tab, M::Error> {
        let mut out = Tab {
            metadata: Value::Object(Default::default()),
            ..Default::default()
        };
        let mut member = 0;
        while let Some(key) = map.next_key::<String>()? {
            if key == "bubbles" {
                out.bubbles = map
                    .next_value_seed(Array {
                        f: (self.target == Some(member)).then_some(&mut *self.f),
                    })?
                    .map(|count| (member, count));
                member += 1;
            } else if self.target.is_none() && matches!(key.as_str(), "tabId" | "chatTitle") {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_tab_projection_preserves_selection_and_strict_tail() {
        for text in [
            r#"{"tabs":[{"tabId":"a","chatTitle":"A","bubbles":[1,2]},{"tabId":"a","bubbles":[3]}]}"#,
            r#"{"tabs":[{"tabId":"obsolete","bubbles":[1]}],"tabs":[{"tabId":"a","bubbles":[2],"bubbles":[3]}]}"#,
            r#"{"tabs":[{"tabId":"a","bubbles":[1],"bubbles":false}]}"#,
            r#"{"tabs":[null,1,[],{"tabId":"a","bubbles":[]}]}"#,
            r#"{"tabs":[{"tabId":"a","bubbles":[1]}],"ignored":"\ud800"}"#,
            r#"{"tabs":[{"tabId":"a","bubbles":[1]}],"ignored":1e400}"#,
            r#"{"tabs":[{"tabId":"a","bubbles":[1]}]} trailing"#,
            "null",
            "[]",
            r#"{"tabs":false}"#,
        ] {
            let old = serde_json::from_str::<Value>(text).ok();
            let source = ByteSource::read(text.as_bytes()).unwrap();
            let header = header(&source);
            assert_eq!(header.is_some(), old.is_some(), "{text}");
            let Some(header) = header else { continue };
            let old = old.unwrap();
            let tabs = old.get("tabs").and_then(Value::as_array);
            assert_eq!(header.tabs.is_some(), tabs.is_some());
            if let (Some((_, actual)), Some(expected)) = (&header.tabs, tabs) {
                assert_eq!(actual.len(), expected.len());
                for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                    for key in ["tabId", "chatTitle"] {
                        assert_eq!(actual.metadata.get(key), expected.get(key));
                    }
                    let bubbles = expected
                        .get("bubbles")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut replayed = Vec::new();
                    header
                        .replay(&source, index, &mut |v| {
                            replayed.push(v);
                            Flow::Continue
                        })
                        .unwrap();
                    assert_eq!(replayed, bubbles);
                    let mut stopped = 0;
                    header
                        .replay(&source, index, &mut |_| {
                            stopped += 1;
                            Flow::Stop
                        })
                        .unwrap();
                    assert_eq!(stopped, usize::from(!bubbles.is_empty()));
                }
            }
        }
    }
}
