//! Selected Grok payloads; the existing turn classifier receives only bounded representatives.
use super::*;
use crate::json::{JsonSource, JsonView};
use anyhow::Result;

fn join(view: Option<JsonView<'_>>, source: &JsonSource) -> Result<Text> {
    let Some(view) = view else { return Ok(Text::default()) };
    if let Some(text) = view.source_text()? {
        return Ok(text);
    }
    let Some(array) = view.array()? else {
        return Ok(Text::default());
    };
    let mut parts = Vec::new();
    for part in array {
        if let Some(text) = part?.get("text")? {
            if text.is_string()? {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        return Ok(Text::default());
    }
    if parts.len() == 1 {
        return Ok(parts[0].source_text()?.unwrap_or_default());
    }
    crate::harness::store::text::append(source, &source.decoded_backing()?, |out| {
        for (i, part) in parts.into_iter().enumerate() {
            if i != 0 {
                out.push("\n")?;
            }
            out.push_string(part)?;
        }
        Ok(())
    })
}
fn representative(text: &Text) -> Result<String> {
    let (mut head, present) = document::text_head(text, &crate::lazy::Resolver::new(None), 16, true)?;
    if head.is_empty() && present {
        head.push(' ');
    }
    Ok(head)
}
pub(super) fn message(
    view: JsonView<'_>,
    source: &JsonSource,
    enrich: &HashMap<String, ToolEnrich>,
) -> Result<(bool, Option<Message>)> {
    let ty = match view.get("type")? {
        Some(value) if value.is_string()? => value.resolve()?.as_str().unwrap_or_default().to_string(),
        _ => return Ok((false, None)),
    };
    // Strict whole-row validation is performed by the caller. Unsupported record kinds
    // have no conversational shape, so do not construct decoded bodies or arguments for them.
    if !matches!(
        ty.as_str(),
        "reasoning" | "tool_result" | "system" | "user" | "assistant"
    ) {
        return Ok((false, None));
    }
    let mut record = serde_json::Map::new();
    record.insert("type".into(), Value::String(ty));
    for key in [
        "model_id",
        "model_fingerprint",
        "synthetic_reason",
        "encrypted_content",
        "tool_call_id",
    ] {
        if let Some(value) = view.get(key)? {
            record.insert(key.into(), value.resolve()?.into_owned());
        }
    }
    let reasoning_record = record.get("type").and_then(Value::as_str) == Some("reasoning");
    let payload = view.get(if reasoning_record { "summary" } else { "content" })?;
    let payload = if reasoning_record {
        match payload {
            Some(payload) if payload.is_array()? => Some(payload),
            _ => None,
        }
    } else {
        payload
    };
    let content = join(payload, source)?;
    if reasoning_record {
        record.insert(
            "summary".into(),
            serde_json::json!([{"text": representative(&content)?}]),
        );
    } else {
        record.insert("content".into(), Value::String(representative(&content)?));
    }
    let mut reasoning = None;
    if let Some(value) = view.get("reasoning")? {
        let text = value
            .get("text")?
            .map(|v| v.source_text())
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let mut small = serde_json::Map::new();
        small.insert("text".into(), Value::String(representative(&text)?));
        if let Some(encrypted) = value.get("encrypted")? {
            small.insert("encrypted".into(), encrypted.resolve()?.into_owned());
        }
        record.insert("reasoning".into(), Value::Object(small));
        reasoning = Some(text);
    }
    let mut inputs = Vec::new();
    if let Some(calls) = view.get("tool_calls")?.map(|v| v.array()).transpose()?.flatten() {
        let mut small = Vec::new();
        for call in calls {
            let call = call?;
            let mut metadata = serde_json::Map::new();
            for key in ["id", "name"] {
                if let Some(value) = call.get(key)? {
                    metadata.insert(key.into(), value.resolve()?.into_owned());
                }
            }
            inputs.push(
                call.get("arguments")?
                    .map(JsonView::encoded_argument)
                    .transpose()?
                    .unwrap_or_else(|| serde_json::json!({}).into()),
            );
            small.push(Value::Object(metadata));
        }
        record.insert("tool_calls".into(), Value::Array(small));
    }
    let Some(mut message) = chat_message(&Value::Object(record), enrich) else {
        return Ok((reasoning_record, None));
    };
    let mut inputs = inputs.into_iter();
    for block in &mut message.content {
        match block {
            Block::Text { text } | Block::ToolResult { content: text, .. } => *text = content.clone(),
            Block::Thinking { text, .. } => {
                *text = if reasoning_record {
                    content.clone()
                } else {
                    reasoning.clone().unwrap_or_default()
                }
            }
            Block::ToolUse { input, .. } => *input = inputs.next().expect("one input per classified tool call"),
            _ => {}
        }
    }
    Ok((reasoning_record, Some(message)))
}

pub(super) fn for_each(
    file: &fs::File,
    offset: u64,
    source: &JsonSource,
    enrich: &HashMap<String, ToolEnrich>,
    accept: &mut impl FnMut(bool, Option<Message>, u64, bool, u64) -> Flow,
) -> Result<u64> {
    super::super::store::checked_lines::for_each(file, offset, source, |view, offset, complete, skipped| {
        let (reasoning, message) = message(view, source, enrich)?;
        Ok(accept(reasoning, message, offset, complete, skipped))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_records_and_empty_parts_create_no_decoded_body_backing() -> Result<()> {
        let world = cv_testkit::World::in_process("grok-ignored-selected-body");
        let path = world.base.join("rows.json");
        let body = "雪\\n".repeat(128 * 1024);
        for record in [
            serde_json::json!({"type":"ignored","content":[{"text":body},{"text":body}],"tool_calls":[{"arguments":body}]}),
            serde_json::json!({"type":null,"content":[{"text":body},{"text":body}]}),
            serde_json::json!({"type":"user","content":[{"image":body}]}),
            serde_json::json!({"type":"reasoning","summary":[]}),
        ] {
            fs::write(&path, serde_json::to_vec(&record)?)?;
            let source = JsonSource::capture(&path)?;
            let backing = source.decoded_backing()?;
            let (_, actual) = source.read_document(|view| message(view, &source, &HashMap::new()))?;
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(chat_message(&record, &HashMap::new()))?
            );
            assert_eq!(backing.file()?.metadata()?.len(), 0);
        }
        Ok(())
    }
}
