//! Rendering an IR [`Session`] to human/LLM-friendly text. Shared by the `cv` CLI and `cv mcp`.

use crate::ir::*;
use chrono::{DateTime, Utc};
use std::io::Write;
use std::path::Path;

/// Write complete Markdown with bounded scratch for lazy text and JSON leaves. Callers must
/// discard staged output on error: a source or writer failure may follow an emitted prefix.
pub fn write_markdown(s: &Session, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    let reads = crate::source::session_reads(s);
    out.write_all(
        md_header(
            &s.label(),
            s.harness,
            &s.id,
            s.cwd.as_deref(),
            s.model.as_deref(),
            s.created_at,
        )
        .as_bytes(),
    )?;
    for message in &s.messages {
        write_md_message(message, out, resolver)?;
    }
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "session sources changed during Markdown output"
    );
    Ok(())
}

/// Write one Markdown section, preserving the owned renderer's exact formatting.
pub fn write_md_message(m: &Message, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    write!(out, "## {}\n\n", turn_label(m))?;
    for block in &m.content {
        write_md_block(block, out, resolver)?;
    }
    Ok(())
}

/// Write a Markdown block. Result excerpts retain only 4001 decoded scalars, while the entire
/// source is demanded so discarded tails cannot hide read errors or stale dependencies.
pub fn write_md_block(b: &Block, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    match b {
        Block::Text { text } => {
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                out.write_all(part.as_bytes())?;
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::Thinking { text, .. } => {
            out.write_all("> 🧠 ".as_bytes())?;
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                for (i, line) in part.split('\n').enumerate() {
                    if i != 0 {
                        out.write_all(b"\n> ")?;
                    }
                    out.write_all(line.as_bytes())?;
                }
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::ToolUse { name, input, .. } => {
            write!(out, "**🔧 {name}**\n\n```json\n")?;
            input.write_canonical(out)?;
            out.write_all(b"\n```\n\n")?;
        }
        Block::ToolResult { content, is_error, .. } => {
            let mut head = String::new();
            let mut remaining = 4001;
            content.try_for_each_chunk(resolver, 64 * 1024, |part| {
                for c in part.chars().take(remaining) {
                    head.push(c);
                    remaining -= 1;
                }
                Ok(())
            })?;
            write!(
                out,
                "**↩ result{}**\n\n```\n{}\n```\n\n",
                if *is_error { " (error)" } else { "" },
                truncate(&head, 4000)
            )?;
        }
        Block::File { path, source, .. } => write!(out, "_[file: {}]_\n\n", file_label(path, source))?,
        Block::Image { .. } => out.write_all(b"_[image]_\n\n")?,
    }
    Ok(())
}

/// Render a session as Markdown (good for sharing / feeding to another model). The streaming
/// `cv export --format md` renders the same header and sections one message at a time through
/// [`md_header`] and [`md_message`].
pub fn to_markdown(s: &Session) -> anyhow::Result<String> {
    let mut out = Vec::new();
    write_markdown(s, &mut out, &s.resolver())?;
    Ok(String::from_utf8(out)?)
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
pub fn md_message(m: &Message, out: &mut String) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_md_message(m, &mut bytes, &crate::Resolver::new(None))?;
    out.push_str(&String::from_utf8(bytes)?);
    Ok(())
}

/// Append one block using the checked Markdown writer. Use `write_md_block` to avoid retaining
/// the complete rendered block when writing a stream or staging a document.
pub fn md_block(b: &Block, out: &mut String) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_md_block(b, &mut bytes, &crate::Resolver::new(None))?;
    out.push_str(&String::from_utf8(bytes)?);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Source(std::path::PathBuf);
    impl Source {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("cv-render-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
        fn json(&self) -> crate::Json {
            crate::Json::source(&self.0, 0, std::fs::metadata(&self.0).unwrap().len()).unwrap()
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn source_text(value: &crate::Json, key: &str) -> crate::Text {
        value
            .select(key)
            .unwrap()
            .unwrap()
            .with_view(|view| Ok(view.source_text()?.unwrap()))
            .unwrap()
    }
    fn old_md_block(b: &Block, out: &mut String) -> anyhow::Result<()> {
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
                out.push_str(&format!("**🔧 {name}**\n\n```json\n"));
                crate::projection::append_json(out, input)?;
                out.push_str("\n```\n\n");
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
        Ok(())
    }

    fn old_markdown(session: &Session) -> String {
        let mut out = md_header(
            &session.label(),
            session.harness,
            &session.id,
            session.cwd.as_deref(),
            session.model.as_deref(),
            session.created_at,
        );
        for message in &session.messages {
            out.push_str(&format!("## {}\n\n", turn_label(message)));
            for block in &message.content {
                old_md_block(block, &mut out).unwrap();
            }
        }
        out
    }
    #[test]
    fn checked_session_writers_match_complete_owned_format_oracles() {
        let fixture = serde_json::json!({
            "text": "α\n🌙\\\"".repeat(20000),
            "result": "🌙\n".repeat(3000),
            "input": {"nested": [[], {}, "α\n\\\"", 1.25]}
        });
        let source = Source::new(&serde_json::to_vec(&fixture).unwrap());
        let value = source.json();
        let mut session = Session::new("checked", Harness::Codex);
        session.title = Some("complete formatting".into());
        session.model = Some("model".into());
        session.cwd = Some("/tmp/fixture".into());
        session.system_prompt = Some("system\n🌙".into());
        session
            .extra
            .insert("cv".into(), serde_json::json!({"loom": [{"source_id": "one"}]}));
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text {
                text: source_text(&value, "text"),
            },
            Block::Thinking {
                text: source_text(&value, "text"),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            Block::ToolUse {
                id: "call".into(),
                name: "tool".into(),
                namespace: None,
                input: value.select("input").unwrap().unwrap(),
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: source_text(&value, "result"),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            },
            Block::File {
                mime: None,
                path: None,
                source: Some("asset".into()),
            },
            Block::Image {
                media_type: None,
                data_ref: None,
            },
        ];
        session.messages.push(message);
        session.messages.push(Message::new(Role::Assistant));
        let mut owned = session.clone();
        owned.materialize().unwrap();
        let resolver = session.resolver();
        let mut markdown = Vec::new();
        write_markdown(&session, &mut markdown, &resolver).unwrap();
        assert_eq!(markdown, old_markdown(&owned).as_bytes());
        assert_eq!(to_markdown(&session).unwrap(), old_markdown(&owned));
        let mut compact = Vec::new();
        session.write_json(&mut compact, &resolver).unwrap();
        assert_eq!(compact, serde_json::to_vec(&owned).unwrap());
        let mut pretty = Vec::new();
        session.write_json_pretty(&mut pretty, &resolver).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&owned).unwrap());
        let empty = Session::new("empty", Harness::Claude);
        pretty.clear();
        empty.write_json_pretty(&mut pretty, &empty.resolver()).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&empty).unwrap());
    }

    #[test]
    fn staged_session_outputs_reject_a_source_changed_after_its_last_leaf() {
        struct Mutating<'a> {
            stage: crate::bytes::ByteStage,
            path: &'a Path,
            changed: bool,
        }
        impl Write for Mutating<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                // The later message's role is emitted after all first-message lazy demands.
                if !self.changed && bytes.windows(9).any(|part| part == b"assistant") {
                    std::fs::remove_file(self.path)?;
                    self.changed = true;
                }
                self.stage.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.stage.flush()
            }
        }
        for json in [false, true] {
            let source = Source::new(br#"{"text":"early"}"#);
            let mut session = Session::new("guarded", Harness::Codex);
            let mut first = Message::new(Role::User);
            first.content.push(Block::Text {
                text: source_text(&source.json(), "text"),
            });
            session.messages = vec![first, Message::new(Role::Assistant)];
            let mut writer = Mutating {
                stage: crate::bytes::ByteStage::new(),
                path: &source.0,
                changed: false,
            };
            let result = if json {
                session.write_json_pretty(&mut writer, &session.resolver())
            } else {
                write_markdown(&session, &mut writer, &session.resolver())
            };
            assert!(writer.changed);
            assert!(result.is_err());
            assert!(writer.stage.len() > 0);
            // No finished ByteSource exists for publication after an error.
        }
    }
}
