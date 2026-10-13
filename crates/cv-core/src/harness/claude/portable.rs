//! Positioned transport and selected conversation fields. The native borrowed codec stays intact;
//! both representations use the same conversation finalizer and Anthropic block builders.
use super::super::store::{document::Fields, jsonl::ranges};
use super::*;
use crate::json::{InvalidJson, JsonView};
use ranges::Range;

/// Project retained metadata independently of content. Unknown fields and unrequested sidecars
/// remain source ranges rather than an owned record tree.
fn conversation(view: JsonView<'_>, opts: &ParseOptions) -> Result<Option<(Value, ConversationContent)>> {
    let mut keys = vec![
        "type",
        "message",
        "uuid",
        "parentUuid",
        "timestamp",
        "cwd",
        "gitBranch",
        "permissionMode",
        "isMeta",
        "isCompactSummary",
        "isApiErrorMessage",
        "scheduledTaskId",
        "scheduledFireId",
        "turnOrigin",
        "origin",
        "toolUseResult",
        "error",
        "errorDetails",
        "apiErrorStatus",
        "requestId",
    ];
    if opts.extra {
        for &key in CLAUDE_EXTRA_TOP {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    let Some(top) = Fields::parse(view, &keys)? else {
        return Ok(None);
    };
    let ty = top.string("type")?.unwrap_or_default();
    if !matches!(ty.as_str(), "user" | "assistant") {
        return Ok(None);
    }
    let Some(body) = top
        .get("message")
        .map(|v| {
            Fields::parse(
                v,
                &[
                    "model",
                    "content",
                    "usage",
                    "id",
                    "stop_reason",
                    "stop_sequence",
                    "stop_details",
                ],
            )
        })
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let mut meta = top.project(&[
        "type",
        "uuid",
        "parentUuid",
        "timestamp",
        "cwd",
        "gitBranch",
        "permissionMode",
        "isMeta",
        "isCompactSummary",
        "isApiErrorMessage",
        "turnOrigin",
    ])?;
    for key in ["scheduledTaskId", "scheduledFireId"] {
        if top.get(key).is_some() {
            meta.insert(key.into(), Value::Bool(true));
        }
    }
    if let Some(origin) = top
        .get("origin")
        .map(|v| Fields::parse(v, &["kind"]))
        .transpose()?
        .flatten()
    {
        meta.insert("origin".into(), Value::Object(origin.project(&["kind"])?));
    }
    if let Some(result) = top.get("toolUseResult") {
        if opts.extra {
            meta.insert("toolUseResult".into(), result.resolve()?.into_owned());
        } else if let Some(fields) = Fields::parse(result, &["agentId", "status"])? {
            let mut result = fields.project(&["status"])?;
            if fields.get("agentId").is_some() {
                result.insert("agentId".into(), Value::Bool(true));
            }
            meta.insert("toolUseResult".into(), Value::Object(result));
        }
    }
    let mut message = body.project(&["model", "usage"])?;
    let synthetic = ty == "assistant"
        && (meta.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
            || message.get("model").and_then(Value::as_str) == Some(SYNTHETIC_MODEL));
    if synthetic {
        for key in ["error", "errorDetails", "apiErrorStatus", "requestId"] {
            if let Some(value) = top.value(key)? {
                meta.insert(key.into(), value);
            }
        }
    }
    if opts.extra {
        meta.extend(top.project(CLAUDE_EXTRA_TOP)?);
        message.extend(body.project(CLAUDE_EXTRA_MSG)?);
    } else if opts.usage_identity {
        if let Some(id) = body.value("id")? {
            message.insert("id".into(), id);
        }
    }
    let origin = user_origin_from(
        top.get("scheduledTaskId").is_some()
            || top.get("scheduledFireId").is_some()
            || meta.get("turnOrigin").and_then(Value::as_str) == Some("scheduled"),
        meta.get("isMeta").and_then(Value::as_bool) == Some(true),
        meta.get("origin").and_then(|v| v.get("kind")).and_then(Value::as_str),
    );
    let mut blocks = Vec::new();
    let mut head = String::new();
    let mut user = user_kind_from_head(origin, "");
    let mut bare = false;
    if let Some(content) = body.get("content") {
        if content.is_string()? {
            bare = true;
            if ty == "user" {
                (head, user) = borrowed::classification_view(content, origin)?;
            }
            blocks.push(Block::Text {
                text: content.source_text()?.context("Claude content string")?,
            });
        } else if let Some(items) = content.array()? {
            for (index, item) in items.enumerate() {
                let item = item?;
                if index == 0 && ty == "user" {
                    if let Some(text) = item.get("text")? {
                        if text.is_string()? {
                            (head, user) = borrowed::classification_view(text, origin)?;
                        }
                    }
                }
                if let Some(block) = block(item)? {
                    blocks.push(block);
                }
            }
        }
    }
    meta.insert("message".into(), Value::Object(message));
    Ok(Some((
        Value::Object(meta),
        ConversationContent {
            blocks,
            user,
            slash_command: bare && (head.starts_with("<command-name>") || head.starts_with("<local-command-stdout>")),
        },
    )))
}

fn block(view: JsonView<'_>) -> Result<Option<Block>> {
    let Some(mut block) = anthropic::block_from_view(view, &anthropic::ANTHROPIC, true)? else {
        return Ok(None);
    };
    let field = match &block {
        Block::Text { .. } => Some("text"),
        Block::Thinking { redacted: false, .. } => Some("thinking"),
        Block::ToolResult { .. } => Some("content"),
        _ => None,
    };
    if let Some(field) = field {
        if let Some(value) = view.get(field)? {
            if value.is_string()? {
                let text = value.source_text()?.context("Claude projected string")?;
                match &mut block {
                    Block::Text { text: target } | Block::Thinking { text: target, .. } => *target = text,
                    Block::ToolResult { content, details, .. } => {
                        *content = text;
                        // The derived path/size demand needs the actual stub; ordinary giant output
                        // is never decoded into an owned string just to look for this prefix.
                        let prefix = borrowed::view_prefix(value, false)?;
                        if prefix.starts_with("<persisted-output>") {
                            let decoded = value.resolve()?;
                            *details = decoded
                                .as_str()
                                .and_then(persisted_output)
                                .map(|(path, size)| serde_json::json!({"persistedOutput":{"path":path,"size":size}}));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Block::ToolResult { content, details, .. } = &mut block {
        if details.is_none() {
            *details = content
                .inline_str()
                .and_then(persisted_output)
                .map(|(path, size)| serde_json::json!({"persistedOutput":{"path":path,"size":size}}));
        }
    }
    Ok(Some(block))
}

pub(super) fn stream(
    mut session: Session,
    file: &fs::File,
    inputs: Option<&JsonInputs>,
    offset: u64,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let source = match inputs {
        Some(inputs) => inputs.source.clone(),
        None => JsonSource::capture(session.source_path.as_deref().context("Claude source path")?)?,
    };
    source.check_metadata(file.metadata()?)?;
    let extent = source.len();
    if offset > extent {
        return Ok(session);
    }
    let mut published = false;
    let mut frames = ranges::records(
        file,
        Range {
            offset,
            len: extent - offset,
        },
    )?;
    let mut spawns = crate::affordance::AffordanceSink::new(Harness::Claude, sink);
    let sink = &mut spawns;
    frames.keep_small_records(64 * 1024);
    while let Some(row) = frames.next() {
        let (raw, complete) = match row {
            Ok(row) => row,
            Err(_) => break,
        };
        let range = if frames.small_record().is_some() {
            raw
        } else {
            trim_ascii(file, raw)?
        };
        let mut flow = Flow::Continue;
        if let Some(bytes) = frames.small_record() {
            flow = ingest_bytes(&mut session, bytes, raw.offset, opts, sink, inputs);
        } else if range.len == 0 {
            // A giant whitespace-only physical record has no JSON body.
        } else if opts.spans && !opts.complete {
            match source.read_range_value(range.offset, range.len, |view| conversation(view, opts)) {
                Ok(Some((meta, content))) => {
                    published |= content.blocks.iter().any(sourced);
                    if published {
                        if let Some(inputs) = inputs {
                            inputs.has_sources.set(true);
                        }
                    }
                    if let Some(mode) = meta.get("permissionMode").and_then(Value::as_str) {
                        crate::affordance::note_mode(session.harness_extra_mut(Harness::Claude), mode);
                    }
                    note_conversation_metadata(
                        &mut session,
                        meta.get("cwd").and_then(Value::as_str),
                        meta.get("gitBranch").and_then(Value::as_str),
                        meta.get("timestamp").and_then(Value::as_str).and_then(parse_ts),
                    );
                    let ty = meta.get("type").and_then(Value::as_str).unwrap_or("");
                    if let Some(message) = parse_message_projected(ty, &meta, opts, None, Some(content)) {
                        let ctx = SpanCtx {
                            slice: &[],
                            base_off: range.offset,
                            json_inputs: None,
                        };
                        flow = emit_conversation(&mut session, message, opts, sink, Some(&ctx));
                    }
                }
                Ok(None) => {
                    flow = fallback(&mut session, file, &source, range, opts, sink)?;
                }
                Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                    flow = fallback(&mut session, file, &source, range, opts, sink)?;
                }
                Err(error) => return Err(error),
            }
        } else {
            flow = fallback(&mut session, file, &source, range, opts, sink)?;
        }
        if published || inputs.is_some_and(|i| i.has_sources.get()) {
            source.check_metadata(file.metadata()?)?;
        }
        if flow == Flow::Stop {
            break;
        }
        let end = frames.offset();
        if complete && sink.wants_checkpoint(end) {
            let mut cp = Checkpoint::new(end);
            cp.put(ADAPTER_LAYER, &session);
            sink.checkpoint(&mut cp);
        }
    }
    if published {
        source.check_metadata(file.metadata()?)?;
    }
    Ok(session)
}

fn fallback(
    session: &mut Session,
    file: &fs::File,
    source: &JsonSource,
    range: Range,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Flow> {
    if opts.usage_identity {
        match source.read_range_value(range.offset, range.len, |_| Ok(())) {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => return Ok(Flow::Continue),
            Err(error) => return Err(error),
        }
    }
    let lean = if !opts.extra && !opts.complete {
        Some(lean::parse_reader(ranges::reader(file, range)?))
    } else {
        None
    };
    let value = match lean {
        Some(Ok(value)) => value,
        _ => match serde_json::from_reader(ranges::reader(file, range)?) {
            Ok(value) => value,
            Err(_) => return Ok(Flow::Continue),
        },
    };
    let ctx = SpanCtx {
        slice: &[],
        base_off: range.offset,
        json_inputs: None,
    };
    // These uncommon/full fidelity shapes require owned payloads; the transport still never
    // holds a duplicate raw physical line alongside them.
    Ok(ingest_value(session, &value, opts, sink, Some(&ctx)))
}

/// Claude's existing byte driver trims ASCII only; Unicode whitespace remains invalid JSON.
fn trim_ascii(file: &fs::File, mut range: Range) -> Result<Range> {
    let mut bytes = [0; 64 * 1024];
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset)?;
        let lead = bytes[..n].iter().take_while(|b| b.is_ascii_whitespace()).count();
        range.offset += lead as u64;
        range.len -= lead as u64;
        if lead != n {
            break;
        }
    }
    while range.len > 0 {
        let n = range.len.min(bytes.len() as u64) as usize;
        ranges::read_exact_at(file, &mut bytes[..n], range.offset + range.len - n as u64)?;
        let tail = bytes[..n].iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
        range.len -= tail as u64;
        if tail != n {
            break;
        }
    }
    Ok(range)
}

fn sourced(block: &Block) -> bool {
    match block {
        Block::Text { text } | Block::Thinking { text, .. } => matches!(text, Text::Source(_)),
        Block::ToolResult { content, .. } => matches!(content, Text::Source(_)),
        Block::ToolUse { input, .. } => matches!(input, Json::Source(_)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectSink;
    use serde_json::json;

    #[test]
    fn positioned_giant_conversations_match_existing_codec_and_metadata() {
        let world = cv_testkit::World::in_process("claude-positioned-conversation");
        let path = world.proj.join("source.jsonl");
        let body = "\n\"/\\🌙".repeat(16000);
        let notification = format!("\u{2003}<task-notification><output>{body}</output><summary>WOrkFLOW complete</summary></task-notification>");
        let records = [
            json!({"type":"assistant","uuid":"a","parentUuid":"p","timestamp":"2026-10-12T12:00:00Z","cwd":"/work","gitBranch":"main","permissionMode":"plan","message":{"model":"model","content":body,"usage":{"input_tokens":3},"id":"msg","stop_reason":"end_turn"}}),
            json!({"type":"user","uuid":"u","scheduledTaskId":null,"message":{"content":body}}),
            json!({"type":"user","isMeta":true,"message":{"content":notification}}),
            json!({"type":"user","message":{"content":[{"type":"unknown","text":notification},{"type":"text","text":body}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":body,"signature":"sig"},{"type":"tool_use","id":"call","name":"custom","input":{"body":body}}]}}),
            json!({"type":"user","toolUseResult":{"agentId":"agent","status":"finished","stdout":body},"message":{"content":[{"type":"tool_result","tool_use_id":"call","content":body,"is_error":true}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"text","text":format!("<persisted-output>\nOutput too large (66KB). Full output saved to: /tmp/output.txt\n\nPreview:\n{body}\n</persisted-output>") }]}]}}),
            json!({"type":"assistant","isApiErrorMessage":true,"errorDetails":{"body":body},"requestId":"request","message":{"model":"<synthetic>","content":[{"type":"text","text":body}]}}),
            json!({"type":"user","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":body}},{"type":"document","title":"pdf","source":{"type":"file","file_id":"file"}}]}}),
        ];
        let mut text = String::new();
        for (i, row) in records.iter().enumerate() {
            text.push_str(" \t");
            text.push_str(&row.to_string());
            if i + 1 < records.len() {
                text.push_str(" \r\n");
            }
        }
        fs::write(&path, &text).unwrap();
        for mut opts in [
            ParseOptions::lazy(),
            ParseOptions::lazy_extra(),
            ParseOptions::bulk(),
            ParseOptions::full(),
            ParseOptions::complete(),
        ] {
            opts.offsets = opts.spans;
            let inputs = JsonInputs::capture(&path, &opts).unwrap();
            let file = fs::File::open(&path).unwrap();
            let mut sink = CollectSink::default();
            let mut actual = stream(
                new_session_for("source", Some(path.clone()), &opts),
                &file,
                inputs.as_ref(),
                0,
                &opts,
                &mut sink,
            )
            .unwrap();
            actual.messages = sink.messages;
            if opts.spans {
                assert!(actual.messages.iter().flat_map(|m| &m.content).any(sourced), "{opts:?}");
            }
            actual.materialize().unwrap();
            let mut expected_sink = CollectSink::default();
            let mut expected = stream_reader_at(
                new_session_for("source", Some(path.clone()), &opts),
                std::io::Cursor::new(text.as_bytes()),
                0,
                &opts,
                &mut expected_sink,
                inputs.as_ref(),
            );
            expected.messages = expected_sink.messages;
            expected.materialize().unwrap();
            assert_eq!(
                serde_json::to_value(&actual).unwrap(),
                serde_json::to_value(&expected).unwrap(),
                "{opts:?}"
            );
        }
    }

    #[test]
    fn positioned_fallback_preserves_ignored_sidecar_acceptance_and_last_wins() {
        let world = cv_testkit::World::in_process("claude-positioned-fallback");
        let path = world.proj.join("source.jsonl");
        let body = "x".repeat(70000);
        let rows = [
            format!(r#"{{"type":"user","message":{{"content":"{body}"}},"toolUseResult":{{"ignored":"\ud800"}}}}"#),
            format!(r#"{{"type":"user","message":{{"content":"old","content":"{body}"}}}}"#),
            format!(
                r#"{{"type":"assistant","uuid":3,"message":{{"model":false,"content":[{{"type":"text","text":"{body}"}}]}}}}"#
            ),
            format!(r#"{{"type":"user","message":{{"content":"{body}"}},"ignored":"\ud800"}}"#),
            format!("\u{2003}{{\"type\":\"user\",\"message\":{{\"content\":\"{body}\"}}}}"),
        ];
        let text = rows.join("\n") + "\n";
        fs::write(&path, &text).unwrap();
        for opts in [
            ParseOptions::lazy(),
            ParseOptions::lazy_extra(),
            ParseOptions::usage(),
            ParseOptions::complete(),
        ] {
            let mut sink = CollectSink::default();
            let mut actual = stream(
                new_session_for("source", Some(path.clone()), &opts),
                &fs::File::open(&path).unwrap(),
                None,
                0,
                &opts,
                &mut sink,
            )
            .unwrap();
            actual.messages = sink.messages;
            actual.materialize().unwrap();
            let mut expected_sink = CollectSink::default();
            let mut expected = stream_reader_at(
                new_session_for("source", Some(path.clone()), &opts),
                std::io::Cursor::new(text.as_bytes()),
                0,
                &opts,
                &mut expected_sink,
                None,
            );
            expected.messages = expected_sink.messages;
            expected.materialize().unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{opts:?}"
            );
        }
    }
    #[test]
    fn positioned_text_checks_original_source_and_only_checkpoints_complete_rows() {
        struct Ends(Vec<u64>);
        impl MessageSink for Ends {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
            fn wants_checkpoint(&mut self, _: u64) -> bool {
                true
            }
            fn checkpoint(&mut self, cp: &mut Checkpoint) {
                self.0.push(cp.offset);
            }
        }
        let world = cv_testkit::World::in_process("claude-positioned-text-guard");
        let path = world.proj.join("source.jsonl");
        let row = json!({"type":"assistant","message":{"content":"🌙\n".repeat(30000)}}).to_string();
        let completed = row.len() as u64 + 1;
        fs::write(&path, format!("{row}\n{row}")).unwrap();
        let opts = ParseOptions::lazy();
        let mut ends = Ends(Vec::new());
        stream(
            new_session_for("source", Some(path.clone()), &opts),
            &fs::File::open(&path).unwrap(),
            None,
            0,
            &opts,
            &mut ends,
        )
        .unwrap();
        assert_eq!(ends.0, vec![completed]);
        let adapter = Claude::with_config_dir(&world.proj);
        let reference = SessionRef::new("source", Harness::Claude, path.clone());
        let mut captured = None;
        let mut remove = |message: Message| {
            let Block::Text { text } = &message.content[0] else {
                panic!("text")
            };
            assert!(matches!(text, Text::Source(_)));
            captured = Some(text.clone());
            fs::remove_file(&path).unwrap();
            Flow::Stop
        };
        assert!(adapter.stream(&reference, &opts, &mut remove).is_err());
        assert!(captured
            .unwrap()
            .try_resolve_prefix(&crate::lazy::Resolver::new(Some(path)), 32)
            .is_err());
    }
}
