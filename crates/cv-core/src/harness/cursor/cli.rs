//! Cursor Agent CLI: hex JSON metadata and content-addressed protobuf/JSON blobs.
//! Root/summary references replay one message at a time under one SQLite snapshot. The
//! protobuf schema was recovered from the installed 2026.10.01-e373342 bundle; the corpus
//! contains actual CLI-written data, not reconstructed protocol examples.
use super::*;
use crate::harness::store::bytes::ByteSource;
use std::io::Read;

pub(super) fn roots(home: &Path) -> Vec<PathBuf> {
    ["chats", "acp-sessions"]
        .into_iter()
        .map(|name| home.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

pub(super) fn discover(home: &Path, out: &mut Vec<SessionRef>) {
    for root in roots(home) {
        for entry in walkdir::WalkDir::new(root).max_depth(3).into_iter().flatten() {
            if !entry.file_type().is_file() || entry.file_name() != "store.db" {
                continue;
            }
            let path = entry.path();
            let Ok(conn) = open_ro(path) else { continue };
            let result = (|| -> Result<SessionRef> {
                let mut collector = Count::default();
                let r = SessionRef::new("cli:", Harness::Cursor, path);
                let session = read(&conn, &r, &ParseOptions::default(), true, &mut collector)?;
                Ok(SessionRef {
                    id: session.id,
                    cwd: session.cwd,
                    title: session.title.or(collector.title),
                    created_at: session.created_at,
                    updated_at: session.updated_at,
                    message_count: collector.count,
                    ..r
                })
            })();
            if let Ok(reference) = result {
                // An empty or uninitialized native store is not a conversation.
                if reference.message_count > 0 {
                    out.push(reference);
                }
            }
        }
    }
}

#[derive(Default)]
struct Count {
    count: usize,
    title: Option<String>,
}
impl MessageSink for Count {
    fn message(&mut self, message: Message) -> Flow {
        self.count += usize::from(message.is_turn());
        if self.title.is_none() && message.kind == MessageKind::Prompt && message.origin == Origin::Human {
            self.title = message
                .text()
                .filter(|text| !text.trim().is_empty())
                .map(|text| crate::ir::truncate(&text, 80));
        }
        Flow::Continue
    }
}

pub(super) fn stream(
    conn: &Connection,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    read(conn, r, opts, false, sink)
}

fn read(
    conn: &Connection,
    r: &SessionRef,
    opts: &ParseOptions,
    counting: bool,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let lazy = opts.spans && !opts.extra && !opts.complete;
    let captured = if counting || lazy {
        // Capture the compound provenance before the first SQL read establishes its snapshot.
        // A checkpoint can leave the DB tuple unchanged while the WAL or optional ACP head changes.
        let mut reads = crate::source::ReadSet::default();
        let mut wal = r.path.as_os_str().to_os_string();
        wal.push("-wal");
        reads.record(&PathBuf::from(wal));
        if let Some(meta) = crate::source::cursor_cli_metadata(r) {
            reads.record(&meta);
        }
        Some(crate::json::JsonSource::capture_with_reads(&r.path, &reads)?)
    } else {
        None
    };
    // One operation-owned arena serves every retained message range; no per-message descriptor
    // or private-file growth. Counting releases each transient blob after its bounded projection.
    let arena = lazy.then(|| ByteSource::arena().map(std::sync::Arc::new)).transpose()?;
    let result = read_snapshot(conn, r, opts, counting, sink, captured.as_ref(), arena.as_ref());
    // Even Stop and callback errors must not hide a source mutation before publication.
    if let Some(captured) = &captured {
        captured.check()?;
    }
    result
}

fn read_snapshot(
    conn: &Connection,
    r: &SessionRef,
    opts: &ParseOptions,
    counting: bool,
    sink: &mut dyn MessageSink,
    captured: Option<&crate::json::JsonSource>,
    arena: Option<&std::sync::Arc<ByteSource>>,
) -> Result<Session> {
    let _snapshot = conn.is_autocommit().then(|| conn.unchecked_transaction()).transpose()?;
    let encoded: String = conn.query_row("SELECT value FROM meta WHERE key='0'", [], |row| row.get(0))?;
    let metadata: Value = serde_json::from_slice(&decode_hex(&encoded)?).context("Cursor CLI metadata JSON")?;
    let id = metadata
        .get("agentId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("Cursor CLI agentId missing")?;
    if !counting {
        anyhow::ensure!(
            r.id == format!("cli:{id}"),
            "Cursor CLI store identity changed since discovery"
        );
    }
    let root_id = metadata
        .get("latestRootBlobId")
        .and_then(Value::as_str)
        .context("Cursor CLI root missing")?;
    anyhow::ensure!(
        decode_hex(root_id)?.len() == 32,
        "Cursor CLI root is not a SHA-256 identifier"
    );
    let root = blob(conn, root_id)?;
    let mut session = Session {
        source_path: Some(r.path.clone()),
        title: metadata
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && *name != "New Agent")
            .map(str::to_owned),
        created_at: metadata
            .get("createdAt")
            .and_then(Value::as_i64)
            .and_then(|ms| ts_from_epoch(ms as f64)),
        ..Session::new(format!("cli:{id}"), Harness::Cursor)
    };
    session
        .harness_extra_mut(Harness::Cursor)
        .insert("store".into(), "agent-cli".into());
    for key in ["mode", "isRunEverything", "approvalMode"] {
        if let Some(value) = metadata.get(key) {
            session
                .harness_extra_mut(Harness::Cursor)
                .insert(key.into(), value.clone());
        }
    }
    if let Some(info) = metadata.get("subagentInfo") {
        session.lineage.parent = info
            .get("parentAgentId")
            .and_then(Value::as_str)
            .map(|id| format!("cli:{id}"));
        if let Some(root) = info.get("rootParentAgentId") {
            session
                .harness_extra_mut(Harness::Cursor)
                .insert("root_parent_agent_id".into(), root.clone());
        }
        session.lineage.spawned_by_tool_use = info.get("toolCallId").and_then(Value::as_str).map(str::to_owned);
        session.lineage.agent_path = info.get("typeName").and_then(Value::as_str).map(str::to_owned);
    }
    session.model = metadata.get("lastUsedModel").and_then(Value::as_str).map(str::to_owned);
    // First validate the entire root while extracting only demanded metadata. No callbacks have
    // run yet, and repeated blob references do not become a growing Vec.
    fields(root.reader()?, &mut |number, wire, value, input| {
        match (number, wire) {
            (9, 2) if session.cwd.is_none() && value <= 65536 => {
                let mut uri = String::new();
                input.read_to_string(&mut uri)?;
                if let Some(path) = file_uri_to_path(&uri) {
                    session.cwd = Some(path);
                }
            }
            (10, 0) => {
                let mode = match value {
                    0 => "unspecified",
                    1 => "agent",
                    2 => "ask",
                    3 => "plan",
                    4 => "debug",
                    5 => "triage",
                    6 => "project",
                    7 => "multitask",
                    8 => "custom",
                    _ => "unknown",
                };
                session
                    .harness_extra_mut(Harness::Cursor)
                    .insert("unified_mode".into(), mode.into());
                session
                    .harness_extra_mut(Harness::Cursor)
                    .insert("cli_mode_raw".into(), value.into());
            }
            (26, 0) => {
                session.created_at = i64::try_from(value)
                    .ok()
                    .and_then(|ms| ts_from_epoch(ms as f64))
                    .or(session.created_at)
            }
            _ => {}
        }
        Ok(Flow::Continue)
    })?;
    if session.cwd.is_none() || session.title.is_none() {
        if let Some(meta) = acp_metadata(&r.path, sink) {
            session.cwd = session
                .cwd
                .or_else(|| meta.get("cwd").and_then(Value::as_str).map(PathBuf::from));
            session.title = session
                .title
                .or_else(|| meta.get("title").and_then(Value::as_str).map(str::to_owned));
        }
    }
    // Native metadata has a creation time but no logical update timestamp. WAL writes may not
    // change the main database yet; filesystem time is an explicitly approximate fallback.
    let mut wal = r.path.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    sink.dependency(&wal);
    session.updated_at = [&r.path, &wal]
        .into_iter()
        .filter_map(|path| {
            std::fs::metadata(path)
                .ok()?
                .modified()
                .ok()
                .map(chrono::DateTime::from)
        })
        .max()
        .or(session.created_at);
    sink.meta(&session);
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    let mut emit = |id: &str| -> Result<Flow> {
        let message = if let Some(captured) = captured {
            let (backing, offset, length) = if let Some(arena) = arena {
                let (offset, length) =
                    crate::harness::store::sqlite::append_cell(conn, "blobs", "data", blob_rowid(conn, id)?, arena)?;
                (arena.clone(), offset, length)
            } else {
                let backing = std::sync::Arc::new(blob(conn, id)?);
                let length = backing.len()?;
                (backing, 0, length)
            };
            captured.read_backing_value(backing, offset, length, |view| {
                if counting {
                    Ok(message(projection::view(view)?, false, false))
                } else {
                    let message = selected::message(view)?;
                    if let Some(model) = selected::model(view)? {
                        session.model.get_or_insert(model);
                    }
                    Ok(message)
                }
            })?
        } else {
            let source = blob(conn, id)?;
            let value: Value = serde_json::from_reader(source.reader()?)?;
            if let Some(model) = value
                .pointer("/providerOptions/cursor/systemPromptFingerprint/model")
                .and_then(Value::as_str)
            {
                session.model.get_or_insert_with(|| model.to_owned());
            }
            message(value, opts.extra, opts.complete)
        };
        if let Some(message) = message {
            return Ok(sink.message(message));
        }
        Ok(Flow::Continue)
    };
    // Archives precede the current prompt. Preserve each archive's ordered message references;
    // the summary_message field is represented in the current prompt, not emitted twice.
    let mut has_archives = false;
    fields(root.reader()?, &mut |n, _, _, _| {
        has_archives |= n == 13;
        Ok(Flow::Continue)
    })?;
    let archive_field = if has_archives { 13 } else { 11 };
    if references(&root, archive_field, &mut |id| {
        let archive = blob(conn, id)?;
        fields(archive.reader()?, &mut |_, _, _, _| Ok(Flow::Continue))?;
        references(&archive, 1, &mut emit)
    })? == Flow::Stop
    {
        return Ok(session);
    }
    references(&root, 1, &mut emit)?;
    Ok(session)
}

fn acp_metadata(path: &Path, sink: &mut dyn MessageSink) -> Option<Value> {
    let path = path.parent()?.join("meta.json");
    sink.dependency(&path);
    serde_json::from_reader(std::fs::File::open(path).ok()?).ok()
}

fn blob_input<'a>(conn: &'a Connection, id: &str) -> Result<rusqlite::blob::Blob<'a>> {
    Ok(conn.blob_open(
        rusqlite::DatabaseName::Main,
        "blobs",
        "data",
        blob_rowid(conn, id)?,
        true,
    )?)
}
fn blob_rowid(conn: &Connection, id: &str) -> Result<i64> {
    conn.query_row("SELECT rowid FROM blobs WHERE id=?1", [id], |row| row.get(0))
        .with_context(|| format!("Cursor CLI blob {id} missing"))
}
fn blob(conn: &Connection, id: &str) -> Result<ByteSource> {
    Ok(ByteSource::read(blob_input(conn, id)?)?)
}

fn references(source: &ByteSource, field: u32, f: &mut dyn FnMut(&str) -> Result<Flow>) -> Result<Flow> {
    fields(source.reader()?, &mut |number, wire, length, input| {
        if number == field {
            anyhow::ensure!(
                wire == 2 && length == 32,
                "Cursor CLI blob reference has invalid wire shape"
            );
            let mut id = [0; 32];
            input.read_exact(&mut id)?;
            return f(&encode_hex(&id));
        }
        Ok(Flow::Continue)
    })
}

/// A bounded protobuf wire walker. Unknown fields are skipped exactly, including truncated
/// payload checks; no wire length controls an allocation. Callbacks may read a limited field.
type FieldVisitor<'a> = dyn FnMut(u32, u8, u64, &mut dyn Read) -> Result<Flow> + 'a;
fn fields(mut input: impl Read, f: &mut FieldVisitor<'_>) -> Result<Flow> {
    loop {
        let mut first = [0];
        if input.read(&mut first)? == 0 {
            return Ok(Flow::Continue);
        }
        let tag = varint(&mut input, Some(first[0]))?;
        let number = u32::try_from(tag >> 3)?;
        anyhow::ensure!(
            number > 0 && number < (1 << 29),
            "invalid Cursor CLI protobuf field number"
        );
        let wire = (tag & 7) as u8;
        if wire == 0 {
            let value = varint(&mut input, None)?;
            if f(number, wire, value, &mut std::io::empty())? == Flow::Stop {
                return Ok(Flow::Stop);
            }
        } else {
            let length = match wire {
                1 => 8,
                2 => varint(&mut input, None)?,
                5 => 4,
                _ => anyhow::bail!("unsupported Cursor CLI protobuf wire type {wire}"),
            };
            let mut field = (&mut input).take(length);
            if f(number, wire, length, &mut field)? == Flow::Stop {
                return Ok(Flow::Stop);
            }
            let remaining = field.limit();
            anyhow::ensure!(
                std::io::copy(&mut field, &mut std::io::sink())? == remaining,
                "truncated Cursor CLI protobuf field"
            );
        }
    }
}
fn varint(input: &mut dyn Read, first: Option<u8>) -> Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = if shift == 0 {
            if let Some(first) = first {
                first
            } else {
                let mut b = [0];
                input.read_exact(&mut b)?;
                b[0]
            }
        } else {
            let mut b = [0];
            input.read_exact(&mut b)?;
            b[0]
        };
        anyhow::ensure!(shift != 63 || byte <= 1, "overflowing Cursor CLI protobuf varint");
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    anyhow::bail!("overflowing Cursor CLI protobuf varint")
}
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "odd Cursor CLI hex length");
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let hex = |b: u8| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => anyhow::bail!("invalid Cursor CLI hex digit"),
            };
            Ok((hex(pair[0])? << 4) | hex(pair[1])?)
        })
        .collect()
}
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

fn take_string(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    match object.remove(key) {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}
fn message(mut value: Value, extra: bool, complete: bool) -> Option<Message> {
    let original = complete.then(|| value.clone());
    let object = value.as_object_mut()?;
    let role = match object.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" | "developer" => Role::System,
        _ => return None,
    };
    let cursor = object.get("providerOptions").and_then(|v| v.get("cursor"));
    let injected = role == Role::System
        || (role == Role::User
            && cursor
                .and_then(|v| v.get("requestContextCompleteness"))
                .is_some_and(|v| !v.is_null()));
    let summary = cursor.and_then(|v| v.get("isSummary")).and_then(Value::as_bool) == Some(true);
    let is_error = cursor
        .and_then(|v| v.pointer("/highLevelToolCallResult/isError"))
        .and_then(Value::as_bool)
        == Some(true);
    let mut message = if injected {
        Message::of_kind(role, MessageKind::InjectedContext, Origin::Harness)
    } else if summary {
        Message::of_kind(role, MessageKind::CompactionSummary, Origin::Harness)
    } else {
        Message::new(role)
    };
    message.id = take_string(object, "id");
    let content = object.remove("content").unwrap_or(Value::Null);
    let parts = match content {
        Value::String(text) => {
            if !text.is_empty() {
                message.content.push(Block::Text { text: text.into() });
            }
            Vec::new()
        }
        Value::Array(parts) => parts,
        _ => Vec::new(),
    };
    for mut part in parts {
        let Some(part) = part.as_object_mut() else { continue };
        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = take_string(part, "text").filter(|s| !s.is_empty()) {
                    message.content.push(Block::Text { text: text.into() });
                }
            }
            "reasoning" | "redacted-reasoning" => {
                let redacted = part.get("type").and_then(Value::as_str) == Some("redacted-reasoning");
                message.content.push(Block::Thinking {
                    text: take_string(part, "text").unwrap_or_default().into(),
                    signature: take_string(part, "signature"),
                    encrypted: None,
                    redacted,
                });
            }
            "tool-call" => message.content.push(Block::ToolUse {
                id: take_string(part, "toolCallId").unwrap_or_default(),
                name: take_string(part, "toolName").unwrap_or_else(|| "tool".into()),
                input: part.remove("args").unwrap_or(Value::Null).into(),
                namespace: None,
            }),
            "tool-result" => {
                let result = part.remove("result").unwrap_or(Value::Null);
                let content = match result {
                    Value::String(text) => text,
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                message.content.push(Block::ToolResult {
                    tool_use_id: take_string(part, "toolCallId").unwrap_or_default(),
                    content: content.into(),
                    is_error: is_error || part.get("isError").and_then(Value::as_bool) == Some(true),
                    tool_name: take_string(part, "toolName"),
                    status: None,
                    details: None,
                });
            }
            "image" => message.content.push(Block::Image {
                media_type: take_string(part, "mimeType"),
                data_ref: Some("inline:cursor-cli".into()),
            }),
            "file" => message.content.push(Block::File {
                mime: take_string(part, "mimeType"),
                path: take_string(part, "filename"),
                source: None,
            }),
            _ => {
                if complete {
                    message
                        .harness_extra_mut(Harness::Cursor)
                        .entry("unhandled_parts")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()?
                        .push(Value::Object(std::mem::take(part)));
                }
            }
        }
        if extra && !complete && part.len() > 1 {
            message
                .harness_extra_mut(Harness::Cursor)
                .entry("part_metadata")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()?
                .push(Value::Object(std::mem::take(part)));
        }
    }
    if message.content.is_empty() {
        if !complete {
            return None;
        }
        message.kind = MessageKind::Carrier;
        message.origin = Origin::Harness;
    }
    if extra || complete {
        object.remove("role");
        if !object.is_empty() {
            message
                .harness_extra_mut(Harness::Cursor)
                .insert("record_metadata".into(), Value::Object(std::mem::take(object)));
        }
    }
    if let Some(original) = original {
        message
            .harness_extra_mut(Harness::Cursor)
            .insert("record".into(), original);
    }
    (!message.content.is_empty() || !message.extra.is_empty()).then_some(message)
}

mod projection;
mod selected;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse;
    use crate::stream::CollectSink;
    use serde_json::json;

    fn field(number: u32, bytes: &[u8]) -> Vec<u8> {
        fn v(mut n: u64, out: &mut Vec<u8>) {
            while n > 127 {
                out.push((n as u8 & 127) | 128);
                n >>= 7;
            }
            out.push(n as u8);
        }
        let mut out = Vec::new();
        v(u64::from(number) << 3 | 2, &mut out);
        v(bytes.len() as u64, &mut out);
        out.extend(bytes);
        out
    }
    fn fixture(root: Vec<u8>, messages: &[(u8, Vec<u8>)]) -> (Connection, SessionRef) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);",
        )
        .unwrap();
        let metadata = json!({"agentId":"fixture", "latestRootBlobId":encode_hex(&[255;32]), "createdAt":1, "subagentInfo":{"parentAgentId":"parent","toolCallId":"spawn","typeName":"worker"}});
        conn.execute(
            "INSERT INTO meta VALUES('0',?1)",
            [encode_hex(metadata.to_string().as_bytes())],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs VALUES(?1,?2)",
            rusqlite::params![encode_hex(&[255; 32]), root],
        )
        .unwrap();
        for (id, message) in messages {
            conn.execute(
                "INSERT INTO blobs VALUES(?1,?2)",
                rusqlite::params![encode_hex(&[*id; 32]), message],
            )
            .unwrap();
        }
        (conn, SessionRef::new("cli:fixture", Harness::Cursor, ":memory:"))
    }
    #[test]
    fn native_capture_counts_and_tool_links_match_stream() {
        let home = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/cursor/2026.10.01-e373342/.cursor");
        let adapter = Cursor::with_cli_home(&home);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].message_count, 5);
        let session = adapter.parse(&refs[0]).unwrap();
        assert_eq!(session.messages.len(), 10);
        assert_eq!(
            session.messages.iter().filter(|m| m.is_turn()).count(),
            refs[0].message_count
        );
        let uses: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolUse { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        let results: Vec<_> = session
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                Block::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results);
        assert_eq!(uses.len(), 3);
        assert_eq!(session.model.as_deref(), Some("grok-4.5"));
        assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["unified_mode"], "agent");
    }
    #[test]
    fn archives_precede_live_messages_and_unknown_mode_is_retained() {
        for archive_field in [11, 13] {
            let mut root = field(archive_field, &[1; 32]);
            root.extend(field(1, &[3; 32]));
            root.extend([80, 99]);
            let archive = field(1, &[2; 32]);
            let older = json!({"role":"user","content":"older"}).to_string().into_bytes();
            let newer = json!({"role":"assistant","content":[{"type":"text","text":"newer"}]})
                .to_string()
                .into_bytes();
            let (conn, r) = fixture(root, &[(1, archive), (2, older), (3, newer)]);
            let mut sink = CollectSink::default();
            let session = read(&conn, &r, &ParseOptions::default(), false, &mut sink).unwrap();
            assert_eq!(sink.messages.len(), 2);
            assert_eq!(sink.messages[0].text().as_deref(), Some("older"));
            assert_eq!(session.lineage.parent.as_deref(), Some("cli:parent"));
            assert_eq!(session.harness_extra(Harness::Cursor).unwrap()["cli_mode_raw"], 99);
        }
    }
    #[test]
    fn projected_count_obeys_injected_and_summary_semantics() {
        let cases = [
            json!({"role":"user","content":"injected","providerOptions":{"cursor":{"requestContextCompleteness":{}}}}),
            json!({"role":"user","content":"summary","providerOptions":{"cursor":{"isSummary":true}}}),
            json!({"role":"assistant","content":[{"type":"tool-call","toolCallId":"call","toolName":"Read","args":{"large":"x".repeat(2_000_000)}}]}),
            json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"call","result":"x".repeat(2_000_000)}]}),
            json!({"role":"user","content":"   "}),
        ];
        for value in cases {
            let full = message(value.clone(), false, false).unwrap();
            let projected = message(projection::read(value.to_string().as_bytes()).unwrap(), false, false).unwrap();
            assert_eq!((full.kind, full.is_turn()), (projected.kind, projected.is_turn()));
        }
        let empty = json!({"role":"assistant","content":[],"providerOptions":{"cursor":{"opaque":1}}});
        assert!(message(empty.clone(), true, false).is_none());
        assert!(!message(empty, true, true).unwrap().is_turn());
        let v = json!({"role":"assistant","content":[{"type":"reasoning","text":"thought","providerOptions":{"opaque":42}}]});
        assert_eq!(
            message(v.clone(), true, false)
                .unwrap()
                .harness_extra(Harness::Cursor)
                .unwrap()["part_metadata"][0]["providerOptions"]["opaque"],
            42
        );
        assert_eq!(
            message(v.clone(), true, true)
                .unwrap()
                .harness_extra(Harness::Cursor)
                .unwrap()["record"],
            v
        );
    }
    #[test]
    fn malformed_wire_and_missing_blob_fail_while_stop_is_demand_bounded() {
        for root in [vec![0], vec![10, 32, 1], vec![128; 11], field(1, &[1; 31])] {
            let (conn, r) = fixture(root, &[]);
            assert!(read(&conn, &r, &ParseOptions::default(), false, &mut Count::default()).is_err());
        }
        let (conn, r) = fixture(field(1, &[1; 32]), &[]);
        assert!(read(&conn, &r, &ParseOptions::default(), false, &mut Count::default()).is_err());
        let mut root = field(1, &[1; 32]);
        root.extend(field(1, &[2; 32]));
        let (conn, r) = fixture(root, &[(1, br#"{"role":"user","content":"one"}"#.to_vec())]);
        struct Stop;
        impl MessageSink for Stop {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Stop
            }
        }
        assert!(read(&conn, &r, &ParseOptions::default(), false, &mut Stop).is_ok());
        for text in [
            r#"{"role":"user","content":"ok","ignored":"\uD800"}"#,
            r#"{"role":"user","content":"ok","ignored":1e9999}"#,
            r#"{"role":"user","content":"ok"} garbage"#,
        ] {
            assert!(projection::read(text.as_bytes()).is_err());
        }
    }
    #[test]
    fn acp_sidecar_creation_change_and_identity_invalidate() {
        let world = cv_testkit::World::in_process("cursor-acp");
        let dir = &world.base;
        std::fs::create_dir_all(dir).unwrap();
        let (conn, mut reference) = fixture(
            field(1, &[1; 32]),
            &[(1, br#"{"role":"user","content":"one"}"#.to_vec())],
        );
        reference.path = dir.join("store.db");
        struct Reads(crate::source::ReadSet);
        impl MessageSink for Reads {
            fn message(&mut self, _: Message) -> Flow {
                Flow::Continue
            }
            fn dependency(&mut self, path: &Path) {
                self.0.record(path)
            }
        }
        let mut absent = Reads(crate::source::ReadSet::default());
        read(&conn, &reference, &ParseOptions::bulk(), false, &mut absent).unwrap();
        assert!(absent.0.fresh(&mut crate::source::Probe::default()));
        std::fs::write(dir.join("meta.json"), r#"{"cwd":"/scratch/project","title":"ACP"}"#).unwrap();
        assert!(!absent.0.fresh(&mut crate::source::Probe::default()));
        let mut current = Reads(crate::source::ReadSet::default());
        let session = read(&conn, &reference, &ParseOptions::bulk(), false, &mut current).unwrap();
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/scratch/project")));
        assert_eq!(session.title.as_deref(), Some("ACP"));
        std::fs::write(
            dir.join("meta.json"),
            r#"{"cwd":"/scratch/project2","title":"changed title"}"#,
        )
        .unwrap();
        assert!(!current.0.fresh(&mut crate::source::Probe::default()));
        reference.id = "cli:replacement".into();
        assert!(read(&conn, &reference, &ParseOptions::bulk(), false, &mut Count::default()).is_err());
    }
    #[test]
    fn updated_time_observes_active_wal() {
        let world = cv_testkit::World::in_process("cursor-cli-wal-time");
        std::fs::create_dir_all(&world.base).unwrap();
        let (conn, mut reference) = fixture(Vec::new(), &[]);
        reference.path = world.base.join("store.db");
        for (name, seconds) in [("store.db", 1), ("store.db-wal", 2)] {
            let file = std::fs::File::create(world.base.join(name)).unwrap();
            file.set_times(
                std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .unwrap();
        }
        let session = read(&conn, &reference, &ParseOptions::bulk(), false, &mut Count::default()).unwrap();
        assert_eq!(session.updated_at.unwrap().timestamp(), 2);
    }
    fn disk_fixture(root: Vec<u8>, messages: &[(u8, Vec<u8>)], dir: &Path) -> (Connection, SessionRef) {
        std::fs::create_dir_all(dir).unwrap();
        let (memory, mut reference) = fixture(root, messages);
        reference.path = dir.join("store.db");
        memory
            .execute("VACUUM INTO ?1", [reference.path.to_str().unwrap()])
            .unwrap();
        let disk = Connection::open(&reference.path).unwrap();
        (disk, reference)
    }
    #[test]
    fn selected_large_fields_match_owned_messages_and_dataset() {
        let world = cv_testkit::World::in_process("cursor-cli-selected");
        let big = "日本語\n".repeat(4096);
        let records = [
            json!({"role":"user","content":[{"type":"text","text":big}],"providerOptions":{"cursor":{"requestId":"request"}}}),
            json!({"role":"assistant","content":[{"type":"reasoning","text":big,"signature":"sig"},{"type":"tool-call","toolCallId":"call","toolName":"Shell","args":{"text":big,"small":7}}],"providerOptions":{"cursor":{"ignored":big}}}),
            json!({"role":"tool","content":[{"type":"tool-result","toolCallId":"call","toolName":"Shell","result":big,"experimental_content":[{"type":"text","text":big}]}],"providerOptions":{"cursor":{"highLevelToolCallResult":{"isError":true,"output":big}}}}),
        ];
        let root = (1..=3).flat_map(|id| field(1, &[id; 32])).collect();
        let records: Vec<_> = records
            .into_iter()
            .enumerate()
            .map(|(i, v)| ((i + 1) as u8, v.to_string().into_bytes()))
            .collect();
        let (_conn, reference) = disk_fixture(root, &records, &world.base.join("source"));
        let adapter = Cursor::with_cli_home(&world.base);
        let owned = crate::stream::collect_with(&adapter, &reference, &ParseOptions::bulk()).unwrap();
        let mut lazy = crate::stream::collect_with(&adapter, &reference, &ParseOptions::lazy()).unwrap();
        let mut owned_output = Vec::new();
        let mut lazy_output = Vec::new();
        crate::dataset::write_record(&owned, &mut owned_output, crate::dataset::Format::Chatml, None).unwrap();
        let staged = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            None,
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        staged.publish(&mut lazy_output).unwrap();
        lazy_output.pop(); // stage publication includes the record's newline
        assert_eq!(owned_output, lazy_output);
        // A filter retains deferred descriptors before rendering rather than eagerly demanding
        // payloads. This exercises the same checked text/JSON carriers through registry restore.
        let query = crate::SessionQuery::parse("harness:cursor").unwrap();
        let filtered = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            Some(&query),
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        let mut filtered_output = Vec::new();
        filtered.publish(&mut filtered_output).unwrap();
        filtered_output.pop();
        assert_eq!(owned_output, filtered_output);
        lazy.materialize().unwrap();
        assert_eq!(
            serde_json::to_value(&lazy.messages).unwrap(),
            serde_json::to_value(&owned.messages).unwrap()
        );
    }
    #[test]
    fn selected_schema_edges_and_ignored_invalid_values_match_owned_acceptance() {
        let world = cv_testkit::World::in_process("cursor-cli-selected-edges");
        let cases = [
            r#"{"role":"user","content":"first","content":"last","providerOptions":{"cursor":{"requestContextCompleteness":false,"isSummary":true}}}"#,
            r#"{"role":"developer","content":[{"type":"text","text":""},{"type":"unknown","opaque":7},{"type":"redacted-reasoning","text":null},{"type":"image","mimeType":false},{"type":"file","filename":"a"}]}"#,
            r#"{"role":"assistant","content":[{"type":"tool-call","args":"{\"literal\":true}"},{"type":"tool-result","result":{"z":2,"a":1}},{"type":"tool-result","result":null},{"type":"reasoning","text":false}]}"#,
            r#"{"role":"user","content":"ok","ignored":"\uD800"}"#,
            r#"{"role":"user","content":"ok","ignored":1e9999}"#,
            r#"{"role":"user","content":"ok"} garbage"#,
        ];
        for (index, record) in cases.into_iter().enumerate() {
            let (conn, reference) = disk_fixture(
                field(1, &[1; 32]),
                &[(1, record.as_bytes().to_vec())],
                &world.base.join(index.to_string()),
            );
            let mut owned = CollectSink::default();
            let mut selected = CollectSink::default();
            let full = read(&conn, &reference, &ParseOptions::bulk(), false, &mut owned);
            let lazy = read(&conn, &reference, &ParseOptions::lazy(), false, &mut selected);
            assert_eq!(full.is_ok(), lazy.is_ok(), "case {index}");
            if full.is_ok() {
                let resolver = crate::lazy::Resolver::new(Some(reference.path.clone()));
                for message in &mut selected.messages {
                    message.materialize(&resolver).unwrap();
                }
                assert_eq!(
                    serde_json::to_value(owned.messages).unwrap(),
                    serde_json::to_value(selected.messages).unwrap(),
                    "case {index}"
                );
            }
        }
    }
    #[test]
    fn wal_guard_survives_lazy_blob_arguments_and_intentional_stop() {
        let world = cv_testkit::World::in_process("cursor-cli-guard");
        let record=json!({"role":"assistant","content":[{"type":"tool-call","toolCallId":"call","toolName":"Read","args":{"body":"x".repeat(8192)}},{"type":"text","text":"日本語\n".repeat(4096)}]}).to_string().into_bytes();
        let (conn, reference) = disk_fixture(field(1, &[1; 32]), &[(1, record)], &world.base.join("source"));
        let mut retained = CollectSink::default();
        read(&conn, &reference, &ParseOptions::lazy(), false, &mut retained).unwrap();
        let input = match &retained.messages[0].content[0] {
            Block::ToolUse { input, .. } => input,
            _ => panic!("expected tool"),
        };
        assert!(matches!(input, crate::json::Json::Source(_)));
        let text = match &retained.messages[0].content[1] {
            Block::Text {
                text: crate::lazy::Text::Source(text),
            } => text,
            _ => panic!("expected checked source text"),
        };
        assert_eq!(text.try_decoded_len().unwrap(), "日本語\n".len() * 4096);
        let adapter = Cursor::with_cli_home(&world.base);
        let staged = crate::dataset::stage_record(
            &adapter,
            &reference,
            crate::dataset::Format::Chatml,
            None,
            0,
            None,
            &crate::query::TextSets::default(),
        )
        .unwrap()
        .unwrap();
        let wal = reference.path.with_file_name("store.db-wal");
        std::fs::write(&wal, b"new WAL identity").unwrap();
        assert!(input.canonical_len().is_err());
        assert!(text.try_prefix(10).is_err());
        assert!(text.try_decoded_len().is_err());
        let mut output = Vec::new();
        // Staging has already demanded and validated every field. These finalized private
        // bytes represent the accepted snapshot and remain publishable after it changes.
        staged.publish(&mut output).unwrap();
        assert!(!output.is_empty());
        let resolver = crate::lazy::Resolver::new(Some(reference.path.clone()));
        assert!(retained.messages[0].materialize(&resolver).is_err());
        std::fs::remove_file(&wal).unwrap();
        struct Mutate(PathBuf);
        impl MessageSink for Mutate {
            fn message(&mut self, _: Message) -> Flow {
                std::fs::write(&self.0, b"new WAL identity").unwrap();
                Flow::Stop
            }
        }
        let error = read(&conn, &reference, &ParseOptions::lazy(), false, &mut Mutate(wal)).unwrap_err();
        assert!(error.to_string().contains("dependencies changed"), "{error:#}");
    }
}
