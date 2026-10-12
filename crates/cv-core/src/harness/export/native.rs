//! Archive metadata and source ranges, never cached transcript bodies or source mappings.
use super::*;
use crate::harness::store::document::{accepts_value, array_iter, text_summary, ArrayIter, Raw, Record, Source};
use crate::lazy::{Resolver, Text};
use crate::stream::Flow;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

const CACHE_ENTRIES: usize = 8;
const CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}
impl Identity {
    fn read(path: &Path) -> Result<Self> {
        let md = std::fs::metadata(path)?;
        Ok(Self {
            len: md.len(),
            modified: md.modified().ok(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                (md.dev(), md.ino(), md.ctime(), md.ctime_nsec())
            },
            #[cfg(not(unix))]
            created: md.created().ok(),
        })
    }
}
struct Entry {
    id: String,
    offset: u64,
    len: u64,
    refs: [Option<SessionRef>; 2],
}
struct Archive {
    identity: Identity,
    entries: Vec<Entry>,
    charge: usize,
}
struct Cached {
    path: PathBuf,
    archive: Arc<Archive>,
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Cached>,
    bytes: usize,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn slot(kind: Kind) -> usize {
    usize::from(kind == Kind::Claude)
}

/// A cache hit validates the file identity and carries only metadata. An uncached source is
/// retained through the current consumer so a cold selected read need not open the archive twice.
fn load(path: &Path) -> Result<(Arc<Archive>, Option<Source>)> {
    let cache = CACHE.get_or_init(Mutex::default);
    // Relative roots can name different archives after a cwd change. Cache by absolute spelling,
    // while discovery stamps the caller's original path spelling onto returned refs.
    let key = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for _ in 0..2 {
        let identity = Identity::read(path)?;
        {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(position) = guard.entries.iter().position(|e| e.path == key) {
                let hit = guard.entries.remove(position).unwrap();
                if hit.archive.identity == identity {
                    let archive = hit.archive.clone();
                    guard.entries.push_back(hit);
                    return Ok((archive, None));
                }
                guard.bytes -= hit.archive.charge;
            }
        }
        let source = Source::open(path)?;
        let raw = source.raw();
        // The old Vec<RawValue> parser treats a non-array or invalid JSON document as empty.
        let values = raw.ok().and_then(array_iter).into_iter().flatten();
        let shapes = values
            .clone()
            .next()
            .filter(|r| accepts_value(*r))
            .and_then(Record::parse)
            .map(|r| [r.get("mapping").is_some(), r.get("chat_messages").is_some()])
            .unwrap_or([false; 2]);
        let mut entries = Vec::new();
        for raw in values {
            // Match Value acceptance at each conversation boundary, including discarded fields.
            if !accepts_value(raw) {
                continue;
            }
            let Some(record) = Record::parse(raw) else { continue };
            let Some(id) = record
                .get("conversation_id")
                .or_else(|| record.get("id"))
                .or_else(|| record.get("uuid"))
                .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
            else {
                continue;
            };
            let (offset, len) = source.location(raw).context("export source range")?;
            let refs = [Kind::ChatGpt, Kind::Claude].map(|kind| {
                shapes[slot(kind)]
                    .then(|| raw_ref(kind, Path::new(""), &record, &id))
                    .flatten()
            });
            entries.push(Entry { id, offset, len, refs });
        }
        if Identity::read(path)? != identity {
            continue;
        }
        let mut charge = key.as_os_str().len() + entries.capacity() * std::mem::size_of::<Entry>();
        for entry in &entries {
            charge = charge.saturating_add(entry.id.capacity());
            for r in entry.refs.iter().flatten() {
                charge = charge.saturating_add(r.id.capacity() + r.path.as_os_str().len());
                charge = charge.saturating_add(r.title.as_ref().map_or(0, String::capacity));
            }
        }
        let archive = Arc::new(Archive {
            identity,
            entries,
            charge,
        });
        if charge <= CACHE_BYTES {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            // Concurrent readers may build the same archive; replace rather than double-charge.
            if let Some(position) = guard.entries.iter().position(|e| e.path == key) {
                let old = guard.entries.remove(position).unwrap();
                guard.bytes -= old.archive.charge;
            }
            while guard.entries.len() >= CACHE_ENTRIES || guard.bytes + charge > CACHE_BYTES {
                if let Some(old) = guard.entries.pop_front() {
                    guard.bytes -= old.archive.charge;
                } else {
                    break;
                }
            }
            guard.bytes += charge;
            guard.entries.push_back(Cached {
                path: key.clone(),
                archive: archive.clone(),
            });
        }
        return Ok((archive, Some(source)));
    }
    anyhow::bail!("export changed while reading {}", path.display())
}

pub(super) fn discover(kind: Kind, paths: Vec<PathBuf>) -> Vec<SessionRef> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let Ok((archive, _)) = load(&path) else { continue };
        for r in archive.entries.iter().filter_map(|e| e.refs[slot(kind)].as_ref()) {
            if seen.insert(r.id.clone()) {
                let mut r = r.clone();
                r.path = path.clone();
                out.push(r);
            }
        }
    }
    out
}
fn raw_ref(kind: Kind, path: &Path, r: &Record<'_>, id: &str) -> Option<SessionRef> {
    let title = r
        .string(if kind == Kind::ChatGpt { "title" } else { "name" })
        .filter(|s| !s.is_empty());
    let timestamp = |primary, fallback| {
        r.get(primary)
            .or_else(|| r.get(fallback))
            .and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok())
            .as_ref()
            .and_then(ts_from_value)
    };
    let message_count = if kind == Kind::ChatGpt {
        active_chain(r)
            .into_iter()
            .filter_map(Record::parse)
            .filter_map(|r| r.get("message").and_then(Record::parse))
            .filter_map(|r| r.get("author").and_then(Record::parse))
            .filter_map(|r| r.string("role"))
            .filter(|role| matches!(role_from_str(role), Role::User | Role::Assistant))
            .count()
    } else {
        r.get("chat_messages")
            .and_then(array_iter)
            .into_iter()
            .flatten()
            .filter_map(Record::parse)
            .filter(|r| matches!(r.string("sender").as_deref(), Some("human" | "assistant")))
            .count()
    };
    Some(SessionRef {
        title,
        created_at: timestamp("create_time", "created_at"),
        updated_at: timestamp("update_time", "updated_at"),
        message_count,
        ..SessionRef::new(id, kind.harness(), path.to_path_buf())
    })
}
fn active_chain<'a>(conv: &Record<'a>) -> Vec<Raw<'a>> {
    let Some(mapping) = conv.get("mapping").and_then(Record::parse) else {
        return vec![];
    };
    let Some(mut current) = conv.string("current_node") else {
        return mapping.values_in_order().collect();
    };
    let mut chain = Vec::new();
    for _ in 0..100_000 {
        let Some(node) = mapping.get(&current) else { break };
        chain.push(node);
        let Some(parent) = Record::parse(node).and_then(|r| r.string("parent")) else {
            break;
        };
        current = parent;
    }
    chain.reverse();
    chain
}

#[derive(Clone)]
enum Nodes<'a> {
    Branch(std::vec::IntoIter<Raw<'a>>),
    Chronological(Option<ArrayIter<'a>>),
}
impl<'a> Iterator for Nodes<'a> {
    type Item = Raw<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Branch(nodes) => nodes.next(),
            Self::Chronological(nodes) => nodes.as_mut()?.next(),
        }
    }
}

pub(super) fn stream(kind: Kind, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
    let (archive, cold) = load(&r.path)?;
    let entry = archive
        .entries
        .iter()
        .find(|e| e.id == r.id)
        .with_context(|| format!("conversation {} not found in {}", r.id, r.path.display()))?;
    let source = if let Some(source) = cold {
        source
    } else {
        #[cfg(feature = "mmap")]
        {
            Source::open(&r.path)?
        }
        #[cfg(not(feature = "mmap"))]
        {
            Source::range(&r.path, entry.offset, entry.len)?
        }
    };
    anyhow::ensure!(
        Identity::read(&r.path)? == archive.identity,
        "export changed while reading {}",
        r.path.display()
    );
    let raw = source
        .slice(entry.offset, entry.len)
        .context("export source range changed")?;
    // A warm mmap needs the local endpoint index, but only validates the selected conversation.
    anyhow::ensure!(accepts_value(raw), "export conversation changed");
    let conv = Record::parse(raw).context("export conversation is not an object")?;
    let mut session = Session::new(&r.id, kind.harness());
    session.title = r.title.clone();
    session.created_at = r.created_at;
    session.updated_at = r.updated_at;
    session.source_path = Some(r.path.clone());
    session.model = conv.string("default_model_slug").filter(|s| !s.is_empty());
    let nodes = if kind == Kind::ChatGpt {
        Nodes::Branch(active_chain(&conv).into_iter())
    } else {
        Nodes::Chronological(conv.get("chat_messages").and_then(array_iter))
    };
    let resolver = Resolver::new(Some(r.path.clone()));
    // The old replay emits a final fallback title even when the consumer stops immediately.
    // Inspect only up to the first emitted user message; use source spans to bound its body.
    if session.title.is_none() {
        let mut calls = HashMap::new();
        for raw in nodes.clone() {
            let record = Record::parse(raw);
            let role = match kind {
                Kind::Claude => record.and_then(|r| r.string("sender")),
                Kind::ChatGpt => record
                    .and_then(|r| r.get("message"))
                    .and_then(Record::parse)
                    .and_then(|r| r.get("author"))
                    .and_then(Record::parse)
                    .and_then(|r| r.string("role")),
            };
            if role.as_deref().map(role_from_str) != Some(Role::User) {
                continue;
            }
            if let Some(m) = build_message(kind, raw, &source, true, &resolver, &mut calls) {
                if m.role == Role::User {
                    session.title = text_summary(
                        m.content.iter().filter_map(|b| match b {
                            Block::Text { text } => Some(text),
                            _ => None,
                        }),
                        &resolver,
                        true,
                    );
                    break;
                }
            }
        }
    }
    sink.meta(&session);
    let mut calls = HashMap::new();
    for raw in nodes {
        if let Some(m) = build_message(kind, raw, &source, opts.spans, &resolver, &mut calls) {
            if sink.message(m) == Flow::Stop {
                break;
            }
        }
    }
    Ok(session)
}

fn build_message(
    kind: Kind,
    raw: Raw<'_>,
    source: &Source,
    spans: bool,
    resolver: &Resolver,
    calls: &mut HashMap<String, String>,
) -> Option<Message> {
    match kind {
        Kind::Claude => claude_message(raw, source, spans),
        Kind::ChatGpt => chatgpt_message(raw, source, spans, resolver, calls),
    }
}
fn claude_message(raw: Raw<'_>, source: &Source, spans: bool) -> Option<Message> {
    let r = Record::parse(raw)?;
    let role = role_from_str(&r.string("sender")?);
    let mut content = Vec::new();
    for raw in r.get("content").and_then(array_iter).into_iter().flatten() {
        let Some(mut block) = anthropic::block_from_raw(raw, source, &anthropic::CLAUDE_AI, spans) else {
            continue;
        };
        match &mut block {
            Block::ToolResult { content, .. } | Block::Thinking { text: content, .. } if content.is_empty() => {
                if let Some(text) = Record::parse(raw)
                    .and_then(|r| r.get("text"))
                    .and_then(|raw| source.text(raw, spans))
                {
                    *content = text;
                }
            }
            _ => {}
        }
        content.push(block);
    }
    if content.is_empty() {
        if let Some(text) = r
            .get("text")
            .and_then(|raw| source.text(raw, spans))
            .filter(|s| !s.is_empty())
        {
            content.push(Block::Text { text });
        }
    }
    if content.is_empty() {
        return None;
    }
    let mut m = Message::new(role);
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.id = r.string("uuid");
    m.parent_id = r.string("parent_message_uuid");
    m.timestamp = r.value("created_at").as_ref().and_then(ts_from_value);
    m.content = content;
    Some(m)
}
fn chatgpt_message(
    raw: Raw<'_>,
    source: &Source,
    spans: bool,
    resolver: &Resolver,
    calls: &mut HashMap<String, String>,
) -> Option<Message> {
    let node = Record::parse(raw)?;
    let msg = node.get("message").and_then(Record::parse)?;
    let author = msg.get("author").and_then(Record::parse)?;
    let role = role_from_str(&author.string("role")?);
    let id = node.string("id").unwrap_or_default();
    let recipient = msg.string("recipient").unwrap_or_else(|| "all".into());
    let raw_content = msg.get("content");
    let content_record = raw_content.and_then(Record::parse);
    let mut content = Vec::new();
    if role == Role::Assistant && recipient != "all" {
        content.push(Block::ToolUse {
            id: id.clone(),
            name: recipient.clone(),
            input: raw_content
                .and_then(|raw| serde_json::from_str(raw.get()).ok())
                .unwrap_or(Value::Null),
            namespace: None,
        });
        calls.insert(recipient, id.clone());
    } else {
        let text = chatgpt_text(content_record.as_ref(), source, spans, resolver);
        if role == Role::Tool {
            let name = author.string("name").unwrap_or_default();
            content.push(Block::ToolResult {
                tool_use_id: calls.get(&name).cloned().unwrap_or_default(),
                content: text,
                is_error: false,
                tool_name: (!name.is_empty()).then_some(name),
                status: None,
                details: None,
            });
        } else if text_summary(std::iter::once(&text), resolver, true).is_some() {
            content.push(Block::Text { text });
        }
        if let Some(parts) = content_record
            .as_ref()
            .and_then(|r| r.get("parts"))
            .and_then(array_iter)
        {
            for pointer in parts
                .into_iter()
                .filter_map(Record::parse)
                .filter_map(|r| r.string("asset_pointer"))
            {
                content.push(Block::Image {
                    media_type: None,
                    data_ref: Some(pointer),
                });
            }
        }
    }
    if content.is_empty() {
        return None;
    }
    let mut m = Message::new(role);
    if role == Role::System {
        m.kind = MessageKind::SystemPrompt;
    }
    m.id = (!id.is_empty()).then_some(id);
    m.parent_id = node.string("parent");
    m.timestamp = msg.value("create_time").as_ref().and_then(ts_from_value);
    m.content = content;
    Some(m)
}
fn chatgpt_text(r: Option<&Record<'_>>, source: &Source, spans: bool, resolver: &Resolver) -> Text {
    let Some(r) = r else { return Text::default() };
    if let Some(parts) = r.get("parts").and_then(array_iter) {
        let mut texts = parts.filter_map(|raw| source.text(raw, spans));
        let Some(first) = texts.next() else {
            return Text::default();
        };
        let Some(second) = texts.next() else { return first };
        // A joined string has no single source interval. Preserve its exact owned semantics.
        let mut joined = first.resolve(resolver).into_owned();
        joined.push('\n');
        joined.push_str(&second.resolve(resolver));
        for text in texts {
            joined.push('\n');
            joined.push_str(&text.resolve(resolver));
        }
        return joined.into();
    }
    r.get("text")
        .and_then(|raw| source.text(raw, spans))
        .unwrap_or_default()
}
