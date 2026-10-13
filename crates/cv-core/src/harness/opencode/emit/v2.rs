//! Native 2.0.25 schema; one checked, contiguous TEXT row at a time.
use crate::{
    emit::*,
    harness::wire::write::{self, ObjectWriter},
    ir::*,
    lazy::Resolver,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{collections::HashMap, io::Write};
use uuid::Uuid;

/// Native empty-store bootstrap: install the complete schema and record its migration frontier.
/// The SQL and IDs are one versioned unit taken from the native-written 2.0.25 corpus.
fn bootstrap(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(include_str!("schema-v2.0.25.sql"))?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut insert = tx.prepare("INSERT INTO migration (id,time_completed) VALUES (?1,?2)")?;
    for id in include_str!("migrations-v2.0.25.txt").lines() {
        insert.execute(params![id, now])?;
    }
    Ok(())
}

pub(super) fn write_database(
    session: &Session,
    opts: &EmitOptions,
    id: &str,
    cwd: &str,
    conn: &mut Connection,
) -> Result<()> {
    let resolver = session.resolver();
    let tx = conn.transaction()?;
    let empty: bool = tx.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
        [],
        |row| row.get(0),
    )?;
    if empty {
        bootstrap(&tx)?;
    }
    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));
    // Reuse native ownership for this directory, otherwise the native non-git project.
    let project: String = tx
        .query_row(
            "SELECT project_id FROM session_v2 WHERE directory=?1 LIMIT 1",
            [cwd],
            |r| r.get(0),
        )
        .optional()?
        .or(tx
            .query_row("SELECT id FROM project WHERE worktree=?1 LIMIT 1", [cwd], |r| r.get(0))
            .optional()?)
        .unwrap_or_else(|| "global".into());
    tx.execute(
        "INSERT OR IGNORE INTO project (id,worktree,time_created,time_updated,sandboxes) VALUES (?1,?2,?3,?3,'[]')",
        params![project, cwd, created],
    )?;
    let bag = session.harness_extra(Harness::OpenCode);
    let provider = bag
        .and_then(|b| b.get("model"))
        .and_then(|v| v.get("providerID"))
        .and_then(Value::as_str)
        .unwrap_or("clustervision");
    let model = session
        .model
        .as_ref()
        .map(|m| json!({"id":m,"providerID":provider}).to_string());
    // INSERT deliberately rejects a reused id: replacing a native session would strand its history.
    tx.execute("INSERT INTO session_v2 (id,project_id,slug,directory,title,version,model,agent,time_created,time_updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![id,project,short_hash(id),cwd,session.title,env!("CARGO_PKG_VERSION"),model,"build",created,updated])?;
    let results: HashMap<&str, &Block> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some((tool_use_id.as_str(), b)),
            _ => None,
        })
        .collect();
    let called = called_tool_ids(session);
    let mut insert = tx.prepare("INSERT INTO session_message (id,session_id,type,seq,time_created,time_updated,data) VALUES (?1,?2,?3,?4,?5,?5,?6)")?;
    let mut seq = 0_i64;
    for msg in &session.messages {
        if msg.role == Role::Tool && !msg.content.iter().any(|b| is_orphan_result(b, &called)) {
            continue;
        }
        let ts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(created + seq);
        let mid = format!("msg_{}", Uuid::now_v7().simple());
        let mut bytes = Vec::new();
        let mut row = ObjectWriter::new(&mut bytes)?;
        let extra = msg.harness_extra(Harness::OpenCode);
        let ty = if msg.role == Role::Assistant || msg.role == Role::Tool {
            "assistant"
        } else if msg.role == Role::System {
            "system"
        } else {
            "user"
        };
        // Complete carriers retain the native typed record. Known model-facing rows use the IR
        // projection below, so thinking policy and edits apply rather than replaying stale text.
        let raw = (msg.kind == MessageKind::Carrier && session.harness == Harness::OpenCode)
            .then(|| msg.extra.get(crate::harness::claude::CARRIER_KEY))
            .flatten();
        let ty = if let Some(raw) = raw {
            let ty = raw
                .get("type")
                .and_then(Value::as_str)
                .context("OpenCode carrier has no type")?;
            for (key, value) in raw.as_object().context("OpenCode carrier is not an object")? {
                if !matches!(key.as_str(), "id" | "type" | "sessionID") {
                    row.field(key, value)?;
                }
            }
            ty
        } else {
            row.field("time", json!({"created":ts,"completed":ts}))?;
            if ty == "assistant" {
                row.field(
                    "agent",
                    extra
                        .and_then(|b| b.get("agent"))
                        .and_then(Value::as_str)
                        .unwrap_or("build"),
                )?;
                row.field("model",json!({"id":msg.model.as_ref().or(session.model.as_ref()).map(String::as_str).unwrap_or("unknown"),"providerID":extra.and_then(|b|b.get("providerID")).and_then(Value::as_str).unwrap_or(provider)}))?;
                for key in [
                    "finish",
                    "rawFinish",
                    "snapshot",
                    "error",
                    "retry",
                    "providerState",
                    "metadata",
                ] {
                    if let Some(value) = extra.and_then(|b| b.get(key)) {
                        row.field(key, value)?;
                    }
                }
                row.field_with("content", |w| {
                    assistant_content(w, msg, opts, &resolver, &results, &called, ts)
                })?;
                if let Some(u) = &msg.usage {
                    row.field("tokens",json!({"input":u.input_tokens.unwrap_or(0),"output":u.output_tokens.unwrap_or(0),"reasoning":u.reasoning_tokens.unwrap_or(0),"cache":{"read":u.cache_read_tokens.unwrap_or(0),"write":u.cache_creation_tokens.unwrap_or(0)}}))?;
                    if let Some(cost) = u.cost_usd {
                        row.field("cost", cost)?;
                    }
                }
            } else {
                row.field_with("text", |w| {
                    // A single JSON string streamed from checked text leaves; no decoded body copy.
                    let mut values = Vec::new();
                    for block in &msg.content {
                        match block {
                            Block::Text { text } => values.push(text.clone()),
                            Block::Thinking { .. } => {
                                let fate = thinking::fate(block, Harness::OpenCode, opts.thinking, &resolver)?;
                                if let Some(text) = thinking::text_with_fate(block, fate) {
                                    values.push(text.into_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                    write::joined_text(w, values.iter(), &resolver)
                })?;
                if let Some(metadata) = extra.and_then(|b| b.get("metadata")) {
                    row.field("metadata", metadata)?;
                }
                let files: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Image { media_type, data_ref } => {
                            Some(attachment(media_type.as_deref(), data_ref.as_deref()))
                        }
                        Block::File { mime, path, source } => {
                            Some(attachment(mime.as_deref(), source.as_deref().or(path.as_deref())))
                        }
                        _ => None,
                    })
                    .collect();
                if !files.is_empty() {
                    row.field("files", files)?;
                }
            }
            ty
        };
        row.finish()?;
        insert
            .execute(params![mid, id, ty, seq, ts, String::from_utf8(bytes)?])
            .context("inserting native OpenCode 2.x message")?;
        seq += 1;
    }
    drop(insert);
    write::check_source_fields(session)?;
    tx.commit()?;
    Ok(())
}

fn attachment(mime: Option<&str>, uri: Option<&str>) -> Value {
    let uri = uri.unwrap_or("");
    // Native attachments require base64 data even for references; empty is valid base64.
    json!({"data":"","mime":mime.unwrap_or("application/octet-stream"),"source":{"type":"uri","uri":uri}})
}

fn assistant_content(
    w: &mut dyn Write,
    msg: &Message,
    opts: &EmitOptions,
    resolver: &Resolver,
    results: &HashMap<&str, &Block>,
    called: &std::collections::HashSet<&str>,
    ts: i64,
) -> Result<()> {
    w.write_all(b"[")?;
    let mut first = true;
    for block in &msg.content {
        let fate = thinking::fate(block, Harness::OpenCode, opts.thinking, resolver)?;
        if fate == ThinkingFate::Drop
            || matches!(block,Block::ToolResult{tool_use_id,..} if called.contains(tool_use_id.as_str()))
        {
            continue;
        }
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        let mut item = ObjectWriter::new(w)?;
        match block {
            Block::Text { text } => {
                item.field("type", "text")?;
                item.field_with("text", |w| write::text(w, text, resolver))?;
            }
            Block::Thinking { text, signature, .. } if fate == ThinkingFate::Keep => {
                item.field("type", "reasoning")?;
                item.field_with("text", |w| write::text(w, text, resolver))?;
                if let Some(signature) = signature {
                    item.field("state", json!({"anthropic":{"signature":signature}}))?;
                }
            }
            Block::Thinking { .. } => {
                let text = thinking::text_with_fate(block, fate).unwrap();
                item.field("type", "text")?;
                item.field_with("text", |w| write::text(w, &text, resolver))?;
            }
            Block::ToolUse { id, name, input, .. } => {
                item.field("type", "tool")?;
                item.field("id", id)?;
                item.field("name", name)?;
                let result = results.get(id.as_str()).copied();
                tool_metadata(&mut item, result, ts)?;
                item.field_with("state", |w| tool_state(w, Some(input), result, resolver))?;
            }
            Block::ToolResult {
                tool_use_id, tool_name, ..
            } => {
                item.field("type", "tool")?;
                item.field("id", tool_use_id)?;
                item.field("name", tool_name.as_deref().unwrap_or("unknown"))?;
                tool_metadata(&mut item, Some(block), ts)?;
                item.field_with("state", |w| tool_state(w, None, Some(block), resolver))?;
            }
            // Native assistant content has no file variant. Preserve a visible URI as text.
            Block::Image { data_ref, .. } => {
                item.field("type", "text")?;
                item.field("text", data_ref.as_deref().unwrap_or("[image]"))?;
            }
            Block::File { path, source, .. } => {
                item.field("type", "text")?;
                item.field("text", source.as_deref().or(path.as_deref()).unwrap_or("[file]"))?;
            }
        }
        item.finish()?;
    }
    w.write_all(b"]")?;
    Ok(())
}
fn tool_metadata(item: &mut ObjectWriter<'_>, result: Option<&Block>, ts: i64) -> Result<()> {
    let details = match result {
        Some(Block::ToolResult { details, .. }) => details.as_ref(),
        _ => None,
    };
    let time = details
        .and_then(|d| d.get("time"))
        .filter(|t| t.get("created").and_then(Value::as_i64).is_some());
    if let Some(time) = time {
        item.field("time", time)?;
    } else {
        item.field("time", json!({"created":ts,"completed":ts}))?;
    }
    for key in ["executed", "providerState", "providerResultState"] {
        if let Some(value) = details.and_then(|d| d.get(key)) {
            item.field(key, value)?;
        }
    }
    Ok(())
}
fn tool_state(
    w: &mut dyn Write,
    input: Option<&crate::Json>,
    result: Option<&Block>,
    resolver: &Resolver,
) -> Result<()> {
    let mut state = ObjectWriter::new(w)?;
    state.field_with("input", |w| match input {
        Some(input) => {
            input.with_view(|v| {
                anyhow::ensure!(v.is_object()?, "OpenCode 2.x tool arguments must be an object");
                Ok(())
            })?;
            input.write_canonical(&mut &mut *w)
        }
        None => {
            w.write_all(b"{}")?;
            Ok(())
        }
    })?;
    if let Some(Block::ToolResult {
        content,
        is_error,
        details,
        ..
    }) = result
    {
        state.field("status", if *is_error { "error" } else { "completed" })?;
        if *is_error {
            if let Some(error) = details.as_ref().and_then(|d| d.get("error")).filter(|e| {
                e.get("type").and_then(Value::as_str).is_some() && e.get("message").and_then(Value::as_str).is_some()
            }) {
                state.field("error", error)?;
            } else {
                state.field_with("error", |w| {
                    let mut error = ObjectWriter::new(w)?;
                    error.field("type", "Tool.Error")?;
                    error.field_with("message", |w| write::text(w, content, resolver))?;
                    error.finish()
                })?;
            }
        }
        state.field_with("content", |w| {
            w.write_all(b"[")?;
            let mut item = ObjectWriter::new(w)?;
            item.field("type", "text")?;
            item.field_with("text", |w| write::text(w, content, resolver))?;
            item.finish()?;
            w.write_all(b"]")?;
            Ok(())
        })?;
        if let Some(details) = details {
            state.field_with("metadata", |w| {
                let mut metadata = ObjectWriter::new(w)?;
                if let Some(native) = details.get("metadata").and_then(Value::as_object) {
                    for (key, value) in native {
                        if key != "clustervision.details" {
                            metadata.field(key, value)?;
                        }
                    }
                }
                if details.get("metadata").is_some() || details.get("title").is_some() || details.get("time").is_some()
                {
                    metadata.field("clustervision.details", details)?;
                } else if let Some(fields) = details.as_object() {
                    for (key, value) in fields {
                        metadata.field(key, value)?;
                    }
                } else {
                    metadata.field("details", details)?;
                }
                metadata.finish()
            })?;
        }
    } else {
        // No result is historical evidence, not a pending operation to execute on resume.
        state.field("status", "completed")?;
        state.field("content", json!([{"type":"text","text":""}]))?;
    }
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_v2_rows_and_fresh_bootstrap_roll_back() -> Result<()> {
        crate::harness::store::sqlite::export_tests::exercise_v2(|session, opts, conn| {
            write_database(session, opts, "ses_checked", "/synthetic/sqlite", conn)
        })
    }
    #[test]
    fn native_v2_export_fixture_and_atomic_duplicate() -> Result<()> {
        let root = std::env::var_os("CV_NATIVE_OPENCODE_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("cv-native-opencode-{}", Uuid::new_v4())));
        std::fs::create_dir_all(&root)?;
        let db = root.join("opencode.db");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/opencode/2.0.25/.local/share/opencode/opencode.db");
        std::fs::copy(fixture, &db)?;
        let mut s = Session::new("foreign", Harness::Claude);
        s.cwd = Some(root.join("project"));
        s.model = Some("fixture-model".into());
        s.title = Some("cv native v2 compatibility".into());
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "fixture prompt".into(),
        });
        s.messages.push(user);
        let mut assistant = Message::new(Role::Assistant);
        assistant.content.push(Block::Thinking {
            text: "fixture reasoning".into(),
            signature: Some("fixture-signature".into()),
            encrypted: None,
            redacted: false,
        });
        assistant.content.push(Block::ToolUse {
            id: "fixture-tool".into(),
            name: "bash".into(),
            input: json!({"command":"printf fixture"}).into(),
            namespace: None,
        });
        assistant.content.push(Block::Text {
            text: "fixture answer".into(),
        });
        s.messages.push(assistant);
        let mut result = Message::new(Role::Tool);
        result.content.push(Block::ToolResult {
            tool_use_id: "fixture-tool".into(),
            content: "fixture output".into(),
            is_error: false,
            tool_name: Some("bash".into()),
            status: Some("completed".into()),
            details: Some(json!({"metadata":{"exit":0},"time":{"created":1700000000000_i64}})),
        });
        s.messages.push(result);
        let opts = EmitOptions {
            new_id: Some("ses_cv_native_v2".into()),
            ..Default::default()
        };
        super::super::emit(&s, &db, &opts)?;
        let conn = Connection::open(&db)?;
        let legacy: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='session'", [], |r| {
            r.get(0)
        })?;
        assert_eq!(legacy, 0);
        let rows: i64 = conn.query_row(
            "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(rows, 2);
        assert!(super::super::emit(&s, &db, &opts).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            2
        );
        let mut malformed = s.clone();
        if let Block::ToolUse { input, .. } = &mut malformed.messages[1].content[1] {
            *input = json!("invalid object").into();
        }
        let bad = EmitOptions {
            new_id: Some("ses_cv_failed_v2".into()),
            ..Default::default()
        };
        assert!(super::super::emit(&malformed, &db, &bad).is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM session_v2 WHERE id='ses_cv_failed_v2'", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        let migrated = root.join("migrated");
        std::fs::create_dir_all(&migrated)?;
        let migrated_db = migrated.join("opencode.db");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus/opencode/1.18.20-migrated-2.0.25/.local/share/opencode/opencode.db"),
            &migrated_db,
        )?;
        let migrated_conn = Connection::open(&migrated_db)?;
        let legacy_before: i64 = migrated_conn.query_row("SELECT count(*) FROM message", [], |r| r.get(0))?;
        s.cwd = Some(migrated.join("project"));
        super::super::emit(&s, &migrated_db, &opts)?;
        assert_eq!(
            migrated_conn.query_row("SELECT count(*) FROM message", [], |r| r.get::<_, i64>(0))?,
            legacy_before
        );
        assert_eq!(
            migrated_conn.query_row(
                "SELECT count(*) FROM session_message WHERE session_id='ses_cv_native_v2'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            2
        );
        drop(migrated_conn);
        if std::env::var_os("CV_NATIVE_OPENCODE_OUT").is_some() {
            let fresh = root.join("fresh-v2");
            std::fs::create_dir_all(&fresh)?;
            s.cwd = Some(fresh.join("project"));
            super::super::emit(&s, &fresh, &opts)?;
        }
        if std::env::var_os("CV_NATIVE_OPENCODE_OUT").is_none() {
            drop(conn);
            std::fs::remove_dir_all(root)?;
        }
        Ok(())
    }
}
