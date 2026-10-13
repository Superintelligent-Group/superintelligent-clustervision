//! Existing v3 records, written from guarded leaves without payload-bearing intermediate Values.
use super::*;
use crate::{
    harness::wire::write::{self as wire, ObjectWriter},
    lazy::Resolver,
};
use std::io::Write;

pub(super) fn content(writer: &mut dyn Write, blocks: &[Block], mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    writer.write_all(b"[")?;
    let mut first = true;
    for block in blocks {
        if matches!(block, Block::ToolResult { .. }) {
            continue;
        }
        let fate = crate::emit::thinking::fate(block, Harness::OpenClaw, mode, resolver)?;
        if fate == ThinkingFate::Drop {
            continue;
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        if let Some(text) = crate::emit::thinking::text_with_fate(block, fate) {
            let mut projected = ObjectWriter::new(writer)?;
            projected.field("type", "text")?;
            projected.field_with("text", |w| wire::text(w, &text, resolver))?;
            projected.finish()?;
        } else if let Block::ToolUse {
            id,
            name,
            input,
            namespace: Some(namespace),
        } = block
        {
            if namespace == crate::affordance::DEFERRED {
                // The shared replay definition supplies OpenClaw's distinct {id,args} keys.
                let (name_key, args_key) = crate::affordance::deferred_keys(Harness::OpenClaw);
                let mut call = ObjectWriter::new(writer)?;
                call.field("type", anthropic::OPENCLAW.tool_use.0)?;
                call.field("id", id)?;
                call.field("name", crate::affordance::DEFERRED)?;
                call.field_with(anthropic::OPENCLAW.tool_use.1, |w| {
                    let mut wrapper = ObjectWriter::new(w)?;
                    wrapper.field(name_key, name)?;
                    wrapper.field_with(args_key, |w| input.write_canonical(&mut &mut *w))?;
                    wrapper.finish()
                })?;
                call.finish()?;
            } else {
                anthropic::write::block(writer, block, &anthropic::OPENCLAW, false, resolver)?;
            }
        } else {
            anthropic::write::block(writer, block, &anthropic::OPENCLAW, false, resolver)?;
        }
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn summary(writer: &mut dyn Write, msg: &crate::Message, mode: ThinkingMode, resolver: &Resolver) -> Result<()> {
    let mut texts = Vec::new();
    for block in &msg.content {
        if let Some(text) = crate::emit::thinking::text(block, Harness::OpenClaw, mode, resolver)? {
            texts.push(text);
        }
    }
    wire::joined_text(writer, texts.iter().map(|v| v.as_ref()), resolver)
}

fn tool_result(writer: &mut dyn Write, msg: &crate::Message, timestamp: i64, resolver: &Resolver) -> Result<()> {
    let result = msg.content.iter().find_map(|block| {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            ..
        } = block
        {
            Some((tool_use_id.as_str(), content, *is_error, tool_name.as_deref()))
        } else {
            None
        }
    });
    let mut inner = ObjectWriter::new(writer)?;
    inner.field("role", "toolResult")?;
    inner.field("toolCallId", result.map(|r| r.0).unwrap_or(""))?;
    if let Some(name) = result.and_then(|r| r.3) {
        inner.field("toolName", name)?;
    }
    inner.field_with("content", |w| {
        w.write_all(b"[")?;
        let mut part = ObjectWriter::new(w)?;
        part.field("type", "text")?;
        part.field_with("text", |w| match result {
            Some((_, text, _, _)) => wire::text(w, text, resolver),
            None => serde_json::to_writer(w, "").map_err(Into::into),
        })?;
        part.finish()?;
        w.write_all(b"]")?;
        Ok(())
    })?;
    inner.field("isError", result.is_some_and(|r| r.2))?;
    inner.field("timestamp", timestamp)?;
    inner.finish()
}

pub(super) fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    wire::check_source_fields(session)?;
    let resolver = session.resolver();
    // Match explicit-title precedence, first-user selection, whitespace filtering and truncation.
    let label = wire::fallback_title(session, Harness::OpenClaw, opts.thinking, &resolver)?;
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let agents_root = if out_dir.file_name().is_some_and(|n| n == "agents") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("agents")
    };
    let sessions_dir = agents_root.join("main/sessions");
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut lines = JsonlStage::new();
    lines.push_with(|w| {
        let mut header = ObjectWriter::new(w)?;
        header.field("type", "session")?;
        header.field("version", 3)?;
        header.field("id", &new_id)?;
        header.field("timestamp", &created_iso)?;
        if !cwd_str.is_empty() {
            header.field("cwd", &cwd_str)?;
        }
        header.finish()
    })?;
    if let Some(title) = session.title.as_deref().filter(|t| !t.trim().is_empty()) {
        lines.push_with(|w| {
            let mut row = ObjectWriter::new(w)?;
            row.field("type", "session_info")?;
            row.field("id", openclaw_short_id())?;
            row.field("parentId", Value::Null)?;
            row.field("timestamp", &created_iso)?;
            row.field("name", title)?;
            row.finish()
        })?;
    }
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        let entry_id = openclaw_short_id();
        let ts = msg.timestamp.unwrap_or(created);
        let control = match msg.kind {
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary => Some("compaction"),
            MessageKind::Branch => Some("reset"),
            MessageKind::ModelChange => Some("model_change"),
            _ => None,
        };
        lines.push_with(|w| {
            let mut row = ObjectWriter::new(w)?;
            row.field("type", control.unwrap_or("message"))?;
            row.field("id", &entry_id)?;
            row.field("parentId", &parent)?;
            row.field("timestamp", ts.to_rfc3339_opts(SecondsFormat::Millis, true))?;
            match control {
                Some("compaction") => row.field_with("summary", |w| summary(w, msg, opts.thinking, &resolver))?,
                Some("reset") => row.field("reason", "reset")?,
                Some("model_change") => {
                    row.field("provider", Value::Null)?;
                    row.field("modelId", msg.model.as_ref().or(session.model.as_ref()))?;
                }
                _ => row.field_with("message", |w| {
                    if msg.role == Role::Tool {
                        return tool_result(w, msg, ts.timestamp_millis(), &resolver);
                    }
                    let mut inner = ObjectWriter::new(w)?;
                    inner.field(
                        "role",
                        match msg.role {
                            Role::User => "user",
                            Role::System => "system",
                            _ => "assistant",
                        },
                    )?;
                    inner.field_with("content", |w| content(w, &msg.content, opts.thinking, &resolver))?;
                    inner.field("timestamp", ts.timestamp_millis())?;
                    if msg.role == Role::Assistant {
                        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                            inner.field("model", model)?;
                        }
                        if let Some(usage) = openclaw_usage(msg.usage.as_ref()) {
                            inner.field("usage", usage)?;
                        }
                    }
                    inner.finish()
                })?,
            }
            row.finish()
        })?;
        parent = Some(entry_id);
    }
    // All source demands complete before opening the transcript or changing its index.
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    lines.publish(&file_path)?;
    let index_path = sessions_dir.join("sessions.json");
    let mut index: Map<String, Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(label) = label {
        entry.insert("label".into(), json!(label));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    index.insert(new_id.clone(), Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Object(index))?)
        .with_context(|| format!("writing {}", index_path.display()))?;
    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}
