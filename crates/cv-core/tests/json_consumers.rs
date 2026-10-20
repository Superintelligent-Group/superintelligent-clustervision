//! Source-backed and owned full-output consumers share one wire contract; source failures are errors.
use cv_core::{Block, Harness, Json, Message, Role, Session};
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture {
    dir: PathBuf,
    owned: Session,
    sourced: Session,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cv-json-consumers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let input = json!({"command": "printf 'é\\n'", "description": "nested <input>", "nested": [true, null, {"key": "quote\"\\"}]});
        let bytes = serde_json::to_vec(&input).unwrap();
        let path = dir.join("input.json");
        std::fs::write(&path, &bytes).unwrap();
        let mut owned = Session::new("fixture", Harness::Claude);
        owned.title = Some("fixture".into());
        let mut prompt = Message::new(Role::User);
        prompt.content.push(Block::Text {
            text: "Please run this command".into(),
        });
        let mut call = Message::new(Role::Assistant);
        call.content.push(Block::ToolUse {
            id: "call".into(),
            name: "Bash".into(),
            input: input.into(),
            namespace: None,
        });
        owned.messages.extend([prompt, call]);
        let mut sourced = owned.clone();
        let Block::ToolUse { input, .. } = &mut sourced.messages[1].content[0] else {
            unreachable!()
        };
        *input = Json::source(&path, 0, bytes.len() as u64).unwrap();
        Self { dir, owned, sourced }
    }
    fn invalidate(&self) {
        std::fs::remove_file(self.dir.join("input.json")).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn json_consumers_owned_and_source_full_outputs_match() {
    let fixture = Fixture::new();
    assert_eq!(
        serde_json::to_value(&fixture.owned).unwrap(),
        serde_json::to_value(&fixture.sourced).unwrap()
    );
    for format in [cv_core::dataset::Format::Chatml, cv_core::dataset::Format::ShareGpt] {
        for redact in [None, Some(cv_core::redact::RedactOptions::default())] {
            let mut owned = Vec::new();
            let mut source = Vec::new();
            cv_core::dataset::write_record(&fixture.owned, &mut owned, format, redact.as_ref()).unwrap();
            cv_core::dataset::write_record(&fixture.sourced, &mut source, format, redact.as_ref()).unwrap();
            assert_eq!(owned, source);
            let _: Value = serde_json::from_slice(&source).unwrap();
        }
    }
    let mut owned = String::new();
    let mut source = String::new();
    cv_core::html::share_message(&fixture.owned.messages[1], 1, &mut owned).unwrap();
    cv_core::html::share_message(&fixture.sourced.messages[1], 1, &mut source).unwrap();
    assert_eq!(owned, source);
    let redacted_owned = cv_core::redact::redact(&fixture.owned).unwrap();
    let redacted_source = cv_core::redact::redact(&fixture.sourced).unwrap();
    assert_eq!(
        serde_json::to_value(redacted_owned).unwrap(),
        serde_json::to_value(redacted_source).unwrap()
    );
    let options = cv_core::distill::DistillOptions::default();
    let owned = cv_core::distill::prepare_distill(&fixture.owned, &options)
        .unwrap()
        .distilled;
    let source = cv_core::distill::prepare_distill(&fixture.sourced, &options)
        .unwrap()
        .distilled;
    assert_eq!(owned.pack(), source.pack());
    assert_eq!(owned.stats.source_est_tokens, source.stats.source_est_tokens);
    assert_eq!(
        serde_json::to_value(cv_core::distill::resumable_session(&fixture.owned, &owned, &options).unwrap()).unwrap(),
        serde_json::to_value(cv_core::distill::resumable_session(&fixture.sourced, &source, &options).unwrap())
            .unwrap()
    );
}

#[test]
fn json_consumers_unreadable_sources_return_errors_before_emit_writes() {
    let fixture = Fixture::new();
    fixture.invalidate();
    let mut html = String::new();
    assert!(cv_core::html::share_message(&fixture.sourced.messages[1], 0, &mut html).is_err());
    assert!(cv_core::dataset::write_record(
        &fixture.sourced,
        &mut Vec::new(),
        cv_core::dataset::Format::Chatml,
        None
    )
    .is_err());
    assert!(cv_core::dataset::write_record(
        &fixture.sourced,
        &mut Vec::new(),
        cv_core::dataset::Format::Chatml,
        Some(&cv_core::redact::RedactOptions::default())
    )
    .is_err());
    assert!(cv_core::redact::redact(&fixture.sourced).is_err());
    assert!(cv_core::distill::prepare_distill(&fixture.sourced, &cv_core::distill::DistillOptions::default()).is_err());
    assert!(cv_core::distill::findings(&fixture.sourced, &cv_core::distill::DistillOptions::default()).is_err());
    for target in cv_core::emit::supported_targets() {
        let out = fixture.dir.join(target.as_str());
        assert!(cv_core::emit::emit(&fixture.sourced, *target, &out, &cv_core::emit::EmitOptions::default()).is_err());
        assert!(
            !out.exists(),
            "{target} created output before rejecting a missing source"
        );
    }
    // Direct public harness writers also preflight; generic dispatch cannot mask a missed guard.
    type Emitter =
        fn(&Session, &std::path::Path, &cv_core::emit::EmitOptions) -> anyhow::Result<cv_core::harness::EmitResult>;
    let direct: Vec<Emitter> = vec![
        cv_core::harness::claude::emit,
        cv_core::harness::codex::emit,
        cv_core::harness::grok::emit,
        cv_core::harness::opencode::emit,
        cv_core::harness::openclaw::emit,
        cv_core::harness::gemini::emit,
        cv_core::harness::lmstudio::emit,
        cv_core::harness::cline::emit,
        cv_core::harness::roo::emit,
        cv_core::harness::continuedev::emit,
        cv_core::harness::qwen::emit,
    ];
    for (index, emit) in direct.iter().enumerate() {
        let out = fixture.dir.join(format!("direct-{index}"));
        assert!(emit(&fixture.sourced, &out, &cv_core::emit::EmitOptions::default()).is_err());
        assert!(!out.exists());
    }
    #[cfg(feature = "sqlite")]
    {
        let out = fixture.dir.join("direct-hermes");
        assert!(
            cv_core::harness::hermes::emit(&fixture.sourced, &out, &cv_core::emit::EmitOptions::default()).is_err()
        );
        assert!(!out.exists());
    }
    assert!(cv_core::harness::wire::anthropic::assistant_blocks(
        &fixture.sourced.messages[1].content,
        &cv_core::harness::wire::anthropic::ANTHROPIC
    )
    .is_err());
}

#[test]
fn json_consumers_changed_source_is_an_error_in_full_and_mutable_paths() {
    let fixture = Fixture::new();
    std::fs::write(fixture.dir.join("input.json"), b"{}").unwrap();
    assert!(cv_core::redact::redact(&fixture.sourced).is_err());
    assert!(cv_core::html::share_message(&fixture.sourced.messages[1], 1, &mut String::new()).is_err());
    assert!(cv_core::dataset::write_record(
        &fixture.sourced,
        &mut Vec::new(),
        cv_core::dataset::Format::Chatml,
        None
    )
    .is_err());
    assert!(cv_core::distill::prepare_distill(&fixture.sourced, &cv_core::distill::DistillOptions::default()).is_err());
    assert!(serde_json::to_value(&fixture.sourced).is_err());
}

#[test]
fn json_consumers_resumable_selection_does_not_read_elided_input() {
    let fixture = Fixture::new();
    let options = cv_core::distill::DistillOptions::default();
    let mut distilled = cv_core::distill::prepare_distill(&fixture.owned, &options)
        .unwrap()
        .distilled;
    distilled.tail_start = fixture.owned.messages.len();
    fixture.invalidate();
    assert!(cv_core::distill::resumable_session(&fixture.sourced, &distilled, &options).is_ok());
    distilled.tail_start = 1;
    assert!(cv_core::distill::resumable_session(&fixture.sourced, &distilled, &options).is_err());
}
