//! Borrowed recording payloads. Metadata and tool arguments keep their normal JSON shapes;
//! large text fields stay in the source until a consumer asks for them.

use super::*;
use crate::lazy::{Resolver, Span, Text, INLINE_MAX};

/// Slices inside a RawValue already validated by serde at the file/line boundary. Walking a
/// shallow object must not repeatedly validate its potentially enormous string descendants.
#[derive(Clone, Copy)]
pub(super) struct Raw<'a>(&'a str, &'a Index);

/// Memoize only long string endpoints. Nested payload wrappers then visit each large body once;
/// entries contain two offsets, never text, and live only as long as this source traversal.
#[derive(Default)]
pub(super) struct Index(std::cell::RefCell<BTreeMap<usize, usize>>);
impl<'a> Raw<'a> {
    /// `raw` must be a serde-validated RawValue or a slice returned by this module.
    pub fn new(raw: &'a str, index: &'a Index) -> Self {
        Self(raw, index)
    }
    pub fn get(self) -> &'a str {
        self.0
    }
}

pub(super) fn accepts_value(raw: Raw<'_>) -> bool {
    let b = raw.get().as_bytes();
    super::super::store::json::value_compatible_with(b, |start, end| {
        if end - start > INLINE_MAX {
            raw.1
                 .0
                .borrow_mut()
                .insert(b.as_ptr() as usize + start, b.as_ptr() as usize + end);
        }
    })
}

pub(super) struct Record<'a> {
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
    pub fn string(&self, key: &str) -> Option<String> {
        serde_json::from_str(self.fields.get(key)?.get()).ok()
    }
}

pub(super) fn array(raw: Raw<'_>) -> Option<Vec<Raw<'_>>> {
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
    let end = super::super::store::json::string_end(b, i)?;
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

pub(super) struct Payload<'a> {
    content: Vec<Block>,
    results: Vec<Option<Text>>,
    tool_response: bool,
    resolver: &'a Resolver,
}

impl Payload<'_> {
    pub fn content(&self) -> Vec<Block> {
        self.content.clone()
    }

    pub fn result(&self, i: usize) -> Option<Text> {
        self.results.get(i).cloned().flatten()
    }

    pub fn is_tool_response(&self) -> bool {
        self.tool_response
    }

    /// First non-whitespace characters, and the title's first 81 characters. Scanning has
    /// bounded state even for a pasted field that begins with megabytes of whitespace.
    fn inspect<'a>(&self, texts: impl Iterator<Item = &'a Text>) -> TextInfo {
        let mut info = TextInfo::default();
        let mut present = false;
        for text in texts {
            if present {
                info.feed("\n");
            }
            match text {
                Text::Inline(s) => info.feed(s),
                Text::Span(sp) => {
                    // Ordinary prompts need only a short prefix. Fall back to a bounded scan
                    // when leading whitespace hides the classification marker beyond it.
                    let mut prefix = info.clone();
                    prefix.feed(&self.resolver.resolve_prefix(sp, 4096));
                    if prefix.complete() || sp.len <= 4096 {
                        info = prefix;
                    } else {
                        self.resolver.for_each_chunk(sp, 4096, |s| info.feed(s));
                    }
                }
            }
            present |= !text.is_empty();
            if info.complete() {
                break;
            }
        }
        info
    }

    pub fn is_hook(&self, text: &Text) -> bool {
        self.inspect(std::iter::once(text))
            .trimmed
            .starts_with("<hook_context>")
    }

    pub fn user_facts(&self, m: &mut Message, st: &mut EmitState) {
        let info = self.inspect(m.content.iter().filter_map(|b| match b {
            Block::Text { text } => Some(text),
            _ => None,
        }));
        type_user_turn_from(m, &info.trimmed);
        if st.title.is_none() && info.has_text && !is_ignored_user_content(&info.trimmed) {
            st.title = Some(crate::ir::truncate(&info.head, 80));
        }
    }
}

#[derive(Clone, Default)]
struct TextInfo {
    head: String,
    head_chars: usize,
    trimmed: String,
    trimmed_chars: usize,
    has_text: bool,
    nonwhite: bool,
}

impl TextInfo {
    fn complete(&self) -> bool {
        self.head_chars == 81 && self.trimmed_chars == 64
    }

    fn feed(&mut self, s: &str) {
        self.has_text |= !s.is_empty();
        for c in s.chars() {
            if self.head_chars < 81 {
                self.head.push(c);
                self.head_chars += 1;
            }
            if !self.nonwhite && !c.is_whitespace() {
                self.nonwhite = true;
            }
            if self.nonwhite && self.trimmed_chars < 64 {
                self.trimmed.push(c);
                self.trimmed_chars += 1;
            }
            if self.head_chars == 81 && self.trimmed_chars == 64 {
                break;
            }
        }
    }
}

pub(super) fn emit(
    raw: Raw<'_>,
    bytes: &[u8],
    harness: Harness,
    st: &mut EmitState,
    resolver: &Resolver,
    sink: &mut dyn MessageSink,
) -> Flow {
    let Some(rec) = Record::parse(raw) else {
        return Flow::Continue;
    };
    let mut meta = serde_json::Map::new();
    for key in [
        "type",
        "id",
        "timestamp",
        "model",
        "tokens",
        "thoughts",
        "displayContent",
        "rewindTo",
    ] {
        if let Some(raw) = rec.fields.get(key) {
            if let Ok(v) = serde_json::from_str(raw.get()) {
                meta.insert(key.into(), v);
            }
        }
    }
    let mut content = Vec::new();
    let raw_content = rec.fields.get("content").copied();
    if let Some(raw) = raw_content {
        content_blocks(raw, bytes, &mut content);
    }
    let tool_response = raw_content.and_then(|r| array(r)).is_some_and(|parts| {
        !parts.is_empty()
            && parts
                .iter()
                .all(|p| Record::parse(*p).is_some_and(|p| p.fields.contains_key("functionResponse")))
    });
    let mut results = Vec::new();
    if let Some(calls) = rec.fields.get("toolCalls") {
        if let Some(calls) = array(*calls) {
            let calls: Vec<Value> = calls
                .iter()
                .map(|raw| {
                    let mut call_meta = serde_json::Map::new();
                    let call = Record::parse(*raw);
                    if let Some(call) = &call {
                        for key in ["id", "name", "args", "agentId", "status"] {
                            if let Some(raw) = call.fields.get(key) {
                                if let Ok(v) = serde_json::from_str(raw.get()) {
                                    call_meta.insert(key.into(), v);
                                }
                            }
                        }
                    }
                    results.push(call.and_then(|call| {
                        call.fields
                            .get("result")
                            .and_then(|raw| result_text(*raw, bytes, resolver))
                            .or_else(|| {
                                call.fields
                                    .get("resultDisplay")
                                    .and_then(|raw| string_text(*raw, bytes))
                            })
                    }));
                    Value::Object(call_meta)
                })
                .collect();
            meta.insert("toolCalls".into(), Value::Array(calls));
        }
    }
    let payload = Payload {
        content,
        results,
        tool_response,
        resolver,
    };
    emit_record_message_with(harness, &meta, st, Some(&payload), sink)
}

fn string_text(raw: Raw<'_>, bytes: &[u8]) -> Option<Text> {
    let b = raw.get().as_bytes();
    if b.first() != Some(&b'"') || b.last() != Some(&b'"') || b.len() < 2 {
        return None;
    }
    let off = (b.as_ptr() as usize).checked_sub(bytes.as_ptr() as usize)?;
    if off + b.len() > bytes.len() {
        return None;
    }
    let span = Span {
        source: None,
        offset: off as u64 + 1,
        len: b.len() as u64 - 2,
        escaped: memchr::memchr(b'\\', &b[1..b.len() - 1]).is_some(),
    };
    if span.len > INLINE_MAX as u64 {
        Some(Text::Span(span))
    } else {
        serde_json::from_str::<String>(raw.get()).ok().map(Text::Inline)
    }
}

fn value_text(raw: Raw<'_>, bytes: &[u8]) -> Text {
    if let Some(text) = string_text(raw, bytes) {
        return text;
    }
    if let Some(obj) = Record::parse(raw) {
        for key in ["output", "error"] {
            if let Some(text) = obj.fields.get(key).and_then(|r| string_text(*r, bytes)) {
                return text;
            }
        }
    }
    serde_json::from_str::<Value>(raw.get())
        .ok()
        .map(|v| value_to_text(&v))
        .unwrap_or_default()
        .into()
}

fn result_text(raw: Raw<'_>, bytes: &[u8], resolver: &Resolver) -> Option<Text> {
    if let Some(text) = string_text(raw, bytes) {
        return Some(text);
    }
    if let Some(parts) = array(raw) {
        let mut chunks = parts.into_iter().filter_map(|p| {
            let chunk = result_part(p, bytes, true);
            (!chunk.is_empty()).then_some(chunk)
        });
        let first = chunks.next()?;
        let Some(second) = chunks.next() else {
            return Some(first);
        };
        // A joined result has no contiguous source range. Only this genuinely composite field
        // needs an owned string; individual large outputs stay lazy.
        let mut joined = first.resolve(resolver).into_owned();
        joined.push('\n');
        joined.push_str(&second.resolve(resolver));
        for chunk in chunks {
            joined.push('\n');
            joined.push_str(&chunk.resolve(resolver));
        }
        return Some(joined.into());
    }
    if Record::parse(raw).is_some() {
        Some(result_part(raw, bytes, false))
    } else {
        None
    }
}

fn result_part(raw: Raw<'_>, bytes: &[u8], text_part: bool) -> Text {
    if let Some(obj) = Record::parse(raw) {
        if let Some(fr) = obj.fields.get("functionResponse") {
            return Record::parse(*fr)
                .and_then(|fr| fr.fields.get("response").map(|r| value_text(*r, bytes)))
                .unwrap_or_default();
        }
        if text_part {
            if let Some(text) = obj.fields.get("text").and_then(|r| string_text(*r, bytes)) {
                return text;
            }
        }
    }
    value_text(raw, bytes)
}

fn content_blocks(raw: Raw<'_>, bytes: &[u8], out: &mut Vec<Block>) {
    if let Some(text) = string_text(raw, bytes) {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
    } else if let Some(parts) = array(raw) {
        for part in parts {
            part_block(part, bytes, out);
        }
    } else {
        part_block(raw, bytes, out);
    }
}

fn part_block(raw: Raw<'_>, bytes: &[u8], out: &mut Vec<Block>) {
    if let Some(text) = string_text(raw, bytes) {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
        return;
    }
    let Some(obj) = Record::parse(raw) else {
        return;
    };
    if let Some(fc) = obj.fields.get("functionCall").filter(|r| r.get().starts_with('{')) {
        if let Ok(fc) = serde_json::from_str::<Value>(fc.get()) {
            push_part(out, &serde_json::json!({"functionCall": fc}));
        }
    } else if let Some(fr) = obj.fields.get("functionResponse").filter(|r| r.get().starts_with('{')) {
        if let Some(fr) = Record::parse(*fr) {
            let name = fr.string("name").unwrap_or_else(|| "tool".into());
            let id = fr.string("id").unwrap_or_else(|| format!("{name}-call"));
            out.push(Block::ToolResult {
                tool_use_id: id,
                content: fr
                    .fields
                    .get("response")
                    .map(|r| value_text(*r, bytes))
                    .unwrap_or_default(),
                is_error: false,
                tool_name: Some(name),
                status: None,
                details: None,
            });
        }
    } else if let Some(inline) = obj.fields.get("inlineData").filter(|r| r.get().starts_with('{')) {
        if let Some(inline) = Record::parse(*inline) {
            out.push(Block::Image {
                media_type: inline.string("mimeType"),
                data_ref: None,
            });
        }
    } else if let Some(fd) = obj.fields.get("fileData").filter(|r| r.get().starts_with('{')) {
        if let Some(fd) = Record::parse(*fd) {
            out.push(Block::File {
                mime: fd.string("mimeType"),
                path: None,
                source: fd.string("fileUri"),
            });
        }
    } else if let Some(text) = obj.fields.get("text").and_then(|r| string_text(*r, bytes)) {
        if !text.is_empty() {
            let thought = obj.fields.get("thought").is_some_and(|r| r.get() == "true");
            out.push(if thought {
                Block::Thinking {
                    text,
                    signature: None,
                    encrypted: None,
                    redacted: false,
                }
            } else {
                Block::Text { text }
            });
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
