//! LM Studio desktop app adapter — local chats under `~/.lmstudio/conversations/`.
//!
//! LM Studio is a closed-source local-LLM desktop app, but it stores its chat transcripts as
//! **plaintext JSON**, one file per conversation:
//!
//! - **Transcripts:** `~/.lmstudio/conversations/<ms-epoch>.conversation.json`. The filename stem is
//!   a millisecond-epoch id (also the chat's `createdAt`). There is no `cwd` — it's a chat app, not
//!   an agent, so [`Session::cwd`] is always `None`.
//! - **Top-level fields:** `name` (user/auto title, may be `null` or `"Untitled"`), `createdAt`
//!   (ms epoch), `messages[]`, `systemPrompt`, `tokenCount`, `pinned`, `perChatPredictionConfig`
//!   (holds a per-chat `llm.prediction.systemPrompt` override under `fields[]`).
//! - **`messages[]`:** each entry is `{versions:[...], currentlySelected:<idx>}`. LM Studio keeps
//!   every regenerated/edited variant of a turn in `versions[]`; we render the `currentlySelected`
//!   one (falling back to the last).
//! - **A version** is `{type, role, ...}`:
//!     * `type:"singleStep"` (typically `role:"user"`): `content:[{type:"text",text} |
//!       {type:"file",fileIdentifier|identifier,name?,fileType,sizeBytes}]`.
//!     * `type:"multiStep"` (`role:"assistant"`): `steps:[...]` where each step `type` ∈
//!       `contentBlock` | `debugInfoBlock` | `status`.
//!         - `contentBlock`: `content:[{type:"text",text}]`. A `style.type=="thinking"` marks the
//!           block as reasoning (`<think>`); these get [`Block::Thinking`]. `genInfo` carries the
//!           model (`indexedModelIdentifier`/`identifier`, e.g. `openai/gpt-oss-120b`) and `stats`
//!           (`promptTokensCount`/`predictedTokensCount` → [`Usage`]). The `stepIdentifier` is a
//!           `"<ms-epoch>-<rand>"` string; its prefix is a usable per-step timestamp.
//!         - `debugInfoBlock` / `status`: app-internal (naming technique, prompt-processing status);
//!           ignored for content.
//!
//! Fidelity notes / variants (sniffed by content, never a version field):
//! - There are **no first-class tool-call structures** on disk. Models like gpt-oss embed tool calls
//!   inline in the assistant text via channel markers; we preserve that text verbatim (no `ToolUse`).
//! - File attachments are references only (`fileIdentifier`), not inlined bytes; images → [`Block::Image`],
//!   other files → [`Block::File`]. The actual bytes live under `~/.lmstudio/.internal/files/`.
//! - Older/newer schema drift is handled by being totally tolerant: unknown step/part types are
//!   skipped, a `content` that is a string OR an array of `{type,text}` parts both work, and a
//!   missing `~/.lmstudio` dir yields an empty discover (never a panic).

use super::store::document::Fields;
use super::Adapter;
use super::EmitResult;
use crate::ir::*;
use crate::json::{JsonSource, JsonView};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
#[cfg(test)]
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct LmStudio {
    root: Option<PathBuf>,
}

impl LmStudio {
    /// Read an explicit conversations directory, without consulting installed harness homes.
    pub fn with_root(conversations: &Path) -> Self {
        Self {
            root: Some(conversations.to_path_buf()),
        }
    }

    pub fn detect() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".lmstudio").join("conversations"));
        LmStudio {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Adapter for LmStudio {
    fn harness(&self) -> Harness {
        Harness::LmStudio
    }

    fn roots(&self) -> Vec<PathBuf> {
        // Detect the install even if no conversations dir exists yet.
        self.root
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".lmstudio")).filter(|p| p.exists()))
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return Ok(vec![]),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !is_conversation_file(&path) {
                continue;
            }
            match scan(&path) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
            }
        }
        Ok(out)
    }

    /// Walk the validated document and selected versions with scoped source views. Lazy text
    /// keeps byte spans; unused versions, unknown fields and internal steps never become Values.
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let source = JsonSource::capture(&r.path)?;
            source
                .read_syntax(|view| {
                    let doc = Fields::parse_struct(
                        view,
                        &[
                            "createdAt",
                            "systemPrompt",
                            "perChatPredictionConfig",
                            "pinned",
                            "messages",
                        ],
                    )?
                    .context("LM Studio conversation is not an object")?;
                    // Match the original derived envelope: typed fields reject malformed/wrong
                    // values before callbacks; ignored fields remain grammar-only. The root consumes
                    // one recursion slot for its Value metadata, while each raw turn resets the budget.
                    for key in ["createdAt", "perChatPredictionConfig"] {
                        if let Some(value) = doc.get(key) {
                            anyhow::ensure!(value.accepts_value_at_depth(1)?, "invalid LM Studio {key}");
                        }
                    }
                    if let Some(value) = doc.get("systemPrompt") {
                        anyhow::ensure!(
                            (value.is_null()? || value.is_string()?) && value.accepts_value()?,
                            "invalid LM Studio systemPrompt"
                        );
                    }
                    if let Some(value) = doc.get("pinned") {
                        anyhow::ensure!(
                            value.is_null()? || value.as_bool()?.is_some(),
                            "invalid LM Studio pinned"
                        );
                    }
                    if let Some(value) = doc.get("messages") {
                        anyhow::ensure!(value.is_array()?, "invalid LM Studio messages");
                    }
                    let created_at = if let Some(value) = doc.get("createdAt") {
                        if value.is_string()? || value.as_number()?.is_some() {
                            doc.value("createdAt")?.as_ref().and_then(super::ts_from_value)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let mut s = Session {
                        cwd: None,
                        title: r.title.clone(),
                        created_at,
                        updated_at: file_mtime(&r.path).or(created_at),
                        source_path: Some(r.path.clone()),
                        system_prompt: projected_system_prompt(&doc)?,
                        ..Session::new(r.id.clone(), Harness::LmStudio)
                    };
                    if doc.get("pinned").map(JsonView::as_bool).transpose()?.flatten() == Some(true) {
                        s.harness_extra_mut(Harness::LmStudio)
                            .insert("pinned".into(), Value::Bool(true));
                    }
                    let mut session_model = None;
                    sink.meta(&s);
                    if let Some(messages) = doc.get("messages").map(JsonView::array).transpose()?.flatten() {
                        'turns: for turn in messages {
                            let turn = turn?;
                            if !turn.accepts_value()? {
                                continue;
                            }
                            let Some(fields) = Fields::parse(turn, &["versions", "currentlySelected"])? else {
                                continue;
                            };
                            let Some(versions) = fields.get("versions") else {
                                continue;
                            };
                            let Some((selected, version)) = projected_selected_version(&fields, versions)? else {
                                continue;
                            };
                            if let Some(message) = projected_version(version, &mut session_model, opts)? {
                                if sink.message(message) == Flow::Stop {
                                    break;
                                }
                            }
                            if opts.complete {
                                let mut throwaway_model = None;
                                if let Some(variants) = versions.array()? {
                                    for (index, version) in variants.enumerate() {
                                        let version = version?;
                                        if index == selected {
                                            continue;
                                        }
                                        if let Some(mut message) =
                                            projected_version(version, &mut throwaway_model, opts)?
                                        {
                                            message.kind = MessageKind::Branch;
                                            message
                                                .harness_extra_mut(Harness::LmStudio)
                                                .insert("variant".into(), Value::Number(index.into()));
                                            if sink.message(message) == Flow::Stop {
                                                break 'turns;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    s.model = session_model;
                    Ok(s)
                })
                .with_context(|| format!("parsing {}", r.path.display()))
        })
    }
}

/// Resolve only the effective system prompt, skipping unrelated prediction-config values.
fn projected_system_prompt(doc: &Fields<'_>) -> Result<Option<String>> {
    if let Some(config) = doc.get("perChatPredictionConfig") {
        if let Some(config) = Fields::parse(config, &["fields"])? {
            if let Some(fields) = config.get("fields").map(JsonView::array).transpose()?.flatten() {
                for field in fields {
                    let Some(field) = Fields::parse(field?, &["key", "value"])? else {
                        continue;
                    };
                    if string_is(field.get("key"), "llm.prediction.systemPrompt")? {
                        if let Some(value) = field.string("value")?.filter(|s| !s.trim().is_empty()) {
                            return Ok(Some(value));
                        }
                    }
                }
            }
        }
    }
    Ok(doc.string("systemPrompt")?.filter(|s| !s.trim().is_empty()))
}

/// Compare discriminators without owning an unknown, potentially enormous string.
fn string_is(view: Option<JsonView<'_>>, expected: &str) -> Result<bool> {
    let Some(view) = view else { return Ok(false) };
    let mut bytes = 0usize;
    let mut equal = true;
    let string = view.for_each_str_chunk(|chunk| {
        let end = bytes.saturating_add(chunk.len());
        equal &= expected.as_bytes().get(bytes..end) == Some(chunk.as_bytes());
        bytes = end;
        Ok(())
    })?;
    Ok(string && equal && bytes == expected.len())
}

/// Find the selected or final version with one retained descriptor, irrespective of variant count.
fn projected_selected_version<'a>(
    fields: &Fields<'a>,
    versions: JsonView<'a>,
) -> Result<Option<(usize, JsonView<'a>)>> {
    let wanted = fields
        .get("currentlySelected")
        .map(JsonView::as_number)
        .transpose()?
        .flatten()
        .and_then(|n| n.as_u64())
        .map(|i| i as usize);
    let Some(versions) = versions.array()? else {
        return Ok(None);
    };
    let mut selected = None;
    let mut last = None;
    for (index, version) in versions.enumerate() {
        let version = version?;
        if Some(index) == wanted {
            selected = Some((index, version));
        }
        last = Some((index, version));
    }
    Ok(selected.or(last))
}

fn projected_version(
    version: JsonView<'_>,
    session_model: &mut Option<String>,
    opts: &ParseOptions,
) -> Result<Option<Message>> {
    let Some(version) = Fields::parse(version, &["role", "type", "steps", "content"])? else {
        return Ok(None);
    };
    let role = if string_is(version.get("role"), "user")? {
        Role::User
    } else if string_is(version.get("role"), "assistant")? {
        Role::Assistant
    } else if string_is(version.get("role"), "system")? {
        Role::System
    } else {
        return Ok(None);
    };
    let mut message = Message::new(role);
    if role == Role::System {
        message.kind = MessageKind::SystemPrompt;
    }
    if string_is(version.get("type"), "multiStep")? {
        if let Some(steps) = version.get("steps").map(JsonView::array).transpose()?.flatten() {
            for step in steps {
                projected_step(step?, &mut message, session_model, opts)?;
            }
        }
    } else {
        projected_content(version.get("content"), &mut message, false, opts)?;
    }
    Ok((!message.content.is_empty()).then_some(message))
}

fn projected_step(
    step: JsonView<'_>,
    message: &mut Message,
    session_model: &mut Option<String>,
    opts: &ParseOptions,
) -> Result<()> {
    let Some(step) = Fields::parse(step, &["type", "genInfo", "stepIdentifier", "style", "content"])? else {
        return Ok(());
    };
    if !string_is(step.get("type"), "contentBlock")? {
        return Ok(());
    }
    if let Some(gen) = step.get("genInfo") {
        if let Some(gen) = Fields::parse(gen, &["indexedModelIdentifier", "identifier", "stats"])? {
            // A present wrong-type indexed identifier must keep masking the fallback identifier.
            let model = gen.get("indexedModelIdentifier").or_else(|| gen.get("identifier"));
            if let Some(model) = model.map(|v| v.text(false)).transpose()?.flatten() {
                let model = model.inline_str().expect("inline metadata");
                match session_model {
                    None => *session_model = Some(model.to_owned()),
                    Some(current) if current != model && message.model.is_none() => {
                        message.model = Some(model.to_owned())
                    }
                    _ => {}
                }
            }
            if message.usage.is_none() {
                if let Some(stats) = gen.get("stats") {
                    if let Some(stats) = Fields::parse(stats, &["promptTokensCount", "predictedTokensCount"])? {
                        let input = stats
                            .get("promptTokensCount")
                            .map(JsonView::as_number)
                            .transpose()?
                            .flatten()
                            .and_then(|n| n.as_u64());
                        let output = stats
                            .get("predictedTokensCount")
                            .map(JsonView::as_number)
                            .transpose()?
                            .flatten()
                            .and_then(|n| n.as_u64());
                        if input.is_some() || output.is_some() {
                            message.usage = Some(Usage {
                                input_tokens: input,
                                output_tokens: output,
                                ..Usage::default()
                            });
                        }
                    }
                }
            }
        }
    }
    if message.timestamp.is_none() {
        message.timestamp = projected_step_timestamp(step.get("stepIdentifier"))?;
    }
    let thinking = if let Some(style) = step.get("style") {
        if let Some(style) = Fields::parse(style, &["type"])? {
            string_is(style.get("type"), "thinking")?
        } else {
            false
        }
    } else {
        false
    };
    projected_content(step.get("content"), message, thinking, opts)
}

/// Only the integer prefix is demanded from a step identifier. Its random suffix (or leading
/// zeros in the prefix) may be enormous, so accumulate digits rather than owning the string.
fn projected_step_timestamp(view: Option<JsonView<'_>>) -> Result<Option<DateTime<Utc>>> {
    let Some(view) = view else { return Ok(None) };
    let mut value = Some(0i64);
    let mut first = true;
    let mut digits = false;
    let mut done = false;
    let string = view.for_each_str_chunk(|chunk| {
        if done {
            return Ok(());
        }
        for byte in chunk.bytes() {
            if byte == b'-' {
                done = true;
                break;
            }
            if first && byte == b'+' {
                first = false;
                continue;
            }
            first = false;
            if byte.is_ascii_digit() {
                digits = true;
                value = value.and_then(|v| v.checked_mul(10)?.checked_add(i64::from(byte - b'0')));
            } else {
                value = None;
            }
        }
        Ok(())
    })?;
    Ok(if string && digits {
        value
            .filter(|ms| (1_200_000_000_000..=4_000_000_000_000).contains(ms))
            .and_then(DateTime::from_timestamp_millis)
    } else {
        None
    })
}

fn projected_content(
    content: Option<JsonView<'_>>,
    message: &mut Message,
    thinking: bool,
    opts: &ParseOptions,
) -> Result<()> {
    let Some(content) = content else { return Ok(()) };
    if content.is_string()? {
        return projected_text(content, message, thinking, opts);
    }
    if let Some(parts) = content.array()? {
        for part in parts {
            let Some(part) = Fields::parse(
                part?,
                &["type", "text", "fileIdentifier", "identifier", "name", "fileType"],
            )?
            else {
                continue;
            };
            if string_is(part.get("type"), "text")? {
                if let Some(text) = part.get("text") {
                    projected_text(text, message, thinking, opts)?;
                }
            } else if string_is(part.get("type"), "file")? {
                let data_ref = part
                    .get("identifier")
                    .or_else(|| part.get("fileIdentifier"))
                    .map(|view| view.text(false))
                    .transpose()?
                    .flatten()
                    .map(|text| text.inline_str().expect("inline metadata").to_owned());
                let name = part.string("name")?;
                if string_is(part.get("fileType"), "image")? {
                    message.content.push(Block::Image {
                        media_type: None,
                        data_ref: data_ref.or(name),
                    });
                } else {
                    message.content.push(Block::File {
                        mime: part.string("fileType")?,
                        path: name,
                        source: data_ref,
                    });
                }
            }
        }
    }
    Ok(())
}

fn projected_text(view: JsonView<'_>, message: &mut Message, thinking: bool, opts: &ParseOptions) -> Result<()> {
    let Some(text) = view.text(opts.spans)? else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    if thinking {
        message.content.push(Block::Thinking {
            text,
            signature: None,
            encrypted: None,
            redacted: false,
        });
    } else {
        let harmony = if let Some(text) = text.inline_str() {
            super::harmony::looks_like_harmony(text)
        } else {
            projected_harmony(view)?
        };
        if harmony {
            let inline = view.text(false)?.expect("string view");
            message.content.extend(super::harmony::decode_content(
                inline.inline_str().expect("inline decoding"),
            ));
        } else {
            message.content.push(Block::Text { text });
        }
    }
    Ok(())
}

/// Harmony markers may cross decoder chunks or be Unicode escaped on disk. Scan decoded bytes
/// with only the longest marker's overlap; actual Harmony decoding retains its existing owned path.
fn projected_harmony(view: JsonView<'_>) -> Result<bool> {
    const MARKERS: [&[u8]; 3] = [b"<|channel|>", b"<|message|>", b"<|start|>assistant"];
    const OVERLAP: usize = 17;
    let mut tail = Vec::with_capacity(OVERLAP);
    let mut found = false;
    view.for_each_str_chunk(|chunk| {
        if !found {
            found = super::harmony::looks_like_harmony(chunk);
            let mut boundary = [0u8; OVERLAP * 2];
            boundary[..tail.len()].copy_from_slice(&tail);
            let head = chunk.len().min(OVERLAP);
            boundary[tail.len()..tail.len() + head].copy_from_slice(&chunk.as_bytes()[..head]);
            found |= MARKERS
                .iter()
                .any(|marker| memchr::memmem::find(&boundary[..tail.len() + head], marker).is_some());
            if chunk.len() >= OVERLAP {
                tail.clear();
            }
            tail.extend_from_slice(&chunk.as_bytes()[chunk.len().saturating_sub(OVERLAP)..]);
            if tail.len() > OVERLAP {
                tail.drain(..tail.len() - OVERLAP);
            }
        }
        Ok(())
    })?;
    Ok(found)
}

// ------------------------------------------------------------------------------------------------
// Emit (port-a-sesh target): the faithful inverse of `parse`.
// ------------------------------------------------------------------------------------------------

/// Emit `session` into LM Studio's native format: a single
/// `<out_dir>/conversations/<ms-epoch>.conversation.json` file. The filename stem is the chat id
/// (also `createdAt`, ms-epoch), exactly as the parser expects.
///
/// This is the inverse of [`parse`]. Each IR [`Message`] becomes one `messages[]` turn wrapped in
/// `{versions:[<version>], currentlySelected:0}`:
/// - **User/System** → a `singleStep` version with `content` parts (`text` / `file`).
/// - **Assistant** → a `multiStep` version whose `steps[]` are `contentBlock`s: text → a plain
///   block, [`Block::Thinking`] → a block tagged `style.type=="thinking"` (the form the parser reads
///   back as reasoning), and tool calls/results are flattened to readable text blocks (LM Studio has
///   no first-class tool structures on disk, so we don't try to fabricate any). Model + usage ride
///   on the first content step's `genInfo`, and a `stepIdentifier` carries the step timestamp.
///
/// LM Studio is a chat app with no working directory, so `opts.new_cwd` / `session.cwd` are not
/// written into the file; we surface the chosen cwd only in the resume hint.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<EmitResult> {
    emit::emit(session, out_dir, opts)
}
mod emit;
#[cfg(test)]

/// Build the single `version` object for one IR message turn (the parser only reads the
/// `currentlySelected` version, so we emit exactly one). Returns `None` for empty turns.
fn emit_version(session: &Session, msg: &Message, fallback: DateTime<Utc>) -> Result<Option<Value>> {
    Ok(match msg.role {
        Role::Assistant => {
            let steps = emit_assistant_steps(session, msg, fallback)?;
            if steps.is_empty() {
                return Ok(None);
            }
            Some(json!({
                "type": "multiStep",
                "role": "assistant",
                "steps": steps,
            }))
        }
        // User, System, and Tool turns all become a single-step turn with content parts.
        role => {
            let content = emit_content_parts(&msg.content)?;
            if content.is_empty() {
                return Ok(None);
            }
            let role_str = match role {
                Role::System => "system",
                _ => "user", // Tool results have no native shape; carry them on a user turn.
            };
            Some(json!({
                "type": "singleStep",
                "role": role_str,
                "content": content,
            }))
        }
    })
}

/// Map an assistant message's blocks into LM Studio `steps[]` (`contentBlock`s). Model + usage ride
/// on the first step's `genInfo`; the `stepIdentifier` prefix carries the step timestamp.
#[cfg(test)]
fn emit_assistant_steps(session: &Session, msg: &Message, fallback: DateTime<Utc>) -> Result<Vec<Value>> {
    let ts = msg.timestamp.unwrap_or(fallback);
    let step_id = format!("{}-0.{}", ts.timestamp_millis(), "0000000000000000");
    let model = msg.model.as_ref().or(session.model.as_ref());

    let mut steps: Vec<Value> = Vec::new();
    let mut attached_gen = false;

    let mut push_block = |content: Value, thinking: bool, steps: &mut Vec<Value>| {
        let mut step = Map::new();
        step.insert("type".into(), json!("contentBlock"));
        step.insert("role".into(), json!("assistant"));
        step.insert("content".into(), content);
        step.insert("stepIdentifier".into(), json!(step_id));
        if thinking {
            step.insert("style".into(), json!({ "type": "thinking" }));
        }
        // Attach model + usage to the first content block, mirroring how the parser reads them.
        if !attached_gen {
            let mut gen = Map::new();
            if let Some(m) = model {
                gen.insert("indexedModelIdentifier".into(), json!(m));
            }
            if let Some(u) = &msg.usage {
                let mut stats = Map::new();
                if let Some(i) = u.input_tokens {
                    stats.insert("promptTokensCount".into(), json!(i));
                }
                if let Some(o) = u.output_tokens {
                    stats.insert("predictedTokensCount".into(), json!(o));
                }
                if !stats.is_empty() {
                    gen.insert("stats".into(), Value::Object(stats));
                }
            }
            if !gen.is_empty() {
                step.insert("genInfo".into(), Value::Object(gen));
                attached_gen = true;
            }
        }
        steps.push(Value::Object(step));
    };

    for b in &msg.content {
        match b {
            Block::Text { text } => {
                push_block(text_parts(text), false, &mut steps);
            }
            Block::Thinking { text, .. } => {
                if !text.is_empty() {
                    push_block(text_parts(text), true, &mut steps);
                }
            }
            Block::ToolUse { name, input, .. } => {
                // No native tool structure: render a readable, self-describing text block. The
                // parser reads it back as text (round-trips as content, not as a ToolUse).
                let args = serde_json::to_string(input.resolve()?.as_ref())?;
                push_block(text_parts(&format!("[tool call: {name} {args}]")), false, &mut steps);
            }
            Block::ToolResult { content, .. } => {
                push_block(text_parts(&format!("[tool result: {content}]")), false, &mut steps);
            }
            Block::File { .. } | Block::Image { .. } => {
                if let Some(part) = emit_file_part(b) {
                    push_block(Value::Array(vec![part]), false, &mut steps);
                }
            }
        }
    }
    Ok(steps)
}

/// Map a (user/system/tool) message's blocks into a `content[]` parts array (`text` / `file`).
#[cfg(test)]
fn emit_content_parts(blocks: &[Block]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            // Reasoning is unusual on a user turn, but keep the text rather than drop it.
            Block::Thinking { text, .. } if !text.is_empty() => out.push(json!({ "type": "text", "text": text })),
            Block::ToolResult { content, .. } => {
                out.push(json!({ "type": "text", "text": format!("[tool result: {content}]") }))
            }
            Block::ToolUse { name, input, .. } => {
                let args = serde_json::to_string(input.resolve()?.as_ref())?;
                out.push(json!({ "type": "text", "text": format!("[tool call: {name} {args}]") }))
            }
            Block::File { .. } | Block::Image { .. } => {
                if let Some(part) = emit_file_part(b) {
                    out.push(part);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// A `content[]` array holding a single `text` part.
#[cfg(test)]
fn text_parts(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Map a [`Block::File`] / [`Block::Image`] into LM Studio's `{type:"file", ...}` part (a reference,
/// matching how the parser reads `identifier`/`fileIdentifier`, `name`, `fileType`).
fn emit_file_part(b: &Block) -> Option<Value> {
    let mut part = Map::new();
    part.insert("type".into(), json!("file"));
    match b {
        Block::Image { media_type, data_ref } => {
            part.insert("fileType".into(), json!("image"));
            if let Some(r) = data_ref {
                part.insert("identifier".into(), json!(r));
                part.insert("name".into(), json!(r));
            }
            if let Some(mt) = media_type {
                part.insert("mimeType".into(), json!(mt));
            }
        }
        Block::File { mime, path, source } => {
            if let Some(mt) = mime {
                part.insert("fileType".into(), json!(mt));
            }
            if let Some(s) = source {
                part.insert("identifier".into(), json!(s));
            }
            if let Some(p) = path {
                part.insert("name".into(), json!(p));
            }
        }
        _ => return None,
    }
    Some(Value::Object(part))
}

/// Files we treat as conversations: `*.conversation.json` (the real format) and, defensively, any
/// `*.json` directly in the conversations dir.
fn is_conversation_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.ends_with(".conversation.json") || name.ends_with(".json"),
        None => false,
    }
}

/// Build a cheap [`SessionRef`] without parsing every step.
fn scan(path: &Path) -> Result<Option<SessionRef>> {
    let source = JsonSource::capture(path)?;
    let doc = source.read_document(scan_view)?;

    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.trim_end_matches(".conversation.json")
                .trim_end_matches(".json")
                .to_string()
        })
        .unwrap_or_default();

    let title = doc.name.or(doc.messages.title);
    let created_at = doc.created_at;
    let updated_at = file_mtime(path).or(created_at);
    let message_count = doc.messages.count;

    Ok(Some(SessionRef {
        title,
        created_at,
        updated_at,
        message_count,
        ..SessionRef::new(id, Harness::LmStudio, path.to_path_buf())
    }))
}

/// Validate one turn at a time, including unused versions/fields, while retaining only the
/// title and count. Keep Value's last-key-wins and wrong-type tolerance at the document boundary.
#[derive(Default)]
struct Scan {
    name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    messages: ScanMessages,
}

#[derive(Default)]
struct ScanMessages {
    count: usize,
    title: Option<String>,
}

fn scan_view(view: JsonView<'_>) -> Result<Scan> {
    let Some(fields) = Fields::parse(view, &["name", "createdAt", "messages"])? else {
        // The original whole-Value scan accepts non-object JSON as empty metadata.
        return Ok(Scan::default());
    };
    let mut scan = Scan::default();
    if let Some(name) = fields.get("name") {
        let mut head = TitleHead::default();
        head.read(name, true)?;
        let name = head.finish(true);
        scan.name = name.filter(|name| name != "Untitled" && !name.starts_with("Untitled -"));
    }
    if let Some(created) = fields.get("createdAt") {
        if created.is_string()? || created.as_number()?.is_some() {
            scan.created_at = super::ts_from_value(created.resolve()?.as_ref());
        }
    }
    let Some(messages) = fields.get("messages").map(JsonView::array).transpose()?.flatten() else {
        return Ok(scan);
    };
    let mut search_title = scan.name.is_none();
    for message in messages {
        scan.messages.count += 1;
        let message = message?;
        if !search_title {
            continue;
        }
        let selected = if let Some(turn) = Fields::parse(message, &["currentlySelected", "versions"])? {
            turn.get("versions")
                .map(|versions| projected_selected_version(&turn, versions))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let Some((_, version)) = selected else {
            // chat_title's `?` stops at the first versionless turn.
            search_title = false;
            continue;
        };
        let Some(version) = Fields::parse(version, &["role", "content"])? else {
            continue;
        };
        if !string_is(version.get("role"), "user")? {
            continue;
        }
        let mut head = TitleHead::default();
        if let Some(content) = version.get("content") {
            if content.is_string()? {
                head.read(content, false)?;
            } else if let Some(parts) = content.array()? {
                let mut first = true;
                for part in parts {
                    let Some(text) = part?.get("text")? else { continue };
                    if text.is_string()? {
                        if !first {
                            head.push("\n", false);
                        }
                        first = false;
                        head.read(text, false)?;
                    }
                }
            }
        }
        scan.messages.title = head.finish(false);
        search_title = scan.messages.title.is_none();
    }
    Ok(scan)
}

/// Preserve trim/empty detection and truncate's character semantics without retaining a title
/// candidate or its trailing whitespace. Keep one extra character to distinguish truncation.
#[derive(Default)]
struct TitleHead {
    head: String,
    chars: usize,
    nonwhite: usize,
}
#[derive(Debug, thiserror::Error)]
#[error("LM Studio title prefix complete")]
struct TitleComplete;

impl TitleHead {
    // Called only inside read_document, after whole-source Value validation. A prefix stop
    // still passes its enclosing source-identity check; only this local marker is swallowed.
    fn read(&mut self, view: JsonView<'_>, trim: bool) -> Result<()> {
        let result = view.for_each_str_chunk(|text| {
            self.push(text, trim);
            if self.complete(trim) {
                Err(TitleComplete.into())
            } else {
                Ok(())
            }
        });
        match result {
            Err(error) if error.downcast_ref::<TitleComplete>().is_some() => Ok(()),
            result => result.map(|_| ()),
        }
    }
    fn complete(&self, trim: bool) -> bool {
        self.chars == 81 && (self.nonwhite == 81 || (!trim && self.nonwhite > 0))
    }
    fn push(&mut self, text: &str, trim: bool) {
        if self.complete(trim) {
            return;
        }
        for c in text.chars() {
            if trim && self.chars == 0 && c.is_whitespace() {
                continue;
            }
            if self.chars < 81 {
                self.head.push(c);
            }
            self.chars = (self.chars + 1).min(81);
            if !c.is_whitespace() {
                self.nonwhite = self.chars;
            }
            if self.complete(trim) {
                return;
            }
        }
    }
    fn finish(self, trim: bool) -> Option<String> {
        if self.nonwhite == 0 {
            return None;
        }
        let head = if trim {
            self.head.chars().take(self.nonwhite).collect()
        } else {
            self.head
        };
        Some(crate::ir::truncate(&head, 80))
    }
}

/// Title: the explicit `name` (when meaningful), else the first user text. LM Studio writes
/// `"Untitled"` / `"Untitled - Branched"` for un-named chats, so we treat those as absent.
#[cfg(test)]
fn chat_title(v: &Value) -> Option<String> {
    if let Some(name) = v.get("name").and_then(Value::as_str) {
        let t = name.trim();
        if !t.is_empty() && t != "Untitled" && !t.starts_with("Untitled -") {
            return Some(crate::ir::truncate(t, 80));
        }
    }
    // Fall back to the first user text part.
    let messages = v.get("messages").and_then(Value::as_array)?;
    for msg in messages {
        let version = selected_version(msg)?;
        if version.get("role").and_then(Value::as_str) == Some("user") {
            let t = super::wire::text_parts(version.get("content"));
            if !t.trim().is_empty() {
                return Some(crate::ir::truncate(&t, 80));
            }
        }
    }
    None
}

/// The system prompt over a whole `Value`: the per-chat override (`perChatPredictionConfig.fields[
/// key==llm.prediction.systemPrompt]`) takes precedence over the top-level `systemPrompt`. The
/// streaming parser uses the borrowed [`Doc::system_prompt`] instead; this whole-`Value` form is
/// retained as the reference the unit test pins the precedence rule against.
#[cfg(test)]
fn system_prompt(v: &Value) -> Option<String> {
    if let Some(fields) = v.pointer("/perChatPredictionConfig/fields").and_then(Value::as_array) {
        for f in fields {
            if f.get("key").and_then(Value::as_str) == Some("llm.prediction.systemPrompt") {
                if let Some(val) = f.get("value").and_then(Value::as_str) {
                    if !val.trim().is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    v.get("systemPrompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Pick the `currentlySelected` version of a message turn, falling back to the last / first.
#[cfg(test)]
fn selected_version(msg: &Value) -> Option<&Value> {
    let versions = msg.get("versions").and_then(Value::as_array)?;
    if versions.is_empty() {
        return None;
    }
    let idx = msg
        .get("currentlySelected")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .filter(|i| *i < versions.len())
        .unwrap_or(versions.len() - 1);
    versions.get(idx)
}

/// Parse one message turn's visible (`currentlySelected`) version into an IR [`Message`]. Updates
/// `session_model` with the first model seen.
#[cfg(test)]
fn parse_message(msg: &Value, session_model: &mut Option<String>) -> Option<Message> {
    parse_version(selected_version(msg)?, session_model)
}

/// Parse one specific `version` object into an IR [`Message`].
#[cfg(test)]
fn parse_version(version: &Value, session_model: &mut Option<String>) -> Option<Message> {
    let role = match version.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);
    // A `system` turn is the system prompt (session-level copy in `Session::system_prompt`).
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }

    match version.get("type").and_then(Value::as_str) {
        // Assistant turns: a list of steps (contentBlock / debugInfoBlock / status).
        Some("multiStep") => {
            if let Some(steps) = version.get("steps").and_then(Value::as_array) {
                for step in steps {
                    parse_step(step, &mut m, session_model);
                }
            }
        }
        // User (and any other single-step) turns: content parts directly on the version.
        _ => {
            push_content_parts(version.get("content"), &mut m, false);
        }
    }

    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// Every NON-selected `versions[]` entry of a turn, as `Branch`-kind messages — the regenerations
/// and edits LM Studio keeps beside the visible one. Only emitted under [`ParseOptions::complete`];
/// each carries its `versions[]` index in `extra["lmstudio"]["variant"]`. A throwaway model sink is
/// used so an alternate never redefines the session model.
#[cfg(test)]
fn parse_variants(msg: &Value) -> Vec<Message> {
    let versions = match msg.get("versions").and_then(Value::as_array) {
        Some(v) if v.len() > 1 => v,
        _ => return Vec::new(),
    };
    let selected = msg
        .get("currentlySelected")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .filter(|i| *i < versions.len())
        .unwrap_or(versions.len() - 1);
    let mut out = Vec::new();
    let mut throwaway_model = None;
    for (i, version) in versions.iter().enumerate() {
        if i == selected {
            continue;
        }
        if let Some(mut m) = parse_version(version, &mut throwaway_model) {
            m.kind = MessageKind::Branch;
            m.harness_extra_mut(Harness::LmStudio)
                .insert("variant".into(), Value::Number(i.into()));
            out.push(m);
        }
    }
    out
}

/// Parse one assistant `step`. Only `contentBlock` carries content; others are app-internal.
#[cfg(test)]
fn parse_step(step: &Value, m: &mut Message, session_model: &mut Option<String>) {
    if step.get("type").and_then(Value::as_str) != Some("contentBlock") {
        return; // debugInfoBlock / status — nothing to render.
    }

    // Model + usage live on genInfo. IR diet: the first model seen becomes the session default, so
    // it is NOT repeated on the message; a later, differing model would name itself.
    if let Some(gen) = step.get("genInfo") {
        if let Some(model) = gen
            .get("indexedModelIdentifier")
            .or_else(|| gen.get("identifier"))
            .and_then(Value::as_str)
        {
            match session_model {
                None => *session_model = Some(model.to_string()),
                Some(cur) if cur != model && m.model.is_none() => m.model = Some(model.to_string()),
                _ => {}
            }
        }
        if m.usage.is_none() {
            if let Some(u) = parse_usage(gen.get("stats")) {
                m.usage = Some(u);
            }
        }
    }

    // Per-step timestamp from the stepIdentifier prefix ("<ms-epoch>-<rand>").
    if m.timestamp.is_none() {
        if let Some(ts) = step.get("stepIdentifier").and_then(Value::as_str).and_then(step_id_ts) {
            m.timestamp = Some(ts);
        }
    }

    // A `style.type == "thinking"` block is chain-of-thought reasoning.
    let is_thinking = step.pointer("/style/type").and_then(Value::as_str) == Some("thinking");
    push_content_parts(step.get("content"), m, is_thinking);
}

/// Append text/file/image parts from a `content` array (or bare string) onto `m`.
/// When `as_thinking`, text parts become [`Block::Thinking`] instead of [`Block::Text`].
#[cfg(test)]
fn push_content_parts(content: Option<&Value>, m: &mut Message, as_thinking: bool) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                push_text(m, s.clone(), as_thinking);
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                push_text(m, t.to_string(), as_thinking);
                            }
                        }
                    }
                    Some("file") => push_file(m, part),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn push_text(m: &mut Message, text: String, as_thinking: bool) {
    if as_thinking {
        // Explicit reasoning steps (`style.type=="thinking"`) stay a single Thinking block.
        m.content.push(Block::Thinking {
            text: text.into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
    } else if super::harmony::looks_like_harmony(&text) {
        // gpt-oss (common in LM Studio) embeds Harmony channel markers in plain assistant text:
        // analysis → Thinking, commentary tool calls → ToolUse, final → Text. Decode them into
        // first-class IR blocks instead of leaving the raw `<|channel|>…` markers in the text.
        m.content.extend(super::harmony::decode_content(&text));
    } else {
        m.content.push(Block::Text { text: text.into() });
    }
}

/// A `{type:"file",...}` part. `fileType:"image"` → [`Block::Image`]; otherwise [`Block::File`].
/// The reference is `identifier`/`fileIdentifier` (bytes live under `.lmstudio/.internal/files/`).
#[cfg(test)]
fn push_file(m: &mut Message, part: &Value) {
    let data_ref = part
        .get("identifier")
        .or_else(|| part.get("fileIdentifier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = part.get("name").and_then(Value::as_str).map(str::to_string);
    let file_type = part.get("fileType").and_then(Value::as_str);

    if file_type == Some("image") {
        m.content.push(Block::Image {
            media_type: None,
            data_ref: data_ref.or(name),
        });
    } else {
        m.content.push(Block::File {
            mime: file_type.map(str::to_string),
            path: name,
            source: data_ref,
        });
    }
}

/// Map LM Studio's `genInfo.stats` to IR [`Usage`].
#[cfg(test)]
fn parse_usage(stats: Option<&Value>) -> Option<Usage> {
    let stats = stats?;
    let input = stats.get("promptTokensCount").and_then(Value::as_u64);
    let output = stats.get("predictedTokensCount").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    })
}

/// A `stepIdentifier` is `"<ms-epoch>-<rand>"`; mine its prefix as a timestamp.
#[cfg(test)]
fn step_id_ts(id: &str) -> Option<DateTime<Utc>> {
    let prefix = id.split('-').next()?;
    let ms: i64 = prefix.parse().ok()?;
    // Sanity bound: between ~2010 and ~2100 (ms epoch).
    if !(1_200_000_000_000..=4_000_000_000_000).contains(&ms) {
        return None;
    }
    DateTime::from_timestamp_millis(ms)
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lmstudio")
            .join(name)
    }

    /// The pre-projection borrowed envelope; pin its stricter top-level schema independently
    /// from record Values, whose malformed scalar/depth fields are silently skipped.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct PreviousDoc<'a> {
        #[serde(default, rename = "createdAt")]
        created_at: Option<Value>,
        #[serde(default, rename = "systemPrompt")]
        system_prompt: Option<String>,
        #[serde(default, rename = "perChatPredictionConfig")]
        config: Option<Value>,
        #[serde(default)]
        pinned: Option<bool>,
        #[serde(default, borrow)]
        messages: Vec<&'a serde_json::value::RawValue>,
    }

    #[test]
    fn projection_matches_owned_turns_and_variants() {
        let cases = [
            r#"{"versions":[]}"#,
            r#"{"currentlySelected":0,"currentlySelected":99,"versions":[{"role":"user","content":"old"},{"role":"user","content":"last"}]}"#,
            r#"{"currentlySelected":1,"versions":[{"role":"system","content":"system"},{"role":"assistant","type":"multiStep","steps":[{"type":"debugInfoBlock","content":"ignored"},{"type":"contentBlock","genInfo":{"indexedModelIdentifier":42,"identifier":"masked","stats":{"promptTokensCount":12,"predictedTokensCount":13}},"style":{"type":"thinking"},"stepIdentifier":"1754425094719-1","content":[{"type":"text","text":"reason"},{"type":"file","fileType":"image","identifier":"image"}]},{"type":"contentBlock","genInfo":{"identifier":"test"},"content":"answer"}]}]}"#,
            r#"{"versions":[{"role":"user","content":[{"type":"file","fileIdentifier":42,"identifier":"masked","name":"file","fileType":"text/plain"},{"type":"text","text":"first","text":"final"},{"type":"unknown","text":"ignored"}]}]}"#,
            r#"{"versions":null,"versions":[{"role":"assistant","content":"<|channel|>final<|message|>answer<|end|>"}]}"#,
            r#"{"currentlySelected":-1,"versions":[{"role":"user","content":null},{"role":"invalid","content":"ignored"}]}"#,
            "null",
        ];
        for text in cases {
            let value: Value = serde_json::from_str(text).unwrap();
            let expected = parse_message(&value, &mut None);
            let fields = Fields::parse(JsonView::from(&value), &["versions", "currentlySelected"]).unwrap();
            let actual = if let Some(fields) = fields {
                if let Some(versions) = fields.get("versions") {
                    projected_selected_version(&fields, versions)
                        .unwrap()
                        .and_then(|(_, version)| projected_version(version, &mut None, &ParseOptions::full()).unwrap())
                } else {
                    None
                }
            } else {
                None
            };
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{text}"
            );
            let expected = parse_variants(&value);
            let mut actual = Vec::new();
            if let Some(fields) = Fields::parse(JsonView::from(&value), &["versions", "currentlySelected"]).unwrap() {
                if let Some(versions) = fields.get("versions") {
                    if let Some((selected, _)) = projected_selected_version(&fields, versions).unwrap() {
                        let mut model = None;
                        for (index, version) in versions.array().unwrap().unwrap().enumerate() {
                            if index != selected {
                                if let Some(mut message) =
                                    projected_version(version.unwrap(), &mut model, &ParseOptions::complete()).unwrap()
                                {
                                    message.kind = MessageKind::Branch;
                                    message
                                        .harness_extra_mut(Harness::LmStudio)
                                        .insert("variant".into(), Value::Number(index.into()));
                                    actual.push(message);
                                }
                            }
                        }
                    }
                }
            }
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "variants {text}"
            );
        }
    }

    #[test]
    fn projected_stream_preserves_document_schema_rejection() {
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-schema-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("schema.conversation.json");
        let mut cases = vec![
            r#"{"messages":[],"messages":[]}"#.to_owned(),
            r#"{"messages":[],"\u006dessages":[]}"#.to_owned(),
            r#"{"systemPrompt":"a","systemPrompt":null}"#.to_owned(),
            r#"{"createdAt":null,"createdAt":1}"#.to_owned(),
            r#"{"pinned":null,"pinned":null}"#.to_owned(),
            r#"{"messages":null}"#.to_owned(),
            r#"{"messages":false}"#.to_owned(),
            r#"{"systemPrompt":42}"#.to_owned(),
            r#"{"systemPrompt":"bad\uD800","perChatPredictionConfig":{"fields":[{"key":"llm.prediction.systemPrompt","value":"valid"}]}}"#.to_owned(),
            r#"{"pinned":1}"#.to_owned(),
            r#"{"createdAt":1e999}"#.to_owned(),
            r#"{"perChatPredictionConfig":{"ignored":"bad\uD800"}}"#.to_owned(),
            r#"{"ignored":"bad\uD800","ignored":1e999,"messages":[]}"#.to_owned(),
            r#"{"ignored\uD800":1,"messages":[]}"#.to_owned(),
            r#"{"messages":[{"versions":[{"role":"user","content":"a","ignored":"bad\uD800"}]},{"versions":[{"role":"user","content":"good"}]}]}"#.to_owned(),
            "{}".to_owned(), "[]".to_owned(),
            r#"[1754425094719,"prompt",null,true,[]]"#.to_owned(),
            "[null,null,null,null,[],0]".to_owned(),
            "[null,42]".to_owned(),
        ];
        for depth in 124..=130 {
            cases.push(format!(
                r#"{{"perChatPredictionConfig":{}0{}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
            cases.push(format!(
                r#"{{"ignored":{}0{},"messages":[]}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for text in cases {
            fs::write(&path, &text).unwrap();
            let expected = serde_json::from_str::<PreviousDoc>(&text);
            let r = SessionRef::new("schema", Harness::LmStudio, path.clone());
            let actual = crate::stream::collect_with(&LmStudio { root: None }, &r, &ParseOptions::full());
            assert_eq!(actual.is_ok(), expected.is_ok(), "{text}");
            if let Ok(doc) = expected {
                let mut model = None;
                let messages: Vec<_> = doc
                    .messages
                    .iter()
                    .filter_map(|raw| serde_json::from_str::<Value>(raw.get()).ok())
                    .filter_map(|value| parse_message(&value, &mut model))
                    .collect();
                assert_eq!(
                    serde_json::to_value(&actual.unwrap().messages).unwrap(),
                    serde_json::to_value(messages).unwrap(),
                    "{text}"
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_mutation_during_stream_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-identity-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.conversation.json");
        fs::write(
            &path,
            r#"{"messages":[{"versions":[{"role":"user","content":"selected"}]}]}"#,
        )
        .unwrap();
        let r = SessionRef::new("source", Harness::LmStudio, path.clone());
        struct MutatingSink(PathBuf);
        impl MessageSink for MutatingSink {
            fn message(&mut self, _: Message) -> Flow {
                fs::write(&self.0, r#"{"messages":[]}"#).unwrap();
                Flow::Stop
            }
        }
        let error = LmStudio { root: None }
            .stream(&r, &ParseOptions::lazy(), &mut MutatingSink(path))
            .unwrap_err();
        assert!(format!("{error:#}").contains("source changed"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lazy_selected_text_and_thinking_remain_source_backed() {
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-lazy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lazy.conversation.json");
        let text = "plain \"line\n 🌙".repeat(8192);
        let thinking = "reason\n".repeat(8192);
        let value = json!({"messages":[{"currentlySelected":1,"versions":[
            {"role":"assistant","content":"unused".repeat(100_000)},
            {"role":"assistant","type":"multiStep","steps":[
                {"type":"contentBlock","style":{"type":"thinking"},"content":thinking},
                {"type":"contentBlock","content":[{"type":"text","text":text}]}
            ]}
        ]}]});
        fs::write(&path, value.to_string()).unwrap();
        let r = SessionRef::new("lazy", Harness::LmStudio, path.clone());
        let lazy = crate::stream::collect_with(&LmStudio { root: None }, &r, &ParseOptions::lazy()).unwrap();
        assert_eq!(lazy.messages.len(), 1);
        assert!(matches!(&lazy.messages[0].content[0], Block::Thinking { text, .. } if text.is_span()));
        assert!(matches!(&lazy.messages[0].content[1], Block::Text { text } if text.is_span()));
        let full = crate::stream::collect_with(&LmStudio { root: None }, &r, &ParseOptions::full()).unwrap();
        let mut lazy = lazy;
        lazy.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(lazy.messages).unwrap(),
            serde_json::to_value(full.messages).unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn harmony_marker_detection_crosses_decoded_chunks() {
        for marker in [r"\u003c|channel|>", r"\u003c|message|>", r"\u003c|start|>assistant"] {
            for padding in 65_510..65_542 {
                let plain = format!("{}{marker}final", "x".repeat(padding));
                let dir = std::env::temp_dir().join(format!("cv-lmstudio-harmony-{}", uuid::Uuid::new_v4()));
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("text.json");
                fs::write(&path, format!("\"{plain}\"")).unwrap();
                let source = JsonSource::capture(&path).unwrap();
                assert!(
                    source.read_document(projected_harmony).unwrap(),
                    "{marker} at {padding}"
                );
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn parses_user_assistant_thinking_and_usage() {
        let path = fixture("sample.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        assert_eq!(r.id, "sample");
        assert_eq!(r.title.as_deref(), Some("Hello there"));
        assert_eq!(r.message_count, 2);

        let s = LmStudio { root: None }.parse(&r).unwrap();

        crate::harness::assert_no_flat_keys(&s);
        assert!(s.cwd.is_none(), "lmstudio chats have no cwd");
        assert_eq!(s.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(s.messages.len(), 2);

        // user turn
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!(u.text().as_deref(), Some("Hello there"));

        // assistant turn: thinking block (from style.thinking) then text, plus model+usage.
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(a.content[0], Block::Thinking { .. }));
        match &a.content[0] {
            Block::Thinking { text, .. } => assert_eq!(text, "the user said hi"),
            _ => unreachable!(),
        }
        assert_eq!(a.text().as_deref(), Some("Hi! How can I help?"));
        // IR diet: the assistant's model equals the session default, so it is not repeated.
        assert!(a.model.is_none());
        assert_eq!(s.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!((u.kind, u.origin), (MessageKind::Prompt, Origin::Human));
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        let usage = a.usage.as_ref().expect("usage present");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));
        assert!(a.timestamp.is_some(), "timestamp from stepIdentifier");
    }

    #[test]
    fn empty_messages_and_untitled_yield_no_title() {
        let v: Value = serde_json::from_str(r#"{"name":"Untitled","createdAt":1754424586190,"messages":[]}"#).unwrap();
        assert!(chat_title(&v).is_none());
    }

    #[test]
    fn per_chat_system_prompt_overrides_top_level() {
        let v: Value = serde_json::from_str(
            r#"{"systemPrompt":"global","perChatPredictionConfig":{"fields":[{"key":"llm.prediction.systemPrompt","value":"tsundere"}]}}"#,
        )
        .unwrap();
        assert_eq!(system_prompt(&v).as_deref(), Some("tsundere"));
    }

    #[test]
    fn user_file_image_part_becomes_image_block() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"look"},{"type":"file","name":"image.png","identifier":"123 - 4.png","fileType":"image","sizeBytes":100}]}]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert!(matches!(m.content[1], Block::Image { .. }));
        match &m.content[1] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("123 - 4.png")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn selects_currently_selected_version() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":1,"versions":[
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v0"}]},
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v1"}]}
            ]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("v1"));
    }

    #[test]
    fn variant_versions_emitted_as_branches_under_complete() {
        use crate::stream::{collect_with, ParseOptions};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-branch-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.conversation.json");
        // One user turn, then an assistant turn with two regenerated versions (index 1 selected).
        fs::write(
            &path,
            r#"{"name":"T","createdAt":1754425094719,"messages":[
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"hi"}]}]},
                {"currentlySelected":1,"versions":[
                    {"type":"multiStep","role":"assistant","steps":[{"type":"contentBlock","content":[{"type":"text","text":"first try"}]}]},
                    {"type":"multiStep","role":"assistant","steps":[{"type":"contentBlock","content":[{"type":"text","text":"second try"}]}]}
                ]}
            ]}"#,
        )
        .unwrap();
        let r = scan(&path).unwrap().unwrap();

        // Default parse: only the selected version — no branches.
        let normal = LmStudio { root: None }.parse(&r).unwrap();
        assert_eq!(normal.messages.len(), 2);
        assert_eq!(normal.messages[1].text().as_deref(), Some("second try"));
        assert!(normal.messages.iter().all(|m| m.kind != MessageKind::Branch));

        // Complete parse: the non-selected version rides along as a Branch turn.
        let full = collect_with(&LmStudio { root: None }, &r, &ParseOptions::complete()).unwrap();
        let branch = full
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::Branch)
            .expect("a branch turn");
        assert_eq!(branch.role, Role::Assistant);
        assert_eq!(branch.text().as_deref(), Some("first try"));
        assert_eq!(
            branch
                .harness_extra(Harness::LmStudio)
                .and_then(|b| b.get("variant"))
                .and_then(Value::as_u64),
            Some(0)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_discovers_empty() {
        let a = LmStudio { root: None };
        assert!(a.discover().unwrap().is_empty());
    }

    #[test]
    fn projected_timestamp_matches_integer_prefix_parser() {
        let mut cases = vec![
            "1754425094719-1".to_owned(),
            "+1754425094719-x".to_owned(),
            "1754425094719".to_owned(),
            "-1754425094719-x".to_owned(),
            "++1754425094719-x".to_owned(),
            "1754425094719🌙-x".to_owned(),
            "18446744073709551616-x".to_owned(),
            "-x".to_owned(),
        ];
        cases.push(format!("{}1754425094719-{}", "0".repeat(100_000), "x".repeat(100_000)));
        for text in cases {
            let value = Value::String(text.clone());
            assert_eq!(
                projected_step_timestamp(Some(JsonView::from(&value))).unwrap(),
                step_id_ts(&text)
            );
        }
    }

    #[test]
    fn step_id_timestamp_parses_prefix() {
        let ts = step_id_ts("1754425094719-0.022738884687463323").unwrap();
        assert_eq!(ts.timestamp_millis(), 1754425094719);
        assert!(step_id_ts("garbage").is_none());
    }

    #[test]
    fn emit_round_trips_through_parse() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        // Build a small session: a user turn and an assistant turn with thinking + text.
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "Hello there".to_string().into(),
        });
        let mut assistant = Message::new(Role::Assistant);
        assistant.model = Some("openai/gpt-oss-120b".to_string());
        assistant.content.push(Block::Thinking {
            text: "the user said hi".to_string().into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        assistant.content.push(Block::Text {
            text: "Hi! How can I help?".to_string().into(),
        });
        assistant.usage = Some(Usage {
            input_tokens: Some(12),
            output_tokens: Some(34),
            ..Usage::default()
        });

        let session = Session {
            title: Some("Greeting".to_string()),
            created_at: DateTime::from_timestamp_millis(1_754_425_094_719),
            model: Some("openai/gpt-oss-120b".to_string()),
            messages: vec![user, assistant],
            ..Session::new("orig".to_string(), Harness::LmStudio)
        };

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_dir = std::env::temp_dir().join(format!("cv-lmstudio-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&out_dir);

        let result = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).unwrap();
        assert!(result.path.exists(), "emitted file exists");

        // Re-parse the emitted file with this adapter.
        let r = scan(&result.path).unwrap().unwrap();
        let parsed = LmStudio { root: None }.parse(&r).unwrap();

        assert_eq!(parsed.messages.len(), 2, "both turns survive");
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(
            parsed.messages[0].text().as_deref(),
            Some("Hello there"),
            "first user text survives"
        );

        let a = &parsed.messages[1];
        assert!(
            matches!(a.content[0], Block::Thinking { .. }),
            "thinking block round-trips via style.type==thinking"
        );
        assert_eq!(a.text().as_deref(), Some("Hi! How can I help?"));
        assert!(
            a.model.is_none(),
            "model rides on the session, not repeated on the turn"
        );
        assert_eq!(parsed.model.as_deref(), Some("openai/gpt-oss-120b"));
        let usage = a.usage.as_ref().expect("usage round-trips");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));

        let _ = fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn gpt_oss_harmony_text_decodes_into_blocks() {
        // gpt-oss output stored by LM Studio as raw Harmony-framed assistant text should be decoded
        // into Thinking + ToolUse + Text instead of a single text block full of `<|channel|>` markers.
        let path = fixture("harmony.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        let s = LmStudio { root: None }.parse(&r).unwrap();
        assert_eq!(s.messages.len(), 2);

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // No raw Harmony markers should survive in the decoded blocks.
        assert_eq!(a.content.len(), 3, "thinking + tooluse + text");

        match &a.content[0] {
            Block::Thinking { text, .. } => {
                assert!(text.contains("get_weather"));
                assert!(!text.contains("<|channel|>"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &a.content[1] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input.resolve().unwrap()["location"], "San Francisco, CA");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &a.content[2] {
            Block::Text { text } => {
                assert!(text.contains("sunny"));
                assert!(!text.contains("<|"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // Model + usage still parsed from genInfo (model on the session, not repeated per message).
        assert!(a.model.is_none());
        assert_eq!(s.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(68));
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first turn ends the parse with exactly one message seen, never materializing the rest.
    /// Session-level extras (the system prompt) are still captured up front.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-stream-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.conversation.json");
        fs::write(
            &path,
            r#"{"name":"T","createdAt":1754425094719,"systemPrompt":"be terse","messages":[
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"one"}]}]},
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"two"}]}]},
                {"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"three"}]}]}
            ]}"#,
        )
        .unwrap();

        let r = scan(&path).unwrap().unwrap();

        struct StopFirst {
            seen: Vec<String>,
        }
        impl MessageSink for StopFirst {
            fn message(&mut self, m: Message) -> Flow {
                self.seen.push(m.text().unwrap_or_default());
                Flow::Stop
            }
        }
        let mut sink = StopFirst { seen: Vec::new() };
        let s = LmStudio { root: None }
            .stream(&r, &ParseOptions::full(), &mut sink)
            .unwrap();

        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        // The system prompt is a first-class session field, captured before the early stop.
        assert_eq!(s.system_prompt.as_deref(), Some("be terse"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_projection_preserves_trim_join_and_truncation() {
        let texts = [
            String::new(),
            " \n\t\u{2003}".repeat(100),
            format!("{}Untitled{}", " ".repeat(100), " ".repeat(100)),
            "Untitled - branch".into(),
            format!("{}{}\nend{}", " ".repeat(99), "é☺".repeat(100), " ".repeat(99)),
            format!("short{}", " ".repeat(1000)),
            "a".repeat(80),
            "a".repeat(81),
        ];
        for text in texts {
            let chars: Vec<_> = text.chars().collect();
            for width in (1..19).chain(std::iter::once(chars.len().max(1))) {
                for trim in [false, true] {
                    let mut head = TitleHead::default();
                    for chunk in chars.chunks(width) {
                        head.push(&chunk.iter().collect::<String>(), trim);
                    }
                    let expected = if trim { text.trim() } else { &text };
                    assert_eq!(
                        head.finish(trim),
                        (!text.trim().is_empty()).then(|| crate::ir::truncate(expected, 80))
                    );
                }
            }
        }
    }

    #[test]
    fn title_prefix_stop_keeps_enclosing_source_identity_check() {
        let dir = std::env::temp_dir().join(format!("cv-lm-title-prefix-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("title.conversation.json");
        let original = serde_json::to_vec(&serde_json::json!({"name": "text\n".repeat(40_000)})).unwrap();
        std::fs::write(&path, &original).unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let result = source.read_document(|view| {
            let mut head = TitleHead::default();
            head.read(view.get("name")?.unwrap(), true)?;
            assert!(head.complete(true));
            // A prefix stop must not bypass the enclosing check when the unread tail changes.
            std::fs::write(&path, b"{}").unwrap();
            Ok(head.finish(true))
        });
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discovery_scan_matches_whole_value_metadata() {
        let _world = cv_testkit::World::in_process("lmstudio-scan");
        let dir = std::env::temp_dir().join(format!("cv-lmstudio-scan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.conversation.json");
        let mut cases = vec![
            r#"{"name":" \"named\"\n\uD834\uDD1E ","createdAt":1700000000000,"messages":[null,42,{}]}"#.to_owned(),
            r#"{"messages":[{"versions":[{"role":"user","content":"fallback"}]}],"name":"old","name":null,"createdAt":1,"createdAt":"2026-01-01T00:00:00Z"}"#.to_owned(),
            r#"{"name":"Untitled - Branched","messages":[{"versions":[]},{"versions":[{"role":"user","content":"must not become title"}]}]}"#.to_owned(),
            r#"{"name":42,"messages":[{"currentlySelected":0,"currentlySelected":99,"versions":[{"role":"user","content":"old"},{"role":"user","content":[{"type":"text","text":"selected\ntext"}]}]}]}"#.to_owned(),
            r#"{"messages":[{"versions":[{"role":"assistant","content":"ignored"}]},{"versions":[{"role":"user","content":"   "}]},{"versions":[{"role":"user","content":"first user"}]}]}"#.to_owned(),
            r#"{"messages":[{"versions":[{"role":"user","content":[{"text":""},{"text":""},{"text":"é☺\\nfinal"},false,{"text":3}]}]}]}"#.to_owned(),
            r#"{"messages":[1,2],"messages":false,"name":"  Untitled  "}"#.to_owned(),
            r#"{"messages":null,"messages":[1,2,3],"name":"explicit"}"#.to_owned(),
            "[1,2,3]".to_owned(), "null".to_owned(), "42".to_owned(),
            r#"{"name":"valid","messages":[{"versions":[{"role":"user","content":"first"}]},{"ignored":"bad\uD800"}]}"#.to_owned(),
            r#"{"messages":[],"ignored":"bad\uDC00"}"#.to_owned(),
            r#"{"messages":[1e999]}"#.to_owned(),
            r#"{"messages":[1],"messages":[2,}"#.to_owned(),
            r#"{"messages":[1]} trailing"#.to_owned(),
        ];
        for depth in 123..131 {
            cases.push(format!(
                r#"{{"messages":[{}0{}]}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        cases.push(
            serde_json::json!({
                "name": "title", "messages": (0..24).map(|i| serde_json::json!({
                    "versions": [{ "role": if i == 0 {"user"} else {"assistant"},
                        "content": if i == 0 {"first".to_owned()} else {"payload\\\n".repeat(16_384)} },
                        {"role":"assistant", "content": "unused variant".repeat(4096)}],
                    "currentlySelected": 0,
                })).collect::<Vec<_>>()
            })
            .to_string(),
        );
        for text in cases {
            fs::write(&path, &text).unwrap();
            // The independent pre-change scanner: whole Value, original chat_title and array len.
            let expected = serde_json::from_str::<Value>(&text).ok().map(|v| {
                (
                    chat_title(&v),
                    v.get("createdAt").and_then(super::super::ts_from_value),
                    v.get("messages").and_then(Value::as_array).map_or(0, Vec::len),
                )
            });
            let actual = scan(&path).ok().flatten().map(|r| {
                assert_eq!(r.id, "sample");
                assert_eq!(r.updated_at, file_mtime(&path).or(r.created_at));
                (r.title, r.created_at, r.message_count)
            });
            assert_eq!(actual, expected, "case prefix: {:?}", &text[..text.len().min(120)]);
        }
        fs::write(&path, b"{\"messages\":[\"\xff\"]}").unwrap();
        assert!(scan(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
