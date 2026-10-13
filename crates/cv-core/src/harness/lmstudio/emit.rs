use super::*;
use crate::{
    emit::{
        thinking::{fate, text_with_fate},
        ThinkingMode,
    },
    harness::{
        store::{bytes::ByteStage, publish_stage, text::generate_string},
        wire::write::{append_text, text, ObjectWriter},
    },
    ir_json::PrettyJson,
    lazy::Resolver,
};
use std::borrow::Cow;
use std::io::{BufWriter, Write};

fn projected<'a>(block: &'a Block, mode: ThinkingMode, resolver: &Resolver) -> Result<Option<(Cow<'a, Block>, bool)>> {
    if let Block::Thinking { text, .. } = block {
        let fate = fate(block, Harness::LmStudio, mode, resolver)?;
        if let Some(value) = text_with_fate(block, fate) {
            return Ok(Some((
                match value {
                    Cow::Borrowed(_) => Cow::Borrowed(block),
                    Cow::Owned(text) => Cow::Owned(Block::Text { text }),
                },
                false,
            )));
        }
        if fate == crate::emit::ThinkingFate::Drop {
            return Ok(None);
        }
        if crate::emit::thinking::first_character(text, resolver, false)?.is_none() {
            return Ok(None);
        }
        return Ok(Some((Cow::Borrowed(block), true)));
    }
    Ok(Some((Cow::Borrowed(block), false)))
}
fn part(w: &mut dyn Write, block: &Block, resolver: &Resolver) -> Result<()> {
    match block {
        Block::Text { text: value } | Block::Thinking { text: value, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| text(w, value, resolver))?;
            o.finish()?;
        }
        Block::ToolUse { name, input, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| {
                generate_string(w, |b| {
                    b.push("[tool call: ")?;
                    b.push(name)?;
                    b.push(" ")?;
                    input.with_view(|v| b.push_json(v))?;
                    b.push("]")
                })
            })?;
            o.finish()?;
        }
        Block::ToolResult { content, .. } => {
            let mut o = ObjectWriter::new(w)?;
            o.field("type", "text")?;
            o.field_with("text", |w| {
                generate_string(w, |b| {
                    b.push("[tool result: ")?;
                    append_text(b, content, resolver)?;
                    b.push("]")
                })
            })?;
            o.finish()?;
        }
        _ => {
            if let Some(part) = emit_file_part(block) {
                serde_json::to_writer(&mut *w, &part)?;
            }
        }
    }
    Ok(())
}
fn parts(w: &mut dyn Write, block: &Block, resolver: &Resolver) -> Result<()> {
    w.write_all(b"[")?;
    part(w, block, resolver)?;
    w.write_all(b"]")?;
    Ok(())
}
pub(super) fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<EmitResult> {
    super::super::wire::write::check_source_fields(session)?;
    let resolver = Resolver::new(session.source_path.clone());
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| created.timestamp_millis().to_string());
    let file_path = out_dir
        .join("conversations")
        .join(format!("{new_id}.conversation.json"));
    let title = super::super::wire::write::fallback_title(session, Harness::LmStudio, opts.thinking, &resolver)?
        .unwrap_or_else(|| "Untitled".into());
    let mut stage = BufWriter::with_capacity(64 * 1024, ByteStage::new());
    {
        let mut pretty = PrettyJson::new(&mut stage, 0);
        let mut root = ObjectWriter::new(&mut pretty)?;
        root.field("name", title)?;
        root.field("createdAt", created.timestamp_millis())?;
        root.field_with("messages", |w| {
            w.write_all(b"[")?;
            let mut first = true;
            for msg in &session.messages {
                let blocks = msg
                    .content
                    .iter()
                    .filter_map(|b| match projected(b, opts.thinking, &resolver) {
                        Ok(Some(b)) => Some(Ok(b)),
                        Ok(None) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<Vec<_>>>()?;
                let blocks = blocks
                    .iter()
                    .filter(|(b, _)| {
                        !matches!(b.as_ref(), Block::File { .. } | Block::Image { .. }) || emit_file_part(b).is_some()
                    })
                    .collect::<Vec<_>>();
                if blocks.is_empty() {
                    continue;
                }
                if !first {
                    w.write_all(b",")?;
                }
                first = false;
                let mut turn = ObjectWriter::new(w)?;
                turn.field_with("versions", |w| {
                    w.write_all(b"[")?;
                    let mut version = ObjectWriter::new(w)?;
                    version.field(
                        "type",
                        if msg.role == Role::Assistant {
                            "multiStep"
                        } else {
                            "singleStep"
                        },
                    )?;
                    version.field(
                        "role",
                        if msg.role == Role::Assistant {
                            "assistant"
                        } else if msg.role == Role::System {
                            "system"
                        } else {
                            "user"
                        },
                    )?;
                    if msg.role == Role::Assistant {
                        version.field_with("steps", |w| {
                            w.write_all(b"[")?;
                            let ts = msg.timestamp.unwrap_or(created);
                            let step_id = format!("{}-0.0000000000000000", ts.timestamp_millis());
                            let mut attached = false;
                            for (i, (b, thinking)) in blocks.iter().enumerate() {
                                if i != 0 {
                                    w.write_all(b",")?;
                                }
                                let mut step = ObjectWriter::new(w)?;
                                step.field("type", "contentBlock")?;
                                step.field("role", "assistant")?;
                                step.field_with("content", |w| parts(w, b, &resolver))?;
                                step.field("stepIdentifier", &step_id)?;
                                if *thinking {
                                    step.field("style", json!({"type":"thinking"}))?;
                                }
                                if !attached {
                                    let mut gen = Map::new();
                                    if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                                        gen.insert("indexedModelIdentifier".into(), json!(model));
                                    }
                                    if let Some(u) = &msg.usage {
                                        let mut stats = Map::new();
                                        if let Some(n) = u.input_tokens {
                                            stats.insert("promptTokensCount".into(), json!(n));
                                        }
                                        if let Some(n) = u.output_tokens {
                                            stats.insert("predictedTokensCount".into(), json!(n));
                                        }
                                        if !stats.is_empty() {
                                            gen.insert("stats".into(), Value::Object(stats));
                                        }
                                    }
                                    if !gen.is_empty() {
                                        step.field("genInfo", gen)?;
                                        attached = true;
                                    }
                                }
                                step.finish()?;
                            }
                            w.write_all(b"]")?;
                            Ok(())
                        })?;
                    } else {
                        version.field_with("content", |w| {
                            w.write_all(b"[")?;
                            for (i, (b, _)) in blocks.iter().enumerate() {
                                if i != 0 {
                                    w.write_all(b",")?;
                                } // one part per represented block
                                  // Strip the single-part array by directly writing its content to a wrapper.
                                part(w, b, &resolver)?;
                            }
                            w.write_all(b"]")?;
                            Ok(())
                        })?;
                    }
                    version.finish()?;
                    w.write_all(b"]")?;
                    Ok(())
                })?;
                turn.field("currentlySelected", 0)?;
                turn.finish()?;
            }
            w.write_all(b"]")?;
            Ok(())
        })?;
        root.field("tokenCount", 0)?;
        root.field("pinned", false)?;
        if let Some(sp) = &session.system_prompt {
            root.field("systemPrompt", sp)?;
        }
        root.finish()?;
    }
    let stage = stage.into_inner().map_err(|e| e.into_error())?;
    fs::create_dir_all(file_path.parent().unwrap())?;
    publish_stage(stage, &file_path)?;
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some("open the chat in LM Studio".into()),
    })
}
#[cfg(test)]
fn legacy_emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_ms = created.timestamp_millis();

    // The id (filename stem) is the ms-epoch createdAt unless explicitly overridden.
    let new_id = opts.new_id.clone().unwrap_or_else(|| created_ms.to_string());

    let conv_dir = out_dir.join("conversations");
    fs::create_dir_all(&conv_dir).with_context(|| format!("creating {}", conv_dir.display()))?;
    let file_path = conv_dir.join(format!("{new_id}.conversation.json"));

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        if let Some(version) = emit_version(session, msg, created)? {
            messages.push(json!({
                "versions": [version],
                "currentlySelected": 0,
            }));
        }
    }

    let mut root = Map::new();
    let title = session
        .title
        .clone()
        .or_else(|| session.first_user_text())
        .map(|t| crate::ir::truncate(&t, 80))
        .unwrap_or_else(|| "Untitled".to_string());
    root.insert("name".into(), json!(title));
    root.insert("createdAt".into(), json!(created_ms));
    root.insert("messages".into(), Value::Array(messages));
    root.insert("tokenCount".into(), json!(0));
    root.insert("pinned".into(), json!(false));

    // Preserve any system prompt the source carried (parse puts it on `Session::system_prompt`).
    if let Some(sp) = &session.system_prompt {
        root.insert("systemPrompt".into(), json!(sp));
    }

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(root))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some("open the chat in LM Studio".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_native_writer_matches_owned_oracle_and_preserves_changed_source() -> Result<()> {
        let world = cv_testkit::World::in_process("checked-lmstudio");
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
        let mut lazy = crate::emit::parse_for_emit(&adapter, &reference, Harness::LmStudio)?;
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
        for message in &mut lazy.messages[..2] {
            message.content.push(Block::Thinking {
                text: " \n\t ".into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
        }
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
            crate::emit::project_owned_thinking_for_test(&mut oracle, Harness::LmStudio, mode);
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
        let world = cv_testkit::World::in_process("late-argument-LmStudio");
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
    #[test]
    fn dropped_source_failure_matches_direct_owned_error_contract() -> Result<()> {
        let world = cv_testkit::World::in_process("dropped-source-LmStudio");
        let path = world.base.join("thinking.json");
        fs::write(&path, serde_json::to_vec(&"private thought".repeat(3000))?)?;
        let text = crate::Json::source(&path, 0, fs::metadata(&path)?.len())?
            .with_view(|v| v.source_text()?.ok_or_else(|| anyhow::anyhow!("text source")))?;
        assert!(matches!(text, crate::Text::Source(_)));
        let mut session = Session::new("synthetic", Harness::Claude);
        session.title = Some("explicit".into());
        session.created_at =
            Some(chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")?.with_timezone(&chrono::Utc));
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::Thinking {
            text,
            signature: None,
            encrypted: None,
            redacted: false,
        });
        session.messages.push(message);
        let opts = crate::emit::EmitOptions {
            new_id: Some("fixed".into()),
            thinking: crate::emit::ThinkingMode::Drop,
            ..Default::default()
        };
        let dest = world.base.join("new");
        let emitted = super::super::emit(&session, &dest, &opts)?;
        let before = fs::read(&emitted.path)?;
        let index = dest.join("sessions.json");
        let index_before = fs::read(&index).ok();
        fs::remove_file(&path)?;
        assert!(legacy_emit(&session, &world.base.join("old"), &opts).is_err());
        assert!(super::super::emit(&session, &dest, &opts).is_err());
        assert_eq!(fs::read(&emitted.path)?, before);
        assert_eq!(fs::read(&index).ok(), index_before);
        // Omitted legacy raw Spans retain best-effort materialization acceptance.
        // Demanded raw Spans still use the checked output path.
        fs::write(&path, b"short")?;
        if let Block::Thinking { text, .. } = &mut session.messages[0].content[0] {
            *text = crate::Text::Span(crate::lazy::Span {
                source: Some(path.clone()),
                offset: 0,
                len: 99,
                escaped: true,
            });
        }
        let mut owned = session.clone();
        owned.materialize()?;
        crate::emit::project_owned_thinking_for_test(&mut owned, Harness::LmStudio, opts.thinking);
        let oracle = legacy_emit(&owned, &world.base.join("span-oracle"), &opts)?;
        let new = super::super::emit(&session, &dest, &opts)?;
        assert_eq!(fs::read(oracle.path)?, fs::read(new.path)?);
        assert_eq!(fs::read(&emitted.path)?, before);
        assert_eq!(fs::read(index).ok(), index_before);
        Ok(())
    }
}
