// Frozen pre-checked writer from 2a2d35f9; independent compatibility oracle.
//! The Qwen Code emitter: an IR [`Session`] written as Qwen Code 0.25's session JSONL, the shape
//! [`super`] reads first. Registered in `crate::emit::emitter_for`.
//!
//! The file is `<projects>/<sanitized cwd>/chats/<sessionId>.jsonl`, one `ChatRecord` per line
//! (`{uuid, parentUuid, sessionId, timestamp, type, subtype?, provenance, cwd, version?, gitBranch?,
//! message?, model?, usageMetadata?, toolCallResult?, systemPayload?}`), each record's
//! `parentUuid` the previous record's `uuid`. `out_dir` is `~/.qwen/projects` (the adapter's first
//! root), `~/.qwen/tmp` (the first root of a home that only ever held the gemini-cli shape; the
//! file goes to the sibling `projects/`, the only place 0.25 looks), or a Qwen home itself
//! (`--out ~/.qwen`, or a scratch dir: the file goes to its `projects/`).

use crate::emit::*;
use crate::harness::store::JsonlStage;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The project dir name for a cwd: every non-alphanumeric byte a dash (`/home/u/p` → `-home-u-p`).
fn sanitize_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `<home>/projects`, from the projects root, its `tmp/` sibling, or the home itself.
fn projects_dir(out_dir: &Path) -> PathBuf {
    match out_dir.file_name().and_then(|n| n.to_str()) {
        Some("projects") => out_dir.to_path_buf(),
        Some("tmp") => out_dir.parent().unwrap_or(out_dir).join("projects"),
        _ => out_dir.join("projects"),
    }
}

/// Gemini `Part`s for a message's content: text, thoughts, images, files and calls. Results are
/// written by the tool-turn arm, one record each.
fn parts(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } if !text.is_empty() => out.push(json!({ "text": text })),
            Block::Thinking { text, signature, .. } if !text.is_empty() => {
                let mut p = Map::new();
                p.insert("text".into(), json!(text));
                p.insert("thought".into(), json!(true));
                if let Some(s) = signature {
                    p.insert("thoughtSignature".into(), json!(s));
                }
                out.push(Value::Object(p));
            }
            Block::Image { media_type, .. } => {
                let mut inline = Map::new();
                if let Some(mt) = media_type {
                    inline.insert("mimeType".into(), json!(mt));
                }
                out.push(json!({ "inlineData": inline }));
            }
            Block::File { mime, path, source } => {
                let mut fd = Map::new();
                if let Some(mt) = mime {
                    fd.insert("mimeType".into(), json!(mt));
                }
                if let Some(uri) = source.as_deref().or(path.as_deref()) {
                    fd.insert("fileUri".into(), json!(uri));
                }
                out.push(json!({ "fileData": fd }));
            }
            Block::ToolUse {
                id,
                name,
                input,
                namespace,
            } => {
                // A deferred tool is invoked through `tool_call {name, arguments}` (the reader's
                // `namespace: tool_call`).
                let call = if namespace.as_deref() == Some(super::super::TOOL_CALL) {
                    json!({ "id": id, "name": super::super::TOOL_CALL, "args": { "name": name, "arguments": input } })
                } else {
                    json!({ "id": id, "name": name, "args": input })
                };
                out.push(json!({ "functionCall": call }));
            }
            _ => {}
        }
    }
    out
}

fn usage_metadata(m: &Message) -> Option<Value> {
    let u = m.usage.as_ref()?;
    let cached = u.cache_read_tokens.unwrap_or(0);
    let input = u.input_tokens.map(|i| i + cached);
    let mut out = Map::new();
    if let Some(i) = input {
        out.insert("promptTokenCount".into(), json!(i));
    }
    if let Some(o) = u.output_tokens {
        out.insert("candidatesTokenCount".into(), json!(o));
    }
    if let Some(r) = u.reasoning_tokens {
        out.insert("thoughtsTokenCount".into(), json!(r));
    }
    out.insert("cachedContentTokenCount".into(), json!(cached));
    out.insert(
        "totalTokenCount".into(),
        json!(input.unwrap_or(0) + u.output_tokens.unwrap_or(0)),
    );
    Some(Value::Object(out))
}

/// The subtype a model-facing, harness-written user record carries: the one it was read with
/// (a Qwen→Qwen port), else one named for its origin.
fn injected_subtype(m: &Message) -> String {
    if let Some(s) = m
        .harness_extra(Harness::Qwen)
        .and_then(|e| e.get("subtype"))
        .and_then(Value::as_str)
    {
        return s.to_string();
    }
    match m.origin {
        Origin::Scheduler => "cron",
        Origin::Subagent => "agent_message",
        _ => "notification",
    }
    .to_string()
}

/// Appends records, each parented on the one before.
struct Writer<'a> {
    session_id: &'a str,
    cwd: String,
    version: Option<String>,
    branch: Option<String>,
    last_uuid: Option<String>,
    fallback_ts: String,
    lines: JsonlStage,
    forked_from: Option<&'a str>,
}

impl Writer<'_> {
    fn push(&mut self, ts: Option<String>, ty: &str, fields: Map<String, Value>) -> Result<()> {
        let uuid = Uuid::new_v4().to_string();
        let mut r = Map::new();
        r.insert("uuid".into(), json!(uuid));
        r.insert("parentUuid".into(), json!(self.last_uuid));
        r.insert("sessionId".into(), json!(self.session_id));
        r.insert(
            "timestamp".into(),
            json!(ts.unwrap_or_else(|| self.fallback_ts.clone())),
        );
        r.insert("type".into(), json!(ty));
        r.insert("cwd".into(), json!(self.cwd));
        if let Some(v) = &self.version {
            r.insert("version".into(), json!(v));
        }
        if let Some(b) = &self.branch {
            r.insert("gitBranch".into(), json!(b));
        }
        r.extend(fields);
        if self.lines.is_empty() {
            if let Some(from) = self.forked_from {
                r.insert("forkedFrom".into(), json!({ "sessionId": from }));
            }
        }
        self.lines.push(Value::Object(r))?;
        self.last_uuid = Some(uuid);
        Ok(())
    }

    fn system(&mut self, ts: Option<String>, subtype: &str, payload: Value) -> Result<()> {
        let mut f = Map::new();
        f.insert("subtype".into(), json!(subtype));
        f.insert("provenance".into(), json!("system"));
        f.insert("systemPayload".into(), payload);
        self.push(ts, "system", f)
    }

    fn user(&mut self, ts: Option<String>, subtype: Option<&str>, provenance: &str, parts: Vec<Value>) -> Result<()> {
        let mut f = Map::new();
        if let Some(s) = subtype {
            f.insert("subtype".into(), json!(s));
        }
        f.insert("provenance".into(), json!(provenance));
        f.insert("message".into(), json!({ "role": "user", "parts": parts }));
        self.push(ts, "user", f)
    }
}

/// Emit Qwen Code 0.25 session JSONL under `out_dir` (see the module doc for what it may be).
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .context("a Qwen Code session is filed under its cwd; this session has none (pass --cwd)")?;
    let chats = projects_dir(out_dir).join(sanitize_cwd(&cwd_str)).join("chats");
    fs::create_dir_all(&chats).with_context(|| format!("creating {}", chats.display()))?;
    let path = chats.join(format!("{new_id}.jsonl"));

    let stamp = |t: Option<chrono::DateTime<Utc>>| t.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
    let mut w = Writer {
        session_id: &new_id,
        cwd: cwd_str.clone(),
        version: session
            .harness_extra(Harness::Qwen)
            .and_then(|e| e.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string),
        branch: branch_of(session),
        last_uuid: None,
        fallback_ts: stamp(session.created_at.or(session.updated_at))
            .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        lines: JsonlStage::new(),
        forked_from: session.lineage.forked_from.as_deref(),
    };

    let msgs = &session.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        let ts = stamp(m.timestamp);
        i += 1;
        match (m.role, m.kind) {
            (_, MessageKind::SystemPrompt) => {} // Qwen Code rebuilds its system prompt; none is stored
            (Role::Assistant, _) => {
                let p = parts(&m.content);
                if p.is_empty() {
                    continue;
                }
                let mut f = Map::new();
                f.insert("provenance".into(), json!("assistant_output"));
                if let Some(model) = m.model.as_ref().or(session.model.as_ref()) {
                    f.insert("model".into(), json!(model));
                }
                f.insert("message".into(), json!({ "role": "model", "parts": p }));
                if let Some(u) = usage_metadata(m) {
                    f.insert("usageMetadata".into(), u);
                }
                w.push(ts, "assistant", f)?;
            }
            (Role::Tool, _) => {
                for b in &m.content {
                    let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    else {
                        continue;
                    };
                    let key = if *is_error { "error" } else { "output" };
                    let name = tool_name.as_deref().unwrap_or("tool");
                    let status = if *is_error { "error" } else { "success" };
                    let mut f = Map::new();
                    f.insert("provenance".into(), json!("tool_result"));
                    f.insert(
                        "message".into(),
                        json!({ "role": "user", "parts": [{ "functionResponse": {
                            "id": tool_use_id, "name": name, "response": { key: content.to_string() } } }] }),
                    );
                    f.insert(
                        "toolCallResult".into(),
                        json!({ "callId": tool_use_id, "status": status, "resultDisplay": "" }),
                    );
                    w.push(ts.clone(), "tool_result", f)?;
                }
            }
            (Role::User, MessageKind::Prompt) => {
                let p = parts(&m.content);
                if !p.is_empty() {
                    w.user(ts, None, "real_user", p)?;
                }
            }
            (_, MessageKind::CompactionBoundary) => {
                // The boundary carries the summary that seeds the next window, when one follows.
                let mut payload = Map::new();
                let info: Map<String, Value> = m
                    .harness_extra(Harness::Qwen)
                    .map(|e| {
                        e.iter()
                            .filter(|(k, _)| k.as_str() != "subtype")
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                payload.insert("info".into(), Value::Object(info));
                if let Some(next) = msgs.get(i).filter(|n| n.kind == MessageKind::CompactionSummary) {
                    payload.insert(
                        "compressedHistory".into(),
                        json!([{ "role": "user", "parts": parts(&next.content) }]),
                    );
                    i += 1;
                }
                w.system(ts, "chat_compression", Value::Object(payload))?;
            }
            (_, MessageKind::ModelChange) => {
                if let Some(model) = &m.model {
                    w.system(ts, "session_model", json!({ "model": model }))?;
                }
            }
            (_, MessageKind::Error) => {
                let text = m.text().unwrap_or_default();
                w.system(ts, "turn_result", json!({ "error": { "message": text } }))?;
            }
            (_, MessageKind::Branch) => w.system(ts, "rewind", json!({}))?,
            (_, MessageKind::Notice) => {
                if let Some(text) = m.text().filter(|t| !t.is_empty()) {
                    w.system(ts, "notification", json!({ "text": text }))?;
                }
            }
            // Everything else the model saw (injected context, a summary without its boundary, a
            // sub-agent's or scheduler's turn): a harness-written user record.
            _ => {
                let p = parts(&m.content);
                if !p.is_empty() {
                    let sub = injected_subtype(m);
                    w.user(ts, Some(&sub), "system", p)?;
                }
            }
        }
    }
    if let Some(t) = &session.title {
        w.system(stamp(session.updated_at), "custom_title", json!({ "title": t }))?;
    }
    if w.lines.is_empty() {
        // The original Qwen writer emits one blank line even for an empty session.
        fs::write(&path, b"\n").with_context(|| format!("writing {}", path.display()))?;
    } else {
        w.lines.publish(&path)?;
    }
    Ok(EmitResult {
        path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("qwen --resume {new_id}  (run from {cwd_str})")),
    })
}
