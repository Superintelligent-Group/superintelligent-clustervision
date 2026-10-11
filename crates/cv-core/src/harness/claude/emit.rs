//! The Claude emitter: an IR [`Session`] written as a Claude Code transcript that `claude --resume`
//! accepts — the Anthropic codec run backwards plus the JSONL writer, and the resumability rules
//! (linear parents, no orphan `tool_use`, sidechain identity) that make the result resumable,
//! checked by [`verify_claude_resumable`] inside `emit_verified`. Registered in
//! `crate::emit::emitter_for`; the shared skeleton is in [`crate::emit`].

use crate::emit::*;
use crate::harness::store::write_jsonl;
use crate::harness::wire::anthropic;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Resolve symlinks in `cwd` the way Claude Code does when it looks for sessions to `--resume`.
///
/// Claude derives a session's project dir from the *realpath* of the directory it's launched in, so
/// a session we write must be keyed off the resolved path or `claude --resume` reports "No
/// conversation found". This bites on macOS, where `/tmp` and `/var` are symlinks into `/private`:
/// porting a session whose cwd is `/tmp/foo` would write `-tmp-foo` while Claude, launched from
/// `/tmp/foo`, looks under `-private-tmp-foo`. Canonicalizing here keeps the two in lockstep and
/// also matches what Claude records in its own transcripts (it runs in the resolved dir).
///
/// Falls back to the path as-given when it can't be resolved — e.g. the target dir doesn't exist
/// yet — which preserves the prior behavior for those cases.
fn realpath_for_claude(cwd: &Path) -> PathBuf {
    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Encode a cwd into Claude's project-dir name: leading `-`, then every `/` and `.` becomes `-`.
pub(crate) fn claude_encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    // Strip a single leading '/' so the mandatory leading '-' isn't doubled.
    let s = s.strip_prefix('/').unwrap_or(&s);
    let mut out = String::with_capacity(s.len() + 1);
    out.push('-');
    for ch in s.chars() {
        match ch {
            '/' | '.' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    // The id every record is stamped with, and the id the caller resumes by. For a main-thread
    // session they are one uuid; for a sub-agent transcript the records carry the ROOT's id and the
    // handle is the agent id (see [`EmitOptions::sidechain_of`]).
    let (session_id, agent) = match &opts.sidechain_of {
        Some((root, agent)) => (root.clone(), Some(agent.clone())),
        None => (opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()), None),
    };
    let new_id = agent.clone().unwrap_or_else(|| session_id.clone());
    // Resolve symlinks so the project-dir name matches the realpath Claude resolves at resume time.
    let cwd = effective_cwd(session, opts).map(|p| realpath_for_claude(&p));
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir_name = cwd
        .as_ref()
        .map(|p| claude_encode_cwd(p))
        .unwrap_or_else(|| "-".to_string());
    let branch = branch_of(session);

    let file_path = match &agent {
        Some(a) => {
            fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
            out_dir.join(format!("agent-{a}.jsonl"))
        }
        None => {
            let proj_dir = out_dir.join(&dir_name);
            fs::create_dir_all(&proj_dir).with_context(|| format!("creating {}", proj_dir.display()))?;
            proj_dir.join(format!("{new_id}.jsonl"))
        }
    };

    let mut lines: Vec<Value> = Vec::new();
    let ts0 = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // ai-title line (so the title round-trips) — unless a format-complete carrier already replays the
    // original ai-title record verbatim (emitting both would duplicate it).
    let has_title_carrier = session
        .messages
        .iter()
        .any(|m| carrier_record_of(m).is_some_and(|r| r.get("type").and_then(Value::as_str) == Some("ai-title")));
    if let Some(title) = &session.title {
        if !has_title_carrier && agent.is_none() {
            lines.push(json!({
                "type": "ai-title",
                "aiTitle": title,
                "sessionId": session_id,
                "cwd": cwd_str,
                "timestamp": ts0,
            }));
        }
    }

    // Session-level system prompt → a `prompt_snapshot` attachment (Claude's own slot for it), so a
    // cross-harness port keeps the instructions the model ran under. A Claude→Claude session already
    // carries this (its own `prompt_snapshot` attachment rides through as a carrier), so emit it only
    // for a foreign source to avoid a duplicate.
    if session.harness != Harness::Claude {
        if let Some(sp) = session.system_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            lines.push(json!({
                "type": "attachment",
                "sessionId": session_id,
                "cwd": cwd_str,
                "timestamp": ts0,
                "attachment": { "type": "prompt_snapshot", "systemPrompt": [sp] },
            }));
        }
    }

    // Thread the conversation by uuid / parentUuid.
    let mut parent: Option<String> = None;
    // Non-carrier System turns can't be replayed (see below) and are dropped — record each dropped
    // record's uuid → its own (surviving) parent, so a child that pointed at a dropped uuid is
    // re-linked over the gap instead of left with a dangling parentUuid. `resolve_parent` follows
    // chains of consecutively-dropped ancestors (bounded; parent links form a DAG).
    let mut dropped: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    fn resolve_parent(dropped: &std::collections::HashMap<String, Option<String>>, pid: String) -> Option<String> {
        let mut p = Some(pid);
        for _ in 0..=dropped.len() {
            match p.as_ref().and_then(|id| dropped.get(id)) {
                Some(next) => p = next.clone(),
                None => break,
            }
        }
        p
    }
    for msg in &session.messages {
        // Format-complete carrier (a meta/system record carried verbatim under
        // `ParseOptions::complete`): replay it byte-for-byte and skip envelope reconstruction. These
        // are non-conversational, so they don't participate in uuid/parentUuid threading. The one
        // exception to "verbatim": a record's `sessionId`/`cwd` are the session's *identity* — a
        // ported session must carry the NEW ones (Claude keys resume off them), so rewrite them to
        // match the envelope. On a pure round-trip (same id, same cwd) this is byte-identical.
        if let Some(record) = carrier_record_of(msg) {
            let mut record = record.clone();
            if let Some(obj) = record.as_object_mut() {
                if obj.contains_key("sessionId") {
                    obj.insert("sessionId".into(), json!(session_id));
                }
                if obj.contains_key("cwd") {
                    obj.insert("cwd".into(), json!(cwd_str));
                }
            }
            lines.push(record);
            continue;
        }
        // Cross-harness kind-aware records: a compaction pair, or injected context, carried into
        // Claude's own record shapes (a Claude→Claude session replays these verbatim as carriers
        // above, so this runs only when the source is another harness). Each threads normally so a
        // child's parentUuid resolves. See `docs/INTERFACE-V2.md` §4.
        if session.harness != Harness::Claude
            && matches!(
                msg.kind,
                MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
            )
        {
            let uuid = msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
            let parent_uuid = match msg.parent_id.clone() {
                Some(pid) => resolve_parent(&dropped, pid),
                None => parent.clone(),
            };
            let ts = msg
                .timestamp
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
                .unwrap_or_else(|| ts0.clone());
            let text = msg.text().unwrap_or_default();
            let rec = match msg.kind {
                MessageKind::CompactionBoundary => json!({
                    "type": "system", "subtype": "compact_boundary", "level": "info",
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": session_id, "cwd": cwd_str,
                    "timestamp": ts, "content": text,
                    "compactMetadata": { "trigger": "manual", "preTokens": 0 },
                }),
                MessageKind::CompactionSummary => json!({
                    "type": "user", "isCompactSummary": true,
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": session_id, "cwd": cwd_str,
                    "timestamp": ts, "message": { "role": "user", "content": text },
                }),
                // InjectedContext → an `attachment` whose `rendered[].content` is the injected text
                // (exactly what Claude's parser reads back as InjectedContext).
                _ => json!({
                    "type": "attachment",
                    "uuid": uuid, "parentUuid": parent_uuid, "sessionId": session_id, "cwd": cwd_str,
                    "timestamp": ts,
                    "attachment": { "type": "injected_context" },
                    "rendered": [{ "content": text }],
                }),
            };
            lines.push(rec);
            parent = Some(uuid);
            continue;
        }
        // Claude transcripts have no standalone system turn; skip to avoid polluting user text. (A
        // *carrier* System message was already handled above; this is a synthetic/rendered one —
        // under `ParseOptions::full`, `system` records parse into these.) Its uuid leaves the file,
        // so remember where its children should re-link to.
        if msg.role == Role::System {
            if let Some(id) = &msg.id {
                let p = match msg.parent_id.clone() {
                    Some(pid) => resolve_parent(&dropped, pid),
                    None => parent.clone(),
                };
                dropped.insert(id.clone(), p);
            }
            continue;
        }
        // Preserve the source uuid/parentUuid/timestamp when the message carries them (a
        // format-complete round-trip), else generate fresh threading (cross-harness convert/port).
        // A source parentUuid pointing at a dropped system record re-links to its survivor.
        let uuid = msg.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
        let parent_uuid = match msg.parent_id.clone() {
            Some(pid) => resolve_parent(&dropped, pid),
            None => parent.clone(),
        };
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts0.clone());

        // Common envelope fields present on every threaded line.
        let mut line = Map::new();
        line.insert("uuid".into(), json!(uuid));
        line.insert("parentUuid".into(), json!(parent_uuid));
        line.insert("sessionId".into(), json!(session_id));
        line.insert("cwd".into(), json!(cwd_str));
        line.insert("timestamp".into(), json!(ts));
        if let Some(b) = &branch {
            line.insert("gitBranch".into(), json!(b));
        }

        match msg.role {
            Role::User => {
                line.insert("type".into(), json!("user"));
                // Text-only user turn → Claude's native bare-string content. Anything richer
                // (images, document/File attachments, mixed content) → the array-of-blocks form
                // Claude also writes; flattening to `msg.text()` dropped every non-text block.
                let content = if msg.content.iter().all(|b| matches!(b, Block::Text { .. })) {
                    json!(msg.text().unwrap_or_default())
                } else {
                    Value::Array(claude_user_blocks(&msg.content))
                };
                line.insert("message".into(), json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                line.insert("type".into(), json!("assistant"));
                let blocks = claude_assistant_blocks(&msg.content);
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    message.insert("model".into(), json!(model));
                }
                // Reconstruct the API `usage` block from the first-classed [`Usage`] (the parser
                // pulls it out of `message.usage`), so the token counts survive. This is NOT
                // same-harness-only: `message.usage` is Claude's own slot for exactly these four
                // numbers, and every harness that records usage records the same ones, so a Codex
                // or OpenCode session ported here keeps them too (it used to read back
                // `usage: 2032 → 0`). A Claude→Claude replay is byte-identical either way —
                // `usage` is first-classed out of the record (`CLAUDE_FIRSTCLASS_MSG`), so this is
                // the only thing that writes it and the key order is unchanged.
                if let Some(usage) = claude_usage(msg.usage.as_ref()) {
                    message.insert("usage".into(), usage);
                }
                message.insert("content".into(), Value::Array(blocks));
                line.insert("message".into(), Value::Object(message));
            }
            Role::Tool => {
                // Tool results ride on a `user` line whose content is tool_result blocks.
                let blocks = claude_tool_result_blocks(&msg.content);
                line.insert("type".into(), json!("user"));
                line.insert(
                    "message".into(),
                    json!({ "role": "user", "content": Value::Array(blocks) }),
                );
            }
            Role::System => unreachable!("system turns are carried/skipped above"),
        }

        // A tool result's structured `details` → Claude's `toolUseResult` sidecar: the slot Claude
        // itself writes it in and reads it back from. INTERFACE-V2 §4 puts that sidecar on the block,
        // so `details` is the source of truth for EVERY source harness — a Claude→Claude replay and
        // a cross-harness port alike (foreign details survive rather than vanishing). The
        // complete-mode bag copy applied below is only the fallback, for a message whose block-level
        // details were rewritten or dropped between parse and emit.
        if let Some(tur) = claude_tool_use_result(&msg.content) {
            line.insert(crate::harness::claude::TOOL_USE_RESULT_KEY.into(), tur);
        }

        // Format-complete / same-harness port: fold every captured replay field back onto the
        // record — top-level keys directly, `message.<k>` keys inside the `message` object — so the
        // emitted line carries the same key set + values as the source. (`requestId`, `version`,
        // `userType`, `message.id`, `message.stop_reason`, … all ride here; `toolUseResult` came
        // from the block's `details` above and is not overwritten by the bag's copy of it.)
        // These live in the Claude fact bag (`extra["claude"]`, IR-v2 nesting), so read from there;
        // internal sidecar keys (lazy offset stamp, the carrier key) are never emitted. Only for a
        // Claude→Claude session: a foreign source's bag holds *that* harness's fields, which aren't
        // valid Claude record keys, so cross-harness convert keeps its prior lean output.
        if session.harness == Harness::Claude {
            if let Some(bag) = msg.harness_extra(Harness::Claude) {
                apply_claude_extra(&mut line, bag);
            }
        }

        lines.push(Value::Object(line));
        parent = Some(uuid);
    }

    make_resumable(&mut lines, agent.as_deref());
    write_jsonl(&file_path, &lines)?;

    let resume = match (&agent, &cwd) {
        (Some(a), _) => format!("from session {session_id}: SendMessage to=\"{a}\" with the next instruction"),
        (None, Some(c)) => format!("claude --resume {new_id}  (run from {})", c.display()),
        (None, None) => format!("claude --resume {new_id}"),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}

/// The content blocks of a threaded record, if it has any.
fn record_blocks(v: &Value) -> Option<&Vec<Value>> {
    v.pointer("/message/content").and_then(Value::as_array)
}

/// The resumability rules Claude Code enforces on a transcript, applied once to the records the
/// emitter is about to write — so every IR→Claude door (port, distill, splice, pack) gets them
/// without knowing they exist, and no selector has to re-thread, close calls or strip markers
/// itself. Three rules, in this order:
///
/// 1. **No orphan `tool_use`.** A call whose `tool_result` is not in the file (the source stopped
///    mid-call, or the selector cut between them) is dropped; an assistant record left with no
///    content goes with it. The API rejects a resume with an unanswered call.
/// 2. **No dangling `parentUuid`.** Claude Code resumes by walking `parentUuid` back from the newest
///    record and stops silently at the first missing link, so a parent that names a record not in
///    the file (a dropped turn, a hook attachment the IR never carried, a span cut) is re-pointed at
///    the previous record. A parent that resolves is kept, so a source's branches survive.
/// 3. **One identity.** A sub-agent transcript carries `isSidechain: true` and its `agentId` on
///    every record; a main-thread session carries neither (`isSidechain` false where present), so a
///    span lifted from a sub-agent joins the thread instead of being skipped by the loader.
///
/// [`verify_claude_resumable`] checks the same three on a written file.
fn make_resumable(lines: &mut Vec<Value>, agent: Option<&str>) {
    // 1. Orphan calls.
    let answered: std::collections::HashSet<String> = lines
        .iter()
        .filter_map(record_blocks)
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    lines.retain_mut(|v| {
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return true;
        }
        let Some(blocks) = v.pointer_mut("/message/content").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = blocks.len();
        blocks.retain(|b| {
            b.get("type").and_then(Value::as_str) != Some("tool_use")
                || b.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| answered.contains(id))
        });
        before == 0 || !blocks.is_empty()
    });
    // 2. Dangling parents.
    let uuids: std::collections::HashSet<String> = lines
        .iter()
        .filter_map(|v| v.get("uuid").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut prev: Option<String> = None;
    for v in lines.iter_mut() {
        if let Some(p) = v.get("parentUuid").and_then(Value::as_str) {
            if !uuids.contains(p) {
                v["parentUuid"] = prev.clone().map(Value::String).unwrap_or(Value::Null);
            }
        }
        if let Some(u) = v.get("uuid").and_then(Value::as_str) {
            prev = Some(u.to_string());
        }
    }
    // 3. Identity.
    if agent.is_some() {
        lines.retain(|v| v.get("type").and_then(Value::as_str) != Some("ai-title"));
    }
    for v in lines.iter_mut() {
        let Some(o) = v.as_object_mut() else { continue };
        match agent {
            Some(a) => {
                o.insert("isSidechain".into(), Value::Bool(true));
                o.insert("agentId".into(), json!(a));
            }
            None => {
                if o.contains_key("isSidechain") {
                    o.insert("isSidechain".into(), Value::Bool(false));
                }
                o.shift_remove("agentId");
            }
        }
    }
}

/// Check a written Claude transcript against the three rules [`make_resumable`] establishes: no
/// dangling `parentUuid`, no `tool_use` without its `tool_result`, and one identity (`agent`
/// `None`: no record with `isSidechain: true`; `Some`: every record `isSidechain: true` with that
/// `agentId`). The post-emit assertion [`emit_verified`] runs on every Claude emit; an error here
/// is a bug in cv, never in the input.
pub fn verify_claude_resumable(path: &Path, agent: Option<&str>) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).with_context(|| format!("{}: not a JSON record: {l}", path.display())))
        .collect::<Result<_>>()?;
    let uuids: std::collections::HashSet<&str> = lines
        .iter()
        .filter_map(|v| v.get("uuid").and_then(Value::as_str))
        .collect();
    let answered: std::collections::HashSet<&str> = lines
        .iter()
        .filter_map(record_blocks)
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
        .collect();
    for (i, v) in lines.iter().enumerate() {
        let n = i + 1;
        if let Some(p) = v.get("parentUuid").and_then(Value::as_str) {
            if !uuids.contains(p) {
                anyhow::bail!("{}:{n}: parentUuid {p} names no record in the file", path.display());
            }
        }
        for b in record_blocks(v).into_iter().flatten() {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                if !answered.contains(id) {
                    anyhow::bail!("{}:{n}: tool_use {id} has no tool_result", path.display());
                }
            }
        }
        let side = v.get("isSidechain").and_then(Value::as_bool);
        let aid = v.get("agentId").and_then(Value::as_str);
        match agent {
            None if side == Some(true) || aid.is_some() => {
                anyhow::bail!(
                    "{}:{n}: a main-thread session carries a sidechain marker",
                    path.display()
                )
            }
            Some(a) if side != Some(true) || aid != Some(a) => anyhow::bail!(
                "{}:{n}: a sub-agent record must carry isSidechain:true and agentId {a:?}",
                path.display()
            ),
            _ => {}
        }
    }
    Ok(())
}

/// The verbatim meta record a format-complete carrier message stashed (an object under
/// `extra["_record"]`), or `None` for an ordinary message. See
/// [`claude::carrier_record`](crate::harness::claude::CARRIER_KEY).
fn carrier_record_of(m: &crate::ir::Message) -> Option<&Value> {
    m.extra
        .get(crate::harness::claude::CARRIER_KEY)
        .filter(|v| v.is_object())
}

/// Internal `extra` keys the IR adds for its own bookkeeping — never part of the source record, so
/// they must NOT be replayed into an emitted Claude line.
fn is_internal_extra_key(k: &str) -> bool {
    k == crate::harness::claude::CARRIER_KEY || k == crate::offsets::OFFSET_KEY
}

/// Fold a message's captured `extra` back onto an emitted Claude record (format-complete replay): a
/// `message.<k>` key is routed inside the `message` object; any other key is a top-level field. The
/// captured value is the source-of-truth for that key (it IS the original record's value), so it
/// overrides the envelope reconstruction — e.g. the original `gitBranch`/`version` win over the
/// emit-time params. Two exceptions: `sessionId` and `cwd` are the session's *identity*, and the
/// envelope (which carries the possibly-rehomed values from `EmitOptions::new_id`/`new_cwd`) must
/// stay authoritative — replaying the captured ones would stamp a ported session with its OLD
/// id/cwd, breaking `--new-cwd` porting and `claude --resume` (which keys off both). On a pure
/// round-trip the envelope equals the captured values anyway, so nothing changes there. The
/// IR-first-classed keys (uuid/parentUuid/timestamp/type and `message.content`) are never present
/// in `extra` (the parser excludes them), so they're untouched and the [`Message`]'s own fields
/// remain authoritative.
fn apply_claude_extra(line: &mut Map<String, Value>, extra: &Map<String, Value>) {
    use crate::harness::claude::MESSAGE_EXTRA_PREFIX;
    for (k, val) in extra {
        if is_internal_extra_key(k) || k == "sessionId" || k == "cwd" {
            continue;
        }
        // `toolUseResult` was already rebuilt from the block's `details` (the IR's home for it);
        // the bag's verbatim complete-mode copy is only the fallback, so it must not clobber it.
        if k == crate::harness::claude::TOOL_USE_RESULT_KEY && line.contains_key(k) {
            continue;
        }
        if let Some(sub) = k.strip_prefix(MESSAGE_EXTRA_PREFIX) {
            let msg = line
                .entry("message".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(mobj) = msg.as_object_mut() {
                mobj.insert(sub.to_string(), val.clone());
            }
        } else {
            line.insert(k.clone(), val.clone());
        }
    }
}

/// Reconstruct Claude's `message.usage` object from the IR [`Usage`], reversing
/// [`claude::parse_usage`]'s field renames (`cache_read_input_tokens` ↔ `cache_read_tokens`,
/// `cache_creation_input_tokens` ↔ `cache_creation_tokens`). Emits only the fields actually present,
/// so a source that carried just `input_tokens`/`output_tokens` round-trips with the same key set.
/// `None` when there's no usage at all (an assistant turn the source had no `usage` block on).
fn claude_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    if let Some(x) = u.input_tokens {
        m.insert("input_tokens".into(), json!(x));
    }
    if let Some(x) = u.output_tokens {
        m.insert("output_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_read_tokens {
        m.insert("cache_read_input_tokens".into(), json!(x));
    }
    if let Some(x) = u.cache_creation_tokens {
        m.insert("cache_creation_input_tokens".into(), json!(x));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// An assistant line's content: the Anthropic blocks, never empty (an empty turn is one empty
/// text block, as Claude Code writes it).
fn claude_assistant_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = anthropic::assistant_blocks(content, &anthropic::ANTHROPIC);
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

/// A user line's content: text/image natively, a [`Block::File`] as the `document` attachment
/// the parser reads back into a File, a stray tool result kept as a `tool_result` block.
fn claude_user_blocks(content: &[Block]) -> Vec<Value> {
    anthropic::user_blocks(content, &anthropic::ANTHROPIC)
}

fn claude_tool_result_blocks(content: &[Block]) -> Vec<Value> {
    anthropic::tool_result_blocks(content)
}

/// A record's `toolUseResult` sidecar, rebuilt from the message's tool-result `details`
/// (INTERFACE-V2 §4: a tool result's structured extras live on the block, so `details` is where
/// both a Claude replay and a cross-harness port read them from).
///
/// Two adjustments reverse what `claude::attach_tool_use_result` did on the way in: a sidecar that
/// was not an object is parked under `details["toolUseResult"]` and
/// comes back out verbatim, and cv's derived `persistedOutput` pointer is stripped (it is
/// re-derived from the transcript stub on every parse and was never a Claude wire field — replaying
/// it would invent a key the source record never had). `None` when the block carried nothing but
/// that pointer.
fn claude_tool_use_result(content: &[Block]) -> Option<Value> {
    let details = content.iter().find_map(|b| match b {
        Block::ToolResult { details: Some(d), .. } => Some(d),
        _ => None,
    })?;
    let Value::Object(map) = details else {
        return Some(details.clone());
    };
    if let Some(parked) = map.get(crate::harness::claude::TOOL_USE_RESULT_KEY) {
        return Some(parked.clone());
    }
    let mut out = map.clone();
    out.remove(crate::harness::claude::PERSISTED_OUTPUT_KEY);
    (!out.is_empty()).then(|| Value::Object(out))
}

// ------------------------------------------------------------------------------------------------
// shared by the emitters (each lives beside its reader: `harness/<h>/emit.rs`)
// ------------------------------------------------------------------------------------------------
