//! One shape, one identity: the shared serializable row shapes every task read surface (CLI,
//! MCP tools, cvd HTTP routes) renders from. Field names and declaration order here ARE the wire
//! contract — the workspace builds serde_json with `preserve_order`, so struct field order is the
//! emitted key order, and all three surfaces must stay byte-identical to what they shipped.
//!
//! One surface asymmetry that predates this module is carried as optional fields rather than
//! re-unified (that would break existing consumers): the HTTP task row carries timestamps the MCP
//! row never did ([`TaskRow::full`] vs [`TaskRow::brief`]).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::project::{self, InboxEntry, InboxReason};
use super::reduce::{TaskProjection, TaskReadModel};

/// One task-list row (`cv task list`, MCP `task_list`, `GET /api/tasks`).
#[derive(Clone, Debug, Serialize)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub effective_state: String,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    pub channel: String,
    /// Timestamps ride the HTTP row only ([`TaskRow::full`]); the MCP row shipped without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<DateTime<Utc>>,
    /// Tags and blockers ride every surface, omitted when empty — a task that has neither
    /// serializes byte-for-byte as it did before they existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

impl TaskRow {
    /// The lean row (MCP `task_list`): no timestamps on the wire.
    pub fn brief(t: &TaskProjection) -> TaskRow {
        TaskRow {
            id: t.task_id.clone(),
            title: t.title.clone(),
            effective_state: project::effective_display(t),
            assignee: t.assignee.clone(),
            repo: t.repo.clone(),
            channel: t.channel.clone(),
            opened_at: None,
            last_ts: None,
            tags: t.tags.clone(),
            blocked_by: t.blocked_by.clone(),
        }
    }

    /// The timestamped row (`GET /api/tasks`, and the CLI's text rendering ages off `last_ts`).
    pub fn full(t: &TaskProjection) -> TaskRow {
        TaskRow {
            opened_at: Some(t.opened_at),
            last_ts: Some(t.last_ts),
            ..TaskRow::brief(t)
        }
    }
}

/// One inbox row (`cv task inbox`, MCP `task_inbox`, `GET /api/tasks/inbox/{who}`).
#[derive(Clone, Debug, Serialize)]
pub struct InboxRow {
    pub id: String,
    pub title: String,
    pub reason: InboxReason,
    pub effective_state: String,
    /// The aging anchor the CLI renders (`⏰`, age column); never on the JSON wire — neither MCP
    /// nor HTTP ever carried it.
    #[serde(skip)]
    pub since: DateTime<Utc>,
    /// The decision facet when the row is a posed decision: what the default is, every option,
    /// and the deadline. Absent (and off the wire) for actions and for tag-only decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<InboxDecision>,
}

/// The one-screen summary of a posed decision an inbox row carries.
#[derive(Clone, Debug, Serialize)]
pub struct InboxDecision {
    #[serde(rename = "default")]
    pub default_choice: String,
    pub options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
}

impl InboxRow {
    /// The whole inbox view as rows, stalest first (the projection's order).
    pub fn compute(model: &TaskReadModel, who: &str) -> Vec<InboxRow> {
        project::inbox(model, who).iter().map(InboxRow::from_entry).collect()
    }

    pub fn from_entry(e: &InboxEntry<'_>) -> InboxRow {
        InboxRow {
            id: e.task.task_id.clone(),
            title: e.task.title.clone(),
            reason: e.reason,
            effective_state: project::effective_display(e.task),
            since: e.since,
            decision: e.task.decision.as_ref().map(|d| InboxDecision {
                default_choice: d.default_choice.clone(),
                options: d.options.clone(),
                deadline: d.deadline,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{TaskEvent, TaskEventKind};
    use crate::task::reduce::TaskReducer;

    fn model_with_claimed_task() -> (TaskReadModel, String) {
        let ts: DateTime<Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
        let open = TaskEvent {
            id: "00000000-0000-7000-8000-000000000001".into(),
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            ts,
            by: "h".into(),
            kind: TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: Some(PathBuf::from("/tmp/r")),
                issue: None,
                channel: "tasks".into(),
                assignee: None,
            },
        };
        let id = open.task_id.clone();
        let claim = TaskEvent {
            id: "00000000-0000-7000-8000-000000000002".into(),
            task_id: id.clone(),
            ts,
            by: "agent:a".into(),
            kind: TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        };
        (TaskReducer::reduce(&[open, claim]).unwrap(), id)
    }

    /// The wire contract: key names AND order are frozen per surface (preserve_order means
    /// declaration order is emitted order).
    #[test]
    fn row_serialization_is_the_shipped_wire_shape() {
        let (model, id) = model_with_claimed_task();
        let t = &model.tasks[&id];

        let brief = serde_json::to_value(TaskRow::brief(t)).unwrap();
        assert_eq!(
            serde_json::to_string(&brief).unwrap(),
            format!(
                r#"{{"id":"{id}","title":"t","effective_state":"claimed","assignee":"agent:a","repo":"/tmp/r","channel":"tasks"}}"#
            ),
        );
        let full = serde_json::to_value(TaskRow::full(t)).unwrap();
        let keys: Vec<&str> = full.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "title",
                "effective_state",
                "assignee",
                "repo",
                "channel",
                "opened_at",
                "last_ts"
            ],
        );
    }

    #[test]
    fn inbox_rows_carry_reason_and_hidden_since() {
        let (model, id) = model_with_claimed_task();
        let rows = InboxRow::compute(&model, "agent:a");
        assert_eq!(rows.len(), 1, "the claimant holds the task");
        assert_eq!(rows[0].id, id);
        let v = serde_json::to_value(&rows[0]).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "title", "reason", "effective_state"],
            "since never on the wire"
        );
        assert_eq!(v["reason"], "claimed_by_you");
        assert_eq!(v["effective_state"], "claimed");
    }
}
