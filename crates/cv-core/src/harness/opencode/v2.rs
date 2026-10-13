//! The OpenCode 2.x message model: `session_v2` + `session_message` in `opencode.db`
//! (`packages/core/src/session/sql.ts`, payloads in `packages/schema/src/session-message.ts`, at
//! opencode `v2.0.25`).
//!
//! 2.x replaced the 1.x `message` + `part` pair with one typed, sequenced row per event:
//! `session_message(id, session_id, type, seq, time_created, time_updated, data)`, where `data` is
//! the `Session.Message.Info` JSON minus `id`/`type`. A 1.x store is migrated in place on first run
//! (`database/v1-migration.ts`): the legacy `session`/`message`/`part` tables are left untouched and
//! every session is copied into the new tables under the same id, so a migrated database carries
//! both; the new tables win (2.x writes only those).
//!
//! | `type` | IR |
//! |---|---|
//! | `user` | `Prompt`/Human: `text`, `files[]` (Image or File), `agents[]` (`@name`, a `SubagentSpawn`), `skills[]` (bag) |
//! | `synthetic` | `InjectedContext`/Harness, User role (`description` in the bag); a background sub-agent's completion (`metadata.source = "subagent"`) is a `SubagentReturn`/Subagent, a background shell's (`"shell"`) stays `InjectedContext` |
//! | `system` | `InjectedContext`/Harness, System role: a mid-session model-facing update (not the system prompt) |
//! | `skill` | `InjectedContext`/Harness, User role: a loaded skill's text (`skill`, `name` in the bag) |
//! | `shell` | `InjectedContext`/Human: a user `!command`, its output as text (`shellID`, `status`, `exit` in the bag) |
//! | `assistant` | `Reply`/Model: `content[]` of `text`, `reasoning`, `tool` (results split into a trailing `ToolResult`); `error` makes it an `Error` |
//! | `compaction` | `CompactionBoundary`/Harness, then (status `completed`) a `CompactionSummary` with `summary` |
//! | `model-switched` | `ModelChange`/Harness, `Message::model` = the new model |
//! | `agent-switched`, `location-switched` | `Notice`/Harness |
//! | `idle` | a turn-end marker: a `Carrier` under `ParseOptions::complete`, otherwise skipped |
//!
//! Anything else is kept verbatim as a `Carrier` under `complete`, and is counted in
//! `extra["opencode"]["unknown_message_types"]` on the session otherwise — never silently dropped.

use crate::ir::*;
use crate::stream::ParseOptions;
use serde_json::{json, Map, Value};

pub(super) fn tag_complete(messages: &mut [Message], id: &str, ty: &str) {
    if let Some(primary) = messages.first_mut() {
        primary
            .harness_extra_mut(Harness::OpenCode)
            .insert("record_type".into(), json!(ty));
    }
    for derived in messages.iter_mut().skip(1) {
        derived
            .harness_extra_mut(Harness::OpenCode)
            .insert("record_owner".into(), json!(id));
    }
}
pub(super) fn attach_complete(messages: &mut [Message], id: &str, ty: &str, data: crate::Json) {
    tag_complete(messages, id, ty);
    let mut identity = Map::new();
    identity.insert("id".into(), json!(id));
    identity.insert("type".into(), json!(ty));
    crate::ir::CompleteRecord::attach_replay(messages, Harness::OpenCode, data, identity);
}

/// Map one `session_message` row to IR messages (0–2: a tool-bearing reply gets a trailing
/// `ToolResult`, a completed compaction gets its summary). `Err(())` means an unknown `type`, which
/// the caller tallies; it is still carried under `complete`.
pub(super) fn build(id: &str, ty: &str, data: &Value, opts: &ParseOptions) -> Result<Vec<Message>, ()> {
    build_projected(id, ty, data, opts, None, None, None)
}

/// Selected ordinary shell payloads; shell metadata remains in its existing owned bag.
#[derive(Default)]
pub(super) struct ProjectedShell {
    pub command: Option<crate::lazy::Text>,
    pub output: Option<crate::lazy::Text>,
}

pub(super) fn build_projected(
    id: &str,
    ty: &str,
    data: &Value,
    opts: &ParseOptions,
    text: Option<&crate::lazy::Text>,
    parts: Option<&[super::ProjectedPart]>,
    shell: Option<&ProjectedShell>,
) -> Result<Vec<Message>, ()> {
    let ts = data.pointer("/time/created").and_then(super::super::ts_from_value);
    let new = |role: Role, kind: MessageKind, origin: Origin| {
        let mut m = Message::of_kind(role, kind, origin);
        m.id = Some(id.to_string());
        m.timestamp = ts;
        m
    };
    let text_of = |key: &str| data.get(key).and_then(Value::as_str).filter(|t| !t.is_empty());
    let mut out = Vec::new();
    match ty {
        "user" => {
            let mut m = new(Role::User, MessageKind::Prompt, Origin::Human);
            let mut bag = Map::new();
            if let Some(t) = text_of("text") {
                m.content.push(Block::Text {
                    text: text.cloned().unwrap_or_else(|| t.to_string().into()),
                });
            }
            for f in data.get("files").and_then(Value::as_array).into_iter().flatten() {
                m.content.push(file_block(f));
            }
            if let Some(agents) = data.get("agents").and_then(Value::as_array).filter(|a| !a.is_empty()) {
                for a in agents {
                    if let Some(name) = a.get("name").and_then(Value::as_str) {
                        m.content.push(Block::Text {
                            text: format!("@{name}").into(),
                        });
                    }
                }
                bag.insert("agent_refs".into(), Value::Array(agents.clone()));
                m.kind = MessageKind::SubagentSpawn;
            }
            if let Some(skills) = data.get("skills").and_then(Value::as_array).filter(|a| !a.is_empty()) {
                bag.insert("skills".into(), Value::Array(skills.clone()));
            }
            keep(data, &mut bag, &["metadata"]);
            finish(&mut m, bag);
            out.push(m);
        }
        "synthetic" | "system" | "skill" => {
            let role = if ty == "system" { Role::System } else { Role::User };
            // A background sub-agent's completion arrives as a `synthetic` row with
            // `metadata.source = "subagent"` (and `childID`): that is the sub-agent's return.
            let mut m =
                if ty == "synthetic" && data.pointer("/metadata/source").and_then(Value::as_str) == Some("subagent") {
                    new(role, MessageKind::SubagentReturn, Origin::Subagent)
                } else {
                    new(role, MessageKind::InjectedContext, Origin::Harness)
                };
            if let Some(t) = text_of("text") {
                m.content.push(Block::Text {
                    text: text.cloned().unwrap_or_else(|| t.to_string().into()),
                });
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!(ty));
            keep(data, &mut bag, &["description", "skill", "name", "metadata"]);
            finish(&mut m, bag);
            out.push(m);
        }
        "shell" => {
            let mut m = new(Role::User, MessageKind::InjectedContext, Origin::Human);
            if let Some(c) = text_of("command") {
                m.content.push(Block::Text {
                    text: shell
                        .and_then(|shell| shell.command.clone())
                        .unwrap_or_else(|| format!("$ {c}").into()),
                });
            }
            if let Some(shell) = shell {
                if let Some(text) = &shell.output {
                    m.content.push(Block::Text { text: text.clone() });
                }
            } else if let Some(o) = data.get("output").filter(|o| !o.is_null()) {
                let t = match o {
                    Value::String(s) => s.clone(),
                    // `Shell.Output`: `{output, cursor, size, truncated?}`.
                    o => o
                        .get("output")
                        .or_else(|| o.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| o.to_string()),
                };
                if !t.is_empty() {
                    m.content.push(Block::Text { text: t.into() });
                }
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!("shell"));
            keep(data, &mut bag, &["shellID", "status", "exit", "metadata"]);
            finish(&mut m, bag);
            out.push(m);
        }
        "assistant" => out.extend(assistant(
            new(Role::Assistant, MessageKind::Reply, Origin::Model),
            data,
            parts,
        )),
        "compaction" => {
            let mut b = new(Role::System, MessageKind::CompactionBoundary, Origin::Harness);
            let mut bag = Map::new();
            keep(data, &mut bag, &["status", "reason", "error", "metadata"]);
            let completed = data.get("status").and_then(Value::as_str) == Some("completed");
            if data.get("error").is_some_and(|e| !e.is_null()) {
                if let Some(t) = data.pointer("/error/type").and_then(Value::as_str) {
                    bag.insert("error_name".into(), json!(t));
                }
            }
            finish(&mut b, bag);
            out.push(b);
            if completed {
                let mut s = new(Role::Assistant, MessageKind::CompactionSummary, Origin::Harness);
                s.id = Some(format!("{id}#summary"));
                if let Some(t) = text_of("summary") {
                    s.content.push(Block::Text {
                        text: text.cloned().unwrap_or_else(|| t.to_string().into()),
                    });
                }
                s.model = data.pointer("/model/id").and_then(Value::as_str).map(str::to_string);
                s.usage = super::parse_usage(data);
                let mut bag = Map::new();
                // `recent` is the verbatim tail kept after the summary; `providerContext` the
                // provider-side state the next request resumes from.
                keep(data, &mut bag, &["recent", "providerState", "providerContext"]);
                if let Some(p) = data.pointer("/model/providerID") {
                    bag.insert("providerID".into(), p.clone());
                }
                finish(&mut s, bag);
                out.push(s);
            }
        }
        "model-switched" => {
            let mut m = new(Role::System, MessageKind::ModelChange, Origin::Harness);
            m.model = data.pointer("/model/id").and_then(Value::as_str).map(str::to_string);
            let mut bag = Map::new();
            keep(data, &mut bag, &["model", "previous"]);
            finish(&mut m, bag);
            out.push(m);
        }
        "agent-switched" | "location-switched" => {
            let mut m = new(Role::System, MessageKind::Notice, Origin::Harness);
            let note = if ty == "agent-switched" {
                data.get("agent").and_then(Value::as_str).map(|a| format!("agent: {a}"))
            } else {
                data.pointer("/location/directory")
                    .or_else(|| data.get("subpath"))
                    .and_then(Value::as_str)
                    .map(|l| format!("location: {l}"))
            };
            if let Some(t) = note {
                m.content.push(Block::Text { text: t.into() });
            }
            let mut bag = Map::new();
            bag.insert("message_type".into(), json!(ty));
            keep(
                data,
                &mut bag,
                &["agent", "location", "projectID", "subpath", "previous"],
            );
            finish(&mut m, bag);
            out.push(m);
        }
        "idle" => {
            if opts.complete {
                out.push(carrier(id, ty, data, ts));
            }
        }
        _ => {
            if opts.complete {
                out.push(carrier(id, ty, data, ts));
            }
            return Err(());
        }
    }
    crate::affordance::refine_complete_group(Harness::OpenCode, &mut out).map_err(|_| ())?;
    Ok(out)
}

fn assistant(mut m: Message, data: &Value, projected: Option<&[super::ProjectedPart]>) -> Vec<Message> {
    m.model = data.pointer("/model/id").and_then(Value::as_str).map(str::to_string);
    m.usage = super::parse_usage(data);
    let mut results = Vec::new();
    for (index, c) in data
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let part = projected.and_then(|parts| parts.get(index));
        match c.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = c.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    m.content.push(Block::Text {
                        text: part
                            .and_then(|p| p.text.clone())
                            .unwrap_or_else(|| t.to_string().into()),
                    });
                }
            }
            "reasoning" => {
                let t = c.get("text").and_then(Value::as_str).unwrap_or("");
                let signature = super::find_signature(c.get("state"))
                    .or_else(|| c.pointer("/state/signature").and_then(Value::as_str))
                    .map(str::to_string);
                if !t.is_empty() || signature.is_some() {
                    m.content.push(Block::Thinking {
                        text: part
                            .and_then(|p| p.text.clone())
                            .unwrap_or_else(|| t.to_string().into()),
                        signature,
                        encrypted: None,
                        redacted: false,
                    });
                }
            }
            "tool" => {
                let (u, r) = tool_blocks(c, part);
                m.content.push(u);
                results.extend(r);
            }
            _ => {
                let bag = m.harness_extra_mut(Harness::OpenCode);
                super::push_extra_array(bag, "unknown_content", c.clone());
            }
        }
    }
    let mut bag = Map::new();
    keep(
        data,
        &mut bag,
        &[
            "agent",
            "finish",
            "rawFinish",
            "snapshot",
            "error",
            "retry",
            "providerState",
            "metadata",
        ],
    );
    if let Some(p) = data.pointer("/model/providerID") {
        bag.insert("providerID".into(), p.clone());
    }
    if let Some(v) = data.pointer("/model/variant") {
        bag.insert("variant".into(), v.clone());
    }
    if data.get("error").is_some_and(|e| !e.is_null()) {
        m.kind = MessageKind::Error;
        m.origin = Origin::Harness;
        if let Some(t) = data.pointer("/error/type").and_then(Value::as_str) {
            bag.insert("error_name".into(), json!(t));
        }
        // An error turn often carries no content; its message is what the user saw.
        if m.content.is_empty() {
            if let Some(t) = data.pointer("/error/message").and_then(Value::as_str) {
                m.content.push(Block::Text {
                    text: t.to_string().into(),
                });
            }
        }
    }
    finish(&mut m, bag);
    let mut out = vec![m];
    if !results.is_empty() {
        let mut tm = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        tm.timestamp = out[0].timestamp;
        tm.content = results;
        out.push(tm);
    }
    out
}

/// A 2.x `tool` content item: `{id, name, state{status, input, content[]?, error?, metadata?}, time}`.
/// `content[]` is `text` and `file` items; a `file` result becomes an Image/File block after the
/// result, as a 1.x `state.attachments[]` does.
fn tool_blocks(c: &Value, projected: Option<&super::ProjectedPart>) -> (Block, Vec<Block>) {
    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = c.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let status = c.pointer("/state/status").and_then(Value::as_str).unwrap_or("");
    let input = match c.pointer("/state/input") {
        // `streaming` holds the raw, still-partial argument text.
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    let use_block = Block::ToolUse {
        id: id.clone(),
        name: name.clone(),
        input: projected.and_then(|p| p.input.clone()).unwrap_or_else(|| input.into()),
        namespace: None,
    };
    let mut text = Vec::new();
    let mut files = Vec::new();
    for item in c
        .pointer("/state/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => text.push(item.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
            Some("file") => files.push(result_file(item)),
            _ => text.push(item.to_string()),
        }
    }
    let is_error = status == "error";
    if is_error {
        if let Some(msg) = c.pointer("/state/error/message").and_then(Value::as_str) {
            if text.is_empty() {
                text.push(msg.to_string());
            }
        }
    }
    let mut details = Map::new();
    for (k, v) in [
        ("metadata", c.pointer("/state/metadata")),
        ("error", c.pointer("/state/error")),
        ("time", c.get("time")),
    ] {
        if let Some(v) = v.filter(|v| !v.is_null() && !v.as_object().is_some_and(|o| o.is_empty())) {
            details.insert(k.to_string(), v.clone());
        }
    }
    for k in ["executed", "providerState", "providerResultState"] {
        if let Some(v) = c.get(k) {
            details.insert(k.to_string(), v.clone());
        }
    }
    let mut out = Vec::new();
    if !text.is_empty() || is_error || status == "completed" {
        out.push(Block::ToolResult {
            tool_use_id: id,
            content: projected
                .and_then(|p| p.output.clone())
                .unwrap_or_else(|| text.join("\n").into()),
            is_error,
            tool_name: (!name.is_empty()).then_some(name),
            status: (!status.is_empty()).then(|| status.to_string()),
            details: c
                .pointer("/state/metadata/clustervision.details")
                .cloned()
                .or_else(|| (!details.is_empty()).then_some(Value::Object(details))),
        });
    }
    out.extend(files);
    (use_block, out)
}

/// A user `files[]` attachment: `{data (base64), mime, source{type: inline|uri, uri?}, name?}`. The
/// bytes stay out of the IR; `data_ref`/`source` name the attachment instead.
fn file_block(f: &Value) -> Block {
    let mime = f.get("mime").and_then(Value::as_str).filter(|m| !m.is_empty());
    let uri = f.pointer("/source/uri").and_then(Value::as_str);
    let name = f.get("name").and_then(Value::as_str);
    if mime.is_some_and(|m| m.starts_with("image/")) {
        Block::Image {
            media_type: mime.map(str::to_string),
            data_ref: uri.or(name).map(str::to_string),
        }
    } else {
        Block::File {
            mime: mime.map(str::to_string),
            path: name.map(str::to_string),
            source: uri.map(str::to_string),
        }
    }
}

/// A tool result's `file` content item: `{uri, mime, name?}`.
fn result_file(f: &Value) -> Block {
    let mime = f.get("mime").and_then(Value::as_str).filter(|m| !m.is_empty());
    let uri = f.get("uri").and_then(Value::as_str).map(str::to_string);
    if mime.is_some_and(|m| m.starts_with("image/")) {
        Block::Image {
            media_type: mime.map(str::to_string),
            data_ref: uri,
        }
    } else {
        Block::File {
            mime: mime.map(str::to_string),
            path: f.get("name").and_then(Value::as_str).map(str::to_string),
            source: uri,
        }
    }
}

fn carrier(id: &str, ty: &str, data: &Value, ts: Option<chrono::DateTime<chrono::Utc>>) -> Message {
    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
    m.id = Some(id.to_string());
    m.timestamp = ts;
    m.harness_extra_mut(Harness::OpenCode)
        .insert("record_type".into(), json!(ty));
    let mut rec = data.as_object().cloned().unwrap_or_default();
    rec.insert("id".into(), json!(id));
    rec.insert("type".into(), json!(ty));
    m.extra
        .insert(crate::harness::claude::CARRIER_KEY.into(), Value::Object(rec));
    m
}

fn keep(data: &Value, bag: &mut Map<String, Value>, keys: &[&str]) {
    for k in keys {
        if let Some(v) = data.get(*k).filter(|v| !v.is_null()) {
            bag.insert((*k).to_string(), v.clone());
        }
    }
}

fn finish(m: &mut Message, bag: Map<String, Value>) {
    if !bag.is_empty() {
        m.harness_extra_mut(Harness::OpenCode).extend(bag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(ty: &str, data: &str) -> Vec<Message> {
        build("msg_1", ty, &serde_json::from_str(data).unwrap(), &ParseOptions::full()).unwrap()
    }

    #[test]
    fn each_message_type_has_its_kind() {
        let kinds = |ms: &[Message]| ms.iter().map(|m| (m.role, m.kind, m.origin)).collect::<Vec<_>>();
        let u = one(
            "user",
            r#"{"time":{"created":1},"text":"hi","files":[{"data":"AA==","mime":"image/png","source":{"type":"inline"},"name":"dot.png"}]}"#,
        );
        assert_eq!(kinds(&u), [(Role::User, MessageKind::Prompt, Origin::Human)]);
        assert!(matches!(&u[0].content[1], Block::Image { data_ref: Some(n), .. } if n == "dot.png"));

        let a = one(
            "assistant",
            r#"{"time":{"created":1},"agent":"build","model":{"id":"glm","providerID":"corpus"},
                "content":[{"type":"reasoning","text":"t"},{"type":"text","text":"x"},
                  {"type":"tool","id":"c1","name":"shell","state":{"status":"error","input":{"command":"cat nope"},
                   "error":{"type":"tool.execution","message":"No such file"}},"time":{"created":1}}],
                "cost":0.5,"tokens":{"input":3,"output":2,"reasoning":1,"cache":{"read":0,"write":0}}}"#,
        );
        assert_eq!(
            kinds(&a),
            [
                (Role::Assistant, MessageKind::Reply, Origin::Model),
                (Role::Tool, MessageKind::ToolResult, Origin::Harness)
            ]
        );
        assert_eq!(a[0].model.as_deref(), Some("glm"));
        assert_eq!(a[0].usage.as_ref().unwrap().cost_usd, Some(0.5));
        assert!(
            matches!(&a[1].content[0], Block::ToolResult { is_error: true, content, .. } if content == "No such file")
        );

        let e = one(
            "assistant",
            r#"{"time":{"created":1},"agent":"build","model":{"id":"glm","providerID":"corpus"},"content":[],
                "error":{"type":"provider.error","message":"boom"}}"#,
        );
        assert_eq!(kinds(&e), [(Role::Assistant, MessageKind::Error, Origin::Harness)]);
        assert_eq!(e[0].extra["opencode"]["error_name"], "provider.error");

        let c = one(
            "compaction",
            r#"{"time":{"created":1},"status":"completed","reason":"manual","summary":"S","recent":"R"}"#,
        );
        assert_eq!(
            kinds(&c),
            [
                (Role::System, MessageKind::CompactionBoundary, Origin::Harness),
                (Role::Assistant, MessageKind::CompactionSummary, Origin::Harness)
            ]
        );
        assert_eq!(c[1].text().as_deref(), Some("S"));

        let m = one(
            "model-switched",
            r#"{"time":{"created":1},"model":{"id":"alt","providerID":"corpus"}}"#,
        );
        assert_eq!(
            (m[0].kind, m[0].model.as_deref()),
            (MessageKind::ModelChange, Some("alt"))
        );
        assert_eq!(
            one("agent-switched", r#"{"time":{"created":1},"agent":"plan"}"#)[0].kind,
            MessageKind::Notice
        );
        assert_eq!(
            one("synthetic", r#"{"time":{"created":1},"text":"done"}"#)[0].kind,
            MessageKind::InjectedContext
        );
        let r = one(
            "synthetic",
            r#"{"time":{"created":1},"text":"ok","metadata":{"source":"subagent","childID":"ses_c"}}"#,
        );
        assert_eq!(
            (r[0].kind, r[0].origin),
            (MessageKind::SubagentReturn, Origin::Subagent)
        );
        assert_eq!(
            one(
                "shell",
                r#"{"time":{"created":1},"shellID":"s","command":"ls","status":"exited"}"#
            )[0]
            .origin,
            Origin::Human
        );
        let sh = one(
            "shell",
            r#"{"time":{"created":1},"shellID":"s","command":"echo hi","status":"exited","exit":0,"output":{"output":"hi\n","cursor":3,"size":3}}"#,
        );
        assert_eq!(sh[0].text().as_deref(), Some("$ echo hi\nhi\n"));
    }

    #[test]
    fn idle_and_unknown_types_are_carried_only_when_complete() {
        let d = serde_json::json!({"time": {"created": 1}, "outcome": "succeeded"});
        assert!(build("i", "idle", &d, &ParseOptions::full()).unwrap().is_empty());
        let c = build("i", "idle", &d, &ParseOptions::complete()).unwrap();
        assert_eq!(c[0].kind, MessageKind::Carrier);
        assert!(build("x", "from-the-future", &d, &ParseOptions::full()).is_err());
    }
}
