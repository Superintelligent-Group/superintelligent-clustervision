//! The Gemini emitter: an IR [`Session`] written into Gemini's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::{Block, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit a gemini-cli legacy chat recording: `out_dir/<sessionId>.json`, a single whole-file
/// `ConversationRecord` `{sessionId, projectHash, startTime, lastUpdated, messages[]}`. The reader
/// (`harness/gemini.rs`) requires the file to live under a `chats/` dir to be picked up by
/// `discover`, but `parse_all_str` / `parse` will read it directly from any path.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Gemini's `--resume` / `--list-sessions` only scan `<globalTemp>/<projectIdentifier>/chats/`,
    // where `out_dir` is `<globalTemp>` (~/.gemini/tmp) and `<projectIdentifier>` is the cwd basename
    // registered in `<globalTemp>/../projects.json`. Writing to a bare `chats/` (no identifier dir)
    // makes the session invisible to the installed binary. So register the cwd→slug mapping (matching
    // gemini's getShortId) and write into the slug's chats dir. (Verified live: with the file in the
    // right slug dir, `gemini --resume <uuid>` recalls the conversation.)
    let chats_dir = match gemini_register_project(out_dir, &cwd_str) {
        Some(slug) => out_dir.join(slug).join("chats"),
        None => out_dir.join("chats"), // no cwd → fall back to the legacy bare path
    };
    fs::create_dir_all(&chats_dir).with_context(|| format!("creating {}", chats_dir.display()))?;
    let file_path = chats_dir.join(format!("session-{new_id}.json"));

    let start = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let last = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // Index tool results by tool_use_id so they can be folded into the producing assistant's
    // `toolCalls[].result` (the canonical legacy shape the reader pairs from).
    let mut tool_results: std::collections::HashMap<String, (String, bool)> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    tool_results.insert(tool_use_id.clone(), (content.to_string(), *is_error));
                }
            }
        }
    }

    let called = called_tool_ids(session);

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        let ts = msg.timestamp.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
        match msg.role {
            Role::User | Role::System => {
                let mty = if msg.role == Role::System { "info" } else { "user" };
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!(mty));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                messages.push(Value::Object(m));
            }
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!("gemini"));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Per-message usage → Gemini's `tokens{input, output, cached}` (the parser reads it
                // back into `Usage`), so token counts survive a port into Gemini.
                if let Some(u) = &msg.usage {
                    let mut tokens = Map::new();
                    // Gemini's `input` includes `cached`; the IR's excludes it.
                    if let Some(x) = u.input_tokens {
                        tokens.insert("input".into(), json!(x + u.cache_read_tokens.unwrap_or(0)));
                    }
                    if let Some(x) = u.output_tokens {
                        tokens.insert("output".into(), json!(x));
                    }
                    if let Some(x) = u.cache_read_tokens {
                        tokens.insert("cached".into(), json!(x));
                    }
                    if !tokens.is_empty() {
                        m.insert("tokens".into(), Value::Object(tokens));
                    }
                }
                // Thoughts (thinking) ride in `thoughts[]`, the rest in `content`.
                let mut thoughts = Vec::new();
                for b in &msg.content {
                    if let Block::Thinking { text, .. } = b {
                        thoughts.push(json!({ "subject": "", "description": text }));
                    }
                }
                if !thoughts.is_empty() {
                    m.insert("thoughts".into(), Value::Array(thoughts));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                // Tool calls → toolCalls[] (legacy ToolCallRecord), folding in the paired result so
                // the reader emits a proper Tool turn for it.
                let mut calls = Vec::new();
                for b in &msg.content {
                    if let Block::ToolUse { id, name, input, .. } = b {
                        let mut call = Map::new();
                        call.insert("id".into(), json!(id));
                        call.insert("name".into(), json!(name));
                        call.insert("args".into(), input.resolve()?.into_owned());
                        if let Some((content, is_error)) = tool_results.get(id) {
                            call.insert("status".into(), json!(if *is_error { "error" } else { "success" }));
                            call.insert(
                                "result".into(),
                                json!([{
                                    "functionResponse": {
                                        "id": id,
                                        "name": name,
                                        "response": { "output": content },
                                    }
                                }]),
                            );
                        }
                        calls.push(Value::Object(call));
                    }
                }
                if !calls.is_empty() {
                    m.insert("toolCalls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(m));
            }
            // Tool turns are folded into the assistant's toolCalls[].result above — except an
            // ORPHAN result ([`called_tool_ids`]), which has no call here to fold onto. It gets a
            // `gemini` record of its own holding the call shell (no `args`) and the response, so
            // the output survives instead of vanishing.
            Role::Tool => {
                let calls: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            tool_name,
                            ..
                        } if !called.contains(tool_use_id.as_str()) => {
                            let name = tool_name.clone().unwrap_or_else(|| "tool".into());
                            Some(json!({
                                "id": tool_use_id,
                                "name": name,
                                "status": if *is_error { "error" } else { "success" },
                                "result": [{
                                    "functionResponse": {
                                        "id": tool_use_id,
                                        "name": name,
                                        "response": { "output": content.to_string() },
                                    }
                                }],
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                    m.insert("type".into(), json!("gemini"));
                    if let Some(t) = &ts {
                        m.insert("timestamp".into(), json!(t));
                    }
                    m.insert("toolCalls".into(), Value::Array(calls));
                    messages.push(Value::Object(m));
                }
            }
        }
    }

    let mut record = Map::new();
    record.insert("sessionId".into(), json!(new_id));
    // gemini keys recordings by an opaque projectHash; synthesize a stable one from the cwd.
    // (The resume path doesn't validate this value; discovery is by the slug dir computed above.)
    record.insert("projectHash".into(), json!(short_hash(&cwd_str)));
    record.insert("startTime".into(), json!(start));
    record.insert("lastUpdated".into(), json!(last));
    if let Some(t) = &session.title {
        record.insert("summary".into(), json!(t));
    }
    if let Some(c) = &cwd {
        record.insert("directories".into(), json!([c.to_string_lossy()]));
    }
    record.insert("messages".into(), Value::Array(messages));

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(record))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(match &cwd {
            Some(c) => format!("gemini --resume {new_id}  (run from {})", c.display()),
            None => format!("gemini --resume {new_id}"),
        }),
    })
}

/// Bind `cwd` to a slug dir under gemini's `<globalTemp>` (=`out_dir`), replicating
/// `ProjectRegistry.getShortId`/`claimNewSlug`. Gemini owns a slug dir via a `.project_root` marker
/// file containing the project path; on launch it RE-derives the slug and, if our dir lacks a marker
/// (or the projects.json entry can't be verified), reassigns a fresh slug — so writing only
/// projects.json is not enough. We therefore: pick the cwd basename (suffixing `-2`, `-3`… if a
/// DIFFERENT path already owns that dir's marker), write the `.project_root` marker, and record the
/// projects.json mapping (the fast path; gemini heals from the marker regardless). After this,
/// `gemini --resume`/`--list-sessions` discover the session. Returns the slug, `None` if `cwd` empty.
fn gemini_register_project(global_tmp: &Path, cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    const MARKER: &str = ".project_root";
    // Reuse the slug whose marker already owns this cwd, if any (gemini's findExistingSlugForPath).
    let owned_by = |slug_dir: &Path| -> Option<String> {
        fs::read_to_string(slug_dir.join(MARKER))
            .ok()
            .map(|s| s.trim().to_string())
    };
    if let Ok(entries) = fs::read_dir(global_tmp) {
        for e in entries.flatten() {
            if owned_by(&e.path()).as_deref() == Some(cwd) {
                return e.file_name().to_str().map(str::to_string);
            }
        }
    }
    // Otherwise claim the basename, skipping dirs whose marker owns a different path.
    let base = Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string());
    let mut slug = base.clone();
    let mut n = 2;
    loop {
        let dir = global_tmp.join(&slug);
        match owned_by(&dir) {
            Some(owner) if owner != cwd => {
                slug = format!("{base}-{n}");
                n += 1;
            }
            _ => break, // free, or already ours
        }
    }
    let slug_dir = global_tmp.join(&slug);
    let _ = fs::create_dir_all(&slug_dir);
    let _ = fs::write(slug_dir.join(MARKER), cwd);
    // Record the projects.json fast-path mapping too (best-effort).
    if let Some(parent) = global_tmp.parent() {
        let projects_json = parent.join("projects.json");
        let mut doc: Value = fs::read_to_string(&projects_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let projects = doc
            .as_object_mut()
            .unwrap()
            .entry("projects")
            .or_insert_with(|| json!({}));
        if let Some(m) = projects.as_object_mut() {
            m.insert(cwd.to_string(), json!(slug));
        }
        if let Ok(s) = serde_json::to_string_pretty(&doc) {
            let _ = fs::write(&projects_json, s);
        }
    }
    Some(slug)
}

/// Map IR blocks → a Gemini `Part[]` (`text` / thought `text` / `inlineData`). Tool calls are
/// emitted separately via `toolCalls[]`, so they're skipped here.
fn gemini_parts(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "text": text })),
            Block::Thinking { text, .. } => out.push(json!({ "text": text, "thought": true })),
            Block::Image { media_type, .. } => {
                let mut inline = Map::new();
                if let Some(mt) = media_type {
                    inline.insert("mimeType".into(), json!(mt));
                }
                out.push(json!({ "inlineData": Value::Object(inline) }));
            }
            Block::File { mime, path, source } => {
                let mut fd = Map::new();
                if let Some(mt) = mime {
                    fd.insert("mimeType".into(), json!(mt));
                }
                if let Some(uri) = source.as_deref().or(path.as_deref()) {
                    fd.insert("fileUri".into(), json!(uri));
                }
                out.push(json!({ "fileData": Value::Object(fd) }));
            }
            // Tool use/result handled out of band.
            Block::ToolUse { .. } | Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}
