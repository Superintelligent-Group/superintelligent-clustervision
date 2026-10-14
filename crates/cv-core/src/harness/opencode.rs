//! OpenCode adapter — `${XDG_DATA_HOME:-~/.local/share}/opencode/`.
//!
//! ## Where sessions live
//!
//! Since 2026-01 (opencode ≥ 1.1) the canonical store is a SQLite database, `opencode.db`
//! (`$OPENCODE_DB` overrides it; non-release channels write `opencode-<channel>.db`; WAL mode):
//! `session` rows (one per session — `directory` = cwd, `title`, `model`, `agent`, cost and token
//! totals), `message` rows (`data` = the message Info JSON minus `id`/`sessionID`) and `part` rows
//! (`data` = the Part JSON minus `id`/`sessionID`/`messageID`), keyed by `message_id`. Source of
//! truth: `packages/core/src/session/sql.ts` (tables) and `packages/schema/src/v1/session.ts`
//! (payload shapes) at opencode `fee476bb` (1.18). The older JSON tree under `storage/` —
//! `session/**/ses_*.json`, `message/<sid>/<msgid>.json`, `part/<msgid>/<partid>.json` (parts keyed
//! by *messageID*) — was only ever the input to a one-shot importer that was deleted on 2026-06-02:
//! a machine that ran the import still carries a frozen copy, a fresh install has none. We read the
//! database when it exists (behind the `sqlite` cargo feature) and fall back to the JSON tree,
//! de-duplicating by session id (db wins). The payload JSON is identical in both, so one mapping
//! ([`build_messages`]) serves both sources; db-backed [`SessionRef`]s point at the `.db` file.
//!
//! ## Two storage generations (JSON tree)
//!
//! OpenCode rewrote its on-disk model. We sniff by content (presence of separate part files /
//! an inline `summary` object) rather than any version field:
//!
//! * **Inline-summary generation (older):** a message carried an AI-generated `summary`
//!   *object* `{title, body, diffs[]}` inline; there were no separate part files. We render
//!   `summary.body` (falling back to `title`) as the message text and stash `diffs` in `extra`.
//! * **Parts generation (newer):** message content lives in `part/<msgid>/<partid>.json`
//!   files, one per part. The message record holds only metadata. The richest variant.
//!   Confusingly, a *boolean* `summary: true` here is a compaction marker on an assistant
//!   message (mode/agent == "compaction"), NOT inline content — we keep it in `extra`.
//!
//! The on-disk part taxonomy (source of truth: opencode `session/message-v2.ts`) is:
//! `text` (may be `synthetic`/`ignored`), `reasoning` (carries `metadata.anthropic.signature`),
//! `tool` (with a `state` union pending|running|completed|error, each holding
//! input/output/title/metadata/error/time), `file` (file/dir/image attachment with `mime`,
//! `url`, `source`), `agent` (an `@agent` mention / sub-agent reference), `subtask` (a spawned
//! sub-session with its own prompt/agent/model), `patch`, `snapshot`, `step-start`,
//! `step-finish` (per-step cost/tokens), `retry`, `compaction`.
//!
//! ## Kinds (`docs/INTERFACE-V2.md` §4)
//!
//! A `user` record is a `Prompt`/Human and an `assistant` record a `Reply`/Model, except: a record
//! whose parts include `compaction` (the user-side request that starts a compaction — on this
//! machine always a user record with that one part) is the `CompactionBoundary`; an assistant
//! record with `summary: true` (`agent: compaction`) is the `CompactionSummary`; an assistant record
//! carrying `error` is an `Error` (`extra["opencode"]["error"]` + `error_name`), whatever partial
//! text it kept; a record with an `agent` mention or a `subtask` part is a `SubagentSpawn`; a
//! `system` record is the `SystemPrompt` (also `Session::system_prompt`). Compaction/error origin
//! is Harness. Tool results are split into a trailing `ToolResult`/Harness message.
//! `step-start`/`step-finish`/`retry`/`snapshot`/`patch` parts never surface as messages; they ride
//! on their record's `extra["opencode"]` bag (`steps`, `retries`, `snapshots`, `patches`), as do
//! `agent_refs`, `subtasks`, `compactions`, `unknown_parts`, the record's `agent`/`mode`/
//! `providerID`/`finish`/`variant`/`structured`/`format`/`system`/`tools`/`cwd`, `summary_diffs`
//! (old generation) and `error`/`error_name`. `cost` and `tokens.reasoning` are first-class
//! (`Usage::cost_usd`, `Usage::reasoning_tokens`); `session.parent_id` is `Lineage::parent`; the
//! remaining `session` columns land in `Session::extra["opencode"]`.

mod emit;
#[cfg(any(feature = "sqlite", test))]
mod v2;
pub use emit::emit;

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenCode {
    /// The legacy JSON tree (`<data>/opencode/storage`), if present.
    root: Option<PathBuf>,
    /// The canonical SQLite store, if present (and the `sqlite` feature is on).
    db: Option<PathBuf>,
}

impl OpenCode {
    pub fn detect() -> Self {
        let data = data_dir();
        let root = data.as_ref().map(|d| d.join("storage")).filter(|p| p.exists());
        let db = data.as_deref().and_then(resolve_db);
        OpenCode { root, db }
    }

    /// An adapter rooted at an explicit JSON storage dir (`…/opencode/storage`) instead of `$HOME` —
    /// lets emit-verification and tests re-parse a just-written session without mutating the
    /// process environment (env writes race other threads' `getenv`). No database.
    pub fn with_root(root: PathBuf) -> Self {
        OpenCode {
            root: Some(root),
            db: None,
        }
    }

    /// An adapter over an explicit OpenCode data dir (`…/opencode`, holding `opencode.db` and maybe
    /// the old `storage/` tree), read the way [`detect`](Self::detect) reads the real one but without
    /// consulting `$OPENCODE_DB` (the corpus test, other machines' copies).
    pub fn with_data_dir(data: &Path) -> Self {
        let root = Some(data.join("storage")).filter(|p| p.exists());
        #[cfg(feature = "sqlite")]
        let db = Some(data.join("opencode.db")).filter(|p| p.is_file());
        #[cfg(not(feature = "sqlite"))]
        let db = None;
        OpenCode { root, db }
    }

    /// An adapter over one explicit `opencode.db` (tests, alternate data dirs). No JSON tree.
    #[cfg(feature = "sqlite")]
    pub fn with_db(db: PathBuf) -> Self {
        OpenCode {
            root: None,
            db: Some(db),
        }
    }

    /// Sessions from the SQLite store, plus their ids so the JSON walk can skip duplicates.
    #[cfg(feature = "sqlite")]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        let Some(db) = &self.db else {
            return (Vec::new(), HashSet::new());
        };
        match db::discover(db) {
            Ok(refs) => {
                let ids = refs.iter().map(|r| r.id.clone()).collect();
                (refs, ids)
            }
            Err(e) => {
                eprintln!("cv: skipping {}: {e:#}", db.display());
                (Vec::new(), HashSet::new())
            }
        }
    }
    #[cfg(not(feature = "sqlite"))]
    fn discover_db(&self) -> (Vec<SessionRef>, HashSet<String>) {
        (Vec::new(), HashSet::new())
    }

    fn message_dir(&self, sid: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("message").join(sid))
    }
    fn part_dir(&self, mid: &str) -> Option<PathBuf> {
        // Parts are keyed by messageID only: storage/part/<messageID>/<partid>.json
        self.root.as_ref().map(|r| r.join("part").join(mid))
    }
}

/// `${XDG_DATA_HOME:-~/.local/share}/opencode` — opencode's data dir (`packages/core/src/global.ts`,
/// via `xdg-basedir`).
fn data_dir() -> Option<PathBuf> {
    Some(super::store::xdg_data_dir()?.join("opencode"))
}

/// The database opencode itself would open (`packages/core/src/database/database.ts::path`):
/// `$OPENCODE_DB` (absolute, or relative to the data dir; `:memory:` means none), else
/// `opencode.db` (release channels), else the newest `opencode-<channel>.db`.
#[cfg(feature = "sqlite")]
fn resolve_db(data: &Path) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("OPENCODE_DB") {
        if v == ":memory:" {
            return None;
        }
        let v = PathBuf::from(v);
        let p = if v.is_absolute() { v } else { data.join(v) };
        return p.is_file().then_some(p);
    }
    let main = data.join("opencode.db");
    if main.is_file() {
        return Some(main);
    }
    let mut channel: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(data)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !(name.starts_with("opencode-") && name.ends_with(".db")) {
                return None;
            }
            Some((e.metadata().ok()?.modified().ok()?, p))
        })
        .collect();
    channel.sort();
    channel.pop().map(|(_, p)| p)
}
#[cfg(not(feature = "sqlite"))]
fn resolve_db(_data: &Path) -> Option<PathBuf> {
    None
}

impl Adapter for OpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root
            .clone()
            .or_else(|| self.db.as_ref().and_then(|d| d.parent()).map(Path::to_path_buf))
            .into_iter()
            .collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // The database first (it is the live store), then the frozen JSON tree for anything the
        // database doesn't have — a session present in both (the import left copies) appears once.
        let (mut out, seen) = self.discover_db();
        let Some(root) = &self.root else {
            return Ok(out);
        };
        let session_dir = root.join("session");
        for entry in WalkDir::new(&session_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("ses_") || !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name.trim_end_matches(".json"))
                .to_string();
            if seen.contains(&id) {
                continue;
            }
            let message_count = self.message_dir(&id).map(|d| count_turns(&d)).unwrap_or(0);
            out.push(SessionRef {
                id,
                harness: Harness::OpenCode,
                path: path.to_path_buf(),
                cwd: v.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| crate::ir::truncate(t, 80)),
                created_at: v.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: v.pointer("/time/updated").and_then(super::ts_from_value),
                message_count,
            });
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::OpenCode, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            // A db-backed ref points at the `.db` file itself (like the other SQLite adapters).
            #[cfg(feature = "sqlite")]
            if db::is_db_ref(r) {
                return db::stream(r, _opts, sink);
            }
            // OpenCode stores a session as a *directory* of per-message JSON files (plus per-message
            // part files). The old `parse` slurped every message body into a `Vec` to sort it — for a
            // large session that resident-spikes the whole transcript. Here we instead:
            //   1. read the small session meta file,
            //   2. cheaply order the message files (read each only to pull its `time/created` key, then
            //      drop the body — peak is O(one message file), not the whole session),
            //   3. re-read each file in order, build its IR message(s), emit to `sink`, and drop it
            //      before touching the next.
            // `load_parts` already loads one message's parts at a time, so peak stays O(one message).
            let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
            let meta: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

            let mut s = Session {
                id: r.id.clone(),
                harness: Harness::OpenCode,
                cwd: meta.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: meta.get("title").and_then(Value::as_str).map(str::to_string),
                created_at: meta.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: meta.pointer("/time/updated").and_then(super::ts_from_value),
                model: None,
                git: None,
                messages: Vec::new(),
                source_path: Some(r.path.clone()),
                extra: serde_json::Map::new(),
                system_prompt: None,
                lineage: Lineage {
                    parent: meta
                        .get("parentID")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                    ..Lineage::default()
                },
            };

            // Order message files by `time/created` (same ordering as before) while holding only the
            // sort key + path per file — never all message bodies at once.
            let mut order: Vec<(i64, PathBuf)> = Vec::new();
            if let Some(mdir) = self.message_dir(&r.id) {
                if let Ok(rd) = fs::read_dir(&mdir) {
                    for e in rd.filter_map(|e| e.ok()) {
                        let path = e.path();
                        let Ok(source) = crate::json::JsonSource::capture(&path) else {
                            continue;
                        };
                        match source.read_syntax(|view| {
                            if !view.accepts_value()? {
                                return Ok(None);
                            }
                            let when = view
                                .get("time")?
                                .map(|time| time.get("created"))
                                .transpose()?
                                .flatten()
                                .map(|created| created.as_number())
                                .transpose()?
                                .flatten()
                                .and_then(|number| number.as_i64())
                                .unwrap_or(0);
                            Ok(Some(when))
                        }) {
                            Ok(Some(when)) => order.push((when, path)),
                            Ok(None) => {}
                            Err(error) if error.is::<crate::json::InvalidJson>() => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
            order.sort_by_key(|(t, _)| *t);

            sink.meta(&s);

            // All regenerated payloads share one private file for this session pass.
            let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena()?);

            // Re-read one message file at a time, emit, then drop before the next.
            for (_, path) in &order {
                let built = if _opts.spans {
                    let Ok(source) = crate::json::JsonSource::capture(path) else {
                        continue;
                    };
                    let header = match source.read_syntax(|view| {
                        if !view.accepts_value()? {
                            return Ok(None);
                        }
                        ProjectedHeader::read(view).map(Some)
                    }) {
                        Ok(Some(header)) => header,
                        Ok(None) => continue,
                        Err(error) if error.is::<crate::json::InvalidJson>() => continue,
                        Err(error) => return Err(error),
                    };
                    let msg_id = header.value.get("id").and_then(Value::as_str).unwrap_or("");
                    let parts = self.load_projected_parts(msg_id, &arena)?;
                    build_header_projected(&header, &parts)
                } else {
                    let Ok(t) = fs::read_to_string(path) else { continue };
                    let Ok(mv) = serde_json::from_str::<Value>(&t) else {
                        continue;
                    };
                    let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
                    build_messages(&mv, &self.load_parts(msg_id))
                };
                for mut built in built {
                    fold_into_session(&mut s, &mut built)?;
                    if sink.message(built) == Flow::Stop {
                        return Ok(s);
                    }
                }
            }

            Ok(s)
        })
    }

    /// The todo list `todowrite` keeps in `opencode.db`'s `todo` table (1.x; 2.x has none).
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        #[cfg(feature = "sqlite")]
        if db::is_db_ref(r) {
            return db::todos(r).into_iter().collect();
        }
        Vec::new()
    }
}

/// Session facts a message reveals as it streams past: the session model (the first one seen when
/// the store gave none), the system prompt (a `system` record), and the IR diet — `Message::model`
/// stays set only when it differs from the session's.
fn fold_into_session(s: &mut Session, m: &mut Message) -> Result<()> {
    if s.model.is_none() {
        s.model = m.model.clone();
    }
    if m.model.is_some() && m.model == s.model {
        m.model = None;
    }
    if m.kind == MessageKind::SystemPrompt && s.system_prompt.is_none() {
        // System prompts are required as owned session metadata even for bounded readers.
        m.materialize(&s.resolver())?;
        s.system_prompt = m.text();
    }
    Ok(())
}

/// Count a session's conversational turns: message records whose `role` is `user`/`assistant`.
/// The [`SessionRef::message_count`] contract is user+assistant turns only — counting directory
/// entries (the old behavior) also picked up system/summary records and any stray files, so the
/// number disagreed with every other adapter. Project only the role while validating each header;
/// older headers can contain large inline summaries. This never touches part files.
fn count_turns(dir: &std::path::Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|entry| {
            let Ok(source) = crate::json::JsonSource::capture(&entry.path()) else {
                return false;
            };
            source
                .read_syntax(|view| {
                    if !view.accepts_value()? {
                        return Ok(false);
                    }
                    Ok(view
                        .get("role")?
                        .map(|role| role.resolve())
                        .transpose()?
                        .is_some_and(|role| matches!(role.as_str(), Some("user") | Some("assistant"))))
                })
                .unwrap_or(false)
        })
        .count()
}

impl OpenCode {
    fn load_projected_parts(
        &self,
        mid: &str,
        arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
    ) -> Result<Vec<ProjectedPart>> {
        let Some(dir) = self.part_dir(mid) else {
            return Ok(Vec::new());
        };
        let mut paths = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                paths.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
            }
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        let mut parts = Vec::new();
        for (_, path) in paths {
            let Ok(source) = crate::json::JsonSource::capture(&path) else {
                continue;
            };
            match source.read_syntax(|view| {
                if !view.accepts_value()? || !view.is_object()? {
                    return Ok(None);
                }
                Ok(Some(ProjectedPart::read(view, &source, arena)?))
            }) {
                Ok(Some(part)) => parts.push(part),
                Ok(None) => {}
                Err(error) if error.is::<crate::json::InvalidJson>() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(parts)
    }

    fn load_parts(&self, mid: &str) -> Vec<Value> {
        let Some(pdir) = self.part_dir(mid) else {
            return vec![];
        };
        let mut parts: Vec<(String, Value)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&pdir) {
            for e in rd.filter_map(|e| e.ok()) {
                let fname = e.file_name().to_string_lossy().to_string();
                if let Ok(t) = fs::read_to_string(e.path()) {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        parts.push((fname, v));
                    }
                }
            }
        }
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        parts.into_iter().map(|(_, v)| v).collect()
    }
}

// ── the SQLite store ─────────────────────────────────────────────────────────────────────────

#[cfg(feature = "sqlite")]
mod db {
    //! `opencode.db` — `session`, `message`, `part` (`packages/core/src/session/sql.ts`). Rows are
    //! hydrated back into the exact JSON the tree stored (`{...data, id, sessionID, messageID}`,
    //! as `message-v2.ts` does) and handed to the shared mapping, so the two sources can't drift.
    use super::super::store::sqlite::open_ro;
    use super::super::ts_from_epoch;
    use super::*;
    use rusqlite::types::ValueRef;
    use std::collections::HashMap;

    /// Session columns the IR first-classes (`parent_id` is `Lineage::parent`; everything else lands
    /// in `Session::extra["opencode"]` verbatim).
    const FIRSTCLASS: &[&str] = &[
        "id",
        "directory",
        "title",
        "time_created",
        "time_updated",
        "parent_id",
        "fork_session_id",
    ];
    /// Columns opencode declares `{ mode: "json" }` — stored as JSON text, decoded on read.
    const JSON_COLS: &[&str] = &[
        "model",
        "metadata",
        "revert",
        "permission",
        "summary_diffs",
        "fork_boundary",
    ];

    /// The session's `todo` rows in position order, as its plan. `None` when the table is absent
    /// (2.x) or holds nothing for the session.
    pub(super) fn todos(r: &SessionRef) -> Option<crate::affordance::Affordance> {
        let conn = open_ro(&r.path).ok()?;
        conn.read(|conn| {
            if !has_table(conn, "todo") {
                return Ok(None);
            }
            let mut st = conn.prepare("SELECT content, status FROM todo WHERE session_id = ?1 ORDER BY position")?;
            let items: Vec<crate::affordance::Item> = st
                .query_map([&r.id], |row| {
                    Ok(crate::affordance::Item {
                        content: row.get(0)?,
                        status: row.get(1)?,
                    })
                })?
                .filter_map(|x| x.ok())
                .collect();
            Ok(crate::affordance::plan_state(
                crate::affordance::Kind::Plan,
                "todo",
                items,
            ))
        })
        .ok()
        .flatten()
    }

    pub(super) fn is_db_ref(r: &SessionRef) -> bool {
        r.path.extension().and_then(|e| e.to_str()) == Some("db")
    }

    fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// One [`SessionRef`] per session, newest first: the 2.x `session_v2` rows, then the 1.x
    /// `session` rows they do not already cover (a migrated database holds both under the same id;
    /// 2.x writes only the new tables, so they win). Either table set may be absent: a 1.x database
    /// has no `session_v2`, a database 2.x created has no `session`.
    pub(super) fn discover(db: &Path) -> Result<Vec<SessionRef>> {
        let conn = open_ro(db)?;
        conn.read(|reader| {
            let source = crate::json::JsonSource::capture_with_reads(db, reader.reads())?;
            let result = discover_snapshot(db, &source, reader);
            source.check()?;
            result
        })
    }

    fn discover_snapshot(
        db: &Path,
        source: &crate::json::JsonSource,
        conn: &rusqlite::Connection,
    ) -> Result<Vec<SessionRef>> {
        let _transaction = conn.unchecked_transaction()?;
        let mut refs = if has_table(conn, "session_v2") && has_table(conn, "session_message") {
            discover_v2(conn, db)?
        } else {
            Vec::new()
        };
        if has_table(conn, "session") && has_table(conn, "message") {
            let seen: HashSet<String> = refs.iter().map(|r| r.id.clone()).collect();
            refs.extend(
                discover_v1(conn, db, source)?
                    .into_iter()
                    .filter(|r| !seen.contains(&r.id)),
            );
        }
        Ok(refs)
    }

    fn discover_v2(conn: &rusqlite::Connection, db: &Path) -> Result<Vec<SessionRef>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT session_id, COUNT(*) FROM session_message WHERE type IN ('user', 'assistant') GROUP BY session_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for (sid, n) in rows.flatten() {
                counts.insert(sid, n.max(0) as usize);
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, directory, title, time_created, time_updated FROM session_v2 ORDER BY time_updated DESC",
        )?;
        let refs = stmt
            .query_map([], |r| session_ref(db, r))?
            .flatten()
            .map(|mut r| {
                r.message_count = counts.get(&r.id).copied().unwrap_or(0);
                r
            })
            .collect();
        Ok(refs)
    }

    fn session_ref(db: &Path, r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRef> {
        Ok(SessionRef {
            id: r.get(0)?,
            harness: Harness::OpenCode,
            path: db.to_path_buf(),
            cwd: r
                .get::<_, Option<String>>(1)?
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            title: r
                .get::<_, Option<String>>(2)?
                .filter(|t| !t.is_empty())
                .map(|t| crate::ir::truncate(&t, 80)),
            created_at: r.get::<_, Option<i64>>(3)?.and_then(ts_from_epoch),
            updated_at: r.get::<_, Option<i64>>(4)?.and_then(ts_from_epoch),
            message_count: 0,
        })
    }

    /// Count the roles the actual Value-based reader accepts, including final duplicate keys.
    /// Incremental cells keep old inline summary bodies out of SQLite's native value buffers.
    fn discover_v1(
        conn: &rusqlite::Connection,
        db: &Path,
        source: &crate::json::JsonSource,
    ) -> Result<Vec<SessionRef>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let (mut stmt, rowid) = match conn.prepare("SELECT session_id, rowid, typeof(data) FROM message") {
            Ok(statement) => (statement, true),
            Err(_) => (conn.prepare("SELECT session_id, data FROM message")?, false),
        };
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let Ok(sid) = row.get::<_, String>(0) else { continue };
            let backing = if rowid {
                if row.get::<_, String>(2)? != "text" {
                    continue;
                }
                let blob = conn.blob_open(rusqlite::DatabaseName::Main, "message", "data", row.get(1)?, true)?;
                crate::harness::store::bytes::ByteSource::read(blob)?
            } else {
                let Ok(data) = row.get_ref(1)?.as_str() else { continue };
                crate::harness::store::bytes::ByteSource::read(data.as_bytes())?
            };
            let len = backing.len()?;
            if len == 0 {
                continue;
            }
            let accepted = source.read_backing_syntax(std::sync::Arc::new(backing), 0, len, |view| {
                if !view.accepts_value()? || !view.is_object()? {
                    return Ok(false);
                }
                Ok(view
                    .get("role")?
                    .map(|role| role.resolve())
                    .transpose()?
                    .is_some_and(|role| matches!(role.as_str(), Some("user") | Some("assistant"))))
            });
            match accepted {
                Ok(true) => *counts.entry(sid).or_default() += 1,
                Ok(false) => {}
                Err(error) if error.is::<crate::json::InvalidJson>() => {}
                Err(error) => return Err(error),
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, directory, title, time_created, time_updated FROM session ORDER BY time_updated DESC",
        )?;
        let refs = stmt
            .query_map([], |r| session_ref(db, r))?
            .flatten()
            .map(|mut r| {
                r.message_count = counts.get(&r.id).copied().unwrap_or(0);
                r
            })
            .collect();
        Ok(refs)
    }

    /// One row as a JSON object keyed by column name (`NULL`s dropped, JSON columns decoded).
    fn row_to_map(cols: &[String], row: &rusqlite::Row<'_>) -> Result<Map<String, Value>> {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                ValueRef::Null => continue,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => {
                    let t = String::from_utf8_lossy(t).into_owned();
                    if JSON_COLS.contains(&c.as_str()) {
                        serde_json::from_str(&t).unwrap_or(Value::String(t))
                    } else {
                        Value::String(t)
                    }
                }
                ValueRef::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
            };
            m.insert(c.clone(), v);
        }
        Ok(m)
    }

    /// Stream one db-backed session: the `session` row → [`Session`] metadata + `extra`, then each
    /// `message` row with its `part` rows hydrated and mapped one message at a time.
    pub(super) fn stream(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        let conn = open_ro(&r.path)?;
        conn.read(|reader| {
            let source = crate::json::JsonSource::capture_with_reads(&r.path, reader.reads())?;
            let result = stream_snapshot(r, opts, sink, &source, reader);
            source.check()?;
            result
        })
    }

    fn stream_snapshot(
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        source: &crate::json::JsonSource,
        conn: &rusqlite::Connection,
    ) -> Result<Session> {
        let _transaction = conn.unchecked_transaction()?;
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena()?);
        // The 2.x tables win for a session in both (a migrated database; module doc of `v2`).
        let v2 = has_table(conn, "session_v2")
            && conn
                .query_row(
                    "SELECT 1 FROM session_v2 WHERE id = ?1",
                    rusqlite::params![r.id],
                    |_| Ok(()),
                )
                .is_ok();
        let table = if v2 { "session_v2" } else { "session" };
        let row = {
            // `SELECT *` + names, so a column that only exists in newer schemas (`metadata`,
            // 2026-05; `workspace_id`, `path`, …) is read when present and never demanded.
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))?;
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = stmt.query(rusqlite::params![r.id])?;
            match rows.next()? {
                Some(row) => row_to_map(&cols, row)?,
                None => anyhow::bail!("no opencode session {} in {}", r.id, r.path.display()),
            }
        };
        let mut bag = Map::new();
        for (k, v) in &row {
            // Keep the session's own facts (agent, model, cost, tokens_*, summary_*, share_url,
            // version, project_id, time_archived, metadata, …) verbatim; skip blanks.
            if FIRSTCLASS.contains(&k.as_str()) || v.as_str().is_some_and(str::is_empty) {
                continue;
            }
            bag.insert(k.clone(), v.clone());
        }
        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::OpenCode,
            system_prompt: None,
            // A sub-agent session (`@agent` mention, `subtask`) points at the session that spawned it.
            lineage: Lineage {
                parent: row
                    .get("parent_id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                // 2.x `fork_session_id`: the session `--fork` copied this one from.
                forked_from: row
                    .get("fork_session_id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                ..Lineage::default()
            },
            cwd: row
                .get("directory")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            title: row
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
            created_at: row.get("time_created").and_then(super::super::ts_from_value),
            updated_at: row.get("time_updated").and_then(super::super::ts_from_value),
            // `session.model` = `{id, providerID, variant?}`; the first message's model is the
            // fallback for rows written before the column existed.
            model: row
                .get("model")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: Map::new(),
        };
        if !bag.is_empty() {
            s.harness_extra_mut(Harness::OpenCode).extend(bag);
        }
        sink.meta(&s);
        if v2 {
            return stream_v2(conn, s, opts, sink, source, &arena);
        }

        let (mut msgs, message_rowid) = if opts.spans {
            match conn
                .prepare("SELECT id, rowid, typeof(data) FROM message WHERE session_id = ?1 ORDER BY time_created, id")
            {
                Ok(statement) => (statement, true),
                Err(_) => (
                    conn.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?,
                    false,
                ),
            }
        } else {
            (
                conn.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?,
                false,
            )
        };
        let (mut parts, part_rowid) = if opts.spans {
            match conn.prepare("SELECT rowid, id, typeof(data) FROM part WHERE message_id = ?1 ORDER BY id") {
                Ok(statement) => (statement, true),
                Err(_) => (
                    conn.prepare("SELECT id, data FROM part WHERE message_id = ?1 ORDER BY id")?,
                    false,
                ),
            }
        } else {
            (
                conn.prepare("SELECT id, data FROM part WHERE message_id = ?1 ORDER BY id")?,
                false,
            )
        };
        let mut rows = msgs.query(rusqlite::params![r.id])?;
        while let Some(row) = rows.next()? {
            let mid: String = row.get(0)?;
            let mut header = if opts.spans {
                let (offset, len) = if message_rowid {
                    if row.get::<_, String>(2)? != "text" {
                        anyhow::bail!("OpenCode SQLite message data is not TEXT");
                    }
                    crate::harness::store::sqlite::append_cell(conn, "message", "data", row.get(1)?, &arena)?
                } else {
                    let data = row.get_ref(1)?.as_str()?;
                    arena.append(|writer| {
                        writer.write_all(data.as_bytes())?;
                        Ok(())
                    })?
                };
                if len == 0 {
                    continue;
                }
                match source.read_backing_syntax(arena.clone(), offset, len, |view| {
                    if !view.accepts_value()? || !view.is_object()? {
                        return Ok(None);
                    }
                    ProjectedHeader::read(view).map(Some)
                }) {
                    Ok(Some(header)) => header,
                    Ok(None) => continue,
                    Err(error) if error.is::<crate::json::InvalidJson>() => {
                        if !backing_utf8(&arena, offset, len)? {
                            anyhow::bail!("OpenCode SQLite message data is not UTF-8");
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let data: String = row.get(1)?;
                let Ok(Value::Object(mv)) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                ProjectedHeader {
                    value: Value::Object(mv),
                    summary: None,
                }
            };
            let mv = header.value.as_object_mut().unwrap();
            mv.insert("id".into(), json!(mid));
            mv.insert("sessionID".into(), json!(r.id));
            let mut part_rows = parts.query(rusqlite::params![mid])?;
            let mut pv = Vec::new();
            let mut projected = Vec::new();
            while let Some(pr) = part_rows.next()? {
                let Ok(pid) = pr.get::<_, String>(if part_rowid { 1 } else { 0 }) else {
                    continue;
                };
                if part_rowid && pr.get::<_, String>(2)? != "text" {
                    continue;
                }
                if !part_rowid && pr.get_ref(1)?.as_str().is_err() {
                    continue;
                }
                if opts.spans {
                    let (offset, len) = if part_rowid {
                        crate::harness::store::sqlite::append_cell(conn, "part", "data", pr.get(0)?, &arena)?
                    } else {
                        let pdata = pr.get_ref(1)?.as_str()?;
                        arena.append(|writer| {
                            writer.write_all(pdata.as_bytes())?;
                            Ok(())
                        })?
                    };
                    if len == 0 {
                        continue;
                    }
                    let part = source.read_backing_syntax(arena.clone(), offset, len, |view| {
                        if !view.accepts_value()? || !view.is_object()? {
                            return Ok(None);
                        }
                        Ok(Some(ProjectedPart::read(view, source, &arena)?))
                    });
                    match part {
                        Ok(Some(mut part)) => {
                            let po = part.value.as_object_mut().unwrap();
                            po.insert("id".into(), json!(pid));
                            po.insert("messageID".into(), json!(mid));
                            po.insert("sessionID".into(), json!(r.id));
                            projected.push(part);
                        }
                        Ok(None) => {}
                        Err(error) if error.is::<crate::json::InvalidJson>() => {}
                        Err(error) => return Err(error),
                    }
                } else if let Ok(Value::Object(mut po)) = serde_json::from_str::<Value>(pr.get_ref(1)?.as_str()?) {
                    po.insert("id".into(), json!(pid));
                    po.insert("messageID".into(), json!(mid));
                    po.insert("sessionID".into(), json!(r.id));
                    pv.push(Value::Object(po));
                }
            }
            let built = if opts.spans {
                build_header_projected(&header, &projected)
            } else {
                build_messages(&header.value, &pv)
            };
            for mut built in built {
                fold_into_session(&mut s, &mut built)?;
                if sink.message(built) == Flow::Stop {
                    return Ok(s);
                }
            }
        }
        Ok(s)
    }

    // Owned SQLite String reads reject invalid UTF-8 before JSON parsing. Preserve that failure
    // policy on malformed rows without adding a second traversal to valid payloads.
    fn backing_utf8(arena: &crate::harness::store::bytes::ByteSource, offset: u64, len: u64) -> Result<bool> {
        if let Some(valid) =
            arena.with_bytes(|bytes| std::str::from_utf8(&bytes[offset as usize..(offset + len) as usize]).is_ok())?
        {
            return Ok(valid);
        }
        use std::io::{Read, Seek};
        let mut file = arena.file()?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut input = file.take(len);
        let mut bytes = [0u8; 64 * 1024 + 3];
        let mut pending = 0;
        loop {
            let read = input.read(&mut bytes[pending..])?;
            if read == 0 {
                return Ok(pending == 0);
            }
            let end = pending + read;
            match std::str::from_utf8(&bytes[..end]) {
                Ok(_) => pending = 0,
                Err(error) if error.error_len().is_none() => {
                    pending = end - error.valid_up_to();
                    bytes.copy_within(error.valid_up_to()..end, 0);
                }
                Err(_) => return Ok(false),
            }
        }
    }

    /// The 2.x `session_message` rows of one session, in `seq` order (module doc of `v2`).
    fn stream_v2(
        conn: &rusqlite::Connection,
        mut s: Session,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
        source: &crate::json::JsonSource,
        arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
    ) -> Result<Session> {
        let (mut stmt, rowid) = if opts.spans {
            match conn
                .prepare("SELECT id, type, rowid, typeof(data) FROM session_message WHERE session_id = ?1 ORDER BY seq")
            {
                Ok(statement) => (statement, true),
                Err(_) => (
                    conn.prepare("SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?,
                    false,
                ),
            }
        } else {
            (
                conn.prepare("SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?,
                false,
            )
        };
        let mut rows = stmt.query(rusqlite::params![s.id])?;
        let mut unknown: std::collections::BTreeMap<String, u64> = Default::default();
        let mut unreadable = 0u64;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let ty: String = row.get(1)?;
            if rowid && row.get::<_, String>(3)? != "text" {
                anyhow::bail!("OpenCode SQLite message data is not TEXT");
            }
            let mut built = if opts.spans
                && matches!(
                    ty.as_str(),
                    "user" | "synthetic" | "system" | "skill" | "shell" | "assistant" | "compaction"
                ) {
                let (offset, len) = if rowid {
                    crate::harness::store::sqlite::append_cell(conn, "session_message", "data", row.get(2)?, arena)?
                } else {
                    let bytes = row.get_ref(2)?.as_str()?;
                    arena.append(|writer| {
                        writer.write_all(bytes.as_bytes())?;
                        Ok(())
                    })?
                };
                if len == 0 {
                    unreadable += 1;
                    continue;
                }
                let result = source.read_backing_syntax(arena.clone(), offset, len, |view| {
                    if !view.accepts_value()? {
                        return Ok(None);
                    }
                    let mut data = Map::new();
                    let mut parts = Vec::new();
                    let mut text = None;
                    let shell = (ty == "shell")
                        .then(|| project_shell(view, source, arena))
                        .transpose()?;
                    view.for_each_entry(|key, value| {
                        if ty == "shell" && matches!(key, "command" | "output") {
                            // Payloads are retained separately; only the command-presence gate
                            // is needed by the unchanged shell codec.
                            data.insert(
                                key.to_owned(),
                                if key == "command" && value.is_string()? {
                                    json!(if nonempty_string(value)? { "x" } else { "" })
                                } else {
                                    Value::Null
                                },
                            );
                        } else if ((key == "text" && ty != "assistant") || (key == "summary" && ty == "compaction"))
                            && value.is_string()?
                        {
                            text = value.source_text()?;
                            data.insert(key.to_owned(), json!(if nonempty_string(value)? { "x" } else { "" }));
                        } else if key == "files" && ty == "user" && value.is_array()? {
                            let mut files = Vec::new();
                            for file in value.array()?.unwrap() {
                                let file = file?;
                                let mut selected = Map::new();
                                for key in ["mime", "source", "name"] {
                                    if let Some(value) = file.get(key)? {
                                        selected.insert(key.into(), value.resolve()?.into_owned());
                                    }
                                }
                                files.push(Value::Object(selected));
                            }
                            data.insert(key.into(), Value::Array(files));
                        } else if key == "content" && ty == "assistant" && value.is_array()? {
                            for item in value.array()?.unwrap() {
                                parts.push(ProjectedPart::read_inner(item?, source, arena, true)?);
                            }
                            data.insert(
                                key.to_owned(),
                                Value::Array(parts.iter().map(|p| p.value.clone()).collect()),
                            );
                        } else {
                            data.insert(key.to_owned(), value.resolve()?.into_owned());
                        }
                        Ok(())
                    })?;
                    let mut built = v2::build_projected(
                        &id,
                        &ty,
                        &Value::Object(data),
                        opts,
                        text.as_ref(),
                        Some(&parts),
                        shell.as_ref(),
                    );
                    if opts.complete {
                        if let Ok(messages) = &mut built {
                            v2::attach_complete(messages, &id, &ty, view.to_json()?);
                        }
                    }
                    Ok(Some(built))
                });
                match result {
                    Ok(Some(Ok(built))) => built,
                    Ok(Some(Err(()))) => {
                        *unknown.entry(ty).or_default() += 1;
                        Vec::new()
                    }
                    Ok(None) => {
                        unreadable += 1;
                        continue;
                    }
                    Err(error) if error.is::<crate::json::InvalidJson>() => {
                        anyhow::ensure!(
                            backing_utf8(arena, offset, len)?,
                            "OpenCode SQLite message data is not UTF-8"
                        );
                        unreadable += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let data = if rowid {
                    conn.query_row(
                        "SELECT data FROM session_message WHERE rowid = ?1",
                        rusqlite::params![row.get::<_, i64>(2)?],
                        |row| row.get::<_, String>(0),
                    )?
                } else {
                    row.get::<_, String>(2)?
                };
                let Ok(data) = serde_json::from_str::<Value>(&data) else {
                    unreadable += 1;
                    continue;
                };
                match v2::build(&id, &ty, &data, opts) {
                    Ok(mut built) => {
                        if opts.complete {
                            v2::attach_complete(&mut built, &id, &ty, data.into());
                        }
                        built
                    }
                    Err(()) => {
                        *unknown.entry(ty).or_default() += 1;
                        Vec::new()
                    }
                }
            };
            if let Some(record) = built.first_mut().and_then(|message| message.complete_record.as_mut()) {
                let mut model = s.model.clone();
                let record = std::sync::Arc::make_mut(record);
                for original in &mut record.projection {
                    if model.is_none() {
                        model = original.model.clone();
                    }
                    if original.model.is_some() && original.model == model {
                        original.model = None;
                    }
                }
            }
            for mut m in built {
                fold_into_session(&mut s, &mut m)?;
                if sink.message(m) == Flow::Stop {
                    return Ok(s);
                }
            }
        }
        if !unknown.is_empty() {
            s.harness_extra_mut(Harness::OpenCode)
                .insert("unknown_message_types".into(), json!(unknown));
        }
        super::super::note_skipped_lines(&mut s, unreadable);
        Ok(s)
    }
}

// ── pure mapping (filesystem-free, so it's unit-testable for both generations) ──────────────

/// Map one OpenCode message record + its parts into IR messages. Tool results are split into a
/// trailing `Role::Tool` message (mirroring the Claude adapter) so conversions re-encode them
/// correctly. Returns 0–2 messages.
fn build_messages(mv: &Value, parts: &[Value]) -> Vec<Message> {
    build_parts(mv, parts.iter().map(|v| (v, None)), None)
}

fn json_text(
    view: crate::json::JsonView<'_>,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
) -> Result<crate::lazy::Text> {
    crate::harness::store::text::append(source, arena, |writer| writer.push_json(view))
}

fn decoded_input(
    view: crate::json::JsonView<'_>,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
) -> Result<crate::json::Json> {
    if !view
        .source_range()?
        .is_some_and(|(_, len)| len > crate::lazy::INLINE_MAX as u64)
    {
        let value = view.resolve()?.into_owned();
        return Ok(match serde_json::from_str::<Value>(value.as_str().unwrap()) {
            Ok(decoded) => decoded.into(),
            Err(_) => value.into(),
        });
    }
    let (offset, len) = arena.append(|writer| {
        view.for_each_str_chunk(|chunk| {
            writer.write_all(chunk.as_bytes())?;
            Ok(())
        })?;
        Ok(())
    })?;
    if len != 0 {
        match source.read_backing_syntax(arena.clone(), offset, len, |decoded| {
            if decoded.accepts_value()? {
                Ok(Some(decoded.argument(true)?))
            } else {
                Ok(None)
            }
        }) {
            Ok(Some(decoded)) => return Ok(decoded),
            Ok(None) => {}
            Err(error) if error.is::<crate::json::InvalidJson>() => {}
            Err(error) => return Err(error),
        }
    }
    view.argument(true)
}

#[cfg(any(feature = "sqlite", test))]
fn project_shell(
    view: crate::json::JsonView<'_>,
    source: &crate::json::JsonSource,
    arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
) -> Result<v2::ProjectedShell> {
    let command = match view.get("command")? {
        Some(command) if command.is_string()? && nonempty_string(command)? => {
            Some(crate::harness::store::text::append(source, arena, |writer| {
                writer.push("$ ")?;
                anyhow::ensure!(writer.push_string(command)?, "expected shell command string");
                Ok(())
            })?)
        }
        _ => None,
    };
    let output = match view.get("output")? {
        Some(output) if !output.is_null()? => {
            let string = if output.is_string()? {
                Some(output)
            } else {
                // Presence wins before type: a non-string output field must NOT fall through
                // to text. The historical codec canonicalizes the entire object instead.
                match output.get("output")?.or(output.get("text")?) {
                    Some(value) if value.is_string()? => Some(value),
                    _ => None,
                }
            };
            match string {
                Some(string) if nonempty_string(string)? => string.source_text()?,
                Some(_) => None,
                None => Some(crate::harness::store::text::append(source, arena, |writer| {
                    writer.push_json(output)
                })?),
            }
        }
        _ => None,
    };
    Ok(v2::ProjectedShell { command, output })
}

fn nonempty_string(view: crate::json::JsonView<'_>) -> Result<bool> {
    match view.source_range()? {
        Some((_, len)) => Ok(len > 2),
        None => Ok(view.resolve()?.as_str().is_some_and(|text| !text.is_empty())),
    }
}

struct ProjectedHeader {
    value: Value,
    summary: Option<crate::lazy::Text>,
}

impl ProjectedHeader {
    fn read(view: crate::json::JsonView<'_>) -> Result<Self> {
        let keys = [
            "id",
            "role",
            "parentID",
            "time",
            "modelID",
            "model",
            "tokens",
            "cost",
            "agent",
            "mode",
            "providerID",
            "finish",
            "error",
            "variant",
            "structured",
            "format",
            "system",
            "tools",
            "path",
            "summary",
        ];
        let Some(fields) = view.select_fields(&keys)? else {
            return Ok(Self {
                value: Value::Null,
                summary: None,
            });
        };
        let mut object = Map::new();
        let mut summary = None;
        for (key, value) in keys.into_iter().zip(fields) {
            let Some(value) = value else { continue };
            if key == "summary" {
                if let Some(fields) = value.select_fields(&["body", "title", "diffs"])? {
                    let mut selected = Map::new();
                    let has_body = fields[0].is_some();
                    for (index, (key, value)) in ["body", "title", "diffs"].into_iter().zip(fields).enumerate() {
                        let Some(value) = value else { continue };
                        if index < 2 {
                            let text = value.source_text()?;
                            if index == 0 || !has_body {
                                summary = text;
                            }
                            selected.insert(
                                key.to_owned(),
                                if value.is_string()? {
                                    json!(if nonempty_string(value)? { "x" } else { "" })
                                } else {
                                    Value::Null
                                },
                            );
                        } else {
                            selected.insert(key.to_owned(), value.resolve()?.into_owned());
                        }
                    }
                    object.insert(key.to_owned(), Value::Object(selected));
                } else {
                    object.insert(key.to_owned(), value.as_bool()?.map(Value::Bool).unwrap_or(Value::Null));
                }
            } else {
                object.insert(key.to_owned(), value.resolve()?.into_owned());
            }
        }
        Ok(Self {
            value: Value::Object(object),
            summary,
        })
    }
}

struct ProjectedPart {
    value: Value,
    text: Option<crate::lazy::Text>,
    input: Option<crate::json::Json>,
    output: Option<crate::lazy::Text>,
    error: Option<crate::lazy::Text>,
}

impl ProjectedPart {
    fn read(
        view: crate::json::JsonView<'_>,
        source: &crate::json::JsonSource,
        arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
    ) -> Result<Self> {
        Self::read_inner(view, source, arena, false)
    }
    fn read_inner(
        view: crate::json::JsonView<'_>,
        source: &crate::json::JsonSource,
        arena: &std::sync::Arc<crate::harness::store::bytes::ByteSource>,
        v2: bool,
    ) -> Result<Self> {
        let mut part = Self {
            value: Value::Null,
            text: None,
            input: None,
            output: None,
            error: None,
        };
        if !view.is_object()? {
            part.value = view.resolve()?.into_owned();
            return Ok(part);
        }
        let ty = view
            .get("type")?
            .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
            .transpose()?
            .unwrap_or_default();
        let mut object = Map::new();
        view.for_each_entry(|key, value| {
            if key == "text" && matches!(ty.as_str(), "text" | "reasoning") && value.is_string()? {
                part.text = value.source_text()?;
                object.insert(key.to_owned(), json!(if nonempty_string(value)? { "x" } else { "" }));
            } else if key == "state" && ty == "tool" && value.is_object()? {
                let mut state = Map::new();
                value.for_each_entry(|key, value| {
                    if key == "input" {
                        part.input = Some(if v2 && value.is_string()? {
                            // 2.x accepts encoded scalars and preserves empty/partial strings.
                            // Its established scalar policy differs from OpenAI's helper.
                            decoded_input(value, source, arena)?
                        } else {
                            value.argument(true)?
                        });
                        state.insert(key.to_owned(), Value::Null);
                    } else if key == "content" && v2 && value.is_array()? {
                        // A single text result already has the exact joined representation.
                        // Retain its source range instead of copying it through another arena extent.
                        let mut content = value.array()?.unwrap();
                        if let Some(item) = content.next().transpose()? {
                            if content.next().transpose()?.is_none()
                                && item
                                    .get("type")?
                                    .map(|ty| ty.resolve().map(|ty| ty.as_str() == Some("text")))
                                    .transpose()?
                                    .unwrap_or(false)
                            {
                                part.output = Some(
                                    item.get("text")?
                                        .map(|text| text.source_text())
                                        .transpose()?
                                        .flatten()
                                        .unwrap_or_default(),
                                );
                                state.insert(key.to_owned(), json!([{"type":"text","text":""}]));
                                return Ok(());
                            }
                        }
                        let mut items = Vec::new();
                        let mut text_count = 0usize;
                        let output = crate::harness::store::text::append(source, arena, |writer| {
                            for item in value.array()?.unwrap() {
                                let item = item?;
                                let is_text = item
                                    .get("type")?
                                    .is_some_and(|v| v.resolve().is_ok_and(|v| v.as_str() == Some("text")));
                                if is_text {
                                    if text_count > 0 {
                                        writer.push("\n")?;
                                    }
                                    text_count += 1;
                                    if let Some(text) = item.get("text")? {
                                        writer.push_string(text)?;
                                    }
                                    items.push(json!({"type":"text", "text":""}));
                                } else {
                                    let is_file = item
                                        .get("type")?
                                        .map(|v| v.resolve().map(|v| v.as_str() == Some("file")))
                                        .transpose()?
                                        .unwrap_or(false);
                                    if !is_file {
                                        if text_count > 0 {
                                            writer.push("\n")?;
                                        }
                                        text_count += 1;
                                        writer.push_json(item)?;
                                        items.push(Value::Null);
                                    } else {
                                        items.push(item.resolve()?.into_owned());
                                    }
                                }
                            }
                            Ok(())
                        })?;
                        if text_count > 0 {
                            part.output = Some(output);
                        }
                        state.insert(key.to_owned(), Value::Array(items));
                    } else if !v2 && matches!(key, "output" | "error") {
                        let text = if value.is_string()? {
                            value.source_text()?
                        } else {
                            Some(json_text(value, source, arena)?)
                        };
                        if key == "output" {
                            part.output = text;
                        } else {
                            part.error = text;
                        }
                        state.insert(key.to_owned(), json!(""));
                    } else {
                        state.insert(key.to_owned(), value.resolve()?.into_owned());
                    }
                    Ok(())
                })?;
                object.insert(key.to_owned(), Value::Object(state));
            } else {
                object.insert(key.to_owned(), value.resolve()?.into_owned());
            }
            Ok(())
        })?;
        part.value = Value::Object(object);
        Ok(part)
    }
}

#[cfg(test)]
fn build_projected(mv: &Value, parts: &[ProjectedPart]) -> Vec<Message> {
    build_parts(mv, parts.iter().map(|p| (&p.value, Some(p))), None)
}

fn build_header_projected(header: &ProjectedHeader, parts: &[ProjectedPart]) -> Vec<Message> {
    build_parts(
        &header.value,
        parts.iter().map(|p| (&p.value, Some(p))),
        header.summary.as_ref(),
    )
}

fn build_parts<'a>(
    mv: &Value,
    parts: impl IntoIterator<Item = (&'a Value, Option<&'a ProjectedPart>)>,
    summary_text: Option<&crate::lazy::Text>,
) -> Vec<Message> {
    let role = match mv.get("role").and_then(Value::as_str) {
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => Role::User,
    };

    let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
    let mut m = match role {
        Role::System => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
        role => Message::new(role),
    };
    m.id = (!msg_id.is_empty()).then(|| msg_id.to_string());
    m.parent_id = mv.get("parentID").and_then(Value::as_str).map(str::to_string);
    m.timestamp = mv.pointer("/time/created").and_then(super::ts_from_value);
    // Newer records carry `modelID`; user records nest it under `model.modelID`.
    m.model = mv
        .get("modelID")
        .and_then(Value::as_str)
        .or_else(|| mv.pointer("/model/modelID").and_then(Value::as_str))
        .map(str::to_string);
    m.usage = parse_usage(mv);
    // Harness facts, gathered here and nested under `extra["opencode"]` at the end (module doc).
    let mut bag = Map::new();
    collect_message_extra(mv, &mut bag);
    // Kind evidence from the parts: a `compaction` part, an `agent` mention / `subtask` spawn.
    let mut compaction = false;
    let mut spawn = false;

    let mut tool_results: Vec<Block> = Vec::new();
    for (part, projected) in parts {
        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        m.content.push(Block::Text {
                            text: projected
                                .and_then(|p| p.text.clone())
                                .unwrap_or_else(|| t.to_string().into()),
                        });
                    }
                }
            }
            "reasoning" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    // Providers stash an opaque signature under `metadata.<provider>.signature`
                    // (e.g. anthropic). Preserve it so a thinking block can round-trip.
                    let signature = part
                        .pointer("/metadata/anthropic/signature")
                        .and_then(Value::as_str)
                        .or_else(|| find_signature(part.get("metadata")))
                        .map(str::to_string);
                    // A signature-only part (empty text, opaque blob) is still the whole of a
                    // redacted thinking turn: dropping it deleted the turn's only content, and with
                    // it the message — which cost a ported session 134 assistant turns and their
                    // usage, timestamps and ids.
                    if !t.is_empty() || signature.is_some() {
                        m.content.push(Block::Thinking {
                            text: projected
                                .and_then(|p| p.text.clone())
                                .unwrap_or_else(|| t.to_string().into()),
                            signature,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }
            "tool" => {
                let (use_block, results) = tool_blocks_projected(part, projected);
                m.content.push(use_block);
                tool_results.extend(results);
            }
            "file" => {
                m.content.push(file_block(part));
            }
            "agent" => {
                // An `@agent` mention / sub-agent reference embedded in a user prompt — opencode
                // routes the prompt to that agent in a child session. No IR block fits; surface
                // its name as text and keep the structured form in the bag.
                if let Some(name) = part.get("name").and_then(Value::as_str) {
                    m.content.push(Block::Text {
                        text: format!("@{name}").into(),
                    });
                }
                push_extra_array(&mut bag, "agent_refs", part.clone());
                spawn = true;
            }
            "subtask" => {
                // A spawned sub-session (Task tool style). Render its prompt + which agent ran
                // it; keep the full record (agent/model/command/description) in the bag.
                let agent = part.get("agent").and_then(Value::as_str).unwrap_or("agent");
                let prompt = part.get("prompt").and_then(Value::as_str).unwrap_or("");
                m.content.push(Block::Text {
                    text: format!("[subtask → {agent}] {prompt}").into(),
                });
                push_extra_array(&mut bag, "subtasks", part.clone());
                spawn = true;
            }
            "patch" => push_extra_array(&mut bag, "patches", part.clone()),
            "snapshot" => push_extra_array(&mut bag, "snapshots", part.clone()),
            "step-start" | "step-finish" => push_extra_array(&mut bag, "steps", part.clone()),
            "retry" => push_extra_array(&mut bag, "retries", part.clone()),
            "compaction" => {
                push_extra_array(&mut bag, "compactions", part.clone());
                compaction = true;
            }
            other if !other.is_empty() => {
                // Unknown / future part type: never drop it silently — stash verbatim.
                push_extra_array(&mut bag, "unknown_parts", part.clone());
            }
            _ => {}
        }
    }

    // Older inline-summary generation stored no parts — fall back to the AI summary so the turn
    // isn't blank. (`summary` must be an *object* here; a boolean `summary: true` is a newer
    // compaction marker: the `CompactionSummary` kind below.)
    if m.content.is_empty() {
        let summary = mv
            .pointer("/summary/body")
            .or_else(|| mv.pointer("/summary/title"))
            .and_then(Value::as_str);
        if let Some(text) = summary {
            if !text.is_empty() {
                m.content.push(Block::Text {
                    text: summary_text.cloned().unwrap_or_else(|| text.to_string().into()),
                });
            }
        }
    }

    // What the record IS, most specific first (module doc "Kinds").
    if compaction {
        m.kind = MessageKind::CompactionBoundary;
        m.origin = Origin::Harness;
    } else if mv.get("summary").and_then(Value::as_bool) == Some(true) {
        m.kind = MessageKind::CompactionSummary;
        m.origin = Origin::Harness;
    } else if mv.get("error").is_some_and(|e| !e.is_null()) {
        m.kind = MessageKind::Error;
        m.origin = Origin::Harness;
    } else if spawn {
        m.kind = MessageKind::SubagentSpawn;
    }
    if !bag.is_empty() {
        m.harness_extra_mut(Harness::OpenCode).extend(bag);
    }

    let mut out = Vec::new();
    if !m.content.is_empty() || !m.extra.is_empty() || m.kind != MessageKind::for_role(role) {
        out.push(m);
    }
    if !tool_results.is_empty() {
        let mut tm = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        tm.content = tool_results;
        out.push(tm);
    }
    out
}

/// Build the `ToolUse` block for a `tool` part, plus the blocks that belong in the Tool message:
/// the `ToolResult` (if the state carries one) followed by any `state.attachments[]` (files/images a
/// completed tool handed back — a `read` of an image, a screenshot — same shape as a `file` part).
/// The state's `title`/`metadata`/`time` ride in the result's `details`. Tolerant of the
/// `completed`/`error`/`running`/`pending` state shapes.
fn tool_blocks_projected(part: &Value, projected: Option<&ProjectedPart>) -> (Block, Vec<Block>) {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = part.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
    let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");

    let tool_name = (!name.is_empty()).then(|| name.clone());
    let status_field = (!status.is_empty()).then(|| status.to_string());
    let details = tool_details(part);
    let use_block = Block::ToolUse {
        id: call_id.clone(),
        name,
        input: projected
            .and_then(|p| p.input.clone())
            .unwrap_or_else(|| part.pointer("/state/input").cloned().unwrap_or(Value::Null).into()),
        namespace: None,
    };

    // A completed tool carries `output`; an error tool carries `error`.
    let result = match status {
        "completed" | "running" | "pending" => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: projected
                .and_then(|p| p.output.clone())
                .unwrap_or_else(|| coerce_text(out).into()),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: details.clone(),
        }),
        "error" => {
            let content = part
                .pointer("/state/error")
                .or_else(|| part.pointer("/state/output"))
                .map(coerce_text)
                .unwrap_or_default();
            Some(Block::ToolResult {
                tool_use_id: call_id.clone(),
                content: projected
                    .and_then(|p| {
                        if part.pointer("/state/error").is_some() {
                            p.error.clone()
                        } else {
                            p.output.clone()
                        }
                    })
                    .unwrap_or_else(|| content.into()),
                is_error: true,
                tool_name: tool_name.clone(),
                status: status_field.clone(),
                details: details.clone(),
            })
        }
        // Unknown status but an output is present — still surface it.
        _ => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: projected
                .and_then(|p| p.output.clone())
                .unwrap_or_else(|| coerce_text(out).into()),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: details.clone(),
        }),
    };

    let mut out: Vec<Block> = result.into_iter().collect();
    if let Some(atts) = part.pointer("/state/attachments").and_then(Value::as_array) {
        out.extend(atts.iter().map(file_block));
    }
    (use_block, out)
}

/// `state.{title, metadata, time}` of a tool part — the tool's own label (e.g. the file read),
/// structured metadata (exit code, diff stats, …) and start/end times — as the result's `details`.
fn tool_details(part: &Value) -> Option<Value> {
    let mut d = Map::new();
    for key in ["title", "metadata", "time"] {
        if let Some(v) = part.pointer(&format!("/state/{key}")) {
            let blank = v.is_null() || v.as_object().is_some_and(|o| o.is_empty()) || v.as_str() == Some("");
            if !blank {
                d.insert(key.to_string(), v.clone());
            }
        }
    }
    (!d.is_empty()).then_some(Value::Object(d))
}

/// Map a `file` part to a block. Genuine images become `Block::Image`; everything else
/// (text files, directories via `@path`, symbol/resource refs) becomes a first-class
/// `Block::File` reference.
fn file_block(part: &Value) -> Block {
    let mime = part.get("mime").and_then(Value::as_str).unwrap_or("");
    if mime.starts_with("image/") {
        Block::Image {
            media_type: (!mime.is_empty()).then(|| mime.to_string()),
            data_ref: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    } else {
        // `source.path` is the workspace-relative path when present; `filename`/`url` are fallbacks.
        let path = part
            .pointer("/source/path")
            .or_else(|| part.get("filename"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Block::File {
            mime: (!mime.is_empty()).then(|| mime.to_string()),
            path,
            source: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    }
}

/// Preserve message-level fields the IR has no home for, verbatim, in the record's
/// `extra["opencode"]` bag. (`cost`/`tokens` are [`Usage`]; `summary: true` is the
/// `CompactionSummary` kind; `error` also sets the `Error` kind.)
fn collect_message_extra(mv: &Value, bag: &mut Map<String, Value>) {
    for key in [
        "agent",      // which agent ran this turn (Sisyphus, build, compaction, …)
        "mode",       // deprecated alias of agent; kept for old records
        "providerID", // amazon-bedrock, anthropic, …
        "finish",     // finish reason: stop | tool-calls | length | …
        "error",      // assistant error object (aborted / overflow / api error)
        "variant",    // model variant
        "structured", // structured-output result
        "format",     // requested output format
        "system",     // per-message system prompt (user records)
        "tools",      // enabled-tools map (user records)
    ] {
        if let Some(v) = mv.get(key) {
            if !v.is_null() {
                bag.insert(key.to_string(), v.clone());
            }
        }
    }
    // `path: {cwd, root}` — per-turn working dir.
    if let Some(cwd) = mv.pointer("/path/cwd").and_then(Value::as_str) {
        bag.insert("cwd".to_string(), json!(cwd));
    }
    // The assistant `error` union is tagged by `name` — ProviderAuthError, UnknownError,
    // MessageOutputLengthError, MessageAbortedError, StructuredOutputError, ContextOverflowError,
    // ContentFilterError (2026-06), APIError — with the detail under `data`. Lift the tag so
    // consumers can filter on it without unpicking the object.
    if let Some(name) = mv.pointer("/error/name").and_then(Value::as_str) {
        bag.insert("error_name".to_string(), json!(name));
    }
    // The older inline summary's `diffs[]` are worth keeping even though we render body as text.
    if let Some(diffs) = mv.pointer("/summary/diffs") {
        if diffs.is_array() && !diffs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            bag.insert("summary_diffs".to_string(), diffs.clone());
        }
    }
}

fn push_extra_array(extra: &mut Map<String, Value>, key: &str, v: Value) {
    match extra.get_mut(key) {
        Some(Value::Array(a)) => a.push(v),
        _ => {
            extra.insert(key.to_string(), Value::Array(vec![v]));
        }
    }
}

/// Look for a `signature` string anywhere one provider-level deep inside `metadata`
/// (`metadata.<provider>.signature`), without hard-coding the provider name.
fn find_signature(metadata: Option<&Value>) -> Option<&str> {
    let obj = metadata?.as_object()?;
    for v in obj.values() {
        if let Some(sig) = v.pointer("/signature").and_then(Value::as_str) {
            return Some(sig);
        }
    }
    None
}

fn coerce_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `tokens{input, output, reasoning, cache{read, write}}` + `cost` (USD) of an assistant record.
/// `reasoning` is always written by opencode; it is reported only when non-zero.
fn parse_usage(mv: &Value) -> Option<Usage> {
    let t = mv.get("tokens").filter(|t| t.is_object());
    let cost = mv.get("cost").and_then(Value::as_f64);
    if t.is_none() && cost.is_none() {
        return None;
    }
    let get = |k: &str| t.and_then(|t| t.pointer(k)).and_then(Value::as_u64);
    Some(Usage {
        input_tokens: get("/input"),
        output_tokens: get("/output"),
        cache_read_tokens: get("/cache/read"),
        cache_creation_tokens: get("/cache/write"),
        reasoning_tokens: get("/reasoning").filter(|&r| r > 0),
        cost_usd: cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn parts(json_strs: &[&str]) -> Vec<Value> {
        json_strs.iter().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    #[test]
    fn parts_generation_text_reasoning_tool_split() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"msg_1","role":"assistant","modelID":"claude","agent":"build",
                "providerID":"anthropic","cost":0.01,"finish":"tool-calls",
                "time":{"created":1767000000000},
                "tokens":{"input":10,"output":5,"reasoning":3,"cache":{"read":1,"write":2}}}"#,
        )
        .unwrap();
        let p = parts(&[
            r#"{"type":"reasoning","text":"thinking...","metadata":{"anthropic":{"signature":"SIG"}},"time":{"start":1}}"#,
            r#"{"type":"text","text":"Hello"}"#,
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List","metadata":{"exit":0}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        // assistant message + a tool-role message
        assert_eq!(out.len(), 2);
        let asst = &out[0];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!(asst.model.as_deref(), Some("claude"));
        // thinking signature preserved
        match &asst.content[0] {
            Block::Thinking { text, signature, .. } => {
                assert_eq!(text, "thinking...");
                assert_eq!(signature.as_deref(), Some("SIG"));
            }
            b => panic!("expected thinking, got {b:?}"),
        }
        assert!(matches!(&asst.content[1], Block::Text { text } if text == "Hello"));
        assert!(matches!(&asst.content[2], Block::ToolUse { name, .. } if name == "bash"));
        assert_eq!((asst.kind, asst.origin), (MessageKind::Reply, Origin::Model));
        // message-level facts live in the harness bag only
        let bag = &asst.extra["opencode"];
        assert_eq!(bag["agent"], "build");
        assert_eq!(bag["finish"], "tool-calls");
        assert_eq!(bag["providerID"], "anthropic");
        assert!(
            bag.get("cost").is_none() && bag.get("reasoning_tokens").is_none(),
            "{bag}"
        );
        assert_eq!(asst.extra.len(), 1, "{:?}", asst.extra);
        // usage, cost and reasoning first-class
        let u = asst.usage.as_ref().unwrap();
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.cache_creation_tokens, Some(2));
        assert_eq!(u.reasoning_tokens, Some(3));
        assert_eq!(u.cost_usd, Some(0.01));
        // tool result became its own Tool message
        assert_eq!(out[1].role, Role::Tool);
        assert_eq!((out[1].kind, out[1].origin), (MessageKind::ToolResult, Origin::Harness));
        match &out[1].content[0] {
            Block::ToolResult {
                content,
                is_error,
                tool_use_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(content, "a\nb");
                assert!(!is_error);
                assert_eq!(tool_use_id, "c1");
                assert_eq!(tool_name.as_deref(), Some("bash"));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            b => panic!("expected tool result, got {b:?}"),
        }
    }

    #[test]
    fn tool_error_state_marks_is_error() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c","tool":"edit","state":{"status":"error","input":{},"error":"oldString not found","time":{"start":1,"end":2}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        let res = out.iter().flat_map(|m| &m.content).find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.to_string(), *is_error)),
            _ => None,
        });
        assert_eq!(res, Some(("oldString not found".to_string(), true)));
    }

    #[test]
    fn file_part_image_vs_reference() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"file","mime":"image/png","url":"file:///x.png","filename":"x.png"}"#,
            r#"{"type":"file","mime":"text/plain","url":"file:///Makefile","filename":"Makefile","source":{"type":"file","path":"Makefile"}}"#,
            r#"{"type":"file","mime":"application/x-directory","url":"file:///tools/","filename":"tools/"}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        // image -> Image block
        assert!(matches!(&m.content[0], Block::Image { media_type, .. } if media_type.as_deref()==Some("image/png")));
        // text file -> File reference, NOT an image
        assert!(matches!(&m.content[1], Block::File { path, .. } if path.as_deref()==Some("Makefile")));
        // directory -> File reference, NOT an image
        assert!(
            matches!(&m.content[2], Block::File { source, .. } if source.as_deref().unwrap_or("").contains("tools/"))
        );
    }

    #[test]
    fn agent_subtask_patch_snapshot_steps_preserved() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"agent","name":"oracle","source":{"value":"@oracle","start":0,"end":7}}"#,
            r#"{"type":"subtask","prompt":"do the thing","description":"d","agent":"build"}"#,
            r#"{"type":"patch","hash":"abc","files":["/a.rs"]}"#,
            r#"{"type":"snapshot","snapshot":"snap123"}"#,
            r#"{"type":"step-start"}"#,
            r#"{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
            r#"{"type":"retry","attempt":1,"error":{"name":"APIError"},"time":{"created":9}}"#,
            r#"{"type":"compaction","auto":false}"#,
            r#"{"type":"galaxy-brain-future-part","wat":true}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        let bag = &m.extra["opencode"];
        let len = |k: &str| bag.get(k).and_then(|v| v.as_array()).map(|a| a.len());
        assert_eq!(len("patches"), Some(1));
        assert_eq!(len("snapshots"), Some(1));
        assert_eq!(len("steps"), Some(2), "step-start/finish never surface as messages");
        assert_eq!(len("retries"), Some(1));
        assert_eq!(len("compactions"), Some(1));
        assert_eq!(len("agent_refs"), Some(1));
        assert_eq!(len("subtasks"), Some(1));
        // unknown part type is never dropped
        assert_eq!(len("unknown_parts"), Some(1));
        assert_eq!(m.extra.len(), 1, "nothing flat: {:?}", m.extra);
        // the compaction part outranks the spawn parts for the kind (it is the rarer, structural one)
        assert_eq!((m.kind, m.origin), (MessageKind::CompactionBoundary, Origin::Harness));
        // without it, the agent mention / subtask make the record a spawn
        let out = build_messages(&mv, &p[..2]);
        assert_eq!(
            (out[0].kind, out[0].origin),
            (MessageKind::SubagentSpawn, Origin::Model)
        );
        // agent + subtask also surface as text
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "@oracle")));
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("subtask → build"))));
    }

    #[test]
    fn inline_summary_generation_fallback() {
        // Older generation: no parts, inline summary object with diffs.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "summary":{"title":"Cleanup","body":"Removed debug comments.",
                "diffs":[{"file":"a.rs","before":"x","after":"y"}]}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text == "Removed debug comments."));
        assert_eq!((out[0].kind, out[0].origin), (MessageKind::Prompt, Origin::Human));
        // diffs preserved in the bag
        assert!(out[0].extra["opencode"].get("summary_diffs").is_some());
    }

    #[test]
    fn compaction_is_a_user_boundary_then_an_assistant_summary() {
        // As opencode writes it: the user-side request is one record with a lone `compaction`
        // part; the summary is an assistant record flagged `summary: true` (agent `compaction`).
        let req: Value =
            serde_json::from_str(r#"{"id":"u","role":"user","time":{"created":1},"agent":"Sisyphus"}"#).unwrap();
        let out = build_messages(&req, &parts(&[r#"{"type":"compaction","auto":true}"#]));
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(
            (b.role, b.kind, b.origin),
            (Role::User, MessageKind::CompactionBoundary, Origin::Harness)
        );
        assert!(b.content.is_empty(), "the boundary has no text of its own");
        assert_eq!(b.extra["opencode"]["compactions"][0]["auto"], true);

        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":2},"agent":"compaction","summary":true}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"compacted history…"}"#]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::Assistant, MessageKind::CompactionSummary, Origin::Harness)
        );
        assert!(
            m.extra["opencode"].get("is_compaction_summary").is_none(),
            "the kind says it"
        );
        // the boolean must NOT be rendered as a text body; the real text comes from parts
        assert!(matches!(&m.content[0], Block::Text { text } if text == "compacted history…"));
    }

    #[test]
    fn projected_headers_preserve_summary_precedence_and_part_fallback() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-header-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("message.json");
        let body = "summary\n\"text\" 🌟".repeat(9000);
        for summary in [
            json!({"body":body,"title":body,"diffs":[{"file":"a.rs"}]}),
            json!({"title":body}),
            json!({"body":"","title":body}),
            json!({"body":null,"title":body}),
            Value::Bool(true),
        ] {
            let value = json!({"id":"m","role":"assistant","time":{"created":2},"summary":summary,"ignored":body});
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let header = source.read_syntax(ProjectedHeader::read).unwrap();
            for with_parts in [false, true] {
                let parts = if with_parts {
                    vec![json!({"type":"text","text":"part wins"})]
                } else {
                    vec![]
                };
                let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
                let projected = parts
                    .iter()
                    .map(|part| ProjectedPart::read(part.into(), &source, &arena).unwrap())
                    .collect::<Vec<_>>();
                let mut actual = build_header_projected(&header, &projected);
                for message in &mut actual {
                    message.materialize(&crate::lazy::Resolver::new(None)).unwrap();
                }
                assert!(
                    serde_json::to_value(actual).unwrap()
                        == serde_json::to_value(build_messages(&value, &parts)).unwrap(),
                    "summary precedence differs with_parts={with_parts}"
                );
            }
        }
        let value = json!({"id":"m","role":"user","summary":{"body":body}});
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let header = source.read_syntax(ProjectedHeader::read).unwrap();
        let mut messages = build_header_projected(&header, &[]);
        std::fs::write(&path, b"{}").unwrap();
        assert!(messages[0].materialize(&crate::lazy::Resolver::new(None)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn projected_parts_preserve_resolved_payloads_and_errors() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-projection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("parts.json");
        let payload = "line\n\"quoted\"\t\\tail".repeat(9000);
        let parts = json!([
            {"type":"text", "text":payload},
            {"type":"reasoning", "text":payload, "metadata":{"anthropic":{"signature":"SIG"}}},
            {"type":"reasoning", "text":"", "metadata":{"anthropic":{"signature":"ONLY"}}},
            {"type":"tool", "callID":"one", "tool":"read", "state":{"status":"completed", "input":{"payload":payload}, "output":payload, "metadata":{"exit":0}}},
            {"type":"tool", "callID":"two", "tool":"read", "state":{"status":"error", "input":null, "error":{"message":"structured"}, "output":payload}},
            {"type":"tool", "callID":"three", "tool":"read", "state":{"status":"error", "input":true, "error":payload}},
            {"type":"tool", "callID":"four", "tool":"read", "state":{"status":"completed", "input":null, "output":{"payload":payload}}},
            {"type":"future", "payload":"preserved"}
        ]);
        std::fs::write(&path, serde_json::to_vec(&parts).unwrap()).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let projected = source
            .read_syntax(|view| {
                view.array()?
                    .unwrap()
                    .map(|part| ProjectedPart::read(part?, &source, &arena))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        let mv = json!({"id":"message", "role":"assistant", "modelID":"model", "tokens":{"input":1}});
        let expected = build_messages(&mv, parts.as_array().unwrap());
        let mut actual = build_projected(&mv, &projected);
        let resolver = crate::lazy::Resolver::new(Some(dir.join("wrong-primary")));
        for message in &mut actual {
            message.materialize(&resolver).unwrap();
        }
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn projected_v2_shell_matches_owned_command_output_priority_and_canonical_fallback() {
        let world = cv_testkit::World::in_process("opencode-shell-projection");
        let path = world.base.join("shell.json");
        let body = "escaped\n\" λ🌙".repeat(6000);
        let command = serde_json::to_string(&body).unwrap();
        let output = serde_json::to_string(&body).unwrap();
        let cases = [
            format!(r#"{{"command":{command},"output":{{"output":{output},"cursor":1,"size":3,"truncated":false}}}}"#),
            format!(r#"{{"command":"echo hi","output":{output}}}"#),
            format!(r#"{{"command":"echo hi","output":{{"text":{output}}}}}"#),
            format!(r#"{{"output":{{"output":null,"text":{output},"z":2,"a":1}}}}"#),
            format!(r#"{{"output":{{"output":false,"text":{output}}}}}"#),
            format!(r#"{{"output":{{"output":"","text":{output}}}}}"#),
            format!(
                r#"{{"command":"first","command":{command},"output":{{"output":"first","outpu\u0074":{output}}}}}"#
            ),
            format!(r#"{{"output":{{"z":{output},"a":1,"z":"duplicate winner"}}}}"#),
            r#"{"command":"","output":null}"#.into(),
            r#"{"command":null,"output":false}"#.into(),
            r#"{"output":1.00}"#.into(),
            r#"{"output":[1,"text",null]}"#.into(),
            r#"{"output":{}}"#.into(),
        ];
        for raw in cases {
            let mut owned: Value = serde_json::from_str(&raw).unwrap();
            owned
                .as_object_mut()
                .unwrap()
                .insert("time".into(), json!({"created":1}));
            owned
                .as_object_mut()
                .unwrap()
                .insert("metadata".into(), json!({"source":"shell","background":true}));
            // Preserve duplicate-key cases in the checked input instead of serializing the Value.
            let wire = format!(
                "{{\"time\":{{\"created\":1}},\"metadata\":{{\"source\":\"shell\",\"background\":true}},{}",
                &raw[1..]
            );
            fs::write(&path, &wire).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
            let shell = source
                .read_range_value(0, source.len(), |view| project_shell(view, &source, &arena))
                .unwrap();
            if owned
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|value| value.len() > crate::lazy::INLINE_MAX)
            {
                assert!(
                    matches!(shell.command, Some(crate::Text::Source(_))),
                    "large prefixed command stays checked"
                );
            }
            let mut skeleton = owned.clone();
            skeleton["command"] = match owned.get("command").and_then(Value::as_str) {
                Some(value) => json!(if value.is_empty() { "" } else { "x" }),
                None => Value::Null,
            };
            skeleton["output"] = Value::Null;
            let mut actual = v2::build_projected(
                "message",
                "shell",
                &skeleton,
                &ParseOptions::complete(),
                None,
                None,
                Some(&shell),
            )
            .unwrap();
            let expected = v2::build("message", "shell", &owned, &ParseOptions::complete()).unwrap();
            for message in &mut actual {
                message.materialize(&crate::lazy::Resolver::new(None)).unwrap();
            }
            assert!(
                serde_json::to_vec(&actual).unwrap() == serde_json::to_vec(&expected).unwrap(),
                "shell output differs for {}",
                &wire[..wire.len().min(200)]
            );
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_v2_shell_projection_preserves_modes_corpus_and_source_errors() {
        let world = cv_testkit::World::in_process("opencode-shell-db");
        let db = world.base.join("opencode.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data TEXT); INSERT INTO session_v2 VALUES('session','/synthetic','shell',1,1);").unwrap();
        let body = "escaped\nλ🌙".repeat(5000);
        let value = json!({"time":{"created":1},"shellID":"shell","command":"echo user-shell","status":"exited","exit":0,"output":{"output":body,"cursor":3,"size":3,"truncated":false},"metadata":{"background":true}});
        connection
            .execute(
                "INSERT INTO session_message VALUES('message','session','shell',1,?1)",
                [value.to_string()],
            )
            .unwrap();
        let adapter = OpenCode::with_db(db.clone());
        let reference = SessionRef::new("session", Harness::OpenCode, &db);
        for opts in [
            ParseOptions::lazy(),
            ParseOptions::facts(),
            ParseOptions {
                spans: true,
                ..ParseOptions::complete()
            },
        ] {
            let mut actual = crate::stream::collect_with(&adapter, &reference, &opts).unwrap();
            assert!(matches!(
                actual.messages[0].content[1],
                Block::Text {
                    text: crate::Text::Source(_)
                }
            ));
            actual.materialize().unwrap();
            // Complete demand now retains the actual native record. Compare against the
            // same owned demand, rather than the bulk projection that omits rich metadata.
            let owned_opts = ParseOptions { spans: false, ..opts };
            let owned = crate::stream::collect_with(&adapter, &reference, &owned_opts).unwrap();
            let actual = serde_json::to_vec(&actual).unwrap();
            let expected = serde_json::to_vec(&owned).unwrap();
            let mismatch = actual
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(actual.len().min(expected.len()));
            let context = |bytes: &[u8]| {
                String::from_utf8_lossy(&bytes[mismatch.saturating_sub(60)..bytes.len().min(mismatch + 120)])
                    .into_owned()
            };
            assert!(
                actual == expected,
                "serialized mismatch at byte {mismatch}: actual length {}, owned length {}; actual {:?}; owned {:?}",
                actual.len(),
                expected.len(),
                context(&actual),
                context(&expected)
            );
        }
        let mut retained = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
        connection.execute("UPDATE session_message SET data='{}'", []).unwrap();
        assert!(retained.materialize().is_err());
        connection
            .execute("UPDATE session_message SET data=?1", [value.to_string()])
            .unwrap();
        let mut sink = crate::stream::TryFnSink::new(|_| {
            connection.execute("UPDATE session_message SET data='{}'", []).unwrap();
            anyhow::bail!("consumer cap")
        });
        let error = adapter
            .stream(&reference, &ParseOptions::lazy(), &mut sink)
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed"), "{error:#}");
        assert!(!format!("{error:#}").contains("consumer cap"));
        // The current native user-shell record is part of the committed corpus, not a legacy edge.
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db");
        if !corpus.exists() {
            return; // The published crate excludes the real harness corpus.
        }
        let corpus_adapter = OpenCode::with_db(corpus.clone());
        // Locate its containing session read-only; fixture IDs should not become codec knowledge.
        let reader = crate::harness::store::sqlite::open_ro(&corpus).unwrap();
        let id: String = reader
            .query_row(
                "SELECT session_id FROM session_message WHERE type='shell' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let corpus_reference = SessionRef::new(id, Harness::OpenCode, &corpus);
        let expected = crate::stream::collect_with(&corpus_adapter, &corpus_reference, &ParseOptions::bulk()).unwrap();
        let mut actual =
            crate::stream::collect_with(&corpus_adapter, &corpus_reference, &ParseOptions::lazy()).unwrap();
        actual.materialize().unwrap();
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[test]
    fn projected_v2_tools_keep_join_order_and_scalar_arguments() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-projection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("content.json");
        let payload = "escaped\n\"text\"".repeat(9000);
        let content = json!([
            {"type":"text", "text":payload},
            {"type":"reasoning", "text":payload, "state":{"signature":"SIG"}},
            {"type":"tool", "id":"one", "name":"read", "state":{"status":"completed", "input":{"payload":payload}, "content":[{"type":"text", "text":payload},{"type":"file", "mime":"text/plain", "url":"file:///x"},{"type":"text", "text":""},{"future":payload}]}},
            {"type":"tool", "id":"two", "name":"read", "state":{"status":"completed", "input":"42", "content":[]}},
            {"type":"tool", "id":"three", "name":"read", "state":{"status":"error", "input":"", "error":{"message":"failure"}}},
            {"type":"tool", "id":"four", "name":"read", "state":{"status":"running", "input":serde_json::to_string(&json!({"payload":payload})).unwrap()}},
            {"type":"tool", "id":"five", "name":"read", "state":{"status":"running", "input":format!("true{}", " ".repeat(50000))}},
            {"type":"tool", "id":"six", "name":"read", "state":{"status":"running", "input":serde_json::to_string(&payload).unwrap()}},
            {"type":"tool", "id":"seven", "name":"read", "state":{"status":"running", "input":format!("{{partial {payload}")}},
            {"type":"tool", "id":"eight", "name":"read", "state":{"status":"completed", "content":[{"type":"text","text":payload}]}},
            {"type":"tool", "id":"nine", "name":"read", "state":{"status":"error", "content":[{"type":"text","text":42}],"error":{"message":"unused"}}},
            {"type":"tool", "id":"ten", "name":"read", "state":{"status":"completed", "content":[{"type":"text"}]}}
        ]);
        std::fs::write(&path, serde_json::to_vec(&content).unwrap()).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let arena = std::sync::Arc::new(crate::harness::store::bytes::ByteSource::arena().unwrap());
        let projected = source
            .read_syntax(|view| {
                view.array()?
                    .unwrap()
                    .map(|part| ProjectedPart::read_inner(part?, &source, &arena, true))
                    .collect::<Result<Vec<_>>>()
            })
            .unwrap();
        let original =
            json!({"content":content,"model":{"id":"model"},"tokens":{"input":1},"metadata":{"preserve":true}});
        let skeleton = json!({"content":projected.iter().map(|p|p.value.clone()).collect::<Vec<_>>(),"model":{"id":"model"},"tokens":{"input":1},"metadata":{"preserve":true}});
        let opts = ParseOptions::lazy();
        let expected = v2::build("message", "assistant", &original, &opts).unwrap();
        let mut actual =
            v2::build_projected("message", "assistant", &skeleton, &opts, None, Some(&projected), None).unwrap();
        for message in &mut actual {
            message.materialize(&crate::lazy::Resolver::new(None)).unwrap();
        }
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert!(matches!(projected[9].output, Some(crate::lazy::Text::Source(_))));
        std::fs::write(&path, b"[]").unwrap();
        assert!(projected[9]
            .output
            .as_ref()
            .unwrap()
            .try_resolve_prefix(&crate::lazy::Resolver::new(None), 8)
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn system_record_is_the_system_prompt() {
        let mv: Value = serde_json::from_str(r#"{"id":"s","role":"system","time":{"created":1}}"#).unwrap();
        let out = build_messages(&mv, &parts(&[r#"{"type":"text","text":"You are opencode."}"#]));
        let mut m = out.into_iter().next().unwrap();
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );
        let mut s = Session::new("x", Harness::OpenCode);
        fold_into_session(&mut s, &mut m).unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are opencode."));
        // the IR diet: a message model equal to the session's is dropped, a different one kept
        let mut a: Value =
            serde_json::from_str(r#"{"id":"a","role":"assistant","time":{"created":2},"modelID":"m1"}"#).unwrap();
        let mut first = build_messages(&a, &parts(&[r#"{"type":"text","text":"hi"}"#])).remove(0);
        fold_into_session(&mut s, &mut first).unwrap();
        assert_eq!(s.model.as_deref(), Some("m1"));
        assert_eq!(first.model, None);
        a["modelID"] = json!("m2");
        let mut second = build_messages(&a, &parts(&[r#"{"type":"text","text":"hi"}"#])).remove(0);
        fold_into_session(&mut s, &mut second).unwrap();
        assert_eq!(second.model.as_deref(), Some("m2"));
    }

    #[test]
    fn synthetic_text_still_rendered() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[r#"{"type":"text","text":"[search-mode] ...","synthetic":true}"#]);
        let out = build_messages(&mv, &p);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text.starts_with("[search-mode]")));
    }

    #[test]
    fn user_model_nested_under_model_object() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "model":{"providerID":"amazon-bedrock","modelID":"claude-opus"}}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"hi"}"#]);
        let out = build_messages(&mv, &p);
        assert_eq!(out[0].model.as_deref(), Some("claude-opus"));
    }

    /// End-to-end against on-disk fixtures for BOTH storage generations.
    #[test]
    fn parses_fixtures_both_generations() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");

        // Newer parts generation.
        let oc = OpenCode::with_root(base.join("parts_gen"));
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "one session in parts_gen fixture");
        let s = oc.parse(&refs[0]).unwrap();
        // Every OpenCode part fact is nested under `extra["opencode"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "bash")));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool && !m.content.is_empty()));
        assert_eq!(s.model.as_deref(), Some("anthropic.claude-sonnet-4-5"));

        // Older inline-summary generation (message records only, no part files).
        let oc2 = OpenCode::with_root(base.join("summary_gen"));
        let refs2 = oc2.discover().unwrap();
        assert_eq!(refs2.len(), 1, "one session in summary_gen fixture");
        let s2 = oc2.parse(&refs2[0]).unwrap();
        crate::harness::assert_no_flat_keys(&s2);
        assert!(s2
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.contains("debug comments"))));
    }

    #[test]
    fn tool_attachments_and_state_details_ride_with_the_result() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c1","tool":"read","state":{"status":"completed","input":{"filePath":"x.png"},"output":"","title":"x.png","metadata":{"preview":"…"},"time":{"start":1,"end":2},"attachments":[{"type":"file","mime":"image/png","url":"data:image/png;base64,iVBOR","filename":"x.png"},{"type":"file","mime":"text/plain","url":"file:///notes.txt","filename":"notes.txt"}]}}"#,
        ]);
        let out = build_messages(&mv, &p);
        assert_eq!(out.len(), 2);
        let tool = &out[1];
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.content.len(), 3, "result + two attachments");
        match &tool.content[0] {
            Block::ToolResult { details, .. } => {
                let d = details.as_ref().expect("details");
                assert_eq!(d["title"], "x.png");
                assert_eq!(d["time"]["end"], 2);
                assert_eq!(d["metadata"]["preview"], "…");
            }
            b => panic!("expected tool result, got {b:?}"),
        }
        assert!(
            matches!(&tool.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/png"))
        );
        assert!(matches!(&tool.content[2], Block::File { path, .. } if path.as_deref() == Some("notes.txt")));
    }

    #[test]
    fn error_name_is_lifted() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},
                "error":{"name":"ContentFilterError","data":{"message":"blocked"}}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        let m = &out[0];
        assert_eq!(
            (m.role, m.kind, m.origin),
            (Role::Assistant, MessageKind::Error, Origin::Harness)
        );
        assert_eq!(m.extra["opencode"]["error_name"], "ContentFilterError");
        assert_eq!(m.extra["opencode"]["error"]["data"]["message"], "blocked");
        // an abort that kept partial text is still an Error, text and all
        let aborted: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},"error":{"name":"MessageAbortedError","data":{}}}"#,
        )
        .unwrap();
        let out = build_messages(&aborted, &parts(&[r#"{"type":"text","text":"I was about to"}"#]));
        assert_eq!(out[0].kind, MessageKind::Error);
        assert_eq!(out[0].text().as_deref(), Some("I was about to"));
    }

    /// The real `opencode.db` DDL (opencode `fee476bb`, `packages/core/src/session/sql.ts`), minus
    /// the `project` foreign key. `with_metadata` adds the 2026-05 `session.metadata` column.
    #[cfg(feature = "sqlite")]
    fn make_db(path: &std::path::Path, with_metadata: bool) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        let metadata_col = if with_metadata { ", `metadata` text" } else { "" };
        conn.execute_batch(&format!(
            "CREATE TABLE `session` (`id` text PRIMARY KEY, `project_id` text NOT NULL, `parent_id` text, \
             `slug` text NOT NULL, `directory` text NOT NULL, `title` text NOT NULL, `version` text NOT NULL, \
             `share_url` text, `summary_additions` integer, `summary_deletions` integer, `summary_files` integer, \
             `summary_diffs` text, `revert` text, `permission` text, `time_created` integer NOT NULL, \
             `time_updated` integer NOT NULL, `time_compacting` integer, `time_archived` integer, \
             `workspace_id` text, `path` text, `agent` text, `model` text, `cost` real DEFAULT 0 NOT NULL, \
             `tokens_input` integer DEFAULT 0 NOT NULL, `tokens_output` integer DEFAULT 0 NOT NULL, \
             `tokens_reasoning` integer DEFAULT 0 NOT NULL, `tokens_cache_read` integer DEFAULT 0 NOT NULL, \
             `tokens_cache_write` integer DEFAULT 0 NOT NULL{metadata_col});\
             CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);\
             CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL, \
             `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);"
        ))
        .unwrap();
        conn
    }

    #[cfg(feature = "sqlite")]
    fn seed_session(conn: &rusqlite::Connection, sid: &str, with_metadata: bool) {
        let extra_cols = if with_metadata { ", metadata" } else { "" };
        let extra_vals = if with_metadata { ", '{\"origin\":\"test\"}'" } else { "" };
        conn.execute(
            &format!(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, \
                 time_updated, agent, model, cost, tokens_input, tokens_output, tokens_cache_read{extra_cols}) VALUES \
                 (?1, 'proj', 'ses_PARENT', '', '/home/dev/proj', 'Removing debug comments', '1.15.10', \
                 1767591058589, 1767591138434, 'Sisyphus', '{{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\",\"variant\":\"default\"}}', \
                 0.27, 42987, 2055, 123762{extra_vals})"
            ),
            rusqlite::params![sid],
        )
        .unwrap();
        // Message rows: `data` = Info minus id/sessionID (exactly what opencode writes).
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591060000, 1767591060000, ?3)",
            rusqlite::params![
                "msg_user01",
                sid,
                r#"{"role":"user","time":{"created":1767591060000},"model":{"providerID":"anthropic","modelID":"claude-sonnet-4-5"},"agent":"Sisyphus"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1767591135919, 1767591138413, ?3)",
            rusqlite::params![
                "msg_asst01",
                sid,
                r#"{"role":"assistant","time":{"created":1767591135919,"completed":1767591138413},"parentID":"msg_user01","modelID":"claude-sonnet-4-5","providerID":"anthropic","agent":"Sisyphus","path":{"cwd":"/home/dev/proj","root":"/home/dev/proj"},"cost":0.03,"tokens":{"input":8708,"output":7,"reasoning":0,"cache":{"read":20627,"write":0}},"finish":"tool-calls"}"#
            ],
        )
        .unwrap();
        // Part rows: `data` = Part minus id/sessionID/messageID; ordered by id.
        let add = |pid: &str, mid: &str, data: &str| {
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 1, 1, ?4)",
                rusqlite::params![pid, mid, sid, data],
            )
            .unwrap();
        };
        add(
            "prt_a",
            "msg_user01",
            r#"{"type":"text","text":"remove the debug comments"}"#,
        );
        add("prt_b", "msg_asst01", r#"{"type":"step-start","snapshot":"c2e2"}"#);
        add(
            "prt_c",
            "msg_asst01",
            r#"{"type":"reasoning","text":"scanning…","metadata":{"anthropic":{"signature":"SIG"}}}"#,
        );
        add("prt_d", "msg_asst01", r#"{"type":"text","text":"Reverted."}"#);
        add(
            "prt_e",
            "msg_asst01",
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"ls","metadata":{"exit":0},"time":{"start":1,"end":2}}}"#,
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn v2_projection_preserves_malformed_cell_failure_policy() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-cell-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data); INSERT INTO session_v2 VALUES('session','/tmp','test',1,1); INSERT INTO session_message VALUES('message','session','assistant',1,'{broken');").unwrap();
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        for ty in ["assistant", "shell"] {
            connection.execute("UPDATE session_message SET type=?1", [ty]).unwrap();
            for expression in [
                "'{broken'",
                "CAST(X'FF' AS TEXT)",
                "X'7B7D'",
                "NULL",
                "'{\"output\":\"\\ud800\"}'",
                "'{\"output\":1e400}'",
                "'{\"output\":\"\\ud800\",\"output\":\"valid duplicate winner\"}'",
            ] {
                connection
                    .execute_batch(&format!("UPDATE session_message SET data = {expression}"))
                    .unwrap();
                let expected = adapter.stream(&reference, &ParseOptions::bulk(), &mut |_| Flow::Continue);
                let actual = adapter.stream(&reference, &ParseOptions::lazy(), &mut |_| Flow::Continue);
                assert_eq!(expected.is_err(), actual.is_err(), "{expression}");
                if let (Ok(expected), Ok(actual)) = (expected, actual) {
                    assert_eq!(
                        serde_json::to_value(expected).unwrap(),
                        serde_json::to_value(actual).unwrap()
                    );
                }
            }
        }
        drop(connection);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_v1_discovery_counts_follow_readable_final_roles() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-role-count-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("opencode.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,data TEXT);").unwrap();
        let cases = [
            (
                r#"{"role":"system","role":"assistant","summary":{"body":"body"}}"#,
                0,
                1,
            ),
            (
                r#"{"role":"assistant","role":"system","summary":{"body":"body"}}"#,
                1,
                0,
            ),
            (r#"{role:'user',summary:{body:'body'}}"#, 1, 0),
            (
                r#"{"role":"assistant","summary":{"body":"body"},"ignored":1e400}"#,
                1,
                0,
            ),
            (
                r#"{"role":"assistant","summary":{"body":"body"},"ignored":"\ud800"}"#,
                1,
                0,
            ),
            (r#"{"role":"\u0075ser","summary":{"body":"body"}}"#, 1, 1),
        ];
        for (index, (data, old, new)) in cases.iter().enumerate() {
            let id = format!("s{index}");
            connection
                .execute("INSERT INTO session VALUES(?1,'/perf','roles',1,1)", [&id])
                .unwrap();
            connection
                .execute("INSERT INTO message VALUES(?1,?1,1,?2)", rusqlite::params![id, data])
                .unwrap();
            let old_count: usize = connection.query_row("SELECT COUNT(*) FROM message WHERE session_id=?1 AND json_extract(data,'$.role') IN ('user','assistant')", [&id], |row| row.get(0)).unwrap();
            assert_eq!(old_count, *old);
            let adapter = OpenCode::with_db(path.clone());
            let reference = adapter
                .discover()
                .unwrap()
                .into_iter()
                .find(|reference| reference.id == id)
                .unwrap();
            assert_eq!(reference.message_count, *new);
            let session = adapter.parse(&reference).unwrap();
            assert_eq!(
                session
                    .messages
                    .iter()
                    .filter(|message| matches!(message.role, Role::User | Role::Assistant))
                    .count(),
                *new
            );
        }
        drop(connection);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cold_wal_selected_text_keeps_original_guard_without_creating_sidecars() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-cold-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        let writer = make_db(&db, false);
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        seed_session(&writer, "session", false);
        let body = "selected ".repeat(8192);
        writer
            .execute(
                "UPDATE part SET data=?1 WHERE id='prt_a'",
                [json!({"type":"text","text":body}).to_string()],
            )
            .unwrap();
        let shm = dir.join("opencode.db-shm");
        fs::remove_file(&shm).unwrap();
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        assert_eq!(reference.message_count, 2);
        let mut messages = Vec::new();
        let mut sink = crate::stream::TryFnSink::new(|message| {
            messages.push(message);
            Ok(crate::stream::Flow::Continue)
        });
        adapter.stream(&reference, &ParseOptions::lazy(), &mut sink).unwrap();
        let Block::Text { text } = &messages[0].content[0] else {
            panic!("selected text")
        };
        assert_eq!(
            text.try_resolve_prefix(&crate::lazy::Resolver::new(None), u64::MAX)
                .unwrap()
                .as_ref(),
            body
        );
        assert!(!shm.exists(), "original store must remain untouched");
        writer
            .execute("UPDATE session SET title='changed' WHERE id='session'", [])
            .unwrap();
        assert!(
            text.try_resolve_prefix(&crate::lazy::Resolver::new(None), 8).is_err(),
            "copied payload must validate original WAL"
        );
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_snapshot_rejects_wal_mutation_even_after_sink_failure() {
        let dir = std::env::temp_dir().join(format!("cv-opencode-wal-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let db = dir.join("opencode.db");
        let writer = make_db(&db, false);
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        seed_session(&writer, "session", false);
        let adapter = OpenCode::with_db(db);
        let reference = adapter.discover().unwrap().remove(0);
        let mut sink = crate::stream::TryFnSink::new(|_message| {
            writer
                .execute("UPDATE session SET title = 'changed' WHERE id = 'session'", [])
                .unwrap();
            anyhow::bail!("consumer cap")
        });
        let error = adapter
            .stream(&reference, &ParseOptions::lazy(), &mut sink)
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed"), "{error:#}");
        assert!(!format!("{error:#}").contains("consumer cap"));
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_store_discover_and_parse() {
        for with_metadata in [false, true] {
            let dir = std::env::temp_dir().join(format!("cv-opencode-db-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let db = dir.join("opencode.db");
            let conn = make_db(&db, with_metadata);
            seed_session(&conn, "ses_DB01", with_metadata);
            drop(conn);

            let oc = OpenCode::with_db(db.clone());
            let refs = oc.discover().unwrap();
            assert_eq!(refs.len(), 1);
            let r = &refs[0];
            assert_eq!(r.id, "ses_DB01");
            assert_eq!(r.path, db, "a db-backed ref points at the database");
            assert_eq!(r.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(r.title.as_deref(), Some("Removing debug comments"));
            assert_eq!(
                r.message_count, 2,
                "user + assistant turns, from the role inside `data`"
            );
            assert!(r.created_at.is_some() && r.updated_at >= r.created_at);

            let s = oc.parse(r).unwrap();
            crate::harness::assert_no_flat_keys(&s);
            assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-5"), "session.model.id wins");
            assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/proj")));
            assert_eq!(s.lineage.parent.as_deref(), Some("ses_PARENT"), "parent_id is lineage");
            let bag = &s.extra["opencode"];
            assert_eq!(bag["agent"], "Sisyphus");
            assert!(bag.get("parent_id").is_none(), "first-class facts leave the bag");
            assert_eq!(bag["tokens_input"], 42987);
            assert_eq!(bag["model"]["providerID"], "anthropic");
            assert!(bag.get("slug").is_none(), "blank columns stay out of extra");
            assert_eq!(
                bag.get("metadata").map(|m| m["origin"] == "test"),
                with_metadata.then_some(true)
            );
            assert_eq!(
                s.extra.len(),
                1,
                "session facts live only in the harness bag: {:?}",
                s.extra
            );
            // user, assistant, tool — the same mapping the JSON tree gets
            let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
            assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
            let kinds: Vec<MessageKind> = s.messages.iter().map(|m| m.kind).collect();
            assert_eq!(
                kinds,
                vec![MessageKind::Prompt, MessageKind::Reply, MessageKind::ToolResult]
            );
            // both records name the session's own model, so neither carries it (IR diet)
            assert!(s.messages.iter().all(|m| m.model.is_none()), "{:?}", s.messages);
            assert_eq!(s.messages[1].usage.as_ref().unwrap().cost_usd, Some(0.03));
            for m in &s.messages {
                assert!(m.extra.keys().all(|k| k == "opencode"), "{:?}", m.extra);
            }
            assert_eq!(
                s.messages[0].id.as_deref(),
                Some("msg_user01"),
                "row id hydrated into the record"
            );
            assert_eq!(s.messages[1].parent_id.as_deref(), Some("msg_user01"));
            assert!(
                matches!(&s.messages[1].content[0], Block::Thinking { signature, .. } if signature.as_deref() == Some("SIG"))
            );
            assert!(matches!(&s.messages[1].content[1], Block::Text { text } if text == "Reverted."));
            assert!(matches!(&s.messages[2].content[0], Block::ToolResult { content, .. } if content == "a\nb"));
            assert_eq!(s.messages[1].usage.as_ref().unwrap().cache_read_tokens, Some(20627));
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn db_wins_over_a_json_duplicate_and_json_still_fills_gaps() {
        // The JSON fixture holds ses_FIX01; a database holding the same id (the import left both)
        // must yield ONE ref, the db-backed one — while a JSON-only session is still discovered.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen");
        let dir = std::env::temp_dir().join(format!("cv-opencode-dedupe-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let conn = make_db(&db, false);
        seed_session(&conn, "ses_FIX01", false);
        drop(conn);
        let oc = OpenCode {
            root: Some(base),
            db: Some(db.clone()),
        };
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "duplicate collapsed: {refs:?}");
        assert_eq!(refs[0].path, db);
        // and with a db that lacks it, the JSON copy is used
        let oc_json_only = OpenCode {
            root: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/parts_gen")),
            db: None,
        };
        assert_eq!(oc_json_only.discover().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// A `reasoning` part whose text is empty but which carries a provider signature is the WHOLE
    /// of a redacted thinking turn. Dropping it used to delete the turn's only block, and an
    /// assistant message with no content is discarded — which silently cost a real ported session
    /// 134 assistant turns along with their usage, timestamps and ids.
    #[test]
    fn signature_only_reasoning_keeps_its_turn() {
        let mv = serde_json::json!({"id": "msg_1", "role": "assistant", "modelID": "claude-x"});
        let parts = vec![serde_json::json!({
            "type": "reasoning",
            "text": "",
            "metadata": {"anthropic": {"signature": "SIGBLOB=="}}
        })];
        let msgs = build_messages(&mv, &parts);
        assert_eq!(msgs.len(), 1, "the turn must survive: {msgs:?}");
        assert!(
            matches!(
                msgs[0].content.as_slice(),
                [Block::Thinking { signature: Some(sig), redacted: false, .. }] if sig == "SIGBLOB=="
            ),
            "signature-only thinking preserved: {:?}",
            msgs[0].content
        );

        // A reasoning part with neither text nor signature carries nothing and is still dropped.
        let empty = vec![serde_json::json!({"type": "reasoning", "text": ""})];
        assert!(
            build_messages(&mv, &empty).is_empty(),
            "a contentless part adds no turn"
        );
    }
}
