//! Selected flat JSONL reads retain checked payload ranges; complete/tree/custom rows keep their
//! existing owned policy. One captured descriptor/context serves the entire operation.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges};
use crate::json::{InvalidJson, JsonSource, JsonView};
use std::io::Read;

pub(super) fn has_leaf_controls(path: &Path) -> Result<bool> {
    let file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let extent = ranges::Range {
        offset: 0,
        len: file.metadata()?.len(),
    };
    for row in ranges::records(&file, extent)? {
        let (row, _) = row?;
        if !ranges::valid_utf8(&mut ranges::reader(&file, row)?)? {
            continue;
        }
        let mut reader = ranges::reader(&file, row)?;
        let needle = b"\"type\":\"leaf\"";
        let mut window = [0u8; 64 * 1024 + 12];
        let mut keep = 0;
        loop {
            let n = reader.read(&mut window[keep..])?;
            if n == 0 {
                break;
            }
            let end = keep + n;
            if memchr::memmem::find(&window[..end], needle).is_some() {
                return Ok(true);
            }
            keep = end.min(needle.len() - 1);
            window.copy_within(end - keep..end, 0);
        }
    }
    Ok(false)
}

pub(super) fn stream(
    r: &SessionRef,
    facts: &KeyFacts,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let source = JsonSource::capture(&r.path)?;
    let file = fs::File::open(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
    source.check_metadata(file.metadata()?)?;
    let result = (|| {
        #[cfg(feature = "mmap")]
        {
            let mapped = crate::harness::store::document::Source::map_file(&file)?;
            let mut offset = 0u64;
            let rows = mapped.bytes().split(|b| *b == b'\n').map(|bytes| {
                let at = offset;
                offset += bytes.len() as u64 + 1;
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return Ok(None);
                };
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                source.read_borrowed_range_value(
                    at + (trimmed.as_ptr() as usize - text.as_ptr() as usize) as u64,
                    trimmed,
                    |view| project(view, &source),
                )
            });
            consume(rows, r, facts, opts, sink)
        }
        #[cfg(not(feature = "mmap"))]
        {
            let context = crate::harness::store::document::range::RangeSource::from_captured(source.clone())?;
            let extent = ranges::Range {
                offset: 0,
                len: source.len(),
            };
            let rows = ranges::records(&file, extent)?.map(|row| {
                let range = ranges::trim(&file, row?.0)?;
                if range.len == 0 {
                    return Ok(None);
                }
                source.read_positioned_range_value(&context, range.offset, range.len, |view| project(view, &source))
            });
            consume(rows, r, facts, opts, sink)
        }
    })();
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    result
}

fn consume(
    rows: impl Iterator<Item = Result<Option<Row>>>,
    r: &SessionRef,
    facts: &KeyFacts,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let mut error = None;
    let rows = rows
        .scan(false, |failed, row| {
            if *failed {
                return None;
            }
            match row {
                Ok(row) => Some(row),
                Err(e) if e.is::<InvalidJson>() => Some(None),
                Err(e) => {
                    error = Some(e);
                    *failed = true;
                    None
                }
            }
        })
        .flatten();
    let session = stream_rows(rows, r, facts, opts, sink);
    if let Some(error) = error {
        return Err(error);
    }
    Ok(session)
}

fn project(view: JsonView<'_>, source: &JsonSource) -> Result<Option<Row>> {
    const TOP: &[&str] = &["type", "id", "parentId", "timestamp", "message"];
    let Some(top) = Fields::parse(view, TOP)? else {
        return Ok(Some(Row::active(view.resolve()?.into_owned())));
    };
    if top.string("type")?.as_deref() != Some("message") {
        return Ok(Some(Row::active(view.resolve()?.into_owned())));
    }
    const META: &[&str] = &[
        "role",
        "timestamp",
        "model",
        "provenance",
        "api",
        "provider",
        "responseModel",
        "responseId",
        "stopReason",
        "diagnostics",
        "usage",
        "errorMessage",
        "errorCode",
        "errorType",
        "toolCallId",
        "toolName",
        "isError",
        "details",
        "content",
    ];
    let Some(body) = top.get("message") else {
        return Ok(Some(Row::active(view.resolve()?.into_owned())));
    };
    let Some(body) = Fields::parse(body, META)? else {
        return Ok(Some(Row::active(view.resolve()?.into_owned())));
    };
    let role = body.string("role")?.unwrap_or_default();
    if !matches!(role.as_str(), "user" | "assistant" | "toolResult") {
        // System text is also demanded as owned Session.system_prompt; custom role composition
        // remains the established owned boundary.
        return Ok(Some(Row::active(view.resolve()?.into_owned())));
    }
    let content = body.get("content");
    let mut blocks = Vec::new();
    if role == "toolResult" {
        let mut skeleton = body.project(&META[..META.len() - 1])?;
        skeleton.insert("content".into(), Value::String(String::new()));
        let mut message = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
        parse_tool_result(&mut message, &Value::Object(skeleton));
        if let Block::ToolResult { content: target, .. } = &mut message.content[0] {
            *target = result_text(content, source)?;
        }
        blocks = message.content;
        if let Some(items) = content.map(|v| v.array()).transpose()?.flatten() {
            for item in items {
                let item = item?;
                if item.get("type")?.map(|v| v.resolve()).transpose()?.as_deref()
                    == Some(&Value::String("image".into()))
                {
                    if let Some(block) = projected_block(item)? {
                        blocks.push(block);
                    }
                }
            }
        }
    } else if let Some(content) = content {
        if role == "user" && content.is_string()? {
            blocks.push(Block::Text {
                text: content.source_text()?.unwrap(),
            });
        } else if let Some(items) = content.array()? {
            for item in items {
                if let Some(block) = projected_block(item?)? {
                    blocks.push(block);
                }
            }
        }
    }
    let mut skeleton = top.project(&TOP[..TOP.len() - 1])?;
    let mut body = body.project(&META[..META.len() - 1])?;
    // Reuse the original metadata/kind/timestamp logic without owning any payload. A tiny sentinel
    // keeps its empty-message filter until we restore the real blocks and repeat that filter.
    body.insert("content".into(), serde_json::json!([{"type":"text","text":""}]));
    skeleton.insert("message".into(), Value::Object(body));
    let v = Value::Object(skeleton);
    let mut messages = parse_entry(&v);
    for message in &mut messages {
        message.content = std::mem::take(&mut blocks);
    }
    messages.retain(|m| !m.content.is_empty() || !m.extra.is_empty());
    Ok(Some(Row {
        v,
        projected: Some(messages),
        active: true,
        parent: None,
    }))
}

fn projected_block(view: JsonView<'_>) -> Result<Option<Block>> {
    let mut block = anthropic::block_from_view(view, &anthropic::OPENCLAW, true)?;
    match &mut block {
        Some(Block::Text { text }) => {
            if let Some(value) = view.get("text")? {
                *text = value.source_text()?.unwrap();
            }
        }
        Some(Block::Thinking { text, .. }) => {
            if let Some(value) = view.get("thinking")? {
                *text = value.source_text()?.unwrap_or_default();
            }
        }
        Some(Block::ToolResult { content, .. }) => {
            if let Some(value) = view.get("content")? {
                if let Some(text) = value.source_text()? {
                    *content = text;
                }
            }
        }
        _ => {}
    }

    if let Some(Block::Thinking {
        redacted, encrypted, ..
    }) = &mut block
    {
        if view.get("redacted")?.map(|v| v.resolve()).transpose()?.as_deref() == Some(&Value::Bool(true)) {
            *redacted = true;
            *encrypted = Some("redacted".into());
        }
    }
    Ok(block)
}

fn result_text(content: Option<JsonView<'_>>, source: &JsonSource) -> Result<crate::lazy::Text> {
    let Some(content) = content else {
        return Ok(String::new().into());
    };
    if let Some(text) = content.source_text()? {
        return Ok(text);
    }
    let Some(items) = content.array()? else {
        return Ok(String::new().into());
    };
    let mut texts = Vec::new();
    for item in items {
        if let Some(text) = item?.get("text")? {
            if text.is_string()? {
                texts.push(text.to_json()?);
            }
        }
    }
    if texts.is_empty() {
        return Ok(String::new().into());
    }
    if texts.len() == 1 {
        return texts[0].with_view(|v| Ok(v.source_text()?.unwrap()));
    }
    crate::harness::store::text::append(source, &source.decoded_backing()?, |builder| {
        for (i, text) in texts.iter().enumerate() {
            if i > 0 {
                builder.push("\n")?;
            }
            text.with_view(|view| {
                builder.push_string(view)?;
                Ok(())
            })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect(adapter: &OpenClaw, r: &SessionRef, opts: &ParseOptions) -> Result<Session> {
        let mut sink = crate::stream::CollectSink::default();
        let mut session = adapter.stream(r, opts, &mut sink)?;
        session.messages = sink.messages;
        Ok(session)
    }

    fn compare(path: &Path) -> Result<()> {
        let r = SessionRef::new("selected", Harness::OpenClaw, path);
        let adapter = OpenClaw { roots: vec![] };
        // Complete + spans must keep the rich owned carrier policy; selected flat reads
        // must match the same parser after payload materialization.
        for complete in [false, true] {
            let mut options = if complete {
                ParseOptions::complete()
            } else {
                ParseOptions::default()
            };
            let mut owned = collect(&adapter, &r, &options)?;
            options.spans = true;
            let mut selected = collect(&adapter, &r, &options)?;
            owned.materialize()?;
            selected.materialize()?;
            assert_eq!(
                serde_json::to_value(owned)?,
                serde_json::to_value(selected)?,
                "complete={complete}"
            );
        }
        Ok(())
    }

    #[test]
    fn checked_openclaw_selected_matches_owned_payloads_metadata_and_corrupt_rows() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-selected");
        let path = world.base.join("source.jsonl");
        let content = json!([
            {"type":"text","text":"λ\n".repeat(1000)},
            {"type":"thinking","thinking":"reasoning 🌙".repeat(1000),"thinkingSignature":"sig","redacted":true},
            {"type":"thinking","thinking":false},
            {"type":"text","text":false},
            {"type":"toolCall","id":"call","name":"Bash","arguments":{"command":"echo λ","unknown":[null,1]}},
            {"type":"toolCall","id":"literal","name":"Literal","arguments":"{\"raw\":true}"},
            {"type":"image","mimeType":"image/png","data":"image-data"},
            {"type":"unknown","text":"ignored"}
        ]);
        let rows = [
            json!({"type":"session","version":4,"cwd":"/synthetic","timestamp":"2026-10-12T12:00:00Z"}),
            json!({"type":"session_info","name":"named"}),
            json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"bare λ".repeat(1000),"provenance":{"kind":"internal_system","sourceTool":"cron"}}}),
            json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant","content":content,"model":"m","api":"api","provider":"p","usage":{"input":3,"output":2,"cost":{"total":0.1}},"diagnostics":{"x":true},"stopReason":"error","errorMessage":"failed"}}),
            json!({"type":"message","id":"t","message":{"role":"toolResult","toolCallId":"call","toolName":"Bash","content":[{"text":""},{"type":"other","text":"λ".repeat(1000)},{"type":"image","mimeType":"image/png","data":"img"},{"text":"last"}],"details":{"native":true},"isError":true}}),
            json!({"type":"message","message":{"role":"assistant","content":[]}}),
            json!({"type":"message","message":{"role":"assistant","content":[],"provider":"empty-but-metadata"}}),
            json!({"type":"message","message":{"role":"system","content":"owned system metadata"}}),
            json!({"type":"compaction","id":"c","summary":"summary","tokensBefore":42}),
            json!({"type":"model_change","modelId":"new","provider":"p"}),
            json!({"type":"reset","reason":"reset"}),
            json!({"type":"message","message":{"role":"branchSummary","summary":"custom"}}),
        ];
        let mut bytes = Vec::new();
        for row in rows {
            bytes.extend_from_slice("\u{2003}".as_bytes());
            serde_json::to_writer(&mut bytes, &row)?;
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"{\"type\":\"leaf\",\"invalid\":\"\xff\"}\n");
        bytes.extend_from_slice(
            b"{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"bad\"},\"ignored\":\"\\ud800\"}\n",
        );
        bytes.extend_from_slice(
            b"{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"bad\"},\"ignored\":1e999}\n",
        );
        bytes.extend_from_slice(
            b"{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"first\",\"content\":\"last\"}}\n",
        );
        bytes.extend_from_slice(
            b"malformed\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"after corrupt\"}}",
        );
        fs::write(&path, bytes)?;
        assert!(!has_leaf_controls(&path)?);
        compare(&path)
    }

    #[test]
    fn checked_openclaw_selected_retained_source_and_stop_reject_mutation() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-selected-guard");
        let path = world.base.join("source.jsonl");
        fs::write(
            &path,
            serde_json::to_vec(&json!({"type":"message","message":{"role":"user","content":"x".repeat(20000)}}))?,
        )?;
        let r = SessionRef::new("selected", Harness::OpenClaw, &path);
        let adapter = OpenClaw { roots: vec![] };
        let opts = ParseOptions {
            spans: true,
            ..Default::default()
        };
        let session = collect(&adapter, &r, &opts)?;
        assert!(matches!(
            &session.messages[0].content[0],
            Block::Text {
                text: crate::Text::Source(_)
            }
        ));
        fs::write(&path, b"replacement")?;
        let mut session = session;
        assert!(session.materialize().is_err());
        fs::write(
            &path,
            serde_json::to_vec(&json!({"type":"message","message":{"role":"user","content":"x".repeat(20000)}}))?,
        )?;
        struct Mutate<'a>(&'a Path);
        impl MessageSink for Mutate<'_> {
            fn message(&mut self, _: Message) -> Flow {
                fs::write(self.0, b"replacement").unwrap();
                Flow::Stop
            }
        }
        let error = adapter.stream(&r, &opts, &mut Mutate(&path)).unwrap_err();
        assert!(format!("{error:#}").contains("source changed"));
        Ok(())
    }
}
