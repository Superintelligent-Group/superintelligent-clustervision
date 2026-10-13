// Frozen owned Cline/Roo writer for exact compatibility tests.
use super::super::*;
pub(super) fn emit(
    session: &Session,
    out_dir: &Path,
    opts: &crate::emit::EmitOptions,
    harness: Harness,
) -> Result<crate::harness::EmitResult> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let extension = match harness {
        Harness::Roo => "Roo Code",
        _ => "Cline",
    };
    // taskId: an explicit override, else a ms-epoch string the parser can turn back into created_at.
    let created = session.created_at.or(session.updated_at).unwrap_or_else(Utc::now);
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| created.timestamp_millis().to_string());

    let cwd = opts.new_cwd.clone().or_else(|| session.cwd.clone());
    let cwd_str = cwd.as_ref().map(|p| p.to_string_lossy().to_string());

    let task_dir = out_dir.join("tasks").join(&new_id);
    fs::create_dir_all(&task_dir).with_context(|| format!("creating {}", task_dir.display()))?;

    // ── api_conversation_history.json ──
    let mut history: Vec<Value> = Vec::new();
    let mut first_user_seen = false;
    for msg in &session.messages {
        match msg.role {
            Role::User => {
                // Thread the cwd through the first user turn's <environment_details>, mirroring
                // where the parser reads it back from.
                let inject = if !first_user_seen {
                    first_user_seen = true;
                    cwd_str.as_deref()
                } else {
                    None
                };
                let mut blocks = anthropic::user_blocks(&msg.content, &anthropic::ANTHROPIC)?;
                if let Some(cwd) = inject {
                    // Prepend a <task> title block (when we have one) so titles round-trip, then
                    // append the environment_details so cwd extraction finds it.
                    if let Some(title) = &session.title {
                        blocks.insert(0, json!({ "type": "text", "text": format!("<task>{title}</task>") }));
                    }
                    blocks.push(json!({
                        "type": "text",
                        "text": format!(
                            "<environment_details>\n# Current Working Directory ({cwd}) Files\n</environment_details>"
                        ),
                    }));
                }
                history.push(json!({ "role": "user", "content": Value::Array(blocks) }));
            }
            Role::Assistant => {
                history.push(json!({
                    "role": "assistant",
                    "content": Value::Array(anthropic::assistant_blocks(&msg.content, &anthropic::ANTHROPIC)?),
                }));
            }
            Role::Tool => {
                // A tool turn is a `user` message of only tool_result blocks; the parser
                // reclassifies it back to Role::Tool.
                history.push(json!({
                    "role": "user",
                    "content": Value::Array(anthropic::tool_result_blocks(&msg.content)?),
                }));
            }
            Role::System => {
                history.push(json!({
                    "role": "system",
                    "content": Value::Array(anthropic::assistant_blocks(&msg.content, &anthropic::ANTHROPIC)?),
                }));
            }
        }
    }
    let history_path = task_dir.join("api_conversation_history.json");
    fs::write(&history_path, serde_json::to_string_pretty(&Value::Array(history))?)
        .with_context(|| format!("writing {}", history_path.display()))?;

    // ── task_metadata.json (cwd + model hints; sidecar the parser falls back to for cwd) ──
    let mut meta = Map::new();
    if let Some(c) = &cwd_str {
        meta.insert("cwd".into(), json!(c));
    }
    meta.insert("files_in_context".into(), json!([]));
    if let Some(model) = &session.model {
        meta.insert("model_usage".into(), json!([{ "model_id": model }]));
    }
    let meta_path = task_dir.join("task_metadata.json");
    fs::write(&meta_path, serde_json::to_string_pretty(&Value::Object(meta))?)
        .with_context(|| format!("writing {}", meta_path.display()))?;

    // ── ui_messages.json (minimal event log carrying created/updated timestamps) ──
    let created_ms = created.timestamp_millis();
    let updated_ms = session
        .updated_at
        .or(session.created_at)
        .unwrap_or(created)
        .timestamp_millis();
    let mut ui: Vec<Value> = Vec::new();
    if let Some(title) = &session.title {
        ui.push(json!({ "ts": created_ms, "type": "say", "say": "task", "text": title }));
    } else {
        ui.push(json!({ "ts": created_ms, "type": "say", "say": "task", "text": "" }));
    }
    if updated_ms != created_ms {
        ui.push(json!({ "ts": updated_ms, "type": "say", "say": "completion_result", "text": "" }));
    }
    let ui_path = task_dir.join("ui_messages.json");
    fs::write(&ui_path, serde_json::to_string_pretty(&Value::Array(ui))?)
        .with_context(|| format!("writing {}", ui_path.display()))?;

    let resume = match &cwd {
        Some(c) => format!(
            "open {} in {extension} (task {new_id}, cwd {})",
            task_dir.display(),
            c.display()
        ),
        None => format!("open {} in {extension} (task {new_id})", task_dir.display()),
    };
    Ok(crate::harness::EmitResult {
        path: task_dir,
        new_id,
        resume_hint: Some(resume),
    })
}
