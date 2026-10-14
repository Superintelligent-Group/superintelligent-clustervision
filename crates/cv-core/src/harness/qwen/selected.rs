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
                .and_then(|mut values| values.next())
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
    replay(
        path,
        std::fs::File::open(path)?,
        session_shell(path, sink),
        ReplayState::default(),
        0,
        sink,
        discovery,
    )
}

pub(super) fn resume(
    path: &Path,
    file: std::fs::File,
    session: Session,
    state: ReplayState,
    at: u64,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    replay(path, file, session, state, at, sink, false)
}

fn replay(
    path: &Path,
    file: std::fs::File,
    mut session: Session,
    mut state: ReplayState,
    at: u64,
    sink: &mut dyn MessageSink,
    discovery: bool,
) -> Result<Session> {
    let source = JsonSource::capture(path)?;
    let parent = session.lineage.parent.is_some();
    let mut stopped = false;
    let prior_skipped = state.skipped;
    let skipped =
        super::super::store::checked_lines::for_each_record(&file, at, &source, |view, offset, complete, skipped| {
            if let Some(view) = view {
                if view.is_object()? {
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
                    if !stopped
                        && emit_record_with_content(&record, &mut session, &mut state, sink, Some(blocks)) == Flow::Stop
                    {
                        stopped = true;
                    }
                }
            }
            state.skipped = prior_skipped + skipped;
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
    super::super::note_skipped_lines(&mut session, prior_skipped + skipped);
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Kept {
        messages: Vec<Message>,
        checkpoints: Vec<(usize, Checkpoint)>,
    }
    impl MessageSink for Kept {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.push(message);
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
            self.checkpoints.push((self.messages.len(), checkpoint.clone()));
        }
    }

    #[test]
    fn checked_resume_keeps_selected_payloads_and_exact_skipped_checkpoint_state() {
        let world = cv_testkit::World::in_process("qwen_selected_resume");
        let home = world.home.join(".qwen");
        let dir = home.join("projects/hash");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let payload = "雪\n\"\\".repeat(20_000);
        let body = format!(
            "bad json\n{}\nbad suffix\n{}\n{}\n{}",
            serde_json::json!({"type":"user","uuid":"p","sessionId":"session","message":{"parts":[{"text":"start"}]}}),
            serde_json::json!({"type":"assistant","uuid":"a","model":"model","message":{"parts":[{"text":payload},{"functionCall":{"id":"call","name":"Read","args":{"payload":payload}}}]}}),
            serde_json::json!({"type":"tool_result","uuid":"r","message":{"parts":[{"functionResponse":{"id":"call","name":"Read","response":{"output":payload}}}]}}),
            serde_json::json!({"type":"user","uuid":"tail","message":{"parts":[{"text":"partial physical tail"}]}})
        );
        std::fs::write(&path, &body).unwrap();
        let adapter = Qwen::with_home(&home);
        let r = SessionRef::new("session", Harness::Qwen, &path);
        let mut full = Kept::default();
        let shell = adapter.stream(&r, &ParseOptions::full(), &mut full).unwrap();
        let (count, checkpoint) = full.checkpoints.iter().find(|(count, _)| *count == 1).unwrap();
        let mut resumed = Kept::default();
        let suffix = adapter
            .resume(&r, &ParseOptions::lazy(), &mut resumed, checkpoint)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(suffix).unwrap(),
            serde_json::to_value(shell).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&resumed.messages).unwrap(),
            serde_json::to_value(&full.messages[*count..]).unwrap()
        );
        assert!(resumed
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text: Text::Source(_) })));
        assert_eq!(
            resumed.checkpoints.iter().map(|(_, cp)| cp).collect::<Vec<_>>(),
            full.checkpoints
                .iter()
                .skip_while(|(_, cp)| cp.offset <= checkpoint.offset)
                .map(|(_, cp)| cp)
                .collect::<Vec<_>>()
        );
        std::fs::write(&path, format!("{body}\n")).unwrap();
        assert!(serde_json::to_value(&resumed.messages).is_err());
    }
}
