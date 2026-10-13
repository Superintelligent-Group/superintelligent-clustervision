//! Selected Goose wire fields over checked source ranges.
use super::*;

/// Reducers require visibility, usage, and model identity, not arbitrary native operations.
pub(super) fn project_metadata(view: crate::json::JsonView<'_>) -> Result<Value> {
    use super::super::store::document::Fields;
    let Some(fields) = Fields::parse(
        view,
        &["userVisible", "agentVisible", "turnContext", "usage", "inference"],
    )?
    else {
        return Ok(Value::Null);
    };
    let mut out = serde_json::Map::new();
    for key in ["userVisible", "agentVisible", "turnContext"] {
        if let Some(flag) = fields.get(key).map(|v| v.as_bool()).transpose()?.flatten() {
            out.insert(key.into(), Value::Bool(flag));
        }
    }
    if let Some(usage) = fields
        .get("usage")
        .map(|v| {
            Fields::parse(
                v,
                &[
                    "inputTokens",
                    "outputTokens",
                    "cacheReadTokens",
                    "cacheWriteTokens",
                    "cost",
                    "isCompaction",
                ],
            )
        })
        .transpose()?
        .flatten()
    {
        let mut out_usage = serde_json::Map::new();
        for key in [
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheWriteTokens",
            "cost",
        ] {
            if let Some(number) = usage.get(key).map(|v| v.as_number()).transpose()?.flatten() {
                out_usage.insert(key.into(), Value::Number(number));
            }
        }
        if let Some(flag) = usage.get("isCompaction").map(|v| v.as_bool()).transpose()?.flatten() {
            out_usage.insert("isCompaction".into(), Value::Bool(flag));
        }
        out.insert("usage".into(), Value::Object(out_usage));
    }
    if let Some(inference) = fields
        .get("inference")
        .map(|v| Fields::parse(v, &["provider", "resolvedModel", "requestedModel"]))
        .transpose()?
        .flatten()
    {
        let mut out_inference = serde_json::Map::new();
        for key in ["provider", "resolvedModel", "requestedModel"] {
            // Preserve present-but-invalid resolvedModel precedence from the native decoder.
            if inference.get(key).is_some() {
                out_inference.insert(
                    key.into(),
                    inference.string(key)?.map(Value::String).unwrap_or(Value::Null),
                );
            }
        }
        out.insert("inference".into(), Value::Object(out_inference));
    }
    Ok(Value::Object(out))
}

/// Project ordinary text and reasoning without decoding their payloads into owned trees.
pub(super) fn decode_content_view(
    view: crate::json::JsonView<'_>,
    tools: &mut HashMap<String, String>,
    opts: &ParseOptions,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<super::super::store::bytes::ByteSource>,
) -> Result<Decoded> {
    use super::super::store::document::Fields;
    if !opts.spans {
        return Ok(decode_content(view.resolve()?.as_ref(), tools));
    }
    let Some(items) = view.array()? else {
        let mut d = Decoded::default();
        if let Some(text) = view.source_text()? {
            if text
                .try_resolve_prefix(&crate::lazy::Resolver::new(None), 4)?
                .is_empty()
            {
                return Ok(d);
            }
            d.blocks.push(Block::Text { text });
            d.substantive = true;
        }
        return Ok(d);
    };
    let mut d = Decoded::default();
    for item in items {
        let item = item?;
        let fields = Fields::parse(item, &["type"])?;
        let ty = fields.as_ref().map(|f| f.string("type")).transpose()?.flatten();
        if matches!(
            ty.as_deref(),
            Some("text" | "thinking" | "redactedThinking" | "tool_use" | "tool_result")
        ) {
            let q = anthropic::Quirks {
                flat_media: None,
                ..anthropic::GOOSE
            };
            if let Some(block) = anthropic::block_from_view(item, &q, opts.spans)? {
                d.blocks.push(block);
                d.substantive = true;
            }
        } else if matches!(ty.as_deref(), Some("image" | "document")) {
            let fields = Fields::parse(item, &["mimeType", "data", "name"])?.expect("object with type");
            let media_type = fields.string("mimeType")?;
            let data = fields.get("data").map(|v| v.source_text()).transpose()?.flatten();
            let block = if ty.as_deref() == Some("image") {
                let data_ref = data
                    .map(|text| {
                        text.try_resolve_prefix(&crate::lazy::Resolver::new(None), 484)
                            .map(|prefix| crate::ir::truncate(&prefix, 120))
                    })
                    .transpose()?;
                Block::Image { media_type, data_ref }
            } else {
                Block::File {
                    mime: media_type,
                    path: fields.string("name")?,
                    source: data.map(|_| "base64:inline".into()),
                }
            };
            d.blocks.push(block);
            d.substantive = true;
        } else if matches!(
            ty.as_deref(),
            Some("error" | "systemNotification" | "actionRequired" | "toolConfirmationRequest")
        ) {
            let fields = Fields::parse(
                item,
                &["kind", "message", "msg", "toolName", "data", "actionType", "id"],
            )?
            .expect("object with type");
            let (prefix, payload, suffix) = match ty.as_deref() {
                Some("error") => (
                    format!("[error: {}] ", fields.string("kind")?.unwrap_or_else(|| "other".into())),
                    fields.get("message"),
                    "",
                ),
                Some("systemNotification") => ("[SystemNotification: ".into(), fields.get("msg"), "]"),
                Some("toolConfirmationRequest") => ("[ToolConfirmationRequest: ".into(), fields.get("toolName"), "]"),
                _ => {
                    let data = fields.get("data").unwrap_or(item);
                    let data = Fields::parse(data, &["actionType", "toolName", "message", "id"])?;
                    let action = data.as_ref().map(|f| f.string("actionType")).transpose()?.flatten();
                    let (prefix, key) = match action.as_deref() {
                        Some("toolConfirmation") => ("[ActionRequired: ToolConfirmation for ".into(), Some("toolName")),
                        Some("elicitation") => ("[ActionRequired: Elicitation - ".into(), Some("message")),
                        Some("elicitationResponse") => ("[ActionRequired: ElicitationResponse for ".into(), Some("id")),
                        Some("toolConfirmationResponse") => {
                            ("[ActionRequired: ToolConfirmationResponse for ".into(), Some("id"))
                        }
                        other => (format!("[ActionRequired: {}", other.unwrap_or("?")), None),
                    };
                    (prefix, key.and_then(|k| data.as_ref().and_then(|f| f.get(k))), "]")
                }
            };
            let text = super::super::store::text::append(source, arena, |b| {
                b.push(&prefix)?;
                if let Some(v) = payload {
                    b.push_string(v)?;
                }
                b.push(suffix)
            })?;
            d.blocks.push(Block::Text { text });
            if ty.as_deref() == Some("error") {
                if d.error.is_none() {
                    d.error = Some(if !opts.wants_full_extra() {
                        Value::Object(Default::default())
                    } else {
                        serde_json::json!({ "kind": fields.string("kind")?.unwrap_or_else(|| "other".into()), "message": fields.string("message")?.unwrap_or_default() })
                    });
                }
            } else if opts.wants_full_extra() {
                d.control.push(item.resolve()?.into_owned());
            } else {
                d.control.push(Value::Null);
            }
        } else if matches!(
            ty.as_deref(),
            Some("toolRequest" | "frontendToolRequest" | "toolResponse")
        ) {
            if let Some(block) = tool_from_view(item, tools, opts, source, arena)? {
                d.blocks.push(block);
                d.substantive = true;
            }
        }
    }
    // Anthropic's shared projection exposes original-file string spans. Goose retains them as
    // checked source values so collected windows remain usable without a primary-path resolver.
    for block in &mut d.blocks {
        let text = match block {
            Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => text,
            _ => continue,
        };
        if let crate::lazy::Text::Span(span) = text {
            let crate::json::Json::Source(range) = source.validated_range(span.offset - 1, span.len + 2)? else {
                unreachable!("validated range is source-backed");
            };
            *text = crate::lazy::Text::Source(crate::lazy::SourceText::new(range));
        }
    }
    Ok(d)
}

fn tool_from_view(
    view: crate::json::JsonView<'_>,
    tools: &mut HashMap<String, String>,
    opts: &ParseOptions,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<super::super::store::bytes::ByteSource>,
) -> Result<Option<Block>> {
    use super::super::store::document::Fields;
    let Some(item) = Fields::parse(view, &["type", "id", "toolCall", "toolResult"])? else {
        return Ok(None);
    };
    let id = item.string("id")?.unwrap_or_default();
    if item.string("type")?.as_deref() != Some("toolResponse") {
        let Some(call) = item
            .get("toolCall")
            .map(|v| Fields::parse(v, &["status", "error", "value"]))
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        if call.string("status")?.as_deref() == Some("error") {
            let error = call.get("error");
            let (offset, len) = arena.append(|mut writer| {
                writer.write_all(b"{\"error\":")?;
                if let Some(v) = string_view(error)? {
                    v.write_canonical(&mut writer)?;
                } else {
                    writer.write_all(b"\"\"")?;
                }
                writer.write_all(b"}")?;
                Ok(())
            })?;
            let input = source.read_backing_value(arena.clone(), offset, len, |v| v.argument(opts.spans))?;
            return Ok(Some(Block::ToolUse {
                id,
                name: String::new(),
                input,
                namespace: None,
            }));
        }
        let Some(value_view) = call.get("value") else {
            return Ok(None);
        };
        let value = Fields::parse(value_view, &["name", "arguments"])?;
        let name = value
            .as_ref()
            .map(|f| f.string("name"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let input = value
            .as_ref()
            .and_then(|f| f.get("arguments"))
            .map(|v| v.argument(opts.spans))
            .transpose()?
            .unwrap_or_default();
        if !id.is_empty() && !name.is_empty() {
            tools.insert(id.clone(), name.clone());
        }
        return Ok(Some(Block::ToolUse {
            id,
            name,
            input,
            namespace: None,
        }));
    }
    let Some(result_view) = item.get("toolResult") else {
        return Ok(None);
    };
    let result = Fields::parse(result_view, &["status", "error", "value"])?;
    let value = result.as_ref().and_then(|f| f.get("value"));
    let fields = value
        .map(|v| Fields::parse(v, &["content", "isError", "structuredContent", "_meta"]))
        .transpose()?
        .flatten();
    let mut details = serde_json::Map::new();
    if opts.wants_full_extra() {
        if let Some(fields) = &fields {
            for key in ["structuredContent", "_meta"] {
                if let Some(v) = fields.value(key)? {
                    details.insert(key.into(), v);
                }
            }
        }
    }
    let error = result
        .as_ref()
        .map(|f| f.string("status"))
        .transpose()?
        .flatten()
        .as_deref()
        == Some("error");
    let content = if error {
        result
            .as_ref()
            .and_then(|f| f.get("error"))
            .map(|v| v.source_text())
            .transpose()?
            .flatten()
            .unwrap_or_default()
    } else {
        let content = if value.map(|v| v.is_array()).transpose()?.unwrap_or(false) {
            value
        } else {
            fields.as_ref().and_then(|f| f.get("content"))
        };
        match content {
            Some(v) => flatten_result_view(v, source, arena)?,
            None => crate::lazy::Text::default(),
        }
    };
    let is_error = error
        || fields
            .as_ref()
            .and_then(|f| f.get("isError"))
            .map(|v| v.as_bool())
            .transpose()?
            .flatten()
            .unwrap_or(false);
    Ok(Some(Block::ToolResult {
        tool_use_id: id.clone(),
        content,
        is_error,
        tool_name: tools.get(&id).cloned(),
        status: None,
        details: (!details.is_empty()).then_some(Value::Object(details)),
    }))
}

fn flatten_result_view(
    view: crate::json::JsonView<'_>,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<super::super::store::bytes::ByteSource>,
) -> Result<crate::lazy::Text> {
    use super::super::store::document::Fields;
    let Some(items) = view.array()? else {
        return Ok(view.text(true)?.unwrap_or_default());
    };
    super::super::store::text::append(source, arena, |builder| {
        let mut first = true;
        for item in items {
            let item = item?;
            let Some(fields) = Fields::parse(item, &["type", "text", "mimeType", "resource", "uri"])? else {
                continue;
            };
            let ty = fields.string("type")?;
            let (text, marker) = match ty.as_deref() {
                Some("text") => (string_view(fields.get("text"))?, None),
                Some("image" | "audio") => (
                    None,
                    Some(format!(
                        "[{}: {}]",
                        ty.as_deref().unwrap(),
                        fields.string("mimeType")?.unwrap_or_else(|| ty.clone().unwrap())
                    )),
                ),
                Some("resource_link") => (
                    None,
                    Some(format!(
                        "[resource: {}]",
                        fields.string("uri")?.unwrap_or_else(|| "?".into())
                    )),
                ),
                Some("resource") => {
                    let resource = fields.get("resource").unwrap_or(item);
                    let resource = Fields::parse(resource, &["text", "mimeType"])?;
                    let text = string_view(resource.as_ref().and_then(|f| f.get("text")))?;
                    let marker = if text.is_none() {
                        Some(format!(
                            "[blob: {}]",
                            resource
                                .as_ref()
                                .map(|f| f.string("mimeType"))
                                .transpose()?
                                .flatten()
                                .unwrap_or_else(|| "binary".into())
                        ))
                    } else {
                        None
                    };
                    (text, marker)
                }
                _ => continue,
            };
            if text.is_none() && marker.is_none() {
                continue;
            }
            if !first {
                builder.push("\n")?;
            }
            first = false;
            if let Some(text) = text {
                builder.push_string(text)?;
            }
            if let Some(marker) = marker {
                builder.push(&marker)?;
            }
        }
        Ok(())
    })
}

fn string_view(view: Option<crate::json::JsonView<'_>>) -> Result<Option<crate::json::JsonView<'_>>> {
    match view {
        Some(view) if view.is_string()? => Ok(Some(view)),
        _ => Ok(None),
    }
}
