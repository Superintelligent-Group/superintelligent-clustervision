//! Live session following — the one loop behind `cv scry`, `cv watch`, and the MCP `await_omen`
//! and `observe_stream` tools (the last one across processes, through a [`Cursor`]).
//!
//! It polls discovery on an interval, diffs each session's message tail against what it saw last,
//! and emits [`SessionEvent`]s for new sessions and newly-appended messages. Polling (rather than
//! inotify) keeps it uniform across all harness layouts (single file, dir-of-files, sqlite).

use crate::harness;
use crate::ir::{Harness, Message, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

mod read;
pub use read::{read as read_tail, read_session, read_session_into, ReadPass, ReadState};

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A session we'd never seen before appeared.
    New,
    /// An existing session gained messages.
    Updated,
}

/// A live event about a session.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub kind: EventKind,
    pub reference: SessionRef,
    /// For `New`: the whole conversation. For `Updated`: only the newly-appended messages.
    pub new_messages: Vec<Message>,
}

/// Narrow what to follow.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub harness: Option<Harness>,
    pub cwd_contains: Option<String>,
}

impl Filter {
    /// Whether a session passes this filter (harness + cwd-substring). Public so non-`Watcher`
    /// consumers (e.g. the MCP `observe_stream` tool) can reuse the exact same filtering semantics
    /// rather than re-deriving them and drifting.
    pub fn matches(&self, r: &SessionRef) -> bool {
        if let Some(h) = self.harness {
            if r.harness != h {
                return false;
            }
        }
        if let Some(needle) = &self.cwd_contains {
            let hit = r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(needle))
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        true
    }
}

/// Where a follower stands in one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// This many parsed IR messages have already been seen.
    Messages(usize),
    /// Not parsed yet: the transcript was this many bytes long when the follower started. An
    /// append-only JSONL transcript's first `n` bytes are exactly what a parse at that moment would
    /// have read, so [`resolve`] recovers the same message count later — when, and only if, the
    /// session changes.
    Bytes(u64),
}

/// Transcripts that only ever grow by appending lines, so a byte length pins a past state: Claude
/// and plain Codex `.jsonl`, plus Qwen's chronological `projects/...` ChatRecords. Compressed
/// Codex and Qwen's legacy Gemini replay are excluded: their byte prefix is not a logical prefix.
fn append_only(r: &SessionRef) -> bool {
    (matches!(r.harness, Harness::Claude | Harness::Codex) && r.path.extension().is_some_and(|e| e == "jsonl"))
        || (r.harness == Harness::Qwen && harness::qwen::is_session_jsonl(&r.path))
}

/// Record where `r` stands now, as cheaply as possible: one `stat` for an append-only transcript,
/// a streamed count otherwise. `None` when the session can't be read.
///
/// Following "from now on" used to parse every matching session up front just to learn its
/// message count — tens of seconds and ~1 GB transient on a large corpus, almost all of it for
/// sessions that never change while followed. A byte mark defers that parse to [`resolve`].
pub fn baseline(r: &SessionRef) -> Option<Mark> {
    if append_only(r) {
        if let Ok(md) = std::fs::metadata(&r.path) {
            return Some(Mark::Bytes(md.len()));
        }
    }
    count(r).map(Mark::Messages)
}

/// The message count a full parse of `r` returns, without holding the parsed session.
///
/// It is the parse's own count by construction: the session is streamed through the adapter and
/// nothing is kept. A count taken any other way (a row count, a hand-written survival query) is a
/// second definition of "which records become messages" that has to agree with the adapter's
/// mapping forever, and a mark that disagrees with the parse makes a follower skip or repeat.
fn count(r: &SessionRef) -> Option<usize> {
    tail(r, usize::MAX, 0).map(|(total, _)| total)
}

/// What a full parse of `r` returns past the first `skip` messages: `(total, messages[skip..])`,
/// keeping at most `keep` of them. Every adapter's `parse` is its `stream` into a collecting sink
/// ([`crate::stream::collect`]), so streaming and keeping only the tail gives the same messages
/// while holding O(tail) instead of the whole transcript — what a follower needs, since it only
/// reports what was appended.
pub fn tail(r: &SessionRef, skip: usize, keep: usize) -> Option<(usize, Vec<Message>)> {
    let adapter = harness::for_harness(r.harness)?;
    let mut total = 0usize;
    let mut kept = Vec::new();
    let mut sink = |m: Message| {
        if total >= skip && kept.len() < keep {
            kept.push(m);
        }
        total += 1;
        crate::stream::Flow::Continue
    };
    let session = adapter
        .try_stream(r, &crate::stream::ParseOptions::full(), &mut sink)
        .ok()?;
    // As `stream::collect` does: own any lazy content (full-fidelity streams rarely produce any).
    let resolver = session.resolver();
    for m in &mut kept {
        m.materialize(&resolver).ok()?;
    }
    Some((total, kept))
}

/// The parsed message count `mark` stands for. `None` when a byte mark's transcript can't be read.
pub fn resolve(r: &SessionRef, mark: Mark) -> Option<usize> {
    match mark {
        Mark::Messages(n) => Some(n),
        Mark::Bytes(len) => match r.harness {
            Harness::Claude => harness::claude::count_prefix(r, len).ok(),
            Harness::Codex => harness::codex::count_prefix(r, len).ok(),
            Harness::Qwen => harness::qwen::count_prefix(r, len).ok(),
            _ => None,
        },
    }
}

/// Where a follower stands in one session.
#[derive(Debug, Clone)]
struct State {
    /// Cheap change signal from discovery (no full parse needed to detect change).
    trigger: (usize, Option<i64>),
    /// How far we've already reported.
    mark: Mark,
    /// Messages past `mark` were left unread when a bounded poll's budget ran out: the next poll
    /// reads this session again even if its trigger has not moved, or the rest would wait for the
    /// session's next append.
    pending: bool,
    read: Option<ReadState>,
}

/// One session's position, as the cursor carries it. The field names are the wire: `t` the
/// trigger, `n` the parsed count, `b` a byte baseline not yet resolved (see [`Mark::Bytes`]),
/// `more` the pending flag. Optional fields keep a cursor issued before they existed decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pos {
    t: (usize, Option<i64>),
    n: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    b: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r: Option<ReadState>,
}

impl From<State> for Pos {
    fn from(s: State) -> Pos {
        let (n, b) = match s.mark {
            Mark::Messages(n) => (n, None),
            Mark::Bytes(len) => (0, Some(len)),
        };
        Pos {
            t: s.trigger,
            n,
            b,
            more: s.pending,
            r: s.read.filter(|read| read.fits_cache(64 * 1024)),
        }
    }
}

impl From<Pos> for State {
    fn from(p: Pos) -> State {
        State {
            trigger: p.t,
            mark: match p.b {
                Some(len) => Mark::Bytes(len),
                None => Mark::Messages(p.n),
            },
            pending: p.more,
            read: p.r,
        }
    }
}

/// A [`Watcher`]'s whole position, serializable: the `seen` map as a compact JSON string, so a
/// stateless caller (the MCP `observe_stream`, one `tools/call` at a time) hands it back on its
/// next call and resumes exactly where an in-memory watcher would have. Versioned: a cursor from
/// another schema decodes to `None`, and the caller starts a fresh baseline — an unusable cursor
/// is NOT "no prior positions" (that would make every session look new and dump its history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    v: u32,
    o: HashMap<String, Pos>,
}

const CURSOR_VERSION: u32 = 1;

impl Cursor {
    /// The compact string a caller carries between polls.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a cursor serializes")
    }

    /// `None` when `s` is not a cursor this build issued (unparseable, or another version).
    pub fn decode(s: &str) -> Option<Cursor> {
        serde_json::from_str::<Cursor>(s).ok().filter(|c| c.v == CURSOR_VERSION)
    }

    /// How many sessions the cursor positions.
    pub fn len(&self) -> usize {
        self.o.len()
    }

    pub fn is_empty(&self) -> bool {
        self.o.is_empty()
    }
}

/// A stateful poller. Call [`poll`](Watcher::poll) repeatedly, or [`run`](Watcher::run) to loop;
/// [`to_cursor`](Watcher::to_cursor) / [`from_cursor`](Watcher::from_cursor) carry the state
/// across processes. Every poller in cv (`cv scry`, `cv watch`, MCP `await_omen` and
/// `observe_stream`) is this one loop: discover → cheap trigger → resolve the mark → stream the
/// tail — so every one of them compares against a parsed count, never a row count.
pub struct Watcher {
    filter: Filter,
    seen: HashMap<String, State>,
    /// Where sessions come from: [`crate::discover_all`] (stat the fleet, the default) or
    /// [`crate::sessions`] (the catalog fast path, for a poller that runs every few seconds).
    discover: fn() -> Vec<SessionRef>,
}

impl Watcher {
    /// Create a watcher. If `emit_existing` is false, the current sessions are recorded silently so
    /// only activity *after* construction is reported (the usual `tail -f` feel).
    pub fn new(filter: Filter, emit_existing: bool) -> Self {
        Self::with_discovery(filter, emit_existing, crate::discover_all)
    }

    /// [`Watcher::new`] over another session source (see [`Watcher::discover`]).
    pub fn with_discovery(filter: Filter, emit_existing: bool, discover: fn() -> Vec<SessionRef>) -> Self {
        let mut w = Watcher {
            filter,
            seen: HashMap::new(),
            discover,
        };
        if !emit_existing {
            w.prime();
        }
        w
    }

    /// Resume from a [`Cursor`]: the watcher stands exactly where the one that issued it stood.
    pub fn from_cursor(filter: Filter, cursor: Cursor, discover: fn() -> Vec<SessionRef>) -> Self {
        Watcher {
            filter,
            seen: cursor.o.into_iter().map(|(k, p)| (k, State::from(p))).collect(),
            discover,
        }
    }

    /// Where this watcher stands, to hand to [`Watcher::from_cursor`] later.
    pub fn to_cursor(&self) -> Cursor {
        // Keep the existing positions for every session, with at most eight <=64 KiB optional
        // reader states. Omitted/older states replay once, preserving output and wire version.
        let mut readers = 0;
        Cursor {
            v: CURSOR_VERSION,
            o: self
                .seen
                .iter()
                .map(|(k, s)| {
                    let mut pos = Pos::from(s.clone());
                    if pos.r.is_some() {
                        readers += 1;
                        if readers > 8 {
                            pos.r = None;
                        }
                    }
                    (k.clone(), pos)
                })
                .collect(),
        }
    }

    fn key(r: &SessionRef) -> String {
        format!("{}:{}", r.harness.as_str(), r.id)
    }

    fn trigger_of(r: &SessionRef) -> (usize, Option<i64>) {
        (r.message_count, r.updated_at.map(|t| t.timestamp_millis()))
    }

    /// Record current state without emitting anything.
    fn prime(&mut self) {
        for r in self.discover() {
            // The cheap discover `message_count` is NOT the parsed IR length for several harnesses
            // (codex/hermes/claude add reasoning/tool/system turns), so seeding from it makes the
            // first `Updated` poll re-emit messages that predate the watcher. Record the true
            // baseline instead — a byte mark where the transcript is append-only (see [`baseline`]).
            let mark = baseline(&r).unwrap_or(Mark::Messages(r.message_count));
            self.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    mark,
                    pending: false,
                    read: Some(ReadState::baseline(&r)),
                },
            );
        }
    }

    /// The sessions to follow, oldest-touched first, so a bounded poll yields a chronological
    /// slice. Followers use full discovery independently of catalog synchronization. The
    /// identity-aware per-file scan cache avoids reparsing unchanged transcripts.
    fn discover(&self) -> Vec<SessionRef> {
        let mut refs: Vec<SessionRef> = (self.discover)()
            .into_iter()
            .filter(|r| self.filter.matches(r))
            .collect();
        refs.sort_by_key(|r| r.updated_at.or(r.created_at));
        refs
    }

    // Reconcile identities, not just fleet size: an equally sized replacement can remove
    // a session too. Reuse these keys during polling and allocate the live set only when
    // an existing position actually disappeared.
    fn discover_live(&mut self) -> Vec<(String, SessionRef)> {
        let refs: Vec<_> = self.discover().into_iter().map(|r| (Self::key(&r), r)).collect();
        let existing = refs.iter().filter(|(key, _)| self.seen.contains_key(key)).count();
        if existing < self.seen.len() {
            let live: std::collections::HashSet<&str> = refs.iter().map(|(key, _)| key.as_str()).collect();
            self.seen.retain(|key, _| live.contains(key.as_str()));
        }
        refs
    }

    /// Poll once and return any events since the previous poll.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.poll_bounded(usize::MAX, |ev| {
            let n = ev.new_messages.len();
            events.push(ev);
            n
        });
        events
    }

    /// Poll once, offering each session's newly appended messages to `take` — at most
    /// `max_messages` across the call — which returns how many of them it consumed. What it
    /// leaves (and every message past the budget) stays pending: the next poll re-reads that
    /// session even if its cheap trigger has not moved. Returns whether anything is pending.
    pub fn poll_bounded(&mut self, max_messages: usize, mut take: impl FnMut(SessionEvent) -> usize) -> bool {
        self.poll_bounded_inner(max_messages, &mut take, false, false)
            .unwrap_or(false)
    }

    /// A demanded bounded read: source/materialization failures are errors, so callers cannot
    /// return a successful cursor for output that could not be read. The legacy polling API
    /// remains best effort and leaves unreadable sessions retryable.
    pub fn try_poll_bounded(
        &mut self,
        max_messages: usize,
        mut take: impl FnMut(SessionEvent) -> usize,
    ) -> anyhow::Result<bool> {
        self.poll_bounded_inner(max_messages, &mut take, true, false)
    }

    /// Checked polling retaining source-backed message payloads. The callback must resolve
    /// demanded content through checked APIs; source changes before acceptance fail the poll.
    pub fn try_poll_bounded_retained(
        &mut self,
        max_messages: usize,
        mut take: impl FnMut(SessionEvent) -> usize,
    ) -> anyhow::Result<bool> {
        self.poll_bounded_inner(max_messages, &mut take, true, true)
    }

    fn poll_bounded_inner(
        &mut self,
        max_messages: usize,
        take: &mut impl FnMut(SessionEvent) -> usize,
        strict: bool,
        retained: bool,
    ) -> anyhow::Result<bool> {
        let _reads = crate::source::read_scope();
        let refs = self.discover_live();
        let mut consumed = 0usize;
        let mut pending = false;
        for (key, r) in refs {
            let trigger = Self::trigger_of(&r);
            let (kind, prev_len) = match self.seen.get(&key) {
                None => (EventKind::New, 0),
                Some(state)
                    if state.trigger != trigger
                        || state.pending
                        || state
                            .read
                            .as_ref()
                            .map_or(matches!(state.mark, Mark::Messages(_)), |read| {
                                !read.baseline_unchanged(&r)
                            }) =>
                {
                    let Some(prev_len) = resolve(&r, state.mark) else {
                        if strict {
                            anyhow::bail!(
                                "could not resolve prior watch position: {} {}",
                                r.harness,
                                r.path.display()
                            );
                        }
                        continue; // unreadable right now: keep the position, a transient failure is not a skip
                    };
                    (EventKind::Updated, prev_len)
                }
                Some(_) => continue,
            };
            let room = max_messages.saturating_sub(consumed);
            let Some(adapter) = harness::for_harness(r.harness) else {
                continue;
            };
            let previous = self.seen.get(&key).and_then(|state| state.read.as_ref());
            let read = if retained { read::read_retained } else { read_tail };
            let mut pass = match read(adapter.as_ref(), &r, previous, prev_len, room) {
                Ok(pass) => pass,
                Err(error) if strict => {
                    return Err(error.context(format!("reading watched {} session {}", r.harness, r.path.display())))
                }
                Err(_) => continue,
            };
            let total = pass.total;
            let new_messages = std::mem::take(&mut pass.messages);
            // A mark past the end of a rewritten transcript: nothing to re-emit (see the tests).
            let prev_len = prev_len.min(total);
            let offered = new_messages.len();
            let taken = if offered == 0 {
                0
            } else {
                take(SessionEvent {
                    kind,
                    reference: r.clone(),
                    new_messages,
                })
                .min(offered)
            };
            if retained {
                anyhow::ensure!(
                    pass.accepted(prev_len).fresh_with(adapter.as_ref(), &r),
                    "watched source changed during retained projection"
                );
            }
            consumed += taken;
            let seen_to = prev_len + taken;
            let left = total > seen_to;
            pending |= left;
            self.seen.insert(
                key,
                State {
                    trigger,
                    mark: Mark::Messages(seen_to),
                    pending: left,
                    read: Some(pass.accepted(seen_to)),
                },
            );
        }
        Ok(pending)
    }

    /// Observe sources on the same discovery/filter loop without first reading their message
    /// tails. The consumer validates both source and derived artifact freshness, performs its one required read and
    /// returns the completed count/baseline only after publication succeeds. Failure leaves the
    /// previous position untouched, so the next poll retries even without a new append.
    pub fn poll_sources(
        &mut self,
        mut take: impl FnMut(EventKind, &SessionRef, usize) -> Option<(usize, ReadState)>,
    ) -> usize {
        let _reads = crate::source::read_scope();
        let refs = self.discover_live();
        let mut accepted = 0;
        for (key, r) in refs {
            let trigger = Self::trigger_of(&r);
            let (kind, previous) = match self.seen.get(&key) {
                None => (EventKind::New, 0),
                Some(state) => {
                    let Some(previous) = resolve(&r, state.mark) else {
                        continue;
                    };
                    (EventKind::Updated, previous)
                }
            };
            let Some((total, read)) = take(kind, &r, previous) else {
                continue;
            };
            self.seen.insert(
                key,
                State {
                    trigger,
                    mark: Mark::Messages(total),
                    pending: false,
                    read: Some(read),
                },
            );
            accepted += 1;
        }
        accepted
    }

    /// Poll forever, invoking `on_event` for each event. Blocks the calling thread.
    pub fn run<F: FnMut(SessionEvent)>(&mut self, interval: Duration, mut on_event: F) -> ! {
        loop {
            for ev in self.poll() {
                on_event(ev);
            }
            std::thread::sleep(interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sref(harness: Harness, path: std::path::PathBuf) -> SessionRef {
        SessionRef {
            id: "baseline-test".into(),
            harness,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    #[test]
    fn same_size_replacement_prunes_positions_even_when_new_source_cannot_be_read() {
        fn replacement() -> Vec<SessionRef> {
            let mut r = sref(
                Harness::Claude,
                std::path::PathBuf::from("/nonexistent/cv-watch-replacement"),
            );
            r.id = "replacement".into();
            vec![r]
        }
        fn original() -> Vec<SessionRef> {
            let mut r = replacement().remove(0);
            r.id = "original".into();
            vec![r]
        }
        let cursor = Cursor {
            v: CURSOR_VERSION,
            o: [(
                "claude:original".into(),
                Pos {
                    t: (0, None),
                    n: 12,
                    b: None,
                    more: false,
                    r: None,
                },
            )]
            .into_iter()
            .collect(),
        };
        for source_poll in [false, true] {
            let mut watcher = Watcher::from_cursor(Filter::default(), cursor.clone(), replacement);
            if source_poll {
                assert_eq!(watcher.poll_sources(|_, _, _| None), 0);
            } else {
                assert!(watcher
                    .try_poll_bounded(1, |_| panic!("missing source emitted"))
                    .is_err());
            }
            let encoded = watcher.to_cursor().encode();
            let cursor = Cursor::decode(&encoded).unwrap();
            assert!(cursor.is_empty(), "vanished position survived: {encoded}");
            let mut resumed = Watcher::from_cursor(Filter::default(), cursor, original);
            resumed.poll_sources(|kind, _, previous| {
                assert_eq!(kind, EventKind::New);
                assert_eq!(previous, 0, "reappeared identity inherited its removed position");
                None
            });
        }
    }

    /// The codex shapes that make a prefix count subtle: `has_events` decided from the head, an
    /// `event_msg` twin that dedups its `response_item`, and an assistant message held back until a
    /// trailing `token_count` attaches usage.
    const CODEX: &str = concat!(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"x","cwd":"/work"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3}}}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
        "\n",
    );

    /// A byte baseline resolved after the file grew must equal what a full parse returned when the
    /// file was that long — at EVERY cut point, including mid-line (a record still being written).
    #[test]
    fn byte_baseline_resolves_to_the_count_a_parse_saw_then() {
        let dir = std::env::temp_dir().join(format!("cv-watch-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");
        let mut cases: Vec<(Harness, Vec<u8>)> = std::fs::read_dir(fixtures)
            .unwrap()
            .map(|e| (Harness::Claude, std::fs::read(e.unwrap().path()).unwrap()))
            .collect();
        cases.push((Harness::Codex, CODEX.as_bytes().to_vec()));

        for (h, full) in cases {
            let then = dir.join("then.jsonl");
            let now = dir.join("now.jsonl");
            std::fs::write(&now, &full).unwrap();
            let adapter = harness::for_harness(h).unwrap();
            let total = adapter.parse(&sref(h, now.clone())).unwrap().messages.len();
            assert!(total > 1, "{h} fixture must have several messages");
            for cut in 0..=full.len() {
                std::fs::write(&then, &full[..cut]).unwrap();
                let Ok(parsed) = adapter.parse(&sref(h, then.clone())) else {
                    continue; // the old baseline recorded nothing for an unparseable file either
                };
                assert_eq!(
                    resolve(&sref(h, now.clone()), Mark::Bytes(cut as u64)),
                    Some(parsed.messages.len()),
                    "{h} cut at byte {cut}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The baseline count and the appended tail are streamed instead of taken from a held parse;
    /// both must be exactly the parse's.
    #[test]
    fn streamed_count_and_tail_equal_the_parse() {
        let fx = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let mut checked = 0;
        for (h, rel) in [
            (Harness::Claude, "claude/rich_blocks.jsonl"),
            (Harness::Claude, "claude/sidechain_subagent.jsonl"),
            (Harness::Claude, "claude/compact_summary.jsonl"),
            (Harness::Claude, "claude/system_lines.jsonl"),
            (Harness::Gemini, "gemini/session_0_46_session_context.jsonl"),
            (Harness::Gemini, "gemini/checkpoint_legacy_array.json"),
            (Harness::OpenClaw, "openclaw/v3-full.jsonl"),
            (Harness::OpenClaw, "openclaw/acp-session.jsonl"),
            (Harness::LmStudio, "lmstudio/sample.conversation.json"),
            (Harness::LmStudio, "lmstudio/harmony.conversation.json"),
            (Harness::Cline, "cline/api_conversation_history.json"),
            (Harness::Roo, "roo/api_conversation_history.json"),
        ] {
            let r = sref(h, std::path::Path::new(fx).join(rel));
            let Ok(parsed) = harness::for_harness(h).unwrap().parse(&r) else {
                continue;
            };
            if !append_only(&r) {
                assert_eq!(baseline(&r), Some(Mark::Messages(parsed.messages.len())), "{h} {rel}");
            }
            // The streamed tail is the parse's tail, message for message, at every split.
            let n = parsed.messages.len();
            for skip in [0, 1, n / 2, n.saturating_sub(1), n, n + 3] {
                for keep in [0, 1, usize::MAX] {
                    let (total, got) = tail(&r, skip, keep).unwrap();
                    assert_eq!(total, n, "{h} {rel}");
                    let want: Vec<&Message> = parsed.messages.iter().skip(skip).take(keep).collect();
                    assert_eq!(
                        serde_json::to_value(&got).unwrap(),
                        serde_json::to_value(&want).unwrap(),
                        "{h} {rel} skip={skip} keep={keep}"
                    );
                }
            }
            checked += 1;
        }
        assert!(checked >= 8, "only {checked} fixtures parsed standalone");
    }

    /// A transcript REWRITTEN under a byte mark (truncated, or replaced by different content) is
    /// not an append, so no count can recover "what was seen then". What must hold is that the mark
    /// still resolves to something the new file can index: past the new end it resolves to the
    /// whole file (nothing re-emitted, nothing invented), and inside a replaced prefix it resolves
    /// to that prefix's own count. Neither Claude's `/compact` (it appends) nor `cv prune` (it
    /// writes a new session) rewrites in place; this pins the failure mode should anything else.
    #[test]
    fn a_byte_mark_on_a_rewritten_transcript_resolves_within_the_new_file() {
        let dir = std::env::temp_dir().join(format!("cv-watch-rewrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        for h in [Harness::Claude, Harness::Codex] {
            let full: Vec<u8> = if h == Harness::Codex {
                CODEX.as_bytes().to_vec()
            } else {
                std::fs::read(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/claude/rich_blocks.jsonl"
                ))
                .unwrap()
            };
            let r = sref(h, path.clone());
            let adapter = harness::for_harness(h).unwrap();
            let mark = Mark::Bytes(full.len() as u64);

            // Truncated to its first half (cut on a line boundary).
            let half = full[..full.len() / 2].iter().rposition(|b| *b == b'\n').unwrap() + 1;
            std::fs::write(&path, &full[..half]).unwrap();
            let now = adapter.parse(&r).unwrap().messages.len();
            assert_eq!(
                resolve(&r, mark),
                Some(now),
                "{h}: a mark past the end is the whole file"
            );
            let (total, fresh) = tail(&r, now, usize::MAX).unwrap();
            assert_eq!((total, fresh.len()), (now, 0), "{h}: nothing re-emitted");

            // Replaced: the old file's length now cuts through different records.
            let mut other = full[half..].to_vec();
            other.extend_from_slice(&full);
            std::fs::write(&path, &other).unwrap();
            let total = adapter.parse(&r).unwrap().messages.len();
            let seen = resolve(&r, mark).unwrap();
            std::fs::write(dir.join("prefix.jsonl"), &other[..full.len()]).unwrap();
            let prefix = adapter
                .parse(&sref(h, dir.join("prefix.jsonl")))
                .unwrap()
                .messages
                .len();
            assert_eq!(seen, prefix, "{h}: a mark inside a rewrite is that prefix's count");
            assert!(seen <= total, "{h}");
            assert_eq!(tail(&r, seen, usize::MAX).unwrap().1.len(), total - seen, "{h}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn baseline_is_a_byte_mark_only_for_append_only_transcripts() {
        let dir = std::env::temp_dir().join(format!("cv-watch-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("s.jsonl");
        std::fs::write(&jsonl, CODEX).unwrap();
        let len = CODEX.len() as u64;
        assert_eq!(baseline(&sref(Harness::Claude, jsonl.clone())), Some(Mark::Bytes(len)));
        assert_eq!(baseline(&sref(Harness::Codex, jsonl.clone())), Some(Mark::Bytes(len)));
        // A compressed rollout's byte prefix is not a transcript prefix: parse it instead.
        assert!(!append_only(&sref(Harness::Codex, dir.join("r.jsonl.zst"))));
        // Missing file: no mark, as before.
        assert_eq!(baseline(&sref(Harness::Gemini, dir.join("absent.json"))), None);
        assert_eq!(resolve(&sref(Harness::Claude, jsonl), Mark::Messages(3)), Some(3));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cursor is the watcher's state and nothing else: a bounded poll takes what the budget
    /// allows and leaves the rest pending; a watcher resumed from the encoded cursor drains exactly
    /// the remainder; an idle poll then yields nothing. The session source is pluggable so this
    /// runs over one temp transcript instead of the machine's fleet.
    #[test]
    fn bounded_poll_leaves_the_rest_pending_and_a_cursor_resumes_it() {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let dir = DIR
            .get_or_init(|| std::env::temp_dir().join(format!("cv-watch-cursor-{}", std::process::id())))
            .clone();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fn discover() -> Vec<SessionRef> {
            let path = DIR.get().unwrap().join("s.jsonl");
            let len = std::fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(0);
            let mut r = sref(Harness::Claude, path);
            r.id = "cursor-test".into();
            r.message_count = len; // the cheap trigger: the file grew
            vec![r]
        }
        let full = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/claude/rich_blocks.jsonl"
        ))
        .unwrap();
        let cut = full[..full.len() / 2].iter().rposition(|b| *b == b'\n').unwrap() + 1;
        std::fs::write(&path, &full[..cut]).unwrap();
        let adapter = harness::for_harness(Harness::Claude).unwrap();
        let r = discover().remove(0);
        let before = adapter.parse(&r).unwrap().messages.len();

        // Baseline: nothing emitted, one position recorded, as a byte mark (no parse).
        let w = Watcher::with_discovery(Filter::default(), false, discover);
        let cursor = w.to_cursor();
        assert_eq!(cursor.len(), 1);
        let encoded = cursor.encode();
        assert!(
            encoded.contains("\"b\":"),
            "a byte baseline rides the cursor: {encoded}"
        );
        assert!(
            Cursor::decode("{\"v\":99,\"o\":{}}").is_none(),
            "another schema is not a cursor"
        );
        assert!(Cursor::decode("garbage").is_none());

        // The transcript grows; a budget of one takes one message and leaves the rest pending.
        std::fs::write(&path, &full).unwrap();
        let after = adapter.parse(&r).unwrap().messages.len();
        assert!(
            after > before + 1,
            "fixture must grow by several messages ({before} → {after})"
        );
        let mut w = Watcher::from_cursor(Filter::default(), Cursor::decode(&encoded).unwrap(), discover);
        let mut got = Vec::new();
        let pending = w.poll_bounded(3, |mut ev| {
            assert_eq!(ev.kind, EventKind::Updated);
            assert!(ev.new_messages.len() >= 2);
            got.push(ev.new_messages.remove(0));
            1
        });
        assert!(pending);
        assert_eq!(got.len(), 1);
        let encoded = w.to_cursor().encode();
        assert!(encoded.contains("\"more\":true"), "{encoded}");

        // Resumed from that cursor with no budget: exactly the remainder, nothing twice.
        let mut w = Watcher::from_cursor(Filter::default(), Cursor::decode(&encoded).unwrap(), discover);
        let events = w.poll();
        assert_eq!(events.len(), 1);
        got.extend(events.into_iter().next().unwrap().new_messages);
        assert_eq!(got.len(), after - before, "the appended tail, once");
        let want = adapter.parse(&r).unwrap().messages;
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            serde_json::to_value(&want[before..]).unwrap()
        );
        assert!(w.poll().is_empty(), "idle: nothing pending, trigger unchanged");
        assert!(
            !w.to_cursor().encode().contains("more"),
            "nothing pending rides the cursor"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn optional_resume_state_is_bounded_and_old_cursors_keep_positions() {
        let mut watcher = Watcher::with_discovery(Filter::default(), true, Vec::new);
        let r = sref(
            Harness::Claude,
            std::path::PathBuf::from("/absent/cv-cursor-test.jsonl"),
        );
        for i in 0..32 {
            watcher.seen.insert(
                format!("session-{i}"),
                State {
                    trigger: (i, None),
                    mark: Mark::Messages(i),
                    pending: true,
                    read: Some(ReadState::baseline(&r)),
                },
            );
        }
        let cursor = watcher.to_cursor();
        assert_eq!(cursor.len(), 32);
        assert_eq!(cursor.o.values().filter(|pos| pos.r.is_some()).count(), 8);
        assert!(cursor.encode().len() < 512 * 1024 + 4096);
        let old = Cursor::decode(r#"{"v":1,"o":{"old":{"t":[8,null],"n":3,"more":true}}}"#).unwrap();
        assert_eq!(old.o["old"].n, 3);
        assert!(old.o["old"].more);
        assert!(old.o["old"].r.is_none());
    }

    #[test]
    fn source_observer_retries_failure_without_advancing_positions() {
        fn discover() -> Vec<SessionRef> {
            vec![sref(
                Harness::Claude,
                std::path::PathBuf::from("/absent/cv-observer-test.jsonl"),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        for _ in 0..2 {
            assert_eq!(
                watcher.poll_sources(|kind, _, previous| {
                    assert_eq!(kind, EventKind::New);
                    assert_eq!(previous, 0);
                    None
                }),
                0
            );
            assert!(watcher.to_cursor().is_empty());
        }
        assert_eq!(
            watcher.poll_sources(|kind, r, previous| {
                assert_eq!(kind, EventKind::New);
                assert_eq!(previous, 0);
                Some((4, ReadState::baseline(r)))
            }),
            1
        );
        assert_eq!(
            watcher.poll_sources(|kind, r, previous| {
                assert_eq!(kind, EventKind::Updated);
                assert_eq!(previous, 4);
                Some((4, ReadState::baseline(r)))
            }),
            1
        );
    }

    #[test]
    fn demanded_poll_surfaces_read_failure_without_a_success_position() {
        fn discover() -> Vec<SessionRef> {
            vec![sref(
                Harness::Claude,
                std::path::PathBuf::from("/absent/cv-failed-watch-read.jsonl"),
            )]
        }
        let mut watcher = Watcher::with_discovery(Filter::default(), true, discover);
        assert!(watcher
            .try_poll_bounded(1, |_| panic!("failed source emitted a message"))
            .is_err());
        assert!(watcher.to_cursor().is_empty());
        assert!(!watcher.poll_bounded(1, |_| panic!("failed source emitted a message")));
        assert!(watcher.to_cursor().is_empty());
    }
}
