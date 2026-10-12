//! Read-only document sources and borrowed structural slices. Strict scalar/depth validation
//! remains in `store::json`; these indexes describe the bytes rather than retain their payloads.

use crate::lazy::{Span, Text, INLINE_MAX};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Source bytes are mapped in native builds. The fallback owns one byte buffer, without a
/// simultaneous decoded document tree. Export cache hits can read just a selected range.
pub(crate) struct Source {
    bytes: Bytes,
    index: Index,
    base: u64,
}

enum Bytes {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped(m) => m,
            Self::Owned(v) => v,
        }
    }
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(feature = "mmap")]
        let bytes = {
            let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            // Transcripts are read-only inputs. The mapping stays alive through every borrowed
            // descriptor; adapters never mutate its backing file.
            if file.metadata()?.len() == 0 {
                Bytes::Owned(Vec::new())
            } else {
                Bytes::Mapped(
                    unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mapping {}", path.display()))?,
                )
            }
        };
        #[cfg(not(feature = "mmap"))]
        let bytes = Bytes::Owned(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        Ok(Self {
            bytes,
            index: Index::default(),
            base: 0,
        })
    }

    #[cfg(not(feature = "mmap"))]
    pub fn range(path: &Path, offset: u64, len: u64) -> Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(len).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 == len,
            "document range changed in {}",
            path.display()
        );
        Ok(Self {
            bytes: Bytes::Owned(bytes),
            index: Index::default(),
            base: offset,
        })
    }

    pub fn raw(&self) -> Result<Raw<'_>> {
        let raw: &crate::lazy::RawValue = serde_json::from_slice(&self.bytes).context("not JSON")?;
        Ok(Raw::new(raw.get(), &self.index))
    }

    pub fn slice(&self, offset: u64, len: u64) -> Option<Raw<'_>> {
        let start = usize::try_from(offset.checked_sub(self.base)?).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        let text = std::str::from_utf8(self.bytes.get(start..end)?).ok()?;
        Some(Raw::new(text, &self.index))
    }

    pub fn location(&self, raw: Raw<'_>) -> Option<(u64, u64)> {
        let b = raw.get().as_bytes();
        let off = (b.as_ptr() as usize).checked_sub(self.bytes.as_ptr() as usize)?;
        (off.checked_add(b.len())? <= self.bytes.len()).then_some((self.base + off as u64, b.len() as u64))
    }

    pub fn text(&self, raw: Raw<'_>, spans: bool) -> Option<Text> {
        let b = raw.get().as_bytes();
        if b.first() != Some(&b'"') || b.last() != Some(&b'"') || b.len() < 2 {
            return None;
        }
        if spans && b.len() - 2 > INLINE_MAX {
            let (offset, len) = self.location(raw)?;
            Some(Text::Span(Span {
                source: None,
                offset: offset + 1,
                len: len - 2,
                escaped: memchr::memchr(b'\\', &b[1..b.len() - 1]).is_some(),
            }))
        } else {
            serde_json::from_str::<String>(raw.get()).ok().map(Text::Inline)
        }
    }
}

/// Slices inside a RawValue already validated by serde at the file/line boundary. Walking a
/// shallow object must not repeatedly validate its potentially enormous string descendants.
#[derive(Clone, Copy)]
pub(crate) struct Raw<'a>(&'a str, &'a Index);

/// Memoize only long string endpoints. Nested payload wrappers then visit each large body once;
/// entries contain two offsets, never text, and live only as long as this source traversal.
#[derive(Default)]
pub(crate) struct Index(std::cell::RefCell<BTreeMap<usize, usize>>);
impl<'a> Raw<'a> {
    /// `raw` must be a serde-validated RawValue or a slice returned by this module.
    pub fn new(raw: &'a str, index: &'a Index) -> Self {
        Self(raw, index)
    }
    pub fn get(self) -> &'a str {
        self.0
    }
}

pub(crate) fn accepts_value(raw: Raw<'_>) -> bool {
    let b = raw.get().as_bytes();
    super::json::value_compatible_with(b, |start, end| {
        if end - start > INLINE_MAX {
            raw.1
                 .0
                .borrow_mut()
                .insert(b.as_ptr() as usize + start, b.as_ptr() as usize + end);
        }
    })
}

pub(crate) struct Record<'a> {
    pub fields: BTreeMap<String, Raw<'a>>,
}
impl<'a> Record<'a> {
    pub fn parse(raw: Raw<'a>) -> Option<Self> {
        let b = raw.get().as_bytes();
        if b.first() != Some(&b'{') {
            return None;
        }
        let mut fields = BTreeMap::new();
        let mut i = 1;
        loop {
            whitespace(b, &mut i);
            if b.get(i) == Some(&b'}') {
                break;
            }
            let start = i;
            i = string_end(b, i, raw.1)?;
            let key = serde_json::from_str(&raw.get()[start..i]).ok()?;
            whitespace(b, &mut i);
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            whitespace(b, &mut i);
            let start = i;
            i = value_end(b, i, raw.1)?;
            fields.insert(key, Raw(&raw.get()[start..i], raw.1));
            whitespace(b, &mut i);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b'}') => break,
                _ => return None,
            }
        }
        Some(Self { fields })
    }
    pub fn get(&self, key: &str) -> Option<Raw<'a>> {
        self.fields.get(key).copied()
    }
    pub fn value(&self, key: &str) -> Option<Value> {
        serde_json::from_str(self.get(key)?.get()).ok()
    }
    pub fn without(&self, keys: &[&str]) -> Result<serde_json::Map<String, Value>> {
        self.fields
            .iter()
            .filter(|(key, _)| !keys.contains(&key.as_str()))
            .map(|(key, raw)| Ok((key.clone(), serde_json::from_str(raw.get())?)))
            .collect()
    }
    pub fn string(&self, key: &str) -> Option<String> {
        serde_json::from_str(self.fields.get(key)?.get()).ok()
    }
}

pub(crate) fn array(raw: Raw<'_>) -> Option<Vec<Raw<'_>>> {
    let b = raw.get().as_bytes();
    if b.first() != Some(&b'[') {
        return None;
    }
    let mut items = Vec::new();
    let mut i = 1;
    loop {
        whitespace(b, &mut i);
        if b.get(i) == Some(&b']') {
            break;
        }
        let start = i;
        i = value_end(b, i, raw.1)?;
        items.push(Raw(&raw.get()[start..i], raw.1));
        whitespace(b, &mut i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b']') => break,
            _ => return None,
        }
    }
    Some(items)
}

fn whitespace(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}
fn string_end(b: &[u8], i: usize, index: &Index) -> Option<usize> {
    let key = b.as_ptr() as usize + i;
    if let Some(end) = index.0.borrow().get(&key) {
        return Some(*end - b.as_ptr() as usize);
    }
    let end = super::json::string_end(b, i)?;
    if end - i > INLINE_MAX {
        index.0.borrow_mut().insert(key, b.as_ptr() as usize + end);
    }
    Some(end)
}
fn value_end(b: &[u8], mut i: usize, index: &Index) -> Option<usize> {
    match b.get(i)? {
        b'"' => string_end(b, i, index),
        b'{' | b'[' => {
            let mut depth = 1usize;
            i += 1;
            while depth != 0 {
                match b.get(i)? {
                    b'"' => {
                        i = string_end(b, i, index)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            Some(i)
        }
        _ => {
            let start = i;
            while b
                .get(i)
                .is_some_and(|c| !c.is_ascii_whitespace() && !b",]}".contains(c))
            {
                i += 1;
            }
            (i > start).then_some(i)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shallow_slices_preserve_validated_json_semantics() {
        for input in [
            r#"{"a":1,"a":2,"\u0061":3,"comma,key":{"q":"escaped\"quote\\slash","a":[1,-2.5e+3,true,false,null,[],{}]},"尾":"🌙"}"#,
            "{ \n \"a\" : [ 1e-5, {\"a\": \"\\uD83C\\uDF19\"}, \"end\" ], \"z\" : null }",
        ] {
            let validated: &crate::lazy::RawValue = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(input).unwrap();
            let index = Index::default();
            let actual = Record::parse(Raw::new(validated.get(), &index)).unwrap();
            assert_eq!(actual.fields.len(), expected.as_object().unwrap().len());
            for (key, raw) in actual.fields {
                let value: Value = serde_json::from_str(raw.get()).unwrap();
                assert_eq!(value, expected[&key]);
                if let Some(items) = array(raw) {
                    assert_eq!(items.len(), value.as_array().unwrap().len());
                    for (raw, expected) in items.into_iter().zip(value.as_array().unwrap()) {
                        assert_eq!(serde_json::from_str::<Value>(raw.get()).unwrap(), *expected);
                    }
                }
            }
        }
        for input in ["null", "true", "123", "[]", "\"string\""] {
            let index = Index::default();
            assert!(Record::parse(Raw::new(input, &index)).is_none());
        }
    }
}

/// The same joined text/title as Message::text, retaining only the first 81 characters. When
/// discovery filters whitespace-only text, scan any remaining whitespace with bounded state.
pub(crate) fn text_summary<'a>(
    texts: impl Iterator<Item = &'a crate::lazy::Text>,
    resolver: &crate::lazy::Resolver,
    require_nonwhite: bool,
) -> Option<String> {
    let mut head = String::new();
    let mut chars = 0;
    let mut present = false;
    let mut nonwhite = false;
    for (index, text) in texts.enumerate() {
        if (present || (require_nonwhite && index > 0)) && chars < 81 {
            head.push('\n');
            chars += 1;
        }
        let mut feed = |chunk: &str| {
            present |= !chunk.is_empty();
            nonwhite |= chunk.chars().any(|c| !c.is_whitespace());
            for c in chunk.chars().take(81 - chars) {
                head.push(c);
                chars += 1;
            }
        };
        match text {
            crate::lazy::Text::Inline(s) => feed(s),
            crate::lazy::Text::Span(sp) => {
                let prefix = resolver.resolve_prefix(sp, 1024);
                feed(&prefix);
                if require_nonwhite && !nonwhite && sp.len > 1024 {
                    resolver.for_each_chunk(sp, 4096, |chunk| nonwhite |= chunk.chars().any(|c| !c.is_whitespace()));
                }
            }
        }
        if chars == 81 && (!require_nonwhite || nonwhite) {
            break;
        }
    }
    (present && (!require_nonwhite || nonwhite)).then(|| crate::ir::truncate(&head, 80))
}
