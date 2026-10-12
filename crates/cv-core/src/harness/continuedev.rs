//! Continue adapter — `~/.continue/sessions/` (continue.dev VS Code / JetBrains extension).
//!
//! ## Storage layout
//! Root is `$CONTINUE_GLOBAL_DIR/sessions` or `~/.continue/sessions`. It holds:
//! - **`sessions.json`** — the index: a JSON array of
//!   `{sessionId, title, dateCreated, workspaceDirectory}`. `dateCreated` is a string holding a
//!   millisecond epoch (e.g. `"1700000000000"`) on older builds, or an ISO-8601 timestamp on newer
//!   ones — we accept both. This gives id + title + cwd + created for a listing without opening each
//!   transcript.
//! - **`<sessionId>.json`** — one full session per file:
//!   `{sessionId, title, workspaceDirectory, history: [Item, ...]}`.
//!
//! ## `history[]` item (a `ChatHistoryItem` in Continue)
//! `{message: Message, contextItems: [ContextItem], editorState?, promptLogs?, ...}`. We map one
//! item to one IR `Message`.
//!
//! ### `message` (Continue uses OpenAI-ish `ChatMessage`s)
//! - `role`: `"user" | "assistant" | "system" | "tool"`.
//! - `content`: a **string** OR an array of parts (`{type:"text",text}` /
//!   `{type:"imageUrl",imageUrl:{url}}`). Both shapes are handled.
//! - `toolCalls` (assistant, newer Continue): `[{id, type:"function", function:{name, arguments}}]`.
//!   `arguments` is a JSON-encoded *string* (streamed, so possibly partial) → `Block::ToolUse`.
//! - `toolCallId` (role `tool`): the id of the call this result answers → `Block::ToolResult`.
//! - `usage` (assistant, cn CLI 1.5): OpenAI usage names plus `model` and `cost_cents` → [`Usage`].
//!
//! ### `toolCallStates[]` (assistant items, cn CLI 1.5)
//! The cn CLI writes no `role: "tool"` items; each assistant item carries its calls' outcomes as
//! `{toolCallId, toolCall, status, parsedArgs, output: [{content, name, description}]}`, emitted
//! right after the assistant turn as a Tool message (`status: "errored"` → `is_error`).
//!
//! ### `contextItems[]` (attached files / snippets / docs)
//! `{name, description, content, uri?: {type, value}}`. Best-effort → `Block::File` (path/source from
//! `uri.value` or `description`), appended to the owning message so search can see them.
//!
//! ## Tolerance
//! Missing index → glob `*.json` (minus `sessions.json`). Missing fields → skip. `content` as
//! string or array both work. `dateCreated` ms-epoch or ISO both work. Never panics.

use super::store::DiscardValue;
use super::wire::{openai, text_parts};
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Continue {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
}

impl Continue {
    pub fn detect() -> Self {
        let root = std::env::var_os("CONTINUE_GLOBAL_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".continue")));
        let sessions = root.map(|r| r.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }

    /// Continue's store under an explicit global dir (what `CONTINUE_GLOBAL_DIR` names).
    pub fn with_root(root: &Path) -> Self {
        let sessions = Some(root.join("sessions")).filter(|p| p.exists());
        Continue { sessions }
    }
}

impl Adapter for Continue {
    fn harness(&self) -> Harness {
        Harness::Continue
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.sessions.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };

        // Index entries from sessions.json, if present.
        let index = read_index(&sessions.join("sessions.json"));

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Prefer the index, but only keep entries whose `<id>.json` actually exists.
        for meta in &index {
            let path = sessions.join(format!("{}.json", meta.session_id));
            if !path.exists() {
                continue;
            }
            seen.insert(meta.session_id.clone());
            out.push(ref_from(
                meta,
                &path,
                scan_session(&path).map_or(0, |s| s.message_count),
            ));
        }

        // Fold in any `<id>.json` on disk that the index didn't mention (tolerant of a missing or
        // stale index).
        if let Ok(rd) = fs::read_dir(sessions) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name == "sessions.json" || !name.ends_with(".json") {
                    continue;
                }
                let id = name.trim_end_matches(".json").to_string();
                if seen.contains(&id) {
                    continue;
                }
                let scan = scan_session(&p).unwrap_or_default();
                let meta = IndexMeta {
                    session_id: id,
                    title: scan.title,
                    cwd: scan.cwd,
                    created_at: None,
                };
                out.push(ref_from(&meta, &p, scan.message_count));
            }
        }

        Ok(out)
    }

    /// Native streaming parse. A Continue session is a single JSON document
    /// (`{title, workspaceDirectory, history:[Item,…]}`), so instead of building the whole-document
    /// `Value` (which owns every `history` item body — gigabytes for a large session), we
    /// borrow-deserialize only the document *structure*: the small session-level strings by value,
    /// and `history` as a `Vec<&RawValue>` slicing the mapped/owned bytes. Each item is then turned
    /// into one small `Value` ([`history_item_to_message`]), emitted to `sink`, and dropped before
    /// the next — so peak memory is O(largest item) + the mmap (reclaimable) + the slice vec.
    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Continue, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            let mut s = Session {
                cwd: r.cwd.clone(),
                title: r.title.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at.or_else(|| mtime(&r.path)),
                source_path: Some(r.path.clone()),
                ..Session::new(r.id.clone(), Harness::Continue)
            };

            // Map (or read) the file's raw bytes; the borrowed `Doc` slices into them.
            let Some(bytes) = read_bytes(&r.path) else {
                sink.meta(&s);
                return Ok(s);
            };
            let Ok(doc) = serde_json::from_slice::<Doc>(bytes.as_ref()) else {
                sink.meta(&s);
                return Ok(s);
            };

            // Backfill session-level fields from the file itself (index may be sparse/absent).
            if s.title.is_none() {
                s.title = doc
                    .title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, 80));
            }
            if s.cwd.is_none() {
                s.cwd = doc
                    .workspace_directory
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path);
            }

            // Metadata is fully known now (model is only ever backfilled per-item below, and the bridge
            // sinks read it from the returned Session); hand the header to the sink before the body.
            sink.meta(&s);

            // Tool calls whose results came from an item's `toolCallStates` (the cn CLI's shape), so a
            // later `role: "tool"` item answering the same call is not emitted twice.
            let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
            for item in doc.history {
                // ONE small Value per item — parsed, emitted, dropped before the next.
                let Ok(v) = serde_json::from_str::<Value>(item.get()) else {
                    continue;
                };
                let reply_id = v.pointer("/message/toolCallId").and_then(Value::as_str);
                if reply_id.is_some_and(|id| answered.contains(id)) {
                    continue;
                }
                let results = tool_states_message(&v);
                if let Some(m) = history_item_to_message(&v, &mut s.model) {
                    // A `system` history item is the system prompt — the session-level fact.
                    if s.system_prompt.is_none() && m.kind == MessageKind::SystemPrompt {
                        if let Some(t) = m.text() {
                            if !t.trim().is_empty() {
                                s.system_prompt = Some(t);
                            }
                        }
                    }
                    if sink.message(m) == Flow::Stop {
                        break;
                    }
                }
                if let Some(r) = results {
                    for b in &r.content {
                        if let Block::ToolResult { tool_use_id, .. } = b {
                            answered.insert(tool_use_id.clone());
                        }
                    }
                    if sink.message(r) == Flow::Stop {
                        break;
                    }
                }
            }

            Ok(s)
        })
    }
}

// ---------------------------------------------------------------------------
// Streaming document structure (borrows the mapped/owned bytes)
// ---------------------------------------------------------------------------

/// The document *structure* of a Continue session, borrowed from the source bytes: the small
/// session-level strings by value, and `history` as raw item slices (NOT materialized into owned
/// `Value`s). Borrowing avoids building the whole-document `Value` for [`Continue::stream`].
#[derive(serde::Deserialize)]
struct Doc<'a> {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "workspaceDirectory")]
    workspace_directory: Option<String>,
    #[serde(default, borrow)]
    history: Vec<&'a RawValue>,
}

/// Owns either an mmap of `path` or its bytes read into a `Vec`, exposing `.as_ref()` as `&[u8]`.
/// `None` on any read/map error (callers treat it as an empty/unparseable session, like the old
/// `read_to_string`/`from_str` did).
enum Bytes {
    #[cfg(feature = "mmap")]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Bytes::Mapped(m) => m,
            Bytes::Owned(v) => v,
        }
    }
}

/// Memory-map `path` (default `mmap` feature) so a large session's bytes are paged in on demand and
/// reclaimable; fall back to reading the whole file into a `Vec` when the feature is off or the map
/// fails (e.g. a zero-length file, where mmap is invalid).
fn read_bytes(path: &Path) -> Option<Bytes> {
    #[cfg(feature = "mmap")]
    {
        if let Ok(file) = fs::File::open(path) {
            // SAFETY: cv reads its own session store; a concurrent truncation by another process
            // could in theory cause a SIGBUS, the standard caveat for all mmap'd file reads.
            if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
                return Some(Bytes::Mapped(map));
            }
        }
    }
    fs::read(path).ok().map(Bytes::Owned)
}

// ---------------------------------------------------------------------------
// Emit (IR -> Continue native layout)
// ---------------------------------------------------------------------------

/// Emit `session` into Continue's native layout under `out_dir` (the `sessions` directory):
///   `out_dir/<sessionId>.json`     — the transcript `{sessionId, title, workspaceDirectory, history}`
///   `out_dir/sessions.json`        — the index array (merged: our entry added/updated)
///
/// This is the faithful inverse of [`Continue::parse`] / [`Continue::discover`]:
/// - Each IR [`Message`] becomes one `history` item `{message, contextItems}`.
/// - `message` is the OpenAI-ish `ChatMessage` (`role`, string-or-array `content`, assistant
///   `toolCalls[]` with `arguments` as a JSON-encoded *string*, `role:"tool"` + `toolCallId`).
/// - `Block::File`/`Block::Image` on a turn become `contextItems[]` (so the parser's
///   `context_item_to_block` recovers them as `File` blocks); inline images also go in `content`.
///
/// The parser backfills `cwd`/`title` from the transcript itself, so writing the index is for
/// `discover()` fidelity, not correctness; we still write it.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<super::EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    use anyhow::Context;

    let new_id = opts.new_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ── transcript: <sessionId>.json ──
    let title = session
        .title
        .clone()
        .or_else(|| session.first_user_text())
        .map(|t| crate::ir::truncate(&t, 80));

    let mut history: Vec<Value> = Vec::new();
    for msg in &session.messages {
        history.push(history_item(msg, session.model.as_deref())?);
    }

    let mut transcript = Map::new();
    transcript.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        transcript.insert("title".into(), json!(t));
    }
    if let Some(c) = &cwd_str {
        transcript.insert("workspaceDirectory".into(), json!(c));
    }
    transcript.insert("history".into(), Value::Array(history));

    let file_path = out_dir.join(format!("{new_id}.json"));
    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(transcript))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    // ── index: sessions.json (merge-or-create) ──
    let index_path = out_dir.join("sessions.json");
    let mut entries: Vec<Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    // Drop any stale entry for this id, then append a fresh one.
    entries.retain(|e| e.get("sessionId").and_then(Value::as_str) != Some(new_id.as_str()));
    let created = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(chrono::Utc::now);
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = &title {
        entry.insert("title".into(), json!(t));
    }
    // `dateCreated` is a string holding millisecond epoch (the older-build shape, which the parser
    // accepts alongside ISO).
    entry.insert("dateCreated".into(), json!(created.timestamp_millis().to_string()));
    if let Some(c) = &cwd_str {
        entry.insert("workspaceDirectory".into(), json!(c));
    }
    entries.push(Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Array(entries))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(super::EmitResult {
        path: file_path,
        new_id,
        resume_hint: None,
    })
}

/// Build one `history` item (`{message, contextItems}`) from an IR message.
fn history_item(msg: &Message, session_model: Option<&str>) -> Result<Value> {
    let role_str = match msg.role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::User => "user",
    };

    let mut message = Map::new();
    message.insert("role".into(), json!(role_str));

    let mut context_items: Vec<Value> = Vec::new();

    match msg.role {
        Role::Tool => {
            // A tool result: Continue stores output in `content`, keyed by `toolCallId`.
            let (tool_use_id, content) = tool_result_of(&msg.content);
            message.insert("toolCallId".into(), json!(tool_use_id));
            message.insert("content".into(), json!(content));
        }
        _ => {
            // content: array of text/image parts (the array shape the parser handles).
            let mut parts: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => {
                        parts.push(json!({ "type": "text", "text": text }));
                    }
                    Block::Thinking { text, .. } => {
                        // Continue's ChatMessage has no thinking part; preserve as text so it's
                        // searchable and survives a round-trip as content.
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    Block::Image { data_ref, .. } => {
                        if let Some(url) = data_ref {
                            parts.push(json!({
                                "type": "imageUrl",
                                "imageUrl": { "url": url },
                            }));
                        }
                    }
                    Block::ToolUse { id, name, input, .. } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                // `arguments` is a JSON-encoded *string*.
                                "arguments": serde_json::to_string(input.resolve()?.as_ref())?,
                            },
                        }));
                    }
                    Block::File { path, source, mime } => {
                        context_items.push(file_context_item(path, source, mime));
                    }
                    Block::ToolResult { .. } => {}
                }
            }
            message.insert("content".into(), Value::Array(parts));
            if msg.role == Role::Assistant && !tool_calls.is_empty() {
                message.insert("toolCalls".into(), Value::Array(tool_calls));
            }
        }
    }

    let mut item = Map::new();
    item.insert("message".into(), Value::Object(message));
    item.insert("contextItems".into(), Value::Array(context_items));
    // Carry the model so the parser can backfill it (assistant turns).
    if msg.role == Role::Assistant {
        if let Some(model) = msg.model.as_deref().or(session_model) {
            item.insert("promptLogs".into(), json!([{ "modelTitle": model }]));
        }
    }
    Ok(Value::Object(item))
}

/// Map a `Block::File` to a Continue `contextItem` (`{name, description, uri:{type, value}}`) that
/// the parser's `context_item_to_block` round-trips back into a `File` block.
fn file_context_item(path: &Option<String>, source: &Option<String>, mime: &Option<String>) -> Value {
    let mut ci = Map::new();
    match (path, source) {
        (Some(p), _) => {
            ci.insert("name".into(), json!(file_name_of(p)));
            ci.insert("description".into(), json!(p));
            ci.insert("uri".into(), json!({ "type": "file", "value": p }));
        }
        (None, Some(s)) => {
            ci.insert("name".into(), json!(s));
            ci.insert("description".into(), json!(s));
            ci.insert("uri".into(), json!({ "type": "url", "value": s }));
        }
        (None, None) => {
            ci.insert("name".into(), json!("file"));
        }
    }
    if let Some(m) = mime {
        ci.insert("content".into(), json!(""));
        ci.insert("mime".into(), json!(m));
    }
    Value::Object(ci)
}

/// Basename of a path-ish string, for a `contextItem` name.
fn file_name_of(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// Pull `(toolCallId, content)` from a Tool turn's first ToolResult block.
fn tool_result_of(content: &[Block]) -> (String, String) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id, content, ..
        } = b
        {
            return (tool_use_id.clone(), content.to_string());
        }
    }
    (String::new(), String::new())
}

// ---------------------------------------------------------------------------
// Index (sessions.json)
// ---------------------------------------------------------------------------

struct IndexMeta {
    session_id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    created_at: Option<DateTime<Utc>>,
}

/// Read `sessions.json` (array of `{sessionId, title, dateCreated, workspaceDirectory}`). Empty vec
/// on any error.
fn read_index(path: &Path) -> Vec<IndexMeta> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let session_id = e.get("sessionId").and_then(Value::as_str)?.to_string();
            if session_id.is_empty() {
                return None;
            }
            Some(IndexMeta {
                session_id,
                title: e
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, 80)),
                cwd: e
                    .get("workspaceDirectory")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path),
                created_at: e.get("dateCreated").and_then(parse_date),
            })
        })
        .collect()
}

/// Build a `SessionRef` from index metadata + the on-disk transcript path.
fn ref_from(meta: &IndexMeta, path: &Path, message_count: usize) -> SessionRef {
    let updated = mtime(path);
    SessionRef {
        id: meta.session_id.clone(),
        harness: Harness::Continue,
        path: path.to_path_buf(),
        cwd: meta.cwd.clone(),
        title: meta.title.clone(),
        created_at: meta.created_at.or(updated),
        updated_at: updated,
        message_count,
    }
}

/// Discovery keeps metadata and a count, never a whole history. Validate each discarded item
/// before dropping it: IgnoredAny would accept malformed Unicode that the old Value rejected.
/// Duplicate fields retain their last value, including null/wrong-type replacements.
#[derive(Default)]
struct Scan {
    title: Option<String>,
    cwd: Option<PathBuf>,
    message_count: usize,
}

impl<'de> Deserialize<'de> for Scan {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Scan;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a Continue session object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Scan, A::Error> {
                let mut scan = Scan::default();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "history" {
                        scan.message_count = map.next_value::<HistoryCount>()?.0;
                    } else {
                        match key.as_str() {
                            "title" => {
                                let v: Value = map.next_value()?;
                                scan.title = v
                                    .as_str()
                                    .filter(|t| !t.trim().is_empty())
                                    .map(|t| crate::ir::truncate(t, 80));
                            }
                            "workspaceDirectory" => {
                                let v: Value = map.next_value()?;
                                scan.cwd = v.as_str().filter(|p| !p.is_empty()).map(workspace_to_path);
                            }
                            _ => {
                                map.next_value::<DiscardValue>()?;
                            }
                        }
                    }
                }
                Ok(scan)
            }
        }
        d.deserialize_map(Fields)
    }
}

struct HistoryCount(usize);

impl<'de> Deserialize<'de> for HistoryCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Count;
        impl<'de> Visitor<'de> for Count {
            type Value = HistoryCount;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<HistoryCount, A::Error> {
                let mut count = 0;
                while seq.next_element::<DiscardValue>()?.is_some() {
                    count += 1;
                }
                Ok(HistoryCount(count))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<HistoryCount, A::Error> {
                DiscardValue::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(HistoryCount(0))
            }
            fn visit_bool<E>(self, _: bool) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
            fn visit_i64<E>(self, _: i64) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
            fn visit_u64<E>(self, _: u64) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
            fn visit_f64<E>(self, _: f64) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
            fn visit_str<E>(self, _: &str) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
            fn visit_unit<E>(self) -> Result<HistoryCount, E> {
                Ok(HistoryCount(0))
            }
        }
        d.deserialize_any(Count)
    }
}

fn scan_session(path: &Path) -> Option<Scan> {
    let bytes = read_bytes(path)?;
    let bytes = bytes.as_ref();
    // Non-object JSON used to yield empty metadata, rather than a parse error.
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        serde_json::from_slice::<DiscardValue>(bytes).ok()?;
        return Some(Scan::default());
    }
    serde_json::from_slice(bytes).ok()
}

// ---------------------------------------------------------------------------
// history[] -> IR
// ---------------------------------------------------------------------------

/// Convert one `history` item into an IR `Message`. `None` for an empty/message-less item. `model`
/// is filled opportunistically from a `promptLogs[].modelTitle`/`modelProvider` if present.
fn history_item_to_message(item: &Value, model: &mut Option<String>) -> Option<Message> {
    let message = item.get("message")?;
    let role_str = message.get("role").and_then(Value::as_str).unwrap_or("user");
    let role = match role_str {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    };

    let mut m = Message::new(role);
    // A `system` role history item carries the system prompt.
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }

    // Opportunistically capture the model from promptLogs (assistant turns).
    if model.is_none() {
        if let Some(mt) = item
            .get("promptLogs")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|l| l.get("modelTitle").or_else(|| l.get("modelProvider")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *model = Some(mt.to_string());
        }
    }

    match role {
        Role::Tool => {
            // A tool result. Continue stores the output in `content`.
            let content = text_parts(message.get("content"));
            let tool_use_id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            m.content.push(Block::ToolResult {
                tool_use_id,
                content: content.into(),
                is_error: false,
                tool_name: None,
                status: Some("completed".into()),
                details: None,
            });
        }
        _ => {
            push_content(&mut m, message.get("content"));
            if role == Role::Assistant {
                m.content.extend(openai::tool_calls(message.get("toolCalls")));
            }
        }
    }

    // Attached context items (files / snippets / docs) → File blocks (best-effort).
    if let Some(items) = item.get("contextItems").and_then(Value::as_array) {
        for ci in items {
            if let Some(b) = context_item_to_block(ci) {
                m.content.push(b);
            }
        }
    }

    // Per-request usage on assistant turns (`message.usage`, OpenAI's names): `prompt_tokens`
    // includes the cached part, which the IR keeps disjoint.
    if let Some(u) = message.get("usage").filter(|u| u.is_object()) {
        let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
        let cached = n("/prompt_tokens_details/cached_tokens");
        m.usage = Some(Usage {
            input_tokens: n("/prompt_tokens").map(|p| p.saturating_sub(cached.unwrap_or(0))),
            output_tokens: n("/completion_tokens"),
            cache_read_tokens: cached,
            reasoning_tokens: n("/completion_tokens_details/reasoning_tokens"),
            cost_usd: u.get("cost_cents").and_then(Value::as_f64).map(|c| c / 100.0),
            ..Usage::default()
        });
        m.model = u.get("model").and_then(Value::as_str).map(str::to_string);
        if model.is_none() {
            model.clone_from(&m.model);
        }
    }

    (!m.content.is_empty()).then_some(m)
}

/// The tool results an assistant item carries in `toolCallStates` (what the cn CLI writes instead
/// of `role: "tool"` items): `[{toolCallId, toolCall: {function: {name}}, status, output:
/// [{content, name, description}]}]` → one [`Role::Tool`] message of [`Block::ToolResult`]s.
/// `status` is `done` | `errored` | `canceled` | ...; a state with no output yet is skipped.
fn tool_states_message(item: &Value) -> Option<Message> {
    let states = item.get("toolCallStates")?.as_array()?;
    let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
    for st in states {
        let Some(out) = st.get("output").and_then(Value::as_array) else {
            continue;
        };
        let content = out
            .iter()
            .filter_map(|o| o.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let status = st.get("status").and_then(Value::as_str);
        m.content.push(Block::ToolResult {
            tool_use_id: st.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string(),
            content: content.into(),
            is_error: status == Some("errored"),
            tool_name: st
                .pointer("/toolCall/function/name")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: status.map(str::to_string),
            details: None,
        });
    }
    (!m.content.is_empty()).then_some(m)
}

/// Push `message.content` (string or `[Part]`) onto a message as Text/Image blocks.
fn push_content(m: &mut Message, content: Option<&Value>) {
    m.content.extend(openai::blocks_from(content, &openai::CONTINUE));
}

/// A `contextItem` → a `File` block (best-effort). The path/source is taken from `uri.value` (a file
/// path or URL) and falls back to `description`/`name`.
fn context_item_to_block(ci: &Value) -> Option<Block> {
    let uri_val = ci
        .get("uri")
        .and_then(|u| u.get("value"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let desc = ci.get("description").and_then(Value::as_str).filter(|s| !s.is_empty());
    let name = ci.get("name").and_then(Value::as_str).filter(|s| !s.is_empty());

    let uri_type = ci
        .get("uri")
        .and_then(|u| u.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Nothing identifiable → skip.
    if uri_val.is_none() && desc.is_none() && name.is_none() {
        return None;
    }

    // `uri.type == "file"` → a filesystem path; otherwise treat the value as an opaque source/URL.
    let (path, source) = match (uri_val, uri_type) {
        (Some(v), "file") => (Some(v.to_string()), None),
        (Some(v), _) => (None, Some(v.to_string())),
        (None, _) => (None, desc.or(name).map(str::to_string)),
    };

    Some(Block::File {
        mime: None,
        path,
        source,
    })
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Continue's `workspaceDirectory` may be a `file://` URI or a plain path. Normalize to a path.
fn workspace_to_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("file://") {
        // file:///Users/... -> /Users/... (drop the empty authority's leading slash group).
        PathBuf::from(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Parse a `dateCreated` value: a number or numeric-string of **milliseconds** since epoch, or an
/// ISO-8601 string.
fn parse_date(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => n.as_i64().and_then(ts_from_epoch),
        Value::String(s) => {
            let s = s.trim();
            if let Ok(ms) = s.parse::<i64>() {
                return ts_from_epoch(ms);
            }
            super::parse_ts(s)
        }
        _ => None,
    }
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    #[test]
    fn cn_tool_call_states_become_tool_results_and_usage_is_read() {
        let dir = std::env::temp_dir().join(format!("cv-continue-cn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        fs::write(&path, r#"{"sessionId":"s","history":[
            {"message":{"role":"user","content":"read a"},"contextItems":[]},
            {"message":{"role":"assistant","content":"","toolCalls":[{"id":"c1","type":"function","function":{"name":"Read","arguments":"{}"}}],
              "usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40},"model":"glm","cost_cents":2}},
             "contextItems":[],
             "toolCallStates":[{"toolCallId":"c1","toolCall":{"function":{"name":"Read"}},"status":"errored",
               "output":[{"content":"no such file","name":"Tool Result"}]}]},
            {"message":{"role":"tool","content":"no such file","toolCallId":"c1"},"contextItems":[]}
        ]}"#).unwrap();
        let r = SessionRef::new("s", Harness::Continue, path);
        let s = Continue { sessions: None }.parse(&r).unwrap();
        let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MessageKind::Prompt, MessageKind::Reply, MessageKind::ToolResult]
        );
        let u = s.messages[1].usage.as_ref().unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (Some(60), Some(40), Some(7))
        );
        assert_eq!(s.model.as_deref(), Some("glm"));
        match &s.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                is_error,
                tool_name,
                ..
            } => {
                assert_eq!(
                    (tool_use_id.as_str(), *is_error, tool_name.as_deref()),
                    ("c1", true, Some("Read"))
                )
            }
            b => panic!("{b:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/continue")
    }

    #[test]
    fn date_parsing_ms_and_iso() {
        let ms = parse_date(&Value::String("1700000000000".into())).unwrap();
        assert_eq!(ms.timestamp_millis(), 1700000000000);
        let num = parse_date(&Value::Number(1700000000000i64.into())).unwrap();
        assert_eq!(num.timestamp_millis(), 1700000000000);
        let iso = parse_date(&Value::String("2026-05-29T12:00:00Z".into())).unwrap();
        assert_eq!(iso.to_rfc3339(), "2026-05-29T12:00:00+00:00");
        assert!(parse_date(&Value::Null).is_none());
    }

    #[test]
    fn workspace_uri_and_plain() {
        assert_eq!(workspace_to_path("/Users/me/proj"), PathBuf::from("/Users/me/proj"));
        assert_eq!(
            workspace_to_path("file:///Users/me/proj"),
            PathBuf::from("/Users/me/proj")
        );
    }

    #[test]
    fn user_string_and_assistant_toolcall() {
        let mut model = None;
        let user: Value =
            serde_json::from_str(r#"{"message":{"role":"user","content":"hello"},"contextItems":[]}"#).unwrap();
        let m = history_item_to_message(&user, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let asst: Value = serde_json::from_str(
            r#"{"message":{"role":"assistant","content":"let me check","toolCalls":[
                {"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&asst, &mut model).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.text().as_deref(), Some("let me check"));
        match m.content.iter().find(|b| matches!(b, Block::ToolUse { .. })) {
            Some(Block::ToolUse { id, name, input, .. }) => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(input.resolve().unwrap()["path"], "a.rs");
            }
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn content_array_text_and_image() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":[
                {"type":"text","text":"look at this"},
                {"type":"imageUrl","imageUrl":{"url":"data:image/png;base64,AAA"}}
            ]}}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("look at this"));
        assert!(m.content.iter().any(|b| matches!(
            b,
            Block::Image { data_ref, .. } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        )));
    }

    #[test]
    fn kind_and_origin_across_roles() {
        let mut model = None;
        let mk = |line: &str, model: &mut Option<String>| {
            history_item_to_message(&serde_json::from_str::<Value>(line).unwrap(), model).unwrap()
        };
        let u = mk(r#"{"message":{"role":"user","content":"hi"}}"#, &mut model);
        assert_eq!((u.kind, u.origin), (MessageKind::Prompt, Origin::Human));
        let a = mk(r#"{"message":{"role":"assistant","content":"yo"}}"#, &mut model);
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        let t = mk(
            r#"{"message":{"role":"tool","content":"out","toolCallId":"c1"}}"#,
            &mut model,
        );
        assert_eq!((t.kind, t.origin), (MessageKind::ToolResult, Origin::Harness));
        let sys = mk(r#"{"message":{"role":"system","content":"you are terse"}}"#, &mut model);
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );
        // No flat extra keys were introduced.
        for m in [&u, &a, &t, &sys] {
            for k in m.extra.keys() {
                assert!(k == "continue" || k == "_record", "unexpected extra key {k}");
            }
        }
    }

    #[test]
    fn tool_result_message() {
        let mut model = None;
        let v: Value =
            serde_json::from_str(r#"{"message":{"role":"tool","content":"file contents here","toolCallId":"call_1"}}"#)
                .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id, content, ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents here");
            }
            _ => panic!("expected tool_result"),
        }
    }

    #[test]
    fn context_items_become_files() {
        let mut model = None;
        let v: Value = serde_json::from_str(
            r#"{"message":{"role":"user","content":"check these"},"contextItems":[
                {"name":"main.rs","description":"src/main.rs","content":"fn main(){}","uri":{"type":"file","value":"/proj/src/main.rs"}},
                {"name":"Docs","description":"react docs","content":"...","uri":{"type":"url","value":"https://react.dev"}}
            ]}"#,
        )
        .unwrap();
        let m = history_item_to_message(&v, &mut model).unwrap();
        let files: Vec<_> = m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::File { path, source, .. } => Some((path.clone(), source.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0.as_deref(), Some("/proj/src/main.rs"));
        assert_eq!(files[1].1.as_deref(), Some("https://react.dev"));
    }

    #[test]
    fn parses_fixture_session() {
        let dir = fixtures();
        let path = dir.join("11111111-1111-1111-1111-111111111111.json");
        if !path.exists() {
            return;
        }
        let r = SessionRef::new("11111111-1111-1111-1111-111111111111", Harness::Continue, path.clone());
        let s = Continue { sessions: None }.parse(&r).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::File { .. }))));
    }

    #[test]
    fn emit_round_trips() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-emit-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);

        // Build a small session: user, assistant w/ a tool call, a tool result.
        let mut session = Session {
            cwd: Some(PathBuf::from("/Users/me/proj")),
            title: Some("Roundtrip".into()),
            model: Some("gpt-4o".into()),
            ..Session::new("src", Harness::Continue)
        };

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "please read a.rs".into(),
        });
        session.messages.push(user);

        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text { text: "on it".into() });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.rs" }).into(),
            namespace: None,
        });
        session.messages.push(asst);

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "fn main() {}".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        session.messages.push(tool);

        let opts = crate::emit::EmitOptions {
            new_id: Some("emit-test-id".into()),
            new_cwd: None,
            ..Default::default()
        };
        let res = emit(&session, &dir, &opts).unwrap();
        assert_eq!(res.new_id, "emit-test-id");
        assert!(res.path.exists());

        // Re-parse via this adapter.
        let r = SessionRef::new(res.new_id.clone(), Harness::Continue, res.path.clone());
        let parsed = Continue { sessions: None }.parse(&r).unwrap();

        assert_eq!(parsed.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[0].text().as_deref(), Some("please read a.rs"));
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert!(parsed.messages[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert_eq!(parsed.messages[2].role, Role::Tool);
        assert!(parsed.messages[2].content.iter().any(|b| matches!(
            b,
            Block::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_1" && content == "fn main() {}"
        )));

        // The index should also discover it.
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        assert!(refs.iter().any(|r| r.id == "emit-test-id"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovers_from_index() {
        let dir = fixtures();
        if !dir.join("sessions.json").exists() {
            return;
        }
        let adapter = Continue {
            sessions: Some(dir.clone()),
        };
        let refs = adapter.discover().unwrap();
        let r = refs
            .iter()
            .find(|r| r.id == "11111111-1111-1111-1111-111111111111")
            .expect("indexed session discovered");
        assert_eq!(r.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(r.cwd.as_deref(), Some(Path::new("/Users/me/proj")));
        assert!(r.message_count >= 3);
        assert!(r.created_at.is_some());
    }

    /// The native `stream` must emit incrementally (not collect-then-replay): a sink that stops on
    /// the first message ends the parse with exactly one message seen, never materializing the rest.
    #[test]
    fn stream_is_incremental_and_honors_stop() {
        use crate::stream::{Flow, MessageSink};
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cv-continue-stream-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        fs::write(
            &path,
            r#"{"sessionId":"s","title":"T","workspaceDirectory":"/w","history":[
                {"message":{"role":"user","content":"one"}},
                {"message":{"role":"assistant","content":"two"}},
                {"message":{"role":"user","content":"three"}}
            ]}"#,
        )
        .unwrap();

        let r = SessionRef::new("s", Harness::Continue, path.clone());

        // A sink that records every message it sees and stops after the first.
        struct StopFirst {
            seen: Vec<String>,
        }
        impl MessageSink for StopFirst {
            fn message(&mut self, m: Message) -> Flow {
                self.seen.push(m.text().unwrap_or_default());
                Flow::Stop
            }
        }
        let mut sink = StopFirst { seen: Vec::new() };
        let s = Continue { sessions: None }
            .stream(&r, &ParseOptions::full(), &mut sink)
            .unwrap();

        // Only the first message was ever materialized; the returned Session is metadata-only.
        assert_eq!(sink.seen, vec!["one".to_string()]);
        assert!(s.messages.is_empty(), "stream returns empty messages");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/w")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_scan_matches_whole_value_metadata() {
        let _world = cv_testkit::World::in_process("continue-scan");
        let dir = std::env::temp_dir().join(format!("cv-continue-scan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let mut cases = vec![
            r#"{"title":"escaped\n\uD834\uDD1E","workspaceDirectory":"file:///p","history":[null,{},42,{"message":{"role":"tool"}}]}"#.to_owned(),
            r#"{"title":"old","title":null,"workspaceDirectory":"/old","workspaceDirectory":42,"history":[1,2],"history":false}"#.to_owned(),
            r#"{"title":42,"title":"new","history":null,"history":[1,2,3]}"#.to_owned(),
            r#"{"history":{"nested":[1,2]},"title":"   "}"#.to_owned(),
            "[1,2,3]".to_owned(), "null".to_owned(), "42".to_owned(),
            r#"{"title":"early","history":[1,{"content":"bad\uD800"}]}"#.to_owned(),
            r#"{"history":[1],"ignored":"bad\uDC00"}"#.to_owned(),
            r#"{"history":[1e999]}"#.to_owned(),
            r#"{"history":[1],"history":[2,}"#.to_owned(),
            r#"{"history":[1]} trailing"#.to_owned(),
        ];
        for depth in 123..131 {
            cases.push(format!(
                r#"{{"history":[{}0{}]}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        // Large discarded bodies and nested context are validated, not retained in a history tree.
        cases.push(
            serde_json::json!({
                "title": "metadata", "history": (0..24).map(|_| serde_json::json!({
                    "message": { "content": "payload\\\n".repeat(16_384) },
                    "contextItems": [{"content": "unused".repeat(4096)}],
                })).collect::<Vec<_>>()
            })
            .to_string(),
        );
        for text in cases {
            fs::write(&path, &text).unwrap();
            let expected = serde_json::from_str::<Value>(&text).ok().map(|v| {
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| crate::ir::truncate(t, 80));
                let cwd = v
                    .get("workspaceDirectory")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(workspace_to_path);
                let count = v.get("history").and_then(Value::as_array).map_or(0, Vec::len);
                (title, cwd, count)
            });
            let actual = scan_session(&path).map(|s| (s.title, s.cwd, s.message_count));
            assert_eq!(actual, expected, "case prefix: {:?}", &text[..text.len().min(120)]);
        }
        fs::write(&path, b"{\"history\":[\"\xff\"]}").unwrap();
        assert!(scan_session(&path).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discovery_scan_preserves_index_precedence_and_unindexed_fallback() {
        let _world = cv_testkit::World::in_process("continue-scan-index");
        let dir = std::env::temp_dir().join(format!("cv-continue-index-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("sessions.json"),
            r#"[
            {"sessionId":"indexed","title":"index title","workspaceDirectory":"/index","dateCreated":"1700000000000"},
            {"sessionId":"missing","title":"missing"}
        ]"#,
        )
        .unwrap();
        fs::write(
            dir.join("indexed.json"),
            r#"{"title":"file title","workspaceDirectory":"/file","history":[1,2]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("extra.json"),
            r#"{"title":"extra title","workspaceDirectory":"file:///extra","history":[1,null,{}]}"#,
        )
        .unwrap();
        fs::write(dir.join("broken.json"), r#"{"title":"partial","history":[1,}"#).unwrap();
        let refs = Continue {
            sessions: Some(dir.clone()),
        }
        .discover()
        .unwrap();
        assert_eq!(refs.len(), 3);
        let indexed = refs.iter().find(|r| r.id == "indexed").unwrap();
        assert_eq!(indexed.title.as_deref(), Some("index title"));
        assert_eq!(indexed.cwd.as_deref(), Some(Path::new("/index")));
        assert_eq!(indexed.created_at.unwrap().timestamp_millis(), 1700000000000);
        assert_eq!(indexed.message_count, 2);
        let extra = refs.iter().find(|r| r.id == "extra").unwrap();
        assert_eq!(extra.title.as_deref(), Some("extra title"));
        assert_eq!(extra.cwd.as_deref(), Some(Path::new("/extra")));
        assert_eq!(extra.created_at, extra.updated_at);
        assert_eq!(extra.message_count, 3);
        let broken = refs.iter().find(|r| r.id == "broken").unwrap();
        assert!(broken.title.is_none());
        assert_eq!(broken.message_count, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
