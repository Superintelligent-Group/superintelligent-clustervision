//! Rendering an IR [`Session`] to human/LLM-friendly text. Shared by the `cv` CLI and `cv-mcp`.

use crate::ir::*;
use chrono::{DateTime, Utc};
use std::path::Path;

/// Render a session as Markdown (good for sharing / feeding to another model). The streaming
/// `cv export --format md` renders the same header and sections one message at a time through
/// [`md_header`] and [`md_message`].
pub fn to_markdown(s: &Session) -> String {
    let mut out = md_header(
        &s.label(),
        s.harness,
        &s.id,
        s.cwd.as_deref(),
        s.model.as_deref(),
        s.created_at,
    );
    for m in &s.messages {
        md_message(m, &mut out);
    }
    out
}

/// The Markdown front-matter: title, harness, id, cwd (home-relative), model and start time, each
/// only when known, then a blank line.
pub fn md_header(
    label: &str,
    harness: Harness,
    id: &str,
    cwd: Option<&Path>,
    model: Option<&str>,
    started: Option<DateTime<Utc>>,
) -> String {
    let mut out = format!("# {label}\n\n- harness: {harness}\n- id: {id}\n");
    if let Some(cwd) = cwd {
        out.push_str(&format!("- cwd: {}\n", home_rel(cwd)));
    }
    if let Some(m) = model {
        out.push_str(&format!("- model: {m}\n"));
    }
    if let Some(t) = started {
        out.push_str(&format!("- started: {}\n", t.to_rfc3339()));
    }
    out.push('\n');
    out
}

/// One Markdown section per turn: `## <turn label>` then every block through [`md_block`].
pub fn md_message(m: &Message, out: &mut String) {
    out.push_str(&format!("## {}\n\n", turn_label(m)));
    for b in &m.content {
        md_block(b, out);
    }
}

/// One block as Markdown: text verbatim, thinking as a quoted `🧠` aside, a tool call as a fenced
/// JSON input, a result as a fenced (4000-char) excerpt, files and images as italic placeholders.
pub fn md_block(b: &Block, out: &mut String) {
    match b {
        Block::Text { text } => {
            out.push_str(text);
            out.push_str("\n\n");
        }
        Block::Thinking { text, .. } => {
            out.push_str("> 🧠 ");
            out.push_str(&text.replace('\n', "\n> "));
            out.push_str("\n\n");
        }
        Block::ToolUse { name, input, .. } => {
            out.push_str(&format!("**🔧 {name}**\n\n```json\n{input}\n```\n\n"));
        }
        Block::ToolResult { content, is_error, .. } => {
            out.push_str(&format!(
                "**↩ result{}**\n\n```\n{}\n```\n\n",
                if *is_error { " (error)" } else { "" },
                truncate(content, 4000)
            ));
        }
        Block::File { path, source, .. } => {
            out.push_str(&format!("_[file: {}]_\n\n", file_label(path, source)));
        }
        Block::Image { .. } => out.push_str("_[image]_\n\n"),
    }
}

fn file_label(path: &Option<String>, source: &Option<String>) -> String {
    path.as_deref().or(source.as_deref()).unwrap_or("?").to_string()
}

pub fn role_label(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The label a rendered turn gets: the role, refined by its `kind` whenever the kind says more than
/// the role does (`system · compaction_boundary`, `user · compaction_summary`, `system · error`),
/// and for injected context by the harness's own attachment kind when it recorded one
/// (`system · injected_context/hook_success`). A plain prompt or reply is just `user` / `assistant`.
pub fn turn_label(m: &Message) -> String {
    let role = role_label(m.role);
    if m.kind == MessageKind::for_role(m.role) {
        return role.to_string();
    }
    let attachment_kind = m
        .extra
        .values()
        .filter_map(serde_json::Value::as_object)
        .find_map(|bag| bag.get("attachment_type"))
        .and_then(serde_json::Value::as_str);
    match (m.kind, attachment_kind) {
        (MessageKind::InjectedContext, Some(k)) => format!("{role} · {}/{k}", m.kind.as_str()),
        _ => format!("{role} · {}", m.kind.as_str()),
    }
}
