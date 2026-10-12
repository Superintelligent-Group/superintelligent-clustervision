//! The Hermes emitter: an IR [`Session`] written into Hermes's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, Role, Session};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit into Hermes's `state.db` SQLite store (creating/appending). Mirrors the schema the Hermes
/// reader (`harness/hermes.rs`) expects: a `sessions` row + OpenAI-shaped `messages` rows
/// (role/content/tool_calls JSON/reasoning/timestamps as REAL unix secs). Multimodal content uses
/// the `\x00json:` sentinel prefix.
#[cfg(feature = "sqlite")]
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    use rusqlite::{params, Connection};

    const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

    let new_id = opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    // `out_dir` is a directory — the Hermes home or a profile dir, which is what the adapter's
    // first root names — and the store is its `state.db`.
    let db_path = out_dir.join("state.db");
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    // Create the (subset of) v14 schema if the DB is fresh. `IF NOT EXISTS` makes append safe.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT,
            cwd TEXT,
            git_branch TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
         );",
    )
    .context("creating hermes schema")?;

    // FTS5 virtual tables + sync triggers, mirroring hermes_state.py's FTS_SQL / FTS_TRIGRAM_SQL.
    // Without these, a port into a *fresh* state.db is invisible to Hermes's search (which queries
    // `messages_fts`). The unicode61 table is part of bundled FTS5 and always available; the trigram
    // tokenizer is too in modern SQLite, but we still guard it so emit succeeds on any build that
    // lacks it. Created BEFORE inserting rows so the AFTER INSERT triggers populate the index.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content);
         CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;",
    )
    .context("creating hermes messages_fts (FTS5)")?;

    // Trigram table for CJK/substring search. Guarded: if the trigram tokenizer isn't compiled in,
    // skip it (and its triggers) rather than failing the whole emit. The unicode61 table above is
    // enough for Hermes to find ASCII content; the trigram index is a CJK nicety.
    let trigram_sql = "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(content, tokenize='trigram');
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;";
    if let Err(e) = conn.execute_batch(trigram_sql) {
        eprintln!("cv: hermes trigram FTS unavailable, skipping ({e}); unicode61 FTS still active");
    }

    let started = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .timestamp_millis() as f64
        / 1000.0;
    let ended = session
        .updated_at
        .or(session.created_at)
        .map(|t| t.timestamp_millis() as f64 / 1000.0);

    // Format-complete: pull the captured per-session columns back out of the Hermes fact bag
    // (`extra["hermes"][SESSION_META_KEY]`, IR-v2 nesting) so the round-trip reproduces every
    // session-row value. Anything absent falls back to a sensible default (`source = "clustervision"`).
    use crate::harness::hermes::{RAW_REASONING_CONTENT_KEY, RAW_REASONING_KEY, SESSION_META_KEY};
    let smeta = session
        .harness_extra(Harness::Hermes)
        .and_then(|b| b.get(SESSION_META_KEY))
        .and_then(Value::as_object);
    let smeta_str =
        |k: &str| -> Option<String> { smeta.and_then(|m| m.get(k)).and_then(Value::as_str).map(str::to_string) };
    let smeta_int = |k: &str| -> i64 { smeta.and_then(|m| m.get(k)).and_then(Value::as_i64).unwrap_or(0) };
    let source = smeta_str("source").unwrap_or_else(|| "clustervision".to_string());
    let user_id = smeta_str("user_id");
    let model_config = smeta_str("model_config");
    // System prompt: the captured Hermes column (same-harness replay) else the first-class
    // `Session::system_prompt` (a cross-harness port carries it here into Hermes's own slot).
    let system_prompt = smeta_str("system_prompt").or_else(|| session.system_prompt.clone());
    let end_reason = smeta_str("end_reason");
    // First-class cwd/git → Hermes's own `cwd`/`git_branch` columns (v-recent schema), so a ported
    // session keeps its working dir (the reader probes these columns).
    let cwd = effective_cwd(session, opts).map(|p| p.to_string_lossy().into_owned());
    let git_branch = branch_of(session);

    conn.execute(
        "INSERT OR REPLACE INTO sessions \
         (id, source, user_id, model, model_config, system_prompt, started_at, ended_at, \
          end_reason, message_count, tool_call_count, input_tokens, output_tokens, \
          cache_read_tokens, cache_write_tokens, reasoning_tokens, title, cwd, git_branch) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            new_id,
            source,
            user_id,
            session.model,
            model_config,
            system_prompt,
            started,
            ended,
            end_reason,
            session.messages.len() as i64,
            smeta_int("tool_call_count"),
            smeta_int("input_tokens"),
            smeta_int("output_tokens"),
            smeta_int("cache_read_tokens"),
            smeta_int("cache_write_tokens"),
            smeta_int("reasoning_tokens"),
            session.title,
            cwd,
            git_branch,
        ],
    )
    .context("inserting hermes session")?;

    let mut base_ts = started;
    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|| {
                base_ts += 0.001;
                base_ts
            });

        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };

        // Reasoning columns. For a lossless round-trip we prefer the verbatim source columns the
        // parser stashed in the Hermes fact bag (`extra["hermes"]`, IR-v2 nesting: `reasoning` /
        // `reasoning_content` / `reasoning_details` / `codex_*`) over reconstructing from the folded
        // `Thinking.text` projection — that projection merges + dedups across columns and so can't
        // recover the originals. We synthesize from Thinking blocks only when no verbatim column was
        // captured (a session built in-memory or ported from another harness).
        let hbag = msg.harness_extra(Harness::Hermes);
        let extra_str =
            |k: &str| -> Option<String> { hbag.and_then(|b| b.get(k)).and_then(Value::as_str).map(str::to_string) };
        let extra_json = |k: &str| -> Option<String> { hbag.and_then(|b| b.get(k)).map(|v| v.to_string()) };

        let mut reasoning = extra_str(RAW_REASONING_KEY);
        let reasoning_content = extra_str(RAW_REASONING_CONTENT_KEY);
        // `reasoning_details` / codex items were stashed as parsed JSON; re-serialize them verbatim.
        let mut reasoning_details = extra_json("reasoning_details");
        let codex_reasoning_items = extra_json("codex_reasoning_items");
        let codex_message_items = extra_json("codex_message_items");

        // Fallback synthesis only when nothing verbatim was captured: a foreign/in-memory session
        // still gets its Thinking text + encrypted blob persisted (matching the prior behaviour).
        if reasoning.is_none() && reasoning_content.is_none() && reasoning_details.is_none() {
            let mut text = String::new();
            let mut enc: Option<&str> = None;
            for b in &msg.content {
                if let Block::Thinking { text: t, encrypted, .. } = b {
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                    if enc.is_none() {
                        enc = encrypted.as_deref();
                    }
                }
            }
            if !text.is_empty() {
                reasoning = Some(text);
            }
            if let Some(enc) = enc {
                reasoning_details =
                    Some(json!([{ "type": "reasoning.encrypted_content", "encrypted_content": enc }]).to_string());
            }
        }

        let finish_reason = extra_str("finish_reason");
        let platform_message_id = extra_str("platform_message_id");
        let observed: i64 = if hbag.and_then(|b| b.get("observed")) == Some(&Value::Bool(true)) {
            1
        } else {
            0
        };
        // token_count: the parser routes a row's single combined count to Usage (output for
        // assistant turns, input otherwise); reverse that mapping to recover the column.
        let token_count: Option<i64> = msg.usage.as_ref().and_then(|u| {
            let v = if msg.role == Role::Assistant {
                u.output_tokens
            } else {
                u.input_tokens
            };
            v.map(|n| n as i64)
        });

        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    tool_name,
                    ..
                } = b
                {
                    // `tool_name` matters: Hermes's own reader (and our adapter) surfaces it, so
                    // dropping it here made every ported tool turn come back nameless. The optional
                    // per-row columns (token_count / finish_reason / platform_message_id / observed)
                    // can also ride a tool row, so write them back for a lossless round-trip.
                    conn.execute(
                        "INSERT INTO messages \
                         (session_id, role, content, tool_call_id, tool_name, timestamp, \
                          token_count, finish_reason, platform_message_id, observed) \
                         VALUES (?1, 'tool', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            new_id,
                            content.to_string(),
                            tool_use_id,
                            tool_name,
                            ts,
                            token_count,
                            finish_reason,
                            platform_message_id,
                            observed,
                        ],
                    )
                    .context("inserting hermes tool message")?;
                }
            }
            continue;
        }

        // Content: plain text, or the multimodal sentinel form if there are images.
        let has_image = msg.content.iter().any(|b| matches!(b, Block::Image { .. }));
        let content: Option<String> = if has_image {
            let mut parts = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => parts.push(json!({ "type": "text", "text": text })),
                    Block::Image { data_ref, .. } => {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": data_ref.clone().unwrap_or_default() },
                        }));
                    }
                    _ => {}
                }
            }
            Some(format!(
                "{MULTIMODAL_SENTINEL}{}",
                serde_json::to_string(&Value::Array(parts))?
            ))
        } else {
            msg.text()
        };

        // Assistant tool calls → OpenAI-shaped tool_calls JSON.
        let tool_calls: Option<String> = {
            let mut calls = Vec::new();
            for block in &msg.content {
                let rewrapped = crate::affordance::rewrap_deferred(Harness::Hermes, block)?;
                let block = rewrapped.as_ref().unwrap_or(block);
                if let Block::ToolUse { id, name, input, .. } = block {
                    calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input.resolve()?.as_ref())?,
                        },
                    }));
                }
            }
            (!calls.is_empty()).then(|| Value::Array(calls).to_string())
        };

        conn.execute(
            "INSERT INTO messages \
             (session_id, role, content, tool_calls, timestamp, token_count, finish_reason, \
              reasoning, reasoning_content, reasoning_details, codex_reasoning_items, \
              codex_message_items, platform_message_id, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                new_id,
                role,
                content,
                tool_calls,
                ts,
                token_count,
                finish_reason,
                reasoning,
                reasoning_content,
                reasoning_details,
                codex_reasoning_items,
                codex_message_items,
                platform_message_id,
                observed,
            ],
        )
        .context("inserting hermes message")?;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("hermes --resume {new_id}")),
    })
}
