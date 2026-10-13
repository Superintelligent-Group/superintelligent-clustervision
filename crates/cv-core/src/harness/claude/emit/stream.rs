//! One staged transcript, with only identity/tool facts retained for global resumability.
use super::*;
use crate::{
    harness::{
        store::JsonlStage,
        wire::{
            anthropic::write as blocks,
            write::{Object, ObjectWriter},
        },
    },
    lazy::Resolver,
    Message,
};
use anyhow::Context as _;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::Write,
};

struct Context<'a> {
    session: &'a Session,
    session_id: String,
    agent: Option<String>,
    cwd: String,
    branch: Option<String>,
    ts: String,
    mode: ThinkingMode,
}
struct Record<'a> {
    generated: bool,
    base: Cow<'a, Map<String, Value>>,
    extras: Option<&'a Map<String, Value>>,
    blocks: Vec<Cow<'a, Block>>,
    role: Role,
    special: Option<MessageKind>,
    sidecar: Option<&'a Value>,
}
impl Record<'_> {
    fn with_fields<T>(&self, f: impl FnOnce(Object<'_>, Option<Object<'_>>) -> Result<T>) -> Result<T> {
        let mut top = Object::new(&self.base);
        let mut message = top.get("message").and_then(Value::as_object).map(Object::new);
        if let Some(extras) = self.extras {
            for (key, value) in extras {
                if is_internal_extra_key(key) || key == "sessionId" || key == "cwd" {
                    continue;
                }
                if key == crate::harness::claude::TOOL_USE_RESULT_KEY && top.get(key).is_some() {
                    continue;
                }
                if let Some(sub) = key.strip_prefix(crate::harness::claude::MESSAGE_EXTRA_PREFIX) {
                    if let Some(message) = message.as_mut() {
                        message.set(sub, value);
                    }
                } else {
                    top.set(key, value);
                    if key == "message" {
                        message = value.as_object().map(Object::new);
                    }
                }
            }
        }
        f(top, message)
    }
    fn generated_content(&self, message: &Object<'_>) -> bool {
        if !self.generated {
            return false;
        }
        self.base
            .get("message")
            .and_then(|m| m.get("content"))
            .zip(message.get("content"))
            .is_some_and(|(original, current)| std::ptr::eq(original, current))
    }
    fn bare(&self) -> bool {
        self.role == Role::User && self.blocks.iter().all(|b| matches!(b.as_ref(), Block::Text { .. }))
    }
    fn eligible(&self, b: &Block) -> bool {
        match self.role {
            Role::Tool => matches!(b, Block::ToolResult { .. }),
            Role::Assistant => !matches!(b, Block::ToolResult { .. }),
            _ => true,
        }
    }
    fn default_empty(&self) -> bool {
        self.role == Role::Assistant && !self.blocks.iter().any(|b| self.eligible(b))
    }
    /// (before, after): only an assistant nonempty array reduced to empty drops its record.
    fn array_facts(
        &self,
        message: Option<&Object<'_>>,
        answered: Option<&HashSet<String>>,
        mut note: impl FnMut(&str, bool),
    ) -> (usize, usize) {
        let Some(message) = message else { return (0, 0) };
        if self.special.is_some() {
            return (0, 0);
        }
        if self.generated_content(message) {
            if self.bare() {
                return (0, 0);
            }
            if self.default_empty() {
                return (1, 1);
            }
            let (mut before, mut after) = (0, 0);
            for b in &self.blocks {
                if !self.eligible(b) {
                    continue;
                }
                before += 1;
                match b.as_ref() {
                    Block::ToolUse { id, .. } => {
                        note(id, false);
                        if answered.is_some_and(|a| !a.contains(id)) {
                            continue;
                        }
                    }
                    Block::ToolResult { tool_use_id, .. } => note(tool_use_id, true),
                    _ => {}
                }
                after += 1;
            }
            (before, after)
        } else {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                return (0, 0);
            };
            let mut after = 0;
            for b in content {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        note(id, false);
                        if answered
                            .is_some_and(|a| b.get("id").and_then(Value::as_str).is_none_or(|id| !a.contains(id)))
                        {
                            continue;
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                            note(id, true);
                        }
                    }
                    _ => {}
                }
                after += 1;
            }
            (content.len(), after)
        }
    }
    fn survives(
        &self,
        top: &Object<'_>,
        message: Option<&Object<'_>>,
        answered: &HashSet<String>,
        agent: Option<&str>,
    ) -> bool {
        if agent.is_some() && top.get("type").and_then(Value::as_str) == Some("ai-title") {
            return false;
        }
        if top.get("type").and_then(Value::as_str) == Some("assistant") {
            let (before, after) = self.array_facts(message, Some(answered), |_, _| {});
            if before > 0 && after == 0 {
                return false;
            }
        }
        true
    }
    fn write_content(
        &self,
        writer: &mut dyn Write,
        message: &Object<'_>,
        answered: Option<&HashSet<String>>,
        resolver: &Resolver,
    ) -> Result<()> {
        if self.generated_content(message) {
            if self.special == Some(MessageKind::CompactionSummary) {
                return self.write_joined(writer, resolver);
            }
            if self.bare() {
                return crate::harness::wire::write::joined_text(
                    writer,
                    self.blocks.iter().filter_map(|b| {
                        if let Block::Text { text } = b.as_ref() {
                            Some(text)
                        } else {
                            None
                        }
                    }),
                    resolver,
                );
            }
            writer.write_all(b"[")?;
            let mut first = true;
            for b in &self.blocks {
                if !self.eligible(b) {
                    continue;
                }
                if let Block::ToolUse { id, .. } = b.as_ref() {
                    if answered.is_some_and(|a| !a.contains(id)) {
                        continue;
                    }
                }
                if !first {
                    writer.write_all(b",")?;
                }
                first = false;
                blocks::block(writer, b, &anthropic::ANTHROPIC, self.role != Role::Assistant, resolver)?;
            }
            if first && self.default_empty() {
                serde_json::to_writer(&mut *writer, &json!({"type":"text","text":""}))?;
            }
            writer.write_all(b"]")?;
        } else if let Some(content) = message.get("content") {
            if let (Some(answered), Some(array)) = (answered, content.as_array()) {
                writer.write_all(b"[")?;
                let mut first = true;
                for b in array {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && b.get("id")
                            .and_then(Value::as_str)
                            .is_none_or(|id| !answered.contains(id))
                    {
                        continue;
                    }
                    if !first {
                        writer.write_all(b",")?;
                    }
                    first = false;
                    serde_json::to_writer(&mut *writer, b)?;
                }
                writer.write_all(b"]")?;
            } else {
                serde_json::to_writer(writer, content)?;
            }
        }
        Ok(())
    }
    fn write(
        &self,
        writer: &mut dyn Write,
        c: &Context<'_>,
        answered: &HashSet<String>,
        ids: &HashSet<String>,
        previous: Option<&str>,
        resolver: &Resolver,
    ) -> Result<Option<String>> {
        self.with_fields(|top, message| {
            if !self.survives(&top, message.as_ref(), answered, c.agent.as_deref()) {
                return Ok(None);
            }
            let uuid = top.get("uuid").and_then(Value::as_str).map(str::to_owned);
            let parent = top
                .get("parentUuid")
                .and_then(Value::as_str)
                .filter(|p| !ids.contains(*p))
                .map(|_| json!(previous));
            let identity = json!(c.session_id);
            let cwd = json!(c.cwd);
            let agent = json!(c.agent);
            let side = Value::Bool(c.agent.is_some());
            let mut top = top.shortened();
            if let Some(parent) = &parent {
                top.set("parentUuid", parent);
            }
            // Carriers only rewrite identity fields they originally held.
            if top.get("sessionId").is_some() {
                top.set("sessionId", &identity);
            }
            if top.get("cwd").is_some() {
                top.set("cwd", &cwd);
            }
            if c.agent.is_some() {
                top.set("isSidechain", &side);
                top.set("agentId", &agent);
            } else {
                if top.get("isSidechain").is_some() {
                    top.set("isSidechain", &side);
                }
                top.remove("agentId");
            }
            let prune = top.get("type").and_then(Value::as_str) == Some("assistant");
            top.write(writer, |key, value, writer| {
                if key == "message" {
                    if let Some(message) = &message {
                        message.write(writer, |key, _, writer| {
                            if key == "content" {
                                self.write_content(writer, message, prune.then_some(answered), resolver)?;
                                Ok(true)
                            } else {
                                Ok(false)
                            }
                        })?;
                        return Ok(true);
                    }
                }
                if self.special.is_some() && self.base.get(key).is_some_and(|original| std::ptr::eq(original, value)) {
                    if key == "content" {
                        self.write_joined(writer, resolver)?;
                        return Ok(true);
                    }
                    if key == "rendered" {
                        writer.write_all(b"[{\"content\":")?;
                        self.write_joined(writer, resolver)?;
                        writer.write_all(b"}]")?;
                        return Ok(true);
                    }
                }
                if key == crate::harness::claude::TOOL_USE_RESULT_KEY
                    && self.base.get(key).is_some_and(|original| std::ptr::eq(original, value))
                {
                    let Some(details) = self.sidecar else { return Ok(false) };
                    if let Some(parked) = details.get(crate::harness::claude::TOOL_USE_RESULT_KEY) {
                        serde_json::to_writer(writer, parked)?;
                    } else if let Some(map) = details.as_object() {
                        let mut object = Object::new(map);
                        object.remove(crate::harness::claude::PERSISTED_OUTPUT_KEY);
                        object.write(writer, |_, _, _| Ok(false))?;
                    } else {
                        serde_json::to_writer(writer, details)?;
                    }
                    return Ok(true);
                }
                Ok(false)
            })?;
            Ok(uuid)
        })
    }
    fn write_joined(&self, writer: &mut dyn Write, resolver: &Resolver) -> Result<()> {
        crate::harness::wire::write::joined_text(
            writer,
            self.blocks.iter().filter_map(|b| {
                if let Block::Text { text } = b.as_ref() {
                    Some(text)
                } else {
                    None
                }
            }),
            resolver,
        )
    }
}

#[derive(Debug)]
struct Nonblank;
impl std::fmt::Display for Nonblank {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nonblank text found")
    }
}
impl std::error::Error for Nonblank {}
fn nonblank(text: &crate::Text, resolver: &Resolver) -> Result<bool> {
    if let Some(inline) = text.inline_str() {
        return Ok(!inline.trim_start().is_empty());
    }
    match text.try_for_each_chunk(resolver, 64 * 1024, |chunk| {
        if !chunk.trim_start().is_empty() {
            return Err(Nonblank.into());
        }
        Ok(())
    }) {
        Ok(()) => Ok(false),
        Err(error) if error.is::<Nonblank>() => Ok(true),
        Err(error) => Err(error),
    }
}

fn projected<'a>(message: &'a Message, mode: ThinkingMode, resolver: &Resolver) -> Result<Vec<Cow<'a, Block>>> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for b in &message.content {
        if let Block::Thinking {
            text,
            signature,
            encrypted,
            redacted,
        } = b
        {
            if mode == ThinkingMode::Drop {
                continue;
            }
            if mode == ThinkingMode::Text {
                // Signatures/encrypted payloads can establish structural retention alone.
                if crate::emit::thinking_fate_with_presence(
                    Harness::Claude,
                    mode,
                    false,
                    signature.as_deref(),
                    encrypted.as_deref(),
                    *redacted,
                ) == crate::emit::ThinkingFate::Keep
                {
                    blocks.push(Cow::Borrowed(b));
                    continue;
                }
                let nonblank = nonblank(text, resolver)?;
                match crate::emit::thinking_fate_with_presence(
                    Harness::Claude,
                    mode,
                    nonblank,
                    signature.as_deref(),
                    encrypted.as_deref(),
                    *redacted,
                ) {
                    crate::emit::ThinkingFate::Drop => continue,
                    crate::emit::ThinkingFate::Placeholder => {
                        blocks.push(Cow::Owned(Block::Text {
                            text: crate::emit::thinking_placeholder(
                                signature.as_deref(),
                                encrypted.as_deref(),
                                *redacted,
                            )
                            .into(),
                        }));
                        continue;
                    }
                    _ => {}
                }
            }
        }
        blocks.push(Cow::Borrowed(b));
    }
    Ok(blocks)
}
fn resolve_parent(dropped: &HashMap<String, Option<String>>, pid: String) -> Option<String> {
    let mut p = Some(pid);
    for _ in 0..=dropped.len() {
        match p.as_ref().and_then(|id| dropped.get(id)) {
            Some(next) => p = next.clone(),
            None => break,
        }
    }
    p
}
fn record<'a>(
    message: &'a Message,
    c: &Context<'a>,
    uuid: Option<&str>,
    previous: &mut Option<String>,
    dropped: &mut HashMap<String, Option<String>>,
    resolver: &Resolver,
) -> Result<Option<Record<'a>>> {
    if let Some(carrier) = carrier_record_of(message) {
        return Ok(Some(Record {
            generated: false,
            base: Cow::Borrowed(carrier.as_object().unwrap()),
            extras: None,
            blocks: Vec::new(),
            role: message.role,
            special: None,
            sidecar: None,
        }));
    }
    let special = (c.session.harness != Harness::Claude
        && matches!(
            message.kind,
            MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
        ))
    .then_some(message.kind);
    let parent = message
        .parent_id
        .clone()
        .map(|p| resolve_parent(dropped, p))
        .unwrap_or_else(|| previous.clone());
    if special.is_none() && message.role == Role::System {
        if let Some(id) = &message.id {
            dropped.insert(id.clone(), parent);
        }
        return Ok(None);
    }
    let uuid = uuid.context("missing reconstructed UUID")?;
    let ts = message
        .timestamp
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| c.ts.clone());
    let mut base = Map::new();
    // Special-record key order is the existing codec's literal order.
    if let Some(kind) = special {
        match kind {
            MessageKind::CompactionBoundary => {
                base.insert("type".into(), json!("system"));
                base.insert("subtype".into(), json!("compact_boundary"));
                base.insert("level".into(), json!("info"));
            }
            MessageKind::CompactionSummary => {
                base.insert("type".into(), json!("user"));
                base.insert("isCompactSummary".into(), json!(true));
            }
            _ => {
                base.insert("type".into(), json!("attachment"));
            }
        }
    }
    base.insert("uuid".into(), json!(uuid));
    base.insert("parentUuid".into(), json!(parent));
    base.insert("sessionId".into(), json!(c.session_id));
    base.insert("cwd".into(), json!(c.cwd));
    base.insert("timestamp".into(), json!(ts));
    if special.is_none() {
        if let Some(branch) = &c.branch {
            base.insert("gitBranch".into(), json!(branch));
        }
    }
    if let Some(kind) = special {
        match kind {
            MessageKind::CompactionBoundary => {
                base.insert("content".into(), Value::Null);
                base.insert("compactMetadata".into(), json!({"trigger":"manual","preTokens":0}));
            }
            MessageKind::CompactionSummary => {
                base.insert("message".into(), json!({"role":"user","content":Value::Null}));
            }
            _ => {
                base.insert("attachment".into(), json!({"type":"injected_context"}));
                base.insert("rendered".into(), Value::Null);
            }
        }
    } else {
        base.insert(
            "type".into(),
            json!(if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            }),
        );
        let mut inner = Map::new();
        inner.insert(
            "role".into(),
            json!(if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            }),
        );
        if message.role == Role::Assistant {
            if let Some(model) = message.model.as_ref().or(c.session.model.as_ref()) {
                inner.insert("model".into(), json!(model));
            }
            if let Some(usage) = claude_usage(message.usage.as_ref()) {
                inner.insert("usage".into(), usage);
            }
        }
        inner.insert("content".into(), Value::Null);
        base.insert("message".into(), Value::Object(inner));
    }
    let sidecar = if special.is_none() {
        message
            .content
            .iter()
            .find_map(|b| {
                if let Block::ToolResult {
                    details: Some(details), ..
                } = b
                {
                    Some(details)
                } else {
                    None
                }
            })
            .filter(|details| {
                details.as_object().is_none_or(|map| {
                    map.get(crate::harness::claude::TOOL_USE_RESULT_KEY).is_some()
                        || map.keys().any(|k| k != crate::harness::claude::PERSISTED_OUTPUT_KEY)
                })
            })
    } else {
        None
    };
    if sidecar.is_some() {
        base.insert(crate::harness::claude::TOOL_USE_RESULT_KEY.into(), Value::Null);
    }
    *previous = Some(uuid.to_owned());
    Ok(Some(Record {
        generated: true,
        base: Cow::Owned(base),
        extras: (c.session.harness == Harness::Claude && special.is_none())
            .then(|| message.harness_extra(Harness::Claude))
            .flatten(),
        blocks: projected(message, c.mode, resolver)?,
        role: if special == Some(MessageKind::CompactionSummary) {
            Role::User
        } else {
            message.role
        },
        special,
        sidecar,
    }))
}

pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    crate::emit::validate_native_source(session, Harness::Claude, opts.thinking)?;
    let (session_id, agent) = match &opts.sidechain_of {
        Some((root, agent)) => (root.clone(), Some(agent.clone())),
        None => (opts.new_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()), None),
    };
    let new_id = agent.clone().unwrap_or_else(|| session_id.clone());
    let cwd = effective_cwd(session, opts).map(|p| realpath_for_claude(&p));
    let c = Context {
        session,
        session_id,
        agent,
        cwd: cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        branch: branch_of(session),
        ts: session
            .created_at
            .or(session.updated_at)
            .unwrap_or_else(Utc::now)
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        mode: opts.thinking,
    };
    let resolver = session.resolver();
    let uuid: Vec<_> = session
        .messages
        .iter()
        .map(|message| {
            let special = session.harness != Harness::Claude
                && matches!(
                    message.kind,
                    MessageKind::CompactionBoundary | MessageKind::CompactionSummary | MessageKind::InjectedContext
                );
            (carrier_record_of(message).is_none() && (special || message.role != Role::System))
                .then(|| message.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string()))
        })
        .collect();
    let mut answered = HashSet::new();
    let mut ids = HashSet::new();
    let (mut previous, mut dropped) = (None, HashMap::new());
    for (message, uuid) in session.messages.iter().zip(&uuid) {
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            record.with_fields(|_, message| {
                record.array_facts(message.as_ref(), None, |id, result| {
                    if result {
                        answered.insert(id.to_owned());
                    }
                });
                Ok(())
            })?;
        }
    }
    previous = None;
    dropped.clear();
    for (message, uuid) in session.messages.iter().zip(&uuid) {
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            record.with_fields(|top, message| {
                if record.survives(&top, message.as_ref(), &answered, c.agent.as_deref()) {
                    if let Some(id) = top.get("uuid").and_then(Value::as_str) {
                        ids.insert(id.to_owned());
                    }
                }
                Ok(())
            })?;
        }
    }
    let mut lines = JsonlStage::new();
    let has_title_carrier = session
        .messages
        .iter()
        .any(|m| carrier_record_of(m).is_some_and(|r| r.get("type").and_then(Value::as_str) == Some("ai-title")));
    if let Some(title) = &session.title {
        if !has_title_carrier && c.agent.is_none() {
            lines.push_with(|writer| {
                let mut o = ObjectWriter::new(writer)?;
                o.field("type", "ai-title")?;
                o.field("aiTitle", title)?;
                o.field("sessionId", &c.session_id)?;
                o.field("cwd", &c.cwd)?;
                o.field("timestamp", &c.ts)?;
                o.finish()
            })?;
        }
    }
    if session.harness != Harness::Claude {
        if let Some(prompt) = session.system_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            lines.push_with(|writer| {
                let mut o = ObjectWriter::new(writer)?;
                o.field("type", "attachment")?;
                o.field("sessionId", &c.session_id)?;
                o.field("cwd", &c.cwd)?;
                o.field("timestamp", &c.ts)?;
                o.field_with("attachment", |writer| {
                    let mut a = ObjectWriter::new(writer)?;
                    a.field("type", "prompt_snapshot")?;
                    a.field_with("systemPrompt", |writer| {
                        writer.write_all(b"[")?;
                        serde_json::to_writer(&mut *writer, prompt)?;
                        writer.write_all(b"]")?;
                        Ok(())
                    })?;
                    a.finish()
                })?;
                if let Some(agent) = &c.agent {
                    o.field("isSidechain", true)?;
                    o.field("agentId", agent)?;
                }
                o.finish()
            })?;
        }
    }
    previous = None;
    dropped.clear();
    let mut surviving_previous = None;
    for (message, uuid) in session.messages.iter().zip(&uuid) {
        if let Some(record) = record(message, &c, uuid.as_deref(), &mut previous, &mut dropped, &resolver)? {
            let survives = record.with_fields(|top, message| {
                Ok(record.survives(&top, message.as_ref(), &answered, c.agent.as_deref()))
            })?;
            if !survives {
                continue;
            }
            let mut emitted_uuid = None;
            lines.push_with(|writer| {
                emitted_uuid = record.write(writer, &c, &answered, &ids, surviving_previous.as_deref(), &resolver)?;
                Ok(())
            })?;
            if emitted_uuid.is_some() {
                surviving_previous = emitted_uuid;
            }
        }
    }
    let file_path = match &c.agent {
        Some(agent) => {
            fs::create_dir_all(out_dir)?;
            out_dir.join(format!("agent-{agent}.jsonl"))
        }
        None => {
            let project = out_dir.join(cwd.as_deref().map(claude_encode_cwd).unwrap_or_else(|| "-".into()));
            fs::create_dir_all(&project)?;
            project.join(format!("{new_id}.jsonl"))
        }
    };
    lines.publish(&file_path)?;
    let resume = match (&c.agent, &cwd) {
        (Some(agent), _) => format!(
            "from session {}: SendMessage to=\"{agent}\" with the next instruction",
            c.session_id
        ),
        (None, Some(cwd)) => format!("claude --resume {new_id}  (run from {})", cwd.display()),
        (None, None) => format!("claude --resume {new_id}"),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}
