//! 📋 The task substrate — durable, replayable dispatch objects for agent fleets.
//!
//! Tasks let a fleet *commit to work* and let a human see what has been said about it. A task
//! is a durable object with a replay-derived lifecycle; a decision is a task with a posed
//! question. The log is the fleet's one record: follow it with `cv task events --since <id>`.
//!
//! # The three laws
//!
//! 1. **cv never claims to have observed what it has not.** A completion is the agent's own
//!    word: `Done { observed }` is recorded and labeled self-reported, and nothing in cv turns it
//!    into a verified fact. What git can say about a task is a `sweep` candidate, never a close.
//! 2. **No authority machinery.** No seats, grants, tokens or debts. Identity (`CV_ENDPOINT`,
//!    `--from`) is recorded, never authenticated.
//! 3. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
//!    design-notes graveyard, next to the 200k-line system this replaced.
//!
//! Until 0.15.0 a **land facet** (reviewed code revisions, a git verifier, TOFU tokens,
//! reviewer receipts, completion checks) lived here too. Its nine event kinds are
//! [`model::RETIRED_TAGS`]: a log that holds them still replays, as state-neutral events.
//!
//! Storage is a single append-only event log (`$CLUSTERVISION_HOME/tasks/events.jsonl`) under a
//! crash-safe flock.

pub mod decide;
pub mod model;
pub mod project;
pub mod reduce;
pub mod store;
pub mod views;

pub use decide::{options_of, parse_deadline, parse_decide_note, DecideNote, AS_PROPOSED};
pub use model::{RetiredEvent, TaskEvent, TaskEventKind, TaskState, RETIRED_TAGS};
pub use project::{
    age_short, blocks, effective_display, in_scope, inbox, involves, is_blocked, list, parse_duration, parse_since,
    resolve_id, same_actor, unique_prefix_len, uuid_v7_timestamp, InboxEntry, InboxReason, TaskFilter, DECISION_TAG,
    DISCUSS_TAG, STATE_VOCABULARY,
};
pub use reduce::{Decision, Note, PinnedStatus, ReduceError, Resolution, TaskProjection, TaskReadModel, TaskReducer};
pub use store::{new_event, replay, ReplayOutcome, TaskStore};
pub use views::{InboxDecision, InboxRow, TaskRow};

/// The fleet identity convention (audit G4): the spawner sets `CV_ENDPOINT=agent:<pane>` in each
/// agent's environment, and cv front-ends derive their default `--from` here — the *bare*
/// command records the right actor. This is environment, not assertion: nothing verifies the
/// string (law 2, no authority machinery); it only stops a forgotten flag from silently
/// detaching an agent from its own inbox and misattributing the durable record forever.
pub fn default_endpoint() -> Option<String> {
    let v = std::env::var("CV_ENDPOINT").ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Identity resolution (G4) for verbs where the actor is bookkeeping, not semantics
/// (open/note/done/abandon/supersede): explicit actor wins, else the spawner-set
/// `CV_ENDPOINT`, else the surface's deliberate default sink — `"cv"` for CLI chat-grade verbs
/// (a human at a shell owes no ceremony), `"agent"` for MCP (the legacy anonymous sink).
pub fn actor(explicit: Option<String>, default_sink: &str) -> String {
    explicit
        .or_else(default_endpoint)
        .unwrap_or_else(|| default_sink.to_string())
}

/// Identity resolution for identity-BEARING verbs (claim/release/status/resolve), whose endpoint
/// string keys inbox and decision semantics: no resolvable identity is an error naming the cure,
/// never a silent fall-through to a shared sink. `flag` is the surface's spelling of the explicit
/// parameter (`--from` for the CLI, `` `from` `` for MCP), so the error names the exact knob the
/// caller has.
pub fn require_actor(explicit: Option<String>, flag: &str) -> anyhow::Result<String> {
    explicit
        .or_else(default_endpoint)
        .ok_or_else(|| anyhow::anyhow!("set CV_ENDPOINT or pass {flag}; identity-bearing events must record who acted"))
}

/// What one agent-facing append produced: the durable event, the task's new state, and the
/// advisory warnings to surface.
#[derive(Debug)]
pub struct AppendOutcome {
    pub event: TaskEvent,
    /// `None` only if the appended event's task vanished from the replay (should not happen —
    /// the append CAS validated against it).
    pub effective_state: Option<String>,
    /// The caller's carried-in advisory warnings — the set MCP ships in its JSON reply.
    pub warnings: Vec<String>,
    /// Warnings from the post-append replay (log corruption etc.) — kept separate so surfaces
    /// that already showed the pre-append replay's warnings don't double-report.
    pub replay_warnings: Vec<String>,
}

/// The one agent-facing append path shared by the CLI and MCP: append the event through the
/// store's CAS, then replay for the task's new state.
pub fn append(
    store: &TaskStore,
    task_id: Option<&str>,
    from: &str,
    kind: TaskEventKind,
    warnings: Vec<String>,
) -> anyhow::Result<AppendOutcome> {
    let event = store.append_agent_event(new_event(task_id, from, kind))?;
    let outcome = store.replay()?;
    let proj = outcome.model.tasks.get(&event.task_id);
    Ok(AppendOutcome {
        effective_state: proj.map(effective_display),
        event,
        warnings,
        replay_warnings: outcome.warnings,
    })
}

/// [`append`] for several events the caller built with [`new_event`], appended as ONE unit
/// ([`TaskStore::append_agent_events`]): all land or none does. Later events may name the task
/// the FIRST one opens (`cv task decide`: the `opened` id must be known before the
/// `tagged`/`posed`/`resolved` that follow it are built). The outcome's `effective_state` is the
/// LAST event's task after the batch; `events` holds all of them in append order.
pub fn append_events(
    store: &TaskStore,
    candidates: Vec<TaskEvent>,
    warnings: Vec<String>,
) -> anyhow::Result<BatchOutcome> {
    let events = store.append_agent_events(candidates)?;
    let outcome = store.replay()?;
    let effective_state = events
        .last()
        .and_then(|e| outcome.model.tasks.get(&e.task_id))
        .map(effective_display);
    Ok(BatchOutcome {
        events,
        effective_state,
        warnings,
        replay_warnings: outcome.warnings,
    })
}

/// What one batch append produced (see [`append_events`]).
#[derive(Debug)]
pub struct BatchOutcome {
    pub events: Vec<TaskEvent>,
    /// The last event's task, after the batch.
    pub effective_state: Option<String>,
    pub warnings: Vec<String>,
    pub replay_warnings: Vec<String>,
}

use std::path::PathBuf;

/// Root dir for the task log: `$CLUSTERVISION_HOME/tasks` (or `~/.clustervision/tasks`).
pub fn tasks_dir() -> PathBuf {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clustervision")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tasks")
}

#[cfg(test)]
mod tests {
    use super::default_endpoint;

    /// All env cases live in ONE test: `set_var` is process-global and lib tests run in
    /// parallel threads, so sequencing inside a single test is the serialization.
    #[test]
    fn default_endpoint_reads_cv_endpoint_trimmed_nonempty() {
        std::env::remove_var("CV_ENDPOINT");
        assert_eq!(default_endpoint(), None, "unset → no identity");

        std::env::set_var("CV_ENDPOINT", "agent:demo");
        assert_eq!(default_endpoint().as_deref(), Some("agent:demo"));

        std::env::set_var("CV_ENDPOINT", "  agent:padded \n");
        assert_eq!(default_endpoint().as_deref(), Some("agent:padded"), "trimmed");

        std::env::set_var("CV_ENDPOINT", "   ");
        assert_eq!(default_endpoint(), None, "whitespace-only → no identity");

        std::env::set_var("CV_ENDPOINT", "");
        assert_eq!(default_endpoint(), None, "empty → no identity");

        std::env::remove_var("CV_ENDPOINT");
    }
}
