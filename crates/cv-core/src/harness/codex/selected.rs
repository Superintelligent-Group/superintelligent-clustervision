//! Selected rollout bodies keep their original checked source; the normal dispatcher still
//! owns metadata, authority, inheritance, usage and message classification.
use super::*;
use crate::{
    harness::store::{document::Fields, text},
    json::{Json, JsonSource, JsonView},
};

pub(super) enum Projection {
    Text { record: Value, text: Text },
    Blocks { record: Value, blocks: Vec<Block> },
    Input { record: Value, input: Json },
}
impl Projection {
    pub(super) fn dispatch(self, s: &mut Session, scratch: &mut Vec<Message>, events: bool, ctx: &mut CodexCtx) {
        let before = scratch.len();
        let record = match &self {
            Self::Text { record, .. } | Self::Blocks { record, .. } | Self::Input { record, .. } => record,
        };
        dispatch_line(s, scratch, record, events, ctx);
        self.replace(scratch.get_mut(before));
    }

    pub(super) fn dispatch_item(self, scratch: &mut Vec<Message>, ctx: &mut CodexCtx) {
        let before = scratch.len();
        let record = match &self {
            Self::Text { record, .. } | Self::Blocks { record, .. } | Self::Input { record, .. } => record,
        };
        handle_item(record.get("payload"), false, None, scratch, ctx);
        self.replace(scratch.get_mut(before));
    }

    fn replace(self, message: Option<&mut Message>) {
        let Some(message) = message else { return };
        match self {
            Self::Text { text, .. } => match message.content.first_mut() {
                Some(Block::Text { text: body }) | Some(Block::Thinking { text: body, .. }) => *body = text,
                _ => {}
            },
            Self::Blocks { blocks, .. } => message.content = blocks,
            Self::Input { input, .. } => {
                if let Some(Block::ToolUse { input: body, .. }) = message.content.first_mut() {
                    *body = input;
                }
            }
        }
    }
}

pub(super) fn project(
    view: JsonView<'_>,
    source: &JsonSource,
    spans: bool,
    events: bool,
    complete: bool,
) -> Result<Option<Projection>> {
    let Some(top) = Fields::parse(view, &["type", "timestamp", "ordinal", "payload"])? else {
        return Ok(None);
    };
    let Some(payload_view) = top.get("payload") else {
        return Ok(None);
    };
    let record_type = top.string("type")?.unwrap_or_default();
    let make_record = |p| -> Result<Value> {
        let mut record = Map::new();
        record.insert("type".into(), record_type.clone().into());
        record.insert("payload".into(), Value::Object(p));
        for key in ["timestamp", "ordinal"] {
            if let Some(value) = top.get(key) {
                record.insert(key.into(), value.resolve()?.into_owned());
            }
        }
        Ok(Value::Object(record))
    };
    project_payload(
        payload_view,
        source,
        spans,
        (events, complete),
        &record_type,
        make_record,
    )
}

/// Legacy documents contain response payloads directly; retain the same body projection and
/// item dispatcher without applying the modern envelope's metadata or carrier policies.
pub(super) fn project_item(view: JsonView<'_>, source: &JsonSource, spans: bool) -> Result<Option<Projection>> {
    project_payload(view, source, spans, (false, false), "response_item", |p| {
        Ok(serde_json::json!({"type": "response_item", "payload": p}))
    })
}

fn project_payload(
    payload_view: JsonView<'_>,
    source: &JsonSource,
    spans: bool,
    (events, complete): (bool, bool),
    record_type: &str,
    make_record: impl Fn(Map<String, Value>) -> Result<Value>,
) -> Result<Option<Projection>> {
    let Some(payload) = Fields::parse(
        payload_view,
        &[
            "type",
            "role",
            "message",
            "content",
            "summary",
            "arguments",
            "input",
            "encrypted_content",
        ],
    )?
    else {
        return Ok(None);
    };
    let ty = payload.string("type")?.unwrap_or_default();
    let mut p = Map::new();
    p.insert("type".into(), ty.clone().into());
    // The dispatcher reads these exact fields for recognized selected records, in every mode.
    for key in [
        "role",
        "phase",
        "kind",
        "name",
        "id",
        "call_id",
        "namespace",
        "author",
        "recipient",
        "status",
        "internal_chat_message_metadata_passthrough",
    ] {
        if let Some(value) = payload_view.get(key)? {
            p.insert(key.into(), value.resolve()?.into_owned());
        }
    }
    match (record_type, ty.as_str()) {
        ("event_msg", "user_message" | "agent_message") if !events && complete => Ok(None),
        ("event_msg", "user_message" | "agent_message") if !events => Ok(Some(Projection::Blocks {
            record: make_record(p)?,
            blocks: Vec::new(),
        })),
        ("event_msg", "user_message" | "agent_message") if events => {
            let Some(body) = payload.get("message") else {
                return Ok(None);
            };
            let Some(text) = body_text(body, spans)? else {
                return Ok(None);
            };
            let surrogate = if ty == "user_message" {
                classify_prefix(&text)?
            } else {
                "selected".into()
            };
            p.insert("message".into(), surrogate.into());
            Ok(Some(Projection::Text {
                record: make_record(p)?,
                text,
            }))
        }
        ("response_item", "message") => {
            let role = payload.string("role")?.unwrap_or_else(|| "user".into());
            if events && matches!(role.as_str(), "user" | "assistant") {
                // A validated model-visible twin is suppressed; only envelope metadata is demanded.
                return Ok(Some(Projection::Blocks {
                    record: make_record(p)?,
                    blocks: Vec::new(),
                }));
            }
            let Some(content) = payload.get("content") else {
                return Ok(None);
            };
            let blocks = blocks(content, spans)?;
            let surrogate = content_prefix(content)?;
            p.insert("content".into(), if blocks.is_empty() {Value::Array(Vec::new())} else {serde_json::json!([{"type":"input_text","text":if surrogate.is_empty() {"selected"} else {&surrogate}}])});
            Ok(Some(Projection::Blocks {
                record: make_record(p)?,
                blocks,
            }))
        }
        ("response_item", "agent_message") => {
            let mut blocks = Vec::new();
            let mut skeleton = Vec::new();
            if let Some(parts) = payload.get("content").map(JsonView::array).transpose()?.flatten() {
                for part in parts {
                    let part = part?;
                    let ty = part
                        .get("type")?
                        .map(|v| v.resolve())
                        .transpose()?
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    if ty == "encrypted_content" {
                        skeleton.push(serde_json::json!({"type":"encrypted_content"}));
                    } else if ty == "input_text" {
                        if let Some(text) = part.get("text")?.map(|v| body_text(v, spans)).transpose()?.flatten() {
                            if selected_nonempty(&text)? {
                                blocks.push(Block::Text { text });
                                skeleton.push(serde_json::json!({"type":"input_text","text":"selected"}));
                            }
                        }
                    }
                }
            }
            p.insert("content".into(), skeleton.into());
            Ok(Some(Projection::Blocks {
                record: make_record(p)?,
                blocks,
            }))
        }
        ("response_item", "reasoning") => {
            let raw = match payload.get("content") {
                Some(v) => joined(v, source, spans)?,
                None => Text::from(""),
            };
            let raw_nonempty = selected_nonempty(&raw)?;
            let summary_view = payload.get("summary");
            let summary = match summary_view {
                Some(v) => joined(v, source, spans)?,
                None => Text::from(""),
            };
            let text = if raw_nonempty { raw } else { summary.clone() };
            if let Some(encrypted) = payload.get("encrypted_content") {
                p.insert("encrypted_content".into(), encrypted.resolve()?.into_owned());
            }
            if raw_nonempty {
                // Distinct summaries are a rich owned extra in the IR. Retain that contract;
                // selected raw thinking stays bounded, but this explicitly demanded extra may not.
                let summary = summary.try_resolve(&crate::Resolver::new(None))?.into_owned();
                let same = text
                    .try_resolve_prefix(&crate::Resolver::new(None), summary.len().saturating_add(4) as u64)?
                    .as_ref()
                    == summary;
                p.insert(
                    "content".into(),
                    if same {
                        summary.clone()
                    } else {
                        format!("{summary}\0selected")
                    }
                    .into(),
                );
                p.insert("summary".into(), summary.into());
            } else {
                p.insert(
                    "summary".into(),
                    if !selected_nonempty(&text)? { "" } else { "selected" }.into(),
                );
            }
            Ok(Some(Projection::Text {
                record: make_record(p)?,
                text,
            }))
        }
        ("response_item", "local_shell_call") if spans => {
            let arena = source.decoded_backing()?;
            let (offset, len) = arena.append(|writer| {
                writer.write_all(b"{")?;
                let mut prior = false;
                for key in ["action", "status"] {
                    if let Some(value) = payload_view.get(key)? {
                        if prior {
                            writer.write_all(b",")?;
                        }
                        serde_json::to_writer(&mut *writer, key)?;
                        writer.write_all(b":")?;
                        value.write_canonical(&mut { &mut *writer })?;
                        prior = true;
                    }
                }
                writer.write_all(b"}")?;
                Ok(())
            })?;
            let input = source.read_backing_value(arena, offset, len, |view| view.to_json())?;
            p.insert("action".into(), serde_json::json!({}));
            Ok(Some(Projection::Input {
                record: make_record(p)?,
                input,
            }))
        }
        ("response_item", "function_call" | "custom_tool_call") => {
            let args = match payload.get("arguments") {
                Some(args) if args.is_string()? => Some(args),
                _ => payload.get("input"),
            };
            let Some(args) = args else { return Ok(None) };
            let input = if spans {
                if args.is_string()? {
                    args.encoded_argument()?
                } else {
                    args.to_json()?
                }
            } else {
                openai::parse_arguments(Some(args.resolve()?.as_ref())).into()
            };
            p.insert("arguments".into(), "{}".into());
            Ok(Some(Projection::Input {
                record: make_record(p)?,
                input,
            }))
        }
        _ => Ok(None),
    }
}
// These descriptors were constructed in the enclosing strictly accepted source view and have
// no decoded prefix skipped. Every valid nonempty JSON string representation decodes at least
// one scalar; inspecting its extent avoids reopening/revalidating giant text just for emptiness.
fn selected_nonempty(text: &Text) -> Result<bool> {
    match text {
        Text::Inline(text) => Ok(!text.is_empty()),
        Text::Source(source) => Ok(source.raw_len() != 0),
        Text::Span(_) => Ok(!text.try_resolve_prefix(&crate::Resolver::new(None), 4)?.is_empty()),
    }
}
fn body_text(view: JsonView<'_>, spans: bool) -> Result<Option<Text>> {
    if spans {
        view.source_text()
    } else {
        view.text(false)
    }
}
fn blocks(content: JsonView<'_>, spans: bool) -> Result<Vec<Block>> {
    if let Some(text) = body_text(content, spans)? {
        return Ok(if !selected_nonempty(&text)? {
            Vec::new()
        } else {
            vec![Block::Text { text }]
        });
    }
    let mut out = Vec::new();
    if let Some(parts) = content.array()? {
        for part in parts {
            let part = part?;
            let ty = part
                .get("type")?
                .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
                .transpose()?
                .unwrap_or_default();
            if super::super::wire::is_image_part(&ty) || matches!(ty.as_str(), "audio_url" | "video_url") {
                if let Some(block) = openai::part_to_block(part.resolve()?.as_ref(), &openai::CODEX) {
                    out.push(block);
                }
            } else if let Some(text) = part.get("text")?.map(|v| body_text(v, spans)).transpose()?.flatten() {
                if selected_nonempty(&text)? {
                    out.push(Block::Text { text });
                }
            }
        }
    }
    Ok(out)
}
fn joined(content: JsonView<'_>, source: &JsonSource, spans: bool) -> Result<Text> {
    if let Some(text) = body_text(content, spans)? {
        return Ok(text);
    };
    if !spans {
        return Ok(text_parts(Some(content.resolve()?.as_ref())).into());
    }
    let Some(parts) = content.array()? else {
        return Ok(Text::from(""));
    };
    let mut strings = Vec::new();
    for part in parts {
        if let Some(view) = part?.get("text")? {
            if view.is_string()? {
                strings.push(view)
            }
        }
    }
    match strings.as_slice() {
        [] => Ok(Text::from("")),
        [view] => Ok(view.source_text()?.unwrap_or_else(|| Text::from(""))),
        _ => text::append(source, &source.decoded_backing()?, |builder| {
            for (i, view) in strings.into_iter().enumerate() {
                if i > 0 {
                    builder.push("\n")?;
                }
                builder.push_string(view)?;
            }
            Ok(())
        }),
    }
}
fn content_prefix(content: JsonView<'_>) -> Result<String> {
    let mut prefix = String::new();
    let append = |prefix: &mut String, chunk: &str| -> Result<()> {
        let chunk = if prefix.is_empty() { chunk.trim_start() } else { chunk };
        for ch in chunk.chars() {
            prefix.push(ch);
            if prefix.len() >= 64 {
                return Err(PrefixDone.into());
            }
        }
        Ok(())
    };
    let result = (|| -> Result<()> {
        if content.is_string()? {
            content.for_each_str_chunk(|chunk| append(&mut prefix, chunk))?;
        } else if let Some(parts) = content.array()? {
            let mut prior = false;
            for part in parts {
                if let Some(text) = part?.get("text")? {
                    if text.is_string()? {
                        if prior {
                            append(&mut prefix, "\n")?;
                        }
                        text.for_each_str_chunk(|chunk| append(&mut prefix, chunk))?;
                        prior = true;
                    }
                }
            }
        }
        Ok(())
    })();
    match result {
        Err(error) if error.downcast_ref::<PrefixDone>().is_some() => {}
        other => other?,
    }
    Ok(prefix)
}

#[derive(Debug)]
struct PrefixDone;
impl std::fmt::Display for PrefixDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("classification prefix complete")
    }
}
impl std::error::Error for PrefixDone {}
fn classify_prefix(text: &Text) -> Result<String> {
    let mut prefix = String::new();
    let result = text.try_for_each_chunk(&crate::Resolver::new(None), 4096, |chunk| {
        let chunk = if prefix.is_empty() { chunk.trim_start() } else { chunk };
        for ch in chunk.chars() {
            prefix.push(ch);
            if prefix.len() >= 64 {
                return Err(PrefixDone.into());
            }
        }
        Ok(())
    });
    match result {
        Err(error) if error.downcast_ref::<PrefixDone>().is_some() => {}
        other => other?,
    }
    Ok(prefix)
}
