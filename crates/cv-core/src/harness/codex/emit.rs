//! The Codex emitter: an IR [`Session`] written into Codex's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::fidelity::unrepresentable;
use crate::emit::*;
use crate::harness::store::write_jsonl;
use crate::harness::EmitResult;
use crate::ir::{Block, GitInfo, Harness, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    // Which tool calls exist at all, so a result whose call was pruned away can be paired below.
    let called = called_tool_ids(session);
    // `session_meta.cwd` (and `turn_context.cwd`) are REQUIRED by Codex's decoder (`SessionMeta.cwd:
    // PathBuf`, no default): a meta line without one is undecodable, the thread id can't be read,
    // and resume fails with "failed to parse thread ID from rollout file". A session with no known
    // cwd (some harnesses never record one) lands in the home dir rather than nowhere.
    let cwd = effective_cwd(session, opts)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd_str = cwd.to_string_lossy().into_owned();

    let now = session.created_at.unwrap_or_else(Utc::now);
    // Path: out_dir/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl — in LOCAL time and with no
    // zone suffix, exactly as `RolloutFileName::render` writes it (`OffsetDateTime::now_local()`):
    // Codex's `RolloutFileName::parse` requires a 19-char timestamp with byte 19 == `-` before the
    // uuid, so a `…T11-04-49Z-<uuid>` name (what we used to write) is invisible to every
    // filename-based lookup (thread-id resolution, migration, `builder_from_items`) and the thread
    // only resumed while its sqlite row happened to exist.
    let local = now.with_timezone(&chrono::Local);
    let date_dir = out_dir
        .join(local.format("%Y").to_string())
        .join(local.format("%m").to_string())
        .join(local.format("%d").to_string());
    fs::create_dir_all(&date_dir).with_context(|| format!("creating {}", date_dir.display()))?;
    let stamp = local.format("%Y-%m-%dT%H-%M-%S").to_string();
    let file_path = date_dir.join(format!("rollout-{stamp}-{new_id}.jsonl"));

    let ts_str = now.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();

    // session_meta first line.
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    // `session_id` is the root-thread id (Codex backfills it from `id` on read; explicit is clearer).
    meta.insert("session_id".into(), json!(new_id));
    meta.insert("timestamp".into(), json!(ts_str));
    meta.insert("cwd".into(), json!(cwd_str));
    meta.insert("source".into(), json!("cli"));
    meta.insert("thread_source".into(), json!("user"));
    // We write the pre-paginated layout (`response_item` message + `event_msg` user/agent twins);
    // say so — Codex ≥ 0.147 keys its persistence policy and ordinal handling off `history_mode`,
    // defaulting to `legacy` when absent, and the legacy→paginated migration canonicalizes it.
    meta.insert("history_mode".into(), json!("legacy"));
    meta.insert("originator".into(), json!("clustervision"));
    meta.insert("cli_version".into(), json!(env!("CARGO_PKG_VERSION")));
    // Codex persists this field into `state_5.sqlite.threads.model_provider` when it first
    // discovers the rollout. Omitting it creates an empty provider entry which the TUI then
    // cannot resume (`Model provider `` not found`), even though `turn_context.model` is valid.
    // The Codex target uses its standard OpenAI provider; the model id remains independently
    // carried by `turn_context` below.
    meta.insert("model_provider".into(), json!("openai"));
    // Session-level system prompt → Codex's own `base_instructions {text}` slot (the parser reads it
    // back into `Session::system_prompt`), so a ported session keeps the instructions it ran under.
    if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
        meta.insert("base_instructions".into(), json!({ "text": sp }));
    }
    if let Some(g) = &session.git {
        meta.insert("git".into(), codex_git(g));
    }
    lines.push(json!({
        "type": "session_meta",
        "timestamp": ts_str,
        "payload": Value::Object(meta),
    }));

    // The model rides in a `turn_context` record (per-turn config: model/cwd/approval/sandbox) —
    // that's the ONLY place Codex's own parser (and ours, `apply_turn_context`) reads a model id
    // from. (`session_meta.model_provider` is the *provider* name, not a model id; stuffing the
    // model there was silently dropped on re-parse.)
    // `TurnContextItem` requires `cwd`, `approval_policy`, `sandbox_policy`, `model` and `summary`
    // (no serde defaults); a `{cwd, model}` record fails `decode_rollout_line` and is silently
    // skipped on resume, so the model never actually came back. `on-request` + `workspace-write`
    // are Codex's own defaults (both decode with no further fields).
    if let Some(model) = &session.model {
        lines.push(json!({
            "type": "turn_context",
            "timestamp": ts_str,
            "payload": {
                "turn_id": Uuid::now_v7().to_string(),
                "cwd": cwd_str,
                "approval_policy": "on-request",
                "sandbox_policy": { "type": "workspace-write" },
                "model": model,
                "summary": "auto",
            },
        }));
    }

    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts_str.clone());

        // A compaction pair → Codex's own top-level `compacted` record (the parser reads it back as
        // a CompactionBoundary, plus a CompactionSummary when `message` is non-empty). The boundary
        // carries no text; the summary carries the seed text — both become `compacted` records so the
        // boundary survives even without a summary.
        if matches!(
            msg.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary
        ) {
            let text = if msg.kind == MessageKind::CompactionSummary {
                msg.text().unwrap_or_default()
            } else {
                String::new()
            };
            lines.push(json!({
                "type": "compacted",
                "timestamp": ts,
                "payload": { "message": text },
            }));
            continue;
        }

        match msg.role {
            Role::System => {
                let text = msg.text().unwrap_or_default();
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
            }
            Role::User => {
                let text = msg.text().unwrap_or_default();
                // The `response_item` message is what Codex feeds back into the model on resume;
                // the `event_msg` is only the UI transcript event. Emit BOTH (matching real
                // rollouts) — without the response_item the resumed model has no memory of the
                // user's turns. User content uses `input_text`.
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
                lines.push(codex_event_msg(&ts, json!({ "type": "user_message", "message": text })));
            }
            Role::Assistant => {
                let first_record = lines.len();
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match b {
                        Block::Text { text } => {
                            // Same as user turns: the response_item carries the assistant text
                            // into the resumed model context; the event_msg is UI-only. Assistant
                            // content uses `output_text`.
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": text }],
                                }),
                            ));
                            lines.push(codex_event_msg(
                                &ts,
                                json!({ "type": "agent_message", "message": text }),
                            ));
                        }
                        Block::Thinking { text, encrypted, .. } => {
                            let mut p = Map::new();
                            p.insert("type".into(), json!("reasoning"));
                            // `reasoning.content` is an output-only/raw-reasoning surface for the
                            // current Codex Responses wire. Replaying even one `reasoning_text`
                            // entry makes the next request fail with `array_above_max_length`
                            // (the input schema accepts zero raw content entries). Preserve the
                            // useful text through the replayable summary and the opaque state
                            // through `encrypted_content`, matching native Codex rollouts.
                            // The distinct user-visible summary a Codex source recorded, when it
                            // had one (IR v2: harness facts live in the harness bag, never flat).
                            let summary = msg
                                .harness_extra(Harness::Codex)
                                .and_then(|b| b.get("reasoning_summary"))
                                .and_then(Value::as_str)
                                .unwrap_or(text);
                            let summary_items = if summary.is_empty() {
                                Vec::new()
                            } else {
                                vec![json!({ "type": "summary_text", "text": summary })]
                            };
                            p.insert("summary".into(), Value::Array(summary_items));
                            if let Some(enc) = encrypted {
                                p.insert("encrypted_content".into(), json!(enc));
                            }
                            lines.push(codex_response_item(&ts, Value::Object(p)));
                        }
                        Block::ToolUse { id, name, input, .. } => {
                            let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args,
                                }),
                            ));
                        }
                        Block::File { path, source, .. } => {
                            let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                            lines.push(codex_event_msg(
                                &ts,
                                json!({
                                    "type": "agent_message",
                                    "message": format!("[file: {label}]"),
                                }),
                            ));
                        }
                        Block::ToolResult { .. } | Block::Image { .. } => {}
                    }
                }
                // Per-turn token usage → Codex's own `token_count` event, the ONLY record its parser
                // (and ours, `codex::apply_token_count`) reads usage from: it attaches
                // `info.last_token_usage` to the assistant message the event trails. Without it a
                // ported session read back `usage: 393 → 0`. Only when the turn actually wrote
                // records Codex reads back: trailing nothing (or only an empty `reasoning` item,
                // which its reader drops) the event would synthesize a bare usage-only turn.
                if lines.len() > first_record && !unrepresentable(Harness::Codex, opts.thinking, msg) {
                    if let Some(info) = codex_token_usage(msg.usage.as_ref()) {
                        lines.push(codex_event_msg(
                            &ts,
                            json!({ "type": "token_count", "info": { "last_token_usage": info } }),
                        ));
                    }
                }
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    {
                        // `FunctionCallOutputPayload` deserializes ONLY a string or a content-item
                        // array (and never serializes a `success` flag) — Codex itself has no
                        // persisted error bit. The object form `{content, success:false}` we used
                        // to write is undecodable: the line was dropped on resume, orphaning its
                        // `function_call`. Keep the error visible in the text instead; cv's own
                        // Codex parser recognizes the `[error] ` prefix and restores `is_error`.
                        let output = if *is_error {
                            json!(format!("[error] {content}"))
                        } else {
                            json!(content)
                        };
                        // Codex pairs an output with the call above it and logs "Orphan function
                        // call output for call id" for one it cannot pair — which a pruned session
                        // produces, since its first turn can be a result whose call was snipped.
                        // Emit a call shell first so the pair is well-formed and the output's text
                        // still reaches the model, rather than dropping the turn.
                        if is_orphan_result(b, &called) {
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "function_call",
                                    "call_id": tool_use_id,
                                    "name": tool_name.clone().unwrap_or_else(|| "unknown".into()),
                                    "arguments": "{}",
                                }),
                            ));
                        }
                        lines.push(codex_response_item(
                            &ts,
                            json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }),
                        ));
                    }
                }
            }
        }
    }

    write_jsonl(&file_path, &lines)?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("codex resume {new_id}")),
    })
}

/// IR [`Usage`](crate::ir::Usage) → Codex's `last_token_usage` block, reversing
/// [`codex::parse_usage`]'s renames (`cache_read_tokens` → `cached_input_tokens`,
/// `cache_creation_tokens` → `cache_write_input_tokens`, `reasoning_tokens` →
/// `reasoning_output_tokens`). Only the fields actually present are written, so a Codex source
/// round-trips with the same key set. `None` when there is nothing Codex's parser would read back
/// (it requires at least one of input / output / cached-input).
fn codex_token_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    // The IR's `input_tokens` excludes the cache counts; Codex's includes them.
    let input = u
        .input_tokens
        .map(|i| i + u.cache_read_tokens.unwrap_or(0) + u.cache_creation_tokens.unwrap_or(0));
    for (key, val) in [
        ("input_tokens", input),
        ("cached_input_tokens", u.cache_read_tokens),
        ("cache_write_input_tokens", u.cache_creation_tokens),
        ("output_tokens", u.output_tokens),
        ("reasoning_output_tokens", u.reasoning_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return None;
    }
    // Codex's own totals are input + output (cached input is a *subset* of its `input_tokens`, not
    // an addend — 35507 + 55 = 35562 in a real 0.155 record whose cached count was 34432).
    let total: u64 = u.total_tokens();
    m.insert("total_tokens".into(), json!(total));
    Some(Value::Object(m))
}

fn codex_event_msg(ts: &str, payload: Value) -> Value {
    json!({ "type": "event_msg", "timestamp": ts, "payload": payload })
}

fn codex_response_item(ts: &str, payload: Value) -> Value {
    json!({ "type": "response_item", "timestamp": ts, "payload": payload })
}

fn codex_git(g: &GitInfo) -> Value {
    let mut m = Map::new();
    if let Some(b) = &g.branch {
        m.insert("branch".into(), json!(b));
    }
    if let Some(c) = &g.commit {
        m.insert("commit_hash".into(), json!(c));
    }
    if let Some(r) = &g.remote {
        m.insert("repository_url".into(), json!(r));
    }
    Value::Object(m)
}
