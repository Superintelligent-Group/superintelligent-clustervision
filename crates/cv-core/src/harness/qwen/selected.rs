//! ChatRecord payload selection. System bookkeeping keeps the existing owned metadata contract.
use super::*;
use crate::{
    json::{JsonSource, JsonView},
    lazy::{Resolver, Text},
};

pub(super) fn prefix(text: &Text, trim: bool) -> Result<String> {
    Ok(super::super::store::document::text_head(text, &Resolver::new(None), 80, trim)?.0)
}

fn string(view: JsonView<'_>, key: &str) -> Result<Option<String>> {
    Ok(view
        .get(key)?
        .map(|v| v.resolve())
        .transpose()?
        .and_then(|v| v.as_str().map(str::to_string)))
}
fn response(view: Option<JsonView<'_>>, source: &JsonSource) -> Result<Text> {
    let Some(view) = view else { return Ok(Text::default()) };
    let payload = if view.is_object()? {
        view.get("output")?.or(view.get("error")?).unwrap_or(view)
    } else {
        view
    };
    if let Some(text) = payload.source_text()? {
        return Ok(text);
    }
    crate::harness::store::text::append(source, &source.decoded_backing()?, |builder| builder.push_json(payload))
}
fn content(view: JsonView<'_>, source: &JsonSource) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    let parts = view
        .get("message")?
        .map(|v| v.get("parts"))
        .transpose()?
        .flatten()
        .map(|v| v.array())
        .transpose()?
        .flatten();
    for part in parts.into_iter().flatten() {
        let part = part?;
        if let Some(call) = part.get("functionCall")? {
            let name = string(call, "name")?.unwrap_or_else(|| "tool".into());
            let id = string(call, "id")?.unwrap_or_else(|| name.clone());
            let args = call.get("args")?;
            let inner = if name == TOOL_CALL {
                args.map(|v| string(v, "name")).transpose()?.flatten()
            } else {
                None
            };
            let (name, namespace, input) = if let Some(inner) = inner {
                (
                    inner,
                    Some(TOOL_CALL.into()),
                    args.map(|v| v.get("arguments")).transpose()?.flatten(),
                )
            } else {
                (name, None, args)
            };
            out.push(Block::ToolUse {
                id,
                name,
                namespace,
                input: input
                    .map(|v| v.to_json())
                    .transpose()?
                    .unwrap_or_else(|| Value::Null.into()),
            });
        } else if let Some(result) = part.get("functionResponse")? {
            let name = string(result, "name")?.unwrap_or_else(|| "tool".into());
            let resp = result.get("response")?;
            out.push(Block::ToolResult {
                tool_use_id: string(result, "id")?.unwrap_or_else(|| name.clone()),
                content: response(resp, source)?,
                is_error: resp.map(|v| v.get("error")).transpose()?.flatten().is_some(),
                tool_name: Some(name),
                status: None,
                details: None,
            });
        } else if let Some(inline) = part.get("inlineData")? {
            out.push(Block::Image {
                media_type: string(inline, "mimeType")?,
                data_ref: None,
            });
        } else if let Some(file) = part.get("fileData")? {
            out.push(Block::File {
                mime: string(file, "mimeType")?,
                path: None,
                source: string(file, "fileUri")?,
            });
        } else if let Some(text) = part.get("text")?.map(|v| v.source_text()).transpose()?.flatten() {
            if !super::super::store::document::text_head(&text, &Resolver::new(None), 1, false)?.1 {
                continue;
            }
            if part.get("thought")?.map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
                out.push(Block::Thinking {
                    text,
                    signature: string(part, "thoughtSignature")?,
                    encrypted: None,
                    redacted: false,
                });
            } else {
                out.push(Block::Text { text });
            }
        }
    }
    Ok(out)
}
fn discovery_payload(payload: JsonView<'_>, subtype: Option<&str>) -> Result<Value> {
    let mut small = Map::new();
    match subtype {
        Some("custom_title" | "parent_session" | "session_model") => {
            for key in [
                "title",
                "customTitle",
                "parentSessionId",
                "sessionId",
                "model",
                "modelId",
            ] {
                if let Some(value) = payload.get(key)?.filter(|v| v.is_string().unwrap_or(false)) {
                    small.insert(key.into(), value.resolve()?.into_owned());
                }
            }
        }
        Some("chat_compression") => {
            let first = payload
                .get("compressedHistory")?
                .map(|v| v.array())
                .transpose()?
                .flatten()
                .map(|mut values| values.next())
                .flatten()
                .transpose()?;
            let parts = first
                .map(|v| v.get("parts"))
                .transpose()?
                .flatten()
                .map(|v| v.array())
                .transpose()?
                .flatten();
            let mut texts = 0;
            let mut present = false;
            for part in parts.into_iter().flatten() {
                if let Some(text) = part?.get("text")?.map(|v| v.source_text()).transpose()?.flatten() {
                    texts += 1;
                    present |= super::super::store::document::text_head(&text, &Resolver::new(None), 1, false)?.1;
                }
            }
            // Empty text parts still contribute joining newlines to the old summary.
            if present || texts > 1 {
                small.insert(
                    "compressedHistory".into(),
                    serde_json::json!([{"parts":[{"text":"present"}]}]),
                );
            }
        }
        Some("goal_state") => {
            let objective = payload
                .get("snapshot")?
                .map(|v| v.get("goal"))
                .transpose()?
                .flatten()
                .map(|v| v.get("objective"))
                .transpose()?
                .flatten();
            if objective.map(|v| v.is_string()).transpose()?.unwrap_or(false) {
                small.insert("snapshot".into(), serde_json::json!({"goal":{"objective":""}}));
            }
        }
        Some("turn_result") => {
            if payload
                .get("error")?
                .map(|v| v.is_null())
                .transpose()?
                .is_some_and(|null| !null)
            {
                small.insert("error".into(), Value::Bool(true));
            }
        }
        Some("slash_command" | "at_command" | "notification" | "background_task_completed") => {
            for key in ["rawCommand", "userText", "text", "message", "summary"] {
                if let Some(text) = payload.get(key)?.map(|v| v.source_text()).transpose()?.flatten() {
                    small.insert(
                        key.into(),
                        Value::String(
                            super::super::store::document::text_head(&text, &Resolver::new(None), 1, false)?.0,
                        ),
                    );
                    break;
                }
            }
        }
        _ => {}
    }
    Ok(Value::Object(small))
}

pub(super) fn stream(path: &Path, sink: &mut dyn MessageSink) -> Result<Session> {
    stream_with_projection(path, sink, false)
}
/// Discovery needs exact replay metadata and message presence, never system display bodies.
pub(super) fn stream_for_discovery(path: &Path, sink: &mut dyn MessageSink) -> Result<Session> {
    stream_with_projection(path, sink, true)
}
fn stream_with_projection(path: &Path, sink: &mut dyn MessageSink, discovery: bool) -> Result<Session> {
    let source = JsonSource::capture(path)?;
    let mut session = session_shell(path, sink);
    let parent = session.lineage.parent.is_some();
    let mut state = ReplayState::default();
    let mut stopped = false;
    let file = std::fs::File::open(path)?;
    let skipped =
        super::super::store::checked_lines::for_each(&file, 0, &source, |view, offset, complete, skipped| {
            if !view.is_object()? {
                return Ok(Flow::Continue);
            }
            let mut record = Map::new();
            for key in [
                "type",
                "subtype",
                "provenance",
                "uuid",
                "parentUuid",
                "sessionId",
                "timestamp",
                "cwd",
                "gitBranch",
                "version",
                "isSidechain",
                "model",
                "usageMetadata",
            ] {
                if discovery && key == "usageMetadata" {
                    continue;
                }
                if let Some(value) = view.get(key)? {
                    record.insert(key.into(), value.resolve()?.into_owned());
                }
            }
            if let Some(payload) = view.get("systemPayload")? {
                let payload = if discovery {
                    discovery_payload(payload, record.get("subtype").and_then(Value::as_str))?
                } else {
                    payload.resolve()?.into_owned()
                };
                record.insert("systemPayload".into(), payload);
            }
            for (key, selected) in [
                ("executionContext", "approvalMode"),
                ("forkedFrom", "sessionId"),
                ("toolCallResult", "status"),
            ] {
                if let Some(value) = view.get(key)?.map(|v| v.get(selected)).transpose()?.flatten() {
                    record.insert(key.into(), serde_json::json!({selected: value.resolve()?.into_owned()}));
                }
            }
            session_facts(&mut session, &record, parent);
            let blocks = if matches!(
                record.get("type").and_then(Value::as_str),
                Some("user" | "assistant" | "tool_result")
            ) {
                content(view, &source)?
            } else {
                Vec::new()
            };
            if !stopped && emit_record_with_content(&record, &mut session, &mut state, sink, Some(blocks)) == Flow::Stop
            {
                stopped = true;
            }
            state.skipped = skipped;
            if !stopped && complete && sink.wants_checkpoint(offset) {
                let mut checkpoint = Checkpoint::new(offset);
                checkpoint.put(
                    ADAPTER_LAYER,
                    &ReplayCheckpoint {
                        session: &session,
                        replay: &state,
                    },
                );
                sink.checkpoint(&mut checkpoint);
            }
            // Like the owned replay, continue through EOF for final session facts after sink stop.
            Ok(Flow::Continue)
        })?;
    session.model = state.session_model.take();
    session.title = state.title.take().or(state.first_prompt.take());
    super::super::note_skipped_lines(&mut session, skipped);
    Ok(session)
}
