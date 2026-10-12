//! Borrowed recording payloads. Metadata and tool arguments keep their normal JSON shapes;
//! large text fields stay in the source until a consumer asks for them.

use super::*;
use crate::lazy::{Resolver, Span, Text, INLINE_MAX};

pub(super) use super::super::store::document::{accepts_value, array, Index, Raw, Record};

pub(super) struct Payload<'a> {
    content: std::cell::RefCell<Vec<Block>>,
    results: std::cell::RefCell<Vec<Option<Text>>>,
    tool_response: bool,
    resolver: &'a Resolver,
}

impl Payload<'_> {
    pub fn take_content(&self) -> Vec<Block> {
        std::mem::take(&mut *self.content.borrow_mut())
    }

    pub fn take_result(&self, i: usize) -> Option<Text> {
        self.results.borrow_mut().get_mut(i).and_then(Option::take)
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
        content: std::cell::RefCell::new(content),
        results: std::cell::RefCell::new(results),
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
