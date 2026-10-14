//! Positioned projections of independently accepted recording messages. Large strings/arguments
//! use the shared JSON view transport rather than an encoded document or record buffer.
use super::*;
use crate::harness::store::document::Fields;
use crate::json::JsonView;

pub(crate) fn emit(
    view: JsonView<'_>,
    output: output::Output<'_>,
    harness: Harness,
    st: &mut EmitState,
    resolver: &Resolver,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    const META: &[&str] = &["type", "id", "timestamp", "model", "tokens", "rewindTo"];
    let Some(rec) = Fields::parse(
        view,
        &[
            "type",
            "id",
            "timestamp",
            "model",
            "tokens",
            "thoughts",
            "displayContent",
            "rewindTo",
            "content",
            "toolCalls",
        ],
    )?
    else {
        return Ok(Flow::Continue);
    };
    let mut meta = rec.project(META)?;
    let display_text = rec.get("displayContent").map(output::display_text).transpose()?;
    let mut thoughts = Vec::new();
    if let Some(value) = rec.get("thoughts") {
        let (headers, blocks) = output.thoughts(value)?;
        meta.insert("thoughts".into(), Value::Array(headers));
        thoughts = blocks;
    }
    let mut content = Vec::new();
    let mut tool_response = false;
    if let Some(raw) = rec.get("content") {
        content_blocks(raw, output, &mut content)?;
        if let Some(parts) = raw.array()? {
            let mut count = 0;
            tool_response = true;
            for part in parts {
                count += 1;
                if part?.get("functionResponse")?.is_none() {
                    tool_response = false;
                    break;
                }
            }
            tool_response &= count != 0;
        }
    }
    let mut results = Vec::new();
    let mut inputs = Vec::new();
    if let Some(calls) = rec.get("toolCalls").map(JsonView::array).transpose()?.flatten() {
        let mut headers = Vec::new();
        for call in calls {
            let call = Fields::parse(
                call?,
                &["id", "name", "agentId", "status", "args", "result", "resultDisplay"],
            )?;
            let (header, input, result) = if let Some(call) = call {
                let header = call.project(&["id", "name", "agentId", "status"])?;
                let input = call
                    .get("args")
                    .map(|v| v.argument(true))
                    .transpose()?
                    .unwrap_or_default();
                let result = match call.get("result").map(|v| output.result(v)).transpose()?.flatten() {
                    Some(text) => Some(text),
                    None => call.get("resultDisplay").map(|v| v.text(true)).transpose()?.flatten(),
                };
                (header, input, result)
            } else {
                (Default::default(), crate::Json::default(), None)
            };
            headers.push(Value::Object(header));
            inputs.push(Some(input));
            results.push(result);
        }
        meta.insert("toolCalls".into(), Value::Array(headers));
    }
    let payload = Payload {
        content: std::cell::RefCell::new(content),
        results: std::cell::RefCell::new(results),
        inputs: std::cell::RefCell::new(inputs),
        thoughts: Some(std::cell::RefCell::new(thoughts)),
        display_text: std::cell::RefCell::new(display_text),
        tool_response,
        resolver,
    };
    Ok(emit_record_message_with(harness, &meta, st, Some(&payload), sink))
}

fn content_blocks(view: JsonView<'_>, output: output::Output<'_>, out: &mut Vec<Block>) -> Result<()> {
    if let Some(text) = view.text(true)? {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
    } else if let Some(parts) = view.array()? {
        for part in parts {
            part_block(part?, output, out)?;
        }
    } else {
        part_block(view, output, out)?;
    }
    Ok(())
}

fn object_field<'a>(fields: &Fields<'a>, key: &str) -> Result<Option<JsonView<'a>>> {
    match fields.get(key) {
        Some(view) if view.is_object()? => Ok(Some(view)),
        _ => Ok(None),
    }
}

fn part_block(view: JsonView<'_>, output: output::Output<'_>, out: &mut Vec<Block>) -> Result<()> {
    if let Some(text) = view.text(true)? {
        if !text.is_empty() {
            out.push(Block::Text { text });
        }
        return Ok(());
    }
    let Some(obj) = Fields::parse(
        view,
        &[
            "functionCall",
            "functionResponse",
            "inlineData",
            "fileData",
            "text",
            "thought",
        ],
    )?
    else {
        return Ok(());
    };
    if let Some(fc) = object_field(&obj, "functionCall")? {
        if let Some(fc) = Fields::parse(fc, &["name", "id", "args"])? {
            let name = fc.string("name")?.unwrap_or_else(|| "tool".into());
            let id = fc.string("id")?.unwrap_or_else(|| format!("{name}-call"));
            out.push(Block::ToolUse {
                id,
                name,
                input: fc
                    .get("args")
                    .map(|v| v.argument(true))
                    .transpose()?
                    .unwrap_or_default(),
                namespace: None,
            });
        }
    } else if let Some(fr) = object_field(&obj, "functionResponse")? {
        if let Some(fr) = Fields::parse(fr, &["name", "id", "response"])? {
            let name = fr.string("name")?.unwrap_or_else(|| "tool".into());
            let id = fr.string("id")?.unwrap_or_else(|| format!("{name}-call"));
            out.push(Block::ToolResult {
                tool_use_id: id,
                content: fr
                    .get("response")
                    .map(|v| output.value(v))
                    .transpose()?
                    .unwrap_or_default(),
                is_error: false,
                tool_name: Some(name),
                status: None,
                details: None,
            });
        }
    } else if let Some(inline) = object_field(&obj, "inlineData")? {
        if let Some(inline) = Fields::parse(inline, &["mimeType"])? {
            out.push(Block::Image {
                media_type: inline.string("mimeType")?,
                data_ref: None,
            });
        }
    } else if let Some(fd) = object_field(&obj, "fileData")? {
        if let Some(fd) = Fields::parse(fd, &["mimeType", "fileUri"])? {
            out.push(Block::File {
                mime: fd.string("mimeType")?,
                path: None,
                source: fd.string("fileUri")?,
            });
        }
    } else if let Some(text) = obj.get("text").map(|v| v.text(true)).transpose()?.flatten() {
        if !text.is_empty() {
            let thought = obj.get("thought").map(JsonView::as_bool).transpose()?.flatten() == Some(true);
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
    Ok(())
}
