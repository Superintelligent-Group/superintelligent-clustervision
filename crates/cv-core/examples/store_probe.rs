//! One explicitly rooted, read-only operation per process. See examples/store_probe.md.

use anyhow::{bail, Context, Result};
use cv_core::{Adapter, Block, Flow, Harness, Message, ParseOptions, Session, SessionRef};
use serde_json::{json, Map, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(feature = "probe-heap")]
mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    pub struct Allocator;
    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static REQUESTED: AtomicUsize = AtomicUsize::new(0);

    fn added(bytes: usize) {
        let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
        PEAK.fetch_max(live, Relaxed);
        CALLS.fetch_add(1, Relaxed);
        REQUESTED.fetch_add(bytes, Relaxed);
    }

    // No allocation in these hooks. Count successful Rust requests, including realloc growth;
    // SQLite/C allocations, mmap, stacks and allocator overhead are deliberately outside this.
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(layout) };
            if !p.is_null() {
                added(layout.size());
            }
            p
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc_zeroed(layout) };
            if !p.is_null() {
                added(layout.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Relaxed);
            unsafe { System.dealloc(p, layout) };
        }
        unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let out = unsafe { System.realloc(p, layout, size) };
            if !out.is_null() {
                LIVE.fetch_sub(layout.size(), Relaxed);
                added(size);
            }
            out
        }
    }

    pub fn begin() -> usize {
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        CALLS.store(0, Relaxed);
        REQUESTED.store(0, Relaxed);
        baseline
    }

    #[derive(serde::Serialize)]
    pub struct Snapshot {
        baseline_live_bytes: usize,
        peak_live_bytes: usize,
        peak_above_baseline_bytes: usize,
        stage_end_live_bytes: usize,
        allocation_requests: usize,
        requested_bytes: usize,
    }

    pub fn end(baseline: usize) -> Snapshot {
        let peak = PEAK.load(Relaxed);
        // Capture on the stack; allocating report JSON must not count itself as stage work.
        Snapshot {
            baseline_live_bytes: baseline,
            peak_live_bytes: peak,
            peak_above_baseline_bytes: peak.saturating_sub(baseline),
            stage_end_live_bytes: LIVE.load(Relaxed),
            allocation_requests: CALLS.load(Relaxed),
            requested_bytes: REQUESTED.load(Relaxed),
        }
    }
}

#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: heap::Allocator = heap::Allocator;

struct Args {
    operation: String,
    harness: Harness,
    id: String,
    path: PathBuf,
    root: PathBuf,
    mode: String,
    materialize: bool,
    span: Option<(u64, u64)>,
    escaped: bool,
    chunk_bytes: usize,
    last: usize,
    hash_output: bool,
    hash_detail: bool,
    warm_discover: bool,
}

fn args() -> Result<Args> {
    let mut argv = std::env::args().skip(1);
    let mut positional = Vec::new();
    let (mut root, mut mode, mut materialize) = (None, "lazy".to_string(), false);
    let (mut span, mut escaped, mut chunk_bytes) = (None, false, 64 << 10);
    let mut last = 20;
    let mut hash_output = false;
    let mut hash_detail = false;
    let mut warm_discover = false;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(argv.next().context("--root needs a path")?)),
            "--mode" => mode = argv.next().context("--mode needs lazy|lazy-extra|bulk|full|complete")?,
            "--materialize" => materialize = true,
            "--hash-output" => hash_output = true,
            "--hash-detail" => hash_detail = true,
            "--warm-discover" => warm_discover = true,
            "--span" => {
                let range = argv.next().context("--span needs OFFSET:LENGTH")?;
                let (off, len) = range.split_once(':').context("--span needs OFFSET:LENGTH")?;
                span = Some((off.parse()?, len.parse()?));
            }
            "--escaped" => escaped = true,
            "--chunk-bytes" => chunk_bytes = argv.next().context("--chunk-bytes needs an integer")?.parse()?,
            "--last" => last = argv.next().context("--last needs an integer")?.parse()?,
            "--help" | "-h" => bail!(
                "store_probe OP HARNESS ID PATH [--root DIR] [--mode lazy|lazy-extra|bulk|full|complete] \
                 [--materialize] [--hash-output] [--hash-detail] [--span OFFSET:LENGTH --escaped --chunk-bytes N]\n\
                 OP: baseline|discard|count|collect|window|affordances|affordance-read|doctor|hash|discover|events|events-collected|chunks|resolve|resolved-len\n\
                 HARNESS: claude|codex|gemini|openclaw|continue|lmstudio|cline|kimi|kimi-code|grok|devin|opensession|chatgpt-export|claude-export; use scratch CLUSTERVISION_HOME"
            ),
            s if s.starts_with('-') => bail!("unknown option {s}"),
            _ => positional.push(arg),
        }
    }
    if positional.len() != 4 {
        bail!("expected OP HARNESS ID PATH; see --help");
    }
    let path = std::fs::canonicalize(&positional[3]).context("input path")?;
    let root = root.unwrap_or_else(|| path.parent().unwrap_or(Path::new("/")).to_path_buf());
    let root = std::fs::canonicalize(root).context("adapter root")?;
    if !matches!(mode.as_str(), "lazy" | "lazy-extra" | "bulk" | "full" | "complete") {
        bail!("unknown mode {mode}");
    }
    if chunk_bytes == 0 {
        bail!("--chunk-bytes must be positive");
    }
    if hash_detail && positional[0] != "hash" {
        bail!("--hash-detail applies only to the hash operation");
    }
    Ok(Args {
        operation: positional[0].clone(),
        harness: Harness::parse(&positional[1]).context("unknown harness")?,
        id: positional[2].clone(),
        path,
        root,
        mode,
        materialize,
        span,
        escaped,
        chunk_bytes,
        last,
        hash_output,
        hash_detail,
        warm_discover,
    })
}

fn options(mode: &str) -> ParseOptions {
    match mode {
        "bulk" => ParseOptions::bulk(),
        "full" => ParseOptions::full(),
        "complete" => ParseOptions::complete(),
        "lazy-extra" => ParseOptions::lazy_extra(),
        _ => ParseOptions::lazy(),
    }
}

fn adapter(a: &Args) -> Result<Box<dyn Adapter>> {
    Ok(match a.harness {
        Harness::Claude => Box::new(cv_core::harness::claude::Claude::with_config_dir(&a.root)),
        Harness::Codex => Box::new(cv_core::harness::codex::Codex::with_home(&a.root)),
        Harness::Gemini => Box::new(cv_core::harness::gemini::Gemini::with_home(&a.root)),
        Harness::OpenClaw => Box::new(cv_core::harness::openclaw::OpenClaw::with_state_dir(&a.root)),
        Harness::Continue => Box::new(cv_core::harness::continuedev::Continue::with_root(&a.root)),
        Harness::LmStudio => Box::new(cv_core::harness::lmstudio::LmStudio::with_root(&a.root)),
        Harness::Cline => Box::new(cv_core::harness::cline::Cline::with_home(&a.root)),
        Harness::Kimi => Box::new(cv_core::harness::kimi::Kimi::with_home(&a.root)),
        Harness::KimiCode => Box::new(cv_core::harness::kimi_code::KimiCode::with_home(&a.root)),
        Harness::Grok => Box::new(cv_core::harness::grok::Grok::with_home(&a.root)),
        Harness::OpenSession => Box::new(cv_core::harness::opensession::OpenSession::with_roots(vec![a
            .root
            .clone()])),
        Harness::ChatGptExport => Box::new(cv_core::harness::export::ChatGptExport::with_roots(vec![a
            .root
            .clone()])),
        Harness::ClaudeExport => Box::new(cv_core::harness::export::ClaudeExport::with_roots(vec![a.root.clone()])),
        #[cfg(feature = "sqlite")]
        Harness::Devin => Box::new(cv_core::harness::devin::Devin::with_root(&a.root)),
        #[cfg(feature = "sqlite")]
        Harness::Zed => Box::new(cv_core::harness::zed::Zed::with_root(&a.root)),
        _ => {
            bail!("unsupported probe harness or disabled feature; use explicit-root constructors when extending it")
        }
    })
}

// Sort owned maps in place: deterministic JSON without cloning giant payloads into Value trees.
fn sort_map(map: &mut Map<String, Value>) {
    map.sort_keys();
    for v in map.values_mut() {
        match v {
            Value::Object(m) => sort_map(m),
            Value::Array(a) => sort_values(a),
            _ => {}
        }
    }
}

fn sort_values(values: &mut [Value]) {
    for v in values {
        match v {
            Value::Object(m) => sort_map(m),
            Value::Array(a) => sort_values(a),
            _ => {}
        }
    }
}

fn canonical_message(m: &mut Message, resolver: &cv_core::Resolver) {
    m.materialize(resolver);
    sort_map(&mut m.extra);
    for b in &mut m.content {
        match b {
            Block::ToolUse { input, .. } => sort_values(std::slice::from_mut(input)),
            Block::ToolResult { details: Some(d), .. } => sort_values(std::slice::from_mut(d)),
            _ => {}
        }
    }
}

#[derive(Default)]
struct DigestWriter {
    digest: cv_core::digest::Sha256,
    bytes: u64,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.digest.update(bytes);
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Outcome {
    messages: usize,
    detail: Value,
    // Keep actual collection alive until the stage's heap snapshot is taken.
    session: Option<Session>,
    rows: Vec<cv_core::affordance::Affordance>,
    resolved: Option<String>,
}

fn affordance_detail(rows: &[cv_core::affordance::Affordance], hash_output: bool) -> Result<Value> {
    if !hash_output {
        return Ok(json!({ "affordances": rows.len() }));
    }
    let mut digest = DigestWriter::default();
    for row in rows {
        serde_json::to_writer(&mut digest, row)?;
        digest.write_all(b"\n")?;
    }
    Ok(
        json!({ "affordances": rows.len(), "affordances_sha256": digest.digest.finish_hex(),
               "serialized_affordance_bytes": digest.bytes }),
    )
}

fn run(a: &Args, adapter: &dyn Adapter, r: &SessionRef) -> Result<Outcome> {
    let opts = options(&a.mode);
    let mut out = Outcome::default();
    match a.operation.as_str() {
        "baseline" => {}
        "discover" => {
            let mut refs = adapter.discover()?;
            out.detail = json!({ "discovered_sessions": refs.len() });
            if a.hash_output {
                refs.sort_by(|a, b| (&a.id, &a.path).cmp(&(&b.id, &b.path)));
                let mut digest = DigestWriter::default();
                for r in &refs {
                    serde_json::to_writer(&mut digest, r)?;
                    digest.write_all(b"\n")?;
                }
                out.detail = json!({ "discovered_sessions": refs.len(), "discovery_sha256": digest.digest.finish_hex(),
                                 "serialized_ref_bytes": digest.bytes });
            }
        }
        "events" | "events-collected" => {
            let source = cv_core::source::Probe::default().snapshot(r);
            let mut count = |_m: Message| {
                out.messages += 1;
                Flow::Continue
            };
            let (events, temporary_bytes) = if a.operation == "events" {
                let mut sink = cv_core::events::SpoolingSink::new(r.cwd.clone());
                adapter.stream(r, &opts, &mut cv_core::TeeSink::new(&mut count, &mut sink))?;
                let source = source.with_reads(sink.inner().dependencies());
                let spool = sink.into_spool()?;
                let events = spool.len();
                let temporary_bytes = spool.temporary_bytes()?;
                cv_core::events::record_spooled_with_source(r, 0, spool, &source, &Default::default())?;
                (events, temporary_bytes)
            } else {
                let mut sink = cv_core::events::EventSink::new(r.cwd.clone());
                adapter.stream(r, &opts, &mut cv_core::TeeSink::new(&mut count, &mut sink))?;
                let source = source.with_reads(sink.dependencies());
                let events = sink.into_events();
                cv_core::events::record_from_with_source(r, 0, &events, &source, &Default::default());
                (events.len(), 0)
            };
            out.detail = json!({ "events": events, "temporary_bytes_written": temporary_bytes,
                               "temporary_bytes_read": temporary_bytes });
        }
        "discard" | "count" => {
            let mut counter = cv_core::affordance::Counter::new(r);
            let mut sink = |m: Message| {
                out.messages += 1;
                if a.operation == "count" {
                    counter.note(&m);
                }
                Flow::Continue
            };
            let s = adapter.stream(r, &opts, &mut sink)?;
            if a.operation == "count" {
                out.detail = serde_json::to_value(counter.finish(&s, &adapter.store_affordances(r)))?;
            }
        }
        "doctor" => {
            let mut report = cv_core::doctor::Report::default();
            let mut pass = report.session(Some(r.path.clone()));
            adapter.stream(r, &opts, &mut pass)?;
            pass.finish();
            out.messages = report.messages as usize;
            out.detail = report.to_json();
        }
        "collect" | "affordances" => {
            let opts = if a.operation == "affordances" {
                ParseOptions::full()
            } else {
                opts
            };
            let mut sink = cv_core::CollectSink::default();
            let mut s = adapter.stream(r, &opts, &mut sink)?;
            s.messages = sink.messages;
            if a.materialize || a.operation == "affordances" {
                s.materialize();
            }
            out.messages = s.messages.len();
            if a.operation == "affordances" {
                out.rows = cv_core::affordance::of_session(&s);
                out.rows.extend(adapter.store_affordances(r));
                out.detail = affordance_detail(&out.rows, a.hash_output)?;
            }
            out.session = Some(s);
        }
        "window" => {
            let mut last = std::collections::VecDeque::new();
            let mut sink = |m: Message| {
                out.messages += 1;
                if a.last > 0 {
                    if last.len() == a.last {
                        last.pop_front();
                    }
                    last.push_back(m);
                }
                Flow::Continue
            };
            let mut s = adapter.stream(r, &opts, &mut sink)?;
            s.messages = last.into_iter().collect();
            if a.materialize {
                s.materialize();
            }
            out.detail = json!({ "retained_messages": s.messages.len() });
            out.session = Some(s);
        }
        "affordance-read" => {
            out.rows = cv_core::affordance::read(adapter, r)?;
            out.detail = affordance_detail(&out.rows, a.hash_output)?;
            out.detail["message_callbacks"] = json!("not observed");
        }
        "hash" => {
            let resolver = cv_core::Resolver::new(Some(r.path.clone()));
            let mut messages = DigestWriter::default();
            let mut error = None;
            let mut fingerprints = Vec::new();
            let mut sink = |mut m: Message| {
                out.messages += 1;
                canonical_message(&mut m, &resolver);
                if let Err(e) = serde_json::to_writer(&mut messages, &m) {
                    error = Some(e);
                    return Flow::Stop;
                }
                messages.write_all(b"\n").expect("digest writer");
                if a.hash_detail {
                    let mut one = DigestWriter::default();
                    serde_json::to_writer(&mut one, &m).expect("serializable message");
                    one.write_all(b"\n").expect("digest writer");
                    fingerprints.push(json!({ "index_zero_based": out.messages - 1,
                                              "sha256": one.digest.finish_hex(), "serialized_bytes": one.bytes }));
                }
                Flow::Continue
            };
            let mut s = adapter.stream(r, &opts, &mut sink)?;
            if let Some(e) = error {
                return Err(e.into());
            }
            s.messages.clear();
            s.source_path = None; // Transport location, not interpreted session content.
            sort_map(&mut s.extra);
            let mut meta = DigestWriter::default();
            serde_json::to_writer(&mut meta, &s)?;
            out.detail = json!({
                "algorithm": "sha256", "canonical_json_version": 1,
                "messages_sha256": messages.digest.finish_hex(),
                "metadata_sha256": meta.digest.finish_hex(),
                "serialized_message_bytes": messages.bytes,
            });
            if a.hash_detail {
                out.detail["message_fingerprints"] = json!(fingerprints);
                out.detail["metadata"] = serde_json::to_value(&s)?;
            }
        }
        "chunks" | "resolve" | "resolved-len" => {
            let (offset, len) = a
                .span
                .context("chunks/resolve/resolved-len requires --span OFFSET:LENGTH")?;
            if offset
                .checked_add(len)
                .is_none_or(|end| end > std::fs::metadata(&r.path).map_or(0, |m| m.len()))
            {
                bail!("span is outside the source file");
            }
            let span = cv_core::Span {
                source: None,
                offset,
                len,
                escaped: a.escaped,
            };
            let resolver = cv_core::Resolver::new(Some(r.path.clone()));
            if a.operation == "resolved-len" {
                out.detail = json!({ "decoded_bytes": resolver.resolved_len(&span) });
                return Ok(out);
            }
            if a.operation == "resolve" {
                let text = resolver.resolve(&span).into_owned();
                out.detail = json!({ "decoded_bytes": text.len() });
                if a.hash_output {
                    let mut digest = DigestWriter::default();
                    digest.write_all(text.as_bytes())?;
                    out.detail["decoded_sha256"] = json!(digest.digest.finish_hex());
                }
                out.resolved = Some(text);
                return Ok(out);
            }
            let (mut chunks, mut bytes, mut max) = (0usize, 0usize, 0usize);
            resolver.for_each_chunk(&span, a.chunk_bytes, |text| {
                chunks += 1;
                bytes += text.len();
                max = max.max(text.len());
                std::hint::black_box(text);
            });
            out.detail = json!({ "chunks": chunks, "decoded_bytes": bytes, "largest_chunk_bytes": max });
        }
        op => bail!("unknown operation {op}"),
    }
    Ok(out)
}

fn main() -> Result<()> {
    let a = args()?;
    // Production affordance::read can persist counts, and discovery cache initialization opens
    // a writable catalog. Keep composed operations faithful with an explicitly isolated catalog.
    if a.warm_discover || matches!(
        a.operation.as_str(),
        "affordance-read" | "discover" | "events" | "events-collected"
    ) {
        let home = std::fs::canonicalize(
            std::env::var_os("CLUSTERVISION_HOME").context("this operation requires scratch CLUSTERVISION_HOME")?,
        )
        .context("scratch CLUSTERVISION_HOME must exist")?;
        if let Some(real) = dirs::home_dir().map(|p| p.join(".clustervision")) {
            let real = std::fs::canonicalize(&real).unwrap_or(real);
            if home.starts_with(real) {
                bail!("CLUSTERVISION_HOME must be a scratch catalog, not the real store");
            }
        }
    }
    let adapter = adapter(&a)?;
    // Warm native descriptor/discovery caches in this process, then drop returned refs before
    // the stage. Baseline heap includes any cache state that survives setup; source buffers
    // dropped by discovery are deliberately outside a warm selected-read measurement.
    let setup_discovered_sessions = if a.warm_discover {
        Some(adapter.discover()?.len())
    } else {
        None
    };
    let r = SessionRef::new(&a.id, a.harness, &a.path);
    let physical_bytes = std::fs::metadata(&a.path)?.len();
    #[cfg(feature = "probe-heap")]
    let baseline = heap::begin();
    let start = Instant::now();
    let out = run(&a, adapter.as_ref(), &r)?;
    let seconds = start.elapsed().as_secs_f64();
    #[cfg(feature = "probe-heap")]
    let requested_rust_heap = Some(heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let requested_rust_heap: Option<()> = None;
    println!(
        "{}",
        json!({
            "operation": a.operation, "harness": a.harness.as_str(), "id": a.id,
            "path": a.path, "root": a.root, "requested_mode": a.mode,
            "effective_mode": match a.operation.as_str() {
                "affordances" => "full",
                "affordance-read" => "production-defined",
                "baseline" | "discover" | "chunks" | "resolve" | "resolved-len" => "not-applicable",
                _ => &a.mode,
            },
            "materialize": if a.operation == "affordance-read" { Value::Null }
                           else { json!(a.materialize || matches!(a.operation.as_str(), "affordances" | "hash" | "resolve")) },
            "physical_input_bytes": physical_bytes, "traversed_bytes": null,
            "build_features": { "mmap": cfg!(feature = "mmap"), "probe_heap": cfg!(feature = "probe-heap") },
            "hash_output": a.hash_output,
            "hash_detail": a.hash_detail,
            "warm_discover": a.warm_discover,
            "setup_discovered_sessions": setup_discovered_sessions,
            "wall_seconds": seconds, "messages": out.messages,
            "requested_rust_heap": requested_rust_heap, "detail": out.detail,
        })
    );
    std::hint::black_box((&out.session, &out.rows, &out.resolved));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_message(mut m: Message, resolver: &cv_core::Resolver) -> String {
        canonical_message(&mut m, resolver);
        let mut writer = DigestWriter::default();
        serde_json::to_writer(&mut writer, &m).unwrap();
        writer.digest.finish_hex()
    }

    #[test]
    fn digest_resolves_escaped_spans_and_ignores_json_map_insertion_order() {
        let path = std::env::temp_dir().join(format!("cv-probe-hash-{}.json", uuid::Uuid::new_v4()));
        let text = "escaped\n\"quote\"\\☃".repeat(1024);
        let quoted = serde_json::to_string(&text).unwrap();
        std::fs::write(&path, &quoted).unwrap();
        let resolver = cv_core::Resolver::new(Some(path.clone()));
        let mut inline = Message::new(cv_core::Role::Assistant);
        inline.content.push(Block::Text { text: text.into() });
        inline.content.push(Block::ToolUse {
            id: "call".into(),
            name: "tool".into(),
            namespace: None,
            input: serde_json::from_str(r#"{"z":{"b":2,"a":1},"a":0}"#).unwrap(),
        });
        let mut lazy = inline.clone();
        lazy.content[0] = Block::Text {
            text: cv_core::Text::Span(cv_core::Span {
                source: None,
                offset: 1,
                len: quoted.len() as u64 - 2,
                escaped: true,
            }),
        };
        if let Block::ToolUse { input, .. } = &mut lazy.content[1] {
            *input = serde_json::from_str(r#"{"a":0,"z":{"a":1,"b":2}}"#).unwrap();
        }
        assert_eq!(hash_message(inline, &resolver), hash_message(lazy, &resolver));
        std::fs::remove_file(path).unwrap();
    }
}
