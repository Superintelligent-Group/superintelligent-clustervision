//! Checked projections of the immutable decoded thread. Large text/arguments retain the
//! shared backing; demanded extras and metadata keep the adapter's existing owned contract.
use super::*;
use crate::harness::store::document::{text_head, Fields};
use crate::json::{JsonSource, JsonView};
use crate::lazy::{Resolver, Text};

pub(super) fn replay(view: JsonView<'_>, source: &JsonSource, f: &mut dyn FnMut(Message) -> Flow) -> Result<()> {
    let Some(root) = Fields::parse(view, &["messages"])? else {
        return Ok(());
    };
    let Some(messages) = root.get("messages").map(|v| v.array()).transpose()?.flatten() else {
        return Ok(());
    };
    let mut names = HashMap::new();
    for message in messages {
        let Some(fields) = Fields::parse(
            message?,
            &[
                "role",
                "id",
                "segments",
                "text",
                "tool_uses",
                "context",
                "is_hidden",
                "creases",
                "tool_results",
                "User",
                "Agent",
                "Compaction",
            ],
        )?
        else {
            continue;
        };
        if legacy(&fields, source, &mut names, f)? == Flow::Stop || modern(&fields, source, f)? == Flow::Stop {
            break;
        }
    }
    Ok(())
}

fn text(fields: &Fields<'_>, key: &str) -> Result<Option<Text>> {
    fields.get(key).map(|v| v.text(true)).transpose().map(Option::flatten)
}
fn nonempty(text: Option<Text>) -> Option<Text> {
    text.filter(|text| !text.is_empty())
}
fn nonnull(fields: &Fields<'_>, key: &str) -> Result<Option<Value>> {
    let Some(value) = fields.get(key) else { return Ok(None) };
    if value.is_null()? {
        return Ok(None);
    }
    Ok(Some(value.resolve()?.into_owned()))
}
fn remember(block: &Block, names: &mut HashMap<String, String>) {
    if let Block::ToolUse { id, name, .. } = block {
        if !id.is_empty() && !name.is_empty() {
            names.insert(id.clone(), name.clone());
        }
    }
}
fn tool_use(view: JsonView<'_>) -> Result<Block> {
    let fields = Fields::parse(view, &["id", "name", "input"])?;
    Ok(Block::ToolUse {
        id: fields
            .as_ref()
            .map(|v| v.string("id"))
            .transpose()?
            .flatten()
            .unwrap_or_default(),
        name: fields
            .as_ref()
            .map(|v| v.string("name"))
            .transpose()?
            .flatten()
            .unwrap_or_default(),
        input: fields
            .as_ref()
            .and_then(|v| v.get("input"))
            .map(|v| v.argument(true))
            .transpose()?
            .unwrap_or_default(),
        namespace: None,
    })
}

fn legacy(
    fields: &Fields<'_>,
    source: &JsonSource,
    names: &mut HashMap<String, String>,
    f: &mut dyn FnMut(Message) -> Flow,
) -> Result<Flow> {
    let Some(role) = fields.string("role")? else {
        return Ok(Flow::Continue);
    };
    let role = match role.as_str() {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    };
    let mut msg = Message::new(role);
    if role == Role::System {
        msg.kind = MessageKind::SystemPrompt;
    }
    msg.id = fields
        .get("id")
        .map(|v| v.as_number())
        .transpose()?
        .flatten()
        .and_then(|v| v.as_i64())
        .map(|id| id.to_string());
    if let Some(segments) = fields.get("segments").map(|v| v.array()).transpose()?.flatten() {
        for segment in segments {
            if let Some(block) = anthropic::block_from_view(segment?, &anthropic::ZED, true)? {
                msg.content.push(block);
            }
        }
    } else if let Some(text) = nonempty(text(fields, "text")?) {
        msg.content.push(Block::Text { text });
    }
    if let Some(uses) = fields.get("tool_uses").map(|v| v.array()).transpose()?.flatten() {
        for value in uses {
            let block = tool_use(value?)?;
            remember(&block, names);
            msg.content.push(block);
        }
    }
    if let Some(context) = nonempty(text(fields, "context")?) {
        if msg.content.is_empty() {
            msg.content.push(Block::Text { text: context });
        } else {
            msg.harness_extra_mut(Harness::Zed).insert(
                "context".into(),
                Value::String(context.try_resolve(&Resolver::new(None))?.into_owned()),
            );
        }
    }
    if fields.get("is_hidden").map(|v| v.as_bool()).transpose()?.flatten() == Some(true) {
        msg.harness_extra_mut(Harness::Zed)
            .insert("is_hidden".into(), Value::Bool(true));
        if role == Role::User {
            msg.kind = MessageKind::InjectedContext;
            msg.origin = Origin::Harness;
        }
    }
    if let Some(value) = fields.get("creases") {
        if let Some(mut array) = value.array()? {
            if array.next().transpose()?.is_some() {
                msg.harness_extra_mut(Harness::Zed)
                    .insert("creases".into(), value.resolve()?.into_owned());
            }
        }
    }
    if !msg.content.is_empty() && f(msg) == Flow::Stop {
        return Ok(Flow::Stop);
    }
    let mut blocks = Vec::new();
    if let Some(results) = fields.get("tool_results").map(|v| v.array()).transpose()?.flatten() {
        for result in results {
            if let Some(block) = tool_result(result?, source, names)? {
                blocks.push(block);
            }
        }
    }
    Ok(emit_tools(blocks, f))
}

fn emit_tools(blocks: Vec<Block>, f: &mut dyn FnMut(Message) -> Flow) -> Flow {
    if blocks.is_empty() {
        return Flow::Continue;
    }
    let mut msg = Message::new(Role::Tool);
    msg.content = blocks;
    f(msg)
}
fn tool_result(view: JsonView<'_>, source: &JsonSource, names: &HashMap<String, String>) -> Result<Option<Block>> {
    let Some(fields) = Fields::parse(view, &["tool_use_id", "tool_name", "content", "is_error", "output"])? else {
        return Ok(None);
    };
    let Some(id) = fields.string("tool_use_id")? else {
        return Ok(None);
    };
    Ok(Some(Block::ToolResult {
        tool_name: fields.string("tool_name")?.or_else(|| names.get(&id).cloned()),
        tool_use_id: id,
        content: result_content(fields.get("content"), source)?,
        is_error: fields
            .get("is_error")
            .map(|v| v.as_bool())
            .transpose()?
            .flatten()
            .unwrap_or(false),
        details: nonnull(&fields, "output")?,
        status: None,
    }))
}
fn result_content(view: Option<JsonView<'_>>, source: &JsonSource) -> Result<Text> {
    let Some(view) = view else { return Ok(Text::default()) };
    if let Some(text) = view.text(true)? {
        return Ok(text);
    }
    if let Some(fields) = Fields::parse(view, &["Text", "text", "Image", "image"])? {
        // Presence wins before type checking, including a nonstring uppercase field.
        if let Some(text) = fields
            .get("Text")
            .or_else(|| fields.get("text"))
            .map(|v| v.text(true))
            .transpose()?
            .flatten()
        {
            return Ok(text);
        }
        if fields.get("Image").or_else(|| fields.get("image")).is_some() {
            return Ok("[image]".into());
        }
    }
    if view.is_null()? {
        return Ok(Text::default());
    }
    crate::harness::store::text::append(source, &source.decoded_backing()?, |writer| writer.push_json(view))
}

fn modern(fields: &Fields<'_>, source: &JsonSource, f: &mut dyn FnMut(Message) -> Flow) -> Result<Flow> {
    if let Some(user) = fields.get("User") {
        let mut msg = Message::new(Role::User);
        if let Some(user) = Fields::parse(user, &["id", "content"])? {
            msg.id = user.string("id")?;
            if let Some(content) = user.get("content").map(|v| v.array()).transpose()?.flatten() {
                for item in content {
                    if let Some(block) = user_block(item?)? {
                        msg.content.push(block);
                    }
                }
            }
        }
        return Ok(if msg.content.is_empty() { Flow::Continue } else { f(msg) });
    }
    if let Some(agent) = fields.get("Agent") {
        let mut msg = Message::new(Role::Assistant);
        let mut names = HashMap::new();
        let Some(agent) = Fields::parse(agent, &["content", "reasoning_details", "tool_results"])? else {
            return Ok(Flow::Continue);
        };
        if let Some(content) = agent.get("content").map(|v| v.array()).transpose()?.flatten() {
            for item in content {
                if let Some(block) = agent_block(item?)? {
                    remember(&block, &mut names);
                    msg.content.push(block);
                }
            }
        }
        if let Some(value) = nonnull(&agent, "reasoning_details")? {
            msg.harness_extra_mut(Harness::Zed)
                .insert("reasoning_details".into(), value);
        }
        if (!msg.content.is_empty() || !msg.extra.is_empty()) && f(msg) == Flow::Stop {
            return Ok(Flow::Stop);
        }
        let mut blocks = Vec::new();
        if let Some(results) = agent.get("tool_results") {
            results.for_each_entry(|_, result| {
                if let Some(block) = tool_result(result, source, &names)? {
                    blocks.push(block);
                }
                Ok(())
            })?;
        }
        return Ok(emit_tools(blocks, f));
    }
    if let Some(compaction) = fields.get("Compaction") {
        if let Some(compaction) = Fields::parse(compaction, &["Summary"])? {
            if let Some(text) = text(&compaction, "Summary")? {
                if !text_head(&text, &Resolver::new(None), 1, true)?.0.is_empty() {
                    let mut msg = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
                    msg.content.push(Block::Text { text });
                    return Ok(f(msg));
                }
            }
        }
    }
    Ok(Flow::Continue)
}
fn user_block(view: JsonView<'_>) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(view, &["Text", "Mention", "Image"])? else {
        return Ok(None);
    };
    if let Some(text) = text(&item, "Text")? {
        return Ok(nonempty(Some(text)).map(|text| Block::Text { text }));
    }
    if let Some(mention) = item.get("Mention") {
        let Some(mention) = Fields::parse(mention, &["uri"])? else {
            return Ok(None);
        };
        let Some(uri) = mention.get("uri") else { return Ok(None) };
        let value = uri.resolve()?;
        let uri = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
        return Ok(Some(Block::File {
            mime: None,
            path: None,
            source: Some(uri),
        }));
    }
    if let Some(image) = item.get("Image") {
        let fields = Fields::parse(image, &["source"])?;
        let data_ref = if let Some(text) = fields.as_ref().map(|v| text(v, "source")).transpose()?.flatten() {
            // 121 characters detect the existing 120-character truncation boundary; retained
            // source payloads are never fully decoded merely to produce this reference.
            Some(crate::ir::truncate(
                &text_head(&text, &Resolver::new(None), 121, false)?.0,
                120,
            ))
        } else {
            None
        };
        return Ok(Some(Block::Image {
            media_type: None,
            data_ref,
        }));
    }
    Ok(None)
}
fn agent_block(view: JsonView<'_>) -> Result<Option<Block>> {
    let Some(item) = Fields::parse(view, &["Text", "Thinking", "RedactedThinking", "ToolUse"])? else {
        return Ok(None);
    };
    if let Some(text) = text(&item, "Text")? {
        return Ok(nonempty(Some(text)).map(|text| Block::Text { text }));
    }
    if let Some(thinking) = item.get("Thinking") {
        let fields = Fields::parse(thinking, &["text", "signature"])?;
        return Ok(Some(Block::Thinking {
            text: fields
                .as_ref()
                .map(|v| text(v, "text"))
                .transpose()?
                .flatten()
                .unwrap_or_default(),
            signature: fields
                .as_ref()
                .map(|v| v.string("signature"))
                .transpose()?
                .flatten()
                .filter(|s| !s.is_empty()),
            encrypted: None,
            redacted: false,
        }));
    }
    if let Some(data) = item.string("RedactedThinking")? {
        return Ok(Some(Block::Thinking {
            text: Text::default(),
            signature: None,
            encrypted: Some(data),
            redacted: true,
        }));
    }
    item.get("ToolUse").map(tool_use).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::zed::blob;

    #[test]
    fn checked_projection_matches_owned_messages_and_value_acceptance() {
        let world = cv_testkit::World::new("zed-checked-oracle");
        let path = world.base.join("provenance");
        std::fs::write(&path, b"unchanged").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let mut cases = vec![
            serde_json::to_string(&super::super::tests::legacy_thread()).unwrap(),
            serde_json::to_string(&super::super::tests::modern_thread()).unwrap(),
            r#"{"messages":[{"role":"user","text":"old"}],"title":"old","messages":[{"role":"alien","id":-1,"text":"","context":"attached"},{"role":"system","segments":false,"text":"oldest","is_hidden":true,"creases":[1]},{"role":"assistant","segments":[{"type":"thinking","text":"雪\n\"\\","signature":"sig"},{"type":"RedactedThinking","data":"secret"},{"type":"tool_use","id":"s","name":"shell","input":{"z":1,"a":2,"z":3}}],"tool_uses":[{"id":"call","name":"shell","input":[1,2]}],"tool_results":[{"tool_use_id":"call","content":{"Text":null,"text":"ignored","z":1}},{"tool_use_id":"call","content":{"image":null}},{"tool_use_id":"call","content":null}]},{"User":null,"Agent":{"content":[{"Text":"ignored"}]}},{"User":{"content":[{"Text":"","Mention":{"uri":"ignored"}},{"Text":null,"Mention":{"uri":{"z":1,"a":2},"content":"discarded"}},{"Image":{"source":"雪\nabc"}}]}},{"Agent":{"content":[{"Text":""},{"Thinking":null},{"Thinking":{"text":"reason","signature":""}},{"RedactedThinking":"opaque"},{"ToolUse":false},{"ToolUse":{"id":"q","name":"query"}}],"reasoning_details":{"z":1,"a":2},"tool_results":{"z":{"tool_use_id":"q","content":"obsolete"},"a":{"tool_use_id":"q","content":{"text":"tool","Text":null}},"z":{"tool_use_id":"q","content":"winner","output":{"b":1,"a":2}}}}},{"Compaction":{"Summary":" \n雪 "}},{"Compaction":{"Summary":" \n"}},{"role":"user","text":"legacy","User":{"id":"mixed","content":[{"Text":"modern"}]}}],"ti\u0074le":"last"}"#.into(),
            r#"{"messages":[{"role":"user","text":"obsolete"}],"messages":false}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":"\ud800"}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":1e400}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}],"ignored":{"a":"\ud800","a":null}}"#.into(),
            r#"{"messages":[{"role":"user","text":"ok"}]} trailing"#.into(),
            "null".into(), "42".into(), "\"scalar\"".into(), "[]".into(),
        ];
        for size in [120, 121, 200] {
            cases.push(
                serde_json::json!({"messages":[{"User":{"content":[{"Image":{"source":"雪\n".repeat(size)}}]}}]})
                    .to_string(),
            );
        }
        for depth in 125..=129 {
            cases.push(format!(
                "{{\"messages\":[{{\"role\":\"user\",\"text\":\"ok\",\"ignored\":{}0{}}}]}}",
                "[".repeat(depth),
                "]".repeat(depth)
            ));
        }
        for raw in cases {
            let owned = serde_json::from_str::<Value>(&raw).ok();
            let mut expected = Vec::new();
            if let Some(value) = owned.as_ref() {
                super::super::each_ir_message(value, &mut |message| {
                    expected.push(message);
                    Flow::Continue
                });
            }
            for compressed in [false, true] {
                let bytes = if compressed {
                    ruzstd::encoding::compress_to_vec(raw.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
                } else {
                    raw.as_bytes().to_vec()
                };
                let header = blob::prepare_checked_reader(None, bytes.as_slice(), &source);
                assert_eq!(header.is_some(), owned.is_some(), "{raw}");
                let Some(header) = header else { continue };
                let view = header.checked.as_ref().unwrap();
                let mut actual = Vec::new();
                view.with_view(|view| {
                    replay(view, &source, &mut |message| {
                        actual.push(message);
                        Flow::Continue
                    })
                })
                .unwrap();
                assert_eq!(
                    serde_json::to_vec(&actual).unwrap(),
                    serde_json::to_vec(&expected).unwrap(),
                    "{raw}"
                );
                let mut stopped = Vec::new();
                view.with_view(|view| {
                    replay(view, &source, &mut |message| {
                        stopped.push(message);
                        Flow::Stop
                    })
                })
                .unwrap();
                assert_eq!(
                    serde_json::to_vec(&stopped).unwrap(),
                    serde_json::to_vec(&expected[..usize::from(!expected.is_empty())]).unwrap()
                );
            }
        }
    }

    #[test]
    fn checked_text_arguments_and_canonical_results_survive_header_and_detect_change() {
        let world = cv_testkit::World::new("zed-checked-carriers");
        let path = world.base.join("provenance");
        std::fs::write(&path, b"unchanged").unwrap();
        let source = JsonSource::capture(&path).unwrap();
        let large = "雪\n\"\\".repeat(crate::harness::store::bytes::MEMORY_LIMIT / 4);
        let value = serde_json::json!({"messages":[
            {"role":"user","text":large},
            {"Agent":{"content":[{"Thinking":{"text":large}},{"ToolUse":{"id":"t","name":"shell","input":{"body":large}}}],
                "tool_results":{"t":{"tool_use_id":"t","content":{"b":large,"a":1}}}}},
            {"User":{"content":[{"Mention":{"uri":"file:///reference","content":large}}]}}
        ]});
        let raw = serde_json::to_vec(&value).unwrap();
        let header = blob::prepare_checked_reader(Some("json"), raw.as_slice(), &source).unwrap();
        let mut messages = Vec::new();
        header
            .checked
            .as_ref()
            .unwrap()
            .with_view(|view| {
                replay(view, &source, &mut |message| {
                    messages.push(message);
                    Flow::Continue
                })
            })
            .unwrap();
        drop(header);
        let Block::Text { text } = &messages[0].content[0] else {
            panic!("text")
        };
        assert!(matches!(text, Text::Source(_)));
        let Block::Thinking { text: thinking, .. } = &messages[1].content[0] else {
            panic!("thinking")
        };
        assert!(matches!(thinking, Text::Source(_)));
        let Block::ToolUse { input, .. } = &messages[1].content[1] else {
            panic!("tool input")
        };
        assert!(matches!(input, crate::json::Json::Source(_)));
        let Block::ToolResult { content, .. } = &messages[2].content[0] else {
            panic!("result")
        };
        assert!(matches!(content, Text::Source(_)));
        assert_eq!(text.try_resolve(&Resolver::new(None)).unwrap().as_ref(), large);
        assert_eq!(
            input.resolve().unwrap().get("body").and_then(Value::as_str),
            Some(large.as_str())
        );
        assert_eq!(
            content.try_resolve(&Resolver::new(None)).unwrap().as_ref(),
            value["messages"][1]["Agent"]["tool_results"]["t"]["content"].to_string()
        );
        std::fs::write(&path, b"changed identity").unwrap();
        assert!(text.try_resolve(&Resolver::new(None)).is_err());
        assert!(input.resolve().is_err());
        assert!(content.try_resolve(&Resolver::new(None)).is_err());
    }
}
