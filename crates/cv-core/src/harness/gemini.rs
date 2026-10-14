//! Gemini / Antigravity adapter — best-effort, multi-format.
//!
//! `~/.gemini/` holds several distinct on-disk shapes; we read every readable one:
//!
//! 1. **Chat recordings** (richest) — `~/.gemini/tmp/<projectHash>/chats/session-<ts>-<id>.json`
//!    (legacy, a single whole-file `ConversationRecord`) or `.jsonl` (modern, append-only: a metadata
//!    first line, then per-message records, `{"$set":…}` metadata patches, and `{"$rewindTo":id}`
//!    truncation markers). These carry real assistant turns, thinking, tool calls *and* tool results
//!    plus per-message token usage and model. Subagent recordings live under
//!    `chats/<parentId>/<id>.jsonl`. (See gemini-cli `core/src/services/chatRecordingService.ts`.)
//! 2. **gemini-cli checkpoints** — `~/.gemini/tmp/<hash>/checkpoint-<tag>.json`, either
//!    `{ history: Content[], authType? }` or a bare legacy `Content[]` array, where each `Content` is
//!    `{ role: "user"|"model", parts: Part[] }`. (See gemini-cli `core/src/core/logger.ts`.) These are
//!    transient (deleted on resume) so they may not be present, but we parse them when they are.
//! 3. **Readable fallback** — `~/.gemini/tmp/<hash>/logs.json`, an array of
//!    `{sessionId, messageId, type, message, timestamp}` (user prompts reliably; weak on assistant).
//!    One `logs.json` may interleave several `sessionId`s; each becomes its own IR session.
//!
//! **Roots.** gemini-cli's runtime state normally lives under `~/.gemini/`, but under the macOS
//! Seatbelt sandbox (`SANDBOX=sandbox-exec`) the profile blocks writes there, so
//! `Storage.getGlobalRuntimeDir()` routes everything to `~/.cache/.gemini/` instead
//! (`packages/core/src/config/storage.ts:90-107`, `getGlobalTempDir` `:195-197`). Both `tmp` roots
//! are scanned; a session is keyed by its id, so nothing is listed twice.
//!
//! **cwd.** A recording lives at `<runtime>/tmp/<projectIdentifier>/chats/…`, where the identifier is
//! either the legacy sha256 of the project path or (current) a short id such as `claurdvoyant`. The
//! path itself is recorded in `<runtime>/projects.json` (`{"projects": {"/abs/path": "short-id"}}`,
//! pretty-printed) and in a bare-path `.project_root` marker the registry writes into EVERY base dir
//! it manages — `<runtime>/tmp/<short-id>/.project_root` (right next to `chats/`) and
//! `<runtime>/history/<short-id>/.project_root` (`config/projectRegistry.ts` `baseDirs` +
//! `PROJECT_ROOT_FILE`, `storage.ts:283-296`). Verified on 0.46.0: a fresh project dir got all three
//! at once, the short id being the dir's basename (`proj`, `proj-1` on collision). The record's own
//! `directories[]` wins when present; otherwise the cwd is recovered from the registry — before that,
//! almost every Gemini session had `cwd: None`.
//!
//! The closed Antigravity IDE's `~/.gemini/antigravity/conversations/*.pb` are opaque (compressed /
//! no on-disk schema, no readable strings) and are *not* parsed.
//!
//! `parse_all_str` (used by the wasm ingest path) handles all three readable JSON/JSONL shapes purely,
//! with no filesystem access.

mod emit;
mod input;
mod lean;
#[cfg(not(feature = "mmap"))]
mod replay;
pub use emit::emit;

use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Gemini {
    /// Every `tmp` root that exists: `~/.gemini/tmp`, then the sandbox root `~/.cache/.gemini/tmp`.
    roots: Vec<PathBuf>,
}

impl Gemini {
    pub fn detect() -> Self {
        let roots = dirs::home_dir()
            .map(|h| {
                vec![
                    h.join(".gemini").join("tmp"),
                    // `SANDBOX=sandbox-exec` runs (macOS Seatbelt) write here instead — storage.ts:90-107.
                    h.join(".cache").join(".gemini").join("tmp"),
                ]
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        Gemini { roots }
    }

    /// Rooted at one runtime dir (a `.gemini` or `.cache/.gemini`): reads `<dir>/tmp`, and the
    /// registry beside it. For tests and the corpus; never consults the process environment.
    pub fn with_home(runtime: &Path) -> Self {
        Gemini {
            roots: vec![runtime.join("tmp")],
        }
    }
}

impl Adapter for Gemini {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        if self.roots.is_empty() {
            return Ok(vec![]);
        }
        // Collect candidate file paths first (cheap walk over every root), then read+parse in
        // parallel. A single `logs.json` can expand into many sessions, so this is a flat-map.
        let paths: Vec<_> = self
            .roots
            .iter()
            .flat_map(|root| WalkDir::new(root).into_iter().filter_map(|e| e.ok()))
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect();
        let refs = crate::par_discovery_flat_map(paths, |path| {
            crate::discover_cache::cached_scan_many(&path, || scan_session_file(&path, Harness::Gemini))
        });
        Ok(dedupe_by_id(refs))
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| stream_for(Harness::Gemini, r, opts, sink))
    }

    /// The per-session side files under `tmp/<project>/<session id>/`: the task tracker
    /// (`tracker/<id>.json`, `tracker_*` tools) and the plan-mode plans (`plans/*.md`).
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        side_affordances(&r.path, &r.id)
    }
}

/// Keep one [`SessionRef`] per session id across files and roots (a sandboxed and an unsandboxed run
/// of the same project could both hold a copy, and a resumed 0.46.0 process leaves a stub): a file
/// with turns beats one without, then the most recently updated wins; input order is otherwise
/// preserved.
fn dedupe_by_id(refs: Vec<SessionRef>) -> Vec<SessionRef> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<SessionRef> = Vec::with_capacity(refs.len());
    for r in refs {
        match seen.get(&r.id) {
            Some(&i) => {
                // A file with turns beats a startup stub: a resumed 0.46.0 process writes a second
                // `session-<ts>-<id>.jsonl` for the same id holding only the `<session_context>`
                // preamble (later gemini-cli deletes it, `deleteCurrentSessionIfNotResumableAsync`),
                // and it is the NEWER file. Between equals the most recently updated wins.
                let key = |r: &SessionRef| (r.message_count > 0, r.updated_at);
                if key(&r) > key(&out[i]) {
                    out[i] = r;
                }
            }
            None => {
                seen.insert(r.id.clone(), out.len());
                out.push(r);
            }
        }
    }
    out
}

/// The project cwd a Gemini-format file belongs to, from gemini-cli's project registry.
///
/// A recording/checkpoint/log lives at `<runtime>/tmp/<projectIdentifier>/…`. The identifier is a
/// short id (`claurdvoyant`) or, for pre-registry installs, the sha256 of the project path. The
/// registry `<runtime>/projects.json` maps `{"/abs/path": "short-id"}`
/// (`config/projectRegistry.ts`), and `<runtime>/history/<id>/.project_root` holds the path too
/// (the ownership marker gemini-cli uses to self-heal a lost registry). Hash-named dirs have no
/// marker, so they stay `None` — exactly as before. Registry reads are cached per runtime root.
/// gemini-cli's own rule for user content that is NOT a prompt (`utils/sessionUtils.ts:97-105`
/// `isIgnoredUserContent`): empty, a slash command, a `?` query, or the injected `<session_context>`
/// / `<hook_context>` preamble the CLI writes as the first "user" message of every session. Such a
/// turn is kept in the transcript (it is in the record) but must not become the session title —
/// verified on 0.46.0, where a fresh session's only user turn was the `<session_context>` block.
fn is_ignored_user_content(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with('/')
        || t.starts_with('?')
        || t.starts_with("<session_context>")
        || t.starts_with("<hook_context>")
}

/// Type a `user`-typed turn by its text: a prompt the human typed stays a [`Role::User`]
/// [`MessageKind::Prompt`]; context gemini-cli itself fed the model and skips when rebuilding
/// history (`<session_context>` preambles, `<hook_context>` hook output, `/`-command and `?` echoes —
/// `isIgnoredUserContent`, `utils/sessionUtils.ts:97`) is a [`Role::System`]
/// [`MessageKind::InjectedContext`] turn (origin [`Origin::Hook`] for hook output), the same shape
/// the Claude adapter gives its attachments, so titles and first-prompt previews never see it.
fn type_user_turn(m: &mut Message) {
    let Some(text) = m.text() else { return };
    type_user_turn_from(m, &text);
}

fn type_user_turn_from(m: &mut Message, text: &str) {
    let t = text.trim_start();
    let (kind, origin) = if t.starts_with("<hook_context>") {
        (MessageKind::InjectedContext, Origin::Hook)
    } else if !t.is_empty() && is_ignored_user_content(t) {
        (MessageKind::InjectedContext, Origin::Harness)
    } else {
        (MessageKind::Prompt, Origin::Human)
    };
    if kind == MessageKind::InjectedContext {
        m.role = Role::System;
    }
    m.kind = kind;
    m.origin = origin;
}

/// The parent session of a sub-agent recording, from its path: gemini-cli writes sub-agent
/// recordings under `chats/<parentId>/<id>.jsonl` (`chatRecordingService.ts`), a level below the
/// parent's own `chats/session-*.jsonl`.
fn parent_session_from_path(path: Option<&Path>) -> Option<String> {
    let dir = path?.parent()?;
    (dir.parent()?.file_name()? == "chats")
        .then(|| dir.file_name()?.to_str().map(str::to_string))
        .flatten()
}

fn project_cwd(path: &Path) -> Option<PathBuf> {
    let comps: Vec<&std::ffi::OsStr> = path.components().map(|c| c.as_os_str()).collect();
    // The INNERMOST `tmp`, not the first: the layout is `<runtime>/tmp/<id>/chats/<file>`, and a
    // runtime root can itself sit under a directory called `tmp` — `/tmp/...` on Linux, or a
    // Seatbelt/CI root. Scanning left-to-right picked that outer one, made `<runtime>` the
    // filesystem root and silently lost every cwd.
    let tmp_at = comps.iter().rposition(|c| *c == "tmp")?;
    let identifier = comps.get(tmp_at + 1)?.to_str()?;
    // `<runtime>` is the parent of `tmp`: rebuild it from the leading components.
    let runtime: PathBuf = comps[..tmp_at].iter().collect();
    if runtime.as_os_str().is_empty() {
        return None;
    }
    registry_for(&runtime).get(identifier).cloned()
}

/// `short-id → project path`, one map per runtime root.
type Registry = std::sync::Arc<std::collections::HashMap<String, PathBuf>>;

/// The [`Registry`] for one runtime root, read once per process.
fn registry_for(runtime: &Path) -> Registry {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Registry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(runtime).cloned()) {
        return hit;
    }
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    // The `.project_root` markers first (`tmp/<id>/` — the dir the chats live in — then
    // `history/<id>/`), so a (newer) registry entry overrides them below. The marker is the bare
    // absolute path, no trailing newline (0.46.0), but trim defensively.
    for base in ["tmp", "history"] {
        if let Ok(entries) = fs::read_dir(runtime.join(base)) {
            for e in entries.flatten() {
                if let Ok(root) = fs::read_to_string(e.path().join(".project_root")) {
                    let root = root.trim();
                    if !root.is_empty() {
                        map.insert(e.file_name().to_string_lossy().into_owned(), PathBuf::from(root));
                    }
                }
            }
        }
    }
    if let Ok(text) = fs::read_to_string(runtime.join("projects.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(projects) = v.get("projects").and_then(Value::as_object) {
                for (abs_path, id) in projects {
                    if let Some(id) = id.as_str() {
                        map.insert(id.to_string(), PathBuf::from(abs_path));
                    }
                }
            }
        }
    }
    let arc = Arc::new(map);
    if let Ok(mut c) = cache.lock() {
        c.insert(runtime.to_path_buf(), arc.clone());
    }
    arc
}

/// Discovery scan of one Gemini-format file into 0..n [`SessionRef`]s, tagged as `harness`.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter (identical on-disk format). Notably,
/// checkpoints derive their id from the `checkpoint-<tag>` *filename* here — the same id
/// [`stream_for`]'s bounded path produces on parse — so discover/parse/stream all agree.
pub(crate) fn scan_session_file(path: &Path, harness: Harness) -> Vec<SessionRef> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == "logs.json" {
        if let Ok(text) = fs::read_to_string(path) {
            return parse_logs_str(&text, Some(path.to_path_buf()))
                .iter()
                .map(|s| session_ref(s, path, harness))
                .collect();
        }
    } else if is_chat_recording(path) {
        let mut turns = 0usize;
        let mut sink = |m: Message| {
            turns += usize::from(matches!(m.role, Role::User | Role::Assistant));
            Flow::Continue
        };
        let r = SessionRef::new("", harness, path.to_path_buf());
        if let Ok(s) = stream_for(harness, &r, &ParseOptions::lazy(), &mut sink) {
            let mut r = session_ref(&s, path, harness);
            r.message_count = turns;
            return vec![r];
        }
    } else if is_checkpoint(&name) {
        if let Ok(text) = fs::read_to_string(path) {
            if let Some(s) = parse_checkpoint(&text, &name, Some(path.to_path_buf())) {
                return vec![session_ref(&s, path, harness)];
            }
        }
    }
    Vec::new()
}

/// Streaming parse of any Gemini-format session file, tagged as `harness` in the returned session.
/// Shared with the [`Qwen`](crate::harness::qwen) adapter — Qwen Code is a gemini-cli fork with an
/// **identical** on-disk format rooted at `~/.qwen`, so it reuses this machinery and only re-tags.
pub(crate) fn stream_for(
    harness: Harness,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    // Lazy reads borrow large text from recordings; full reads materialize one record at a time.
    // The small harness facts remain identical in both modes.
    let name = r
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Chat recording (legacy whole-file .json / modern append-only .jsonl): one session per
    // file, id authoritative. These are the only shapes large enough to OOM, so they get
    // the native streaming paths; the bounded shapes below stay on the materializing bridge.
    if is_chat_recording(&r.path) {
        let ext = r.path.extension().and_then(|e| e.to_str());
        if ext == Some("jsonl") {
            if let Some(mut s) = stream_jsonl_recording(&r.path, harness, opts, sink)? {
                s.harness = harness;
                return Ok(s);
            }
        } else {
            // Legacy `.json`: a single JSON document. Memory-map it and iterate the `messages`
            // array as borrowed `RawValue`s so the whole-document `Value` is never built — peak
            // is one message + the (reclaimable) mapping. If the file isn't actually the
            // single-document shape (e.g. a `.json`-extensioned JSONL recording), fall through.
            if let Some(mut s) = stream_legacy_json_recording(&r.path, harness, opts, sink)? {
                s.harness = harness;
                return Ok(s);
            }
            if let Some(mut s) = stream_jsonl_recording(&r.path, harness, opts, sink)? {
                s.harness = harness;
                return Ok(s);
            }
        }
    }

    // Bounded shapes (checkpoints, logs.json): a few KB–MB, never the OOM risk. Bridge through a
    // whole-Session parse and replay its messages into the sink.
    let mut session = parse_bounded(r, &name, harness)?;
    session.harness = harness;
    let messages = std::mem::take(&mut session.messages);
    sink.meta(&session);
    for m in messages {
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    Ok(session)
}

/// Whole-`Session` parse for the bounded (non-streamed) shapes: gemini-cli checkpoints and
/// `logs.json` (many sessions per file — picks the one matching `r.id`). These are small enough
/// to materialize. Also handles the rare case where a chat-recording path didn't stream.
fn parse_bounded(r: &SessionRef, name: &str, harness: Harness) -> Result<Session> {
    let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;

    // A chat recording that the streaming paths couldn't handle (shouldn't normally happen) —
    // fall back to the pure parser so we never silently lose a session.
    if is_chat_recording(&r.path) {
        if let Some(s) = parse_chat_recording(&text, harness, Some(r.path.clone())) {
            return Ok(s);
        }
    }
    // Checkpoint: one session per file.
    if is_checkpoint(name) {
        if let Some(s) = parse_checkpoint(&text, name, Some(r.path.clone())) {
            return Ok(s);
        }
    }
    // logs.json: many sessions per file — pick out the one matching r.id.
    let sessions = parse_logs_str(&text, Some(r.path.clone()));
    if let Some(s) = sessions.into_iter().find(|s| s.id == r.id) {
        return Ok(s);
    }

    anyhow::bail!(
        "could not parse Gemini-format session {} from {}",
        r.id,
        r.path.display()
    )
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// A chat-recording file: `chats/session-*.json` or `chats/session-*.jsonl`, or a subagent recording
/// `chats/<parentId>/<id>.jsonl`. We key off the `chats/` directory in the path.
fn is_chat_recording(path: &Path) -> bool {
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    if !in_chats {
        return false;
    }
    matches!(path.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl"))
}

fn is_checkpoint(name: &str) -> bool {
    name.starts_with("checkpoint-") && name.ends_with(".json")
}

// ---------------------------------------------------------------------------
// Streaming chat recordings — the only Gemini shapes large enough to OOM.
// ---------------------------------------------------------------------------

/// Stream a *legacy whole-file* chat recording (`{sessionId, messages:[…]}` as one JSON document)
/// into `sink` without ever building the whole-document `Value`.
///
/// The file is memory-mapped (behind the `mmap` feature; a `fs::read` fallback otherwise) and a thin
/// wrapper is deserialized that keeps each element of `messages` as a borrowed
/// [`RawValue`](serde_json::value::RawValue). We then parse one raw item at a time into a [`Value`],
/// run it through [`emit_record_message`], and drop it before the next — so peak memory is one
/// message plus the (reclaimable) mapping, not the multi-GB document.
///
/// Returns `Ok(None)` if the bytes aren't the single-document shape (e.g. a `.json`-named JSONL log),
/// so the caller can fall back to the JSONL path.
fn stream_legacy_json_recording(
    path: &Path,
    harness: Harness,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    #[cfg(not(feature = "mmap"))]
    if opts.spans {
        return stream_legacy_views(path, harness, sink);
    }
    use serde::Deserialize;
    use serde_json::value::RawValue;

    /// Borrows just the metadata fields [`record_metadata`] reads, plus the message array as raw
    /// (unparsed) items. `#[serde(borrow)]` keeps everything pointing into the mapping.
    #[derive(Deserialize)]
    struct Wrapper<'a> {
        #[serde(rename = "sessionId", borrow, default)]
        session_id: Option<std::borrow::Cow<'a, str>>,
        #[serde(rename = "startTime", borrow, default)]
        start_time: Option<std::borrow::Cow<'a, str>>,
        #[serde(rename = "lastUpdated", borrow, default)]
        last_updated: Option<std::borrow::Cow<'a, str>>,
        #[serde(borrow, default)]
        summary: Option<std::borrow::Cow<'a, str>>,
        #[serde(borrow, default)]
        directories: Option<Vec<std::borrow::Cow<'a, str>>>,
        #[serde(borrow, default)]
        messages: Option<Vec<&'a RawValue>>,
        #[serde(borrow, default)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }

    let source = opts.spans.then(|| crate::json::JsonSource::capture(path)).transpose()?;
    let arena = source
        .as_ref()
        .map(|_| super::store::bytes::ByteSource::arena().map(std::sync::Arc::new))
        .transpose()?;
    let bytes = map_file(path)?;
    if let Some(source) = &source {
        source.check_metadata(bytes.file.metadata()?)?;
    }
    // A legacy recording is a single JSON object with both `sessionId` and `messages`. If this parse
    // fails, or those keys are absent, it's not the whole-file shape — let the caller try JSONL.
    let Ok(w) = serde_json::from_slice::<Wrapper>(&bytes) else {
        return Ok(None);
    };
    let (Some(_sid), Some(raw_msgs)) = (&w.session_id, &w.messages) else {
        return Ok(None);
    };

    // Rebuild a tiny metadata map (only the fields record_metadata consults) — never the document.
    let mut meta = serde_json::Map::new();
    if let Some(v) = w.session_id {
        meta.insert("sessionId".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.start_time {
        meta.insert("startTime".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.last_updated {
        meta.insert("lastUpdated".into(), Value::String(v.to_string()));
    }
    if let Some(v) = w.summary {
        meta.insert("summary".into(), Value::String(v.to_string()));
    }
    if let Some(dirs) = &w.directories {
        meta.insert(
            "directories".into(),
            Value::Array(dirs.iter().map(|d| Value::String(d.to_string())).collect()),
        );
    }

    if let Some(kind) = w.kind {
        meta.insert("kind".into(), Value::String(kind.into_owned()));
    }

    let mut session = record_metadata(&meta, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut st = EmitState::default();

    let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
    let index = lean::Index::default();
    let mut sink = input::SourceSink::new(source.as_ref(), sink);
    for item in raw_msgs {
        if opts.spans {
            let raw = lean::Raw::new(item.get(), &index);
            if !lean::accepts_value(raw) {
                continue;
            }
            if lean::emit(
                raw,
                lean::Context {
                    bytes: &bytes,
                    base: 0,
                    source: source.as_ref(),
                    arena: arena.as_ref(),
                },
                harness,
                &mut st,
                &resolver,
                &mut sink,
            )? == Flow::Stop
            {
                break;
            }
            continue;
        }
        // Parse just this one message; it (and its owned strings) drop at the end of the iteration.
        let Ok(rm) = serde_json::from_str::<Value>(item.get()) else {
            continue;
        };
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut st, &mut sink) == Flow::Stop {
            break;
        }
    }
    if sink.finish()? {
        let source = source.as_ref().expect("sourced payload has a capture");
        source.check_metadata(bytes.file.metadata()?)?;
        source.check()?;
    }

    session.model = st.session_model;
    session.title = summary.or(st.title);
    Ok(Some(session))
}

/// Preserve the legacy Wrapper's typed/unique known fields and independent message acceptance,
/// while portable views keep large payloads in the positioned source rather than encoded buffers.
#[cfg(not(feature = "mmap"))]
fn stream_legacy_views(path: &Path, harness: Harness, sink: &mut dyn MessageSink) -> Result<Option<Session>> {
    const KEYS: &[&str] = &[
        "sessionId",
        "startTime",
        "lastUpdated",
        "summary",
        "directories",
        "messages",
        "kind",
    ];
    let captured = crate::json::JsonSource::capture(path)?;
    let arena = std::sync::Arc::new(super::store::bytes::ByteSource::arena()?);
    if captured.len() == 0 {
        return Ok(None);
    }
    let result = captured.read_syntax(|view| {
        // serde's struct decoder rejects duplicate known fields, even when their values agree.
        let Some(fields) = super::store::document::Fields::parse_struct(view, KEYS)? else {
            return Ok(None);
        };
        let mut meta = serde_json::Map::new();
        for key in KEYS {
            let value = fields.get(key);
            if matches!(*key, "directories" | "messages") {
                continue;
            }
            let Some(value) = value else { continue };
            if value.is_null()? {
                continue;
            }
            if !value.is_string()? || !value.accepts_value()? {
                return Ok(None);
            }
            meta.insert((*key).into(), value.resolve()?.into_owned());
        }
        if !meta.contains_key("sessionId") {
            return Ok(None);
        }
        if let Some(dirs) = fields.get("directories") {
            if !dirs.is_null()? {
                let Some(dirs) = dirs.array()? else { return Ok(None) };
                let mut directories = Vec::new();
                for dir in dirs {
                    let dir = dir?;
                    if !dir.is_string()? || !dir.accepts_value()? {
                        return Ok(None);
                    }
                    directories.push(dir.resolve()?.into_owned());
                }
                meta.insert("directories".into(), Value::Array(directories));
            }
        }
        let Some(messages) = fields
            .get("messages")
            .map(crate::json::JsonView::accepted_array)
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        let mut session = record_metadata(&meta, harness, Some(path.to_path_buf()));
        let summary = session.title.take();
        let mut st = EmitState::default();
        let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
        let mut sink = input::SourceSink::new(Some(&captured), sink);
        for message in messages {
            let Some(message) = message? else { continue };
            if lean::emit_view(
                message,
                lean::output::Output {
                    source: &captured,
                    arena: &arena,
                },
                harness,
                &mut st,
                &resolver,
                &mut sink,
            )? == Flow::Stop
            {
                break;
            }
        }
        sink.finish()?;
        // read_syntax checks the same opened descriptor and named identity after this callback.
        session.model = st.session_model;
        session.title = summary.or(st.title);
        Ok(Some(session))
    });
    match result {
        Err(error) if error.is::<crate::json::InvalidJson>() || error.is::<super::store::document::InvalidStruct>() => {
            Ok(None)
        }
        result => result,
    }
}

/// Memory-map `path` (feature `mmap`) or read it into a `Vec<u8>` (fallback). Returns an opaque
/// byte container that derefs to `[u8]` either way, so callers don't branch on the feature.
fn map_file(path: &Path) -> Result<FileBytes> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    #[cfg(feature = "mmap")]
    {
        // Safety: cv reads its own on-disk transcripts; we treat the mapping as immutable bytes and
        // never alias it mutably. A concurrent external truncation is the documented mmap caveat and
        // not a case cv creates.
        let mmap = unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmapping {}", path.display()))?;
        Ok(FileBytes {
            file,
            bytes: FileContents::Mapped(mmap),
        })
    }
    #[cfg(not(feature = "mmap"))]
    {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut &file, &mut bytes).with_context(|| format!("reading {}", path.display()))?;
        Ok(FileBytes {
            file,
            bytes: FileContents::Owned(bytes),
        })
    }
}

/// Byte container abstracting over an mmap (feature `mmap`) and an owned read (fallback).
struct FileBytes {
    file: fs::File,
    bytes: FileContents,
}
enum FileContents {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    #[cfg(not(feature = "mmap"))]
    Owned(Vec<u8>),
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.bytes {
            #[cfg(feature = "mmap")]
            FileContents::Mapped(m) => m,
            #[cfg(not(feature = "mmap"))]
            FileContents::Owned(v) => v,
        }
    }
}

/// Replay a modern JSONL recording's navigation into borrowed record descriptors, then emit
/// the effective messages. A later record can replace/truncate earlier output, so selection
/// must reach EOF; only keys and source ranges are retained, never historical payload Values.
///
/// Returns `Ok(None)` if the file is not a recording (no metadata line and no message records), so
/// the caller can fall back.
#[cfg(feature = "mmap")]
fn stream_jsonl_recording(
    path: &Path,
    harness: Harness,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    let source = opts.spans.then(|| crate::json::JsonSource::capture(path)).transpose()?;
    let arena = source
        .as_ref()
        .map(|_| super::store::bytes::ByteSource::arena().map(std::sync::Arc::new))
        .transpose()?;
    let bytes = map_file(path)?;
    if let Some(source) = &source {
        source.check_metadata(bytes.file.metadata()?)?;
    }
    let index = lean::Index::default();
    let mut replay = RecordingReplay::<ReplayBody<'_>>::default();
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(raw) = serde_json::from_slice::<&crate::lazy::RawValue>(line) else {
            continue;
        };
        let raw = lean::Raw::new(raw.get(), &index);
        if lean::accepts_value(raw) {
            replay.ingest_projected(raw, ReplayBody::Raw, ReplayBody::Marker);
        }
    }
    if !replay.saw_meta && replay.msgs.is_empty() {
        return Ok(None);
    }
    let mut session = record_metadata(&replay.metadata, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut st = EmitState::default();
    let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
    let mut sink = input::SourceSink::new(source.as_ref(), sink);
    for id in &replay.order {
        let Some(body) = replay.msgs.remove(id) else { continue };
        let flow = match body {
            ReplayBody::Raw(raw) if opts.spans => lean::emit(
                raw,
                lean::Context {
                    bytes: &bytes,
                    base: 0,
                    source: source.as_ref(),
                    arena: arena.as_ref(),
                },
                harness,
                &mut st,
                &resolver,
                &mut sink,
            )?,
            body => {
                let rm = match body {
                    ReplayBody::Raw(raw) => serde_json::from_str::<Value>(raw.get()).unwrap_or(Value::Null),
                    ReplayBody::Marker(v) => v,
                };
                let Some(obj) = rm.as_object() else { continue };
                emit_record_message(harness, obj, &mut st, &mut sink)
            }
        };
        if flow == Flow::Stop {
            break;
        }
    }
    if sink.finish()? {
        let source = source.as_ref().expect("sourced payload has a capture");
        source.check_metadata(bytes.file.metadata()?)?;
        source.check()?;
    }
    session.model = st.session_model;
    session.title = summary.or(st.title);
    Ok(Some(session))
}

// Keep the original buffered replay when mapping is unavailable: a long historical file can
// rewind to a tiny active history, so reading the entire source would increase its heap cost.
#[cfg(not(feature = "mmap"))]
fn stream_jsonl_recording(
    path: &Path,
    harness: Harness,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    if opts.spans {
        return stream_jsonl_ranges(path, harness, sink);
    }
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut replay = RecordingReplay::default();
    super::for_each_json_line(std::io::BufReader::new(file), |rec| {
        replay.ingest(rec);
        Flow::Continue
    });
    if !replay.saw_meta && replay.msgs.is_empty() {
        return Ok(None);
    }

    let mut session = record_metadata(&replay.metadata, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut st = EmitState::default();

    // Emit retained messages in order, dropping each raw Value as we go (so we never hold the
    // reassembled array and the IR messages at the same time).
    for id in &replay.order {
        let Some(rm) = replay.msgs.remove(id) else { continue };
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut st, sink) == Flow::Stop {
            break;
        }
    }

    session.model = st.session_model;
    session.title = summary.or(st.title);
    Ok(Some(session))
}

/// Portable lazy replay owns navigation ranges and one encoded record at a time. The positioned
/// cursor is the same captured source used by JSON consumers, not a second source authority.
#[cfg(not(feature = "mmap"))]
fn stream_jsonl_ranges(path: &Path, harness: Harness, sink: &mut dyn MessageSink) -> Result<Option<Session>> {
    use super::store::document::range::{Range, RangeSource};
    use std::io::BufRead;
    let captured = crate::json::JsonSource::capture(path)?;
    let arena = std::sync::Arc::new(super::store::bytes::ByteSource::arena()?);
    let source = RangeSource::from_captured(captured.clone())?;
    let mut cursor = source.cursor(source.range())?;
    let mut recording = RecordingReplay::<replay::RangeBody>::default();
    loop {
        let offset = cursor.position();
        loop {
            let bytes = cursor.fill_buf()?;
            if bytes.is_empty() {
                break;
            }
            let newline = memchr::memchr(b'\n', bytes);
            let n = newline.map_or(bytes.len(), |i| i + 1);
            cursor.consume(n);
            if newline.is_some() {
                break;
            }
        }
        let len = cursor.position() - offset;
        if len == 0 {
            break;
        }
        let Some(range) = replay::trim(&source, Range { offset, len })? else {
            continue;
        };
        if source.validate(range)? {
            replay::ingest(&mut recording, &source, range)?;
        }
    }
    if !recording.saw_meta && recording.msgs.is_empty() {
        return Ok(None);
    }
    let mut session = record_metadata(&recording.metadata, harness, Some(path.to_path_buf()));
    let summary = session.title.take();
    let mut st = EmitState::default();
    drop(cursor);
    let resolver = crate::lazy::Resolver::new(Some(path.to_path_buf()));
    let mut sink = input::SourceSink::new(Some(&captured), sink);
    for id in &recording.order {
        let Some(body) = recording.msgs.remove(id) else {
            continue;
        };
        let flow = match body {
            replay::RangeBody::Range(range) => {
                captured.validated_range(range.offset, range.len)?.with_view(|view| {
                    lean::emit_view(
                        view,
                        lean::output::Output {
                            source: &captured,
                            arena: &arena,
                        },
                        harness,
                        &mut st,
                        &resolver,
                        &mut sink,
                    )
                })?
            }
            replay::RangeBody::Marker(value) => {
                let Some(obj) = value.as_object() else { continue };
                emit_record_message(harness, obj, &mut st, &mut sink)
            }
        };
        if flow == Flow::Stop {
            break;
        }
    }
    sink.finish()?;
    source.check_identity()?;
    session.model = st.session_model;
    session.title = summary.or(st.title);
    Ok(Some(session))
}

/// Effective messages point into the mapped source. Navigation retains small keys/ranges,
/// never the content of an inactive or superseded message.
#[cfg(feature = "mmap")]
enum ReplayBody<'a> {
    Raw(lean::Raw<'a>),
    Marker(Value),
}

/// Replays a *modern* append-only JSONL recording's records into its effective state: the metadata
/// map plus the retained raw message records (insertion-ordered, with `$set` replacement and
/// `$rewindTo` truncation applied). Shared by the streaming [`stream_jsonl_recording`] and the pure
/// [`parse_chat_recording`] so both apply identical control-record semantics.
struct RecordingReplay<T = Value> {
    metadata: serde_json::Map<String, Value>,
    /// Insertion order of message keys (replacement keeps the original position). A key is the
    /// record's `type` and `id` ([`replay_key`]): a resume's `$set.messages` (re-synced from the API
    /// history) can give the tool-response `user` turn the SAME id as the `gemini` turn that made the
    /// call (seen on 0.63.0), and keying by id alone let it overwrite the assistant turn.
    order: Vec<String>,
    msgs: BTreeMap<String, T>,
    /// Whether a real metadata line (sessionId + projectHash) was seen.
    saw_meta: bool,
    /// How many `$rewindTo` records were replayed (names the synthetic Branch markers).
    rewinds: usize,
}

/// A message record's replay key, `<type>\u{1f}<id>` (see [`RecordingReplay::order`]).
fn replay_key(rec: &Value) -> Option<String> {
    let id = rec.get("id").and_then(Value::as_str)?;
    let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
    Some(format!("{ty}\u{1f}{id}"))
}

/// The id part of a [`replay_key`].
fn key_id(key: &str) -> &str {
    key.split_once('\u{1f}').map_or(key, |(_, id)| id)
}

impl<T> Default for RecordingReplay<T> {
    fn default() -> Self {
        Self {
            metadata: Default::default(),
            order: Vec::new(),
            msgs: BTreeMap::new(),
            saw_meta: false,
            rewinds: 0,
        }
    }
}

impl<T> RecordingReplay<T> {
    fn insert(&mut self, key: String, body: T) {
        if !self.msgs.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.msgs.insert(key, body);
    }

    fn rewind(&mut self, target: &str, make_marker: impl FnOnce(Value) -> T) {
        if let Some(pos) = self.order.iter().position(|k| key_id(k) == target) {
            for id in self.order.drain(pos..) {
                self.msgs.remove(&id);
            }
        } else {
            self.order.clear();
            self.msgs.clear();
        }
        self.rewinds += 1;
        let marker_id = format!("$rewind-{}", self.rewinds);
        let marker = serde_json::json!({ "id": marker_id, "type": "rewind", "rewindTo": target });
        self.insert(marker_id, make_marker(marker));
    }
}

#[cfg(feature = "mmap")]
impl<T> RecordingReplay<T> {
    fn ingest_projected<'a>(
        &mut self,
        raw: lean::Raw<'a>,
        mut body: impl FnMut(lean::Raw<'a>) -> T,
        marker: impl FnOnce(Value) -> T,
    ) {
        let Some(rec) = lean::Record::parse(raw) else {
            return;
        };
        if let Some(target) = rec.string("$rewindTo") {
            self.rewind(&target, marker);
            return;
        }
        if let Some(set) = rec.fields.get("$set") {
            if let Some(set) = lean::Record::parse(*set) {
                for (k, v) in set.fields {
                    if k == "messages" {
                        self.order.clear();
                        self.msgs.clear();
                        if let Some(messages) = lean::array(v) {
                            for raw in messages {
                                if let Some(m) = lean::Record::parse(raw) {
                                    if let Some(key) = raw_replay_key(&m) {
                                        self.insert(key, body(raw));
                                    }
                                }
                            }
                        }
                    } else if RECORD_METADATA.contains(&k.as_str()) {
                        if let Ok(v) = serde_json::from_str(v.get()) {
                            self.metadata.insert(k, v);
                        }
                    }
                }
                return;
            }
        }
        if let Some(key) = raw_replay_key(&rec) {
            if rec.fields.contains_key("type") || rec.fields.contains_key("content") {
                self.insert(key, body(raw));
            }
            return;
        }
        if rec.fields.contains_key("sessionId") && rec.fields.contains_key("projectHash") {
            self.saw_meta = true;
            for (k, v) in rec.fields {
                if RECORD_METADATA.contains(&k.as_str()) {
                    if let Ok(v) = serde_json::from_str(v.get()) {
                        self.metadata.insert(k, v);
                    }
                }
            }
        }
    }
}

#[cfg(feature = "mmap")]
fn raw_replay_key(rec: &lean::Record<'_>) -> Option<String> {
    Some(format!(
        "{}\u{1f}{}",
        rec.string("type").unwrap_or_default(),
        rec.string("id")?
    ))
}

impl RecordingReplay<Value> {
    /// Fold one JSONL record into the replay state. Non-object junk is ignored.
    fn ingest(&mut self, mut rec: Value) {
        if !rec.is_object() {
            return;
        }
        if let Some(rewind) = rec.get("$rewindTo").and_then(Value::as_str) {
            self.rewind(rewind, |v| v);
            return;
        }
        if rec.get("$set").is_some_and(Value::is_object) {
            // This replay owns the record: transfer checkpoint bodies and metadata rather than
            // cloning the entire effective history before dropping its source record.
            let Value::Object(set) = rec.as_object_mut().unwrap().remove("$set").unwrap() else {
                unreachable!("$set was checked as an object")
            };
            for (k, v) in set {
                if k == "messages" {
                    // Checkpoint: rebuild the whole message list.
                    self.order.clear();
                    self.msgs.clear();
                    if let Value::Array(arr) = v {
                        for m in arr {
                            if let Some(key) = replay_key(&m) {
                                self.insert(key, m);
                            }
                        }
                    }
                } else {
                    self.metadata.insert(k, v);
                }
            }
            return;
        }
        if let Some(key) = replay_key(&rec) {
            // A message record (must have an id and a type/content); a junk record with an id but
            // no message shape is ignored either way.
            if rec.get("type").is_some() || rec.get("content").is_some() {
                self.insert(key, rec);
            }
            return;
        }
        // Metadata line(s): a real recording's metadata carries BOTH sessionId and projectHash
        // (gemini-cli's `isPartialMetadataRecord`). This guards against mistaking a `logs.json`
        // entry (sessionId + messageId + message, no projectHash) for a recording.
        if rec.get("sessionId").is_some() && rec.get("projectHash").is_some() {
            self.saw_meta = true;
            if let Value::Object(obj) = rec {
                for (k, v) in obj {
                    self.metadata.insert(k, v);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure entry point (WASM-safe): handles all three readable JSON/JSONL shapes.
// ---------------------------------------------------------------------------

/// Parse any readable Gemini session text into IR [`Session`]s, with no filesystem access.
///
/// Sniffs the shape:
/// - a chat-recording `ConversationRecord` (`.json` object with `messages[]`, or `.jsonl` with a
///   `sessionId`+`projectHash` metadata line),
/// - a gemini-cli checkpoint (`{history:[…]}` or a bare `Content[]` array),
/// - or the `logs.json` array of `{sessionId,messageId,message,…}` entries (possibly several sessions).
pub fn parse_all_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    parse_all_str_for(Harness::Gemini, text, source_path)
}

/// [`parse_all_str`] tagged as `harness` — Qwen Code shares the format, so its harness-specific
/// facts (`extra["qwen"]`) and the session tag must say "qwen", not "gemini".
pub fn parse_all_str_for(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let mut out = parse_all_str_inner(harness, text, source_path);
    for s in &mut out {
        s.harness = harness;
    }
    out
}

fn parse_all_str_inner(harness: Harness, text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    // Chat recording? (object with sessionId+messages, or jsonl with a metadata line)
    if let Some(s) = parse_chat_recording(text, harness, source_path.clone()) {
        return vec![s];
    }

    let trimmed = text.trim_start();
    // Checkpoint object `{history:[…]}`.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("history") {
                if let Some(s) = parse_checkpoint(text, "", source_path.clone()) {
                    return vec![s];
                }
            }
        }
    }
    // Array: either a logs.json (entries have messageId/message) or a bare checkpoint Content[].
    if trimmed.starts_with('[') {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
            let looks_logs = items
                .iter()
                .take(8)
                .any(|it| it.get("messageId").is_some() && it.get("message").is_some());
            if looks_logs {
                return parse_logs_str(text, source_path);
            }
            let looks_checkpoint = items
                .iter()
                .take(8)
                .any(|it| it.get("role").is_some() && it.get("parts").is_some());
            if looks_checkpoint {
                if let Some(s) = parse_checkpoint(text, "", source_path) {
                    return vec![s];
                }
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// logs.json
// ---------------------------------------------------------------------------

/// Parse a Gemini `logs.json` from its text contents into one [`Session`] per distinct `sessionId`.
fn parse_logs_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let entries: Vec<Value> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut by_session: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for e in &entries {
        let sid = e.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        by_session.entry(sid).or_default().push(e);
    }

    let mut out = Vec::new();
    for (sid, mut items) in by_session {
        if sid.is_empty() {
            continue;
        }
        items.sort_by_key(|e| e.get("messageId").and_then(Value::as_i64).unwrap_or(0));

        let title = items
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("user"))
            .filter_map(|e| e.get("message").and_then(Value::as_str))
            .find(|t| !is_ignored_user_content(t))
            .map(|t| crate::ir::truncate(t, 80));
        let times: Vec<DateTime<Utc>> = items
            .iter()
            .filter_map(|e| e.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
            .collect();

        let mut s = Session {
            cwd: source_path.as_deref().and_then(project_cwd),
            title,
            created_at: times.iter().min().copied(),
            updated_at: times.iter().max().copied(),
            source_path: source_path.clone(),
            ..Session::new(sid, Harness::Gemini)
        };
        for e in items {
            let role = match e.get("type").and_then(Value::as_str) {
                Some("assistant") | Some("model") | Some("gemini") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            let text = e.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }
            let mut m = Message::new(role);
            m.timestamp = e.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            m.content.push(Block::Text { text: text.into() });
            if role == Role::User {
                type_user_turn(&mut m);
            }
            s.messages.push(m);
        }
        out.push(s);
    }
    out
}

// ---------------------------------------------------------------------------
// Chat recordings (ConversationRecord) — legacy whole-file .json or modern .jsonl
// ---------------------------------------------------------------------------

/// Parse a gemini-cli chat recording. Handles both the legacy whole-file JSON object
/// (`{sessionId, messages:[…]}`) and the modern append-only JSONL log (metadata line + message
/// records + `$set` / `$rewindTo` control records). Returns `None` if the text is not a recording.
fn parse_chat_recording(text: &str, harness: Harness, source_path: Option<PathBuf>) -> Option<Session> {
    let trimmed = text.trim_start();

    // Legacy: a single JSON object spanning the whole file.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("sessionId") && map.contains_key("messages") {
                return Some(record_to_session(&map, harness, source_path));
            }
            // A modern recording happens to also start with `{` on its metadata first line,
            // but a *whole-file* parse of multi-line JSONL fails, so we fall through to JSONL below.
        }
    }

    // Modern JSONL: replay metadata + message records + control records.
    let mut replay = RecordingReplay::default();
    super::for_each_json_line_str(text, |rec| {
        replay.ingest(rec);
        Flow::Continue
    });

    if !replay.saw_meta && replay.msgs.is_empty() {
        return None;
    }

    // Reassemble a ConversationRecord-shaped map.
    let mut metadata = replay.metadata;
    let messages: Vec<Value> = replay.order.iter().filter_map(|id| replay.msgs.remove(id)).collect();
    metadata.insert("messages".into(), Value::Array(messages));
    Some(record_to_session(&metadata, harness, source_path))
}

/// Build the IR [`Session`] *metadata* (everything but `messages`) from a `ConversationRecord`'s
/// top-level fields. `title` is the record's `summary` when present; the per-message scan fills in a
/// first-user-turn fallback and the session `model`. `messages` is left empty — the caller streams
/// them via [`emit_record_message`].
const RECORD_METADATA: &[&str] = &[
    "sessionId",
    "startTime",
    "lastUpdated",
    "summary",
    "directories",
    "kind",
];

fn record_metadata(rec: &serde_json::Map<String, Value>, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let id = rec.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    let created_at = rec.get("startTime").and_then(Value::as_str).and_then(parse_ts);
    let updated_at = rec.get("lastUpdated").and_then(Value::as_str).and_then(parse_ts);
    let title = rec.get("summary").and_then(Value::as_str).map(str::to_string);

    // cwd: `directories[]` (added via /dir) may carry real absolute paths — use the first; else
    // recover the project path from gemini-cli's registry via the file's `tmp/<id>/` location.
    let cwd = rec
        .get("directories")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| source_path.as_deref().and_then(project_cwd));

    // A sub-agent recording (`kind: "subagent"`, stored under `chats/<parentId>/`) points at its
    // parent; the recording's own `kind` is a Gemini fact.
    let lineage = Lineage {
        parent: parent_session_from_path(source_path.as_deref()),
        ..Default::default()
    };
    let mut extra = serde_json::Map::new();
    if let Some(kind) = rec.get("kind").and_then(Value::as_str) {
        let mut bag = serde_json::Map::new();
        bag.insert("kind".into(), Value::String(kind.to_string()));
        extra.insert(harness.as_str().into(), Value::Object(bag));
    }

    Session {
        cwd,
        title,
        created_at,
        updated_at,
        source_path,
        extra,
        lineage,
        ..Session::new(id, harness)
    }
}

/// Cross-message state of one recording's emission.
#[derive(Default)]
struct EmitState {
    /// The first assistant `model` seen; becomes `Session::model`.
    session_model: Option<String>,
    /// The first prompt's text: the fallback title when the record has no `summary`.
    title: Option<String>,
    /// Tool call ids whose result was already emitted from a `toolCalls[]` entry. gemini-cli also
    /// records each tool response as a `user` record of bare `functionResponse` parts (the wire
    /// turn, `recordSyntheticMessage` in `geminiChat.ts`; 0.46.0 and later), and a resume re-syncs it
    /// into `$set.messages` once more; those copies are not new results.
    answered: std::collections::HashSet<String>,
}

/// Translate one raw `ConversationRecord` message (`obj`) into 0..2 IR [`Message`]s and hand them to
/// `sink` (an assistant turn may also emit a trailing [`Role::Tool`] turn for its tool results).
///
/// Threads [`EmitState`] across messages. Returns the sink's [`Flow`] so a streaming caller can stop
/// early.
fn emit_record_message(
    harness: Harness,
    obj: &serde_json::Map<String, Value>,
    st: &mut EmitState,
    sink: &mut dyn MessageSink,
) -> Flow {
    emit_record_message_with(harness, obj, st, None, sink)
}

fn emit_record_message_with(
    harness: Harness,
    obj: &serde_json::Map<String, Value>,
    st: &mut EmitState,
    payload: Option<&lean::Payload<'_>>,
    sink: &mut dyn MessageSink,
) -> Flow {
    let content_blocks = |out: &mut Vec<Block>| match payload {
        Some(payload) => out.extend(payload.take_content()),
        None => push_content_blocks(out, obj.get("content")),
    };
    let mty = obj.get("type").and_then(Value::as_str).unwrap_or("user");
    let ts = obj.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let id = obj.get("id").and_then(Value::as_str).map(str::to_string);

    match mty {
        "gemini" | "model" | "assistant" => {
            let mut m = Message::new(Role::Assistant);
            m.id = id;
            m.timestamp = ts;
            // The first model seen becomes `Session::model`; a turn records its own only when it
            // differs (`Message::model` is the exception, not the rule — IR diet).
            if let Some(model) = obj.get("model").and_then(Value::as_str) {
                match st.session_model.as_deref() {
                    None => st.session_model = Some(model.to_string()),
                    Some(m0) if m0 != model => m.model = Some(model.to_string()),
                    _ => {}
                }
            }
            m.usage = obj.get("tokens").and_then(parse_tokens);

            // Thoughts → Thinking blocks (subject + description).
            let thoughts = obj.get("thoughts").and_then(Value::as_array);
            if let Some(thoughts) = payload.and_then(lean::Payload::take_thoughts) {
                m.content.extend(thoughts);
            } else if let Some(thoughts) = thoughts {
                for t in thoughts {
                    let subj = t.get("subject").and_then(Value::as_str).unwrap_or("");
                    let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                    let text = if subj.is_empty() {
                        desc.to_string()
                    } else if desc.is_empty() {
                        subj.to_string()
                    } else {
                        format!("**{subj}** {desc}")
                    };
                    if !text.is_empty() {
                        m.content.push(Block::Thinking {
                            text: text.into(),
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }

            // Assistant content: string, or array of Parts (text / inlineData / functionCall). A
            // record re-synced from the API history (`updateMessagesFromHistory`, every `$set.messages`
            // since 0.46.0) carries its thoughts and calls twice: as `thought`/`functionCall` parts
            // in `content` AND in `thoughts[]`/`toolCalls[]`. The structured fields win; the parts
            // are kept only when the fields are absent (older records, checkpoints).
            let calls = obj.get("toolCalls").and_then(Value::as_array);
            let call_ids: std::collections::HashSet<&str> = calls
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("id").and_then(Value::as_str))
                .collect();
            let has_thoughts = thoughts.is_some_and(|t| !t.is_empty());
            let mut parts = Vec::new();
            content_blocks(&mut parts);
            m.content.extend(parts.into_iter().filter(|b| match b {
                Block::Thinking { .. } => !has_thoughts,
                Block::ToolUse { id, .. } => !call_ids.contains(id.as_str()),
                _ => true,
            }));

            // Tool calls (gemini-cli ToolCallRecord) → ToolUse + paired ToolResult. A call with an
            // `agentId` ran a sub-agent (`invoke_agent`), whose own recording is
            // `chats/<this session>/<agentId>.jsonl`: the turn is a SubagentSpawn, its result a
            // SubagentReturn.
            let mut tool_results: Vec<Block> = Vec::new();
            let mut agent_ids: Vec<Value> = Vec::new();
            if let Some(calls) = calls {
                for (i, c) in calls.iter().enumerate() {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let call_id = c
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{name}-call"));
                    let input = payload
                        .and_then(|p| p.take_input(i))
                        .unwrap_or_else(|| c.get("args").cloned().unwrap_or(Value::Null).into());
                    if let Some(a) = c.get("agentId").and_then(Value::as_str) {
                        agent_ids.push(Value::String(a.to_string()));
                    }
                    m.content.push(Block::ToolUse {
                        id: call_id.clone(),
                        name: name.to_string(),
                        input,
                        namespace: None,
                    });
                    let status = c.get("status").and_then(Value::as_str);
                    let is_error = status.map(|s| s.eq_ignore_ascii_case("error")).unwrap_or(false);
                    let content = payload
                        .and_then(|p| p.take_result(i))
                        .or_else(|| {
                            tool_result_text(c.get("result"))
                                .or_else(|| c.get("resultDisplay").and_then(Value::as_str).map(str::to_string))
                                .map(crate::lazy::Text::from)
                        })
                        .unwrap_or_default();
                    // A tool that completed with EMPTY output still ran and still answered — the
                    // turn is real. Requiring non-empty text dropped it, and with it the whole Tool
                    // turn, so a `ToolSearch` that found nothing vanished from a round trip. A call
                    // with no `status` yet has not completed and still contributes no result.
                    if !content.is_empty() || status.is_some() {
                        st.answered.insert(call_id.clone());
                        tool_results.push(Block::ToolResult {
                            tool_use_id: call_id,
                            content,
                            is_error,
                            tool_name: Some(name.to_string()),
                            status: status.map(str::to_string),
                            details: None,
                        });
                    }
                }
            }

            let spawned = !agent_ids.is_empty();
            if spawned {
                m.kind = MessageKind::SubagentSpawn;
                m.harness_extra_mut(harness)
                    .insert("subagent_ids".into(), Value::Array(agent_ids));
            }
            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
            // Emit tool results as a separate Tool-role turn (mirrors how Gemini feeds
            // functionResponses back as a user turn, but kept distinct in the IR).
            if !tool_results.is_empty() {
                let mut tm = Message::new(Role::Tool);
                if spawned {
                    tm.kind = MessageKind::SubagentReturn;
                    tm.origin = Origin::Subagent;
                }
                tm.timestamp = ts;
                tm.content = tool_results;
                if sink.message(tm) == Flow::Stop {
                    return Flow::Stop;
                }
            }
        }
        "info" | "error" | "warning" => {
            // Harness notices (`info`/`warning`) and errors the model never answered (`error`):
            // System turns, searchable, typed by kind; the record type is a Gemini fact.
            let kind = if mty == "error" {
                MessageKind::Error
            } else {
                MessageKind::Notice
            };
            let mut m = Message::of_kind(Role::System, kind, Origin::Harness);
            m.id = id;
            m.timestamp = ts;
            m.harness_extra_mut(harness)
                .insert("record_type".into(), Value::String(mty.to_string()));
            content_blocks(&mut m.content);
            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
        }
        "rewind" => {
            // A `$rewindTo` control record (see `RecordingReplay::ingest`): what follows does not
            // continue what precedes. Kept as a Branch marker so a reader sees the cut; it is not a
            // message record, so it carries no id or timestamp of its own.
            let target = obj.get("rewindTo").and_then(Value::as_str).unwrap_or("");
            let mut m = Message::of_kind(Role::System, MessageKind::Branch, Origin::Harness);
            m.content.push(Block::Text {
                text: format!("[rewound to {target}]").into(),
            });
            let bag = m.harness_extra_mut(harness);
            bag.insert("record_type".into(), Value::String("rewind".into()));
            bag.insert("rewind_to".into(), Value::String(target.to_string()));
            if sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
        }
        _ if payload.map_or_else(
            || is_tool_response_turn(obj.get("content")),
            lean::Payload::is_tool_response,
        ) =>
        {
            // A `user` record of bare `functionResponse` parts: the tool output fed back to the
            // model (see [`EmitState::answered`]). A copy of results already emitted is dropped;
            // otherwise (no `toolCalls[]` result to pair with) it is the Tool turn itself.
            let mut blocks = Vec::new();
            content_blocks(&mut blocks);
            blocks.retain(|b| match b {
                Block::ToolResult { tool_use_id, .. } => st.answered.insert(tool_use_id.clone()),
                _ => true,
            });
            if !blocks.is_empty() {
                let mut m = Message::new(Role::Tool);
                m.id = id;
                m.timestamp = ts;
                m.content = blocks;
                if sink.message(m) == Flow::Stop {
                    return Flow::Stop;
                }
            }
        }
        _ => {
            // user (and anything else) → a human prompt, or context gemini-cli injected.
            let mut m = Message::new(Role::User);
            m.id = id;
            m.timestamp = ts;
            content_blocks(&mut m.content);
            // A BeforeAgent hook's `additionalContext` rides in the SAME record as the prompt, as a
            // trailing `<hook_context>` part (0.46.0, 0.63.0): split it off into its own Hook turn.
            let mut hook = Message::of_kind(Role::System, MessageKind::InjectedContext, Origin::Hook);
            if m.content.len() > 1 {
                let (hooked, rest): (Vec<Block>, Vec<Block>) = std::mem::take(&mut m.content).into_iter().partition(
                    |b| matches!(b, Block::Text { text } if payload.map_or_else(|| text.trim_start().starts_with("<hook_context>"), |p| p.is_hook(text))),
                );
                m.content = rest;
                hook.content = hooked;
                if m.content.is_empty() {
                    m.content = std::mem::take(&mut hook.content);
                }
            }
            if let Some(payload) = payload {
                payload.user_facts(&mut m, st);
            } else {
                type_user_turn(&mut m);
            }
            // `displayContent`: what the user typed when it differs from what the model got (a
            // custom `/command` before expansion, an `@file` prompt before the file was inlined).
            let typed = match payload {
                Some(payload) => payload.take_display_text(),
                None => obj.get("displayContent").map(display_text),
            };
            if let Some(typed) = typed.filter(|t| !t.is_empty()) {
                m.harness_extra_mut(harness)
                    .insert("display_text".into(), Value::String(typed));
            }
            if payload.is_none() && st.title.is_none() {
                st.title = m
                    .text()
                    .filter(|t| !is_ignored_user_content(t))
                    .map(|t| crate::ir::truncate(&t, 80));
            }
            if !m.content.is_empty() && sink.message(m) == Flow::Stop {
                return Flow::Stop;
            }
            if !hook.content.is_empty() {
                hook.timestamp = ts;
                if sink.message(hook) == Flow::Stop {
                    return Flow::Stop;
                }
            }
        }
    }
    Flow::Continue
}

/// The text of a `displayContent` (`PartListUnion`): a string, or its parts' `text` joined.
fn display_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.as_str().or_else(|| p.get("text").and_then(Value::as_str)))
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(_) => v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Whether a record's `content` is a non-empty list of `functionResponse` parts and nothing else.
fn is_tool_response_turn(content: Option<&Value>) -> bool {
    let Some(Value::Array(parts)) = content else {
        return false;
    };
    !parts.is_empty() && parts.iter().all(|p| p.get("functionResponse").is_some())
}

/// Map a `ConversationRecord` (as a JSON object) onto an IR [`Session`] — the whole-`Session`
/// convenience used by the pure (no-filesystem) [`parse_all_str`] / [`parse_chat_recording`] paths.
/// Builds the metadata via [`record_metadata`] and collects messages via [`emit_record_message`].
///
/// `summary` (when present) is the title and overrides the first-user-turn fallback that the message
/// scan fills in — same precedence as before this was split for streaming.
fn record_to_session(rec: &serde_json::Map<String, Value>, harness: Harness, source_path: Option<PathBuf>) -> Session {
    let mut session = record_metadata(rec, harness, source_path);
    let summary = session.title.take();

    let empty = vec![];
    let raw_msgs = rec.get("messages").and_then(Value::as_array).unwrap_or(&empty);

    let mut sink = CollectSink::default();
    let mut st = EmitState::default();
    for rm in raw_msgs {
        let Some(obj) = rm.as_object() else { continue };
        if emit_record_message(harness, obj, &mut st, &mut sink) == Flow::Stop {
            break;
        }
    }

    session.model = st.session_model;
    session.title = summary.or(st.title);
    session.messages = sink.messages;
    session
}

/// Append IR blocks for a Gemini `content` value, which is a `PartListUnion`: a bare string, a single
/// Part object, or an array of Parts. Each Part may carry `text`, `inlineData`, `functionCall`, or
/// `functionResponse`.
fn push_content_blocks(out: &mut Vec<Block>, content: Option<&Value>) {
    let Some(content) = content else { return };
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(Block::Text { text: s.clone().into() });
            }
        }
        Value::Array(parts) => {
            for p in parts {
                push_part(out, p);
            }
        }
        Value::Object(_) => push_part(out, content),
        _ => {}
    }
}

fn push_part(out: &mut Vec<Block>, part: &Value) {
    if let Some(s) = part.as_str() {
        if !s.is_empty() {
            out.push(Block::Text {
                text: s.to_string().into(),
            });
        }
        return;
    }
    let Some(obj) = part.as_object() else { return };

    if let Some(fc) = obj.get("functionCall").and_then(Value::as_object) {
        let name = fc.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fc
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        out.push(Block::ToolUse {
            id,
            name: name.to_string(),
            input: fc.get("args").cloned().unwrap_or(Value::Null).into(),
            namespace: None,
        });
        return;
    }
    if let Some(fr) = obj.get("functionResponse").and_then(Value::as_object) {
        let name = fr.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fr
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        let content = fr.get("response").map(value_to_text).unwrap_or_default();
        out.push(Block::ToolResult {
            tool_use_id: id,
            content: content.into(),
            is_error: false,
            tool_name: Some(name.to_string()),
            status: None,
            details: None,
        });
        return;
    }
    if let Some(inline) = obj.get("inlineData").and_then(Value::as_object) {
        out.push(Block::Image {
            media_type: inline.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: None, // we don't inline base64 bytes into the IR
        });
        return;
    }
    if let Some(fd) = obj.get("fileData").and_then(Value::as_object) {
        out.push(Block::File {
            mime: fd.get("mimeType").and_then(Value::as_str).map(str::to_string),
            path: None,
            source: fd.get("fileUri").and_then(Value::as_str).map(str::to_string),
        });
        return;
    }
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        let is_thought = obj.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if text.is_empty() {
            return;
        }
        if is_thought {
            out.push(Block::Thinking {
                text: text.to_string().into(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
        } else {
            out.push(Block::Text {
                text: text.to_string().into(),
            });
        }
    }
}

/// Extract human-readable text from a `toolCall.result` (a `PartListUnion`, typically a list of
/// `{functionResponse:{response:{output}}}` parts).
fn tool_result_text(result: Option<&Value>) -> Option<String> {
    let result = result?;
    match result {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                let chunk = if let Some(fr) = p.get("functionResponse") {
                    fr.get("response").map(value_to_text).unwrap_or_default()
                } else if let Some(t) = p.get("text").and_then(Value::as_str) {
                    t.to_string()
                } else {
                    value_to_text(p)
                };
                if !chunk.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&chunk);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Value::Object(_) => {
            if let Some(fr) = result.get("functionResponse") {
                Some(fr.get("response").map(value_to_text).unwrap_or_default())
            } else {
                Some(value_to_text(result))
            }
        }
        _ => None,
    }
}

/// Turn an arbitrary JSON value into display text, unwrapping the common `{output: "…"}` wrapper.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(Value::String(o)) = map.get("output") {
                o.clone()
            } else if let Some(Value::String(e)) = map.get("error") {
                e.clone()
            } else {
                v.to_string()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Gemini's `tokens.input` is the API's `promptTokenCount`, which *includes* `cached`; the IR's
/// disjoint `input_tokens` is the remainder (the emitter adds `cached` back).
fn parse_tokens(v: &Value) -> Option<Usage> {
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(Value::as_u64);
    let cached = get("cached");
    Some(Usage {
        input_tokens: get("input").map(|i| i.saturating_sub(cached.unwrap_or(0))),
        output_tokens: get("output"),
        cache_read_tokens: get("cached"),
        ..Usage::default()
    })
}

// ---------------------------------------------------------------------------
// gemini-cli checkpoints — `{history: Content[]}` or bare `Content[]`
// ---------------------------------------------------------------------------

/// Parse a gemini-cli checkpoint into an IR [`Session`]. The id is derived from the `checkpoint-<tag>`
/// filename when available (the `tag` is percent-encoded by gemini-cli; we decode it best-effort).
fn parse_checkpoint(text: &str, file_name: &str, source_path: Option<PathBuf>) -> Option<Session> {
    let v: Value = serde_json::from_str(text).ok()?;
    let history = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(map) => map.get("history").and_then(Value::as_array).cloned()?,
        _ => return None,
    };

    let id = checkpoint_id(file_name);
    let mut messages = Vec::new();
    let mut title: Option<String> = None;

    for content in &history {
        let Some(obj) = content.as_object() else { continue };
        let role = match obj.get("role").and_then(Value::as_str) {
            Some("model") | Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => Role::User, // gemini uses "user" for both prompts and tool responses
        };
        let mut m = Message::new(role);
        if role == Role::System {
            m.kind = MessageKind::SystemPrompt;
        }
        push_content_blocks(&mut m.content, obj.get("parts"));
        // If this "user" turn is purely tool results, retag it as a Tool turn.
        if role == Role::User
            && !m.content.is_empty()
            && m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }))
        {
            m.role = Role::Tool;
            m.kind = MessageKind::ToolResult;
            m.origin = Origin::Harness;
        } else if role == Role::User {
            // The API history merges consecutive user parts into one Content, so a 0.63.0 checkpoint
            // holds the `<session_context>` preamble and the first prompt as two parts of ONE turn.
            // Split the leading injected parts off into their own InjectedContext turn.
            let lead = m
                .content
                .iter()
                .take_while(
                    |b| matches!(b, Block::Text { text } if is_ignored_user_content(text) && !text.trim().is_empty()),
                )
                .count();
            if lead > 0 && lead < m.content.len() {
                let mut ctx = Message::new(Role::User);
                ctx.content = m.content.drain(..lead).collect();
                type_user_turn(&mut ctx);
                messages.push(ctx);
            }
            type_user_turn(&mut m);
        }
        if m.role == Role::User && title.is_none() {
            title = m
                .text()
                .filter(|t| !is_ignored_user_content(t))
                .map(|t| crate::ir::truncate(&t, 80));
        }
        if !m.content.is_empty() {
            messages.push(m);
        }
    }

    if messages.is_empty() {
        return None;
    }

    // The file is a `/chat save <tag>` point that `/chat resume <tag>` restores: a checkpoint.
    let tag = id.strip_prefix("checkpoint-").unwrap_or("").to_string();
    let mut s = Session {
        cwd: source_path.as_deref().and_then(project_cwd),
        title,
        messages,
        source_path,
        ..Session::new(id, Harness::Gemini)
    };
    let bag = s.harness_extra_mut(Harness::Gemini);
    crate::affordance::note_checkpoints(bag, 1, []);
    if let Some(cp) = bag
        .get_mut(crate::affordance::CHECKPOINTS_KEY)
        .and_then(Value::as_object_mut)
    {
        if !tag.is_empty() {
            cp.insert("tag".into(), Value::String(tag));
        }
    }
    Some(s)
}

fn checkpoint_id(file_name: &str) -> String {
    let tag = file_name
        .strip_prefix("checkpoint-")
        .and_then(|s| s.strip_suffix(".json"))
        .unwrap_or("");
    if tag.is_empty() {
        return "checkpoint".to_string();
    }
    let decoded = percent_encoding::percent_decode_str(tag)
        .decode_utf8()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| tag.to_string());
    format!("checkpoint-{decoded}")
}

// ---------------------------------------------------------------------------
// SessionRef helpers
// ---------------------------------------------------------------------------

fn session_ref(s: &Session, path: &Path, harness: Harness) -> SessionRef {
    SessionRef {
        id: s.id.clone(),
        harness,
        path: path.to_path_buf(),
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at,
        // The contract count (`SessionRef::message_count`): user + assistant turns only — a tool
        // result, an `info`/`error` notice or a `$rewindTo` branch marker is a message, not a turn.
        message_count: s
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User | Role::Assistant))
            .count(),
    }
}

/// `tracker/*.json` as the session's plan and each `plans/*.md` as one plan row (its first
/// heading), from the session's directory beside `chats/`.
fn side_affordances(recording: &Path, id: &str) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{item_of, json_files, plan_state, Affordance, Kind};
    let Some(project) = recording
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "chats"))
        .and_then(Path::parent)
    else {
        return Vec::new();
    };
    let dir = project.join(id);
    let mut out = Vec::new();
    let items: Vec<_> = json_files(&dir.join("tracker"))
        .iter()
        .filter_map(|(_, v)| item_of(v))
        .collect();
    out.extend(plan_state(Kind::Plan, "tracker/", items));
    let mut plans: Vec<PathBuf> = fs::read_dir(dir.join("plans"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    plans.sort();
    for p in plans {
        let text = fs::read_to_string(&p).unwrap_or_default();
        let head = text
            .lines()
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("plan.md");
        out.push(Affordance::store(
            Kind::Plan,
            format!("plans/{name}"),
            truncate(head, 120),
            Vec::new(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn checked_complete() -> ParseOptions {
        ParseOptions {
            spans: true,
            ..ParseOptions::complete()
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/gemini/{}", env!("CARGO_MANIFEST_DIR"), name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[cfg(not(feature = "mmap"))]
    #[test]
    fn portable_legacy_views_preserve_wrapper_acceptance_and_independent_children() {
        let world = cv_testkit::World::in_process("gemini-legacy-acceptance");
        let path = world.base.join("source.json");
        let large = serde_json::to_string(&"x\nβ".repeat(6000)).unwrap();
        let message = format!("{{\"type\":\"gemini\",\"id\":\"a\",\"toolCalls\":[{{\"name\":\"custom\",\"args\":{{\"payload\":{large}}}}}]}}");
        let child_poison = r#"{"type":"user","id":"bad","content":"poison","ignored":"\ud800"}"#;
        let mut cases = vec![
            format!("{{\"sessionId\":\"source\",\"messages\":[{child_poison},{message}],\"ignored\":1e400}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"ignored\":\"\\ud800\"}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"ignored\":1,\"ignored\":2}}"),
            format!("[\"source\",null,null,\"summary\",[],[{message}],\"main\"]"),
            format!("[\"source\",null,null,\"summary\",[],[{message}]]"),
            format!("[\"source\",null,null,\"summary\",[],[{message}],\"main\",0]"),
            format!("{{\"sessionId\":\"source\",\"session\\u0049d\":\"source\",\"messages\":[{message}]}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"messages\":[]}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"directories\":[\"ok\",false]}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"directories\":[\"\\ud800\"]}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"\\ud800\":0}}"),
            format!("{{\"sessionId\":\"source\",\"messages\":[{message}],\"summary\":\"\\ud800\"}}"),
            "".into(),
            "not JSON".into(),
            "null".into(),
            r#"{"sessionId":"source","messages":null}"#.into(),
        ];
        for field in ["sessionId", "startTime", "lastUpdated", "summary", "kind"] {
            cases.push(format!(
                "{{\"sessionId\":\"source\",\"messages\":[{message}],\"{field}\":false}}"
            ));
        }
        for (i, text) in cases.into_iter().enumerate() {
            fs::write(&path, &text).unwrap();
            let mut full = CollectSink::default();
            let expected = stream_legacy_json_recording(&path, Harness::Gemini, &ParseOptions::full(), &mut full)
                .unwrap()
                .map(|mut s| {
                    s.messages = full.messages;
                    s
                });
            let mut lazy = CollectSink::default();
            let actual = stream_legacy_json_recording(&path, Harness::Gemini, &ParseOptions::lazy(), &mut lazy)
                .unwrap()
                .map(|mut s| {
                    s.messages = lazy.messages;
                    s.materialize().unwrap();
                    s
                });
            assert_eq!(actual.as_ref().map(as_json), expected.as_ref().map(as_json), "case {i}");
        }
    }

    fn json_inputs(session: &Session) -> Vec<&crate::Json> {
        session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { input, .. } => Some(input),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn native_json_arguments_keep_real_values_and_exact_ranges() {
        let world = cv_testkit::World::in_process("gemini-json-inputs");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let big = serde_json::to_string(&"x\nβ".repeat(crate::lazy::INLINE_MAX)).unwrap();
        let args = [
            format!("{{\"payload\":{big},\"duplicate\":1,\"dupl\\u0069cate\":2}}"),
            format!("[{big},null,true,3]"),
            big,
            "true".into(),
            "null".into(),
            "2.5".into(),
            format!("\"{}\"", "x".repeat(crate::lazy::INLINE_MAX - 2)),
            format!("\"{}\"", "x".repeat(crate::lazy::INLINE_MAX - 1)),
        ];
        let mut records: Vec<String> = args.iter().enumerate().map(|(i, arg)| format!(
            "{{\"type\":\"gemini\",\"id\":\"a{i}\",\"toolCalls\":[{{\"name\":\"custom\",\"id\":\"c{i}\",\"args\": {arg} }}]}}"
        )).collect();
        records.push(format!("{{\"type\":\"gemini\",\"id\":\"parts\",\"content\":[{{\"functionCall\":{{\"name\":\"custom\",\"args\": {} }} }}]}}", args[0]));
        records.push(r#"{"type":"gemini","id":"missing","toolCalls":[{"name":"custom"}]}"#.into());
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-source.jsonl"
            } else {
                "session-source.json"
            });
            let text = if modern {
                format!(
                    "{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\r\n \t{}\r\n",
                    records.join("\r\n \t")
                )
            } else {
                format!(
                    "{{\"sessionId\":\"source\",\"messages\":[ {} ]}}",
                    records.join(",\n \t")
                )
            };
            fs::write(&path, &text).unwrap();
            let adapter = Gemini { roots: vec![] };
            let reference = SessionRef::new("source", Harness::Gemini, path.clone());
            let pure = parse_all_str(&text, Some(path.clone())).remove(0);
            assert!(json_inputs(&pure).iter().all(|j| matches!(j, crate::Json::Owned(_))));
            for opts in [
                ParseOptions::full(),
                ParseOptions::bulk(),
                ParseOptions::complete(),
                checked_complete(),
                ParseOptions::lazy(),
                ParseOptions::lazy_extra(),
            ] {
                let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
                let inputs = json_inputs(&actual);
                assert_eq!(inputs.len(), args.len() + 2);
                for (i, input) in inputs.iter().enumerate() {
                    let expected_source =
                        opts.spans && (i == args.len() || (i < args.len() && args[i].len() > crate::lazy::INLINE_MAX));
                    assert_eq!(
                        matches!(input, crate::Json::Source(_)),
                        expected_source,
                        "mode {opts:?}, input {i}"
                    );
                    if let crate::Json::Source(range) = input {
                        let actual_raw = &text[range.offset() as usize..(range.offset() + range.len()) as usize];
                        assert_eq!(actual_raw, &args[if i == args.len() { 0 } else { i }]);
                    }
                }
                actual.materialize().unwrap();
                assert_eq!(as_json(&actual), as_json(&pure));
            }
        }
    }

    #[test]
    fn native_json_arguments_survive_set_replacement_rewind_and_invalid_lines() {
        let world = cv_testkit::World::in_process("gemini-json-replay");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-source.jsonl");
        let large = serde_json::json!({"payload": "x\nβ".repeat(5000)});
        let record = |id: &str| serde_json::json!({"type":"gemini","id":id,"content":[{"functionCall":{"name":"custom","id":id,"args":large}}]});
        let lines = [
            serde_json::json!({"sessionId":"source","projectHash":"x","future":large}),
            serde_json::json!({"$set":{"future":large,"summary":"retained summary"}}),
            record("old"),
            serde_json::json!({"$set":{"messages":[record("keep"),record("replace"),record("drop")]}}),
            serde_json::json!({"type":"user","id":"keep","content":"same id, separate kind"}),
            serde_json::json!({"$rewindTo":"drop"}),
            record("replace"),
            record("after"),
        ];
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(format!(" \t{line}\r\n").as_bytes());
            bytes.extend_from_slice(b"invalid JSON\n\xff\n{\"$set\":{\"messages\":[]},\"invalid\":\"\\ud800\"}\n");
        }
        bytes.extend_from_slice(
            "\u{2003}{\"type\":\"user\",\"id\":\"unicode-whitespace\",\"content\":\"keep portable trim\"}\u{2003}\n"
                .as_bytes(),
        );
        fs::write(&path, bytes).unwrap();
        let adapter = Gemini { roots: vec![] };
        let reference = SessionRef::new("source", Harness::Gemini, path);
        let expected = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full()).unwrap();
        let mut actual = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
        assert_eq!(json_inputs(&actual).len(), 3);
        assert!(json_inputs(&actual).iter().all(|j| matches!(j, crate::Json::Source(_))));
        actual.materialize().unwrap();
        assert_eq!(as_json(&actual), as_json(&expected));
    }

    #[test]
    fn selected_composite_results_and_thoughts_match_owned_and_keep_source_guards() {
        let world = cv_testkit::World::in_process("gemini-selected-results");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let body = "escaped\n\"quote\" β😀".repeat(7000);
        let assistant = serde_json::json!({"type":"gemini","id":"a","content":[{"text":"duplicate thought","thought":true}],"thoughts":[{"subject":"subject","description":body},{"subject":"","description":body},{"subject":body,"description":""},null],"toolCalls":[
            {"id":"composite","name":"custom","args":{},"status":"success","result":[null,{"text":""},{"text":body},{"functionResponse":{"response":{"output":body}}},{"future":body}]},
            {"id":"canonical","name":"custom","status":"success","result":{"z":body,"a":1}},
            {"id":"empty","name":"custom","status":"error","result":{"functionResponse":null},"resultDisplay":"unused"}
        ]});
        let adapter = Gemini { roots: vec![] };
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-selected.jsonl"
            } else {
                "session-selected.json"
            });
            let bytes = if modern {
                format!("{{\"sessionId\":\"selected\"}}\n{assistant}\n")
            } else {
                serde_json::json!({"sessionId":"selected","messages":[assistant]}).to_string()
            };
            fs::write(&path, &bytes).unwrap();
            let reference = SessionRef::new("selected", Harness::Gemini, path.clone());
            let expected = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full()).unwrap();
            let mut actual = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
            assert!(matches!(
                actual.messages[0].content[0],
                Block::Thinking {
                    text: crate::lazy::Text::Source(_),
                    ..
                }
            ));
            actual.materialize().unwrap();
            assert_eq!(as_json(&actual), as_json(&expected));
            let mut retained = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
            fs::write(&path, b"{}").unwrap();
            assert!(retained.materialize().is_err());
            for at in [1, 2] {
                fs::write(&path, &bytes).unwrap();
                let mut calls = 0;
                let mut sink = crate::stream::TryFnSink::new(|_| {
                    calls += 1;
                    if calls == at {
                        fs::write(&path, b"{}").unwrap();
                        anyhow::bail!("consumer cap");
                    }
                    Ok(Flow::Continue)
                });
                let error = adapter
                    .stream(&reference, &ParseOptions::lazy(), &mut sink)
                    .unwrap_err();
                assert!(format!("{error:#}").contains("changed"), "{error:#}");
                assert!(!format!("{error:#}").contains("consumer cap"));
            }
        }
    }

    #[test]
    fn native_json_source_mutation_stops_before_paired_result_and_at_completion() {
        let world = cv_testkit::World::in_process("gemini-json-errors");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        struct Delete {
            path: PathBuf,
            at: usize,
            calls: usize,
        }
        impl MessageSink for Delete {
            fn message(&mut self, _: Message) -> Flow {
                self.calls += 1;
                if self.calls == self.at {
                    fs::remove_file(&self.path).unwrap();
                }
                Flow::Continue
            }
        }
        let large = serde_json::json!({"payload":"x".repeat(6000)});
        let assistant = serde_json::json!({"type":"gemini","id":"a","toolCalls":[{"id":"c","name":"custom","args":large,"status":"success","result":"done"}]});
        for modern in [false, true] {
            let path = chats.join(if modern {
                "session-source.jsonl"
            } else {
                "session-source.json"
            });
            let user = serde_json::json!({"type":"user","id":"u","content":"later"});
            let text = if modern {
                format!("{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\n{assistant}\n{user}\n")
            } else {
                serde_json::json!({"sessionId":"source","messages":[assistant,user]}).to_string()
            };
            for at in [1, 3] {
                fs::write(&path, &text).unwrap();
                let mut sink = Delete {
                    path: path.clone(),
                    at,
                    calls: 0,
                };
                let result = Gemini { roots: vec![] }.stream(
                    &SessionRef::new("source", Harness::Gemini, path.clone()),
                    &ParseOptions::lazy(),
                    &mut sink,
                );
                assert!(result.is_err());
                assert_eq!(sink.calls, at);
            }
        }
    }

    #[test]
    fn native_json_arguments_feed_consumers_and_fail_when_detached_source_changes() {
        let world = cv_testkit::World::in_process("gemini-json-consumers");
        let chats = world.base.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-source.jsonl");
        let tools = [
            ("read_file", serde_json::json!({"absolute_path":"/tmp/a.rs"})),
            ("run_shell_command", serde_json::json!({"command":"echo hello"})),
            (
                "write_todos",
                serde_json::json!({"todos":[{"description":"ship", "status":"pending"}]}),
            ),
        ];
        let text = "{\"sessionId\":\"source\",\"projectHash\":\"x\"}\n".to_owned() + &tools.into_iter().enumerate().map(|(i, (name, mut args))| {
            args.as_object_mut().unwrap().insert("ignored_payload".into(), Value::String("x\nβ".repeat(6000)));
            serde_json::json!({"type":"gemini","id":i.to_string(),"toolCalls":[{"id":i.to_string(),"name":name,"args":args}]}).to_string() + "\n"
        }).collect::<String>();
        fs::write(&path, text).unwrap();
        let adapter = Gemini { roots: vec![] };
        let reference = SessionRef::new("source", Harness::Gemini, path.clone());
        let full = crate::stream::collect_with(&adapter, &reference, &ParseOptions::full()).unwrap();
        let mut expected = crate::events::EventSink::new(None);
        for message in &full.messages {
            expected.try_message(message.clone()).unwrap();
        }
        let mut actual = crate::events::EventSink::new(None);
        adapter.stream(&reference, &ParseOptions::lazy(), &mut actual).unwrap();
        assert_eq!(actual.into_events().unwrap(), expected.into_events().unwrap());
        assert_eq!(
            serde_json::to_value(crate::affordance::read(&adapter, &reference).unwrap()).unwrap(),
            serde_json::to_value(crate::affordance::of_session(&full).unwrap()).unwrap()
        );
        let lazy = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
        assert!(json_inputs(&lazy).iter().all(|j| matches!(j, crate::Json::Source(_))));
        fs::remove_file(path).unwrap();
        assert!(json_inputs(&lazy)[0].resolve().is_err());
        let mut events = crate::events::EventSink::new(None);
        assert!(events.try_message(lazy.messages[0].clone()).is_err());
    }

    #[test]
    fn tokens_input_is_made_disjoint_from_cached() {
        // Gemini's `input` is `promptTokenCount`, which includes `cached`.
        let u = parse_tokens(&serde_json::json!({"input": 1000, "output": 40, "cached": 600})).unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (Some(400), Some(600), Some(40))
        );
        assert_eq!(u.prompt_tokens(), 1000, "the prompt the model read is unchanged");
    }

    #[test]
    fn parses_logs_json_grouped_by_session() {
        let text = fixture("logs.json");
        let mut sessions = parse_all_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[1].id, "sess-b");
        assert_eq!(sessions[1].harness, Harness::Gemini);
    }

    #[test]
    fn parses_legacy_chat_recording() {
        let text = fixture("session_legacy.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        assert_eq!(s.model.as_deref(), Some("gemini-3-flash-preview"));
        assert_eq!(
            s.title.as_deref(),
            Some("Investigated whether brorb is a useful meditation aid.")
        );
        assert!(s.created_at.is_some() && s.updated_at.is_some());

        // user, gemini(with thinking+text+tooluse), tool(result), gemini(text), info(system)
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
            ]
        );
        assert!(
            s.messages.iter().all(|m| m.model.is_none()),
            "every turn used the session model, so none repeats it"
        );

        let asst = &s.messages[1];
        assert!(asst.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(asst.content.iter().any(|b| matches!(b, Block::Text { .. })));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert!(asst.usage.is_some());

        // tool result carried in the following Tool turn
        let tool = &s.messages[2];
        let got = tool
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("meditation timer")));
        assert!(got, "tool result text should be extracted");

        // info message kept as a System notice, its record type a Gemini fact
        let info = &s.messages[4];
        assert_eq!(info.role, Role::System);
        assert_eq!(info.harness_extra(Harness::Gemini).unwrap()["record_type"], "info");
        assert_nested_extra(s, Harness::Gemini);
    }

    fn kinds(s: &Session) -> Vec<(Role, MessageKind, Origin)> {
        s.messages.iter().map(|m| (m.role, m.kind, m.origin)).collect()
    }

    /// Every `extra` key, on the session and on each message, is the harness's own: no flat keys.
    fn assert_nested_extra(s: &Session, h: Harness) {
        for k in s.extra.keys() {
            assert_eq!(k, h.as_str(), "flat session extra key {k:?}");
        }
        for (i, m) in s.messages.iter().enumerate() {
            for k in m.extra.keys() {
                assert_eq!(k, h.as_str(), "flat extra key {k:?} on message {i}");
            }
        }
    }

    #[test]
    fn user_turn_kinds_and_notices() {
        // gemini-cli writes context of its own as `user` records — `isIgnoredUserContent`
        // (`utils/sessionUtils.ts:97`): slash commands, `?` queries, the `<session_context>` preamble,
        // `<hook_context>` hook output — and `info` / `warning` / `error` records.
        let text = fixture("session_ignored_user_content.jsonl");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::System, MessageKind::InjectedContext, Origin::Hook),
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Notice, Origin::Harness),
                (Role::System, MessageKind::Error, Origin::Harness),
            ]
        );
        assert_eq!(
            s.title.as_deref(),
            Some("fix the build"),
            "only the human prompt titles"
        );
        assert_eq!(s.first_user_text().as_deref(), Some("fix the build"));
        let record_type = |i: usize| s.messages[i].harness_extra(Harness::Gemini).unwrap()["record_type"].clone();
        assert_eq!(record_type(6), "info");
        assert_eq!(record_type(7), "warning");
        assert_eq!(record_type(8), "error");
        assert!(s.messages[4].extra.is_empty(), "a plain prompt carries no Gemini facts");
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "main");
        assert!(s.lineage.is_empty());
        assert_nested_extra(s, Harness::Gemini);
    }

    #[test]
    fn message_model_only_when_it_differs_from_the_session() {
        let text = fixture("session_model_fallback.jsonl");
        let s = &parse_all_str(&text, None)[0];
        assert_eq!(s.model.as_deref(), Some("gemini-3-pro"));
        assert_eq!(s.messages[1].model, None);
        assert_eq!(s.messages[2].model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn owned_checkpoint_transfers_payloads_and_preserves_control_precedence() {
        let mut replay = RecordingReplay::<Value>::default();
        let body = "checkpoint body".repeat(1024);
        let body_ptr = body.as_ptr();
        let summary = "checkpoint metadata".repeat(1024);
        let summary_ptr = summary.as_ptr();
        let mut checkpoint = serde_json::json!({"$set": {
            "messages": [
                {"id":"same", "type":"user", "content":"obsolete"},
                {"id":"same", "type":"user", "content":null},
                {"type":"user", "content":"missing id"}
            ],
            "summary":null
        }});
        checkpoint["$set"]["messages"][1]["content"] = Value::String(body);
        checkpoint["$set"]["summary"] = Value::String(summary);
        replay.ingest(checkpoint);
        assert_eq!(replay.order, ["user\u{1f}same"]);
        assert_eq!(
            replay.msgs["user\u{1f}same"]["content"].as_str().unwrap().as_ptr(),
            body_ptr
        );
        assert_eq!(replay.metadata["summary"].as_str().unwrap().as_ptr(), summary_ptr);

        replay.ingest(serde_json::json!({"$rewindTo":"same", "$set":{"summary":"ignored"}}));
        assert_eq!(replay.metadata["summary"].as_str().unwrap().as_ptr(), summary_ptr);
        replay.ingest(serde_json::json!({"$set":false,"id":"fallback","type":"user","content":"kept"}));
        assert!(replay.msgs.contains_key("user\u{1f}fallback"));
        replay.ingest(serde_json::json!({"$set":{"messages":false,"summary":null}}));
        assert!(replay.order.is_empty());
        assert!(replay.msgs.is_empty());
        assert_eq!(replay.metadata["summary"], Value::Null);
    }

    #[test]
    fn parses_modern_jsonl_with_rewind_and_set() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "11112222-3333-4444-5555-666677778888");
        // $set provided a summary used as title
        assert_eq!(s.title.as_deref(), Some("Listed the project files."));

        // The $rewindTo a3 dropped the first a3, then a3 was re-appended with new text.
        let a3s: Vec<&Message> = s.messages.iter().filter(|m| m.id.as_deref() == Some("a3")).collect();
        assert_eq!(a3s.len(), 1, "rewind should have de-duplicated a3");
        assert!(a3s[0]
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("exactly two files"))));
        // The cut itself is visible: one Branch marker, right before the re-appended a3.
        let branches: Vec<usize> = s
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.kind == MessageKind::Branch)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(branches.len(), 1, "{:?}", kinds(s));
        let b = &s.messages[branches[0]];
        assert_eq!((b.role, b.origin), (Role::System, Origin::Harness));
        assert_eq!(b.id, None, "a control record is not a message record");
        assert_eq!(b.harness_extra(Harness::Gemini).unwrap()["rewind_to"], "a3");
        assert_eq!(b.harness_extra(Harness::Gemini).unwrap()["record_type"], "rewind");
        assert_eq!(s.messages[branches[0] + 1].id.as_deref(), Some("a3"));
        assert!(s.messages.iter().all(|m| m.model.is_none()), "one model throughout");
        assert_nested_extra(s, Harness::Gemini);

        // tool call + result present
        assert!(s.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "list_directory"))));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool
            && m.content
                .iter()
                .any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("main.py")))));
    }

    #[test]
    fn parses_checkpoint_history() {
        let text = fixture("checkpoint.json");
        let s = parse_checkpoint(&text, "checkpoint-my%20tag.json", None).expect("checkpoint");
        assert_eq!(s.id, "checkpoint-my tag");
        // user, model(text+thinking+tooluse), tool(result), model(text)
        let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
        let model_turn = &s.messages[1];
        assert!(model_turn.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(model_turn
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "calculator")));
        // the tool turn carries the functionResponse output
        assert!(s.messages[2]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content == "4")));
    }

    #[test]
    fn parses_legacy_array_checkpoint_via_parse_all() {
        let text = fixture("checkpoint_legacy_array.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(s.title.as_deref(), Some("hi"));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_all_str("not json at all", None).is_empty());
        assert!(parse_all_str("{}", None).is_empty());
        assert!(parse_all_str("[]", None).is_empty());
    }

    // ── on-disk streaming paths ──────────────────────────────────────────────
    //
    // `is_chat_recording` keys off a `chats/` path component, so these write each fixture into a
    // unique temp `chats/` dir, then drive the real `Adapter::parse` (= `stream` → `collect`) and the
    // native `Adapter::stream`, asserting both reproduce the pure `parse_all_str` Session byte-for-
    // byte (Session has no `PartialEq`, so we compare its JSON serialization).

    fn chats_fixture(name: &str) -> (PathBuf, PathBuf) {
        let text = fixture(name);
        let dir = std::env::temp_dir().join(format!(
            "cv-gemini-stream-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let chats = dir.join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(name);
        fs::write(&file, text).unwrap();
        (dir, file)
    }

    fn as_json(s: &Session) -> Value {
        serde_json::to_value(s).unwrap()
    }

    /// Drive the on-disk `parse` and `stream` for a chat-recording fixture and assert both equal the
    /// pure `parse_all_str` Session. `source_path` differs (on-disk paths carry provenance), so we
    /// blank it before comparing.
    fn assert_stream_matches_pure(name: &str) {
        let (dir, file) = chats_fixture(name);
        let id = parse_all_str(&fixture(name), None)
            .into_iter()
            .next()
            .expect("fixture parses to one session")
            .id;
        let r = SessionRef::new(id.clone(), Harness::Gemini, file.clone());
        let adapter = Gemini { roots: vec![] };

        // Reference: the pure parser, with source_path set to the on-disk file for an apples-to-
        // apples comparison.
        let reference = parse_all_str(&fixture(name), Some(file.clone()));
        let reference = reference.into_iter().next().unwrap();

        let via_parse = adapter.parse(&r).expect("on-disk parse");
        assert_eq!(
            as_json(&via_parse),
            as_json(&reference),
            "parse() must stay byte-identical to the pure parser for {name}"
        );

        // Native stream and explicit checked complete preparation retain the existing codec.
        for opts in [ParseOptions::full(), ParseOptions::complete(), checked_complete()] {
            let mut sink = CollectSink::default();
            let mut streamed = adapter.stream(&r, &opts, &mut sink).expect("on-disk stream");
            assert!(streamed.messages.is_empty(), "stream returns empty messages");
            streamed.messages = sink.messages;
            streamed.materialize().unwrap();
            assert_eq!(
                serde_json::to_vec(&streamed).unwrap(),
                serde_json::to_vec(&reference).unwrap(),
                "stream() must reassemble the same Session for {name}, {opts:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn streams_legacy_json_recording_on_disk() {
        // The mmap + RawValue single-document path.
        assert_stream_matches_pure("session_legacy.json");
    }

    #[test]
    fn legacy_stream_preserves_escaped_metadata_and_kind() {
        let (dir, file) = chats_fixture("session_legacy.json");
        let text = r#"{"sessionId":"escaped\"id","startTime":"2026-01-01T00:00:00Z","summary":"line one\nline two","directories":["/tmp/quoted\"path"],"kind":"subagent","messages":[{"type":"user","id":"u","content":"hello"}]}"#;
        fs::write(&file, text).unwrap();
        let mut sink = CollectSink::default();
        // Call this path directly: a fallback would hide the borrowing failure.
        let mut streamed = stream_legacy_json_recording(&file, Harness::Gemini, &ParseOptions::full(), &mut sink)
            .unwrap()
            .expect("escaped strings are a legacy recording");
        streamed.messages = sink.messages;
        assert_eq!(
            as_json(&streamed),
            as_json(&parse_all_str(text, Some(file.clone())).remove(0))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn streams_modern_jsonl_recording_on_disk() {
        // The line-buffered JSONL replay path (with $set / $rewindTo).
        assert_stream_matches_pure("session_modern.jsonl");
    }

    #[test]
    fn lazy_legacy_content_resolves_identically() {
        let (dir, file) = chats_fixture("session_legacy.json");
        let giant = "giant output \n\t\"\\🌙".repeat(2000);
        let input = serde_json::json!({
            "sessionId": "lazy", "projectHash": "x", "kind": "subagent", "summary": "escaped\nsummary",
            "messages": [
                {"type":"user", "id":"u", "content":[{"text":giant}, {"text":format!("<hook_context>{giant}")}]},
                {"type":"gemini", "id":"a", "content":[{"text":giant,"thought":true},{"text":giant}],
                 "toolCalls":[{"id":"c","name":"shell","args":{"command":"ls"},"status":"success",
                     "result":[{"functionResponse":{"response":{"output":giant}}}]}]},
                {"type":"user","id":"t","content":[{"functionResponse":{"id":"c","name":"shell","response":{"output":giant}}}]},
                {"type":"warning","id":"w","content":giant},
                {"type":"user","id":"white","content":format!("{}<session_context>hidden marker", " ".repeat(9000))}
            ]
        });
        let text = serde_json::to_string(&input).unwrap();
        fs::write(&file, &text).unwrap();
        let mut sink = CollectSink::default();
        let mut s = Gemini { roots: vec![] }
            .stream(
                &SessionRef::new("lazy", Harness::Gemini, file.clone()),
                &ParseOptions::lazy(),
                &mut sink,
            )
            .unwrap();
        s.messages = sink.messages;
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.is_span())));
        s.materialize().unwrap();
        assert_eq!(
            as_json(&s),
            as_json(&parse_all_str(&text, Some(file.clone())).remove(0))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn raw_jsonl_replay_and_lazy_fields_match_pure() {
        let (dir, file) = chats_fixture("session_modern.jsonl");
        let giant = "escaped\n\t\"\\🌙".repeat(1200);
        let lines = [
            serde_json::json!({"sessionId":"replay","projectHash":"x","directories":["/tmp/q\"uote"]}),
            serde_json::json!({"type":"user","id":"u","content":"superseded"}),
            serde_json::json!({"type":"gemini","id":"same","content":giant}),
            serde_json::json!({"type":"user","id":"same","content":"same id different kind"}),
            serde_json::json!({"$set":{"summary":"new\nsummary","messages":[
                {"type":"user","id":"u","content":giant},
                {"type":"gemini","id":"same","content":giant},
                {"type":"user","id":"same","content":"paired"}]}}),
            serde_json::json!({"type":"gemini","id":"same","content":"replacement","toolCalls":[
                {"id":"a","name":"shell","status":"success","result":{"text":"json object text"}},
                {"id":"b","name":"shell","status":"success","result":[{"text":"first"},{"text":giant}]},
                {"id":"c","name":"shell","status":"success","resultDisplay":giant}]}),
            serde_json::json!({"$rewindTo":"same"}),
            serde_json::json!({"type":"user","id":"after","content":[{"functionCall":null,"text":giant}]}),
        ];
        for end in [6, lines.len()] {
            let text = lines[..end].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
            fs::write(&file, &text).unwrap();
            let reference = parse_all_str(&text, Some(file.clone())).remove(0);
            for opts in [
                ParseOptions::full(),
                ParseOptions::lazy(),
                ParseOptions::complete(),
                checked_complete(),
            ] {
                let mut sink = CollectSink::default();
                let mut s = Gemini { roots: vec![] }
                    .stream(
                        &SessionRef::new("replay", Harness::Gemini, file.clone()),
                        &opts,
                        &mut sink,
                    )
                    .unwrap();
                s.messages = sink.messages;
                s.materialize().unwrap();
                assert_eq!(as_json(&s), as_json(&reference));
            }
            let refs = scan_session_file(&file, Harness::Gemini);
            assert_eq!(refs.len(), 1);
            assert_eq!(
                refs[0].message_count,
                reference
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                    .count()
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn raw_replay_rejects_invalid_values_before_navigation() {
        let (dir, file) = chats_fixture("session_modern.jsonl");
        let deep = format!("{}0{}", "[".repeat(127), "]".repeat(127));
        for unknown in ["1e400", r#""\ud800""#, r#""\udc00""#, &deep] {
            let text = format!(
                "{{\"sessionId\":\"validation\",\"projectHash\":\"x\"}}\n\
                 {{\"type\":\"user\",\"id\":\"u\",\"content\":\"keep\"}}\n\
                 {{\"type\":\"gemini\",\"id\":\"a\",\"content\":\"keep suffix\"}}\n\
                 {{\"type\":\"user\",\"id\":\"u\",\"content\":\"poison\",\"unknown\":{unknown},\"unknown\":0}}\n\
                 {{\"$rewindTo\":\"u\",\"unknown\":{unknown}}}\n\
                 {{\"$set\":{{\"messages\":[]}},\"unknown\":{unknown}}}"
            );
            fs::write(&file, &text).unwrap();
            let reference = parse_all_str(&text, Some(file.clone())).remove(0);
            assert_eq!(reference.messages.len(), 2);
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::full(),
                ParseOptions::complete(),
                checked_complete(),
            ] {
                let mut sink = CollectSink::default();
                let mut streamed = stream_jsonl_recording(&file, Harness::Gemini, &opts, &mut sink)
                    .unwrap()
                    .unwrap();
                streamed.messages = sink.messages;
                streamed.materialize().unwrap();
                assert_eq!(as_json(&streamed), as_json(&reference), "invalid field {unknown}");
            }
        }
        // Legacy streaming has always rejected individual invalid records, independently of
        // the whole-file wrapper's depth. The lazy path must match that full native path.
        let text = format!(
            r#"{{"sessionId":"validation","messages":[{{"type":"user","id":"u","content":"keep"}},{{"type":"user","id":"bad","content":"{}\ud800"}}]}}"#,
            "x".repeat(9000)
        );
        fs::write(&file, text).unwrap();
        let mut sessions = Vec::new();
        for opts in [
            ParseOptions::lazy(),
            ParseOptions::full(),
            ParseOptions::complete(),
            checked_complete(),
        ] {
            let mut sink = CollectSink::default();
            let mut streamed = stream_legacy_json_recording(&file, Harness::Gemini, &opts, &mut sink)
                .unwrap()
                .unwrap();
            streamed.messages = sink.messages;
            streamed.materialize().unwrap();
            assert_eq!(streamed.messages.len(), 1);
            sessions.push(as_json(&streamed));
        }
        assert!(sessions.windows(2).all(|s| s[0] == s[1]));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lazy_recording_matches_every_recording_fixture() {
        for name in ["session_legacy.json", "session_modern.jsonl"] {
            let (dir, file) = chats_fixture(name);
            let text = fs::read_to_string(&file).unwrap();
            let reference = parse_all_str(&text, Some(file.clone())).remove(0);
            let mut sink = CollectSink::default();
            let mut s = Gemini { roots: vec![] }
                .stream(
                    &SessionRef::new(&reference.id, Harness::Gemini, file.clone()),
                    &ParseOptions::lazy(),
                    &mut sink,
                )
                .unwrap();
            s.messages = sink.messages;
            s.materialize().unwrap();
            assert_eq!(as_json(&s), as_json(&reference));
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn stream_honors_stop_early() {
        // A sink returning Stop after the first message halts the legacy-JSON streaming path without
        // emitting the rest.
        let (dir, file) = chats_fixture("session_legacy.json");
        let r = SessionRef::new("9aeb2942-7c46-47b7-aded-13772d4d4e63", Harness::Gemini, file);
        let adapter = Gemini { roots: vec![] };
        let mut seen = 0usize;
        let mut sink = |_m: Message| {
            seen += 1;
            Flow::Stop
        };
        let _ = adapter.stream(&r, &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A fake gemini-cli runtime root (`<runtime>/tmp/<id>/chats/<recording>` + registry files).
    fn runtime_fixture(tag: &str, project_id: &str, recording: &str) -> (PathBuf, PathBuf) {
        let runtime = std::env::temp_dir().join(format!(
            "cv-gemini-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let chats = runtime.join("tmp").join(project_id).join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join(recording);
        fs::write(&file, fixture(recording)).unwrap();
        (runtime, file)
    }

    #[test]
    fn discovers_both_runtime_roots_and_dedupes_by_id() {
        // `~/.gemini/tmp` and the Seatbelt root `~/.cache/.gemini/tmp` are both scanned; the same
        // session id under both roots is listed once.
        let (rt_a, _) = runtime_fixture("root-a", "proj", "session_modern.jsonl");
        let (rt_b, _) = runtime_fixture("root-b", "proj", "session_modern.jsonl");
        let (rt_c, _) = runtime_fixture("root-c", "proj", "session_legacy.json");
        let adapter = Gemini {
            roots: vec![rt_a.join("tmp"), rt_b.join("tmp"), rt_c.join("tmp")],
        };
        let refs = adapter.discover().unwrap();
        let mut ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "11112222-3333-4444-5555-666677778888",
                "9aeb2942-7c46-47b7-aded-13772d4d4e63"
            ],
            "two distinct sessions across three roots; the duplicate collapsed"
        );
        assert_eq!(adapter.roots().into_iter().next(), Some(rt_a.join("tmp")));
        for d in [rt_a, rt_b, rt_c] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    /// A runtime root that itself lives under a directory named `tmp` — `/tmp/...` on Linux, a
    /// Seatbelt root, a CI temp dir — must still resolve. Reading the components left-to-right
    /// matched that outer `tmp`, so `<runtime>` became `/` and every cwd came back `None`.
    #[test]
    fn runtime_root_under_a_tmp_directory_still_resolves_cwd() {
        let outer = std::env::temp_dir().join(format!("cv-gemini-outer-{}", uuid::Uuid::now_v7()));
        let runtime = outer.join("tmp").join("gemini-runtime");
        let chats = runtime.join("tmp").join("proj-id").join("chats");
        fs::create_dir_all(&chats).unwrap();
        let file = chats.join("session_modern.jsonl");
        fs::write(&file, fixture("session_modern.jsonl")).unwrap();
        fs::write(
            runtime.join("projects.json"),
            r#"{"projects": {"/Users/u/work/proj": "proj-id"}}"#,
        )
        .unwrap();

        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(
            refs[0].cwd.as_deref(),
            Some(Path::new("/Users/u/work/proj")),
            "the innermost `tmp` is the runtime's own"
        );
        let _ = fs::remove_dir_all(&outer);
    }

    #[test]
    fn cwd_comes_from_the_project_registry() {
        // `projects.json` maps path → short id; `history/<id>/.project_root` is the fallback marker.
        let (runtime, file) = runtime_fixture("registry", "claurdvoyant", "session_modern.jsonl");
        fs::write(
            runtime.join("projects.json"),
            r#"{"projects": {"/Users/u/pug/claurdvoyant": "claurdvoyant"}}"#,
        )
        .unwrap();
        let (runtime2, file2) = runtime_fixture("marker", "gemtest", "session_legacy.json");
        fs::create_dir_all(runtime2.join("history").join("gemtest")).unwrap();
        fs::write(
            runtime2.join("history").join("gemtest").join(".project_root"),
            "/Users/u/cvrt8/gemtest\n",
        )
        .unwrap();
        // hash-named dirs (pre-registry) have no mapping → None, as before
        let (runtime3, file3) = runtime_fixture("hash", &"ab".repeat(32), "session_modern.jsonl");

        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        let refs2 = scan_session_file(&file2, Harness::Gemini);
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/cvrt8/gemtest")));
        // session_legacy.json parses to user, assistant, tool, assistant, info-notice: three turns.
        assert_eq!(refs2[0].message_count, 3, "user + assistant turns, not every message");
        let refs3 = scan_session_file(&file3, Harness::Gemini);
        assert_eq!(refs3[0].cwd, None);

        // the streamed session carries it too (the sink's `meta` sees the cwd)
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/pug/claurdvoyant")));
        // a record's own `directories[]` still wins (unchanged behaviour): covered by
        // `parses_legacy_chat_recording` fixtures carrying no registry at all.
        for d in [runtime, runtime2, runtime3] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn registry_files_in_their_real_0_46_shapes_recover_cwd() {
        // Fixtures mirror what gemini-cli 0.46.0 wrote for a fresh project dir on 2026-09-19:
        // `projects.json` pretty-printed with a nested `projects` map, and an identical bare-path
        // `.project_root` (no trailing newline) in BOTH `tmp/<id>/` and `history/<id>/`.
        let (runtime, file) = runtime_fixture("real-shapes", "gemtest-proj", "session_modern.jsonl");
        fs::write(runtime.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        fs::create_dir_all(runtime.join("history").join("gemtest-proj")).unwrap();
        fs::write(
            runtime.join("history").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));

        // The marker beside the chats is enough on its own (a runtime root with no registry file
        // and no history dir — e.g. one the CLI is still populating).
        let (runtime2, file2) = runtime_fixture("tmp-marker-only", "solo", "session_legacy.json");
        fs::write(
            runtime2.join("tmp").join("solo").join(".project_root"),
            "/Users/u/work/solo",
        )
        .unwrap();
        let refs2 = scan_session_file(&file2, Harness::Gemini);
        assert_eq!(refs2[0].cwd.as_deref(), Some(Path::new("/Users/u/work/solo")));
        for d in [runtime, runtime2] {
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn real_0_46_recording_keeps_the_context_preamble_but_never_titles_from_it() {
        // Written by gemini-cli 0.46.0 on 2026-09-19 (paths neutralized): a metadata line, then ONE
        // `$set` carrying the whole `messages` array, whose only user turn is the injected
        // `<session_context>` preamble. gemini-cli ignores such content when rebuilding history
        // (`isIgnoredUserContent`), so it must not become the title; the turn itself stays.
        let (runtime, file) = runtime_fixture("real-0-46", "gemtest-proj", "session_0_46_session_context.jsonl");
        fs::write(runtime.join("projects.json"), fixture("registry/projects.json")).unwrap();
        fs::write(
            runtime.join("tmp").join("gemtest-proj").join(".project_root"),
            fixture("registry/project_root"),
        )
        .unwrap();
        let refs = scan_session_file(&file, Harness::Gemini);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "ad7102ea-0b4f-4822-94f7-f3bbf67f1614");
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/Users/u/scratch/gemtest-proj")));
        assert_eq!(refs[0].title, None, "the <session_context> preamble is not a prompt");

        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(s.title, None);
        assert_eq!(sink.messages.len(), 1, "the preamble turn is still in the transcript");
        let m = &sink.messages[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::InjectedContext, Origin::Harness),
            "gemini-cli fed the model that block; nobody typed it"
        );
        assert!(m.text().unwrap().starts_with("<session_context>"));
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "main");
        assert!(s.lineage.is_empty(), "a main recording has no parent");
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn subagent_recording_points_at_its_parent() {
        // A sub-agent recording lives under `chats/<parentId>/<id>.jsonl` with `kind: "subagent"`
        // (`chatRecordingService.ts`); its parent is the recording one level up.
        let (runtime, _) = runtime_fixture("subagent", "proj", "session_modern.jsonl");
        let parent_id = "11112222-3333-4444-5555-666677778888";
        let child_dir = runtime.join("tmp").join("proj").join("chats").join(parent_id);
        fs::create_dir_all(&child_dir).unwrap();
        let child = child_dir.join("aaaa0000-0000-0000-0000-000000000001.jsonl");
        let text = fixture("session_modern.jsonl")
            .replacen(
                "11112222-3333-4444-5555-666677778888",
                "aaaa0000-0000-0000-0000-000000000001",
                1,
            )
            .replacen(r#""kind":"main""#, r#""kind":"subagent""#, 1);
        fs::write(&child, text).unwrap();

        let refs = scan_session_file(&child, Harness::Gemini);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "aaaa0000-0000-0000-0000-000000000001");
        let adapter = Gemini { roots: vec![] };
        let mut sink = CollectSink::default();
        let s = adapter.stream(&refs[0], &ParseOptions::full(), &mut sink).unwrap();
        assert_eq!(
            s.lineage,
            Lineage {
                parent: Some(parent_id.into()),
                ..Default::default()
            }
        );
        assert_eq!(s.harness_extra(Harness::Gemini).unwrap()["kind"], "subagent");
        // the parent's own recording, one level up, has no parent
        let parent_refs = scan_session_file(&runtime.join("tmp/proj/chats/session_modern.jsonl"), Harness::Gemini);
        let mut sink = CollectSink::default();
        let p = adapter
            .stream(&parent_refs[0], &ParseOptions::full(), &mut sink)
            .unwrap();
        assert!(p.lineage.is_empty());
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn rewrite_siblings_are_not_sessions() {
        // `rewriteConversationFile` (chatRecordingService.ts:575-640) leaves `<file>.unreadable-<ms>`
        // and writes through `<file>.tmp-<pid>`; neither is a recording.
        let (runtime, file) = runtime_fixture("siblings", "proj", "session_modern.jsonl");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(file.with_extension("jsonl.unreadable-1700000000000"), &text).unwrap();
        fs::write(file.with_extension("jsonl.tmp-4242"), &text).unwrap();
        let adapter = Gemini {
            roots: vec![runtime.join("tmp")],
        };
        let refs = adapter.discover().unwrap();
        assert_eq!(
            refs.len(),
            1,
            "only the real recording is discovered: {:?}",
            refs.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        assert!(refs[0].path.ends_with("session_modern.jsonl"));
        let _ = fs::remove_dir_all(&runtime);
    }

    /// The 0.63.0 resume shape: a `$set.messages` re-sync from the API history carries the
    /// assistant turn's thoughts and calls as parts AND as `thoughts[]`/`toolCalls[]`, a `user`
    /// tool-response turn sharing the assistant's id, and the original tool-response record.
    #[test]
    fn resynced_history_neither_doubles_nor_overwrites_turns() {
        let call = r#"{"id":"c1","name":"read_file","args":{"file_path":"a.txt"},"status":"success","timestamp":"2026-10-11T00:00:02.000Z","result":[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"alpha"}}}]}"#;
        let fr = r#"[{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"alpha"}}}]"#;
        let gem = format!(
            r#"{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"gemini","content":[{{"text":"thinking","thought":true}},{{"functionCall":{{"id":"c1","name":"read_file","args":{{}}}}}}],"thoughts":[{{"subject":"","description":"thinking","timestamp":"2026-10-11T00:00:02.000Z"}}],"toolCalls":[{call}],"model":"gemini-2.5-flash"}}"#
        );
        let text = [
            r#"{"sessionId":"s1","projectHash":"h","startTime":"2026-10-11T00:00:00.000Z","lastUpdated":"2026-10-11T00:00:00.000Z","kind":"main"}"#.to_string(),
            r#"{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{"text":"read a.txt"}]}"#.to_string(),
            gem.clone(),
            format!(r#"{{"id":"t1","timestamp":"2026-10-11T00:00:03.000Z","type":"user","content":{fr}}}"#),
            r#"{"id":"g2","timestamp":"2026-10-11T00:00:04.000Z","type":"gemini","content":"alpha","model":"gemini-2.5-flash"}"#.to_string(),
            format!(
                r#"{{"$set":{{"messages":[{{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{{"text":"read a.txt"}}]}},{gem},{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"user","content":{fr}}},{{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"user","content":{fr}}},{{"id":"t1","timestamp":"2026-10-11T00:00:03.000Z","type":"user","content":{fr}}},{{"id":"g2","timestamp":"2026-10-11T00:00:04.000Z","type":"gemini","content":[{{"text":"alpha"}}],"model":"gemini-2.5-flash"}}]}}}}"#
            ),
        ]
        .join("\n");
        let s = &parse_all_str(&text, None)[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        let reply = &s.messages[1];
        let n = |f: fn(&Block) -> bool| reply.content.iter().filter(|b| f(b)).count();
        assert_eq!(
            n(|b| matches!(b, Block::Thinking { .. })),
            1,
            "thought part and thoughts[] are one"
        );
        assert_eq!(
            n(|b| matches!(b, Block::ToolUse { .. })),
            1,
            "functionCall part and toolCalls[] are one"
        );
        assert_eq!(s.messages[2].content.len(), 1, "one result, not three");
    }

    #[test]
    fn selected_content_dispatch_preserves_tool_response_presence() {
        let root = std::env::temp_dir().join(format!("cv-gemini-dispatch-{}", uuid::Uuid::new_v4()));
        // Native recording selection keys off the chats directory, not merely JSON contents.
        let dir = root.join("chats");
        fs::create_dir_all(&dir).unwrap();
        let adapter = Gemini { roots: vec![] };
        let response =
            serde_json::json!({"functionResponse":{"id":"call","name":"read","response":{"output":"result"}}});
        let cases = [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!([response]),
            serde_json::json!([response, {"text":"mixed prompt"}]),
            serde_json::json!(["", response]),
            serde_json::json!([{ "functionResponse":null, "text":"presence is authoritative" }]),
            serde_json::json!([{ "functionResponse":false, "functionCall":{"id":"call","name":"read","args":{}} }]),
            response.clone(),
        ];
        for modern in [false, true] {
            let path = dir.join(if modern { "session.jsonl" } else { "session.json" });
            let reference = SessionRef::new("source", Harness::Gemini, path.clone());
            for content in &cases {
                let record = serde_json::json!({"id":"u", "type":"user", "content":content});
                let text = if modern {
                    format!("{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\n{record}\n")
                } else {
                    serde_json::json!({"sessionId":"source", "messages":[record]}).to_string()
                };
                fs::write(&path, &text).unwrap();
                let expected = parse_all_str(&text, Some(path.clone())).remove(0);
                for opts in [ParseOptions::lazy(), ParseOptions::lazy_extra(), checked_complete()] {
                    let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
                    actual.materialize().unwrap();
                    assert_eq!(as_json(&actual), as_json(&expected), "content={content}");
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_display_content_matches_owned_shapes_and_demand_modes() {
        let root = std::env::temp_dir().join(format!("cv-gemini-display-{}", uuid::Uuid::new_v4()));
        let dir = root.join("chats");
        fs::create_dir_all(&dir).unwrap();
        let body = "display é\n\"quoted\" ".repeat(512);
        let displays = [
            Value::String(body.clone()),
            serde_json::json!({"text":body,"ignored":"not display"}),
            serde_json::json!(["prefix", {"text":body}, null, {"text":false}, ["ignored"], "suffix"]),
            serde_json::json!(["", {"text":""}]),
            serde_json::json!({"text":false}),
            Value::Null,
        ];
        let adapter = Gemini { roots: vec![] };
        for modern in [false, true] {
            let path = dir.join(if modern { "session.jsonl" } else { "session.json" });
            let reference = SessionRef::new("source", Harness::Gemini, path.clone());
            for display in &displays {
                let records = serde_json::json!([
                    {"id":"u", "type":"user", "content":"ordinary prompt", "displayContent":display},
                    {"id":"a", "type":"gemini", "content":"reply", "displayContent":display},
                    {"id":"empty", "type":"user", "content":"", "displayContent":display}
                ]);
                let text = if modern {
                    format!(
                        "{{\"sessionId\":\"source\",\"projectHash\":\"x\"}}\n{}\n",
                        records
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                } else {
                    serde_json::json!({"sessionId":"source", "messages":records}).to_string()
                };
                fs::write(&path, &text).unwrap();
                let expected = parse_all_str(&text, Some(path.clone())).remove(0);
                for opts in [
                    ParseOptions::lazy(),
                    ParseOptions::bulk(),
                    ParseOptions::full(),
                    ParseOptions::lazy_extra(),
                    ParseOptions::complete(),
                    checked_complete(),
                    ParseOptions::facts(),
                ] {
                    let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
                    actual.materialize().unwrap();
                    assert_eq!(as_json(&actual), as_json(&expected));
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn subagent_call_hook_context_and_display_text() {
        let text = [
            r#"{"sessionId":"s2","projectHash":"h","startTime":"2026-10-11T00:00:00.000Z","lastUpdated":"2026-10-11T00:00:00.000Z","kind":"main"}"#,
            r#"{"id":"u1","timestamp":"2026-10-11T00:00:01.000Z","type":"user","content":[{"text":"Reply with exactly: hi"},{"text":"<hook_context>be brief</hook_context>"}],"displayContent":[{"text":"/hello"}]}"#,
            r#"{"id":"g1","timestamp":"2026-10-11T00:00:02.000Z","type":"gemini","content":"","toolCalls":[{"id":"a1","name":"invoke_agent","args":{"agent_name":"codebase_investigator"},"status":"success","agentId":"child-1","timestamp":"2026-10-11T00:00:03.000Z","result":[{"functionResponse":{"id":"a1","name":"invoke_agent","response":{"output":"done"}}}]}]}"#,
        ]
        .join("\n");
        let s = &parse_all_str(&text, None)[0];
        assert_eq!(
            kinds(s),
            vec![
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::System, MessageKind::InjectedContext, Origin::Hook),
                (Role::Assistant, MessageKind::SubagentSpawn, Origin::Model),
                (Role::Tool, MessageKind::SubagentReturn, Origin::Subagent),
            ]
        );
        let ex = s.messages[0].harness_extra(Harness::Gemini).unwrap();
        assert_eq!(ex["display_text"], "/hello");
        assert_eq!(
            s.messages[2].harness_extra(Harness::Gemini).unwrap()["subagent_ids"][0],
            "child-1"
        );
        assert_eq!(s.title.as_deref(), Some("Reply with exactly: hi"));
    }

    #[test]
    fn a_startup_stub_never_shadows_the_session_it_shares_an_id_with() {
        let mk = |n: usize, mins: i64| {
            let mut r = SessionRef::new("same", Harness::Gemini, format!("/x/{n}.jsonl"));
            r.message_count = n;
            r.updated_at = Some(chrono::DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(mins));
            r
        };
        let kept = dedupe_by_id(vec![mk(4, 1), mk(0, 2)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message_count, 4, "the newer stub lost");
        assert_eq!(
            dedupe_by_id(vec![mk(2, 1), mk(3, 2)])[0].message_count,
            3,
            "newer real copy wins"
        );
    }

    #[test]
    fn a_merged_checkpoint_turn_splits_the_preamble_from_the_prompt() {
        let text = r#"{"history":[{"role":"user","parts":[{"text":"<session_context>\nctx\n</session_context>"},{"text":"Say saved."}]},{"role":"model","parts":[{"text":"saved"}]}]}"#;
        let s = parse_checkpoint(text, "checkpoint-x.json", None).unwrap();
        assert_eq!(
            kinds(&s),
            vec![
                (Role::System, MessageKind::InjectedContext, Origin::Harness),
                (Role::User, MessageKind::Prompt, Origin::Human),
                (Role::Assistant, MessageKind::Reply, Origin::Model),
            ]
        );
        assert_eq!(s.title.as_deref(), Some("Say saved."));
    }
}
