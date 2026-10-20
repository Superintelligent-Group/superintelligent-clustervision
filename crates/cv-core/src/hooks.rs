//! A harness's hook call as one shape. Each adapter decodes its own stdin payload
//! ([`Adapter::hook_event`](crate::harness::Adapter::hook_event)) into an [`Event`], so the
//! hook bodies (`cv hook <name>`) read sessions, agents and edits without naming a harness.

use crate::ir::{Harness, SessionRef};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One hook call, as the harness's adapter decoded it.
#[derive(Debug, Clone)]
pub struct Event {
    pub harness: Harness,
    /// The session the hook speaks for: the payload's own, or for a sub-agent event the session
    /// that owns the agent. `path` is its transcript, empty when the payload names none
    /// ([`Event::transcript`]).
    pub session: SessionRef,
    /// The harness's event name (`SessionStart`, `PreToolUse`, `SubagentStop`, …).
    pub event: Option<String>,
    pub cwd: Option<PathBuf>,
    /// The sub-agent the event is about, or the one it fired inside.
    pub agent: Option<Agent>,
    /// The files the tool call would write, as the harness's tool input names them (relative
    /// paths are the harness's, against `cwd`). Empty for anything but a file-writing tool.
    pub edits: Vec<String>,
}

/// A sub-agent named by a hook payload.
#[derive(Debug, Clone)]
pub struct Agent {
    /// The id the harness addresses it by (Claude's without its `agent-` file prefix).
    pub id: String,
    pub kind: Option<String>,
    pub transcript: Option<PathBuf>,
    /// The agent's final answer (on its stop event).
    pub last_message: Option<String>,
}

impl Event {
    /// The session's transcript, when the payload named one.
    pub fn transcript(&self) -> Option<&Path> {
        (!self.session.path.as_os_str().is_empty()).then_some(self.session.path.as_path())
    }
}

/// The Claude Code hook payload's fields cv reads; Codex sends the same names. Every other
/// field is ignored.
#[derive(Deserialize, Default)]
pub(crate) struct Payload {
    #[serde(default)]
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub hook_event_name: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub agent_transcript_path: Option<PathBuf>,
    pub last_assistant_message: Option<String>,
    pub tool_input: Option<Value>,
}

impl Payload {
    pub(crate) fn decode(v: &Value) -> Result<Payload> {
        Payload::deserialize(v).context("stdin is not the hook JSON")
    }

    /// `transcript_path` with a leading `~` expanded (the docs' payloads spell it so).
    pub(crate) fn transcript(&self) -> Option<PathBuf> {
        self.transcript_path.as_deref().map(expand)
    }

    pub(crate) fn agent_transcript(&self) -> Option<PathBuf> {
        self.agent_transcript_path.as_deref().map(expand)
    }

    pub(crate) fn is_subagent_event(&self) -> bool {
        matches!(self.hook_event_name.as_deref(), Some("SubagentStart" | "SubagentStop"))
    }
}

fn expand(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), crate::env::var_os("HOME")) {
        (Ok(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => p.to_path_buf(),
    }
}
