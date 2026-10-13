//! Checked native fields; structural metadata keeps the existing writer's layout.
use super::*;
use crate::{
    harness::{
        store::{bytes::ByteStage, publish_stage},
        wire::write::{text, ObjectWriter},
    },
    ir_json::PrettyJson,
    lazy::Resolver,
};
use std::io::{BufWriter, Write};

pub(super) fn transcript(
    session: &Session,
    id: &str,
    title: Option<&str>,
    cwd: Option<&str>,
    path: &Path,
    mode: crate::emit::ThinkingMode,
) -> Result<()> {
    let resolver = Resolver::new(session.source_path.clone());
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        let mut root = ObjectWriter::new(&mut pretty)?;
        root.field("sessionId", id)?;
        if let Some(title) = title {
            root.field("title", title)?;
        }
        if let Some(cwd) = cwd {
            root.field("workspaceDirectory", cwd)?;
        }
        root.field_with("history", |w| {
            w.write_all(b"[")?;
            for (i, msg) in session.messages.iter().enumerate() {
                if i != 0 {
                    w.write_all(b",")?;
                }
                item(w, msg, session.model.as_deref(), &resolver, mode)?;
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
        root.finish()?;
    }
    let stage = stage.into_inner().map_err(|e| e.into_error())?;
    publish_stage(stage, path)
}
fn item(
    w: &mut dyn Write,
    msg: &Message,
    model: Option<&str>,
    resolver: &Resolver,
    mode: crate::emit::ThinkingMode,
) -> Result<()> {
    let mut item = ObjectWriter::new(w)?;
    item.field_with("message", |w| {
        let mut message = ObjectWriter::new(w)?;
        message.field(
            "role",
            match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            },
        )?;
        if msg.role == Role::Tool {
            let result = msg.content.iter().find_map(|b| match b {
                Block::ToolResult {
                    tool_use_id, content, ..
                } => Some((tool_use_id, content)),
                _ => None,
            });
            message.field("toolCallId", result.map(|r| r.0.as_str()).unwrap_or(""))?;
            message.field_with("content", |w| match result {
                Some((_, content)) => text(w, content, resolver),
                None => {
                    serde_json::to_writer(w, "")?;
                    Ok(())
                }
            })?;
        } else {
            message.field_with("content", |w| {
                w.write_all(b"[")?;
                let mut first = true;
                for b in &msg.content {
                    let value = if mode == crate::emit::ThinkingMode::Native {
                        match b {
                            Block::Text { text } => Some(std::borrow::Cow::Borrowed(text)),
                            Block::Thinking { text, .. }
                                if crate::emit::thinking::first_character(text, resolver, false)?.is_some() =>
                            {
                                Some(std::borrow::Cow::Borrowed(text))
                            }
                            _ => None,
                        }
                    } else {
                        crate::emit::thinking::text(b, Harness::Continue, mode, resolver)?
                    };
                    if let Some(value) = value {
                        if !first {
                            w.write_all(b",")?;
                        }
                        first = false;
                        let mut part = ObjectWriter::new(w)?;
                        part.field("type", "text")?;
                        part.field_with("text", |w| text(w, &value, resolver))?;
                        part.finish()?;
                    } else if let Block::Image {
                        data_ref: Some(url), ..
                    } = b
                    {
                        if !first {
                            w.write_all(b",")?;
                        }
                        first = false;
                        serde_json::to_writer(&mut *w, &json!({"type":"imageUrl","imageUrl":{"url":url}}))?;
                    }
                }
                w.write_all(b"]")?;
                Ok(())
            })?;
            if msg.role == Role::Assistant && msg.content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
                message.field_with("toolCalls", |w| {
                    w.write_all(b"[")?;
                    let mut first = true;
                    for b in &msg.content {
                        if let Block::ToolUse { id, name, input, .. } = b {
                            if !first {
                                w.write_all(b",")?;
                            }
                            first = false;
                            let mut call = ObjectWriter::new(w)?;
                            call.field("id", id)?;
                            call.field("type", "function")?;
                            call.field_with("function", |w| {
                                let mut f = ObjectWriter::new(w)?;
                                f.field("name", name)?;
                                f.field_with("arguments", |w| {
                                    crate::harness::store::text::generate_string(w, |builder| {
                                        input.with_view(|view| builder.push_json(view))
                                    })
                                })?;
                                f.finish()
                            })?;
                            call.finish()?;
                        }
                    }
                    w.write_all(b"]")?;
                    Ok(())
                })?;
            }
        }
        message.finish()
    })?;
    item.field_with("contextItems", |w| {
        w.write_all(b"[")?;
        let mut first = true;
        for b in &msg.content {
            if msg.role != Role::Tool {
                if let Block::File { path, source, mime } = b {
                    if !first {
                        w.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut *w, &file_context_item(path, source, mime))?;
                }
            }
        }
        w.write_all(b"]")?;
        Ok(())
    })?;
    if msg.role == Role::Assistant {
        if let Some(model) = msg.model.as_deref().or(model) {
            item.field("promptLogs", json!([{"modelTitle":model}]))?;
        }
    }
    item.finish()
}

#[cfg(test)]
fn legacy_emit(
    session: &Session,
    out_dir: &Path,
    opts: &crate::emit::EmitOptions,
) -> Result<crate::harness::EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    use anyhow::Context;

    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ── transcript: <sessionId>.json ──
    let title = session
        .title
        .clone()
        .or_else(|| session.first_user_text())
        .map(|t| crate::ir::truncate(&t, 80));

    let mut history: Vec<Value> = Vec::new();
    for msg in &session.messages {
        history.push(history_item(msg, session.model.as_deref())?);
    }

    let mut transcript = Map::new();
    transcript.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        transcript.insert("title".into(), json!(t));
    }
    if let Some(c) = &cwd_str {
        transcript.insert("workspaceDirectory".into(), json!(c));
    }
    transcript.insert("history".into(), Value::Array(history));

    let file_path = out_dir.join(format!("{new_id}.json"));
    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(transcript))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    // ── index: sessions.json (merge-or-create) ──
    let index_path = out_dir.join("sessions.json");
    let mut entries: Vec<Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    // Drop any stale entry for this id, then append a fresh one.
    entries.retain(|e| e.get("sessionId").and_then(Value::as_str) != Some(new_id.as_str()));
    let created = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(chrono::Utc::now);
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        entry.insert("title".into(), json!(t));
    }
    // `dateCreated` is a string holding millisecond epoch (the older-build shape, which the parser
    // accepts alongside ISO).
    entry.insert("dateCreated".into(), json!(created.timestamp_millis().to_string()));
    if let Some(c) = &cwd_str {
        entry.insert("workspaceDirectory".into(), json!(c));
    }
    entries.push(Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Array(entries))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(crate::harness::EmitResult {
        path: file_path,
        new_id,
        resume_hint: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_native_writer_matches_owned_oracle_and_preserves_changed_source() -> Result<()> {
        let world = cv_testkit::World::in_process("checked-continuedev");
        let path = world.base.join("source.jsonl");
        let mut rows = [
            json!({"type":"user","message":{"role":"user","content":[{"type":"thinking","thinking":"user thought","signature":"user-sig"},{"type":"text","text":" \n"},{"type":"text","text":"λ prompt".repeat(3000)}]}}),
            json!({"type":"assistant","message":{"role":"assistant","model":"test-model","usage":{"input_tokens":7,"output_tokens":3},"content":[{"type":"thinking","thinking":"reasoning \nλ".repeat(3000),"signature":"sig"},{"type":"tool_use","id":"call","name":"Read","input":{"z":2,"a":"input"}},{"type":"text","text":"answer"}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"result \nλ".repeat(3000)}]}}),
        ];
        for row in &mut rows {
            row["timestamp"] = json!("2026-10-13T12:00:00Z");
        }
        fs::write(
            &path,
            rows.iter()
                .map(|v| serde_json::to_string(v).unwrap() + "\n")
                .collect::<String>(),
        )?;
        let adapter = crate::harness::claude::Claude::detect();
        let reference = crate::SessionRef::new("synthetic", Harness::Claude, &path);
        let mut lazy = crate::emit::parse_for_emit(&adapter, &reference, Harness::Continue)?;
        lazy.messages[0].content.extend([
            Block::File {
                path: Some("/synthetic/input.txt".into()),
                source: None,
                mime: Some("text/plain".into()),
            },
            Block::Image {
                media_type: Some("image/png".into()),
                data_ref: Some("https://synthetic/image.png".into()),
            },
        ]);
        lazy.messages[1].content.push(Block::Thinking {
            text: "".into(),
            signature: Some("empty-signature".into()),
            encrypted: None,
            redacted: false,
        });
        let mut owned = lazy.clone();
        owned.materialize()?;
        for mode in [
            crate::emit::ThinkingMode::Native,
            crate::emit::ThinkingMode::Text,
            crate::emit::ThinkingMode::Drop,
        ] {
            let opts = crate::emit::EmitOptions {
                new_id: Some("fixed".into()),
                thinking: mode,
                ..Default::default()
            };
            let mut oracle = owned.clone();
            crate::emit::project_owned_thinking_for_test(&mut oracle, Harness::Continue, mode);
            let old = legacy_emit(&oracle, &world.base.join(format!("old-{mode:?}")), &opts)?;
            let new = super::super::emit(&lazy, &world.base.join(format!("new-{mode:?}")), &opts)?;
            let old = fs::read(old.path)?;
            let new = fs::read(new.path)?;
            assert!(
                old == new,
                "wire mismatch {mode:?} at {:?}",
                old.iter().zip(&new).position(|(a, b)| a != b)
            );
        }
        let dest = world.base.join("mutation");
        fs::create_dir_all(&dest)?;
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let first = super::super::emit(&lazy, &dest, &opts)?;
        let before = fs::read(&first.path)?;
        fs::write(&path, b"changed")?;
        assert!(super::super::emit(&lazy, &dest, &opts).is_err());
        assert_eq!(fs::read(first.path)?, before);
        Ok(())
    }
    #[test]
    fn late_argument_source_failure_preserves_published_files() -> Result<()> {
        let world = cv_testkit::World::in_process("late-argument-Continue");
        let source = world.base.join("arguments.json");
        fs::write(&source, br#"{"path":"old"}"#)?;
        let input = crate::Json::source(&source, 0, fs::metadata(&source)?.len())?;
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        session.created_at =
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "inline prompt".into(),
        });
        session.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Read".into(),
            input,
            namespace: None,
        });
        session.messages.push(assistant);
        let dest = world.base.join("output");
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            ..Default::default()
        };
        let result = super::super::emit(&session, &dest, &opts)?;
        let transcript = fs::read(&result.path)?;
        let index = dest.join("sessions.json");
        let index_before = fs::read(&index).ok();
        fs::write(&source, br#"{"path":"new"}"#)?;
        assert!(super::super::emit(&session, &dest, &opts).is_err());
        assert_eq!(fs::read(result.path)?, transcript);
        assert_eq!(fs::read(index).ok(), index_before);
        Ok(())
    }
}
