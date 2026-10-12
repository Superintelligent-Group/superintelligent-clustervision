//! Select Continue's output fields without retaining unrelated history/context payloads.
use super::*;

pub(super) fn reply_id(item: JsonView<'_>) -> Result<Option<String>> {
    let Some(item) = Fields::parse(item, &["message"])? else {
        return Ok(None);
    };
    let Some(message) = item
        .get("message")
        .map(|v| Fields::parse(v, &["toolCallId"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    message.string("toolCallId")
}

pub(super) fn has_sources(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(
            block,
            Block::ToolUse {
                input: crate::Json::Source(_),
                ..
            }
        )
    })
}

/// Retain only values consumed by the established message codec. Body blocks are projected below.
pub(super) fn messages(item: JsonView<'_>, model: &mut Option<String>) -> Result<(Option<Message>, Option<Message>)> {
    let Some(item) = Fields::parse(item, &["message", "contextItems", "promptLogs", "toolCallStates"])? else {
        return Ok((None, None));
    };
    let mut metadata = Map::new();
    let mut body = Vec::new();
    if let Some(message_view) = item.get("message") {
        let message = Fields::parse(message_view, &["role", "toolCallId", "usage", "content", "toolCalls"])?;
        let mut fields = Map::new();
        if let Some(message) = &message {
            fields = message.project(&["role", "toolCallId"])?;
            if let Some(usage) = message.get("usage") {
                if let Some(usage) = Fields::parse(
                    usage,
                    &[
                        "prompt_tokens",
                        "completion_tokens",
                        "prompt_tokens_details",
                        "completion_tokens_details",
                        "cost_cents",
                        "model",
                    ],
                )? {
                    let mut value = usage.project(&["prompt_tokens", "completion_tokens", "cost_cents", "model"])?;
                    for (key, inner) in [
                        ("prompt_tokens_details", "cached_tokens"),
                        ("completion_tokens_details", "reasoning_tokens"),
                    ] {
                        if let Some(details) = usage
                            .get(key)
                            .map(|v| Fields::parse(v, &[inner]))
                            .transpose()?
                            .flatten()
                        {
                            value.insert(key.into(), Value::Object(details.project(&[inner])?));
                        }
                    }
                    fields.insert("usage".into(), Value::Object(value));
                }
            }
            let role = fields.get("role").and_then(Value::as_str).unwrap_or("user");
            if role == "tool" {
                body.push(Block::ToolResult {
                    tool_use_id: fields.get("toolCallId").and_then(Value::as_str).unwrap_or("").into(),
                    content: joined_content(message.get("content"))?,
                    is_error: false,
                    tool_name: None,
                    status: Some("completed".into()),
                    details: None,
                });
            } else {
                body = content_blocks(message.get("content"))?;
                if role == "assistant" {
                    if let Some(calls) = message.get("toolCalls").map(JsonView::array).transpose()?.flatten() {
                        for call in calls {
                            body.push(tool_call(call?)?);
                        }
                    }
                }
            }
        }
        metadata.insert("message".into(), Value::Object(fields));
    }
    if let Some(context) = item.get("contextItems").map(JsonView::array).transpose()?.flatten() {
        for context in context {
            let Some(context) = Fields::parse(context?, &["name", "description", "uri"])? else {
                continue;
            };
            let mut value = context.project(&["name", "description"])?;
            if let Some(uri) = context
                .get("uri")
                .map(|v| Fields::parse(v, &["type", "value"]))
                .transpose()?
                .flatten()
            {
                value.insert("uri".into(), Value::Object(uri.project(&["type", "value"])?));
            }
            if let Some(block) = context_item_to_block(&Value::Object(value)) {
                body.push(block);
            }
        }
    }
    if let Some(mut logs) = item.get("promptLogs").map(JsonView::array).transpose()?.flatten() {
        if let Some(log) = logs.next() {
            if let Some(log) = Fields::parse(log?, &["modelTitle", "modelProvider"])? {
                metadata.insert(
                    "promptLogs".into(),
                    json!([log.project(&["modelTitle", "modelProvider"])?]),
                );
            }
        }
    }
    let message = history_item_with_body(&Value::Object(metadata), model, Some(body));
    Ok((message, tool_states(item.get("toolCallStates"))?))
}

fn content_blocks(content: Option<JsonView<'_>>) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    let Some(content) = content else { return Ok(out) };
    if let Some(text) = content.text(true)? {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
    } else if let Some(parts) = content.array()? {
        for part in parts {
            let part = part?;
            let Some(fields) = Fields::parse(
                part,
                &["type", "text", "image_url", "imageUrl", "url", "audio_url", "video_url"],
            )?
            else {
                continue;
            };
            let kind = fields.string("type")?.unwrap_or_default();
            if super::super::wire::is_image_part(&kind) || matches!(kind.as_str(), "audio_url" | "video_url") {
                let mut value = fields.project(&["type", "url"])?;
                for key in ["image_url", "imageUrl", "audio_url", "video_url"] {
                    if let Some(nested) = fields.get(key) {
                        if let Some(fields) = Fields::parse(nested, &["url"])? {
                            value.insert(key.into(), Value::Object(fields.project(&["url"])?));
                        } else if nested.is_string()? {
                            value.insert(key.into(), nested.resolve()?.into_owned());
                        } else {
                            value.insert(key.into(), Value::Null);
                        }
                    }
                }
                if let Some(block) = openai::part_to_block(&Value::Object(value), &openai::CONTINUE) {
                    out.push(block);
                }
            } else if let Some(text) = fields.get("text").map(|v| v.text(true)).transpose()?.flatten() {
                if !text.is_empty() {
                    out.push(Block::Text { text });
                }
            }
        }
    }
    Ok(out)
}

fn tool_call(call: JsonView<'_>) -> Result<Block> {
    let call = Fields::parse(call, &["id", "call_id", "name", "arguments", "function"])?;
    let func = call
        .as_ref()
        .and_then(|c| c.get("function"))
        .map(|v| Fields::parse(v, &["name", "arguments"]))
        .transpose()?
        .flatten();
    let get = |key| {
        func.as_ref()
            .and_then(|f| f.get(key))
            .or_else(|| call.as_ref().and_then(|c| c.get(key)))
    };
    let name = get("name")
        .map(JsonView::resolve)
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let id = if let Some(call) = &call {
        if call.get("id").is_some() {
            call.string("id")?
        } else {
            call.string("call_id")?
        }
    } else {
        None
    }
    .unwrap_or_default();
    let input = match get("arguments") {
        Some(value) if value.is_string()? => openai::parse_arguments(Some(value.resolve()?.as_ref())).into(),
        Some(value) if !value.is_null()? => value.argument(true)?,
        _ => json!({}).into(),
    };
    Ok(Block::ToolUse {
        id,
        name,
        input,
        namespace: None,
    })
}

/// A single selected text remains a range. Joining several is requested composite output.
fn join_text<'a>(values: impl Iterator<Item = Result<Option<JsonView<'a>>>>) -> Result<crate::Text> {
    let mut first = None::<JsonView<'a>>;
    let mut joined = None::<String>;
    for value in values {
        let Some(value) = value? else { continue };
        if !value.is_string()? {
            continue;
        }
        if let Some(joined) = &mut joined {
            joined.push('\n');
            value.for_each_str_chunk(|chunk| {
                joined.push_str(chunk);
                Ok(())
            })?;
        } else if let Some(first) = first.take() {
            let mut text = String::new();
            first.for_each_str_chunk(|chunk| {
                text.push_str(chunk);
                Ok(())
            })?;
            text.push('\n');
            value.for_each_str_chunk(|chunk| {
                text.push_str(chunk);
                Ok(())
            })?;
            joined = Some(text);
        } else {
            first = Some(value);
        }
    }
    match joined {
        Some(text) => Ok(text.into()),
        None => first
            .map(|view| view.text(true))
            .transpose()
            .map(|text| text.flatten().unwrap_or_default()),
    }
}
fn joined_content(content: Option<JsonView<'_>>) -> Result<crate::Text> {
    let Some(content) = content else {
        return Ok(crate::Text::default());
    };
    if let Some(text) = content.text(true)? {
        return Ok(text);
    }
    let Some(parts) = content.array()? else {
        return Ok(crate::Text::default());
    };
    join_text(parts.map(|part| {
        let Some(part) = Fields::parse(part?, &["text"])? else {
            return Ok(None);
        };
        Ok(part.get("text"))
    }))
}
fn tool_states(states: Option<JsonView<'_>>) -> Result<Option<Message>> {
    let Some(states) = states.map(JsonView::array).transpose()?.flatten() else {
        return Ok(None);
    };
    let mut message = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
    for state in states {
        let Some(state) = Fields::parse(state?, &["output", "status", "toolCallId", "toolCall"])? else {
            continue;
        };
        let Some(output) = state.get("output").map(JsonView::array).transpose()?.flatten() else {
            continue;
        };
        let content = join_text(output.map(|output| {
            let Some(output) = Fields::parse(output?, &["content"])? else {
                return Ok(None);
            };
            Ok(output.get("content"))
        }))?;
        let status = state.string("status")?;
        let call = state
            .get("toolCall")
            .map(|v| Fields::parse(v, &["function"]))
            .transpose()?
            .flatten();
        let function = call
            .and_then(|c| c.get("function"))
            .map(|v| Fields::parse(v, &["name"]))
            .transpose()?
            .flatten();
        message.content.push(Block::ToolResult {
            tool_use_id: state.string("toolCallId")?.unwrap_or_default(),
            content,
            is_error: status.as_deref() == Some("errored"),
            tool_name: function.map(|f| f.string("name")).transpose()?.flatten(),
            status,
            details: None,
        });
    }
    Ok((!message.content.is_empty()).then_some(message))
}

/// Titles retain only the published prefix, while checking nonempty whitespace over the full text.
pub(super) fn title(view: Option<JsonView<'_>>, strict: bool) -> Result<Option<String>> {
    let Some(view) = view else { return Ok(None) };
    if view.is_null()? {
        return Ok(None);
    }
    if !view.is_string()? {
        return if strict {
            Err(InvalidStruct("expected optional title string").into())
        } else {
            Ok(None)
        };
    }
    if strict && !view.accepts_value()? {
        return Err(InvalidStruct("invalid title string").into());
    }
    let mut prefix = String::new();
    let mut count = 0;
    let mut nonwhite = false;
    view.for_each_str_chunk(|chunk| {
        for c in chunk.chars() {
            nonwhite |= !c.is_whitespace();
            if count < 81 {
                prefix.push(c);
                count += 1;
            }
        }
        Ok(())
    })?;
    Ok(nonwhite.then(|| crate::ir::truncate(&prefix, 80)))
}
