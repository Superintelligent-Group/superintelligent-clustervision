//! Selected partial reads of modern IDE composers; rich reads keep their existing fidelity path.
use super::*;
use crate::harness::store::{bytes::ByteSource, document::Fields};
use crate::json::{Json, JsonSource, JsonView};
use crate::lazy::Text;
use std::sync::Arc;

#[cfg(test)]
pub(super) fn global(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    global_with_reads(conn, r, &crate::source::ReadSet::default(), sink)
}

pub(super) fn global_with_reads(
    conn: &Connection,
    r: &SessionRef,
    original_reads: &crate::source::ReadSet,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let mut wal = r.path.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    sink.dependency(&wal);
    let mut reads = original_reads.clone();
    reads.record(&wal);
    let captured = JsonSource::capture_with_reads(&r.path, &reads)?;
    let arena = Arc::new(ByteSource::arena()?);
    let result = (|| {
        let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
        let (offset, length) = source::append_cell(conn, "cursorDiskKV", &format!("composerData:{}", r.id), &arena)?;
        captured.read_backing_value(arena.clone(), offset, length, |root| {
            let fields = Fields::parse(
                root,
                &[
                    "name",
                    "createdAt",
                    "lastUpdatedAt",
                    "subagentInfo",
                    "unifiedMode",
                    "modelConfig",
                    "fullConversationHeadersOnly",
                    "conversation",
                ],
            )?;
            let mut metadata = Map::new();
            if let Some(fields) = &fields {
                if let Some(name) = fields.string("name")? {
                    metadata.insert("name".into(), name.into());
                }
                for key in ["createdAt", "lastUpdatedAt", "unifiedMode"] {
                    if let Some(value) = fields.get(key) {
                        let value = if key != "unifiedMode" && value.is_string()? {
                            Value::Null
                        } else {
                            scalar(value)?
                        };
                        metadata.insert(key.into(), value);
                    }
                }
                for (key, names) in [
                    (
                        "subagentInfo",
                        &["parentComposerId", "toolCallId", "subagentTypeName"][..],
                    ),
                    ("modelConfig", &["modelName"][..]),
                ] {
                    if let Some(value) = fields.get(key) {
                        if let Some(selected) = Fields::parse(value, names)? {
                            let mut object = Map::new();
                            for name in names {
                                if let Some(value) = selected.string(name)? {
                                    object.insert((*name).into(), value.into());
                                }
                            }
                            metadata.insert(key.into(), Value::Object(object));
                        }
                    }
                }
            }
            let metadata = Value::Object(metadata);
            let mut session = global_session(&metadata, r);
            let headers = fields
                .as_ref()
                .and_then(|f| f.get("fullConversationHeadersOnly"))
                .map(|v| v.array())
                .transpose()?
                .flatten();
            let external = headers.is_some();
            let conversation = if external {
                headers
            } else {
                fields
                    .as_ref()
                    .and_then(|f| f.get("conversation"))
                    .map(|v| v.array())
                    .transpose()?
                    .flatten()
            };
            let mut last_model = None;
            if let Some(conversation) = conversation {
                for row in conversation {
                    let view = row?;
                    let message = if external {
                        let Some(id) = view.get("bubbleId")?.map(string).transpose()?.flatten() else {
                            continue;
                        };
                        let key = format!("bubbleId:{}:{id}", r.id);
                        let Ok((offset, length)) = source::append_cell(conn, "cursorDiskKV", &key, &arena) else {
                            continue;
                        };
                        if length == 0 {
                            continue;
                        }
                        // Existing IDE reads skip missing or malformed external bubbles. The final
                        // compound guard still rejects source mutations even when a bubble is skipped.
                        match captured
                            .read_backing_value(arena.clone(), offset, length, |view| bubble(view, &captured, &arena))
                        {
                            Ok(message) => message,
                            Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => None,
                            Err(error) => return Err(error),
                        }
                    } else {
                        bubble(view, &captured, &arena)?
                    };
                    if let Some(message) = message {
                        if message.model.is_some() {
                            last_model = message.model.clone();
                        }
                        if sink.message(message) == Flow::Stop {
                            break;
                        }
                    }
                }
            }
            session.model = last_model.or_else(|| {
                metadata
                    .pointer("/modelConfig/modelName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            });
            Ok(session)
        })
    })();
    captured.check()?;
    result
}
fn string(view: JsonView<'_>) -> Result<Option<String>> {
    Ok(view.text(false)?.and_then(|text| match text {
        Text::Inline(text) => Some(text),
        _ => None,
    }))
}
fn nonempty(text: &Text) -> bool {
    match text {
        Text::Inline(text) => !text.is_empty(),
        Text::Source(text) => text.raw_len() != 0,
        _ => true,
    }
}
fn scalar(view: JsonView<'_>) -> Result<Value> {
    if view.is_object()? || view.is_array()? {
        return Ok(Value::Null);
    }
    Ok(view.resolve()?.into_owned())
}
fn decoded_argument(view: JsonView<'_>, captured: &JsonSource, arena: &Arc<ByteSource>) -> Result<Option<Json>> {
    if !view.is_string()? {
        return Ok(None);
    }
    let (offset, length) = arena.append(|writer| {
        view.for_each_str_chunk(|part| {
            // append's shared writer supplies independent bounded scratch for mapped input.
            writer.write_all(part.as_bytes())?;
            Ok(())
        })?;
        Ok(())
    })?;
    // Cursor accepts any valid JSON scalar/array/object here, unlike OpenAI's object-only
    // encoded arguments. Invalid rawArgs falls through to params; invalid params stays a string.
    if length == 0 {
        return Ok(None);
    }
    let result = captured.read_backing_value(arena.clone(), offset, length, |view| view.argument(true));
    captured.check()?;
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => Ok(None),
        Err(error) => Err(error),
    }
}
fn lexical(view: JsonView<'_>, captured: &JsonSource, arena: &Arc<ByteSource>) -> Result<Option<Text>> {
    fn walk(node: JsonView<'_>, out: &mut crate::harness::store::text::Builder<'_>) -> Result<bool> {
        let Some(fields) = Fields::parse(node, &["text", "children"])? else {
            return Ok(false);
        };
        if let Some(text) = fields.get("text") {
            out.push_string(text)?;
        }
        let children = fields.get("children");
        if let Some(array) = children.map(|v| v.array()).transpose()?.flatten() {
            let mut array = array.peekable();
            while let Some(child) = array.next() {
                let has_children = walk(child?, out)?;
                if array.peek().is_some() && has_children {
                    out.push("\n")?;
                }
            }
        }
        Ok(children.is_some())
    }
    #[derive(Debug)]
    struct Nonblank;
    impl std::fmt::Display for Nonblank {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("nonblank text found")
        }
    }
    impl std::error::Error for Nonblank {}
    let build = |view: JsonView<'_>| -> Result<Option<Text>> {
        let Some(root) = view.get("root")? else { return Ok(None) };
        let text = crate::harness::store::text::append(captured, arena, |out| walk(root, out).map(|_| ()))?;
        // The quoted builder output is complete and validated. Stop at the first nonblank
        // chunk; checked demands still give source mutations precedence over this sentinel.
        let check = text.try_for_each_chunk(&crate::lazy::Resolver::new(None), 1024, |part| {
            if part.chars().any(|c| !c.is_whitespace()) {
                return Err(anyhow::Error::new(Nonblank));
            }
            Ok(())
        });
        let nonblank = match check {
            Ok(()) => false,
            Err(error) if error.downcast_ref::<Nonblank>().is_some() => true,
            Err(error) => return Err(error),
        };
        Ok(nonblank.then_some(text))
    };
    if !view.is_string()? {
        return build(view);
    }
    let (offset, length) = arena.append(|out| {
        view.for_each_str_chunk(|part| {
            std::io::Write::write_all(out, part.as_bytes())?;
            Ok(())
        })?;
        Ok(())
    })?;
    // An empty encoded editor state is malformed JSON, so the owned reader ignores it.
    // It has no range to validate; do not turn this fallback into a backing-bounds error.
    if length == 0 {
        return Ok(None);
    }
    match captured.read_backing_value(arena.clone(), offset, length, build) {
        Ok(text) => Ok(text),
        Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => Ok(None),
        Err(error) => Err(error),
    }
}
fn result_text(view: JsonView<'_>, captured: &JsonSource, arena: &Arc<ByteSource>) -> Result<Option<Text>> {
    if view.is_null()? {
        return Ok(None);
    }
    Ok(Some(match view.text(true)? {
        Some(text) => text,
        None => crate::harness::store::text::append(captured, arena, |out| out.push_json(view))?,
    }))
}
fn bubble(view: JsonView<'_>, captured: &JsonSource, arena: &Arc<ByteSource>) -> Result<Option<Message>> {
    let Some(fields) = Fields::parse(
        view,
        &[
            "type",
            "bubbleId",
            "thinking",
            "text",
            "richText",
            "toolFormerData",
            "modelInfo",
            "modelType",
            "model",
            "tokenCount",
            "unifiedMode",
        ],
    )?
    else {
        return Ok(None);
    };
    let role = if fields
        .get("type")
        .map(|v| scalar(v).map(|v| v.as_i64() == Some(1)))
        .transpose()?
        == Some(true)
    {
        Role::User
    } else {
        Role::Assistant
    };
    let mut message = Message::new(role);
    message.id = fields.string("bubbleId")?;
    if let Some(thinking) = fields
        .get("thinking")
        .map(|v| Fields::parse(v, &["text", "signature"]))
        .transpose()?
        .flatten()
    {
        let text = thinking
            .get("text")
            .map(|v| v.text(true))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let signature = thinking.string("signature")?.filter(|s| !s.is_empty());
        if nonempty(&text) || signature.is_some() {
            message.content.push(Block::Thinking {
                text,
                signature,
                encrypted: None,
                redacted: false,
            });
        }
    }
    if let Some(text) = fields
        .get("text")
        .map(|v| v.text(true))
        .transpose()?
        .flatten()
        .filter(nonempty)
    {
        message.content.push(Block::Text { text });
    } else if let Some(rich) = fields.get("richText") {
        if let Some(text) = lexical(rich, captured, arena)? {
            message.content.push(Block::Text { text });
        }
    }
    if let Some(tool) = fields.get("toolFormerData") {
        // The old adapter emits a tool call even for a non-object toolFormerData.
        let tf = Fields::parse(
            tool,
            &[
                "name",
                "toolCallId",
                "modelCallId",
                "rawArgs",
                "params",
                "status",
                "error",
                "result",
            ],
        )?;
        let name = tf
            .as_ref()
            .map(|f| f.string("name"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| "tool".into());
        let id = tf
            .as_ref()
            .map(|f| f.string("toolCallId"))
            .transpose()?
            .flatten()
            .or(tf.as_ref().map(|f| f.string("modelCallId")).transpose()?.flatten())
            .unwrap_or_default();
        let mut input = None;
        if let Some(raw) = tf.as_ref().and_then(|f| f.get("rawArgs")) {
            input = decoded_argument(raw, captured, arena)?;
        }
        if input.is_none() {
            if let Some(params) = tf.as_ref().and_then(|f| f.get("params")) {
                input = decoded_argument(params, captured, arena)?.or(Some(params.argument(true)?));
            }
        }
        message.content.push(Block::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.unwrap_or_else(|| Value::Null.into()),
            namespace: None,
        });
        let status = tf
            .as_ref()
            .map(|f| f.string("status"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let error = tf.as_ref().and_then(|f| f.get("error"));
        let is_error = status.eq_ignore_ascii_case("error") || error.map(|v| v.is_null()).transpose()? == Some(false);
        let mut content = tf
            .as_ref()
            .and_then(|f| f.get("result"))
            .map(|view| result_text(view, captured, arena))
            .transpose()?
            .flatten();
        if content.is_none() {
            content = error
                .map(|view| result_text(view, captured, arena))
                .transpose()?
                .flatten();
        }
        if let Some(content) = content {
            message.content.push(Block::ToolResult {
                tool_use_id: id,
                content,
                is_error,
                tool_name: Some(name),
                status: (!status.is_empty()).then_some(status),
                details: None,
            });
        }
    }
    let model = fields
        .get("modelInfo")
        .map(|v| v.get("modelName"))
        .transpose()?
        .flatten()
        .or(fields.get("modelType"))
        .or(fields.get("model"));
    message.model = model.map(string).transpose()?.flatten().filter(|s| !s.is_empty());
    if let Some(tokens) = fields
        .get("tokenCount")
        .map(|v| Fields::parse(v, &["inputTokens", "outputTokens"]))
        .transpose()?
        .flatten()
    {
        let input = tokens
            .get("inputTokens")
            .map(|v| v.resolve().map(|v| v.as_u64()))
            .transpose()?
            .flatten();
        let output = tokens
            .get("outputTokens")
            .map(|v| v.resolve().map(|v| v.as_u64()))
            .transpose()?
            .flatten();
        if input.is_some() || output.is_some() {
            message.usage = Some(Usage {
                input_tokens: input,
                output_tokens: output,
                ..Usage::default()
            });
        }
    }
    if let Some(mode) = fields.get("unifiedMode") {
        let metadata = unified_mode_metadata(&scalar(mode)?);
        if !metadata.is_empty() {
            message.harness_extra_mut(Harness::Cursor).extend(metadata);
        }
    }
    Ok((!message.content.is_empty() || !message.extra.is_empty()).then_some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectSink;
    use serde_json::json;
    fn fixture(dir: &Path, root: &str, bubbles: &[(&str, &[u8])]) -> (Connection, SessionRef) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("state.vscdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO cursorDiskKV VALUES('composerData:test',?1)", [root])
            .unwrap();
        for (id, raw) in bubbles {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES(?1,?2)",
                rusqlite::params![format!("bubbleId:test:{id}"), raw],
            )
            .unwrap();
        }
        (conn, SessionRef::new("test", Harness::Cursor, path))
    }
    fn compare(conn: &Connection, r: &SessionRef) -> Vec<Message> {
        let mut old = CollectSink::default();
        let mut new = CollectSink::default();
        let old_session = stream_global(conn, r, &mut old).unwrap();
        let new_session = global(conn, r, &mut new).unwrap();
        assert_eq!(
            serde_json::to_value(old_session).unwrap(),
            serde_json::to_value(new_session).unwrap()
        );
        let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
        for message in &mut new.messages {
            message.materialize(&resolver).unwrap();
        }
        assert_eq!(
            serde_json::to_value(&old.messages).unwrap(),
            serde_json::to_value(&new.messages).unwrap()
        );
        old.messages
    }
    #[test]
    fn retained_giant_identity_and_model_match_owned_ide_without_dropping_bubble() {
        let world = cv_testkit::World::in_process("cursor-giant-metadata");
        let id = "bubble-🌙".repeat(crate::lazy::INLINE_MAX);
        let model = "native-model-🌙".repeat(crate::lazy::INLINE_MAX);
        assert!(id.len() > crate::lazy::INLINE_MAX && model.len() > crate::lazy::INLINE_MAX);
        let root = json!({"fullConversationHeadersOnly":[{"bubbleId":id}]});
        let bubble = json!({"bubbleId":id,"type":2,"text":"retained message","modelInfo":{"modelName":model}});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[(&id, bubble.to_string().as_bytes())]);
        let messages = compare(&conn, &r);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id.as_deref(), Some(id.as_str()));
        assert_eq!(messages[0].model.as_deref(), Some(model.as_str()));
        let mut sink = CollectSink::default();
        let captured = global(&conn, &r, &mut sink).unwrap();
        assert_eq!(captured.model.as_deref(), Some(model.as_str()));
    }

    #[test]
    fn facts_match_owned_ide_messages_doctor_modes_and_affordances() {
        let world = cv_testkit::World::in_process("cursor-ide-facts");
        let big = "日本語\n".repeat(8192);
        let root = json!({"name":"composer","createdAt":1,"lastUpdatedAt":2,"unifiedMode":"agent","subagentInfo":{"parentComposerId":"parent","toolCallId":"spawn","subagentTypeName":"researcher","ignored":big},"modelConfig":{"modelName":"fallback","ignored":big},"fullConversationHeadersOnly":[{"bubbleId":"user"},{"bubbleId":"assistant"}],"conversation":[{"type":1,"text":"must not emit"}],"ignored":big});
        let user = json!({"bubbleId":"user","type":1,"text":"question","unifiedMode":99});
        let assistant = json!({"bubbleId":"assistant","type":2,"thinking":{"text":big,"signature":"sig"},"text":big,"modelInfo":{"modelName":"native-model","ignored":big},"tokenCount":{"inputTokens":10,"outputTokens":5,"ignored":big},"unifiedMode":1,"toolFormerData":{"name":"run_terminal_cmd","toolCallId":"call","rawArgs":json!({"command":"cat /scratch/file","content":big}).to_string(),"result":big,"status":"ERROR","error":{"ignored":big}},"providerOptions":{"ignored":big}});
        let (conn, r) = fixture(
            &world.base,
            &root.to_string(),
            &[
                ("user", user.to_string().as_bytes()),
                ("assistant", assistant.to_string().as_bytes()),
            ],
        );
        compare(&conn, &r);
        let adapter = Cursor::with_user_dir(&world.base);
        let owned = crate::stream::collect_with(&adapter, &r, &ParseOptions::bulk()).unwrap();
        let mut expected = Vec::new();
        crate::dataset::write_record(&owned, &mut expected, crate::dataset::Format::Chatml, None).unwrap();
        let query = crate::SessionQuery::parse("harness:cursor").unwrap();
        for filter in [None, Some(&query)] {
            let staged = crate::dataset::stage_record(
                &adapter,
                &r,
                crate::dataset::Format::Chatml,
                None,
                0,
                filter,
                &crate::query::TextSets::default(),
            )
            .unwrap()
            .unwrap();
            let mut actual = Vec::new();
            staged.publish(&mut actual).unwrap();
            actual.pop();
            assert_eq!(expected, actual);
        }
        let mut reports = Vec::new();
        let mut counts = Vec::new();
        for facts in [false, true] {
            let mut report = crate::doctor::Report::default();
            let mut pass = report.session(Some(r.path.clone()));
            let session = if facts {
                global(&conn, &r, &mut pass)
            } else {
                stream_global(&conn, &r, &mut pass)
            }
            .unwrap();
            pass.finish().unwrap();
            reports.push(report.to_json());
            let mut sink = CollectSink::default();
            if facts {
                global(&conn, &r, &mut sink)
            } else {
                stream_global(&conn, &r, &mut sink)
            }
            .unwrap();
            let mut counter = crate::affordance::Counter::new(&r);
            for message in &sink.messages {
                counter.note(message).unwrap();
            }
            counts.push(counter.finish(&session, &[]));
            if facts {
                assert!(matches!(
                    &sink.messages[1].content[1],
                    Block::Text { text: Text::Source(_) }
                ));
                assert!(matches!(
                    &sink.messages[1].content[2],
                    Block::ToolUse {
                        input: Json::Source(_),
                        ..
                    }
                ));
                let adapter = Cursor::with_user_dir(&world.base);
                for options in [ParseOptions::facts(), ParseOptions::lazy()] {
                    let collected = crate::stream::collect_with(&adapter, &r, &options).unwrap();
                    assert!(matches!(
                        &collected.messages[1].content[1],
                        Block::Text { text: Text::Source(_) }
                    ));
                }
            }
        }
        assert_eq!(reports[0], reports[1]);
        assert_eq!(counts[0], counts[1]);
    }
    #[test]
    fn facts_preserve_array_authority_argument_fallback_and_lexical_results() {
        let world = cv_testkit::World::in_process("cursor-ide-facts-edges");
        let mut bubbles = Vec::new();
        for raw in ["true", "null", "17", "\"literal\"", " ", "{"] {
            bubbles.push(json!({"type":2,"toolFormerData":{"rawArgs":raw,"params":"[1,2]","result":{"z":2,"a":1},"status":"error"},"modelInfo":{"modelName":null},"modelType":"must-not-fallback"}));
        }
        bubbles.extend([
            json!({"type":2,"text":"","richText":{"root":{"children":[{"text":"line","children":[{"text":" two"}]}]}},"unifiedMode":{}}),
            json!({"type":2,"richText":json!({"root":{"text":"encoded lexical"}}).to_string()}),
            json!({"type":2,"toolFormerData":null}),
            json!({"type":2,"toolFormerData":{"params":"invalid string","result":null,"error":"failure"}}),
            json!({"type":2,"unifiedMode":true}),
            json!({"type":2,"unifiedMode":99}),
        ]);
        bubbles.extend([
            json!({"type":2,"richText":{"root":{"text":"start","children":[{"text":"日本語","children":null},{"text":" next"},{"children":[{"text":"last"}]}]}}}),
            json!({"type":2,"richText":{"root":{"text":" \n\t","children":[null,17,{"text":false}]}}}),
            json!({"type":2,"richText":"{broken"}),
            json!({"type":2,"text":"","richText":""}),
            json!({"type":2,"richText":"","toolFormerData":{"name":"shell","result":"still visible"}}),
            json!({"type":2,"richText":"{\"root\":{\"text\":\"first\",\"text\":\"last\"}}"}),
            json!({"type":2,"richText":"{\"root\":{\"text\":\"valid\"},\"ignored\":1e9999}"}),
            json!({"type":2,"toolFormerData":{"result":[true,null,1.5,{"z":"日本語\n","a":"\\quoted\""}]}}),
        ]);
        bubbles.push(json!({"type":2,"richText":{"root":{"text":format!("{}日本語", " ".repeat(2048))}}}));
        bubbles.push(json!({"type":2,"richText":{"root":{"text":"\u{2003}".repeat(2048)}}}));
        let root = json!({"conversation":bubbles,"fullConversationHeadersOnly":false});
        let (conn, r) = fixture(&world.base.join("inline"), &root.to_string(), &[]);
        compare(&conn, &r);
        let roots = [
            r#"{"conversation":[{"type":1,"text":"first"}],"conversation":[{"type":1,"text":"last"}]}"#,
            r#"{"conversation":[{"type":1,"text":"fallback"}],"fullConversationHeadersOnly":[]}"#,
            r#"{"conversation":[{"type":1,"text":"fallback"}],"fullConversationHeadersOnly":[],"fullConversationHeadersOnly":false}"#,
            r#"{"fullConversationHeadersOnly":[{"bubbleId":"missing"},{"bubbleId":"invalid"},{"bubbleId":"empty"},{"bubbleId":"lossy"}]}"#,
            "null",
            "[]",
            "42",
        ];
        for (index, root) in roots.into_iter().enumerate() {
            let (conn, r) = fixture(
                &world.base.join(index.to_string()),
                root,
                &[
                    ("invalid", b"{broken"),
                    ("empty", b""),
                    ("lossy", b"{\"type\":1,\"text\":\"bad\xff\"}"),
                ],
            );
            compare(&conn, &r);
        }
        for (index, root) in [
            r#"{"conversation":[],"ignored":"\uD800"}"#,
            r#"{"conversation":[],"ignored":1e9999}"#,
            r#"{"conversation":[]} trailing"#,
        ]
        .into_iter()
        .enumerate()
        {
            let (conn, r) = fixture(&world.base.join(format!("invalid-{index}")), root, &[]);
            assert!(stream_global(&conn, &r, &mut CollectSink::default()).is_err());
            assert!(global(&conn, &r, &mut CollectSink::default()).is_err());
        }
    }
    #[test]
    fn cold_wal_adapter_preserves_original_sidecars_and_journal_guard() {
        let world = cv_testkit::World::in_process("cursor-ide-cold-wal");
        let root = json!({"conversation":[{"type":1,"text":"日本語\n".repeat(4096)}]});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[]);
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        drop(conn);
        let wal = r.path.with_file_name("state.vscdb-wal");
        let shm = r.path.with_file_name("state.vscdb-shm");
        let journal = r.path.with_file_name("state.vscdb-journal");
        assert!(!wal.exists() && !shm.exists());
        let adapter = Cursor::with_user_dir(&world.base);
        let retained = crate::stream::collect_with(&adapter, &r, &ParseOptions::facts()).unwrap();
        assert!(!wal.exists() && !shm.exists());
        let Block::Text {
            text: Text::Source(text),
        } = &retained.messages[0].content[0]
        else {
            panic!("source text required")
        };
        std::fs::write(&journal, b"changed journal").unwrap();
        assert!(text.try_prefix(10).is_err());
        std::fs::remove_file(&journal).unwrap();
        struct Stop(PathBuf);
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"changed journal").unwrap();
                Flow::Stop
            }
        }
        assert!(adapter
            .stream(&r, &ParseOptions::bulk(), &mut Stop(journal))
            .unwrap_err()
            .to_string()
            .contains("dependencies changed"));
        assert!(!wal.exists() && !shm.exists());
    }
    #[test]
    fn ide_facts_guard_later_demands_and_intentional_stop() {
        let world = cv_testkit::World::in_process("cursor-ide-facts-guard");
        let root = json!({"conversation":[{"type":1,"text":"日本語\n".repeat(4096)}]});
        let (conn, r) = fixture(&world.base, &root.to_string(), &[]);
        let mut sink = CollectSink::default();
        global(&conn, &r, &mut sink).unwrap();
        let wal = r.path.with_file_name("state.vscdb-wal");
        std::fs::write(&wal, b"changed WAL").unwrap();
        let Block::Text {
            text: Text::Source(text),
        } = &sink.messages[0].content[0]
        else {
            panic!("expected source text")
        };
        assert!(text.try_prefix(10).is_err());
        std::fs::remove_file(&wal).unwrap();
        struct Stop(PathBuf);
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"changed WAL").unwrap();
                Flow::Stop
            }
        }
        assert!(global(&conn, &r, &mut Stop(wal))
            .unwrap_err()
            .to_string()
            .contains("dependencies changed"));
    }
}
