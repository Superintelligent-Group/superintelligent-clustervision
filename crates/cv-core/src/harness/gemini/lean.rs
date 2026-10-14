//! Borrowed recording payloads. Metadata and tool arguments keep their normal JSON shapes;
//! large text fields stay in the source until a consumer asks for them.

#[cfg(not(feature = "mmap"))]
mod view;
#[cfg(not(feature = "mmap"))]
pub(super) use view::emit as emit_view;

pub(super) mod output;
use super::*;
use crate::lazy::{Resolver, Span, Text, INLINE_MAX};
use anyhow::Context as _;

pub(super) use super::super::store::document::{accepts_value, array, Index, Raw, Record};

pub(super) struct Payload<'a> {
    content: std::cell::RefCell<Vec<Block>>,
    results: std::cell::RefCell<Vec<Option<Text>>>,
    inputs: std::cell::RefCell<Vec<Option<crate::Json>>>,
    thoughts: Option<std::cell::RefCell<Vec<Block>>>,
    display_text: std::cell::RefCell<Option<String>>,
    tool_response: bool,
    resolver: &'a Resolver,
}

impl Payload<'_> {
    pub fn take_display_text(&self) -> Option<String> {
        self.display_text.borrow_mut().take()
    }

    pub fn take_content(&self) -> Vec<Block> {
        std::mem::take(&mut *self.content.borrow_mut())
    }

    pub fn take_result(&self, i: usize) -> Option<Text> {
        self.results.borrow_mut().get_mut(i).and_then(Option::take)
    }

    pub fn take_thoughts(&self) -> Option<Vec<Block>> {
        self.thoughts
            .as_ref()
            .map(|thoughts| std::mem::take(&mut *thoughts.borrow_mut()))
    }

    pub fn take_input(&self, i: usize) -> Option<crate::Json> {
        self.inputs.borrow_mut().get_mut(i).and_then(Option::take)
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
                Text::Source(source) => {
                    let _ = source.try_for_each_chunk(4096, |part| {
                        info.feed(part);
                        Ok(())
                    });
                }
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

/// Offsets describe the actual source, even when portable replay holds only one record buffer.
#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub bytes: &'a [u8],
    pub base: u64,
    pub source: Option<&'a crate::json::JsonSource>,
    pub arena: Option<&'a std::sync::Arc<crate::harness::store::bytes::ByteSource>>,
}
impl Context<'_> {
    fn offset(self, raw: Raw<'_>) -> Result<u64> {
        let b = raw.get().as_bytes();
        let off = (b.as_ptr() as usize)
            .checked_sub(self.bytes.as_ptr() as usize)
            .filter(|off| off.checked_add(b.len()).is_some_and(|end| end <= self.bytes.len()))
            .context("record field is outside its source bytes")?;
        self.base
            .checked_add(off as u64)
            .context("record field offset overflows")
    }
    fn argument(self, raw: Option<Raw<'_>>) -> Result<crate::Json> {
        let Some(raw) = raw else { return Ok(Value::Null.into()) };
        if raw.get().len() > INLINE_MAX {
            if let Some(source) = self.source {
                // The record boundary has already established complete Value-compatible JSON.
                return source.validated_range(self.offset(raw)?, raw.get().len() as u64);
            }
        }
        Ok(serde_json::from_str::<Value>(raw.get())?.into())
    }
}

pub(super) fn emit(
    raw: Raw<'_>,
    ctx: Context<'_>,
    harness: Harness,
    st: &mut EmitState,
    resolver: &Resolver,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    let Some(rec) = Record::parse(raw) else {
        return Ok(Flow::Continue);
    };
    let mut meta = serde_json::Map::new();
    for key in ["type", "id", "timestamp", "model", "tokens", "rewindTo"] {
        if let Some(raw) = rec.fields.get(key) {
            if let Ok(v) = serde_json::from_str(raw.get()) {
                meta.insert(key.into(), v);
            }
        }
    }
    let display_text = rec
        .fields
        .get("displayContent")
        .map(|raw| ctx.argument(Some(*raw))?.with_view(output::display_text))
        .transpose()?;
    let mut thoughts = Vec::new();
    if let Some(raw) = rec.fields.get("thoughts") {
        if let (Some(source), Some(arena)) = (ctx.source, ctx.arena) {
            let (headers, blocks) = ctx
                .argument(Some(*raw))?
                .with_view(|view| output::Output { source, arena }.thoughts(view))?;
            meta.insert("thoughts".into(), Value::Array(headers));
            thoughts = blocks;
        } else {
            meta.insert("thoughts".into(), serde_json::from_str(raw.get())?);
        }
    }
    let mut content = Vec::new();
    let tool_response = rec
        .fields
        .get("content")
        .map(|raw| content_blocks(*raw, ctx, &mut content))
        .transpose()?
        .unwrap_or(false);
    let mut results = Vec::new();
    let mut inputs = Vec::new();
    if let Some(calls) = rec.fields.get("toolCalls") {
        if let Some(calls) = array(*calls) {
            let calls: Vec<Value> = calls
                .iter()
                .map(|raw| {
                    let mut call_meta = serde_json::Map::new();
                    let call = Record::parse(*raw);
                    if let Some(call) = &call {
                        for key in ["id", "name", "agentId", "status"] {
                            if let Some(raw) = call.fields.get(key) {
                                if let Ok(v) = serde_json::from_str(raw.get()) {
                                    call_meta.insert(key.into(), v);
                                }
                            }
                        }
                    }
                    inputs.push(Some(
                        ctx.argument(call.as_ref().and_then(|c| c.fields.get("args").copied()))?,
                    ));
                    let result = call
                        .as_ref()
                        .and_then(|call| call.fields.get("result"))
                        .map(|raw| result_text(*raw, ctx))
                        .transpose()?
                        .flatten();
                    results.push(result.or_else(|| {
                        call.and_then(|call| call.fields.get("resultDisplay").and_then(|raw| string_text(*raw, ctx)))
                    }));
                    Ok(Value::Object(call_meta))
                })
                .collect::<Result<_>>()?;
            meta.insert("toolCalls".into(), Value::Array(calls));
        }
    }
    let payload = Payload {
        content: std::cell::RefCell::new(content),
        results: std::cell::RefCell::new(results),
        inputs: std::cell::RefCell::new(inputs),
        display_text: std::cell::RefCell::new(display_text),
        thoughts: (ctx.source.is_some() && ctx.arena.is_some()).then(|| std::cell::RefCell::new(thoughts)),
        tool_response,
        resolver,
    };
    Ok(emit_record_message_with(harness, &meta, st, Some(&payload), sink))
}

fn string_text(raw: Raw<'_>, ctx: Context<'_>) -> Option<Text> {
    let b = raw.get().as_bytes();
    if b.first() != Some(&b'"') || b.last() != Some(&b'"') || b.len() < 2 {
        return None;
    }
    let off = ctx.offset(raw).ok()?;
    let span = Span {
        source: None,
        offset: off + 1,
        len: b.len() as u64 - 2,
        escaped: memchr::memchr(b'\\', &b[1..b.len() - 1]).is_some(),
    };
    if span.len > INLINE_MAX as u64 {
        Some(Text::Span(span))
    } else {
        serde_json::from_str::<String>(raw.get()).ok().map(Text::Inline)
    }
}

fn value_text(raw: Raw<'_>, ctx: Context<'_>) -> Result<Text> {
    if let (Some(source), Some(arena)) = (ctx.source, ctx.arena) {
        return ctx
            .argument(Some(raw))?
            .with_view(|view| output::Output { source, arena }.value(view));
    }
    Ok(value_to_text(&serde_json::from_str::<Value>(raw.get())?).into())
}

fn result_text(raw: Raw<'_>, ctx: Context<'_>) -> Result<Option<Text>> {
    if let (Some(source), Some(arena)) = (ctx.source, ctx.arena) {
        return ctx
            .argument(Some(raw))?
            .with_view(|view| output::Output { source, arena }.result(view));
    }
    Ok(tool_result_text(Some(&serde_json::from_str::<Value>(raw.get())?)).map(Text::from))
}

// Return the codec's nonempty-array/all-functionResponse classification while dispatching.
fn content_blocks(raw: Raw<'_>, ctx: Context<'_>, out: &mut Vec<Block>) -> Result<bool> {
    if let Some(text) = string_text(raw, ctx) {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
    } else if let Some(parts) = array(raw) {
        let mut tool_response = !parts.is_empty();
        for part in parts {
            tool_response &= part_block(part, ctx, out)?;
        }
        return Ok(tool_response);
    } else {
        part_block(raw, ctx, out)?;
    }
    Ok(false)
}

fn part_block(raw: Raw<'_>, ctx: Context<'_>, out: &mut Vec<Block>) -> Result<bool> {
    if let Some(text) = string_text(raw, ctx) {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
        return Ok(false);
    }
    let Some(obj) = Record::parse(raw) else {
        return Ok(false);
    };
    if let Some(fc) = obj.fields.get("functionCall").filter(|r| r.get().starts_with('{')) {
        if let Some(fc) = Record::parse(*fc) {
            let name = fc.string("name").unwrap_or_else(|| "tool".into());
            let id = fc.string("id").unwrap_or_else(|| format!("{name}-call"));
            out.push(Block::ToolUse {
                id,
                name,
                input: ctx.argument(fc.fields.get("args").copied())?,
                namespace: None,
            });
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
                    .map(|r| value_text(*r, ctx))
                    .transpose()?
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
    } else if let Some(text) = obj.fields.get("text").and_then(|r| string_text(*r, ctx)) {
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
    // Presence is authoritative even when the value is scalar or functionCall wins dispatch.
    Ok(obj.fields.contains_key("functionResponse"))
}
