//! Selected Grok payloads; the existing turn classifier receives only bounded representatives.
use super::*;
use crate::json::{JsonSource, JsonView};
use anyhow::Result;
use std::io::BufRead;

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
        if let Some(text) = part?.get("text")?.filter(|text| text.is_string().unwrap_or(false)) {
            parts.push(text);
        }
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
    let mut head = String::new();
    let mut present = false;
    text.try_for_each_chunk(&crate::lazy::Resolver::new(None), 4096, |chunk| {
        present |= !chunk.is_empty();
        if head.chars().count() >= 16 {
            return Ok(());
        }
        for c in chunk.chars() {
            if head.is_empty() && c.is_whitespace() {
                continue;
            }
            if head.chars().count() < 16 {
                head.push(c);
            }
        }
        Ok(())
    })?;
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
    let mut record = serde_json::Map::new();
    for key in [
        "type",
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
        payload.filter(|v| v.is_array().unwrap_or(false))
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
            inputs.push(match call.get("arguments")? {
                Some(value) if value.is_string()? => value.encoded_argument()?,
                Some(value) => openai::parse_arguments(Some(value.resolve()?.as_ref())).into(),
                None => serde_json::json!({}).into(),
            });
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
    reader: impl BufRead,
    offset: u64,
    source: &JsonSource,
    enrich: &HashMap<String, ToolEnrich>,
    accept: &mut impl FnMut(bool, Option<Message>, u64, bool, u64) -> Flow,
) -> Result<u64> {
    super::super::store::checked_lines::for_each(reader, offset, source, |view, offset, complete, skipped| {
        let (reasoning, message) = message(view, source, enrich)?;
        Ok(accept(reasoning, message, offset, complete, skipped))
    })
}
