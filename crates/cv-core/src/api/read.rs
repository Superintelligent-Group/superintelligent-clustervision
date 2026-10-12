//! Small verified continuations for repeated API reads. No messages are cached.
use super::{session_meta_json, WindowSink};
use crate::source::{Probe, ReadSet, Snapshot};
use crate::stream::{Checkpoint, Flow, ParseOptions};
use crate::{Adapter, Session, SessionRef};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

const MAX_ENTRIES: usize = 32;
const MAX_ENTRY_BYTES: usize = 256 * 1024;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize)]
struct Key {
    harness: String,
    id: String,
    path: std::path::PathBuf,
    version: String,
    extra: bool,
}

impl Key {
    fn of(r: &SessionRef, version: String, extra: bool) -> Self {
        Self {
            harness: r.harness.as_str().into(),
            id: r.id.clone(),
            path: r.path.clone(),
            version,
            extra,
        }
    }
}

#[derive(Clone, Serialize)]
enum Payload {
    Window {
        index: usize,
        checkpoint: Checkpoint,
        meta: Option<Value>,
    },
    Head(Value),
}

#[derive(Clone, Serialize)]
struct Entry {
    key: Key,
    source: Snapshot,
    payload: Payload,
    #[serde(skip)]
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    entries: Mutex<Vec<Entry>>,
}

impl Cache {
    fn get(&self, key: &Key) -> Option<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|entry| &entry.key == key)
            .cloned()
    }
    fn remove(&self, key: &Key) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| &entry.key != key);
    }
    fn insert(&self, mut entry: Entry) {
        let Some(bytes) = bounded_size(&entry) else { return };
        entry.bytes = bytes;
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|old| old.key != entry.key);
        while entries.len() >= MAX_ENTRIES
            || entries.iter().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_CACHE_BYTES
        {
            entries.remove(0);
        }
        entries.push(entry);
    }
}

/// Count serialization through a bounded writer; never allocate a checkpoint-sized JSON string.
pub(super) fn bounded_size(value: &impl Serialize) -> Option<usize> {
    struct Size(usize);
    impl std::io::Write for Size {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_ENTRY_BYTES {
                return Err(std::io::Error::other("API read state exceeds budget"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut size = Size(0);
    serde_json::to_writer(&mut size, value).ok()?;
    Some(size.0)
}

pub(super) fn shared() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

pub(super) fn note_json_sources(message: &crate::Message, reads: &mut ReadSet) {
    for block in &message.content {
        if let crate::Block::ToolUse {
            input: crate::json::Json::Source(range),
            ..
        } = block
        {
            reads.record(range.path());
        }
    }
}

fn captured(r: &SessionRef) -> (Snapshot, ReadSet) {
    let before = Probe::default().snapshot(r);
    let mut reads = ReadSet::default();
    // Include replacement identity and ctime, even when size and mtime were restored.
    reads.record(&r.path);
    (before, reads)
}

fn fresh(r: &SessionRef, before: &Snapshot) -> bool {
    let mut probe = Probe::default();
    let current = probe.snapshot(r);
    before.fresh(&current, &mut probe)
}

fn window_sink(r: &SessionRef, start: usize, end: Option<usize>, capture: bool, reads: ReadSet) -> WindowSink {
    WindowSink {
        resolver: crate::Resolver::new(Some(r.path.clone())),
        idx: 0,
        start,
        end,
        msgs: Vec::new(),
        more: false,
        meta: None,
        fail: None,
        capture,
        checkpoint: None,
        reads,
    }
}

pub(super) fn window(
    cache: &Cache,
    adapter: &dyn Adapter,
    r: &SessionRef,
    start: usize,
    end: Option<usize>,
    extra: bool,
) -> Result<(Session, WindowSink)> {
    let opts = ParseOptions {
        extra,
        ..ParseOptions::lazy()
    };
    let version = crate::resume::version(adapter, "api-window1");
    if version.is_none() {
        let mut sink = window_sink(r, start, end, false, ReadSet::default());
        return Ok((adapter.try_stream(r, &opts, &mut sink)?, sink));
    }
    let key = version.map(|version| Key::of(r, version, extra));
    let (before, reads) = captured(r);
    let mut sink = window_sink(r, start, end, key.is_some(), reads);
    let mut resumed = false;
    let mut shell = None;
    if let Some(key) = &key {
        if let Some(entry) = cache.get(key) {
            if fresh(r, &entry.source) {
                if let Payload::Window {
                    index,
                    checkpoint,
                    meta,
                } = entry.payload
                {
                    if index <= start {
                        sink.idx = index;
                        sink.meta = meta;
                        if let Some(old_reads) = entry.source.reads() {
                            sink.reads.merge(&old_reads);
                        }
                        shell = adapter.try_resume(r, &opts, &mut sink, &checkpoint)?;
                        resumed = shell.is_some();
                    }
                }
            } else {
                cache.remove(key);
            }
        }
    }
    if shell.is_none() {
        // A declined resume promises no callbacks, but discard its initial state too.
        let (_, reads) = captured(r);
        sink = window_sink(r, start, end, key.is_some(), reads);
        shell = Some(adapter.try_stream(r, &opts, &mut sink)?);
    }
    let source = before.with_reads(&sink.reads);
    let stable = fresh(r, &source);
    if resumed && !stable {
        // Do not return a new suffix composed with an old prefix's metadata after a mutation.
        if let Some(key) = &key {
            cache.remove(key);
        }
        let (_, reads) = captured(r);
        let mut whole = window_sink(r, start, end, false, reads);
        let shell = adapter.try_stream(r, &opts, &mut whole)?;
        return Ok((shell, whole));
    }
    if stable {
        if let (Some(key), Some((index, checkpoint, meta))) = (key, sink.checkpoint.take()) {
            cache.insert(Entry {
                key,
                source,
                payload: Payload::Window {
                    index,
                    checkpoint,
                    meta,
                },
                bytes: 0,
            });
        }
    }
    Ok((shell.expect("a stream or accepted resume returned metadata"), sink))
}

pub(super) fn head(cache: &Cache, adapter: &dyn Adapter, r: &SessionRef) -> Result<Value> {
    let key = Key::of(r, format!("{}/api-head1", env!("CARGO_PKG_VERSION")), false);
    if let Some(entry) = cache.get(&key) {
        if fresh(r, &entry.source) {
            if let Payload::Head(value) = entry.payload {
                return Ok(value);
            }
        } else {
            cache.remove(&key);
        }
    }
    let (before, reads) = captured(r);
    struct Counter {
        total: usize,
        reads: ReadSet,
    }
    impl crate::MessageSink for Counter {
        fn message(&mut self, message: crate::Message) -> Flow {
            note_json_sources(&message, &mut self.reads);
            self.total += 1;
            Flow::Continue
        }
        fn dependency(&mut self, path: &std::path::Path) {
            self.reads.record(path);
        }
    }
    let mut counter = Counter { total: 0, reads };
    let shell = adapter.try_stream(r, &ParseOptions::lazy(), &mut counter)?;
    let mut value = session_meta_json(&shell);
    let map = value.as_object_mut().expect("session metadata is an object");
    map.insert("total".into(), json!(counter.total));
    map.insert("extra".into(), Value::Object(shell.extra));
    let source = before.with_reads(&counter.reads);
    if fresh(r, &source) && bounded_size(&value).is_some() {
        cache.insert(Entry {
            key,
            source,
            payload: Payload::Head(value.clone()),
            bytes: 0,
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, Message, MessageSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Observed<'a> {
        inner: &'a dyn Adapter,
        messages: AtomicUsize,
        dependency: Option<std::path::PathBuf>,
        rewrite: Mutex<Option<String>>,
        decline: bool,
    }
    impl<'a> Observed<'a> {
        fn new(inner: &'a dyn Adapter) -> Self {
            Self {
                inner,
                messages: AtomicUsize::new(0),
                dependency: None,
                rewrite: Mutex::new(None),
                decline: false,
            }
        }
    }
    struct Observer<'a> {
        sink: &'a mut dyn MessageSink,
        messages: &'a AtomicUsize,
    }
    impl MessageSink for Observer<'_> {
        fn message(&mut self, message: Message) -> Flow {
            self.messages.fetch_add(1, Ordering::Relaxed);
            self.sink.message(message)
        }
        fn meta(&mut self, session: &Session) {
            self.sink.meta(session);
        }
        fn dependency(&mut self, path: &std::path::Path) {
            self.sink.dependency(path);
        }
        fn wants_checkpoint(&mut self, offset: u64) -> bool {
            self.sink.wants_checkpoint(offset)
        }
        fn checkpoint(&mut self, checkpoint: &mut Checkpoint) {
            self.sink.checkpoint(checkpoint);
        }
        fn take_error(&mut self) -> Option<anyhow::Error> {
            self.sink.take_error()
        }
    }
    impl Adapter for Observed<'_> {
        fn harness(&self) -> Harness {
            self.inner.harness()
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            self.inner.roots()
        }
        fn discover(&self) -> Result<Vec<SessionRef>> {
            self.inner.discover()
        }
        fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.try_stream(
                r,
                opts,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
            )
        }
        fn resume_format(&self) -> Option<u32> {
            self.inner.resume_format()
        }
        fn resume(
            &self,
            r: &SessionRef,
            opts: &ParseOptions,
            sink: &mut dyn MessageSink,
            cp: &Checkpoint,
        ) -> Result<Option<Session>> {
            if self.decline {
                return Ok(None);
            }
            if let Some(body) = self.rewrite.lock().unwrap().take() {
                std::fs::write(&r.path, body)?;
            }
            if let Some(path) = &self.dependency {
                sink.dependency(path);
            }
            self.inner.try_resume(
                r,
                opts,
                &mut Observer {
                    sink,
                    messages: &self.messages,
                },
                cp,
            )
        }
    }
    fn result(shell: &Session, sink: &WindowSink) -> Value {
        json!({ "messages": sink.msgs, "idx": sink.idx, "more": sink.more,
            "meta": sink.meta.clone().unwrap_or_else(|| session_meta_json(shell)) })
    }
    fn oracle(adapter: &dyn Adapter, r: &SessionRef, start: usize, end: Option<usize>, extra: bool) -> Value {
        let mut sink = window_sink(r, start, end, false, ReadSet::default());
        let shell = adapter
            .try_stream(
                r,
                &ParseOptions {
                    extra,
                    ..ParseOptions::lazy()
                },
                &mut sink,
            )
            .unwrap();
        result(&shell, &sink)
    }
    fn fixture(world: &cv_testkit::World, count: usize) -> SessionRef {
        use std::io::Write as _;
        let path = world.transcript("cached-pages");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for i in 0..count {
            writeln!(
                file,
                r#"{{"type":"user","uuid":"m{i}","message":{{"role":"user","content":"hello"}}}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();
        SessionRef::new("cached-pages", Harness::Claude, path)
    }

    #[test]
    fn native_window_cache_matches_whole_pass_oracles() {
        let world = cv_testkit::World::in_process("api_native_windows");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut checked = 0;
        for harness in [Harness::Claude, Harness::Codex] {
            let adapter: Box<dyn Adapter> = match harness {
                Harness::Claude => Box::new(crate::harness::claude::Claude::with_config_dir(
                    &world.home.join(".claude"),
                )),
                _ => Box::new(crate::harness::codex::Codex::with_home(&world.home.join(".codex"))),
            };
            for file in walkdir::WalkDir::new(root.join(harness.as_str()))
                .into_iter()
                .filter_map(Result::ok)
            {
                if file.path().extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let r = SessionRef::new("oracle", harness, file.path());
                for extra in [false, true] {
                    let cache = Cache::default();
                    for (start, end) in [
                        (0, Some(2)),
                        (2, Some(4)),
                        (4, Some(6)),
                        (0, Some(1)),
                        (1000, Some(1000)),
                        (1, None),
                    ] {
                        let expected = oracle(adapter.as_ref(), &r, start, end, extra);
                        let (shell, sink) = window(&cache, adapter.as_ref(), &r, start, end, extra).unwrap();
                        assert_eq!(
                            result(&shell, &sink),
                            expected,
                            "{} {start} {end:?} extra={extra}",
                            file.path().display()
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(
            checked >= 30,
            "expected frozen Claude/Codex native JSONL fixtures, found {checked}"
        );
    }

    #[test]
    fn pages_resume_and_heads_reuse_only_unchanged_sources() {
        let world = cv_testkit::World::in_process("api_cache_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in [0, 5, 10] {
            let expected = oracle(&inner, &r, start, Some(start + 5), false);
            let (shell, sink) = window(&cache, &adapter, &r, start, Some(start + 5), false).unwrap();
            assert_eq!(result(&shell, &sink), expected);
        }
        assert_eq!(adapter.messages.load(Ordering::Relaxed), 18); // Five selected plus one probe per page.
        let first = head(&cache, &adapter, &r).unwrap();
        let counted = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(head(&cache, &adapter, &r).unwrap(), first);
        assert_eq!(adapter.messages.load(Ordering::Relaxed), counted);
        // An append invalidates both count and continuation rather than assuming prefix immutability.
        use std::io::Write as _;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&r.path).unwrap(),
            r#"{{"type":"user","uuid":"new","message":{{"role":"user","content":"added"}}}}"#
        )
        .unwrap();
        assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 21);
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, sink) = window(&cache, &adapter, &r, 15, Some(20), false).unwrap();
        assert_eq!(result(&shell, &sink), oracle(&inner, &r, 15, Some(20), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
        // Switching extra flavor must read whole rather than reuse a checkpoint built lean.
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 20, Some(21), true).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 21);
    }

    #[test]
    fn cache_retention_has_entry_and_byte_budgets() {
        let world = cv_testkit::World::in_process("api_cache_budget");
        let r = fixture(&world, 1);
        let (source, reads) = captured(&r);
        let source = source.with_reads(&reads);
        let cache = Cache::default();
        for i in 0..100 {
            cache.insert(Entry {
                key: Key::of(&r, format!("head{i}"), false),
                source: source.clone(),
                payload: Payload::Head(json!("x".repeat(100_000))),
                bytes: 0,
            });
        }
        let entries = cache.entries.lock().unwrap();
        assert!(entries.len() <= MAX_ENTRIES);
        assert!(entries.iter().map(|entry| entry.bytes).sum::<usize>() <= MAX_CACHE_BYTES);
        drop(entries);
        assert!(bounded_size(&json!("x".repeat(MAX_ENTRY_BYTES))).is_none());
    }
    #[test]
    fn changed_dynamic_reads_and_declined_resume_fall_back_whole() {
        let world = cv_testkit::World::in_process("api_dependency_invalidation");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let mut adapter = Observed::new(&inner);
        let dependency = world.home.join("absent-helper.json");
        adapter.dependency = Some(dependency.clone());
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        std::fs::write(dependency, "new metadata").unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
        adapter.decline = true;
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, sink) = window(&cache, &adapter, &r, 10, Some(15), false).unwrap();
        assert_eq!(result(&shell, &sink), oracle(&inner, &r, 10, Some(15), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 16);
    }

    #[test]
    fn mutation_during_resume_retries_without_stale_prefix_metadata() {
        let world = cv_testkit::World::in_process("api_resume_mutation");
        let r = fixture(&world, 20);
        let old = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"old-title\"}}\n{}",
            std::fs::read_to_string(&r.path).unwrap()
        );
        std::fs::write(&r.path, &old).unwrap();
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        *adapter.rewrite.lock().unwrap() = Some(old.replace("old-title", "new-title"));
        let (shell, sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(shell.title.as_deref(), Some("new-title"));
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn same_length_rewrite_with_restored_mtime_invalidates_primary_read() {
        let world = cv_testkit::World::in_process("api_primary_identity");
        let r = fixture(&world, 20);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        window(&cache, &adapter, &r, 0, Some(5), false).unwrap();
        let modified = std::fs::metadata(&r.path).unwrap().modified().unwrap();
        let body = std::fs::read_to_string(&r.path).unwrap().replace("hello", "other");
        std::fs::write(&r.path, body).unwrap();
        std::fs::File::open(&r.path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let before = adapter.messages.load(Ordering::Relaxed);
        let (shell, sink) = window(&cache, &adapter, &r, 5, Some(10), false).unwrap();
        assert_eq!(result(&shell, &sink), oracle(&inner, &r, 5, Some(10), false));
        assert_eq!(adapter.messages.load(Ordering::Relaxed) - before, 11);
    }

    #[test]
    #[ignore = "paired callback proof using a frozen scratch transcript"]
    fn sequential_pages_and_repeated_heads_read_work() {
        let world = cv_testkit::World::in_process("api_cached_read_work");
        let r = fixture(&world, 20_000);
        let inner = crate::harness::claude::Claude::with_config_dir(&world.home.join(".claude"));
        let adapter = Observed::new(&inner);
        let cache = Cache::default();
        for start in (0..20_000).step_by(200) {
            let expected = oracle(&inner, &r, start, Some(start + 200), false);
            let (shell, sink) = window(&cache, &adapter, &r, start, Some(start + 200), false).unwrap();
            assert_eq!(result(&shell, &sink), expected);
        }
        let page_callbacks = adapter.messages.load(Ordering::Relaxed);
        assert_eq!(page_callbacks, 20_099);
        for _ in 0..5 {
            assert_eq!(head(&cache, &adapter, &r).unwrap()["total"], 20_000);
        }
        let head_callbacks = adapter.messages.load(Ordering::Relaxed) - page_callbacks;
        assert_eq!(head_callbacks, 20_000);
        eprintln!("pages=100 selected=20000 cached_page_callbacks={page_callbacks} repeated_heads=5 head_callbacks={head_callbacks}");
    }
}
