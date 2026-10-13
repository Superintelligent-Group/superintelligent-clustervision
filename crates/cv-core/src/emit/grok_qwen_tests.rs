//! Independent pre-checked wire and fidelity oracles; only generated Qwen UUIDs normalize.
use super::*;
use crate::harness::Adapter;
use crate::ir::{GitInfo, Message, Origin, Usage};
use crate::stream::ParseOptions;
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture(harness: Harness) -> Session {
    let mut session = Session::new("source", harness);
    let timestamp = "2026-10-12T12:00:00Z".parse().unwrap();
    session.created_at = Some(timestamp);
    session.updated_at = Some(timestamp + chrono::Duration::seconds(2));
    session.cwd = Some("/synthetic/grok qwen λ".into());
    session.title = Some("title \"λ\"".into());
    session.model = Some("foreign-model".into());
    session.git = Some(GitInfo {
        branch: Some("branch".into()),
        commit: Some("commit".into()),
        remote: Some("remote".into()),
    });
    session.lineage.forked_from = Some("parent-session".into());
    session
        .harness_extra_mut(Harness::Qwen)
        .insert("version".into(), json!("0.25.1"));
    let text = |value: &str| Block::Text { text: value.into() };
    let mut system = Message::new(Role::System);
    system.kind = crate::MessageKind::SystemPrompt;
    system.content.push(text("system"));
    let mut user = Message::new(Role::User);
    user.content = vec![
        text(""),
        text("prompt\nλ"),
        text(""),
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("file:image".into()),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("path".into()),
            source: Some("https://source".into()),
        },
    ];
    let mut assistant = Message::new(Role::Assistant);
    assistant.model = Some("grok-message-model".into());
    assistant.usage = Some(Usage {
        input_tokens: Some(10),
        output_tokens: Some(3),
        cache_read_tokens: Some(4),
        reasoning_tokens: Some(2),
        ..Default::default()
    });
    assistant.content = vec![
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("provider-bound".into()),
            redacted: true,
        },
        Block::Thinking {
            text: "reasoning λ\n".into(),
            signature: Some("sig".into()),
            encrypted: Some("second-blob".into()),
            redacted: false,
        },
        Block::Thinking {
            text: " ".into(),
            signature: Some("blank-signature".into()),
            encrypted: None,
            redacted: false,
        },
        text("answer \"\\\t"),
        text(""),
        Block::ToolUse {
            id: "linked".into(),
            name: "Read".into(),
            namespace: Some("tool_call".into()),
            input: json!({"β":[1,true,null,{"float":-0.0}]}).into(),
        },
        Block::ToolUse {
            id: "second".into(),
            name: "Run".into(),
            namespace: None,
            input: json!({"command":"printf λ"}).into(),
        },
        Block::Image {
            media_type: None,
            data_ref: None,
        },
        Block::File {
            mime: None,
            path: Some("fallback-path".into()),
            source: None,
        },
    ];
    let mut tool = Message::new(Role::Tool);
    tool.content = vec![
        text("not emitted"),
        Block::ToolResult {
            tool_use_id: "linked".into(),
            content: "failed λ\n".into(),
            is_error: true,
            tool_name: Some("Read".into()),
            status: Some("failed".into()),
            details: None,
        },
        Block::ToolResult {
            tool_use_id: "second".into(),
            content: "success".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        },
    ];
    let mut boundary = Message::new(Role::System);
    boundary.kind = crate::MessageKind::CompactionBoundary;
    boundary.harness_extra_mut(Harness::Qwen).extend([
        ("subtype".into(), json!("old-compression")),
        ("tokens".into(), json!(17)),
        ("detail".into(), json!({"x":["y",true]})),
    ]);
    let mut summary = Message::new(Role::User);
    summary.kind = crate::MessageKind::CompactionSummary;
    summary.content.push(text("summary\n"));
    let mut change = Message::new(Role::System);
    change.kind = crate::MessageKind::ModelChange;
    change.model = Some("new-model".into());
    let mut error = Message::new(Role::System);
    error.kind = crate::MessageKind::Error;
    error.content = vec![text(""), text("failure"), text("")];
    let mut notice = Message::new(Role::System);
    notice.content = vec![text(""), text("notice"), text("")];
    let mut branch = Message::new(Role::System);
    branch.kind = crate::MessageKind::Branch;
    let mut injected = Message::new(Role::User);
    injected.kind = crate::MessageKind::InjectedContext;
    injected.origin = Origin::Scheduler;
    injected
        .harness_extra_mut(Harness::Qwen)
        .insert("subtype".into(), json!("original-subtype"));
    injected.content.push(text("injected"));
    let mut empty = Message::new(Role::Assistant);
    empty.content.push(text(""));
    session.messages = vec![
        system,
        user,
        assistant,
        tool,
        boundary,
        summary,
        change,
        error,
        notice,
        branch,
        injected,
        empty,
        Message::new(Role::System),
    ];
    for message in &mut session.messages {
        message.timestamp = Some(timestamp);
    }
    session
}

fn normalized_qwen(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes == b"\n" {
        return Ok(bytes.to_vec());
    }
    anyhow::ensure!(bytes.ends_with(b"\n"), "JSONL terminator");
    let mut output = Vec::new();
    let mut prior = None;
    let mut prior_normalized = None;
    for (index, line) in std::str::from_utf8(bytes)?.lines().enumerate() {
        let mut row: Value = serde_json::from_str(line)?;
        assert_eq!(
            serde_json::to_string(&row)?,
            line,
            "compact wire/order before normalization"
        );
        let uuid = row["uuid"].as_str().unwrap().to_owned();
        assert_eq!(Uuid::parse_str(&uuid)?.get_version(), Some(uuid::Version::Random));
        assert_eq!(row["parentUuid"], json!(prior));
        row["uuid"] = json!(format!("generated-{index}"));
        row["parentUuid"] = json!(prior_normalized);
        prior = Some(uuid);
        prior_normalized = Some(format!("generated-{index}"));
        output.extend(serde_json::to_vec(&row)?);
        output.push(b'\n');
    }
    Ok(output)
}

fn compare(session: &Session, target: Harness, mode: ThinkingMode, base: &Path) -> Result<()> {
    let opts = EmitOptions {
        new_id: Some("fixed-session".into()),
        thinking: mode,
        ..Default::default()
    };
    let mut owned = session.clone();
    owned.materialize()?;
    let mut projected = owned.clone();
    project_owned_thinking_for_test(&mut projected, target, mode);
    let old = match target {
        Harness::Grok => crate::harness::grok::emit::legacy::emit(&projected, &base.join("old"), &opts)?,
        Harness::Qwen => crate::harness::qwen::emit::legacy::emit(&projected, &base.join("old"), &opts)?,
        _ => unreachable!(),
    };
    let (new, report) = emit_report(session, target, &base.join("new"), &opts)?;
    if target == Harness::Qwen {
        assert_eq!(
            normalized_qwen(&fs::read(&old.path)?)?,
            normalized_qwen(&fs::read(&new.path)?)?
        );
    } else {
        for name in ["summary.json", "chat_history.jsonl", "updates.jsonl"] {
            let old_path = old.path.join(name);
            let new_path = new.path.join(name);
            assert_eq!(old_path.exists(), new_path.exists(), "{name}");
            if old_path.exists() {
                assert_eq!(fs::read(old_path)?, fs::read(new_path)?, "{name}");
            }
        }
    }
    let old_read = fidelity::reparse_emitted(target, &old, &ParseOptions::full())?;
    let old_report = fidelity::diff_fidelity(target, mode, &owned, &old_read);
    assert_eq!(
        serde_json::to_value(old_report)?,
        serde_json::to_value(report)?,
        "exact fidelity classification"
    );
    Ok(())
}

fn capture_bodies(session: &mut Session, base: &Path) -> Result<()> {
    fs::create_dir_all(base)?;
    let mut index = 0;
    for message in &mut session.messages {
        for block in &mut message.content {
            index += 1;
            let path = base.join(format!("{index}.json"));
            match block {
                Block::Text { text } | Block::Thinking { text, .. } | Block::ToolResult { content: text, .. } => {
                    let raw = serde_json::to_vec(&text.inline_str().unwrap())?;
                    fs::write(&path, &raw)?;
                    // For tiny/empty leaves also force a captured Source while retaining strict acceptance.
                    let json = crate::Json::source(&path, 0, raw.len() as u64)?;
                    if let crate::Json::Source(source) = json {
                        *text = crate::lazy::Text::Source(crate::lazy::SourceText::new(source));
                    }
                }
                Block::ToolUse { input, .. } => {
                    let raw = serde_json::to_vec(input.resolve()?.as_ref())?;
                    fs::write(&path, &raw)?;
                    *input = crate::Json::source(&path, 0, raw.len() as u64)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[test]
fn checked_grok_qwen_frozen_all_modes_owned_and_captured() -> Result<()> {
    let world = cv_testkit::World::in_process("checked-grok-qwen-wire");
    for (target, source) in [
        (Harness::Grok, Harness::Grok),
        (Harness::Grok, Harness::Claude),
        (Harness::Qwen, Harness::Qwen),
        (Harness::Qwen, Harness::Claude),
    ] {
        let owned = fixture(source);
        let mut captured = owned.clone();
        capture_bodies(&mut captured, &world.base.join(format!("sources-{target}-{source}")))?;
        for mode in ThinkingMode::ALL {
            compare(
                &owned,
                target,
                mode,
                &world.base.join(format!("owned-{target}-{source}-{mode}")),
            )?;
            compare(
                &captured,
                target,
                mode,
                &world.base.join(format!("captured-{target}-{source}-{mode}")),
            )?;
        }
    }
    Ok(())
}

#[test]
fn checked_grok_qwen_real_corpus_matches_frozen_all_modes() -> Result<()> {
    let world = cv_testkit::World::in_process("checked-grok-qwen-corpus");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for (target, version, adapter) in [
        (
            Harness::Grok,
            "1.0.50",
            Box::new(crate::harness::grok::Grok::with_home(&root.join("grok/1.0.50/.grok"))) as Box<dyn Adapter>,
        ),
        (
            Harness::Qwen,
            "0.25.1",
            Box::new(crate::harness::qwen::Qwen::with_home(&root.join("qwen/0.25.1/.qwen"))) as Box<dyn Adapter>,
        ),
    ] {
        let refs = adapter.discover()?;
        assert!(!refs.is_empty(), "{target}/{version}");
        for reference in refs {
            let session = adapter.parse(&reference)?;
            for mode in ThinkingMode::ALL {
                compare(
                    &session,
                    target,
                    mode,
                    &world.base.join(format!("{target}-{}-{mode}", reference.id)),
                )?;
            }
        }
    }
    Ok(())
}

#[test]
fn checked_grok_qwen_source_failure_preserves_every_destination() -> Result<()> {
    let world = cv_testkit::World::in_process("checked-grok-qwen-failure");
    for target in [Harness::Grok, Harness::Qwen] {
        for mode in ThinkingMode::ALL {
            for variant in ["text", "thinking", "argument", "omitted"] {
                let base = world.base.join(format!("{target}-{mode}-{variant}"));
                fs::create_dir_all(&base)?;
                let out = base.join("out");
                let mut session = fixture(Harness::Claude);
                let opts = EmitOptions {
                    new_id: Some("fixed".into()),
                    thinking: mode,
                    ..Default::default()
                };
                let existing = emit(&session, target, &out, &opts)?;
                let files = if target == Harness::Grok {
                    vec![
                        existing.path.join("summary.json"),
                        existing.path.join("chat_history.jsonl"),
                        existing.path.join("updates.jsonl"),
                    ]
                } else {
                    vec![existing.path]
                };
                for file in &files {
                    fs::write(file, b"existing destination")?;
                }
                let path = base.join("source.json");
                let raw = if variant == "argument" {
                    serde_json::to_vec(&json!({"body":"guarded λ\n".repeat(10000)}))?
                } else {
                    serde_json::to_vec(&"guarded λ\n".repeat(10000))?
                };
                fs::write(&path, &raw)?;
                let json = crate::Json::source(&path, 0, raw.len() as u64)?;
                let mut message = Message::new(if variant == "omitted" {
                    Role::System
                } else {
                    Role::Assistant
                });
                message.content.push(if variant == "argument" {
                    Block::ToolUse {
                        id: "guarded".into(),
                        name: "Read".into(),
                        namespace: None,
                        input: json,
                    }
                } else {
                    let text = json.with_view(|view| Ok(view.source_text()?.unwrap()))?;
                    if variant == "thinking" {
                        Block::Thinking {
                            text,
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        }
                    } else {
                        Block::Text { text }
                    }
                });
                if variant == "omitted" {
                    message.kind = crate::MessageKind::SystemPrompt;
                }
                session.messages.push(message);
                fs::write(&path, b"changed source")?;
                assert!(
                    emit(&session, target, &out, &opts).is_err(),
                    "{target}/{mode}/{variant}"
                );
                for file in files {
                    assert_eq!(fs::read(file)?, b"existing destination");
                }
                let missing = base.join("missing-output");
                assert!(emit(&session, target, &missing, &opts).is_err());
                assert!(!missing.exists());
            }
        }
    }
    Ok(())
}
