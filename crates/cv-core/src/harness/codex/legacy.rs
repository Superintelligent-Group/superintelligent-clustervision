//! Legacy single-document rollouts use the same checked source views and response-item
//! dispatcher as modern rollouts. Only one item's demanded metadata is owned at a time.
use super::*;
use crate::harness::store::{document::Fields, jsonl::ranges, text};
use crate::json::{InvalidJson, JsonSource, JsonView};

/// Value acceptance is whole-document, including ignored descendants. Invalid JSON remains an
/// empty session; read errors (including invalid UTF-8) remain fatal, as with read_to_string.
fn read<T>(path: &Path, f: impl for<'a> FnOnce(&JsonSource, JsonView<'a>) -> Result<T>) -> Result<Option<T>> {
    let source = JsonSource::capture(path)?;
    let file = fs::File::open(path)?;
    source.check_metadata(file.metadata()?)?;
    if file.metadata()?.len() == 0 {
        source.check()?;
        return Ok(None);
    }
    let mut entered = false;
    match source.read_document(|view| {
        entered = true;
        f(&source, view)
    }) {
        Ok(value) => Ok(Some(value)),
        Err(error) if !entered && error.is::<InvalidJson>() => {
            let valid = ranges::valid_utf8(&mut BufReader::new(file))?;
            source.check()?;
            anyhow::ensure!(valid, "reading {}: stream did not contain valid UTF-8", path.display());
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn items(root: JsonView<'_>) -> Result<Option<crate::json::JsonArray<'_>>> {
    if let Some(items) = root.get("items")? {
        if let Some(array) = items.array()? {
            return Ok(Some(array));
        }
    }
    root.array()
}

fn session(root: JsonView<'_>, s: &mut Session, ctx: &mut CodexCtx) -> Result<()> {
    let Some(meta) = root.get("session")? else {
        return Ok(());
    };
    let mut keys = META_EXTRA_KEYS.to_vec();
    keys.extend([
        "id",
        "cwd",
        "timestamp",
        "parent_thread_id",
        "forked_from_id",
        "agent_path",
        "base_instructions",
        "git",
    ]);
    if let Some(fields) = Fields::parse(meta, &keys)? {
        let value = Value::Object(fields.project(&keys)?);
        apply_meta(s, ctx, Some(&value));
    }
    Ok(())
}

/// Discovery deliberately keeps the legacy policy: only session timestamps, no cwd, no event
/// authority, and turns counted by the same reduced-item dispatcher as the owned scan.
pub(super) fn scan(path: &Path) -> Result<CodexScan> {
    Ok(read(path, |_source, root| {
        let mut scan = CodexScan::default();
        let meta = root
            .get("session")?
            .map(|v| Fields::parse(v, &["id", "timestamp", "agent_path"]))
            .transpose()?
            .flatten();
        let mut ctx = CodexCtx::default();
        if let Some(meta) = meta {
            scan.id = meta.string("id")?.unwrap_or_default();
            scan.created_at = meta.string("timestamp")?.as_deref().and_then(parse_ts);
            scan.updated_at = scan.created_at;
            ctx.agent_path = meta.string("agent_path")?;
        }
        if let Some(items) = items(root)? {
            for item in items {
                let item = item?;
                let Some(fields) = Fields::parse(
                    item,
                    &["type", "role", "author", "content", "summary", "encrypted_content"],
                )?
                else {
                    continue;
                };
                let ty = fields.string("type")?.unwrap_or_default();
                let mut p = Map::new();
                p.insert("type".into(), ty.clone().into());
                let role = if ty == "message" { fields.string("role")? } else { None };
                if let Some(role) = &role {
                    p.insert("role".into(), role.clone().into());
                }
                if ty == "agent_message" {
                    if let Some(author) = fields.string("author")? {
                        p.insert("author".into(), author.into());
                    }
                }
                if matches!(ty.as_str(), "message" | "reasoning") {
                    if let Some(value) = fields.get("content") {
                        p.insert("content".into(), count_content(value)?);
                    }
                }
                if ty == "reasoning" {
                    if let Some(value) = fields.get("summary") {
                        p.insert("summary".into(), count_content(value)?);
                    }
                    if let Some(value) = fields.get("encrypted_content") {
                        if let Some(value) = count_text(value)? {
                            p.insert("encrypted_content".into(), value.into());
                        }
                    }
                }
                let mut out = Vec::new();
                handle_item(Some(&Value::Object(p)), false, None, &mut out, &mut ctx);
                scan.message_count += out.iter().filter(|m| m.is_turn()).count();
                if scan.title.is_none() && ty == "message" && role.as_deref().unwrap_or("user") == "user" {
                    if let Some(content) = fields.get("content") {
                        scan.consider_title(&title(content)?);
                    }
                }
            }
        }
        Ok(scan)
    })?
    .unwrap_or_default())
}

// Match metadata::count_text over a checked source without owning its complete decoded string.
fn count_text(view: JsonView<'_>) -> Result<Option<String>> {
    let Some(text) = view.source_text()? else {
        return Ok(None);
    };
    let (head, present) = crate::harness::store::document::text_head(&text, &crate::Resolver::new(None), 96, true)?;
    Ok(Some(if head.is_empty() && present { " ".into() } else { head }))
}
pub(super) fn count_content(view: JsonView<'_>) -> Result<Value> {
    if let Some(text) = count_text(view)? {
        return Ok(text.into());
    }
    let Some(parts) = view.array()? else {
        return Ok(Value::Null);
    };
    let mut out = Vec::new();
    for part in parts {
        let part = part?;
        let mut p = Map::new();
        if let Some(fields) = Fields::parse(part, &["type", "text"])? {
            if let Some(ty) = fields.string("type")? {
                p.insert("type".into(), ty.into());
            }
            if let Some(text) = fields.get("text").map(count_text).transpose()?.flatten() {
                p.insert("text".into(), text.into());
            }
        }
        out.push(Value::Object(p));
    }
    Ok(Value::Array(out))
}

/// Value's title is text_parts(content).trim(), then truncate(..., 80). A title can contain
/// arbitrarily long leading/trailing whitespace, so keep the first 81 characters and the last
/// nonwhite position until the answer is determined; joined empty parts still add newlines.
pub(super) fn title(content: JsonView<'_>) -> Result<String> {
    let mut head = String::new();
    let mut chars = 0usize;
    let mut last_nonwhite = 0usize;
    let mut feed = |chunk: &str| -> Result<()> {
        for ch in chunk.chars() {
            if head.is_empty() && ch.is_whitespace() {
                continue;
            }
            if chars < 81 {
                head.push(ch);
                chars += 1;
            }
            if !ch.is_whitespace() {
                last_nonwhite = chars;
            }
            if last_nonwhite == 81 {
                return Err(TitleComplete.into());
            }
        }
        Ok(())
    };
    let result = (|| -> Result<()> {
        if content.is_string()? {
            content.for_each_str_chunk(&mut feed)?;
        } else if let Some(parts) = content.array()? {
            let mut prior = false;
            for part in parts {
                if let Some(text) = part?.get("text")? {
                    if text.is_string()? {
                        if prior {
                            feed("\n")?;
                        }
                        text.for_each_str_chunk(&mut feed)?;
                        prior = true;
                    }
                }
            }
        }
        Ok(())
    })();
    match result {
        Err(error) if error.is::<TitleComplete>() => {}
        other => other?,
    }
    let trimmed: String = head.chars().take(last_nonwhite).collect();
    Ok(crate::ir::truncate(&trimmed, 80))
}
#[derive(Debug, thiserror::Error)]
#[error("legacy title prefix complete")]
struct TitleComplete;

pub(super) fn stream(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    let empty = || Session {
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Codex)
    };
    let parsed = read(&r.path, |source, root| {
        let mut s = Session {
            source_path: Some(r.path.clone()),
            ..Session::new(String::new(), Harness::Codex)
        };
        // Legacy parse has never enabled complete carriers or modern event authority/inheritance.
        let mut ctx = CodexCtx::default();
        session(root, &mut s, &mut ctx)?;
        if s.id.is_empty() {
            s.id = r.id.clone();
        }
        sink.meta(&s);
        if let Some(error) = sink.take_error() {
            return Err(error);
        }
        let mut scratch = Vec::new();
        if let Some(items) = items(root)? {
            for item in items {
                let item = item?;
                if let Some(projection) = selected::project_item(item, source, opts.spans)? {
                    projection.dispatch_item(&mut scratch, &mut ctx);
                } else if !project_output(item, source, opts.spans, &mut scratch, &mut ctx)? {
                    // Unrecognized entries have no conversational shape, even under complete.
                    // Resolve only the fields read by a recognized fallback item.
                    if let Some(value) = fallback(item)? {
                        handle_item(Some(&value), false, None, &mut scratch, &mut ctx);
                    }
                }
                for message in scratch.drain(..) {
                    if sink.try_message(message)? == Flow::Stop {
                        return Ok(s);
                    }
                }
            }
        }
        Ok(s)
    })?;
    match parsed {
        Some(s) => Ok(s),
        None => crate::stream::replay(empty(), sink),
    }
}

fn project_output(
    item: JsonView<'_>,
    source: &JsonSource,
    spans: bool,
    scratch: &mut Vec<Message>,
    ctx: &mut CodexCtx,
) -> Result<bool> {
    let Some(fields) = Fields::parse(
        item,
        &[
            "type",
            "call_id",
            "name",
            "namespace",
            "output",
            "internal_chat_message_metadata_passthrough",
        ],
    )?
    else {
        return Ok(false);
    };
    if !matches!(
        fields.string("type")?.as_deref(),
        Some("function_call_output" | "custom_tool_call_output")
    ) {
        return Ok(false);
    }
    let output = fields.get("output");
    if !spans {
        let value = Value::Object(fields.project(&[
            "type",
            "call_id",
            "name",
            "namespace",
            "output",
            "internal_chat_message_metadata_passthrough",
        ])?);
        handle_item(Some(&value), false, None, scratch, ctx);
        return Ok(true);
    }
    let (mut body, images) = output_text(output, source)?;
    let mut error = false;
    if let Some(output) = output {
        if output.is_string()? {
            error = body
                .try_resolve_prefix(&crate::Resolver::new(None), 64)?
                .starts_with(ERROR_PREFIX);
            if error {
                body = body.with_skipped_prefix(ERROR_PREFIX.len())?;
            }
        } else if let Some(markers) = Fields::parse(output, &["success", "metadata"])? {
            error = markers.get("success").map(JsonView::as_bool).transpose()?.flatten() == Some(false)
                || markers
                    .get("metadata")
                    .map(|v| v.get("exit_code"))
                    .transpose()?
                    .flatten()
                    .map(JsonView::as_number)
                    .transpose()?
                    .flatten()
                    .and_then(|v| v.as_i64())
                    .is_some_and(|v| v != 0);
        }
    }
    let value = Value::Object(fields.project(&[
        "type",
        "call_id",
        "name",
        "namespace",
        "internal_chat_message_metadata_passthrough",
    ])?);
    let before = scratch.len();
    handle_item_with_output(Some(&value), false, None, scratch, ctx, Some((body, error)));
    if let Some(message) = scratch.get_mut(before) {
        message.content.extend(images);
    }
    Ok(true)
}

fn output_text(output: Option<JsonView<'_>>, source: &JsonSource) -> Result<(Text, Vec<Block>)> {
    let Some(output) = output else {
        return Ok((Text::from(""), Vec::new()));
    };
    if let Some(text) = output.source_text()? {
        return Ok((text, Vec::new()));
    }
    if let Some(parts) = output.array()? {
        let mut images = Vec::new();
        let body = text::append(source, &source.decoded_backing()?, |builder| {
            let mut prior = false;
            for part in parts {
                let part = part?;
                let ty = part
                    .get("type")?
                    .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
                    .transpose()?
                    .unwrap_or_default();
                match ty.as_str() {
                    "input_image" | "output_image" => {
                        images.extend(openai::part_to_block(part.resolve()?.as_ref(), &openai::CODEX));
                    }
                    "encrypted_content" => {}
                    _ => {
                        if let Some(value) = part.get("text")? {
                            if value.is_string()? {
                                if prior {
                                    builder.push("\n")?;
                                }
                                builder.push_string(value)?;
                                prior = true;
                            }
                        }
                    }
                }
            }
            Ok(())
        })?;
        return Ok((body, images));
    }
    if let Some(fields) = Fields::parse(output, &["output", "content"])? {
        if let Some(inner) = fields.get("output") {
            if inner.is_string()? || inner.is_array()? {
                return output_text(Some(inner), source);
            }
        }
        if let Some(content) = fields.get("content") {
            if let Some(text) = content.source_text()? {
                return Ok((text, Vec::new()));
            }
        }
    }
    let body = text::append(source, &source.decoded_backing()?, |builder| builder.push_json(output))?;
    Ok((body, Vec::new()))
}

fn fallback(item: JsonView<'_>) -> Result<Option<Value>> {
    let ty = item
        .get("type")?
        .map(|v| v.resolve().map(|v| v.as_str().unwrap_or("").to_owned()))
        .transpose()?
        .unwrap_or_default();
    let keys: &[&str] = match ty.as_str() {
        "message" => &["role", "phase", "content"],
        "reasoning" => &["summary", "content", "encrypted_content"],
        "agent_message" => &["author", "recipient", "content"],
        "function_call" | "custom_tool_call" => &["id", "call_id", "name", "namespace", "status", "arguments", "input"],
        "local_shell_call" => &["id", "call_id", "action", "status", "namespace"],
        "tool_search_call" => &["call_id", "arguments", "execution"],
        "tool_search_output" => &["call_id", "tools"],
        "web_search_call" => &["id", "call_id", "action", "status"],
        "image_generation_call" => &["revised_prompt"],
        "compaction" | "compaction_summary" | "context_compaction" => &[],
        _ => return Ok(None),
    };
    let mut p = Map::new();
    p.insert("type".into(), ty.clone().into());
    for key in keys
        .iter()
        .copied()
        .chain(["internal_chat_message_metadata_passthrough"])
    {
        if let Some(view) = item.get(key)? {
            p.insert(key.into(), view.resolve()?.into_owned());
        }
    }
    let present = match ty.as_str() {
        "image_generation_call" => Some("result"),
        "compaction" | "compaction_summary" | "context_compaction" => Some("encrypted_content"),
        _ => None,
    };
    if let Some(key) = present {
        if item.get(key)?.map(JsonView::is_string).transpose()?.unwrap_or(false) {
            p.insert(key.into(), "".into());
        }
    }
    Ok(Some(Value::Object(p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn oracle(path: &Path, raw: &str) -> Value {
        serde_json::to_value(parse_str("fallback", raw, false, Some(path.to_path_buf()))).unwrap()
    }
    fn scan_oracle(raw: &str) -> CodexScan {
        let mut scan = CodexScan::default();
        if let Ok(root) = serde_json::from_str::<Value>(raw) {
            scan.feed_json_object(&root);
        }
        scan
    }
    fn collected(path: &Path, opts: &ParseOptions) -> Session {
        let r = SessionRef::new("fallback", Harness::Codex, path);
        let mut sink = crate::stream::CollectSink::default();
        let mut session = Codex { roots: Vec::new() }.stream(&r, opts, &mut sink).unwrap();
        session.messages = sink.messages;
        session
    }

    #[test]
    fn checked_legacy_items_match_owned_full_and_complete_oracles() {
        let world = cv_testkit::World::in_process("codex-legacy-checked");
        let path = world.base.join("rollout-legacy.json");
        let giant = "🌙\n\\\"".repeat(8192);
        let metadata = json!({"create_time":1234.5,"turn_id":"turn"});
        let mut entries = vec![
            json!({"type":"unknown","ignored":giant}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"not legacy authority"}}),
            json!({"type":"response_item","payload":{"type":"message","content":"not a direct item"}}),
            json!({"type":"reasoning","summary":[{"text":"summary"}],"content":[{"text":giant},{"text":""}],"encrypted_content":"blob"}),
            json!({"type":"reasoning","summary":giant,"content":[],"encrypted_content":null}),
            json!({"type":"agent_message","author":"/child","recipient":"/root","content":[{"type":"input_text","text":"task"},{"type":"encrypted_content","encrypted_content":giant}]}),
            json!({"type":"agent_message","author":"/root","recipient":"/child","content":[]}),
            json!({"type":"local_shell_call","id":"local","status":"done","action":{"type":"exec","command":[giant]},"namespace":"shell"}),
            json!({"type":"tool_search_call","call_id":"search","arguments":{"keyword":"needle"},"execution":"client"}),
            json!({"type":"tool_search_output","call_id":"search","tools":[{"name":"one"}]}),
            json!({"type":"web_search_call","id":"web","action":{"query":"needle"},"status":"done"}),
            json!({"type":"image_generation_call","result":giant,"revised_prompt":"prompt"}),
            json!({"type":"context_compaction","encrypted_content":giant}),
            json!({"type":"message","content":{},"role":false}),
        ];
        for role in ["user", "assistant", "developer", "system", "other"] {
            entries.push(json!({"type":"message","role":role,"phase":"commentary","content":[
                {"type":"input_text","text":format!("{}# AGENTS.md instructions {giant}","\u{2003}".repeat(5000))},
                {"type":"other","text":giant},{"type":"input_text","text":""},
                {"type":"input_image","image_url":"data:image/png;base64,AAA"},
                {"type":"audio_url","audio_url":{"url":"file:///sound"}},
            ]}));
        }
        for args in [
            json!({"key":giant}).to_string(),
            "42".into(),
            "{partial".into(),
            " \t".into(),
        ] {
            entries.push(json!({"type":"function_call","arguments":args,"call_id":"call","name":"tool","namespace":"ns","status":"done"}));
            entries.push(json!({"type":"custom_tool_call","input":args,"call_id":"custom","name":"apply_patch"}));
        }
        for output in [
            json!(format!("[error] {giant}")),
            json!(giant),
            json!(null),
            json!(42),
            json!({"output":giant,"success":false}),
            json!({"output":[{"text":""},{"text":giant},{"type":"input_image","image_url":"https://image"}],"metadata":{"exit_code":2}}),
            json!({"content":giant,"success":true}),
            json!({"arbitrary":giant,"success":false}),
            json!([{"type":"input_text","text":giant},{"type":"encrypted_content","text":"hidden"},{"text":""},{"type":"input_image","image_url":"https://image"}]),
        ] {
            entries.push(
                json!({"type":"function_call_output","call_id":"call","name":"tool","namespace":"ns","output":output}),
            );
        }
        for item in &mut entries {
            item.as_object_mut()
                .unwrap()
                .insert("internal_chat_message_metadata_passthrough".into(), metadata.clone());
            item.as_object_mut()
                .unwrap()
                .insert("ignored".into(), giant.clone().into());
        }
        let meta = json!({"id":"legacy","timestamp":"2025-05-05T19:24:54Z","cwd":"/workspace","agent_path":"/root","history_mode":"legacy","subagent_history_start_ordinal":999,
            "base_instructions":{"text":"system"},"git":{"branch":"main","commit_hash":"hash","repository_url":"remote"},"ignored":giant});
        for root in [json!({"session":meta,"items":entries}), json!(entries)] {
            let raw = root.to_string();
            fs::write(&path, &raw).unwrap();
            let expected = oracle(&path, &raw);
            assert_eq!(scan(&path).unwrap(), scan_oracle(&raw));
            for opts in [
                ParseOptions::lazy(),
                ParseOptions::lazy_extra(),
                ParseOptions::full(),
                ParseOptions::complete(),
            ] {
                let mut actual = collected(&path, &opts);
                actual.materialize().unwrap();
                assert_eq!(serde_json::to_value(actual).unwrap(), expected, "{opts:?}");
            }
        }
        // Value's final duplicate fields, including escaped keys, remain authoritative.
        let raw = r#"{"session":{"id":"first","id":"last"},"items":[{"type":"message","content":"old","co\u006etent":"new"}],"items":[{"type":"message","content":"winner"}]}"#;
        fs::write(&path, raw).unwrap();
        let mut actual = collected(&path, &ParseOptions::lazy());
        actual.materialize().unwrap();
        assert_eq!(serde_json::to_value(actual).unwrap(), oracle(&path, raw));
        assert_eq!(scan(&path).unwrap(), scan_oracle(raw));
    }

    #[test]
    fn checked_legacy_discovery_titles_and_counts_match_owned_oracle() {
        let world = cv_testkit::World::in_process("codex-legacy-discovery");
        let path = world.base.join("rollout-legacy.json");
        let whitespace = " \t\n\u{2003}".repeat(8192);
        let tail = "🌙\n\\\"".repeat(8192);
        let mut bodies = vec![
            json!(""),
            json!(&whitespace),
            json!(format!("{whitespace}# AGENTS.md instructions{tail}")),
            json!(format!("{whitespace}title{whitespace}")),
            json!(format!("{}{whitespace}", "x".repeat(80))),
            json!(format!("{}{whitespace}tail", "x".repeat(80))),
            json!("🌙".repeat(81)),
            json!([{"text":""},{"text":whitespace},{"text":" actual"},{"text":tail}]),
            json!([{"type":"input_image","image_url":"https://image","text":"title"}]),
            json!([{"type":"audio_url","audio_url":{"url":"file:///sound"}}]),
        ];
        bodies.extend([json!({"text":tail}), json!(null), json!(42), json!(true)]);
        for (case, body) in bodies.into_iter().enumerate() {
            for role in [
                json!("user"),
                json!("assistant"),
                json!("developer"),
                json!("unknown"),
                json!(null),
                json!(42),
            ] {
                let mut entries = vec![json!({"type":"message","role":role,"content":body})];
                entries.push(json!({"type":"message","content":"next title"}));
                for encrypted in [json!(null), json!(""), json!(tail), json!({"ignored":tail})] {
                    entries.push(json!({"type":"reasoning","content":body,"summary":[{"text":""},{"text":"summary"}],"encrypted_content":encrypted}));
                }
                entries.push(json!({"type":"unknown","content":tail,"arguments":tail}));
                let root = json!({"session":{"id":"legacy","timestamp":"2025-05-05T19:24:54Z","cwd":"ignored cwd","agent_path":"/root","ignored":tail},"items":entries});
                let raw = root.to_string();
                fs::write(&path, &raw).unwrap();
                assert_eq!(scan(&path).unwrap(), scan_oracle(&raw), "case={case}, role={role:?}");
            }
        }
        // Body prefix classification and title joining remain distinct, including empty parts.
        for raw in [
            r##"{"items":[{"type":"message","content":[{"text":" #"},{"text":" AGENTS.md instructions"}]}]}"##,
            r##"{"items":[{"type":"message","content":[{"text":""},{"text":" # AGENTS.md instructions"}]}]}"##,
            r#"{"session":{"id":"first","timestamp":false},"session":{"id":"last","timestamp":"2025-05-05T19:24:54Z"},"items":[{"type":"message","role":"assistant","role":null,"content":"first","content":"winner"}]}"#,
        ] {
            fs::write(&path, raw).unwrap();
            assert_eq!(scan(&path).unwrap(), scan_oracle(raw));
        }
    }

    #[test]
    fn checked_legacy_acceptance_stop_and_source_failures_are_preserved() {
        let world = cv_testkit::World::in_process("codex-legacy-acceptance");
        let path = world.base.join("rollout-legacy.json");
        let valid_prefix = r#"{"items":[{"type":"message","content":"visible"}],"ignored":"#;
        let deep = format!("{valid_prefix}{}0{} }}", "[".repeat(130), "]".repeat(130));
        for raw in [
            "",
            " ",
            "false",
            "{}",
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":"\ud800"}"#,
            &deep,
            r#"{"items":[{"type":"message","content":"visible"}],"ignored":1e9999}"#,
            r#"{"items":[{"type":"message","content":"visible"}]"#,
        ] {
            fs::write(&path, raw).unwrap();
            let actual = collected(&path, &ParseOptions::lazy());
            assert_eq!(serde_json::to_value(actual).unwrap(), oracle(&path, raw), "{raw}");
            assert_eq!(scan(&path).unwrap(), scan_oracle(raw), "{raw}");
        }
        let mut bad_utf8 = valid_prefix.as_bytes().to_vec();
        bad_utf8.extend_from_slice(b"\"\xff\"}");
        fs::write(&path, bad_utf8).unwrap();
        let r = SessionRef::new("fallback", Harness::Codex, &path);
        let mut sink = crate::stream::CollectSink::default();
        assert!(stream(&r, &ParseOptions::lazy(), &mut sink).is_err());
        assert!(sink.messages.is_empty());
        assert!(scan(&path).is_err());

        let raw = json!({"session":{"id":"legacy","timestamp":"2025-05-05T19:24:54Z"},"items":[
            {"type":"message","content":"body".repeat(8192)}, {"type":"message","content":"second"},
        ]})
        .to_string();
        fs::write(&path, &raw).unwrap();
        let mut saved = None;
        let mut stop = |m: Message| {
            saved = Some(m);
            Flow::Stop
        };
        let meta = stream(&r, &ParseOptions::lazy(), &mut stop).unwrap();
        assert_eq!(meta.id, "legacy");
        let saved = saved.unwrap();
        assert!(matches!(
            saved.content.first(),
            Some(Block::Text { text: Text::Source(_) })
        ));
        fs::write(&path, "replacement source").unwrap();
        if let Block::Text { text } = &saved.content[0] {
            assert!(text.try_resolve(&crate::Resolver::new(None)).is_err());
        }
        fs::write(&path, &raw).unwrap();
        let mut mutate = |_m: Message| {
            fs::write(&path, "replacement source").unwrap();
            Flow::Stop
        };
        assert!(stream(&r, &ParseOptions::lazy(), &mut mutate).is_err());
    }
}
