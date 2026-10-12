//! Grok CLI adapter — xAI "Grok Build" CLI, `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/`.
//!
//! Each session is a *directory*. The files we read:
//! - `summary.json` — metadata (`info{id,cwd}`, timestamps, model, git, `session_kind`,
//!   `chat_format_version`, `generated_title`/`session_summary`).
//! - `chat_history.jsonl` — the canonical transcript. Per line `type` ∈
//!   `system | user | assistant | tool_result | reasoning`:
//!     * `system`/`user`: `content` is a string (system) or `[{type:text,text}]` (user). User lines
//!       may carry `synthetic_reason` (e.g. `project_instructions`) for injected context.
//!     * `assistant`: `content` (string), optional `reasoning{text,encrypted,id}`, `model_id`,
//!       `model_fingerprint`, and **`tool_calls[]`** (`{id,name,arguments(JSON string)}`).
//!     * `tool_result`: `{tool_call_id, content}` — the output fed back to the model.
//!     * `reasoning` (1.0.5x): `{id, summary[{type:summary_text,text}], encrypted_content?}`, written
//!       before the assistant line it belongs to; it becomes that message's leading `Thinking` block.
//! - `updates.jsonl` — an ACP-style `session/update` event stream (plus `_x.ai/session/update`).
//!   We mine `tool_call`/`tool_call_update` entries (keyed by `toolCallId`) to *enrich* tool calls
//!   with a human `title`, a `kind`, file `locations`, and a `completed`/`failed` status that lets us
//!   set `ToolResult.is_error` correctly (chat_history alone doesn't mark errors). Its
//!   `hook_execution` updates are the session's hook runs (`hook`/`run` affordance rows).
//! - `events.jsonl` — timing / permission lifecycle (`tool_started`, `permission_*`,
//!   `tool_completed{outcome,duration_ms}`, `turn_*`). Not needed for content; we don't ingest it.
//!
//! Historical / variant handling (sniffed by content, never by a version field):
//! - Tool data is read from `chat_history.jsonl` (canonical, always present). `updates.jsonl` is
//!   strictly additive enrichment — if it's empty or absent we still get full tool fidelity.
//! - Sessions with no tool activity (`events.jsonl` empty, short `chat_history`) parse fine.
//! - `session_kind: "subagent"` (under `.grok/worktrees/.../subagent-<id>/`) vs primary sessions
//!   are both handled: the raw `session_kind`/`agent_name` ride in `extra["grok"]`, and a subagent's
//!   `agent_name` nickname also becomes [`Lineage::agent_path`]. 1.0.5x links them: a fork's
//!   `summary.json` has `parent_session_id` (→ `lineage.forked_from`), and a parent lists each child
//!   under `subagents/<child id>/meta.json` (→ the child's `lineage.parent`).

pub(crate) mod emit;
pub use emit::emit;

use super::wire::{openai, text_parts};
use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Grok {
    root: Option<PathBuf>,
}

impl Grok {
    pub fn detect() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".grok").join("sessions"));
        Grok {
            root: root.filter(|p| p.exists()),
        }
    }

    /// An adapter over an explicit Grok home (the dir holding `sessions/`, what `$GROK_HOME` names),
    /// instead of `~/.grok` — for the harness corpus and other tests, without touching the environment.
    pub fn with_home(grok_home: &Path) -> Self {
        let root = grok_home.join("sessions");
        Grok {
            root: root.exists().then_some(root),
        }
    }
}

impl Adapter for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.root.clone().into_iter().collect()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if entry.file_name() != "summary.json" {
                continue;
            }
            match scan(entry.path()) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", entry.path().display()),
            }
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Grok, sink);
        let sink: &mut dyn MessageSink = &mut spawns;
        // r.path is the session directory. `summary.json` (small) carries all session-level
        // metadata; the transcript is `chat_history.jsonl`, one record per line — streamed.
        let dir = &r.path;
        let summary: Value = fs::read_to_string(dir.join("summary.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Grok,
            cwd: summary
                .pointer("/info/cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| r.cwd.clone()),
            title: r.title.clone(),
            created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
            updated_at: summary
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(parse_ts)
                .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
            model: summary
                .get("current_model_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            git: grok_git(&summary),
            messages: Vec::new(),
            source_path: Some(dir.clone()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        };

        // Sidecar enrichment: tool_call / tool_call_update entries from the ACP update stream.
        let enrich = read_update_enrichment(dir);

        // Subagent / agent-name facts + lineage from summary.json (and a sub-agent's parent from the
        // parent's `subagents/<id>/meta.json`).
        grok_session_facts(&mut s, &summary);
        grok_lineage(&mut s, &summary, dir);

        // Grok persists the system prompt it sent as `system_prompt.txt` — byte-identical to the
        // leading `system` transcript record. It is the session-level fact, never a message.
        if let Ok(sp) = fs::read_to_string(dir.join("system_prompt.txt")) {
            if !sp.trim().is_empty() {
                s.system_prompt = Some(sp);
            }
        }

        // All session metadata comes from summary.json (already set above), so hand it to the sink
        // before the body — header-rendering consumers (cv show) get the title/model/cwd up front.
        sink.meta(&s);

        // IR diet: a message names its model only when it differs from the session default.
        let session_model = s.model.clone();
        let file = fs::File::open(dir.join("chat_history.jsonl"))
            .with_context(|| format!("reading chat_history in {}", dir.display()))?;
        // A 1.0.5x `reasoning` record joins the assistant turn after it, so one model turn stays one
        // message (and discover's user+assistant count still matches); reasoning with no assistant
        // turn after it (an interrupted turn) is flushed as a message of its own.
        let mut pending: Option<Message> = None;
        let mut stopped = false;
        let skipped = super::for_each_json_line(BufReader::new(file), |v| {
            let is_reasoning = v.get("type").and_then(Value::as_str) == Some("reasoning");
            let Some(mut m) = chat_message(&v, &enrich) else {
                return Flow::Continue;
            };
            if is_reasoning {
                match pending.as_mut() {
                    Some(p) => p.content.append(&mut m.content),
                    None => pending = Some(m),
                }
                return Flow::Continue;
            }
            if let Some(mut p) = pending.take() {
                if m.role == Role::Assistant {
                    p.content.append(&mut m.content);
                    m.content = p.content;
                } else if let Flow::Stop = sink.message(p) {
                    stopped = true;
                    return Flow::Stop;
                }
            }
            if m.model.is_some() && m.model == session_model {
                m.model = None;
            }
            let f = sink.message(m);
            stopped = matches!(f, Flow::Stop);
            f
        });
        if let (Some(p), false) = (pending, stopped) {
            sink.message(p);
        }
        super::note_skipped_lines(&mut s, skipped);
        Ok(s)
    }

    /// The todo list `todo_write` keeps in the session directory (`plan.json` `{todos: {id: …}}`),
    /// then the per-prompt restore points of `rewind_points.jsonl`, then every hook run
    /// `updates.jsonl` recorded ([`hook_runs`]).
    fn store_affordances(&self, r: &SessionRef) -> Vec<crate::affordance::Affordance> {
        let plan = fs::read_to_string(r.path.join("plan.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                let items = crate::affordance::items_of(&v);
                crate::affordance::plan_state(crate::affordance::Kind::Plan, "plan.json", items)
            });
        plan.into_iter()
            .chain(rewind_points(&r.path))
            .chain(hook_runs(&r.path))
            .collect()
    }
}

/// `rewind_points.jsonl`: one line per prompt, `{prompt_index, created_at, file_snapshots,
/// after_snapshots}` — the files as they were before (and after) that prompt, which `/rewind`
/// puts back. Each line is a checkpoint.
fn rewind_points(dir: &Path) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{Affordance, Kind};
    let Ok(text) = fs::read_to_string(dir.join("rewind_points.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| {
            let ts = v.get("created_at").and_then(Value::as_str).and_then(parse_ts);
            let files: Vec<&str> = v
                .get("file_snapshots")
                .and_then(Value::as_object)
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let mut summary = match v.get("prompt_index").and_then(Value::as_u64) {
                Some(i) => format!("prompt {i}"),
                None => "prompt".to_string(),
            };
            if !files.is_empty() {
                summary.push_str(&format!(" · {}", files.join(", ")));
            }
            Affordance::store_event(Kind::Checkpoint, "create", ts, "rewind_points.jsonl", &summary)
        })
        .collect()
}

/// The hook runs `updates.jsonl` records as `hook_execution` updates (`{event_name, runs: [{name,
/// status: {status, elapsed_ms}}]}`), one `hook`/`run` row each. Grok keeps no hook output, so
/// these say a hook ran and how it ended, not what it told the model. Only lines naming
/// `hook_execution` are parsed.
fn hook_runs(dir: &Path) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{Affordance, Kind};
    let Ok(text) = fs::read_to_string(dir.join("updates.jsonl")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines().filter(|l| l.contains("\"hook_execution\"")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(u) = v.pointer("/params/update") else {
            continue;
        };
        if u.get("sessionUpdate").and_then(Value::as_str) != Some("hook_execution") {
            continue;
        }
        let event = u.get("event_name").and_then(Value::as_str).unwrap_or("hook");
        let timestamp = v
            .pointer("/params/_meta/agentTimestampMs")
            .and_then(Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_millis)
            .or_else(|| {
                v.get("timestamp")
                    .and_then(Value::as_i64)
                    .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
            });
        for run in u.get("runs").and_then(Value::as_array).into_iter().flatten() {
            let name = run.get("name").and_then(Value::as_str).unwrap_or("");
            let status = run.pointer("/status/status").and_then(Value::as_str).unwrap_or("");
            let mut summary = format!("{event}: {name} · {status}");
            if let Some(ms) = run.pointer("/status/elapsed_ms").and_then(Value::as_u64) {
                summary.push_str(&format!(" · {ms} ms"));
            }
            out.push(Affordance::store_event(
                Kind::Hook,
                "run",
                timestamp,
                "updates.jsonl",
                &summary,
            ));
        }
    }
    out
}

/// Subagent nickname + Grok-specific session facts from `summary.json`: a subagent's `agent_name`
/// becomes [`Lineage::agent_path`]; the raw `session_kind` / `agent_name` ride in `extra["grok"]`.
fn grok_session_facts(s: &mut Session, summary: &Value) {
    let kind = summary.get("session_kind").and_then(Value::as_str);
    let agent = summary.get("agent_name").and_then(Value::as_str);
    if kind == Some("subagent") {
        if let Some(a) = agent {
            s.lineage.agent_path = Some(a.to_string());
        }
    }
    if kind.is_some() || agent.is_some() {
        let bag = s.harness_extra_mut(Harness::Grok);
        if let Some(k) = kind {
            bag.insert("session_kind".into(), Value::String(k.to_string()));
        }
        if let Some(a) = agent {
            bag.insert("agent_name".into(), Value::String(a.to_string()));
        }
    }
}

/// Who this session came from. A forked or restored session's `summary.json` names its source in
/// `parent_session_id` (1.0.5x, with `forked_at`). A sub-agent's own summary names no parent; the
/// parent records the child under `<parent>/subagents/<child id>/meta.json` (`parent_session_id`),
/// and the parent sits in the same cwd group, so one directory listing finds it.
fn grok_lineage(s: &mut Session, summary: &Value, dir: &Path) {
    let subagent = summary.get("session_kind").and_then(Value::as_str) == Some("subagent");
    let named = summary
        .get("parent_session_id")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    if let (Some(p), false) = (named, subagent) {
        s.lineage.forked_from = Some(p.to_string());
    }
    if !subagent {
        return;
    }
    let Some(group) = dir.parent() else { return };
    let Ok(rd) = fs::read_dir(group) else { return };
    for e in rd.flatten() {
        let meta = e.path().join("subagents").join(&s.id).join("meta.json");
        let Some(v) = fs::read_to_string(&meta)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        s.lineage.parent = v
            .get("parent_session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| e.file_name().to_str().map(str::to_string));
        s.lineage.spawned_by_tool_use = v.get("tool_call_id").and_then(Value::as_str).map(str::to_string);
        return;
    }
}

fn grok_git(summary: &Value) -> Option<GitInfo> {
    let branch = summary.get("head_branch").and_then(Value::as_str);
    let commit = summary.get("head_commit").and_then(Value::as_str);
    let remote = summary
        .get("git_remotes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    if branch.is_none() && commit.is_none() && remote.is_none() {
        return None;
    }
    Some(GitInfo {
        branch: branch.map(str::to_string),
        commit: commit.map(str::to_string),
        remote: remote.map(str::to_string),
    })
}

/// Enrichment for one tool call, mined from `updates.jsonl`.
#[derive(Default, Clone)]
struct ToolEnrich {
    /// Human-friendly title, e.g. `List \`.\`` (last `tool_call_update.title` wins).
    title: Option<String>,
    /// ACP `kind`, e.g. `read`/`edit`/`execute`/`search`/`other`.
    kind: Option<String>,
    /// File paths the call touched.
    locations: Vec<String>,
    /// Terminal status: `completed` / `failed` (drives `ToolResult.is_error`).
    status: Option<String>,
}

impl ToolEnrich {
    fn is_error(&self) -> bool {
        matches!(self.status.as_deref(), Some("failed") | Some("error"))
    }
}

/// Parse `updates.jsonl` into per-`toolCallId` enrichment. Tolerant: missing/empty/malformed file
/// yields an empty map and zero behavior change.
fn read_update_enrichment(dir: &Path) -> HashMap<String, ToolEnrich> {
    let mut map: HashMap<String, ToolEnrich> = HashMap::new();
    let Ok(text) = fs::read_to_string(dir.join("updates.jsonl")) else {
        return map;
    };
    super::for_each_json_line_str(&text, |v| {
        let Some(u) = v.pointer("/params/update") else {
            return Flow::Continue;
        };
        let su = u.get("sessionUpdate").and_then(Value::as_str);
        if !matches!(su, Some("tool_call") | Some("tool_call_update")) {
            return Flow::Continue;
        }
        let Some(id) = u.get("toolCallId").and_then(Value::as_str) else {
            return Flow::Continue;
        };
        let e = map.entry(id.to_string()).or_default();
        // Prefer the descriptive title from tool_call_update over the bare tool name in tool_call.
        if let Some(t) = u.get("title").and_then(Value::as_str) {
            if su == Some("tool_call_update") || e.title.is_none() {
                e.title = Some(t.to_string());
            }
        }
        if let Some(k) = u.get("kind").and_then(Value::as_str) {
            e.kind = Some(k.to_string());
        }
        if let Some(locs) = u.get("locations").and_then(Value::as_array) {
            for l in locs {
                if let Some(p) = l.get("path").and_then(Value::as_str) {
                    if !e.locations.iter().any(|x| x == p) {
                        e.locations.push(p.to_string());
                    }
                }
            }
        }
        if let Some(st) = u.get("status").and_then(Value::as_str) {
            e.status = Some(st.to_string());
        }
        Flow::Continue
    });
    map
}

fn chat_message(v: &Value, enrich: &HashMap<String, ToolEnrich>) -> Option<Message> {
    let ty = v.get("type").and_then(Value::as_str)?;

    // tool_result lines become a distinct Tool-role message so conversions re-encode them correctly.
    if ty == "tool_result" {
        let tool_use_id = v
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let content = text_parts(v.get("content"));
        let e = enrich.get(&tool_use_id);
        let is_error = e.map(|e| e.is_error()).unwrap_or(false);
        let status = e.and_then(|e| e.status.clone());
        let tool_name = e.and_then(|e| e.title.clone());
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id,
            content: content.into(),
            is_error,
            tool_name,
            status,
            details: None,
        });
        return Some(m);
    }

    // 1.0.5x writes reasoning as its own record before the assistant turn it belongs to:
    // `{type: reasoning, id, summary: [{type: summary_text, text}], encrypted_content?}`. Older
    // versions nest it in the assistant line (`reasoning{text,encrypted}`, below).
    if ty == "reasoning" {
        let text = v
            .get("summary")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let encrypted = v.get("encrypted_content").and_then(Value::as_str).map(str::to_string);
        if text.is_empty() && encrypted.is_none() {
            return None;
        }
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Thinking {
            text: text.into(),
            signature: None,
            encrypted,
            redacted: false,
        });
        return Some(m);
    }

    let role = match ty {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let mut m = Message::new(role);
    // kind/origin from Grok's own turn semantics. `assistant` keeps the role-implied Reply/Model.
    match ty {
        // The system prompt (also `system_prompt.txt`, mirrored into `Session::system_prompt`).
        "system" => m.kind = MessageKind::SystemPrompt,
        "user" => {
            let (kind, origin) = user_kind_origin(v);
            m.kind = kind;
            m.origin = origin;
        }
        _ => {}
    }

    // assistant lines may carry reasoning (summarized text + opaque encrypted blob).
    if let Some(reasoning) = v.get("reasoning") {
        let text = reasoning.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let encrypted = reasoning.get("encrypted").and_then(Value::as_str).map(str::to_string);
        if !text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: text.into(),
                signature: None,
                encrypted,
                redacted: false,
            });
        }
    }
    if let Some(model) = v.get("model_id").and_then(Value::as_str) {
        m.model = Some(model.to_string());
    }

    let text = text_parts(v.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }

    // assistant tool_calls → ToolUse blocks (arguments is a JSON *string*).
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let id = c.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = c.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let input = openai::parse_arguments(c.get("arguments"));
            // Stash sidecar enrichment so nothing is lost (ir.rs has no field for it yet).
            if let Some(e) = enrich.get(&id) {
                let mut info = serde_json::Map::new();
                if let Some(t) = &e.title {
                    info.insert("title".into(), Value::String(t.clone()));
                }
                if let Some(k) = &e.kind {
                    info.insert("kind".into(), Value::String(k.clone()));
                }
                if !e.locations.is_empty() {
                    info.insert(
                        "locations".into(),
                        Value::Array(e.locations.iter().cloned().map(Value::String).collect()),
                    );
                }
                if !info.is_empty() {
                    let bag = m.harness_extra_mut(Harness::Grok);
                    if let Value::Object(obj) = bag
                        .entry("tool_calls")
                        .or_insert_with(|| Value::Object(serde_json::Map::new()))
                    {
                        obj.insert(id.clone(), Value::Object(info));
                    }
                }
            }
            m.content.push(Block::ToolUse {
                id,
                name,
                input,
                namespace: None,
            });
        }
    }

    // Preserve a couple of high-signal scalars for round-tripping (the injected-context flavor —
    // `system_reminder` / `project_instructions` / `task_completed` — is already on `kind`/`origin`,
    // but the exact `synthetic_reason` is kept so the distinction survives).
    let fingerprint = v.get("model_fingerprint").and_then(Value::as_str).map(str::to_string);
    let synthetic = v.get("synthetic_reason").and_then(Value::as_str).map(str::to_string);
    if fingerprint.is_some() || synthetic.is_some() {
        let bag = m.harness_extra_mut(Harness::Grok);
        if let Some(fp) = fingerprint {
            bag.insert("model_fingerprint".into(), Value::String(fp));
        }
        if let Some(sr) = synthetic {
            bag.insert("synthetic_reason".into(), Value::String(sr));
        }
    }

    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// kind/origin for a Grok `user` record from its own semantics. A real typed prompt is
/// Prompt/Human; the harness injects several kinds of context as their own `user` turns —
/// `<system-reminder>` skill/rule blocks and background-task notices (flagged by
/// `synthetic_reason`) and the unflagged `<user_info>` environment block — all InjectedContext /
/// Harness. A `compaction_meta` turn is the summary that seeds the post-compaction window.
fn user_kind_origin(v: &Value) -> (MessageKind, Origin) {
    match v.get("synthetic_reason").and_then(Value::as_str) {
        // After a compaction 1.0.5x re-tags the leading `<user_info>` block `compaction_meta` too;
        // only the summary itself is the compaction summary.
        Some("compaction_meta") if !text_parts(v.get("content")).trim_start().starts_with("<user_info>") => {
            (MessageKind::CompactionSummary, Origin::Harness)
        }
        Some(_) => (MessageKind::InjectedContext, Origin::Harness),
        None => {
            if text_parts(v.get("content")).trim_start().starts_with("<user_info>") {
                (MessageKind::InjectedContext, Origin::Harness)
            } else {
                (MessageKind::Prompt, Origin::Human)
            }
        }
    }
}

/// `summary_path` is `.../<sid>/summary.json`; the session dir is its parent.
fn scan(summary_path: &Path) -> Result<Option<SessionRef>> {
    let dir = summary_path.parent().map(Path::to_path_buf);
    let Some(dir) = dir else { return Ok(None) };
    let text = fs::read_to_string(summary_path)?;
    let summary: Value = serde_json::from_str(&text)?;

    let id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_default();

    let cwd = summary.pointer("/info/cwd").and_then(Value::as_str).map(PathBuf::from);

    // title: explicit summary/generated title, else first user message from chat_history.
    let mut title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            summary
                .get("generated_title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(|s| crate::ir::truncate(s, 80));
    // `message_count` is the contract's user + assistant turns (`SessionRef::message_count`), so it
    // is counted from the transcript: summary.json's `num_chat_messages` counts every line —
    // `system` and `tool_result` included — and `num_messages` is the user-turn count. The same
    // pass supplies the title fallback (first non-empty user text).
    let mut message_count = 0usize;
    if let Ok(chat) = fs::read_to_string(dir.join("chat_history.jsonl")) {
        super::for_each_json_line_str(&chat, |v| {
            let ty = v.get("type").and_then(Value::as_str);
            if matches!(ty, Some("user") | Some("assistant")) {
                message_count += 1;
            }
            if title.is_none() && ty == Some("user") {
                let t = text_parts(v.get("content"));
                if !t.trim().is_empty() {
                    title = Some(crate::ir::truncate(&t, 80));
                }
            }
            Flow::Continue
        });
    }

    Ok(Some(SessionRef {
        id,
        harness: Harness::Grok,
        path: dir,
        cwd,
        title,
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .or_else(|| summary.get("last_active_at").and_then(Value::as_str).and_then(parse_ts)),
        message_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;

    fn enrich_from(s: &str) -> HashMap<String, ToolEnrich> {
        let dir = tempdir_with("updates.jsonl", s);
        let e = read_update_enrichment(&dir);
        let _ = fs::remove_dir_all(&dir);
        e
    }

    /// Write `name`->`contents` into a fresh temp dir, return the dir.
    fn tempdir_with(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-grok-test-{}-{}",
            std::process::id(),
            // cheap unique-ish suffix
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }

    #[test]
    fn assistant_tool_calls_become_tooluse_blocks() {
        let line = r#"{"type":"assistant","content":"","model_id":"grok-build","model_fingerprint":"fp_x","reasoning":{"text":"thinking","encrypted":"BLOB","id":"r1"},"tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"a.rs\",\"limit\":10}"}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("assistant message");
        assert_eq!(m.role, Role::Assistant);
        // thinking + tool_use (content empty so no text block)
        assert!(matches!(m.content[0], Block::Thinking { .. }));
        match &m.content[1] {
            Block::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "read_file");
                assert_eq!(input["target_file"], "a.rs");
                assert_eq!(input["limit"], 10);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(m.kind, MessageKind::Reply);
        assert_eq!(m.origin, Origin::Model);
        let bag = m.harness_extra(Harness::Grok).expect("grok bag");
        assert_eq!(bag["model_fingerprint"], "fp_x");
        assert!(!m.extra.contains_key("model_fingerprint"), "no flat keys");
        // encrypted reasoning preserved
        match &m.content[0] {
            Block::Thinking { encrypted, text, .. } => {
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(text, "thinking");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let line = r#"{"type":"tool_result","tool_call_id":"call-1","content":"file contents"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).expect("tool message");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.kind, MessageKind::ToolResult);
        assert_eq!(m.origin, Origin::Harness);
        match &m.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn enrichment_marks_failed_results_as_error_and_adds_metadata() {
        let updates = concat!(
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-9","title":"grep","rawInput":{}}}}"#,
            "\n",
            r#"{"timestamp":2,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","kind":"search","title":"Search foo","locations":[{"path":"src/lib.rs"}]}}}"#,
            "\n",
            r#"{"timestamp":3,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-9","status":"failed"}}}"#,
        );
        let enrich = enrich_from(updates);
        let e = enrich.get("call-9").expect("enrichment present");
        assert_eq!(e.title.as_deref(), Some("Search foo"));
        assert_eq!(e.kind.as_deref(), Some("search"));
        assert_eq!(e.locations, vec!["src/lib.rs".to_string()]);
        assert!(e.is_error());

        // tool_result with this id should be flagged as error and carry metadata via the call.
        let res: Value =
            serde_json::from_str(r#"{"type":"tool_result","tool_call_id":"call-9","content":"boom"}"#).unwrap();
        let m = chat_message(&res, &enrich).unwrap();
        match &m.content[0] {
            Block::ToolResult { is_error, .. } => assert!(*is_error),
            _ => unreachable!(),
        }

        // and the ToolUse extra picks up title/kind/locations.
        let call: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-9","name":"grep","arguments":"{}"}]}"#,
        )
        .unwrap();
        let m = chat_message(&call, &enrich).unwrap();
        let tc = &m.harness_extra(Harness::Grok).expect("grok bag")["tool_calls"]["call-9"];
        assert_eq!(tc["title"], "Search foo");
        assert_eq!(tc["kind"], "search");
    }

    #[test]
    fn user_synthetic_reason_preserved() {
        let line =
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"synthetic_reason":"project_instructions"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = chat_message(&v, &HashMap::new()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hi"));
        // A `synthetic_reason` user turn is harness-injected context, not a prompt.
        assert_eq!(m.kind, MessageKind::InjectedContext);
        assert_eq!(m.origin, Origin::Harness);
        assert_eq!(
            m.harness_extra(Harness::Grok).unwrap()["synthetic_reason"],
            "project_instructions"
        );
        assert!(!m.extra.contains_key("synthetic_reason"), "no flat keys");
    }

    /// The kind/origin mapping across every Grok record shape, plus the `<user_info>` env block.
    #[test]
    fn kind_and_origin_by_record() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new());

        // A real typed prompt.
        let p = msg(r#"{"type":"user","content":[{"type":"text","text":"do the thing"}],"synthetic_reason":null}"#)
            .unwrap();
        assert_eq!((p.kind, p.origin), (MessageKind::Prompt, Origin::Human));

        // The `<user_info>` environment block is harness-injected context, though unflagged.
        // (The newlines inside the block are JSON `\n` escapes: a literal newline inside a JSON
        // string is a control character and would not parse.)
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":null}"#,
        )
        .unwrap();
        assert!(ui.text().is_some_and(|t| t.contains("\nOS: mac\n")));
        assert_eq!((ui.kind, ui.origin), (MessageKind::InjectedContext, Origin::Harness));

        // A compaction summary seeds the next window.
        let cm = msg(
            r#"{"type":"user","content":[{"type":"text","text":"summary…"}],"synthetic_reason":"compaction_meta"}"#,
        )
        .unwrap();
        assert_eq!((cm.kind, cm.origin), (MessageKind::CompactionSummary, Origin::Harness));

        // The system record is the system prompt.
        let sys = msg(r#"{"type":"system","content":"You are Grok."}"#).unwrap();
        assert_eq!(
            (sys.role, sys.kind, sys.origin),
            (Role::System, MessageKind::SystemPrompt, Origin::Harness)
        );

        // A model reply.
        let a = msg(r#"{"type":"assistant","content":"hi"}"#).unwrap();
        assert_eq!((a.kind, a.origin), (MessageKind::Reply, Origin::Model));
    }

    #[test]
    fn reasoning_records_join_the_next_turn_and_lineage_comes_from_summary_and_meta() {
        let group = tempdir_with("x", "");
        let parent = group.join("p1");
        let child = group.join("c1");
        let fork = group.join("f1");
        for d in [&parent, &child, &fork] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(parent.join("subagents/c1")).unwrap();
        fs::write(
            parent.join("subagents/c1/meta.json"),
            r#"{"parent_session_id":"p1","child_session_id":"c1","tool_call_id":"call_7"}"#,
        )
        .unwrap();
        fs::write(
            child.join("summary.json"),
            r#"{"info":{"id":"c1"},"session_kind":"subagent","agent_name":"general-purpose"}"#,
        )
        .unwrap();
        fs::write(
            fork.join("summary.json"),
            r#"{"info":{"id":"f1"},"parent_session_id":"p1","forked_at":"2026-10-11T21:04:37Z"}"#,
        )
        .unwrap();
        // Shapes from the 1.0.50 corpus: reasoning, then the reply; then trailing reasoning alone.
        let chat = [
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}],"prompt_index":0}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"think"}]}"#,
            r#"{"type":"assistant","content":"hello","model_id":"m"}"#,
            r#"{"type":"reasoning","id":"","summary":[{"type":"summary_text","text":"cut off"}]}"#,
        ]
        .join("\n");
        for d in [&child, &fork] {
            fs::write(d.join("chat_history.jsonl"), &chat).unwrap();
        }
        let grok = Grok {
            root: Some(group.clone()),
        };
        let c = grok
            .parse(&scan(&child.join("summary.json")).unwrap().unwrap())
            .unwrap();
        assert_eq!(c.lineage.parent.as_deref(), Some("p1"));
        assert_eq!(c.lineage.spawned_by_tool_use.as_deref(), Some("call_7"));
        assert_eq!(c.lineage.forked_from, None);
        let f = grok.parse(&scan(&fork.join("summary.json")).unwrap().unwrap()).unwrap();
        assert_eq!(f.lineage.forked_from.as_deref(), Some("p1"));
        assert_eq!(f.lineage.parent, None);
        let kinds: Vec<Vec<&str>> = f
            .messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        Block::Thinking { .. } => "thinking",
                        Block::Text { .. } => "text",
                        _ => "other",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(kinds, vec![vec!["text"], vec!["thinking", "text"], vec!["thinking"]]);
        let _ = fs::remove_dir_all(&group);
    }

    #[test]
    fn compaction_meta_on_the_user_info_block_is_context_not_the_summary() {
        let msg = |line: &str| chat_message(&serde_json::from_str::<Value>(line).unwrap(), &HashMap::new()).unwrap();
        let ui = msg(
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS: mac\n</user_info>"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(ui.kind, MessageKind::InjectedContext);
        let sum = msg(
            r#"{"type":"user","content":[{"type":"text","text":"This session is being continued"}],"synthetic_reason":"compaction_meta"}"#,
        );
        assert_eq!(sum.kind, MessageKind::CompactionSummary);
    }

    #[test]
    fn empty_updates_is_harmless() {
        assert!(read_update_enrichment(Path::new("/nonexistent/dir/xyz")).is_empty());
    }

    #[test]
    fn parses_fixture_session_v1() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/v1_tools");
        if !dir.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&dir.join("summary.json")).unwrap().unwrap();
        let grok = Grok { root: None };
        let s = grok.parse(&r).unwrap();
        // The contract count: user + assistant turns (the fixture's `system` line and two
        // `tool_result` lines are not turns; summary.json's `num_chat_messages` says 5).
        assert_eq!(r.message_count, 4);
        assert_eq!(
            r.message_count,
            s.messages
                .iter()
                .filter(|m| matches!(m.role, Role::User | Role::Assistant))
                .count(),
            "discover's count is the parse's user+assistant count"
        );
        // Every Grok fact is nested under `extra["grok"]` (`docs/INTERFACE-V2.md` §4).
        crate::harness::assert_no_flat_keys(&s);
        // roles present: system, user, assistant(with tool_use), tool(result)
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        // call-2 (write_file) is marked `failed` in updates.jsonl -> is_error must be true;
        // call-1 (read_file) is `completed` -> false.
        let mut saw_call2 = false;
        for m in s.messages.iter().filter(|m| m.role == Role::Tool) {
            if let Block::ToolResult {
                tool_use_id, is_error, ..
            } = &m.content[0]
            {
                match tool_use_id.as_str() {
                    "call-1" => assert!(!*is_error),
                    "call-2" => {
                        assert!(*is_error, "fixture marks call-2 failed");
                        saw_call2 = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_call2, "expected call-2 tool result");
        assert_eq!(s.title.as_deref(), Some("Add a greeting function"));
        // The system record → SystemPrompt kind; system_prompt.txt (if present) seeds the session.
        assert!(s
            .messages
            .iter()
            .any(|m| m.kind == MessageKind::SystemPrompt && m.role == Role::System));
        // summary.json marks this a subagent session.
        assert_eq!(
            s.harness_extra(Harness::Grok)
                .and_then(|b| b.get("session_kind"))
                .and_then(Value::as_str),
            Some("subagent")
        );
    }
}
