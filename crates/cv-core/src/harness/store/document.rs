//! Read-only document sources and borrowed structural slices. Strict scalar/depth validation
//! remains in `store::json`; these indexes describe the bytes rather than retain their payloads.

use crate::lazy::{Span, Text, INLINE_MAX};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
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
        {
            let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            Self::map_file(&file).with_context(|| format!("mapping {}", path.display()))
        }
        #[cfg(not(feature = "mmap"))]
        {
            Ok(Self {
                bytes: Bytes::Owned(std::fs::read(path).with_context(|| format!("reading {}", path.display()))?),
                index: Index::default(),
                base: 0,
            })
        }
    }

    /// Borrow bytes from an already-open read-only descriptor. JSONL callers retain that
    /// descriptor for selected-field replay and can fall back to buffered lines if mapping fails.
    #[cfg(feature = "mmap")]
    pub fn map_file(file: &std::fs::File) -> Result<Self> {
        // Transcripts are immutable inputs; callers never mutate their backing file. External
        // truncation has the same documented caveat as the other mapped adapters.
        let bytes = if file.metadata()?.len() == 0 {
            Bytes::Owned(Vec::new())
        } else {
            Bytes::Mapped(unsafe { memmap2::Mmap::map(file) }.context("mapping source")?)
        };
        Ok(Self {
            bytes,
            index: Index::default(),
            base: 0,
        })
    }

    #[cfg(feature = "mmap")]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
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

/// Memoize at most 1024 recent long string endpoints. Nested record wrappers reuse body bounds
/// without retaining offsets for the entire history. Eviction affects only scan cost, not data.
const ENDPOINTS: usize = 1024;
#[derive(Default)]
pub(crate) struct Index(std::cell::RefCell<Endpoints>);
#[derive(Default)]
struct Endpoints {
    ends: BTreeMap<usize, usize>,
    order: VecDeque<usize>,
}
impl Index {
    fn get(&self, key: usize) -> Option<usize> {
        self.0.borrow().ends.get(&key).copied()
    }
    fn remember(&self, start: usize, end: usize) {
        let mut cache = self.0.borrow_mut();
        if cache.ends.contains_key(&start) {
            return;
        }
        if cache.ends.len() == ENDPOINTS {
            if let Some(old) = cache.order.pop_front() {
                cache.ends.remove(&old);
            }
        }
        cache.ends.insert(start, end);
        cache.order.push_back(start);
    }
}
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
            raw.1.remember(b.as_ptr() as usize + start, b.as_ptr() as usize + end);
        }
    })
}

/// Syntax-validated object entries in wire order, including duplicate names.
struct ObjectIter<'a> {
    raw: Raw<'a>,
    cursor: usize,
    done: bool,
}
impl<'a> Iterator for ObjectIter<'a> {
    type Item = (String, Raw<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let b = self.raw.get().as_bytes();
        whitespace(b, &mut self.cursor);
        if b.get(self.cursor) == Some(&b'}') {
            self.done = true;
            return None;
        }
        let start = self.cursor;
        self.cursor = string_end(b, start, self.raw.1)?;
        let key = serde_json::from_str(&self.raw.get()[start..self.cursor]).ok()?;
        whitespace(b, &mut self.cursor);
        if b.get(self.cursor) != Some(&b':') {
            return None;
        }
        self.cursor += 1;
        whitespace(b, &mut self.cursor);
        let start = self.cursor;
        self.cursor = value_end(b, start, self.raw.1)?;
        let value = Raw(&self.raw.get()[start..self.cursor], self.raw.1);
        whitespace(b, &mut self.cursor);
        match b.get(self.cursor) {
            Some(b',') => self.cursor += 1,
            Some(b'}') => self.done = true,
            _ => return None,
        }
        Some((key, value))
    }
}
pub(crate) struct Record<'a> {
    raw: Raw<'a>,
    pub fields: BTreeMap<String, Raw<'a>>,
}
impl<'a> Record<'a> {
    /// Missing/nonobject nested fields behave as an empty object without allocating a Value.
    pub fn empty(raw: Raw<'a>) -> Self {
        Self {
            raw,
            fields: BTreeMap::new(),
        }
    }
    pub fn parse(raw: Raw<'a>) -> Option<Self> {
        if !raw.get().starts_with('{') {
            return None;
        }
        let mut entries = ObjectIter {
            raw,
            cursor: 1,
            done: false,
        };
        let fields = entries.by_ref().collect();
        entries.done.then_some(Self { raw, fields })
    }
    /// serde_json's preserve_order keeps a key's first position and its final value. Only
    /// callers that require navigation order retain the set; ordinary records retain no roster.
    pub fn values_in_order(&self) -> impl Iterator<Item = Raw<'a>> + '_ {
        let mut seen = HashSet::new();
        ObjectIter {
            raw: self.raw,
            cursor: 1,
            done: self.fields.is_empty(),
        }
        .filter_map(move |(key, _)| seen.insert(key.clone()).then(|| self.get(&key)).flatten())
    }
    pub fn get(&self, key: &str) -> Option<Raw<'a>> {
        self.fields.get(key).copied()
    }
    pub fn value(&self, key: &str) -> Option<Value> {
        serde_json::from_str(self.get(key)?.get()).ok()
    }
    /// Materialize only named fields after strict validation at the source boundary.
    pub fn project(&self, keys: &[&str]) -> Result<serde_json::Map<String, Value>> {
        keys.iter()
            .filter_map(|key| self.get(key).map(|raw| (*key, raw)))
            .map(|(key, raw)| Ok((key.to_string(), serde_json::from_str(raw.get())?)))
            .collect()
    }
    pub fn string(&self, key: &str) -> Option<String> {
        serde_json::from_str(self.fields.get(key)?.get()).ok()
    }
}

/// Chronological traversal keeps one descriptor regardless of record count. Input must already
/// be syntax validated, as with every structural slice operation in this module.
#[derive(Clone, Copy)]
pub(crate) struct ArrayIter<'a> {
    raw: Raw<'a>,
    cursor: usize,
    done: bool,
}
pub(crate) fn array_iter(raw: Raw<'_>) -> Option<ArrayIter<'_>> {
    raw.get().starts_with('[').then_some(ArrayIter {
        raw,
        cursor: 1,
        done: false,
    })
}
impl<'a> Iterator for ArrayIter<'a> {
    type Item = Raw<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let bytes = self.raw.get().as_bytes();
        whitespace(bytes, &mut self.cursor);
        if bytes.get(self.cursor) == Some(&b']') {
            self.done = true;
            return None;
        }
        let start = self.cursor;
        let end = value_end(bytes, start, self.raw.1)?;
        self.cursor = end;
        whitespace(bytes, &mut self.cursor);
        match bytes.get(self.cursor) {
            Some(b',') => self.cursor += 1,
            Some(b']') => self.done = true,
            _ => {
                self.done = true;
                return None;
            }
        }
        Some(Raw(&self.raw.get()[start..end], self.raw.1))
    }
}
/// Retained descriptors are reserved for replay/navigation that genuinely needs random access.
pub(crate) fn array(raw: Raw<'_>) -> Option<Vec<Raw<'_>>> {
    Some(array_iter(raw)?.collect())
}

fn whitespace(b: &[u8], i: &mut usize) {
    while b.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}
fn string_end(b: &[u8], i: usize, index: &Index) -> Option<usize> {
    let key = b.as_ptr() as usize + i;
    if let Some(end) = index.get(key) {
        return Some(end - b.as_ptr() as usize);
    }
    let end = super::json::string_end(b, i)?;
    if end - i > INLINE_MAX {
        index.remember(key, b.as_ptr() as usize + end);
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

/// The same joined text/title as Message::text, retaining only the first 81 characters. When
/// discovery filters whitespace-only text, scan any remaining whitespace with bounded state.
pub(crate) fn text_summary<T: std::borrow::Borrow<crate::lazy::Text>>(
    texts: impl Iterator<Item = T>,
    resolver: &crate::lazy::Resolver,
    require_nonwhite: bool,
) -> Option<String> {
    let mut head = String::new();
    let mut chars = 0;
    let mut present = false;
    let mut nonwhite = false;
    for (index, text) in texts.enumerate() {
        let text = text.borrow();
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
            let raw = Raw::new(input, &index);
            assert!(Record::parse(raw).is_none());
            let empty = Record::empty(raw);
            assert!(empty.fields.is_empty());
            assert_eq!(empty.values_in_order().count(), 0);
        }
    }
    #[test]
    fn array_iteration_and_endpoint_eviction_preserve_records() {
        let body = "a\\\"x".repeat(2048);
        let values = (0..ENDPOINTS * 3)
            .map(|n| serde_json::json!({"n":n,"body":body}))
            .collect::<Vec<_>>();
        let text = serde_json::to_string(&values).unwrap();
        let validated: &crate::lazy::RawValue = serde_json::from_str(&text).unwrap();
        let index = Index::default();
        let raw = Raw::new(validated.get(), &index);
        assert!(accepts_value(raw));
        assert!(index.0.borrow().ends.len() <= ENDPOINTS);
        let iterator = array_iter(raw).unwrap();
        assert_eq!(iterator.count(), values.len());
        for (raw, expected) in iterator.zip(values.iter()) {
            let record = Record::parse(raw).unwrap();
            assert_eq!(record.value("n").unwrap(), expected["n"]);
            assert_eq!(record.value("body").unwrap(), expected["body"]);
            assert!(index.0.borrow().ends.len() <= ENDPOINTS);
        }
    }
}
