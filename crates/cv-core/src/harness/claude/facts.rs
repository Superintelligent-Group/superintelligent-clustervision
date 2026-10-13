//! Shared metadata facts for Claude reducers. Selection happens before unrelated sidecars become
//! Values. Full/complete message construction does not use this projection.

use super::*;
use crate::json::JsonView;
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use serde::Deserializer;
use std::fmt;

/// Project only the compaction fields consumed by `CompactionSink`. Wrong-typed fields are absent,
/// just as its typed lookups ignore them in full metadata; unknown fields are never materialized.
pub(super) fn compact_metadata(view: JsonView<'_>) -> Result<Option<Value>> {
    if !view.is_object()? {
        return Ok(None);
    }
    let mut fields = Map::new();
    if let Some(value) = view.get("trigger")? {
        if value.is_string()? {
            fields.insert("trigger".into(), value.resolve()?.into_owned());
        }
    }
    for key in ["preTokens", "durationMs"] {
        if let Some(value) = view.get(key)? {
            if let Some(number) = value.as_number()?.filter(|number| number.as_u64().is_some()) {
                fields.insert(key.into(), Value::Number(number));
            }
        }
    }
    Ok(Some(Value::Object(fields)))
}

/// Classification/count facts from `toolUseResult`. `agentId` is a presence test, including null;
/// only a string status can distinguish the asynchronous launch receipt. Generic checkpoint
/// counting also needs `restorable`, independently of the sidecar's unrelated rich payload.
#[cfg(not(feature = "mmap"))]
pub(super) fn agent_return(view: JsonView<'_>) -> Result<Value> {
    if !view.is_object()? {
        return Ok(Value::Null);
    }
    let mut fields = Map::new();
    if view.get("agentId")?.is_some() {
        fields.insert("agentId".into(), Value::Null);
    }
    if let Some(status) = view.get("status")? {
        if status.is_string()? {
            fields.insert("status".into(), status.resolve()?.into_owned());
        }
    }
    if let Some(restorable) = view.get("restorable")? {
        fields.insert("restorable".into(), restorable.resolve()?.into_owned());
    }
    Ok(Value::Object(fields))
}

/// Generic checkpoint counting consumes this field even on Claude tool results. Other rich
/// sidecar fields remain unowned.
pub(super) fn attach_restorable(blocks: &mut [Block], sidecar: &Value) {
    if let Some(restorable) = sidecar.get("restorable") {
        attach_tool_use_result_owned(blocks, serde_json::json!({"restorable": restorable}));
    }
}

/// Attachment interpretation still needs the prompt snapshot as well as its subtype.
#[cfg(not(feature = "mmap"))]
pub(super) fn attachment(view: JsonView<'_>) -> Result<Value> {
    let mut fields = Map::new();
    for key in ["type", "systemPrompt"] {
        if let Some(value) = view.get(key)? {
            fields.insert(key.into(), value.resolve()?.into_owned());
        }
    }
    Ok(Value::Object(fields))
}

/// Collect facts from the already-owned compatibility codec. It deliberately does not clone
/// `toolUseResult`, synthetic error details or rich attribution into the returned message.
pub(super) fn collect_message(msg: &Value, bag: &mut Map<String, Value>) {
    if let Some(id) = msg.get("id").and_then(Value::as_str) {
        bag.insert("message.id".into(), Value::String(id.to_owned()));
    }
}

pub(super) fn collect_compaction(v: &Value, bag: &mut Map<String, Value>) {
    if let Some(value) = v.get("compactMetadata") {
        // Owned views cannot produce source/provenance failures.
        if let Some(projected) = compact_metadata(value.into()).expect("owned compaction projection") {
            bag.insert("compactMetadata".into(), projected);
        }
    }
}

/// Compatibility fallback for records the conversational borrowed codec cannot interpret.
/// Validate the original rich-codec acceptance boundary before skipping values. This preserves
/// last-wins duplicate semantics without decoding unrelated metadata or tool-result sidecars.
pub(super) fn parse(slice: &[u8]) -> serde_json::Result<Value> {
    if !super::super::store::json::value_compatible(slice) {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "record is not compatible with the rich Value codec",
        ));
    }
    let mut de = serde_json::Deserializer::from_slice(slice);
    let value = Object(Shape::Top).deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

#[derive(Clone, Copy)]
enum Shape {
    Top,
    Message,
    Attachment,
    AgentReturn,
    Compaction,
    Origin,
    Snapshot,
    Backups,
}
struct Object(Shape);
impl<'de> DeserializeSeed<'de> for Object {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> std::result::Result<Value, D::Error> {
        // RawValue rejects neither scalars nor arrays here: the owned codec's object lookup simply
        // ignores them, while all ignored input has already passed strict Value validation.
        let raw = <&RawValue as serde::Deserialize>::deserialize(de)?;
        if !raw.get().starts_with('{') {
            return Ok(Value::Null);
        }
        let mut nested = serde_json::Deserializer::from_str(raw.get());
        nested.deserialize_map(self).map_err(serde::de::Error::custom)
    }
}
impl<'de> Visitor<'de> for Object {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a Claude record object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let nested = nested(self.0, &key);
            if let Some(shape) = nested {
                out.insert(key, map.next_value_seed(Object(shape))?);
            } else if keep(self.0, &key) {
                let raw: &RawValue = map.next_value()?;
                let value = match (self.0, key.as_str()) {
                    (Shape::AgentReturn, "agentId")
                    | (Shape::Backups, _)
                    | (Shape::Top, "scheduledTaskId" | "scheduledFireId") => Value::Null,
                    (Shape::Message, "id") | (Shape::AgentReturn, "status") | (Shape::Compaction, "trigger")
                        if !raw.get().starts_with('"') =>
                    {
                        Value::Null
                    }
                    (Shape::Compaction, "preTokens" | "durationMs")
                        if !matches!(raw.get().as_bytes().first(), Some(b'0'..=b'9')) =>
                    {
                        Value::Null
                    }
                    _ => serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?,
                };
                out.insert(key, value);
            } else {
                map.next_value::<&RawValue>()?;
            }
        }
        Ok(Value::Object(out))
    }
}
fn nested(shape: Shape, key: &str) -> Option<Shape> {
    match (shape, key) {
        (Shape::Top, "message") => Some(Shape::Message),
        (Shape::Top, "attachment") => Some(Shape::Attachment),
        (Shape::Top, "toolUseResult") => Some(Shape::AgentReturn),
        (Shape::Top, "compactMetadata" | "compact_metadata") => Some(Shape::Compaction),
        (Shape::Top, "origin") => Some(Shape::Origin),
        (Shape::Top, "snapshot") => Some(Shape::Snapshot),
        (Shape::Snapshot, "trackedFileBackups") => Some(Shape::Backups),
        _ => None,
    }
}
fn keys(shape: Shape) -> &'static [&'static str] {
    match shape {
        Shape::Message => &["role", "content", "model", "usage", "id"],
        Shape::Attachment => &["type", "systemPrompt"],
        Shape::AgentReturn => &["agentId", "status", "restorable"],
        Shape::Compaction => &["trigger", "preTokens", "durationMs"],
        Shape::Origin => &["kind"],
        Shape::Snapshot => &["trackedFileBackups"],
        Shape::Backups => &[],
        Shape::Top => &[
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
            "scheduledTaskId",
            "scheduledFireId",
            "turnOrigin",
            "subtype",
            "content",
            "rendered",
            "hookCount",
            "hasOutput",
            "hookAdditionalContext",
            "hookInfos",
            "hookErrors",
            "isSnapshotUpdate",
            "snapshot",
            "trackingPath",
            "aiTitle",
            "customTitle",
            "summary",
            "agentName",
            "tag",
            "relocatedCwd",
            "continuedInSessionId",
            "prUrl",
            "message",
            "attachment",
            "toolUseResult",
            "compactMetadata",
            "compact_metadata",
            "origin",
        ],
    }
}
fn keep(shape: Shape, key: &str) -> bool {
    matches!(shape, Shape::Backups) || keys(shape).contains(&key)
}

/// Use the same selected shape as the borrowed compatibility codec after the producer's strict
/// acceptance boundary. Descendants stay views until a retained interpreter field needs them.
#[cfg(not(feature = "mmap"))]
pub(super) fn parse_view(view: JsonView<'_>) -> Result<Value> {
    project_view(view, Shape::Top)
}
#[cfg(not(feature = "mmap"))]
fn project_view(view: JsonView<'_>, shape: Shape) -> Result<Value> {
    use crate::harness::store::document::Fields;
    if !view.is_object()? {
        return Ok(Value::Null);
    }
    match shape {
        Shape::AgentReturn => return agent_return(view),
        Shape::Attachment => return attachment(view),
        Shape::Compaction => return Ok(compact_metadata(view)?.unwrap_or(Value::Null)),
        Shape::Backups => {
            let mut values = Map::new();
            view.for_each_entry(|key, _| {
                values.insert(key.into(), Value::Null);
                Ok(())
            })?;
            return Ok(Value::Object(values));
        }
        _ => {}
    }
    let fields = Fields::parse(view, keys(shape))?.context("Claude facts object")?;
    let mut values = Map::new();
    for &key in keys(shape) {
        let Some(field) = fields.get(key) else { continue };
        let value = if let Some(child) = nested(shape, key) {
            project_view(field, child)?
        } else if matches!((shape, key), (Shape::Top, "scheduledTaskId" | "scheduledFireId"))
            || (matches!((shape, key), (Shape::Message, "id")) && !field.is_string()?)
        {
            Value::Null
        } else {
            field.resolve()?.into_owned()
        };
        values.insert(key.into(), value);
    }
    Ok(Value::Object(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path, options: &ParseOptions) -> Session {
        let r = SessionRef::from_path(path, Harness::Claude);
        let adapter = Claude::with_config_dir(path.parent().unwrap());
        let mut sink = CollectSink::default();
        let mut session = adapter.try_stream(&r, options, &mut sink).unwrap();
        session.messages = sink.messages;
        session
    }

    fn reducer_outputs(session: &Session) -> Value {
        let r = SessionRef::new(
            &session.id,
            Harness::Claude,
            session.source_path.clone().unwrap_or_default(),
        );
        let mut report = crate::doctor::Report::default();
        let mut pass = report.session(session.source_path.clone());
        let mut compactions = crate::compaction::CompactionSink::new(true);
        let mut counts = crate::affordance::Counter::new(&r);
        for message in &session.messages {
            pass.message(message.clone());
            counts.note(message).unwrap();
            let mut owned = message.clone();
            owned.materialize(&session.resolver()).unwrap();
            compactions.message(owned);
        }
        pass.finish().unwrap();
        serde_json::json!({
            "doctor": report.to_json(),
            "compactions": compactions.into_boundaries().unwrap(),
            "affordances": crate::affordance::of_session(session).unwrap(),
            "counts": counts.finish(session, &[]),
            "usage_keys": session.messages.iter().map(|m| crate::stats::usage_key(Harness::Claude, m)).collect::<Vec<_>>(),
            "model": session.model,
            "title": session.title,
            "created_at": session.created_at,
            "updated_at": session.updated_at,
            "lineage": session.lineage,
            "system_prompt": session.system_prompt,
        })
    }

    fn assert_reducers(path: &Path) {
        let full = read(path, &ParseOptions::lazy_extra());
        let facts = read(path, &ParseOptions::facts());
        assert_eq!(reducer_outputs(&facts), reducer_outputs(&full), "{}", path.display());
        for message in &facts.messages {
            for block in &message.content {
                if let Block::ToolResult {
                    details: Some(details), ..
                } = block
                {
                    assert!(details.get("stdout").is_none());
                    assert!(details.get("structuredPatch").is_none());
                }
            }
        }
    }

    #[test]
    fn claude_facts_matches_rich_corpus_reducers() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/claude/2.1.296/.claude/projects");
        let mut count = 0;
        for entry in walkdir::WalkDir::new(&root) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                assert_reducers(entry.path());
                count += 1;
            }
        }
        assert!(count >= 20, "corpus discovery found only {count} transcripts");
    }

    #[test]
    fn claude_facts_preserves_rich_duplicate_type_and_ignored_validation() {
        let directory = cv_testkit::temp_dir("claude-facts-fields");
        let path = directory.join("session.jsonl");
        for identity in [
            r#""id":"first","id":"last""#,
            r#""id":null""#,
            r#""id":{"ignored":"large"}""#,
        ] {
            for content in [r#""content":"small""#, r#""content":"discarded","content":"small""#] {
                let text = format!(
                    r#"{{"type":"assistant","uuid":"reply","isApiErrorMessage":true,"errorDetails":{{"unused":"sidecar"}},"message":{{{identity},{content},"model":"perf","usage":{{"input_tokens":17}}}}}}
{{"type":"system","subtype":"compact_boundary","uuid":"boundary","compactMetadata":{{"trigger":"first","trigger":"auto","preTokens":1,"preTokens":42,"durationMs":5,"unused":{{"large":"ignored"}}}}}}
{{"type":"assistant","parentUuid":"boundary","isCompactSummary":true,"message":{{"content":"summary"}}}}
{{"type":"attachment","attachment":{{"type":"first","type":"hook_success","unused":"ignored"}},"rendered":[{{"content":"injected"}}]}}
{{"type":"permission-mode","permissionMode":"plan"}}
{{"type":"file-history-snapshot","snapshot":{{"trackedFileBackups":{{"src/main.rs":{{"unused":"ignored"}}}}}}}}
{{"type":"custom-title","customTitle":"EOF title"}}
"#
                );
                std::fs::write(&path, text).unwrap();
                assert_reducers(&path);
            }
        }
        for metadata in [
            r#"null"#,
            r#"{"trigger":12,"preTokens":-1,"durationMs":1.2}"#,
            r#"{"trigger":"manual","preTokens":18446744073709551615}"#,
        ] {
            std::fs::write(
                &path,
                format!(r#"{{"type":"system","subtype":"compact_boundary","compactMetadata":{metadata}}}"#),
            )
            .unwrap();
            assert_reducers(&path);
        }
        for ignored in [
            r#"{"unused":1e400}"#.to_string(),
            r#"{"unused":"\ud800"}"#.into(),
            format!("{}0{}", "[".repeat(130), "]".repeat(130)),
        ] {
            for force_fallback in ["", r#", "content":"small""#] {
                std::fs::write(&path, format!(r#"{{"type":"assistant","attachment":{ignored},"message":{{"id":"response","content":"small"{force_fallback}}}}}"#)).unwrap();
                let full = read(&path, &ParseOptions::lazy_extra());
                let facts = read(&path, &ParseOptions::facts());
                assert!(full.messages.is_empty());
                assert!(facts.messages.is_empty());
            }
        }
    }

    #[test]
    fn claude_facts_discards_unrelated_large_sidecars_before_retention() {
        let directory = cv_testkit::temp_dir("claude-facts-sidecars");
        let path = directory.join("session.jsonl");
        for bytes in [16 << 20, 64 << 20] {
            use std::io::Write;
            let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            file.write_all(br#"{"type":"user","toolUseResult":{"agentId":null,"status":"completed","stdout":""#)
                .unwrap();
            let chunk = [b'x'; 65536];
            for _ in 0..bytes / chunk.len() {
                file.write_all(&chunk).unwrap();
            }
            file.write_all(
                br#""},"message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"small"}]}}
{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":42,"unused":""#,
            )
            .unwrap();
            for _ in 0..bytes / chunk.len() {
                file.write_all(&chunk).unwrap();
            }
            file.write_all(
                br#""}}
"#,
            )
            .unwrap();
            file.flush().unwrap();
            assert_reducers(&path);
            let facts = read(&path, &ParseOptions::facts());
            assert!(serde_json::to_vec(&facts).unwrap().len() < 2048);
            assert_eq!(facts.messages[0].kind, MessageKind::SubagentReturn);
        }
    }
}
