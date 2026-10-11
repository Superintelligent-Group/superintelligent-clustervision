//! The read API every door maps onto: a decoded path plus a raw query string in, a `(status,
//! json)` pair out. `cv serve` puts it behind HTTP (adding the Host/CORS/token model and the two
//! tantivy routes it alone can answer); the desktop app calls it in-process. Neither has a second
//! implementation of a noun, so a consumer cannot tell which door a row came through
//! (docs/INTERFACE-V2.md §3) — the rows here are the same functions `cv <cmd> --json` prints with.
//!
//! Pure in the sense that matters: no socket, no headers, no process state. Every error is a JSON
//! `{"error": …}` with the status that names it (400 caller mistake, 404 absent, 410 removed with a
//! pointer, 500 broke).

use crate::ir::{Harness, SessionRef};
use crate::stream::{Flow, MessageSink, ParseOptions};
use crate::{Message, Session};
use serde_json::{json, Value};

/// Route a decoded path (`["api", "session", "claude", "abc"]`) plus a raw query string.
/// Never panics; an unknown path is a 404.
pub fn route_json(parts: &[&str], query: &str) -> (u16, Value) {
    match parts {
        ["api", "health"] => ok(json!({
            "ok": true,
            "harnesses": Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        })),

        ["api", "sessions"] => sessions(query),

        ["api", "session", harness, id] => session(harness, id),

        ["api", "session", harness, id, "head"] => session_head(harness, id),

        ["api", "session", harness, id, "messages"] => messages(harness, id, query),

        ["api", "session", harness, id, "events"] => session_events(harness, id, query),

        ["api", "session", harness, id, "compactions"] => compactions(harness, id),

        ["api", "touched"] => touched(query),

        ["api", "session", harness, id, "subagents"] => subagents(harness, id),

        ["api", "session", harness, parent, "subagent", agent] => subagent(harness, parent, agent),

        ["api", "session", harness, id, "workflow", wf, "script"] => workflow_script(harness, id, wf),

        // Removed in 0.15.0 with the coordination board: a pointer, not a silent 404.
        ["api", "board", ..] | ["api", "claims", _] | ["api", "who", _] | ["api", "channels"] => err(
            410,
            "removed in 0.15.0 with the coordination board — the task log is the fleet's record: \
             /api/tasks, /api/tasks/inbox/{who}, /api/task/{id}/events",
        ),

        ["api", "tasks"] => tasks(query),

        // Removed in 0.15.0 with the land facet: a pointer, not a silent 404.
        ["api", "tasks", "debt"] | ["api", "tasks", "stats"] => err(
            410,
            "removed in 0.15.0 with the task land facet — completion is self-reported and labeled; \
             read /api/tasks and /api/task/{id}",
        ),

        ["api", "tasks", "inbox", who] => tasks_inbox(who),

        ["api", "task", id] => task_one(id, Query::parse(query).flag("events")),

        ["api", "task", id, "events"] => task_events(id),

        _ => err(404, "not found"),
    }
}

// --- the task log -------------------------------------------------------------------------------

/// Replay the task log; on success hand the outcome to `f`. Log warnings ride every response so
/// a corrupted coordination log is visible from any consumer.
fn with_tasks(f: impl FnOnce(&crate::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match crate::task::replay() {
        Ok(outcome) => f(&outcome),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/tasks?state=&assignee=&repo=&tag=&all=` — task projections, oldest first.
fn tasks(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    with_tasks(|outcome| {
        let filter = crate::task::TaskFilter {
            state: params.get("state"),
            assignee: params.get("assignee"),
            repo: params.get("repo").map(std::path::PathBuf::from),
            include_terminal: params.flag("all"),
            tag: params.get("tag"),
            // The wire sees the whole store: scoping is the shell's default.
            ..Default::default()
        };
        // An unknown state string is a clear 400 (naming the vocabulary), not a silent [].
        let tasks: Vec<crate::task::TaskRow> = match crate::task::list(&outcome.model, &filter) {
            Ok(tasks) => tasks.into_iter().map(crate::task::TaskRow::full).collect(),
            Err(e) => return err(400, &e),
        };
        ok(json!({ "tasks": tasks, "warnings": outcome.warnings }))
    })
}

/// `GET /api/task/{id}[?events=1]` — one task's full projection, plus its raw history on request.
fn task_one(id: &str, with_events: bool) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let t = &outcome.model.tasks[&id];
        let mut v = json!({
            "task": t,
            "effective_state": crate::task::effective_display(t),
            "warnings": outcome.warnings,
        });
        if with_events {
            let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
            v["events"] = json!(history);
        }
        ok(v)
    })
}

/// `GET /api/task/{id}/events` — the raw durable history of one task.
fn task_events(id: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let id = match crate::task::resolve_id(&outcome.model, id) {
            Ok(id) => id.to_string(),
            Err(e) => return err(404, &e),
        };
        let history: Vec<&crate::task::TaskEvent> = outcome.events.iter().filter(|e| e.task_id == id).collect();
        ok(json!({ "events": history, "warnings": outcome.warnings }))
    })
}

/// `GET /api/tasks/inbox/{who}` — what needs this endpoint.
fn tasks_inbox(who: &str) -> (u16, Value) {
    with_tasks(|outcome| {
        let entries = crate::task::inbox_page(outcome, who, None, None, false, chrono::Utc::now()).items;
        ok(json!({ "inbox": entries, "warnings": outcome.warnings }))
    })
}

// --- sessions -----------------------------------------------------------------------------------

/// `GET /api/sessions?harness=&cwd=&limit=` — the §3 session rows, filtered, newest activity first.
fn sessions(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let harness = match params.get("harness") {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            // Validate up front so a typo is a clear 400 rather than silently empty.
            None => return err(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };
    let cwd = params.get("cwd");
    let limit = params.get("limit").and_then(|l| l.parse::<usize>().ok());

    // The fast catalog read (~ms when warm); transparently escalates to a full scan when cold.
    let mut refs = crate::sessions();
    if let Some(h) = harness {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .is_some_and(|c| c.to_string_lossy().contains(needle.as_str()))
        });
    }
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
    if let Some(n) = limit {
        refs.truncate(n);
    }
    ok(json!(refs
        .iter()
        .map(|r| crate::rows::session_row(r, size_of(r)))
        .collect::<Vec<_>>()))
}

/// The transcript's on-disk size, or `None` when it has been deleted since discovery.
fn size_of(r: &SessionRef) -> Option<u64> {
    std::fs::metadata(&r.path).ok().map(|m| m.len())
}

/// Look up one session by harness name + id (or unique prefix), with each failure already a reply.
fn lookup(harness: &str, id: &str) -> Result<(SessionRef, Box<dyn crate::Adapter>), (u16, Value)> {
    let h = Harness::parse(harness).ok_or_else(|| err(400, &format!("unknown harness {harness:?}")))?;
    match crate::find(id, Some(h)) {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(err(404, "session not found")),
        Err(e) => Err(err(500, &e.to_string())),
    }
}

/// `GET /api/session/{harness}/{id}` — the full parsed session (the IR), or 404.
fn session(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    match adapter.parse(&r) {
        Ok(session) => match serde_json::to_value(&session) {
            Ok(v) => ok(v),
            Err(e) => err(500, &e.to_string()),
        },
        Err(e) => err(500, &format!("parse failed: {e:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/head` — everything a session knows about ITSELF, and no
/// messages: the §3 row plus `model`, `git`, `system_prompt`, `lineage`, `extra` and the exact
/// message `total`.
///
/// Why this exists rather than folding into the windowed read: a window stops streaming once it
/// is full, so a session-level fact recorded LATER in the transcript — Claude writes its
/// `prompt_snapshot` well after the opening turns — is simply not reached, and asking the window
/// to keep scanning would make every transcript open pay for the whole file. One cheap pass that
/// keeps no messages answers it instead.
fn session_head(harness: &str, id: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let mut total = 0usize;
    let mut counter = |_m: Message| {
        total += 1;
        Flow::Continue
    };
    let shell = match adapter.stream(&r, &ParseOptions::lazy(), &mut counter) {
        Ok(shell) => shell,
        Err(e) => return err(500, &format!("stream failed: {e:#}")),
    };
    let mut row = crate::rows::session_row(&r, size_of(&r));
    let map = row.as_object_mut().expect("session_row is an object");
    // The freshly-parsed title/cwd beat the catalog's, which can be stale.
    if let Some(t) = &shell.title {
        map.insert("title".into(), json!(t));
    }
    if let Some(c) = &shell.cwd {
        map.insert("cwd".into(), json!(c.to_string_lossy()));
    }
    map.insert("total".into(), json!(total));
    map.insert("model".into(), json!(shell.model));
    map.insert("git".into(), json!(shell.git));
    map.insert("system_prompt".into(), json!(shell.system_prompt));
    map.insert("lineage".into(), json!(shell.lineage));
    map.insert("extra".into(), Value::Object(shell.extra));
    ok(row)
}

/// Collects the message window `[start, end)` as serialized IR, never holding more than the
/// window: out-of-window messages pass by as unmaterialized lazy handles, and the stream stops
/// at `end` (after noting whether a message exists there — the `has_more` probe).
struct WindowSink {
    resolver: crate::Resolver,
    /// Index of the next message to arrive.
    idx: usize,
    start: usize,
    /// Exclusive window end; `None` streams to EOF.
    end: Option<usize>,
    msgs: Vec<Value>,
    /// A message at `end` exists (so the window isn't the tail of the session).
    more: bool,
    /// Session metadata the stream delivered (codex meta / the parse-bridge), when any did —
    /// claude's native stream emits none.
    meta: Option<Value>,
    fail: Option<String>,
}

/// The `session` object a windowed read returns. Whatever a session knows about itself has to
/// arrive here or it never reaches a reader: the windowed path is the one every transcript open
/// uses.
fn session_meta_json(s: &Session) -> Value {
    json!({
        "title": s.title,
        "model": s.model,
        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
        "system_prompt": s.system_prompt,
        "lineage": s.lineage,
        "git": s.git,
    })
}

impl MessageSink for WindowSink {
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(session_meta_json(s));
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.idx;
        self.idx += 1;
        if idx < self.start {
            return Flow::Continue; // before the window: skip without touching content bytes
        }
        if self.end.is_some_and(|e| idx >= e) {
            self.more = true;
            return Flow::Stop; // at the window's end: note the existence, read no further
        }
        let mut m = m;
        m.materialize(&self.resolver); // resolve lazy spans for just this message
        match serde_json::to_value(&m) {
            Ok(v) => {
                self.msgs.push(v);
                Flow::Continue
            }
            Err(e) => {
                self.fail = Some(e.to_string());
                Flow::Stop
            }
        }
    }
}

/// `GET /api/session/{harness}/{id}/messages?start=&end=&extra=` — the window `[start, end)` of the
/// session's messages as IR JSON, without parsing the rest of a huge transcript.
///
/// A lazy stream that skips the messages before `start` without materializing them and **stops
/// at `end`** (the `cv show --range` pattern), so the tail is never read.
/// The reply carries the window's actual indices, `total_known` (+ `total` when the stream
/// reached EOF so the count is exact), and `has_more`.
///
/// `extra=1` additionally keeps each message's harness `extra` map (e.g. `compactMetadata` on a
/// compaction boundary) and the structured tool-result `details` sidecar — the structure view
/// pages through with this on to find the seams.
fn messages(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    // Window bounds must parse cleanly — a typo'd index is a clear 400, not a silent full read.
    let idx_param = |key: &str| -> Result<Option<usize>, (u16, Value)> {
        match params.get(key) {
            None => Ok(None),
            Some(v) => v
                .parse::<usize>()
                .map(Some)
                .map_err(|_| err(400, &format!("{key} must be a non-negative integer, got {v:?}"))),
        }
    };
    let start = match idx_param("start") {
        Ok(v) => v.unwrap_or(0),
        Err(e) => return e,
    };
    let end = match idx_param("end") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if end.is_some_and(|e| e < start) {
        return err(400, "end must be >= start");
    }
    let want_extra = params.flag("extra");

    let (r, adapter) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };

    // Lazy spans (so unread giant fields aren't materialized) plus, when asked, the `extra` maps.
    let opts = ParseOptions {
        extra: want_extra,
        ..ParseOptions::lazy()
    };
    let mut sink = WindowSink {
        resolver: crate::Resolver::new(Some(r.path.clone())),
        idx: 0,
        start,
        end,
        msgs: Vec::new(),
        more: false,
        meta: None,
        fail: None,
    };
    match adapter.stream(&r, &opts, &mut sink) {
        // Some adapters never call `meta()` — Claude is one — so the sink would stay empty and
        // the response say `session: null` for the single most-read harness. `stream` RETURNS
        // the session shell it assembled; use it when nothing was pushed.
        Ok(shell) if sink.meta.is_none() => sink.meta = Some(session_meta_json(&shell)),
        Ok(_) => {}
        Err(e) => return err(500, &format!("stream failed: {e:#}")),
    }
    if let Some(f) = sink.fail {
        return err(500, &f);
    }

    // The count is exact when the stream ran to EOF.
    let total_known = !sink.more;
    ok(json!({
        "harness": r.harness.as_str(),
        "id": r.id,
        "start": start,
        "end": start + sink.msgs.len(),
        "has_more": sink.more,
        "total_known": total_known,
        "total": total_known.then_some(sink.idx),
        // Discovery-time count, as a hint while `total` is unknown.
        "message_count": r.message_count,
        "session": sink.meta,
        "messages": sink.msgs,
    }))
}

/// `GET /api/session/{harness}/{id}/events?kind=` — the session's extracted tool events (file
/// edits/reads, commands, errors) from the event catalog, in transcript order, as `cv events
/// --json` rows. A stale or missing catalog row is (re)ingested on the spot.
fn session_events(harness: &str, id: &str, query: &str) -> (u16, Value) {
    use crate::events;
    let kind = Query::parse(query).get("kind");
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        if let Err(e) = events::ingest_ref(&r) {
            return err(500, &format!("event ingest failed: {e:#}"));
        }
    }
    let out: Vec<Value> = events::events_for(r.harness.as_str(), &r.id, kind.as_deref())
        .iter()
        .map(crate::rows::event_row)
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/compactions` — every compaction boundary in the session, in
/// transcript order, as `cv show --kind compaction_boundary --json` rows plus a `headline` for listings.
///
/// Built on [`crate::compaction::detect`], which streams the whole transcript (memory-safe on a
/// multi-GB session) and pairs each `compact_boundary` with the `isCompactSummary` message that
/// seeds the next window, keeping that **summary text** — how a continued agent recovers the
/// context it can no longer see. `pre_compaction_span` is the `[start, boundary)` range that was
/// compacted away, in the same numbering `/messages` uses.
fn compactions(harness: &str, id: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let boundaries = match crate::compaction::detect(&r, true) {
        Ok(b) => b,
        Err(e) => return err(500, &format!("compaction scan failed: {e:#}")),
    };
    let out: Vec<Value> = (0..boundaries.len())
        .map(|n| {
            let mut v = crate::compaction::row(&boundaries, n);
            if let Some(obj) = v.as_object_mut() {
                // An addition for the dashboard, never a rename: the CLI's keys stay the CLI's.
                obj.insert("headline".into(), json!(boundaries[n].headline(n + 1)));
            }
            v
        })
        .collect();
    // A bare array, like every other list this API serves.
    ok(json!(out))
}

/// `GET /api/touched?path=&edits_only=` — sessions whose tool events touched a file (exact or
/// suffix path match), newest activity first, as `cv touched --json` rows. Empty on a cold catalog.
fn touched(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(path) = params.get("path") else {
        return err(400, "missing path parameter");
    };
    let out: Vec<Value> = crate::events::sessions_touching(&path, params.flag("edits_only"))
        .iter()
        .map(crate::rows::touched_row)
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/subagents` — the sub-agent **forest** a session spawned
/// (directly-spawned `Agent`/`Task` agents and the workflow tier one level deeper), each a §3
/// session row plus the sidecar enrichment the forest view groups by. Empty array if none.
fn subagents(harness: &str, id: &str) -> (u16, Value) {
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let out: Vec<Value> = crate::subagent_tree_of(&r)
        .iter()
        .map(|s| {
            let mut row = crate::rows::session_row(&s.session, size_of(&s.session));
            let map = row.as_object_mut().expect("session_row is an object");
            map.insert("agent_id".into(), json!(s.agent_id()));
            map.insert("agent_type".into(), json!(s.agent_type));
            map.insert("description".into(), json!(s.description));
            map.insert("tool_use_id".into(), json!(s.tool_use_id));
            map.insert("workflow".into(), json!(s.workflow));
            map.insert("result_status".into(), json!(s.result_status));
            map.insert("result_summary".into(), json!(s.result_summary));
            row
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{parent}/subagent/{agent}` — the full parsed transcript of one
/// sub-agent (sub-agents aren't in the main pool, so they're loaded relative to their parent);
/// `agent` may be the full transcript id or the bare `agentId` the journal uses.
fn subagent(harness: &str, parent: &str, agent: &str) -> (u16, Value) {
    let (r, adapter) = match lookup(harness, parent) {
        Ok(x) => x,
        Err((404, _)) => return err(404, "parent session not found"),
        Err(e) => return e,
    };
    let Some(sr) = crate::subagent_tree_of(&r)
        .into_iter()
        .find(|s| s.session.id == agent || s.agent_id() == agent)
        .map(|s| s.session)
    else {
        return err(404, "subagent not found");
    };
    match adapter.parse(&sr) {
        Ok(session) => match serde_json::to_value(&session) {
            Ok(v) => ok(v),
            Err(e) => err(500, &e.to_string()),
        },
        Err(e) => err(500, &format!("parse failed: {e:#}")),
    }
}

/// `GET /api/session/{harness}/{id}/workflow/{wf}/script` — the driving script Claude Code recorded
/// for a workflow run, as `{ workflow, name, source }`. Scripts live in the session's sidecar tree
/// at `<session>/workflows/scripts/<slug>-<wf>.js`; the run id (`wf_…`) is matched as the filename
/// suffix (the slug prefix varies per workflow). 404 if there's no script for that run.
fn workflow_script(harness: &str, id: &str, wf: &str) -> (u16, Value) {
    // Reject a path-y run id up front — it's only ever a `wf_…` token, never a path component.
    if wf.contains('/') || wf.contains("..") || wf.is_empty() {
        return err(400, "invalid workflow id");
    }
    let (r, _) = match lookup(harness, id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(scripts_dir) = crate::harness::claude::sidecar::workflows_dir(&r.path).map(|d| d.join("scripts")) else {
        return err(404, "session has no sidecar tree");
    };
    let found = std::fs::read_dir(&scripts_dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("js"))
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == wf || s.ends_with(&format!("-{wf}")))
            })
    });
    let Some(path) = found else {
        return err(404, "no script recorded for that workflow run");
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => ok(json!({
            "workflow": wf,
            "name": path.file_name().and_then(|n| n.to_str()),
            "source": source,
        })),
        Err(e) => err(500, &format!("could not read script: {e}")),
    }
}

// --- small helpers ------------------------------------------------------------------------------

/// A trivially-parsed query string: `k=v&k2=v2`, percent-decoded, `+` as space.
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn parse(q: &str) -> Self {
        Query(
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (decode(&k.replace('+', " ")), decode(&v.replace('+', " ")))
                })
                .collect(),
        )
    }

    /// First non-empty value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }

    /// A boolean parameter: present with any value but `0`/`false` (so `?all=1`, `?all=true` and a
    /// bare `?all` all count).
    pub fn flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .any(|(k, v)| k == key && v != "0" && !v.eq_ignore_ascii_case("false"))
    }
}

/// Percent-decode one URL component (a path segment or a query value), lossily for bad UTF-8.
pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decodes_plus_percent_and_flags() {
        let q = Query::parse("who=ember&since=2h&x=a+b%21&all=1&off=0&bare");
        assert_eq!(q.get("who").as_deref(), Some("ember"));
        assert_eq!(q.get("x").as_deref(), Some("a b!"));
        assert_eq!(q.get("missing"), None);
        assert!(q.flag("all"));
        assert!(!q.flag("off"));
        assert!(!q.flag("missing"));
        // A bare key is an empty value: not a `get` hit, but a flag.
        assert_eq!(q.get("bare"), None);
        assert!(q.flag("bare"));
    }

    #[test]
    fn decode_handles_segments() {
        assert_eq!(decode("%2Fwork%2Fproj"), "/work/proj");
        assert_eq!(decode("plain"), "plain");
    }

    #[test]
    fn removed_routes_point_not_404() {
        for parts in [
            &["api", "board", "fleet"][..],
            &["api", "channels"],
            &["api", "tasks", "debt"],
        ] {
            let (status, v) = route_json(parts, "");
            assert_eq!(status, 410, "{parts:?}");
            assert!(v["error"].as_str().unwrap().contains("removed in 0.15.0"), "{v}");
        }
        assert_eq!(route_json(&["api", "nope"], "").0, 404);
        assert_eq!(route_json(&["api", "sessions"], "harness=warpdrive").0, 400);
    }
}
