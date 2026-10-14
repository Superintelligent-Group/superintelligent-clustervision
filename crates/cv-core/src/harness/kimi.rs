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
//!   `context_1.jsonl…context_N.jsonl`. `/compact`, `/clear` and a rewind each rotate the live file
//!   out; the session is ALL segments, oldest first, then the live `context.jsonl` (the head a new
//!   segment repeats — the system prompt, or a rewind's copied prefix — is read once). A segment
//!   that opens with the compaction summary gets a `CompactionBoundary`; any other rotation gets a
//!   synthetic `Branch` (`extra["kimi"]["segment"]`). The count stays in
//!   `extra["kimi"]["compaction_segments"]`.
//! - **Sub-agents:** `<session>/subagents/<agent_id>/` holds each `Agent`-tool child in the same
//!   format (`context.jsonl`, `wire.jsonl`) plus `meta.json` (`subagent_type`, `description`, …);
//!   [`subagent_refs`] lists them, parsed with `lineage.parent` = the session and `agent_path` =
//!   the agent id. Also in a session dir, read by nothing here: `tasks/<id>/` (background shells:
//!   `spec.json`, `runtime.json`, `output.log`, …) and `notifications/<id>/` (`event.json`,
//!   `delivery.json`); `state.json` also keeps `todos` (read as the session's `plan` state,
//!   [`Adapter::store_affordances`]), `plan_mode`, `approval`.
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

use super::store::document::{Raw, Record};
use super::store::jsonl::scan as scan_wire;
use super::wire::{openai, text_parts};
use super::Adapter;
use crate::ir::*;
use crate::lazy::{Span, Text, INLINE_MAX};
use crate::stream::{Checkpoint, Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

mod records;
mod selected;

pub struct Kimi {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
    /// The share root itself (`~/.kimi`), for locating `kimi.json` / `config.toml`.
    root: Option<PathBuf>,
}

impl Kimi {
    pub fn detect() -> Self {
        let root = std::env::var_os("KIMI_SHARE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".kimi")))
            .filter(|p| p.exists());
        match root {
            Some(r) => Kimi::with_home(&r),
            None => Kimi {
                sessions: None,
                root: None,
            },
        }
    }

    /// An adapter over an explicit kimi-cli share dir (the dir holding `sessions/` and
    /// `kimi.json`), instead of `$KIMI_SHARE_DIR` / `~/.kimi` — for the harness corpus and other
    /// tests, without mutating the process environment.
    pub fn with_home(share_dir: &Path) -> Self {
        let root = Some(share_dir.to_path_buf()).filter(|p| p.exists());
        let sessions = root.as_ref().map(|r| r.join("sessions")).filter(|p| p.exists());
        Kimi { sessions, root }
    }
}

impl Adapter for Kimi {
    fn harness(&self) -> Harness {
        Harness::Kimi
    }

    fn roots(&self) -> Vec<PathBuf> {
        // The share root (`~/.kimi`), NOT `<root>/sessions`: `kimi.json` (the cwd registry) lives
        // at this root beside `sessions/`, so the root is what a consumer needs.
        // (Returning `sessions` here produced `~/.kimi/sessions/sessions/...`, which kimi-cli never
        // discovers.) Fall back to `<sessions>/..` if only the sessions dir was detected.
        self.root
            .clone()
            .or_else(|| self.sessions.as_ref().and_then(|s| s.parent().map(Path::to_path_buf)))
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let _cache_scope = crate::discover_cache::read::Context::current().enter();
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };
        let mut shared_reads = crate::source::ReadSet::default();
        if let Some(root) = &self.root {
            shared_reads.record(&root.join("kimi.json"));
        }
        // hash (dir basename, possibly "<kaos>_<md5>") -> cwd, from kimi.json.
        let hash_to_cwd = self
            .root
            .as_ref()
            .map(|r| load_work_dirs(&r.join("kimi.json")))
            .unwrap_or_default();
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
                let mut reads = shared_reads.clone();
                reads.record(&dir.join("state.json"));
                if transcript.file_name().is_some_and(|name| name == "context.jsonl") {
                    // Capture membership before enumerating rotations, and each existing file
                    // before the scan reads it. Missing wire/state files are dependencies too.
                    reads.record(&dir);
                    reads.record(&dir.join("wire.jsonl"));
                    for segment in transcript_segments(&dir, &transcript) {
                        reads.record(&segment);
                    }
                }
                let hit = crate::discover_cache::cached_scan_with_reads(&transcript, reads, || {
                    scan(&id, &transcript, &dir, cwd.clone())
                });
                if let Some(r) = hit {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Kimi, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let (transcript, wire) = locate_transcript(&r.path, &r.id);
            let segments = transcript_segments(&r.path, &transcript);
            note_config_dependency(self, sink);
            let (s, enrich) = session_header(self, r, opts, &segments, wire.as_deref());
            sink.meta(&s);
            let primary = self.resume_source(r);
            let capture = segments.last() == Some(&primary);
            stream_context(
                s,
                &segments,
                &enrich,
                opts,
                ContextState::default(),
                ContextStart { at: None, capture },
                sink,
            )
        })
    }

    /// The todo list `SetTodoList` keeps in the session dir's `state.json` (`todos: [{title,
    /// status}]`). A legacy flat session (no `context.jsonl` in its dir) has no state file.
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        if !r.path.join("context.jsonl").is_file() {
            return Vec::new();
        }
        let Some(todos) = fs::read_to_string(r.path.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("todos").cloned())
        else {
            return Vec::new();
        };
        let items = crate::affordance::items_of(&todos);
        crate::affordance::plan_state(crate::affordance::Kind::Plan, "state.json", items)
            .into_iter()
            .collect()
    }

    fn resume_format(&self) -> Option<u32> {
        Some(1)
    }

    fn resume_source(&self, r: &SessionRef) -> PathBuf {
        // Native layout is fixed: sessions/<hash>/<id> (or its subagent dir) is modern;
        // sessions/<hash> is the legacy flat store. No mutable exists() decision selects a mark.
        let modern = r
            .path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
            .and_then(|root| r.path.strip_prefix(root).ok())
            .is_some_and(|p| p.components().count() > 1)
            || r.path.file_name().and_then(|p| p.to_str()) == r.id.rsplit('/').next();
        if modern {
            r.path.join("context.jsonl")
        } else {
            r.path.join(format!("{}.jsonl", r.id))
        }
    }

    fn resume(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let Some(state) = from.get::<ContextCheckpoint>(crate::stream::ADAPTER_LAYER) else {
                return Ok(None);
            };
            let primary = self.resume_source(r);
            if state.primary != primary || state.flavor != context_flavor(opts) {
                return Ok(None);
            }
            let (transcript, wire) = locate_transcript(&r.path, &r.id);
            let segments = transcript_segments(&r.path, &transcript);
            if transcript != primary || state.segments != segments {
                return Ok(None);
            }
            if fs::metadata(&primary)?.len() < from.offset {
                return Ok(None);
            }
            note_config_dependency(self, sink);
            let (s, enrich) = session_header(self, r, opts, &segments, wire.as_deref());
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Kimi, sink);
            spawns.meta(&s);
            Ok(Some(stream_context(
                s,
                &segments,
                &enrich,
                opts,
                state.ctx,
                ContextStart {
                    at: Some(from.offset),
                    capture: true,
                },
                &mut spawns,
            )?))
        })
    }
}

fn note_config_dependency(adapter: &Kimi, sink: &mut dyn MessageSink) {
    if let Some(root) = &adapter.root {
        sink.dependency(&root.join("config.toml"));
    }
}

fn session_header(
    adapter: &Kimi,
    r: &SessionRef,
    opts: &ParseOptions,
    segments: &[PathBuf],
    wire: Option<&Path>,
) -> (Session, WireEnrich) {
    let mut s = Session {
        id: r.id.clone(),
        harness: Harness::Kimi,
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: adapter
            .root
            .as_ref()
            .and_then(|root| default_model_from_config(&root.join("config.toml"))),
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
        system_prompt: None,
        lineage: subagent_lineage(&r.path),
    };
    if let Some(meta) = subagent_meta(&r.path) {
        s.harness_extra_mut(Harness::Kimi).insert("subagent".into(), meta);
    }

    // Sidecar enrichment from wire.jsonl (tolerant: absent/empty/malformed ⇒ empty). The
    // per-message `tool_results` map (keyed by tool_call_id) is a per-line enrichment; the
    // positional passes (StatusUpdate usage/message_id zipped onto assistant turns, the first
    // wire timestamp stamped onto the first user turn) only ever walk *forward* with O(1)
    // state, so they run inline here too — `parse()` is exactly `collect(stream())`.
    let enrich = wire
        .as_ref()
        .map(|p| read_wire_enrichment(p, opts.spans))
        .unwrap_or_default();

    // Rotated segments (context_1.jsonl…) are read first, in order; their count stays as
    // bookkeeping.
    let seg_count = segments.len().saturating_sub(1) as u64;
    if seg_count > 0 {
        s.harness_extra_mut(Harness::Kimi)
            .insert("compaction_segments".into(), Value::Number(seg_count.into()));
    }
    // The system prompt is the first `context.jsonl` record; peek so the session-level copy is
    // known before the body streams.
    s.system_prompt = peek_system_prompt(&segments[0]);

    if let Some(mode) = approval_mode(&r.path) {
        crate::affordance::note_mode(s.harness_extra_mut(Harness::Kimi), &mode);
    }

    (s, enrich)
}

fn context_flavor(opts: &ParseOptions) -> [bool; 4] {
    [opts.extra, opts.spans, opts.offsets, opts.complete]
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ContextState {
    call_names: HashMap<String, String>,
    status_index: usize,
    first_user_seen: bool,
    checkpoints: u64,
    rotated: Option<String>,
    skipped: u64,
}
#[derive(serde::Deserialize)]
struct ContextCheckpoint {
    primary: PathBuf,
    segments: Vec<PathBuf>,
    flavor: [bool; 4],
    ctx: ContextState,
}

struct ContextStart {
    at: Option<u64>,
    capture: bool,
}

fn stream_context(
    mut s: Session,
    segments: &[PathBuf],
    enrich: &WireEnrich,
    opts: &ParseOptions,
    mut ctx: ContextState,
    start: ContextStart,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let mut pending: Option<Message> = None;
    let mut stopped = false;
    let prior_skipped = ctx.skipped;
    let mut accept = |view: crate::json::JsonView<'_>,
                      source: Option<&crate::json::JsonSource>,
                      starts: Option<&str>,
                      position: Option<u64>,
                      skipped: u64|
     -> Result<Flow> {
        if let Some(name) = starts {
            ctx.rotated = Some(name.into());
        }
        let role = view
            .get("role")?
            .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
            .transpose()?
            .unwrap_or_default();
        let checkpoint_record = u64::from(role == "_checkpoint");
        ctx.checkpoints += checkpoint_record;
        if role == "_usage" {
            if let Some(p) = pending.as_mut() {
                selected::fold_usage(p, view)?;
            }
            return Ok(Flow::Continue);
        }
        // Interpret without applying the current record's call-name/positional state. The anchor
        // below replays this record and must retain only the immutable prefix's state.
        let message = match source {
            Some(source) => selected::message(view, source, enrich, opts.complete, &ctx.call_names)?,
            None => context_message_inner(view.resolve()?.as_ref(), enrich, opts.complete, &ctx.call_names),
        };
        let Some(mut m) = message else {
            return Ok(Flow::Continue);
        };
        if let Some(prev) = pending.take() {
            if sink.message(prev) == Flow::Stop {
                stopped = true;
                return Ok(Flow::Stop);
            }
            if let Some(offset) = position.filter(|_| start.capture) {
                if sink.wants_checkpoint(offset) {
                    #[derive(serde::Serialize)]
                    struct State<'a> {
                        primary: &'a Path,
                        segments: &'a [PathBuf],
                        flavor: [bool; 4],
                        ctx: &'a ContextState,
                    }
                    // The current checkpoint carrier is not part of the prefix yet.
                    ctx.checkpoints -= checkpoint_record;
                    ctx.skipped = prior_skipped + skipped;
                    let mut cp = Checkpoint::new(offset);
                    cp.put(
                        crate::stream::ADAPTER_LAYER,
                        &State {
                            primary: segments.last().unwrap(),
                            segments,
                            flavor: context_flavor(opts),
                            ctx: &ctx,
                        },
                    );
                    sink.checkpoint(&mut cp);
                    ctx.checkpoints += checkpoint_record;
                }
            }
        }
        note_call_names(&m, &mut ctx.call_names);
        match m.role {
            Role::Assistant => {
                if let Some(st) = enrich.statuses.get(ctx.status_index) {
                    if m.usage.is_none() {
                        m.usage = st.usage.clone();
                    }
                    if m.id.is_none() {
                        m.id = st.message_id.clone();
                    }
                }
                ctx.status_index += 1;
            }
            Role::User if !ctx.first_user_seen => {
                ctx.first_user_seen = true;
                if m.timestamp.is_none() {
                    m.timestamp = enrich.first_timestamp;
                }
            }
            _ => {}
        }
        if m.kind == MessageKind::CompactionSummary && sink.message(compaction_boundary(m.timestamp)) == Flow::Stop {
            stopped = true;
            return Ok(Flow::Stop);
        }
        if let Some(segment) = ctx.rotated.take() {
            if m.kind != MessageKind::CompactionSummary
                && sink.message(rotation_branch(m.timestamp, segment)) == Flow::Stop
            {
                stopped = true;
                return Ok(Flow::Stop);
            }
        }
        pending = Some(m);
        Ok(Flow::Continue)
    };
    let skipped = if opts.spans {
        records::visit(segments, start.at, |view, source, starts, position, skipped| {
            accept(view, Some(source), starts, position, skipped)
        })?
    } else {
        let mut error = None;
        let skipped = for_each_segment_record_at(segments, start.at, |v, starts, position, skipped| {
            match accept((&v).into(), None, starts, position, skipped) {
                Ok(flow) => flow,
                Err(e) => {
                    error = Some(e);
                    Flow::Stop
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        skipped
    };
    if !stopped {
        if let Some(last) = pending {
            sink.message(last);
        }
    }
    if ctx.checkpoints > 0 {
        crate::affordance::note_checkpoints(s.harness_extra_mut(Harness::Kimi), ctx.checkpoints, []);
    }
    super::note_skipped_lines(&mut s, prior_skipped + skipped);
    Ok(s)
}

#[cfg(test)]
fn old_context_pass(
    mut s: Session,
    segments: &[PathBuf],
    enrich: &WireEnrich,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    // Stream the segments line-by-line (a single session can be large; `read_to_string` would
    // resident-spike the whole file). Each record becomes at most one Message, emitted to `sink`
    // and dropped before the next line, so peak memory is O(largest message).
    if !segments.iter().any(|p| p.exists()) {
        return s;
    }
    // Positional wire enrichment, streamed: one StatusUpdate per assistant step (in wire
    // order) and the first wire timestamp for the first user turn. Forward-only, O(1) state.
    let mut statuses = enrich.statuses.iter();
    let first_wire_ts = enrich.first_timestamp;
    let mut first_user_seen = false;
    // `tool_call_id` → tool name from the assistant's `tool_calls`, so a result names its tool.
    let mut call_names: HashMap<String, String> = HashMap::new();
    let complete = opts.complete;
    // One-message lookahead: a `_usage` record describes the message BEFORE it, so each message
    // waits here until the next record shows what follows. Peak memory stays O(one message).
    let mut pending: Option<Message> = None;
    let mut stopped = false;
    // Set at the first new record of a rotated-in segment: unless that record is the
    // compaction summary (which gets its own boundary), the rotation was a `/clear` or a
    // rewind, and what follows does not continue what precedes.
    let mut rotated: Option<String> = None;
    // `_checkpoint` records (rewind targets), tallied for the session's affordances.
    let mut checkpoints = 0u64;
    let skipped = old_for_each_segment_record(segments, |v, starts| {
        if let Some(name) = starts {
            rotated = Some(name.to_string());
        }
        checkpoints += u64::from(v.get("role").and_then(Value::as_str) == Some("_checkpoint"));
        if v.get("role").and_then(Value::as_str) == Some("_usage") {
            if let Some(p) = pending.as_mut() {
                fold_usage_record(p, &v);
            }
            return Flow::Continue;
        }
        let Some(mut m) = context_message(&v, enrich, complete, &mut call_names) else {
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
        if m.kind == MessageKind::CompactionSummary && sink.message(compaction_boundary(m.timestamp)) == Flow::Stop {
            stopped = true;
            return Flow::Stop;
        }
        if let Some(segment) = rotated.take() {
            if m.kind != MessageKind::CompactionSummary
                && sink.message(rotation_branch(m.timestamp, segment)) == Flow::Stop
            {
                stopped = true;
                return Flow::Stop;
            }
        }
        pending = Some(m);
        Flow::Continue
    });
    if !stopped {
        if let Some(last) = pending.take() {
            sink.message(last);
        }
    }
    if checkpoints > 0 {
        crate::affordance::note_checkpoints(s.harness_extra_mut(Harness::Kimi), checkpoints, []);
    }
    super::note_skipped_lines(&mut s, skipped);

    s
}

/// The approval mode `state.json` records (`approval{yolo, afk, auto_approve_actions}`): `yolo`
/// and `afk` approve everything, otherwise kimi-cli asks, except for the listed actions.
fn approval_mode(dir: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join("state.json")).ok()?).ok()?;
    let a = v.get("approval")?;
    let on = |k: &str| a.get(k).and_then(Value::as_bool) == Some(true);
    let auto: Vec<&str> = a
        .get("auto_approve_actions")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Some(match () {
        _ if on("yolo") => "yolo".to_string(),
        _ if on("afk") => "afk".to_string(),
        _ if !auto.is_empty() => format!("ask (auto: {})", auto.join(", ")),
        _ => "ask".to_string(),
    })
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

/// The top-level model follows TOML string/comment/table rules. The workspace already includes
/// the parser; typed decoding leaves unrelated configuration out of the adapter's state.
fn default_model_from_config(config_toml: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Config {
        default_model: Option<String>,
    }
    let text = fs::read_to_string(config_toml).ok()?;
    toml::from_str::<Config>(&text)
        .ok()?
        .default_model
        .filter(|model| !model.is_empty())
}

/// Cheap scan of one transcript into a `SessionRef`. `dir` is the session's source dir (the
/// `<uuid>/` dir for modern, the hash dir for legacy). Returns `None` only on a fully unreadable
/// transcript path that doesn't exist.
fn scan(id: &str, transcript: &Path, dir: &Path, cwd: Option<PathBuf>) -> Option<SessionRef> {
    let meta = fs::metadata(transcript).ok()?;
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = read_custom_title(dir);
    let mut message_count = 0usize;
    records::visit(&transcript_segments(dir, transcript), None, |view, _, _, _, _| {
        let Some(fields) = super::store::document::Fields::parse(view, &["role", "content"])? else {
            return Ok(Flow::Continue);
        };
        let role = fields.string("role")?;
        if matches!(role.as_deref(), Some("user" | "assistant")) {
            message_count += 1;
        }
        if title.is_none() && role.as_deref() == Some("user") {
            title = super::wire::text_title(fields.get("content"))?;
        }
        Ok(Flow::Continue)
    })
    .ok()?;

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

#[cfg(test)]
fn old_scan(id: &str, transcript: &Path, dir: &Path, cwd: Option<PathBuf>) -> Option<SessionRef> {
    let meta = fs::metadata(transcript).ok()?;
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = old_read_custom_title(dir);
    let mut message_count = 0usize;
    old_for_each_segment_record(&transcript_segments(dir, transcript), |v, _| {
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        // SessionRef contract: user + assistant turns only (no tool / _system_prompt records).
        if matches!(role, "user" | "assistant") {
            message_count += 1;
        }
        if title.is_none() && role == "user" {
            let t = text_parts(v.get("content"));
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
        .and_then(|w| old_wire_time_bounds(w))
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
    let path = dir.join("state.json");
    let source = crate::json::JsonSource::capture(&path).ok()?;
    source
        .read_range_value(0, source.len(), |view| {
            super::wire::text_title(
                view.get("custom_title")?
                    .filter(|value| value.is_string().unwrap_or(false)),
            )
        })
        .ok()
        .flatten()
}

#[cfg(test)]
fn old_read_custom_title(dir: &Path) -> Option<String> {
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

/// The transcript's segments, oldest first: the rotated-out `context_1.jsonl…context_N.jsonl`
/// (kimi-cli's `next_available_rotation`, numbered upward) and then the live `context.jsonl`. A
/// legacy flat transcript has no segments of its own.
fn transcript_segments(dir: &Path, transcript: &Path) -> Vec<PathBuf> {
    let mut rotated: Vec<(u64, PathBuf)> = Vec::new();
    if transcript.file_name().and_then(|n| n.to_str()) == Some("context.jsonl") {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = e.file_name();
                let Some(n) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("context_"))
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .and_then(|n| n.parse::<u64>().ok())
                else {
                    continue;
                };
                rotated.push((n, e.path()));
            }
        }
    }
    rotated.sort();
    let mut v: Vec<PathBuf> = rotated.into_iter().map(|(_, p)| p).collect();
    v.push(transcript.to_path_buf());
    v
}

/// Every record of every segment, in order. kimi-cli rotates the live file on `/clear`, on a
/// compaction (`Context.clear`) and on a rewind (`Context.revert_to`, which copies the kept prefix
/// into the new file), and every new file starts with the system prompt again; so the leading
/// lines a segment shares with the one before it are the same records and are not repeated. The
/// callback's second argument names the segment at its first new record (never for the first
/// segment). Returns the count of malformed lines.
#[cfg(test)]
fn old_for_each_segment_record(segments: &[PathBuf], mut f: impl FnMut(Value, Option<&str>) -> Flow) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::io::BufRead;
    let line_hash = |l: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        l.trim().hash(&mut h);
        h.finish()
    };
    let mut skipped = 0u64;
    let mut prev: Vec<u64> = Vec::new();
    for (i, path) in segments.iter().enumerate() {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut cur: Vec<u64> = Vec::new();
        let mut shared = i > 0;
        let mut announce = i > 0;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                skipped += 1;
                continue;
            };
            let h = line_hash(&line);
            cur.push(h);
            if shared && prev.get(cur.len() - 1) == Some(&h) {
                continue;
            }
            shared = false;
            let mut stop = false;
            skipped += super::for_each_json_line_str(&line, |v| {
                let starts = if announce { Some(name) } else { None };
                announce = false;
                if f(v, starts) == Flow::Stop {
                    stop = true;
                    return Flow::Stop;
                }
                Flow::Continue
            });
            if stop {
                return skipped;
            }
        }
        prev = cur;
    }
    skipped
}

fn for_each_segment_record_at(
    segments: &[PathBuf],
    at: Option<u64>,
    mut f: impl FnMut(Value, Option<&str>, Option<u64>, u64) -> Flow,
) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::io::BufRead;
    let mut skipped = 0;
    let mut prev: Vec<u64> = Vec::new();
    for (i, path) in segments.iter().enumerate() {
        let live = i + 1 == segments.len();
        if at.is_some() && !live {
            continue;
        }
        let Ok(mut file) = fs::File::open(path) else { continue };
        let mut offset = if live { at.unwrap_or(0) } else { 0 };
        if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut reader = BufReader::new(file);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut cur = Vec::new();
        let mut line_index = 0;
        let mut shared = at.is_none() && i > 0;
        let mut announce = shared;
        let mut known = true;
        let mut line = Vec::new();
        loop {
            line.clear();
            let start = offset;
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(n) => offset += n as u64,
                Err(_) => {
                    skipped += 1;
                    known = false;
                    continue;
                }
            }
            let Ok(text) = std::str::from_utf8(&line) else {
                skipped += 1;
                continue;
            };
            let text = text.trim();
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hash);
            let hash = hash.finish();
            // The final segment has no successor needing its prefix fingerprints.
            if !live {
                cur.push(hash);
            }
            let matches = shared && prev.get(line_index) == Some(&hash);
            line_index += 1;
            if matches {
                continue;
            }
            shared = false;
            let mut stop = false;
            let line_skipped = super::for_each_json_line_str(text, |v| {
                let starts = if announce { Some(name) } else { None };
                announce = false;
                // Only live-file anchors whose byte accounting is certain, after a complete
                // record, are offered. A trailing partial record is replayed from an older anchor.
                let position = (live && known && line.last() == Some(&b'\n')).then_some(start);
                if f(v, starts, position, skipped) == Flow::Stop {
                    stop = true;
                    return Flow::Stop;
                }
                Flow::Continue
            });
            skipped += line_skipped;
            if stop {
                return skipped;
            }
        }
        prev = cur;
    }
    skipped
}

/// The lineage of a sub-agent transcript (`<session>/subagents/<agent_id>`, kimi-cli's
/// `SubagentStore`): the owning session and the agent id. Empty for a session's own dir.
fn subagent_lineage(path: &Path) -> crate::ir::Lineage {
    let mut l = crate::ir::Lineage::default();
    let parent = path.parent();
    if parent.and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("subagents") {
        l.parent = parent
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
        l.agent_path = path.file_name().and_then(|n| n.to_str()).map(str::to_string);
    }
    l
}

/// A sub-agent's `meta.json` (`{agent_id, subagent_type, status, description, launch_spec, …}`).
fn subagent_meta(path: &Path) -> Option<Value> {
    subagent_lineage(path).parent?;
    serde_json::from_str(&fs::read_to_string(path.join("meta.json")).ok()?).ok()
}

/// One [`SessionRef`] per sub-agent of a discovered session (`<session>/subagents/<agent_id>/`
/// with a `context.jsonl`). Discovery lists only top-level sessions; parsing one of these yields a
/// [`Session`] whose `lineage.parent` is the session id and `lineage.agent_path` the agent id.
pub fn subagent_refs(main: &SessionRef) -> Vec<SessionRef> {
    let _cache_scope = crate::discover_cache::read::Context::current().enter();
    let Ok(rd) = fs::read_dir(main.path.join("subagents")) else {
        return Vec::new();
    };
    let mut out: Vec<SessionRef> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("context.jsonl").is_file())
        .filter_map(|p| {
            let agent = p.file_name()?.to_str()?.to_string();
            let transcript = p.join("context.jsonl");
            let mut reads = crate::source::ReadSet::default();
            reads.record(&p);
            for path in [p.join("state.json"), p.join("meta.json"), p.join("wire.jsonl")] {
                reads.record(&path);
            }
            for segment in transcript_segments(&p, &transcript) {
                reads.record(&segment);
            }
            let mut r = crate::discover_cache::cached_scan_with_reads(&transcript, reads, || {
                let mut r = scan(&agent, &transcript, &p, None)?;
                if let Some(d) =
                    subagent_meta(&p).and_then(|m| m.get("description").and_then(Value::as_str).map(str::to_string))
                {
                    r.title = Some(crate::ir::truncate(&d, 80));
                }
                Some(r)
            })?;
            // Parent display identity/cwd are supplied by the caller, not by these cached bytes.
            r.id = format!("{}/{agent}", main.id);
            r.cwd.clone_from(&main.cwd);
            Some(r)
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The marker for a segment that a `/clear` or a rewind rotated in (a compaction's rotation gets
/// [`compaction_boundary`] from its summary instead).
fn rotation_branch(ts: Option<DateTime<Utc>>, segment: String) -> Message {
    let mut m = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
    m.timestamp = ts;
    m.content.push(Block::Text {
        text: "[context cleared or rewound]".into(),
    });
    let x = m.harness_extra_mut(Harness::Kimi);
    x.insert("synthetic".into(), Value::Bool(true));
    x.insert("segment".into(), Value::String(segment));
    m
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
            let text = text_parts(v.get("content"));
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
fn note_call_names(message: &Message, names: &mut HashMap<String, String>) {
    for block in &message.content {
        if let Block::ToolUse { id, name, .. } = block {
            if !id.is_empty() && !name.is_empty() {
                names.insert(id.clone(), name.clone());
            }
        }
    }
}

#[cfg(test)]
fn context_message(
    v: &Value,
    enrich: &WireEnrich,
    complete: bool,
    call_names: &mut HashMap<String, String>,
) -> Option<Message> {
    let message = context_message_inner(v, enrich, complete, call_names)?;
    note_call_names(&message, call_names);
    Some(message)
}

fn context_message_inner(
    v: &Value,
    enrich: &WireEnrich,
    complete: bool,
    call_names: &HashMap<String, String>,
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
            let text = text_parts(v.get("content"));
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
                    let call = openai::tool_call(c);
                    m.content.push(call);
                }
            }
            (!m.content.is_empty()).then_some(m)
        }
        "tool" => {
            let tool_use_id = v.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
            // Prefer the richer wire return_value if we have it; else the context text parts.
            let we = enrich.tool_results.get(&tool_use_id);
            let (content, is_error, details) = match we {
                Some(tr) => (enrich.output(tr), tr.is_error, enrich.extras(tr)),
                None => (text_parts(v.get("content")).into(), false, None),
            };
            let status = Some(if is_error { "error" } else { "completed" }.to_string());
            let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
            m.content.push(Block::ToolResult {
                tool_name: call_names.get(&tool_use_id).cloned(),
                tool_use_id,
                content,
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
#[cfg(test)]
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
    m.content.extend(openai::blocks_from(content, &openai::KIMI));
}

// ---------------------------------------------------------------------------
// wire.jsonl sidecar enrichment
// ---------------------------------------------------------------------------

/// Only navigation survives the wire pass: no output or extras body is retained.
#[derive(Default)]
struct WireEnrich {
    tool_results: HashMap<String, WireToolResult>,
    statuses: Vec<WireStatus>,
    first_timestamp: Option<DateTime<Utc>>,
    file: Option<fs::File>,
    path: PathBuf,
    spans: bool,
}

#[derive(Clone, Copy)]
struct WireRange {
    offset: u64,
    len: usize,
}

struct WireToolResult {
    is_error: bool,
    output: Option<WireRange>,
    output_escaped: bool,
    extras: Option<WireRange>,
}

impl WireEnrich {
    fn read(&self, range: WireRange) -> Option<Vec<u8>> {
        // Keep the original descriptor open: replacing the path during a pass cannot redirect
        // full-mode enrichment to an unrelated file. This reader is used sequentially.
        let mut file = self.file.as_ref()?;
        file.seek(SeekFrom::Start(range.offset)).ok()?;
        let mut bytes = vec![0; range.len];
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    fn output(&self, result: &WireToolResult) -> Text {
        let Some(range) = result.output else {
            return String::new().into();
        };
        if self.spans && range.len.saturating_sub(2) > INLINE_MAX {
            return Text::Span(Span {
                source: Some(self.path.clone()),
                offset: range.offset + 1,
                len: range.len as u64 - 2,
                escaped: result.output_escaped,
            });
        }
        self.read(range)
            .and_then(|b| serde_json::from_slice::<String>(&b).ok())
            .unwrap_or_default()
            .into()
    }

    fn extras(&self, result: &WireToolResult) -> Option<Value> {
        self.read(result.extras?).and_then(|b| serde_json::from_slice(&b).ok())
    }
}

#[derive(Default)]
struct WireStatus {
    usage: Option<Usage>,
    message_id: Option<String>,
}

fn wire_fields(raw: Raw<'_>) -> Record<'_> {
    Record::parse(raw).unwrap_or_else(|| Record::empty(raw))
}

/// Index ToolResult fields by byte range, decoding only usage/identity metadata. A later result
/// for the same call replaces its descriptor, including missing/non-string output and null extras.
fn read_wire_enrichment(wire: &Path, spans: bool) -> WireEnrich {
    let mut e = WireEnrich {
        path: wire.to_path_buf(),
        spans,
        ..WireEnrich::default()
    };
    let file = scan_wire(wire, |raw, offset| {
        let v = wire_fields(raw);
        if v.string("type").as_deref() == Some("metadata") {
            return;
        }
        let Some(msg) = v.get("message") else { return };
        let msg = wire_fields(msg);
        let mty = msg.string("type").unwrap_or_default();
        if let Some(payload) = msg.get("payload") {
            let p = wire_fields(payload);
            match mty.as_str() {
                "ToolResult" => {
                    let id = p.string("tool_call_id").unwrap_or_default();
                    if !id.is_empty() {
                        let rv = p
                            .get("return_value")
                            .map(wire_fields)
                            .unwrap_or_else(|| Record::empty(raw));
                        let range = |r: Raw<'_>| WireRange {
                            offset: offset + (r.get().as_ptr() as usize - raw.get().as_ptr() as usize) as u64,
                            len: r.get().len(),
                        };
                        let string = |r: &Raw<'_>| r.get().starts_with('"');
                        let output_raw = rv
                            .get("output")
                            .filter(string)
                            .or_else(|| rv.get("message").filter(string));
                        let output = output_raw.map(range);
                        let output_escaped = output_raw.is_some_and(|r| r.get().as_bytes().contains(&b'\\'));
                        let extras = rv.get("extras").filter(|r| r.get() != "null").map(range);
                        let is_error = rv.get("is_error").is_some_and(|r| r.get() == "true");
                        e.tool_results.insert(
                            id,
                            WireToolResult {
                                is_error,
                                output,
                                output_escaped,
                                extras,
                            },
                        );
                    }
                }
                "StatusUpdate" => {
                    let usage = p.get("token_usage").map(|r| wire_usage(r));
                    let message_id = p.string("message_id");
                    e.statuses.push(WireStatus { usage, message_id });
                }
                _ => {}
            }
        }
        if matches!(mty.as_str(), "TurnBegin" | "ContentPart" | "ToolCall") {
            if let Some(ts) = v.get("timestamp").and_then(|r| wire_timestamp(r)) {
                if e.first_timestamp.is_none() {
                    e.first_timestamp = Some(ts);
                }
            }
        }
    });
    match file {
        Some(file) => {
            e.file = Some(file);
            e
        }
        None => WireEnrich::default(),
    }
}

fn wire_usage(raw: Raw<'_>) -> Usage {
    let tu = wire_fields(raw);
    let g = |k: &str| tu.get(k).and_then(|r| serde_json::from_str::<u64>(r.get()).ok());
    Usage {
        input_tokens: g("input_other"),
        output_tokens: g("output"),
        cache_read_tokens: g("input_cache_read"),
        cache_creation_tokens: g("input_cache_creation"),
        ..Usage::default()
    }
}

fn wire_timestamp(raw: Raw<'_>) -> Option<DateTime<Utc>> {
    serde_json::from_str::<serde_json::Number>(raw.get())
        .ok()?
        .as_f64()
        .and_then(epoch_to_dt)
}

/// First and last event timestamps in a wire file (epoch floats), for created/updated bounds.
fn wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    use super::store::jsonl::ranges;
    let file = fs::File::open(wire).ok()?;
    let source = crate::json::JsonSource::capture(wire).ok()?;
    source.check_metadata(file.metadata().ok()?).ok()?;
    // The old shared wire scanner rejects invalid UTF-8 anywhere, unlike context records.
    if !ranges::valid_utf8(
        &mut ranges::reader(
            &file,
            ranges::Range {
                offset: 0,
                len: source.len(),
            },
        )
        .ok()?,
    )
    .ok()?
    {
        return None;
    }
    let mut first = None;
    let mut last = None;
    super::store::checked_lines::for_each(&file, 0, &source, |view, _, _, _| {
        if let Some(ts) = view
            .get("timestamp")?
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_f64())
            .and_then(epoch_to_dt)
        {
            if first.is_none() {
                first = Some(ts);
            }
            last = Some(ts);
        }
        Ok(Flow::Continue)
    })
    .ok()?;
    Some((first?, last?))
}

#[cfg(test)]
fn old_wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut first = None;
    let mut last = None;
    scan_wire(wire, |raw, _| {
        let v = wire_fields(raw);
        if let Some(ts) = v.get("timestamp").and_then(|r| wire_timestamp(r)) {
            if first.is_none() {
                first = Some(ts);
            }
            last = Some(ts);
        }
    })?;
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
    use crate::harness::Parse;

    #[test]
    fn discovery_refreshes_titles_registry_rotations_and_wire_without_live_append() -> Result<()> {
        let world = cv_testkit::World::in_process("kimi_aux_discovery");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let home = world.home.join(".kimi");
        let cwd = "/synthetic/project";
        let dir = home.join("sessions").join(md5_hex(cwd.as_bytes())).join("session");
        fs::create_dir_all(&dir)?;
        let registry = home.join("kimi.json");
        fs::write(&registry, serde_json::json!({"work_dirs":[{"path":cwd}]}).to_string())?;
        let live = dir.join("context.jsonl");
        let body =
            "{\"role\":\"user\",\"content\":\"prompt fallback\"}\n{\"role\":\"assistant\",\"content\":\"answer\"}\n";
        fs::write(&live, body)?;
        let adapter = Kimi::with_home(&home);
        let first = adapter.discover()?.remove(0);
        assert_eq!(first.title.as_deref(), Some("prompt fallback"));
        assert_eq!(first.message_count, 2);
        assert_eq!(first.cwd.as_deref(), Some(Path::new(cwd)));
        fs::write(
            dir.join("state.json"),
            "{\"custom_title\":\"renamed without a new turn\"}",
        )?;
        assert_eq!(
            adapter.discover()?[0].title.as_deref(),
            Some("renamed without a new turn")
        );
        fs::write(dir.join("state.json"), "{}")?;
        assert_eq!(adapter.discover()?[0].title.as_deref(), Some("prompt fallback"));
        fs::write(
            &registry,
            serde_json::json!({"work_dirs":[{"path":cwd,"kaos":"remote"}]}).to_string(),
        )?;
        assert_eq!(adapter.discover()?[0].cwd, None);
        let rotated = dir.join("context_1.jsonl");
        fs::write(&rotated, "{\"role\":\"user\",\"content\":\"earlier\"}\n")?;
        assert_eq!(adapter.discover()?[0].message_count, 3);
        fs::write(
            &rotated,
            "{\"role\":\"user\",\"content\":\"earlier\"}\n{\"role\":\"assistant\",\"content\":\"prior reply\"}\n",
        )?;
        assert_eq!(adapter.discover()?[0].message_count, 4);
        fs::write(
            dir.join("wire.jsonl"),
            "{\"timestamp\":1700000000}\n{\"timestamp\":1700000001}\n",
        )?;
        let last = adapter.discover()?.remove(0);
        assert_eq!(last.created_at, epoch_to_dt(1700000000.0));
        assert_eq!(last.updated_at, epoch_to_dt(1700000001.0));
        assert_eq!(
            fs::read_to_string(live)?,
            body,
            "refresh never needs a transcript append"
        );
        Ok(())
    }

    #[test]
    fn checked_discovery_kimi_matches_owned_segments_titles_and_wire_bounds() -> Result<()> {
        let world = cv_testkit::World::in_process("kimi-discovery-selected");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir)?;
        let live = dir.join("context.jsonl");
        let body = "body λ\n".repeat(512 * 1024);
        let prompt = serde_json::json!({"role":"user","content":[{"text":""},{"text":body}]}).to_string();
        let prefix = format!("\u{2003}{prompt}\r\n\ninvalid-json\n");
        let mut rotated = prefix.as_bytes().to_vec();
        rotated.extend_from_slice(b"\xff\n{\"role\":\"assistant\",\"content\":\"old\"}\n");
        fs::write(dir.join("context_1.jsonl"), rotated)?;
        let mut current = prefix.as_bytes().to_vec();
        current.extend_from_slice(b"\xff\n{\"ignored\":1e999}\n{\"role\":\"assistant\",\"content\":\"new\"}\n{\"role\":\"user\",\"content\":\"partial\"}");
        current.extend_from_slice(b"\n{\"role\":\"assistant\",\"role\":\"tool\",\"content\":\"duplicate role\"}\n");
        fs::write(&live, current)?;
        fs::write(
            dir.join("wire.jsonl"),
            format!(
                "{{\"timestamp\":1,\"ignored\":{}}}\n{{\"timestamp\":9}}\n",
                serde_json::to_string(&body)?
            ),
        )?;
        for title in [
            Value::Null,
            serde_json::json!(" custom λ "),
            serde_json::json!("\u{2003}".repeat(8192)),
            serde_json::json!([{"text":"invalid title type"}]),
        ] {
            fs::write(
                dir.join("state.json"),
                serde_json::to_vec(&serde_json::json!({"custom_title":title,"unused":body}))?,
            )?;
            let actual = scan("session", &live, &dir, Some("/p".into()));
            let expected = old_scan("session", &live, &dir, Some("/p".into()));
            assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
        }
        let mut file = fs::OpenOptions::new().append(true).open(dir.join("wire.jsonl"))?;
        use std::io::Write;
        file.write_all(b"\xff")?;
        assert_eq!(
            wire_time_bounds(&dir.join("wire.jsonl")),
            old_wire_time_bounds(&dir.join("wire.jsonl"))
        );
        for entry in walkdir::WalkDir::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi"))
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if entry.file_name() != "context.jsonl" {
                continue;
            }
            let dir = entry.path().parent().unwrap();
            let id = dir.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                serde_json::to_value(scan(id, entry.path(), dir, None))?,
                serde_json::to_value(old_scan(id, entry.path(), dir, None))?
            );
        }
        Ok(())
    }

    #[derive(Default)]
    pub(super) struct ResumeKeep {
        pub(super) messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for ResumeKeep {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.push(message);
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), cp.clone()));
        }
    }
    pub(super) fn old_context_oracle(adapter: &Kimi, r: &SessionRef, opts: &ParseOptions) -> Session {
        let (transcript, wire) = locate_transcript(&r.path, &r.id);
        let segments = transcript_segments(&r.path, &transcript);
        let (s, enrich) = session_header(adapter, r, opts, &segments, wire.as_deref());
        let mut kept = ResumeKeep::default();
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Kimi, &mut kept);
        let mut s = old_context_pass(s, &segments, &enrich, opts, &mut spawns);
        s.messages = kept.messages;
        s.materialize().unwrap();
        s
    }
    fn materialized(messages: &[Message], r: &SessionRef) -> Value {
        let resolver = crate::Resolver::new(Some(r.path.clone()));
        let mut messages = messages.to_vec();
        for message in &mut messages {
            message.materialize(&resolver).unwrap();
        }
        serde_json::to_value(messages).unwrap()
    }

    #[test]
    fn resume_corpus_replays_usage_rotation_prefixes_and_exact_suffixes() {
        let world = cv_testkit::World::in_process("kimi_resume_corpus");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/kimi");
        let mut files = 0;
        let mut resumes = 0;
        for file in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if file.file_name() != "context.jsonl" {
                continue;
            }
            let dir = file.path().parent().unwrap();
            let id = dir.file_name().unwrap().to_str().unwrap();
            let r = SessionRef::new(id, Harness::Kimi, dir);
            let home = dir
                .ancestors()
                .find(|p| p.file_name().is_some_and(|n| n == ".kimi"))
                .unwrap();
            let adapter = Kimi::with_home(home);
            assert_eq!(adapter.resume_source(&r), file.path());
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::lazy_extra(),
                ParseOptions::full(),
                ParseOptions::complete(),
            ] {
                let old = old_context_oracle(&adapter, &r, &opts);
                let mut full = ResumeKeep::default();
                let mut shell = adapter.try_stream(&r, &opts, &mut full).unwrap();
                shell.messages = full.messages.clone();
                shell.materialize().unwrap();
                assert_eq!(
                    serde_json::to_value(&shell).unwrap(),
                    serde_json::to_value(&old).unwrap(),
                    "{} old full collector",
                    file.path().display()
                );
                shell.messages.clear();
                let stride = (full.checkpoints.len() / 12).max(1);
                for (i, (count, cp)) in full.checkpoints.iter().enumerate().step_by(stride) {
                    let mut rest = ResumeKeep::default();
                    let suffix = adapter.try_resume(&r, &opts, &mut rest, cp).unwrap().unwrap();
                    assert_eq!(
                        serde_json::to_value(suffix).unwrap(),
                        serde_json::to_value(&shell).unwrap(),
                        "{} @{} metadata",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        materialized(&rest.messages, &r),
                        materialized(&full.messages[*count..], &r),
                        "{} @{} suffix",
                        file.path().display(),
                        cp.offset
                    );
                    assert_eq!(
                        rest.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
                        full.checkpoints[i + 1..].iter().map(|(_, cp)| cp).collect::<Vec<_>>()
                    );
                    resumes += 1;
                }
            }
            files += 1;
        }
        assert!(
            files >= 4 && resumes >= 20,
            "native files={files} suffix resumes={resumes}"
        );
        drop(world);
        eprintln!("kimi native files={files} exact live-file suffix resumes={resumes}");
    }

    #[test]
    fn resume_anchors_replay_large_pending_messages_and_usage_without_payload_state() {
        use std::io::Write as _;
        let world = cv_testkit::World::in_process("kimi_pending_anchor");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(home.join("config.toml"), "default_model = \"kimi-test\"\n").unwrap();
        let line = "{\"role\":\"user\",\"content\":\"hello\"}\n";
        let giant = serde_json::json!({"role":"assistant","content":"pending-giant-reply".repeat(100_000),"tool_calls":[{"id":"call","type":"function","function":{"name":"Read","arguments":"{}"}}]});
        let body = format!(
            "{}bad json\n{}\n{{\"role\":\"_usage\",\"token_count\":77}}",
            line.repeat(20_000),
            giant
        );
        fs::write(dir.join("context.jsonl"), &body).unwrap();
        let mut r = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&home);
        let opts = ParseOptions::lazy();
        let mut first = ResumeKeep::default();
        adapter.try_stream(&r, &opts, &mut first).unwrap();
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        assert_eq!(cp.offset as usize, line.len() * 20_000 + "bad json\n".len());
        let state = serde_json::to_string(cp).unwrap();
        assert!(state.len() < 1024 && !state.contains("pending-giant-reply"));
        assert_eq!(
            first.messages.last().unwrap().usage.as_ref().unwrap().input_tokens,
            Some(77)
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(adapter.resume_source(&r))
            .unwrap();
        writeln!(file).unwrap();
        file.write_all(line.repeat(200).as_bytes()).unwrap();
        drop(file);
        r.title = Some("new catalog title".into());
        let mut suffix = ResumeKeep::default();
        let mut shell = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
        assert_eq!(suffix.messages.len(), 201);
        assert_eq!(shell.title, r.title);
        assert_eq!(shell.model.as_deref(), Some("kimi-test"));
        shell.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        shell.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(old_context_oracle(&adapter, &r, &opts)).unwrap()
        );
        // Adding a rotation changes the dedup/branch context and declines before callbacks.
        fs::write(dir.join("context_1.jsonl"), line).unwrap();
        let mut none = ResumeKeep::default();
        assert!(adapter.try_resume(&r, &opts, &mut none, cp).unwrap().is_none());
        assert!(none.messages.is_empty());
        eprintln!(
            "kimi stable_prefix=20000 appended_callbacks=201 whole_callbacks=20201 checkpoint_bytes={}",
            state.len()
        );
    }

    #[test]
    fn resume_rotated_prefix_branch_and_partial_eof_match_old_acceptance() {
        use std::io::Write as _;
        let world = cv_testkit::World::in_process("kimi_rotated_anchor");
        let home = world.home.join(".kimi");
        let dir = home.join("sessions/hash/session");
        fs::create_dir_all(&dir).unwrap();
        let old = concat!(
            "{\"role\":\"_system_prompt\",\"content\":\"system\"}\n",
            "{\"role\":\"user\",\"content\":\"old\"}\n",
            "{\"role\":\"_usage\",\"token_count\":1}\n"
        );
        fs::write(dir.join("context_1.jsonl"), old).unwrap();
        let body = format!("{old}{{\"role\":\"user\",\"content\":\"new\"}}\r\n{{\"role\":\"_checkpoint\",\"id\":\"point\"}}\n{{\"role\":\"assistant\",\"content\":\"answer\"}}\n{{\"role\":\"_usage\",\"token_count\":9");
        fs::write(dir.join("context.jsonl"), body).unwrap();
        let r = SessionRef::new("session", Harness::Kimi, &dir);
        let adapter = Kimi::with_home(&home);
        for opts in [ParseOptions::lazy(), ParseOptions::complete()] {
            let mut first = ResumeKeep::default();
            let mut full = adapter.try_stream(&r, &opts, &mut first).unwrap();
            full.messages = first.messages.clone();
            full.materialize().unwrap();
            assert_eq!(
                serde_json::to_value(&full).unwrap(),
                serde_json::to_value(old_context_oracle(&adapter, &r, &opts)).unwrap()
            );
            for (count, cp) in &first.checkpoints {
                let mut suffix = ResumeKeep::default();
                let shell = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
                full.messages.clear();
                assert_eq!(
                    serde_json::to_value(shell).unwrap(),
                    serde_json::to_value(&full).unwrap()
                );
                assert_eq!(
                    materialized(&suffix.messages, &r),
                    materialized(&first.messages[*count..], &r)
                );
            }
        }
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(adapter.resume_source(&r))
                .unwrap(),
            "}}"
        )
        .unwrap();
        let mut bytes = fs::read(adapter.resume_source(&r)).unwrap();
        bytes.extend_from_slice(b"\xff\n{\"role\":\"user\",\"content\":\"tail\"}\n");
        fs::write(adapter.resume_source(&r), bytes).unwrap();
        let opts = ParseOptions::complete();
        let mut kept = ResumeKeep::default();
        let mut shell = adapter.try_stream(&r, &opts, &mut kept).unwrap();
        shell.messages = kept.messages;
        shell.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(shell).unwrap(),
            serde_json::to_value(old_context_oracle(&adapter, &r, &opts)).unwrap()
        );
        // Unsupported non-native layout still reads faithfully but never offers a wrong-file mark.
        let flat = home.join("sessions/legacy");
        fs::create_dir_all(&flat).unwrap();
        fs::write(
            flat.join("id.jsonl"),
            "{\"role\":\"user\",\"content\":\"one\"}\n{\"role\":\"user\",\"content\":\"two\"}\n",
        )
        .unwrap();
        let legacy = SessionRef::new("id", Harness::Kimi, &flat);
        let mut kept = ResumeKeep::default();
        adapter.try_stream(&legacy, &opts, &mut kept).unwrap();
        assert!(!kept.checkpoints.is_empty());
        assert_eq!(adapter.resume_source(&legacy), flat.join("id.jsonl"));
    }

    #[test]
    fn plain_wire_output_span_borrows_without_decoding() {
        let world = cv_testkit::World::new("kimi-wire-plain");
        let path = world.base.join("wire.jsonl");
        let output = "plain 雪".repeat(2000);
        fs::write(
            &path,
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"plain", "return_value":{"output":output}
            }}})
            .to_string(),
        )
        .unwrap();
        let e = read_wire_enrichment(&path, true);
        let text = e.output(&e.tool_results["plain"]);
        assert!(!text.as_span().unwrap().escaped);
        let resolver = crate::lazy::Resolver::new(None);
        let resolved = text.resolve(&resolver);
        assert_eq!(resolved.as_ref(), output);
        #[cfg(feature = "mmap")]
        assert!(matches!(resolved, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn wire_ranges_match_value_enrichment_oracle() {
        let world = cv_testkit::World::new("kimi-wire-ranges");
        let path = world.base.join("wire.jsonl");
        let giant = "雪\n\"\\".repeat(3000);
        let mut lines = vec![
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"giant", "return_value":{"output":giant, "extras":{"rich": [1, true]}}
            }}}).to_string(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"dup","return_value":{"output":"old","extras":{"old":1}}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"dup","return_value":{"output":"first","outp\u0075t":9,"message":"fallback","is_error":true,"extras":null}}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"bad","return_value":{"output":"must not win"}},"ignored":"\ud800"}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"bad-number","return_value":{"output":"must not win"}},"ignored":1e400}}"#.into(),
            r#"{"type":"metadata","message":{"type":"ToolResult","payload":{"tool_call_id":"header","return_value":{"output":"ignored"}}}}"#.into(),
            r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"empty","return_value":{"output":null,"message":false,"extras":42}}}}"#.into(),
            r#"{"timestamp":1700000000,"message":{"type":"TurnBegin","payload":{}}}"#.into(),
            r#"{"timestamp":1700000001,"message":{"type":"StatusUpdate","payload":{"message_id":"reply","token_usage":null}}}"#.into(),
            r#"{"message":{"type":"StatusUpdate","payload":false}}"#.into(),
        ];
        lines.push(format!(r#"{{"message":{{"type":"ToolResult","payload":{{"tool_call_id":"too-deep","return_value":{{"output":"ignored"}}}},"ignored":{}0{}}}}}"#, "[".repeat(128), "]".repeat(128)));
        let text = format!(" \n{}\ninvalid\n", lines.join("\n"));
        fs::write(&path, &text).unwrap();
        let mut expected = HashMap::new();
        super::super::for_each_json_line_str(&text, |v| {
            if v.get("type").and_then(Value::as_str) != Some("metadata")
                && v.pointer("/message/type").and_then(Value::as_str) == Some("ToolResult")
            {
                let p = &v["message"]["payload"];
                let id = p.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if !id.is_empty() {
                    let rv = &p["return_value"];
                    let output = rv
                        .get("output")
                        .and_then(Value::as_str)
                        .or_else(|| rv.get("message").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    expected.insert(
                        id.to_string(),
                        (
                            output,
                            rv.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                            rv.get("extras").filter(|x| !x.is_null()).cloned(),
                        ),
                    );
                }
            }
            Flow::Continue
        });
        for spans in [false, true] {
            let e = read_wire_enrichment(&path, spans);
            assert_eq!(e.tool_results.len(), expected.len());
            let resolver = crate::lazy::Resolver::new(None);
            for (id, (output, is_error, extras)) in &expected {
                let result = &e.tool_results[id];
                let actual = e.output(result);
                assert_eq!(actual.resolve(&resolver).as_ref(), output, "{id}");
                assert_eq!(result.is_error, *is_error);
                assert_eq!(e.extras(result), *extras);
                assert_eq!(actual.is_span(), spans && id == "giant");
                if let Some(span) = actual.as_span() {
                    assert_eq!(span.source.as_ref(), Some(&path));
                }
            }
            assert_eq!(e.statuses.len(), 2);
            assert_eq!(e.statuses[0].message_id.as_deref(), Some("reply"));
            assert!(
                e.statuses[0].usage.is_some(),
                "present null token_usage is default Usage"
            );
            assert!(e.statuses[1].usage.is_none());
            assert_eq!(e.first_timestamp, epoch_to_dt(1700000000.));
        }
        assert_eq!(
            wire_time_bounds(&path),
            Some((epoch_to_dt(1700000000.).unwrap(), epoch_to_dt(1700000001.).unwrap()))
        );
        // UTF-8 failure anywhere discards even the earlier valid enrichment, like read_to_string.
        let mut invalid = text.into_bytes();
        invalid.extend_from_slice(b"\xff\n");
        fs::write(&path, invalid).unwrap();
        assert!(read_wire_enrichment(&path, false).tool_results.is_empty());
        assert!(wire_time_bounds(&path).is_none());
    }

    #[test]
    fn wire_spans_preserve_all_stream_modes() {
        let world = cv_testkit::World::in_process("kimi-wire-modes");
        let dir = world.base.join("session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("context.jsonl"),
            r#"{"role":"_checkpoint","id":0}
{"role":"user","content":"question"}
{"role":"assistant","content":"answer"}
{"role":"tool","tool_call_id":"selected","content":"fallback"}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("wire.jsonl"),
            serde_json::json!({"message":{"type":"ToolResult","payload":{
                "tool_call_id":"selected","return_value":{"output":"雪\n\"\\".repeat(3000),"extras":{"k":"v"}}
            }}})
            .to_string(),
        )
        .unwrap();
        let adapter = Kimi::with_home(&world.base);
        let r = SessionRef::new("session", Harness::Kimi, dir);
        let mut lean = None;
        for opts in [
            ParseOptions::full(),
            ParseOptions::bulk(),
            ParseOptions::lazy(),
            ParseOptions::lazy_extra(),
            ParseOptions::complete(),
            ParseOptions {
                spans: true,
                ..ParseOptions::complete()
            },
        ] {
            let mut sink = crate::stream::CollectSink::default();
            let mut session = adapter.stream(&r, &opts, &mut sink).unwrap();
            session.messages = sink.messages;
            assert_eq!(session.messages.len(), if opts.complete { 4 } else { 3 });
            let tool = session.messages.last().unwrap();
            let Block::ToolResult { content, .. } = &tool.content[0] else {
                panic!("tool result")
            };
            assert_eq!(content.is_span(), opts.spans);
            session.materialize().unwrap();
            session.messages.retain(|m| m.kind != MessageKind::Carrier);
            let value = serde_json::to_value(session).unwrap();
            if let Some(expected) = &lean {
                assert_eq!(&value, expected);
            } else {
                lean = Some(value);
            }
        }
    }

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
    fn model_config_uses_top_level_toml_semantics_in_session_headers() -> Result<()> {
        let world = cv_testkit::World::in_process("kimi_toml_model");
        let home = world.home.join(".kimi");
        let session = home.join("sessions/hash/session");
        fs::create_dir_all(&session)?;
        fs::write(
            session.join("context.jsonl"),
            "{\"role\":\"user\",\"content\":\"prompt\"}\n",
        )?;
        let config = home.join("config.toml");
        let adapter = Kimi::with_home(&home);
        let reference = SessionRef::new("session", Harness::Kimi, &session);
        for (text, expected) in [
            (
                "default_model_provider = 'unused'\ndefault_model = 'actual' # comment\n",
                Some("actual"),
            ),
            ("\"default_model\" = \"escaped\\u002fmodel\"\n", Some("escaped/model")),
            ("[profiles.demo]\ndefault_model = 'nested'\n", None),
            ("default_model = ''\n", None),
            ("default_model = 42\n", None),
            ("default_model = 'before'\ninvalid TOML\n", None),
        ] {
            fs::write(&config, text)?;
            assert_eq!(default_model_from_config(&config).as_deref(), expected);
            let mut sink = |_: Message| Flow::Continue;
            assert_eq!(
                adapter
                    .stream(&reference, &ParseOptions::full(), &mut sink)?
                    .model
                    .as_deref(),
                expected
            );
        }
        Ok(())
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
                assert_eq!(input.resolve().unwrap()["command"], "cat .gitmodules");
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
        let temp = cv_testkit::World::new("kimi-wire-enrich");
        let wire = temp.base.join("wire.jsonl");
        fs::write(&wire, r#"{"message":{"type":"ToolResult","payload":{"tool_call_id":"tool_1","return_value":{"is_error":true,"output":"boom","extras":{"k":"v"}}}}}"#).unwrap();
        let enrich = read_wire_enrichment(&wire, false);
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
        let r = SessionRef::new(id, Harness::Kimi, dir.clone());
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
        let comps = crate::compaction::detect_in_session(&s, true).unwrap();
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
            cwd: Some(PathBuf::from("/proj")),
            ..SessionRef::new("11111111-1111-1111-1111-111111111111", Harness::Kimi, dir.clone())
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
            cwd: Some(PathBuf::from("/proj")),
            ..SessionRef::new("11111111-1111-1111-1111-111111111111", Harness::Kimi, dir.clone())
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
        let r = SessionRef::new(id, Harness::Kimi, dir.clone());
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
    /// `/compact` and `/clear` rotate `context.jsonl` into `context_N.jsonl`; the whole history is
    /// read, oldest segment first, the repeated head (system prompt, checkpoint 0) once, with a
    /// compaction boundary before a summary and a branch before a cleared segment.
    #[test]
    fn rotated_segments_read_in_order_with_their_boundaries() {
        let sp = r#"{"role":"_system_prompt","content":"You are Kimi."}"#;
        let c0 = r#"{"role":"_checkpoint","id":0}"#;
        let dir = write_temp(
            "context_1.jsonl",
            &[
                sp,
                c0,
                r#"{"role":"user","content":"first"}"#,
                r#"{"role":"assistant","content":"one"}"#,
            ]
            .join("\n"),
        );
        fs::write(
            dir.join("context_2.jsonl"),
            [
                sp,
                c0,
                r#"{"role":"user","content":[{"type":"text","text":"<system>Previous context has been compacted. Here is the compaction output:</system>"},{"type":"text","text":"we said one"}]}"#,
                r#"{"role":"user","content":"second"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            dir.join("context.jsonl"),
            [sp, c0, r#"{"role":"user","content":"third"}"#].join("\n"),
        )
        .unwrap();
        let id = "s1";
        let r = scan(id, &dir.join("context.jsonl"), &dir, None).unwrap();
        assert_eq!(
            r.title.as_deref(),
            Some("first"),
            "the title is the first prompt of the first segment"
        );
        let k = Kimi {
            sessions: None,
            root: None,
        };
        let s = k.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::SystemPrompt,
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::CompactionBoundary,
                MessageKind::CompactionSummary,
                MessageKind::Prompt,
                MessageKind::Branch,
                MessageKind::Prompt,
            ]
        );
        assert_eq!(
            s.messages[6]
                .harness_extra(Harness::Kimi)
                .and_then(|x| x.get("segment")),
            Some(&Value::String("context.jsonl".into()))
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// kimi-cli's `Agent` tool keeps each sub-agent under `<session>/subagents/<agent_id>/` in the
    /// session's own format; it parses as its own session under the parent.
    #[test]
    fn subagent_discovery_reuses_histories_and_refreshes_auxiliary_and_parent_metadata() -> Result<()> {
        let world = cv_testkit::World::in_process("kimi-subagent-cache");
        let context = crate::discover_cache::read::Context::current();
        let _scope = context.enter();
        let parent = world.home.join("parent");
        let mut main = SessionRef::new("parent-id", Harness::Kimi, &parent);
        main.cwd = Some(PathBuf::from("/first"));
        let body = "{\"role\":\"user\",\"content\":\"child task\"}\n".repeat(128);
        for index in 0..12 {
            let child = parent.join("subagents").join(format!("agent-{index:02}"));
            fs::create_dir_all(&child)?;
            fs::write(child.join("context.jsonl"), &body)?;
        }
        let first = subagent_refs(&main);
        assert_eq!(first.len(), 12);
        for reference in &first {
            assert_eq!(reference.message_count, 128);
            assert_eq!(reference.cwd, main.cwd);
            let hit = crate::discover_cache::cached_scan(&reference.path.join("context.jsonl"), || {
                panic!("unchanged child context must be cached")
            })
            .unwrap();
            assert_eq!(hit.message_count, reference.message_count);
        }
        main.id = "new-parent".into();
        main.cwd = Some(PathBuf::from("/second"));
        let unchanged = subagent_refs(&main);
        assert!(unchanged
            .iter()
            .all(|reference| reference.id.starts_with("new-parent/") && reference.cwd == main.cwd));
        let child = &unchanged[0].path;
        fs::write(child.join("meta.json"), r#"{"description":"new task description"}"#)?;
        assert_eq!(subagent_refs(&main)[0].title.as_deref(), Some("new task description"));
        fs::write(
            child.join("context_1.jsonl"),
            "{\"role\":\"assistant\",\"content\":\"older\"}\n",
        )?;
        assert_eq!(subagent_refs(&main)[0].message_count, 129);
        fs::write(
            child.join("wire.jsonl"),
            "{\"timestamp\":1700000000}\n{\"timestamp\":1700000001}\n",
        )?;
        let refreshed = subagent_refs(&main);
        assert_eq!(refreshed[0].created_at, epoch_to_dt(1700000000.0));
        assert_eq!(refreshed[0].updated_at, epoch_to_dt(1700000001.0));
        assert_eq!(fs::read_to_string(child.join("context.jsonl"))?, body);
        Ok(())
    }

    #[test]
    fn subagent_dirs_parse_with_lineage() {
        let dir = write_temp("context.jsonl", r#"{"role":"user","content":"parent"}"#);
        let sub = dir.join("subagents").join("a1b2");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("context.jsonl"), r#"{"role":"user","content":"child task"}"#).unwrap();
        fs::write(
            sub.join("meta.json"),
            r#"{"agent_id":"a1b2","subagent_type":"coder","description":"do it"}"#,
        )
        .unwrap();
        let main = scan("p1", &dir.join("context.jsonl"), &dir, None).unwrap();
        let refs = subagent_refs(&main);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "p1/a1b2");
        assert_eq!(refs[0].title.as_deref(), Some("do it"));
        let s = Kimi {
            sessions: None,
            root: None,
        }
        .parse(&refs[0])
        .unwrap();
        assert_eq!(s.lineage.agent_path.as_deref(), Some("a1b2"));
        assert_eq!(s.lineage.parent.as_deref(), dir.file_name().and_then(|n| n.to_str()));
        assert_eq!(
            s.harness_extra(Harness::Kimi)
                .and_then(|x| x.get("subagent").and_then(|a| a.get("subagent_type"))),
            Some(&Value::String("coder".into()))
        );
        fs::remove_dir_all(&dir).ok();
    }

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
