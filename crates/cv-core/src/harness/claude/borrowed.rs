//! Borrowed conversational records for lean readers. Full/complete parsing stays in the owned
//! codec. Strings that become spans are validated and sized without constructing decoded Values;
//! escaped metadata strings use Cow rather than rejecting a valid record.

use super::*;
use serde::de::{value::MapAccessDeserializer, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::borrow::Cow;
use std::fmt;

struct Str<'a>(Cow<'a, str>);
impl<'de: 'a, 'a> Deserialize<'de> for Str<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E>(self, s: &'de str) -> Result<Self::Value, E> {
                Ok(Str(Cow::Borrowed(s)))
            }
            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
                Ok(Str(Cow::Owned(s.to_owned())))
            }
            fn visit_string<E>(self, s: String) -> Result<Self::Value, E> {
                Ok(Str(Cow::Owned(s)))
            }
        }
        d.deserialize_str(V)
    }
}

/// Distinguish a missing key from a present null (scheduler ids use key presence).
#[derive(Default)]
struct Raw<'a>(Option<&'a RawValue>);
impl<'de: 'a, 'a> Deserialize<'de> for Raw<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <&RawValue>::deserialize(d).map(|raw| Raw(Some(raw)))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    #[serde(borrow, rename = "type", deserialize_with = "hot_kind")]
    kind: Str<'a>,
    #[serde(borrow, deserialize_with = "object")]
    message: Body<'a>,
    #[serde(borrow, default)]
    uuid: Option<Str<'a>>,
    #[serde(borrow, default)]
    parent_uuid: Option<Str<'a>>,
    #[serde(borrow, default)]
    timestamp: Option<Str<'a>>,
    #[serde(borrow, default)]
    cwd: Option<Str<'a>>,
    #[serde(borrow, default)]
    git_branch: Option<Str<'a>>,
    #[serde(borrow, default)]
    permission_mode: Option<Str<'a>>,
    #[serde(default)]
    is_meta: Option<bool>,
    #[serde(default)]
    is_compact_summary: Option<bool>,
    #[serde(default)]
    is_api_error_message: Option<bool>,
    #[serde(borrow, default)]
    scheduled_task_id: Raw<'a>,
    #[serde(borrow, default)]
    scheduled_fire_id: Raw<'a>,
    #[serde(borrow, default)]
    turn_origin: Option<Str<'a>>,
    #[serde(borrow, default)]
    origin: Raw<'a>,
    #[serde(borrow, default)]
    tool_use_result: Option<AgentReturn<'a>>,
    #[serde(borrow, default)]
    error: Raw<'a>,
    #[serde(borrow, default)]
    error_details: Raw<'a>,
    #[serde(borrow, default)]
    api_error_status: Raw<'a>,
    #[serde(borrow, default)]
    request_id: Raw<'a>,
}

fn hot_kind<'de, D: Deserializer<'de>>(d: D) -> Result<Str<'de>, D::Error> {
    let kind = Str::deserialize(d)?;
    if matches!(kind.0.as_ref(), "user" | "assistant") {
        Ok(kind)
    } else {
        Err(serde::de::Error::custom("not a conversational record"))
    }
}

struct AgentReturn<'a> {
    agent_id: Raw<'a>,
    status: Raw<'a>,
}
impl<'de: 'a, 'a> Deserialize<'de> for AgentReturn<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields: AgentReturnFields<'a> = object(d)?;
        Ok(Self {
            agent_id: fields.agent_id,
            status: fields.status,
        })
    }
}

/// Serde's derived structs also accept positional arrays. The owned codec interprets only
/// objects as messages, blocks and tool sidecars, so use a map-only boundary here.
fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    struct V<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    d.deserialize_map(V(std::marker::PhantomData))
}
struct Object<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        object(d).map(Self)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentReturnFields<'a> {
    #[serde(borrow, default)]
    agent_id: Raw<'a>,
    #[serde(borrow, default)]
    status: Raw<'a>,
}

#[derive(Deserialize)]
struct Body<'a> {
    #[serde(borrow, default)]
    model: Option<Str<'a>>,
    #[serde(borrow, default)]
    content: Raw<'a>,
    #[serde(borrow, default)]
    usage: Raw<'a>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(borrow, rename = "type", default)]
    kind: Option<Str<'a>>,
    #[serde(borrow, default)]
    text: Raw<'a>,
    #[serde(borrow, default)]
    thinking: Raw<'a>,
    #[serde(borrow, default)]
    content: Raw<'a>,
    #[serde(borrow, default)]
    id: Option<Str<'a>>,
    #[serde(borrow, default)]
    name: Option<Str<'a>>,
    #[serde(borrow, default)]
    tool_use_id: Option<Str<'a>>,
    #[serde(borrow, default)]
    input: Raw<'a>,
    #[serde(borrow, default)]
    signature: Option<Str<'a>>,
    #[serde(borrow, default)]
    data: Raw<'a>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(borrow, default)]
    source: Raw<'a>,
    #[serde(borrow, default)]
    title: Option<Str<'a>>,
}

fn s<'a>(field: &'a Option<Str<'_>>) -> Option<&'a str> {
    field.as_ref().map(|s| s.0.as_ref())
}
fn owned(field: Option<Str<'_>>) -> Option<String> {
    field.map(|s| s.0.into_owned())
}

/// Return None only before any session/sink mutation: the existing codec can then handle an
/// unusual shape, duplicate key, non-string metadata, or a future record without losing it.
pub(super) fn ingest(
    session: &mut Session,
    slice: &[u8],
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    span: Option<&SpanCtx>,
) -> Option<Flow> {
    if slice.first() != Some(&b'{') {
        return None;
    }
    let record: Record<'_> = serde_json::from_slice(slice).ok()?;
    // This is eligibility for the borrowed projection, not a new rejection policy. A failed
    // check retries the existing lean codec, including its permissive ignored-sidecar behavior.
    if !super::super::store::json::value_compatible(slice) {
        return None;
    }
    let role = match record.kind.0.as_ref() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let origin_kind = match record.origin.0 {
        Some(raw) if raw.get().starts_with('{') => {
            #[derive(Deserialize)]
            struct OriginKind<'a> {
                #[serde(borrow, default)]
                kind: Option<Str<'a>>,
            }
            serde_json::from_str::<OriginKind<'_>>(raw.get()).ok()?.kind
        }
        _ => None,
    };
    let user_origin = user_origin_from(
        record.scheduled_task_id.0.is_some()
            || record.scheduled_fire_id.0.is_some()
            || s(&record.turn_origin) == Some("scheduled"),
        record.is_meta == Some(true),
        s(&origin_kind),
    );
    let (blocks, head, bare) = content(
        &record.message.content,
        (role == Role::User).then_some(user_origin),
        span,
    )?;
    let sync_agent_return = record.tool_use_result.as_ref().is_some_and(|result| {
        let status = result
            .status
            .0
            .and_then(|v| serde_json::from_str::<Str<'_>>(v.get()).ok());
        result.agent_id.0.is_some() && status.as_ref().map(|s| s.0.as_ref()) != Some("async_launched")
    });
    let synthetic = role == Role::Assistant
        && (record.is_api_error_message == Some(true) || s(&record.message.model) == Some(SYNTHETIC_MODEL));
    let role =
        if role == Role::User && !blocks.is_empty() && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. })) {
            Role::Tool
        } else {
            role
        };
    let class = ConversationClass {
        synthetic,
        api_error: synthetic && record.is_api_error_message == Some(true),
        compact_summary: record.is_compact_summary == Some(true),
        sync_agent_return,
        slash_command: bare
            && (head.text.trim_start().starts_with("<command-name>")
                || head.text.trim_start().starts_with("<local-command-stdout>")),
        user: head
            .user
            .unwrap_or_else(|| user_kind_from_head(user_origin, &head.text)),
    };
    let (role, kind, origin) = classify_conversation(role, class, false);
    let mut bag = Map::new();
    if synthetic {
        if let Some(value) = record.is_api_error_message {
            bag.insert("isApiErrorMessage".into(), Value::Bool(value));
        }
        for (key, raw) in [
            ("error", &record.error),
            ("errorDetails", &record.error_details),
            ("apiErrorStatus", &record.api_error_status),
            ("requestId", &record.request_id),
        ] {
            if let Some(raw) = raw.0.filter(|v| v.get() != "null") {
                bag.insert(key.into(), serde_json::from_str(raw.get()).ok()?);
            }
        }
    }
    let mut extra = Map::new();
    if !bag.is_empty() {
        extra.insert(Harness::Claude.as_str().into(), Value::Object(bag));
    }
    let timestamp = s(&record.timestamp).and_then(parse_ts);
    let usage = match record.message.usage.0 {
        Some(raw) => Some(parse_usage(&serde_json::from_str::<Value>(raw.get()).ok()?)),
        None => None,
    };
    if let Some(mode) = s(&record.permission_mode) {
        crate::affordance::note_mode(session.harness_extra_mut(Harness::Claude), mode);
    }
    note_conversation_metadata(session, s(&record.cwd), s(&record.git_branch), timestamp);
    let message = Message {
        id: owned(record.uuid),
        parent_id: owned(record.parent_uuid),
        role,
        kind,
        origin,
        timestamp,
        model: if synthetic { None } else { owned(record.message.model) },
        content: blocks,
        usage,
        extra,
    };
    Some(emit_conversation(session, message, opts, sink, span))
}

fn content<'a>(
    raw: &Raw<'a>,
    head_origin: Option<Origin>,
    span: Option<&SpanCtx>,
) -> Option<(Vec<Block>, Head<'a>, bool)> {
    let mut blocks = Vec::new();
    let mut head = Head::default();
    let Some(raw) = raw.0 else {
        return Some((blocks, head, false));
    };
    if raw.get().starts_with('"') {
        if let Some(origin) = head_origin {
            head = classification_head(raw, origin)?;
        }
        blocks.push(Block::Text { text: text(raw, span)? });
        return Some((blocks, head, true));
    }
    if raw.get().starts_with('[') {
        let mut de = serde_json::Deserializer::from_str(raw.get());
        let parsed = Items { span, head_origin }.deserialize(&mut de).ok()?;
        return Some((parsed.0, parsed.1, false));
    }
    Some((blocks, head, false))
}

struct Items<'s, 'c> {
    span: Option<&'s SpanCtx<'c>>,
    head_origin: Option<Origin>,
}
impl<'de, 's, 'c> DeserializeSeed<'de> for Items<'s, 'c> {
    type Value = (Vec<Block>, Head<'de>);
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_seq(self)
    }
}
impl<'de, 's, 'c> Visitor<'de> for Items<'s, 'c> {
    type Value = (Vec<Block>, Head<'de>);
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("content blocks")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut blocks = Vec::new();
        let mut head = Head::default();
        let mut first = true;
        while let Some(Object(item)) = seq.next_element::<Object<Item<'de>>>()? {
            if first {
                if let (Some(origin), Some(raw)) = (self.head_origin, item.text.0.filter(|v| v.get().starts_with('"')))
                {
                    head = classification_head(raw, origin).ok_or_else(|| serde::de::Error::custom("invalid text"))?;
                }
            }
            first = false;
            let block = block(item, self.span).ok_or_else(|| serde::de::Error::custom("unsupported content shape"))?;
            if let Some(block) = block {
                blocks.push(block);
            }
        }
        Ok((blocks, head))
    }
}

fn block(item: Item<'_>, span: Option<&SpanCtx>) -> Option<Option<Block>> {
    Some(match s(&item.kind) {
        Some("text") if item.text.0.is_some_and(|v| v.get().starts_with('"')) => Some(Block::Text {
            text: text(item.text.0?, span)?,
        }),
        Some("thinking") => Some(Block::Thinking {
            text: match item.thinking.0.filter(|v| v.get().starts_with('"')) {
                Some(v) => text(v, span)?,
                None => Text::default(),
            },
            signature: owned(item.signature),
            encrypted: None,
            redacted: false,
        }),
        Some("redacted_thinking") => Some(Block::Thinking {
            text: Text::default(),
            signature: None,
            encrypted: item
                .data
                .0
                .and_then(|v| serde_json::from_str::<Str<'_>>(v.get()).ok())
                .map(|s| s.0.into_owned()),
            redacted: true,
        }),
        Some("tool_use") => Some(Block::ToolUse {
            id: owned(item.id).unwrap_or_default(),
            name: owned(item.name).unwrap_or_default(),
            input: match item.input.0 {
                Some(v) => serde_json::from_str(v.get()).ok()?,
                None => Value::Null,
            },
            namespace: None,
        }),
        Some("tool_result") => {
            let content = match item.content.0.filter(|v| v.get().starts_with('"')) {
                Some(v) => text(v, span)?,
                None => {
                    let value: Option<Value> =
                        item.content.0.map(|v| serde_json::from_str(v.get())).transpose().ok()?;
                    Text::Inline(super::super::wire::coerce_text(value.as_ref()))
                }
            };
            // A persisted-output stub has a derived pointer even when content is a span.
            let details = match item.content.0 {
                Some(raw) if raw.get().starts_with('"') && json_prefix(raw, "<persisted-output>")? => {
                    let decoded: Str<'_> = serde_json::from_str(raw.get()).ok()?;
                    persisted_output(&decoded.0)
                }
                _ => content.inline_str().and_then(persisted_output),
            }
            .map(|(path, size)| serde_json::json!({"persistedOutput":{"path":path,"size":size}}));
            Some(Block::ToolResult {
                tool_use_id: owned(item.tool_use_id).unwrap_or_default(),
                content,
                is_error: item.is_error.unwrap_or(false),
                tool_name: None,
                status: None,
                details,
            })
        }
        Some("image" | "document") => {
            let image = s(&item.kind) == Some("image");
            let (mime, source) = source(item.source, image)?;
            Some(if image {
                Block::Image {
                    media_type: mime,
                    data_ref: source,
                }
            } else {
                Block::File {
                    mime,
                    path: owned(item.title),
                    source,
                }
            })
        }
        _ => None,
    })
}

fn source(raw: Raw<'_>, base64_marker: bool) -> Option<(Option<String>, Option<String>)> {
    #[derive(Deserialize)]
    struct Source<'a> {
        #[serde(borrow, rename = "type", default)]
        kind: Option<Str<'a>>,
        #[serde(borrow, default)]
        media_type: Option<Str<'a>>,
        #[serde(borrow, default)]
        url: Option<Str<'a>>,
        #[serde(borrow, default)]
        file_id: Option<Str<'a>>,
        #[serde(borrow, default)]
        data: Raw<'a>,
    }
    let Some(raw) = raw.0.filter(|v| v.get().starts_with('{')) else {
        return Some((None, None));
    };
    let source: Source<'_> = serde_json::from_str(raw.get()).ok()?;
    let reference = match s(&source.kind) {
        Some("url") => owned(source.url),
        Some("file") => owned(source.file_id).map(|s| format!("file:{s}")),
        Some("base64") | None if base64_marker => source
            .data
            .0
            .filter(|v| v.get().starts_with('"'))
            .map(|_| "base64:inline".into()),
        Some(other) if base64_marker => Some(other.into()),
        _ => None,
    };
    Some((owned(source.media_type), reference))
}

/// Decoded UTF-8 byte length and raw characters, without allocating the string. In particular,
/// validate surrogate pairs: RawValue skips may accept them but the ordinary Value path does not.
fn decoded_chars(raw: &RawValue, mut visit: impl FnMut(char) -> bool) -> Option<usize> {
    let quoted = raw.get();
    let body = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = body.chars();
    let mut len = 0;
    while let Some(c) = chars.next() {
        let c = if c != '\\' {
            c
        } else {
            match chars.next()? {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let mut hex =
                        || -> Option<u32> { (0..4).try_fold(0, |n, _| Some(n * 16 + chars.next()?.to_digit(16)?)) };
                    let mut code = hex()?;
                    if (0xd800..=0xdbff).contains(&code) {
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        let low = (0..4).try_fold(0, |n, _| Some(n * 16 + chars.next()?.to_digit(16)?))?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                    }
                    char::from_u32(code)?
                }
                _ => return None,
            }
        };
        len += c.len_utf8();
        if !visit(c) {
            break;
        }
    }
    Some(len)
}

/// Count escape reductions over byte slices; ordinary UTF-8 runs are never walked character by
/// character. This preserves the decoded threshold even for ASCII written as Unicode escapes.
fn decoded_len(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut at = 0;
    let mut len = 0;
    while let Some(next) = memchr::memchr(b'\\', &bytes[at..]) {
        len += next;
        at += next;
        if *bytes.get(at + 1)? == b'u' {
            let hex =
                |offset: usize| u32::from_str_radix(std::str::from_utf8(bytes.get(offset..offset + 4)?).ok()?, 16).ok();
            let mut code = hex(at + 2)?;
            at += 6;
            if (0xd800..=0xdbff).contains(&code) {
                if bytes.get(at..at + 2)? != b"\\u" {
                    return None;
                }
                let low = hex(at + 2)?;
                if !(0xdc00..=0xdfff).contains(&low) {
                    return None;
                }
                code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                at += 6;
            }
            len += char::from_u32(code)?.len_utf8();
        } else {
            at += 2;
            len += 1;
        }
    }
    Some(len + bytes.len() - at)
}

#[derive(Default)]
struct Head<'a> {
    text: Cow<'a, str>,
    user: Option<(MessageKind, Origin)>,
}

/// Keep the actual prefix and an explicit classification projection. A notification's first
/// complete summary can occur after a giant output; inspect it without owning the decoded body.
fn classification_head(raw: &RawValue, origin: Origin) -> Option<Head<'_>> {
    let body = raw.get().strip_prefix('"')?.strip_suffix('"')?;
    if !body.contains('\\') {
        return Some(Head {
            text: Cow::Borrowed(body),
            user: Some(user_kind_from_head(origin, body)),
        });
    }
    let mut prefix = String::new();
    decoded_chars(raw, |c| {
        if prefix.is_empty() && c.is_whitespace() {
            return true;
        }
        prefix.push(c);
        prefix.len() < 64
    })?;
    let user = if origin == Origin::Harness && prefix.starts_with("<task-notification>") {
        if notification_returns_agent(raw)? {
            (MessageKind::SubagentReturn, Origin::Subagent)
        } else {
            (MessageKind::InjectedContext, Origin::Harness)
        }
    } else {
        user_kind_from_head(origin, &prefix)
    };
    Some(Head {
        text: Cow::Owned(prefix),
        user: Some(user),
    })
}

/// Match tag_inner's first opening tag and its first following closing tag. Decoder windows and
/// tag/word suffixes are bounded; a promising summary without a close remains absent. Ingest has
/// already syntax-validated the whole record and checked Value compatibility. A decoder failure
/// still discards all tentative state, before any session or sink mutation.
fn notification_returns_agent(raw: &RawValue) -> Option<bool> {
    let body = raw.get().strip_prefix('"')?.strip_suffix('"')?;
    let mut summary = NotificationSummary::default();
    crate::lazy::decode_json_body(&mut std::io::Cursor::new(body.as_bytes()), &mut |chunk| {
        summary.push(chunk)
    })
    .ok()?;
    Some(summary.complete && ((summary.agent_possible && summary.agent == 6) || summary.workflow_found))
}

struct NotificationSummary {
    opened: bool,
    complete: bool,
    pending: String,
    scratch: String,
    leading: bool,
    agent_possible: bool,
    agent: usize,
    workflow_found: bool,
    workflow: usize,
}
impl Default for NotificationSummary {
    fn default() -> Self {
        Self {
            opened: false,
            complete: false,
            pending: String::new(),
            scratch: String::new(),
            leading: true,
            agent_possible: true,
            agent: 0,
            workflow_found: false,
            workflow: 0,
        }
    }
}
impl NotificationSummary {
    fn push(&mut self, chunk: &str) {
        if self.complete {
            return;
        }
        self.scratch.clear();
        self.scratch.push_str(&self.pending);
        self.scratch.push_str(chunk);
        self.pending.clear();
        let mut body = self.scratch.as_str();
        if !self.opened {
            if let Some(at) = body.find("<summary>") {
                self.opened = true;
                body = &body[at + 9..];
            } else {
                let n = suffix(body.as_bytes(), b"<summary>", false);
                self.pending.push_str(&body[body.len() - n..]);
                return;
            }
        }
        if let Some(at) = body.find("</summary>") {
            body = &body[..at];
            self.complete = true;
        } else {
            let n = suffix(body.as_bytes(), b"</summary>", false);
            self.pending.push_str(&body[body.len() - n..]);
            body = &body[..body.len() - n];
        }
        if self.leading {
            body = body.trim_start();
            self.leading = body.is_empty();
        }
        if self.agent_possible && self.agent < 6 {
            let n = body.len().min(6 - self.agent);
            if body.as_bytes()[..n] == b"Agent "[self.agent..self.agent + n] {
                self.agent += n;
            } else {
                self.agent_possible = false;
            }
        }
        if !self.workflow_found {
            const WORD: &[u8] = b"workflow";
            // Only a proper suffix can bridge windows. Never copy the complete summary.
            let mut bridge = [0; 14];
            bridge[..self.workflow].copy_from_slice(&WORD[..self.workflow]);
            let n = body.len().min(7);
            bridge[self.workflow..self.workflow + n].copy_from_slice(&body.as_bytes()[..n]);
            let bridge = &bridge[..self.workflow + n];
            let contains = |bytes: &[u8]| {
                memchr::memchr2_iter(b'w', b'W', bytes).any(|at| {
                    bytes
                        .get(at..at + 8)
                        .is_some_and(|word| word.eq_ignore_ascii_case(WORD))
                })
            };
            self.workflow_found = contains(bridge) || contains(body.as_bytes());
            self.workflow = suffix(if body.len() < 7 { bridge } else { body.as_bytes() }, WORD, true);
        }
    }
}

/// Longest proper suffix matching an ASCII tag/word prefix (therefore a UTF-8 boundary).
fn suffix(body: &[u8], wanted: &[u8], insensitive: bool) -> usize {
    (1..wanted.len().min(body.len() + 1))
        .rev()
        .find(|&n| {
            let end = &body[body.len() - n..];
            if insensitive {
                end.eq_ignore_ascii_case(&wanted[..n])
            } else {
                end == &wanted[..n]
            }
        })
        .unwrap_or(0)
}

fn text(raw: &RawValue, span: Option<&SpanCtx>) -> Option<Text> {
    // The common unescaped case needs no character walk for sizing. Escaped strings are sized
    // without a decoded buffer, maintaining the existing decoded INLINE_MAX threshold.
    let body = raw.get().strip_prefix('"')?.strip_suffix('"')?;
    if let Some(ctx) = span {
        // A JSON escape needs at most six raw bytes per decoded UTF-8 byte. Large strings thus
        // exceed the threshold without a second walk of their escapes; close cases are exact.
        if body.len() > INLINE_MAX * 6 || decoded_len(body)? > INLINE_MAX {
            return raw_string_span(raw, ctx).map(Text::Span);
        }
    }
    serde_json::from_str::<String>(raw.get()).ok().map(Text::Inline)
}

fn json_prefix(raw: &RawValue, prefix: &str) -> Option<bool> {
    let body = raw.get().strip_prefix('"')?.strip_suffix('"')?;
    if !body.contains('\\') {
        return Some(body.trim_start().starts_with(prefix));
    }
    let mut expected = prefix.chars();
    let mut matching = true;
    let mut started = false;
    decoded_chars(raw, |c| {
        if !matching {
            return false;
        }
        if !started && c.is_whitespace() {
            return true;
        }
        started = true;
        if let Some(want) = expected.next() {
            matching = c == want;
        }
        matching && expected.clone().next().is_some()
    })?;
    Some(matching && expected.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_notification_classification_matches_complete_summary_oracle() {
        let mut bodies = vec![
            "<task-notification><summary>Agent a completed</summary>".to_string(),
            "<task-notification><summary>agent a completed</summary>".into(),
            "<task-notification><summary>  \u{2003}Agent a completed</summary>".into(),
            "<task-notification><summary>WoRkFlOw completed</summary>".into(),
            "<task-notification><summary>work</summary>flow".into(),
            "<task-notification>workflow<summary>command done</summary>".into(),
            "<task-notification><summary>command done</summary><summary>Agent a</summary>".into(),
            "<task-notification><summary>Agent a".into(),
            "<task-notification><summary>workflow</Summary>".into(),
            "<task-notification><summary></summary><summary>workflow</summary>".into(),
            "<task-notification></summary><summ<summary>workflow</summary>".into(),
            "<task-notification><summary><summary>Agent a</summary></summary>".into(),
            "<task-notification><summary>wo🦝rkflow</summary>".into(),
            "<task-notification><summary>wwoWorkflow done</summary>".into(),
            "[Request interrupted by user]".into(),
            "<command-name>/help</command-name>".into(),
        ];
        for summary in ["Agent completed", "worKflow done", "command done", "Agent unclosed"] {
            let close = if summary.ends_with("unclosed") {
                ""
            } else {
                "</summary>"
            };
            bodies.push(format!(
                "\u{2003}<task-notification>{}<summary>{summary}{close}",
                "ignored\n🦝".repeat(20_000)
            ));
        }
        for body in bodies {
            for origin in [Origin::Harness, Origin::Human, Origin::Scheduler] {
                // Force both escaped and unescaped JSON representations, including escaped tags.
                for source in [
                    serde_json::to_string(&format!("{body}\n")).unwrap(),
                    serde_json::to_string(&format!("{body}\n"))
                        .unwrap()
                        .replace('<', r"\u003c"),
                ] {
                    let raw: &RawValue = serde_json::from_str(&source).unwrap();
                    let decoded: String = serde_json::from_str(&source).unwrap();
                    let head = decoded.trim_start();
                    let expected = if head.starts_with("[Request interrupted by user") {
                        (MessageKind::InjectedContext, Origin::Harness)
                    } else if origin != Origin::Harness {
                        (MessageKind::Prompt, origin)
                    } else if head.starts_with("<task-notification>")
                        && tag_inner(head, "summary").is_some_and(|summary| {
                            let summary = summary.trim_start();
                            summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
                        })
                    {
                        (MessageKind::SubagentReturn, Origin::Subagent)
                    } else {
                        (MessageKind::InjectedContext, Origin::Harness)
                    };
                    assert_eq!(classification_head(raw, origin).unwrap().user, Some(expected));
                }
            }
            for content in [
                serde_json::json!(body),
                serde_json::json!([{"type":"text","text":body}]),
            ] {
                assert_modes(
                    &serde_json::json!({"type":"user","isMeta":true,"message":{"content":content}}).to_string(),
                );
            }
        }
    }

    #[test]
    fn notification_summary_crosses_windows_and_discards_late_decode_failure() {
        for body in [
            "🦝<summary>\u{2003}Agent completed</summary>outside workflow",
            "<summary>woRkFlOw done</summary>",
            "<summary>work</summary>flow<summary>Agent ignored</summary>",
            "<summary>Agent unclosed",
            "<summary><summary>workflow</summary>",
        ] {
            let expected = tag_inner(body, "summary").is_some_and(|summary| {
                let summary = summary.trim_start();
                summary.starts_with("Agent ") || summary.to_ascii_lowercase().contains("workflow")
            });
            for width in 1..24 {
                let mut summary = NotificationSummary::default();
                let mut start = 0;
                while start < body.len() {
                    let mut end = (start + width).min(body.len());
                    while !body.is_char_boundary(end) {
                        end += 1;
                    }
                    summary.push(&body[start..end]);
                    start = end;
                }
                assert_eq!(
                    summary.complete && ((summary.agent_possible && summary.agent == 6) || summary.workflow_found),
                    expected,
                    "width {width}"
                );
            }
        }
        // RawValue permits this surrogate. Even a completed positive summary in an earlier
        // window must not publish classification if the defensive decoder later rejects input.
        let invalid = format!("\"<summary>Agent complete</summary>{}\\ud800\"", "a".repeat(70_000));
        let raw: &RawValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(notification_returns_agent(raw), None);
    }

    fn assert_modes(text: &str) {
        let dir = cv_testkit::temp_dir("claude-borrowed-parity");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, text).unwrap();
        for opts in [
            ParseOptions::bulk(),
            ParseOptions::lazy(),
            ParseOptions::full(),
            ParseOptions::complete(),
        ] {
            let mut expected_sink = CollectSink::default();
            let mut expected = stream_str("session", text, Some(path.clone()), &opts, &mut expected_sink);
            expected.messages = expected_sink.messages;
            let mut actual_sink = CollectSink::default();
            let reader = BufReader::new(std::fs::File::open(&path).unwrap());
            let mut actual = stream_reader("session", reader, Some(path.clone()), &opts, &mut actual_sink);
            actual.messages = actual_sink.messages;
            actual.materialize();
            let actual = serde_json::to_value(actual).unwrap();
            let expected = serde_json::to_value(expected).unwrap();
            #[cfg(feature = "mmap")]
            if opts.spans {
                let mut mapped_sink = CollectSink::default();
                let mut mapped = stream_bytes_at(
                    new_session("session", Some(path.clone())),
                    text.as_bytes(),
                    0,
                    &opts,
                    &mut mapped_sink,
                );
                mapped.messages = mapped_sink.messages;
                mapped.materialize();
                assert!(
                    serde_json::to_value(mapped).unwrap() == expected,
                    "mapped mode {opts:?}"
                );
            }
            fn mismatch(a: &Value, b: &Value, path: &str) -> Option<String> {
                match (a, b) {
                    (Value::Object(a), Value::Object(b)) => {
                        for key in a.keys().chain(b.keys()) {
                            let p = format!("{path}/{key}");
                            match (a.get(key), b.get(key)) {
                                (Some(a), Some(b)) => {
                                    if let Some(m) = mismatch(a, b, &p) {
                                        return Some(m);
                                    }
                                }
                                _ => return Some(format!("{p}: field presence differs")),
                            }
                        }
                    }
                    (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
                        for (i, (a, b)) in a.iter().zip(b).enumerate() {
                            if let Some(m) = mismatch(a, b, &format!("{path}/{i}")) {
                                return Some(m);
                            }
                        }
                    }
                    _ if a != b => return Some(format!("{path}: actual {a}, expected {b}")),
                    _ => {}
                }
                None
            }
            assert!(
                actual == expected,
                "mode {opts:?}: {}",
                mismatch(&actual, &expected, "").unwrap()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn borrowed_records_match_owned_vocabulary_and_unusual_shapes() {
        let mut records = vec![
            serde_json::json!({"type":"assistant","uuid":"id\n\"","cwd":"/escaped\npath","gitBranch":"b\n","permissionMode":"plan","timestamp":"2026-01-02T00:00:00Z","message":{"model":"model\n","usage":null,"content":[
                {"type":"text","text":"quoted\n\"text\"".repeat(1000)},
                {"type":"thinking","thinking":"thought\n".repeat(1000),"signature":"sig\n"},
                {"type":"redacted_thinking","data":"cipher"},
                {"type":"tool_use","id":"call","name":"Read","input":{"path":"/a","content":"input"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"base64".repeat(1000)}},
                {"type":"document","title":"pdf","source":{"type":"file","file_id":"f"}},
                {"type":"future_block","text":"ignored"}
            ]}}),
            serde_json::json!({"type":"user","scheduledTaskId":null,"message":{"content":"scheduled\n".repeat(1000)}}),
            serde_json::json!({"type":"user","isMeta":true,"origin":{"kind":"hook"},"message":{"content":[{"type":"text","text":"<task-notification><summary>Workflow done</summary></task-notification>\n".repeat(1000)}]}}),
            serde_json::json!({"type":"user","toolUseResult":{"agentId":null,"status":"done"},"message":{"content":[{"type":"tool_result","tool_use_id":"call","content":"result\n".repeat(1000)}]}}),
            serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"part"}],"is_error":true}]}}),
            serde_json::json!({"type":"user","toolUseResult":[{"type":"text","text":"5"}],"message":{"content":[{"type":"tool_result","content":"5"}]}}),
            serde_json::json!({"type":"assistant","isApiErrorMessage":false,"errorDetails":{"request":"r"},"message":{"model":"<synthetic>","content":"error"}}),
            serde_json::json!({"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"content":"  [Request interrupted by user]\n".repeat(1000)}}),
            serde_json::json!({"type":"user","message":{"content":"  <command-name>/compact</command-name>\n"}}),
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":42},{"type":"thinking","thinking":null},42,{"type":"tool_result","content":{"x":"y"}}]}}),
            serde_json::json!({"type":"assistant","uuid":42,"message":null}),
            serde_json::json!({"type":"assistant","message":["model", "content", null]}),
            serde_json::json!({"type":"assistant","message":{"content":[["text", "positional text"]]}}),
            serde_json::json!({"type":"assistant","message":{"model":42,"usage":42,"content":false}}),
            serde_json::json!({"type":"new_record","message":{"content":"unmodelled"}}),
        ];
        for name in ["rich_blocks", "sidechain_subagent", "compact_summary", "system_lines"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/claude/{name}.jsonl"));
            let text = std::fs::read_to_string(path).unwrap();
            assert_modes(&text);
        }
        // Order and escaped keys must not determine which fast path understands the record.
        for record in records.drain(..) {
            let mut text = record.to_string();
            text = text
                .replace("\"message\"", r#""mess\u0061ge""#)
                .replace("\"type\"", r#""ty\u0070e""#);
            assert_modes(&format!("{text}\n"));
        }
        assert_modes("{\"type\":\"assistant\",\"message\":{\"content\":\"a\",\"content\":\"b\"}}\n");
        assert_modes("{\"type\":\"assistant\",\"ignored\":\"\\ud800\",\"message\":{\"content\":\"ok\"}}\n");
        for field in ["ignored", "usage", "input"] {
            let text = match field {
                "ignored" => r#"{"type":"user","ignored":1e400,"message":{"content":"ok"}}"#.to_owned(),
                "usage" => r#"{"type":"assistant","message":{"usage":{"ignored":1e400},"content":"ok"}}"#.to_owned(),
                _ => r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"ignored":1e400}}]}}"#
                    .to_owned(),
            };
            assert_modes(&format!("{text}\n"));
        }
        for depth in [126, 127, 128, 129] {
            let nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert_modes(&format!(
                r#"{{"type":"assistant","ignored":{nested},"message":{{"content":"ok"}}}}"#
            ));
        }
    }

    #[test]
    fn borrowed_records_match_committed_claude_transcripts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/claude");
        let mut n = 0;
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                assert_modes(&std::fs::read_to_string(path).unwrap());
                n += 1;
            }
        }
        assert!(n > 0, "no Claude corpus transcripts tested");
    }

    #[test]
    fn borrowed_strings_and_decoded_threshold_match_serde() {
        let samples = [
            "plain λ🦝",
            "new\nline\tquote\"",
            "\\uD800 literal",
            "\u{8}\u{c}\r/",
            "🦝",
        ];
        for sample in samples {
            let quoted = serde_json::to_string(sample).unwrap();
            let raw: &RawValue = serde_json::from_str(&quoted).unwrap();
            assert_eq!(decoded_len(&quoted[1..quoted.len() - 1]), Some(sample.len()));
            assert_eq!(text(raw, None).unwrap().inline_str(), Some(sample));
        }
        for quoted in [
            r#""\u0061\u03bb\ud83e\udd9d""#,
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud800\u0041""#,
        ] {
            let oracle = serde_json::from_str::<String>(quoted).ok();
            assert_eq!(
                decoded_len(&quoted[1..quoted.len() - 1]),
                oracle.as_ref().map(String::len)
            );
        }
        let quoted = format!("\"{}\"", r"\u0061".repeat(INLINE_MAX));
        let raw: &RawValue = serde_json::from_str(&quoted).unwrap();
        let ctx = SpanCtx {
            slice: quoted.as_bytes(),
            base_off: 0,
        };
        assert!(
            !text(raw, Some(&ctx)).unwrap().is_span(),
            "threshold is decoded bytes, not escaped bytes"
        );
        let launch =
            br#"{"type":"user","toolUseResult":{"agentId":"a","status":"async_launched"},"message":{"content":[]}}"#;
        let parsed: Record<'_> = serde_json::from_slice(launch).unwrap();
        let result = parsed.tool_use_result.unwrap();
        let status: Str<'_> = serde_json::from_str(result.status.0.unwrap().get()).unwrap();
        assert_eq!(status.0, "async_launched");
        let record = br#"{"type":"assistant","message":{"model":"m","content":"x"},"uuid":"u"}"#;
        let parsed: Record<'_> = serde_json::from_slice(record).unwrap();
        assert!(matches!(parsed.kind.0, Cow::Borrowed(_)));
        assert!(matches!(parsed.uuid.unwrap().0, Cow::Borrowed(_)));
    }
}
