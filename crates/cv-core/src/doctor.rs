//! Compaction doctor: attribute a session's context-window pressure to its sources.
//!
//! Powers both `cv doctor` (CLI rendering) and the `doctor` MCP tool. Walks a session's typed
//! blocks and tallies context tokens by source — tool results (ranked per tool, MCP vs builtin),
//! thinking (plaintext PLUS the signature/encrypted blob, both re-sent to the API), assistant/user
//! text, images, the system reminders Claude Code appends per turn (hook output, edited-file
//! notices, queued task notifications, CLAUDE.md, … — itemized by attachment kind) — plus
//! compaction frequency/triggers and a usage-derived size of the fixed system+tools overhead. That
//! fixed block (base prompt, skills, MCP tool schemas) can't be itemized: the transcript records
//! the conversation, not the system block, so it's only *sized* from token usage, not broken down.

use crate::compaction::CompactionSink;
use crate::ir::{est_tokens, is_mcp_tool, Block, Message, MessageKind, Role};
use crate::stream::{Flow, MessageSink};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

/// Claude tokenizer ≈ 3.3–3.7 bytes/token.
const BYTES_PER_TOKEN: f64 = 3.5;

/// The bucket name for an injected-context turn: the harness's own attachment kind when it recorded
/// one (`extra[h]["attachment_type"]`, e.g. Claude's `hook_success`), else the IR kind name.
fn injected_kind(m: &Message) -> String {
    m.extra
        .values()
        .filter_map(Value::as_object)
        .find_map(|bag| bag.get("attachment_type"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| m.kind.as_str().to_string())
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct ToolStat {
    pub calls: u64,
    pub result_tokens: u64,
    pub is_mcp: bool,
}

/// A context-attribution report. Accumulate sessions with [`Report::observe`], then render
/// (CLI) or serialize ([`Report::to_json`], for `--json` and the MCP tool).
#[derive(Default, Debug, Clone, Serialize)]
pub struct Report {
    pub sessions: u64,
    pub messages: u64,
    // measured conversational content, by source
    pub user_text: u64,
    pub assistant_text: u64,
    pub thinking: u64,
    pub tool_call_args: u64,
    pub tool_results: u64,
    pub images: u64,
    pub system_text: u64,
    /// System reminders Claude Code appended to the prompt (model-visible `attachment` records).
    pub attachments: u64,
    /// The same, by attachment kind (`hook_success`, `edited_text_file`, `queued_command`, …).
    pub by_attachment: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, ToolStat>,
    // compaction
    pub compactions: u64,
    pub auto_compactions: u64,
    pub pre_tokens: Vec<u64>,
    // usage-derived context sizing
    pub startup_ctx: u64, // worst per-session first-turn total context (≈ fixed overhead)
    pub peak_ctx: u64,    // largest total context observed across turns
}

impl Report {
    /// Start folding one session into the report. Stream its messages into the returned sink (a
    /// [`ParseOptions::full`](crate::ParseOptions::full) pass: attribution counts decoded text,
    /// and the attachment kind and compaction metadata live in `extra`), then call
    /// [`SessionPass::finish`]. One pass, one message in memory at a time.
    pub fn session(&mut self, source: Option<std::path::PathBuf>) -> SessionPass<'_> {
        self.sessions += 1;
        SessionPass {
            rep: self,
            resolver: crate::lazy::Resolver::new(source),
            names: HashMap::new(),
            unnamed: HashMap::new(),
            startup: None,
            compactions: CompactionSink::new(false),
        }
    }

    /// Total measured conversational tokens across all sources.
    pub fn conv_total(&self) -> u64 {
        self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
            + self.attachments
    }

    /// Tool-result tokens attributable to MCP tools ([`is_mcp_tool`]).
    pub fn mcp_tool_results(&self) -> u64 {
        self.by_tool
            .values()
            .filter(|s| s.is_mcp)
            .map(|s| s.result_tokens)
            .sum()
    }

    /// The machine-readable diagnosis — `cv doctor --json` and the MCP `doctor` tool share this.
    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .by_tool
            .iter()
            .map(|(name, s)| json!({"tool": name, "mcp": s.is_mcp, "calls": s.calls, "result_tokens": s.result_tokens}))
            .collect();
        let mcp = self.mcp_tool_results();
        json!({
            "sessions": self.sessions,
            "messages": self.messages,
            "compactions": self.compactions,
            "auto_compactions": self.auto_compactions,
            "pre_tokens": self.pre_tokens,
            "fixed_overhead_est": self.startup_ctx,
            "peak_context": self.peak_ctx,
            "conversational": {
                "total": self.conv_total(),
                "tool_results": self.tool_results,
                "thinking": self.thinking,
                "assistant_text": self.assistant_text,
                "user_text": self.user_text,
                "tool_call_args": self.tool_call_args,
                "images": self.images,
                "system_msgs": self.system_text,
                "system_reminders": self.attachments,
            },
            "by_attachment": self.by_attachment,
            "tool_results_mcp": mcp,
            "tool_results_builtin": self.tool_results.saturating_sub(mcp),
            "by_tool": tools,
        })
    }
}

/// One session streaming into a [`Report`] (see [`Report::session`]): content attribution, the
/// usage-derived context sizing and the compaction tally, folded per message.
pub struct SessionPass<'a> {
    rep: &'a mut Report,
    resolver: crate::lazy::Resolver,
    /// `tool_use_id` → tool name, the last call per id wins. A result usually carries only the
    /// id, so results attribute to real tools (and MCP names), not "(unknown tool)".
    names: HashMap<String, String>,
    /// Result tokens by `tool_use_id` for results that named no tool, attributed at
    /// [`finish`](SessionPass::finish): a call can stream past after its result (a resumed
    /// transcript re-copies history), so the id is resolved against the whole session.
    unnamed: HashMap<String, u64>,
    /// The first turn's total context (≈ system + tools + rules + first message).
    startup: Option<u64>,
    compactions: CompactionSink,
}

impl SessionPass<'_> {
    /// Fold the session's compactions and fixed-overhead estimate into the report.
    pub fn finish(self) {
        let rep = self.rep;
        for (id, t) in self.unnamed {
            let name = self.names.get(&id).map_or("(unknown tool)", String::as_str);
            let e = rep.by_tool.entry(name.to_string()).or_default();
            e.result_tokens += t;
            e.is_mcp |= is_mcp_tool(name);
        }
        for c in self.compactions.into_boundaries() {
            rep.compactions += 1;
            if c.trigger.as_deref() == Some("auto") {
                rep.auto_compactions += 1;
            }
            if let Some(p) = c.pre_tokens {
                rep.pre_tokens.push(p);
            }
        }
        rep.startup_ctx = rep.startup_ctx.max(self.startup.unwrap_or(0));
    }

    fn observe(&mut self, m: &Message) {
        let rep = &mut *self.rep;
        rep.messages += 1;
        if let Some(u) = &m.usage {
            let total = u.prompt_tokens();
            if total > 0 {
                rep.peak_ctx = rep.peak_ctx.max(total);
                self.startup.get_or_insert(total);
            }
        }
        for b in &m.content {
            // What this block costs, by the one shared rule; the arms below only decide which
            // bucket the number lands in.
            let t = est_tokens(b, BYTES_PER_TOKEN);
            match b {
                Block::Text { .. } => {
                    match (m.role, m.kind) {
                        // Harness-side notices, errors, markers and carriers are never sent to
                        // the model: they cost no context.
                        (_, MessageKind::Notice)
                        | (_, MessageKind::Error)
                        | (_, MessageKind::Carrier)
                        | (_, MessageKind::CompactionBoundary)
                        | (_, MessageKind::Branch)
                        | (_, MessageKind::ModelChange) => {}
                        // Context the harness injected into the prompt (system reminders, hook
                        // output, turn context): real per-turn context, itemized by kind — it
                        // used to hide inside "fixed overhead".
                        (_, MessageKind::InjectedContext) => {
                            rep.attachments += t;
                            *rep.by_attachment.entry(injected_kind(m)).or_default() += t;
                        }
                        (Role::Assistant, _) => rep.assistant_text += t,
                        (Role::System, _) => rep.system_text += t,
                        _ => rep.user_text += t,
                    }
                }
                Block::Thinking { .. } => rep.thinking += t,
                Block::ToolUse { id, name, .. } => {
                    rep.tool_call_args += t;
                    self.names.insert(id.clone(), name.clone());
                    let e = rep.by_tool.entry(name.clone()).or_default();
                    e.calls += 1;
                    e.is_mcp |= is_mcp_tool(name);
                }
                Block::ToolResult {
                    tool_name, tool_use_id, ..
                } => {
                    rep.tool_results += t;
                    match tool_name {
                        Some(name) => {
                            let e = rep.by_tool.entry(name.clone()).or_default();
                            e.result_tokens += t;
                            e.is_mcp |= is_mcp_tool(name);
                        }
                        None => *self.unnamed.entry(tool_use_id.clone()).or_default() += t,
                    }
                }
                Block::Image { .. } => rep.images += t,
                Block::File { .. } => {}
            }
        }
    }
}

impl MessageSink for SessionPass<'_> {
    fn message(&mut self, mut m: Message) -> Flow {
        m.materialize(&self.resolver);
        self.observe(&m);
        self.compactions.message(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, body: &str) -> Message {
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: body.to_string().into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });
        m
    }

    fn call(id: &str, name: &str) -> Message {
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            namespace: None,
        });
        m
    }

    /// A result that names no tool attributes to its call's tool even when the call streams past
    /// after it (a resumed transcript re-copies history); an id no call carries is unknown.
    #[test]
    fn unnamed_results_resolve_against_the_whole_session() {
        let mut rep = Report::default();
        let mut pass = rep.session(None);
        for m in [
            result("t1", &"x".repeat(70)),
            call("t1", "mcp__s__q"),
            result("t9", "y"),
        ] {
            let _ = pass.message(m);
        }
        pass.finish();
        let mcp = &rep.by_tool["mcp__s__q"];
        assert_eq!((mcp.calls, mcp.result_tokens, mcp.is_mcp), (1, 20, true));
        assert_eq!(rep.by_tool["(unknown tool)"].result_tokens, 1);
        assert_eq!(rep.mcp_tool_results(), 20);
        assert_eq!((rep.sessions, rep.messages), (1, 3));
    }
}
