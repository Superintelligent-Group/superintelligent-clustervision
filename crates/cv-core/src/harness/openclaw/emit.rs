//! The OpenClaw emitter: an IR [`Session`] written into OpenClaw's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::store::write_jsonl;
use crate::harness::wire::anthropic;
use crate::harness::EmitResult;
use crate::ir::{Block, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit into OpenClaw's `agents/<agentId>/sessions/<sid>.jsonl` transcript (v3 parent-linked) plus
/// a `sessions.json` index entry. Mirrors `harness/openclaw.rs`'s reader: a `{type:session,…}`
/// header line followed by `{type:message,id,parentId,timestamp,message:{role,content,…}}` lines.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_id = "main";
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // `out_dir` is already the agents dir: the adapter's first root is `~/.openclaw/agents` and discover()
    // walks it for `<agentId>/sessions/<id>.jsonl`. Re-joining "agents" here produced
    // `~/.openclaw/agents/agents/main/sessions/…`, which openclaw never scans (the session was written
    // but undiscoverable). If a caller instead points `--out` at the state dir (`~/.openclaw`), accept
    // that too by appending the missing `agents` segment.
    let agents_root = if out_dir.file_name().is_some_and(|n| n == "agents") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("agents")
    };
    let sessions_dir = agents_root.join(agent_id).join("sessions");
    fs::create_dir_all(&sessions_dir).with_context(|| format!("creating {}", sessions_dir.display()))?;
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));

    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();
    // Header line.
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(3));
    header.insert("id".into(), json!(new_id));
    header.insert("timestamp".into(), json!(created_iso));
    if !cwd_str.is_empty() {
        header.insert("cwd".into(), json!(cwd_str));
    }
    lines.push(Value::Object(header));

    // A `session_info` entry carries the session title (OpenClaw's `name`), which the reader reads
    // back as `Session::title` — the `sessions.json`/`session_nodes` index label alone is only a
    // discovery hint, not seen by a direct `parse`. Emit it so the title round-trips.
    if let Some(title) = session.title.as_deref().filter(|t| !t.trim().is_empty()) {
        lines.push(json!({
            "type": "session_info",
            "id": openclaw_short_id(),
            "parentId": Value::Null,
            "timestamp": created_iso,
            "name": title,
        }));
    }

    // Message lines, parent-linked (v3).
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        let entry_id = openclaw_short_id();
        let ts = msg.timestamp.unwrap_or(created);
        let ts_iso = ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        let ts_ms = ts.timestamp_millis();

        // Kind-aware control entries → OpenClaw's own top-level records (the reader parses them as
        // System notes / lineage markers), so a cross-harness compaction, rewind, or model switch is
        // carried rather than flattened into a plain system turn. They still thread by parentId.
        let control: Option<Value> = match msg.kind {
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary => Some(json!({
                "type": "compaction",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "summary": msg.text().unwrap_or_default(),
            })),
            MessageKind::Branch => Some(json!({
                "type": "reset",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "reason": "reset",
            })),
            MessageKind::ModelChange => Some(json!({
                "type": "model_change",
                "id": entry_id,
                "parentId": parent,
                "timestamp": ts_iso,
                "provider": Value::Null,
                "modelId": msg.model.clone().or_else(|| session.model.clone()),
            })),
            _ => None,
        };
        if let Some(entry) = control {
            lines.push(entry);
            parent = Some(entry_id);
            continue;
        }

        let inner = match msg.role {
            Role::User => json!({
                "role": "user",
                "content": openclaw_content_blocks(&msg.content)?,
                "timestamp": ts_ms,
            }),
            Role::System => json!({
                "role": "system",
                "content": openclaw_content_blocks(&msg.content)?,
                "timestamp": ts_ms,
            }),
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), openclaw_content_blocks(&msg.content)?);
                m.insert("timestamp".into(), json!(ts_ms));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Assistant-level token usage → OpenClaw's own `message.usage` block, which its
                // reader (`openclaw::parse_usage`) reads straight back into [`Usage`]. Without it a
                // ported session read back `usage: 462 → 0`.
                if let Some(u) = openclaw_usage(msg.usage.as_ref()) {
                    m.insert("usage".into(), u);
                }
                Value::Object(m)
            }
            Role::Tool => {
                // OpenClaw models each tool result as its own `toolResult` message.
                let (tool_use_id, content, is_error, tool_name) = openclaw_tool_result(&msg.content);
                let mut m = Map::new();
                m.insert("role".into(), json!("toolResult"));
                m.insert("toolCallId".into(), json!(tool_use_id));
                if let Some(tn) = tool_name {
                    m.insert("toolName".into(), json!(tn));
                }
                m.insert("content".into(), json!([{ "type": "text", "text": content }]));
                m.insert("isError".into(), json!(is_error));
                m.insert("timestamp".into(), json!(ts_ms));
                Value::Object(m)
            }
        };

        lines.push(json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent,
            "timestamp": ts_iso,
            "message": inner,
        }));
        parent = Some(entry_id);
    }

    write_jsonl(&file_path, &lines)?;

    // sessions.json index: merge-or-create.
    let index_path = sessions_dir.join("sessions.json");
    let mut index: Map<String, Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = session.title.as_ref().or(session.first_user_text().as_ref()) {
        entry.insert("label".into(), json!(crate::ir::truncate(t, 80)));
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
        // Headless resume is the `agent` subcommand with `--session-id` + a `--message`, run
        // in-process via `--local` (verified against openclaw's CLI registration; there is no
        // `openclaw --session` flag outside the interactive TUI launcher).
        resume_hint: Some(format!(
            "openclaw agent --session-id {new_id} --message \"<your prompt>\" --local"
        )),
    })
}

/// 8-hex-char id slice, matching OpenClaw's entry-id convention.
/// IR [`Usage`](crate::ir::Usage) → OpenClaw's `usage{input, output, cacheRead, cacheWrite,
/// totalTokens, cost{total}}`, reversing [`openclaw::parse_usage`]'s names. Only the fields present
/// are written, so an OpenClaw source round-trips with the same key set; `None` when the turn
/// carried no counts and no cost (nothing its reader would accept).
fn openclaw_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    for (key, val) in [
        ("input", u.input_tokens),
        ("output", u.output_tokens),
        ("cacheRead", u.cache_read_tokens),
        ("cacheWrite", u.cache_creation_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if let Some(c) = u.cost_usd {
        m.insert("cost".into(), json!({ "total": c }));
    }
    if m.is_empty() {
        return None;
    }
    m.insert(
        "totalTokens".into(),
        json!(u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0)),
    );
    Some(Value::Object(m))
}

fn openclaw_short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// IR blocks → an OpenClaw content array (`text`/`thinking`/`toolCall`/`image`).
/// A deferred tool's call goes back through OpenClaw's `tool_call` wrapper.
fn openclaw_content_blocks(content: &[Block]) -> Result<Value> {
    let mut wrapped = Vec::new();
    let mut changed = false;
    for block in content {
        if let Some(rewrapped) = crate::affordance::rewrap_deferred(crate::ir::Harness::OpenClaw, block)? {
            changed = true;
            wrapped.push(rewrapped);
        } else {
            wrapped.push(block.clone());
        }
    }
    let blocks = if changed { wrapped.as_slice() } else { content };
    Ok(Value::Array(anthropic::assistant_blocks(blocks, &anthropic::OPENCLAW)?))
}

/// Pull the (id, content, is_error, name) out of a Tool turn's blocks.
fn openclaw_tool_result(content: &[Block]) -> (String, String, bool, Option<String>) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string(), *is_error, tool_name.clone());
        }
    }
    (String::new(), String::new(), false, None)
}
