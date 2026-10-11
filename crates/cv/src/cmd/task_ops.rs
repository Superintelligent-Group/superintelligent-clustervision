//! The composite task operations and read views the CLI verbs and `cv task serve` share, so the
//! page and the shell are one store: posing a decision (`opened` + `posed`, one unit), resolving
//! one, the event feed (`cv task events` ≡ `GET /api/events`), and the inbox page
//! (`cv task inbox` ≡ `GET /api/inbox`).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::truncate;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{
    self, InboxReason, ReplayOutcome, TaskEvent, TaskEventKind, TaskProjection, TaskReadModel, TaskStore,
};
use serde::Serialize;

use crate::util::fmt_local;

// ───────────────────────────── pose / resolve ─────────────────────────────

/// Everything a decision is opened with.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecisionSpec {
    pub title: String,
    pub body: String,
    pub for_who: String,
    pub default_choice: String,
    /// Alternatives; the default is prepended and duplicates dropped.
    pub options: Vec<String>,
    pub deadline: Option<DateTime<Utc>>,
    pub repo: Option<PathBuf>,
    pub issue: Option<String>,
    pub tags: Vec<String>,
    /// The note event this decision was posed from (an old log's `cv task split` wrote these; the
    /// page's `reopen` of such a decision carries it forward).
    pub source: Option<String>,
    /// Tasks (full ids) this decision blocks.
    pub blocks: Vec<String>,
    /// Resolve it on its default at once, by the poser, with a veto window: a `resolved`
    /// (`provisional: true`) lands in the same unit as the pose.
    pub provisional: bool,
}

/// The option list a spec poses: the default first, then the alternatives, deduplicated.
pub(crate) fn option_list(default_choice: &str, options: &[String]) -> Vec<String> {
    let mut out = vec![default_choice.trim().to_string()];
    for o in options {
        let o = o.trim();
        if !o.is_empty() && !out.iter().any(|have| have == o) {
            out.push(o.to_string());
        }
    }
    out
}

/// The events that pose a decision, in append order: `opened` (assigned to the decider) and
/// `posed` (the options, the default, the deadline) — one creating event per decision, plus a
/// `tagged` only for the caller's own tags, one `blocked_by` per task it blocks, and the
/// provisional `resolved` when `spec.provisional`. Built, not written: [`pose`] appends them as
/// ONE unit, and the page's `reopen` adds its notes to the same unit.
pub(crate) fn pose_events(from: &str, spec: &DecisionSpec) -> Result<Vec<TaskEvent>> {
    if spec.default_choice.trim().is_empty() {
        bail!("a decision needs a --default (the option that stands if nobody speaks)");
    }
    if spec.for_who.trim().is_empty() {
        bail!("a decision needs --for <who> (whose inbox it lands in)");
    }
    let options = option_list(&spec.default_choice, &spec.options);
    let default_choice = spec.default_choice.trim().to_string();
    let opened = task::new_event(
        None,
        from,
        TaskEventKind::Opened {
            title: spec.title.clone(),
            body: spec.body.clone(),
            repo: spec.repo.clone(),
            issue: spec.issue.clone(),
            channel: None,
            assignee: Some(spec.for_who.trim().to_string()),
        },
    );
    let id = opened.task_id.clone();
    let mut candidates = vec![
        opened,
        task::new_event(
            Some(&id),
            from,
            TaskEventKind::Posed {
                options,
                default_choice: default_choice.clone(),
                deadline: spec.deadline,
                source: spec.source.clone(),
            },
        ),
    ];
    let mut tags: Vec<String> = Vec::new();
    for t in &spec.tags {
        if !tags.iter().any(|have| have == t) {
            tags.push(t.clone());
        }
    }
    if !tags.is_empty() {
        candidates.push(task::new_event(Some(&id), from, TaskEventKind::Tagged { tags }));
    }
    for other in &spec.blocks {
        candidates.push(task::new_event(
            Some(other),
            from,
            TaskEventKind::BlockedBy { task: id.clone() },
        ));
    }
    if spec.provisional {
        candidates.push(task::new_event(
            Some(&id),
            from,
            TaskEventKind::Resolved {
                choice: default_choice,
                note: None,
                provisional: true,
            },
        ));
    }
    Ok(candidates)
}

/// Pose a decision: [`pose_events`] appended as ONE unit (`TaskStore::append_agent_events`), so
/// a decision is never left half-posed. Returns the events in append order and the advisory
/// warnings.
pub(crate) fn pose(store: &TaskStore, from: &str, spec: DecisionSpec) -> Result<(Vec<TaskEvent>, Vec<String>)> {
    let out = task::append_events(store, pose_events(from, &spec)?, Vec::new())?;
    let mut warnings = out.replay_warnings;
    warnings.extend(out.warnings);
    Ok((out.events, warnings))
}

/// What a resolution says: the literal choice, or the posed default.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Choice(String),
    AcceptDefault,
    /// Confirm a provisional resolution: the decider answers with the choice made for them.
    Confirm,
}

impl Answer {
    pub fn from_flags(choice: Option<String>, accept_default: bool, confirm: bool) -> Result<Answer> {
        match (choice, accept_default, confirm) {
            (Some(c), _, _) => Ok(Answer::Choice(c.trim().to_string())),
            (None, true, _) => Ok(Answer::AcceptDefault),
            (None, false, true) => Ok(Answer::Confirm),
            (None, false, false) => bail!("pass --choice \"<option>\", --accept-default, or --confirm"),
        }
    }
}

/// Resolve a decision (full task id) with `answer`. `from` is the resolver and is recorded.
/// `provisional`: resolve it on its default on the decider's behalf, with a veto window. The
/// rules — a posed decision, a known option, provisional only on the default — are the reducer's;
/// this only turns `--accept-default` / `--confirm` into the literal choice they stand for.
pub(crate) fn resolve(
    store: &TaskStore,
    model: &TaskReadModel,
    from: &str,
    id: &str,
    answer: Answer,
    note: Option<String>,
    provisional: bool,
) -> Result<task::AppendOutcome> {
    let t = model.tasks.get(id).with_context(|| format!("no task {id}"))?;
    let short = &id[..13.min(id.len())];
    let decision = || {
        t.decision.as_ref().with_context(|| {
            format!("task {short} is not a decision (nothing was posed) — finish it with `cv task done`")
        })
    };
    let choice = match answer {
        Answer::Choice(c) => c,
        Answer::AcceptDefault => decision()?.default_choice.clone(),
        Answer::Confirm => match decision()?.resolution.as_ref().filter(|r| r.provisional) {
            Some(r) => r.choice.clone(),
            None => bail!(
                "task {short} has no provisional resolution to confirm — answer it with --choice or --accept-default"
            ),
        },
    };
    let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    task::append(
        store,
        Some(id),
        from,
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        },
        Vec::new(),
    )
}

/// `note event id → decision task id` for every decision that names a note as its `source` (what
/// `cv task split` wrote until 0.15.0; `show` and the page still point such a note at its
/// decision).
pub(crate) fn decisions_from_notes(model: &TaskReadModel) -> HashMap<String, String> {
    model
        .tasks
        .values()
        .filter_map(|d| {
            d.decision
                .as_ref()
                .and_then(|dec| dec.source.clone())
                .map(|s| (s, d.task_id.clone()))
        })
        .collect()
}

// ───────────────────────────── the event feed ─────────────────────────────

/// A `--since` argument, resolved: an instant, or an event id (exclusive cursor).
#[derive(Clone, Debug, Default)]
pub(crate) struct Since {
    pub ts: Option<DateTime<Utc>>,
    pub id: Option<String>,
}

impl Since {
    pub fn parse(s: Option<&str>, now: DateTime<Utc>) -> Result<Since> {
        let Some(s) = s else {
            return Ok(Since::default());
        };
        let s = s.trim();
        // An event/task id (or a ≥13-hex prefix of one) is a cursor: everything after it.
        let hex_only: String = s.chars().filter(|c| *c != '-').collect();
        let looks_like_id = hex_only.len() >= 13 && hex_only.chars().all(|c| c.is_ascii_hexdigit());
        if looks_like_id && task::uuid_v7_timestamp(s).is_some() {
            return Ok(Since {
                ts: None,
                id: Some(s.to_string()),
            });
        }
        let ts = task::parse_since(s, now).map_err(|e| anyhow::anyhow!(e))?;
        Ok(Since { ts: Some(ts), id: None })
    }

    fn admits(&self, ev: &TaskEvent) -> bool {
        match (&self.id, self.ts) {
            (Some(id), _) => ev.id.as_str() > id.as_str(),
            (None, Some(ts)) => ev.ts >= ts,
            (None, None) => true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EventFilter {
    pub since: Since,
    /// Wire tags (already canonicalized by [`kind_tag`]).
    pub kinds: Vec<String>,
    pub by: Option<String>,
    pub not_by: Option<String>,
    pub assignee: Option<String>,
    /// Full task id.
    pub task: Option<String>,
}

/// The wire tag for a kind spelling a person would type: `note`→`noted`, `resolve`→`resolved`,
/// `open`→`opened`, `claim`→`claimed`, … Unknown spellings pass through (and match nothing,
/// loudly empty rather than wrong).
pub(crate) fn kind_tag(s: &str) -> String {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "note" | "notes" => "noted",
        "resolve" | "resolution" | "decided" | "decide" => "resolved",
        "open" => "opened",
        "claim" => "claimed",
        "release" => "released",
        "abandon" => "abandoned",
        "supersede" => "superseded",
        "tag" => "tagged",
        "block" | "blocked" => "blocked_by",
        "pose" | "decision" => "posed",
        "status" | "status-set" => "status_set",
        other => other,
    }
    .to_string()
}

/// The events matching `f`, log order.
pub(crate) fn select_events<'a>(outcome: &'a ReplayOutcome, f: &EventFilter) -> Vec<&'a TaskEvent> {
    outcome
        .events
        .iter()
        .filter(|ev| f.since.admits(ev))
        .filter(|ev| f.kinds.is_empty() || f.kinds.iter().any(|k| k == ev.kind.tag()))
        .filter(|ev| f.by.as_deref().is_none_or(|b| task::same_actor(&ev.by, b)))
        .filter(|ev| !f.not_by.as_deref().is_some_and(|b| task::same_actor(&ev.by, b)))
        .filter(|ev| f.task.as_deref().is_none_or(|t| ev.task_id == t))
        .filter(|ev| {
            f.assignee.as_deref().is_none_or(|a| {
                outcome
                    .model
                    .tasks
                    .get(&ev.task_id)
                    .is_some_and(|t| t.assignee.as_deref() == Some(a))
            })
        })
        .collect()
}

/// One event as a JSON line: the event's own wire fields, plus the task's `title` and current
/// effective `task_state` so a poller can act without a second lookup.
pub(crate) fn event_json(ev: &TaskEvent, model: &TaskReadModel) -> serde_json::Value {
    let mut v = serde_json::to_value(ev).expect("TaskEvent serializes");
    if let Some(obj) = v.as_object_mut() {
        if let Some(t) = model.tasks.get(&ev.task_id) {
            obj.insert("title".into(), serde_json::Value::String(t.title.clone()));
            obj.insert(
                "task_state".into(),
                serde_json::Value::String(task::effective_display(t)),
            );
            if let Some(a) = &t.assignee {
                obj.insert("assignee".into(), serde_json::Value::String(a.clone()));
            }
        }
        // The same telling detail the text feed prints, so a page need not know every kind.
        obj.insert("detail".into(), serde_json::Value::String(event_detail(ev)));
    }
    v
}

/// The telling detail of one event (what `event_text` prints after the kind), sanitized.
pub(crate) fn event_detail(ev: &TaskEvent) -> String {
    match &ev.kind {
        TaskEventKind::Noted { text, .. } => truncate(&sanitize_line(text), 160),
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        } => {
            let p = if *provisional { " (provisional — veto?)" } else { "" };
            match note {
                Some(n) => format!("→ {}{p} ({})", sanitize_line(choice), truncate(&sanitize_line(n), 80)),
                None => format!("→ {}{p}", sanitize_line(choice)),
            }
        }
        TaskEventKind::Posed { default_choice, .. } => format!("default: {}", sanitize_line(default_choice)),
        TaskEventKind::StatusSet { text } => format!("STATUS: {}", first_line(text, 150)),
        TaskEventKind::Opened { title, .. } => truncate(&sanitize_line(title), 120),
        TaskEventKind::Claimed { assignee } => format!("by {}", sanitize_line(assignee)),
        TaskEventKind::Done { observed, .. } => observed
            .as_deref()
            .map(|o| truncate(&sanitize_line(o), 120))
            .unwrap_or_default(),
        TaskEventKind::Tagged { tags } => tags
            .iter()
            .map(|t| format!("#{}", sanitize_line(t)))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// One event as a human line: time, kind, task, actor, and the telling detail.
pub(crate) fn event_text(ev: &TaskEvent, model: &TaskReadModel, plen: usize) -> String {
    let title = model
        .tasks
        .get(&ev.task_id)
        .map(|t| truncate(&sanitize_line(&t.title), 60))
        .unwrap_or_default();
    let detail = match &ev.kind {
        TaskEventKind::Noted { text, .. } => truncate(&sanitize_line(text), 100),
        TaskEventKind::Resolved {
            choice,
            note,
            provisional,
        } => {
            let p = if *provisional { " (provisional)" } else { "" };
            match note {
                Some(n) => format!("→ {}{p} ({})", sanitize_line(choice), truncate(&sanitize_line(n), 60)),
                None => format!("→ {}{p}", sanitize_line(choice)),
            }
        }
        TaskEventKind::Done { observed, .. } => observed
            .as_deref()
            .map(|o| sanitize_line(o).to_string())
            .unwrap_or_default(),
        TaskEventKind::Abandoned { reason } => sanitize_line(reason).to_string(),
        TaskEventKind::Claimed { assignee } => sanitize_line(assignee).to_string(),
        TaskEventKind::Tagged { tags } => format!("#{}", tags.join(" #")),
        TaskEventKind::Posed { default_choice, .. } => format!("default: {}", sanitize_line(default_choice)),
        TaskEventKind::StatusSet { text } => format!("STATUS: {}", first_line(text, 100)),
        _ => String::new(),
    };
    format!(
        "{}  {:<16} {}  {:<24} {}{}",
        fmt_local(ev.ts, "%m-%d %H:%M"),
        ev.kind.tag(),
        ev.task_id.get(..plen).unwrap_or(&ev.task_id),
        truncate(&sanitize_line(&ev.by), 24),
        title,
        if detail.is_empty() {
            String::new()
        } else {
            format!(" · {detail}")
        }
    )
}

// ───────────────────────────── the inbox page ─────────────────────────────

/// One inbox item with everything a page or a Markdown export needs — the full projection is
/// flattened into the fields a reader looks at, not embedded.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InboxItem {
    pub id: String,
    pub short: String,
    pub title: String,
    /// `decision` or `action`.
    pub kind: &'static str,
    /// `None` for a closed item (the page's Resolved filter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<InboxReason>,
    pub state: String,
    pub since: DateTime<Utc>,
    pub age: String,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub opened_by: String,
    pub opened_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    pub body: String,
    pub blocked: bool,
    pub last_by: String,
    pub last_ts: DateTime<Utc>,
    pub unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionView>,
    /// The pinned STATUS text, when one is pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub notes: Vec<NoteView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DecisionView {
    #[serde(rename = "default")]
    pub default_choice: String,
    pub options: Vec<String>,
    pub alternatives: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_phrase: Option<String>,
    pub posed_by: String,
    pub posed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionView>,
    /// The provisional resolution a decider confirmed or vetoed (kept, never rewritten).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_provisional: Option<ResolutionView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResolutionView {
    pub choice: String,
    pub by: String,
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub accepted_default: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
}

impl ResolutionView {
    fn of(r: &task::Resolution) -> ResolutionView {
        ResolutionView {
            choice: r.choice.clone(),
            by: r.by.clone(),
            ts: r.ts,
            note: r.note.clone(),
            accepted_default: r.accepted_default,
            provisional: r.provisional,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NoteView {
    pub by: String,
    pub ts: DateTime<Utc>,
    pub text: String,
    /// Appended after the task closed (the page renders "(after close)").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post_close: bool,
    /// The decision posed from this note, when one names it as its `source`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_into: Option<String>,
}

/// The whole page for one person.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct InboxPage {
    pub who: String,
    pub now: DateTime<Utc>,
    /// Open items, stalest first (decisions age from their pose).
    pub items: Vec<InboxItem>,
    /// Closed items assigned to `who` in the window, newest first (resolved / done / abandoned).
    pub closed: Vec<InboxItem>,
    /// Open items outside the window (not involving the caller).
    pub hidden: usize,
    pub counts: InboxCounts,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct InboxCounts {
    pub decisions: usize,
    /// Decisions parked for discussion (open, not owed).
    pub discussing: usize,
    /// Decisions made for `who` provisionally, awaiting a confirm or veto (resolved, not owed).
    pub provisional: usize,
    pub assigned: usize,
    pub claimed: usize,
    pub unread: usize,
    pub closed: usize,
}

/// `task_id → by of its last event`, from the replayed log.
fn last_authors(events: &[TaskEvent]) -> HashMap<&str, &str> {
    let mut m = HashMap::new();
    for ev in events {
        m.insert(ev.task_id.as_str(), ev.by.as_str());
    }
    m
}

/// Human deadline: `by 10-03 (in 2d)` / `by 10-03 (OVERDUE 1d)`.
pub(crate) fn deadline_phrase(deadline: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let day = fmt_local(deadline, "%m-%d");
    if deadline >= now {
        format!("by {day} (in {})", task::age_short(now, deadline))
    } else {
        format!("by {day} (OVERDUE {})", task::age_short(deadline, now))
    }
}

/// What every item on one page shares.
struct PageCtx<'a> {
    model: &'a TaskReadModel,
    who: &'a str,
    plen: usize,
    children: HashMap<String, String>,
    authors: HashMap<&'a str, &'a str>,
    now: DateTime<Utc>,
}

fn item_of(ctx: &PageCtx<'_>, t: &TaskProjection, reason: Option<InboxReason>, since: DateTime<Utc>) -> InboxItem {
    let (model, who, plen, children, now) = (ctx.model, ctx.who, ctx.plen, &ctx.children, ctx.now);
    let last_by = ctx.authors.get(t.task_id.as_str()).copied().unwrap_or("");
    InboxItem {
        id: t.task_id.clone(),
        short: t.task_id.get(..plen).unwrap_or(&t.task_id).to_string(),
        title: t.title.clone(),
        kind: t.kind(),
        reason,
        state: task::effective_display(t),
        since,
        age: task::age_short(since, now),
        stale: now.signed_duration_since(since) > chrono::Duration::hours(24),
        assignee: t.assignee.clone(),
        opened_by: t.opened_by.clone(),
        opened_at: t.opened_at,
        tags: t.tags.clone(),
        repo: t.repo.clone(),
        issue: t.issue.clone(),
        body: t.body.clone(),
        blocked: task::is_blocked(model, t),
        last_by: last_by.to_string(),
        last_ts: t.last_ts,
        unread: !task::same_actor(last_by, who),
        decision: t.decision.as_ref().map(|d| DecisionView {
            default_choice: d.default_choice.clone(),
            options: d.options.clone(),
            alternatives: d.alternatives().map(str::to_string).collect(),
            deadline: d.deadline,
            deadline_phrase: d.deadline.map(|dl| deadline_phrase(dl, now)),
            posed_by: d.posed_by.clone(),
            posed_at: d.posed_at,
            source: d.source.clone(),
            resolution: d.resolution.as_ref().map(ResolutionView::of),
            superseded_provisional: d.superseded_provisional.as_ref().map(ResolutionView::of),
        }),
        status: t.status.as_ref().map(|s| s.text.clone()),
        notes: t
            .notes
            .iter()
            .map(|n| NoteView {
                by: n.by.clone(),
                ts: n.ts,
                text: n.text.clone(),
                post_close: n.post_close,
                split_into: children.get(&n.event_id).cloned(),
            })
            .collect(),
    }
}

/// Build the page: the inbox projection for `who`, scoped to `since` (items involving `caller`
/// escape the window), `unread_only` keeping items whose last event is not `who`'s own.
pub(crate) fn inbox_page(
    outcome: &ReplayOutcome,
    who: &str,
    caller: Option<&str>,
    since: Option<DateTime<Utc>>,
    unread_only: bool,
    now: DateTime<Utc>,
) -> InboxPage {
    let model = &outcome.model;
    let ctx = PageCtx {
        model,
        who,
        plen: task::unique_prefix_len(model.tasks.keys().map(String::as_str), 8),
        children: decisions_from_notes(model),
        authors: last_authors(&outcome.events),
        now,
    };
    let mut items = Vec::new();
    let mut hidden = 0;
    for e in task::inbox(model, who) {
        if !task::in_scope(e.task, since, caller) {
            hidden += 1;
            continue;
        }
        let item = item_of(&ctx, e.task, Some(e.reason), e.since);
        if unread_only && !item.unread {
            continue;
        }
        items.push(item);
    }
    let mut closed: Vec<InboxItem> = model
        .tasks
        .values()
        .filter(|t| t.state.is_terminal() && t.assignee.as_deref() == Some(who))
        // A provisional resolution awaiting a veto is an open item, not a closed one.
        .filter(|t| !t.decision.as_ref().is_some_and(|d| d.awaiting_veto()))
        .filter(|t| task::in_scope(t, since, caller))
        .map(|t| item_of(&ctx, t, None, t.last_ts))
        .collect();
    closed.sort_by_key(|i| std::cmp::Reverse(i.last_ts));
    let count = |r: InboxReason| items.iter().filter(|i| i.reason == Some(r)).count();
    let counts = InboxCounts {
        decisions: count(InboxReason::DecisionOwed),
        discussing: count(InboxReason::Discussing),
        provisional: count(InboxReason::Provisional),
        assigned: count(InboxReason::AssignedOpen),
        claimed: count(InboxReason::ClaimedByYou),
        unread: items.iter().filter(|i| i.unread).count(),
        closed: closed.len(),
    };
    InboxPage {
        who: who.to_string(),
        now,
        items,
        closed,
        hidden,
        counts,
    }
}

/// The inbox's groups, in the order they are printed: a decision owed is the slowest blocker a
/// fleet has, so it comes first; then the actions handed to you; then the work you hold.
pub(crate) const INBOX_GROUPS: &[(InboxReason, &str)] = &[
    (InboxReason::DecisionOwed, "decisions owed"),
    (
        InboxReason::Discussing,
        "in discussion (parked, still yours to resolve)",
    ),
    (InboxReason::Provisional, "made for you (veto?)"),
    (InboxReason::AssignedOpen, "assigned actions"),
    (InboxReason::ClaimedByYou, "claimed work"),
];

/// The first non-empty line of a text, trimmed and capped.
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let l = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    truncate(&sanitize_line(l), max)
}

/// `cv task inbox <who>` as text.
pub(crate) fn render_inbox_text(page: &InboxPage) -> String {
    let mut out = String::new();
    if page.items.is_empty() {
        out.push_str(&format!("(inbox empty for {})\n", sanitize_line(&page.who)));
    }
    for (reason, label) in INBOX_GROUPS {
        let group: Vec<&InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("{label} ({}):\n", group.len()));
        for i in group {
            out.push_str(&format!(
                "{} {}  {:>4}  {:10} {}\n",
                if i.stale { "⏰" } else { "  " },
                i.short,
                i.age,
                i.state,
                sanitize_line(&i.title)
            ));
            if let Some(d) = &i.decision {
                if let Some(r) = d.resolution.as_ref().filter(|r| r.provisional) {
                    let mut line = format!("⇒ made: {} — by {}", sanitize_line(&r.choice), sanitize_line(&r.by));
                    if !d.alternatives.is_empty() {
                        line.push_str(&format!(
                            " · veto to: {}",
                            d.alternatives
                                .iter()
                                .map(|a| sanitize_line(a))
                                .collect::<Vec<_>>()
                                .join(" / ")
                        ));
                    }
                    out.push_str(&format!(
                        "{:width$}{}\n",
                        "",
                        truncate(&line, 200),
                        width = i.short.len() + 3
                    ));
                    continue;
                }
                let mut line = format!("⇒ default: {}", sanitize_line(&d.default_choice));
                if !d.alternatives.is_empty() {
                    line.push_str(&format!(
                        " · alt: {}",
                        d.alternatives
                            .iter()
                            .map(|a| sanitize_line(a))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ));
                }
                if let Some(p) = &d.deadline_phrase {
                    line.push_str(&format!(" · {p}"));
                }
                out.push_str(&format!(
                    "{:width$}{}\n",
                    "",
                    truncate(&line, 200),
                    width = i.short.len() + 3
                ));
            }
        }
    }
    if page.hidden > 0 {
        out.push_str(&format!(
            "({} older item(s) hidden — `--all`, or `--since 90d`)\n",
            page.hidden
        ));
    }
    if page.counts.decisions > 0 {
        out.push_str(&format!(
            "resolve: cv task resolve <id> --accept-default --from {who} · or --choice \"<option>\" · note: cv task note <id> -\n",
            who = sanitize_line(&page.who)
        ));
    }
    if page.counts.provisional > 0 {
        out.push_str(&format!(
            "made for you: cv task resolve <id> --confirm --from {who} · or veto with --choice \"<option>\"\n",
            who = sanitize_line(&page.who)
        ));
    }
    out
}

/// `cv task inbox <who> --md`: the whole inbox as one Markdown page — every item's body and the
/// first line of every note, ready to paste anywhere.
pub(crate) fn render_inbox_md(page: &InboxPage) -> String {
    let mut out = String::new();
    let who = sanitize_line(&page.who);
    out.push_str(&format!(
        "# Inbox for {who} — {}\n\n",
        fmt_local(page.now, "%Y-%m-%d %H:%M")
    ));
    let c = &page.counts;
    out.push_str(&format!(
        "{} decision(s) owed · {} assigned action(s) · {} claimed · {} unread",
        c.decisions, c.assigned, c.claimed, c.unread
    ));
    if c.provisional > 0 {
        out.push_str(&format!(" · {} made for you (veto?)", c.provisional));
    }
    if page.hidden > 0 {
        out.push_str(&format!(" · {} older hidden (`--all`)", page.hidden));
    }
    out.push_str("\n\n");
    if page.items.is_empty() {
        out.push_str("_Nothing waits on you._\n");
    }
    let mut n = 0;
    for (reason, label) in INBOX_GROUPS {
        let group: Vec<&InboxItem> = page.items.iter().filter(|i| i.reason == Some(*reason)).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("## {} ({})\n\n", capitalize(label), group.len()));
        for i in group {
            n += 1;
            out.push_str(&format!("### {n}. {}\n\n", md_inline(&i.title)));
            let mut facts = vec![
                format!("`{}`", i.short),
                i.state.clone(),
                format!(
                    "{} {} ago by {}",
                    if i.decision.is_some() { "asked" } else { "waiting" },
                    i.age,
                    md_inline(&i.opened_by)
                ),
            ];
            if i.unread {
                facts.push(format!("**unread** (last: {})", md_inline(&i.last_by)));
            }
            if i.blocked {
                facts.push("blocked".into());
            }
            if let Some(d) = &i.decision {
                if let Some(p) = &d.deadline_phrase {
                    facts.push(p.clone());
                }
            }
            out.push_str(&facts.join(" · "));
            out.push_str("\n\n");
            if let Some(d) = &i.decision {
                out.push_str(&format!("- **default:** {}\n", md_inline(&d.default_choice)));
                for a in &d.alternatives {
                    out.push_str(&format!("- alternative: {}\n", md_inline(a)));
                }
                match d.resolution.as_ref().filter(|r| r.provisional) {
                    Some(r) => out.push_str(&format!(
                        "- **made for you:** {} — by {} · confirm: `cv task resolve {} --confirm --from {who}` · or veto with `--choice`\n",
                        md_inline(&r.choice),
                        md_inline(&r.by),
                        i.short
                    )),
                    None => out.push_str(&format!(
                        "- resolve: `cv task resolve {} --accept-default --from {who}`\n",
                        i.short
                    )),
                }
                out.push('\n');
            }
            if !i.body.trim().is_empty() {
                for para in i.body.trim().split("\n\n") {
                    out.push_str(&md_block(para));
                    out.push_str("\n\n");
                }
            }
            if !i.notes.is_empty() {
                out.push_str(&format!("notes ({}):\n\n", i.notes.len()));
                for note in &i.notes {
                    let mut line = format!(
                        "- _{}, {}{}:_ {}",
                        md_inline(&note.by),
                        fmt_local(note.ts, "%m-%d %H:%M"),
                        if note.post_close { " (after close)" } else { "" },
                        md_inline(&first_line(&note.text, 200))
                    );
                    if let Some(child) = &note.split_into {
                        line.push_str(&format!(" → split into `{}`", child.get(..13).unwrap_or(child)));
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Inline Markdown-safe text: terminal-sanitized, pipes/newlines flattened; backticks kept (the
/// titles are full of code spans and a reader wants them rendered).
fn md_inline(s: &str) -> String {
    sanitize_line(s).replace('\n', " ").replace('|', "\\|")
}

/// A body paragraph: sanitized line by line, kept as written.
fn md_block(s: &str) -> String {
    s.lines()
        .map(|l| sanitize_line(l.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_spellings_map_to_wire_tags() {
        assert_eq!(kind_tag("note"), "noted");
        assert_eq!(kind_tag("Resolve"), "resolved");
        assert_eq!(kind_tag("done"), "done");
        assert_eq!(kind_tag("bogus"), "bogus");
    }

    #[test]
    fn since_reads_ids_as_exclusive_cursors_and_times_as_inclusive() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        let s = Since::parse(Some("01a0f54c-d1dd-72e2-8b19-e99f146df6ff"), now).unwrap();
        assert!(s.id.is_some() && s.ts.is_none());
        let s = Since::parse(Some("2h"), now).unwrap();
        assert_eq!(s.ts, Some(now - chrono::Duration::hours(2)));
        let s = Since::parse(Some("2026-10-01T10:00:00Z"), now).unwrap();
        assert_eq!(s.ts, Some("2026-10-01T10:00:00Z".parse().unwrap()));
        assert!(Since::parse(Some("yesterday-ish"), now).is_err());
    }

    #[test]
    fn option_lists_put_the_default_first_and_deduplicate() {
        assert_eq!(
            option_list(" keep ", &["delete".into(), "keep".into(), "".into(), "delete".into()]),
            vec!["keep", "delete"]
        );
    }

    #[test]
    fn deadline_phrases_say_in_or_overdue() {
        let now: DateTime<Utc> = "2026-10-01T12:00:00Z".parse().unwrap();
        assert!(deadline_phrase(now + chrono::Duration::days(2), now).contains("(in 2d)"));
        assert!(deadline_phrase(now - chrono::Duration::hours(5), now).contains("OVERDUE 5h"));
    }
}
