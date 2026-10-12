//! **Account data-export** adapters — the archives you download from ChatGPT ("Export data") and
//! Claude.ai. Both pack *many* conversations into one big JSON file (`conversations.json`, often split
//! `conversations-000.json … -NNN.json`), so unlike the per-session harnesses one file yields many
//! [`SessionRef`]s. Two shapes, sniffed by content (never a filename):
//!
//! - **ChatGPT export** (`Harness::ChatGptExport`): each conversation has a `mapping` — a DAG of
//!   `node_id -> {parent, children[], message}` — with `current_node` as the active leaf. We walk the
//!   root→`current_node` chain (the live branch). A message has `author.role`, `content.{content_type,
//!   parts|text}`, and `recipient` (≠`all` ⇒ a tool call). `default_model_slug` is the model.
//! - **Claude.ai export** (`Harness::ClaudeExport`): each conversation has `chat_messages[]`, linear
//!   via `parent_message_uuid`; `sender` ∈ human/assistant; `content[]` blocks are
//!   text/thinking/tool_use/tool_result.
//!
//! Distinct from `claude` (Claude Code JSONL). Exports have no fixed home, so discovery is
//! **opt-in**: the sources registered with `cv config --add-export <path>`, unioned with
//! `$CV_EXPORTS` (`:`-separated dirs/files), scanned for `conversations*.json`. With nothing
//! registered no scan happens (see [`export_dirs`]).

use super::wire::anthropic;
use super::{ts_from_value, Adapter};
use crate::ir::*;
use crate::stream::{MessageSink, ParseOptions};
use anyhow::{Context, Result};
#[cfg(test)]
use serde_json::value::RawValue;

mod native;
#[cfg(test)]
use serde_json::Map;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ChatGpt,
    Claude,
}

impl Kind {
    fn harness(self) -> Harness {
        match self {
            Kind::ChatGpt => Harness::ChatGptExport,
            Kind::Claude => Harness::ClaudeExport,
        }
    }
}

// ── Adapters (thin wrappers over the shared engine, one per export shape) ──────

pub struct ChatGptExport;
pub struct ClaudeExport;
impl Adapter for ChatGptExport {
    fn harness(&self) -> Harness {
        Harness::ChatGptExport
    }
    fn roots(&self) -> Vec<PathBuf> {
        export_dirs()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::ChatGpt))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(Kind::ChatGpt, r, opts, sink))
    }
}

impl Adapter for ClaudeExport {
    fn harness(&self) -> Harness {
        Harness::ClaudeExport
    }
    fn roots(&self) -> Vec<PathBuf> {
        export_dirs()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind(Kind::Claude))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(Kind::Claude, r, opts, sink))
    }
}

impl ChatGptExport {
    /// Discover only supplied export files/directories, without consulting configuration.
    pub fn with_roots(roots: Vec<PathBuf>) -> impl Adapter {
        RootedExport {
            kind: Kind::ChatGpt,
            roots,
        }
    }
}
impl ClaudeExport {
    /// Discover only supplied export files/directories, without consulting configuration.
    pub fn with_roots(roots: Vec<PathBuf>) -> impl Adapter {
        RootedExport {
            kind: Kind::Claude,
            roots,
        }
    }
}
struct RootedExport {
    kind: Kind,
    roots: Vec<PathBuf>,
}
impl Adapter for RootedExport {
    fn harness(&self) -> Harness {
        self.kind.harness()
    }
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_kind_in(
            self.kind,
            files_under(self.roots.clone(), is_conversations_file),
        ))
    }
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| native::stream(self.kind, r, opts, sink))
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Where to look for export archives — the registered sources from the config file
/// (`cv config --add-export <path>`; see [`crate::config`]), unioned with `$CV_EXPORTS` (a
/// `:`-separated ad-hoc list). **Opt-in**: with nothing registered and the env unset, export discovery
/// is a no-op, so the default `cv ls`/`discover_all` never pays to scan (account exports are huge —
/// enumerating thousands of conversations across multi-MB files takes seconds).
pub(crate) fn export_dirs() -> Vec<PathBuf> {
    let mut dirs = crate::config::load().exports;
    if let Ok(spec) = std::env::var("CV_EXPORTS") {
        dirs.extend(spec.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    dirs.retain(|p| p.exists());
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Every `conversations*.json` under the export dirs.
fn export_files() -> Vec<PathBuf> {
    files_under(export_dirs(), is_conversations_file)
}

/// Every file under `roots` matching `pred` (depth ≤ 2; a dir entry is scanned, a file entry is
/// taken directly), sorted and deduped. Shared with [`opensession`](super::opensession), which
/// discovers `.opensession.json` documents through the same registered sources — they have no
/// fixed home either.
pub(crate) fn files_under(roots: Vec<PathBuf>, pred: fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        if root.is_file() {
            if pred(&root) {
                out.push(root);
            }
            continue;
        }
        // root dir + one level of subdirs (an extracted export is usually a subfolder)
        for depth1 in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let p = depth1.path();
            if p.is_file() && pred(&p) {
                out.push(p);
            } else if p.is_dir() {
                for depth2 in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let q = depth2.path();
                    if q.is_file() && pred(&q) {
                        out.push(q);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_conversations_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "conversations.json" || (n.starts_with("conversations") && n.ends_with(".json")))
}

/// Read a file's top-level conversation array as borrowed raw slices (no per-conversation
/// materialization until we pick one).
#[cfg(test)]
fn read_array(path: &Path) -> Result<(String, Vec<Box<RawValue>>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading export {}", path.display()))?;
    // Tolerate either a bare array or a single object; non-arrays yield nothing.
    let arr: Vec<Box<RawValue>> = serde_json::from_str(&text).unwrap_or_default();
    Ok((text, arr))
}

/// Enumerate every conversation of `kind` across all export files as lightweight [`SessionRef`]s.
/// Reads each file once: parse the array, sniff the kind from its first element, then enumerate.
/// Dedups by conversation id (keeping the first) — overlapping exports (e.g. a loose
/// `conversations-000.json` plus an extracted full archive) commonly repeat the same conversation.
fn discover_kind(kind: Kind) -> Vec<SessionRef> {
    discover_kind_in(kind, export_files())
}

fn discover_kind_in(kind: Kind, paths: Vec<PathBuf>) -> Vec<SessionRef> {
    native::discover(kind, paths)
}

/// Build a [`SessionRef`] for one conversation `v` (metadata only — counts message turns cheaply).
#[cfg(test)]
fn conv_ref(kind: Kind, path: &Path, v: &Value) -> Option<SessionRef> {
    let id = conv_id(v)?;
    let title = match kind {
        Kind::ChatGpt => v.get("title").and_then(Value::as_str),
        Kind::Claude => v.get("name").and_then(Value::as_str),
    }
    .filter(|s| !s.is_empty())
    .map(str::to_string);
    let created = v
        .get("create_time")
        .or_else(|| v.get("created_at"))
        .and_then(ts_from_value);
    let updated = v
        .get("update_time")
        .or_else(|| v.get("updated_at"))
        .and_then(ts_from_value);
    let message_count = match kind {
        Kind::ChatGpt => chatgpt_active_chain(v)
            .iter()
            .filter(|n| convo_role(n.get("message")).is_some_and(|r| r == Role::User || r == Role::Assistant))
            .count(),
        Kind::Claude => v
            .get("chat_messages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|m| matches!(m.get("sender").and_then(Value::as_str), Some("human" | "assistant")))
                    .count()
            })
            .unwrap_or(0),
    };
    Some(SessionRef {
        title,
        created_at: created,
        updated_at: updated,
        message_count,
        ..SessionRef::new(id, kind.harness(), path.to_path_buf())
    })
}

#[cfg(test)]
fn conv_id(v: &Value) -> Option<String> {
    v.get("conversation_id")
        .or_else(|| v.get("id"))
        .or_else(|| v.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ── Parse one conversation ──────────────────────────────────────────────────

#[cfg(test)]
fn parse_ref(kind: Kind, r: &SessionRef) -> Result<Session> {
    let (_text, arr) = read_array(&r.path)?;
    let conv = arr
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw.get()).ok())
        .find(|v| conv_id(v).as_deref() == Some(r.id.as_str()))
        .with_context(|| format!("conversation {} not found in {}", r.id, r.path.display()))?;
    let mut s = Session {
        id: r.id.clone(),
        harness: kind.harness(),
        cwd: None,
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: conv
            .get("default_model_slug")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        git: None,
        messages: match kind {
            Kind::ChatGpt => chatgpt_messages(&conv),
            Kind::Claude => claude_messages(&conv),
        },
        source_path: Some(r.path.clone()),
        extra: Map::new(),
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    };
    if s.title.is_none() {
        s.title = s.first_user_text().map(|t| truncate(&t, 80));
    }
    Ok(s)
}

/// The active branch (root → `current_node`) of a ChatGPT conversation's `mapping` DAG, root-first.
/// Falls back to mapping order when `current_node` is absent. Shared by message-building and the
/// metadata count so they agree (dead/regenerated branches are excluded).
#[cfg(test)]
fn chatgpt_active_chain(conv: &Value) -> Vec<&Value> {
    let Some(mapping) = conv.get("mapping").and_then(Value::as_object) else {
        return vec![];
    };
    let Some(cur) = conv.get("current_node").and_then(Value::as_str) else {
        return mapping.values().collect();
    };
    let mut chain: Vec<&Value> = Vec::new();
    let mut node_id = Some(cur.to_string());
    let mut guard = 0;
    while let Some(nid) = node_id {
        guard += 1;
        if guard > 100_000 {
            break;
        }
        let Some(node) = mapping.get(&nid) else { break };
        chain.push(node);
        node_id = node.get("parent").and_then(Value::as_str).map(str::to_string);
    }
    chain.reverse();
    chain
}

/// ChatGPT: walk the active branch of the `mapping` DAG into ordered turns. Stateful, because tool
/// calls and their results are separate turns linked only by tool *name* + order (ChatGPT carries no
/// explicit `tool_use_id`): we remember the last call per tool name and wire each result back to it.
#[cfg(test)]
fn chatgpt_messages(conv: &Value) -> Vec<Message> {
    let mut out = Vec::new();
    let mut last_call: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for node in chatgpt_active_chain(conv) {
        let Some(msg) = node.get("message") else { continue };
        let Some(role) = convo_role(Some(msg)) else { continue };
        let node_id = node.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let recipient = msg.get("recipient").and_then(Value::as_str).unwrap_or("all");
        let text = chatgpt_content_text(msg.get("content"));

        let mut content = Vec::new();
        if role == Role::Assistant && recipient != "all" {
            // A tool call. `name` = the tool (recipient); `input` = the raw content object (the call
            // payload — code/query string — kept verbatim, lossless).
            content.push(Block::ToolUse {
                id: node_id.clone(),
                name: recipient.to_string(),
                input: msg.get("content").cloned().unwrap_or(Value::Null),
                namespace: None,
            });
            last_call.insert(recipient.to_string(), node_id.clone());
        } else if role == Role::Tool {
            // A tool result. Link it to the most recent call of the same tool (author.name).
            let tool = msg
                .get("author")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            content.push(Block::ToolResult {
                tool_use_id: last_call.get(tool).cloned().unwrap_or_default(),
                content: text.clone().into(),
                is_error: false,
                tool_name: (!tool.is_empty()).then(|| tool.to_string()),
                status: None,
                details: None,
            });
            for ptr in chatgpt_image_pointers(msg.get("content")) {
                content.push(Block::Image {
                    media_type: None,
                    data_ref: Some(ptr),
                });
            }
        } else {
            if !text.trim().is_empty() {
                content.push(Block::Text { text: text.into() });
            }
            for ptr in chatgpt_image_pointers(msg.get("content")) {
                content.push(Block::Image {
                    media_type: None,
                    data_ref: Some(ptr),
                });
            }
        }
        if content.is_empty() {
            continue;
        }
        let mut m = Message::new(role);
        // A non-user/assistant/tool author (custom instructions, `user_editable_context`) is a
        // system turn — the system prompt as far as the export records one.
        if role == Role::System {
            m.kind = MessageKind::SystemPrompt;
        }
        m.id = (!node_id.is_empty()).then(|| node_id.clone());
        m.parent_id = node.get("parent").and_then(Value::as_str).map(str::to_string);
        m.timestamp = msg.get("create_time").and_then(ts_from_value);
        m.content = content;
        out.push(m);
    }
    out
}

/// Role of a ChatGPT mapping node's `message` (None if no message / unknown author).
#[cfg(test)]
fn convo_role(msg: Option<&Value>) -> Option<Role> {
    let role = msg?.get("author")?.get("role")?.as_str()?;
    Some(role_from_str(role))
}

fn role_from_str(s: &str) -> Role {
    match s {
        "user" | "human" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::System,
    }
}

/// Flatten a ChatGPT `content` object's text (parts joined; `text` for code/exec; ignores image parts).
#[cfg(test)]
fn chatgpt_content_text(content: Option<&Value>) -> String {
    let Some(c) = content else { return String::new() };
    if let Some(parts) = c.get("parts").and_then(Value::as_array) {
        return parts.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join("\n");
    }
    // code / execution_output / tether_* carry `text`.
    c.get("text").and_then(Value::as_str).unwrap_or("").to_string()
}

/// Image asset pointers in a ChatGPT `multimodal_text` content (`{content_type:image_asset_pointer,
/// asset_pointer:"file-service://…"}` parts).
#[cfg(test)]
fn chatgpt_image_pointers(content: Option<&Value>) -> Vec<String> {
    let Some(parts) = content.and_then(|c| c.get("parts")).and_then(Value::as_array) else {
        return vec![];
    };
    parts
        .iter()
        .filter_map(|p| p.as_object())
        .filter_map(|o| o.get("asset_pointer").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Claude.ai export: `chat_messages[]` (already in order) → IR turns.
#[cfg(test)]
fn claude_messages(conv: &Value) -> Vec<Message> {
    let Some(msgs) = conv.get("chat_messages").and_then(Value::as_array) else {
        return vec![];
    };
    msgs.iter()
        .filter_map(|m| {
            let role = role_from_str(m.get("sender").and_then(Value::as_str)?);
            let content = claude_content_blocks(m);
            if content.is_empty() {
                return None;
            }
            let mut msg = Message::new(role);
            if role == Role::System {
                msg.kind = MessageKind::SystemPrompt;
            }
            msg.id = m.get("uuid").and_then(Value::as_str).map(str::to_string);
            msg.parent_id = m.get("parent_message_uuid").and_then(Value::as_str).map(str::to_string);
            msg.timestamp = m.get("created_at").and_then(ts_from_value);
            msg.content = content;
            Some(msg)
        })
        .collect()
}

/// A Claude.ai message's `content[]` blocks → IR blocks (the Anthropic shape, empty text dropped;
/// a `tool_result` may carry its text under `text`), with the flat `text` field as a fallback when
/// there are no structured blocks.
#[cfg(test)]
fn claude_content_blocks(m: &Value) -> Vec<Block> {
    let mut out: Vec<Block> = m
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    let mut block = anthropic::block_from(b, &anthropic::CLAUDE_AI)?;
                    // The export spells some thinking and tool-result text under `text`.
                    match &mut block {
                        Block::ToolResult { content, .. } | Block::Thinking { text: content, .. }
                            if content.is_empty() =>
                        {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                *content = t.into();
                            }
                        }
                        _ => {}
                    }
                    Some(block)
                })
                .collect()
        })
        .unwrap_or_default();
    if out.is_empty() {
        if let Some(t) = m.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            out.push(Block::Text { text: t.into() });
        }
    }
    out
}

#[cfg(test)]
mod tests;
