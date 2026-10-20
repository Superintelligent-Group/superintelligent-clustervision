//! Claude Code's hook payload (checked against the 2.1.296 hooks reference): `session_id` is the
//! top-level session even inside a sub-agent, whose `agent_id` names it; an `Edit`/`Write`/
//! `NotebookEdit` call writes its `file_path` or `notebook_path`.

use crate::hooks::{Agent, Event, Payload};
use crate::ir::{Harness, SessionRef};
use anyhow::Result;
use serde_json::Value;

pub(super) fn hook_event(v: &Value) -> Result<Option<Event>> {
    let p = Payload::decode(v)?;
    let transcript = p.transcript();
    let agent = p.agent_id.as_deref().map(|id| {
        let id = id.strip_prefix("agent-").unwrap_or(id).to_string();
        Agent {
            transcript: p.agent_transcript().or_else(|| {
                transcript
                    .as_deref()
                    .and_then(super::sidecar::subagents_dir)
                    .map(|d| d.join(format!("agent-{id}.jsonl")))
            }),
            id,
            kind: p.agent_type.clone(),
            last_message: p.last_assistant_message.clone(),
        }
    });
    let edits = p
        .tool_input
        .as_ref()
        .and_then(|tool| tool.get("file_path").or_else(|| tool.get("notebook_path")))
        .and_then(Value::as_str)
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    Ok(Some(Event {
        harness: Harness::Claude,
        session: SessionRef::new(p.session_id, Harness::Claude, transcript.unwrap_or_default()),
        event: p.hook_event_name,
        cwd: p.cwd,
        agent,
        edits,
    }))
}
