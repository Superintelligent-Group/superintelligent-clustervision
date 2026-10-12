//! The state dir's registries beside the transcripts: the cron registry and the sub-agent
//! registry, read as a session's `schedule`/`state` and `subagent_spawn`/`state` rows
//! ([`crate::harness::Adapter::store_affordances`]).
//!
//! Both are keyed by **session key** (`agent:<agentId>:<kind>:<…>`), never by session id, so the
//! session's key is looked up first (`sessions.json` beside a JSONL transcript, `session_windows`
//! in the sqlite store). Layout by era, under the state dir (`agents/`'s parent):
//!
//! - **JSONL era (2026.1.29):** `cron/jobs.json` `{version, jobs: [{id, agentId, name, enabled,
//!   createdAtMs, schedule: {kind: at|cron|every, atMs|at|expr|everyMs}, sessionTarget, payload:
//!   {kind, message}, state: {lastRunAtMs, lastStatus}}]}` (+ `cron/runs/<jobId>.jsonl`, the run
//!   log), and `subagents/runs.json` `{version, runs: {<runId>: {childSessionKey,
//!   requesterSessionKey, task, label?, createdAt, endedAt?, outcome: {status}}}}`.
//! - **sqlite era (2026.10.1):** `state/openclaw.sqlite` `cron_jobs(job_json, state_json, …)`
//!   (`job_json` is the old `jobs.json` entry, plus `owner: {sessionKey}` for a job an agent
//!   made) and `subagent_runs(requester_session_key, controller_session_key, payload_json)`
//!   (`payload_json`: `task`, `label`, `execution.outcome.status`, `completion.resultText`).
//!
//! A job belongs to a session when the session made it (`owner.sessionKey`,
//! `scheduledToolPolicy.ownerSessionKey`), when the session is one of its runs
//! (`agent:<a>:cron:<jobId>[:run:<id>]`), or when it fires into the session (`sessionTarget:
//! "main"` and the key `agent:<agentId>:main`). A 1.29 job records no maker, so its maker's `cron`
//! tool call is its only trace there. A sub-agent run belongs to the session that requested or
//! controls it; the child already names its parent through `Lineage::parent`.

use crate::affordance::{Affordance, Kind};
use crate::ir::{truncate, SessionRef};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Every registry row that belongs to `r`.
pub(super) fn affordances(r: &SessionRef) -> Vec<Affordance> {
    // `<state>/agents/<agent>/sessions/<sid>.jsonl` and `<state>/agents/<agent>/agent/<db>.sqlite`
    // sit at the same depth.
    let Some(state) = r.path.ancestors().nth(4) else {
        return Vec::new();
    };
    let Some(key) = session_key(r) else {
        return Vec::new();
    };
    let (jobs, runs) = if super::is_sqlite_store_path(&r.path) {
        sqlite_registries(state)
    } else {
        (jsonl_jobs(state), jsonl_runs(state))
    };
    let mut out: Vec<Affordance> = jobs.iter().filter(|j| job_belongs(j, &key)).map(job_row).collect();
    out.extend(runs.iter().filter(|(by, _)| by.contains(&key)).map(|(_, p)| run_row(p)));
    out
}

/// The session's key: the `sessions.json` entry naming it, or its `session_windows` row.
fn session_key(r: &SessionRef) -> Option<String> {
    if super::is_sqlite_store_path(&r.path) {
        return sqlite_session_key(r);
    }
    let text = fs::read_to_string(r.path.parent()?.join("sessions.json")).ok()?;
    let Ok(Value::Object(index)) = serde_json::from_str::<Value>(&text) else {
        return None;
    };
    index
        .iter()
        .find(|(_, e)| e.get("sessionId").and_then(Value::as_str) == Some(r.id.as_str()))
        .map(|(k, _)| k.clone())
}

#[cfg(feature = "sqlite")]
fn sqlite_session_key(r: &SessionRef) -> Option<String> {
    let conn = super::open_ro(&r.path).ok()?;
    conn.query_row(
        "SELECT session_key FROM session_windows WHERE session_id = ?1",
        [&r.id],
        |row| row.get(0),
    )
    .ok()
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_session_key(_r: &SessionRef) -> Option<String> {
    None
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn jsonl_jobs(state: &Path) -> Vec<Value> {
    read_json(&state.join("cron").join("jobs.json"))
        .and_then(|v| v.get("jobs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// A sub-agent run: the keys it belongs to (requester, controller) and its record.
type Run = (Vec<String>, Value);

fn run_keys(p: &Value) -> Vec<String> {
    ["requesterSessionKey", "controllerSessionKey"]
        .iter()
        .filter_map(|k| p.get(*k).and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn jsonl_runs(state: &Path) -> Vec<Run> {
    let Some(Value::Object(runs)) =
        read_json(&state.join("subagents").join("runs.json")).and_then(|v| v.get("runs").cloned())
    else {
        return Vec::new();
    };
    let mut out: Vec<Run> = runs.into_iter().map(|(_, p)| (run_keys(&p), p)).collect();
    out.sort_by_key(|(_, p)| p.get("createdAt").and_then(Value::as_i64).unwrap_or(0));
    out
}

#[cfg(feature = "sqlite")]
fn sqlite_registries(state: &Path) -> (Vec<Value>, Vec<Run>) {
    use super::super::store::sqlite::table_exists;
    let Ok(conn) = super::open_ro(&state.join("state").join("openclaw.sqlite")) else {
        return Default::default();
    };
    let query = |sql: &str| -> Vec<String> {
        conn.prepare(sql)
            .and_then(|mut st| st.query_map([], |row| row.get(0))?.collect())
            .unwrap_or_default()
    };
    let parse = |t: String| serde_json::from_str::<Value>(&t).ok();
    let jobs = if table_exists(&conn, "cron_jobs") {
        // The runtime state rides in its own column; fold it back where the 1.29 entry kept it.
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT job_json, state_json FROM cron_jobs ORDER BY sort_order, updated_at, job_id")
            .and_then(|mut st| st.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect())
            .unwrap_or_default();
        rows.into_iter()
            .filter_map(|(job, st)| {
                let mut job = parse(job)?;
                if let (Some(o), Some(st)) = (job.as_object_mut(), parse(st)) {
                    o.insert("state".into(), st);
                }
                Some(job)
            })
            .collect()
    } else {
        Vec::new()
    };
    let runs = if table_exists(&conn, "subagent_runs") {
        query("SELECT payload_json FROM subagent_runs ORDER BY created_at, run_id")
            .into_iter()
            .filter_map(parse)
            .map(|p| (run_keys(&p), p))
            .collect()
    } else {
        Vec::new()
    };
    (jobs, runs)
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_registries(_state: &Path) -> (Vec<Value>, Vec<Run>) {
    Default::default()
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn job_belongs(job: &Value, key: &str) -> bool {
    let owner = ["/owner/sessionKey", "/scheduledToolPolicy/ownerSessionKey"]
        .iter()
        .any(|p| str_at(job, p) == Some(key));
    let id = str_at(job, "/id").unwrap_or("");
    let mut parts = key.split(':');
    let (agent, kind, rest) = (parts.nth(1), parts.next(), parts.next());
    let run = kind == Some("cron") && !id.is_empty() && rest == Some(id);
    let main = str_at(job, "/sessionTarget") == Some("main")
        && kind == Some("main")
        && agent.is_some()
        && str_at(job, "/agentId") == agent;
    owner || run || main
}

/// `corpus-ping · at 2026-10-11T20:48:42Z · Reply with the word pong. · last ok`.
fn job_row(job: &Value) -> Affordance {
    let when = match str_at(job, "/schedule/kind") {
        Some("cron") => str_at(job, "/schedule/expr").map(str::to_string),
        Some("every") => job
            .pointer("/schedule/everyMs")
            .and_then(Value::as_i64)
            .map(|ms| format!("every {}s", ms / 1000)),
        Some("at") => str_at(job, "/schedule/at").map(|a| format!("at {a}")).or_else(|| {
            job.pointer("/schedule/atMs")
                .and_then(Value::as_i64)
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|t| format!("at {}", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        }),
        _ => None,
    };
    let name = str_at(job, "/displayName").or_else(|| str_at(job, "/name"));
    let message = str_at(job, "/payload/message").or_else(|| str_at(job, "/payload/kind"));
    let last = str_at(job, "/state/lastStatus").map(|s| format!("last {s}"));
    let disabled = (job.get("enabled").and_then(Value::as_bool) == Some(false)).then(|| "disabled".to_string());
    let parts: Vec<String> = [
        name.map(str::to_string),
        when,
        message.map(str::to_string),
        last,
        disabled,
    ]
    .into_iter()
    .flatten()
    .filter(|p| !p.is_empty())
    .collect();
    Affordance::store(Kind::Schedule, "cron", truncate(&parts.join(" · "), 120), Vec::new())
}

/// `ok-reply-check · Reply with the single word ok. · ok → ok`.
fn run_row(p: &Value) -> Affordance {
    let who = str_at(p, "/label")
        .or_else(|| str_at(p, "/childSessionKey"))
        .unwrap_or("");
    let task = str_at(p, "/task").unwrap_or("");
    let status = str_at(p, "/execution/outcome/status").or_else(|| str_at(p, "/outcome/status"));
    let mut summary = format!("{who} · {task}");
    if let Some(s) = status {
        summary.push_str(&format!(" · {s}"));
    }
    if let Some(res) = str_at(p, "/completion/resultText") {
        summary.push_str(&format!(" → {res}"));
    }
    let timestamp = p
        .get("createdAt")
        .and_then(Value::as_i64)
        .and_then(chrono::DateTime::from_timestamp_millis);
    Affordance {
        timestamp,
        ..Affordance::store(Kind::SubagentSpawn, "subagents", truncate(&summary, 120), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The three ways a job belongs to a session: it made the job, it is one of the job's runs, or
    /// the job fires into it.
    #[test]
    fn a_job_belongs_to_its_owner_its_runs_and_its_main_target() {
        let made = json!({"id": "j1", "owner": {"sessionKey": "agent:main:explicit:s1"}, "sessionTarget": "isolated"});
        assert!(job_belongs(&made, "agent:main:explicit:s1"));
        assert!(job_belongs(&made, "agent:main:cron:j1"));
        assert!(job_belongs(&made, "agent:main:cron:j1:run:r9"));
        assert!(!job_belongs(&made, "agent:main:cron:j2"));
        assert!(!job_belongs(&made, "agent:main:main"));
        let heartbeat = json!({"id": "hb", "agentId": "main", "sessionTarget": "main"});
        assert!(job_belongs(&heartbeat, "agent:main:main"));
        assert!(!job_belongs(&heartbeat, "agent:other:main"));
        let every = json!({"name": "hb", "schedule": {"kind": "every", "everyMs": 1_800_000}, "payload": {"kind": "heartbeat"}});
        assert_eq!(job_row(&every).summary, "hb · every 1800s · heartbeat");
    }
}
