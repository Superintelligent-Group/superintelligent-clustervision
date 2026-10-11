//! The OpenCode emitter: an IR [`Session`] written into OpenCode's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::EmitResult;
use crate::ir::Session;
use anyhow::Result;
use std::path::Path;
#[cfg(feature = "sqlite")]
use {
    crate::ir::{Block, Role},
    anyhow::Context,
    serde_json::{json, Map, Value},
    std::fs,
    std::path::PathBuf,
    uuid::Uuid,
};

#[cfg(feature = "sqlite")]
/// Where the `opencode.db` should live for a given `--out`/storage-root `out_dir`: the db file
/// itself if pointed at one, the data-dir sibling of a `storage/` root (the real install layout —
/// `~/.local/share/opencode/opencode.db` next to `storage/`), else `<out_dir>/opencode.db`.
fn opencode_db_path(out_dir: &Path) -> PathBuf {
    if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else if out_dir.file_name().is_some_and(|n| n == "storage") {
        out_dir
            .parent()
            .map(|p| p.join("opencode.db"))
            .unwrap_or_else(|| out_dir.join("opencode.db"))
    } else {
        out_dir.join("opencode.db")
    }
}

/// Emit into OpenCode's canonical SQLite store `opencode.db` (`session`/`message`/`part` tables;
/// schema per `~/pug/opencode/packages/core/src/session/sql.ts`). The dead JSON tree under
/// `storage/` is no longer read by OpenCode 1.18, so a session emitted there was unresumable — we
/// write the db instead. Each `data` column holds the Info/Part JSON *minus* `id`/`sessionID`/
/// `messageID` (the reader re-hydrates them). Tables are created when the db is absent; a `--out`
/// dir gets a fresh db. Tool results are folded onto the originating assistant's `tool` part
/// (`state.output`/`state.error`), since OpenCode has no standalone tool turn.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    #[cfg(not(feature = "sqlite"))]
    {
        let _ = (session, out_dir, opts);
        anyhow::bail!("emit to opencode requires the `sqlite` feature (opencode.db is SQLite)");
    }
    #[cfg(feature = "sqlite")]
    {
        emit_opencode_db(session, out_dir, opts)
    }
}

#[cfg(feature = "sqlite")]
fn emit_opencode_db(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::{params, Connection};

    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| format!("ses_{}", Uuid::now_v7().simple()));
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_id = if cwd_str.is_empty() {
        "prj_global".to_string()
    } else {
        format!("prj_{}", short_hash(&cwd_str))
    };

    let db_path = opencode_db_path(out_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    // FK enforcement stays off (rusqlite's default) so a fresh db needs no `project` row and an
    // append into a real store isn't rejected for the synthetic project_id. Create the reader's
    // schema when absent (`IF NOT EXISTS` no-ops against a real store; our INSERTs name only columns
    // both schemas share, so any extra real columns keep their defaults).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            model TEXT,
            cost REAL NOT NULL DEFAULT 0,
            tokens_input INTEGER NOT NULL DEFAULT 0,
            tokens_output INTEGER NOT NULL DEFAULT 0,
            tokens_reasoning INTEGER NOT NULL DEFAULT 0,
            tokens_cache_read INTEGER NOT NULL DEFAULT 0,
            tokens_cache_write INTEGER NOT NULL DEFAULT 0,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
         );",
    )
    .context("creating opencode schema")?;

    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));

    // Session row. `model` is `{id, providerID}` (the reader reads `model.id`); `title` is NOT NULL.
    let model_json = session
        .model
        .as_deref()
        .map(|m| json!({ "id": m, "providerID": "clustervision" }).to_string());
    conn.execute(
        "INSERT OR REPLACE INTO session \
         (id, project_id, slug, directory, title, version, model, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new_id,
            project_id,
            short_hash(&new_id),
            cwd_str,
            session.title.clone().unwrap_or_default(),
            env!("CARGO_PKG_VERSION"),
            model_json,
            created,
            updated,
        ],
    )
    .context("inserting opencode session")?;

    // Pre-index tool results by tool_use_id so each folds onto its assistant's `tool` part.
    let mut tool_results: std::collections::HashMap<String, &Block> = std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult { tool_use_id, .. } = b {
                    tool_results.insert(tool_use_id.clone(), b);
                }
            }
        }
    }

    let called = called_tool_ids(session);

    let base = created;
    let mut seq: u64 = 0;
    for msg in &session.messages {
        // Tool turns are folded into the assistant's tool parts; no standalone message. The
        // exception is a turn carrying an ORPHAN result ([`called_tool_ids`]): nothing folds it, so
        // it rides in a message of its own or it is lost outright.
        if msg.role == Role::Tool && !msg.content.iter().any(|b| is_orphan_result(b, &called)) {
            continue;
        }
        let role = match msg.role {
            Role::Assistant | Role::Tool => "assistant",
            Role::System => "system",
            _ => "user",
        };
        // Monotonic id + time so the reader's `ORDER BY time_created, id` == emission order.
        let mid = format!("msg_{seq:08}_{}", Uuid::now_v7().simple());
        let mts = msg.timestamp.map(|t| t.timestamp_millis()).unwrap_or(base + seq as i64);

        let mut mdata = Map::new();
        mdata.insert("role".into(), json!(role));
        mdata.insert("time".into(), json!({ "created": mts }));
        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
            mdata.insert("modelID".into(), json!(model));
        }
        // Usage → `tokens` + `cost` (the reader's `parse_usage` reads both off the message record).
        if let Some(u) = &msg.usage {
            let mut tokens = Map::new();
            tokens.insert("input".into(), json!(u.input_tokens.unwrap_or(0)));
            tokens.insert("output".into(), json!(u.output_tokens.unwrap_or(0)));
            tokens.insert("reasoning".into(), json!(u.reasoning_tokens.unwrap_or(0)));
            tokens.insert(
                "cache".into(),
                json!({ "read": u.cache_read_tokens.unwrap_or(0), "write": u.cache_creation_tokens.unwrap_or(0) }),
            );
            mdata.insert("tokens".into(), Value::Object(tokens));
            if let Some(c) = u.cost_usd {
                mdata.insert("cost".into(), json!(c));
            }
        }
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![mid, new_id, mts, mts, Value::Object(mdata).to_string()],
        )
        .context("inserting opencode message")?;

        let mut pidx: u64 = 0;
        let mut write_part = |part: Value| -> Result<()> {
            let pid = format!("prt_{seq:08}_{pidx:04}_{}", Uuid::now_v7().simple());
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![pid, mid, new_id, mts, mts, part.to_string()],
            )
            .context("inserting opencode part")?;
            pidx += 1;
            Ok(())
        };

        for b in &msg.content {
            match b {
                Block::Text { text } => write_part(json!({ "type": "text", "text": text }))?,
                Block::Thinking { text, signature, .. } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("reasoning"));
                    p.insert("text".into(), json!(text));
                    if let Some(sig) = signature {
                        p.insert("metadata".into(), json!({ "anthropic": { "signature": sig } }));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolUse { id, name, input, .. } => {
                    let state = opencode_tool_state(Some(input), tool_results.get(id).copied());
                    write_part(json!({
                        "type": "tool",
                        "callID": id,
                        "tool": name,
                        "state": Value::Object(state),
                    }))?;
                }
                Block::Image { media_type, data_ref } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = media_type {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(d) = data_ref {
                        p.insert("url".into(), json!(d));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::File { mime, path, source } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = mime {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(url) = source.as_deref().or(path.as_deref()) {
                        p.insert("url".into(), json!(url));
                    }
                    write_part(Value::Object(p))?;
                }
                // An ORPHAN result gets a `tool` part of its own — the output, with no `input`,
                // because the call it answers is not in this session. A result that HAS its call
                // here was already folded onto that call's part above, so it is skipped.
                Block::ToolResult {
                    tool_use_id, tool_name, ..
                } if !called.contains(tool_use_id.as_str()) => {
                    write_part(json!({
                        "type": "tool",
                        "callID": tool_use_id,
                        "tool": tool_name.clone().unwrap_or_default(),
                        "state": Value::Object(opencode_tool_state(None, Some(b))),
                    }))?;
                }
                Block::ToolResult { .. } => {}
            }
        }
        seq += 1;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("opencode --session {new_id}")),
    })
}

/// The `state` object of an OpenCode `tool` part: the call's `input`, plus — when the session
/// paired one with it — the result's status, output-or-error and structured `details`.
///
/// `input` is `None` for an **orphan** result (see [`emit_opencode_db`]): the call itself is not in
/// this session, so there are no arguments to record, only what came back.
#[cfg(feature = "sqlite")]
fn opencode_tool_state(input: Option<&Value>, result: Option<&Block>) -> Map<String, Value> {
    let mut state = Map::new();
    if let Some(input) = input {
        state.insert("input".into(), input.clone());
    }
    let Some(Block::ToolResult {
        content,
        is_error,
        details,
        ..
    }) = result
    else {
        state.insert("status".into(), json!("completed"));
        return state;
    };
    if *is_error {
        state.insert("status".into(), json!("error"));
        state.insert("error".into(), json!(content));
    } else {
        state.insert("status".into(), json!("completed"));
        state.insert("output".into(), json!(content));
    }
    // `details` was read from `state.{title,metadata,time}`; fold it back so an OpenCode→OpenCode
    // round-trip reproduces the result's structured details. A FOREIGN details (Claude's
    // `toolUseResult`, Kimi's note/truncation) has none of those three keys, and dropping it read
    // back as `tool result details: 194 → 0`. Its keys ride in `metadata`, OpenCode's own free-form
    // slot for a tool's structured extras — which is exactly where the adapter reads them back from.
    if let Some(d) = details {
        let mut meta = d
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        match d {
            Value::Object(d) => {
                for (k, v) in d {
                    match k.as_str() {
                        "title" | "time" => {
                            state.insert(k.clone(), v.clone());
                        }
                        "metadata" if v.is_object() => {}
                        _ => {
                            meta.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            // A scalar sidecar (no key to hang it on) still belongs in metadata.
            other => {
                meta.insert("details".into(), other.clone());
            }
        }
        if !meta.is_empty() {
            state.insert("metadata".into(), Value::Object(meta));
        }
    }
    state
}
