//! Full-fidelity tail reads continued only from the adapters' existing verified checkpoints.
use crate::{harness::Adapter, source, stream, Harness, Message, Session, SessionRef};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CONSUMER: &str = "watch-full1";
const STATE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "/watch-full1");
const CHECKPOINT_EVERY: u64 = 256 * 1024;
const ANCHOR_BYTES: usize = 64 * 1024;
const METADATA_BYTES: usize = 16 * 1024;

fn state_version(adapter: &dyn Adapter) -> String {
    crate::resume::version(adapter, CONSUMER).unwrap_or_else(|| {
        format!(
            "{STATE_VERSION}/view{}/{}",
            crate::affordance::VIEW_VERSION,
            adapter.harness().as_str()
        )
    })
}

fn fits(value: &impl Serialize, limit: usize) -> bool {
    struct Limited(usize);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("checkpoint exceeds cache budget"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limited(limit), value).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Anchor {
    head: crate::resume::MarkHead,
    checkpoint: stream::Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Completed {
    session: Session,
    total: usize,
}

/// A completed read's source baseline and optional stable append boundary. This is consumer
/// state, not a second source index. Unsupported adapters transparently read whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadState {
    #[serde(default)]
    version: String,
    path: PathBuf,
    pub source: source::Snapshot,
    anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<Completed>,
    // Primary inode/ctime is separate from append dependencies: growth invalidates EOF metadata,
    // while the immutable-prefix checkpoint may remain useful.
    #[serde(default)]
    identity: Option<source::ReadSet>,
}

impl ReadState {
    pub(crate) fn baseline(r: &SessionRef) -> Self {
        Self {
            version: crate::harness::for_harness(r.harness)
                .map_or_else(String::new, |adapter| state_version(adapter.as_ref())),
            path: r.path.clone(),
            source: source::Probe::default().snapshot(r),
            anchor: None,
            completed: None,
            identity: None,
        }
    }

    /// Fully completed consumer read still describes this source, including dynamic dependencies.
    pub fn fresh(&self, r: &SessionRef) -> bool {
        crate::harness::for_harness(r.harness).is_some_and(|adapter| self.fresh_with(adapter.as_ref(), r))
    }

    /// Freshness against the same concrete adapter that will perform the next read.
    pub fn fresh_with(&self, adapter: &dyn Adapter, r: &SessionRef) -> bool {
        let mut probe = source::Probe::default();
        let now = probe.snapshot(r);
        self.version == state_version(adapter)
            && self.path == r.path
            && self.identity.as_ref().is_some_and(|guard| guard.fresh(&mut probe))
            && self.source.fresh(&now, &mut probe)
    }

    pub(crate) fn baseline_unchanged(&self, r: &SessionRef) -> bool {
        crate::harness::for_harness(r.harness).is_some_and(|adapter| self.version == state_version(adapter.as_ref()))
            && self.path == r.path
            && source::unchanged(r, &self.source)
            && self
                .identity
                .as_ref()
                .is_none_or(|guard| guard.fresh(&mut source::Probe::default()))
    }

    pub fn fits_cache(&self, limit: usize) -> bool {
        fits(self, limit)
    }

    /// The checkpoint suitable for a tail starting at `skip`. Growth is interpreted under the
    /// adapter's immutable-prefix contract; MarkHead samples do not prove arbitrary edits absent.
    fn from<'a>(
        &'a self,
        adapter: &dyn Adapter,
        r: &SessionRef,
        current: &source::Snapshot,
        skip: usize,
    ) -> Option<&'a stream::Checkpoint> {
        if self.version != state_version(adapter) || self.path != r.path || current.primary.1 < self.source.primary.1 {
            return None;
        }
        let same = current.primary == self.source.primary;
        if same
            && !self
                .identity
                .as_ref()
                .is_some_and(|guard| guard.fresh(&mut source::Probe::default()))
        {
            return None;
        }
        if !same && current.primary.1 <= self.source.primary.1 {
            return None;
        }
        if !self.source.dependencies_fresh(current, &mut source::Probe::default()) {
            return None;
        }
        let anchor = self.anchor.as_ref()?;
        let version = crate::resume::version(adapter, CONSUMER)?;
        (anchor.checkpoint.messages()? <= skip && anchor.head.holds(&r.path, current.primary.1 as u64, &version))
            .then_some(&anchor.checkpoint)
    }
}

/// EOF metadata and selected full messages, plus positions that can be accepted after a bounded
/// callback reports how much it consumed. Never checkpoint past that accepted message position.
pub struct ReadPass {
    version: String,
    pub session: Session,
    pub messages: Vec<Message>,
    pub total: usize,
    pub resumed: bool,
    /// Old messages proven stable by the adapter checkpoint. Archives replace the suffix after
    /// this boundary, including a Codex EOF message whose trailing usage may change on append.
    pub prefix: usize,
    pub parsed_messages: usize,
    /// Primary bytes from the admitted checkpoint to captured EOF, excluding auxiliary reads
    /// and guard samples. A bounded unchanged read may stop before traversing this whole suffix.
    pub suffix_bytes: u64,
    source: source::Snapshot,
    path: PathBuf,
    before: Option<Anchor>,
    last: Option<Anchor>,
    completed: Option<Completed>,
    identity: source::ReadSet,
}

impl ReadPass {
    pub fn accepted(&self, seen_to: usize) -> ReadState {
        let anchor = self
            .last
            .as_ref()
            .filter(|anchor| anchor.checkpoint.messages().is_some_and(|n| n <= seen_to))
            .or_else(|| {
                self.before
                    .as_ref()
                    .filter(|anchor| anchor.checkpoint.messages().is_some_and(|n| n <= seen_to))
            })
            .cloned();
        ReadState {
            version: self.version.clone(),
            path: self.path.clone(),
            source: self.source.clone(),
            anchor,
            completed: self.completed.clone(),
            identity: Some(self.identity.clone()),
        }
    }
}

struct Tail {
    total: usize,
    skip: usize,
    keep: usize,
    messages: Vec<Message>,
    before: Option<stream::Checkpoint>,
    checkpoint_disabled: bool,
    last: Option<stream::Checkpoint>,
    stop_after_selection: bool,
}
impl stream::MessageSink for Tail {
    fn message(&mut self, message: Message) -> stream::Flow {
        if self.total >= self.skip && self.messages.len() < self.keep {
            self.messages.push(message);
        }
        self.total += 1;
        if self.stop_after_selection && self.total >= self.skip && self.messages.len() >= self.keep {
            stream::Flow::Stop
        } else {
            stream::Flow::Continue
        }
    }
    fn wants_checkpoint(&mut self, _: u64) -> bool {
        !self.checkpoint_disabled
    }
    fn checkpoint(&mut self, cp: &mut stream::Checkpoint) {
        if !fits(cp, ANCHOR_BYTES) {
            self.checkpoint_disabled = true;
            return;
        }
        if cp.messages().is_some_and(|n| n <= self.skip) {
            self.before = Some(cp.clone());
        }
        self.last = Some(cp.clone());
    }
}

/// A whole or verified resumed pass, retaining only the requested tail and always reaching EOF
/// for exact metadata. No early Stop is mistaken for a complete source baseline.
pub fn read(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    skip: usize,
    keep: usize,
) -> Result<ReadPass> {
    read_inner(adapter, r, previous, skip, keep, false)
}

/// Full session output: on a verified append retain only the checkpoint's stable old prefix,
/// replaying its suffix; on declined/invalid reuse collect the whole session in the same pass.
pub fn read_session(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    previous_count: usize,
) -> Result<ReadPass> {
    read_inner(adapter, r, previous, previous_count, usize::MAX, true)
}

fn read_inner(
    adapter: &dyn Adapter,
    r: &SessionRef,
    previous: Option<&ReadState>,
    mut skip: usize,
    keep: usize,
    session_output: bool,
) -> Result<ReadPass> {
    let captured = source::Probe::default().snapshot(r);
    let cached = previous
        .filter(|state| !session_output && state.fresh_with(adapter, r))
        .and_then(|state| state.completed.as_ref());
    let stop_after_selection = cached.is_some_and(|complete| skip.saturating_add(keep) < complete.total);
    let mut primary_reads = source::ReadSet::default();
    primary_reads.record(&r.path);
    let metadata_guard = primary_reads;
    let from = previous.and_then(|state| state.from(adapter, r, &captured, skip));
    if session_output {
        skip = from.and_then(stream::Checkpoint::messages).unwrap_or(0);
    }
    // Claude's buffered full-fidelity reader already checkpoints. Codex's native checkpoint
    // path needs spans; its full-extra equivalence is exercised separately after materialization.
    let opts = if matches!(r.harness, Harness::Codex) {
        stream::ParseOptions::lazy_extra()
    } else {
        stream::ParseOptions::full()
    };
    let mut sink = Tail {
        total: from.and_then(stream::Checkpoint::messages).unwrap_or(0),
        skip,
        keep,
        messages: Vec::new(),
        before: from.cloned(),
        checkpoint_disabled: false,
        last: from.cloned(),
        stop_after_selection,
    };
    let mut resumed = from.is_some();
    let done = match stream::stream_resumable(adapter, r, &opts, &mut sink, CHECKPOINT_EVERY, from)? {
        Some(done) => done,
        None => {
            resumed = false;
            sink = Tail {
                total: 0,
                skip: if session_output { 0 } else { skip },
                keep,
                messages: Vec::new(),
                before: None,
                checkpoint_disabled: false,
                last: None,
                stop_after_selection,
            };
            stream::stream_resumable(adapter, r, &opts, &mut sink, CHECKPOINT_EVERY, None)?
                .context("adapter declined a whole read")?
        }
    };
    let complete = done.counted.is_some();
    let mut reads = if let Some(counted) = done.counted {
        counted.dependencies
    } else {
        anyhow::ensure!(stop_after_selection, "session read stopped before EOF");
        previous
            .and_then(|state| state.source.reads())
            .context("incomplete cached source read set")?
    };
    if resumed {
        if let Some(prefix_reads) = previous.and_then(|state| state.source.reads()) {
            reads.merge(&prefix_reads);
        }
    }
    let resolver = done.session.resolver();
    for message in &mut sink.messages {
        for block in &message.content {
            if let crate::Block::ToolUse {
                input: crate::Json::Source(range),
                ..
            } = block
            {
                if range.path() != r.path {
                    reads.record(range.path());
                }
            }
            let span = match block {
                crate::Block::Text { text } | crate::Block::Thinking { text, .. } => text.as_span(),
                crate::Block::ToolResult { content, .. } => content.as_span(),
                _ => None,
            };
            if let Some(path) = span
                .and_then(|span| span.source.as_ref())
                .filter(|path| *path != &r.path)
            {
                reads.record(path);
            }
        }
        message.materialize(&resolver)?;
    }
    let source = captured.with_reads(&reads);
    let version = crate::resume::version(adapter, CONSUMER);
    let anchor = |cp: &stream::Checkpoint| {
        if !fits(cp, ANCHOR_BYTES) {
            return None;
        }
        Some(Anchor {
            head: crate::resume::MarkHead::of(&r.path, cp, version.as_deref()?)?,
            checkpoint: cp.clone(),
        })
    };
    let before = sink.before.as_ref().and_then(anchor);
    let last = done.last.as_ref().or(sink.last.as_ref()).and_then(anchor);
    anyhow::ensure!(
        if source.dependencies.is_some() {
            source::unchanged(r, &source)
        } else {
            source::Probe::default().snapshot(r).primary == source.primary
        },
        "session source changed during read: {}",
        r.path.display()
    );
    anyhow::ensure!(
        reads.fresh(&mut source::Probe::default()),
        "session dependencies changed during read: {}",
        r.path.display()
    );
    anyhow::ensure!(
        metadata_guard.fresh(&mut source::Probe::default()),
        "session identity changed during read: {}",
        r.path.display()
    );
    let first = if resumed {
        from.and_then(stream::Checkpoint::messages).unwrap_or(0)
    } else {
        0
    };
    let (session, total, completed) = if complete {
        let cached = Completed {
            session: done.session.clone(),
            total: sink.total,
        };
        let completed = fits(&cached, METADATA_BYTES).then_some(cached);
        (done.session, sink.total, completed)
    } else {
        let cached = cached.context("missing complete EOF metadata")?;
        (cached.session.clone(), cached.total, Some(cached.clone()))
    };
    Ok(ReadPass {
        version: state_version(adapter),
        session,
        total,
        parsed_messages: sink.total - first,
        suffix_bytes: (captured.primary.1.max(0) as u64).saturating_sub(if resumed {
            from.map_or(0, |cp| cp.offset)
        } else {
            0
        }),
        messages: sink.messages,
        resumed,
        prefix: first,
        source,
        path: r.path.clone(),
        before,
        last,
        completed,
        identity: metadata_guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness;

    #[test]
    fn failed_materialization_never_establishes_a_completed_read() {
        struct RemovesInput(PathBuf);
        impl Adapter for RemovesInput {
            fn harness(&self) -> Harness {
                Harness::Codex
            }
            fn roots(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> Result<Vec<SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                r: &SessionRef,
                _: &stream::ParseOptions,
                sink: &mut dyn stream::MessageSink,
            ) -> Result<Session> {
                let mut message = Message::new(crate::Role::Assistant);
                message.content.push(crate::Block::ToolUse {
                    id: "call".into(),
                    name: "opaque".into(),
                    namespace: None,
                    input: crate::Json::source(&self.0, 0, 2)?,
                });
                sink.try_message(message)?;
                std::fs::remove_file(&self.0)?;
                Ok(Session::new(&r.id, r.harness))
            }
        }
        let dir = std::env::temp_dir().join(format!("cv-watch-read-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("primary.jsonl");
        let external = dir.join("input.json");
        std::fs::write(&path, "primary").unwrap();
        std::fs::write(&external, "{}").unwrap();
        let r = SessionRef::new("read", Harness::Codex, path);
        assert!(read_session(&RemovesInput(external), &r, None, 0).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn followed_full_messages_and_eof_metadata_equal_the_original_parse() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut checked = 0;
        for (harness, directory) in [(Harness::Claude, "claude"), (Harness::Codex, "codex")] {
            let adapter = harness::for_harness(harness).unwrap();
            for path in std::fs::read_dir(fixtures.join(directory)).unwrap() {
                let path = path.unwrap().path();
                if !path.is_file() || !path.extension().is_some_and(|ext| ext == "jsonl" || ext == "json") {
                    continue;
                }
                let r = SessionRef::new("fidelity", harness, &path);
                let Ok(expected) = adapter.parse(&r) else { continue };
                let mut pass = read(adapter.as_ref(), &r, None, 0, usize::MAX).unwrap();
                pass.session.messages = std::mem::take(&mut pass.messages);
                assert_eq!(
                    serde_json::to_value(pass.session).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{}",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 20, "only {checked} fidelity fixtures");
        let scratch = std::env::temp_dir()
            .join(format!("cv-watch-fidelity-{}", uuid::Uuid::new_v4()))
            .join("chats");
        std::fs::create_dir_all(&scratch).unwrap();
        for (harness, rel) in [
            (Harness::Gemini, "gemini/session_0_46_session_context.jsonl"),
            (Harness::Gemini, "gemini/checkpoint_legacy_array.json"),
            (Harness::OpenClaw, "openclaw/v3-full.jsonl"),
            (Harness::OpenClaw, "openclaw/acp-session.jsonl"),
            (Harness::LmStudio, "lmstudio/sample.conversation.json"),
            (Harness::LmStudio, "lmstudio/harmony.conversation.json"),
            (Harness::Cline, "cline/api_conversation_history.json"),
            (Harness::Roo, "roo/api_conversation_history.json"),
        ] {
            let r = if harness == Harness::Gemini {
                let path = scratch.join(if rel.ends_with(".json") {
                    "checkpoint-legacy-array.json"
                } else {
                    "session.jsonl"
                });
                let text = std::fs::read_to_string(fixtures.join(rel)).unwrap();
                std::fs::write(&path, &text).unwrap();
                let id = crate::harness::gemini::parse_all_str(&text, Some(path.clone()))[0]
                    .id
                    .clone();
                SessionRef::new(id, harness, path)
            } else {
                SessionRef::new("fidelity", harness, fixtures.join(rel))
            };
            let adapter = harness::for_harness(harness).unwrap();
            let expected = adapter.parse(&r).unwrap();
            let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
            pass.session.messages = pass.messages;
            assert_eq!(
                serde_json::to_value(pass.session).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{rel}"
            );
        }
        std::fs::remove_dir_all(scratch.parent().unwrap()).unwrap();
    }

    #[test]
    fn append_resume_and_pending_replay_match_the_full_parse_with_less_work() {
        let dir = std::env::temp_dir().join(format!("cv-watch-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("read.jsonl");
        let mut body = String::new();
        for i in 0..8192 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let r = SessionRef::new("read", Harness::Claude, &path);
        let adapter = harness::for_harness(r.harness).unwrap();
        let first = read(adapter.as_ref(), &r, None, 0, 0).unwrap();
        assert_eq!(first.total, 8192);
        let offered = read(adapter.as_ref(), &r, None, 0, 1).unwrap();
        let pending_state = offered.accepted(1);
        let mut old_version = pending_state.clone();
        old_version.version.push_str("/old");
        assert_eq!(
            read(adapter.as_ref(), &r, Some(&old_version), 1, 1)
                .unwrap()
                .parsed_messages,
            8192
        );
        let pending = read(adapter.as_ref(), &r, Some(&pending_state), 1, 1).unwrap();
        assert_eq!(pending.total, 8192);
        assert_eq!(pending.parsed_messages, 2);
        assert_eq!(
            serde_json::to_value(&pending.session).unwrap(),
            serde_json::to_value(&offered.session).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&pending.messages).unwrap(),
            serde_json::to_value(&adapter.parse(&r).unwrap().messages[1..2]).unwrap()
        );
        let state = first.accepted(first.total);
        for i in 8192..8196 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("new {i}")}}).to_string());
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let next = read(adapter.as_ref(), &r, Some(&state), 8192, 1).unwrap();
        assert!(next.resumed);
        assert!(
            next.parsed_messages < 1024,
            "replayed {} prefix messages",
            next.parsed_messages
        );
        let expected = adapter.parse(&r).unwrap();
        assert_eq!(
            serde_json::to_value(&next.messages).unwrap(),
            serde_json::to_value(&expected.messages[8192..8193]).unwrap()
        );
        let state = next.accepted(8193);
        let bounded = read(adapter.as_ref(), &r, Some(&state), 8193, 1).unwrap();
        assert!(bounded.parsed_messages < next.parsed_messages);
        assert_eq!(bounded.total, expected.messages.len());
        assert_eq!(
            serde_json::to_value(&bounded.messages).unwrap(),
            serde_json::to_value(&expected.messages[8193..8194]).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&bounded.session).unwrap(),
            serde_json::to_value(&next.session).unwrap()
        );
        let rest = read(adapter.as_ref(), &r, Some(&state), 8193, usize::MAX).unwrap();
        assert!(rest.resumed);
        assert_eq!(
            serde_json::to_value(&rest.messages).unwrap(),
            serde_json::to_value(&expected.messages[8193..]).unwrap()
        );
        let mut got = rest.session;
        got.messages = expected.messages.clone();
        assert_eq!(
            serde_json::to_value(got).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // Preserve size+mtime and the sampled bytes, but rewrite an unsampled prefix record.
        // Its changed ctime must decline both unchanged EOF reuse and checkpoint admission.
        let complete = next.accepted(next.total);
        let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
        let middle = body.len() / 2;
        let changed = middle + body[middle..].find("xxx").unwrap();
        assert!(changed > 4096 && changed + 4096 < complete.anchor.as_ref().unwrap().checkpoint.offset as usize);
        body.replace_range(changed..changed + 1, "y");
        std::fs::write(&r.path, &body).unwrap();
        std::fs::File::open(&r.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(source::Probe::default().snapshot(&r).primary, complete.source.primary);
        let anchor = complete.anchor.as_ref().unwrap();
        assert!(anchor.head.holds(&r.path, body.len() as u64, &anchor.head.version));
        assert!(!complete.fresh_with(adapter.as_ref(), &r));
        let mut rewritten = read_session(adapter.as_ref(), &r, Some(&complete), next.total).unwrap();
        assert!(!rewritten.resumed);
        rewritten.session.messages = rewritten.messages;
        assert_eq!(
            serde_json::to_value(rewritten.session).unwrap(),
            serde_json::to_value(adapter.parse(&r).unwrap()).unwrap()
        );
        eprintln!(
            "watch prefix 8192: initial {}, append {}, pending {} messages interpreted",
            first.parsed_messages, next.parsed_messages, rest.parsed_messages
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn qwen_native_and_legacy_full_metadata_and_extras_are_exact() {
        let dir = std::env::temp_dir().join(format!("cv-watch-qwen-{}", uuid::Uuid::new_v4()));
        let chats = dir.join("projects/group/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let path = chats.join("read.jsonl");
        let r = SessionRef::new("s1", Harness::Qwen, &path);
        let native = [
            serde_json::json!({"uuid":"u1","sessionId":"s1","cwd":"/scratch","type":"user","timestamp":"2026-10-11T00:00:01Z","extra_unknown":{"a":3},"message":{"role":"user","parts":[{"text":"hello"}]}}),
            serde_json::json!({"uuid":"a1","sessionId":"s1","type":"assistant","model":"test","message":{"role":"model","parts":[{"text":"hm","thought":true},{"functionCall":{"id":"c1","name":"Read","args":{"file_path":"/scratch/a","nested":{"a":4}}}}]},"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5}}),
            serde_json::json!({"uuid":"r1","sessionId":"s1","type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"c1","name":"Read","response":{"output":"ok"}}}]},"toolCallResult":{"callId":"c1","status":"success"}}),
        ];
        std::fs::write(&path, native.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
        let adapter = harness::for_harness(Harness::Qwen).unwrap();
        let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
        pass.session.messages = pass.messages;
        assert_eq!(
            serde_json::to_value(pass.session).unwrap(),
            serde_json::to_value(adapter.parse(&r).unwrap()).unwrap()
        );
        for name in ["session_modern.jsonl", "checkpoint.json"] {
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/gemini")
                .join(name);
            let out = chats.join(if name == "checkpoint.json" {
                "checkpoint-test.json"
            } else {
                name
            });
            std::fs::copy(fixture, &out).unwrap();
            let r = SessionRef::new(
                if name == "checkpoint.json" {
                    "checkpoint-test"
                } else {
                    "legacy"
                },
                Harness::Qwen,
                out,
            );
            let mut pass = read_session(adapter.as_ref(), &r, None, 0).unwrap();
            pass.session.messages = pass.messages;
            assert_eq!(
                serde_json::to_value(pass.session).unwrap(),
                serde_json::to_value(adapter.parse(&r).unwrap()).unwrap(),
                "{name}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
