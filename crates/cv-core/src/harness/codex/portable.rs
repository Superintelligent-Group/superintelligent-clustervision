//! Positioned JSONL replay. Plain files keep their captured identity; compressed rollouts decode
//! once into one private arena whose lazy children retain the original compressed provenance.
use super::*;
use crate::harness::store::{
    bytes::ByteSource,
    document::Fields,
    jsonl::ranges::{self, read_exact_at, Range},
};
use crate::json::{InvalidJson, JsonSource, JsonView};
use std::io;
use std::sync::Arc;

struct Rollout {
    path: PathBuf,
    source: JsonSource,
    backing: Option<Arc<ByteSource>>,
    file: fs::File,
    len: u64,
}
enum Projection {
    Output { record: Value, text: Text, error: bool },
    Selected(super::selected::Projection),
}
impl Rollout {
    fn open(path: &Path) -> Result<Self> {
        let source = JsonSource::capture(path)?;
        let (file, backing, len) = if is_zst_path(path) {
            // Keep one descriptor/owner for the entire decoded source, not one tempfile per record.
            let backing = Arc::new(ByteSource::arena()?);
            let (offset, len) = backing.append(|writer| {
                io::copy(&mut open_rollout(path)?, &mut { writer })?;
                Ok(())
            })?;
            source.check()?;
            debug_assert_eq!(offset, 0);
            (backing.file()?, Some(backing), len)
        } else {
            let file = fs::File::open(path)?;
            source.check_metadata(file.metadata()?)?;
            let len = file.metadata()?.len();
            (file, None, len)
        };
        Ok(Self {
            path: path.to_path_buf(),
            source,
            backing,
            file,
            len,
        })
    }
    fn reader(&self, range: Range) -> Result<BufReader<ranges::Reader<'_>>> {
        ranges::reader(&self.file, range)
    }
    fn view<T>(&self, range: Range, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        match &self.backing {
            Some(backing) => self
                .source
                .read_backing_syntax(backing.clone(), range.offset, range.len, f),
            None => self.source.read_range_syntax(range.offset, range.len, f),
        }
    }
    fn value<T>(&self, range: Range, f: impl for<'a> FnOnce(JsonView<'a>) -> Result<T>) -> Result<T> {
        match &self.backing {
            Some(backing) => self
                .source
                .read_backing_value(backing.clone(), range.offset, range.len, f),
            None => self.source.read_range_value(range.offset, range.len, f),
        }
    }
    fn records(&self, at: u64) -> Result<ranges::Records<'_>> {
        ranges::records(
            &self.file,
            Range {
                offset: at,
                len: self.len - at,
            },
        )
    }
    fn trim(&self, range: Range) -> Result<Range> {
        ranges::trim(&self.file, range)
    }
    fn events(&self) -> Result<bool> {
        let mut detector = EventDetector::default();
        for record in self.records(0)? {
            let (range, _) = record?;
            let range = self.trim(range)?;
            if range.len == 0 {
                continue;
            }
            let flags = self.value(range, |view| {
                let Some(top) = Fields::parse(view, &["type", "payload"])? else {
                    return Ok(Some((false, false)));
                };
                let payload = top
                    .get("payload")
                    .map(|view| Fields::parse(view, &["type", "role"]))
                    .transpose()?
                    .flatten();
                let event = equals(top.get("type"), "event_msg")?
                    && match &payload {
                        Some(p) => equals(p.get("type"), "user_message")? || equals(p.get("type"), "agent_message")?,
                        None => false,
                    };
                let response = equals(top.get("type"), "response_item")?
                    && match &payload {
                        Some(p) => {
                            equals(p.get("type"), "message")?
                                && (equals(p.get("role"), "user")? || equals(p.get("role"), "assistant")?)
                        }
                        None => false,
                    };
                Ok(Some((event, response)))
            });
            match flags {
                Ok(Some((event, response))) if detector.feed_flags(event, response) == Flow::Stop => break,
                Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {}
                Err(error) => return Err(error),
                _ => {}
            }
        }
        self.source.check()?;
        Ok(detector.found)
    }
    fn projection(&self, range: Range, spans: bool, agents: bool, complete: bool) -> Result<Option<Projection>> {
        self.value(range, |view| {
            let Some((record, output)) = function_output_fields(view)? else {
                return super::selected::project(view, &self.source, spans, agents, complete)
                    .map(|p| p.map(Projection::Selected));
            };
            let Some(output) = output else { return Ok(None) };
            let Some(mut text) = (if spans {
                output.source_text()?
            } else {
                output.text(false)?
            }) else {
                return Ok(None);
            };
            // A bounded decoded prefix recognizes escaped spellings of the emitter's error marker.
            let resolver = crate::Resolver::new(Some(self.path.clone()));
            let is_error = text.try_resolve_prefix(&resolver, 64)?.starts_with(ERROR_PREFIX);
            if is_error {
                text = text.with_skipped_prefix(ERROR_PREFIX.len())?;
            }
            Ok(Some(Projection::Output {
                record,
                text,
                error: is_error,
            }))
        })
    }
    fn parse(&self, range: Range, complete: bool) -> Result<Option<Value>> {
        if range.len <= crate::lazy::INLINE_MAX as u64 {
            let mut bytes = vec![0; range.len as usize];
            read_exact_at(&self.file, &mut bytes, range.offset)?;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return Ok(None);
            };
            if !complete {
                if let Ok(value) = lean::parse(text.as_bytes(), false) {
                    return Ok(Some(value.v));
                }
            }
            return Ok(serde_json::from_str(text).ok());
        }
        // Old .lines()/from_utf8 rejected malformed UTF-8 even in ignored lean descendants.
        // Syntax validation supplies that boundary without changing ignored scalar acceptance.
        match self.view(range, |_| Ok(())) {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<InvalidJson>().is_some() => return Ok(None),
            Err(error) => return Err(error),
        }
        if !complete {
            if let Ok(value) = lean::parse_reader(self.reader(range)?) {
                return Ok(Some(value));
            }
        }
        Ok(serde_json::from_reader(self.reader(range)?).ok())
    }
}

/// The caller validates the whole Value before applying this dispatcher schema roster.
fn function_output_fields(view: JsonView<'_>) -> Result<Option<(Value, Option<JsonView<'_>>)>> {
    let Some(top) = Fields::parse(view, &["type", "timestamp", "ordinal", "payload"])? else {
        return Ok(None);
    };
    if !equals(top.get("type"), "response_item")? {
        return Ok(None);
    }
    let Some(payload) = top
        .get("payload")
        .map(|view| {
            Fields::parse(
                view,
                &[
                    "type",
                    "call_id",
                    "name",
                    "namespace",
                    "output",
                    "internal_chat_message_metadata_passthrough",
                ],
            )
        })
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let ty = if equals(payload.get("type"), "function_call_output")? {
        "function_call_output"
    } else if equals(payload.get("type"), "custom_tool_call_output")? {
        "custom_tool_call_output"
    } else {
        return Ok(None);
    };
    let mut p = Map::new();
    p.insert("type".into(), ty.into());
    for key in ["call_id", "name", "namespace"] {
        if let Some(value) = payload.string(key)? {
            p.insert(key.into(), value.into());
        }
    }
    if let Some(meta) = payload.get("internal_chat_message_metadata_passthrough") {
        p.insert(
            "internal_chat_message_metadata_passthrough".into(),
            meta.resolve()?.into_owned(),
        );
    }
    Ok(Some((
        record_fields(&top, "response_item", Value::Object(p))?,
        payload.get("output"),
    )))
}

fn agent_text_fields(view: JsonView<'_>) -> Result<Option<(Value, Option<JsonView<'_>>)>> {
    let Some(top) = Fields::parse(view, &["type", "timestamp", "ordinal", "payload"])? else {
        return Ok(None);
    };
    if !equals(top.get("type"), "event_msg")? {
        return Ok(None);
    }
    let Some(payload) = top
        .get("payload")
        .map(|view| Fields::parse(view, &["type", "message"]))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    if !equals(payload.get("type"), "agent_message")? {
        return Ok(None);
    }
    let body = payload.get("message");
    if !body.map(JsonView::is_string).transpose()?.unwrap_or(false) {
        return Ok(None);
    }
    let payload = serde_json::json!({"type":"agent_message", "message":""});
    Ok(Some((record_fields(&top, "event_msg", payload)?, body)))
}

/// Modern prompt/message discovery shares the bounded legacy content/title reducers. Separate
/// title and counting skeletons preserve full joined-title whitespace without making the turn
/// counter retain bodies or changing image/encrypted part presence.
fn discovery_message_fields(view: JsonView<'_>, title_needed: bool) -> Result<Option<(Value, Value)>> {
    let Some(top) = Fields::parse(view, &["type", "timestamp", "ordinal", "payload"])? else {
        return Ok(None);
    };
    let ty = top.string("type")?.unwrap_or_default();
    let Some(payload) = top.get("payload") else {
        return Ok(None);
    };
    let Some(fields) = Fields::parse(payload, &["type", "role", "content", "message"])? else {
        return Ok(None);
    };
    let sub = fields.string("type")?.unwrap_or_default();
    let body_key = match (ty.as_str(), sub.as_str()) {
        ("response_item", "message") => "content",
        ("event_msg", "user_message") => "message",
        _ => return Ok(None),
    };
    let mut count = Map::new();
    count.insert("type".into(), sub.into());
    if let Some(role) = fields.string("role")? {
        count.insert("role".into(), role.into());
    }
    let mut title = count.clone();
    if let Some(body) = fields.get(body_key) {
        // Non-string event bodies are ignored by the dispatcher, unlike response content arrays.
        if body_key == "content" || body.is_string()? {
            count.insert(body_key.into(), legacy::count_content(body)?);
            title.insert(
                body_key.into(),
                if title_needed {
                    legacy::title(body)?.into()
                } else {
                    Value::Null
                },
            );
        }
    }
    Ok(Some((
        record_fields(&top, &ty, Value::Object(count))?,
        record_fields(&top, &ty, Value::Object(title))?,
    )))
}

fn record_fields(top: &Fields<'_>, ty: &str, payload: Value) -> Result<Value> {
    let mut record = Map::new();
    record.insert("type".into(), ty.into());
    record.insert("payload".into(), payload);
    // Only string timestamps and numeric ordinals affect the common dispatcher.
    if let Some(timestamp) = top.string("timestamp")? {
        record.insert("timestamp".into(), timestamp.into());
    }
    if let Some(ordinal) = top.get("ordinal").map(|value| value.as_number()).transpose()?.flatten() {
        record.insert("ordinal".into(), ordinal.into());
    }
    Ok(Value::Object(record))
}

fn equals(value: Option<JsonView<'_>>, expected: &str) -> Result<bool> {
    let Some(value) = value else { return Ok(false) };
    let mut offset = 0usize;
    let mut equal = true;
    let string = value.for_each_str_chunk(|text| {
        let end = offset.saturating_add(text.len());
        equal &= expected.as_bytes().get(offset..end) == Some(text.as_bytes());
        offset = end;
        Ok(())
    })?;
    Ok(string && equal && offset == expected.len())
}

/// Compressed discovery consumes the same frozen decoded ranges as message replay.
pub(super) fn scan(path: &Path, scan: &mut CodexScan, turns: &mut metadata::Turns) -> Result<()> {
    let rollout = Rollout::open(path)?;
    // Accepted views already validate every UTF-8 byte. Only malformed giant rows need an
    // additional encoding check to preserve plain read_to_string's fatal UTF-8 policy;
    // compressed discovery retains tolerant per-record acceptance.
    let rejected = |range| -> Result<()> {
        if rollout.backing.is_none() {
            anyhow::ensure!(
                ranges::valid_utf8(&mut rollout.reader(range)?)?,
                "reading {}: stream did not contain valid UTF-8",
                path.display()
            );
        }
        Ok(())
    };
    for record in rollout.records(0)? {
        let (range, _) = record?;
        let range = rollout.trim(range)?;
        if range.len == 0 {
            continue;
        }
        if range.len > crate::lazy::INLINE_MAX as u64 {
            match rollout.value(range, |view| {
                let projected = match function_output_fields(view)? {
                    Some(record) => Some(record),
                    None => agent_text_fields(view)?,
                };
                match projected {
                    Some((record, _)) => Ok(Some((record.clone(), record))),
                    None => discovery_message_fields(view, scan.title.is_none()),
                }
            }) {
                Ok(Some((value, title))) => {
                    turns.feed_value_with_title(value, title, scan);
                    continue;
                }
                Ok(None) => {}
                Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                    rejected(range)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            // Match .lines()'s whole-line UTF-8 acceptance, including ignored metadata fields.
            match rollout.view(range, |_| Ok(())) {
                Ok(()) => {}
                Err(error) if error.downcast_ref::<InvalidJson>().is_some() => {
                    rejected(range)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            turns.feed_reader(rollout.reader(range)?, scan);
        } else {
            let mut bytes = vec![0; range.len as usize];
            read_exact_at(&rollout.file, &mut bytes, range.offset)?;
            match std::str::from_utf8(&bytes) {
                Ok(text) => turns.feed_line(text, scan),
                Err(error) if rollout.backing.is_none() => return Err(error.into()),
                Err(_) => {}
            }
        }
    }
    rollout.source.check()
}

pub(super) fn stream(r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    let rollout = Rollout::open(&r.path)?;
    let has_events = rollout.events()?;
    let state = ResumeState::new(Some(r.path.clone()), has_events, opts, true);
    stream_at(&rollout, &r.id, state, 0, sink)
}

pub(super) fn resume(
    r: &SessionRef,
    state: ResumeState,
    at: u64,
    sink: &mut dyn MessageSink,
) -> Result<Option<Session>> {
    let rollout = Rollout::open(&r.path)?;
    if at > rollout.len || rollout.events()? != state.has_events {
        return Ok(None);
    }
    Ok(Some(stream_at(&rollout, &r.id, state, at, sink)?))
}

fn stream_at(rollout: &Rollout, id: &str, state: ResumeState, at: u64, sink: &mut dyn MessageSink) -> Result<Session> {
    let ResumeState {
        session: mut s,
        mut ctx,
        has_events,
        mut meta_sent,
        mut skipped,
        reading,
    } = state;
    let mut scratch = Vec::new();
    let mut stopped = false;
    let mut records = rollout.records(at)?;
    while let Some(record) = records.next() {
        let (raw, complete) = record?;
        let off = records.offset();
        let range = rollout.trim(raw)?;
        if range.len == 0 {
            continue;
        }
        let projection = if range.len > crate::lazy::INLINE_MAX as u64 {
            rollout.projection(range, reading.flavor.spans, has_events, ctx.complete)
        } else {
            Ok(None)
        };
        match projection {
            Ok(Some(Projection::Output { record, text, error })) => {
                dispatch_line_with_output(&mut s, &mut scratch, &record, has_events, &mut ctx, Some((text, error)))
            }
            Ok(Some(Projection::Selected(projection))) => {
                projection.dispatch(&mut s, &mut scratch, has_events, &mut ctx)
            }
            Err(error) if error.downcast_ref::<InvalidJson>().is_none() => return Err(error),
            Ok(None) | Err(_) => {
                // Invalid records retain old lean/full fallback and skipped-line semantics.
                rollout.source.check()?;
                let Some(value) = rollout.parse(range, ctx.complete)? else {
                    skipped += 1;
                    continue;
                };
                dispatch_line(&mut s, &mut scratch, &value, has_events, &mut ctx);
            }
        }
        if !meta_sent && s.model.is_some() {
            sink.meta(&s);
            meta_sent = true;
        }
        stopped = flush_all_but_held(&mut scratch, sink) == Flow::Stop;
        if stopped {
            break;
        }
        if scratch.is_empty() && complete && rollout.backing.is_none() && sink.wants_checkpoint(off) {
            checkpoint_at(
                off,
                StateRef {
                    session: &s,
                    ctx: &ctx,
                    has_events,
                    meta_sent,
                    skipped,
                    reading,
                },
                sink,
            );
        }
    }
    rollout.source.check()?;
    super::super::note_skipped_lines(&mut s, skipped);
    if !stopped {
        for message in scratch {
            if sink.message(message) == Flow::Stop {
                break;
            }
        }
    }
    if s.id.is_empty() {
        s.id = id.to_string();
    }
    if !meta_sent {
        sink.meta(&s);
    }
    Ok(s)
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn giant_discovery_messages_keep_bounded_exact_title_and_count_skeletons() -> Result<()> {
        let world = cv_testkit::World::in_process("codex-discovery-message-facts");
        let path = world.base.join("row.json");
        let giant = "body λ\n".repeat(128 * 1024);
        for content in [
            serde_json::json!(giant),
            serde_json::json!([{"text":"title"},{"text":" ".repeat(100_000)},{"text":giant}]),
            serde_json::json!([{"text":" ".repeat(100_000)},{"type":"input_text","text":" tail λ "}]),
            serde_json::json!([{"type":"encrypted_content","text":giant}]),
            serde_json::json!([{"type":"input_image","image_url":giant}]),
        ] {
            let record = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":content}});
            fs::write(&path, serde_json::to_vec(&record)?)?;
            let source = JsonSource::capture(&path)?;
            let (count, title) = source.read_document(|view| Ok(discovery_message_fields(view, true)?.unwrap()))?;
            assert!(serde_json::to_vec(&count)?.len() < 2000);
            assert!(serde_json::to_vec(&title)?.len() < 2000);
            let mut expected_scan = CodexScan::default();
            let mut expected_turns = metadata::Turns::default();
            expected_turns.feed_line(&record.to_string(), &mut expected_scan);
            let mut actual_scan = CodexScan::default();
            let mut actual_turns = metadata::Turns::default();
            actual_turns.feed_value_with_title(count, title, &mut actual_scan);
            assert_eq!(actual_scan, expected_scan);
            assert_eq!(actual_turns.count(), expected_turns.count());
        }
        Ok(())
    }
}
