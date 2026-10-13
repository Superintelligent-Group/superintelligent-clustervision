//! `cv prune` — a *custom, lossless compaction* of a Claude Code session into a **new, resumable**
//! session. Instead of handing a full context window to the summarizer (which rewrites your history
//! into a lossy paragraph), prune snips the **bulky old tool payloads** — large file reads, command
//! logs, base64 screenshots — out of the conversation and into a sidecar, leaving a small `[PRUNED
//! id=…]` marker in their place. Your prompts and the chronological flow are preserved; the most
//! recent turns are left untouched so the model stays sharp on what it's doing right now.
//!
//! The approach is adapted from the validated `flatten-mcp` flattener: we operate on the **raw
//! JSONL lines**, never on our IR — so every Claude-specific field survives a round-trip and the
//! result resumes cleanly with `claude --resume <new-id>`. An untouched line is byte-identical to
//! its source except for the session id ([`crate::restamp_line`]'s byte surgery); a line with a
//! snipped payload goes through serde_json, which may normalize key order and whitespace, with the
//! JSON **values** — and the payloads stashed in the sidecar — preserved verbatim. What we add on
//! top of the flattener: a fresh session id stamped across every line (so it's a genuinely new
//! session, not an in-place rewrite), a copy of the session's sidecar resources (`subagents/`,
//! `workflows/`) under the new id, and a **keep-last** window so only *old* payloads are snipped.
//!
//! Two payload sites are handled, exactly as Claude Code records them:
//!  1. `tool_result` blocks inside a user message's `message.content` — the bytes the model re-reads
//!     every turn (this is what actually costs context tokens).
//!  2. the top-level `toolUseResult` mirror Claude keeps per result line — disk-only (the model never
//!     sees it), but snipping it shrinks the file and our parse.
//!
//! Lines with `isSidechain: true` are sub-agent turns whose token usage belongs to a *different*
//! context window: they ride along as content (kept, dropped, or snipped by position) but are never
//! counted as conversational turns, never contribute usage to `--window` sizing or `--revive`
//! arithmetic, and are never chosen as the re-root of a windowed tail.
//!
//! Claude Code's synthetic assistant notices (`model: "<synthetic>"` — "Prompt is too long" and
//! friends) are not turns either: they ride along as records but never count toward `--keep-last`,
//! `--range` indices or `--window` sizing. Model-visible `attachment` records (those with
//! `rendered[]`) DO count toward byte estimates — that text is in the prompt.

use crate::harness::store::{
    bytes::{ByteSource, ByteStage},
    jsonl::ranges::{self, Range},
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Marker prefix stamped where a payload used to be; carries enough to retrieve the original. The
/// sidecar's public contract: `cv cat` keys retrieval on the id after it.
pub const MARKER_PREFIX: &str = "[PRUNED id=";
// NOTE: `--declassify` is a token-AGNOSTIC primitive. cv-core ships NO built-in token list — the
// terms are always caller-supplied (CLI `--declassify-tokens` / `--declassify-tokens-file`, i.e.
// external config/data). This keeps cv itself free of any domain "shitlist" (Ember, PR #9): a
// list of security badwords baked into the source would (a) be un-adaptable and (b) trip the very
// classifier this feature exists to dodge whenever an agent reads cv's own code. `declassify` with
// no tokens is a safe no-op. Composing a security-declassify tool = supply your token file.
/// Suffix distinguishing a flattened `toolUseResult` mirror entry from its `content` sibling.
const TUR_SUFFIX: &str = "#tur";
/// Claude tokenizer ≈ 3.3–3.7 B/tok. Prune estimates over raw records (it never parses to the IR),
/// so it walks `message.content` itself, with [`crate::ir::est_tokens_bytes`]'s arithmetic and
/// [`crate::ir::IMAGE_TOKENS`] per image — the same numbers the IR-side estimator produces.
const TEXT_BYTES_PER_TOKEN: f64 = 3.5;
const LARGE_RECORD: u64 = 1024 * 1024;

/// Knobs for [`prune_session`].
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// Snip a tool payload only if it is larger than this many bytes.
    pub min_size: usize,
    /// Keep the last N conversational turns' payloads verbatim (recent context stays sharp).
    pub keep_last: usize,
    /// Hard-drop payloads (no sidecar, irreversible) instead of stashing them for retrieval.
    pub drop: bool,
    /// Also flatten assistant **thinking** blocks (extended reasoning). On a long session the
    /// chain-of-thought often dominates the loaded context, and on resume you rarely need the
    /// verbatim old reasoning — the conclusions are in the assistant's text. Snips every old block
    /// regardless of `min_size` (a signature-only block is ~700 bytes on disk but hundreds of tokens
    /// on the wire). Lossless (stashed in the sidecar like any other payload). Off by default — it
    /// changes more than tool output.
    pub thinking: bool,
    /// Explicit new session id; otherwise a fresh UUIDv4.
    pub new_id: Option<String>,
    /// Also copy the session's `subagents/`/`workflows/` resource dir under the new id. Off by
    /// default — `claude --resume` doesn't need it, and for big sessions it can be hundreds of MB /
    /// thousands of files. Turn on if you want cv's forest features to work on the pruned session.
    pub copy_resources: bool,
    /// After pruning, rewrite the trailing `message.usage` records to an **honest** post-prune token
    /// count. Claude Code's resume gate reads the last turn's recorded `usage` (input + cache — from
    /// its `iterations` array when present, since 2.1.277) as the session's current size *before it
    /// re-sends anything* — so an already-maxed session refuses to resume even when the (now pruned)
    /// content would fit. This recomputes the real size of the loaded window and corrects the stale
    /// number (top level and iterations alike), so the gate lets you back in. Off by default — it
    /// edits recorded metadata, not just content. The source is still never touched (new id only).
    pub revive: bool,
    /// Sliding window: keep only the NEWEST conversational turns whose content sums to **≤ this many
    /// tokens**, dropping older turns entirely (lossy — but the source session is never touched, so it
    /// stays the full backup). The kept tail is re-rooted into a standalone resumable session. If even
    /// the single newest turn exceeds the budget, that one turn is kept anyway and a warning is
    /// reported (see [`PruneResult::warnings`]) — resume may still hit the context wall. Snapping the
    /// tail back to its opening user prompt can also add back a fraction of one turn.
    pub window: Option<u64>,
    /// Explicit message subrange `[start, end)` (turn indices) to keep, dropping everything outside
    /// it. `end = None` ⇒ through the last turn. Like `window` but selected by index, not budget.
    pub keep_range: Option<(usize, Option<usize>)>,
    /// `--declassify`: snip conversational message TEXT whose security-token density is high, so a
    /// resumed session does not carry classifier-tripping PROSE into a fresh (e.g. Fable) seat's context.
    /// Lossless (stashed in the sidecar like any other payload). Complements tool-payload/`--thinking`
    /// snipping — the trigger content lives largely in the conversational prose those passes leave
    /// verbatim. Off by default. Unlike the tool/thinking passes, this does NOT respect `keep_last`: a
    /// trigger span anywhere in the loaded context downgrades the seat, so recent security prose is
    /// snipped too (still retrievable). Benign turns (< `declassify_min_hits` terms) are untouched.
    pub declassify: bool,
    /// Terms (lowercase) counted for `--declassify` density — ALWAYS caller-supplied (no built-in
    /// list; empty ⇒ `--declassify` is a no-op). Matched case-insensitively as substrings.
    pub declassify_tokens: Vec<String>,
    /// Snip a message iff it holds at least this many DISTINCT `declassify_tokens` (default 2).
    pub declassify_min_hits: usize,
    /// Compute and report without writing anything.
    pub dry_run: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        PruneOptions {
            min_size: 2048,
            keep_last: 25,
            drop: false,
            thinking: false,
            new_id: None,
            copy_resources: false,
            revive: false,
            window: None,
            keep_range: None,
            declassify: false,
            declassify_tokens: Vec::new(), // token-agnostic: caller supplies the terms (no baked-in list)
            declassify_min_hits: 2,
            dry_run: false,
        }
    }
}

/// What a prune did (or, in `dry_run`, would do). Serializes as the `cv prune --json` report (the
/// renamed keys are that report's spelling); the three `--revive` details are folded by the CLI
/// into its `revived` object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PruneResult {
    pub source_id: String,
    pub new_id: String,
    pub new_path: PathBuf,
    pub sidecar_path: Option<PathBuf>,
    pub copied_resources: Option<PathBuf>,
    #[serde(rename = "snipped_payloads")]
    pub pruned_count: usize,
    pub image_blocks: usize,
    #[serde(rename = "before_bytes")]
    pub original_size: u64,
    #[serde(rename = "after_bytes")]
    pub new_size: u64,
    #[serde(rename = "tokens_freed")]
    pub est_context_tokens_saved: u64,
    /// `--revive`: how many `message.usage` records were corrected.
    #[serde(skip)]
    pub usage_rewritten: usize,
    /// `--window`/`--range`: conversational turns dropped from the head/tail by the selection (0 if
    /// neither was used). The kept turns form the new session; the source keeps everything.
    pub dropped_turns: usize,
    /// `--window`: the real content-token size of the kept tail, from Claude's recorded `usage`
    /// (the figure the API will report on resume, ± system overhead). `None` for `--range`, or when
    /// the session had no usage records and the byte-estimate fallback was used.
    pub window_real_tokens: Option<u64>,
    /// `--revive`: the honest post-prune context-token figure written (None if revive off / nothing to do).
    #[serde(skip)]
    pub revive_tokens: Option<u64>,
    /// `--revive`: the stale figure it replaced — the largest pre-revive usage total in the loaded window.
    #[serde(skip)]
    pub revive_old_tokens: Option<u64>,
    /// Data-safety warnings (e.g. a `--window` tail that exceeds its budget because a single turn is
    /// bigger than the whole budget). Also echoed to stderr.
    pub warnings: Vec<String>,
    pub dry_run: bool,
}

impl PruneResult {
    pub fn bytes_saved(&self) -> u64 {
        self.original_size.saturating_sub(self.new_size)
    }
}

/// One stashed payload, written as a line of `<new-id>.flat.jsonl` and read back by [`retrieve`].
#[derive(serde::Serialize, serde::Deserialize)]
struct SidecarEntry {
    id: String,
    slot: String, // "content" | "toolUseResult"
    name: String,
    input: Value,
    content: Value, // the ORIGINAL value, verbatim — lossless
    size: usize,
    line_count: usize,
    kind: String, // "text" | "image" | "mixed"
}

/// One sidecar line in the `<stem>.flat.jsonl` format [`retrieve`] (and `cv cat`) reads: the stash
/// format is shared with `cv distill`, which elides whole tool outputs the same way prune snips them.
pub fn sidecar_line(id: &str, name: &str, input: &Value, content: &str) -> String {
    let e = SidecarEntry {
        id: id.to_string(),
        slot: "content".into(),
        name: name.to_string(),
        input: input.clone(),
        content: Value::String(content.to_string()),
        size: content.len(),
        line_count: content.lines().count(),
        kind: "text".into(),
    };
    serde_json::to_string(&e).unwrap_or_default()
}

/// Sub-agent (Task tool) lines carry `isSidechain: true`; their usage belongs to the sub-agent's
/// own context window, not the main thread's.
pub(crate) fn is_sidechain(v: &Value) -> bool {
    v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false)
}

/// Classify a line as a main-thread conversational turn: `Some(is_user)` for a real user/assistant
/// turn, `None` for everything else — bookkeeping records, sidechain (sub-agent) lines, and Claude
/// Code's synthetic assistant notices (see [`is_synthetic_assistant`]), which are never sent to the
/// API and must not shift turn indices or eat `--keep-last` budget.
pub(crate) fn turn_kind(v: &Value) -> Option<bool> {
    if is_sidechain(v) {
        return None;
    }
    match v.get("type").and_then(Value::as_str) {
        Some("user") => Some(true),
        Some("assistant") if !is_synthetic_assistant(v) => Some(false),
        _ => None,
    }
}

/// A main-thread conversational turn (sidechain lines and synthetic notices don't count).
fn is_main_turn(v: &Value) -> bool {
    turn_kind(v).is_some()
}

/// Claude Code's client-side assistant notices — an `isApiErrorMessage` ("Prompt is too long", …)
/// or any `message.model == "<synthetic>"` row ("No response requested."). Not model output.
fn is_synthetic_assistant(v: &Value) -> bool {
    v.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false)
        || v.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
}

/// Session-level records written once (or rarely) that a windowed tail must not lose — the label
/// and identity facts Claude Code's picker and loader read: the legacy `summary`, both titles, the
/// tag, agent name, a fork's relocation, the running cost, the ATIS latch and a continuation
/// pointer. The LAST occurrence of each survives wherever it sits; per-prompt records (`mode`,
/// `last-prompt`, …) recur constantly and need no help.
const SINGLETON_RECORDS: &[&str] = &[
    "summary",
    "ai-title",
    "custom-title",
    "tag",
    "agent-name",
    "relocated",
    "cost-state",
    "atis-latch",
    "continued-in",
];

/// Size a `--window` tail by Claude's OWN recorded token counts — no estimator. Returns
/// `(turn to keep from, real content-token size of that tail, overshoot)`, or `None` if the session
/// has no `usage` records (then the caller falls back to a byte estimate). `overshoot` is set when
/// even the single newest turn exceeds the budget — we keep that turn anyway (an empty session
/// wouldn't be a session), and the caller must warn.
///
/// Why this is exact (not a guess): within the last compaction segment, a turn's recorded `usage`
/// (input + cache) IS the real context size at that point, monotonically increasing. The standalone
/// size of the tail kept from turn S is therefore `usage_end - usage[S-1]` — the shared prefix
/// cancels — so we pick the **largest tail whose real size ≤ budget**. Capped at the last segment so
/// the cancellation holds (older segments were compacted away and aren't a clean prefix). This is
/// the same arithmetic that predicts a resume exactly: a 250k-byte-estimate tail that loads as 373k
/// real is just `usage_end - usage[cutoff]`, which no byte ratio can know but the recorded numbers
/// do. Sidechain lines are skipped entirely — their usage measures a different context.
fn usage_window_cutoff(parsed: &[Option<Value>], budget: u64) -> Option<(usize, u64, bool)> {
    // Per main-thread turn that carries a usage record: (turn index, cumulative usage, starts a new
    // compaction segment). Cumulative usage is monotonic WITHIN a segment but resets across a
    // compaction boundary, so we can't subtract across the whole session — we work in per-turn deltas.
    let mut ti = 0usize;
    let mut recs: Vec<(usize, u64, bool)> = Vec::new();
    let mut boundary_pending = false;
    for v in parsed.iter().flatten() {
        if is_sidechain(v) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            _ if turn_kind(v).is_some() => {
                // Prefer `_cv_orig_ctx` (the real count a prior revive stashed) over the live usage,
                // which revive may have overwritten with a pinned figure.
                let tot = v
                    .get("_cv_orig_ctx")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        v.get("message")
                            .and_then(|m| m.get("usage"))
                            .and_then(Value::as_object)
                            .map(usage_total)
                    })
                    .unwrap_or(0);
                if tot > 0 {
                    recs.push((ti, tot, boundary_pending));
                    boundary_pending = false;
                }
                ti += 1;
            }
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                boundary_pending = true;
            }
            _ => {}
        }
    }
    if recs.is_empty() {
        return None;
    }
    // Real token cost contributed by each recorded turn: within a segment it's the usage delta from
    // the previous record; at a segment start it's the whole post-compaction context (summary + that
    // turn) — real content that a resumed tail must re-send.
    let mut costs: Vec<(usize, u64)> = Vec::with_capacity(recs.len());
    let mut prev = 0u64;
    for (t, u, seg_start) in &recs {
        let cost = if *seg_start { *u } else { u.saturating_sub(prev) };
        costs.push((*t, cost));
        prev = *u;
    }
    // Accumulate real cost from the newest turn backward, stopping BEFORE the budget would be
    // exceeded (the ≤-budget contract) — this spans compaction boundaries correctly (each segment's
    // content counted once, via its deltas). If even the newest turn alone busts the budget, keep it
    // and flag the overshoot.
    let mut acc = 0u64;
    let mut start = recs.last().unwrap().0 + 1;
    let mut overshoot = false;
    let mut fits_whole = true;
    for (t, c) in costs.iter().rev() {
        if acc.saturating_add(*c) > budget {
            if acc == 0 {
                acc = *c;
                start = *t;
                overshoot = true;
            }
            fits_whole = false;
            break;
        }
        acc += c;
        start = *t;
    }
    if fits_whole {
        start = 0; // the whole session fits — keep everything, incl. turns before the first record
    }
    Some((start, acc, overshoot))
}

/// Resolve which conversational turns (`[start, end)`) a `--window`/`--range` prune keeps, plus the
/// real token size of the kept tail when known (from `usage`) and whether the ≤-budget contract had
/// to be overshot (a single turn bigger than the budget). `--range` is explicit; `--window` sizes by
/// Claude's recorded usage (falling back to a byte estimate only when usage is absent), snapped back
/// to a user turn so the tail opens on a prompt. Neither set ⇒ the whole session.
fn select_kept_turns(
    parsed: &[Option<Value>],
    opts: &PruneOptions,
    total_turns: usize,
) -> (usize, usize, Option<u64>, bool) {
    if let Some((s, e)) = opts.keep_range {
        let end = e.unwrap_or(total_turns).min(total_turns);
        return (s.min(end), end, None, false);
    }
    let Some(budget) = opts.window else {
        return (0, total_turns, None, false);
    };

    let is_user: Vec<bool> = parsed.iter().flatten().filter_map(turn_kind).collect();
    let snap = |mut start: usize| {
        // Snap back to a user turn so the tail opens on a prompt, not a bare assistant reply.
        while start > 0 && !is_user.get(start).copied().unwrap_or(true) {
            start -= 1;
        }
        start
    };

    // PRINCIPLED path: size by Claude's own recorded token counts.
    if let Some((start, real, overshoot)) = usage_window_cutoff(parsed, budget) {
        return (snap(start), total_turns, Some(real), overshoot);
    }

    // Fallback (no usage records): byte estimate of message.content, newest-backward, keeping the
    // largest tail whose estimate is ≤ budget (same contract as the usage path).
    // Model-visible attachments (system reminders) ride with the turn they precede.
    let mut per_turn: Vec<u64> = Vec::new();
    let mut pending = 0u64;
    for v in parsed.iter().flatten().filter(|v| !is_sidechain(v)) {
        if is_main_turn(v) {
            per_turn.push(fact_est_tokens(v) + pending);
            pending = 0;
        } else {
            pending += fact_est_tokens(v);
        }
    }
    let mut acc = 0u64;
    let mut start = per_turn.len();
    let mut overshoot = false;
    for i in (0..per_turn.len()).rev() {
        if acc.saturating_add(per_turn[i]) > budget {
            if acc == 0 {
                // Even the single newest turn exceeds the budget: keep it, but flag the overshoot.
                start = i;
                overshoot = true;
            }
            break;
        }
        acc += per_turn[i];
        start = i;
    }
    if start == per_turn.len() {
        start = 0; // no turns at all — degenerate, keep everything
    }
    (snap(start), total_turns, None, overshoot)
}

/// Prune `src_path` (a Claude `<id>.jsonl`) into a new session. Returns what happened.
pub fn prune_session(src_path: &Path, opts: &PruneOptions) -> Result<PruneResult> {
    let source = crate::json::JsonSource::capture(src_path)?;
    let file = std::fs::File::open(src_path)?;
    source.check_metadata(file.metadata()?)?;
    let original_size = file.metadata()?.len();
    let mut lines = Vec::new();
    let mut parsed = Vec::new();
    let mut tool_names = HashMap::new();
    let mut id_spans = HashMap::new();
    // Retain only decision facts; payload trees live for one record. Preserve the old global
    // trim_end contract (including whitespace-only trailing records) without copying the file.
    for record in ranges::records(
        &file,
        Range {
            offset: 0,
            len: original_size,
        },
    )? {
        let (range, _) = record?;
        let facts = if range.len >= LARGE_RECORD {
            match source.read_range_value(range.offset, range.len, |view| {
                let spans = crate::restamp::source_id_spans(view)?;
                let facts = decision_facts_source(view, range, opts, &mut tool_names)?;
                id_spans.insert(range.offset, spans);
                Ok(facts)
            }) {
                Ok(facts) => Some(facts),
                Err(e) if e.is::<crate::json::InvalidJson>() => {
                    // Preserve malformed-row passthrough and the historical fatal UTF-8 policy.
                    let line = read_line(&file, range)?;
                    serde_json::from_str::<Value>(&line)
                        .ok()
                        .as_ref()
                        .map(|v| decision_facts(v, opts.window.is_some()))
                }
                Err(e) => return Err(e),
            }
        } else {
            let line = read_line(&file, range)?;
            let value = serde_json::from_str::<Value>(&line).ok();
            if let Some(v) = &value {
                collect_tool_names(v, range, &mut tool_names);
            }
            value.as_ref().map(|v| decision_facts(v, opts.window.is_some()))
        };
        parsed.push(facts);
        lines.push(range);
    }
    while let Some(range) = lines.last_mut() {
        let trimmed = ranges::trim(&file, *range)?;
        if trimmed.len == 0 {
            lines.pop();
            parsed.pop();
        } else {
            // trim() locates both boundaries; only the final boundary belongs to trim_end().
            range.len = trimmed.offset + trimmed.len - range.offset;
            break;
        }
    }
    // split on an empty trimmed file historically produces one empty line.
    if lines.is_empty() {
        lines.push(Range { offset: 0, len: 0 });
        parsed.push(None);
    }
    source.check()?;

    let source_id = parsed
        .iter()
        .flatten()
        .find_map(|v| v.get("sessionId")?.as_str().map(String::from))
        .unwrap_or_default();
    let new_id = opts.new_id.clone().unwrap_or_else(new_uuid);
    if new_id == source_id {
        bail!("new id must differ from the source id");
    }

    let dir = src_path.parent().unwrap_or(Path::new("."));
    let src_stem = src_path.file_stem().and_then(|s| s.to_str()).unwrap_or(&source_id);
    let new_path = dir.join(format!("{new_id}.jsonl"));
    let sidecar_path = dir.join(format!("{new_id}.flat.jsonl"));
    let source_location = std::fs::canonicalize(src_path)?;
    if std::fs::canonicalize(&new_path).is_ok_and(|p| p == source_location) {
        bail!("new session path must differ from the source path");
    }

    // Map tool_use_id -> (name, input) from assistant tool_use blocks (which use `id`, not
    // `tool_use_id`); tool_result blocks only carry the id, not the name.

    // Conversational-turn index per line, so keep-last can spare the most recent turns. A line is
    // "old" (eligible to snip) iff its turn index is before the keep-last window. Sidechain lines
    // are positioned by the surrounding main-thread turn but never counted.
    let total_turns = parsed.iter().flatten().filter(|v| is_main_turn(v)).count();
    let keep_from = total_turns.saturating_sub(opts.keep_last);

    // `--window`/`--range`: select which conversational turns survive; the rest are dropped and the
    // first survivor is re-rooted after the loop. Defaults to the whole session (no-op).
    let (keep_start, keep_end, window_real, overshoot) = select_kept_turns(&parsed, opts, total_turns);
    let windowing = keep_start > 0 || keep_end < total_turns;
    // Session-level singleton records the kept tail must not lose (see [`SINGLETON_RECORDS`]): the
    // line index of the LAST occurrence of each type, kept wherever the window falls.
    let mut keep_singletons: HashSet<usize> = HashSet::new();
    if windowing {
        let mut last: HashMap<&str, usize> = HashMap::new();
        for (i, v) in parsed.iter().enumerate() {
            if let Some(t) = v.as_ref().and_then(|v| v.get("type")).and_then(Value::as_str) {
                if SINGLETON_RECORDS.contains(&t) {
                    last.insert(t, i);
                }
            }
        }
        keep_singletons.extend(last.into_values());
    }
    let mut dropped_turns = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    if overshoot {
        warnings.push(format!(
            "--window {}: the single newest turn alone is ~{} tokens — kept it, but the tail EXCEEDS \
             the requested budget and resume may still hit the context wall",
            opts.window.unwrap_or(0),
            window_real.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
        ));
    }

    let mut output = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    let mut output_facts = Vec::new();
    let mut output_pruned = Vec::new();
    let mut sidecar_stage = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    let mut sidecar_count = 0usize;
    let mut rooted = false;
    let mut sidecar: Vec<SidecarEntry> = Vec::new();
    let mut pruned_count = 0usize;
    let mut image_blocks = 0usize;
    let mut est_tokens_saved = 0u64;
    let mut turn = 0usize;

    for (idx, (facts, range)) in parsed.into_iter().zip(lines.iter()).enumerate() {
        let Some(facts) = facts else {
            let line = read_line(&file, *range)?;
            writeln!(output, "{line}")?;
            if opts.revive {
                output_facts.push(None);
                output_pruned.push(line.contains(MARKER_PREFIX));
            }
            continue;
        };
        let ty = facts.get("type").and_then(Value::as_str).unwrap_or("");
        let is_turn_line = ty == "user" || ty == "assistant";
        let is_turn = turn_kind(&facts).is_some();
        let this_turn = turn;
        if is_turn {
            turn += 1;
        }
        let is_old = is_turn_line && this_turn < keep_from;

        // `--window`/`--range`: drop turns outside the kept range. Non-turn records (system lines,
        // attachments, file-history snapshots, sidechain lines, synthetic notices) travel with the
        // turn they precede — EXCEPT:
        //  * the last occurrence of each session-level singleton (titles, tag, agent name, cost,
        //    … — [`SINGLETON_RECORDS`]) is always kept so the new session keeps its label and
        //    identity (a `summary`'s `leafUuid` may dangle after a head drop; Claude Code tolerates
        //    that);
        //  * records trailing the FINAL kept turn are kept when the range runs to the end of the
        //    session (they belong to the kept tail, not to any dropped turn).
        if windowing {
            let drop_line = if is_turn {
                this_turn < keep_start || this_turn >= keep_end
            } else if keep_singletons.contains(&idx) {
                false
            } else {
                this_turn < keep_start || (keep_end < total_turns && this_turn >= keep_end)
            };
            if drop_line {
                if is_turn {
                    dropped_turns += 1;
                }
                continue;
            }
        }

        let needs_root = windowing && keep_start > 0 && !rooted && is_main_turn(&facts);
        let may_mutate =
            (is_old && ty == "user" && facts.get("_prune_payload").and_then(Value::as_bool).unwrap_or(true))
                || (is_old
                    && ty == "assistant"
                    && opts.thinking
                    && facts.get("_prune_thinking").and_then(Value::as_bool).unwrap_or(true))
                || (is_turn_line && opts.declassify)
                || (needs_root && facts.get("_prune_parent").and_then(Value::as_bool).unwrap_or(true));
        if range.len >= LARGE_RECORD && !may_mutate {
            let mut scan = MarkerWriter::new(&mut output);
            crate::restamp::write_restamped_spans(
                &source,
                &file,
                range.offset,
                range.len,
                &new_id,
                &mut id_spans.remove(&range.offset).unwrap_or_default(),
                &mut scan,
            )?;
            scan.write_all(b"\n")?;
            let pruned = scan.found;
            if needs_root {
                rooted = true;
            }
            if opts.revive {
                output_facts.push(Some(facts));
                output_pruned.push(pruned);
            }
            continue;
        }
        let line = read_line(&file, *range)?;
        let mut v: Value = serde_json::from_str(&line)?;

        // Unique per-line key for sidecar ids when a block has no id of its own: the line's uuid,
        // or its file line index as a last resort — never a shared constant (collisions would make
        // earlier sidecar payloads unretrievable).
        let line_key = v
            .get("uuid")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("L{idx}"));

        let mut modified = false;
        if is_old && ty == "user" {
            // Only old user messages carry snippable tool results.
            let current_tools = materialize_tool_inputs(&file, &v, &tool_names)?;
            prune_user_line(
                &mut v,
                opts,
                &current_tools,
                &new_id,
                &line_key,
                &mut sidecar,
                &mut pruned_count,
                &mut image_blocks,
                &mut est_tokens_saved,
                &mut modified,
            );
        } else if is_old && ty == "assistant" && opts.thinking {
            // `--thinking`: flatten the OLDEST assistant reasoning (recent thinking, within
            // keep-last, stays verbatim so the resumed agent keeps its live train of thought).
            prune_assistant_thinking(
                &mut v,
                opts,
                &new_id,
                &line_key,
                &mut sidecar,
                &mut pruned_count,
                &mut est_tokens_saved,
                &mut modified,
            );
        }

        // `--declassify`: snip security-dense message PROSE (user string or assistant text blocks)
        // wherever it appears — a classifier scores the WHOLE loaded context, so a trigger span
        // ANYWHERE downgrades a restricted (e.g. Fable) seat; recency does NOT protect it (unlike the
        // tool/thinking passes above, which keep recent payloads verbatim for continuity). Retrievable.
        if is_turn_line && opts.declassify {
            prune_declassify_text(
                &mut v,
                opts,
                &new_id,
                &line_key,
                &mut sidecar,
                &mut pruned_count,
                &mut est_tokens_saved,
                &mut modified,
            );
        }

        // An untouched line is the source's bytes with only its session id replaced
        // ([`crate::restamp_line`]); a snipped line is re-serialized (values preserved; key order
        // and whitespace may normalize) and then stamped the same way.
        let out = if modified { v.to_string() } else { line };
        let mut out = crate::restamp_line(&out, &new_id).unwrap_or(out);
        if windowing && keep_start > 0 && !rooted && is_main_turn(&v) {
            rooted = true;
            if v.get("parentUuid").is_some_and(|p| !p.is_null()) {
                // Reparse the stamped line so the new session id survives normalization.
                let mut stamped: Value = serde_json::from_str(&out)?;
                stamped["parentUuid"] = Value::Null;
                out = stamped.to_string();
            }
        }
        if opts.revive {
            output_facts.push(Some(decision_facts(&v, true)));
            output_pruned.push(out.contains(MARKER_PREFIX));
        }
        writeln!(output, "{out}")?;
        for entry in sidecar.drain(..) {
            serde_json::to_writer(&mut sidecar_stage, &entry)?;
            sidecar_stage.write_all(b"\n")?;
            sidecar_count += 1;
        }
    }

    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let output = output.into_inner().map_err(|e| e.into_error())?.finish()?;
    let (output, usage_rewritten, revive_tokens, revive_old_tokens) = if opts.revive {
        revive_staged(output, &output_facts, &output_pruned, windowing, window_real)?
    } else {
        (output, 0, None, None)
    };
    let new_size = output.len()?;
    let sidecar = sidecar_stage.into_inner().map_err(|e| e.into_error())?.finish()?;

    let sidecar_out = (!opts.drop && sidecar_count > 0).then(|| sidecar_path.clone());
    let copied = (opts.copy_resources && dir.join(src_stem).is_dir()).then(|| dir.join(&new_id));
    if sidecar_out
        .as_ref()
        .is_some_and(|p| std::fs::canonicalize(p).is_ok_and(|p| p == source_location))
    {
        bail!("sidecar path must differ from the source path");
    }

    if !opts.dry_run {
        let transcript = PreparedFile::new(&new_path, &output)?;
        let sidecar_file = sidecar_out
            .as_ref()
            .map(|p| PreparedFile::new(p, &sidecar))
            .transpose()?;
        source.check_metadata(file.metadata()?)?;
        source.check()?;
        // Serialization and writes complete before publication. New destinations use atomic renames; existing destinations preserve fs::write
        // behavior. Transcript, sidecar and resource publication remain separate operations.
        transcript.publish(&file)?;
        if let Some(sidecar_file) = sidecar_file {
            sidecar_file.publish(&file)?;
        }
        if let Some(dst) = &copied {
            copy_dir(&dir.join(src_stem), dst)
                .with_context(|| format!("copying session resources to {}", dst.display()))?;
        }
    }

    Ok(PruneResult {
        source_id,
        new_id,
        new_path,
        sidecar_path: sidecar_out,
        copied_resources: copied,
        pruned_count,
        image_blocks,
        original_size,
        new_size,
        est_context_tokens_saved: est_tokens_saved,
        usage_rewritten,
        dropped_turns,
        window_real_tokens: window_real,
        revive_tokens,
        revive_old_tokens,
        warnings,
        dry_run: opts.dry_run,
    })
}

/// Same-directory temporary outputs keep a failed write from truncating a prior destination.
struct PreparedFile {
    temporary: PathBuf,
    destination: PathBuf,
}
impl PreparedFile {
    fn new(destination: &Path, bytes: &ByteSource) -> Result<Self> {
        if destination.exists() && !destination.is_file() {
            bail!("output is not a regular file: {}", destination.display());
        }
        let temporary = destination.with_file_name(format!(".cv-prune-{}", uuid::Uuid::new_v4()));
        let prepared = Self {
            temporary,
            destination: destination.to_owned(),
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&prepared.temporary)?;
        std::io::copy(
            &mut std::io::BufReader::with_capacity(64 * 1024, bytes.reader()?),
            &mut file,
        )?;
        file.flush()?;
        Ok(prepared)
    }
    fn publish(self, source: &std::fs::File) -> Result<()> {
        if std::fs::symlink_metadata(&self.destination).is_ok() {
            // Existing outputs keep fs::write's symlink, hardlink, permissions and read-only
            // semantics. Open without truncation so a source inode alias is rejected first.
            let mut destination = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&self.destination)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let original = source.metadata()?;
                let target = destination.metadata()?;
                anyhow::ensure!(
                    original.dev() != target.dev() || original.ino() != target.ino(),
                    "output aliases source inode"
                );
            }
            #[cfg(not(unix))]
            let _ = source;
            destination.set_len(0)?;
            std::io::copy(
                &mut std::io::BufReader::with_capacity(64 * 1024, std::fs::File::open(&self.temporary)?),
                &mut destination,
            )?;
            Ok(())
        } else {
            std::fs::rename(&self.temporary, &self.destination)
                .with_context(|| format!("publishing {}", self.destination.display()))
        }
    }
}
impl Drop for PreparedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

fn read_line(file: &std::fs::File, range: Range) -> Result<String> {
    let mut line = String::with_capacity(usize::try_from(range.len)?);
    ranges::reader(file, range)?.read_to_string(&mut line)?;
    Ok(line)
}

fn decision_facts_source(
    view: crate::json::JsonView<'_>,
    range: Range,
    opts: &PruneOptions,
    tools: &mut HashMap<String, (String, Range, usize)>,
) -> Result<Value> {
    use crate::harness::store::document::Fields;
    let keys = [
        "type",
        "subtype",
        "isSidechain",
        "isApiErrorMessage",
        "sessionId",
        "_cv_orig_ctx",
        "message",
        "parentUuid",
        "toolUseResult",
        "rendered",
    ];
    let Some(fields) = Fields::parse(view, &keys)? else {
        return Ok(decision_facts(&Value::Null, false));
    };
    let mut facts = fields.project(&keys[..6])?;
    let ty = facts.get("type").and_then(Value::as_str).unwrap_or("");
    let mut message = serde_json::Map::new();
    let mut content = None;
    if let Some(native) = fields.get("message") {
        if let Some(native) = Fields::parse(native, &["model", "usage", "content"])? {
            if let Some(model) = native.value("model")? {
                message.insert("model".into(), model);
            }
            if let Some(usage) = native.get("usage") {
                if let Some(usage) = source_usage_total(usage)? {
                    message.insert("usage".into(), serde_json::json!({"input_tokens":usage}));
                }
            }
            content = native.get("content");
        }
    }
    let mut payload = fields
        .get("toolUseResult")
        .map(|v| value_byte_size_source(v).map(|n| n > opts.min_size))
        .transpose()?
        .unwrap_or(false);
    let mut thinking = false;
    if let Some(blocks) = content.map(|c| c.array()).transpose()?.flatten() {
        for (index, block) in blocks.enumerate() {
            let Some(block) = Fields::parse(block?, &["type", "id", "name", "content"])? else {
                continue;
            };
            match block.string("type")?.as_deref() {
                Some("tool_use") if ty == "assistant" => {
                    if let Some(id) = block.string("id")? {
                        tools.insert(id, (block.string("name")?.unwrap_or_default(), range, index));
                    }
                }
                Some("tool_result") => {
                    payload |= block
                        .get("content")
                        .map(|v| value_byte_size_source(v).map(|n| n > opts.min_size))
                        .transpose()?
                        .unwrap_or(false);
                }
                Some("thinking") => thinking = true,
                _ => {}
            }
        }
    }
    let estimate = if opts.window.is_some() || opts.revive {
        match ty {
            "user" | "assistant" => content.map(est_tokens_source).transpose()?.unwrap_or(0),
            "attachment" => {
                let mut estimate = 0;
                if let Some(items) = fields.get("rendered").map(|v| v.array()).transpose()?.flatten() {
                    for item in items {
                        if let Some(item) = Fields::parse(item?, &["content"])? {
                            if let Some(text) = item.get("content") {
                                if text.is_string()? {
                                    estimate += source_string_tokens(text)?;
                                }
                            }
                        }
                    }
                }
                estimate
            }
            _ => 0,
        }
    } else {
        0
    };
    let parent = fields
        .get("parentUuid")
        .map(|v| v.is_null().map(|n| !n))
        .transpose()?
        .unwrap_or(false);
    facts.insert("message".into(), Value::Object(message));
    facts.insert("_prune_est".into(), Value::from(estimate));
    facts.insert("_prune_payload".into(), Value::from(payload));
    facts.insert("_prune_thinking".into(), Value::from(thinking));
    facts.insert("_prune_parent".into(), Value::from(parent));
    Ok(Value::Object(facts))
}

/// Retain only the counters and the last non-auxiliary iteration that usage_total consumes.
fn source_usage_total(view: crate::json::JsonView<'_>) -> Result<Option<u64>> {
    use crate::harness::store::document::Fields;
    let Some(fields) = Fields::parse(
        view,
        &[
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "iterations",
        ],
    )?
    else {
        return Ok(None);
    };
    let mut usage = serde_json::Map::new();
    for key in USAGE_CTX_KEYS {
        if let Some(value) = fields
            .get(key)
            .map(|v| v.as_number())
            .transpose()?
            .flatten()
            .and_then(|n| n.as_u64())
        {
            usage.insert(key.into(), Value::from(value));
        }
    }
    if ctx_sum(&usage) > 0 {
        if let Some(iterations) = fields.get("iterations").map(|v| v.array()).transpose()?.flatten() {
            let mut last = None;
            for iteration in iterations {
                let Some(fields) = Fields::parse(
                    iteration?,
                    &[
                        "type",
                        "input_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                        "output_tokens",
                    ],
                )?
                else {
                    last = Some(Value::Null);
                    continue;
                };
                let mut record = serde_json::Map::new();
                if let Some(kind) = fields.string("type")? {
                    record.insert("type".into(), Value::String(kind));
                }
                if matches!(
                    record.get("type").and_then(Value::as_str),
                    Some("advisor_message" | "compaction")
                ) {
                    continue;
                }
                for key in USAGE_CTX_KEYS.into_iter().chain(["output_tokens"]) {
                    if let Some(value) = fields
                        .get(key)
                        .map(|v| v.as_number())
                        .transpose()?
                        .flatten()
                        .and_then(|n| n.as_u64())
                    {
                        record.insert(key.into(), Value::from(value));
                    }
                }
                last = Some(Value::Object(record));
            }
            if let Some(last) = last {
                usage.insert("iterations".into(), Value::Array(vec![last]));
            }
        }
    }
    Ok(Some(usage_total(&usage)))
}

fn source_string_tokens(view: crate::json::JsonView<'_>) -> Result<u64> {
    let mut bytes = 0;
    view.for_each_str_chunk(|part| {
        bytes += part.len();
        Ok(())
    })?;
    Ok(crate::ir::est_tokens_bytes(bytes, TEXT_BYTES_PER_TOKEN))
}
fn value_byte_size_source(view: crate::json::JsonView<'_>) -> Result<usize> {
    if view.is_null()? {
        return Ok(0);
    }
    if view.is_string()? {
        let mut bytes = 0;
        view.for_each_str_chunk(|part| {
            bytes += part.len();
            Ok(())
        })?;
        Ok(bytes)
    } else {
        Ok(usize::try_from(view.canonical_len()?)?)
    }
}
fn est_tokens_source(view: crate::json::JsonView<'_>) -> Result<u64> {
    if view.is_string()? {
        return source_string_tokens(view);
    }
    let Some(blocks) = view.array()? else {
        return Ok(crate::ir::est_tokens_bytes(
            usize::try_from(view.canonical_len()?)?,
            TEXT_BYTES_PER_TOKEN,
        ));
    };
    let mut total = 0;
    for block in blocks {
        let block = block?;
        let fields = crate::harness::store::document::Fields::parse(block, &["type", "text"])?;
        match fields
            .as_ref()
            .map(|f| f.string("type"))
            .transpose()?
            .flatten()
            .as_deref()
        {
            Some("image") => total += crate::ir::IMAGE_TOKENS,
            Some("text") => {
                if let Some(text) = fields.and_then(|f| f.get("text")) {
                    if text.is_string()? {
                        total += source_string_tokens(text)?;
                    }
                }
            }
            _ => total += crate::ir::est_tokens_bytes(usize::try_from(block.canonical_len()?)?, TEXT_BYTES_PER_TOKEN),
        }
    }
    Ok(total)
}

/// Marker evidence follows emitted bytes, including source chunks split by id surgery.
struct MarkerWriter<'a, W> {
    output: &'a mut W,
    tail: Vec<u8>,
    found: bool,
}
impl<'a, W: Write> MarkerWriter<'a, W> {
    fn new(output: &'a mut W) -> Self {
        Self {
            output,
            tail: Vec::new(),
            found: false,
        }
    }
}
impl<W: Write> Write for MarkerWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.output.write(bytes)?;
        let bytes = &bytes[..n];
        if !self.found {
            let marker = MARKER_PREFIX.as_bytes();
            let mut boundary = self.tail.clone();
            boundary.extend_from_slice(&bytes[..bytes.len().min(marker.len() - 1)]);
            self.found =
                boundary.windows(marker.len()).any(|p| p == marker) || bytes.windows(marker.len()).any(|p| p == marker);
            self.tail.extend_from_slice(bytes);
            let keep = (marker.len() - 1).min(self.tail.len());
            self.tail.drain(..self.tail.len() - keep);
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// Compact decision roster: no message content, thinking, image or tool-result payloads.
fn decision_facts(v: &Value, estimate: bool) -> Value {
    let mut facts = serde_json::Map::new();
    for key in [
        "type",
        "subtype",
        "isSidechain",
        "isApiErrorMessage",
        "sessionId",
        "_cv_orig_ctx",
    ] {
        if let Some(value) = v.get(key) {
            facts.insert(key.into(), value.clone());
        }
    }
    let mut message = serde_json::Map::new();
    if let Some(model) = v.pointer("/message/model") {
        message.insert("model".into(), model.clone());
    }
    if let Some(usage) = v.pointer("/message/usage").and_then(Value::as_object) {
        message.insert("usage".into(), serde_json::json!({"input_tokens": usage_total(usage)}));
    }
    facts.insert("message".into(), Value::Object(message));
    facts.insert(
        "_prune_est".into(),
        Value::from(if estimate { line_est_tokens(v) } else { 0 }),
    );
    Value::Object(facts)
}

fn fact_est_tokens(v: &Value) -> u64 {
    v.get("_prune_est")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| line_est_tokens(v))
}

/// Rewrite the trailing `message.usage` token counts to an honest post-prune figure (`--revive`).
///
/// Claude Code's resume gate reads the **last turn's recorded `usage`** (input + cache tokens) as the
/// session's current context size — and it does so *before* re-sending anything to the API. After a
/// prune the real content is small, but that recorded number is still the pre-prune total, so the gate
/// refuses to resume a session that would actually fit (the "979.7k / context limit reached" wall).
/// Concretely (2.1.278): it takes the last non-synthetic assistant record's usage — preferring the
/// last request entry of `usage.iterations` over the top-level counters — adds a byte estimate of
/// everything recorded after it, and if that is ≥ context window − max-output reserve (≤ 20k) −
/// 3k it yields a synthesized `Prompt is too long` (no `errorDetails`, no request) instead of
/// calling the API. So the pin has to land in `iterations` too — see [`pin_usage`].
///
/// We recompute the honest size of the **loaded window** — everything after the last compaction
/// boundary, which is what Claude actually re-sends on resume — and rewrite every usage record in that
/// window that still over-reports, pinning it to the honest figure with the cache counters zeroed (a
/// fresh resume has no live cache anyway). The honest figure combines recorded-usage **deltas** (real
/// evidence, span by span — see [`honest_window_tokens`]) with byte estimates, always preferring the
/// **higher** supported figure so the gate errs on the safe side: it must never be talked into loading
/// a session that would blow the real limit. Returns `(records rewritten, honest tokens, stale max)`.
fn revive_staged(
    output: ByteSource,
    parsed: &[Option<Value>],
    pruned: &[bool],
    windowed: bool,
    real_override: Option<u64>,
) -> Result<(ByteSource, usize, Option<u64>, Option<u64>)> {
    let window = if windowed {
        0
    } else {
        parsed
            .iter()
            .rposition(|p| {
                p.as_ref().is_some_and(|v| {
                    v.get("type").and_then(Value::as_str) == Some("system")
                        && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                })
            })
            .map_or(0, |i| i + 1)
    };
    let honest = honest_window_tokens(parsed, pruned, window, window == 0 && !windowed).max(real_override.unwrap_or(0));
    let mut rewritten = 0;
    let mut stale_max = 0;
    let mut stage = std::io::BufWriter::with_capacity(64 * 1024, ByteStage::new());
    use std::io::BufRead;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, output.reader()?);
    for (i, facts) in parsed.iter().enumerate() {
        let total = facts
            .as_ref()
            .filter(|v| !is_sidechain(v))
            .and_then(|v| v.pointer("/message/usage").and_then(Value::as_object).map(usage_total))
            .unwrap_or(0);
        if i >= window && total > honest {
            let mut bytes = Vec::new();
            reader.read_until(b'\n', &mut bytes)?;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            let mut v: Value = serde_json::from_slice(&bytes)?;
            if v.get("_cv_orig_ctx").is_none() {
                v["_cv_orig_ctx"] = Value::from(total);
            }
            if let Some(usage) = v.pointer_mut("/message/usage").and_then(Value::as_object_mut) {
                pin_usage(usage, honest);
            }
            serde_json::to_writer(&mut stage, &v)?;
            stage.write_all(b"\n")?;
            rewritten += 1;
            stale_max = stale_max.max(total);
        } else {
            // Usage metadata tells us this row is unchanged; copy its framed bytes without
            // allocating a giant String just to discover that its usage doesn't need rewriting.
            loop {
                let bytes = reader.fill_buf()?;
                anyhow::ensure!(!bytes.is_empty(), "staged transcript ended before its decision roster");
                let newline = memchr::memchr(b'\n', bytes);
                let n = newline.map_or(bytes.len(), |i| i + 1);
                stage.write_all(&bytes[..n])?;
                reader.consume(n);
                if newline.is_some() {
                    break;
                }
            }
        }
    }
    Ok((
        stage.into_inner().map_err(|e| e.into_error())?.finish()?,
        rewritten,
        (rewritten > 0).then_some(honest),
        (rewritten > 0).then_some(stale_max),
    ))
}

/// Honest post-prune token size of `parsed[window..]`, combining recorded-usage deltas with byte
/// estimates, **span by span** (a span = the lines covered by one usage record, i.e. everything
/// since the previous record).
///
///  * A span whose content was NOT touched by pruning: the recorded delta is real evidence of its
///    cost, usually HIGHER than the 3.5 B/tok estimate — take `max(delta, estimate)` so we never
///    write a figure lower than what the recorded evidence supports.
///  * A span holding a `[PRUNED …]` marker: the recorded delta measured the now-snipped payload, so
///    only the byte estimate of what's actually left is meaningful.
///  * A segment-start record (right after a compaction boundary, or the scan start when
///    `!first_trusted`): its absolute total is not a delta — and after a boundary it's exactly the
///    stale figure revive exists to correct — so its span is byte-estimated too.
///  * Lines after the last record (or when there are no records at all): byte estimate.
///
/// Sidechain lines contribute neither usage nor bytes — they aren't re-sent on the main thread.
fn honest_window_tokens(parsed: &[Option<Value>], pruned: &[bool], window: usize, first_trusted: bool) -> u64 {
    let span_est = |lo: usize, hi: usize| -> u64 {
        parsed[lo..hi]
            .iter()
            .flatten()
            .filter(|v| !is_sidechain(v))
            .map(fact_est_tokens)
            .sum()
    };
    let mut sum = 0u64;
    let mut prev = 0u64;
    let mut seg_start = !first_trusted;
    let mut span_start = window;
    for i in window..parsed.len() {
        let Some(v) = &parsed[i] else { continue };
        if v.get("type").and_then(Value::as_str) == Some("system")
            && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            seg_start = true;
            continue;
        }
        if is_sidechain(v) {
            continue;
        }
        let total = v
            .get("_cv_orig_ctx")
            .and_then(Value::as_u64)
            .or_else(|| v.pointer("/message/usage").and_then(Value::as_object).map(usage_total))
            .unwrap_or(0);
        if total == 0 {
            continue;
        }
        let est = span_est(span_start, i + 1);
        let span_pruned = pruned[span_start..=i].iter().any(|p| *p);
        sum += if seg_start || span_pruned {
            est
        } else {
            est.max(total.saturating_sub(prev))
        };
        prev = total;
        seg_start = false;
        span_start = i + 1;
    }
    sum + span_est(span_start, parsed.len())
}

/// The three context-bearing counters of a Claude `usage` object (input + both cache buckets).
const USAGE_CTX_KEYS: [&str; 3] = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"];

fn ctx_sum(m: &serde_json::Map<String, Value>) -> u64 {
    USAGE_CTX_KEYS
        .iter()
        .map(|k| m.get(*k).and_then(Value::as_u64).unwrap_or(0))
        .sum()
}

/// The context-token count of a Claude `usage` object — read the way Claude Code reads it.
///
/// Since 2.1.277 Claude Code sizes the loaded context from a record's `usage.iterations` when that
/// array is present: it takes the LAST iteration that is not an auxiliary request (`advisor_message`
/// / `compaction`), and if that one is a well-formed request iteration (`message` /
/// `fallback_message`, all four counts present, non-zero context) reads input + both cache buckets
/// from IT — the top-level counters are only the fallback (and the whole thing short-circuits to the
/// top-level when the top-level total is zero). `--window` sizing, the honest-figure arithmetic and
/// `--revive`'s stale-record detection all go through here so cv reasons about the same number the
/// resume gate will see.
fn usage_total(usage: &serde_json::Map<String, Value>) -> u64 {
    let top = ctx_sum(usage);
    if top == 0 {
        return 0;
    }
    usage
        .get("iterations")
        .and_then(Value::as_array)
        .and_then(|iters| iters.iter().rev().find(|it| !is_aux_iteration(it)))
        .and_then(Value::as_object)
        .filter(|it| is_request_iteration(it))
        .map(ctx_sum)
        .unwrap_or(top)
}

/// `advisor_message` / `compaction` iterations are side requests Claude Code skips when reading
/// context size.
fn is_aux_iteration(it: &Value) -> bool {
    matches!(
        it.get("type").and_then(Value::as_str),
        Some("advisor_message" | "compaction")
    )
}

/// A request iteration Claude Code will read the context size from: `message` / `fallback_message`
/// with all four counts present as non-negative numbers and a non-zero context total.
fn is_request_iteration(it: &serde_json::Map<String, Value>) -> bool {
    matches!(
        it.get("type").and_then(Value::as_str),
        Some("message" | "fallback_message")
    ) && [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .all(|k| it.get(*k).and_then(Value::as_u64).is_some())
        && ctx_sum(it) > 0
}

/// Pin a `usage` record to `honest` context tokens everywhere Claude Code might read it: the
/// top-level counters AND every non-auxiliary entry of `iterations`. Pinning only the top level
/// used to be enough; since Claude Code 2.1.277 prefers the last request iteration, a stale
/// iteration silently defeats the pin — the gate reads the old wall figure and refuses the session
/// client-side (a synthesized "Prompt is too long", no API call). Cache counters are zeroed (a fresh
/// resume has no live cache anyway); output counts are real and left alone.
fn pin_usage(u: &mut serde_json::Map<String, Value>, honest: u64) {
    fn pin(m: &mut serde_json::Map<String, Value>, honest: u64) {
        m.insert("input_tokens".into(), Value::from(honest));
        m.insert("cache_read_input_tokens".into(), Value::from(0u64));
        m.insert("cache_creation_input_tokens".into(), Value::from(0u64));
        if let Some(cc) = m.get_mut("cache_creation").and_then(Value::as_object_mut) {
            cc.insert("ephemeral_1h_input_tokens".into(), Value::from(0u64));
            cc.insert("ephemeral_5m_input_tokens".into(), Value::from(0u64));
        }
    }
    pin(u, honest);
    if let Some(iters) = u.get_mut("iterations").and_then(Value::as_array_mut) {
        for it in iters.iter_mut().filter(|it| !is_aux_iteration(it)) {
            if let Some(m) = it.as_object_mut() {
                pin(m, honest);
            }
        }
    }
}

/// Snip the eligible payloads on one (old) user line, in place. `line_key` (the line's uuid or a
/// file-position fallback) keys sidecar ids for blocks that carry no `tool_use_id` of their own.
#[allow(clippy::too_many_arguments)]
fn prune_user_line(
    v: &mut Value,
    opts: &PruneOptions,
    tool_names: &HashMap<String, (String, Value)>,
    new_id: &str,
    line_key: &str,
    sidecar: &mut Vec<SidecarEntry>,
    count: &mut usize,
    images: &mut usize,
    tokens_saved: &mut u64,
    modified: &mut bool,
) {
    // (1) tool_result blocks in message.content (the context-bearing payloads).
    let mut mirror_base: Option<String> = None; // sidecar-id base for the toolUseResult mirror
    let mut mirror_tuid: Option<String> = None; // real tool_use_id for the name/input lookup
    if let Some(blocks) = v.pointer_mut("/message/content").and_then(Value::as_array_mut) {
        for (bi, block) in blocks.iter_mut().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tuid = block.get("tool_use_id").and_then(Value::as_str).map(String::from);
            // Sidecar id: the tool_use_id when present, else `{line uuid}#tr{block idx}` — unique
            // per block, never a shared constant (which made colliding entries unretrievable).
            let entry_id = tuid.clone().unwrap_or_else(|| format!("{line_key}#tr{bi}"));
            if mirror_base.is_none() {
                mirror_base = Some(entry_id.clone());
                mirror_tuid = tuid.clone();
            }
            // Decide using a borrow; commit by cloning the payload out (releasing the borrow), then
            // overwrite the block — so we never hold a read of `block` across the mutation.
            let committed = match block.get("content") {
                Some(c) if !c.is_null() && !c.as_str().is_some_and(|s| s.starts_with(MARKER_PREFIX)) => {
                    let size = value_byte_size(c);
                    if size <= opts.min_size {
                        None
                    } else {
                        let (kind, text, img) = classify(c);
                        if kind == "none" {
                            None
                        } else {
                            Some((c.clone(), size, kind, text, img))
                        }
                    }
                }
                _ => None,
            };
            let Some((content, size, kind, text, img)) = committed else {
                continue;
            };
            let line_count = if text.is_empty() { 1 } else { text.lines().count() };
            let (name, input) = tuid
                .as_deref()
                .and_then(|t| tool_names.get(t).cloned())
                .unwrap_or_else(|| ("unknown".into(), Value::Null));
            let marker = build_marker(&entry_id, &name, &input, &kind, size, line_count, new_id, opts.drop);

            *tokens_saved += est_tokens(&content).saturating_sub(str_tokens(&marker));
            *images += img;
            if !opts.drop {
                sidecar.push(SidecarEntry {
                    id: entry_id,
                    slot: "content".into(),
                    name,
                    input,
                    content,
                    size,
                    line_count,
                    kind,
                });
            }
            block["content"] = Value::String(marker);
            *modified = true;
            *count += 1;
        }
    }

    // (2) the top-level toolUseResult mirror (disk-only; shrinks the file, not context). Clone it out
    // first so the borrow is released before we overwrite the field.
    let tur_committed = match v.get("toolUseResult") {
        Some(t) if !t.is_null() && !t.as_str().is_some_and(|s| s.starts_with(MARKER_PREFIX)) => {
            let size = value_byte_size(t);
            (size > opts.min_size).then(|| (t.clone(), size))
        }
        _ => None,
    };
    if let Some((tur, size)) = tur_committed {
        let base = mirror_base.unwrap_or_else(|| line_key.to_string());
        let id = format!("{base}{TUR_SUFFIX}");
        let kind = if tur_is_image(&tur) { "image" } else { "text" };
        let (name, input) = mirror_tuid
            .as_deref()
            .and_then(|t| tool_names.get(t).cloned())
            .unwrap_or_else(|| ("unknown".into(), Value::Null));
        let line_count = tur.as_str().map(|s| s.lines().count()).unwrap_or(1);
        let marker = build_marker(&id, &name, &input, kind, size, line_count, new_id, opts.drop);
        if !opts.drop {
            sidecar.push(SidecarEntry {
                id,
                slot: "toolUseResult".into(),
                name,
                input,
                content: tur,
                size,
                line_count,
                kind: kind.into(),
            });
        }
        v["toolUseResult"] = Value::String(marker);
        *modified = true;
        *count += 1;
    }
}

/// Flatten large `thinking` blocks on one (old) assistant line. Each is replaced by a small **text**
/// block carrying the marker (not a thinking block — keeping the original `signature` against altered
/// text would fail Claude's validation on resume), with the original block stashed verbatim in the
/// sidecar under `<line_key>#think<i>` (`line_key` = the line's uuid, or a file-position fallback).
#[allow(clippy::too_many_arguments)]
fn prune_assistant_thinking(
    v: &mut Value,
    opts: &PruneOptions,
    new_id: &str,
    line_key: &str,
    sidecar: &mut Vec<SidecarEntry>,
    count: &mut usize,
    tokens_saved: &mut u64,
    modified: &mut bool,
) {
    let Some(blocks) = v.pointer_mut("/message/content").and_then(Value::as_array_mut) else {
        return;
    };
    for (i, block) in blocks.iter_mut().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("thinking") {
            continue;
        }
        // Every old thinking block goes, whatever its size — `--min-size` is a payload gate and does
        // not apply here. Claude usually omits the reasoning text in the transcript but keeps a
        // ~600-byte cryptographic `signature`, and on the wire that signature-only block still costs
        // hundreds of tokens (Claude Code's own `thinking_drop` freed ~105k for 236 such blocks), so
        // a byte gate would skip exactly the blocks that matter. Commit by cloning the block out
        // (lossless), releasing the borrow before overwriting.
        let size = value_byte_size(block);
        let line_count = block
            .get("thinking")
            .and_then(Value::as_str)
            .map(|t| t.lines().count().max(1))
            .unwrap_or(1);
        let original = block.clone();
        let id = format!("{line_key}#think{i}");
        let marker = build_marker(
            &id,
            "thinking",
            &Value::Null,
            "text",
            size,
            line_count,
            new_id,
            opts.drop,
        );
        *tokens_saved += str_tokens(&original.to_string()).saturating_sub(str_tokens(&marker));
        if !opts.drop {
            sidecar.push(SidecarEntry {
                id,
                slot: "thinking".into(),
                name: "thinking".into(),
                input: Value::Null,
                content: original,
                size,
                line_count,
                kind: "text".into(),
            });
        }
        *block = serde_json::json!({ "type": "text", "text": marker });
        *modified = true;
        *count += 1;
    }
}

/// `--declassify`: snip a message's TEXT when its security-token density is high, so the resumed
/// session does not carry classifier-tripping PROSE into a fresh seat's context. Handles both message
/// shapes: a user turn's bare-string `content`, and an assistant turn's array of `text` blocks. The
/// original text is stashed in the sidecar verbatim (lossless, readable with `cv cat`, under `<line_key>#declass`)
/// and replaced with a `[PRUNED …]` marker. A block is snipped iff it contains at least
/// `declassify_min_hits` DISTINCT `declassify_tokens` (case-insensitive substring match).
#[allow(clippy::too_many_arguments)]
fn prune_declassify_text(
    v: &mut Value,
    opts: &PruneOptions,
    new_id: &str,
    line_key: &str,
    sidecar: &mut Vec<SidecarEntry>,
    count: &mut usize,
    tokens_saved: &mut u64,
    modified: &mut bool,
) {
    // Distinct security terms present (case-insensitive substring). Density, not raw frequency, so a
    // single benign mention ("the security fix shipped") never trips the snip.
    let distinct_hits = |s: &str| -> usize {
        let low = s.to_lowercase();
        opts.declassify_tokens
            .iter()
            .filter(|t| !t.is_empty() && low.contains(t.as_str()))
            .count()
    };
    let Some(content) = v.pointer_mut("/message/content") else {
        return;
    };
    match content {
        // User turn: `content` is a bare string.
        Value::String(s) => {
            if distinct_hits(s) < opts.declassify_min_hits {
                return;
            }
            let size = s.len();
            let line_count = s.lines().count().max(1);
            let original = Value::String(std::mem::take(s));
            let id = format!("{line_key}#declass");
            let marker = build_marker(
                &id,
                "declassified",
                &Value::Null,
                "text",
                size,
                line_count,
                new_id,
                opts.drop,
            );
            *tokens_saved += str_tokens(original.as_str().unwrap_or("")).saturating_sub(str_tokens(&marker));
            if !opts.drop {
                sidecar.push(SidecarEntry {
                    id,
                    slot: "content".into(),
                    name: "declassified".into(),
                    input: Value::Null,
                    content: original,
                    size,
                    line_count,
                    kind: "text".into(),
                });
            }
            *content = Value::String(marker);
            *modified = true;
            *count += 1;
        }
        // Assistant turn: `content` is an array of blocks; snip dense `text` blocks in place.
        Value::Array(blocks) => {
            for (i, block) in blocks.iter_mut().enumerate() {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if distinct_hits(text) < opts.declassify_min_hits {
                    continue;
                }
                let size = text.len();
                let line_count = text.lines().count().max(1);
                let original = block.clone();
                let id = format!("{line_key}#declass{i}");
                let marker = build_marker(
                    &id,
                    "declassified",
                    &Value::Null,
                    "text",
                    size,
                    line_count,
                    new_id,
                    opts.drop,
                );
                *tokens_saved += str_tokens(text).saturating_sub(str_tokens(&marker));
                if !opts.drop {
                    sidecar.push(SidecarEntry {
                        id,
                        slot: "content".into(),
                        name: "declassified".into(),
                        input: Value::Null,
                        content: original,
                        size,
                        line_count,
                        kind: "text".into(),
                    });
                }
                *block = serde_json::json!({ "type": "text", "text": marker });
                *modified = true;
                *count += 1;
            }
        }
        _ => {}
    }
}

/// Retrieve a stashed original from a prune sidecar by its `tool_use_id` (or `<id>#tur`). Returns the
/// verbatim value (string, content-block array incl. images, or the raw toolUseResult object). Errors
/// if the id matches more than one entry — a duplicated id means the sidecar can't say which payload
/// is which, and silently returning one of them would misattribute data.
pub fn retrieve(sidecar_path: &Path, id: &str) -> Result<Value> {
    let raw =
        std::fs::read_to_string(sidecar_path).with_context(|| format!("reading sidecar {}", sidecar_path.display()))?;
    let mut available = Vec::new();
    let mut found = None;
    let mut duplicates = 0usize;
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<SidecarEntry>(line) else {
            continue;
        };
        available.push(entry.id.clone());
        if entry.id == id {
            if found.is_some() {
                duplicates += 1;
            } else {
                found = Some(entry.content);
            }
        }
    }
    if duplicates > 0 {
        bail!(
            "id {id:?} appears {} times in sidecar {} — ambiguous, refusing to guess which payload you meant",
            duplicates + 1,
            sidecar_path.display()
        );
    }
    found.ok_or_else(|| {
        let shown: Vec<_> = available.iter().take(20).cloned().collect();
        anyhow::anyhow!("id {id:?} not found in sidecar. available: {}", shown.join(", "))
    })
}

// ── helpers ───────────────────────────────────────────────────────────────

pub(crate) fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Tool inputs can themselves contain giant file contents. Keep their source record and block
/// position, then materialize only the inputs needed by the current tool-result record.
fn collect_tool_names(v: &Value, range: Range, map: &mut HashMap<String, (String, Range, usize)>) {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for (index, b) in blocks.iter().enumerate() {
        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let Some(id) = b.get("id").and_then(Value::as_str) {
                map.insert(
                    id.to_owned(),
                    (
                        b.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
                        range,
                        index,
                    ),
                );
            }
        }
    }
}

fn materialize_tool_inputs(
    file: &std::fs::File,
    v: &Value,
    tools: &HashMap<String, (String, Range, usize)>,
) -> Result<HashMap<String, (String, Value)>> {
    let mut result = HashMap::new();
    if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if result.contains_key(id) {
                continue;
            }
            if let Some((name, range, index)) = tools.get(id) {
                let mut original: Value = serde_json::from_str(&read_line(file, *range)?)?;
                let input = original
                    .pointer_mut(&format!("/message/content/{index}/input"))
                    .map(Value::take)
                    .unwrap_or(Value::Null);
                result.insert(id.to_owned(), (name.clone(), input));
            }
        }
    }
    Ok(result)
}

/// Classify a tool_result content value → (kind, projected text, image count).
fn classify(content: &Value) -> (String, String, usize) {
    match content {
        Value::String(s) => ("text".into(), s.clone(), 0),
        Value::Array(arr) => {
            let mut text = String::new();
            let mut images = 0;
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("image") => images += 1,
                    _ => {}
                }
            }
            let kind = match (images > 0, !text.is_empty()) {
                (true, true) => "mixed",
                (true, false) => "image",
                (false, true) => "text",
                // a non-empty array of other block types is still stored verbatim (lossless)
                (false, false) if !arr.is_empty() => "text",
                _ => "none",
            };
            (kind.into(), text, images)
        }
        _ => ("none".into(), String::new(), 0),
    }
}

fn tur_is_image(tur: &Value) -> bool {
    if tur.get("type").and_then(Value::as_str) == Some("image") {
        return true;
    }
    if let Some(file) = tur.get("file") {
        if file.get("base64").is_some() {
            return true;
        }
        if file
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("image/"))
        {
            return true;
        }
    }
    false
}

fn value_byte_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        other => other.to_string().len(),
    }
}

fn str_tokens(s: &str) -> u64 {
    crate::ir::est_tokens_bytes(s.len(), TEXT_BYTES_PER_TOKEN)
}

/// Estimate the context-token cost of a content value (text + image tiles).
/// Byte-estimated tokens one line contributes to the loaded prompt: `message.content` for a turn,
/// `rendered[].content` for a model-visible attachment (Claude Code appends exactly that text to
/// the prompt; an attachment with nothing rendered costs nothing), zero for bookkeeping. Sidechain
/// filtering is the caller's job.
fn line_est_tokens(v: &Value) -> u64 {
    match v.get("type").and_then(Value::as_str) {
        Some("user" | "assistant") => v.pointer("/message/content").map(est_tokens).unwrap_or(0),
        Some("attachment") => v
            .get("rendered")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("content").and_then(Value::as_str))
                    .map(str_tokens)
                    .sum()
            })
            .unwrap_or(0),
        _ => 0,
    }
}

fn est_tokens(content: &Value) -> u64 {
    match content {
        Value::String(s) => str_tokens(s),
        Value::Array(arr) => arr
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("image") => crate::ir::IMAGE_TOKENS,
                Some("text") => str_tokens(b.get("text").and_then(Value::as_str).unwrap_or("")),
                _ => str_tokens(&b.to_string()),
            })
            .sum(),
        other => str_tokens(&other.to_string()),
    }
}

/// A compact key=value summary of the tool's most telling arguments, for the marker.
fn summarize_args(input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let key_args = [
        "file_path",
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "description",
        "prompt",
    ];
    let mut picked: Vec<String> = obj
        .iter()
        .filter(|(k, _)| key_args.contains(&k.as_str()))
        .take(2)
        .map(|(k, v)| fmt_arg(k, v))
        .collect();
    if picked.is_empty() {
        if let Some((k, v)) = obj.iter().next() {
            picked.push(fmt_arg(k, v));
        }
    }
    picked.join(", ")
}

fn fmt_arg(k: &str, v: &Value) -> String {
    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    // Truncate on a char boundary (values can contain multibyte UTF-8).
    let s = if s.chars().count() > 80 {
        format!("{}...", s.chars().take(77).collect::<String>())
    } else {
        s
    };
    format!("{k}={s}")
}

#[allow(clippy::too_many_arguments)]
fn build_marker(
    id: &str,
    name: &str,
    input: &Value,
    kind: &str,
    size: usize,
    lines: usize,
    session: &str,
    dropped: bool,
) -> String {
    let args = summarize_args(input);
    let args = if args.is_empty() {
        String::new()
    } else {
        format!(" {args}")
    };
    let label = match kind {
        "image" => "image",
        "mixed" => "text+image",
        _ => "text",
    };
    // Under --drop the payload is destroyed — never advertise a retrieval that can't work.
    let tail = if dropped {
        "dropped (no sidecar)".to_string()
    } else {
        format!("retrieve: cv cat {session} {id}")
    };
    format!("{MARKER_PREFIX}{id} tool={name}{args} | {label} {size}B/{lines}L | session={session} | {tail}]")
}

/// Recursively copy a directory tree (used to carry `subagents/`/`workflows/` into the new session).
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
