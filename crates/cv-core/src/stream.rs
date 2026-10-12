//! Streaming parse: the message stream — not the whole [`Session`] — is the fundamental unit.
//!
//! [`Adapter::parse`](crate::harness::Adapter::parse) materializes an entire transcript into one
//! owned `Session { messages: Vec<Message> }`. That sets the memory floor for everything: a 1 GB
//! transcript becomes several GB of `Vec<Message>` + owned `String`s, even when the consumer only
//! makes a single forward pass (index, export, dataset, live-search).
//!
//! [`Adapter::stream`](crate::harness::Adapter::stream) instead hands each [`Message`] to a
//! [`MessageSink`] and drops it before reading the next, so peak memory is O(largest message). The
//! whole-`Session` `parse` is recovered for free as "stream into a [`CollectSink`]" (see
//! [`collect`]). Adapters override **one** of the two: a native `stream` (memory-savings) *or* the
//! existing `parse` (and inherit a `stream` that bridges through it). No mutual recursion, so
//! migration is incremental.

use crate::harness::Adapter;
use crate::ir::{Message, Session, SessionRef};
use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Knobs controlling how much of each message an adapter materializes. Everything off (the
/// [`Default`]) is the cheapest faithful pass; full-fidelity callers opt back in.
///
/// # Which constructor? (the decision rule)
///
/// - [`full()`](ParseOptions::full) — you will **re-emit or round-trip** the session (convert,
///   port, `--json`, splice/loom, redact-to-file). Everything materialized, inline. Most
///   expensive; wrong for anything that only *reads*.
/// - [`bulk()`](ParseOptions::bulk) — you read **all** the text once, forward-only, and never
///   touch `extra` (live search over small/whole-doc harnesses, dataset-style flattening of
///   sessions you'll fully consume). Inline content, no sidecars.
/// - [`lazy()`](ParseOptions::lazy) — you skip, partially read, or consume fields in bounded
///   chunks: windowed/`--range` reads, metadata/event extraction, snippet scans, head-only
///   embedding, chunk-wise indexing. Supported large fields arrive as [`Span`](crate::lazy::Span)s
///   you resolve (possibly capped via [`Resolver::resolve_prefix`](crate::lazy::Resolver::
///   resolve_prefix) or chunked via `for_each_chunk`) — a 700 MB record costs 16 bytes until you
///   choose otherwise. Adapters without a span path just hand you inline content, so `lazy()` is
///   never *less* correct than `bulk()`, only cheaper when spans exist.
///
/// Rule of thumb: emit ⇒ `full`, read-everything ⇒ `bulk`, read-some/bounded ⇒ `lazy`.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// Populate the harness-specific `extra` maps — most importantly Claude's `toolUseResult`
    /// sidecar (structured patches, file contents, command stdout), which is `clone`d per tool
    /// turn and can dwarf the visible transcript. Bulk text consumers (index/search/dataset) never
    /// read it; only full-fidelity paths (json export, cross-harness convert/port, round-trips) do.
    pub extra: bool,
    /// Emit large content as lazy [`Span`](crate::lazy::Span)s into the source instead of owned
    /// strings. Partial readers and chunk-wise reducers can avoid retaining complete decoded
    /// fields. Resolving every span into an owned string loses that benefit. Adapters without
    /// span support ignore this flag; adapters may still allocate during record interpretation.
    pub spans: bool,
    /// Stamp each message's source **byte offset** into
    /// `Message.extra[`[`offsets::OFFSET_KEY`](crate::offsets::OFFSET_KEY)`]` (the offset of the
    /// record line that produced it). Only `cv rewind`'s message-to-byte pass turns this on —
    /// ordinary lazy/bulk/full consumers never see the stamp, so their output is unchanged.
    /// Adapters that don't track byte positions (everything but claude/codex JSONL) ignore it.
    /// Implies nothing by itself: the adapters only stamp on their span-tracking paths, so use
    /// [`lazy_offsets()`](ParseOptions::lazy_offsets) (spans + offsets) rather than setting it solo.
    pub offsets: bool,
    /// **Format-complete**: lose nothing on parse. Carry every record — including the
    /// non-conversational meta records the lean passes skip (mode/permission-mode/attachment/
    /// queue-operation/last-prompt/hook telemetry/…) — as round-trippable carrier messages, so a
    /// `parse → emit` round-trip reproduces the original byte-for-byte (modulo key order). Off by
    /// default so the conversational passes (render/dataset/index) stay exactly as they were; turned
    /// on by conversion/round-trip/[`prune`](crate::prune)-via-IR paths that must not drop a field.
    /// Implies [`extra`](ParseOptions::extra). Adapters that can't yet carry every record ignore it.
    pub complete: bool,
}

impl ParseOptions {
    /// Full fidelity: every harness-specific field materialized, inline content. Used by [`collect`]
    /// / `parse` and by conversion/round-trip paths that must lose nothing.
    pub fn full() -> Self {
        ParseOptions {
            extra: true,
            ..Default::default()
        }
    }

    /// The lean bulk-text pass: faithful message text, no fat `extra` sidecars, inline content
    /// (these consumers read everything, so spans wouldn't help). Used by index/search/dataset.
    pub fn bulk() -> Self {
        ParseOptions::default()
    }

    /// Format-complete, lossless: full `extra` **and** every record carried (incl. meta records the
    /// other passes skip), inline content. For `parse → emit` round-trips that must reproduce the
    /// source losslessly. The strongest fidelity setting.
    pub fn complete() -> Self {
        ParseOptions {
            extra: true,
            complete: true,
            ..Default::default()
        }
    }

    /// Partial-access pass: lazy content [`Span`](crate::lazy::Span)s so unread giant fields are
    /// never materialized. For windowed reads / pagination (Phase 2).
    pub fn lazy() -> Self {
        ParseOptions {
            spans: true,
            ..Default::default()
        }
    }

    /// [`lazy()`](ParseOptions::lazy) **plus** the harness-specific `extra` sidecars and metadata
    /// (`subtype`, `compactMetadata`, `isCompactSummary`, attribution, …).
    /// Sidecars and other rich metadata can themselves be large: only supported content fields
    /// become spans. For metadata scans and bounded decoded-text reducers (e.g. doctor).
    pub fn lazy_extra() -> Self {
        ParseOptions {
            spans: true,
            extra: true,
            ..Default::default()
        }
    }

    /// [`lazy()`](ParseOptions::lazy) plus per-message byte-offset stamping — the pass `cv rewind`
    /// makes to map a message to its record ([`offsets`](crate::offsets)). Everything else should
    /// use plain `lazy()`.
    pub fn lazy_offsets() -> Self {
        ParseOptions {
            spans: true,
            offsets: true,
            ..Default::default()
        }
    }
}

/// Whether a [`MessageSink`] wants the stream to keep going. Returning [`Flow::Stop`] lets a
/// consumer that has seen enough (e.g. a head-only embedder, or a `--range` window) end the parse
/// without reading the rest of the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Receives messages as an adapter streams them. [`message`](MessageSink::message) is called once
/// per message, in transcript order; the message is dropped after the call returns unless the sink
/// keeps it.
///
/// [`meta`](MessageSink::meta) is an optional early signal carrying the session-level fields
/// (model/cwd/title/git) with an empty `messages` vec. Adapters that know the metadata before the
/// body call it *before* the first [`message`](MessageSink::message); the bridge `stream` (over a
/// full `parse`) always does. Sinks that must render a header ahead of the body (e.g. `cv show`)
/// use it; most sinks ignore it and just read the returned `Session` afterward.
pub trait MessageSink {
    fn message(&mut self, m: Message) -> Flow;

    /// Return a latched consumer error, if any. A source-backed field can fail while being
    /// inspected: that failure must not be represented as an intentional [`Flow::Stop`].
    /// Wrappers forward this channel; completed passes check it before publishing any output.
    fn take_error(&mut self) -> Option<anyhow::Error> {
        None
    }

    /// Deliver a message and surface failures distinctly from an intentional early stop.
    fn try_message(&mut self, m: Message) -> Result<Flow> {
        let flow = self.message(m);
        match self.take_error() {
            Some(error) => Err(error),
            None => Ok(flow),
        }
    }

    /// Session metadata, possibly emitted before the first message. Default: ignore.
    fn meta(&mut self, _session: &Session) {}

    /// An auxiliary path is about to be consulted (including metadata/absence or directory
    /// membership). Adapters report before reading; wrappers must forward the notification.
    fn dependency(&mut self, _path: &std::path::Path) {}

    /// The adapter is between records, holding nothing back, `offset` bytes into the file: may it
    /// take a [`Checkpoint`] here? Only a sink that can put everything it holds into one says
    /// yes, so a pass through any other sink never takes one. Default: no.
    fn wants_checkpoint(&mut self, _offset: u64) -> bool {
        false
    }

    /// Add this sink's state to `cp` (the adapter's is already in) and pass it on to any sink
    /// this one wraps. Called only after [`wants_checkpoint`](MessageSink::wants_checkpoint) said
    /// yes. Default: nothing to add.
    fn checkpoint(&mut self, _cp: &mut Checkpoint) {}
}

/// Run an adapter boundary with a consumer error guard. Legacy adapter loops can keep using
/// `message() -> Flow`; failures stop delivery immediately and surface after the adapter returns,
/// including errors raised by metadata, dependencies, checkpoints or an EOF-only sink check.
/// An intentional stop remains successful. Any staged output from a failed pass must be discarded.
pub fn checked<T>(sink: &mut dyn MessageSink, read: impl FnOnce(&mut dyn MessageSink) -> Result<T>) -> Result<T> {
    let mut guard = ErrorGuard {
        inner: sink,
        error: None,
        failed: false,
    };
    if let Some(error) = guard.take_error() {
        return Err(error);
    }
    let result = read(&mut guard);
    let error = guard.take_error();
    // An inner checked boundary may already have returned the original diagnostic while its
    // failed outer sink now reports only a persistent failure marker. Preserve that Result.
    match result {
        Err(error) => Err(error),
        Ok(value) => match error {
            Some(error) => Err(error),
            None => Ok(value),
        },
    }
}

struct ErrorGuard<'a> {
    inner: &'a mut dyn MessageSink,
    error: Option<anyhow::Error>,
    failed: bool,
}
impl ErrorGuard<'_> {
    fn capture(&mut self) {
        if let Some(error) = self.inner.take_error() {
            self.failed = true;
            self.error.get_or_insert(error);
        }
    }
}
impl MessageSink for ErrorGuard<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.capture();
        self.error.take().or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("stream consumer has already failed"))
        })
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        match self.inner.try_message(m) {
            Ok(flow) => flow,
            Err(error) => {
                self.failed = true;
                self.error = Some(error);
                Flow::Stop
            }
        }
    }
    fn meta(&mut self, s: &Session) {
        if !self.failed {
            self.inner.meta(s);
            self.capture();
        }
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if !self.failed {
            self.inner.dependency(path);
            self.capture();
        }
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        if self.failed {
            return false;
        }
        let wants = self.inner.wants_checkpoint(offset);
        self.capture();
        wants && !self.failed
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if !self.failed {
            self.inner.checkpoint(cp);
            self.capture();
        }
    }
}

/// A point a pass over an append-only transcript can resume from: `offset` is the byte after the
/// last whole line read, and `state` holds what each layer of the pass held there, by layer
/// (`adapter`, `counter`, and each consumer's own key). Resuming at `offset` with every layer
/// restored from `state` gives what the whole pass would have given from there on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub offset: u64,
    pub state: serde_json::Map<String, serde_json::Value>,
}

impl Checkpoint {
    pub fn new(offset: u64) -> Self {
        Checkpoint {
            offset,
            state: Default::default(),
        }
    }

    /// Store `layer`'s state (serialization of these plain structs cannot fail; if it did, the
    /// layer would be missing and a resume would decline).
    pub fn put(&mut self, layer: &str, state: &impl Serialize) {
        if let Ok(v) = serde_json::to_value(state) {
            self.state.insert(layer.to_string(), v);
        }
    }

    /// `layer`'s state, when it is there and still reads as `T`.
    pub fn get<T: DeserializeOwned>(&self, layer: &str) -> Option<T> {
        serde_json::from_value(self.state.get(layer)?.clone()).ok()
    }

    /// How many messages the pass had delivered at this point (the counter layer's count): the
    /// index the first resumed message gets.
    pub fn messages(&self) -> Option<usize> {
        self.get::<CounterState>(COUNTER_LAYER).map(|c| c.messages)
    }
}

/// A sink that drops every message and holds nothing, so it can always be checkpointed: the
/// consumer of a pass that only counts.
pub struct Discard;

impl MessageSink for Discard {
    fn message(&mut self, _m: Message) -> Flow {
        Flow::Continue
    }
    fn wants_checkpoint(&mut self, _offset: u64) -> bool {
        true
    }
}

/// Any `FnMut(Message) -> Flow` is a sink — the ergonomic form for ad-hoc consumers.
impl<F: FnMut(Message) -> Flow> MessageSink for F {
    fn message(&mut self, m: Message) -> Flow {
        self(m)
    }
}

/// An ad-hoc fallible message callback. Existing `FnMut(Message) -> Flow` sinks stay infallible;
/// this wrapper latches a `Result` error, stops future delivery and exposes it to checked passes.
pub struct TryFnSink<F> {
    callback: F,
    error: Option<anyhow::Error>,
    failed: bool,
}
impl<F: FnMut(Message) -> Result<Flow>> TryFnSink<F> {
    pub fn new(callback: F) -> Self {
        Self {
            callback,
            error: None,
            failed: false,
        }
    }
}
impl<F: FnMut(Message) -> Result<Flow>> MessageSink for TryFnSink<F> {
    fn message(&mut self, message: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        match (self.callback)(message) {
            Ok(flow) => flow,
            Err(error) => {
                self.failed = true;
                self.error = Some(error);
                Flow::Stop
            }
        }
    }
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take().or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("message callback has already failed"))
        })
    }
}

/// Fans one adapter pass into two sinks, so a single read of a transcript feeds multiple consumers
/// — e.g. `cv index` driving the FTS [`ChunkSink`] and the event-catalog
/// [`EventSink`](crate::events::EventSink) off the same `stream()`. Each message is cloned once
/// while both sinks are live; under [`ParseOptions::lazy`] large content is a
/// [`Span`](crate::lazy::Span) handle, so the clone stays O(small).
///
/// Flow semantics: the stream continues until **both** sinks have said [`Flow::Stop`], and a sink
/// that stopped is never called again. (Stopping on the first `Stop` would silently truncate the
/// other consumer's view — e.g. a head-only sink ending event extraction mid-session.)
pub struct TeeSink<'a> {
    a: &'a mut dyn MessageSink,
    b: &'a mut dyn MessageSink,
    a_stopped: bool,
    b_stopped: bool,
    error: Option<anyhow::Error>,
    failed: bool,
}

impl<'a> TeeSink<'a> {
    pub fn new(a: &'a mut dyn MessageSink, b: &'a mut dyn MessageSink) -> Self {
        TeeSink {
            a,
            b,
            a_stopped: false,
            b_stopped: false,
            error: None,
            failed: false,
        }
    }
}

impl TeeSink<'_> {
    fn capture(&mut self) {
        let error = self.a.take_error().or_else(|| self.b.take_error());
        if let Some(error) = error {
            self.failed = true;
            self.error.get_or_insert(error);
        }
    }
    fn deliver(sink: &mut dyn MessageSink, m: Message, stopped: &mut bool) -> Result<()> {
        *stopped = sink.try_message(m)? == Flow::Stop;
        Ok(())
    }
}
impl MessageSink for TeeSink<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.capture();
        self.error.take().or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("stream consumer has already failed"))
        })
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if self.failed {
            return;
        }
        self.a.dependency(path);
        self.capture();
        if !self.failed {
            self.b.dependency(path);
            self.capture();
        }
    }
    fn meta(&mut self, s: &Session) {
        if self.failed {
            return;
        }
        self.a.meta(s);
        self.capture();
        if !self.failed {
            self.b.meta(s);
            self.capture();
        }
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        if self.failed {
            return false;
        }
        let wants = self.a.wants_checkpoint(offset) && self.b.wants_checkpoint(offset);
        self.capture();
        wants && !self.failed
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if self.failed {
            return;
        }
        self.a.checkpoint(cp);
        self.capture();
        if !self.failed {
            self.b.checkpoint(cp);
            self.capture();
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        let result = match (self.a_stopped, self.b_stopped) {
            (false, false) => {
                let copy = m.clone();
                Self::deliver(self.a, m, &mut self.a_stopped)
                    .and_then(|()| Self::deliver(self.b, copy, &mut self.b_stopped))
            }
            (false, true) => Self::deliver(self.a, m, &mut self.a_stopped),
            (true, false) => Self::deliver(self.b, m, &mut self.b_stopped),
            (true, true) => Ok(()),
        };
        if let Err(error) = result {
            self.failed = true;
            self.error = Some(error);
        }
        if self.failed || (self.a_stopped && self.b_stopped) {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// A sink that retains every message — the bridge back to a whole [`Session`]. See [`collect`].
#[derive(Default)]
pub struct CollectSink {
    pub messages: Vec<Message>,
}

impl MessageSink for CollectSink {
    fn message(&mut self, m: Message) -> Flow {
        self.messages.push(m);
        Flow::Continue
    }
}

/// Hand an already-parsed whole session to `sink` — the `stream` of a store that can only be read
/// whole (an account export, an OpenSession document): metadata first, so header-rendering sinks
/// have it, then each message until the sink stops. Returns the metadata with `messages` emptied.
pub fn replay(mut session: Session, sink: &mut dyn MessageSink) -> Result<Session> {
    let messages = std::mem::take(&mut session.messages);
    sink.meta(&session);
    if let Some(error) = sink.take_error() {
        return Err(error);
    }
    for m in messages {
        if sink.try_message(m)? == Flow::Stop {
            break;
        }
    }
    Ok(session)
}

/// Drive `adapter.stream` into a [`CollectSink`] and reattach the messages — i.e. fully parse `r`
/// at full fidelity. This is the default body of [`Adapter::parse`](crate::harness::Adapter::parse)
/// for adapters that implement a native `stream`.
pub fn collect(adapter: &dyn Adapter, r: &SessionRef) -> Result<Session> {
    let mut session = collect_with(adapter, r, &ParseOptions::full())?;
    // Full-fidelity parse: resolve any lazy content spans so the returned `Session` owns all its
    // content inline (safe for `--json` serialization, cross-harness emit, `Deref` reads, etc.).
    session.materialize()?;
    Ok(session)
}

/// Like [`collect`] but with explicit [`ParseOptions`] and **without** materializing — the returned
/// session may carry lazy content [`Span`]s. Consumers that hold the whole session but read it once
/// (e.g. dataset export) use this and resolve per block, so a giant field is never owned wholesale.
/// Use [`collect`] (or call `.materialize()`) when you need owned inline content.
pub fn collect_with(adapter: &dyn Adapter, r: &SessionRef, opts: &ParseOptions) -> Result<Session> {
    let mut sink = CollectSink::default();
    let mut session = stream_counted(adapter, r, opts, &mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// `adapter.try_stream(r, opts, sink)`, plus what a pass that read the whole session learned: when the
/// sink never said [`Flow::Stop`], the parse's own turn count ([`Message::is_turn`]) is written back
/// to the catalog for a row that holds `0` ("not counted" — a Codex rollout over the discovery scan
/// cap, an adapter that cannot count cheaply), so `cv ls` shows the real number from the first full
/// read on. The same pass counts the session's affordances ([`crate::affordance::Counter`]) and
/// records them unless the catalog's are still fresh, which is what lets `has:<kind>` answer from
/// the catalog. The full-read doors (`collect`/`parse`, `cv show`, `cv index`) stream through this.
pub fn stream_counted(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    let source = crate::source::Probe::default().snapshot(r);
    stream_counted_with_source(adapter, r, opts, sink, &source)
}

/// [`stream_counted`] using an operation's already captured source snapshot. The caller must
/// capture it before this pass reads the source; publication still revalidates it and the
/// adapter's dynamic read set. Side-store counts remain conditional on cache freshness.
pub fn stream_counted_with_source(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    source: &crate::source::Snapshot,
) -> Result<Session> {
    let (session, counter) = stream_with_counter(adapter, r, opts, sink)?;
    if let Some((turns, counter, dependencies)) = counter {
        let source = source.with_reads(&dependencies);
        if !crate::source::unchanged(r, &source) {
            return Ok(session);
        }
        note_turns(r, turns);
        crate::catalog::note_affordances_with_source(r, &source, || {
            counter.finish(&session, &adapter.store_affordances(r))
        });
    }
    Ok(session)
}

/// What a whole-session read counted: its turns and its affordances.
#[derive(Debug, Clone)]
pub struct Counted {
    pub turns: usize,
    pub counts: crate::affordance::Counts,
    /// Adapter dependencies consulted before the pass computed these counts.
    pub dependencies: crate::source::ReadSet,
}

impl Counted {
    /// Legacy compatibility entry point. Without a before-read source snapshot, it cannot
    /// safely publish cached turn or affordance counts, and therefore does nothing.
    /// Use [`Self::record_with_source`] to publish counts against the source that was read.
    #[deprecated(note = "no-op without a before-read source snapshot; use record_with_source")]
    pub fn record(self, _r: &SessionRef) {}

    /// Publish against the dependencies captured before this pass, including any side-store rows.
    pub fn record_with_source(self, r: &SessionRef, source: &crate::source::Snapshot) {
        let source = source.with_reads(&self.dependencies);
        if crate::source::unchanged(r, &source) {
            note_turns(r, self.turns);
            crate::catalog::note_affordances_with_source(r, &source, || self.counts);
        }
    }
}

/// [`stream_counted`] without the catalog writes: what the pass counted comes back (`None` when the
/// sink stopped early) for the caller to publish through [`Counted::record_with_source`] with a
/// before-read snapshot. A sweep that parses on many threads keeps its catalog writes on one. Reads the adapter's side store every time (the
/// recording path reads it only when a write is due).
pub fn stream_counting(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<(Session, Option<Counted>)> {
    let (session, counter) = stream_with_counter(adapter, r, opts, sink)?;
    let counted = counter.map(|(turns, counter, dependencies)| Counted {
        turns,
        dependencies,
        counts: counter.finish(&session, &adapter.store_affordances(r)),
    });
    Ok((session, counted))
}

/// What a [`stream_resumable`] pass gives back.
pub struct Resumed {
    pub session: Session,
    /// What the whole session counted (the resumed part added to the checkpoint's), `None` when
    /// the sink stopped early.
    pub counted: Option<Counted>,
    /// The last checkpoint the pass took, or else the one it resumed from (`None` when it has
    /// neither, or stopped early): the mark the next pass over the grown file resumes from.
    pub last: Option<Checkpoint>,
}

/// [`stream_counting`] for a pass that can be continued: it takes a [`Checkpoint`] each time the
/// file has advanced `every` bytes past the last one (where the adapter and every sink allow), and
/// given `from` it starts there instead of at byte 0, with the counts restored from it. The
/// caller restores its own sinks from `from` and checks that `from` still describes the file.
/// `Ok(None)`: the adapter cannot resume from `from` and touched nothing — read the session
/// whole instead.
pub fn stream_resumable(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    every: u64,
    from: Option<&Checkpoint>,
) -> Result<Option<Resumed>> {
    let mut counting = Counting::new(r, sink, Some(every));
    let session = match from {
        Some(cp) => {
            let Some(state) = cp.get::<CounterState>(COUNTER_LAYER) else {
                return Ok(None);
            };
            counting.restore(state, cp.offset);
            match adapter.try_resume(r, opts, &mut counting, cp)? {
                Some(s) => s,
                None => return Ok(None),
            }
        }
        None => adapter.try_stream(r, opts, &mut counting)?,
    };
    // A resumed pass that read too little to take one still passed through `from`.
    let last = counting
        .last
        .take()
        .or_else(|| from.cloned())
        .filter(|_| !counting.stopped);
    let counted = (!counting.stopped).then(|| Counted {
        turns: counting.turns,
        dependencies: counting.dependencies,
        counts: counting.affordances.finish(&session, &adapter.store_affordances(r)),
    });
    Ok(Some(Resumed { session, counted, last }))
}

/// The [`Checkpoint`] layer an adapter keeps its own state under.
pub const ADAPTER_LAYER: &str = "adapter";

/// The [`Checkpoint`] layer the counting wrapper keeps its state under.
const COUNTER_LAYER: &str = "counter";

/// What the counting wrapper holds at a checkpoint.
#[derive(Serialize, Deserialize)]
struct CounterState {
    messages: usize,
    turns: usize,
    counts: std::collections::BTreeMap<String, u32>,
    #[serde(default = "crate::source::ReadSet::unrecorded")]
    dependencies: crate::source::ReadSet,
}

/// The wrapper every counted pass streams through: turns, messages and affordances as they go
/// by, and (in a resumable pass) the checkpoints.
struct Counting<'a> {
    inner: &'a mut dyn MessageSink,
    messages: usize,
    turns: usize,
    affordances: crate::affordance::Counter,
    dependencies: crate::source::ReadSet,
    stopped: bool,
    failed: bool,
    error: Option<anyhow::Error>,
    /// Take a checkpoint every this many bytes (`None`: never).
    every: Option<u64>,
    last_offset: u64,
    last: Option<Checkpoint>,
}

impl<'a> Counting<'a> {
    fn new(r: &SessionRef, inner: &'a mut dyn MessageSink, every: Option<u64>) -> Self {
        Counting {
            inner,
            messages: 0,
            turns: 0,
            affordances: crate::affordance::Counter::new(r),
            dependencies: Default::default(),
            stopped: false,
            failed: false,
            error: None,
            every,
            last_offset: 0,
            last: None,
        }
    }

    fn restore(&mut self, state: CounterState, offset: u64) {
        self.messages = state.messages;
        self.turns = state.turns;
        self.dependencies = state.dependencies;
        let counts = state
            .counts
            .iter()
            .filter_map(|(k, n)| Some((crate::affordance::Kind::parse(k)?, *n)))
            .collect();
        self.affordances.restore(counts);
        self.last_offset = offset;
    }
}

impl MessageSink for Counting<'_> {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        let error = self.error.take().or_else(|| self.inner.take_error());
        self.failed |= error.is_some();
        self.stopped |= self.failed;
        error.or_else(|| {
            self.failed
                .then(|| anyhow::anyhow!("counting stream has already failed"))
        })
    }
    fn dependency(&mut self, path: &std::path::Path) {
        if self.failed {
            return;
        }
        self.dependencies.record(path);
        self.inner.dependency(path);
    }
    fn meta(&mut self, s: &Session) {
        if !self.failed {
            self.inner.meta(s);
        }
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        self.messages += 1;
        self.turns += usize::from(m.is_turn());
        if let Err(error) = self.affordances.note(&m) {
            self.error = Some(error);
            self.failed = true;
            self.stopped = true;
            return Flow::Stop;
        }
        let flow = self.inner.message(m);
        self.stopped |= flow == Flow::Stop;
        flow
    }
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        !self.stopped
            && self.every.is_some_and(|e| offset >= self.last_offset.saturating_add(e))
            && self.inner.wants_checkpoint(offset)
    }
    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        if self.failed {
            return;
        }
        let counts = self
            .affordances
            .counts()
            .iter()
            .map(|(k, n)| (k.as_str().to_string(), *n))
            .collect();
        cp.put(
            COUNTER_LAYER,
            &CounterState {
                messages: self.messages,
                turns: self.turns,
                counts,
                dependencies: self.dependencies.clone(),
            },
        );
        self.inner.checkpoint(cp);
        self.last_offset = cp.offset;
        self.last = Some(cp.clone());
    }
}

type CompletedCounter = (usize, crate::affordance::Counter, crate::source::ReadSet);

/// `adapter.stream` with every message counted: the turns and the affordance [`Counter`]
/// (`None` when the sink stopped early, so the counts would be partial).
///
/// [`Counter`]: crate::affordance::Counter
fn stream_with_counter(
    adapter: &dyn Adapter,
    r: &SessionRef,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Result<(Session, Option<CompletedCounter>)> {
    let mut counting = Counting::new(r, sink, None);
    let session = adapter.try_stream(r, opts, &mut counting)?;
    let counted = (!counting.stopped).then_some((counting.turns, counting.affordances, counting.dependencies));
    Ok((session, counted))
}

/// A whole-session read of `r` counted `turns`: fill a "not counted" catalog row (see
/// [`stream_counted`]).
fn note_turns(r: &SessionRef, turns: usize) {
    if r.message_count == 0 && turns > 0 {
        crate::catalog::fill_count(r, turns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Role};

    fn text_msg(t: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content = vec![Block::Text { text: t.into() }];
        m
    }

    /// A sink that records the texts it saw and stops after `stop_after` messages.
    struct Probe {
        seen: Vec<String>,
        stop_after: usize,
    }
    impl MessageSink for Probe {
        fn message(&mut self, m: Message) -> Flow {
            self.seen.push(m.text().unwrap_or_default());
            if self.seen.len() >= self.stop_after {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    #[test]
    fn tee_feeds_both_until_both_stop_and_never_calls_a_stopped_sink() {
        let mut a = Probe {
            seen: Vec::new(),
            stop_after: 1,
        };
        let mut b = Probe {
            seen: Vec::new(),
            stop_after: 3,
        };
        let mut tee = TeeSink::new(&mut a, &mut b);
        // a stops after the first message; the tee keeps going for b until *it* stops.
        assert_eq!(tee.message(text_msg("one")), Flow::Continue);
        assert_eq!(tee.message(text_msg("two")), Flow::Continue);
        assert_eq!(tee.message(text_msg("three")), Flow::Stop);
        assert_eq!(a.seen, vec!["one"]);
        assert_eq!(b.seen, vec!["one", "two", "three"]);
    }

    /// Keeps every message (as JSON) and every checkpoint it is offered.
    #[derive(Default)]
    struct Keep {
        msgs: Vec<serde_json::Value>,
        cps: Vec<Checkpoint>,
    }
    impl MessageSink for Keep {
        fn message(&mut self, m: Message) -> Flow {
            self.msgs.push(serde_json::to_value(&m).unwrap());
            Flow::Continue
        }
        fn wants_checkpoint(&mut self, _offset: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, cp: &mut Checkpoint) {
            self.cps.push(cp.clone());
        }
    }

    #[test]
    fn wrappers_forward_read_sets_and_counter_checkpoints_keep_the_prefix() {
        let w = cv_testkit::World::in_process("read-set-wrappers");
        let path = w.base.join("session.jsonl");
        std::fs::write(&path, "primary").unwrap();
        let extra = w.base.join("external.txt");
        std::fs::write(&extra, "before").unwrap();
        let r = transcript(crate::ir::Harness::Codex, path);
        let base = crate::source::Probe::default().snapshot(&r);
        let mut a = crate::events::EventSink::new(None);
        let mut b = crate::events::EventSink::new(None);
        let mut tee = TeeSink::new(&mut a, &mut b);
        let mut affordances = crate::affordance::AffordanceSink::new(r.harness, &mut tee);
        let mut counting = Counting::new(&r, &mut affordances, Some(1));
        counting.dependency(&extra);
        let mut cp = Checkpoint::new(1);
        counting.checkpoint(&mut cp);
        let mut legacy = cp.clone();
        legacy
            .state
            .get_mut(COUNTER_LAYER)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("dependencies");
        let legacy_state = legacy.get::<CounterState>(COUNTER_LAYER).unwrap();
        assert!(!crate::source::unchanged(
            &r,
            &base.with_reads(&legacy_state.dependencies)
        ));
        let state = cp.get::<CounterState>(COUNTER_LAYER).unwrap();
        let events_resumed = crate::events::EventSink::resume(&cp).unwrap();
        let mut keep = Keep::default();
        let mut resumed = Counting::new(&r, &mut keep, Some(1));
        resumed.restore(state, cp.offset);
        assert!(crate::source::unchanged(&r, &base.with_reads(&resumed.dependencies)));
        drop(counting);
        std::fs::write(&extra, "after!").unwrap();
        assert!(!crate::source::unchanged(&r, &base.with_reads(a.dependencies())));
        assert!(!crate::source::unchanged(&r, &base.with_reads(b.dependencies())));
        assert!(!crate::source::unchanged(&r, &base.with_reads(&resumed.dependencies)));
        assert!(!crate::source::unchanged(
            &r,
            &base.with_reads(events_resumed.dependencies())
        ));
    }

    #[test]
    fn try_fn_sink_distinguishes_stops_and_latches_errors_after_drain() {
        let mut stop = TryFnSink::new(|_: Message| Ok(Flow::Stop));
        assert_eq!(stop.try_message(text_msg("enough")).unwrap(), Flow::Stop);
        assert!(stop.take_error().is_none());
        let calls = std::cell::Cell::new(0);
        let mut sink = TryFnSink::new(|_: Message| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(Flow::Continue)
            } else {
                anyhow::bail!("selected source changed")
            }
        });
        assert_eq!(sink.try_message(text_msg("first")).unwrap(), Flow::Continue);
        assert_eq!(sink.message(text_msg("second")), Flow::Stop);
        assert_eq!(sink.take_error().unwrap().to_string(), "selected source changed");
        assert_eq!(sink.message(text_msg("ignored")), Flow::Stop);
        assert_eq!(calls.get(), 2);
        assert!(
            checked(&mut sink, |_sink| Ok(())).is_err(),
            "drained sink remains failed"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum FailAt {
        Message,
        Meta,
        Dependency,
        Checkpoint,
        End,
    }
    struct FailingSink {
        at: FailAt,
        seen: usize,
        metas: usize,
        dependencies: usize,
        checkpoints: usize,
        error: Option<anyhow::Error>,
        ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl FailingSink {
        fn new(at: FailAt, ended: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
            Self {
                at,
                seen: 0,
                metas: 0,
                dependencies: 0,
                checkpoints: 0,
                error: None,
                ended,
            }
        }
        fn fail(&mut self) {
            self.error = Some(anyhow::anyhow!("source projection unavailable"));
        }
    }
    impl MessageSink for FailingSink {
        fn message(&mut self, _: Message) -> Flow {
            self.seen += 1;
            if matches!(self.at, FailAt::Message) && self.seen == 2 {
                self.fail();
            }
            Flow::Continue // A pending error must win even over Continue.
        }
        fn meta(&mut self, _: &Session) {
            self.metas += 1;
            if matches!(self.at, FailAt::Meta) {
                self.fail();
            }
        }
        fn dependency(&mut self, _: &std::path::Path) {
            self.dependencies += 1;
            if matches!(self.at, FailAt::Dependency) {
                self.fail();
            }
        }
        fn wants_checkpoint(&mut self, _: u64) -> bool {
            true
        }
        fn checkpoint(&mut self, _: &mut Checkpoint) {
            self.checkpoints += 1;
            if matches!(self.at, FailAt::Checkpoint) {
                self.fail();
            }
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            if matches!(self.at, FailAt::End) && self.ended.load(std::sync::atomic::Ordering::Relaxed) {
                self.fail();
            }
            self.error.take()
        }
    }
    struct IgnoresStop {
        ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
        side_reads: std::sync::atomic::AtomicUsize,
    }
    impl Adapter for IgnoresStop {
        fn harness(&self) -> crate::Harness {
            crate::Harness::Codex
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> Result<Vec<SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(&self, r: &SessionRef, _: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            let session = Session::new(&r.id, r.harness);
            sink.meta(&session);
            for n in 1..=3 {
                sink.message(text_msg("turn")); // Deliberately ignore Stop, as some metadata scans do.
                sink.dependency(&r.path);
                if sink.wants_checkpoint(n) {
                    sink.checkpoint(&mut Checkpoint::new(n));
                }
            }
            sink.meta(&session); // Late callbacks must not resume after a failure.
            self.ended.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(session)
        }
        fn store_affordances(&self, _: &SessionRef) -> Vec<crate::affordance::Affordance> {
            self.side_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Vec::new()
        }
    }

    #[test]
    fn failed_streams_never_finish_counts_or_publish_cache_or_marks() {
        let w = cv_testkit::World::in_process("fallible-stream-publication");
        let path = w.base.join("session.jsonl");
        std::fs::write(&path, "scratch primary").unwrap();
        let r = transcript(crate::Harness::Codex, path);
        for at in [
            FailAt::Message,
            FailAt::Meta,
            FailAt::Dependency,
            FailAt::Checkpoint,
            FailAt::End,
        ] {
            for resumable in [false, true] {
                if matches!(at, FailAt::Checkpoint) && !resumable {
                    continue;
                }
                let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let adapter = IgnoresStop {
                    ended: ended.clone(),
                    side_reads: Default::default(),
                };
                let mut failing = FailingSink::new(at, ended);
                let result = if resumable {
                    stream_resumable(&adapter, &r, &ParseOptions::lazy(), &mut failing, 1, None).map(|_| ())
                } else {
                    stream_counted(&adapter, &r, &ParseOptions::lazy(), &mut failing).map(|_| ())
                };
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("source projection unavailable"),
                    "{at:?}"
                );
                assert_eq!(
                    adapter.side_reads.load(std::sync::atomic::Ordering::Relaxed),
                    0,
                    "{at:?}: no finish after failure"
                );
                assert!(crate::catalog::AffordanceTable::load().counts(&r).is_none());
                assert!(crate::resume::MarkTable::load().get(&r).is_none());
                if matches!(at, FailAt::Message) {
                    assert_eq!((failing.seen, failing.metas, failing.dependencies), (2, 1, 1));
                    assert_eq!(failing.checkpoints, usize::from(resumable));
                }
                if matches!(at, FailAt::Meta) {
                    assert_eq!((failing.seen, failing.metas), (0, 1));
                }
            }
        }
    }

    #[test]
    fn tee_errors_abort_both_but_intentional_stop_keeps_the_other_live() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut failure = FailingSink::new(FailAt::Message, flag);
        let mut other = Keep::default();
        let mut tee = TeeSink::new(&mut failure, &mut other);
        let mut affordances = crate::affordance::AffordanceSink::new(crate::Harness::Codex, &mut tee);
        let error = checked(&mut affordances, |sink| {
            assert_eq!(sink.message(text_msg("first")), Flow::Continue);
            assert_eq!(sink.message(text_msg("second")), Flow::Stop);
            // Even if a legacy caller drains and ignores the diagnostic, the failed latch holds.
            assert!(sink.take_error().is_some());
            assert_eq!(sink.message(text_msg("late")), Flow::Stop);
            sink.meta(&Session::new("test", crate::Harness::Codex));
            sink.checkpoint(&mut Checkpoint::new(3));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("already failed"));
        assert_eq!(failure.seen, 2);
        assert_eq!(other.msgs.len(), 1, "failed first branch stops its peer immediately");
        assert!(other.cps.is_empty());

        let mut one = |_m: Message| Flow::Stop;
        let mut two = Keep::default();
        let mut tee = TeeSink::new(&mut one, &mut two);
        checked(&mut tee, |sink| {
            assert_eq!(sink.try_message(text_msg("one"))?, Flow::Continue);
            assert_eq!(sink.try_message(text_msg("two"))?, Flow::Continue);
            Ok(())
        })
        .unwrap();
        assert_eq!(two.msgs.len(), 2);
    }

    #[test]
    fn built_in_stream_boundary_and_nested_guard_return_the_original_error() {
        let w = cv_testkit::World::in_process("built-in-fallible-stream");
        let dir = w.base.join("projects/group/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.jsonl");
        let record = r#"{"uuid":"u","type":"user","message":{"role":"user","parts":[{"text":"hello"}]}}"#;
        std::fs::write(&path, format!("{record}\n{record}\n{record}\n")).unwrap();
        let r = transcript(crate::Harness::Qwen, path);
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        for checked_entry in [false, true] {
            let mut failing = FailingSink::new(FailAt::Message, Default::default());
            let result = if checked_entry {
                adapter.try_stream(&r, &ParseOptions::lazy(), &mut failing)
            } else {
                adapter.stream(&r, &ParseOptions::lazy(), &mut failing)
            };
            assert_eq!(result.unwrap_err().to_string(), "source projection unavailable");
            assert_eq!(failing.seen, 2);
        }
    }

    /// The claude and codex transcripts of the harness corpus (the two adapters that resume).
    fn corpus_transcripts() -> Vec<SessionRef> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut out = Vec::new();
        for (dir, harness) in [
            ("claude", crate::ir::Harness::Claude),
            ("codex", crate::ir::Harness::Codex),
        ] {
            for e in walkdir::WalkDir::new(root.join(dir)).sort_by_file_name() {
                let p = e.unwrap().into_path();
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                let top = match harness {
                    crate::ir::Harness::Claude => p
                        .parent()
                        .and_then(|d| d.parent())
                        .is_some_and(|d| d.ends_with("projects")),
                    _ => name.starts_with("rollout-"),
                };
                if top && name.ends_with(".jsonl") {
                    out.push(transcript(harness, p));
                }
            }
        }
        out
    }

    fn transcript(harness: crate::ir::Harness, path: std::path::PathBuf) -> SessionRef {
        SessionRef {
            id: path.file_stem().unwrap().to_string_lossy().into_owned(),
            harness,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    /// One pass: the messages, the session (as JSON), the counts, and every checkpoint offered.
    fn pass(r: &SessionRef, from: Option<&Checkpoint>) -> (Keep, serde_json::Value, Counted) {
        let adapter = crate::harness::for_harness(r.harness).unwrap();
        let mut keep = Keep::default();
        let done = stream_resumable(&*adapter, r, &ParseOptions::lazy(), &mut keep, 1, from)
            .unwrap()
            .unwrap_or_else(|| panic!("{} declined to resume", r.path.display()));
        let session = serde_json::to_value(&done.session).unwrap();
        (keep, session, done.counted.expect("a whole pass counts"))
    }

    /// Resuming at any checkpoint a pass took reproduces the rest of that pass: the same messages
    /// from the checkpoint's message on, the same session, the same counts, and the same later
    /// checkpoints. Over every claude and codex transcript of the corpus, at up to a dozen
    /// checkpoints spread through each.
    #[test]
    fn a_resumed_pass_is_the_rest_of_the_whole_pass() {
        let transcripts = corpus_transcripts();
        assert!(transcripts.len() >= 20, "corpus moved? found {}", transcripts.len());
        let mut resumed = 0;
        for r in &transcripts {
            let (full, session, counted) = pass(r, None);
            assert!(full.cps.len() > 1, "{}: no checkpoints", r.path.display());
            let step = (full.cps.len() / 12).max(1);
            for (i, cp) in full.cps.iter().enumerate().step_by(step) {
                let (rest, s, c) = pass(r, Some(cp));
                let n = cp.messages().unwrap();
                let at = format!("{} @ {}", r.path.display(), cp.offset);
                assert_eq!(rest.msgs, full.msgs[n..], "{at}: messages");
                assert_eq!(s, session, "{at}: session");
                assert_eq!((c.turns, &c.counts), (counted.turns, &counted.counts), "{at}: counts");
                assert!(rest.cps == full.cps[i + 1..], "{at}: later checkpoints");
                resumed += 1;
            }
        }
        assert!(resumed > 200, "only {resumed} resumes");
    }

    /// Native Qwen records finalize their own messages, but emission needs prefix call/model
    /// state. Compression and rewind remain IR markers; legacy Gemini replay declines resume.
    #[test]
    fn qwen_record_checkpoints_preserve_state_and_partial_eof_records() {
        let w = cv_testkit::World::in_process("qwen-record-resume");
        let dir = w.base.join("projects/group/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let r = transcript(crate::Harness::Qwen, dir.join("s1.jsonl"));
        let rec = |id: &str, body: serde_json::Value| {
            let mut v = serde_json::json!({"uuid":id,"sessionId":"s1","cwd":"/scratch",
                "version":"0.25.1","timestamp":"2026-10-11T00:00:01Z"});
            v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
            v.to_string()
        };
        let lines = [
            rec(
                "u1",
                serde_json::json!({"type":"user","provenance":"real_user","message":{"role":"user",
                "parts":[{"text":"first prompt"},{"text":"<qwen:user-prompt-submit-context>\nhook\n</qwen:user-prompt-submit-context>"}]}}),
            ),
            rec(
                "a1",
                serde_json::json!({"type":"assistant","model":"first-model","message":{"role":"model",
                "parts":[{"functionCall":{"id":"c1","name":"tool_call","args":{"name":"agent","arguments":{"prompt":"investigate"}}}}]},
                "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":3}}),
            ),
            "invalid json".into(),
            rec(
                "r1",
                serde_json::json!({"type":"tool_result","message":{"role":"user","parts":[
                {"functionResponse":{"id":"c1","name":"tool_call","response":{"output":"returned"}}}]},
                "toolCallResult":{"callId":"c1","status":"success"}}),
            ),
            rec(
                "c1",
                serde_json::json!({"type":"system","subtype":"chat_compression","systemPayload":{
                "info":{"originalTokenCount":9,"newTokenCount":7},"compressedHistory":[{"role":"user","parts":[{"text":"summary"}]}]}}),
            ),
            rec(
                "b1",
                serde_json::json!({"type":"system","subtype":"rewind","systemPayload":{"targetMessageUuid":"u1"}}),
            ),
            rec(
                "t1",
                serde_json::json!({"type":"system","subtype":"custom_title","systemPayload":{"title":"late title"}}),
            ),
            rec(
                "a2",
                serde_json::json!({"type":"assistant","model":"next-model","gitBranch":"other",
                "message":{"role":"model","parts":[{"text":"tail answer"}]}}),
            ),
        ];
        let bytes = lines.join("\n"); // A valid final EOF record, deliberately without newline.
        std::fs::write(&r.path, &bytes).unwrap();
        let (full, session, counted) = pass(&r, None);
        assert_eq!(full.cps.len(), lines.len() - 1);
        assert!(full.cps.last().unwrap().offset < bytes.len() as u64);
        for (i, cp) in full.cps.iter().enumerate() {
            let (rest, s, c) = pass(&r, Some(cp));
            assert_eq!(rest.msgs, full.msgs[cp.messages().unwrap()..]);
            assert_eq!(s, session, "checkpoint {i}: metadata");
            assert_eq!((c.turns, c.counts), (counted.turns, counted.counts.clone()));
            assert_eq!(rest.cps, full.cps[i + 1..]);
        }
        let cp = full.cps.last().unwrap();
        let grown = format!(
            "{bytes}\n{}\n",
            rec(
                "u2",
                serde_json::json!({"type":"user","message":{"role":"user","parts":[{"text":"appended"}]}})
            )
        );
        std::fs::write(&r.path, &grown).unwrap();
        let (whole, expected, counts) = pass(&r, None);
        let (rest, got, c) = pass(&r, Some(cp));
        assert_eq!(rest.msgs, whole.msgs[cp.messages().unwrap()..]);
        assert_eq!(got, expected);
        assert_eq!((c.turns, c.counts), (counts.turns, counts.counts));

        // Every physical byte prefix counts exactly what the full parser saw at that baseline.
        let before = dir.join("prefix.jsonl");
        for cut in (0..grown.len()).step_by(17).chain([grown.len()]) {
            std::fs::write(&before, &grown.as_bytes()[..cut]).unwrap();
            let mut count = 0;
            let mut sink = |_: Message| {
                count += 1;
                Flow::Continue
            };
            crate::harness::for_harness(crate::Harness::Qwen)
                .unwrap()
                .stream(
                    &transcript(crate::Harness::Qwen, before.clone()),
                    &ParseOptions::full(),
                    &mut sink,
                )
                .unwrap();
            assert_eq!(
                crate::watch::resolve(&r, crate::watch::Mark::Bytes(cut as u64)),
                Some(count),
                "cut {cut}"
            );
        }
        assert_eq!(
            crate::watch::baseline(&r),
            Some(crate::watch::Mark::Bytes(grown.len() as u64))
        );

        let legacy = transcript(crate::Harness::Qwen, w.base.join("tmp/hash/chats/session.jsonl"));
        let adapter = crate::harness::for_harness(crate::Harness::Qwen).unwrap();
        let mut keep = Keep::default();
        assert!(adapter
            .resume(&legacy, &ParseOptions::lazy(), &mut keep, cp)
            .unwrap()
            .is_none());
        assert!(
            keep.msgs.is_empty() && keep.cps.is_empty(),
            "decline must touch nothing"
        );
    }

    /// The append case: a pass over a file whose writer stopped mid-line takes its last checkpoint
    /// before that line; once the line and more are appended, resuming there gives what a whole
    /// pass over the grown file gives.
    #[test]
    fn a_pass_resumes_across_an_append_that_finishes_a_partial_line() {
        let dir = std::env::temp_dir().join(format!("cv-resume-{}", uuid::Uuid::new_v4()));
        for src in corpus_transcripts().into_iter().step_by(4) {
            let bytes = std::fs::read(&src.path).unwrap();
            let cut = bytes.len() * 2 / 3;
            assert_ne!(bytes[cut - 1], b'\n', "pick a cut inside a line");
            let path = dir.join(src.path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &bytes[..cut]).unwrap();
            let r = transcript(src.harness, path.clone());
            let (head, _, _) = pass(&r, None);
            let last = head.cps.last().unwrap().clone();
            assert!(last.offset < cut as u64);
            assert_eq!(bytes[last.offset as usize - 1], b'\n');

            std::fs::write(&path, &bytes).unwrap();
            let (full, session, counted) = pass(&r, None);
            let (rest, s, c) = pass(&r, Some(&last));
            let n = last.messages().unwrap();
            assert_eq!(head.msgs[..n], full.msgs[..n]);
            assert_eq!(rest.msgs, full.msgs[n..], "{}", src.path.display());
            assert_eq!(s, session, "{}", src.path.display());
            assert_eq!((c.turns, c.counts), (counted.turns, counted.counts));

            // Resumed at the last checkpoint, the pass takes no new one: it hands that one back,
            // so the next pass still has a mark to resume from.
            let final_cp = full.cps.last().unwrap();
            let adapter = crate::harness::for_harness(r.harness).unwrap();
            let done = stream_resumable(
                &*adapter,
                &r,
                &ParseOptions::lazy(),
                &mut Keep::default(),
                1,
                Some(final_cp),
            )
            .unwrap()
            .unwrap();
            assert_eq!(done.last.as_ref(), Some(final_cp));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
