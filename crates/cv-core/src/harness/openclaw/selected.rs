//! JSONL reads retain checked ordinary payload ranges, including complete and branch-selected messages.
//! Complete unknown/control rows and system/custom roles keep their existing owned policy.
//! One captured original source serves navigation and payload replay.
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
    source: &JsonSource,
) -> Result<Session> {
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
                    |view| project(view, source),
                )
            });
            consume(rows, r, facts, opts, sink)
        }
        #[cfg(not(feature = "mmap"))]
        {
            let context = crate::harness::store::document::range::RangeSource::from_captured((*source).clone())?;
            let extent = ranges::Range {
                offset: 0,
                len: source.len(),
            };
            let rows = ranges::records(&file, extent)?.map(|row| {
                let range = ranges::trim(&file, row?.0)?;
                if range.len == 0 {
                    return Ok(None);
                }
                source.read_positioned_range_value(&context, range.offset, range.len, |view| project(view, source))
            });
            consume(rows, r, facts, opts, sink)
        }
    })();
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    result
}

/// Branch selection retains only navigation fields and source ranges. Required O(events)
/// navigation is distinct from owning every payload on both active and inactive branches.
pub(super) fn tree(
    r: &SessionRef,
    facts: &KeyFacts,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    source: &JsonSource,
) -> Result<Session> {
    let file = fs::File::open(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
    source.check_metadata(file.metadata()?)?;
    let result = (|| {
        // The preceding whole-tree read_to_string rejected any invalid UTF-8, including
        // off-branch/malformed rows. Keep that stronger policy than flat JSONL skipping.
        let extent = ranges::Range {
            offset: 0,
            len: source.len(),
        };
        if !ranges::valid_utf8(&mut ranges::reader(&file, extent)?)? {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8").into(),
            );
        }
        let mut descriptors = Vec::new();
        let mut records = Vec::new();
        super::super::store::checked_lines::for_each(&file, 0, source, |view, _, _, _| {
            descriptors.push(navigation_descriptor(view)?);
            records.push(view.source_range()?.expect("tree records are source ranges"));
            Ok(Flow::Continue)
        })?;
        let navigation = if descriptors.iter().any(is_leaf_control) {
            select_navigation(&descriptors, opts.complete)
        } else {
            (0..descriptors.len())
                .map(|index| NavigationRow {
                    index,
                    active: true,
                    parent: None,
                })
                .collect()
        };
        drop(descriptors);
        let rows = navigation.into_iter().map(|navigation| {
            let (offset, len) = records[navigation.index];
            source.read_range_value(offset, len, |view| {
                let row = if opts.spans {
                    project(view, source)?
                } else {
                    Some(Row::active(view.resolve()?.into_owned()))
                };
                Ok(row.map(|mut row| {
                    row.active = navigation.active;
                    row.parent = navigation.parent;
                    row
                }))
            })
        });
        consume(rows, r, facts, opts, sink)
    })();
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    result
}

fn navigation_descriptor(view: JsonView<'_>) -> Result<Value> {
    const KEYS: &[&str] = &[
        "type",
        "id",
        "parentId",
        "appendMode",
        "targetId",
        "appendParentId",
        "firstKeptEntryId",
    ];
    let Some(fields) = Fields::parse(view, KEYS)? else {
        return Ok(Value::Null);
    };
    let mut descriptor = serde_json::Map::new();
    for key in KEYS {
        if let Some(field) = fields.get(key) {
            // Only string/null values affect navigation. Preserve key presence and invalid
            // type with a tiny sentinel rather than resolving arbitrary malformed metadata.
            let value = if field.is_string()? {
                field.resolve()?.into_owned()
            } else if field.is_null()? {
                Value::Null
            } else {
                Value::Bool(false)
            };
            descriptor.insert((*key).into(), value);
        }
    }
    Ok(Value::Object(descriptor))
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

    #[test]
    fn checked_openclaw_selection_keeps_original_identity_and_race_error() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-selection-identity");
        let path = world.base.join("source.jsonl");
        let flat = b"{\"type\":\"message\",\"id\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"original\"}}\n";
        let tree = b"{\"type\":\"leaf\",\"leafId\":\"new\"}\n{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"replacement\"}}\n";
        for parser_error in [false, true] {
            fs::write(&path, flat)?;
            let r = SessionRef::new("identity", Harness::OpenClaw, &path);
            let error = with_jsonl_source(&path, |source| {
                let facts = jsonl_key_facts(&path, &r.id);
                assert!(!has_leaf_controls(&path)?);
                // Deterministically replace the primary after selection, before opening payloads.
                fs::write(&path, tree)?;
                if parser_error {
                    anyhow::bail!("callback diagnostic");
                }
                let mut sink = crate::stream::CollectSink::default();
                stream(&r, &facts, &ParseOptions::complete(), &mut sink, source)
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("source changed"), "{error:#}");
        }
        Ok(())
    }

    #[test]
    fn checked_openclaw_owned_and_tree_stop_reject_original_source_replacement() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-owned-tree-identity");
        let path = world.base.join("source.jsonl");
        for tree in [false, true] {
            let mut bytes =
                b"{\"type\":\"message\",\"id\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"original\"}}\n"
                    .to_vec();
            if tree {
                bytes.extend_from_slice(b"{\"type\":\"leaf\",\"leafId\":\"u\"}\n");
            }
            for spans in [false, true] {
                fs::write(&path, &bytes)?;
                // Use a fallible callback so its diagnostic cannot hide the primary-source race.
                struct MutateFallible<'a>(&'a Path);
                impl MessageSink for MutateFallible<'_> {
                    fn message(&mut self, _: Message) -> Flow {
                        unreachable!()
                    }
                    fn try_message(&mut self, _: Message) -> Result<Flow> {
                        fs::write(self.0, b"replacement")?;
                        anyhow::bail!("callback diagnostic")
                    }
                }
                let adapter = OpenClaw { roots: vec![] };
                let r = SessionRef::new("identity", Harness::OpenClaw, &path);
                let options = ParseOptions {
                    spans,
                    ..ParseOptions::complete()
                };
                let error = adapter.stream(&r, &options, &mut MutateFallible(&path)).unwrap_err();
                assert!(
                    format!("{error:#}").contains("source changed"),
                    "tree={tree}/spans={spans}: {error:#}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn checked_openclaw_tree_replays_compact_navigation_with_exact_owned_parity() -> Result<()> {
        let world = cv_testkit::World::in_process("openclaw-tree-parity");
        let path = world.base.join("tree.jsonl");
        let adapter = OpenClaw { roots: vec![] };
        for fixture in ["branchy-v4.jsonl", "reset-and-side-entries.jsonl", "v3-full.jsonl"] {
            let original = fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/openclaw")
                    .join(fixture),
            )?;
            let mut text = original;
            text.push_str("\n{\"ignored\":1e999}\n{\"ignored\":\"\\ud800\"}\nmalformed\n");
            text.push_str("\u{2003}{\"type\":\"future\",\"id\":\"future\",\"parentId\":null,\"payload\":{\"unknown\":true}}\u{2003}\n");
            text.push_str("{\"type\":\"message\",\"id\":\"dup\",\"parentId\":null,\"parentId\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"first\",\"content\":\"last\"}}\n");
            fs::write(&path, &text)?;
            let r = SessionRef::new("tree", Harness::OpenClaw, &path);
            for complete in [false, true] {
                let options = ParseOptions {
                    complete,
                    spans: true,
                    ..ParseOptions::full()
                };
                let expected = parse_text_with(&text, &r, &options);
                let mut actual = collect(&adapter, &r, &options)?;
                actual.materialize()?;
                assert_eq!(
                    serde_json::to_value(&expected)?,
                    serde_json::to_value(actual)?,
                    "{fixture}/complete={complete}"
                );
            }
        }
        // Required branch metadata remains tiny even when ordinary active and inactive bodies grow.
        let mut entries: Vec<Value> =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openclaw/branchy-v4.jsonl"))?
                .lines()
                .filter_map(parse_line)
                .collect();
        for value in &mut entries {
            if matches!(value["id"].as_str(), Some("u1" | "a1" | "a2")) {
                value["message"]["content"] = json!([{"type":"text","text":"giant λ\n".repeat(262144)}]);
            }
        }
        let text = entries
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        fs::write(&path, &text)?;
        let r = SessionRef::new("giant-tree", Harness::OpenClaw, &path);
        for complete in [false, true] {
            let options = ParseOptions {
                complete,
                spans: true,
                ..ParseOptions::full()
            };
            let mut actual = collect(&adapter, &r, &options)?;
            assert!(actual
                .messages
                .iter()
                .filter(|m| matches!(m.id.as_deref(), Some("u1" | "a1" | "a2")))
                .all(|m| m.content.iter().any(|b| matches!(
                    b,
                    Block::Text {
                        text: crate::Text::Source(_)
                    }
                ))));
            let expected = parse_text_with(&text, &r, &options);
            actual.materialize()?;
            assert_eq!(serde_json::to_value(expected)?, serde_json::to_value(actual)?);
        }
        fs::write(&path, [text.as_bytes(), b"\n\xff"].concat())?;
        let r = SessionRef::new("bad-tree", Harness::OpenClaw, &path);
        let error = collect(
            &adapter,
            &r,
            &ParseOptions {
                spans: true,
                ..ParseOptions::full()
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("valid UTF-8"));
        Ok(())
    }
}
