//! Per-harness adapters: discover sessions on disk, parse them into the IR, and (optionally)
//! emit the IR back into a harness's native format for cross-harness porting.

use crate::ir::{Harness, Session, SessionRef};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io::BufRead;
use std::path::PathBuf;

pub mod claude;
/// Claude Code `Workflow`-tool runs (phase tree + script), a first-class layer over the
/// `subagents/workflows/` agent tier that [`claude`] walks.
pub mod claude_workflow;
pub mod cline;
pub mod codex;
pub mod continuedev;
#[cfg(feature = "sqlite")]
pub mod cursor;
#[cfg(feature = "sqlite")]
pub mod devin;
pub mod export;
pub mod gemini;
#[cfg(feature = "sqlite")]
pub mod goose;
pub mod grok;
pub mod harmony;
#[cfg(feature = "sqlite")]
pub mod hermes;
pub mod kimi;
pub mod kimi_code;
pub mod lmstudio;
pub mod openclaw;
pub mod opencode;
/// cv's own **OpenSession** interchange documents (`docs/OPENSESSION.md`) — the format
/// `cv export --format json` writes — read back into the IR.
pub mod opensession;
pub mod qwen;
pub mod roo;
pub mod store;
pub mod wire;
#[cfg(feature = "sqlite")]
pub mod zed;

/// Parse an RFC3339 timestamp into UTC — the timestamp shape nearly every harness writes. Shared
/// here so adapters don't each carry a copy.
pub(crate) fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Flexible timestamp from a JSON value: an RFC3339 string, or an epoch number as
/// [`ts_from_epoch`] reads it.
pub(crate) fn ts_from_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_ts(s);
    }
    ts_from_epoch(v.as_f64()?)
}

/// An epoch number → UTC. The ms/s split is a heuristic — magnitudes above 10^12 are
/// **milliseconds** (10^12 seconds is the year 33658; 10^12 ms is Sep 2001, so any modern harness
/// timestamp disambiguates); anything else is **seconds**, and a fractional second keeps its
/// millisecond part. Zero and negative values are "unset", not 1970: every SQLite store defaults
/// an absent time column to 0, and no harness has a transcript older than itself. The one epoch
/// reader for every adapter — a SQLite column, a JSON number, a float of seconds all come here.
pub(crate) fn ts_from_epoch(n: impl EpochNumber) -> Option<DateTime<Utc>> {
    let n = n.to_f64();
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let millis = if n > 1_000_000_000_000.0 { n } else { n * 1000.0 };
    DateTime::from_timestamp_millis(millis as i64)
}

/// The numeric column/field types an epoch timestamp arrives as (see [`ts_from_epoch`]).
pub(crate) trait EpochNumber {
    fn to_f64(self) -> f64;
}
impl EpochNumber for f64 {
    fn to_f64(self) -> f64 {
        self
    }
}
impl EpochNumber for i64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}
impl EpochNumber for u64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}

/// Drive a tolerant JSONL pass over `reader`: blank and malformed lines are skipped (never fail a
/// session over one bad record), every parsed record goes to `f`, and `Flow::Stop` ends the pass
/// early. **Not** for offset-tracking paths (the claude/codex lazy-span readers need each line's
/// byte offset and keep their own `read_until`/mmap loops).
///
/// Returns how many non-blank lines were skipped as unreadable (corrupt JSON or an undecodable
/// read) before the pass ended — adapters surface a non-zero count via
/// [`note_skipped_lines`] so silent corruption is at least visible in the session metadata.
pub(crate) fn for_each_json_line<R: BufRead>(reader: R, mut f: impl FnMut(Value) -> Flow) -> u64 {
    let mut skipped = 0u64;
    for line in reader.lines() {
        // A read error on one line (rare: invalid UTF-8 chunk) shouldn't abort the whole session.
        let Ok(line) = line else {
            skipped += 1;
            continue;
        };
        if each_json_line_inner(&line, &mut skipped, &mut f) == Flow::Stop {
            break;
        }
    }
    skipped
}

/// [`for_each_json_line`] over already-read text (no per-line allocation). Returns the skip count.
pub(crate) fn for_each_json_line_str(text: &str, mut f: impl FnMut(Value) -> Flow) -> u64 {
    let mut skipped = 0u64;
    for line in text.lines() {
        if each_json_line_inner(line, &mut skipped, &mut f) == Flow::Stop {
            break;
        }
    }
    skipped
}

fn each_json_line_inner(line: &str, skipped: &mut u64, f: &mut impl FnMut(Value) -> Flow) -> Flow {
    let line = line.trim();
    if line.is_empty() {
        return Flow::Continue;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        *skipped += 1;
        return Flow::Continue; // tolerate the occasional corrupt line
    };
    f(v)
}

/// The namespace in `Session::extra` / `Message::extra` for cv's OWN bookkeeping — facts about the
/// *parse*, not about the harness. It sits beside the per-harness bags
/// ([`Session::harness_extra_mut`](crate::ir::Session::harness_extra_mut)) and can never collide
/// with one: `Harness::parse("cv")` is `None`.
///
/// Why a namespace and not a flat key (`docs/INTERFACE-V2.md` §4): the flat-key exception list is a
/// list that GROWS — every new cross-harness diagnostic would have to be added to it and to every
/// consumer that walks `extra` (see [`emit::is_internal_extra_key`](crate::emit)). A namespace does
/// not. So §4's exceptions stay exactly what they are today, both on `Message` only:
/// [`claude::CARRIER_KEY`] (`_record`, the verbatim source record under `ParseOptions::complete`)
/// and [`crate::offsets::OFFSET_KEY`] (`cv_byte_offset`, written per message on the lazy-offset hot
/// path, where a nested map per message would be pure allocation). `Session::extra` has **no** flat
/// exception: every top-level key there is a namespace object — a harness name or `"cv"`.
///
/// The namespace itself is [`crate::ir::CV_NAMESPACE`], reached with `Session::cv_extra_mut`.
///
/// Record `skipped` unreadable transcript lines under `extra["cv"]["skipped_lines"]` (only when
/// non-zero, so clean sessions stay byte-identical). The count is a *lower bound* when a pass
/// stopped early. It is a parse diagnostic shared by every adapter, so it lives in cv's own
/// namespace ([`crate::ir::CV_NAMESPACE`]) rather than in any harness bag.
pub(crate) fn note_skipped_lines(s: &mut Session, skipped: u64) {
    if skipped > 0 {
        s.cv_extra_mut()
            .insert("skipped_lines".into(), Value::Number(skipped.into()));
    }
}

/// Result of emitting an IR session into a harness's native on-disk format.
#[derive(Debug, Clone)]
pub struct EmitResult {
    /// Primary file (or directory) written.
    pub path: PathBuf,
    /// The new native session id in the target harness.
    pub new_id: String,
    /// A human hint for how to resume it, if known (e.g. `claude --resume <id>`).
    pub resume_hint: Option<String>,
}

/// A harness adapter: where a harness keeps its sessions ([`roots`](Adapter::roots)), how to list
/// them cheaply ([`discover`](Adapter::discover)), and how to read one as a message stream
/// ([`stream`](Adapter::stream)). Implementors live in submodules and are registered in [`all`].
/// A whole-session parse is not a fourth method: it is [`stream::collect`](crate::stream::collect)
/// over `stream`, spelled `adapter.parse(&r)` on a `dyn Adapter`, so the streaming and the
/// whole-session readers cannot diverge.
///
/// `Send + Sync` so [`crate::discover_all`] can fan discovery across adapters in parallel. Every
/// adapter holds only path data (connections are opened per call), so this bound is always satisfied.
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// Every on-disk root this adapter reads sessions from (resolved against `$HOME`, existing
    /// ones only), the primary one first. Always **directories** — for a SQLite-backed store, the
    /// dir holding the database — so an emitter given the first as `out_dir` writes beside what the
    /// reader reads, and the catalog's freshness probe can watch each one (the database files are
    /// its per-session file-watches), re-discovering the harness when a root appears that it holds
    /// no watch for. Empty when the harness is not installed.
    fn roots(&self) -> Vec<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them.
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// Stream a session's messages into `sink` one at a time, dropping each before the next, so
    /// peak memory is O(largest message) rather than O(transcript). Returns the session's metadata
    /// (id/cwd/title/model/git/timestamps) with an **empty** `messages` vec — the messages went to
    /// the sink. `opts` controls how much of each message is materialized (see [`ParseOptions`]).
    /// See [`claude`](crate::harness::claude) for the reference; a store that can only be read
    /// whole hands its parsed session to [`crate::stream::replay`].
    fn stream(&self, r: &SessionRef, opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session>;
}

/// `parse` on a concrete adapter (`Claude::detect().parse(&r)`): the same
/// [`stream::collect`](crate::stream::collect), by a blanket impl no adapter can override.
pub trait Parse {
    fn parse(&self, r: &SessionRef) -> Result<Session>;
}

impl<T: Adapter> Parse for T {
    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }
}

impl dyn Adapter + '_ {
    /// Fully parse one discovered session into the IR: [`stream::collect`](crate::stream::collect).
    pub fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    /// The primary root (the first of [`roots`](Adapter::roots)), where an emitter writes.
    pub fn root(&self) -> Option<PathBuf> {
        self.roots().into_iter().next()
    }
}
// NOTE: emitting (being a *conversion target*) is deliberately NOT part of this trait. The single
// emit registry is `crate::emit::emitter_for` — an adapter becomes a target by exposing a
// `pub fn emit(&Session, &Path, &EmitOptions) -> Result<EmitResult>` and registering it there.
// (A previous `Adapter::emit`/`can_emit` pair had zero call sites and drifted out of sync with the
// real dispatcher, with `can_emit` returning `false` for supported targets.)

/// All registered adapters.
pub fn all() -> Vec<Box<dyn Adapter>> {
    let mut adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(claude::Claude::detect()),
        Box::new(codex::Codex::detect()),
        Box::new(grok::Grok::detect()),
        Box::new(opencode::OpenCode::detect()),
        Box::new(gemini::Gemini::detect()),
        Box::new(openclaw::OpenClaw::detect()),
        Box::new(kimi::Kimi::detect()),
        Box::new(kimi_code::KimiCode::detect()),
        Box::new(qwen::Qwen::detect()),
        Box::new(lmstudio::LmStudio::detect()),
        Box::new(cline::Cline::detect()),
        Box::new(roo::Roo::detect()),
        Box::new(continuedev::Continue::detect()),
        Box::new(export::ChatGptExport),
        Box::new(export::ClaudeExport),
        Box::new(opensession::OpenSession),
    ];
    #[cfg(feature = "sqlite")]
    {
        adapters.push(Box::new(hermes::Hermes::detect()));
        adapters.push(Box::new(cursor::Cursor::detect()));
        adapters.push(Box::new(goose::Goose::detect()));
        adapters.push(Box::new(zed::Zed::detect()));
        adapters.push(Box::new(devin::Devin::detect()));
    }
    adapters
}

/// The adapter for a specific harness, if registered.
pub fn for_harness(h: Harness) -> Option<Box<dyn Adapter>> {
    all().into_iter().find(|a| a.harness() == h)
}

/// Assert the IR-v2 nesting invariant on a parsed session (`docs/INTERFACE-V2.md` §4), so every
/// adapter's tests state the rule the same way instead of each spelling out its own key list.
///
/// Every top-level key in `Session::extra` and in each `Message::extra` must be a **namespace whose
/// value is an object** — a canonical harness name (`Harness::as_str`, so an alias like `cc` fails)
/// or [`crate::ir::CV_NAMESPACE`]. Exactly two flat keys are allowed, both message-level and both cv's own
/// streaming bookkeeping rather than harness facts: [`claude::CARRIER_KEY`] (`_record`, the verbatim
/// source record under [`ParseOptions::complete`]) and [`crate::offsets::OFFSET_KEY`]
/// (`cv_byte_offset`). `Session::extra` has no flat exception at all.
#[cfg(test)]
pub(crate) fn assert_no_flat_keys(s: &Session) {
    let namespace = |k: &str| Harness::parse(k).is_some_and(|h| h.as_str() == k) || k == crate::ir::CV_NAMESPACE;
    for (k, v) in &s.extra {
        assert!(namespace(k), "flat session extra key {k:?} on a {} session", s.harness);
        assert!(v.is_object(), "namespace {k:?} must hold an object, got {v}");
    }
    for m in &s.messages {
        for (k, v) in &m.extra {
            if k == claude::CARRIER_KEY || k == crate::offsets::OFFSET_KEY {
                continue; // the two documented flat exceptions
            }
            assert!(
                namespace(k),
                "flat message extra key {k:?} on {:?} (a {} session)",
                m.id,
                s.harness
            );
            assert!(v.is_object(), "namespace {k:?} must hold an object, got {v}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_from_value_handles_all_shapes() {
        // RFC3339 string.
        let s = ts_from_value(&serde_json::json!("2026-01-02T03:04:05Z")).unwrap();
        assert_eq!(s.timestamp(), 1_767_323_045);
        // Epoch milliseconds (> 10^12).
        let ms = ts_from_value(&serde_json::json!(1_767_323_045_000i64)).unwrap();
        assert_eq!(ms, s);
        // Epoch seconds (<= 10^12), integer and float; a float keeps its millisecond part.
        assert_eq!(ts_from_value(&serde_json::json!(1_767_323_045i64)).unwrap(), s);
        assert_eq!(
            ts_from_value(&serde_json::json!(1_767_323_045.9f64))
                .unwrap()
                .timestamp_millis(),
            1_767_323_045_900
        );
        // Junk, and the "unset" column values no store means as 1970.
        assert!(ts_from_value(&serde_json::json!("not a time")).is_none());
        assert!(ts_from_value(&serde_json::json!(null)).is_none());
        assert!(ts_from_value(&serde_json::json!({"t": 1})).is_none());
        assert!(ts_from_value(&serde_json::json!(0)).is_none());
        assert!(ts_from_epoch(-5i64).is_none());
        assert!(ts_from_epoch(f64::NAN).is_none());
        assert_eq!(ts_from_epoch(1_767_323_045_000u64).unwrap(), s);
    }

    #[test]
    fn for_each_json_line_skips_junk_and_stops() {
        let text = "{\"n\":1}\n\n   \nnot json\n{\"n\":2}\n{\"n\":3}";
        let mut seen = Vec::new();
        let skipped = for_each_json_line_str(text, |v| {
            let n = v.get("n").and_then(Value::as_i64).unwrap_or(0);
            seen.push(n);
            if n == 2 {
                return Flow::Stop;
            }
            Flow::Continue
        });
        assert_eq!(seen, vec![1, 2], "blank/junk skipped, Stop honored");
        assert_eq!(skipped, 1, "the `not json` line counts; blank lines don't");

        // The reader-driven variant sees the same records and the same skip count.
        let mut seen2 = Vec::new();
        let skipped2 = for_each_json_line(std::io::Cursor::new(text.as_bytes()), |v| {
            seen2.push(v.get("n").and_then(Value::as_i64).unwrap_or(0));
            Flow::Continue
        });
        assert_eq!(seen2, vec![1, 2, 3]);
        assert_eq!(skipped2, 1);

        // note_skipped_lines: zero leaves `extra` untouched (clean sessions stay byte-identical).
        let mut s = Session::new("x", Harness::Codex);
        note_skipped_lines(&mut s, 0);
        assert!(s.extra.is_empty());
        note_skipped_lines(&mut s, 3);
        // cv's own parse diagnostic lives in cv's namespace, never flat on the session
        // (`CV_NAMESPACE`; `docs/INTERFACE-V2.md` §4).
        let noted = |s: &Session| {
            s.extra
                .get(crate::ir::CV_NAMESPACE)
                .and_then(|v| v.get("skipped_lines"))
                .and_then(Value::as_u64)
        };
        assert_eq!(noted(&s), Some(3));
        assert!(!s.extra.contains_key("skipped_lines"), "never a flat session key");
        // Re-noting replaces rather than nesting again, and adds no second top-level key.
        note_skipped_lines(&mut s, 5);
        assert_eq!(noted(&s), Some(5));
        assert_eq!(s.extra.keys().collect::<Vec<_>>(), ["cv"]);
    }

    /// Every top-level key a session's `extra` may carry is a NAMESPACE object — a harness name or
    /// [`crate::ir::CV_NAMESPACE`]. `Session::extra` has no flat exception at all (the two documented flat
    /// keys, `_record` and `cv_byte_offset`, are message-level). Asserted here because
    /// `note_skipped_lines` is the one writer every adapter shares.
    #[test]
    fn cv_namespace_never_collides_with_a_harness_name() {
        assert!(
            Harness::parse(crate::ir::CV_NAMESPACE).is_none(),
            "`cv` must never be parseable as a harness, or its bag would shadow one"
        );
        assert!(!Harness::ALL.iter().any(|h| h.as_str() == crate::ir::CV_NAMESPACE));
    }
}
