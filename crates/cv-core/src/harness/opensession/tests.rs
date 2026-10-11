//! The OpenSession adapter: the document boundary, the byte-identical 0.3 round trip (synthetic
//! and on real data), the refusal of every other version, and discovery over registered sources.

use super::*;
use crate::harness::Parse;
use crate::harness::{claude, codex, Adapter};
use serde_json::{json, Map};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-opensession-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn block_type(b: &Block) -> &'static str {
    match b {
        Block::Text { .. } => "text",
        Block::Thinking { .. } => "thinking",
        Block::ToolUse { .. } => "tool_use",
        Block::ToolResult { .. } => "tool_result",
        Block::Image { .. } => "image",
        Block::File { .. } => "file",
    }
}

// ── the document boundary ─────────────────────────────────────────────────────

/// The version marker is the document's FIRST key — and it is *only* on the document. A `Session`
/// serialized on its own (what `cv show --json`, the MCP payloads and `cv serve` emit) has no
/// `open_session` key, which is the whole reason the marker lives at this boundary.
#[test]
fn document_leads_with_the_version_and_session_never_carries_it() {
    let s = Session {
        messages: vec![Message::new(Role::User)],
        ..Session::new("s1", Harness::Claude)
    };
    let doc = serde_json::to_string_pretty(&document(&s)).unwrap();
    assert!(
        doc.starts_with("{\n  \"open_session\": \"0.3\","),
        "the version marker leads the document:\n{doc}"
    );
    assert_eq!(VERSION, "0.3", "docs/OPENSESSION.md is at 0.3");
    let bare = serde_json::to_value(&s).unwrap();
    assert!(
        bare.get("open_session").is_none(),
        "a raw Session must NOT grow the marker: {bare}"
    );
}

// ── the decisive test: a round trip on real data ──────────────────────────────

/// The newest real local session with a workable size, from whichever harness has one.
fn a_real_session() -> Option<(&'static str, Session)> {
    let adapters: Vec<(&'static str, Box<dyn Adapter>)> = vec![
        ("claude", Box::new(claude::Claude::detect())),
        ("codex", Box::new(codex::Codex::detect())),
    ];
    for (name, a) in adapters {
        if a.roots().is_empty() {
            continue;
        }
        let Ok(mut refs) = a.discover() else { continue };
        refs.retain(|r| (5..=250).contains(&r.message_count));
        refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
        for r in refs.iter().take(4) {
            if let Ok(s) = a.parse(r) {
                if s.messages.len() >= 5 {
                    return Some((name, s));
                }
            }
        }
    }
    None
}

/// **Export a real session to OpenSession, read it back, and compare the two IRs field by field.**
///
/// Everything is asserted equal except one field, and it is not a loss: `source_path` is "where cv
/// read this session from on disk", so for the re-read session it is the document — naming the
/// original transcript there would be a claim about a file this reader never opened. Every other
/// field, including the harness the document came from, survives.
///
/// Skips (loudly) when the machine has no local session to test with; there is nothing to fake
/// here, a synthetic fixture would only prove that the writer and reader agree with each other.
#[test]
fn round_trips_a_real_session_through_a_document() {
    let Some((harness_name, original)) = a_real_session() else {
        eprintln!("no local claude/codex session to round-trip — skipping (nothing to fake here)");
        return;
    };
    eprintln!(
        "round trip: {harness_name} session {} — {} messages, source {}",
        original.id,
        original.messages.len(),
        original
            .source_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );

    let dir = tmpdir("roundtrip");
    let path = dir.join(format!("{}.opensession.json", original.id));
    std::fs::write(&path, serde_json::to_string_pretty(&document(&original)).unwrap()).unwrap();

    // The written bytes really are an OpenSession 0.3 document.
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["open_session"], "0.3");
    assert_eq!(raw["harness"], serde_json::to_value(original.harness).unwrap());

    // Read it back the way any other harness is read: through the adapter.
    let r = doc_ref(&path).expect("the document yields a ref");
    assert_eq!(r.harness, Harness::OpenSession, "the REF is tagged opensession");
    let back = OpenSession.parse(&r).expect("the document parses");

    // Session-level, field by field.
    assert_eq!(back.id, original.id, "id");
    assert_eq!(
        back.harness, original.harness,
        "harness (the document names its origin)"
    );
    assert_eq!(back.cwd, original.cwd, "cwd");
    assert_eq!(back.title, original.title, "title");
    assert_eq!(back.created_at, original.created_at, "created_at");
    assert_eq!(back.updated_at, original.updated_at, "updated_at");
    assert_eq!(back.model, original.model, "model");
    assert_eq!(
        serde_json::to_value(&back.git).unwrap(),
        serde_json::to_value(&original.git).unwrap(),
        "git"
    );
    assert_eq!(back.system_prompt, original.system_prompt, "system_prompt");
    assert_eq!(back.lineage, original.lineage, "lineage");
    assert_eq!(
        serde_json::to_value(&back.extra).unwrap(),
        serde_json::to_value(&original.extra).unwrap(),
        "session extra"
    );
    assert_eq!(
        back.cv_extra().and_then(|e| e.get("skipped_lines")),
        original.cv_extra().and_then(|e| e.get("skipped_lines")),
        "nothing was skipped on the way in that wasn't skipped on the way out"
    );

    // Messages, field by field.
    assert_eq!(back.messages.len(), original.messages.len(), "message count");
    for (i, (b, o)) in back.messages.iter().zip(&original.messages).enumerate() {
        assert_eq!(b.role, o.role, "message {i} role");
        assert_eq!(b.kind, o.kind, "message {i} kind");
        assert_eq!(b.origin, o.origin, "message {i} origin");
        assert_eq!(b.id, o.id, "message {i} id");
        assert_eq!(b.parent_id, o.parent_id, "message {i} parent_id");
        assert_eq!(b.timestamp, o.timestamp, "message {i} timestamp");
        assert_eq!(b.model, o.model, "message {i} model");
        assert_eq!(
            serde_json::to_value(&b.usage).unwrap(),
            serde_json::to_value(&o.usage).unwrap(),
            "message {i} usage"
        );
        assert_eq!(
            serde_json::to_value(&b.extra).unwrap(),
            serde_json::to_value(&o.extra).unwrap(),
            "message {i} extra"
        );
        let bt: Vec<&str> = b.content.iter().map(block_type).collect();
        let ot: Vec<&str> = o.content.iter().map(block_type).collect();
        assert_eq!(bt, ot, "message {i} block types");
        assert_eq!(
            serde_json::to_value(&b.content).unwrap(),
            serde_json::to_value(&o.content).unwrap(),
            "message {i} block contents"
        );
    }

    // The catch-all: everything, including anything the assertions above forgot to name. Only
    // `source_path` is realigned first, for the reason in this test's doc comment.
    assert_eq!(
        back.source_path.as_deref(),
        Some(path.as_path()),
        "source_path is the document"
    );
    let mut back_value = serde_json::to_value(&back).unwrap();
    back_value["source_path"] = serde_json::to_value(&original.source_path).unwrap();
    assert_eq!(
        back_value,
        serde_json::to_value(&original).unwrap(),
        "the whole IR, key for key"
    );

    // A census of what the round trip actually carried, for the record.
    let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut blocks: std::collections::BTreeMap<&str, usize> = Default::default();
    for m in &back.messages {
        *kinds.entry(m.kind.as_str()).or_default() += 1;
        for b in &m.content {
            *blocks.entry(block_type(b)).or_default() += 1;
        }
    }
    eprintln!("  kinds  {kinds:?}\n  blocks {blocks:?}");
    std::fs::remove_dir_all(&dir).ok();
}

// ── one version, read back exactly ────────────────────────────────────────────

/// A session touching every block type, message kind/origin, usage field, lineage pointer, git
/// info and both `extra` bags — written as a document and read back, the bytes are the same.
fn every_field_session() -> Session {
    let mut user = Message::new(Role::User);
    user.id = Some("m1".into());
    user.timestamp = Some(
        chrono::DateTime::parse_from_rfc3339("2026-05-29T21:00:00Z")
            .unwrap()
            .into(),
    );
    user.content = vec![
        Block::Text { text: "hi".into() },
        Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("ref-1".into()),
        },
        Block::File {
            mime: Some("text/plain".into()),
            path: Some("/x/notes.txt".into()),
            source: None,
        },
    ];
    let mut asst = Message::new(Role::Assistant);
    asst.id = Some("m2".into());
    asst.parent_id = Some("m1".into());
    asst.model = Some("claude-opus-4-8".into());
    asst.usage = Some(Usage {
        input_tokens: Some(10),
        output_tokens: Some(3),
        cache_read_tokens: Some(7),
        cache_creation_tokens: Some(1),
        reasoning_tokens: Some(2),
        cost_usd: Some(0.5),
    });
    asst.content = vec![
        Block::Thinking {
            text: "hmm".into(),
            signature: Some("sig".into()),
            encrypted: Some("enc".into()),
            redacted: false,
        },
        Block::Thinking {
            text: "".into(),
            signature: None,
            encrypted: Some("blob".into()),
            redacted: true,
        },
        Block::ToolUse {
            id: "call_1".into(),
            name: "Bash".into(),
            input: json!({ "command": "ls" }),
            namespace: Some("mcp".into()),
        },
    ];
    asst.harness_extra_mut(Harness::Claude)
        .insert("request_id".into(), json!("req_1"));
    let mut tool = Message::new(Role::Tool);
    tool.content = vec![Block::ToolResult {
        tool_use_id: "call_1".into(),
        content: "ok".into(),
        is_error: true,
        tool_name: Some("Bash".into()),
        status: Some("error".into()),
        details: Some(json!({ "exit": 1 })),
    }];
    let mut notice = Message::new(Role::System);
    notice.kind = MessageKind::Notice;
    notice.origin = Origin::Harness;
    notice.content = vec![Block::Text {
        text: "rate limited".into(),
    }];

    let mut s = Session {
        id: "s1".into(),
        harness: Harness::Claude,
        cwd: Some(PathBuf::from("/work/proj")),
        title: Some("every field".into()),
        created_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-05-29T21:00:00Z")
                .unwrap()
                .into(),
        ),
        updated_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-05-29T22:00:00Z")
                .unwrap()
                .into(),
        ),
        model: Some("claude-opus-4-8".into()),
        git: Some(GitInfo {
            branch: Some("main".into()),
            commit: Some("abc123".into()),
            remote: Some("git@example.com:x/y.git".into()),
        }),
        system_prompt: Some("be excellent".into()),
        lineage: Lineage {
            forked_from: Some("s0".into()),
            parent: Some("p0".into()),
            spawned_by_tool_use: Some("call_0".into()),
            continued_in: Some("s2".into()),
            continues: None,
            agent_path: Some("agent-1".into()),
        },
        messages: vec![user, asst, tool, notice],
        source_path: None,
        extra: Map::new(),
    };
    s.harness_extra_mut(Harness::Claude)
        .insert("custom_title".into(), json!("x"));
    s
}

/// The gate for this reader: a 0.3 document is `document()` → `from_str` → `document()` with not
/// one byte changed. There is no canonicalizer to drift; the reader IS the IR's `Deserialize`.
#[test]
fn a_0_3_document_round_trips_byte_identical() {
    let original = every_field_session();
    let written = serde_json::to_string_pretty(&document(&original)).unwrap();
    let back = from_str(&written).expect("a document cv wrote reads back");
    let rewritten = serde_json::to_string_pretty(&document(&back)).unwrap();
    assert_eq!(
        written, rewritten,
        "the document is not byte-identical after one round trip"
    );
    assert_eq!(back.harness, Harness::Claude, "the document names its origin harness");
}

/// A 0.2 document (`openSession`, the pre-0.11 spelling) is refused, and the error says which
/// version was found and where the renames are documented. So is any version that is not 0.3.
#[test]
fn other_versions_error_with_a_pointer() {
    let v02 = json!({ "openSession": "0.2", "harness": "claude", "id": "old-1", "messages": [] });
    let e = from_str(&v02.to_string()).expect_err("0.2 is not read").to_string();
    assert!(e.contains("OpenSession 0.2 document"), "{e}");
    assert!(e.contains("cv reads 0.3 only"), "{e}");
    assert!(e.contains("docs/OPENSESSION.md"), "{e}");

    let v04 = json!({ "open_session": "0.4", "harness": "claude", "id": "new", "messages": [] });
    let e = from_str(&v04.to_string()).expect_err("0.4 is not read").to_string();
    assert!(e.contains("OpenSession 0.4 document"), "{e}");
}

/// A document that names no harness, or one this build cannot name, is an `opensession` session;
/// a document without the marker at all (a bare `Session`, what `cv show --json` prints) reads.
#[test]
fn no_or_unknown_harness_is_an_opensession_session() {
    let s = from_str(&json!({ "id": "x", "messages": [] }).to_string()).unwrap();
    assert_eq!(s.harness, Harness::OpenSession);
    let s =
        from_str(&json!({ "open_session": "0.3", "harness": "chronovisor", "id": "x", "messages": [] }).to_string())
            .unwrap();
    assert_eq!(s.harness, Harness::OpenSession);
}

// ── discovery ─────────────────────────────────────────────────────────────────

/// Discovery over the registered export sources: a `*.opensession.json` is taken by name, a plain
/// `*.json` only when its head carries the version key, and anything else is left alone.
///
/// Mutates `$CV_EXPORTS`/`$XDG_CONFIG_HOME`, which is process-global — fine under nextest (a
/// process per test), and the reason this is the only test here that touches the environment.
#[test]
fn discovers_documents_in_registered_sources() {
    let dir = tmpdir("discover");
    let doc = json!({
        "open_session": "0.3", "harness": "codex", "id": "disco-1", "title": "found me",
        "messages": [
            { "role": "user", "kind": "prompt", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "kind": "reply", "content": [{ "type": "text", "text": "yo" }] },
            { "role": "tool", "kind": "tool_result", "content": [
                { "type": "tool_result", "tool_use_id": "c", "content": "out" }] }
        ]
    });
    std::fs::write(dir.join("disco-1.opensession.json"), doc.to_string()).unwrap();
    // A plain .json that IS a document (sniffed by its key), one that is not, and a non-JSON file.
    let mut by_key = doc.clone();
    by_key["id"] = json!("disco-2");
    std::fs::write(dir.join("some-session.json"), by_key.to_string()).unwrap();
    std::fs::write(dir.join("settings.json"), json!({ "theme": "dark" }).to_string()).unwrap();
    std::fs::write(dir.join("notes.txt"), "not json").unwrap();

    std::env::set_var("XDG_CONFIG_HOME", dir.join("no-config"));
    std::env::set_var("CV_EXPORTS", &dir);
    let refs = OpenSession.discover().unwrap();
    std::env::remove_var("CV_EXPORTS");
    std::env::remove_var("XDG_CONFIG_HOME");

    let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["disco-1", "disco-2"], "by name and by sniff, and nothing else");
    let one = refs.iter().find(|r| r.id == "disco-1").unwrap();
    assert_eq!(one.harness, Harness::OpenSession);
    assert_eq!(one.title.as_deref(), Some("found me"));
    assert_eq!(one.message_count, 2, "conversational turns only: user + assistant");

    // And the ref parses into the session the document describes, with ITS harness.
    let s = OpenSession.parse(one).unwrap();
    assert_eq!(s.harness, Harness::Codex);
    assert_eq!(s.messages.len(), 3);
    std::fs::remove_dir_all(&dir).ok();
}
