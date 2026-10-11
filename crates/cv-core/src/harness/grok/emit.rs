//! The Grok emitter: an IR [`Session`] written into Grok's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::store::write_jsonl;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Percent-encode everything except unreserved chars, so `/` → `%2F` (matching Grok's dir layout).
const GROK_ENCODE: &AsciiSet = &CONTROLS.add(b'/').add(b' ').add(b'.').add(b'%').add(b':').add(b'\\');

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc_cwd = utf8_percent_encode(&cwd_str, GROK_ENCODE).to_string();

    let session_dir = out_dir.join(&enc_cwd).join(&new_id);
    fs::create_dir_all(&session_dir).with_context(|| format!("creating {}", session_dir.display()))?;

    let created = session
        .created_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // summary.json
    let mut summary = Map::new();
    summary.insert("info".into(), json!({ "id": new_id, "cwd": cwd_str }));
    summary.insert("created_at".into(), json!(created));
    summary.insert("updated_at".into(), json!(updated));
    summary.insert("last_active_at".into(), json!(updated));
    summary.insert("num_chat_messages".into(), json!(session.messages.len()));
    // Real Grok summaries carry these too; without at least `chat_format_version`
    // the loader rejects the dir with "Session does not exist". (Verified live against
    // grok 0.1.219 — adding the full field set is what makes `grok --resume` discover it.)
    let num_turns = session.messages.iter().filter(|m| matches!(m.role, Role::User)).count();
    summary.insert("num_messages".into(), json!(num_turns));
    summary.insert("next_trace_turn".into(), json!(0));
    summary.insert("chat_format_version".into(), json!(1));
    summary.insert("agent_name".into(), json!("grok-build"));
    if let Some(home) = std::env::var_os("HOME") {
        summary.insert(
            "grok_home".into(),
            json!(format!("{}/.grok", Path::new(&home).to_string_lossy())),
        );
    }
    // Carry the source model verbatim for a Grok→Grok port (it is already a Grok model), and for a
    // cross-harness port only when it *is* a Grok model; otherwise default to `grok-build`. A
    // foreign model id (e.g. `gpt-5.5`) would make Grok replay the history against an incompatible
    // backend, so it is dropped there — but a same-harness rehome must round-trip the model exactly.
    // The verifier asks [`grok_model_id`] the same question, and classifies a pinned foreign model
    // as an expected substitution rather than an avoidable loss.
    summary.insert("current_model_id".into(), json!(grok_model_id(session)));
    if let Some(g) = &session.git {
        if let Some(b) = &g.branch {
            summary.insert("head_branch".into(), json!(b));
        }
        if let Some(c) = &g.commit {
            summary.insert("head_commit".into(), json!(c));
        }
        if let Some(r) = &g.remote {
            summary.insert("git_remotes".into(), json!([r]));
        }
    }
    summary.insert("session_summary".into(), json!(session.title.as_deref().unwrap_or("")));
    let summary_path = session_dir.join("summary.json");
    fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    // chat_history.jsonl, plus an updates.jsonl sidecar carrying each tool call's terminal status.
    // chat_history alone cannot express a failed tool result — the Grok parser derives
    // `ToolResult.is_error`/`status` ONLY from `updates.jsonl` `tool_call_update` entries — so
    // without the sidecar every ported failure came back as a success.
    let mut lines: Vec<Value> = Vec::new();
    let mut updates: Vec<Value> = Vec::new();
    for msg in &session.messages {
        match msg.role {
            Role::System => {
                let text = grok_concat_text(msg);
                lines.push(json!({ "type": "system", "content": text }));
            }
            Role::User => {
                let text = grok_concat_text(msg);
                lines.push(json!({
                    "type": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            Role::Assistant => {
                let mut line = Map::new();
                line.insert("type".into(), json!("assistant"));
                line.insert("content".into(), json!(grok_concat_text(msg)));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    line.insert("model_id".into(), json!(model));
                }
                for b in &msg.content {
                    if let Block::Thinking { text, encrypted, .. } = b {
                        let mut r = Map::new();
                        r.insert("text".into(), json!(text));
                        // `encrypted` reasoning blobs are provider/account-bound. Carrying one
                        // from a foreign harness (e.g. OpenAI's `encrypted_content`) makes Grok's
                        // backend fail with "Could not decrypt the provided encrypted_content".
                        // Only preserve it for a Grok→Grok port.
                        if let Some(enc) = encrypted {
                            if session.harness == Harness::Grok {
                                r.insert("encrypted".into(), json!(enc));
                            }
                        }
                        line.insert("reasoning".into(), Value::Object(r));
                        break;
                    }
                }
                // Emit assistant tool calls. The Grok parser reads `tool_calls[]` with
                // `arguments` as a JSON-encoded *string*, so re-encode the structured input.
                let calls: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input, .. } => Some(json!({
                            "id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    line.insert("tool_calls".into(), Value::Array(calls));
                }
                lines.push(Value::Object(line));
            }
            // Tool turns map to `tool_result` lines (the parser turns these back into Role::Tool).
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        status,
                        ..
                    } = b
                    {
                        lines.push(json!({
                            "type": "tool_result",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                        // ACP-style status entry: the source status verbatim when it has one,
                        // else derived from `is_error` (Grok's own terminal states).
                        let status = status
                            .clone()
                            .unwrap_or_else(|| if *is_error { "failed" } else { "completed" }.into());
                        updates.push(json!({
                            "timestamp": epoch_ms(msg.timestamp.or(session.updated_at)),
                            "method": "session/update",
                            "params": {
                                "sessionId": new_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": tool_use_id,
                                    "status": status,
                                },
                            },
                        }));
                    }
                }
            }
        }
    }
    let chat_path = session_dir.join("chat_history.jsonl");
    write_jsonl(&chat_path, &lines)?;
    if !updates.is_empty() {
        write_jsonl(&session_dir.join("updates.jsonl"), &updates)?;
    }

    Ok(EmitResult {
        path: session_dir,
        new_id: new_id.clone(),
        resume_hint: Some(format!("grok --resume {new_id}")),
    })
}

/// Concatenate a message's text blocks (Grok chat_history carries plain text only).
fn grok_concat_text(msg: &crate::ir::Message) -> String {
    let mut s = String::new();
    for b in &msg.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

/// The model id [`emit_grok`] records in `summary.json` as `current_model_id` — the session's own
/// when Grok can serve it, else [`GROK_FALLBACK_MODEL`]. Shared with the verifier so "did the
/// emitter pin a foreign model?" is asked of the same code that does the pinning.
pub(crate) fn grok_model_id(session: &Session) -> &str {
    session
        .model
        .as_deref()
        .filter(|m| session.harness == Harness::Grok || m.to_ascii_lowercase().contains("grok"))
        .unwrap_or(GROK_FALLBACK_MODEL)
}

/// What `summary.json` records when the source model is not one Grok can run.
pub(crate) const GROK_FALLBACK_MODEL: &str = "grok-build";
