//! Cursor adapter — IDE chat/composer `state.vscdb` and Agent CLI blob stores (SQLite).
//!
//! See `docs/FORMATS.md`. Cursor stores conversations across two SQLite databases:
//!
//!   * **Global:** `<AppSupport>/Cursor/User/globalStorage/state.vscdb`. Newer Cursor keeps the
//!     actual chat/composer content here, in a `cursorDiskKV(key TEXT, value BLOB)` table:
//!       - `composerData:<composerId>` — one row per composer/chat thread (metadata: `name`,
//!         `createdAt`, `lastUpdatedAt`, `unifiedMode`, `_v`, plus either an inline `conversation`
//!         array (older `_v`) or a `fullConversationHeadersOnly` list of `{bubbleId,type}` pointers
//!         (newer `_v`)).
//!       - `bubbleId:<composerId>:<bubbleId>` — one row per message ("bubble"): `type` (1=user,
//!         2=assistant), `text`, `richText` (Lexical JSON), `thinking:{text}`, `toolFormerData`
//!         (a tool call+result), `tokenCount:{inputTokens,outputTokens}`.
//!   * **Per-workspace:** `<AppSupport>/Cursor/User/workspaceStorage/<hash>/state.vscdb`, with a
//!     sibling `workspace.json` giving the workspace folder URI (= the cwd). Its `ItemTable` has
//!     `composer.composerData` → `{allComposers:[{composerId,name,createdAt,lastUpdatedAt,...}]}`
//!     which links composerIds to that workspace (and hence a cwd). Older Cursor stored chat blobs
//!     in the workspace DB itself (e.g. `workbench.panel.aichat.view.aichat.chatdata`).
//!
//! Strategy: build a composerId→cwd map by scanning every workspace's `composer.composerData`,
//! then enumerate every global `composerData:` thread. Threads not linked to any workspace (common:
//! ~20% on the dev machine) are still surfaced, just with `cwd = None`. The legacy in-workspace
//! `aichat.chatdata` shape is also parsed when present so old installs aren't dropped.
//!
//! Tolerance: Cursor is closed-source and the schema churns across versions (`_v` 1..=10+ seen on
//! one machine). Anything absent is skipped gracefully — a missing table, key, or field never
//! panics and never fails the whole session.

use super::store::sqlite::open_ro;
use super::{ts_from_epoch, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use rusqlite::{Connection, Statement};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod cli;
mod composer;
mod legacy;
mod selected;
mod source;

pub struct Cursor {
    /// `<AppSupport>/Cursor/User` if it exists.
    user_dir: Option<PathBuf>,
    cli_home: Option<PathBuf>,
}

impl Cursor {
    pub fn detect() -> Self {
        // CURSOR_USER_DIR overrides the platform default (used by tests / non-mac).
        let user_dir = crate::env::var_os("CURSOR_USER_DIR")
            .map(PathBuf::from)
            .or_else(default_user_dir)
            .filter(|p| p.exists());
        let cli_home = crate::env::var_os("CURSOR_CONFIG_DIR")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                crate::env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.to_string_lossy().trim().is_empty())
                    .map(|base| PathBuf::from(base).join("cursor"))
            })
            .or_else(|| crate::env::home_dir().map(|home| home.join(".cursor")))
            .filter(|path| path.is_dir());
        Cursor { user_dir, cli_home }
    }

    /// Use an explicit Cursor/User directory without consulting installed homes.
    pub fn with_user_dir(dir: &Path) -> Self {
        Self {
            user_dir: Some(dir.to_path_buf()).filter(|p| p.exists()),
            cli_home: None,
        }
    }

    /// Use only an explicit Cursor Agent CLI configuration home (`.cursor`), including chats
    /// and ACP sessions. Never consult the installed IDE or other harness homes.
    pub fn with_cli_home(home: &Path) -> Self {
        Self {
            user_dir: None,
            cli_home: Some(home.to_path_buf()),
        }
    }

    fn global_db(&self) -> Option<PathBuf> {
        self.user_dir
            .as_ref()
            .map(|d| d.join("globalStorage").join("state.vscdb"))
            .filter(|p| p.exists())
    }

    fn workspace_dirs(&self) -> Vec<PathBuf> {
        let Some(ws_root) = self
            .user_dir
            .as_ref()
            .map(|d| d.join("workspaceStorage"))
            .filter(|p| p.exists())
        else {
            return vec![];
        };
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&ws_root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// Default macOS/Linux/Windows location of `Cursor/User`.
fn default_user_dir() -> Option<PathBuf> {
    let home = crate::env::home_dir()?;
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");
    #[cfg(target_os = "windows")]
    let base = crate::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"));
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = crate::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Some(base.join("Cursor").join("User"))
}

impl Adapter for Cursor {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }

    /// `Cursor/User`: the dir under which both the global `state.vscdb` and every
    /// `workspaceStorage/<hash>/state.vscdb` live. The db files are catalog file-watches.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<_> = self.user_dir.clone().into_iter().collect();
        if let Some(home) = &self.cli_home {
            roots.extend(cli::roots(home));
        }
        roots
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        // Map composerId -> cwd by scanning every workspace's composer.composerData. Also collect
        // metadata fallbacks (name/created) and legacy in-workspace chat threads.
        let cwd_map = self.build_cwd_map();

        if let Some(db) = self.global_db() {
            if let Ok(conn) = open_ro(&db) {
                if let Ok(rows) = conn.read(|conn| {
                    let mut rows = Vec::new();
                    discover_global(conn, &db, &cwd_map, &mut rows);
                    Ok(rows)
                }) {
                    out.extend(rows);
                }
            }
        }

        // Legacy: old Cursor kept chat blobs inside the workspace DB itself.
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let cwd = workspace_cwd(&ws);
            if let Ok(conn) = open_ro(&db) {
                if let Ok(rows) = conn.read(|conn| {
                    let mut rows = Vec::new();
                    discover_legacy_workspace(conn, &db, cwd.as_deref(), &mut rows);
                    Ok(rows)
                }) {
                    out.extend(rows);
                }
            }
        }
        if let Some(home) = &self.cli_home {
            cli::discover(home, &mut out);
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // `path` is the DB the thread lives in. Legacy refs carry a `legacy:` id prefix.
            for suffix in ["-wal", "-journal"] {
                let mut path = r.path.as_os_str().to_os_string();
                path.push(suffix);
                sink.dependency(&PathBuf::from(path));
            }
            let conn = open_ro(&r.path)?;
            conn.read(|conn| {
                if r.id.starts_with("cli:") {
                    return cli::stream(conn, r, opts, sink);
                }
                if let Some(legacy_id) = r.id.strip_prefix("legacy:") {
                    return stream_legacy(conn, r, legacy_id, sink);
                }
                // Modern IDE's selected builder retains the complete emitted field roster;
                // rich lazy output can keep checked carriers without owning every bubble.
                if opts.spans && !opts.complete {
                    return selected::global_with_reads(conn, r, conn.reads(), sink);
                }
                stream_global(conn, r, sink)
            })
        })
    }
}

impl Cursor {
    /// composerId -> cwd, harvested from every workspace's `composer.composerData.allComposers`.
    fn build_cwd_map(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        for ws in self.workspace_dirs() {
            let db = ws.join("state.vscdb");
            if !db.exists() {
                continue;
            }
            let Some(cwd) = workspace_cwd(&ws) else {
                continue;
            };
            let Ok(conn) = open_ro(&db) else { continue };
            let Ok(Some(v)) = conn.read(|conn| {
                Ok(item_table_get(conn, "composer.composerData")
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok()))
            }) else {
                continue;
            };
            if let Some(all) = v.get("allComposers").and_then(Value::as_array) {
                for c in all {
                    if let Some(id) = c.get("composerId").and_then(Value::as_str) {
                        map.entry(id.to_string()).or_insert_with(|| cwd.clone());
                    }
                }
            }
        }
        map
    }
}

/// Read a single ItemTable value (TEXT or BLOB) as a UTF-8 string.
fn item_table_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        // value may be stored as TEXT or BLOB depending on version.
        match row.get::<_, String>(0) {
            Ok(s) => Ok(s),
            Err(_) => row
                .get::<_, Vec<u8>>(0)
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        }
    })
    .ok()
}

/// Read a cursorDiskKV value as a UTF-8 string.
fn disk_kv_get(stmt: &mut Statement<'_>, key: &str) -> Option<String> {
    stmt.query_row([key], |row| match row.get::<_, String>(0) {
        Ok(s) => Ok(s),
        Err(_) => row
            .get::<_, Vec<u8>>(0)
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    })
    .ok()
}

/// Resolve the cwd for a workspace dir from its sibling `workspace.json` `folder` URI.
fn workspace_cwd(ws: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(ws.join("workspace.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    // `folder` is a single-root folder URI; multi-root workspaces use `workspace` (a .json path) —
    // we only resolve the simple single-folder case to a cwd.
    let uri = v.get("folder").and_then(Value::as_str)?;
    file_uri_to_path(uri)
}

/// `file:///Users/x/p%20q` -> `/Users/x/p q`. Returns None for non-file URIs.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an authority component if present (file://host/path); keep the leading '/'.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => rest,
    };
    Some(PathBuf::from(percent_decode(path)))
}

/// Minimal percent-decoder (Cursor URIs use %XX for spaces etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Global (cursorDiskKV) format
// ---------------------------------------------------------------------------

// LIKE is case-insensitive by default and cannot use Cursor's binary key index. Bound the
// exact prefix instead, and keep the table-scan row order so timestamp ties remain stable.
const COMPOSER_ROWS: &str = "SELECT key FROM cursorDiskKV \
    WHERE key >= 'composerData:' AND key < 'composerData;' ORDER BY rowid";
const COMPOSER_ROWS_WITHOUT_ROWID: &str = "SELECT key FROM cursorDiskKV \
    WHERE key >= 'composerData:' AND key < 'composerData;'";

fn discover_global(conn: &Connection, db: &Path, cwd_map: &HashMap<String, PathBuf>, out: &mut Vec<SessionRef>) {
    // Bail quietly if this DB has no cursorDiskKV table (older layout).
    let mut stmt = match conn
        .prepare(COMPOSER_ROWS)
        .or_else(|_| conn.prepare(COMPOSER_ROWS_WITHOUT_ROWID))
    {
        Ok(s) => s,
        Err(_) => return,
    };
    let rows = stmt.query_map([], |row| row.get::<_, String>(0));
    let Ok(rows) = rows else { return };

    for key in rows.flatten() {
        let Some(composer_id) = key.strip_prefix("composerData:") else {
            continue;
        };
        let Ok(source) = source::cell(conn, "cursorDiskKV", &key) else {
            continue;
        };
        let Some(header) = composer::header(&source) else {
            continue;
        };
        let d = &header.metadata;
        let count = header.count();
        // Skip threads that have no messages at all (Cursor leaves empty composer shells around).
        if count == 0 {
            continue;
        }
        let created = d.get("createdAt").and_then(Value::as_i64);
        let updated = d.get("lastUpdatedAt").and_then(Value::as_i64).or(created);
        let title = d
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| crate::ir::truncate(s, 80));
        out.push(SessionRef {
            id: composer_id.to_string(),
            harness: Harness::Cursor,
            path: db.to_path_buf(),
            cwd: cwd_map.get(composer_id).cloned(),
            title,
            created_at: created.and_then(ts_from_epoch),
            updated_at: updated.and_then(ts_from_epoch),
            message_count: count,
        });
    }
}

/// Number of messages in a composerData record, across both shapes.
#[cfg(test)]
fn conversation_len(d: &Value) -> usize {
    if let Some(h) = d.get("fullConversationHeadersOnly").and_then(Value::as_array) {
        return h.len();
    }
    d.get("conversation")
        .and_then(Value::as_array)
        .map(|c| c.len())
        .unwrap_or(0)
}

fn stream_global(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
    let key = format!("composerData:{}", r.id);
    let source = source::cell(conn, "cursorDiskKV", &key)?;
    let header = composer::header(&source).context("composerData JSON")?;
    let d = &header.metadata;
    let mut lookup = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;

    let mut session = global_session(d, r);

    // Session model: the latest message's model (whole-Vec `rev().find_map` originally). Tracked
    // incrementally here — each emitted message overwrites it when it carries a model — so the last
    // one wins without retaining the body. Falls back to the composer's modelConfig below.
    let mut last_model: Option<String> = None;
    let mut emit = |sink: &mut dyn MessageSink, m: Message| -> Flow {
        if m.model.is_some() {
            last_model = m.model.clone();
        }
        sink.message(m)
    };

    // The immutable, fully validated source replays only the authoritative selected array.
    header.replay(&source, &mut |value, external| {
        let bubble = if external {
            let Some(bubble_id) = value.get("bubbleId").and_then(Value::as_str) else {
                return Flow::Continue;
            };
            let bkey = format!("bubbleId:{}:{}", r.id, bubble_id);
            let Some(braw) = disk_kv_get(&mut lookup, &bkey) else {
                return Flow::Continue;
            };
            let Ok(bubble) = serde_json::from_str::<Value>(&braw) else {
                return Flow::Continue;
            };
            bubble
        } else {
            value
        };
        if let Some(message) = bubble_to_message(&bubble) {
            emit(sink, message)
        } else {
            Flow::Continue
        }
    })?;

    session.model = last_model.or_else(|| {
        d.pointer("/modelConfig/modelName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });

    Ok(session)
}

fn global_session(d: &Value, r: &SessionRef) -> Session {
    let created = d.get("createdAt").and_then(Value::as_i64);
    let updated = d.get("lastUpdatedAt").and_then(Value::as_i64).or(created);
    let title = d
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| r.title.clone());

    let mut session = Session {
        cwd: r.cwd.clone(),
        title,
        created_at: created.and_then(ts_from_epoch).or(r.created_at),
        updated_at: updated.and_then(ts_from_epoch).or(r.updated_at),
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Cursor)
    };
    // Sub-agent lineage: a composer spawned as a sub-agent records `subagentInfo`
    // (`parentComposerId`, the spawning `toolCallId`, the `subagentTypeName` nickname).
    if let Some(si) = d.get("subagentInfo").filter(|v| !v.is_null()) {
        if let Some(p) = si
            .get("parentComposerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.parent = Some(p.to_string());
        }
        if let Some(tc) = si.get("toolCallId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            session.lineage.spawned_by_tool_use = Some(tc.to_string());
        }
        if let Some(name) = si
            .get("subagentTypeName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            session.lineage.agent_path = Some(name.to_string());
        }
    }
    // `unifiedMode` (chat/agent/edit) has no first-class IR home. The composer's is a *session*
    // fact — the mode the thread was opened in — so it lands in the session bag; a bubble that
    // carries its own overrides it for that message only (see `bubble_to_message`), which is how
    // Cursor records a mode switched mid-thread.
    if let Some(value) = d.get("unifiedMode") {
        let metadata = unified_mode_metadata(value);
        if !metadata.is_empty() {
            session.harness_extra_mut(Harness::Cursor).extend(metadata);
        }
    }

    session
}

/// Convert one Cursor "bubble" (message) object into an IR [`Message`].
/// `type`: 1 = user, 2 = assistant. Returns None for an entirely empty bubble.
fn bubble_to_message(b: &Value) -> Option<Message> {
    let btype = b.get("type").and_then(Value::as_i64).unwrap_or(0);
    let role = match btype {
        1 => Role::User,
        2 => Role::Assistant,
        _ => Role::Assistant,
    };
    let mut m = Message::new(role);
    m.id = b.get("bubbleId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = b.get("createdAt").and_then(bubble_timestamp);

    // Thinking block (assistant): {text: "..."} (and sometimes signature).
    if let Some(th) = b.get("thinking") {
        let text = th.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let signature = th
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        if !text.is_empty() || signature.is_some() {
            m.content.push(Block::Thinking {
                text: text.into(),
                signature,
                encrypted: None,
                redacted: false,
            });
        }
    }

    // Primary text. Fall back to richText (Lexical JSON) if `text` is empty.
    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
    if !text.is_empty() {
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
    } else if let Some(rt) = b.get("richText") {
        if let Some(extracted) = lexical_plain_text(rt) {
            if !extracted.trim().is_empty() {
                m.content.push(Block::Text { text: extracted.into() });
            }
        }
    }

    // A tool call + its result, stored together in `toolFormerData`.
    if let Some(tf) = b.get("toolFormerData") {
        push_tool_former(&mut m, tf);
    }

    // Model on assistant bubbles: `modelInfo.modelName` (newer) or a bare `modelType`/`model`.
    if let Some(model) = b
        .pointer("/modelInfo/modelName")
        .or_else(|| b.get("modelType"))
        .or_else(|| b.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        m.model = Some(model.to_string());
    }
    if let Some(tc) = b.get("tokenCount") {
        let input = tc.get("inputTokens").and_then(Value::as_u64);
        let output = tc.get("outputTokens").and_then(Value::as_u64);
        if input.is_some() || output.is_some() {
            m.usage = Some(Usage {
                input_tokens: input,
                output_tokens: output,
                ..Usage::default()
            });
        }
    }
    // A bubble's own `unifiedMode` overrides the composer's (which `stream_global` puts on the
    // session bag) for this message only — a mode switched mid-thread.
    if let Some(value) = b.get("unifiedMode") {
        let metadata = unified_mode_metadata(value);
        if !metadata.is_empty() {
            m.harness_extra_mut(Harness::Cursor).extend(metadata);
        }
    }

    (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
}

/// IDE bubbles use RFC3339 creation times; tolerate older numeric epoch records too.
fn bubble_timestamp(value: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match value {
        Value::String(text) => super::parse_ts(text),
        Value::Number(number) => number.as_i64().and_then(ts_from_epoch),
        _ => None,
    }
}

/// Numeric bubble modes use the installed Cursor 3.18.25 protocol enum (see the
/// read-only evidence in tools/harness-corpus/cursor/README.md). Keep the original
/// number alongside its readable mode, including unknown future enum values.
fn unified_mode_metadata(value: &Value) -> Map<String, Value> {
    let mut extra = Map::new();
    let mode = match value {
        Value::String(s) => Some(s.as_str()).filter(|s| !s.is_empty()),
        Value::Number(_) => {
            extra.insert("unified_mode_raw".into(), value.clone());
            match value.as_i64() {
                Some(0) => Some("unspecified"),
                Some(1) => Some("chat"),
                Some(2) => Some("agent"),
                Some(3) => Some("edit"),
                Some(4) => Some("custom"),
                Some(5) => Some("plan"),
                Some(6) => Some("debug"),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(mode) = mode {
        extra.insert("unified_mode".into(), mode.into());
    }
    extra
}

/// `toolFormerData` carries a single tool invocation and (usually) its result:
/// `{name, toolCallId, params|rawArgs (JSON string), result (JSON string), status, error}`.
fn push_tool_former(m: &mut Message, tf: &Value) {
    let name = tf.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
    let id = tf
        .get("toolCallId")
        .and_then(Value::as_str)
        .or_else(|| tf.get("modelCallId").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    // Arguments: prefer the model-issued `rawArgs`, fall back to normalized `params`.
    let input = tf
        .get("rawArgs")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .or_else(|| {
            tf.get("params")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
        })
        .or_else(|| tf.get("params").cloned())
        .unwrap_or(Value::Null);
    let tool_name = name.clone();
    m.content.push(Block::ToolUse {
        id: id.clone(),
        name,
        input: input.into(),
        namespace: None,
    });

    // Result: a JSON string (or nested obj) plus a status/error flag.
    let status = tf.get("status").and_then(Value::as_str).unwrap_or("");
    // Native IDE terminal calls can finish successfully as a tool invocation while the
    // command fails: 3.24.12 records status=completed and additionalData.status=error.
    let command_failed = tf
        .pointer("/additionalData/status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("error"));
    let is_error = status.eq_ignore_ascii_case("error")
        || command_failed
        || tf.get("error").map(|e| !e.is_null()).unwrap_or(false);
    let result_text = tf
        .get("result")
        .and_then(|r| match r {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
        .or_else(|| {
            tf.get("error").and_then(|e| match e {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            })
        });
    if let Some(content) = result_text {
        m.content.push(Block::ToolResult {
            tool_use_id: id,
            content: content.into(),
            is_error,
            tool_name: Some(tool_name),
            status: (!status.is_empty()).then(|| status.to_string()),
            details: None,
        });
    }
}

/// Extract concatenated plain text from a Lexical editor-state JSON tree (Cursor's `richText`).
/// The tree is `{root:{children:[{children:[{text,type:"text"},...]}]}}`. Best-effort; returns
/// None if it doesn't look like a Lexical state.
fn lexical_plain_text(rt: &Value) -> Option<String> {
    // richText may be either a JSON object or a JSON-encoded string.
    let owned;
    let v = if let Some(s) = rt.as_str() {
        owned = serde_json::from_str::<Value>(s).ok()?;
        &owned
    } else {
        rt
    };
    let root = v.get("root")?;
    let mut out = String::new();
    collect_lexical_text(root, &mut out);
    (!out.is_empty()).then_some(out)
}

fn collect_lexical_text(node: &Value, out: &mut String) {
    if let Some(t) = node.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for (i, child) in children.iter().enumerate() {
            collect_lexical_text(child, out);
            // paragraph break between top-level block children
            if i + 1 < children.len() && child.get("children").is_some() {
                out.push('\n');
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy in-workspace format (`workbench.panel.aichat.view.aichat.chatdata`)
// ---------------------------------------------------------------------------

/// Legacy key holding an array of chat tabs in the workspace ItemTable.
const LEGACY_CHAT_KEY: &str = "workbench.panel.aichat.view.aichat.chatdata";

fn discover_legacy_workspace(conn: &Connection, db: &Path, cwd: Option<&Path>, out: &mut Vec<SessionRef>) {
    let Ok(source) = source::cell(conn, "ItemTable", LEGACY_CHAT_KEY) else {
        return;
    };
    let Some(header) = legacy::header(&source) else { return };
    let Some((_, tabs)) = &header.tabs else { return };
    for tab in tabs {
        let Some(tab_id) = tab.metadata.get("tabId").and_then(Value::as_str) else {
            continue;
        };
        let count = tab.bubbles.map_or(0, |(_, count)| count);
        if count == 0 {
            continue;
        }
        let title = tab
            .metadata
            .get("chatTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| crate::ir::truncate(s, 80));
        out.push(SessionRef {
            cwd: cwd.map(Path::to_path_buf),
            title,
            message_count: count,
            ..SessionRef::new(format!("legacy:{tab_id}"), Harness::Cursor, db.to_path_buf())
        });
    }
}

fn stream_legacy(conn: &Connection, r: &SessionRef, tab_id: &str, sink: &mut dyn MessageSink) -> Result<Session> {
    let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
    let source = source::cell(conn, "ItemTable", LEGACY_CHAT_KEY)?;
    let header = legacy::header(&source).context("legacy chatdata JSON")?;
    let (_, tabs) = header.tabs.as_ref().context("legacy chatdata has no tabs")?;
    let selected = tabs
        .iter()
        .position(|tab| tab.metadata.get("tabId").and_then(Value::as_str) == Some(tab_id))
        .with_context(|| format!("legacy tab {tab_id} not found"))?;
    let tab = &tabs[selected].metadata;

    let title = tab
        .get("chatTitle")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| r.title.clone());
    let session = Session {
        cwd: r.cwd.clone(),
        title,
        created_at: r.created_at,
        updated_at: r.updated_at,
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Cursor)
    };
    sink.meta(&session);
    header.replay(&source, selected, &mut |bubble| {
        if let Some(message) = legacy_bubble_to_message(&bubble) {
            sink.message(message)
        } else {
            Flow::Continue
        }
    })?;
    Ok(session)
}

/// Legacy bubbles use a string `type` ("user"/"ai") and a `text` field.
fn legacy_bubble_to_message(b: &Value) -> Option<Message> {
    let role = match b.get("type").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("ai") | Some("assistant") => Role::Assistant,
        _ => Role::Assistant,
    };
    let mut m = Message::new(role);
    if let Some(text) = b.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
    }
    (!m.content.is_empty()).then_some(m)
}

/// Whole-`Session` convenience over [`stream_global`] for tests: stream into a [`CollectSink`].
#[cfg(test)]
fn parse_global(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_global(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

/// Whole-`Session` convenience over [`stream_legacy`] for tests.
#[cfg(test)]
fn parse_legacy(conn: &Connection, r: &SessionRef, tab_id: &str) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_legacy(conn, r, tab_id, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory state.vscdb resembling the global cursorDiskKV layout.
    fn mk_global() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);",
        )
        .unwrap();
        conn
    }

    fn put(conn: &Connection, key: &str, value: &str) {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef::new(id, Harness::Cursor, PathBuf::from(":memory:"))
    }

    #[test]
    fn composer_prefix_uses_key_index_and_preserves_discovery_order() {
        use rusqlite::StatementStatus;
        let mut conn = mk_global();
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO cursorDiskKV VALUES (?1, ?2)").unwrap();
            for i in 0..20_000 {
                insert
                    .execute(rusqlite::params![format!("bubbleId:c:{i}"), "{}"])
                    .unwrap();
            }
            // Insertion order differs from key order; preserve the old table scan's ordering.
            for key in ["composerData:z", "composerData:a", "composerData:m"] {
                insert.execute(rusqlite::params![key, "{}"]).unwrap();
            }
            for key in ["composerData", "composerData;later", "ComposerData:wrong-case"] {
                insert.execute(rusqlite::params![key, "{}"]).unwrap();
            }
        }
        tx.commit().unwrap();
        let mut old = conn
            .prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")
            .unwrap();
        let old_keys: Vec<String> = old
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .filter(|k: &String| k.starts_with("composerData:"))
            .collect();
        let mut bounded = conn.prepare(COMPOSER_ROWS).unwrap();
        let keys: Vec<String> = bounded.query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(keys, old_keys);
        assert_eq!(bounded.get_status(StatementStatus::FullscanStep), 0);
        assert!(bounded.get_status(StatementStatus::VmStep) * 100 < old.get_status(StatementStatus::VmStep));
        let plans: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {COMPOSER_ROWS}"))
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .flatten()
            .collect();
        assert!(
            plans
                .iter()
                .any(|p| p.contains("SEARCH cursorDiskKV") && p.contains("key>?") && p.contains("key<?")),
            "{plans:?}"
        );
        eprintln!(
            "cursor prefix: SQLite {}, old VM steps {}, bounded {}, plans {plans:?}",
            rusqlite::version(),
            old.get_status(StatementStatus::VmStep),
            bounded.get_status(StatementStatus::VmStep),
        );
    }

    #[test]
    fn native_ide_failed_command_and_reopened_conversation() {
        use crate::harness::Parse;

        let _world = crate::test_world("cursor-native-ide");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/3.24.12-ide/Cursor/User");
        let adapter = Cursor::with_user_dir(&root);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        let session = adapter.parse(&refs[0]).unwrap();
        assert_eq!(session.messages.len(), 44);
        assert_eq!(
            session.messages[0].timestamp,
            super::super::parse_ts("2026-10-14T00:49:06.803Z")
        );
        assert!(session.messages.iter().all(|m| m.timestamp.is_some()));
        for opts in [ParseOptions::lazy(), ParseOptions::complete()] {
            let mut sink = crate::stream::CollectSink::default();
            adapter.stream(&refs[0], &opts, &mut sink).unwrap();
            assert_eq!(
                serde_json::to_value(&sink.messages).unwrap(),
                serde_json::to_value(&session.messages).unwrap()
            );
        }
        let users: Vec<_> = session.messages.iter().filter(|m| m.role == Role::User).collect();
        assert_eq!(users.len(), 4);
        assert!(users[3].text().unwrap().contains("We reopened the IDE after stopping"));
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult { content, is_error, .. } => Some((content, *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 17);
        let failed: Vec<_> = results.iter().filter(|(_, error)| *error).collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].0.contains("SyntaxError"));
        assert!(results
            .iter()
            .any(|(text, error)| !error && text.contains("Ran 7 tests")));
    }

    #[test]
    fn parses_headers_only_with_external_bubbles() {
        let conn = mk_global();
        let cid = "comp-1";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-1","name":"My Chat","createdAt":1700000000000,"lastUpdatedAt":1700000050000,"unifiedMode":"agent","_v":3,
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"How do I write a test?"}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b2"),
            r#"{"bubbleId":"b2","type":2,"text":"Here is how.","thinking":{"text":"let me think"},
                "unifiedMode":"chat",
                "tokenCount":{"inputTokens":10,"outputTokens":5},
                "toolFormerData":{"name":"run_terminal_cmd","toolCallId":"t1","status":"completed",
                  "rawArgs":"{\"command\":\"cargo test\"}","result":"{\"output\":\"ok\"}"}}"#,
        );

        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "comp-1");
        assert_eq!(out[0].title.as_deref(), Some("My Chat"));
        assert_eq!(out[0].message_count, 2);
        assert!(out[0].created_at.is_some());

        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.title.as_deref(), Some("My Chat"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("How do I write a test?"));

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // thinking block present
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { text, .. } if text == "let me think")));
        // text block present
        assert_eq!(a.text().as_deref(), Some("Here is how."));
        // tool use + result
        let tu = a.content.iter().find_map(|b| match b {
            Block::ToolUse { name, input, id, .. } => Some((name.clone(), input.clone(), id.clone())),
            _ => None,
        });
        let (name, input, tid) = tu.expect("tool use");
        assert_eq!(name, "run_terminal_cmd");
        assert_eq!(tid, "t1");
        assert_eq!(
            input.resolve().unwrap().get("command").and_then(Value::as_str),
            Some("cargo test")
        );
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { tool_use_id, content, is_error, .. }
            if tool_use_id == "t1" && content.contains("ok") && !is_error)));
        // usage mapped
        assert_eq!(a.usage.as_ref().unwrap().input_tokens, Some(10));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(5));

        // kind/origin: user bubble is a prompt, assistant bubble a reply.
        assert_eq!(
            (s.messages[0].kind, s.messages[0].origin),
            (MessageKind::Prompt, Origin::Human)
        );
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
        // `unifiedMode` rides nested under `extra["cursor"]`, never flat. The COMPOSER's mode is a
        // session fact (the mode the thread was opened in); a bubble's own overrides it per
        // message — here b2 switched to "chat" while the thread is an "agent" thread, and b1
        // (which carries no mode of its own) gets no message-level key at all.
        assert_eq!(
            s.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("agent"),
            "the composer's mode lands on the session bag"
        );
        assert_eq!(
            a.harness_extra(Harness::Cursor)
                .and_then(|b| b.get("unified_mode"))
                .and_then(Value::as_str),
            Some("chat"),
            "the bubble's own mode overrides for that message"
        );
        assert!(s.messages[0]
            .harness_extra(Harness::Cursor)
            .is_none_or(|b| !b.contains_key("unified_mode")));
        // No flat keys survive anywhere: every top-level key is a namespace (the harness bag, cv's
        // own `cv` bag, or the `_record` carrier). See `docs/INTERFACE-V2.md` §4.
        assert!(
            !a.extra.contains_key("unifiedMode"),
            "no flat keys besides the harness bag"
        );
        crate::harness::assert_no_flat_keys(&s);
    }

    #[test]
    fn numeric_modes_preserve_enum_names_raw_values_and_message_overrides() {
        for (number, name) in [
            (0, "unspecified"),
            (1, "chat"),
            (2, "agent"),
            (3, "edit"),
            (4, "custom"),
            (5, "plan"),
            (6, "debug"),
        ] {
            let bubble = serde_json::json!({"type":2,"text":"reply","unifiedMode":number});
            let message = bubble_to_message(&bubble).unwrap();
            let extra = message.harness_extra(Harness::Cursor).unwrap();
            assert_eq!(extra["unified_mode"], name);
            assert_eq!(extra["unified_mode_raw"], number);
        }
        for number in [serde_json::json!(-1), serde_json::json!(7), serde_json::json!(2.5)] {
            let message = bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":number})).unwrap();
            let extra = message.harness_extra(Harness::Cursor).unwrap();
            assert!(!extra.contains_key("unified_mode"));
            assert_eq!(extra["unified_mode_raw"], number);
        }
        for value in [
            serde_json::json!(""),
            Value::Null,
            serde_json::json!(true),
            serde_json::json!({}),
        ] {
            assert!(bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":value})).is_none());
        }
        assert!(bubble_to_message(&serde_json::json!({"type":2})).is_none());
        let future = bubble_to_message(&serde_json::json!({"type":2,"unifiedMode":"future-mode"})).unwrap();
        assert_eq!(
            future.harness_extra(Harness::Cursor).unwrap()["unified_mode"],
            "future-mode"
        );

        let conn = mk_global();
        put(
            &conn,
            "composerData:mode",
            r#"{"composerId":"mode","unifiedMode":"agent","conversation":[{"type":2,"text":"chat reply","unifiedMode":1}]}"#,
        );
        let reference = SessionRef::new("mode", Harness::Cursor, ":memory:");
        let session = parse_global(&conn, &reference).unwrap();
        assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["unified_mode"], "agent");
        assert_eq!(
            session.messages[0].harness_extra(Harness::Cursor).unwrap()["unified_mode"],
            "chat"
        );
        assert_eq!(
            session.messages[0].harness_extra(Harness::Cursor).unwrap()["unified_mode_raw"],
            1
        );
    }

    #[test]
    fn subagent_info_populates_lineage() {
        let conn = mk_global();
        let cid = "comp-sub";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-sub","_v":3,
                "subagentInfo":{"parentComposerId":"comp-parent","toolCallId":"tool_7","subagentTypeName":"explore"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"look"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some("comp-parent"));
        assert_eq!(s.lineage.spawned_by_tool_use.as_deref(), Some("tool_7"));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("explore"));
    }

    #[test]
    fn extracts_model_from_bubble_and_composer() {
        let conn = mk_global();
        let cid = "comp-model";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-model","_v":3,"modelConfig":{"modelName":"gpt-5","maxMode":true},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"answer","modelInfo":{"modelName":"claude-4.5-sonnet-thinking"}}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        // Per-message model from modelInfo.modelName; session model prefers the assistant's.
        assert_eq!(s.messages[0].model.as_deref(), Some("claude-4.5-sonnet-thinking"));
        assert_eq!(s.model.as_deref(), Some("claude-4.5-sonnet-thinking"));
    }

    #[test]
    fn session_model_falls_back_to_composer_config() {
        let conn = mk_global();
        let cid = "comp-cfg";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-cfg","_v":3,"modelConfig":{"modelName":"gpt-5"},
                "fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"hi"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn parses_inline_conversation_old_shape() {
        let conn = mk_global();
        let cid = "comp-old";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-old","createdAt":1700000000000,"_v":1,
                "conversation":[{"bubbleId":"x1","type":1,"text":"hi"},{"bubbleId":"x2","type":2,"text":"hello"}]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_count, 2);
        // no name -> title falls back to None at discover; label() uses first user text.
        assert!(out[0].title.is_none());

        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
        assert_eq!(s.messages[1].text().as_deref(), Some("hello"));
    }

    #[test]
    fn empty_thread_is_skipped_in_discover() {
        let conn = mk_global();
        put(
            &conn,
            "composerData:empty",
            r#"{"composerId":"empty","_v":1,"conversation":[]}"#,
        );
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn richtext_fallback_when_text_empty() {
        let conn = mk_global();
        let cid = "comp-rt";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-rt","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":1}]}"#,
        );
        // text empty, richText holds a Lexical tree.
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":1,"text":"","richText":"{\"root\":{\"children\":[{\"children\":[{\"text\":\"from rich text\",\"type\":\"text\"}],\"type\":\"paragraph\"}],\"type\":\"root\"}}"}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("from rich text"));
    }

    #[test]
    fn tool_error_marks_result_as_error() {
        let conn = mk_global();
        let cid = "comp-err";
        put(
            &conn,
            &format!("composerData:{cid}"),
            r#"{"composerId":"comp-err","_v":3,"fullConversationHeadersOnly":[{"bubbleId":"b1","type":2}]}"#,
        );
        put(
            &conn,
            &format!("bubbleId:{cid}:b1"),
            r#"{"bubbleId":"b1","type":2,"text":"oops",
                "toolFormerData":{"name":"read_file","toolCallId":"t9","status":"error","params":"{\"path\":\"/x\"}","error":"not found"}}"#,
        );
        let s = parse_global(&conn, &sref(cid)).unwrap();
        let tr = s.messages[0].content.iter().find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = tr.expect("tool result");
        assert!(is_error);
        assert!(content.contains("not found"));
    }

    #[test]
    fn missing_table_does_not_panic() {
        // A DB with no cursorDiskKV table at all (older layout) must yield no rows, not error.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT, value BLOB)", [])
            .unwrap();
        let mut out = Vec::new();
        let map = HashMap::new();
        discover_global(&conn, Path::new(":memory:"), &map, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn legacy_workspace_chatdata() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)", [])
            .unwrap();
        let blob = r#"{"tabs":[{"tabId":"tab-1","chatTitle":"Old Tab",
            "bubbles":[{"type":"user","text":"legacy question"},{"type":"ai","text":"legacy answer"}]}]}"#;
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![LEGACY_CHAT_KEY, blob],
        )
        .unwrap();

        let mut out = Vec::new();
        discover_legacy_workspace(&conn, Path::new(":memory:"), Some(Path::new("/tmp/proj")), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "legacy:tab-1");
        assert_eq!(out[0].cwd.as_deref(), Some(Path::new("/tmp/proj")));
        assert_eq!(out[0].message_count, 2);

        let mut r = sref("legacy:tab-1");
        r.cwd = Some(PathBuf::from("/tmp/proj"));
        let s = parse_legacy(&conn, &r, "tab-1").unwrap();
        assert_eq!(s.title.as_deref(), Some("Old Tab"));
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("legacy question"));
        assert_eq!(s.messages[1].role, Role::Assistant);
    }

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/proj"),
            Some(PathBuf::from("/Users/x/proj"))
        );
        assert_eq!(
            file_uri_to_path("file:///Users/x/Application%20Support"),
            Some(PathBuf::from("/Users/x/Application Support"))
        );
        assert_eq!(file_uri_to_path("vscode-remote://foo/bar"), None);
    }
}
