//! Codex CLI adapter — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (+ archived, + 2025 legacy JSON).
//!
//! Format drift handled: the first line is either a `session_meta` record, a bare `{id,timestamp}`
//! header, or (2025 legacy) a single `{session, items[]}` JSON document. Natural-language text
//! appears both as `event_msg` and as `response_item message`; when `event_msg`s are present we take
//! NL text from them and skip the `response_item` duplicates.
//!
//! Record types (ground truth: codex-rs `history::RolloutItem` + `protocol::EventMsg`, verified
//! against Codex `132c2be23`, CLI 0.154):
//! - `session_meta` — id, cwd, originator, cli_version, source, model_provider, git, plus (since
//!   0.147) the thread's place in a swarm: `session_id` (root thread), `parent_thread_id`,
//!   `forked_from_id`, `thread_source`, `agent_nickname`/`agent_path`/`agent_role`, `history_mode`
//!   and `subagent_history_start_ordinal`. Every line carries an `ordinal` in paginated files.
//! - `turn_context`  — per-turn model/effort/cwd/approval/sandbox; we track model drift.
//! - `response_item` — the model-visible items: `message`, `reasoning`, `function_call`,
//!   `function_call_output`, `custom_tool_call(_output)`, `local_shell_call`, `web_search_call`,
//!   `tool_search_call`/`tool_search_output`, `image_generation_call`, `compaction`, and the
//!   inter-agent `agent_message` (paired with a preceding `inter_agent_communication_metadata`).
//! - `event_msg`     — UI-side events. Legacy history mode (CLI ≤ 0.147) echoes NL text as
//!   `user_message`/`agent_message`; paginated mode (0.147+) drops those echoes and instead records
//!   every finished `TurnItem` as `item_completed` (command executions, file changes, MCP calls,
//!   image views, sub-agent activity, plans, …). Both modes carry `token_count` (usage + rate
//!   limits), `thread_settings_applied`, `task_started`/`task_complete`, `turn_aborted`,
//!   `thread_rolled_back`.
//! - `token_usage_record` — per-response usage keyed by `response_id`/`turn_id` (paginated).
//! - `compacted`     — a top-level record marking an auto/manual history compaction boundary.
//!
//! Fork/subagent rollouts embed the parent's history up to the fork point (ordinals below
//! `subagent_history_start_ordinal`, stamped with the FORK time); the lean passes skip those
//! records (the thread's own work is what a listing/title/search should show) and take a message's
//! real time from `internal_chat_message_metadata_passthrough.create_time` when present.
//!
//! Every message carries a precise `kind`/`origin` (INTERFACE-V2 §4): a human prompt vs. Codex's
//! injected preambles, swarm `agent_message`s from `Origin::Subagent`, `item_completed` notes,
//! compaction boundaries, model/effort changes, rollbacks (`Branch`), persisted errors, and — under
//! `ParseOptions::complete` — verbatim `Carrier`s for every record with no conversational shape.
//! Harness-specific facts (rate limits, turn ids, the structured `item`, swarm routing) live in
//! `extra["codex"]`, the only bag this adapter writes; images become `Block::Image` references.

mod detect;
mod emit;
mod lean;
mod metadata;
pub use emit::emit;

use super::wire::{openai, text_parts};
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::lazy::{json_string_span, RawValue, Text};
use crate::stream::{Checkpoint, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// The one door into this adapter's per-message facts: `extra["codex"]` (INTERFACE-V2 §4).
fn bag(m: &mut Message) -> &mut Map<String, Value> {
    m.harness_extra_mut(Harness::Codex)
}

/// The session-level fact bag, `Session::extra["codex"]`.
fn sbag(s: &mut Session) -> &mut Map<String, Value> {
    s.harness_extra_mut(Harness::Codex)
}

/// Read a fact from a message's `extra["codex"]` bag.
fn bag_get<'a>(m: &'a Message, key: &str) -> Option<&'a Value> {
    m.harness_extra(Harness::Codex).and_then(|b| b.get(key))
}

/// Codex's injected preambles — text the HARNESS put in the user turn, not the user's own words:
/// the environment/user-instructions envelopes (0.1xx), the internal-context bundle, the plugin
/// catalog, the AGENTS.md dump and the @-mention file bundle (0.147+). Such a user message is
/// [`MessageKind::InjectedContext`] from [`Origin::Harness`], and never a title.
fn is_injected_text(t: &str) -> bool {
    let trimmed = t.trim_start();
    [
        "<environment_context",
        "<user_instructions",
        "<codex_internal_context",
        "<recommended_plugins",
        "# Codex CLI",
        "# AGENTS.md instructions",
        "# Files mentioned by the user",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
}

/// A harness-side note: `Role::System`, `Origin::Harness`, the given kind, one text block.
fn note(kind: MessageKind, ts: Option<DateTime<Utc>>, text: String) -> Message {
    let mut m = Message::of_kind(Role::System, kind, Origin::Harness);
    m.timestamp = ts;
    m.content.push(Block::Text { text: text.into() });
    m
}

/// Under `ParseOptions::complete`, a record with no conversational shape — one no arm knows, or a
/// lifecycle marker the lean passes drop — becomes a [`MessageKind::Carrier`]: the verbatim line
/// under the top-level `_record` key and its type in `extra["codex"]["record_type"]`, so a
/// same-harness round-trip loses nothing and `cv formats` can count what the adapter does not
/// model. The type is spelled as `formats/codex.toml` names it: the top-level `type`, qualified by
/// `payload.type` for the two envelopes that have one (`event_msg.task_started`,
/// `response_item.additional_tools`) — a bare `event_msg` would match no manifest entry.
fn carry_record(v: &Value, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>) {
    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
    m.timestamp = ts;
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let ty = match v.pointer("/payload/type").and_then(Value::as_str) {
        Some(sub) if matches!(ty, "event_msg" | "response_item") => format!("{ty}.{sub}"),
        _ => ty.to_string(),
    };
    bag(&mut m).insert("record_type".into(), Value::String(ty));
    m.extra.insert(super::claude::CARRIER_KEY.into(), v.clone());
    out.push(m);
}

pub struct Codex {
    roots: Vec<PathBuf>,
}

impl Codex {
    pub fn detect() -> Self {
        // Codex resolves its home as `$CODEX_HOME`, else `~/.codex` (codex-rs/utils/home-dir/src/lib.rs
        // `find_codex_home`); honour the same override so a test home (or a relocated store) is read.
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
        codex_home.map_or(Codex { roots: Vec::new() }, |c| Codex::with_home(&c))
    }

    /// An adapter over an explicit Codex home (the dir holding `sessions/`), instead of
    /// `$CODEX_HOME` / `~/.codex` — for the harness corpus and other tests, without mutating the
    /// process environment.
    pub fn with_home(codex_home: &Path) -> Self {
        let roots = [codex_home.join("sessions"), codex_home.join("archived_sessions")]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        Codex { roots }
    }
}

impl Adapter for Codex {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // Walk (cheap) to collect candidate paths, then scan (file read + parse) them in parallel.
        let mut paths = Vec::new();
        for root in &self.roots {
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("rollout-") {
                    continue;
                }
                // `.jsonl.zst`: Codex compresses rollouts colder than 7 days in place
                // (rollout/src/compression.rs); `.tmp` are its staging files. The zstd decoder rides
                // the `sqlite` feature (ruzstd), so compressed rollouts are only listed when we can
                // read them.
                if !name.ends_with(".jsonl") && !name.ends_with(".json") && !is_zst_rollout(name) {
                    continue;
                }
                paths.push(path.to_path_buf());
            }
        }
        Ok(crate::par_filter_map(paths, |path| {
            crate::discover_cache::cached_scan(&path, || match scan(&path) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("cv: skipping {}: {e:#}", path.display());
                    None
                }
            })
        }))
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let is_jsonl = is_jsonl_path(&r.path);
            if !is_jsonl {
                // 2025 legacy layout is a single JSON document — inherently whole-file. Parse it whole
                // and replay (these sessions are rare and small).
                let text = read_rollout_to_string(&r.path)?;
                return crate::stream::replay(parse_str(&r.id, &text, false, Some(r.path.clone())), sink);
            }
            // Modern `.jsonl` rollout: stream it. `has_events` is a whole-file property (do NL text
            // come from `event_msg`s or `response_item`s?), so we make a cheap first pass to detect it,
            // then a second streaming pass that emits one record's messages at a time. The native
            // lazy path borrows both passes from one mapping, avoiding a giant private line buffer.
            // Span path (partial-access / chunked index): mmap the file and emit a lazy `Span` for a giant
            // `function_call_output` string output instead of reading/materializing the 100s-of-MB line.
            // Never for a compressed rollout — a span can't point into zstd frames.
            #[cfg(feature = "mmap")]
            if opts.spans && !is_zst_path(&r.path) {
                if let Ok(file) = fs::File::open(&r.path) {
                    if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
                        // One forward pass: tell the kernel, so it reads ahead in big runs instead of
                        // faulting the file in a page at a time. Measured on a 12-thread recount of
                        // 88 GB of rollouts: 41-44 s with this hint, 58-59 s without; our own read-ahead on
                        // top (`madvise(WILLNEED)` or `F_RDADVISE`, 32 MB ahead of the parser) made it
                        // 150-180 s, and `WILLNEED` on the whole file pages all of it in for a reader
                        // that stops at the first hit.
                        #[cfg(unix)]
                        let _ = map.advise(memmap2::Advice::Sequential);
                        let has_events = detect_has_events_bytes(&map);
                        return Ok(stream_jsonl_spans_with_options(
                            &r.id,
                            &map,
                            Some(r.path.clone()),
                            has_events,
                            opts,
                            sink,
                        ));
                    }
                }
            }
            let has_events = detect_has_events(open_rollout(&r.path)?);
            Ok(stream_jsonl(
                &r.id,
                open_rollout(&r.path)?,
                Some(r.path.clone()),
                has_events,
                opts,
                sink,
            ))
        })
    }

    fn resume_format(&self) -> Option<u32> {
        Some(RESUME_FORMAT)
    }

    /// Plain JSONL resumes from a physical record boundary using the same parser transport and
    /// option flavor as its checkpoint. Compressed and legacy documents have no physical JSONL
    /// seek position. The whole-file `has_events` decision is revalidated before any callbacks.
    fn resume(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        from: &Checkpoint,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            if is_zst_path(&r.path) || !is_jsonl_path(&r.path) {
                return Ok(None);
            }
            let Some(state) = from.get::<ResumeState>(crate::stream::ADAPTER_LAYER) else {
                return Ok(None);
            };
            if state.reading.flavor != Flavor::of(opts)
                || state.session.source_path.as_deref() != Some(r.path.as_path())
            {
                return Ok(None);
            }
            if state.reading.buffered {
                let mut file = fs::File::open(&r.path).with_context(|| format!("opening {}", r.path.display()))?;
                if file.metadata()?.len() < from.offset || detect_has_events(open_rollout(&r.path)?) != state.has_events
                {
                    return Ok(None);
                }
                file.seek(SeekFrom::Start(from.offset))?;
                return Ok(Some(stream_jsonl_at(
                    &r.id,
                    BufReader::new(file),
                    state,
                    from.offset,
                    sink,
                )));
            }
            #[cfg(feature = "mmap")]
            {
                if !opts.spans {
                    return Ok(None);
                }
                let file = fs::File::open(&r.path).with_context(|| format!("opening {}", r.path.display()))?;
                let map =
                    unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mapping {}", r.path.display()))?;
                if (map.len() as u64) < from.offset {
                    return Ok(None);
                }
                if detect_has_events_bytes(&map) != state.has_events {
                    return Ok(None);
                }
                #[cfg(unix)]
                let _ = map.advise(memmap2::Advice::Sequential);
                Ok(Some(stream_jsonl_spans_at(
                    &r.id,
                    &map,
                    state,
                    from.offset,
                    opts.offsets,
                    sink,
                )))
            }
            #[cfg(not(feature = "mmap"))]
            {
                let _ = (r, opts, sink, from);
                Ok(None)
            }
        })
    }
}

/// What a checkpoint's adapter state means: bump when [`ResumeState`], [`CodexCtx`], or what a
/// record parses to, changes.
const RESUME_FORMAT: u32 = 2;

/// Whole state between records, when no message awaits a following usage record.
#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState {
    session: Session,
    ctx: CodexCtx,
    has_events: bool,
    meta_sent: bool,
    skipped: u64,
    reading: Reading,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Flavor {
    extra: bool,
    spans: bool,
    offsets: bool,
    complete: bool,
}

impl Flavor {
    fn of(opts: &ParseOptions) -> Self {
        Self {
            extra: opts.extra,
            spans: opts.spans,
            offsets: opts.offsets,
            complete: opts.complete,
        }
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Reading {
    buffered: bool,
    flavor: Flavor,
}

impl ResumeState {
    fn new(source_path: Option<PathBuf>, has_events: bool, opts: &ParseOptions, buffered: bool) -> Self {
        Self {
            session: Session {
                source_path,
                ..Session::new(String::new(), Harness::Codex)
            },
            ctx: if buffered {
                CodexCtx::for_opts(opts)
            } else {
                CodexCtx::default()
            },
            has_events,
            meta_sent: false,
            skipped: 0,
            reading: Reading {
                buffered,
                flavor: Flavor::of(opts),
            },
        }
    }
}

/// A `rollout-*.jsonl.zst`: Codex's in-place compression of cold rollouts (readable only with the
/// `sqlite` feature, which brings the pure-Rust ruzstd decoder).
fn is_zst_rollout(name: &str) -> bool {
    cfg!(feature = "sqlite") && name.ends_with(".jsonl.zst")
}

fn is_zst_path(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.ends_with(".jsonl.zst"))
}

/// Modern JSONL rollout (plain or zstd-compressed), as opposed to the 2025 single-JSON layout.
fn is_jsonl_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl") || is_zst_path(path)
}

/// Open a rollout for line-oriented reading, transparently decoding a `.jsonl.zst`.
fn open_rollout(path: &Path) -> Result<Box<dyn BufRead>> {
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    #[cfg(feature = "sqlite")]
    if is_zst_path(path) {
        let dec = ruzstd::decoding::StreamingDecoder::new(BufReader::new(f))
            .map_err(|e| anyhow::anyhow!("zstd header of {}: {e}", path.display()))?;
        return Ok(Box::new(BufReader::new(dec)));
    }
    Ok(Box::new(BufReader::new(f)))
}

/// Whole-file read of a rollout, transparently decoding a `.jsonl.zst`.
fn read_rollout_to_string(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut text = String::new();
    open_rollout(path)?
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(text)
}

/// Per-parse state shared by the full and streaming paths (both call [`dispatch_line`] per
/// record), so their outputs stay identical. A checkpoint keeps it whole.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct CodexCtx {
    /// Format-complete parse: keep inherited-prefix records (tagged `inherited`) instead of
    /// skipping them.
    complete: bool,
    /// `session_meta.subagent_history_start_ordinal`: records with a smaller top-level `ordinal`
    /// are the parent thread's history embedded in this fork/subagent rollout.
    inherit_before: Option<u64>,
    /// This thread's own `agent_path` (from `session_meta`), to attribute `agent_message` authorship.
    agent_path: Option<String>,
    /// `inter_agent_communication_metadata.trigger_turn` seen, waiting for the `agent_message`
    /// record it immediately precedes.
    pending_trigger_turn: Option<bool>,
    /// The canonical (first) `session_meta` has been applied — a fork embeds the parent's as a
    /// second line, whose thread-level fields must not overwrite the child's.
    meta_seen: bool,
    /// Usage from the last `token_usage_record`, until its `token_count` twin (same numbers, a few
    /// records later) has been seen — so the twin never double-counts as a second carrier.
    pending_record_usage: Option<Usage>,
    /// `info.total_token_usage` of the last `token_count`: the thread's running total. A snapshot
    /// that repeats it reports no new model call (Codex re-emits the last one, e.g. after a user
    /// message), so its `last_token_usage` must not be counted again.
    last_total_usage: Option<Value>,
}

impl CodexCtx {
    fn for_opts(opts: &ParseOptions) -> Self {
        CodexCtx {
            complete: opts.complete,
            ..Default::default()
        }
    }
}

/// Parse a Codex transcript from its text contents into a [`Session`].
///
/// Pure (no filesystem); handles both the modern `.jsonl` rollout (`is_jsonl = true`) and the 2025
/// legacy single-JSON layout. `id` is the fallback session id (used when the transcript carries no
/// `session_meta`/header id); `source_path` records provenance when known.
pub fn parse_str(id: &str, text: &str, is_jsonl: bool, source_path: Option<PathBuf>) -> Session {
    // Start the id empty so a transcript-provided id (session_meta / bare header / legacy
    // `session.id`) is authoritative; the `id` argument is only the fallback when none is present.
    let mut s = Session {
        source_path,
        ..Session::new(String::new(), Harness::Codex)
    };

    if is_jsonl {
        // Detect `has_events` in a cheap, bounded first pass (no per-line `Value`s are retained)
        // instead of materializing every record up front. Same detector as `stream`'s pre-pass, so
        // the two paths can never disagree on a file.
        let mut det = EventDetector::default();
        super::for_each_json_line_str(text, |v| det.feed(&v));
        let has_events = det.found;
        // Accumulate into one vec across all lines so a `token_count` event can attach usage to
        // the assistant message it trails. Using a separate `out` (not `&mut s.messages`) avoids
        // borrowing `s` twice in `dispatch_line`.
        let mut out: Vec<Message> = Vec::new();
        let mut ctx = CodexCtx::default();
        let skipped = super::for_each_json_line_str(text, |v| {
            dispatch_line(&mut s, &mut out, &v, has_events, &mut ctx);
            Flow::Continue
        });
        super::note_skipped_lines(&mut s, skipped);
        s.messages = out;
    } else if let Ok(root) = serde_json::from_str::<Value>(text) {
        let mut ctx = CodexCtx::default();
        apply_meta(&mut s, &mut ctx, root.get("session"));
        let items = root.get("items").and_then(Value::as_array).or_else(|| root.as_array());
        if let Some(items) = items {
            for it in items {
                handle_item(Some(it), false, None, &mut s.messages, &mut ctx);
            }
        }
    }

    if s.id.is_empty() {
        s.id = id.to_string();
    }
    s
}

/// Does this record carry natural-language text via `event_msg` (vs. `response_item`)? The
/// `has_events` flag is decided from these by [`EventDetector`].
fn is_nl_event(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("event_msg")
        && matches!(
            v.pointer("/payload/type").and_then(Value::as_str),
            Some("user_message") | Some("agent_message")
        )
}

/// Fold one record into `s`'s metadata and push any resulting messages into `scratch`. Shared by the
/// full [`parse_str`] and the streaming [`stream_jsonl`] so both produce identical messages.
///
/// The cross-record behaviors — `token_count`/`token_usage_record` attach usage to the assistant
/// message they trail — work on both paths because they only ever touch the *last* entry of
/// `scratch` (see [`apply_token_count`]), which the streaming loops hold back until the next
/// message arrives. Everything else that spans records (the `inter_agent_communication_metadata`
/// → `agent_message` pairing, the inherited-prefix cutoff, the thread's `agent_path`) lives in
/// [`CodexCtx`], which both paths carry.
fn dispatch_line(s: &mut Session, scratch: &mut Vec<Message>, v: &Value, has_events: bool, ctx: &mut CodexCtx) {
    dispatch_line_with_output(s, scratch, v, has_events, ctx, None);
}

/// Dispatch with an already-parsed tool output. The span reader changes only body storage;
/// record metadata, inherited-history filtering and tool-result classification stay shared.
fn dispatch_line_with_output(
    s: &mut Session,
    scratch: &mut Vec<Message>,
    v: &Value,
    has_events: bool,
    ctx: &mut CodexCtx,
    output: Option<(Text, bool)>,
) {
    if let Some(ts) = top_ts(v) {
        s.created_at.get_or_insert(ts);
        s.updated_at = Some(ts);
    }
    // Inherited prefix (fork/subagent rollouts embed the parent's history before
    // `subagent_history_start_ordinal`): those records still shape the thread's metadata — the
    // child inherits the parent's model/cwd — but they are the parent's turns, not this thread's,
    // so the lean passes emit no messages for them; `complete` keeps them, tagged.
    let inherited = matches!(
        (v.get("ordinal").and_then(Value::as_u64), ctx.inherit_before),
        (Some(o), Some(cut)) if o < cut
    );
    let before = scratch.len();
    let ts = top_ts(v);
    match v.get("type").and_then(Value::as_str) {
        None => {
            // bare {id,timestamp} header
            if s.id.is_empty() {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    s.id = id.to_string();
                }
            }
        }
        Some("session_meta") => apply_meta(s, ctx, v.get("payload")),
        Some("turn_context") => apply_turn_context(s, scratch, v.get("payload"), ts),
        Some("event_msg") => {
            if !handle_event(s, ctx, v.get("payload"), has_events, ts, scratch) && ctx.complete {
                carry_record(v, ts, scratch);
            }
        }
        Some("response_item") => {
            if !handle_item_with_output(v.get("payload"), has_events, ts, scratch, ctx, output) && ctx.complete {
                carry_record(v, ts, scratch);
            }
        }
        Some("compacted") => handle_compacted(v.get("payload"), ts, scratch),
        Some("token_usage_record") => apply_token_usage_record(v.get("payload"), ts, scratch, ctx),
        // Precedes the `agent_message` it describes (verified on 0.154 rollouts: M at n, A at n+1).
        Some("inter_agent_communication_metadata") => {
            ctx.pending_trigger_turn = v.pointer("/payload/trigger_turn").and_then(Value::as_bool);
        }
        // `world_state`, `security_risk_score`, `retained_context`, `realtime_item`, and whatever
        // a newer Codex adds: bookkeeping with no conversational payload. Lean passes drop them;
        // `complete` carries them so nothing is lost and the census can count them.
        Some(_) => {
            if ctx.complete {
                carry_record(v, ts, scratch);
            }
        }
    }
    if inherited {
        if ctx.complete {
            for m in &mut scratch[before..] {
                bag(m).insert("inherited".into(), Value::Bool(true));
            }
        } else {
            scratch.truncate(before);
        }
    }
}

/// Does this record carry natural-language text via a `response_item` message (the old-format
/// counterpart of [`is_nl_event`])? Used by [`EventDetector`] to bound the pre-pass.
fn is_nl_response_message(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("response_item")
        && v.pointer("/payload/type").and_then(Value::as_str) == Some("message")
        && matches!(
            v.pointer("/payload/role").and_then(Value::as_str),
            Some("user") | Some("assistant")
        )
}

/// Bounded `has_events` detection: does this rollout carry its natural-language text as
/// `event_msg`s (modern) or only as `response_item` messages (old format)?
///
/// In modern rollouts the `event_msg` duplicate *trails* its `response_item` by a couple of
/// records (the user's prompt is recorded as a `response_item message` first, then echoed as an
/// `event_msg user_message`), so the first NL record alone can't decide. Instead we keep scanning
/// for [`LOOKAHEAD`] records past the first NL `response_item`; if no NL `event_msg` shows up by
/// then, the file is old-format. Measured over the full 620-rollout local corpus the worst
/// observed gap is **5 records** (LOOKAHEAD is >6× that), and the first NL event always lands
/// within the first 15 records — so this is byte-identical to the previous whole-file scan on
/// every real file, while old-format files (which used to force a full extra read, twice for a
/// multi-hundred-MB rollout) now stop after the head.
#[derive(Default)]
struct EventDetector {
    /// Records seen since the first NL `response_item` message, once one has been seen.
    past_first_nl_resp: Option<u32>,
    /// The verdict: NL text comes from `event_msg`s.
    found: bool,
}

impl EventDetector {
    /// How many records past the first NL `response_item` to keep looking for its `event_msg` twin.
    const LOOKAHEAD: u32 = 32;

    /// Feed one record; returns [`Flow::Stop`] once the verdict is decided.
    fn feed(&mut self, v: &Value) -> Flow {
        self.feed_flags(is_nl_event(v), is_nl_response_message(v))
    }

    fn feed_flags(&mut self, event: bool, response: bool) -> Flow {
        if event {
            self.found = true;
            return Flow::Stop;
        }
        if let Some(n) = self.past_first_nl_resp.as_mut() {
            *n += 1;
            if *n > Self::LOOKAHEAD {
                return Flow::Stop; // old format: NL response_item with no event_msg echo
            }
        } else if response {
            self.past_first_nl_resp = Some(0);
        }
        Flow::Continue
    }
}

/// Bounded first pass over a rollout: are NL messages carried as `event_msg`s? (See
/// [`EventDetector`] for the bounding rule and its corpus-measured safety margin.)
fn detect_has_events<R: BufRead>(reader: R) -> bool {
    let mut det = EventDetector::default();
    // Retain the buffered/no-mmap transport contract, but never build its giant output Values.
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Some((event, response)) = detect::line(line.as_bytes()) {
            if det.feed_flags(event, response) == Flow::Stop {
                break;
            }
        }
    }
    det.found
}

#[cfg(feature = "mmap")]
fn detect_has_events_bytes(bytes: &[u8]) -> bool {
    let mut det = EventDetector::default();
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', bytes).chain(std::iter::once(bytes.len())) {
        let line = &bytes[start..end];
        start = end + 1;
        if let Some((event, response)) = detect::line(line) {
            if det.feed_flags(event, response) == Flow::Stop {
                break;
            }
        }
    }
    det.found
}

/// Flush `scratch` to `sink` — except a trailing assistant message that has no usage yet, which is
/// held back (it stays in `scratch`) so a following `token_count` record can attach usage to it.
/// This keeps the streaming paths' usage attachment identical to [`parse_str`], where the whole
/// message vec is still reachable when the event arrives: [`apply_token_count`] only ever targets
/// the last message, and the last message is exactly what's held. The hold is at most one message,
/// flushed by the next [`flush_all_but_held`] call or by the caller at EOF.
fn flush_all_but_held(scratch: &mut Vec<Message>, sink: &mut dyn MessageSink) -> Flow {
    // Also hold an assistant message whose usage came from a `token_usage_record`: its
    // `token_count` twin (rate limits, context window) follows a few records later and merges into
    // it — see [`apply_token_count`].
    let hold = scratch.last().is_some_and(|m| {
        m.role == Role::Assistant
            && (m.usage.is_none() || bag_get(m, "usage_source").and_then(Value::as_str) == Some(USAGE_FROM_RECORD))
    });
    let upto = scratch.len() - hold as usize;
    for m in scratch.drain(..upto) {
        if sink.message(m) == Flow::Stop {
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// How many messages a parse of a plain `.jsonl` rollout's first `len` bytes yields — the count a
/// parse would have returned when the file was `len` bytes long, since a live rollout only grows by
/// appending. Both passes read only the prefix, so `has_events` is decided exactly as it was then.
/// Not for `.jsonl.zst` (a byte prefix of a zstd stream is not a prefix of the rollout).
pub(crate) fn count_prefix(r: &SessionRef, len: u64) -> Result<usize> {
    use std::io::Read as _;
    let open = || -> Result<BufReader<std::io::Take<fs::File>>> {
        let f = fs::File::open(&r.path).with_context(|| format!("opening {}", r.path.display()))?;
        Ok(BufReader::new(f.take(len)))
    };
    let has_events = detect_has_events(open()?);
    let mut n = 0usize;
    let mut count = |_: Message| {
        n += 1;
        Flow::Continue
    };
    stream_jsonl(
        &r.id,
        open()?,
        Some(r.path.clone()),
        has_events,
        &ParseOptions::full(),
        &mut count,
    );
    Ok(n)
}

/// Streaming parse of a modern `.jsonl` rollout: emit each record's messages to `sink` and drop them
/// before the next line, so peak memory is O(largest line) rather than O(whole file). Returns the
/// [`Session`] metadata with empty `messages`.
pub fn stream_jsonl<R: BufRead>(
    id: &str,
    reader: R,
    source_path: Option<PathBuf>,
    has_events: bool,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let state = ResumeState::new(source_path, has_events, opts, true);
    stream_jsonl_at(id, reader, state, 0, sink)
}

fn stream_jsonl_at<R: BufRead>(
    id: &str,
    reader: R,
    state: ResumeState,
    at: u64,
    sink: &mut dyn MessageSink,
) -> Session {
    let ResumeState {
        session: mut s,
        mut ctx,
        has_events,
        mut meta_sent,
        skipped: prior_skipped,
        reading,
    } = state;
    let capture = s
        .source_path
        .as_ref()
        .is_some_and(|path| is_jsonl_path(path) && !is_zst_path(path));
    let mut scratch: Vec<Message> = Vec::new();
    let mut stopped = false;
    let complete = ctx.complete;
    let parse = |line: &str| -> Option<Value> {
        if !complete {
            if let Ok(l) = lean::parse(line.as_bytes(), false) {
                return Some(l.v);
            }
        }
        serde_json::from_str(line).ok()
    };
    let skipped = super::for_each_json_line_at_with(reader, at, parse, |v, off, complete, skipped| {
        dispatch_line(&mut s, &mut scratch, &v, has_events, &mut ctx);
        // Hand the session metadata to the sink as soon as the model is known (session_meta /
        // turn_context land in the first records, before any message), so header-rendering sinks
        // have it ahead of the body.
        if !meta_sent && s.model.is_some() {
            sink.meta(&s);
            meta_sent = true;
        }
        let flow = flush_all_but_held(&mut scratch, sink);
        stopped = flow == Flow::Stop;
        // A trailing assistant is provisional until its usage arrives; a partial EOF record is
        // provisional until a newline arrives. Neither belongs in a reusable immutable prefix.
        if !stopped && capture && scratch.is_empty() && complete && sink.wants_checkpoint(off) {
            checkpoint_at(
                off,
                StateRef {
                    session: &s,
                    ctx: &ctx,
                    has_events,
                    meta_sent,
                    skipped: prior_skipped + skipped,
                    reading,
                },
                sink,
            );
        }
        flow
    });
    super::note_skipped_lines(&mut s, prior_skipped + skipped);
    if !stopped {
        // EOF: emit the held trailing message, if any (no token_count followed it).
        for m in scratch.drain(..) {
            if sink.message(m) == Flow::Stop {
                break;
            }
        }
    }
    if s.id.is_empty() {
        s.id = id.to_string();
    }
    if !meta_sent {
        sink.meta(&s);
    }
    s
}

/// Span-producing streaming parse over the source bytes (an mmap). Mirrors [`stream_jsonl`] but
/// iterates line slices (tracking file offsets) and, for a giant `function_call_output` with a plain
/// **string** output, emits a lazy [`Span`](crate::lazy::Span) for that output instead of
/// reading/parsing the (often 100s of MB) line whole. `stamp_offsets` additionally stamps each
/// message's record byte offset into `extra` ([`crate::offsets::OFFSET_KEY`]).
#[cfg(feature = "mmap")]
pub fn stream_jsonl_spans(
    id: &str,
    data: &[u8],
    source_path: Option<PathBuf>,
    has_events: bool,
    stamp_offsets: bool,
    sink: &mut dyn MessageSink,
) -> Session {
    let opts = ParseOptions {
        offsets: stamp_offsets,
        ..ParseOptions::lazy()
    };
    stream_jsonl_spans_with_options(id, data, source_path, has_events, &opts, sink)
}

#[cfg(feature = "mmap")]
fn stream_jsonl_spans_with_options(
    id: &str,
    data: &[u8],
    source_path: Option<PathBuf>,
    has_events: bool,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let state = ResumeState::new(source_path, has_events, opts, false);
    stream_jsonl_spans_at(id, data, state, 0, opts.offsets, sink)
}

/// [`stream_jsonl_spans`] from byte `at` of `data` with `state` (a checkpoint's, or a fresh one at
/// 0). After every whole line that leaves nothing held back it offers the sink a [`Checkpoint`].
#[cfg(feature = "mmap")]
fn stream_jsonl_spans_at(
    id: &str,
    data: &[u8],
    state: ResumeState,
    at: u64,
    stamp_offsets: bool,
    sink: &mut dyn MessageSink,
) -> Session {
    let ResumeState {
        session: mut s,
        mut ctx,
        has_events,
        mut meta_sent,
        mut skipped,
        reading,
    } = state;
    let mut scratch: Vec<Message> = Vec::new();
    let mut off = at;
    let mut start = at as usize;
    'outer: while start < data.len() {
        let end = memchr::memchr(b'\n', &data[start..]).map_or(data.len(), |i| start + i);
        let raw_line = &data[start..end];
        start = end + 1;
        let line_off = off;
        off += raw_line.len() as u64 + 1; // +1 for the consumed '\n'
        let lead = raw_line.iter().take_while(|b| b.is_ascii_whitespace()).count();
        let endw = raw_line.len()
            - raw_line[lead..]
                .iter()
                .rev()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
        if lead >= endw {
            continue;
        }
        let slice = &raw_line[lead..endw];
        let base_off = line_off + lead as u64;

        let before = scratch.len();
        let giant = slice.len() > crate::lazy::INLINE_MAX;
        match lean::parse(slice, giant) {
            Ok(lean::LeanLine { mut v, output }) => match output {
                Some(raw) => match giant_fco_span(&v, raw, slice, base_off) {
                    Some((span, is_error)) => {
                        dispatch_line_with_output(
                            &mut s,
                            &mut scratch,
                            &v,
                            has_events,
                            &mut ctx,
                            Some((Text::Span(span), is_error)),
                        );
                    }
                    None => {
                        // Not the span case after all: build the output in its place.
                        if let Some(slot) = v.get_mut("payload").and_then(|p| p.get_mut("output")) {
                            *slot = serde_json::from_str(raw.get()).unwrap_or(Value::Null);
                        }
                        dispatch_line(&mut s, &mut scratch, &v, has_events, &mut ctx);
                    }
                },
                None => dispatch_line(&mut s, &mut scratch, &v, has_events, &mut ctx),
            },
            Err(_) => match serde_json::from_slice::<Value>(slice) {
                Ok(v) => dispatch_line(&mut s, &mut scratch, &v, has_events, &mut ctx),
                Err(_) => {
                    skipped += 1; // corrupt line — tolerated, but counted (see note_skipped_lines)
                    continue;
                }
            },
        }
        if stamp_offsets {
            // Stamp this record's byte offset on the message(s) it produced (see
            // [`crate::offsets::OFFSET_KEY`]). Messages a later record amends (token_count
            // usage) keep their creating record's offset.
            for m in &mut scratch[before..] {
                m.extra.insert(crate::offsets::OFFSET_KEY.into(), base_off.into());
            }
        }
        if !meta_sent && s.model.is_some() {
            sink.meta(&s);
            meta_sent = true;
        }
        if flush_all_but_held(&mut scratch, sink) == Flow::Stop {
            // Sink asked to stop: drop any held message rather than emitting past the stop.
            scratch.clear();
            break 'outer;
        }
        // Quiescent: nothing held back, and the line was finished (a partial last line is read
        // again, whole, by whoever resumes here).
        if scratch.is_empty() && end < data.len() && sink.wants_checkpoint(off) {
            checkpoint_at(
                off,
                StateRef {
                    session: &s,
                    ctx: &ctx,
                    has_events,
                    meta_sent,
                    skipped,
                    reading,
                },
                sink,
            );
        }
    }
    // EOF: emit the held trailing message, if any (no token_count followed it).
    for m in scratch.drain(..) {
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    super::note_skipped_lines(&mut s, skipped);
    if s.id.is_empty() {
        s.id = id.to_string();
    }
    if !meta_sent {
        sink.meta(&s);
    }
    s
}

#[derive(serde::Serialize)]
struct StateRef<'a> {
    session: &'a Session,
    ctx: &'a CodexCtx,
    has_events: bool,
    meta_sent: bool,
    skipped: u64,
    reading: Reading,
}

/// Offer `sink` the checkpoint after the line ending at `off`.
fn checkpoint_at(off: u64, state: StateRef<'_>, sink: &mut dyn MessageSink) {
    let mut cp = Checkpoint::new(off);
    cp.put(crate::stream::ADAPTER_LAYER, &state);
    sink.checkpoint(&mut cp);
}

/// If a giant line (`v`, parsed by [`lean::parse`] with its `output` captured `raw`) is a
/// `function_call_output`/`custom_tool_call_output` whose `output` is a plain JSON string, emit a
/// lazy span and its error bit without materializing it. The caller supplies this body projection
/// to the shared dispatcher. `None` ⇒ build the output
/// and dispatch normally (other types, non-string outputs).
#[cfg(feature = "mmap")]
fn giant_fco_span(v: &Value, raw: &RawValue, slice: &[u8], base_off: u64) -> Option<(crate::lazy::Span, bool)> {
    if v.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = v.get("payload")?;
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("function_call_output") | Some("custom_tool_call_output")
    ) {
        return None;
    }
    let mut span = json_string_span(raw, slice, base_off)?;
    // cv's own emitter writes a failed result as the string "[error] <content>" (Codex never
    // persists an error bit) — same convention as the materializing path: strip it, flag it. The
    // prefix may itself be escaped: advance by its raw length, not its decoded length.
    let prefix = json_ascii_prefix(&raw.get().as_bytes()[1..raw.get().len() - 1], ERROR_PREFIX.as_bytes());
    if let Some(n) = prefix {
        span.offset += n as u64;
        span.len -= n as u64;
    }
    Some((span, prefix.is_some()))
}

/// Raw byte length of an ASCII prefix in a validated JSON string body. ASCII characters can
/// be literal or `\u00xx`; no whole-output decoding is needed to recognize our error marker.
fn json_ascii_prefix(raw: &[u8], prefix: &[u8]) -> Option<usize> {
    let mut at = 0;
    for &expected in prefix {
        if raw.get(at) == Some(&b'\\') {
            if raw.get(at + 1) != Some(&b'u') {
                return None;
            }
            let hex = std::str::from_utf8(raw.get(at + 2..at + 6)?).ok()?;
            if u16::from_str_radix(hex, 16).ok()? != u16::from(expected) {
                return None;
            }
            at += 6;
        } else {
            if raw.get(at) != Some(&expected) {
                return None;
            }
            at += 1;
        }
    }
    Some(at)
}

/// `session_meta` fields with no first-class IR home, stashed verbatim (wire names) into
/// `Session::extra["codex"]` from the canonical (first) meta record: where the thread sits in a
/// swarm (`session_id` = root thread, `thread_source`, `agent_nickname`/`agent_role`), how it was
/// made (`source` — a string or, for subagents, an object), and how its file is laid out
/// (`history_mode`, `subagent_history_start_ordinal`, `history_base`). The lineage fields
/// (`parent_thread_id`, `forked_from_id`, `agent_path`) are first-class in [`Session::lineage`];
/// `base_instructions.text` is [`Session::system_prompt`].
const META_EXTRA_KEYS: &[&str] = &[
    "session_id",
    "thread_source",
    "agent_nickname",
    "agent_role",
    "source",
    "model_provider",
    "history_mode",
    "subagent_history_start_ordinal",
    "history_base",
    "cli_version",
    "originator",
];

fn apply_meta(s: &mut Session, ctx: &mut CodexCtx, payload: Option<&Value>) {
    let Some(p) = payload else { return };
    if let Some(id) = p.get("id").and_then(Value::as_str) {
        if s.id.is_empty() {
            s.id = id.to_string();
        }
    }
    // Thread-level facts come from the FIRST meta only: a fork/subagent rollout embeds the parent's
    // `session_meta` as its second line (recorder.rs:1108-1112 treats the first as canonical).
    if !ctx.meta_seen {
        ctx.meta_seen = true;
        for key in META_EXTRA_KEYS {
            if let Some(val) = p.get(*key).filter(|v| !v.is_null()) {
                sbag(s).insert((*key).to_string(), val.clone());
            }
        }
        let str_of = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
        s.lineage.parent = str_of("parent_thread_id");
        s.lineage.forked_from = str_of("forked_from_id");
        s.lineage.agent_path = str_of("agent_path");
        // `base_instructions {text, provenance?}` (0.147+): the system prompt this thread runs on.
        s.system_prompt = p
            .pointer("/base_instructions/text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
        ctx.inherit_before = p.get("subagent_history_start_ordinal").and_then(Value::as_u64);
        ctx.agent_path = s.lineage.agent_path.clone();
    }
    if s.cwd.is_none() {
        s.cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    if let Some(ts) = p.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
        s.created_at.get_or_insert(ts);
    }
    if s.git.is_none() {
        if let Some(g) = p.get("git") {
            s.git = Some(GitInfo {
                branch: g.get("branch").and_then(Value::as_str).map(str::to_string),
                commit: g.get("commit_hash").and_then(Value::as_str).map(str::to_string),
                remote: g.get("repository_url").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
}

/// `turn_context` records persist per-turn config (model, effort, cwd, sandbox/approval). We seed
/// the session-level `model`/`cwd`/`reasoning_effort` from the first one, and note every later
/// model or effort change (see [`note_settings_change`]).
fn apply_turn_context(s: &mut Session, out: &mut Vec<Message>, payload: Option<&Value>, ts: Option<DateTime<Utc>>) {
    let Some(p) = payload else { return };
    if s.cwd.is_none() {
        s.cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    if let Some(policy) = p.get("approval_policy").and_then(Value::as_str) {
        crate::affordance::note_mode(sbag(s), policy);
    }
    // 0.147+ also carries the collaboration-mode settings; `model` there mirrors the top-level one.
    let settings = p.pointer("/collaboration_mode/settings");
    let model = p
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| settings.and_then(|c| c.get("model")).and_then(Value::as_str));
    let effort = p
        .get("effort")
        .and_then(Value::as_str)
        .or_else(|| settings.and_then(|c| c.get("reasoning_effort")).and_then(Value::as_str));
    let mut extra = Map::new();
    for key in ["turn_id", "effort", "cwd"] {
        if let Some(val) = p.get(key).filter(|v| !v.is_null()) {
            extra.insert(key.to_string(), val.clone());
        }
    }
    note_settings_change(s, out, model, effort, ts, "turn_context", extra);
}

/// Seed or update the session-level model and reasoning effort (`extra["codex"]["reasoning_effort"]`);
/// when either *changes* mid-session (`/model`, escalation, a `thread_settings_applied` event) the
/// switch is one [`MessageKind::ModelChange`] note so the drift survives into the IR.
/// `Message::model` names the new model when the model is what changed (the one place a message's
/// model equals the session's: the session's has just become this one); an effort-only change
/// leaves it unset. `extra` rides on the note in the bag (which record noticed it, the turn, the
/// effort, the cwd).
fn note_settings_change(
    s: &mut Session,
    out: &mut Vec<Message>,
    model: Option<&str>,
    effort: Option<&str>,
    ts: Option<DateTime<Utc>>,
    source: &str,
    mut extra: Map<String, Value>,
) {
    let mut changes = Vec::new();
    let mut new_model = None;
    if let Some(m) = model {
        if let Some(prev) = s.model.as_deref().filter(|prev| *prev != m) {
            changes.push(format!("model changed: {prev} → {m}"));
            new_model = Some(m.to_string());
        }
        s.model = Some(m.to_string());
    }
    if let Some(e) = effort {
        let prev = sbag(s)
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(prev) = prev.filter(|prev| prev != e) {
            changes.push(format!("effort changed: {prev} → {e}"));
        }
        sbag(s).insert("reasoning_effort".into(), Value::String(e.to_string()));
    }
    if changes.is_empty() {
        return;
    }
    let mut msg = note(MessageKind::ModelChange, ts, format!("[{}]", changes.join("; ")));
    msg.model = new_model;
    let b = bag(&mut msg);
    b.insert("codex_event".into(), Value::String(source.into()));
    b.append(&mut extra);
    out.push(msg);
}

/// Handle an `event_msg` record. These are the UI-side mirror of the model exchange. We emit
/// natural-language `user_message`/`agent_message` text (only when `has_events`, since otherwise the
/// `response_item message`s carry it), surface `view_image_tool_call` as an image attachment
/// (legacy files; paginated ones record it as an `item_completed` `ImageView`), attach
/// `token_count` usage/rate-limit info to the assistant message it trails, turn paginated-mode
/// `item_completed` items into structured system notes, follow `thread_settings_applied` model /
/// effort changes, and note `turn_aborted` (a notice), `thread_rolled_back` (a branch) and
/// persisted `error`s. Streaming deltas, approvals and `exec_command_*`/`web_search_*` events are
/// never persisted (rollout/src/policy.rs), and `task_started`/`task_complete` carry nothing the
/// items don't — except the last agent message, which is stashed session-level as a preview.
///
/// Returns whether the record is represented in the IR (as a message, merged usage or session
/// metadata); `false` lets [`dispatch_line`] carry it verbatim under `ParseOptions::complete`.
fn handle_event(
    s: &mut Session,
    ctx: &mut CodexCtx,
    payload: Option<&Value>,
    has_events: bool,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
) -> bool {
    let Some(p) = payload else { return false };
    match p.get("type").and_then(Value::as_str) {
        Some("user_message") if has_events => {
            if let Some(text) = p.get("message").and_then(Value::as_str) {
                let mut m = user_message(text);
                m.timestamp = ts;
                if let Some(kind) = p.get("kind").and_then(Value::as_str) {
                    bag(&mut m).insert("kind".into(), Value::String(kind.into()));
                }
                out.push(m);
            }
        }
        Some("agent_message") if has_events => {
            if let Some(text) = p.get("message").and_then(Value::as_str) {
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                m.content.push(Block::Text { text: text.into() });
                out.push(m);
            }
        }
        Some("view_image_tool_call") => {
            // The agent attached a local image via the `view_image` tool.
            let path = p.get("path").and_then(Value::as_str);
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            m.content.push(Block::Image {
                media_type: None,
                data_ref: path.map(str::to_string),
            });
            let b = bag(&mut m);
            if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                b.insert("call_id".into(), Value::String(id.into()));
            }
            b.insert("codex_event".into(), Value::String("view_image_tool_call".into()));
            out.push(m);
        }
        Some("token_count") => {
            // Usage totals + rate limits. Attach to the assistant message this event trails so the
            // numbers ride along with the turn they belong to; otherwise drop a bare carrier
            // message so the rate-limit snapshot isn't lost.
            apply_token_count(p, ts, out, ctx);
        }
        Some("item_completed") => handle_item_completed(p, ts, out, ctx),
        Some("thread_settings_applied") => {
            // The reliable "settings changed" signal (protocol.rs:2194-2233): model, provider,
            // effort, personality, cwd, approval/permission profile. The next `turn_context` will
            // agree, so the model-change note fires here at most once.
            let Some(t) = p.get("thread_settings").and_then(Value::as_object) else {
                return true;
            };
            let model = t.get("model").and_then(Value::as_str);
            let mut kept = Map::new();
            for key in [
                "model",
                "model_provider_id",
                "reasoning_effort",
                "reasoning_summary",
                "personality",
                "cwd",
                "approval_policy",
                "service_tier",
            ] {
                if let Some(val) = t.get(key).filter(|v| !v.is_null()) {
                    kept.insert(key.to_string(), val.clone());
                }
            }
            let effort = t.get("reasoning_effort").and_then(Value::as_str);
            if let Some(policy) = t.get("approval_policy").and_then(Value::as_str) {
                crate::affordance::note_mode(sbag(s), policy);
            }
            sbag(s).insert("thread_settings".into(), Value::Object(kept));
            note_settings_change(s, out, model, effort, ts, "thread_settings_applied", Map::new());
        }
        Some("task_complete") => {
            // The turn's closing marker: its `last_agent_message` is the session preview; the
            // record itself has no conversational shape (carried under `complete`, see below).
            if let Some(last) = p.get("last_agent_message").and_then(Value::as_str) {
                if !last.trim().is_empty() {
                    sbag(s).insert("last_agent_message".into(), Value::String(last.to_string()));
                }
            }
            return false;
        }
        Some("error") => {
            // A persisted API/usage error the model never answered — `ErrorEvent {message,
            // codex_error_info}` (protocol.rs:1930). CLI ≤ 0.1xx wrote them (88 in the local
            // 2026-04 corpus, all `usage_limit_exceeded`); the current policy no longer persists
            // `EventMsg::Error`, but old rollouts keep theirs.
            let message = p.get("message").and_then(Value::as_str).unwrap_or("").trim();
            let text = if message.is_empty() {
                "[error]".to_string()
            } else {
                message.to_string()
            };
            let mut m = note(MessageKind::Error, ts, text);
            let mut error = Map::new();
            for (k, val) in p.as_object().into_iter().flatten() {
                if k != "type" && !val.is_null() {
                    error.insert(k.clone(), val.clone());
                }
            }
            let b = bag(&mut m);
            b.insert("codex_event".into(), Value::String("error".into()));
            b.insert("error".into(), Value::Object(error));
            out.push(m);
        }
        Some("turn_aborted") => {
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            let mut m = note(MessageKind::Notice, ts, format!("[turn aborted: {reason}]"));
            let b = bag(&mut m);
            b.insert("codex_event".into(), Value::String("turn_aborted".into()));
            // `error` (an `ErrorEvent`, 0.161+) says why; `root_turn_id` is 0.162+.
            for key in ["turn_id", "reason", "duration_ms", "error", "root_turn_id"] {
                if let Some(val) = p.get(key).filter(|v| !v.is_null()) {
                    b.insert(key.to_string(), val.clone());
                }
            }
            out.push(m);
        }
        Some("thread_rolled_back") => {
            // `/undo`-style rollback: the last N turns left the model's context (protocol.rs:3685)
            // — what follows does not continue what precedes: a [`MessageKind::Branch`].
            let n = p.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
            let mut m = note(
                MessageKind::Branch,
                ts,
                format!("[rolled back {n} turn{}]", if n == 1 { "" } else { "s" }),
            );
            let b = bag(&mut m);
            b.insert("codex_event".into(), Value::String("thread_rolled_back".into()));
            b.insert("num_turns".into(), Value::from(n));
            out.push(m);
        }
        // Legacy-mode twins of records already represented — the `response_item`s, the top-level
        // `compacted`, `item_completed` activity (rollout/src/policy.rs:109-120 persists these only
        // under `history_mode: legacy`): silent in every pass, not unknown and not lost.
        Some(
            "user_message"
            | "agent_message"
            | "agent_reasoning"
            | "agent_reasoning_raw_content"
            | "patch_apply_end"
            | "mcp_tool_call_end"
            | "web_search_end"
            | "image_generation_end"
            | "context_compacted"
            | "sub_agent_activity",
        ) => {}
        // Lifecycle markers with no conversational shape and no twin (`task_started`,
        // `thread_goal_updated`, `entered_review_mode`/`exited_review_mode`) and whatever a newer
        // Codex adds: nothing to show in a lean pass; `complete` carries them verbatim.
        _ => return false,
    }
    true
}

/// A user turn from text: the human's prompt, unless it is one of Codex's injected preambles
/// (then it is context the harness put there).
fn user_message(text: &str) -> Message {
    let mut m = if is_injected_text(text) {
        Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness)
    } else {
        Message::new(Role::User)
    };
    m.content.push(Block::Text { text: text.into() });
    m
}

/// Paginated history mode records every finished `TurnItem` as an `item_completed` event
/// (codex-rs/protocol/src/items.rs). The model-visible twins (`UserMessage`/`AgentMessage`/
/// `Reasoning`/`FunctionCallOutput`/`ContextCompaction`) already arrive as `response_item`s, so
/// those are skipped; the rest are UI-side facts with no `response_item` counterpart — command
/// executions with exit codes, file changes as diffs, MCP calls with errors, viewed/generated
/// images, sub-agent lifecycle, collaboration calls, plans, web searches — and become
/// [`Role::System`] notes: a one-line summary as text (so `cv show` reads), the structured item in
/// `extra.item`.
///
/// Why notes and not `ToolResult`s: on real 0.154 rollouts the `CommandExecution` items PRECEDE the
/// `custom_tool_call_output` they belong to, several map to one output (a unified-exec session runs
/// many commands), and their `exec-…` ids share nothing with the `call_…` id — so there is no safe
/// join, and a `ToolResult` with an unmatched `tool_use_id` would round-trip into an orphan
/// tool result (which Claude Code rejects on resume). System notes are dropped by every emitter.
/// The model saw none of this (its context is the `response_item`s), so the notes also add no
/// tokens to doctor's attribution — and `ImageView`/image-gen items carry the path as text only, the
/// image itself is already a `Block::Image` on the tool output the model received.
fn handle_item_completed(p: &Value, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>, _ctx: &mut CodexCtx) {
    let Some(item) = p.get("item") else { return };
    let ity = item.get("type").and_then(Value::as_str).unwrap_or("");
    let str_of = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("");
    let text = match ity {
        "CommandExecution" => {
            let cmd = item
                .get("command")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let status = str_of("status");
            let exit = item.get("exit_code").and_then(Value::as_i64);
            let mut t = format!("$ {}", truncate(&cmd, 160));
            if !status.is_empty() {
                t.push_str(&format!(" → {status}"));
            }
            if let Some(code) = exit {
                t.push_str(&format!(" (exit {code})"));
            }
            t
        }
        "FileChange" => {
            let paths: Vec<&str> = item
                .get("changes")
                .and_then(Value::as_object)
                .map(|c| c.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let status = str_of("status");
            format!(
                "[file change{}] {}",
                if status.is_empty() {
                    String::new()
                } else {
                    format!(" {status}")
                },
                truncate(&paths.join(", "), 200)
            )
        }
        "McpToolCall" => {
            let err = item.get("error").filter(|e| !e.is_null()).map(|e| e.to_string());
            format!(
                "[mcp {}.{}] {}{}",
                str_of("server"),
                str_of("tool"),
                str_of("status"),
                err.map(|e| format!(": {}", truncate(&e, 120))).unwrap_or_default()
            )
        }
        "ImageView" => format!("[viewed image: {}]", str_of("path")),
        "ImageGeneration" => format!("[generated image: {}]", str_of("saved_path")),
        "Extension" => match str_of("kind") {
            "image_gen.generation" => format!("[generated image: {}]", str_of("saved_path")),
            "web.search" => {
                let queries = item
                    .pointer("/action/queries")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" · "))
                    .filter(|q| !q.is_empty())
                    .unwrap_or_else(|| str_of("query").to_string());
                format!("[web search] {}", truncate(&queries, 200))
            }
            _ => return, // clock.sleep and friends: nothing to say
        },
        "SubAgentActivity" => format!("[sub-agent {} {}]", str_of("agent_path"), str_of("kind")),
        "CollabAgentToolCall" => {
            let receivers = item
                .get("receiver_thread_ids")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            format!(
                "[collab {}] {}{}",
                str_of("tool"),
                str_of("status"),
                if receivers > 0 {
                    format!(" → {receivers} agent(s)")
                } else {
                    String::new()
                }
            )
        }
        "Plan" => str_of("text").to_string(),
        // Twins of `response_item`s already emitted (or, for `FunctionCallOutput`, the output's
        // own record) — nothing new to say.
        _ => return,
    };
    if text.trim().is_empty() {
        return;
    }
    // Sub-agent lifecycle is first-class: a `started` activity is the spawn, `completed` the
    // return; everything else here is a harness notice.
    let kind = match (ity, str_of("kind")) {
        ("SubAgentActivity", "started") => MessageKind::SubagentSpawn,
        ("SubAgentActivity", "completed") => MessageKind::SubagentReturn,
        _ => MessageKind::Notice,
    };
    let when = p
        .get("completed_at_ms")
        .and_then(Value::as_i64)
        .and_then(DateTime::from_timestamp_millis)
        .or(ts);
    let mut m = note(kind, when, text);
    let b = bag(&mut m);
    b.insert(
        "codex_event".into(),
        Value::String(if ity == "Plan" {
            "plan".into()
        } else {
            "item_completed".into()
        }),
    );
    b.insert("item_type".into(), Value::String(ity.into()));
    for key in ["turn_id", "started_at_ms", "completed_at_ms"] {
        if let Some(val) = p.get(key).filter(|v| !v.is_null()) {
            b.insert(key.to_string(), val.clone());
        }
    }
    if ity == "SubAgentActivity" {
        for key in ["agent_thread_id", "agent_path"] {
            if let Some(val) = item.get(key).filter(|v| !v.is_null()) {
                b.insert(key.to_string(), val.clone());
            }
        }
    }
    b.insert("item".into(), item.clone());
    out.push(m);
}

/// Field-wise equality of two usage blocks (the IR type derives no `PartialEq`).
fn usage_eq(a: &Usage, b: &Usage) -> bool {
    a.input_tokens == b.input_tokens
        && a.output_tokens == b.output_tokens
        && a.cache_read_tokens == b.cache_read_tokens
        && a.cache_creation_tokens == b.cache_creation_tokens
        && a.reasoning_tokens == b.reasoning_tokens
}

/// Marker in `extra.usage_source` for usage that came from a `token_usage_record` (authoritative:
/// keyed by `response_id`/`turn_id`), so the `token_count` twin merges instead of duplicating.
const USAGE_FROM_RECORD: &str = "token_usage_record";

/// Paginated rollouts record usage per model response as a top-level `token_usage_record`
/// (protocol.rs:2258): `usage` for this response, plus running `turn_token_usage` /
/// `thread_token_usage`, keyed by `response_id`/`turn_id`. It precedes the `token_count` event for
/// the same response and is the authoritative figure — attach it to the trailing assistant message
/// (same trailing-only rule as [`apply_token_count`]), or carry it if nothing trails.
fn apply_token_usage_record(
    payload: Option<&Value>,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
    ctx: &mut CodexCtx,
) {
    let Some(p) = payload else { return };
    let Some(usage) = p.get("usage").and_then(parse_usage) else {
        return;
    };
    ctx.pending_record_usage = Some(usage.clone());
    let attach = out
        .last()
        .is_some_and(|m| m.role == Role::Assistant && m.usage.is_none());
    if !attach {
        let mut nm = Message::new(Role::Assistant);
        nm.timestamp = ts;
        bag(&mut nm).insert("codex_event".into(), Value::String(USAGE_FROM_RECORD.into()));
        out.push(nm);
    }
    let m = out.last_mut().unwrap();
    m.usage = Some(usage);
    let b = bag(m);
    b.insert("usage_source".into(), Value::String(USAGE_FROM_RECORD.into()));
    for key in ["response_id", "turn_id"] {
        if let Some(val) = p.get(key).filter(|v| !v.is_null()) {
            b.insert(key.to_string(), val.clone());
        }
    }
    if let Some(thread) = p.get("thread_token_usage").filter(|v| !v.is_null()) {
        b.insert("thread_token_usage".into(), thread.clone());
    }
}

/// Pull `last_token_usage` into IR [`Usage`] and stash rate-limit / context-window info in `extra`.
fn apply_token_count(p: &Value, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>, ctx: &mut CodexCtx) {
    let info = p.get("info");
    let mut usage = info.and_then(|i| i.get("last_token_usage")).and_then(parse_usage);
    // The twin of a `token_usage_record` already applied (same numbers; on real 0.154 files the
    // call's `item_completed` notes and its output sit between the two, so the record's message no
    // longer trails): its usage is a duplicate — keep only the rate-limit/context-window snapshot.
    let twin_of_record = match (&usage, &ctx.pending_record_usage) {
        (Some(u), Some(r)) => usage_eq(u, r),
        _ => false,
    };
    if twin_of_record {
        ctx.pending_record_usage = None;
        usage = None;
    }
    // A re-emitted snapshot: the running total has not moved since the last `token_count`.
    let total = info.and_then(|i| i.get("total_token_usage")).filter(|v| !v.is_null());
    if let Some(t) = total {
        if ctx.last_total_usage.as_ref() == Some(t) {
            usage = None;
        }
        ctx.last_total_usage = Some(t.clone());
    }
    let rate_limits = p.get("rate_limits").filter(|v| !v.is_null()).cloned();
    let ctx_window = info
        .and_then(|i| i.get("model_context_window"))
        .filter(|v| !v.is_null())
        .cloned();
    if usage.is_none() && rate_limits.is_none() && ctx_window.is_none() {
        return;
    }
    // Attach to the *trailing* assistant message — the just-finished turn this event reports on.
    // Trailing-only (no reach-back past intervening records): a `token_count` that doesn't directly
    // follow its assistant message reports stale `last_token_usage` (e.g. the snapshot re-emitted
    // after a user message), so attaching it further back would mislabel an older turn. It also
    // keeps the streaming paths exact: they hold back only the trailing assistant message (see
    // [`flush_all_but_held`]), and with this rule that's the only message ever targeted.
    // A `token_usage_record` already gave the trailing assistant message its (authoritative) usage:
    // this is its `token_count` twin — merge the rate-limit/context-window snapshot, keep the usage.
    let from_record = out.last().is_some_and(|m| {
        m.role == Role::Assistant && bag_get(m, "usage_source").and_then(Value::as_str) == Some(USAGE_FROM_RECORD)
    });
    if twin_of_record && !from_record && rate_limits.is_none() && ctx_window.is_none() {
        return; // the twin carried nothing beyond the usage already attached elsewhere
    }
    let attach = from_record
        || out
            .last()
            .is_some_and(|m| m.role == Role::Assistant && m.usage.is_none());
    if !attach {
        let mut nm = Message::new(Role::Assistant);
        nm.timestamp = ts;
        bag(&mut nm).insert("codex_event".into(), Value::String("token_count".into()));
        out.push(nm);
    }
    let m = out.last_mut().unwrap();
    if let Some(u) = usage {
        if !from_record {
            m.usage = Some(u);
        }
    }
    let b = bag(m);
    if let Some(rl) = rate_limits {
        b.insert("rate_limits".into(), rl);
    }
    if let Some(cw) = ctx_window {
        b.insert("model_context_window".into(), cw);
    }
}

/// Codex token-usage block → IR [`Usage`]. `cached_input_tokens` maps to cache-read;
/// `cache_write_input_tokens` (added in 0.147, `#[serde(default)]`, protocol.rs:2235) to
/// cache-creation — absent on older files; `reasoning_output_tokens` to `reasoning_tokens`. Codex
/// stores no cost.
///
/// Codex's `input_tokens` *includes* both cache counts (its `total_tokens` is input + output), so
/// the IR's disjoint `input_tokens` is the remainder; `emit::codex_token_usage` adds them back.
fn parse_usage(v: &Value) -> Option<Usage> {
    let u64f = |k: &str| v.get(k).and_then(Value::as_u64);
    let cache_read = u64f("cached_input_tokens");
    let cache_write = u64f("cache_write_input_tokens");
    let u = Usage {
        input_tokens: u64f("input_tokens")
            .map(|i| i.saturating_sub(cache_read.unwrap_or(0) + cache_write.unwrap_or(0))),
        output_tokens: u64f("output_tokens"),
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_write,
        reasoning_tokens: u64f("reasoning_output_tokens"),
        cost_usd: None,
    };
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return None;
    }
    Some(u)
}

/// A top-level `compacted` record marks an auto/manual history-compaction boundary
/// ([`MessageKind::CompactionBoundary`]); when it carries a human `message`, that summary follows as
/// a [`MessageKind::CompactionSummary`] (usually `""` on 0.147+ files, where the model-visible
/// replacement lives in `replacement_history`, whose length rides in the bag).
fn handle_compacted(payload: Option<&Value>, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>) {
    let Some(p) = payload else { return };
    let mut m = note(MessageKind::CompactionBoundary, ts, "[history compacted]".into());
    let b = bag(&mut m);
    b.insert("codex_event".into(), Value::String("compacted".into()));
    if let Some(rh) = p.get("replacement_history").and_then(Value::as_array) {
        b.insert("replacement_history_len".into(), Value::Number(rh.len().into()));
    }
    for key in ["window_number", "window_id", "compaction_response_id"] {
        if let Some(val) = p.get(key).filter(|v| !v.is_null()) {
            b.insert(key.to_string(), val.clone());
        }
    }
    out.push(m);
    let summary = p.get("message").and_then(Value::as_str).unwrap_or("");
    if !summary.trim().is_empty() {
        let mut sm = note(MessageKind::CompactionSummary, ts, summary.to_string());
        bag(&mut sm).insert("codex_event".into(), Value::String("compacted".into()));
        out.push(sm);
    }
}

/// cv's emitter writes a failed tool result as the string `"[error] <content>"`: Codex's
/// `FunctionCallOutputPayload` persists only a string or a content-item array — never an error
/// bit (models.rs:2252-2275) — so this prefix is the one form that both decodes in Codex and
/// round-trips `is_error` through cv. Recognized on parse and stripped from the content.
const ERROR_PREFIX: &str = "[error] ";

/// The real creation time of a `response_item`, when Codex recorded one:
/// `internal_chat_message_metadata_passthrough.create_time` (epoch seconds, float; models.rs:959).
/// A fork/subagent rollout stamps its inherited prefix with the FORK time as the envelope
/// `timestamp`, so this is the only honest per-item clock there.
fn passthrough_ts(p: &Value) -> Option<DateTime<Utc>> {
    let ct = p.pointer("/internal_chat_message_metadata_passthrough/create_time")?;
    let secs = ct.as_f64()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    DateTime::from_timestamp(secs.trunc() as i64, ((secs.fract()) * 1e9) as u32)
}

/// Handle a `response_item` payload or a legacy `items[]` entry.
fn handle_item(
    payload: Option<&Value>,
    has_events: bool,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
    ctx: &mut CodexCtx,
) -> bool {
    handle_item_with_output(payload, has_events, ts, out, ctx, None)
}

fn handle_item_with_output(
    payload: Option<&Value>,
    has_events: bool,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
    ctx: &mut CodexCtx,
    output: Option<(Text, bool)>,
) -> bool {
    let Some(p) = payload else { return false };
    let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
    let str_field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let ts = passthrough_ts(p).or(ts);
    let turn_id = p
        .pointer("/internal_chat_message_metadata_passthrough/turn_id")
        .filter(|v| !v.is_null())
        .cloned();
    let before = out.len();

    match ty {
        "message" => {
            let role = p.get("role").and_then(Value::as_str).unwrap_or("user");
            // Natural-language user/assistant text is taken from event_msg when present.
            if has_events && (role == "user" || role == "assistant") {
                return true;
            }
            // A message can mix text and images; emit one message carrying all blocks in order.
            let blocks = openai::blocks_from(p.get("content"), &openai::CODEX);
            if !blocks.is_empty() {
                let mut m = match role {
                    "assistant" => Message::new(Role::Assistant),
                    // `developer`/`system` messages are instructions the harness injected
                    // (`<user_instructions>`, AGENTS.md); never the user's own words.
                    "developer" | "system" => {
                        Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Harness)
                    }
                    _ => {
                        let text = text_parts(p.get("content"));
                        if is_injected_text(&text) {
                            Message::of_kind(Role::User, MessageKind::InjectedContext, Origin::Harness)
                        } else {
                            Message::new(Role::User)
                        }
                    }
                };
                m.timestamp = ts;
                if let Some(phase) = p.get("phase").and_then(Value::as_str) {
                    bag(&mut m).insert("phase".into(), Value::String(phase.into()));
                }
                m.content = blocks;
                out.push(m);
            }
        }
        "reasoning" => {
            // `summary` holds the user-visible reasoning summary; `content` (when present) holds the
            // raw chain-of-thought. Prefer raw content, fall back to summary. `encrypted_content`
            // carries the opaque replay blob.
            let summary = text_parts(p.get("summary"));
            let raw = text_parts(p.get("content"));
            let text = if !raw.is_empty() { raw } else { summary.clone() };
            let encrypted = p.get("encrypted_content").and_then(Value::as_str).map(str::to_string);
            if !text.is_empty() || encrypted.is_some() {
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                // When we surfaced raw content but a distinct summary also exists, keep the summary.
                if !summary.is_empty() && text != summary {
                    bag(&mut m).insert("reasoning_summary".into(), Value::String(summary));
                }
                m.content.push(Block::Thinking {
                    text: text.into(),
                    signature: None,
                    encrypted,
                    redacted: false,
                });
                out.push(m);
            }
        }
        // Inter-agent (swarm) message: `{id, author, recipient, content:[input_text |
        // encrypted_content]}` (models.rs:1036), produced by `InterAgentCommunication`
        // (protocol.rs:880-915) and preceded by a top-level `inter_agent_communication_metadata`
        // line carrying `trigger_turn`. From this thread's point of view a message it authored is
        // assistant output; one addressed to it is (user-role) input. The payload body is usually an
        // `encrypted_content` item — only the header text is readable.
        "agent_message" => {
            let author = str_field("author");
            let me = ctx.agent_path.as_deref().unwrap_or("/root");
            // Authored here: this thread's reply; addressed to it: a prompt — either way it came
            // from another agent, not a person.
            let mut m = if author == me {
                Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Subagent)
            } else {
                Message::of_kind(Role::User, MessageKind::Prompt, Origin::Subagent)
            };
            m.timestamp = ts;
            let items = p.get("content").and_then(Value::as_array);
            let encrypted = items.is_some_and(|a| {
                a.iter()
                    .any(|it| it.get("type").and_then(Value::as_str) == Some("encrypted_content"))
            });
            for it in items.into_iter().flatten() {
                if it.get("type").and_then(Value::as_str) == Some("input_text") {
                    if let Some(t) = it.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                        m.content.push(Block::Text { text: t.into() });
                    }
                }
            }
            let trigger = ctx.pending_trigger_turn.take();
            let b = bag(&mut m);
            b.insert("codex_event".into(), Value::String("agent_message".into()));
            b.insert("author".into(), Value::String(author));
            b.insert("recipient".into(), Value::String(str_field("recipient")));
            if encrypted {
                b.insert("encrypted".into(), Value::Bool(true));
            }
            if let Some(trigger) = trigger {
                b.insert("trigger_turn".into(), Value::Bool(trigger));
            }
            out.push(m);
        }
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                // local_shell_call may carry only a legacy `id`.
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let (name, input) = if ty == "local_shell_call" {
                // {status, action:{type:"exec", command:[...], ...}}
                let mut obj = Map::new();
                if let Some(action) = p.get("action") {
                    obj.insert("action".into(), action.clone());
                }
                if let Some(status) = p.get("status") {
                    obj.insert("status".into(), status.clone());
                }
                ("local_shell".to_string(), Value::Object(obj))
            } else {
                let name = str_field("name");
                // Tool `arguments`/`input` arrive as JSON-encoded *strings* (`custom_tool_call`'s
                // `input` is freeform text).
                let input = if let Some(args) = p.get("arguments").and_then(Value::as_str) {
                    openai::structured_arguments(args)
                } else if let Some(input) = p.get("input").and_then(Value::as_str) {
                    openai::structured_arguments(input)
                } else {
                    p.get("input").cloned().unwrap_or(Value::Null)
                };
                (name, input)
            };
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(status) = p.get("status").and_then(Value::as_str) {
                bag(&mut m).insert("status".into(), Value::String(status.into()));
            }
            // `namespace` (0.147+, e.g. `collaboration` for `send_message`/`spawn`/`wait`) is
            // first-class on the block; `name` stays the bare tool name so per-tool stats keep
            // grouping.
            m.content.push(Block::ToolUse {
                id: call_id,
                name,
                input: input.into(),
                namespace: p.get("namespace").and_then(Value::as_str).map(str::to_string),
            });
            out.push(m);
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = str_field("call_id");
            // Error-ness: cv's own `[error] ` string convention (see [`ERROR_PREFIX`]), or the
            // legacy object form `{success:false}` / `{metadata:{exit_code≠0}}` some older
            // recorders wrote. Real Codex 0.147+ files carry neither — a failed command's exit code
            // lives only in the `item_completed CommandExecution` note.
            let (content, images, is_error) = match output {
                Some((content, is_error)) => (content, Vec::new(), is_error),
                None => {
                    let (mut content, images) = coerce_output(p.get("output"));
                    let prefixed =
                        matches!(p.get("output"), Some(Value::String(_))) && content.starts_with(ERROR_PREFIX);
                    if prefixed {
                        content.drain(..ERROR_PREFIX.len());
                    }
                    let is_error = prefixed || output_is_error(p.get("output"));
                    (Text::Inline(content), images, is_error)
                }
            };
            let mut m = Message::new(Role::Tool);
            m.timestamp = ts;
            // 0.147+ outputs name their tool (`name`, `namespace`; models.rs:1113-1131).
            let tool_name = p.get("name").and_then(Value::as_str).map(str::to_string);
            if let Some(ns) = p.get("namespace").and_then(Value::as_str) {
                bag(&mut m).insert("namespace".into(), Value::String(ns.into()));
            }
            m.content.push(Block::ToolResult {
                tool_use_id: call_id,
                content,
                is_error,
                tool_name,
                status: Some(if is_error { "error" } else { "completed" }.into()),
                details: None,
            });
            // Structured outputs can return image content items alongside text.
            m.content.extend(images);
            out.push(m);
        }
        "tool_search_call" => {
            let call_id = str_field("call_id");
            let mut input = Map::new();
            if let Some(args) = p.get("arguments") {
                input.insert("arguments".into(), args.clone());
            }
            if let Some(exec) = p.get("execution") {
                input.insert("execution".into(), exec.clone());
            }
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            m.content.push(Block::ToolUse {
                id: call_id,
                name: "tool_search".into(),
                input: Value::Object(input).into(),
                namespace: None,
            });
            out.push(m);
        }
        "tool_search_output" => {
            let call_id = str_field("call_id");
            let content = p.get("tools").map(|t| t.to_string()).unwrap_or_else(|| "[]".into());
            let mut m = Message::new(Role::Tool);
            m.timestamp = ts;
            m.content.push(Block::ToolResult {
                tool_use_id: call_id,
                content: content.into(),
                is_error: false,
                tool_name: Some("tool_search".into()),
                status: None,
                details: None,
            });
            out.push(m);
        }
        "web_search_call" => {
            // Model-side web search; surface the query/queries as a tool call.
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let input = p.get("action").cloned().unwrap_or(Value::Null);
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(status) = p.get("status").and_then(Value::as_str) {
                bag(&mut m).insert("status".into(), Value::String(status.into()));
            }
            m.content.push(Block::ToolUse {
                id: call_id,
                name: "web_search".into(),
                input: input.into(),
                namespace: None,
            });
            out.push(m);
        }
        "image_generation_call" => {
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(rp) = p.get("revised_prompt").and_then(Value::as_str) {
                bag(&mut m).insert("revised_prompt".into(), Value::String(rp.into()));
            }
            // `result` is base64 image data; record a reference, not the bytes.
            m.content.push(Block::Image {
                media_type: None,
                data_ref: p
                    .get("result")
                    .and_then(Value::as_str)
                    .map(|_| "base64:inline".to_string()),
            });
            out.push(m);
        }
        // Mid-history compaction recorded inline as a response_item (vs. the top-level `compacted`
        // record). Only an opaque encrypted blob survives; note the boundary.
        "compaction" | "compaction_summary" | "context_compaction" => {
            let mut m = note(MessageKind::CompactionBoundary, ts, "[history compacted]".into());
            let b = bag(&mut m);
            b.insert("codex_event".into(), Value::String(ty.into()));
            if p.get("encrypted_content").and_then(Value::as_str).is_some() {
                b.insert("encrypted".into(), Value::Bool(true));
            }
            out.push(m);
        }
        _ => return false,
    }
    // The turn this item belongs to (from the passthrough), on every message it produced.
    if let Some(tid) = turn_id {
        for m in &mut out[before..] {
            bag(m).insert("turn_id".into(), tid.clone());
        }
    }
    true
}

/// Coerce a tool-call `output` into text plus any image blocks. On the wire `output` is one of:
/// a plain string; an object like `{output, metadata}` (legacy) or `{success, content}`; or an
/// array of structured content items (`input_text`/`input_image`/`encrypted_content`).
fn coerce_output(v: Option<&Value>) -> (String, Vec<Block>) {
    match v {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(items)) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for it in items {
                match it.get("type").and_then(Value::as_str) {
                    Some("input_image") | Some("output_image") => {
                        images.extend(openai::part_to_block(it, &openai::CODEX))
                    }
                    Some("encrypted_content") => {}
                    _ => {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            text.push(t.to_string());
                        }
                    }
                }
            }
            (text.join("\n"), images)
        }
        Some(o @ Value::Object(_)) => {
            // `{output: ...}` may itself nest a string or an array of content items.
            let inner = o.get("output");
            match inner {
                Some(Value::String(s)) => (s.clone(), Vec::new()),
                Some(arr @ Value::Array(_)) => coerce_output(Some(arr)),
                _ => {
                    // `{content: "...", success: bool}` or unknown — best-effort string.
                    let s = o
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| o.to_string());
                    (s, Vec::new())
                }
            }
        }
        Some(other) => (other.to_string(), Vec::new()),
        None => (String::new(), Vec::new()),
    }
}

/// Detect a failed tool result from the `success`/`exit_code`/`metadata` markers Codex emits.
fn output_is_error(v: Option<&Value>) -> bool {
    let Some(Value::Object(o)) = v else {
        return false;
    };
    if o.get("success").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    matches!(
        o.get("metadata")
            .and_then(|m| m.get("exit_code"))
            .and_then(Value::as_i64),
        Some(c) if c != 0
    )
}

fn top_ts(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp").and_then(Value::as_str).and_then(parse_ts)
}

/// Accumulates the cheap metadata `discover` needs, one record at a time. Factored out of [`scan`]
/// so it can be fed either the whole file (small sessions) or just a head+tail sample (huge ones).
#[derive(Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct CodexScan {
    id: String,
    cwd: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    message_count: usize,
    /// `subagent_history_start_ordinal` of the first `session_meta`: records below it are the
    /// parent's embedded history, not this thread's (they'd title every subagent with the parent's
    /// first prompt and count its turns).
    inherit_before: Option<u64>,
    /// This thread's `agent_path`, to tell a task handed TO it (title-worthy) from its own sends.
    agent_path: Option<String>,
    meta_seen: bool,
}

impl CodexScan {
    fn consider_title(&mut self, t: &str) {
        let trimmed = t.trim();
        // Skip Codex's injected preambles — they aren't the user's first words (see
        // [`is_injected_text`]).
        if self.title.is_none() && !trimmed.is_empty() && !is_injected_text(trimmed) {
            self.title = Some(crate::ir::truncate(trimmed, 80));
        }
    }

    fn feed_line(&mut self, line: &str) {
        if let Some(metadata) = metadata::project(line, self) {
            self.feed(&metadata);
        }
    }

    /// Ingest one `.jsonl` record.
    fn feed(&mut self, v: &Value) {
        if let Some(ts) = top_ts(v) {
            self.created_at.get_or_insert(ts);
            self.updated_at = Some(ts);
        }
        // Inherited prefix of a fork/subagent rollout: not this thread's turns (see `dispatch_line`).
        if matches!(
            (v.get("ordinal").and_then(Value::as_u64), self.inherit_before),
            (Some(o), Some(cut)) if o < cut
        ) && v.get("type").and_then(Value::as_str) != Some("session_meta")
        {
            return;
        }
        match v.get("type").and_then(Value::as_str) {
            None => {
                if self.id.is_empty() {
                    if let Some(i) = v.get("id").and_then(Value::as_str) {
                        self.id = i.to_string();
                    }
                }
            }
            Some("session_meta") => {
                let p = v.get("payload");
                if self.id.is_empty() {
                    if let Some(i) = p.and_then(|p| p.get("id")).and_then(Value::as_str) {
                        self.id = i.to_string();
                    }
                }
                if self.cwd.is_none() {
                    self.cwd = p.and_then(|p| p.get("cwd")).and_then(Value::as_str).map(PathBuf::from);
                }
                if !self.meta_seen {
                    self.meta_seen = true;
                    self.inherit_before = p
                        .and_then(|p| p.get("subagent_history_start_ordinal"))
                        .and_then(Value::as_u64);
                    self.agent_path = p
                        .and_then(|p| p.get("agent_path"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            Some("event_msg") => {
                let pt = v.pointer("/payload/type").and_then(Value::as_str);
                if pt == Some("user_message") {
                    self.message_count += 1;
                    if let Some(t) = v.pointer("/payload/message").and_then(Value::as_str) {
                        self.consider_title(t);
                    }
                } else if pt == Some("agent_message") {
                    self.message_count += 1;
                }
            }
            Some("response_item") if v.pointer("/payload/type").and_then(Value::as_str) == Some("message") => {
                // `developer`/`system` messages are injected instructions (`handle_item` makes them
                // System turns), not the user + assistant turns `SessionRef::message_count` counts.
                // An absent role is `user`, as in `handle_item`.
                let role = v.pointer("/payload/role").and_then(Value::as_str).unwrap_or("user");
                if matches!(role, "user" | "assistant") {
                    self.message_count += 1;
                }
                if role == "user" {
                    self.consider_title(&text_parts(v.pointer("/payload/content")));
                }
            }
            // A task handed to this thread by another agent is its prompt — the only one a
            // subagent thread usually gets (its `user`-role messages are the developer preamble).
            Some("response_item") if v.pointer("/payload/type").and_then(Value::as_str) == Some("agent_message") => {
                self.message_count += 1;
                let author = v.pointer("/payload/author").and_then(Value::as_str);
                if author != self.agent_path.as_deref() {
                    let text = v
                        .pointer("/payload/content")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter(|it| it.get("type").and_then(Value::as_str) == Some("input_text"))
                                .filter_map(|it| it.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    self.consider_title(&text);
                }
            }
            _ => {}
        }
    }

    /// Ingest a legacy single-object `.json` recording.
    fn feed_json_object(&mut self, root: &Value) {
        self.id = root
            .pointer("/session/id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.created_at = root
            .pointer("/session/timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        self.updated_at = self.created_at;
        if let Some(items) = root.get("items").and_then(Value::as_array) {
            for it in items {
                if it.get("type").and_then(Value::as_str) == Some("message") {
                    let role = it.get("role").and_then(Value::as_str).unwrap_or("user");
                    if matches!(role, "user" | "assistant") {
                        self.message_count += 1;
                    }
                    if role == "user" {
                        self.consider_title(&text_parts(it.get("content")));
                    }
                }
            }
        }
    }
}

/// Read at most `n` bytes from the start of `path`, trimmed back to the last complete line.
fn read_head(path: &Path, n: usize) -> Result<String> {
    use std::io::Read;
    let f = fs::File::open(path)?;
    let mut buf = Vec::with_capacity(n.min(1 << 20));
    f.take(n as u64).read_to_end(&mut buf)?;
    if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
        buf.truncate(pos);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read the last `n` bytes of `path` (the first line is likely partial — callers should skip it).
fn read_tail(path: &Path, n: usize) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(n as u64)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Files up to this size are scanned in full by [`scan`]; larger ones are sampled head+tail.
const FULL_SCAN_CAP: u64 = 8 << 20; // 8 MiB

/// Cheap metadata scan for `discover`. JSONL records validate discarded bodies without building
/// their trees. Files up to [`FULL_SCAN_CAP`] are scanned in full; larger ones
/// (Codex rollout logs can be hundreds of MB) are sampled head+tail so discovery never reads — and
/// JSON-parses — gigabytes just to list sessions. Exact content is parsed lazily on actual open.
fn scan(path: &Path) -> Result<SessionRef> {
    scan_capped(path, FULL_SCAN_CAP)
}

/// [`scan`] with an explicit full-scan cap (tests exercise the sampled path on a small file).
fn scan_capped(path: &Path, full_scan_cap: u64) -> Result<SessionRef> {
    const SAMPLE: usize = 1 << 20; // 1 MiB head, 1 MiB tail

    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let is_jsonl = is_jsonl_path(path);
    let mut s = CodexScan::default();

    if is_zst_path(path) {
        // Compressed (cold, > 7 days) rollout: no seeking, so stream the whole decode — these are
        // exactly the files whose bytes we never mmap.
        for line in open_rollout(path)?.lines() {
            let Ok(line) = line else { continue };
            s.feed_line(&line);
        }
    } else if is_jsonl {
        if size <= full_scan_cap {
            let text = fs::read_to_string(path)?;
            for line in text.lines() {
                s.feed_line(line);
            }
        } else {
            // Head: id / cwd / title / created_at all live near the top.
            let head = read_head(path, SAMPLE)?;
            for line in head.lines() {
                s.feed_line(line);
            }
            // Tail: the last record carries the real updated_at. Skip the (likely partial) first line.
            let tail = read_tail(path, SAMPLE)?;
            let tail = tail.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
            for line in tail.lines() {
                s.feed_line(line);
            }
            // The middle was never read, so the turn count is unknown, and `message_count` is a
            // contract (`SessionRef::message_count`: user + assistant turns, never a proxy). The
            // head+tail extrapolation that used to stand here was measured 35× low on one real
            // rollout and 50% high on another. Reading every byte is not the cheap alternative it
            // looks: one real `~/.codex` holds 1,353 rollouts over this cap (80 GB; 283 over 64 MiB,
            // the largest 700 MB), and a live one would be re-parsed on every `cv ls`. So a file
            // over the cap reports 0 — "not counted" — rather than a number that is wrong.
            s.message_count = 0;
        }
    } else {
        // Legacy single-object `.json` recordings are small; read fully.
        let text = fs::read_to_string(path)?;
        if let Ok(root) = serde_json::from_str::<Value>(&text) {
            s.feed_json_object(&root);
        }
    }

    if s.id.is_empty() {
        // `rollout-<ts>-<uuid>.jsonl[.zst]`: strip both suffixes.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        s.id = name
            .trim_end_matches(".zst")
            .trim_end_matches(".jsonl")
            .trim_end_matches(".json")
            .to_string();
    }

    Ok(SessionRef {
        id: s.id,
        harness: Harness::Codex,
        path: path.to_path_buf(),
        cwd: s.cwd,
        title: s.title,
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: s.message_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    #[derive(Default)]
    struct BufferedKept {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
        metadata: usize,
        dependencies: usize,
    }

    impl MessageSink for BufferedKept {
        fn message(&mut self, m: Message) -> Flow {
            self.messages.push(m);
            Flow::Continue
        }
        fn meta(&mut self, _: &Session) {
            self.metadata += 1;
        }
        fn dependency(&mut self, _: &Path) {
            self.dependencies += 1;
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), cp.clone()));
        }
    }

    fn buffered_pass(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Session {
        stream_jsonl(
            &r.id,
            open_rollout(&r.path).unwrap(),
            Some(r.path.clone()),
            detect_has_events(open_rollout(&r.path).unwrap()),
            opts,
            sink,
        )
    }

    // Independent old buffered transport/collector: preserve .lines(), trim, lean/full fallback,
    // held-message EOF handling, and corruption counts while the production driver tracks bytes.
    fn old_buffered_oracle(r: &SessionRef, opts: &ParseOptions) -> Session {
        let mut s = Session {
            source_path: Some(r.path.clone()),
            ..Session::new(String::new(), Harness::Codex)
        };
        let mut ctx = CodexCtx::for_opts(opts);
        let mut scratch = Vec::new();
        let mut sink = crate::stream::CollectSink::default();
        let mut skipped = 0;
        let has_events = detect_has_events(open_rollout(&r.path).unwrap());
        for line in open_rollout(&r.path).unwrap().lines() {
            let Ok(line) = line else {
                skipped += 1;
                continue;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v = if !opts.complete {
                lean::parse(line.as_bytes(), false).map(|l| l.v).ok()
            } else {
                None
            }
            .or_else(|| serde_json::from_str(line).ok());
            let Some(v) = v else {
                skipped += 1;
                continue;
            };
            dispatch_line(&mut s, &mut scratch, &v, has_events, &mut ctx);
            flush_all_but_held(&mut scratch, &mut sink);
        }
        for m in scratch {
            sink.message(m);
        }
        super::super::note_skipped_lines(&mut s, skipped);
        if s.id.is_empty() {
            s.id.clone_from(&r.id);
        }
        s.messages = sink.messages;
        s
    }

    #[test]
    fn buffered_resume_native_corpus_and_options_match_old_exact_suffixes() {
        let world = cv_testkit::World::in_process("codex_buffered_resume_corpus");
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/codex");
        let mut files = 0;
        let mut resumes = 0;
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            files += 1;
            let r = SessionRef::new("oracle", Harness::Codex, entry.path().to_path_buf());
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::lazy_extra(),
                ParseOptions::full(),
                ParseOptions::bulk(),
                ParseOptions::complete(),
            ] {
                let expected = old_buffered_oracle(&r, &opts);
                let mut full = BufferedKept::default();
                let mut shell = buffered_pass(&r, &opts, &mut full);
                shell.messages = full.messages.clone();
                assert_eq!(
                    serde_json::to_value(&shell).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
                for (index, (count, cp)) in full.checkpoints.iter().enumerate() {
                    let mut suffix = BufferedKept::default();
                    let mut resumed = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
                    assert_eq!(
                        serde_json::to_value(&suffix.messages).unwrap(),
                        serde_json::to_value(&full.messages[*count..]).unwrap()
                    );
                    resumed.messages = full.messages[..*count].iter().cloned().chain(suffix.messages).collect();
                    assert_eq!(
                        serde_json::to_value(resumed).unwrap(),
                        serde_json::to_value(&shell).unwrap()
                    );
                    assert_eq!(
                        suffix.checkpoints,
                        full.checkpoints[index + 1..]
                            .iter()
                            .map(|(n, cp)| (n - count, cp.clone()))
                            .collect::<Vec<_>>()
                    );
                    resumes += 1;
                }
            }
        }
        eprintln!("codex buffered native files={files} exact suffix resumes={resumes}");
        assert!(files >= 16 && resumes > 100);
    }

    #[test]
    fn buffered_resume_replays_pending_usage_partial_eof_and_declines_before_callbacks() {
        use std::io::Write as _;
        let world = cv_testkit::World::in_process("codex_buffered_resume_pending");
        let path = world.base.join("rollout-pending.jsonl");
        let prefix = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"model\":\"test\"}}\r\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
            "bad json\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"held\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{"
        );
        let prefix = prefix.replace("\"held\"", &format!("\"{}\"", "held_sentinel".repeat(100_000)));
        fs::write(&path, &prefix).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let opts = ParseOptions::full();
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 1, "held EOF reply is not part of the immutable prefix");
        assert_eq!(cp.offset as usize, prefix.find("bad json").unwrap());
        let state = serde_json::to_string(cp).unwrap();
        assert!(state.len() < 2048 && !state.contains("held_sentinel"));
        writeln!(
            fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            "\"total_token_usage\":{{\"input_tokens\":9,\"output_tokens\":3}},\"last_token_usage\":{{\"input_tokens\":9,\"output_tokens\":3}}}}}}}}"
        )
        .unwrap();
        let mut bytes = fs::read(&r.path).unwrap();
        bytes.extend_from_slice(b"\xff\n\n");
        bytes.extend_from_slice("\u{2003}{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"escaped \\\" \\u263a\"}}\u{2003}\r\n".as_bytes());
        fs::write(&r.path, bytes).unwrap();
        let mut suffix = BufferedKept::default();
        let mut resumed = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
        resumed.messages = first.messages[..*count]
            .iter()
            .cloned()
            .chain(suffix.messages)
            .collect();
        assert_eq!(resumed.messages[1].usage.as_ref().unwrap().input_tokens, Some(9));
        assert_eq!(
            serde_json::to_value(resumed).unwrap(),
            serde_json::to_value(old_buffered_oracle(&r, &opts)).unwrap()
        );

        let mut untouched = BufferedKept::default();
        assert!(adapter
            .try_resume(&r, &ParseOptions::complete(), &mut untouched, cp)
            .unwrap()
            .is_none());
        let mut elsewhere = r.clone();
        elsewhere.path = world.base.join("rollout-other.jsonl");
        assert!(adapter
            .try_resume(&elsewhere, &opts, &mut untouched, cp)
            .unwrap()
            .is_none());
        elsewhere.path = world.base.join("rollout.json");
        assert!(adapter
            .try_resume(&elsewhere, &opts, &mut untouched, cp)
            .unwrap()
            .is_none());
        elsewhere.path = world.base.join("rollout.jsonl.zst");
        assert!(adapter
            .try_resume(&elsewhere, &opts, &mut untouched, cp)
            .unwrap()
            .is_none());
        fs::write(&r.path, b"").unwrap();
        assert!(adapter.try_resume(&r, &opts, &mut untouched, cp).unwrap().is_none());
        assert!(untouched.messages.is_empty() && untouched.checkpoints.is_empty());
        assert_eq!((untouched.metadata, untouched.dependencies), (0, 0));
    }

    #[test]
    fn buffered_resume_append_work_excludes_stable_prefix_and_revalidates_detector() {
        use std::io::Write as _;
        let world = cv_testkit::World::in_process("codex_buffered_resume_append");
        let path = world.base.join("rollout-append.jsonl");
        let line = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n";
        fs::write(&path, line.repeat(20_000)).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let adapter = Codex::with_home(&world.home.join(".codex"));
        let opts = ParseOptions::full();
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let (count, cp) = first.checkpoints.last().unwrap();
        assert_eq!(*count, 20_000);
        fs::OpenOptions::new()
            .append(true)
            .open(&r.path)
            .unwrap()
            .write_all(line.repeat(200).as_bytes())
            .unwrap();
        let mut suffix = BufferedKept::default();
        let resumed = adapter.try_resume(&r, &opts, &mut suffix, cp).unwrap().unwrap();
        let mut whole = old_buffered_oracle(&r, &opts);
        assert_eq!(
            serde_json::to_value(&suffix.messages).unwrap(),
            serde_json::to_value(&whole.messages[*count..]).unwrap()
        );
        whole.messages.clear();
        assert_eq!(
            serde_json::to_value(resumed).unwrap(),
            serde_json::to_value(whole).unwrap()
        );
        assert_eq!(suffix.messages.len(), 200);
        let bytes = serde_json::to_vec(cp).unwrap().len();
        eprintln!(
            "codex buffered stable_prefix=20000 appended_callbacks=200 whole_callbacks=20200 checkpoint_bytes={bytes}"
        );
        assert!(bytes < 2048);

        let old = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"old\"}]}}\n";
        fs::write(&r.path, old).unwrap();
        let mut first = BufferedKept::default();
        buffered_pass(&r, &opts, &mut first);
        let cp = &first.checkpoints.last().unwrap().1;
        fs::OpenOptions::new()
            .append(true)
            .open(&r.path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let mut untouched = BufferedKept::default();
        assert!(adapter.try_resume(&r, &opts, &mut untouched, cp).unwrap().is_none());
        assert!(untouched.messages.is_empty() && untouched.checkpoints.is_empty());
        assert_eq!((untouched.metadata, untouched.dependencies), (0, 0));
    }

    /// A fixture under `tests/fixtures/codex/`, without its final newline (the text the old
    /// inline `lines.join("\n")` built).
    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/codex/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        text.strip_suffix('\n').map(str::to_string).unwrap_or(text)
    }

    /// A JSONL fixture's records, one string per line.
    fn fixture_lines(name: &str) -> Vec<String> {
        fixture(name).split('\n').map(str::to_string).collect()
    }

    /// Parse JSONL lines into a Session, checking the IR-v2 nesting invariant on the way out:
    /// every Codex fact must be in `extra["codex"]`, never flat (`crate::harness::assert_no_flat_keys`).
    /// Doing it here means every test below that builds a session enforces it for free.
    fn parse_jsonl(lines: &[impl std::borrow::Borrow<str>]) -> Session {
        let text = lines.join("\n");
        let s = parse_str("fallback-id", &text, true, None);
        crate::harness::assert_no_flat_keys(&s);
        s
    }

    /// [`parse_jsonl`] over a fixture's records.
    fn parse_fixture(name: &str) -> Session {
        parse_jsonl(&fixture_lines(name))
    }

    fn first_block<'a>(s: &'a Session, pred: impl Fn(&&'a Block) -> bool) -> &'a Block {
        s.messages
            .iter()
            .flat_map(|m| &m.content)
            .find(pred)
            .expect("matching block")
    }

    #[test]
    fn session_meta_seeds_id_cwd_git() {
        let s = parse_fixture("session_meta_seeds_id_cwd_git.jsonl");
        assert_eq!(s.id, "abc-123");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work")));
        let g = s.git.unwrap();
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.commit.as_deref(), Some("deadbeef"));
        assert_eq!(g.remote.as_deref(), Some("https://x/y"));
    }

    #[test]
    fn bare_header_provides_id() {
        // No session_meta: first line is a bare {id,timestamp} header.
        let s = parse_str("", &fixture("bare_header_provides_id.jsonl"), true, None);
        assert_eq!(s.id, "hdr-1");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn event_msg_dedups_response_item_text() {
        // When event_msgs are present, NL text comes from them and the response_item message dup is
        // skipped — so we expect exactly one user + one assistant text message.
        let s = parse_fixture("event_msg_dedups_response_item_text.jsonl");
        let texts: Vec<_> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["hello", "hi there"]);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
    }

    #[test]
    fn reasoning_prefers_content_keeps_summary_and_encrypted() {
        let s = parse_fixture("reasoning_prefers_content_keeps_summary_and_encrypted.jsonl");
        let m = &s.messages[0];
        match &m.content[0] {
            Block::Thinking { text, encrypted, .. } => {
                assert_eq!(text, "raw thoughts");
                assert_eq!(encrypted.as_deref(), Some("ENC"));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
        assert_eq!(
            m.extra["codex"].get("reasoning_summary").and_then(Value::as_str),
            Some("short summary")
        );
    }

    #[test]
    fn function_call_and_output_roundtrip_with_error() {
        let s = parse_fixture("function_call_and_output_roundtrip_with_error.jsonl");
        match first_block(&s, |b| matches!(b, Block::ToolUse { .. })) {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input.resolve().unwrap()["command"][0], "ls");
            }
            _ => unreachable!(),
        }
        match first_block(&s, |b| matches!(b, Block::ToolResult { .. })) {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content, "boom");
                assert!(is_error);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn custom_tool_call_string_input() {
        let s = parse_fixture("custom_tool_call_string_input.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "apply_patch");
                assert_eq!(
                    input.resolve().unwrap().as_ref(),
                    &Value::String("*** Begin Patch".into())
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            s.messages[0].extra["codex"].get("status").and_then(Value::as_str),
            Some("completed")
        );
    }

    #[test]
    fn local_shell_call_maps_to_tool_use() {
        let s = parse_fixture("local_shell_call_maps_to_tool_use.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "ls1");
                assert_eq!(name, "local_shell");
                assert_eq!(input.resolve().unwrap()["action"]["command"][1], "hi");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn web_search_call_surfaces_query() {
        let s = parse_fixture("web_search_call_surfaces_query.jsonl");
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "web_search");
                assert_eq!(input.resolve().unwrap()["query"], "rust serde");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_search_call_and_output() {
        let s = parse_fixture("tool_search_call_and_output.jsonl");
        assert!(matches!(&s.messages[0].content[0], Block::ToolUse { name, .. } if name == "tool_search"));
        assert!(
            matches!(&s.messages[1].content[0], Block::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == "[]")
        );
    }

    #[test]
    fn user_message_with_input_image() {
        let s = parse_fixture("user_message_with_input_image.jsonl");
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::Text { text } if text == "see this"));
        match &m.content[1] {
            Block::Image { media_type, data_ref } => {
                assert_eq!(media_type.as_deref(), Some("image/png"));
                assert_eq!(data_ref.as_deref(), Some("base64:inline"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn view_image_event_becomes_image_block() {
        let s = parse_fixture("view_image_event_becomes_image_block.jsonl");
        match &s.messages[0].content[0] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("/tmp/x.png")),
            _ => unreachable!(),
        }
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_fco_string_output_spans_and_resolves() {
        use std::io::Write;
        // A function_call_output whose string output is > INLINE_MAX and contains escapes (\n, ").
        let body = "out line \"q\"\nmore ".repeat(400); // ~7 KB, with \n and " escapes
        let rec = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "type": "response_item",
            "payload": { "type": "function_call_output", "call_id": "c1", "output": body }
        })
        .to_string();
        let dir = std::env::temp_dir().join(format!("cv-codex-span-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-x.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{rec}").unwrap();
        }
        let data = std::fs::read(&path).unwrap();
        let mut sink = crate::stream::CollectSink::default();
        let mut s = stream_jsonl_spans("fallback", &data, Some(path.clone()), false, false, &mut sink);
        s.messages = sink.messages;

        let content = s
            .messages
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    Block::ToolResult { content, .. } => Some(content),
                    _ => None,
                })
            })
            .expect("tool result present");
        assert!(content.is_span(), "giant fco output should be a span");
        let resolver = s.resolver();
        assert_eq!(
            content.resolve(&resolver),
            body.as_str(),
            "span must resolve to the exact output"
        );

        // And it matches the inline (bulk) parse of the same record.
        let inline = parse_str("fallback", &String::from_utf8(data).unwrap(), true, Some(path.clone()));
        let inline_c = inline
            .messages
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    Block::ToolResult { content, .. } => content.inline_str(),
                    _ => None,
                })
            })
            .expect("inline tool result");
        assert_eq!(inline_c, body.as_str());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "mmap")]
    fn assert_giant_modes_match(text: &str) {
        let dir = std::env::temp_dir().join(format!("cv-codex-giant-parity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-parity.jsonl");
        std::fs::write(&path, text).unwrap();
        let mut sink = crate::stream::CollectSink::default();
        let mut lazy = stream_jsonl_spans("fallback", text.as_bytes(), Some(path.clone()), false, false, &mut sink);
        lazy.messages = sink.messages;
        lazy.materialize().unwrap();
        let full = parse_str("fallback", text, true, Some(path));
        assert_eq!(serde_json::to_value(lazy).unwrap(), serde_json::to_value(full).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_output_preserves_metadata_and_inherited_filtering() {
        for kind in ["function_call_output", "custom_tool_call_output"] {
            for ordinal in [2, 12] {
                let meta = serde_json::json!({
                    "type": "session_meta", "payload": {
                        "id": "child", "subagent_history_start_ordinal": 10,
                    }
                });
                let result = serde_json::json!({
                    "type": "response_item", "ordinal": ordinal,
                    "timestamp": "2026-01-02T00:00:00Z",
                    "payload": {
                        "type": kind, "call_id": "call", "output": "result\n".repeat(1000),
                        "name": "wait", "namespace": "collaboration",
                        "internal_chat_message_metadata_passthrough": {
                            "turn_id": "turn", "create_time": 1767225600.0,
                        },
                    }
                });
                assert_giant_modes_match(&format!("{meta}\n{result}\n"));
            }
        }
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn giant_output_strips_json_escaped_error_prefix() {
        let result = serde_json::json!({
            "type": "response_item", "payload": {
                "type": "function_call_output", "call_id": "call",
                "output": format!("[error] {}", "failure\n".repeat(1000)),
            }
        });
        let text = result.to_string().replace("[error] ", r"\u005berror\u005d\u0020");
        assert_giant_modes_match(&format!("{text}\n"));
    }

    /// The span path under offset-stamping marks every message with the byte offset of the record
    /// that created it — including an assistant a later `token_count` amends, the stale-snapshot
    /// carrier, and a giant span output.
    #[cfg(feature = "mmap")]
    #[test]
    fn offset_stamps_mark_each_message_creating_record() {
        let big = "giant tool output line\n".repeat(400); // > INLINE_MAX → a Span
        let lines = [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"seek-1","cwd":"/work"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:06Z","type":"response_item",
                "payload":{"type":"function_call_output","call_id":"c1","output":big}})
            .to_string(),
            r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":9.0}}}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:08Z","type":"event_msg","payload":{"type":"agent_message","message":"bye"}}"#.to_string(),
        ];
        let data = format!("{}\n", lines.join("\n")).into_bytes();
        let has_events = detect_has_events(std::io::Cursor::new(&data[..]));
        assert!(has_events);

        let mut full = crate::stream::CollectSink::default();
        let s = stream_jsonl_spans("fb", &data, None, has_events, true, &mut full);
        let full = full.messages;
        assert_eq!(s.model.as_deref(), Some("gpt-test"));
        // user, assistant(+usage), tool-use, giant span result, carrier, trailing assistant.
        assert_eq!(full.len(), 6);
        assert!(full[1].usage.is_some(), "token_count attached to held assistant");
        assert!(full[3]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content.is_span())));
        let offs: Vec<u64> = full
            .iter()
            .map(|m| {
                m.extra
                    .get(crate::offsets::OFFSET_KEY)
                    .and_then(Value::as_u64)
                    .expect("every message stamped")
            })
            .collect();
        let line_starts: Vec<u64> = std::iter::once(0)
            .chain(
                data.iter()
                    .enumerate()
                    .filter(|(_, &b)| b == b'\n')
                    .map(|(i, _)| i as u64 + 1),
            )
            .collect();
        // user, assistant, function_call, its output, carrier, assistant: lines 2, 3, 5, 6, 7, 8.
        let want: Vec<u64> = [2, 3, 5, 6, 7, 8].iter().map(|&l| line_starts[l]).collect();
        assert_eq!(offs, want);
    }

    #[test]
    fn token_count_attaches_usage_to_assistant() {
        let s = parse_fixture("token_count_attaches_usage_to_assistant.jsonl");
        let m = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        let u = m.usage.as_ref().expect("usage attached");
        assert_eq!(
            u.input_tokens,
            Some(90),
            "Codex's 100 includes the 10 cached; the IR's is disjoint"
        );
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(10));
        assert!(m.extra["codex"].get("rate_limits").is_some());
        assert!(m.extra["codex"].get("model_context_window").is_some());
    }

    #[test]
    fn a_reemitted_token_count_counts_no_second_call() {
        // Codex re-emits its last snapshot (same `total_token_usage`) after the next user message;
        // before, that stale `last_token_usage` landed on the following reply as a second call.
        // The third snapshot's running total moved, so it is a real call and keeps its usage.
        let tc = |total: u64, last: u64| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"output_tokens":0,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{last},"output_tokens":0,"total_tokens":{last}}}}}}}}}"#
            )
        };
        let reply = |t: &str| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{{"type":"agent_message","message":"{t}"}}}}"#
            )
        };
        let user = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"again"}}"#;
        let lines = [
            reply("one"),
            tc(100, 100),
            user.to_string(),
            reply("two"),
            tc(100, 100),
            tc(250, 150),
        ];
        let s = parse_str("s", &lines.join("\n"), true, None);
        let usages: Vec<Option<u64>> = s
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.usage.as_ref().and_then(|u| u.input_tokens))
            .collect();
        assert_eq!(
            usages.iter().flatten().sum::<u64>(),
            250,
            "the thread's running total, counted once: {usages:?}"
        );
        assert_eq!(usages.iter().flatten().count(), 2, "{usages:?}");
    }

    #[test]
    fn stream_attaches_usage_like_parse() {
        // Task-4 regression guard: the streaming path holds back the trailing assistant message so
        // a `token_count` event can attach usage to it — and must agree with `parse_str` exactly,
        // including the carrier message for a stale snapshot and the EOF flush of a held message.
        // Lines 4-6: a tool exchange, then a token_count that trails the *tool result* (the
        // re-emitted snapshot codex writes after non-assistant records) — a carrier on both paths.
        // Line 7: trailing assistant message with no token_count after it: held, then flushed at EOF.
        let lines = fixture_lines("stream_attaches_usage_like_parse.jsonl");
        let text = lines.join("\n");
        let parsed = parse_str("s", &text, true, None);

        let mut sink = crate::stream::CollectSink::default();
        let has_events = detect_has_events(std::io::Cursor::new(text.as_bytes()));
        let mut streamed = stream_jsonl(
            "s",
            std::io::Cursor::new(text.as_bytes()),
            None,
            has_events,
            &ParseOptions::full(),
            &mut sink,
        );
        streamed.messages = sink.messages;

        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "parse and stream must produce identical sessions"
        );
        // The first turn's usage attached to the assistant message it trails — on both paths.
        for s in [&parsed, &streamed] {
            let m = &s.messages[1];
            assert_eq!(m.role, Role::Assistant);
            let u = m.usage.as_ref().expect("usage attached");
            assert_eq!(
                (u.input_tokens, u.output_tokens, u.cache_read_tokens),
                (Some(90), Some(20), Some(10))
            );
        }
        // function_call ToolUse msg attached nothing (it has its own pending usage slot untouched);
        // the stale snapshot became a carrier; the trailing assistant survived the EOF flush.
        assert_eq!(parsed.messages.last().unwrap().text().as_deref(), Some("bye"));
    }

    #[test]
    fn event_detector_modern_old_and_bounded() {
        let resp_user = |text: &str| {
            format!(
                r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
            )
        };
        let event_user = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#;
        let filler =
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;

        // Modern shape: preamble response_items, the real user response_item, then its event_msg
        // echo a few records later (codex writes the response_item FIRST) → has_events.
        let mut modern = vec![resp_user("<environment_context>…"), resp_user("real question")];
        modern.extend(std::iter::repeat_n(filler.to_string(), 5));
        modern.push(event_user.to_string());
        let text = modern.join("\n");
        assert!(detect_has_events(std::io::Cursor::new(text.as_bytes())));

        // Old format: NL response_items, no event echo ever → not has_events.
        let old = [resp_user("q"), resp_user("a")].join("\n");
        assert!(!detect_has_events(std::io::Cursor::new(old.as_bytes())));

        // The pass is bounded: it commits to "old format" LOOKAHEAD records past the first NL
        // response_item instead of scanning to EOF (the old whole-file pre-pass read a
        // multi-hundred-MB rollout twice). Verified by feeding records by hand and watching for
        // the Stop verdict.
        let mut det = EventDetector::default();
        let resp: Value = serde_json::from_str(&resp_user("q")).unwrap();
        assert_eq!(det.feed(&resp), Flow::Continue);
        let fill: Value = serde_json::from_str(filler).unwrap();
        let mut fed = 0u32;
        loop {
            fed += 1;
            assert!(
                fed <= EventDetector::LOOKAHEAD + 1,
                "detector must stop within the window"
            );
            if det.feed(&fill) == Flow::Stop {
                break;
            }
        }
        assert!(!det.found);

        // …and an event_msg inside the window still wins.
        let mut det = EventDetector::default();
        det.feed(&resp);
        for _ in 0..EventDetector::LOOKAHEAD {
            assert_eq!(det.feed(&fill), Flow::Continue);
        }
        let ev: Value = serde_json::from_str(event_user).unwrap();
        assert_eq!(det.feed(&ev), Flow::Stop);
        assert!(det.found);
    }

    #[test]
    fn corrupt_lines_are_counted_identically_on_both_paths() {
        // A live/damaged rollout: one corrupt line amid good records. Both paths must (a) keep the
        // good records, (b) surface the same `extra["cv"]["skipped_lines"]` count (cv's own parse
        // diagnostic, not a harness fact — see `ir::CV_NAMESPACE`), and (c) stay
        // byte-identical to each other. Clean files get NO `cv` bag (see the other tests'
        // sessions, which assert exact JSON equality without it).
        // Line 2 is truncated mid-write.
        let lines = fixture_lines("corrupt_lines_are_counted_identically_on_both_paths.jsonl");
        let text = lines.join("\n");
        let parsed = parse_str("s", &text, true, None);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(
            parsed
                .extra
                .get("cv")
                .and_then(|v| v.get("skipped_lines"))
                .and_then(Value::as_u64),
            Some(1),
            "the corrupt line is tolerated but counted"
        );
        assert!(
            !parsed.extra.contains_key("skipped_lines"),
            "the diagnostic is namespaced, never flat"
        );

        let mut sink = crate::stream::CollectSink::default();
        let has_events = detect_has_events(std::io::Cursor::new(text.as_bytes()));
        let mut streamed = stream_jsonl(
            "s",
            std::io::Cursor::new(text.as_bytes()),
            None,
            has_events,
            &ParseOptions::full(),
            &mut sink,
        );
        streamed.messages = sink.messages;
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&streamed).unwrap(),
            "skip accounting must not diverge parse from stream"
        );
    }

    #[test]
    fn compacted_record_is_a_boundary_plus_a_summary_when_readable() {
        // Legacy shape: a human-readable `message` — the boundary, then the summary that seeds the
        // next window.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable.jsonl");
        let kinds: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::CompactionSummary, Origin::Harness),
            ]
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("[history compacted]"));
        assert_eq!(s.messages[0].extra["codex"]["replacement_history_len"], 1);
        assert_eq!(s.messages[0].extra["codex"]["codex_event"], "compacted");
        assert_eq!(s.messages[1].text().as_deref(), Some("summary text"));

        // 0.147+ shape: `message` is "" and the replacement is an encrypted `compaction` item —
        // nothing readable seeds the window, so only the boundary is emitted.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable-2.jsonl");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[0].extra["codex"]["replacement_history_len"], 2);
        assert_eq!(s.messages[0].extra["codex"]["window_number"], 1);

        // The inline `response_item compaction` (mid-history) is a boundary too.
        let s = parse_fixture("compacted_record_is_a_boundary_plus_a_summary_when_readable-3.jsonl");
        assert_eq!(s.messages[0].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[0].extra["codex"]["encrypted"], true);
    }

    #[test]
    fn turn_context_records_model_change() {
        let s = parse_fixture("turn_context_records_model_change.jsonl");
        assert_eq!(s.model.as_deref(), Some("gpt-5.3-codex-high"));
        let note = s.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert_eq!(note.kind, MessageKind::ModelChange);
        assert_eq!(note.origin, Origin::Harness);
        assert_eq!(
            note.text().as_deref(),
            Some("[model changed: gpt-5.3-codex → gpt-5.3-codex-high]")
        );
        assert_eq!(
            note.model.as_deref(),
            Some("gpt-5.3-codex-high"),
            "the new model is first-class on the note"
        );
        assert!(bag_get(note, "model").is_none(), "no bag copy of a first-class field");
        assert_eq!(note.extra["codex"]["codex_event"], "turn_context");
    }

    #[test]
    fn turn_context_effort_change_is_a_model_change_note_without_a_model() {
        let s = parse_fixture("turn_context_effort_change_is_a_model_change_note_without_a_model.jsonl");
        assert_eq!(s.extra["codex"]["reasoning_effort"], "xhigh");
        assert_eq!(
            s.messages.len(),
            1,
            "the first record seeds silently; the agreeing third adds nothing"
        );
        let note = &s.messages[0];
        assert_eq!(note.kind, MessageKind::ModelChange);
        assert_eq!(note.text().as_deref(), Some("[effort changed: high → xhigh]"));
        assert!(note.model.is_none(), "the model did not change");
        // Every `turn_context` fact the note carries is nested under `extra["codex"]`, never flat
        // (`parse_jsonl` also runs the whole-session check).
        assert_eq!(note.extra["codex"]["effort"], "xhigh");
        assert_eq!(note.extra["codex"]["turn_id"], "t1");
        assert_eq!(note.extra["codex"]["cwd"], "/w");
        assert_eq!(note.extra.keys().collect::<Vec<_>>(), ["codex"]);
    }

    #[test]
    fn legacy_2025_json_layout() {
        let text = fixture("legacy_2025_json_layout.json");
        let s = parse_str("fallback", &text, false, None);
        assert_eq!(s.id, "legacy-1");
        // user text, reasoning, tool use, tool result => 4 messages (no event_msg dedup in legacy).
        assert_eq!(s.messages.len(), 4);
        assert!(s
            .messages
            .iter()
            .any(|m| matches!(m.content.first(), Some(Block::Thinking { .. }))));
        assert!(s
            .messages
            .iter()
            .any(|m| matches!(m.content.first(), Some(Block::ToolResult { content, .. }) if content == "ok")));
    }

    /// Developer smoke test against the real on-disk corpus (run with `--ignored`). Parses every
    /// discovered Codex session and asserts no panics + that every tool result references a tool use.
    #[test]
    #[ignore = "requires local ~/.codex corpus"]
    fn real_corpus_parses_without_panic() {
        let cx = Codex::detect();
        let refs = cx.discover().expect("discover");
        eprintln!("codex corpus: {} sessions", refs.len());
        let mut tool_uses = 0usize;
        let mut tool_results = 0usize;
        let mut images = 0usize;
        let mut thinking = 0usize;
        for r in &refs {
            let s = cx.parse(r).expect("parse");
            assert!(!s.id.is_empty());
            for m in &s.messages {
                for b in &m.content {
                    match b {
                        Block::ToolUse { .. } => tool_uses += 1,
                        Block::ToolResult { .. } => tool_results += 1,
                        Block::Image { .. } => images += 1,
                        Block::Thinking { .. } => thinking += 1,
                        _ => {}
                    }
                }
            }
        }
        eprintln!("tool_uses={tool_uses} tool_results={tool_results} images={images} thinking={thinking}");
        assert!(tool_uses > 0, "expected some tool calls in the corpus");
    }

    /// Parse a `.jsonl` transcript under explicit options via the streaming path (the only entry
    /// that takes `ParseOptions`), collecting messages like `parse_jsonl` does.
    fn stream_jsonl_with(lines: &[impl std::borrow::Borrow<str>], opts: &ParseOptions) -> Session {
        let text = lines.join("\n");
        let has_events = detect_has_events(std::io::Cursor::new(text.as_bytes()));
        let mut sink = crate::stream::CollectSink::default();
        let mut s = stream_jsonl(
            "fallback-id",
            std::io::Cursor::new(text.as_bytes()),
            None,
            has_events,
            opts,
            &mut sink,
        );
        s.messages = sink.messages;
        s
    }

    #[test]
    fn session_meta_swarm_fields_land_in_extra_and_first_meta_wins() {
        // The 0.154 subagent-rollout head: the child's meta (with swarm fields and an inherited-prefix
        // cutoff), the parent's meta embedded as line 1, then the parent's history up to the cutoff.
        let s = parse_fixture("session_meta_swarm_fields_land_in_extra_and_first_meta_wins.jsonl");
        assert_eq!(s.id, "child-1", "the first session_meta is canonical");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/ember/dev/minidregg")));
        assert_eq!(s.extra["codex"]["session_id"], "root-1");
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(s.lineage.forked_from.as_deref(), Some("parent-1"));
        assert_eq!(s.extra["codex"]["thread_source"], "subagent");
        assert_eq!(s.extra["codex"]["agent_nickname"], "Newton");
        assert_eq!(
            s.lineage.agent_path.as_deref(),
            Some("/root/cycle_client"),
            "the parent's /root did not overwrite it"
        );
        assert_eq!(
            s.extra["codex"]["source"]["subagent"]["thread_spawn"]["depth"], 1,
            "object-valued source kept raw"
        );
        assert_eq!(s.extra["codex"]["model_provider"], "openai");
        assert_eq!(s.extra["codex"]["history_mode"], "paginated");
        assert_eq!(s.extra["codex"]["subagent_history_start_ordinal"], 4);
        assert_eq!(s.extra["codex"]["cli_version"], "0.154.0");
        assert!(
            !s.extra["codex"].as_object().unwrap().contains_key("agent_role"),
            "null fields are not stashed"
        );
    }

    #[test]
    fn inherited_prefix_is_skipped_lean_and_tagged_complete() {
        // Ordinals 0-3 are the parent's embedded history (cutoff 4): its turn_context still seeds
        // the child's model, but its messages are not the child's turns.
        let lines = fixture_lines("inherited_prefix_is_skipped_lean_and_tagged_complete.jsonl");
        let lean = parse_jsonl(&lines);
        assert_eq!(
            lean.model.as_deref(),
            Some("gpt-6-astra"),
            "inherited turn_context still seeds the model"
        );
        assert_eq!(lean.extra["codex"]["reasoning_effort"], "xhigh");
        let texts: Vec<_> = lean.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["child's own reply"],
            "the parent's prompt is not the child's turn"
        );
        // create_time (epoch seconds, float) beats the envelope timestamp; turn_id rides along.
        let m = &lean.messages[0];
        assert_eq!(m.timestamp.map(|t| t.timestamp()), Some(1789845000));
        assert_eq!(m.extra["codex"]["turn_id"], "t1");

        let complete = stream_jsonl_with(&lines, &ParseOptions::complete());
        let texts: Vec<_> = complete.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["parent's original prompt", "child's own reply"]);
        assert_eq!(complete.messages[0].extra["codex"]["inherited"], true);
        assert_eq!(
            complete.messages[0].timestamp.map(|t| t.timestamp()),
            Some(1789840968),
            "the fork-time envelope stamp is not used"
        );
        assert!(bag_get(&complete.messages[1], "inherited").is_none());

        // parse == stream for the lean pass too
        let streamed = stream_jsonl_with(&lines, &ParseOptions::full());
        assert_eq!(
            serde_json::to_value(&lean).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn agent_message_pairs_with_preceding_metadata_and_attributes_by_author() {
        let lines = fixture_lines("agent_message_pairs_with_preceding_metadata_and_attributes_by_author.jsonl");
        let s = parse_jsonl(&lines);
        let inbound = &s.messages[0];
        assert_eq!(inbound.role, Role::User, "addressed to this thread ⇒ input");
        assert_eq!(
            (inbound.kind, inbound.origin),
            (MessageKind::Prompt, Origin::Subagent),
            "a task from another agent is a prompt, but not a human's"
        );
        assert!(inbound.text().unwrap().starts_with("Message Type: NEW_TASK"));
        assert_eq!(inbound.extra["codex"]["codex_event"], "agent_message");
        assert_eq!(inbound.extra["codex"]["author"], "/root");
        assert_eq!(inbound.extra["codex"]["recipient"], "/root/cycle_client");
        assert_eq!(inbound.extra["codex"]["encrypted"], true);
        assert_eq!(
            inbound.extra["codex"]["trigger_turn"], true,
            "the metadata line that preceded it"
        );
        // namespace kept on the call and its output; name stays bare
        let call = &s.messages[1];
        assert!(matches!(&call.content[0], Block::ToolUse { name, .. } if name == "send_message"));
        assert!(call
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { namespace: Some(ns), .. } if ns == "collaboration")));
        let out = &s.messages[2];
        assert!(
            matches!(&out.content[0], Block::ToolResult { tool_name, .. } if tool_name.as_deref() == Some("send_message"))
        );
        assert_eq!(out.extra["codex"]["namespace"], "collaboration");
        assert_eq!((out.kind, out.origin), (MessageKind::ToolResult, Origin::Harness));
        let outbound = &s.messages[3];
        assert_eq!(outbound.role, Role::Assistant, "authored by this thread ⇒ output");
        assert_eq!((outbound.kind, outbound.origin), (MessageKind::Reply, Origin::Subagent));
        assert_eq!(outbound.extra["codex"]["trigger_turn"], false);
        assert!(outbound.extra["codex"].get("encrypted").is_none());
        // identical on the streaming path (the pairing lives in parser state, not the sink)
        let streamed = stream_jsonl_with(&lines, &ParseOptions::full());
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn item_completed_items_become_system_notes_except_twins() {
        let lines = fixture_lines("item_completed_items_become_system_notes_except_twins.jsonl");
        let s = parse_jsonl(&lines);
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "$ /bin/zsh -lc cargo test → failed (exit 101)",
                "[file change completed] /Users/ember/dev/x/src/lib.rs",
                "[sub-agent /root/cycle_client/helper started]",
                "[sub-agent /root/cycle_client/helper completed]",
                "[viewed image: file:///Users/ember/shot.png]",
                "[mcp node_repl.js] failed: {\"message\":\"boom\"}",
                "[web search] rust zstd crate · ruzstd docs",
                "1. read\n2. fix",
            ],
            "Reasoning/AgentMessage twins and clock.sleep emit nothing"
        );
        assert!(s
            .messages
            .iter()
            .all(|m| m.role == Role::System && m.origin == Origin::Harness));
        // Sub-agent lifecycle is first-class; every other item is a harness notice.
        let kinds: Vec<_> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::SubagentSpawn,
                MessageKind::SubagentReturn,
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::Notice,
            ]
        );
        assert_eq!(s.messages[2].extra["codex"]["agent_thread_id"], "grand-1");
        assert_eq!(s.messages[2].extra["codex"]["agent_path"], "/root/cycle_client/helper");
        assert_eq!(s.messages[3].extra["codex"]["item"]["kind"], "completed");
        assert!(
            s.messages
                .iter()
                .all(|m| !m.content.iter().any(|b| matches!(b, Block::Image { .. }))),
            "the viewed image is not duplicated as an Image block (the tool output already carries it)"
        );
        let exec = &s.messages[0];
        assert_eq!(exec.extra["codex"]["codex_event"], "item_completed");
        assert_eq!(exec.extra["codex"]["item_type"], "CommandExecution");
        assert_eq!(exec.extra["codex"]["item"]["exit_code"], 101);
        assert_eq!(exec.extra["codex"]["item"]["stdout"], "error: test failed");
        assert_eq!(exec.extra["codex"]["turn_id"], "t1");
        assert_eq!(exec.extra["codex"]["started_at_ms"], 1789841584000u64);
        assert_eq!(
            exec.timestamp.map(|t| t.timestamp_millis()),
            Some(1789841587000),
            "completed_at_ms is the note's time"
        );
        assert_eq!(s.messages[7].extra["codex"]["codex_event"], "plan");
    }

    #[test]
    fn token_usage_record_is_authoritative_and_its_token_count_merges() {
        let lines = fixture_lines("token_usage_record_is_authoritative_and_its_token_count_merges.jsonl");
        let s = parse_jsonl(&lines);
        assert_eq!(
            s.messages.len(),
            2,
            "no carrier: the token_count merged into the record-sourced usage"
        );
        let m = &s.messages[0];
        let u = m.usage.as_ref().expect("usage from the record");
        assert_eq!(
            (
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.output_tokens
            ),
            // 35507 on disk includes both cache counts: 35507 - 34432 - 512 = 563 uncached.
            (Some(563), Some(34432), Some(512), Some(55))
        );
        assert_eq!(
            u.reasoning_tokens,
            Some(7),
            "reasoning_output_tokens → reasoning_tokens"
        );
        assert_eq!(u.cost_usd, None, "Codex stores no cost");
        assert_eq!(m.extra["codex"]["usage_source"], "token_usage_record");
        assert_eq!(m.extra["codex"]["response_id"], "resp_1");
        assert_eq!(m.extra["codex"]["turn_id"], "t1");
        assert_eq!(m.extra["codex"]["thread_token_usage"]["input_tokens"], 70000);
        assert!(
            m.extra["codex"].get("rate_limits").is_some(),
            "token_count's snapshot merged in"
        );
        assert_eq!(m.extra["codex"]["model_context_window"], 258400);
        // and the streaming path (which holds the message across the two records) agrees
        let streamed = stream_jsonl_with(&lines, &ParseOptions::full());
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn thread_settings_applied_aborts_and_rollbacks_are_noted() {
        let lines = fixture_lines("thread_settings_applied_aborts_and_rollbacks_are_noted.jsonl");
        let s = parse_jsonl(&lines);
        assert_eq!(s.model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.extra["codex"]["reasoning_effort"], "xhigh");
        assert_eq!(s.extra["codex"]["thread_settings"]["model_provider_id"], "openai");
        assert_eq!(s.extra["codex"]["thread_settings"]["personality"], "pragmatic");
        assert_eq!(s.extra["codex"]["last_agent_message"], "all done here");
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "[model changed: gpt-6-astra → gpt-6-vega; effort changed: high → xhigh]",
                "[turn aborted: interrupted]",
                "[rolled back 2 turns]"
            ],
            "the settings event notes the switch once; the agreeing turn_context adds nothing"
        );
        let kinds: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            kinds,
            vec![
                (Role::System, MessageKind::ModelChange, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Branch, Origin::Harness),
            ]
        );
        assert_eq!(s.messages[0].model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.messages[0].extra["codex"]["codex_event"], "thread_settings_applied");
        assert_eq!(s.messages[1].extra["codex"]["duration_ms"], 10696120u64);
        assert_eq!(s.messages[2].extra["codex"]["num_turns"], 2);
    }

    #[test]
    fn error_prefix_marks_tool_failure_and_is_stripped() {
        let s = parse_fixture("error_prefix_marks_tool_failure_and_is_stripped.jsonl");
        let results: Vec<(&str, bool, Option<&str>)> = s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult {
                    content,
                    is_error,
                    status,
                    ..
                } => Some((&**content, *is_error, status.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("command not found: frobnicate", true, Some("error")),
                ("is only a marker at the very start", true, Some("error")),
                ("[error] inside an array is plain text", false, Some("completed")),
            ]
        );
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn error_prefix_survives_the_giant_span_path() {
        use std::io::Write;
        let body = format!("[error] {}", "boom line\n".repeat(600)); // > INLINE_MAX
        let line = serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item",
            "payload":{"type":"function_call_output","call_id":"c1","output":body}})
        .to_string();
        let dir = std::env::temp_dir().join(format!("cv-codex-err-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-01-01T00-00-00-err.jsonl");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let data = std::fs::read(&path).unwrap();
        let mut sink = crate::stream::CollectSink::default();
        let s = stream_jsonl_spans("x", &data, Some(path.clone()), false, false, &mut sink);
        let m = &sink.messages[0];
        let Block::ToolResult { content, is_error, .. } = &m.content[0] else {
            panic!()
        };
        assert!(is_error);
        let resolved = content.resolve(&s.resolver()).into_owned();
        assert!(
            resolved.starts_with("boom line\n"),
            "prefix stripped from the span: {:?}",
            &resolved[..20]
        );
        assert!(!resolved.contains("[error]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn zstd_compressed_rollouts_are_discovered_scanned_and_parsed() {
        let _world = cv_testkit::World::in_process("codex-zstd");
        let lines = fixture_lines("zstd_compressed_rollouts_are_discovered_scanned_and_parsed.jsonl");
        let raw = lines.join("\n") + "\n";
        let zst = ruzstd::encoding::compress_to_vec(raw.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
        let root = std::env::temp_dir().join(format!("cv-codex-zst-{}", uuid::Uuid::new_v4()));
        let day = root.join("sessions").join("2026").join("08").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-2026-08-01T10-00-00-cold-1.jsonl.zst");
        std::fs::write(&path, &zst).unwrap();
        std::fs::write(day.join("rollout-2026-08-01T10-00-00-cold-1.jsonl.tmp"), b"{}").unwrap();

        let cx = Codex {
            roots: vec![root.join("sessions")],
        };
        let refs = cx.discover().unwrap();
        assert_eq!(refs.len(), 1, "the .zst is listed, the .tmp staging file is not");
        let r = &refs[0];
        assert_eq!(r.id, "cold-1");
        assert_eq!(r.title.as_deref(), Some("cold storage question"));
        assert_eq!(r.message_count, 2);

        let s = cx.parse(r).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(
            s.messages.iter().filter_map(|m| m.text()).collect::<Vec<_>>(),
            vec!["cold storage question", "cold storage answer"]
        );
        // streaming (lazy spans requested, but a compressed file never spans) matches
        let mut sink = crate::stream::CollectSink::default();
        let mut st = cx.stream(r, &ParseOptions::lazy(), &mut sink).unwrap();
        st.messages = sink.messages;
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::to_value(&st).unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_titles_a_subagent_by_its_own_task_not_the_parents_prompt() {
        let lines = fixture_lines("scan_titles_a_subagent_by_its_own_task_not_the_parents_prompt.jsonl");
        let dir = std::env::temp_dir().join(format!("cv-codex-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-09-19T14-12-43-child-1.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let r = scan(&path).unwrap();
        assert_eq!(r.id, "child-1");
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/ember/dev/minidregg")));
        // (`truncate` folds the newline, as it does for every listing title)
        assert_eq!(
            r.title.as_deref(),
            Some("Message Type: NEW_TASK Task name: build the client")
        );
        assert_eq!(
            r.message_count, 2,
            "the inherited parent prompt and the injected developer message are not counted"
        );

        // Over the full-scan cap the middle is never read: the metadata still comes from the head
        // and tail, and the turn count is 0 ("not counted") rather than an extrapolation
        // (`SessionRef::message_count` is user + assistant turns, never a proxy).
        let sampled = scan_capped(&path, 0).unwrap();
        assert_eq!(sampled.id, "child-1");
        assert_eq!(sampled.cwd, r.cwd);
        assert_eq!(sampled.title, r.title);
        assert_eq!(sampled.updated_at, r.updated_at);
        assert_eq!(
            sampled.message_count, 0,
            "over the cap the count is unknown, not estimated"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Developer smoke against the newest real paginated rollout and the newest subagent rollout
    /// on this machine (run with `--ignored`): parse == stream, and the 0.147+ records actually
    /// surface (item notes, usage from records, swarm messages, no inherited-prefix leakage).
    #[test]
    #[ignore = "requires local ~/.codex corpus"]
    fn real_newest_paginated_and_subagent_rollouts_smoke() {
        let root = dirs::home_dir()
            .unwrap()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09");
        let mut files: Vec<PathBuf> = WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_path_buf())
            .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".jsonl")))
            .collect();
        files.sort_by_key(|p| std::cmp::Reverse(fs::metadata(p).and_then(|m| m.modified()).ok()));
        let newest = files.first().cloned().expect("a September rollout");
        let subagent = files
            .iter()
            .find(|p| {
                fs::read_to_string(p).is_ok_and(|t| {
                    t.lines()
                        .next()
                        .is_some_and(|l| l.contains("subagent_history_start_ordinal"))
                })
            })
            .cloned()
            .expect("a subagent rollout");
        // Snapshot each file first: the newest rollout is usually a LIVE session still being
        // appended to, and parse-vs-stream must see identical bytes.
        let snap = std::env::temp_dir().join(format!("cv-codex-smoke-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&snap).unwrap();
        for src in [newest, subagent] {
            let path = snap.join(src.file_name().unwrap());
            fs::copy(&src, &path).unwrap();
            let cx = Codex::detect();
            let r = scan(&path).unwrap();
            let parsed = cx.parse(&r).unwrap();
            let mut sink = crate::stream::CollectSink::default();
            let mut streamed = cx.stream(&r, &ParseOptions::full(), &mut sink).unwrap();
            streamed.messages = sink.messages;
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&streamed).unwrap(),
                "{}",
                path.display()
            );
            let notes = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "codex_event").and_then(Value::as_str) == Some("item_completed"))
                .count();
            let agent_msgs = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "codex_event").and_then(Value::as_str) == Some("agent_message"))
                .count();
            let with_usage = parsed.messages.iter().filter(|m| m.usage.is_some()).count();
            let from_record = parsed
                .messages
                .iter()
                .filter(|m| bag_get(m, "usage_source").is_some())
                .count();
            let carriers = parsed
                .messages
                .iter()
                .filter(|m| m.content.is_empty() && m.usage.is_some())
                .count();
            let inherit = parsed
                .harness_extra(Harness::Codex)
                .and_then(|b| b.get("subagent_history_start_ordinal"))
                .cloned();
            eprintln!(
                "{}: id={} title={:?} msgs={} scan_count={} item_notes={} agent_msgs={} usage={} (from record {}) usage_carriers={} model={:?} agent_path={:?} inherit_before={:?}",
                path.file_name().unwrap().to_string_lossy(), parsed.id, r.title, parsed.messages.len(), r.message_count, notes, agent_msgs, with_usage, from_record, carriers, parsed.model, parsed.lineage.agent_path, inherit
            );
            assert!(from_record > 0, "paginated files carry token_usage_records");
            // A response whose only item was a tool call is followed by the call's OUTPUT before
            // its usage record, so the trailing-only rule has nothing to attach to and carries the
            // usage bare (as `token_count` always did on these files) — never more than one per
            // record, and never a lost record.
            assert!(carriers <= from_record, "at most one bare carrier per usage record");
            if inherit.is_some() {
                assert!(parsed.messages.iter().all(|m| bag_get(m, "inherited").is_none()));
            }
            // The v2 shape holds on real files: nothing flat, every kind precise.
            for m in &parsed.messages {
                assert!(
                    m.extra
                        .keys()
                        .all(|k| k == "codex" || k == super::super::claude::CARRIER_KEY),
                    "flat extra key on {}: {:?}",
                    path.display(),
                    m.extra.keys().collect::<Vec<_>>()
                );
                if m.role == Role::User && m.origin == Origin::Human {
                    assert_eq!(m.kind, MessageKind::Prompt);
                    assert!(!is_injected_text(&m.text().unwrap_or_default()));
                }
            }
        }
        fs::remove_dir_all(&snap).ok();
    }

    #[test]
    fn structured_output_with_image() {
        let s = parse_fixture("structured_output_with_image.jsonl");
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::ToolResult { content, .. } if content == "here"));
        assert!(
            matches!(&m.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/jpeg"))
        );
    }

    #[test]
    fn user_prompts_vs_injected_preambles_kind_and_origin() {
        // Paginated (no NL event twins): the response_items carry the text.
        let s = parse_fixture("user_prompts_vs_injected_preambles_kind_and_origin.jsonl");
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        let injected = (Role::User, MessageKind::InjectedContext, Origin::Harness);
        assert_eq!(
            shape,
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness), // developer preamble
                injected,                                                      // <environment_context>
                injected,                                                      // <recommended_plugins>
                injected,                                                      // <codex_internal_context …>
                injected,                                                      // AGENTS.md dump
                injected,                                                      // files-mentioned bundle
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[11].extra["codex"]["phase"], "final_answer");
        assert!(
            s.messages.iter().all(|m| m.model.is_none()),
            "no message repeats the session model"
        );

        // Legacy mode (NL event twins): the `user_message` event decides the same way.
        let s = parse_fixture("user_prompts_vs_injected_preambles_kind_and_origin-2.jsonl");
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            shape,
            vec![
                injected,
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.messages[1].extra["codex"]["kind"], "plain");
    }

    #[test]
    fn persisted_error_event_is_an_error_message() {
        let s = parse_fixture("persisted_error_event_is_an_error_message.jsonl");
        let m = &s.messages[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::Error, Origin::Harness)
        );
        assert!(m.text().unwrap().starts_with("You've hit your usage limit."));
        assert_eq!(m.extra["codex"]["codex_event"], "error");
        assert_eq!(m.extra["codex"]["error"]["codex_error_info"], "usage_limit_exceeded");
        assert!(
            m.extra["codex"]["error"].get("type").is_none(),
            "the wire `type` is the kind, not a fact"
        );
        assert!(m.extra["codex"]["error"]["message"].is_string());
    }

    #[test]
    fn session_meta_lineage_and_system_prompt() {
        let s = parse_fixture("session_meta_lineage_and_system_prompt.jsonl");
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(s.lineage.forked_from.as_deref(), Some("fork-src-1"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("/root/reviewer"));
        assert_eq!(
            s.lineage.spawned_by_tool_use, None,
            "Codex does not record the spawning call"
        );
        assert_eq!(s.lineage.continued_in, None);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Codex, a coding agent."));
        // The session bag: every listed meta fact, and nothing first-class duplicated into it.
        let bag = s.harness_extra(Harness::Codex).expect("codex bag");
        let mut keys: Vec<&str> = bag.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "agent_nickname",
                "cli_version",
                "history_base",
                "history_mode",
                "model_provider",
                "originator",
                "session_id",
                "source",
                "subagent_history_start_ordinal",
                "thread_source",
            ]
        );
        assert_eq!(bag["history_base"]["end_ordinal_exclusive"], 9);
        assert_eq!(
            s.extra.keys().collect::<Vec<_>>(),
            vec!["codex"],
            "session extra is nested only"
        );

        // Without those fields: empty lineage, no system prompt (a blank one does not count).
        let s = parse_fixture("session_meta_lineage_and_system_prompt-2.jsonl");
        assert!(s.lineage.is_empty());
        assert_eq!(s.system_prompt, None);
    }

    /// One paginated thread touching every arm: the v2 shape must hold everywhere — every message
    /// fact under `extra["codex"]`, `_record` the only other top-level key (complete mode only),
    /// precise kinds, `Message::model` unset except on the model-change note, `namespace` on the
    /// block and not in the bag.
    fn every_arm() -> Vec<String> {
        fixture_lines("every_arm.jsonl")
    }

    #[test]
    fn turn_aborted_keeps_the_0_161_error_and_0_162_root_turn() {
        let line = r#"{"timestamp":"2026-10-11T19:00:00Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t2","root_turn_id":"t1","reason":"interrupted","error":{"message":"stream closed","codex_error_info":"other"}}}"#;
        let s = parse_jsonl(&[line.to_string()]);
        let b = &s.messages[0].extra["codex"];
        assert_eq!(s.messages[0].kind, MessageKind::Notice);
        assert_eq!(b["error"]["message"], "stream closed");
        assert_eq!(b["root_turn_id"], "t1");
        assert_eq!(b["reason"], "interrupted");
    }

    #[test]
    fn every_arm_has_a_precise_kind_and_nothing_flat() {
        let s = parse_jsonl(&every_arm());
        let shape: Vec<_> = s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model), // reasoning
                (Role::Assistant, MessageKind::Reply, Origin::Model), // spawn call
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::System, MessageKind::SubagentSpawn, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Subagent), // helper → /root
                (Role::Assistant, MessageKind::Reply, Origin::Model), // "fixed" (+usage)
                (Role::System, MessageKind::ModelChange, Origin::Harness),
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness), // turn_aborted
                (Role::System, MessageKind::Branch, Origin::Harness), // thread_rolled_back
                (Role::System, MessageKind::Error, Origin::Harness),
            ],
            "lean: no carriers, every kind precise"
        );
        // Nothing flat, anywhere; no `_record` in a lean pass.
        for m in &s.messages {
            let keys: Vec<&String> = m.extra.keys().collect();
            assert!(
                keys.iter().all(|k| *k == "codex"),
                "flat key(s) on {:?}: {keys:?}",
                m.kind
            );
        }
        assert_eq!(s.extra.keys().collect::<Vec<_>>(), vec!["codex"]);
        assert_eq!(s.system_prompt.as_deref(), Some("You are Codex."));
        assert_eq!(s.model.as_deref(), Some("gpt-6-vega"));
        assert_eq!(s.extra["codex"]["reasoning_effort"], "high");
        assert_eq!(s.extra["codex"]["last_agent_message"], "fixed");
        assert_eq!(s.extra["codex"]["thread_settings"]["model"], "gpt-6-vega");
        // `Message::model` only on the model-change note (the session default covers the rest).
        for m in &s.messages {
            assert_eq!(
                m.model.as_deref(),
                (m.kind == MessageKind::ModelChange).then_some("gpt-6-vega"),
                "{:?}",
                m.kind
            );
        }
        // namespace: on the block, not in the call's bag; the output keeps it in the bag (no
        // block field for it) alongside `tool_name`.
        let call = &s.messages[3];
        assert!(matches!(
            &call.content[0],
            Block::ToolUse { name, namespace: Some(ns), .. } if name == "spawn" && ns == "collaboration"
        ));
        assert!(bag_get(call, "namespace").is_none());
        assert_eq!(s.messages[4].extra["codex"]["namespace"], "collaboration");
        // Usage from the record (with the token_count snapshot merged) on the reply it trails.
        let reply = &s.messages[7];
        let u = reply.usage.as_ref().expect("usage");
        assert_eq!(
            (
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.output_tokens,
                u.reasoning_tokens
            ),
            // 100 on disk includes both cache counts: 100 - 50 - 5 = 45 uncached.
            (Some(45), Some(50), Some(5), Some(20), Some(7))
        );
        assert_eq!(reply.extra["codex"]["phase"], "final_answer");
        assert_eq!(reply.extra["codex"]["usage_source"], "token_usage_record");
        assert!(reply.extra["codex"].get("rate_limits").is_some());
        // The exact per-message bag keys, so a new flat-looking key cannot sneak in unnoticed.
        let keys = |i: usize| -> Vec<String> {
            let mut k: Vec<String> = s.messages[i].extra["codex"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            k.sort_unstable();
            k
        };
        assert_eq!(keys(1), vec!["turn_id"]);
        assert_eq!(
            keys(5),
            vec![
                "agent_path",
                "agent_thread_id",
                "codex_event",
                "completed_at_ms",
                "item",
                "item_type",
                "turn_id"
            ]
        );
        assert_eq!(keys(6), vec!["author", "codex_event", "recipient", "trigger_turn"]);
        assert_eq!(
            keys(7),
            vec![
                "model_context_window",
                "phase",
                "rate_limits",
                "response_id",
                "thread_token_usage",
                "turn_id",
                "usage_source"
            ]
        );
        assert_eq!(keys(8), vec!["codex_event"]);
        assert_eq!(keys(9), vec!["codex_event", "replacement_history_len", "window_number"]);
        assert_eq!(keys(10), vec!["codex_event", "reason", "turn_id"]);
        assert_eq!(keys(11), vec!["codex_event", "num_turns"]);
        assert_eq!(keys(12), vec!["codex_event", "error"]);
        // parse == stream
        let streamed = stream_jsonl_with(&every_arm(), &ParseOptions::full());
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
    }

    #[test]
    fn complete_mode_carries_lifecycle_and_unknown_records_verbatim() {
        let lean = parse_jsonl(&every_arm());
        assert!(lean.messages.iter().all(|m| m.kind != MessageKind::Carrier));
        let complete = stream_jsonl_with(&every_arm(), &ParseOptions::complete());
        let carriers: Vec<(&str, &str)> = complete
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Carrier)
            .map(|m| {
                let record = &m.extra[super::super::claude::CARRIER_KEY];
                (
                    m.extra["codex"]["record_type"].as_str().unwrap(),
                    record
                        .pointer("/payload/type")
                        .and_then(Value::as_str)
                        .unwrap_or(record["type"].as_str().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            carriers,
            vec![
                ("event_msg.task_started", "task_started"),
                ("world_state", "world_state"),
                ("event_msg.task_complete", "task_complete"),
            ],
            "lifecycle markers and unmodelled record types ride verbatim; represented records do not"
        );
        assert_eq!(complete.messages.len(), lean.messages.len() + carriers.len());
        for m in &complete.messages {
            let allowed = |k: &String| k == "codex" || k == super::super::claude::CARRIER_KEY;
            assert!(m.extra.keys().all(allowed), "{:?}", m.extra.keys().collect::<Vec<_>>());
            assert_eq!(
                m.extra.contains_key(super::super::claude::CARRIER_KEY),
                m.kind == MessageKind::Carrier,
                "`_record` marks exactly the carriers"
            );
        }
        let ws = complete
            .messages
            .iter()
            .find(|m| bag_get(m, "record_type").and_then(Value::as_str) == Some("world_state"))
            .unwrap();
        assert_eq!((ws.role, ws.origin), (Role::System, Origin::Harness));
        assert_eq!(
            ws.extra[super::super::claude::CARRIER_KEY]["payload"]["agents_md"],
            "# AGENTS.md"
        );
        assert!(ws.content.is_empty());
    }
}
