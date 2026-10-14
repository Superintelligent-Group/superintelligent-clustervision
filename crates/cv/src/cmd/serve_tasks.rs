//! The inbox's routes on `cv serve` (what `cv task serve` was): reads shaped for the one inline
//! page (`serve_tasks_page.html`, served at `/inbox`) and the writes its buttons make, which record
//! **the same events the CLI records**, so the page and the shell are one store. Every write is
//! stamped `web:<who>`. The HTTP model (Host check, CORS allow-list, bearer token, the
//! non-loopback refusal) is `cv serve`'s; this module never sees a socket.
//!
//! Routes:
//! - `GET /api/inbox?who=`                the inbox page (`task::inbox_page`, `all=1` lifts the window)
//! - `POST /api/task/<id>/resolve`        `{choice}` | `{accept_default: true}` | `{confirm: true}` (a
//!   provisional resolution), `{note}` optional
//! - `POST /api/task/<id>/done`           `{observed}` optional
//! - `POST /api/task/<id>/note`           `{text}`
//! - `POST /api/task/<id>/discuss`        `{text}` optional → a `NEEDS DISCUSSION:` note + the `discuss` tag
//! - `POST /api/task/<id>/claim`          claim for `who`
//! - `POST /api/task/<id>/release`
//! - `POST /api/task/<id>/reopen`         a closed task comes back as a NEW task (the record stays)
//! - `GET /api/events?since=&kind=&by=&not_by=&assignee=&task=`  JSON lines — the same query as
//!   `cv task events`, so a poller and the page see one feed.
//! - `GET /api/lanes?session=<id>`        the sub-agent forest of a session (`cv lanes --tasks`),
//!   running first, each lane with the `tasks` its endpoint holds.
//! - `GET /api/backlog?repo=&state=&tag=&all=1` every open task (`cv task list` with no window).
//!
//! Every POST takes `who` in the body (else `?who=`, else the server's `--who`).

use anyhow::{bail, Result};
use chrono::Utc;
use cv_core::api::Query;
use cv_core::task::{self, TaskEventKind, TaskStore};
use serde_json::{json, Value};

use super::task_ops::{self, DecisionSpec, EventFilter, Since};

/// The inbox page: one inline HTML file, no external JS/CSS, no build step.
pub(crate) const PAGE: &str = include_str!("serve_tasks_page.html");

/// A task-route answer: JSON, or the newline-delimited JSON `/api/events` streams.
pub(crate) enum Answer {
    Json(u16, Value),
    Ndjson(String),
}

/// The inbox routes, or `None` when the path is not one of them (the read API answers it).
pub(crate) fn route(post: bool, parts: &[&str], q: &Query, body: &Value, default_who: Option<&str>) -> Option<Answer> {
    let json = |(status, v): (u16, Value)| Some(Answer::Json(status, v));
    match (post, parts) {
        (false, ["api", "inbox"]) => json(api_inbox(default_who, q)),
        (false, ["api", "events"]) => Some(match api_events(q) {
            Ok(lines) => Answer::Ndjson(lines),
            Err(e) => Answer::Json(400, json!({"error": e.to_string()})),
        }),
        (false, ["api", "lanes"]) => json(api_lanes(q)),
        (false, ["api", "backlog"]) => json(api_backlog(q)),
        (true, ["api", "task", id, verb]) => {
            let who = body
                .get("who")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| q.get("who"))
                .or_else(|| default_who.map(str::to_string));
            json(match who {
                Some(who) => api_act(id, verb, &who, body),
                None => (400, json!({"error": "who? pass `who` in the body or start with --who"})),
            })
        }
        _ => None,
    }
}

fn with_replay(f: impl FnOnce(cv_core::task::ReplayOutcome) -> (u16, Value)) -> (u16, Value) {
    match task::replay() {
        Ok(o) => f(o),
        Err(e) => (500, json!({"error": e.to_string()})),
    }
}

fn api_inbox(default_who: Option<&str>, q: &Query) -> (u16, Value) {
    let Some(who) = q.get("who").or_else(|| default_who.map(str::to_string)) else {
        return (400, json!({"error": "who? pass ?who=<name> or start with --who"}));
    };
    let now = Utc::now();
    let window = if q.flag("all") {
        None
    } else {
        match q.get("since") {
            Some(s) => match task::parse_since(&s, now) {
                Ok(t) => Some(t),
                Err(e) => return (400, json!({"error": e})),
            },
            None => Some(now - chrono::Duration::days(14)),
        }
    };
    with_replay(|outcome| {
        let caller = task::default_endpoint();
        let page = task::inbox_page(&outcome, &who, caller.as_deref(), window, false, now);
        let mut v = serde_json::to_value(&page).expect("page serializes");
        if let Some(o) = v.as_object_mut() {
            o.insert("warnings".into(), json!(outcome.warnings));
        }
        (200, v)
    })
}

/// `cv lanes <session>` as JSON: every sub-agent, running first, then by start time (newest
/// first). The page polls this beside the inbox so the orchestrator's forest and the human's
/// decisions sit on one screen.
fn api_lanes(q: &Query) -> (u16, Value) {
    let Some(session) = q.get("session") else {
        return (400, json!({"error": "which session? pass ?session=<id prefix>"}));
    };
    let (r, _adapter) = match crate::util::resolve(&session, None) {
        Ok(x) => x,
        Err(e) => return (404, json!({"error": e.to_string()})),
    };
    let mut lanes = match cv_core::lanes::lanes_of(&r) {
        Ok(lanes) => lanes,
        Err(error) => return (500, json!({"error": format!("{error:#}")})),
    };
    // Task enrichment is optional: retain useful lane facts while exposing degraded replay.
    // An empty forest has no task join to perform and must not inspect the task log.
    let mut warnings = Vec::new();
    if !lanes.is_empty() {
        match task::replay() {
            Ok(outcome) => {
                cv_core::lanes::attach_tasks(&mut lanes, &outcome.model);
                warnings = outcome.warnings;
            }
            Err(error) => warnings.push(format!("task enrichment unavailable: {error:#}")),
        }
    }
    lanes.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then_with(|| b.started_at.cmp(&a.started_at))
    });
    let running = lanes.iter().filter(|l| l.is_running()).count();
    let stranded = lanes.iter().filter(|l| l.stranded).count();
    let done = lanes.iter().filter(|l| l.is_done()).count();
    (
        200,
        json!({
            "session": r.id,
            "counts": {"total": lanes.len(), "running": running, "done": done, "stranded": stranded},
            "lanes": lanes,
            "warnings": warnings,
        }),
    )
}

/// `cv task list` with no time window: the whole open backlog (or everything with `all=1`),
/// one compact row per task, newest activity first.
fn api_backlog(q: &Query) -> (u16, Value) {
    with_replay(|outcome| {
        let filter = task::TaskFilter {
            state: q.get("state"),
            assignee: q.get("assignee"),
            repo: q.get("repo").map(std::path::PathBuf::from),
            include_terminal: q.flag("all"),
            tag: q.get("tag"),
            touched_since: None,
            or_involving: None,
            decisions: None,
        };
        let mut tasks = match task::list(&outcome.model, &filter) {
            Ok(t) => t,
            Err(e) => return (400, json!({"error": e})),
        };
        tasks.sort_by_key(|t| std::cmp::Reverse(t.last_ts));
        let now = Utc::now();
        let rows: Vec<Value> = tasks
            .iter()
            .map(|t| {
                json!({
                    "id": t.task_id,
                    "short": &t.task_id[..t.task_id.len().min(13)],
                    "title": t.title,
                    "state": task::effective_display(t),
                    "assignee": t.assignee,
                    "opened_by": t.opened_by,
                    "repo": t.repo,
                    "issue": t.issue,
                    "tags": t.tags,
                    "blocked": task::is_blocked(&outcome.model, t),
                    "decision": t.decision.is_some(),
                    "notes": t.notes.len(),
                    "last_ts": t.last_ts,
                    "age": task::age_short(t.last_ts, now),
                    "body": t.body,
                })
            })
            .collect();
        let mut repos: Vec<String> = tasks
            .iter()
            .filter_map(|t| t.repo.as_ref().map(|p| p.display().to_string()))
            .collect();
        repos.sort();
        repos.dedup();
        (
            200,
            json!({"count": rows.len(), "repos": repos, "tasks": rows, "warnings": outcome.warnings}),
        )
    })
}

fn api_events(q: &Query) -> Result<String> {
    let outcome = task::replay()?;
    let now = Utc::now();
    let task_id = match q.get("task") {
        Some(p) => Some(
            task::resolve_id(&outcome.model, &p)
                .map_err(|e| anyhow::anyhow!(e))?
                .to_string(),
        ),
        None => None,
    };
    let f = EventFilter {
        since: Since::parse(q.get("since").as_deref(), now)?,
        kinds: q
            .get("kind")
            .map(|k| k.split(',').map(task_ops::kind_tag).collect())
            .unwrap_or_default(),
        by: q.get("by"),
        not_by: q.get("not_by"),
        assignee: q.get("assignee"),
        task: task_id,
    };
    let mut out = String::new();
    for ev in task_ops::select_events(&outcome, &f) {
        out.push_str(&serde_json::to_string(&task_ops::event_json(ev, &outcome.model))?);
        out.push('\n');
    }
    Ok(out)
}

/// One write, as `web:<who>`: the same `TaskEventKind`s the CLI verbs append.
fn api_act(id: &str, verb: &str, who: &str, body: &Value) -> (u16, Value) {
    match act(id, verb, who, body) {
        Ok(v) => (200, v),
        Err(e) => (400, json!({"error": e.to_string()})),
    }
}

fn act(id: &str, verb: &str, who: &str, body: &Value) -> Result<Value> {
    let outcome = task::replay()?;
    let id = task::resolve_id(&outcome.model, id)
        .map_err(|e| anyhow::anyhow!(e))?
        .to_string();
    let t = &outcome.model.tasks[&id];
    let by = format!("web:{}", who.trim());
    let store = TaskStore::default_store();
    let text = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let (appended, state, warnings) = match verb {
        "resolve" => {
            let accept = body.get("accept_default").and_then(Value::as_bool).unwrap_or(false);
            let confirm = body.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            let answer = task_ops::Answer::from_flags(text("choice"), accept, confirm)?;
            let out = task_ops::resolve(&store, &outcome.model, &by, &id, answer, text("note"), false)?;
            (vec![out.event], out.effective_state, out.replay_warnings)
        }
        "done" => {
            let out = task::append(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Done {
                    observed: text("observed"),
                    check: None,
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.replay_warnings)
        }
        "note" => {
            let Some(t) = text("text") else {
                bail!("a note needs text")
            };
            let out = task::append(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: t,
                    session_ref: None,
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.replay_warnings)
        }
        "discuss" => {
            // One unit: the note and the `discuss` tag land together.
            let detail = text("text").unwrap_or_else(|| "(no detail)".into());
            let out = task::append_events(
                &store,
                vec![
                    task::new_event(
                        Some(&id),
                        &by,
                        TaskEventKind::Noted {
                            text: format!("NEEDS DISCUSSION: {detail}"),
                            session_ref: None,
                        },
                    ),
                    task::new_event(
                        Some(&id),
                        &by,
                        TaskEventKind::Tagged {
                            tags: vec![task::DISCUSS_TAG.into()],
                        },
                    ),
                ],
                Vec::new(),
            )?;
            (out.events, out.effective_state, out.replay_warnings)
        }
        "claim" => {
            let out = task::append(
                &store,
                Some(&id),
                &by,
                TaskEventKind::Claimed {
                    assignee: who.trim().to_string(),
                },
                Vec::new(),
            )?;
            (vec![out.event], out.effective_state, out.replay_warnings)
        }
        "release" => {
            let out = task::append(&store, Some(&id), &by, TaskEventKind::Released {}, Vec::new())?;
            (vec![out.event], out.effective_state, out.replay_warnings)
        }
        "reopen" => {
            if !t.state.is_terminal() {
                bail!("task is still open");
            }
            // Terminal is terminal: the record of the close stays. The task comes back as a NEW
            // task carrying the same title/body/assignee (and, for a decision, the same options),
            // tagged `reopened`, and each side notes the other — all as ONE unit.
            let assignee = t.assignee.clone().unwrap_or_else(|| who.trim().to_string());
            let mut events = match &t.decision {
                Some(d) => task_ops::pose_events(
                    &by,
                    &DecisionSpec {
                        title: t.title.clone(),
                        body: t.body.clone(),
                        for_who: assignee,
                        default_choice: d.default_choice.clone(),
                        options: d.options.clone(),
                        deadline: None,
                        repo: t.repo.clone(),
                        issue: t.issue.clone(),
                        tags: vec!["reopened".into()],
                        source: d.source.clone(),
                        blocks: Vec::new(),
                        provisional: false,
                    },
                )?,
                None => {
                    let opened = task::new_event(
                        None,
                        &by,
                        TaskEventKind::Opened {
                            title: t.title.clone(),
                            body: t.body.clone(),
                            repo: t.repo.clone(),
                            issue: t.issue.clone(),
                            channel: None,
                            assignee: Some(assignee),
                        },
                    );
                    let new_id = opened.task_id.clone();
                    vec![
                        opened,
                        task::new_event(
                            Some(&new_id),
                            &by,
                            TaskEventKind::Tagged {
                                tags: vec!["reopened".into()],
                            },
                        ),
                    ]
                }
            };
            let new_id = events[0].task_id.clone();
            events.push(task::new_event(
                Some(&new_id),
                &by,
                TaskEventKind::Noted {
                    text: format!("reopened from {} ({})", &id[..13], task::effective_display(t)),
                    session_ref: None,
                },
            ));
            // The closed one stays closed: a note never changes state.
            events.push(task::new_event(
                Some(&id),
                &by,
                TaskEventKind::Noted {
                    text: format!("reopened as {}", &new_id[..13]),
                    session_ref: None,
                },
            ));
            let out = task::append_events(&store, events, Vec::new())?;
            (out.events, out.effective_state, out.replay_warnings)
        }
        other => bail!("unknown action {other:?}"),
    };
    Ok(json!({
        "ok": true,
        "by": by,
        "events": appended,
        "task_state": state,
        "warnings": warnings,
    }))
}
