//! Codex's text and argument fields written directly into the shared JSONL stage.
use crate::harness::{
    store::{text, JsonlStage},
    wire::write::{append_text, text as write_text},
};
use crate::{emit::ThinkingMode, lazy::Resolver, Harness, Json, Message, Text};
use anyhow::Result;
use std::io::Write;

pub(super) fn record(
    lines: &mut JsonlStage,
    timestamp: &str,
    kind: &str,
    payload: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    lines.push_with(|writer| {
        writer.write_all(b"{\"type\":")?;
        serde_json::to_writer(&mut *writer, kind)?;
        writer.write_all(b",\"timestamp\":")?;
        serde_json::to_writer(&mut *writer, timestamp)?;
        writer.write_all(b",\"payload\":")?;
        payload(writer)?;
        writer.write_all(b"}")?;
        Ok(())
    })
}
pub(super) fn message_string(
    writer: &mut dyn Write,
    message: &Message,
    resolver: &Resolver,
    mode: ThinkingMode,
) -> Result<()> {
    text::generate_string(writer, |builder| {
        let mut started = false;
        for block in &message.content {
            if let Some(text) = crate::emit::thinking::text(block, Harness::Codex, mode, resolver)? {
                if started {
                    builder.push("\n")?;
                }
                started |= append_text(builder, &text, resolver)?;
            }
        }
        Ok(())
    })
}
pub(super) fn string(writer: &mut dyn Write, value: &Text, resolver: &Resolver, prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return write_text(writer, value, resolver);
    }
    text::generate_string(writer, |builder| {
        builder.push(prefix)?;
        append_text(builder, value, resolver)?;
        Ok(())
    })
}
pub(super) fn argument(writer: &mut dyn Write, value: &Json) -> Result<()> {
    text::generate_string(writer, |builder| value.with_view(|view| builder.push_json(view)))
}
#[allow(clippy::too_many_arguments)]
pub(super) fn message(
    lines: &mut JsonlStage,
    timestamp: &str,
    role: &str,
    content_type: &str,
    event_type: Option<&str>,
    message: &Message,
    resolver: &Resolver,
    mode: ThinkingMode,
) -> Result<()> {
    let mut texts = message
        .content
        .iter()
        .filter_map(|block| crate::emit::thinking::text(block, Harness::Codex, mode, resolver).transpose());
    match (texts.next().transpose()?, texts.next().transpose()?, event_type) {
        (Some(value), None, Some(event_type)) if value.inline_str().is_some() => {
            return super::push_text(
                lines,
                timestamp,
                role,
                content_type,
                event_type,
                value.inline_str().unwrap(),
            )
        }
        (None, None, Some(event_type)) => {
            return super::push_text(lines, timestamp, role, content_type, event_type, "")
        }
        _ => {}
    }
    twins(lines, timestamp, role, content_type, event_type, &|writer| {
        message_string(writer, message, resolver, mode)
    })
}
pub(super) fn text(lines: &mut JsonlStage, timestamp: &str, value: &Text, resolver: &Resolver) -> Result<()> {
    if let Some(inline) = value.inline_str() {
        return super::push_text(lines, timestamp, "assistant", "output_text", "agent_message", inline);
    }
    twins(
        lines,
        timestamp,
        "assistant",
        "output_text",
        Some("agent_message"),
        &|writer| string(writer, value, resolver, ""),
    )
}
fn twins(
    lines: &mut JsonlStage,
    timestamp: &str,
    role: &str,
    content_type: &str,
    event_type: Option<&str>,
    body: &impl Fn(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    record(lines, timestamp, "response_item", |writer| {
        writer.write_all(b"{\"type\":\"message\",\"role\":")?;
        serde_json::to_writer(&mut *writer, role)?;
        writer.write_all(b",\"content\":[{\"type\":")?;
        serde_json::to_writer(&mut *writer, content_type)?;
        writer.write_all(b",\"text\":")?;
        body(writer)?;
        writer.write_all(b"}]}")?;
        Ok(())
    })?;
    if let Some(event_type) = event_type {
        record(lines, timestamp, "event_msg", |writer| {
            writer.write_all(b"{\"type\":")?;
            serde_json::to_writer(&mut *writer, event_type)?;
            writer.write_all(b",\"message\":")?;
            body(writer)?;
            writer.write_all(b"}")?;
            Ok(())
        })?;
    }
    Ok(())
}
