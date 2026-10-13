use super::*;
use crate::{harness::wire::anthropic, Json, Message, Origin};
use std::collections::HashMap;

fn normalized(path: &Path) -> Result<Vec<u8>> {
    let mut ids = HashMap::new();
    let mut output = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        let mut value: Value = serde_json::from_str(line)?;
        if value["type"] != "session" {
            let id = value["id"].as_str().unwrap().to_owned();
            assert_eq!(id.len(), 8);
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(!ids.contains_key(&id));
            let replacement = format!("{:08x}", ids.len() + 1);
            ids.insert(id, replacement.clone());
            *value.get_mut("id").unwrap() = json!(replacement);
            if let Some(parent) = value["parentId"].as_str() {
                let replacement = ids.get(parent).unwrap().clone();
                *value.get_mut("parentId").unwrap() = json!(replacement);
            }
        }
        serde_json::to_writer(&mut output, &value)?;
        output.push(b'\n');
    }
    Ok(output)
}

fn source_text(path: &Path, body: &str) -> Result<crate::Text> {
    let bytes = serde_json::to_vec(body)?;
    fs::write(path, &bytes)?;
    Json::source(path, 0, bytes.len() as u64)?.with_view(|v| Ok(v.source_text()?.unwrap()))
}

fn fixture(base: &Path) -> Result<Session> {
    let mut session = Session::new("source", Harness::Claude);
    let ts = "2026-10-12T12:00:00Z".parse()?;
    session.created_at = Some(ts);
    session.updated_at = Some(ts + chrono::Duration::seconds(5));
    session.title = Some("title \"λ\"".into());
    session.cwd = Some("/synthetic/openclaw".into());
    session.model = Some("native-model".into());
    let input = json!({"z":"guarded λ\n".repeat(1000),"a":[1,true,null]});
    let bytes = serde_json::to_vec(&input)?;
    let path = base.join("input.json");
    fs::write(&path, &bytes)?;
    let input = Json::source(&path, 0, bytes.len() as u64)?;
    let mut user = Message::new(Role::User);
    user.content = vec![
        Block::Text {
            text: source_text(&base.join("prompt.json"), &"prompt λ\n".repeat(1000))?,
        },
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:image".into()),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("report λ.txt".into()),
            source: None,
        },
    ];
    let mut assistant = Message::new(Role::Assistant);
    assistant.usage = Some(crate::Usage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        cache_read_tokens: Some(2),
        cache_creation_tokens: Some(1),
        cost_usd: Some(0.1),
        ..Default::default()
    });
    assistant.content = vec![
        Block::Thinking {
            text: source_text(&base.join("thinking.json"), &"reasoning λ\n".repeat(1000))?,
            signature: Some("sig".into()),
            encrypted: None,
            redacted: false,
        },
        Block::Thinking {
            text: " ".into(),
            signature: Some("signature-only".into()),
            encrypted: None,
            redacted: false,
        },
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("opaque".into()),
            redacted: true,
        },
        Block::Text {
            text: "answer \"\\\t".into(),
        },
        Block::ToolUse {
            id: "deferred".into(),
            name: "inner".into(),
            namespace: Some(crate::affordance::DEFERRED.into()),
            input: input.clone(),
        },
        Block::ToolUse {
            id: "ordinary".into(),
            name: "ordinary".into(),
            namespace: None,
            input,
        },
        Block::Image {
            media_type: None,
            data_ref: None,
        },
        Block::ToolResult {
            tool_use_id: "ignored".into(),
            content: "assistant result is omitted".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        },
    ];
    let mut tool = Message::new(Role::Tool);
    tool.content = vec![
        Block::ToolResult {
            tool_use_id: "deferred".into(),
            content: source_text(&base.join("result.json"), &"result λ\n".repeat(1000))?,
            is_error: false,
            tool_name: Some("inner".into()),
            status: Some("completed".into()),
            details: None,
        },
        Block::ToolResult {
            tool_use_id: "ignored-second".into(),
            content: "second result omitted".into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        },
    ];
    let mut summary = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
    summary.content = vec![
        Block::Text { text: "".into() },
        Block::Text {
            text: "summary\nλ".into(),
        },
        Block::Thinking {
            text: "summary reasoning".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        },
    ];
    let mut branch = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
    branch.content.push(Block::Text {
        text: "unused branch body".into(),
    });
    let mut model = Message::of_kind(Role::System, MessageKind::ModelChange, Origin::Harness);
    model.model = Some("changed-model".into());
    let mut empty = Message::new(Role::Assistant);
    empty.content.push(Block::Thinking {
        text: "".into(),
        signature: None,
        encrypted: None,
        redacted: false,
    });
    session.messages = vec![
        user,
        assistant,
        tool,
        summary,
        branch,
        model,
        empty,
        Message::new(Role::System),
        Message::new(Role::Tool),
    ];
    Ok(session)
}

#[test]
fn checked_openclaw_matches_frozen_records_index_and_all_modes() -> Result<()> {
    let world = cv_testkit::World::in_process("openclaw-checked-wire");
    let session = fixture(&world.base)?;
    for mode in ThinkingMode::ALL {
        let options = EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let before_root = world.base.join(format!("old-{mode}/agents"));
        let after_root = world.base.join(format!("new-{mode}/agents"));
        for root in [&before_root, &after_root] {
            fs::create_dir_all(root.join("main/sessions"))?;
            fs::write(
                root.join("main/sessions/sessions.json"),
                serde_json::to_vec(&json!({"untouched":{"label":"keep"},"fixed":{"old":"replace"}}))?,
            )?;
        }
        let mut owned = session.clone();
        owned.materialize()?;
        crate::emit::project_owned_thinking_for_test(&mut owned, Harness::OpenClaw, mode);
        let before = legacy::emit(&owned, &before_root, &options)?;
        let after = crate::emit::emit(&session, Harness::OpenClaw, &after_root, &options)?;
        assert_eq!(normalized(&before.path)?, normalized(&after.path)?, "mode={mode}");
        assert_eq!(
            fs::read(before.path.parent().unwrap().join("sessions.json"))?,
            fs::read(after.path.parent().unwrap().join("sessions.json"))?
        );
        assert_eq!(before.new_id, after.new_id);
        assert_eq!(before.resume_hint, after.resume_hint);
    }
    Ok(())
}

#[test]
fn checked_openclaw_borrowed_wrapper_matches_replay_definition_and_none() -> Result<()> {
    let world = cv_testkit::World::in_process("openclaw-checked-deferred");
    let session = fixture(&world.base)?;
    let Block::ToolUse { input, .. } = &session.messages[1].content[4] else {
        unreachable!()
    };
    for namespace in [None, Some("other"), Some(crate::affordance::DEFERRED)] {
        let block = Block::ToolUse {
            id: "call".into(),
            name: "inner".into(),
            input: input.clone(),
            namespace: namespace.map(str::to_owned),
        };
        let rewrapped = crate::affordance::rewrap_deferred(Harness::OpenClaw, &block)?;
        assert_eq!(rewrapped.is_some(), namespace == Some(crate::affordance::DEFERRED));
        let expected = anthropic::assistant_blocks(&[rewrapped.unwrap_or(block.clone())], &anthropic::OPENCLAW)?;
        let mut bytes = Vec::new();
        stream::content(&mut bytes, &[block], ThinkingMode::Native, &session.resolver())?;
        assert_eq!(bytes, serde_json::to_vec(&expected)?);
    }
    Ok(())
}

#[test]
fn checked_openclaw_title_policy_matches_explicit_and_first_user_legacy() -> Result<()> {
    let world = cv_testkit::World::in_process("openclaw-title-policy");
    for title in [None, Some(""), Some("   "), Some("explicit λ")] {
        for meaningful in [false, true] {
            let mut session = Session::new("source", Harness::Claude);
            session.created_at = Some("2026-10-12T12:00:00Z".parse()?);
            session.updated_at = session.created_at;
            session.title = title.map(str::to_owned);
            let mut first = Message::new(Role::User);
            first.content = vec![
                Block::Text { text: "".into() },
                Block::Text { text: " \n".into() },
                Block::Text {
                    text: if meaningful {
                        "λ\n".repeat(50).into()
                    } else {
                        "".into()
                    },
                },
            ];
            let mut second = Message::new(Role::User);
            second.content.push(Block::Text {
                text: "never a fallback".into(),
            });
            first.content.push(Block::Thinking {
                text: "user reasoning".into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
            session.messages = vec![first, second];
            for mode in ThinkingMode::ALL {
                let options = EmitOptions {
                    new_id: Some("fixed".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let suffix = format!("{}-{meaningful}-{mode}", title.unwrap_or("none"));
                let mut owned = session.clone();
                crate::emit::project_owned_thinking_for_test(&mut owned, Harness::OpenClaw, mode);
                let before = legacy::emit(&owned, &world.base.join(format!("old-{suffix}")), &options)?;
                let after = emit(&session, &world.base.join(format!("new-{suffix}")), &options)?;
                assert_eq!(
                    fs::read(before.path.parent().unwrap().join("sessions.json"))?,
                    fs::read(after.path.parent().unwrap().join("sessions.json"))?
                );
            }
        }
    }
    Ok(())
}

#[test]
fn checked_openclaw_source_failure_preserves_transcript_and_index_even_for_ignored_fields() -> Result<()> {
    let world = cv_testkit::World::in_process("openclaw-source-guard");
    for ignored in [false, true] {
        let mut session = fixture(&world.base)?;
        if ignored {
            session.messages[1].kind = MessageKind::Branch;
        }
        let out = world.base.join(format!("out-{ignored}"));
        let dir = out.join("agents/main/sessions");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("fixed.jsonl"), "old transcript")?;
        fs::write(dir.join("sessions.json"), "old index")?;
        fs::write(world.base.join("input.json"), "changed source")?;
        assert!(emit(
            &session,
            &out,
            &EmitOptions {
                new_id: Some("fixed".into()),
                thinking: ThinkingMode::Drop,
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(fs::read_to_string(dir.join("fixed.jsonl"))?, "old transcript");
        assert_eq!(fs::read_to_string(dir.join("sessions.json"))?, "old index");
    }
    Ok(())
}

#[test]
fn checked_openclaw_late_source_failure_cannot_publish_staged_records() -> Result<()> {
    use std::io::Write;
    let world = cv_testkit::World::in_process("openclaw-source-race");
    let session = fixture(&world.base)?;
    let source = world.base.join("input.json");
    let replacement = world.base.join("replacement.json");
    fs::copy(&source, &replacement)?;
    let destination = world.base.join("existing.jsonl");
    fs::write(&destination, "original transcript")?;
    struct Replace<'a> {
        writer: &'a mut dyn Write,
        source: &'a Path,
        replacement: &'a Path,
        done: bool,
    }
    impl Write for Replace<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.done && bytes.len() > 1000 {
                fs::rename(self.replacement, self.source)?;
                self.done = true;
            }
            self.writer.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut stage = JsonlStage::new();
    let error = stage
        .push_with(|writer| {
            stream::content(
                &mut Replace {
                    writer,
                    source: &source,
                    replacement: &replacement,
                    done: false,
                },
                &session.messages[1].content[4..5],
                ThinkingMode::Native,
                &session.resolver(),
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("source changed"), "{error}");
    assert!(stage.publish(&destination).is_err());
    assert_eq!(fs::read_to_string(&destination)?, "original transcript");
    Ok(())
}

#[test]
fn checked_openclaw_complete_preparation_preserves_ir_wire_report_and_late_failure() -> Result<()> {
    use crate::stream::ParseOptions;
    let world = cv_testkit::World::in_process("openclaw-complete-preparation");
    let source = world.base.join("source.jsonl");
    let rows = [
        json!({"type":"session","version":4,"cwd":"/synthetic","timestamp":"2026-10-12T12:00:00Z","parentSession":"parent"}),
        json!({"type":"session_info","id":"info","name":"explicit"}),
        json!({"type":"message","id":"u","message":{"role":"user","content":"λ\n".repeat(10000),"provenance":{"kind":"internal_system","sourceTool":"cron"}}}),
        json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant","model":"m","content":[{"type":"thinking","thinking":"reasoning".repeat(1000),"thinkingSignature":"sig"},{"type":"toolCall","id":"call","name":"run","arguments":{"command":"echo λ"}},{"type":"unknown","text":"ignored"}],"provider":"p","usage":{"input":3,"output":2,"cost":{"total":0.1}},"diagnostics":{"rich":true}}}),
        json!({"type":"message","id":"t","message":{"role":"toolResult","toolCallId":"call","content":[{"text":"first"},{"type":"other","text":"λ".repeat(10000)}],"details":{"structured":true}}}),
        json!({"type":"message","message":{"role":"system","content":"owned system prompt"}}),
        json!({"type":"message","message":{"role":"custom","content":"custom body","display":true}}),
        json!({"type":"future_record","id":"future","payload":{"unknown":[true,"keep all"]}}),
        json!({"type":"label","id":"label","targetId":"u","label":"first"}),
    ];
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, &row)?;
        bytes.push(b'\n');
    }
    fs::write(&source, &bytes)?;
    let reference = crate::SessionRef::new("complete", Harness::OpenClaw, &source);
    let adapter = crate::harness::for_harness(Harness::OpenClaw).unwrap();
    let owned = crate::stream::collect_with(adapter.as_ref(), &reference, &ParseOptions::complete())?;
    let checked = crate::emit::parse_for_emit(adapter.as_ref(), &reference, Harness::OpenClaw)?;
    assert!(checked.messages.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        Block::Text {
            text: crate::Text::Source(_)
        }
    ))));
    let mut materialized = checked.clone();
    materialized.materialize()?;
    assert_eq!(serde_json::to_value(&owned)?, serde_json::to_value(&materialized)?);
    assert!(owned.messages.iter().any(|m| m.kind == MessageKind::Carrier));
    for mode in ThinkingMode::ALL {
        let checked = crate::emit::parse_for_emit(adapter.as_ref(), &reference, Harness::OpenClaw)?;
        let opts = EmitOptions {
            new_id: Some("fixed".into()),
            thinking: mode,
            ..Default::default()
        };
        let (before, before_report) = crate::emit::emit_report(
            &owned,
            Harness::OpenClaw,
            &world.base.join(format!("owned-{mode}")),
            &opts,
        )?;
        let out = world.base.join(format!("checked-{mode}"));
        let (after, after_report) = crate::emit::emit_report(&checked, Harness::OpenClaw, &out, &opts)?;
        assert_eq!(normalized(&before.path)?, normalized(&after.path)?);
        assert_eq!(
            serde_json::to_value(before_report)?,
            serde_json::to_value(after_report)?
        );
        let index = after.path.parent().unwrap().join("sessions.json");
        assert_eq!(
            fs::read(before.path.parent().unwrap().join("sessions.json"))?,
            fs::read(&index)?
        );
        let transcript = fs::read(&after.path)?;
        let index_bytes = fs::read(&index)?;
        fs::write(&source, b"late replacement")?;
        assert!(crate::emit::emit_report(&checked, Harness::OpenClaw, &out, &opts).is_err());
        assert_eq!(fs::read(&after.path)?, transcript);
        assert_eq!(fs::read(&index)?, index_bytes);
        fs::write(&source, &bytes)?;
    }
    Ok(())
}
