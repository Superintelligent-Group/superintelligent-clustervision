//! Zed editor agent-panel adapter — `threads.db` (SQLite, zstd-compressed JSON blobs).
//! **sqlite feature only** (rusqlite + ruzstd).
//!
//! Zed keeps every agent thread in one SQLite DB under its data dir:
//!   * **macOS:**   `~/Library/Application Support/Zed/threads/threads.db`
//!   * **Linux:**   `$XDG_DATA_HOME/zed/threads/threads.db` (default `~/.local/share/zed/…`)
//!   * **Windows:** `%LOCALAPPDATA%\Zed\threads\threads.db`
//!
//! ## Schema (base table + accreted `ALTER TABLE` columns; probe before SELECTing)
//! ```sql
//! CREATE TABLE threads (
//!   id TEXT PRIMARY KEY, summary TEXT NOT NULL, updated_at TEXT NOT NULL,
//!   data_type TEXT NOT NULL, data BLOB NOT NULL,
//!   -- added later, may be absent in old DBs:
//!   parent_id TEXT, worktree_branch TEXT, folder_paths TEXT, folder_paths_order TEXT,
//!   created_at TEXT );
//! ```
//! `data_type` is `"zstd"` (blob = one zstd frame of JSON; Zed writes level 3) or `"json"` (raw).
//! `folder_paths` = workspace folders, lexicographically sorted, `\n`-joined;
//! `folder_paths_order` = `,`-joined indices restoring the user's original order.
//! Timestamps are RFC3339 with offset. `parent_id` links a **subagent** thread to its parent.
//!
//! ## Blob JSON — three generations, sniffed by `version` + message shape
//! * **legacy / agent1** (`version` `"0.1.0"`/`"0.2.0"`, or absent in the oldest files):
//!   `{version, summary, updated_at, messages:[{id:int, role:"user"|"assistant"|"system",
//!   segments:[{type:"text"|"thinking"|"RedactedThinking", …}], tool_uses:[{id,name,input}],
//!   tool_results:[{tool_use_id,is_error,content,output}], context:"…", creases:[…],
//!   is_hidden}], initial_project_snapshot, cumulative_token_usage, request_token_usage,
//!   detailed_summary_state, model:{provider,model}, completion_mode, profile, …}`.
//!   The very oldest (versionless) messages carry a plain `text` field instead of `segments`.
//!   In 0.1.0 tool results rode on the *next user* message; 0.2.0 moved them onto the assistant
//!   message that called the tool. We split them into a `Role::Tool` turn either way, so both
//!   variants converge.
//! * **modern / agent2 ACP** (`version` `"0.3.0"`): `{title, messages:[…], updated_at,
//!   detailed_summary, initial_project_snapshot, model, profile, subagent_context:
//!   {parent_thread_id, depth}, imported, …}` where each message is an *externally tagged* enum:
//!   `{"User":{id, content:[{"Text":…}|{"Mention":{uri,content}}|{"Image":{…}}]}}`,
//!   `{"Agent":{content:[{"Text":…}|{"Thinking":{text,signature}}|{"RedactedThinking":…}
//!   |{"ToolUse":{id,name,input,…}}], tool_results:{<id>:{tool_use_id,tool_name,is_error,
//!   content,output}}, reasoning_details}}`, `"Resume"`, or `{"Compaction":{…}}`.
//!
//! cwd = `initial_project_snapshot.worktree_snapshots[0].worktree_path` (falls back to the
//! `folder_paths` column); the same snapshot's `git_state` provides [`GitInfo`]. Zed records **no
//! per-message timestamps** — only thread-level created/updated.
//!
//! ## Fidelity caveats
//! * Multi-root workspaces: we take the first worktree as `cwd` (extra worktrees are only in the
//!   project snapshot, which we don't replicate into the IR).
//! * Legacy `context` (the rendered attached-files preamble) is preserved in `extra["zed"]`;
//!   modern `Mention` content (an attached file's body) is reduced to its URI as a [`Block::File`].
//! * `request_token_usage` (per-request usage) has no per-message home — only the thread-level
//!   `cumulative_token_usage` is kept (in `extra["zed"]`). A subagent's `parent_thread_id` is
//!   first-class [`Lineage::parent`]; the `Compaction` summary becomes a `CompactionSummary` turn.

mod blob;
mod source;

use super::store::sqlite::{columns, open_ro};
use super::wire::anthropic;
use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Zed {
    /// `threads.db`, if it exists on disk.
    db: Option<PathBuf>,
}

impl Zed {
    pub fn detect() -> Self {
        Zed { db: threads_db() }
    }

    /// Read an explicit directory holding `threads.db`, without changing process discovery.
    pub fn with_root(root: &Path) -> Self {
        let path = root.join("threads.db");
        Zed {
            db: path.is_file().then_some(path),
        }
    }
}

/// Resolve `…/Zed/threads/threads.db` across platforms. Returns it only if it exists, so discovery
/// degrades to empty on machines without Zed.
fn threads_db() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // XDG override first (portable, and what Zed's `paths::data_dir` honours on Linux).
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg).join("zed/threads/threads.db"));
    }
    if let Some(h) = dirs::home_dir() {
        candidates.push(h.join("Library/Application Support/Zed/threads/threads.db"));
        candidates.push(h.join(".local/share/zed/threads/threads.db"));
        // Flatpak install keeps the same layout under the app sandbox.
        candidates.push(h.join(".var/app/dev.zed.Zed/data/zed/threads/threads.db"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Zed").join("threads").join("threads.db"));
    }
    candidates.into_iter().find(|p| p.exists())
}

impl Adapter for Zed {
    fn harness(&self) -> Harness {
        Harness::Zed
    }

    /// The `threads/` dir holding `threads.db` (a root is a directory; the db itself is a
    /// catalog file-watch, see `catalog::watches_for`).
    fn roots(&self) -> Vec<PathBuf> {
        self.db
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(db) = &self.db else {
            return Ok(vec![]);
        };
        let conn = open_ro(db)?;
        conn.read(|conn| discover_db(conn, db))
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            let conn = open_ro(&r.path)?;
            conn.read(|conn| {
                if opts.spans {
                    let source = crate::json::JsonSource::capture_with_reads(&r.path, conn.reads())?;
                    let result = stream_thread_with_source(conn, r, sink, Some(&source));
                    source.check()?;
                    result
                } else {
                    stream_thread(conn, r, sink)
                }
            })
        })
    }
}

fn discover_db(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
    let rowid = conn.prepare("SELECT _rowid_ FROM threads LIMIT 0").is_ok();
    let cols = columns(conn, "threads");
    let has = |c: &str| cols.contains(c);
    let sql = format!(
        "SELECT id, summary, updated_at, {created}, {folders}, {order}, data_type, {rowid}, typeof(data) \
         FROM threads ORDER BY updated_at DESC",
        rowid = if rowid { "_rowid_" } else { "NULL" },
        created = if has("created_at") { "created_at" } else { "NULL" },
        folders = if has("folder_paths") { "folder_paths" } else { "NULL" },
        order = if has("folder_paths_order") {
            "folder_paths_order"
        } else {
            "NULL"
        },
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        // One unreadable row never fails the harness. Borrow the SQLite cell only while its
        // row is current; no Rust copy of the complete compressed source is needed.
        let Ok(id) = row.get::<_, String>(0) else { continue };
        let summary = row.get::<_, Option<String>>(1).ok().flatten();
        let updated = row.get::<_, Option<String>>(2).ok().flatten();
        let created = row.get::<_, Option<String>>(3).ok().flatten();
        let folders = row.get::<_, Option<String>>(4).ok().flatten();
        let order = row.get::<_, Option<String>>(5).ok().flatten();
        let data_type = row.get::<_, Option<String>>(6).ok().flatten();
        // Sorting only metadata avoids putting every complete blob in SQLite's temporary
        // sorter. Native rowid stores can read the payload in fixed-size pages; older or
        // WITHOUT ROWID stores retain a point-lookup borrowed-cell fallback.
        let rowid = row.get::<_, Option<i64>>(7).ok().flatten();
        let is_blob = row.get::<_, String>(8).is_ok_and(|kind| kind == "blob");
        let thread = is_blob
            .then(|| thread_header(conn, &id, rowid, data_type.as_deref(), true, None))
            .flatten();
        let count = thread.as_ref().map(|t| t.message_count).unwrap_or(0);
        let cwd = thread.as_ref().and_then(|t| snapshot_cwd(&t.metadata));
        let cwd = cwd.or_else(|| first_folder_path(folders.as_deref(), order.as_deref()));
        out.push(SessionRef {
            id,
            harness: Harness::Zed,
            path: db.to_path_buf(),
            cwd,
            title: summary
                .filter(|s| !s.trim().is_empty())
                .map(|t| crate::ir::truncate(&t, 80)),
            created_at: created.as_deref().and_then(super::parse_ts),
            updated_at: updated.as_deref().and_then(super::parse_ts),
            message_count: count,
        });
    }
    Ok(out)
}

fn stream_thread(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    stream_thread_with_source(conn, r, sink, None)
}

fn stream_thread_with_source(
    conn: &Connection,
    r: &SessionRef,
    sink: &mut dyn MessageSink,
    source: Option<&crate::json::JsonSource>,
) -> Result<Session> {
    let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
    let rowid = conn.prepare("SELECT _rowid_ FROM threads LIMIT 0").is_ok();
    let cols = columns(conn, "threads");
    let has = |c: &str| cols.contains(c);
    let sql = format!(
        "SELECT summary, updated_at, {created}, {parent}, {folders}, {order}, data_type, {rowid}, \
         CASE WHEN typeof(data) = 'blob' THEN length(data) ELSE 0 END, typeof(data), {branch} FROM threads WHERE id = ?1",
        rowid = if rowid { "_rowid_" } else { "NULL" },
        created = if has("created_at") { "created_at" } else { "NULL" },
        parent = if has("parent_id") { "parent_id" } else { "NULL" },
        branch = if has("worktree_branch") { "worktree_branch" } else { "NULL" },
        folders = if has("folder_paths") { "folder_paths" } else { "NULL" },
        order = if has("folder_paths_order") {
            "folder_paths_order"
        } else {
            "NULL"
        },
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([&r.id])?;
    let row = rows.next()?.with_context(|| format!("zed thread {} not found", r.id))?;
    let summary = row.get::<_, Option<String>>(0).ok().flatten();
    let updated = row.get::<_, Option<String>>(1).ok().flatten();
    let created = row.get::<_, Option<String>>(2).ok().flatten();
    let parent = row.get::<_, Option<String>>(3).ok().flatten();
    let folders = row.get::<_, Option<String>>(4).ok().flatten();
    let order = row.get::<_, Option<String>>(5).ok().flatten();
    let data_type = row.get::<_, Option<String>>(6).ok().flatten();
    let rowid = row.get::<_, Option<i64>>(7).ok().flatten();
    let data_len = row.get::<_, i64>(8).unwrap_or(0);
    let branch = row
        .get::<_, Option<String>>(10)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    // Both passes read the same SQLite cell under one read snapshot. Validate the complete
    // document before emitting any message, including malformed ignored fields.
    let is_blob = row.get::<_, String>(9).is_ok_and(|kind| kind == "blob");
    let mut thread = is_blob
        .then(|| thread_header(conn, &r.id, rowid, data_type.as_deref(), false, source))
        .flatten();

    let mut zed = serde_json::Map::new();
    let mut model = None;
    let mut git = None;
    let mut blob_title = None;
    let mut blob_updated = None;
    let mut cwd = None;
    // The parent thread of a subagent: from the 0.3.0 blob's `subagent_context`, overridden by the
    // `parent_id` column when both name one. First-class [`Lineage`], not an extra key.
    let mut parent_thread_id: Option<String> = None;
    if let Some(header) = &mut thread {
        // Release parsed metadata before replay. Owned reads retain the final messages-member
        // ordinal; lazy reads retain its checked messages child and immutable decoded backing.
        let t = std::mem::take(&mut header.metadata);
        // `title` (0.3.0) / `summary` (legacy) — same meaning, renamed across generations.
        blob_title = t
            .get("title")
            .or_else(|| t.get("summary"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        blob_updated = t.get("updated_at").and_then(super::ts_from_value);
        cwd = snapshot_cwd(&t);
        git = snapshot_git(&t);
        model = t.get("model").and_then(model_string);
        thread_extras(&t, &mut zed, &mut parent_thread_id);
    } else if data_len > 0 {
        // The row exists but its blob wouldn't decode — surface that instead of erroring, so one
        // corrupt thread never breaks listings or bulk exports.
        zed.insert("undecodable_blob".into(), Value::Bool(true));
    }
    if let Some(branch) = branch {
        git.get_or_insert_with(GitInfo::default).branch.get_or_insert(branch);
    }
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        parent_thread_id = Some(p);
    }

    let mut s = Session {
        source_stamp: None,
        id: r.id.clone(),
        harness: Harness::Zed,
        cwd: cwd
            .or_else(|| first_folder_path(folders.as_deref(), order.as_deref()))
            .or_else(|| r.cwd.clone()),
        title: blob_title
            .or_else(|| summary.filter(|s| !s.trim().is_empty()))
            .or_else(|| r.title.clone()),
        created_at: created.as_deref().and_then(super::parse_ts).or(r.created_at),
        updated_at: blob_updated
            .or_else(|| updated.as_deref().and_then(super::parse_ts))
            .or(r.updated_at),
        model,
        git,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
        principal: None,
        system_prompt: None,
        lineage: Lineage {
            parent: parent_thread_id,
            ..Lineage::default()
        },
    };
    if !zed.is_empty() {
        let bag = s.harness_extra_mut(Harness::Zed);
        for (k, v) in zed {
            bag.insert(k, v);
        }
    }
    // All session metadata is known up front; hand it to the sink before the body.
    sink.meta(&s);

    if let Some(t) = &thread {
        if let (Some(view), Some(source)) = (&t.checked, source) {
            view.with_view(|view| source::replay(view, source, &mut |m| sink.message(m)))?;
        } else {
            thread_replay(conn, &r.id, rowid, data_type.as_deref(), t, &mut |m| sink.message(m));
        }
    }
    Ok(s)
}

/// The native rowid table supports truly incremental read-only I/O. A schema variant or a
/// blob-open failure falls back to one indexed cell lookup, preserving unreadable/nonblob
/// behavior without changing the source database or introducing compressed-byte spans.
fn thread_header(
    conn: &Connection,
    id: &str,
    rowid: Option<i64>,
    data_type: Option<&str>,
    count: bool,
    source: Option<&crate::json::JsonSource>,
) -> Option<blob::Header> {
    if let Some(rowid) = rowid {
        if let Ok(data) = conn.blob_open(rusqlite::DatabaseName::Main, "threads", "data", rowid, true) {
            return if count {
                blob::header_reader(data_type, data, count)
            } else if let Some(source) = source {
                blob::prepare_checked_reader(data_type, data, source)
            } else {
                blob::prepare_reader(data_type, data)
            };
        }
    }
    with_cell(conn, id, rowid, |data| {
        if count {
            blob::header(data_type, data, count)
        } else if let Some(source) = source {
            blob::prepare_checked_reader(data_type, data, source)
        } else {
            blob::prepare_reader(data_type, data)
        }
    })
    .flatten()
}

fn thread_replay(
    conn: &Connection,
    id: &str,
    rowid: Option<i64>,
    data_type: Option<&str>,
    header: &blob::Header,
    f: &mut dyn FnMut(Message) -> Flow,
) {
    if blob::replay_prepared(header, f) {
        return;
    }
    if let Some(rowid) = rowid {
        if let Ok(data) = conn.blob_open(rusqlite::DatabaseName::Main, "threads", "data", rowid, true) {
            blob::replay_reader(data_type, data, header, f);
            return;
        }
    }
    let _ = with_cell(conn, id, rowid, |data| blob::replay(data_type, data, header, f));
}

fn with_cell<T>(conn: &Connection, id: &str, rowid: Option<i64>, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
    let sql = if rowid.is_some() {
        "SELECT data FROM threads WHERE _rowid_ = ?1"
    } else {
        "SELECT data FROM threads WHERE id = ?1"
    };
    let mut stmt = conn.prepare(sql).ok()?;
    let key: &dyn rusqlite::ToSql = match &rowid {
        Some(rowid) => rowid,
        None => &id,
    };
    let mut rows = stmt.query([key]).ok()?;
    let row = rows.next().ok()??;
    let data = match row.get_ref(0) {
        Ok(rusqlite::types::ValueRef::Blob(data)) => data,
        _ => &[],
    };
    Some(f(data))
}

// ---------------------------------------------------------------------------
// Blob decoding
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Thread-level metadata
// ---------------------------------------------------------------------------

/// `{provider, model}` → `"provider/model"` (matching the goose/hermes convention).
fn model_string(v: &Value) -> Option<String> {
    let provider = v.get("provider").and_then(Value::as_str).filter(|s| !s.is_empty());
    let model = v.get("model").and_then(Value::as_str).filter(|s| !s.is_empty());
    match (provider, model) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (None, Some(m)) => Some(m.to_string()),
        (Some(p), None) => Some(p.to_string()),
        (None, None) => None,
    }
}

/// First worktree path of `initial_project_snapshot` — the cwd the thread was created against.
fn snapshot_cwd(t: &Value) -> Option<PathBuf> {
    t.pointer("/initial_project_snapshot/worktree_snapshots/0/worktree_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Git state of the first worktree snapshot.
fn snapshot_git(t: &Value) -> Option<GitInfo> {
    let g = t.pointer("/initial_project_snapshot/worktree_snapshots/0/git_state")?;
    let pick = |k: &str| {
        g.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let info = GitInfo {
        branch: pick("current_branch"),
        commit: pick("head_sha"),
        remote: pick("remote_url"),
    };
    (info.branch.is_some() || info.commit.is_some() || info.remote.is_some()).then_some(info)
}

/// First *ordered* workspace folder from the `folder_paths` (`\n`-joined, lexicographic) +
/// `folder_paths_order` (`,`-joined original indices) columns.
fn first_folder_path(folders: Option<&str>, order: Option<&str>) -> Option<PathBuf> {
    let folders = folders.filter(|s| !s.is_empty())?;
    let paths: Vec<&str> = folders.split('\n').collect();
    let first = order
        .and_then(|o| o.split(',').next())
        .and_then(|ix| ix.trim().parse::<usize>().ok())
        .and_then(|ix| paths.get(ix))
        .copied()
        .or_else(|| paths.first().copied())?;
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// Session-level facts shared by both generations, written into `extra["zed"]`: profile,
/// completion mode, detailed summary, cumulative token usage, subagent depth, imported flag, and
/// the raw format version. A subagent's `parent_thread_id` is first-class lineage, so it is
/// returned via `parent` rather than stashed in the bag.
fn thread_extras(t: &Value, extra: &mut serde_json::Map<String, Value>, parent: &mut Option<String>) {
    if let Some(v) = t.get("version").and_then(Value::as_str) {
        extra.insert("thread_version".into(), Value::String(v.to_string()));
    }
    for key in ["profile", "completion_mode"] {
        if let Some(v) = t.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            extra.insert(key.into(), Value::String(v.to_string()));
        }
    }
    // Legacy `detailed_summary_state: {"Generated": {"text": …}}` / modern `detailed_summary: "…"`.
    let detailed = t
        .pointer("/detailed_summary_state/Generated/text")
        .or_else(|| t.get("detailed_summary"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    if let Some(d) = detailed {
        extra.insert("detailed_summary".into(), Value::String(d.to_string()));
    }
    if let Some(u) = t.get("cumulative_token_usage").and_then(Value::as_object) {
        if u.values().any(|v| v.as_u64().unwrap_or(0) > 0) {
            extra.insert("cumulative_token_usage".into(), Value::Object(u.clone()));
        }
    }
    // 0.3.0 subagent threads: {parent_thread_id, depth}.
    if let Some(sub) = t.get("subagent_context").filter(|v| !v.is_null()) {
        if let Some(p) = sub
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *parent = Some(p.to_string());
        }
        if let Some(d) = sub.get("depth").and_then(Value::as_u64) {
            extra.insert("subagent_depth".into(), Value::Number(d.into()));
        }
    }
    if t.get("imported").and_then(Value::as_bool) == Some(true) {
        extra.insert("imported".into(), Value::Bool(true));
    }
}

// ---------------------------------------------------------------------------
// Messages — one walker for both generations (sniffed per message, not per thread,
// so a hand-migrated or mixed blob still parses)
// ---------------------------------------------------------------------------

/// Convert every thread message to IR and feed it to `f`, stopping on [`Flow::Stop`]. This is the
/// single conversion both `discover` (counting) and `stream` (emitting) run, so the
/// `message_count` contract holds by construction.
#[cfg(test)]
fn each_ir_message(t: &Value, f: &mut dyn FnMut(Message) -> Flow) {
    let Some(messages) = t.get("messages").and_then(Value::as_array) else {
        return;
    };
    // Tool-call ids → names, so legacy tool_results (which carry no name) get labeled.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in messages {
        if legacy_message(m, &mut tool_names, f) == Flow::Stop {
            return;
        }
        if modern_message(m, f) == Flow::Stop {
            return;
        }
    }
}

/// Legacy (agent1) message: an object with a string `role`. Emits the main turn, then its
/// `tool_results` as a separate [`Role::Tool`] turn. Non-legacy shapes are ignored.
fn legacy_message(m: &Value, tool_names: &mut HashMap<String, String>, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    let Some(role) = m.get("role").and_then(Value::as_str) else {
        return Flow::Continue;
    };
    let role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    let mut msg = Message::new(role);
    // A legacy `system` role carries the system prompt (rare; Zed's is normally app-level).
    if role == Role::System {
        msg.kind = MessageKind::SystemPrompt;
    }
    msg.id = m.get("id").and_then(Value::as_i64).map(|i| i.to_string());

    // segments[] (0.1.0+) or a plain `text` field (the oldest, versionless shape).
    if let Some(segments) = m.get("segments").and_then(Value::as_array) {
        for seg in segments {
            if let Some(b) = anthropic::block_from(seg, &anthropic::ZED) {
                msg.content.push(b);
            }
        }
    } else if let Some(text) = m.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            msg.content.push(Block::Text {
                text: text.to_string().into(),
            });
        }
    }

    for tu in m.get("tool_uses").and_then(Value::as_array).into_iter().flatten() {
        let id = tu.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let name = tu.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if !id.is_empty() && !name.is_empty() {
            tool_names.insert(id.clone(), name.clone());
        }
        msg.content.push(Block::ToolUse {
            id,
            name,
            input: tu.get("input").cloned().unwrap_or(Value::Null).into(),
            namespace: None,
        });
    }

    // `context` is the rendered attached-files preamble Zed prepends to the request. When it's the
    // *only* content (an attachment-only user turn), surface it as the text (mirroring Zed's own
    // 0.2.0→0.3.0 upgrade); otherwise preserve it in extra.
    if let Some(ctx) = m.get("context").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        if msg.content.is_empty() {
            msg.content.push(Block::Text {
                text: ctx.to_string().into(),
            });
        } else {
            // Embedded attached-files preamble on an otherwise-real turn: the turn stays a Prompt;
            // the preamble is noted in `extra["zed"]`.
            msg.harness_extra_mut(Harness::Zed)
                .insert("context".into(), Value::String(ctx.to_string()));
        }
    }
    if m.get("is_hidden").and_then(Value::as_bool) == Some(true) {
        msg.harness_extra_mut(Harness::Zed)
            .insert("is_hidden".into(), Value::Bool(true));
        // Hidden user turns are Zed's auto-injected "Continue where you left off" continuations.
        if role == Role::User {
            msg.kind = MessageKind::InjectedContext;
            msg.origin = Origin::Harness;
        }
    }
    if let Some(creases) = m.get("creases").and_then(Value::as_array).filter(|c| !c.is_empty()) {
        msg.harness_extra_mut(Harness::Zed)
            .insert("creases".into(), Value::Array(creases.clone()));
    }

    if !msg.content.is_empty() && f(msg) == Flow::Stop {
        return Flow::Stop;
    }

    // tool_results → one Role::Tool turn (0.2.0 keeps them on the assistant message; 0.1.0 put
    // them on the next user message — either way they become the same Tool turn here).
    let results = m.get("tool_results").and_then(Value::as_array);
    let blocks: Vec<Block> = results
        .into_iter()
        .flatten()
        .filter_map(|tr| tool_result_block(tr, tool_names))
        .collect();
    if !blocks.is_empty() {
        let mut tm = Message::new(Role::Tool);
        tm.content = blocks;
        return f(tm);
    }
    Flow::Continue
}

/// One legacy/modern tool result → a [`Block::ToolResult`]. The legacy shape has no `tool_name`;
/// we recover it from the paired `tool_uses` entry. `output` (the tool's structured return)
/// rides in `details`.
fn tool_result_block(tr: &Value, tool_names: &HashMap<String, String>) -> Option<Block> {
    let id = tr.get("tool_use_id").and_then(Value::as_str)?.to_string();
    let tool_name = tr
        .get("tool_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| tool_names.get(&id).cloned());
    Some(Block::ToolResult {
        content: tool_result_content(tr.get("content")).into(),
        is_error: tr.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        tool_use_id: id,
        tool_name,
        status: None,
        details: tr.get("output").filter(|o| !o.is_null()).cloned(),
    })
}

/// `LanguageModelToolResultContent`: a plain string, `{"Text": "…"}` (case-tolerant), or
/// `{"Image": {…}}` (flattened to a marker; the IR doesn't inline image bytes).
fn tool_result_content(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(s) = v.get("Text").or_else(|| v.get("text")).and_then(Value::as_str) {
        return s.to_string();
    }
    if v.get("Image").or_else(|| v.get("image")).is_some() {
        return "[image]".to_string();
    }
    if v.is_null() {
        return String::new();
    }
    v.to_string()
}

/// Modern (agent2 / 0.3.0) message: the externally tagged `Message` enum. `"Resume"` markers are
/// skipped; `Compaction` summaries become a flagged system turn. Non-modern shapes are ignored.
fn modern_message(m: &Value, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    if let Some(user) = m.get("User") {
        let mut msg = Message::new(Role::User);
        msg.id = user.get("id").and_then(Value::as_str).map(str::to_string);
        for item in user.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(b) = user_content_block(item) {
                msg.content.push(b);
            }
        }
        if !msg.content.is_empty() {
            return f(msg);
        }
        return Flow::Continue;
    }
    if let Some(agent) = m.get("Agent") {
        let mut msg = Message::new(Role::Assistant);
        let mut tool_names: HashMap<String, String> = HashMap::new();
        for item in agent.get("content").and_then(Value::as_array).into_iter().flatten() {
            if let Some(b) = agent_content_block(item) {
                if let Block::ToolUse { id, name, .. } = &b {
                    if !id.is_empty() && !name.is_empty() {
                        tool_names.insert(id.clone(), name.clone());
                    }
                }
                msg.content.push(b);
            }
        }
        if let Some(rd) = agent.get("reasoning_details").filter(|v| !v.is_null()) {
            msg.harness_extra_mut(Harness::Zed)
                .insert("reasoning_details".into(), rd.clone());
        }
        if (!msg.content.is_empty() || !msg.extra.is_empty()) && f(msg) == Flow::Stop {
            return Flow::Stop;
        }
        // tool_results is an id-keyed map ({"<tool_use_id>": {…}}), ordered by insertion.
        let blocks: Vec<Block> = agent
            .get("tool_results")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
            .filter_map(|tr| tool_result_block(tr, &tool_names))
            .collect();
        if !blocks.is_empty() {
            let mut tm = Message::new(Role::Tool);
            tm.content = blocks;
            return f(tm);
        }
        return Flow::Continue;
    }
    if let Some(comp) = m.get("Compaction") {
        // CompactionInfo::Summary(text) — the context-compaction summary that replaced history.
        if let Some(text) = comp
            .get("Summary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            let mut msg = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
            msg.content.push(Block::Text {
                text: text.to_string().into(),
            });
            return f(msg);
        }
    }
    // "Resume" markers and unknown variants carry no transcript content.
    Flow::Continue
}

fn user_content_block(item: &Value) -> Option<Block> {
    if let Some(text) = item.get("Text").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| Block::Text {
            text: text.to_string().into(),
        });
    }
    if let Some(mention) = item.get("Mention") {
        // The mention's resolved `content` (an attached file's whole body) is dropped — the URI is
        // the durable reference.
        let uri = match mention.get("uri") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return None,
        };
        return Some(Block::File {
            mime: None,
            path: None,
            source: Some(uri),
        });
    }
    if let Some(img) = item.get("Image") {
        // LanguageModelImage: {source: <base64>, size}.
        return Some(Block::Image {
            media_type: None,
            data_ref: img
                .get("source")
                .and_then(Value::as_str)
                .map(|s| crate::ir::truncate(s, 120)),
        });
    }
    None
}

fn agent_content_block(item: &Value) -> Option<Block> {
    if let Some(text) = item.get("Text").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| Block::Text {
            text: text.to_string().into(),
        });
    }
    if let Some(th) = item.get("Thinking") {
        return Some(Block::Thinking {
            text: th.get("text").and_then(Value::as_str).unwrap_or("").to_string().into(),
            signature: th
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            encrypted: None,
            redacted: false,
        });
    }
    if let Some(data) = item.get("RedactedThinking").and_then(Value::as_str) {
        return Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: Some(data.to_string()),
            redacted: true,
        });
    }
    if let Some(tu) = item.get("ToolUse") {
        return Some(Block::ToolUse {
            id: tu.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: tu.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: tu.get("input").cloned().unwrap_or(Value::Null).into(),
            namespace: None,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Whole-`Session` convenience over [`stream_thread`] for tests: stream into a CollectSink.
#[cfg(test)]
fn parse_thread(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_thread(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    /// The current full schema (base table + every ALTER-added column).
    const SCHEMA: &str = "
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            data_type TEXT NOT NULL,
            data BLOB NOT NULL,
            parent_id TEXT,
            worktree_branch TEXT,
            folder_paths TEXT,
            folder_paths_order TEXT,
            created_at TEXT
        );
    ";

    /// The original schema, before any ALTER TABLE (no parent_id/folder_paths/created_at).
    const SCHEMA_OLD: &str = "
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            data_type TEXT NOT NULL,
            data BLOB NOT NULL
        );
    ";

    fn mk(schema: &str) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(schema).unwrap();
        c
    }

    fn zstd_blob(json: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(json).unwrap();
        ruzstd::encoding::compress_to_vec(bytes.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Zed, PathBuf::from(":memory:"))
    }

    /// A representative legacy (0.2.0) thread: user text, assistant thinking+text+tool call with
    /// its result on the same message, project snapshot, model, profile.
    pub(super) fn legacy_thread() -> Value {
        serde_json::json!({
            "version": "0.2.0",
            "summary": "Fix the flaky test",
            "updated_at": "2025-08-08T21:08:04.712610Z",
            "messages": [
                {
                    "id": 0, "role": "user",
                    "segments": [{"type": "text", "text": "why is test_foo flaky?"}],
                    "tool_uses": [], "tool_results": [],
                    "context": "<context>attached file foo.rs</context>",
                    "creases": [], "is_hidden": false
                },
                {
                    "id": 1, "role": "assistant",
                    "segments": [
                        {"type": "thinking", "text": "let me look", "signature": "sig1"},
                        {"type": "text", "text": "Checking the test now."}
                    ],
                    "tool_uses": [{"id": "toolu_1", "name": "read_file", "input": {"path": "foo.rs"}}],
                    "tool_results": [{
                        "tool_use_id": "toolu_1", "is_error": false,
                        "content": {"Text": "fn test_foo() {}"},
                        "output": {"lines": 1}
                    }],
                    "context": "", "creases": [], "is_hidden": false
                },
                {
                    "id": 2, "role": "assistant",
                    "segments": [{"type": "text", "text": "It races on the shared tempdir."}],
                    "tool_uses": [], "tool_results": [],
                    "context": "", "creases": [], "is_hidden": false
                }
            ],
            "initial_project_snapshot": {
                "worktree_snapshots": [{
                    "worktree_path": "/Users/u/proj",
                    "git_state": {
                        "remote_url": "git@github.com:u/proj",
                        "head_sha": "abc123",
                        "current_branch": "main",
                        "diff": ""
                    }
                }]
            },
            "cumulative_token_usage": {"input_tokens": 10, "output_tokens": 20},
            "request_token_usage": [],
            "detailed_summary_state": {"Generated": {"text": "We fixed a race."}},
            "model": {"provider": "zed.dev", "model": "claude-sonnet-4-thinking"},
            "completion_mode": "burn",
            "tool_use_limit_reached": false,
            "profile": "write"
        })
    }

    /// A representative modern (0.3.0) thread: tagged-enum messages, Mention, tool_results map,
    /// Resume marker, subagent_context.
    pub(super) fn modern_thread() -> Value {
        serde_json::json!({
            "version": "0.3.0",
            "title": "Refactor the parser",
            "updated_at": "2026-06-10T19:57:44.766848Z",
            "messages": [
                {"User": {"id": "um-1", "content": [
                    {"Text": "split this module"},
                    {"Mention": {"uri": "file:///Users/u/proj/src/lib.rs", "content": "mod a;"}}
                ]}},
                "Resume",
                {"Agent": {
                    "content": [
                        {"Thinking": {"text": "plan the split", "signature": null}},
                        {"Text": "Splitting now."},
                        {"ToolUse": {"id": "toolu_9", "name": "edit_file", "raw_input": "{}",
                                     "input": {"path": "src/lib.rs"}, "is_input_complete": true}}
                    ],
                    "tool_results": {
                        "toolu_9": {"tool_use_id": "toolu_9", "tool_name": "edit_file",
                                     "is_error": true, "content": {"Text": "permission denied"},
                                     "output": null}
                    },
                    "reasoning_details": null
                }}
            ],
            "detailed_summary": null,
            "initial_project_snapshot": {
                "worktree_snapshots": [{
                    "worktree_path": "/Users/u/proj",
                    "git_state": {"remote_url": null, "head_sha": null, "current_branch": "dev", "diff": ""}
                }]
            },
            "cumulative_token_usage": {"input_tokens": 0, "output_tokens": 0},
            "request_token_usage": {},
            "model": {"provider": "openai", "model": "gpt-5-mini"},
            "profile": "write",
            "imported": false,
            "subagent_context": {"parent_thread_id": "parent-1", "depth": 1}
        })
    }

    fn insert(conn: &Connection, id: &str, summary: &str, data_type: &str, data: &[u8]) {
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, folder_paths, folder_paths_order, created_at) \
             VALUES (?1, ?2, '2025-08-08T21:08:04.712610+00:00', ?3, ?4, NULL, NULL, '2025-08-01T00:00:00+00:00')",
            rusqlite::params![id, summary, data_type, data],
        )
        .unwrap();
    }

    #[test]
    fn discover_reads_metadata_and_counts() {
        let c = mk(SCHEMA);
        insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
        // A second, newer thread with workspace folders but an empty summary.
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, folder_paths, folder_paths_order, created_at) \
             VALUES ('t2', '', '2026-06-10T19:57:44.766848+00:00', 'zstd', ?1, \
                     '/Users/u/aaa' || char(10) || '/Users/u/proj', '1,0', '2026-06-10T19:57:44.766848+00:00')",
            [zstd_blob(&modern_thread())],
        )
        .unwrap();

        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        // Ordered by updated_at DESC.
        assert_eq!(refs[0].id, "t2");
        assert_eq!(refs[1].id, "t1");

        let t1 = &refs[1];
        assert_eq!(t1.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(t1.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert!(t1.created_at.unwrap() < t1.updated_at.unwrap());
        // user + assistant turns only: 1 user + 2 assistant (the Tool turn doesn't count).
        assert_eq!(t1.message_count, 3);

        let t2 = &refs[0];
        assert_eq!(t2.title, None, "empty summary must not become a title");
        // Snapshot cwd wins over folder_paths.
        assert_eq!(t2.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        // 1 user + 1 assistant ("Resume" and the Tool turn don't count).
        assert_eq!(t2.message_count, 2);
    }

    #[test]
    fn parses_legacy_thread() {
        let c = mk(SCHEMA);
        insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
        let s = parse_thread(&c, &sref("t1")).unwrap();
        // Every Zed thread fact is nested under `extra["zed"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);

        assert_eq!(s.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
        assert_eq!(s.model.as_deref(), Some("zed.dev/claude-sonnet-4-thinking"));
        let git = s.git.as_ref().unwrap();
        assert_eq!(git.branch.as_deref(), Some("main"));
        assert_eq!(git.commit.as_deref(), Some("abc123"));
        assert_eq!(git.remote.as_deref(), Some("git@github.com:u/proj"));
        // Blob updated_at preferred; created_at from the column.
        assert!(s.created_at.is_some() && s.updated_at.is_some());
        let bag = s.harness_extra(Harness::Zed).expect("zed bag");
        assert_eq!(bag.get("profile").and_then(Value::as_str), Some("write"));
        assert_eq!(bag.get("completion_mode").and_then(Value::as_str), Some("burn"));
        assert_eq!(bag.get("thread_version").and_then(Value::as_str), Some("0.2.0"));
        assert_eq!(
            bag.get("detailed_summary").and_then(Value::as_str),
            Some("We fixed a race.")
        );
        assert!(bag.contains_key("cumulative_token_usage"));
        // Nested only: no flat facts leaked to the top level.
        for k in s.extra.keys() {
            assert!(
                k == "zed" || k == "_record",
                "unexpected top-level session extra key {k}"
            );
        }

        // user, assistant(+tool call), Tool, assistant.
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("why is test_foo flaky?"));
        assert_eq!(
            s.messages[0]
                .harness_extra(Harness::Zed)
                .and_then(|b| b.get("context"))
                .and_then(Value::as_str),
            Some("<context>attached file foo.rs</context>")
        );
        assert!(
            !s.messages[0].extra.contains_key("context"),
            "context nests under the zed bag"
        );
        assert_eq!(
            (s.messages[1].kind, s.messages[1].origin),
            (MessageKind::Reply, Origin::Model)
        );
        assert_eq!(
            (s.messages[2].kind, s.messages[2].origin),
            (MessageKind::ToolResult, Origin::Harness)
        );
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(&a.content[0], Block::Thinking { text, signature, .. }
            if text == "let me look" && signature.as_deref() == Some("sig1")));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "Checking the test now."));
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, input , ..}
            if id == "toolu_1" && name == "read_file"
            && input.resolve().unwrap().get("path").and_then(Value::as_str) == Some("foo.rs")));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, details, .. }
            if tool_use_id == "toolu_1" && content == "fn test_foo() {}" && !is_error
            && tool_name.as_deref() == Some("read_file")
            && details.as_ref().and_then(|d| d.get("lines")).and_then(Value::as_i64) == Some(1))
        );
        assert_eq!(s.messages[3].text().as_deref(), Some("It races on the shared tempdir."));
    }

    #[test]
    fn parses_modern_thread() {
        let c = mk(SCHEMA);
        insert(&c, "t2", "", "zstd", &zstd_blob(&modern_thread()));
        let s = parse_thread(&c, &sref("t2")).unwrap();
        crate::harness::assert_no_flat_keys(&s);

        assert_eq!(s.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(s.model.as_deref(), Some("openai/gpt-5-mini"));
        assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("dev"));
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-1"));
        assert_eq!(
            s.harness_extra(Harness::Zed)
                .and_then(|b| b.get("subagent_depth"))
                .and_then(Value::as_u64),
            Some(1)
        );
        // All-zero usage is not stashed.
        assert!(s
            .harness_extra(Harness::Zed)
            .is_none_or(|b| !b.contains_key("cumulative_token_usage")));

        // user, assistant, Tool ("Resume" skipped).
        assert_eq!(s.messages.len(), 3);
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!(u.id.as_deref(), Some("um-1"));
        assert_eq!(u.text().as_deref(), Some("split this module"));
        assert!(matches!(&u.content[1], Block::File { source: Some(uri), .. }
            if uri == "file:///Users/u/proj/src/lib.rs"));
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(&a.content[0], Block::Thinking { text, .. } if text == "plan the split"));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "Splitting now."));
        assert!(matches!(&a.content[2], Block::ToolUse { id, name, .. } if id == "toolu_9" && name == "edit_file"));
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(
            matches!(&t.content[0], Block::ToolResult { tool_use_id, content, is_error, tool_name, .. }
            if tool_use_id == "toolu_9" && content == "permission denied" && *is_error
            && tool_name.as_deref() == Some("edit_file"))
        );
    }

    #[test]
    fn v010_tool_results_on_next_user_message_become_a_tool_turn() {
        // 0.1.0 carried tool_results on the user message *after* the assistant call.
        let thread = serde_json::json!({
            "version": "0.1.0",
            "summary": "old",
            "updated_at": "2025-05-01T00:00:00Z",
            "messages": [
                {"id": 0, "role": "assistant",
                 "segments": [{"type": "text", "text": "calling"}],
                 "tool_uses": [{"id": "c1", "name": "shell", "input": {"cmd": "ls"}}],
                 "tool_results": []},
                {"id": 1, "role": "user", "segments": [],
                 "tool_uses": [],
                 "tool_results": [{"tool_use_id": "c1", "is_error": false,
                                    "content": {"Text": "file_a"}, "output": null}]}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "old", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 2, "no empty user turn; results become a Tool turn");
        assert_eq!(s.messages[0].role, Role::Assistant);
        assert_eq!(s.messages[1].role, Role::Tool);
        assert!(
            matches!(&s.messages[1].content[0], Block::ToolResult { tool_name, content, .. }
            if tool_name.as_deref() == Some("shell") && content == "file_a")
        );
    }

    #[test]
    fn versionless_text_messages_parse() {
        // The oldest blobs have no `version` and messages carry `text` instead of `segments`.
        let thread = serde_json::json!({
            "summary": "ancient",
            "updated_at": "2025-03-01T00:00:00Z",
            "messages": [
                {"id": 0, "role": "user", "text": "hello from the past"},
                {"id": 1, "role": "assistant", "text": "still parses"}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "ancient", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello from the past"));
        assert_eq!(s.messages[1].text().as_deref(), Some("still parses"));
        assert!(s
            .harness_extra(Harness::Zed)
            .and_then(|b| b.get("thread_version"))
            .is_none());
    }

    #[test]
    fn metadata_sort_preserves_ties_and_without_rowid_fallback() {
        for schema in [
            SCHEMA.to_string(),
            SCHEMA.replace("        );", "        ) WITHOUT ROWID;"),
        ] {
            let c = mk(&schema);
            let data = zstd_blob(&legacy_thread());
            for id in ["z", "a", "m"] {
                insert(&c, id, id, "zstd", &data);
            }
            let expected: Vec<String> = c
                .prepare("SELECT id, data FROM threads ORDER BY updated_at DESC")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let actual = discover_db(&c, Path::new(":memory:")).unwrap();
            assert_eq!(
                actual.iter().map(|r| &r.id).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            for r in actual {
                assert_eq!(r.message_count, 3);
                assert_eq!(parse_thread(&c, &r).unwrap().messages.len(), 4);
            }
            for text in [
                "not a blob".to_string(),
                serde_json::to_string(&legacy_thread()).unwrap(),
            ] {
                c.execute(
                    "UPDATE threads SET data = ?1, data_type = 'json' WHERE id = 'a'",
                    [&text],
                )
                .unwrap();
                assert_eq!(
                    c.query_row("SELECT typeof(data) FROM threads WHERE id = 'a'", [], |row| row
                        .get::<_, String>(0))
                        .unwrap(),
                    "text"
                );
                let unreadable = parse_thread(&c, &sref("a")).unwrap();
                assert!(unreadable.messages.is_empty());
                assert!(unreadable.harness_extra(Harness::Zed).is_none());
                let listed = discover_db(&c, Path::new(":memory:"))
                    .unwrap()
                    .into_iter()
                    .find(|r| r.id == "a")
                    .unwrap();
                assert_eq!(listed.message_count, 0);
                assert_eq!(listed.cwd, None);
            }
        }
    }

    #[test]
    fn read_snapshot_allows_wal_writer_and_keeps_both_passes_consistent() {
        let world = cv_testkit::World::new("zed-snapshot");
        let path = world.base.join("threads.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute_batch(SCHEMA).unwrap();
        insert(&writer, "t", "old", "zstd", &zstd_blob(&legacy_thread()));
        let reader = open_ro(&path).unwrap();
        struct UpdatingSink<'a> {
            writer: &'a Connection,
            messages: Vec<Message>,
        }
        impl MessageSink for UpdatingSink<'_> {
            fn meta(&mut self, _: &Session) {
                self.writer
                    .execute(
                        "UPDATE threads SET data = ?1 WHERE id = 't'",
                        [zstd_blob(&modern_thread())],
                    )
                    .unwrap();
                self.writer
                    .execute(
                        "UPDATE threads SET summary = ?1 WHERE id = 't'",
                        [uuid::Uuid::new_v4().to_string()],
                    )
                    .unwrap();
            }
            fn message(&mut self, message: Message) -> Flow {
                self.messages.push(message);
                Flow::Continue
            }
        }
        let mut sink = UpdatingSink {
            writer: &writer,
            messages: Vec::new(),
        };
        let session = stream_thread(&reader, &sref("t"), &mut sink).unwrap();
        assert_eq!(session.model.as_deref(), Some("zed.dev/claude-sonnet-4-thinking"));
        assert_eq!(sink.messages.len(), 4);
        assert_eq!(sink.messages[0].text().as_deref(), Some("why is test_foo flaky?"));
        // A live WAL store is read through a private snapshot (cv never touches its -shm), so
        // even an unchecked second pass sees the snapshot, not the writer's later commit.
        assert_eq!(
            parse_thread(&reader, &sref("t")).unwrap().model.as_deref(),
            Some("zed.dev/claude-sonnet-4-thinking")
        );
        // The internal SQLite snapshot remains consistent, but publication through the real
        // adapter must reject a source changed by a callback rather than recapture its identity.
        assert!(reader.read(|conn| parse_thread(conn, &sref("t"))).is_err());
        let adapter = Zed::with_root(&world.base);
        let mut reference = sref("t");
        reference.path = path;
        let failure = adapter
            .stream(&reference, &ParseOptions::default(), &mut sink)
            .unwrap_err();
        assert!(failure.to_string().contains("dependencies changed"));
    }

    #[test]
    fn json_data_type_rows_parse_uncompressed() {
        let c = mk(SCHEMA);
        let raw = serde_json::to_vec(&legacy_thread()).unwrap();
        insert(&c, "t", "json row", "json", &raw);
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 4);
    }

    #[test]
    fn corrupt_blob_is_tolerated_everywhere() {
        let c = mk(SCHEMA);
        insert(&c, "bad", "corrupt one", "zstd", b"\x28\xb5\x2f\xfdnot really zstd");
        insert(&c, "good", "fine one", "zstd", &zstd_blob(&legacy_thread()));

        // discover lists both; the corrupt one just has no count/cwd.
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        let bad = refs.iter().find(|r| r.id == "bad").unwrap();
        assert_eq!(bad.message_count, 0);
        assert_eq!(bad.title.as_deref(), Some("corrupt one"));

        // parse returns the row's metadata + a flag, never an error.
        let s = parse_thread(&c, &sref("bad")).unwrap();
        assert!(s.messages.is_empty());
        assert_eq!(
            s.harness_extra(Harness::Zed).and_then(|b| b.get("undecodable_blob")),
            Some(&Value::Bool(true))
        );
        assert_eq!(s.title.as_deref(), Some("corrupt one"));
    }

    #[test]
    fn empty_thread_parses_to_zero_messages() {
        let thread = serde_json::json!({
            "version": "0.2.0",
            "summary": "New Thread",
            "updated_at": "2025-08-08T21:06:36.188213Z",
            "messages": []
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "New Thread", "zstd", &zstd_blob(&thread));
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs[0].message_count, 0);
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert!(s.messages.is_empty());
        assert_eq!(s.title.as_deref(), Some("New Thread"));
    }

    #[test]
    fn old_schema_without_added_columns_still_works() {
        let c = mk(SCHEMA_OLD);
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data) \
             VALUES ('t', 'pre-alter', '2025-07-05T02:22:05.419868+00:00', 'zstd', ?1)",
            [zstd_blob(&legacy_thread())],
        )
        .unwrap();
        let refs = discover_db(&c, Path::new(":memory:")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].message_count, 3);
        assert!(refs[0].created_at.is_none(), "no created_at column in old schema");
        let s = parse_thread(&c, &sref("t")).unwrap();
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/u/proj")));
    }

    #[test]
    fn worktree_branch_fills_missing_snapshot_branch() {
        let c = mk(SCHEMA);
        for (id, thread, expected) in [
            ("missing", serde_json::json!({"messages":[]}), "isolated-branch"),
            (
                "snapshot",
                serde_json::json!({"initial_project_snapshot":{"worktree_snapshots":[{"git_state":{"current_branch":"snapshot-branch","head_sha":"commit"}}]},"messages":[]}),
                "snapshot-branch",
            ),
        ] {
            insert(&c, id, "branch", "json", &serde_json::to_vec(&thread).unwrap());
            c.execute("UPDATE threads SET worktree_branch='isolated-branch' WHERE id=?1", [id])
                .unwrap();
            let session = parse_thread(&c, &sref(id)).unwrap();
            assert_eq!(session.git.unwrap().branch.as_deref(), Some(expected));
        }
    }

    #[test]
    fn parent_id_column_links_subagent_threads() {
        let c = mk(SCHEMA);
        c.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, parent_id) \
             VALUES ('child', 'sub work', '2026-06-10T00:00:00+00:00', 'zstd', ?1, 'parent-thread')",
            [zstd_blob(&legacy_thread())],
        )
        .unwrap();
        let s = parse_thread(&c, &sref("child")).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("parent-thread"));
    }

    /// The modern `Compaction` variant becomes a `CompactionSummary` turn (no flat marker).
    #[test]
    fn modern_compaction_becomes_compaction_summary() {
        let thread = serde_json::json!({
            "version": "0.3.0",
            "title": "compacted",
            "updated_at": "2026-06-10T00:00:00Z",
            "messages": [
                {"User": {"id": "u", "content": [{"Text": "hi"}]}},
                {"Compaction": {"Summary": "we did a lot of things earlier"}}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "compacted", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        let comp = s
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::CompactionSummary)
            .expect("compaction summary");
        assert_eq!(comp.role, Role::System);
        assert_eq!(comp.origin, Origin::Harness);
        assert!(comp.text().unwrap().contains("earlier"));
        assert!(
            !comp.extra.contains_key("compaction"),
            "kind carries it, not a flat key"
        );
    }

    /// A hidden legacy user turn ("Continue where you left off") is harness-injected context.
    #[test]
    fn hidden_user_turn_is_injected_context() {
        let thread = serde_json::json!({
            "version": "0.2.0",
            "summary": "resumed",
            "updated_at": "2026-06-10T00:00:00Z",
            "messages": [
                {"id": 0, "role": "user", "is_hidden": true,
                 "segments": [{"type": "text", "text": "Continue where you left off"}]},
                {"id": 1, "role": "assistant", "segments": [{"type": "text", "text": "ok"}]}
            ]
        });
        let c = mk(SCHEMA);
        insert(&c, "t", "resumed", "zstd", &zstd_blob(&thread));
        let s = parse_thread(&c, &sref("t")).unwrap();
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!((u.kind, u.origin), (MessageKind::InjectedContext, Origin::Harness));
        assert_eq!(
            u.harness_extra(Harness::Zed).and_then(|b| b.get("is_hidden")),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn folder_paths_order_restores_primary_folder() {
        // Lexicographic paths "/a\n/z" with order "1,0" → the user's first folder is "/z".
        assert_eq!(
            first_folder_path(Some("/a\n/z"), Some("1,0")).as_deref(),
            Some(Path::new("/z"))
        );
        assert_eq!(
            first_folder_path(Some("/a\n/z"), None).as_deref(),
            Some(Path::new("/a"))
        );
        assert_eq!(first_folder_path(Some(""), None), None);
        assert_eq!(first_folder_path(None, Some("0")), None);
        // An out-of-range order index degrades to the first path.
        assert_eq!(
            first_folder_path(Some("/a"), Some("7")).as_deref(),
            Some(Path::new("/a"))
        );
    }

    #[test]
    fn stream_equals_parse_on_synthetic_db() {
        let _world = cv_testkit::World::in_process("zed-stream-parse");
        // The corpus-wide invariant, asserted here on a fixture so it runs without real data:
        // parse() and collect(stream()) are the same bytes.
        let dir = std::env::temp_dir().join(format!("cv-zed-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("threads.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            insert(&c, "t1", "Fix the flaky test", "zstd", &zstd_blob(&legacy_thread()));
            insert(&c, "t2", "", "zstd", &zstd_blob(&modern_thread()));
        }
        let zed = Zed { db: Some(db.clone()) };
        let refs = zed.discover().unwrap();
        assert_eq!(refs.len(), 2);
        for r in &refs {
            let parsed = zed.parse(r).unwrap();
            let mut sink = crate::stream::CollectSink::default();
            let mut streamed = zed.stream(r, &crate::stream::ParseOptions::full(), &mut sink).unwrap();
            streamed.messages = sink.messages;
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&streamed).unwrap(),
                "parse != collect(stream) for {}",
                r.id
            );
            // And the discover contract: message_count == parsed user+assistant turns.
            let ua = parsed
                .messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count();
            assert_eq!(r.message_count, ua, "message_count contract for {}", r.id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checked_stream_matches_owned_modes_and_without_rowid() {
        let world = cv_testkit::World::new("zed-checked-stream");
        let path = world.base.join("threads.db");
        for without_rowid in [false, true] {
            let schema = if without_rowid {
                SCHEMA.replace("        );", "        ) WITHOUT ROWID;")
            } else {
                SCHEMA.to_string()
            };
            let writer = Connection::open(&path).unwrap();
            writer.execute_batch("DROP TABLE IF EXISTS threads;").unwrap();
            writer.execute_batch(&schema).unwrap();
            for (id, value) in [("legacy", legacy_thread()), ("modern", modern_thread())] {
                insert(&writer, id, id, "zstd", &zstd_blob(&value));
            }
            insert(
                &writer,
                "invalid",
                "invalid",
                "json",
                br#"{"messages":[{"role":"user","text":"ok"}],"ignored":"\ud800"}"#,
            );
            let zed = Zed { db: Some(path.clone()) };
            for id in ["legacy", "modern", "invalid"] {
                let mut reference = sref(id);
                reference.path = path.clone();
                let expected = zed.parse(&reference).unwrap();
                for options in [
                    ParseOptions::lazy(),
                    ParseOptions::facts(),
                    ParseOptions::lazy_extra(),
                    ParseOptions {
                        spans: true,
                        ..ParseOptions::complete()
                    },
                ] {
                    let mut sink = crate::stream::CollectSink::default();
                    let mut actual = zed.stream(&reference, &options, &mut sink).unwrap();
                    actual.messages = sink.messages;
                    assert_eq!(
                        serde_json::to_vec(&actual).unwrap(),
                        serde_json::to_vec(&expected).unwrap(),
                        "id={id} without_rowid={without_rowid}"
                    );
                }
            }
        }
    }

    #[test]
    fn checked_stream_retained_ranges_reject_wal_changes() {
        let world = cv_testkit::World::new("zed-checked-wal");
        let path = world.base.join("threads.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        writer.execute_batch(SCHEMA).unwrap();
        let body = "large checked body\n".repeat(4096);
        let value = serde_json::json!({"messages":[{"role":"user","text":body}]});
        insert(&writer, "t", "t", "zstd", &zstd_blob(&value));
        let zed = Zed { db: Some(path.clone()) };
        let mut reference = sref("t");
        reference.path = path;
        let mut sink = crate::stream::CollectSink::default();
        zed.stream(&reference, &ParseOptions::lazy(), &mut sink).unwrap();
        let Block::Text { text } = &sink.messages[0].content[0] else {
            panic!("text")
        };
        assert!(matches!(text, crate::lazy::Text::Source(_)));
        assert_eq!(
            text.try_resolve(&crate::lazy::Resolver::new(None)).unwrap().as_ref(),
            body
        );
        writer
            .execute("UPDATE threads SET summary = 'changed' WHERE id = 't'", [])
            .unwrap();
        assert!(text.try_resolve(&crate::lazy::Resolver::new(None)).is_err());
    }

    #[test]
    fn missing_db_discovers_nothing() {
        let zed = Zed { db: None };
        assert!(zed.discover().unwrap().is_empty());
        assert!(zed.roots().is_empty());
    }

    /// Smoke over the real local Zed install (skipped in CI; run with --ignored --nocapture).
    #[test]
    #[ignore = "reads the live ~/Library/Application Support/Zed/threads/threads.db"]
    fn real_zed_threads_discover_and_parse() {
        let zed = Zed::detect();
        let Some(root) = zed.roots().into_iter().next() else {
            eprintln!("no Zed install here; nothing to do");
            return;
        };
        let refs = zed.discover().unwrap();
        eprintln!("zed: {} thread(s) in {}", refs.len(), root.display());
        for r in &refs {
            let s = zed.parse(r).unwrap();
            crate::harness::assert_no_flat_keys(&s);
            eprintln!(
                "  {} [{}] {:?} cwd={:?} msgs={} (count={})",
                &r.id[..8.min(r.id.len())],
                s.model.as_deref().unwrap_or("?"),
                s.label(),
                s.cwd,
                s.messages.len(),
                r.message_count
            );
        }
    }
}
