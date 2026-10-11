//! Kimi CLI (MoonshotAI) adapter — `~/.kimi/sessions/<md5(cwd)>/<session-uuid>/context.jsonl`.
//!
//! ## Storage layout
//! Root is `$KIMI_SHARE_DIR` or `~/.kimi`. Each work directory maps to a project dir whose name is
//! **`md5(cwd_utf8).hexdigest()`** (lowercase hex), or `"<kaos>_<md5>"` when the KAOS isn't `local`.
//! The cwd is NOT stored in the transcript; we recover it from `~/.kimi/kimi.json`'s `work_dirs[]`
//! (`{path, kaos, last_session_id}`) by matching `md5(path) == <hash>`.
//!
//! Two on-disk shapes (sniffed, never via a version field):
//! - **Modern (dir form):** `sessions/<hash>/<uuid>/context.jsonl` plus a `wire.jsonl` sidecar
//!   (timestamps + token usage), optional `state.json` (`custom_title`), and rotated-out segments
//!   `context_1.jsonl…context_N.jsonl` (we read the live `context.jsonl`, recording the count of
//!   segments in `extra["kimi"]["compaction_segments"]`).
//! - **Legacy (flat form):** `sessions/<hash>/<uuid>.jsonl` — the transcript only, no sidecar/dir.
//!   (kimi-cli migrates these to the dir form on next open; we still read them in place.)
//!
//! ## `context.jsonl` records (one JSON object per line) → kind / origin
//! Ground truth is kimi-cli's `soul/context.py` (the writer) and `soul/kimisoul.py` (the loop).
//! - `{"role":"_system_prompt","content":str}` → `SystemPrompt`/Harness, and `Session::system_prompt`
//!   (`Context.write_system_prompt`, the first record of every segment).
//! - `{"role":"_checkpoint","id":N}` → a per-step rewind target (`KimiSoul._checkpoint()` runs at the
//!   top of EVERY step, `kimisoul.py:1036`; `Context.revert_to(id)` truncates back to it). It marks
//!   nothing the reader needs, so it is a `Carrier` under [`ParseOptions::complete`] and dropped
//!   otherwise — NOT a `Branch` and NOT a `CompactionBoundary`: the segment rotation
//!   (`context.jsonl` → `context_N.jsonl`) is done by `revert_to` (rewind) and `clear` (compaction,
//!   `kimisoul.py:1577`), never by a checkpoint.
//! - `{"role":"_usage","token_count":N}` → not a message: the request input-token count kimi-cli
//!   logs after a step (`update_token_count(usage.input)`, `kimisoul.py:1267`). It lands on the
//!   preceding message as `extra["kimi"]["token_count"]`, and — on a `Reply` the wire sidecar gave
//!   no usage for — as `usage.input_tokens`.
//! - `{"role":"user","content": str | [Part]}` → `Prompt`/Human, unless kimi-cli's own markers say
//!   the harness wrote it: a first text part starting `<system>Previous context has been compacted`
//!   (`soul/compaction.py::COMPACTION_OUTPUT_PREFIX`) is the `CompactionSummary` that seeds the
//!   rotated segment — preceded by a synthetic `CompactionBoundary`, since the boundary itself is
//!   the file rotation; `<notification …>` (`notifications/llm.py`) is `InjectedContext`/Scheduler;
//!   `<system>…</system>` / `<system-reminder>` (`soul/message.py::system[_reminder]`) is
//!   `InjectedContext`/Harness. `content` is a BARE STRING for a single text part, else a list.
//! - `{"role":"assistant","content":[Part],"tool_calls":[…]}` → `Reply`/Model.
//! - `{"role":"tool","content": str | [Part],"tool_call_id":id}` → `ToolResult`/Harness (one
//!   ToolResult block, `tool_name` resolved from the call). The first text part is often a
//!   `<system>summary</system>` line.
//!
//! ### Part (`type`-tagged)
//! - `text{text}` → Text.
//! - `think{think,encrypted}` → Thinking{ text: think, encrypted }.
//! - `image_url{image_url:{url,id}}` → Image (data_ref = url).
//! - `audio_url{…}` / `video_url{…}` → File (source = url). (Media URLs nest under a key named after
//!   the type.)
//!
//! ### tool_calls[]
//! `{type:"function", id, function:{name, arguments}}` → ToolUse{id,name, input}. `arguments` is a
//! JSON-encoded *string*; we parse it (null/empty ⇒ `{}`).
//!
//! ## wire.jsonl sidecar (optional enrichment, like grok's `updates.jsonl`)
//! `{"type":"metadata","protocol_version":"1.x"}` header, then per-event lines
//! `{"timestamp": epoch_float, "message": {"type": …, "payload": …}}`. We mine:
//! - `ToolResult.payload.return_value{is_error,output,message,display,extras}` → richer ToolResult.
//! - `StatusUpdate.payload.token_usage{input_other,output,input_cache_read,input_cache_creation}`
//!   and `message_id` (chatcmpl-…) → attached to the most recent assistant message.
//! - `TurnBegin`/event timestamps → first/last session timestamps when no file mtime is better.
//!
//! We tolerate protocol_version drift (1.1–1.9) and unknown message types: anything we don't
//! recognize is ignored.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub struct Kimi {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
    /// The share root itself (`~/.kimi`), for locating `kimi.json` / `config.toml`.
    root: Option<PathBuf>,
}

impl Kimi {
    pub fn new() -> Self {
        let root = std::env::var_os("KIMI_SHARE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".kimi")))
            .filter(|p| p.exists());
        let sessions = root.as_ref().map(|r| r.join("sessions")).filter(|p| p.exists());
        Kimi { sessions, root }
    }
}

impl Default for Kimi {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Kimi {
    fn harness(&self) -> Harness {
        Harness::Kimi
    }

    fn storage_root(&self) -> Option<PathBuf> {
        // The share root (`~/.kimi`), NOT `<root>/sessions`: `kimi.json` (the cwd registry) lives
        // at this root beside `sessions/`, so the root is what a consumer needs.
        // (Returning `sessions` here produced `~/.kimi/sessions/sessions/...`, which kimi-cli never
        // discovers.) Fall back to `<sessions>/..` if only the sessions dir was detected.
        self.root
            .clone()
            .or_else(|| self.sessions.as_ref().and_then(|s| s.parent().map(Path::to_path_buf)))
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };
        // hash (dir basename, possibly "<kaos>_<md5>") -> cwd, from kimi.json.
        let hash_to_cwd = self
            .root
            .as_ref()
            .map(|r| load_work_dirs(&r.join("kimi.json")))
            .unwrap_or_default();
        let default_model = self
            .root
            .as_ref()
            .and_then(|r| default_model_from_config(&r.join("config.toml")));

        let mut out = Vec::new();
        let Ok(hash_dirs) = fs::read_dir(sessions) else {
            return Ok(out);
        };
        for hd in hash_dirs.filter_map(|e| e.ok()) {
            let hash_path = hd.path();
            let Some(basename) = hash_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !hash_path.is_dir() {
                continue;
            }
            let cwd = hash_to_cwd.get(basename).cloned();
            let Ok(entries) = fs::read_dir(&hash_path) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                let (id, transcript, dir): (String, PathBuf, PathBuf) = if p.is_dir() {
                    // Modern: <uuid>/context.jsonl
                    let ctx = p.join("context.jsonl");
                    if !ctx.exists() {
                        continue;
                    }
                    (name.to_string(), ctx, p.clone())
                } else if name.ends_with(".jsonl") {
                    // Legacy: <uuid>.jsonl (flat). source dir is the hash dir.
                    let id = name.trim_end_matches(".jsonl").to_string();
                    (id, p.clone(), hash_path.clone())
                } else {
                    continue;
                };
                if let Some(r) = scan(&id, &transcript, &dir, cwd.clone(), default_model.clone()) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // r.path is the session dir (modern) or the hash dir (legacy). Locate the transcript.
        let (transcript, wire) = locate_transcript(&r.path, &r.id);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Kimi,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            model: self
                .root
                .as_ref()
                .and_then(|root| default_model_from_config(&root.join("config.toml"))),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // Sidecar enrichment from wire.jsonl (tolerant: absent/empty/malformed ⇒ empty). The
        // per-message `tool_results` map (keyed by tool_call_id) is a per-line enrichment; the
        // positional passes (StatusUpdate usage/message_id zipped onto assistant turns, the first
        // wire timestamp stamped onto the first user turn) only ever walk *forward* with O(1)
        // state, so they run inline here too — `parse()` is exactly `collect(stream())`.
        let enrich = wire.as_ref().map(read_wire_enrichment).unwrap_or_default();

        // Note archived compaction segments (context_1.jsonl…), if any, for fidelity bookkeeping.
        let seg_count = count_segments(&r.path);
        if seg_count > 0 {
            s.harness_extra_mut(Harness::Kimi)
                .insert("compaction_segments".into(), Value::Number(seg_count.into()));
        }
        // The system prompt is the first `context.jsonl` record; peek so the session-level copy is
        // known before the body streams.
        s.system_prompt = peek_system_prompt(&transcript);

        // Session metadata is known up front; hand it to the sink before the body.
        sink.meta(&s);

        // Stream the transcript line-by-line (a single session can be large; `read_to_string` would
        // resident-spike the whole file). Each record becomes at most one Message, emitted to `sink`
        // and dropped before the next line, so peak memory is O(largest message).
        let Ok(file) = fs::File::open(&transcript) else {
            return Ok(s);
        };
        // Positional wire enrichment, streamed: one StatusUpdate per assistant step (in wire
        // order) and the first wire timestamp for the first user turn. Forward-only, O(1) state.
        let mut statuses = enrich.statuses.iter();
        let first_wire_ts = enrich.msg_timestamps.first().copied();
        let mut first_user_seen = false;
        // `tool_call_id` → tool name from the assistant's `tool_calls`, so a result names its tool.
        let mut call_names: HashMap<String, String> = HashMap::new();
        let complete = opts.complete;
        // One-message lookahead: a `_usage` record describes the message BEFORE it, so each message
        // waits here until the next record shows what follows. Peak memory stays O(one message).
        let mut pending: Option<Message> = None;
        let mut stopped = false;
        let skipped = super::for_each_json_line(BufReader::new(file), |v| {
            if v.get("role").and_then(Value::as_str) == Some("_usage") {
                if let Some(p) = pending.as_mut() {
                    fold_usage_record(p, &v);
                }
                return Flow::Continue;
            }
            let Some(mut m) = context_message(&v, &enrich, complete, &mut call_names) else {
                return Flow::Continue;
            };
            match m.role {
                Role::Assistant => {
                    if let Some(st) = statuses.next() {
                        if m.usage.is_none() {
                            m.usage = st.usage.clone();
                        }
                        if m.id.is_none() {
                            m.id = st.message_id.clone();
                        }
                    }
                }
                Role::User if !first_user_seen => {
                    first_user_seen = true;
                    if m.timestamp.is_none() {
                        m.timestamp = first_wire_ts;
                    }
                }
                _ => {}
            }
            if let Some(prev) = pending.take() {
                if sink.message(prev) == Flow::Stop {
                    stopped = true;
                    return Flow::Stop;
                }
            }
            // The compaction summary seeds a rotated segment; the boundary itself is the rotation,
            // which leaves no record — synthesize it so the compaction detector can pair the two.
            if m.kind == MessageKind::CompactionSummary && sink.message(compaction_boundary(m.timestamp)) == Flow::Stop
            {
                stopped = true;
                return Flow::Stop;
            }
            pending = Some(m);
            Flow::Continue
        });
        if !stopped {
            if let Some(last) = pending.take() {
                sink.message(last);
            }
        }
        super::note_skipped_lines(&mut s, skipped);

        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Discovery helpers
// ---------------------------------------------------------------------------

/// Parse `kimi.json` into a map of project-dir basename → cwd. The basename is `md5(path)` for the
/// `local` KAOS, else `"<kaos>_<md5>"`, matching kimi-cli's `WorkDirMeta.sessions_dir`.
fn load_work_dirs(kimi_json: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(kimi_json) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    let Some(dirs) = v.get("work_dirs").and_then(Value::as_array) else {
        return map;
    };
    for wd in dirs {
        let Some(path) = wd.get("path").and_then(Value::as_str) else {
            continue;
        };
        let kaos = wd.get("kaos").and_then(Value::as_str).unwrap_or("local");
        let hash = md5_hex(path.as_bytes());
        let basename = if kaos == "local" {
            hash
        } else {
            format!("{kaos}_{hash}")
        };
        map.insert(basename, PathBuf::from(path));
    }
    map
}

/// `default_model` from `config.toml`. We don't pull in a TOML parser for a single scalar: scan for
/// the top-level `default_model = "…"` line.
fn default_model_from_config(config_toml: &Path) -> Option<String> {
    let text = fs::read_to_string(config_toml).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("default_model") {
            let rest = rest.trim_start().strip_prefix('=')?.trim();
            let val = rest.trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Cheap scan of one transcript into a `SessionRef`. `dir` is the session's source dir (the
/// `<uuid>/` dir for modern, the hash dir for legacy). Returns `None` only on a fully unreadable
/// transcript path that doesn't exist.
fn scan(
    id: &str,
    transcript: &Path,
    dir: &Path,
    cwd: Option<PathBuf>,
    _default_model: Option<String>,
) -> Option<SessionRef> {
    let meta = fs::metadata(transcript).ok()?;
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = read_custom_title(dir);
    let mut message_count = 0usize;
    let text = fs::read_to_string(transcript).unwrap_or_default();
    super::for_each_json_line_str(&text, |v| {
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        // SessionRef contract: user + assistant turns only (no tool / _system_prompt records).
        if matches!(role, "user" | "assistant") {
            message_count += 1;
        }
        if title.is_none() && role == "user" {
            let t = coerce_content_text(v.get("content"));
            if !t.trim().is_empty() {
                title = Some(crate::ir::truncate(&t, 80));
            }
        }
        Flow::Continue
    });

    // wire gives nicer first/last timestamps when present.
    let wire = wire_path_for(dir, transcript);
    let (created, updated) = wire
        .as_ref()
        .and_then(|w| wire_time_bounds(w))
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((mtime, mtime));

    Some(SessionRef {
        id: id.to_string(),
        harness: Harness::Kimi,
        path: dir.to_path_buf(),
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count,
    })
}

/// `state.json.custom_title`, when present and non-empty.
fn read_custom_title(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("custom_title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80))
}

/// Given a `SessionRef.path` and id, find the transcript + optional wire sidecar.
fn locate_transcript(dir: &Path, id: &str) -> (PathBuf, Option<PathBuf>) {
    let modern = dir.join("context.jsonl");
    if modern.exists() {
        let wire = dir.join("wire.jsonl");
        return (modern, wire.exists().then_some(wire));
    }
    // Legacy flat: dir is the hash dir, transcript is <id>.jsonl, no wire.
    let flat = dir.join(format!("{id}.jsonl"));
    (flat, None)
}

/// Locate the wire sidecar for a transcript, if it lives next to it (modern only).
fn wire_path_for(dir: &Path, transcript: &Path) -> Option<PathBuf> {
    if transcript.file_name().and_then(|n| n.to_str()) == Some("context.jsonl") {
        let w = dir.join("wire.jsonl");
        return w.exists().then_some(w);
    }
    None
}

/// Count archived pre-compaction segments `context_1.jsonl…` next to a modern transcript.
fn count_segments(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("context_") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .count() as u64
}

// ---------------------------------------------------------------------------
// context.jsonl → IR
// ---------------------------------------------------------------------------

/// The system prompt from the head of `context.jsonl` (kimi-cli writes it as the first record,
/// `{"role":"_system_prompt", …}`; `soul/context.py:99`). Bounded to the first few lines.
fn peek_system_prompt(transcript: &Path) -> Option<String> {
    let file = fs::File::open(transcript).ok()?;
    let mut prompt = None;
    let mut seen = 0usize;
    super::for_each_json_line(BufReader::new(file), |v| {
        seen += 1;
        if v.get("role").and_then(Value::as_str) == Some("_system_prompt") {
            let text = coerce_content_text(v.get("content"));
            if !text.is_empty() {
                prompt = Some(text);
            }
            return Flow::Stop;
        }
        if seen >= 4 {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    prompt
}

/// Convert one `context.jsonl` record into a `Message`, or `None` to skip it (`_usage` is always
/// `None` here: [`Kimi::stream`] folds it onto the preceding message with [`fold_usage_record`]).
///
/// `_checkpoint` records are kimi-cli's numbered rewind targets (`Context.checkpoint()` /
/// `revert_to()`, `soul/context.py:123-200`), written at the top of every step
/// (`kimisoul.py:1036`): no content, and the conversation continues straight through them. Only a
/// later `revert_to` makes one matter, and that rewrites the file (rotating the old one to
/// `context_N.jsonl`) — so a checkpoint still present in the live transcript is one that was never
/// reverted to. Bookkeeping: a `Carrier` under `complete`, dropped otherwise; not a `Branch`, not a
/// `CompactionBoundary` (see the module doc for where those come from).
fn context_message(
    v: &Value,
    enrich: &WireEnrich,
    complete: bool,
    call_names: &mut HashMap<String, String>,
) -> Option<Message> {
    let role = v.get("role").and_then(Value::as_str)?;
    match role {
        "_usage" => None,
        "_checkpoint" => {
            if !complete {
                return None;
            }
            let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
            m.extra.insert(crate::harness::claude::CARRIER_KEY.into(), v.clone());
            m.harness_extra_mut(Harness::Kimi)
                .insert("record_type".into(), Value::String(role.to_string()));
            Some(m)
        }
        "_system_prompt" => {
            let text = coerce_content_text(v.get("content"));
            let mut m = Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness);
            m.content.push(Block::Text { text: text.into() });
            Some(m)
        }
        "user" => {
            let mut m = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
            push_parts(&mut m, v.get("content"));
            classify_user(&mut m);
            (!m.content.is_empty()).then_some(m)
        }
        "assistant" => {
            let mut m = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
            push_parts(&mut m, v.get("content"));
            if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let func = c.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input = parse_arguments(func.and_then(|f| f.get("arguments")));
                    if !id.is_empty() && !name.is_empty() {
                        call_names.insert(id.clone(), name.clone());
                    }
                    m.content.push(Block::ToolUse {
                        id,
                        name,
                        input,
                        namespace: None,
                    });
                }
            }
            (!m.content.is_empty()).then_some(m)
        }
        "tool" => {
            let tool_use_id = v.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
            // Prefer the richer wire return_value if we have it; else the context text parts.
            let we = enrich.tool_results.get(&tool_use_id);
            let (content, is_error, details) = match we {
                Some(tr) => (tr.output.clone(), tr.is_error, tr.extras.clone()),
                None => (coerce_content_text(v.get("content")), false, None),
            };
            let status = Some(if is_error { "error" } else { "completed" }.to_string());
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content.push(Block::ToolResult {
                tool_name: call_names.get(&tool_use_id).cloned(),
                tool_use_id,
                content: content.into(),
                is_error,
                status,
                details,
            });
            Some(m)
        }
        _ => None,
    }
}

/// `soul/compaction.py::COMPACTION_OUTPUT_PREFIX`, as `soul/message.py::system()` wraps it: the first
/// text part of the user message that seeds a rotated segment.
const COMPACTION_SUMMARY_TAG: &str = "<system>Previous context has been compacted.";

/// Refine a `user` record's kind/origin from kimi-cli's own text markers (module doc). A typed
/// prompt never starts with these tags; the harness's `system()` / `system_reminder()` helpers and
/// `build_notification_message` always do.
fn classify_user(m: &mut Message) {
    let Some(Block::Text { text }) = m.content.first() else {
        return;
    };
    let head = text.trim_start();
    let (kind, origin) = if head.starts_with(COMPACTION_SUMMARY_TAG) {
        (MessageKind::CompactionSummary, Origin::Harness)
    } else if head.starts_with("<notification") {
        (MessageKind::InjectedContext, Origin::Scheduler)
    } else if head.starts_with("<system>") || head.starts_with("<system-reminder>") {
        (MessageKind::InjectedContext, Origin::Harness)
    } else {
        return;
    };
    m.kind = kind;
    m.origin = origin;
}

/// Fold a `{"role":"_usage","token_count":N}` record onto the message it follows: always as
/// `extra["kimi"]["token_count"]` (the record's own fact), and as `usage.input_tokens`
/// on a `Reply` the wire sidecar gave no usage for (`token_count` is the request's input tokens,
/// `kimisoul.py:1267`).
fn fold_usage_record(m: &mut Message, v: &Value) {
    let Some(n) = v.get("token_count").filter(|n| n.is_number()) else {
        return;
    };
    m.harness_extra_mut(Harness::Kimi)
        .insert("token_count".into(), n.clone());
    if m.kind == MessageKind::Reply && m.usage.is_none() {
        m.usage = Some(Usage {
            input_tokens: n.as_u64(),
            ..Usage::default()
        });
    }
}

/// The synthetic boundary emitted before a `CompactionSummary`: kimi-cli's compaction rotates the
/// transcript (`Context.clear`, `kimisoul.py:1577`) and leaves no record of its own, so the
/// summary is the only trace — and the compaction detector pairs a summary with the boundary
/// before it.
fn compaction_boundary(ts: Option<DateTime<Utc>>) -> Message {
    let mut m = Message::of_kind(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
    m.timestamp = ts;
    m.content.push(Block::Text {
        text: "[conversation compacted]".into(),
    });
    m.harness_extra_mut(Harness::Kimi)
        .insert("synthetic".into(), Value::Bool(true));
    m
}

/// Push a record's `content` (bare string or `[Part]`) onto a message as Blocks.
fn push_parts(m: &mut Message, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                m.content.push(Block::Text { text: s.clone().into() });
            }
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(b) = part_to_block(p) {
                    m.content.push(b);
                }
            }
        }
        _ => {}
    }
}

/// One `type`-tagged Part → a `Block`.
fn part_to_block(p: &Value) -> Option<Block> {
    let ty = p.get("type").and_then(Value::as_str)?;
    match ty {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            Some(Block::Text { text: text.into() })
        }
        "think" => {
            let text = p.get("think").and_then(Value::as_str).unwrap_or("").to_string();
            let encrypted = p.get("encrypted").and_then(Value::as_str).map(str::to_string);
            Some(Block::Thinking {
                text: text.into(),
                signature: encrypted.clone(),
                encrypted,
                redacted: false,
            })
        }
        "image_url" => {
            let url = media_url(p, "image_url");
            Some(Block::Image {
                media_type: None,
                data_ref: url,
            })
        }
        "audio_url" | "video_url" => {
            let url = media_url(p, ty);
            Some(Block::File {
                mime: None,
                path: None,
                source: url,
            })
        }
        _ => None,
    }
}

/// Media parts nest `{url, id}` under a key named after the type, e.g. `image_url: {url, id}`.
fn media_url(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(|inner| inner.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `arguments` is a JSON-encoded *string*. Parse it; `null`/empty/unparseable ⇒ `{}`.
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if !s.is_empty() => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        Some(Value::Object(_)) => v.cloned().unwrap(),
        _ => Value::Object(Default::default()),
    }
}

/// Flatten a record `content` (string or `[Part]`) to plain text (text parts joined by newline).
fn coerce_content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("think") => p.get("think").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// wire.jsonl sidecar enrichment
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WireEnrich {
    /// tool_call_id → richer result from `ToolResult.return_value`.
    tool_results: HashMap<String, WireToolResult>,
    /// Per assistant step, in wire order.
    statuses: Vec<WireStatus>,
    /// Timestamps of `TurnBegin` and `ContentPart`/`ToolCall` events, in order (best-effort).
    msg_timestamps: Vec<DateTime<Utc>>,
}

struct WireToolResult {
    is_error: bool,
    output: String,
    extras: Option<Value>,
}

#[derive(Default)]
struct WireStatus {
    usage: Option<Usage>,
    message_id: Option<String>,
}

/// Parse `wire.jsonl`. Tolerant of `protocol_version` drift and unknown message types.
fn read_wire_enrichment(wire: &PathBuf) -> WireEnrich {
    let mut e = WireEnrich::default();
    let Ok(text) = fs::read_to_string(wire) else {
        return e;
    };
    super::for_each_json_line_str(&text, |v| {
        // header line: {"type":"metadata","protocol_version":"1.x"}
        if v.get("type").and_then(Value::as_str) == Some("metadata") {
            return Flow::Continue;
        }
        let Some(msg) = v.get("message") else {
            return Flow::Continue;
        };
        let mty = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = msg.get("payload");
        match mty {
            "ToolResult" => {
                if let Some(p) = payload {
                    let id = p.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let rv = p.get("return_value");
                    let is_error = rv
                        .and_then(|r| r.get("is_error"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Prefer `output`, fall back to `message`.
                    let output = rv
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_str)
                        .or_else(|| rv.and_then(|r| r.get("message")).and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let extras = rv.and_then(|r| r.get("extras")).filter(|x| !x.is_null()).cloned();
                    if !id.is_empty() {
                        e.tool_results.insert(
                            id,
                            WireToolResult {
                                is_error,
                                output,
                                extras,
                            },
                        );
                    }
                }
            }
            "StatusUpdate" => {
                if let Some(p) = payload {
                    let usage = p.get("token_usage").map(wire_usage);
                    let message_id = p.get("message_id").and_then(Value::as_str).map(str::to_string);
                    e.statuses.push(WireStatus { usage, message_id });
                }
            }
            _ => {}
        }
        // Record a timestamp for content-bearing turn boundaries.
        if matches!(mty, "TurnBegin" | "ContentPart" | "ToolCall") {
            if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
                e.msg_timestamps.push(ts);
            }
        }
        Flow::Continue
    });
    e
}

fn wire_usage(tu: &Value) -> Usage {
    let g = |k: &str| tu.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: g("input_other"),
        output_tokens: g("output"),
        cache_read_tokens: g("input_cache_read"),
        cache_creation_tokens: g("input_cache_creation"),
        reasoning_tokens: None,
        cost_usd: None,
    }
}

/// First and last event timestamps in a wire file (epoch floats), for created/updated bounds.
fn wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let text = fs::read_to_string(wire).ok()?;
    let mut first = None;
    let mut last = None;
    super::for_each_json_line_str(&text, |v| {
        if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
            if first.is_none() {
                first = Some(ts);
            }
            last = Some(ts);
        }
        Flow::Continue
    });
    Some((first?, last?))
}

fn epoch_to_dt(secs: f64) -> Option<DateTime<Utc>> {
    let nanos = (secs * 1e9).round() as i64;
    Some(DateTime::<Utc>::from_timestamp_nanos(nanos))
}

// ---------------------------------------------------------------------------
// md5 (dependency-free; only used for hashing short cwd strings)
// ---------------------------------------------------------------------------

/// Lowercase hex md5 of `data`. A small self-contained RFC 1321 implementation so we don't add a
/// crate dependency just to match kimi-cli's project-dir naming.
fn md5_hex(data: &[u8]) -> String {
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    // padding
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_reference() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        // The real cwd → project-dir hash observed in ~/.kimi.
        assert_eq!(
            md5_hex(b"/Users/ember/elide/breadstuffs"),
            "a917b40bc491ae8a7f6f508be5446785"
        );
    }

    #[test]
    fn work_dirs_map_local_and_kaos() {
        let json = r#"{"work_dirs":[
            {"path":"/Users/ember/elide/breadstuffs","kaos":"local","last_session_id":"x"},
            {"path":"/remote/proj","kaos":"box","last_session_id":null}
        ]}"#;
        let dir = write_temp("kimi.json", json);
        let map = load_work_dirs(&dir.join("kimi.json"));
        assert_eq!(
            map.get("a917b40bc491ae8a7f6f508be5446785").unwrap(),
            &PathBuf::from("/Users/ember/elide/breadstuffs")
        );
        // non-local KAOS → "<kaos>_<md5>"
        let h = md5_hex(b"/remote/proj");
        assert_eq!(map.get(&format!("box_{h}")).unwrap(), &PathBuf::from("/remote/proj"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_default_model() {
        let toml = "theme = \"dark\"\ndefault_model = \"kimi-code/kimi-for-coding\"\nfoo = 1\n";
        let dir = write_temp("config.toml", toml);
        assert_eq!(
            default_model_from_config(&dir.join("config.toml")).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_bare_string_and_list() {
        let bare: Value = serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        let m = context_message(&bare, &WireEnrich::default(), false, &mut HashMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let list: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA","id":null}}]}"#,
        )
        .unwrap();
        let m = context_message(&list, &WireEnrich::default(), false, &mut HashMap::new()).unwrap();
        assert_eq!(m.text().as_deref(), Some("hi"));
        assert!(matches!(
            m.content[1],
            Block::Image {
                ref data_ref,
                ..
            } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        ));
    }

    #[test]
    fn assistant_think_and_tool_calls() {
        let line = r#"{"role":"assistant","content":[{"type":"think","think":"reasoning","encrypted":"BLOB"},{"type":"text","text":"Let me look."}],"tool_calls":[{"type":"function","id":"tool_1","function":{"name":"Shell","arguments":"{\"command\":\"cat .gitmodules\"}"}}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let mut call_names = HashMap::new();
        let m = context_message(&v, &WireEnrich::default(), false, &mut call_names).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!((m.kind, m.origin), (MessageKind::Reply, Origin::Model));
        assert_eq!(call_names.get("tool_1").map(String::as_str), Some("Shell"));
        match &m.content[0] {
            Block::Thinking {
                text,
                encrypted,
                signature,
                ..
            } => {
                assert_eq!(text, "reasoning");
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(signature.as_deref(), Some("BLOB"));
            }
            o => panic!("expected thinking, got {o:?}"),
        }
        assert!(matches!(&m.content[1], Block::Text { text } if text == "Let me look."));
        match &m.content[2] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Shell");
                assert_eq!(input["command"], "cat .gitmodules");
            }
            o => panic!("expected tool_use, got {o:?}"),
        }
    }

    #[test]
    fn tool_result_bare_string_and_wire_enrichment() {
        // context.jsonl tool record (bare-string content), no wire → uses context text.
        let line =
            r#"{"role":"tool","content":"<system>Command executed successfully.</system>","tool_call_id":"tool_1"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let mut call_names = HashMap::from([("tool_1".to_string(), "Shell".to_string())]);
        let m = context_message(&v, &WireEnrich::default(), false, &mut call_names).unwrap();
        assert_eq!(m.role, Role::Tool);
        assert_eq!((m.kind, m.origin), (MessageKind::ToolResult, Origin::Harness));
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                status,
                tool_name,
                ..
            } => {
                assert_eq!(tool_use_id, "tool_1");
                assert!(content.contains("Command executed successfully"));
                assert!(!is_error);
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(tool_name.as_deref(), Some("Shell"), "resolved from the call");
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        // With wire enrichment, output/is_error come from return_value.
        let mut enrich = WireEnrich::default();
        enrich.tool_results.insert(
            "tool_1".into(),
            WireToolResult {
                is_error: true,
                output: "boom".into(),
                extras: Some(serde_json::json!({"k":"v"})),
            },
        );
        let m = context_message(&v, &enrich, false, &mut HashMap::new()).unwrap();
        match &m.content[0] {
            Block::ToolResult {
                content,
                is_error,
                status,
                details,
                ..
            } => {
                assert_eq!(content, "boom");
                assert!(is_error);
                assert_eq!(status.as_deref(), Some("error"));
                assert_eq!(details.as_ref().unwrap()["k"], "v");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_empty_becomes_object() {
        assert_eq!(parse_arguments(None), Value::Object(Default::default()));
        assert_eq!(
            parse_arguments(Some(&Value::String("".into()))),
            Value::Object(Default::default())
        );
        assert_eq!(
            parse_arguments(Some(&Value::String("not json".into()))),
            Value::Object(Default::default())
        );
    }

    #[test]
    fn internal_roles_skipped() {
        let cp: Value = serde_json::from_str(r#"{"role":"_checkpoint","id":3}"#).unwrap();
        assert!(context_message(&cp, &WireEnrich::default(), false, &mut HashMap::new()).is_none());
        // … but under `complete` a checkpoint rides along as a carrier naming its record type
        let c = context_message(&cp, &WireEnrich::default(), true, &mut HashMap::new()).unwrap();
        assert_eq!((c.kind, c.origin), (MessageKind::Carrier, Origin::Harness));
        assert_eq!(c.extra[crate::harness::claude::CARRIER_KEY], cp);
        assert_eq!(c.extra["kimi"]["record_type"], "_checkpoint");
        // `_usage` is never a message, even under `complete`: it folds onto the message before it
        let us: Value = serde_json::from_str(r#"{"role":"_usage","token_count":7291}"#).unwrap();
        for complete in [false, true] {
            assert!(context_message(&us, &WireEnrich::default(), complete, &mut HashMap::new()).is_none());
        }
        let sp: Value = serde_json::from_str(r#"{"role":"_system_prompt","content":"You are Kimi."}"#).unwrap();
        let m = context_message(&sp, &WireEnrich::default(), false, &mut HashMap::new()).unwrap();
        assert_eq!(m.role, Role::System);
        assert_eq!(m.kind, MessageKind::SystemPrompt);
        assert_eq!(m.origin, Origin::Harness);
        assert_eq!(m.text().as_deref(), Some("You are Kimi."));
    }

    #[test]
    fn user_markers_set_kind_and_origin() {
        let cases = [
            (
                r#"{"role":"user","content":"fix the build"}"#,
                MessageKind::Prompt,
                Origin::Human,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"<current_focus>x</current_focus>"}]}"#,
                MessageKind::CompactionSummary,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system-reminder>\nremember\n</system-reminder>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<system>CHECKPOINT 3</system>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Harness,
            ),
            (
                r#"{"role":"user","content":[{"type":"text","text":"<notification id=\"n1\" category=\"task\" type=\"task.completed\">\nTitle: done\n</notification>"}]}"#,
                MessageKind::InjectedContext,
                Origin::Scheduler,
            ),
            // a typed prompt that merely mentions a tag mid-text stays a prompt
            (
                r#"{"role":"user","content":"why does <system> appear in the log?"}"#,
                MessageKind::Prompt,
                Origin::Human,
            ),
        ];
        for (line, kind, origin) in cases {
            let v: Value = serde_json::from_str(line).unwrap();
            let m = context_message(&v, &WireEnrich::default(), false, &mut HashMap::new()).unwrap();
            assert_eq!(m.role, Role::User, "{line}");
            assert_eq!((m.kind, m.origin), (kind, origin), "{line}");
        }
    }

    /// A rotated (post-compaction) legacy segment, as `Context.clear` + `write_system_prompt` +
    /// `checkpoint` + `append_message(summary)` + `update_token_count` lay it down: the summary user
    /// record gets a synthetic boundary before it; `_usage` folds onto the message before it (as
    /// `usage.input_tokens` on a Reply, as `token_count` on anything).
    #[test]
    fn usage_folds_onto_preceding_message_and_compaction_pairs() {
        let id = "33333333-3333-3333-3333-333333333333";
        let lines = [
            r#"{"role":"_system_prompt","content":"You are Kimi."}"#,
            r#"{"role":"_checkpoint","id":0}"#,
            r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"summary body"}]}"#,
            r#"{"role":"_usage","token_count":900}"#,
            r#"{"role":"_checkpoint","id":1}"#,
            r#"{"role":"user","content":"and now?"}"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"Next."}]}"#,
            r#"{"role":"_usage","token_count":1234}"#,
        ];
        let dir = write_temp(&format!("{id}.jsonl"), &(lines.join("\n") + "\n"));
        let r = SessionRef {
            id: id.into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let adapter = Kimi {
            sessions: None,
            root: None,
        };
        let s = adapter.parse(&r).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Kimi."));
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::SystemPrompt,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Prompt,
                MessageKind::Reply,
            ],
            "{kinds:?}"
        );
        assert_eq!(s.messages[1].extra["kimi"]["synthetic"], true);
        assert_eq!(s.messages[2].extra["kimi"]["token_count"], 900);
        assert!(s.messages[2].usage.is_none(), "token_count is usage only on a Reply");
        let reply = &s.messages[4];
        assert_eq!(reply.extra["kimi"]["token_count"], 1234);
        assert_eq!(reply.usage.as_ref().and_then(|u| u.input_tokens), Some(1234));
        // the compaction detector pairs the synthetic boundary with the summary
        let comps = crate::compaction::detect_in_session(&s, true);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].summary_msg_idx, Some(2));
        assert!(comps[0].summary.as_deref().unwrap_or("").contains("summary body"));
        // every fact is in the harness bag; nothing flat
        for m in &s.messages {
            assert!(
                m.extra
                    .keys()
                    .all(|k| k == "kimi" || k == crate::harness::claude::CARRIER_KEY),
                "{:?}",
                m.extra
            );
        }
        // complete: the checkpoints ride as carriers, `_usage` still folds (the emitter re-creates it)
        let c = crate::stream::collect_with(&adapter, &r, &ParseOptions::complete()).unwrap();
        let record_types: Vec<&str> = c
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Carrier)
            .filter_map(|m| m.extra["kimi"]["record_type"].as_str())
            .collect();
        assert_eq!(record_types, ["_checkpoint", "_checkpoint"]);
    }

    #[test]
    fn stream_matches_parse_on_modern_fixture() {
        // parse() is collect(stream()); the streamed messages must carry the same wire enrichment
        // (StatusUpdate usage/message_id on assistant turns, first wire timestamp on the first user
        // turn) that the full parse reports. This was a real divergence: the positional passes used
        // to run only in parse(), so streaming consumers saw assistant turns without usage.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/11111111-1111-1111-1111-111111111111");
        if !dir.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let adapter = Kimi {
            sessions: None,
            root: None,
        };
        let parsed = adapter.parse(&r).unwrap();

        let mut sink = crate::stream::CollectSink::default();
        let mut streamed = adapter
            .stream(&r, &crate::stream::ParseOptions::full(), &mut sink)
            .unwrap();
        streamed.messages = sink.messages;

        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "kimi parse() and collect(stream()) must agree"
        );
        // And the enrichment is actually present on the streamed side (not just both-empty).
        let asst = streamed.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        assert_eq!(asst.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(asst.id.as_deref(), Some("chatcmpl-abc"));
        let first_user = streamed.messages.iter().find(|m| m.role == Role::User).unwrap();
        assert!(
            first_user.timestamp.is_some(),
            "first user turn gets the first wire timestamp on the streaming path too"
        );
    }

    #[test]
    fn parses_fixture_modern_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/11111111-1111-1111-1111-111111111111");
        if !dir.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        // assistant with a tool_use, and a tool result enriched from wire (is_error=false).
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        let tr = s
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .and_then(|m| m.content.first());
        match tr {
            Some(Block::ToolResult { content, is_error, .. }) => {
                assert!(content.contains("README.md"));
                assert!(!is_error);
            }
            _ => panic!("expected tool result"),
        }
        // assistant message picked up usage + message_id from wire StatusUpdate.
        let asst = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        assert_eq!(asst.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(asst.id.as_deref(), Some("chatcmpl-abc"));
    }

    #[test]
    fn parses_fixture_legacy_flat_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/legacy/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        if !dir.exists() {
            return;
        }
        let id = "22222222-2222-2222-2222-222222222222";
        let r = SessionRef {
            id: id.into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&r)
        .unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
    }

    /// Write `name`→`contents` into a fresh temp dir, return the dir.
    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-kimi-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }
}
