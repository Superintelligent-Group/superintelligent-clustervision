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
pub(super) fn stream(path: &Path, sink: &mut dyn MessageSink) -> Result<Session> {
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
                "systemPayload",
            ] {
                if let Some(value) = view.get(key)? {
                    record.insert(key.into(), value.resolve()?.into_owned());
                }
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
            if !stopped
                && emit_record_with_content(&record, &mut session, &mut state, sink, Some(content(view, &source)?))
                    == Flow::Stop
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
