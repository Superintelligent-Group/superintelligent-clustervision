//! Selected CLI fields over the shared strict JSON view. Large bodies use the same
//! retained backing/transport as ordinary Text and Json; identifiers and metadata remain owned.
use super::*;
use crate::harness::store::document::Fields;
use crate::json::JsonView;

fn string(view: Option<JsonView<'_>>) -> Result<Option<String>> {
    view.map(|v| {
        Ok(v.text(false)?.and_then(|text| match text {
            crate::lazy::Text::Inline(s) => Some(s),
            _ => None,
        }))
    })
    .transpose()
    .map(Option::flatten)
}
fn nonempty(text: &crate::lazy::Text) -> bool {
    match text {
        crate::lazy::Text::Inline(text) => !text.is_empty(),
        crate::lazy::Text::Source(text) => text.raw_len() != 0,
        _ => true,
    }
}

pub(super) fn model(view: JsonView<'_>) -> Result<Option<String>> {
    let value = view
        .get("providerOptions")?
        .map(|v| v.get("cursor"))
        .transpose()?
        .flatten()
        .map(|v| v.get("systemPromptFingerprint"))
        .transpose()?
        .flatten()
        .map(|v| v.get("model"))
        .transpose()?
        .flatten();
    string(value)
}

pub(super) fn message(view: JsonView<'_>) -> Result<Option<Message>> {
    // The snapshot's read_backing_value entry already validated the complete record, including
    // ignored fields. Repeating validation here would rescan giant payloads before projection.
    let Some(fields) = Fields::parse(view, &["role", "id", "content", "providerOptions"])? else {
        return Ok(None);
    };
    let role = match fields.string("role")?.as_deref() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        Some("system" | "developer") => Role::System,
        _ => return Ok(None),
    };
    let cursor = fields
        .get("providerOptions")
        .map(|v| v.get("cursor"))
        .transpose()?
        .flatten();
    let context = cursor
        .map(|v| v.get("requestContextCompleteness"))
        .transpose()?
        .flatten();
    let injected =
        role == Role::System || (role == Role::User && context.map(|v| v.is_null()).transpose()? == Some(false));
    let summary = cursor
        .map(|v| v.get("isSummary"))
        .transpose()?
        .flatten()
        .map(|v| v.as_bool())
        .transpose()?
        .flatten()
        == Some(true);
    let error = cursor
        .map(|v| v.get("highLevelToolCallResult"))
        .transpose()?
        .flatten()
        .map(|v| v.get("isError"))
        .transpose()?
        .flatten()
        .map(|v| v.as_bool())
        .transpose()?
        .flatten()
        == Some(true);
    let mut message = if injected {
        Message::of_kind(role, MessageKind::InjectedContext, Origin::Harness)
    } else if summary {
        Message::of_kind(role, MessageKind::CompactionSummary, Origin::Harness)
    } else {
        Message::new(role)
    };
    message.id = fields.string("id")?;
    if let Some(content) = fields.get("content") {
        if let Some(text) = content.text(true)? {
            if nonempty(&text) {
                message.content.push(Block::Text { text });
            }
        } else if let Some(parts) = content.array()? {
            for part in parts {
                let part = part?;
                let Some(fields) = Fields::parse(
                    part,
                    &[
                        "type",
                        "text",
                        "signature",
                        "toolCallId",
                        "toolName",
                        "args",
                        "result",
                        "isError",
                        "mimeType",
                        "filename",
                    ],
                )?
                else {
                    continue;
                };
                match fields.string("type")?.as_deref() {
                    Some("text") => {
                        if let Some(text) = fields
                            .get("text")
                            .map(|v| v.text(true))
                            .transpose()?
                            .flatten()
                            .filter(nonempty)
                        {
                            message.content.push(Block::Text { text });
                        }
                    }
                    Some(kind @ ("reasoning" | "redacted-reasoning")) => message.content.push(Block::Thinking {
                        text: fields
                            .get("text")
                            .map(|v| v.text(true))
                            .transpose()?
                            .flatten()
                            .unwrap_or_default(),
                        signature: fields.string("signature")?,
                        encrypted: None,
                        redacted: kind == "redacted-reasoning",
                    }),
                    Some("tool-call") => message.content.push(Block::ToolUse {
                        id: fields.string("toolCallId")?.unwrap_or_default(),
                        name: fields.string("toolName")?.unwrap_or_else(|| "tool".into()),
                        input: fields
                            .get("args")
                            .map(|v| v.argument(true))
                            .transpose()?
                            .unwrap_or_else(|| Value::Null.into()),
                        namespace: None,
                    }),
                    Some("tool-result") => {
                        let content = match fields.get("result") {
                            None => crate::lazy::Text::default(),
                            Some(result) => match result.text(true)? {
                                Some(text) => text,
                                None if result.is_null()? => crate::lazy::Text::default(),
                                // Native results are strings. Preserve nonstring results' exact Value serialization;
                                // this explicitly demanded legacy fallback still has an owned-object floor.
                                None => result.resolve()?.to_string().into(),
                            },
                        };
                        message.content.push(Block::ToolResult {
                            tool_use_id: fields.string("toolCallId")?.unwrap_or_default(),
                            content,
                            is_error: error
                                || fields.get("isError").map(|v| v.as_bool()).transpose()?.flatten() == Some(true),
                            tool_name: fields.string("toolName")?,
                            status: None,
                            details: None,
                        });
                    }
                    Some("image") => message.content.push(Block::Image {
                        media_type: fields.string("mimeType")?,
                        data_ref: Some("inline:cursor-cli".into()),
                    }),
                    Some("file") => message.content.push(Block::File {
                        mime: fields.string("mimeType")?,
                        path: fields.string("filename")?,
                        source: None,
                    }),
                    _ => {}
                }
            }
        }
    }
    Ok((!message.content.is_empty()).then_some(message))
}
