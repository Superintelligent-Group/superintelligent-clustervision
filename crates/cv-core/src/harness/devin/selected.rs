//! Selected Devin rows keep bodies/arguments in guarded backing, while the established mapper
//! still owns precisely the metadata its consumers demand.
use super::*;
use crate::harness::store::{bytes::ByteSource, document::Fields};
use crate::json::{JsonSource, JsonView};
use std::sync::Arc;

type CellRanges = HashMap<(i64, &'static str), (u64, u64)>;

pub(super) struct Selected {
    source: JsonSource,
    arena: Arc<ByteSource>,
    cells: std::cell::RefCell<CellRanges>,
}
impl Selected {
    pub(super) fn new(path: &Path, reads: &crate::source::ReadSet) -> Result<Self> {
        Ok(Self {
            source: JsonSource::capture_with_reads(path, reads)?,
            arena: Arc::new(ByteSource::arena()?),
            cells: Default::default(),
        })
    }
    pub(super) fn read<T>(
        &self,
        conn: &Connection,
        rowid: i64,
        f: impl FnOnce(JsonView<'_>) -> Result<T>,
    ) -> Result<T> {
        let (offset, len) = self.cell(conn, rowid, "chat_message")?;
        self.source.read_backing_value(self.arena.clone(), offset, len, f)
    }
    fn cell(&self, conn: &Connection, rowid: i64, column: &'static str) -> Result<(u64, u64)> {
        if let Some(range) = self.cells.borrow().get(&(rowid, column)).copied() {
            return Ok(range);
        }
        let range = super::super::store::sqlite::append_cell(conn, "message_nodes", column, rowid, &self.arena)?;
        self.cells.borrow_mut().insert((rowid, column), range);
        Ok(range)
    }
    pub(super) fn metadata(&self, conn: &Connection, rowid: i64) -> Result<Option<Value>> {
        let kind: String = conn.query_row(
            "SELECT typeof(metadata) FROM message_nodes WHERE rowid=?1",
            [rowid],
            |row| row.get(0),
        )?;
        if kind != "text" {
            return Ok(None);
        }
        let (offset, len) = self.cell(conn, rowid, "metadata")?;
        match self.source.read_backing_value(self.arena.clone(), offset, len, |view| {
            if !view.is_object()? {
                return Ok(None);
            }
            let mut projected = fields(view, &["is_system_prefix", "summarized_from", "num_tokens_preceding"])?;
            if let Some(ext) = view.get("extensions")? {
                projected["extensions"] = fields(ext, &["compact/prior_node_ids"])?;
            }
            Ok(Some(projected))
        }) {
            Err(error) if error.downcast_ref::<crate::json::InvalidJson>().is_some() => Ok(None),
            result => result,
        }
    }
    pub(super) fn node(
        &self,
        conn: &Connection,
        rowid: i64,
        nid: i64,
        created: i64,
        metadata: Option<&Value>,
        tools: &mut HashMap<String, String>,
    ) -> Result<Message> {
        self.read(conn, rowid, |view| node(view, nid, created, metadata, tools))
    }
}
fn fields(view: JsonView<'_>, keys: &[&'static str]) -> Result<Value> {
    let Some(fields) = Fields::parse(view, keys)? else {
        return Ok(Value::Object(Default::default()));
    };
    Ok(Value::Object(fields.project(keys)?))
}
fn metadata(view: JsonView<'_>) -> Result<Value> {
    let mut out = fields(
        view,
        &[
            "created_at",
            "is_user_input",
            "generation_model",
            "finish_reason",
            "request_id",
            "telemetry",
            "num_tokens",
        ],
    )?;
    if let Some(metrics) = view.get("metrics")? {
        out["metrics"] = fields(
            metrics,
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
            ],
        )?;
    }
    if let Some(ext) = view.get("extensions")? {
        let mut reduced = fields(
            ext,
            &[
                "subagent/handoff",
                "affogato/cog-context",
                "chisel/user_question_answers",
                "subagent/profile_name",
                "subagent/model",
                "subagent/agent_id",
                "subagent/chain_node_id",
                "chisel/fusion_lead_model_uid",
                "agent-ext/rules-loaded",
            ],
        )?;
        // These extension objects are reduced by node_to_message. Selecting the same fields
        // here avoids owning embedded terminal output or unrelated producer telemetry first.
        for (key, keys) in [
            ("chisel/tool_result_meta", &["success", "kind"][..]),
            ("chisel/tool_failure", &["reason"][..]),
            ("chisel/tool_call_timing", &["duration_ms"][..]),
        ] {
            if let Some(value) = ext.get(key)? {
                reduced[key] = fields(value, keys)?;
            }
        }
        if let Some(terminal) = ext.get("chisel/terminal_output")? {
            let mut value = fields(terminal, &["cwd"])?;
            if let Some(exit) = terminal.get("exit")? {
                value["exit"] = fields(exit, &["exit_code", "terminal_id"])?;
            }
            reduced["chisel/terminal_output"] = value;
        }
        if let Some(undo) = ext
            .get("chisel/undo")?
            .and_then(|v| v.array().transpose())
            .transpose()?
        {
            let items = undo
                .map(|item| fields(item?, &["kind", "path"]))
                .collect::<Result<Vec<_>>>()?;
            reduced["chisel/undo"] = Value::Array(items);
        }
        if let Some(skills) = ext
            .get("agent-ext/skills-loaded")?
            .and_then(|v| v.get("skills").transpose())
            .transpose()?
            .and_then(|v| v.array().transpose())
            .transpose()?
        {
            let items = skills
                .map(|item| fields(item?, &["name", "path"]))
                .collect::<Result<Vec<_>>>()?;
            reduced["agent-ext/skills-loaded"] = json!({"skills":items});
        }
        if let Some(content) = ext.get("chisel/tool_call_content")? {
            let mut calls = serde_json::Map::new();
            if content.for_each_entry(|id, value| {
                calls.insert(id.into(), fields(value, &["title", "kind", "status"])?);
                Ok(())
            })? {
                reduced["chisel/tool_call_content"] = Value::Object(calls);
            }
        }
        out["extensions"] = reduced;
    }
    Ok(out)
}
fn nonempty_text(view: Option<JsonView<'_>>) -> Result<Option<crate::Text>> {
    let Some(view) = view else { return Ok(None) };
    // The accepted source string includes both quotes. Only the two-byte string is
    // empty: every valid escape encodes at least one decoded scalar. Avoid a second
    // full-body traversal merely to distinguish empty content from present content.
    if let Some((_, len)) = view.source_range()? {
        return if len == 2 { Ok(None) } else { view.source_text() };
    }
    let Some(text) = view.source_text()? else {
        return Ok(None);
    };
    Ok((text.try_decoded_len(&crate::lazy::Resolver::new(None))? != 0).then_some(text))
}
fn node(
    view: JsonView<'_>,
    nid: i64,
    created: i64,
    node_meta: Option<&Value>,
    tools: &mut HashMap<String, String>,
) -> Result<Message> {
    let mut skeleton = fields(view, &["role", "message_id", "tool_call_id"])?;
    if let Some(meta) = view.get("metadata")? {
        skeleton["metadata"] = metadata(meta)?;
    }
    let content = nonempty_text(view.get("content")?)?;
    let assistant = skeleton.get("role").and_then(Value::as_str) == Some("assistant");
    let thinking = if assistant { view.get("thinking")? } else { None };
    let thought = nonempty_text(thinking.and_then(|v| v.get("thinking").transpose()).transpose()?)?;
    if content.is_some() {
        skeleton["content"] = json!("x");
    }
    if let Some(thinking) = thinking {
        let mut value = fields(thinking, &["signature"])?;
        if thought.is_some() {
            value["thinking"] = json!("x");
        }
        skeleton["thinking"] = value;
    }
    let mut arguments = Vec::new();
    if let Some(calls) = (if assistant { view.get("tool_calls")? } else { None })
        .and_then(|v| v.array().transpose())
        .transpose()?
    {
        let mut list = Vec::new();
        for call in calls {
            let call = call?;
            list.push(fields(call, &["id", "name"])?);
            arguments.push(match call.get("arguments")? {
                Some(input) => input.to_json()?,
                None => Value::Null.into(),
            });
        }
        skeleton["tool_calls"] = Value::Array(list);
    }
    let mut message = node_to_message(nid, created, &skeleton, node_meta, tools);
    let mut arguments = arguments.into_iter();
    for block in &mut message.content {
        match block {
            Block::Text { text } => *text = content.clone().expect("mapper emitted selected text"),
            Block::Thinking { text, .. } => *text = thought.clone().unwrap_or_default(),
            Block::ToolResult { content: text, .. } => *text = content.clone().unwrap_or_default(),
            Block::ToolUse { input, .. } => *input = arguments.next().expect("mapper emitted selected call"),
            _ => {}
        }
    }
    Ok(message)
}

pub(super) fn compaction(view: JsonView<'_>) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    if let Some(ext) = view
        .get("metadata")?
        .and_then(|v| v.get("extensions").transpose())
        .transpose()?
    {
        if let Some(src) = ext
            .get("devin-rs/summary")?
            .and_then(|v| v.get("source").transpose())
            .transpose()?
        {
            if !src.is_null()? {
                out.insert("source".into(), src.resolve()?.into_owned());
            }
        }
        if let Some(paths) = ext
            .get("compact/edited_files")?
            .and_then(|v| v.get("paths").transpose())
            .transpose()?
        {
            out.insert("edited_files".into(), paths.resolve()?.into_owned());
        }
        if let Some(todos) = ext.get("compact/todo_list")? {
            if !todos.is_null()? {
                out.insert("todo_list".into(), todos.resolve()?.into_owned());
            }
        }
    }
    if let Some(content) = view.get("content")? {
        let mut scan = HistoryPath::default();
        let scanned = content.for_each_str_chunk(|chunk| {
            scan.push(chunk);
            if scan.ended {
                return Err(HistoryComplete.into());
            }
            Ok(())
        });
        if let Err(error) = scanned {
            if error.downcast_ref::<HistoryComplete>().is_none() {
                return Err(error);
            }
        }
        // The enclosing checked cell read revalidates original source/backing guards,
        // including after this local successful cutoff. Whole-cell acceptance ran first.
        if let Some(path) = scan.finish() {
            out.insert("history_path".into(), json!(path));
        }
    }
    Ok(out)
}
#[derive(Debug, thiserror::Error)]
#[error("history path complete")]
struct HistoryComplete;

#[derive(Default)]
struct HistoryPath {
    matched: usize,
    found: bool,
    ended: bool,
    path: String,
    prefix: String,
    decided: bool,
}
impl HistoryPath {
    fn word(&mut self, ch: char) {
        if ch.is_whitespace() {
            if !self.path.is_empty() {
                self.ended = true;
            }
        } else {
            self.path.push(ch);
        }
    }
    fn push(&mut self, chunk: &str) {
        const NEEDLE: &str = "Full conversation history saved at ";
        for ch in chunk.chars() {
            if self.ended {
                break;
            }
            if !self.found {
                if ch == NEEDLE.as_bytes()[self.matched] as char {
                    self.matched += 1;
                    if self.matched == NEEDLE.len() {
                        self.found = true;
                    }
                } else {
                    self.matched = usize::from(ch == 'F');
                }
            } else if !self.decided {
                if self.prefix.is_empty() && ch.is_whitespace() {
                    continue;
                }
                self.prefix.push(ch);
                // split(NEEDLE).nth(1) ends before a second delimiter. Disambiguate that
                // bounded prefix before accepting "Full" as the first path token.
                if NEEDLE.starts_with(&self.prefix) {
                    if self.prefix == NEEDLE {
                        self.ended = true;
                    }
                } else {
                    self.decided = true;
                    for c in std::mem::take(&mut self.prefix).chars() {
                        if self.ended {
                            break;
                        }
                        self.word(c);
                    }
                }
            } else {
                self.word(ch);
            }
        }
    }
    fn finish(self) -> Option<String> {
        let token = if !self.decided && !self.ended {
            self.prefix.split_whitespace().next().unwrap_or("")
        } else {
            &self.path
        };
        let path = token.trim_end_matches('.');
        (!path.is_empty()).then(|| path.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_only_usage_and_signature_only_thinking_survive_both_read_paths() -> Result<()> {
        let value = json!({"role":"assistant","thinking":{"signature":"provider signature"},
            "metadata":{"metrics":{"cache_read_tokens":17,"cache_creation_tokens":0}}});
        let owned = node_to_message(1, 1000, &value, None, &mut HashMap::new());
        let usage = owned.usage.as_ref().unwrap();
        assert_eq!(usage.cache_read_tokens, Some(17));
        assert_eq!(usage.cache_creation_tokens, Some(0));
        assert_eq!(usage.prompt_tokens(), 17);
        assert!(matches!(&owned.content[0], Block::Thinking { text, signature, .. }
            if text.is_empty() && signature.as_deref() == Some("provider signature")));
        let selected =
            crate::json::Json::from(value).with_view(|view| node(view, 1, 1000, None, &mut HashMap::new()))?;
        assert_eq!(serde_json::to_value(&selected)?, serde_json::to_value(&owned)?);
        Ok(())
    }

    #[test]
    fn selected_mapper_matches_owned_value_and_compaction() -> Result<()> {
        let world = cv_testkit::World::in_process("devin-selected-node");
        let path = world.base.join("body.json");
        let body = "λ\nquoted \"body\"".repeat(10000);
        let base = json!({"role":"assistant","message_id":"id","content":body,"thinking":{"thinking":body,"signature":"sig"},"tool_calls":[{"id":"call","name":"Read","arguments":{"z":1,"a":body}}],"metadata":{"created_at":"2026-10-13T12:00:00Z","is_user_input":true,"generation_model":"model","metrics":{"input_tokens":7,"output_tokens":3},"extensions":{"chisel/terminal_output":{"cwd":"/cwd","exit":{"exit_code":1,"terminal_id":"term"}},"chisel/tool_result_meta":{"success":false,"kind":"shell"},"chisel/undo":[{"kind":"restore_file","path":"/file","original_content":body}],"agent-ext/skills-loaded":{"skills":[{"name":"n","path":"p","description":body}]},"chisel/tool_call_content":{"call":{"title":"t","kind":"k","status":"s","rawInput":body}},"subagent/handoff":true,"subagent/agent_id":"sidekick","unknown":body,"subagent/handoff_history":body}}});
        for role in ["assistant", "tool", "user", "system", "unknown"] {
            let mut value = base.clone();
            value["metadata"]["metrics"]["discarded_samples"] = json!(body);
            value["metadata"]["extensions"]["chisel/terminal_output"]["stdout"] = json!(body);
            value["metadata"]["extensions"]["chisel/terminal_output"]["exit"]["discarded"] = json!(body);
            value["metadata"]["extensions"]["chisel/tool_result_meta"]["discarded"] = json!(body);
            value["metadata"]["extensions"]["chisel/tool_failure"] = json!({"reason":"failed", "discarded":body});
            value["metadata"]["extensions"]["chisel/tool_call_timing"] = json!({"duration_ms":3, "discarded":body});
            value["role"] = json!(role);
            value["tool_call_id"] = json!("call");
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            let source = JsonSource::capture(&path)?;
            let mut names = HashMap::from([("call".into(), "Read".into())]);
            let metadata = json!({"summarized_from":1,"num_tokens_preceding":9,"is_system_prefix":false});
            let projected =
                source.read_range_value(0, source.len(), |view| super::metadata(view.get("metadata")?.unwrap()))?;
            assert_eq!(projected["metrics"], json!({"input_tokens":7,"output_tokens":3}));
            assert_eq!(
                projected["extensions"]["chisel/terminal_output"],
                json!({"cwd":"/cwd","exit":{"exit_code":1,"terminal_id":"term"}})
            );
            let old = node_to_message(7, 1000, &value, Some(&metadata), &mut names);
            let mut selected =
                source.read_range_value(0, source.len(), |view| node(view, 7, 1000, Some(&metadata), &mut names))?;
            let mut session = Session::new("test", Harness::Devin);
            session.messages.push(selected.clone());
            session.materialize()?;
            selected = session.messages.remove(0);
            assert_eq!(serde_json::to_value(selected)?, serde_json::to_value(old)?, "{role}");
        }
        for raw in [r#""""#, r#""\u0000""#, r#""\n""#, r#""λ""#, "null", "12", "[]"] {
            std::fs::write(&path, raw)?;
            let source = JsonSource::capture(&path)?;
            let present = source.read_range_value(0, source.len(), |view| nonempty_text(Some(view)))?;
            let owned: Value = serde_json::from_str(raw)?;
            assert_eq!(
                present.is_some(),
                owned.as_str().is_some_and(|text| !text.is_empty()),
                "{raw}"
            );
        }
        for content in [
            "Full conversation history saved at /a/b.md.",
            "λ Full conversation history saved at \n\t /a/b.md... tail",
            "no history",
            "Full conversation history saved at ",
            "Full Full conversation history saved at /file.",
            "Full conversation history saved at Full conversation history saved at /second",
            "Full conversation history saved at Full convo",
        ] {
            let value = json!({"content":content,"metadata":{"extensions":{"devin-rs/summary":{"source":"async"},"compact/edited_files":{"paths":["/file"]},"compact/todo_list":[1],"subagent/handoff_history":body}}});
            std::fs::write(&path, serde_json::to_vec(&value)?)?;
            let source = JsonSource::capture(&path)?;
            let facts = source.read_range_value(0, source.len(), compaction)?;
            let expected = content
                .split("Full conversation history saved at ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .map(|s| s.trim_end_matches('.'))
                .filter(|s| !s.is_empty());
            assert_eq!(facts.get("history_path").and_then(Value::as_str), expected);
            for size in [1, 7, 31, 64] {
                let mut scan = HistoryPath::default();
                for part in content.as_bytes().chunks(size) {
                    if let Ok(part) = std::str::from_utf8(part) {
                        scan.push(part)
                    }
                }
                assert_eq!(scan.finish().as_deref(), expected);
            }
        }
        Ok(())
    }
}
