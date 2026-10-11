//! The Cline SDK session store: `~/.cline/data/sessions/`, written by the Cline CLI 3.x (npm
//! `cline`) and by the 4.x VS Code extension, which are both built on `@cline/core`. The 3.x
//! extension's per-task `tasks/<id>/api_conversation_history.json` layout is the older format,
//! read by [`super::cline`]; this module is the newer one, and [`super::cline::Cline`] reads both.
//!
//! # Layout (verified against cline 3.0.70, `tests/corpus/cline/3.0.70`)
//!
//! ```text
//! <data>/sessions/<sid>/                 <sid> = "<epoch ms>_<5 chars>", e.g. 1791750113981_lflrg
//!   <sid>.json                            the session row: {version, session_id, source, pid,
//!                                         started_at, ended_at, exit_code, status, interactive,
//!                                         provider, model, cwd, workspace_root, team_name,
//!                                         enable_tools, enable_spawn, enable_teams, prompt,
//!                                         metadata: {title, usage, aggregateUsage, totalCost, ...},
//!                                         messages_path}
//!   <sid>.messages.json                   {version, updated_at, agent: "lead", sessionId, origin,
//!                                         messages: [...], system_prompt}
//!   agent_<uuid>.messages.json            a spawn_agent child: agent "subagent", taskType
//!                                         "subagent_task", origin {mode: "subagent",
//!                                         parentThreadId: <sid>, subagent: "agent_<uuid>"}
//!   <teammate>__<id>.messages.json        a team teammate: agent "teammate", taskType "team",
//!                                         origin {mode: "team", parentThreadId, subagent: <name>}
//! <data>/db/sessions.db                   the same session rows (plus subagent rows, schedules,
//!                                         schedule_executions, subagent_spawn_queue); not needed
//!                                         to read the transcripts, so not read
//! <data>/db/teams.db                      team state (events, tasks, runs, outcomes); not read
//! ```
//!
//! Only the child files carry no `<id>.json`; their session id here is `<sid>/<file stem>` (for
//! example `1791750138208_50wuo/agent_6522b1bb-…`), parented to `<sid>`, with the file's own
//! `sessionId` (the SDK's `<sid>__agent_<uuid>` / `<sid>__teamtask__<name>__<id>`) kept in
//! `extra["cline"]["sdk_session_id"]`.
//!
//! # `messages[]`
//!
//! `{id, role: "user"|"assistant", content: [block] | string, ts: epoch ms}` plus, on assistant
//! turns, `modelInfo {id, provider}` and `metrics {inputTokens, outputTokens, cacheReadTokens,
//! cacheWriteTokens}`. Blocks are Anthropic-shaped (`text`, `thinking`, `tool_use`, `tool_result`,
//! `image`), except that a `tool_result` names its tool (`name`) and its `content` may be an array
//! of plain result objects (`read_files`: `{query, result, success}`), kept as JSON text. A user
//! turn holding only tool results is a [`Role::Tool`] turn. An assistant turn with
//! `metadata {displayOnly: true, displayRole: "error"}` is a run error shown to the user, never
//! sent to the model ([`MessageKind::Error`]).

use super::wire::{anthropic, coerce_text, text_parts};
use super::{parse_ts, ts_from_epoch};
use crate::ir::*;
use crate::stream::{Flow, MessageSink};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The SDK `sessions/` dirs under `home`, honouring the SDK's own overrides (`CLINE_SESSION_DATA_DIR`,
/// `CLINE_DATA_DIR`, `CLINE_DIR`) only when `env` is set. Only existing dirs are returned.
pub fn session_roots(home: &Path, env: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if env {
        if let Some(d) = std::env::var_os("CLINE_SESSION_DATA_DIR") {
            out.push(PathBuf::from(d));
        }
        if let Some(d) = std::env::var_os("CLINE_DATA_DIR") {
            out.push(PathBuf::from(d).join("sessions"));
        }
        if let Some(d) = std::env::var_os("CLINE_DIR") {
            out.push(PathBuf::from(d).join("data").join("sessions"));
        }
    }
    out.push(home.join(".cline").join("data").join("sessions"));
    let mut seen = Vec::new();
    out.retain(|p| {
        p.is_dir() && !seen.contains(p) && {
            seen.push(p.clone());
            true
        }
    });
    out
}

/// True when `path` is an SDK messages file (`*.messages.json`), as opposed to a legacy task dir.
pub fn is_sdk_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".messages.json"))
}

/// One `SessionRef` per messages file under each root: the lead session first, then its children.
pub fn discover(roots: &[PathBuf]) -> Vec<SessionRef> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Some(sid) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = files
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| is_sdk_path(p))
                .collect();
            files.sort();
            let lead = dir.join(format!("{sid}.messages.json"));
            if let Some(r) = scan(&lead, &sid) {
                out.push(r);
            }
            for f in files.iter().filter(|f| **f != lead) {
                if let Some(r) = scan(f, &sid) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// A child file's session id: `<sid>/<stem>`; the lead's is `<sid>`.
fn ref_id(path: &Path, sid: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".messages.json")?;
    Some(if stem == sid {
        sid.to_string()
    } else {
        format!("{sid}/{stem}")
    })
}

/// The `<sid>.json` session row next to a messages file, if any.
fn manifest(dir: &Path, sid: &str) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(dir.join(format!("{sid}.json"))).ok()?).ok()
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path).ok()?.modified().ok().map(DateTime::<Utc>::from)
}

fn scan(path: &Path, sid: &str) -> Option<SessionRef> {
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let msgs = doc.get("messages")?.as_array()?;
    let id = ref_id(path, sid)?;
    let lead = id == sid;
    let row = if lead { manifest(path.parent()?, sid) } else { None };
    let mut r = SessionRef::new(&id, Harness::Cline, path.to_path_buf());
    r.message_count = msgs.len();
    r.cwd = row
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    r.title = row
        .as_ref()
        .and_then(|m| m.pointer("/metadata/title"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| first_user_text(msgs))
        .map(|t| truncate(&t, 80));
    r.created_at = row
        .as_ref()
        .and_then(|m| m.get("started_at"))
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .or_else(|| {
            msgs.first()
                .and_then(|m| m.get("ts"))
                .and_then(Value::as_i64)
                .and_then(ts_from_epoch)
        });
    r.updated_at = doc
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .or_else(|| mtime(path))
        .or(r.created_at);
    Some(r)
}

fn first_user_text(msgs: &[Value]) -> Option<String> {
    msgs.iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| text_parts(m.get("content")))
        .filter(|t| !t.trim().is_empty())
}

/// Read one messages file into `sink`, returning the session with empty `messages`.
pub fn stream(path: &Path, id: &str, sink: &mut dyn MessageSink) -> Result<Session> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut s = Session::new(id.to_string(), Harness::Cline);
    s.source_path = Some(path.to_path_buf());
    let sid = id.split('/').next().unwrap_or(id);
    let lead = sid == id;
    let row = path.parent().and_then(|d| manifest(d, sid));
    let origin = doc.get("origin");
    {
        let bag = s.harness_extra_mut(Harness::Cline);
        bag.insert("store".into(), json!("sdk"));
        for (k, key) in [
            ("agent", "agent"),
            ("task_type", "taskType"),
            ("sdk_session_id", "sessionId"),
        ] {
            if let Some(v) = doc.get(key) {
                bag.insert(k.into(), v.clone());
            }
        }
        if let Some(o) = origin {
            bag.insert("origin".into(), o.clone());
        }
        if let Some(row) = row.as_ref().filter(|_| lead) {
            for k in [
                "source",
                "status",
                "exit_code",
                "provider",
                "team_name",
                "workspace_root",
                "interactive",
            ] {
                if let Some(v) = row.get(k).filter(|v| !v.is_null()) {
                    bag.insert(k.into(), v.clone());
                }
            }
        }
    }
    if !lead {
        s.lineage.parent = origin
            .and_then(|o| o.get("parentThreadId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(sid.to_string()));
        s.lineage.agent_path = origin
            .and_then(|o| o.get("subagent"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let row = row.filter(|_| lead);
    s.cwd = row
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    s.model = row
        .as_ref()
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string);
    s.title = row
        .as_ref()
        .and_then(|m| m.pointer("/metadata/title"))
        .and_then(Value::as_str)
        .map(str::to_string);
    s.created_at = row
        .as_ref()
        .and_then(|m| m.get("started_at"))
        .and_then(Value::as_str)
        .and_then(parse_ts);
    s.updated_at = doc.get("updated_at").and_then(Value::as_str).and_then(parse_ts);
    s.system_prompt = doc.get("system_prompt").and_then(Value::as_str).map(str::to_string);
    sink.meta(&s);

    let empty = Vec::new();
    let msgs = doc.get("messages").and_then(Value::as_array).unwrap_or(&empty);
    let mut prompted = false;
    for v in msgs {
        let Some(m) = message(v, lead, &mut prompted) else {
            continue;
        };
        if s.created_at.is_none() {
            s.created_at = m.timestamp;
        }
        if s.model.is_none() {
            s.model.clone_from(&m.model);
        }
        if s.title.is_none() && m.kind == MessageKind::Prompt {
            s.title = m.text().map(|t| truncate(&t, 80));
        }
        if sink.message(m) == Flow::Stop {
            break;
        }
    }
    if s.updated_at.is_none() {
        s.updated_at = mtime(path).or(s.created_at);
    }
    Ok(s)
}

fn result_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::Array(items)) if items.iter().all(|i| i.get("type").is_some()) => coerce_text(c),
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn message(v: &Value, lead: bool, prompted: &mut bool) -> Option<Message> {
    let role = v.get("role").and_then(Value::as_str)?;
    let mut blocks = Vec::new();
    match v.get("content") {
        Some(Value::String(s)) => blocks.push(Block::Text { text: s.clone().into() }),
        Some(Value::Array(items)) => {
            for it in items {
                if it.get("type").and_then(Value::as_str) == Some("tool_result") {
                    blocks.push(Block::ToolResult {
                        tool_use_id: it.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string(),
                        content: result_text(it.get("content")).into(),
                        is_error: it.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        tool_name: it.get("name").and_then(Value::as_str).map(str::to_string),
                        status: None,
                        details: None,
                    });
                } else if let Some(b) = anthropic::block_from(it, &anthropic::ANTHROPIC) {
                    blocks.push(b);
                }
            }
        }
        _ => {}
    }
    let meta = v.get("metadata");
    let display_error = meta.and_then(|m| m.get("displayOnly")).and_then(Value::as_bool) == Some(true)
        && meta.and_then(|m| m.get("displayRole")).and_then(Value::as_str) == Some("error");
    let mut m = match role {
        "assistant" if display_error => Message::of_kind(Role::Assistant, MessageKind::Error, Origin::Harness),
        "assistant" => Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model),
        "user" if !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })) => {
            Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness)
        }
        "user" => {
            // A child's first prompt is the task its lead handed it.
            let origin = if !lead && !*prompted {
                Origin::Subagent
            } else {
                Origin::Human
            };
            *prompted = true;
            Message::of_kind(Role::User, MessageKind::Prompt, origin)
        }
        "system" => Message::of_kind(Role::System, MessageKind::SystemPrompt, Origin::Harness),
        _ => return None,
    };
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.timestamp = v.get("ts").and_then(Value::as_i64).and_then(ts_from_epoch);
    m.model = v.pointer("/modelInfo/id").and_then(Value::as_str).map(str::to_string);
    if let Some(x) = v.get("metrics").filter(|x| x.is_object()) {
        let n = |k: &str| x.get(k).and_then(Value::as_u64);
        m.usage = Some(Usage {
            input_tokens: n("inputTokens"),
            output_tokens: n("outputTokens"),
            cache_read_tokens: n("cacheReadTokens"),
            cache_creation_tokens: n("cacheWriteTokens"),
            cost_usd: x.get("cost").and_then(Value::as_f64),
            ..Usage::default()
        });
    }
    if let Some(p) = v.pointer("/modelInfo/provider") {
        m.harness_extra_mut(Harness::Cline).insert("provider".into(), p.clone());
    }
    m.content = blocks;
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectSink;

    fn write(dir: &Path, name: &str, v: Value) {
        fs::write(dir.join(name), serde_json::to_vec(&v).unwrap()).unwrap();
    }

    fn read(r: &SessionRef) -> Session {
        let mut sink = CollectSink::default();
        let mut s = stream(&r.path, &r.id, &mut sink).unwrap();
        s.messages = sink.messages;
        s
    }

    #[test]
    fn lead_and_child_sessions_with_tool_results_and_a_display_error() {
        let root = std::env::temp_dir().join(format!("cv-cline-sdk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("100_abcde");
        fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "100_abcde.json",
            json!({"session_id": "100_abcde", "cwd": "/p", "model": "m1",
            "started_at": "2026-10-11T00:00:00Z", "status": "completed", "metadata": {"title": "t"}}),
        );
        write(
            &dir,
            "100_abcde.messages.json",
            json!({"version": 1, "agent": "lead", "sessionId": "100_abcde",
            "system_prompt": "sys", "messages": [
            {"id": "u1", "role": "user", "content": [{"type": "text", "text": "read it"}], "ts": 1791748105679_i64},
            {"id": "a1", "role": "assistant", "content": [{"type": "thinking", "thinking": "hm"},
                {"type": "tool_use", "id": "t1", "name": "read_files", "input": {"files": ["a"]}}],
             "modelInfo": {"id": "m1", "provider": "p"}, "metrics": {"inputTokens": 10, "outputTokens": 2, "cacheReadTokens": 5}},
            {"id": "r1", "role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "name": "read_files",
                "content": [{"query": "a", "result": "alpha", "success": true}]}]},
            {"id": "e1", "role": "assistant", "content": [{"type": "text", "text": "boom"}],
             "metadata": {"displayOnly": true, "displayRole": "error"}}]}),
        );
        write(
            &dir,
            "agent_x.messages.json",
            json!({"agent": "subagent", "sessionId": "100_abcde__agent_x",
            "origin": {"mode": "subagent", "parentThreadId": "100_abcde", "subagent": "agent_x"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "do it"}]},
                         {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}]}),
        );

        let refs = discover(std::slice::from_ref(&root));
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["100_abcde", "100_abcde/agent_x"]);
        assert_eq!(refs[0].title.as_deref(), Some("t"));

        let lead = read(&refs[0]);
        assert_eq!(lead.cwd.as_deref(), Some(Path::new("/p")));
        assert_eq!(lead.system_prompt.as_deref(), Some("sys"));
        let kinds: Vec<MessageKind> = lead.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [
                MessageKind::Prompt,
                MessageKind::Reply,
                MessageKind::ToolResult,
                MessageKind::Error
            ]
        );
        let u = lead.messages[1].usage.as_ref().unwrap();
        assert_eq!((u.input_tokens, u.cache_read_tokens), (Some(10), Some(5)));
        match &lead.messages[2].content[0] {
            Block::ToolResult { content, tool_name, .. } => {
                assert_eq!(tool_name.as_deref(), Some("read_files"));
                assert!(content.to_string().contains("alpha"));
            }
            b => panic!("{b:?}"),
        }

        let child = read(&refs[1]);
        assert_eq!(child.lineage.parent.as_deref(), Some("100_abcde"));
        assert_eq!(child.lineage.agent_path.as_deref(), Some("agent_x"));
        assert_eq!(child.messages[0].origin, Origin::Subagent);
        let _ = fs::remove_dir_all(&root);
    }
}
