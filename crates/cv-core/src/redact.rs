//! Redact secrets / PII from a [`Session`] before sharing or export.
//!
//! The contribution loop ("send us your logs") and any enterprise use both hinge on being able to
//! scrub a transcript first. This returns a copy with secrets replaced by placeholders.
//!
//! # Approach
//!
//! One regex per recognizer finds candidates (`-----BEGIN PRIVATE KEY-----` blocks, known token
//! prefixes, `Authorization:`/`Bearer`, JWTs, emails, `key = "value"` assignments, URL
//! passwords, conservative high-entropy blobs), and a guard beside each pattern applies the
//! false-positive rules below to every candidate. The leftmost accepted candidate is replaced
//! with a stable placeholder of the form `[REDACTED:kind]`, and scanning resumes after it.
//!
//! This replaced a 627-line hand-rolled byte scanner (2026-10-11). On the 40 unit cases and on
//! 50 real sessions found by `cv search` for secret-ish words (5,828 redactions), the two produced
//! byte-identical output; the regex form is 365 code lines to 627 and 38× faster over that
//! sample (2.9 s against 109.8 s, most of it the scanner's quadratic blob walk).
//!
//! ## False-positive guardrails
//!
//! Ordinary prose and code must survive untouched. To that end:
//! * Token recognizers require a known, distinctive prefix (`sk-`, `ghp_`, `AKIA…`, etc.); short
//!   prefixes that collide with identifiers (`npm_`, `hf_`) additionally require a long, pure
//!   alphanumeric tail.
//! * Generic hex/base64 blobs are only redacted when they're long (≥ 32 chars for pure hex, ≥ 40
//!   for base64), are *pure* hex/base64url, AND a secret-ish keyword (`secret`, `token`,
//!   `password`, `api_key`, `key`) appears within ~48 chars before them. This avoids nuking
//!   commit hashes, code identifiers, and base64 data that isn't actually a credential.
//! * Assignment redaction (`password = "…"`) only fires for a small allowlist of key names, and
//!   skips values that look like code (`token = get_token()`) or short prose words
//!   (`password: use a strong one`).
//! * Connection-string passwords (`scheme://user:password@host`) are redacted; user and host stay
//!   visible.
//!
//! The transform is idempotent: a `[REDACTED:kind]` placeholder contains no characters that any
//! matcher will re-trigger on, so redacting twice yields the same string.
//!
//! OWNED BY the redact work.

use crate::ir::{Block, Message, Session};
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::OnceLock;

/// Knobs for [`redact_with`]. Defaults scrub everything.
#[derive(Debug, Clone, Default)]
pub struct RedactOptions {
    /// Which classes of secret to scrub. Defaults to all of them; narrow it to scrub only some
    /// (e.g. only PEM private keys while leaving identities/fixtures intact).
    pub classes: RedactClasses,
    /// Emit deterministic value-hashed `<REDACTED:rule:8-hex-sha256>` placeholders instead of
    /// generic `[REDACTED:kind]` labels, and propagate discovered secret values across the
    /// session so unprefixed echoed copies cannot survive.
    pub hashed: bool,
}

impl RedactOptions {
    /// Deterministic `<REDACTED:rule:sha8>` redaction across all classes (`ADR-067` Step 0).
    pub fn hashed() -> Self {
        RedactOptions {
            classes: RedactClasses::default(),
            hashed: true,
        }
    }
}

/// A toggle per secret class. `Default` enables every class (so `RedactOptions::default()` scrubs
/// everything, unchanged); use [`RedactClasses::none`]/[`RedactClasses::parse_only`] to scope it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactClasses {
    pub api_keys: bool,
    pub private_keys: bool,
    pub jwts: bool,
    pub emails: bool,
    pub blobs: bool,
    pub assignments: bool,
}

impl Default for RedactClasses {
    fn default() -> Self {
        RedactClasses {
            api_keys: true,
            private_keys: true,
            jwts: true,
            emails: true,
            blobs: true,
            assignments: true,
        }
    }
}

impl RedactClasses {
    /// No class enabled (a base to turn specific ones on).
    pub fn none() -> Self {
        RedactClasses {
            api_keys: false,
            private_keys: false,
            jwts: false,
            emails: false,
            blobs: false,
            assignments: false,
        }
    }

    /// Parse a comma-separated class list into an "only these" set, accepting singular/plural and the
    /// placeholder labels: `api_key`, `private_key`, `jwt`, `email`, `blob`/`secret`, `assignment`.
    pub fn parse_only(spec: &str) -> Result<Self, String> {
        let mut c = RedactClasses::none();
        for raw in spec.split(',') {
            let name = raw.trim().trim_end_matches('s').to_ascii_lowercase();
            match name.as_str() {
                "api_key" | "apikey" | "key" => c.api_keys = true,
                "private_key" | "privatekey" | "pem" => c.private_keys = true,
                "jwt" => c.jwts = true,
                "email" => c.emails = true,
                "blob" | "secret" => c.blobs = true,
                "assignment" => c.assignments = true,
                "" => {}
                other => return Err(format!("unknown redaction class {other:?}")),
            }
        }
        Ok(c)
    }
}

/// Counts of how many spans of each class were redacted across a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactStats {
    pub api_keys: usize,
    pub private_keys: usize,
    pub jwts: usize,
    pub emails: usize,
    pub blobs: usize,
    pub assignments: usize,
    /// Tool call IDs (`tool_use.id` / `tool_result.tool_use_id`) whose inputs or results had
    /// secrets scrubbed.
    pub redacted_tool_use_ids: std::collections::BTreeSet<String>,
}

impl RedactStats {
    /// Total number of redactions across all classes.
    pub fn total(&self) -> usize {
        self.api_keys + self.private_keys + self.jwts + self.emails + self.blobs + self.assignments
    }
}

/// Return a copy of `session` with secrets/PII scrubbed from all message + tool content.
pub fn redact(session: &Session) -> anyhow::Result<Session> {
    Ok(redact_with(session, &RedactOptions::default())?.0)
}

/// Return a copy of `session` scrubbed with deterministic `<REDACTED:rule:sha8>` tags and
/// two-pass cross-copy echo propagation across prompt, `tool_use` inputs, and `tool_result` outputs.
pub fn redact_hashed(session: &Session) -> anyhow::Result<Session> {
    Ok(redact_with(session, &RedactOptions::hashed())?.0)
}

/// Stateful multi-pass / multi-string redactor that remembers every secret value it has scrubbed
/// so echoed unprefixed copies in later messages or tool results receive the identical tag.
#[derive(Debug, Clone, Default)]
pub struct SessionRedactor {
    pub opts: RedactOptions,
    pub stats: RedactStats,
    seen: std::collections::BTreeMap<String, String>,
}

impl SessionRedactor {
    pub fn hashed() -> Self {
        Self {
            opts: RedactOptions::hashed(),
            stats: RedactStats::default(),
            seen: std::collections::BTreeMap::new(),
        }
    }

    pub fn with_options(opts: RedactOptions) -> Self {
        Self {
            opts,
            stats: RedactStats::default(),
            seen: std::collections::BTreeMap::new(),
        }
    }

    /// Scrub `s` with Pass-1 recognizers AND any previously discovered secret values.
    pub fn scrub(&mut self, s: &str) -> String {
        let pass1 = scrub_cow_tracking(s, &self.opts, &mut self.stats, Some(&mut self.seen)).into_owned();
        if !self.opts.hashed || self.seen.is_empty() {
            return pass1;
        }
        let ordered = ordered_seen(&self.seen);
        match propagate_seen_cow(&pass1, &ordered, &mut self.stats) {
            Cow::Borrowed(_) => pass1,
            Cow::Owned(pass2) => pass2,
        }
    }

    /// Observe `m` (e.g. an earlier out-of-window message while scanning up to a `cat` target)
    /// so any contextual secret assignment in it propagates to later echoes.
    pub fn observe_message(&mut self, m: &Message, resolver: &crate::Resolver) -> anyhow::Result<()> {
        let mut copy = m.clone();
        redact_message_tracking(&mut copy, &self.opts, &mut self.stats, resolver, Some(&mut self.seen))
    }

    /// Scrub `m` in place (both Pass-1 recognizers and previously discovered secrets).
    pub fn scrub_message(&mut self, m: &mut Message, resolver: &crate::Resolver) -> anyhow::Result<()> {
        redact_message_tracking(m, &self.opts, &mut self.stats, resolver, Some(&mut self.seen))?;
        if self.opts.hashed && !self.seen.is_empty() {
            let ordered = ordered_seen(&self.seen);
            propagate_message(m, &ordered, &mut self.stats)?;
        }
        Ok(())
    }

    /// Scrub a JSON value in place (both Pass-1 recognizers and previously discovered secrets).
    pub fn scrub_json(&mut self, v: &mut Value) {
        scrub_value_tracking(v, &self.opts, &mut self.stats, Some(&mut self.seen));
        if self.opts.hashed && !self.seen.is_empty() {
            let ordered = ordered_seen(&self.seen);
            propagate_value(v, &ordered, &mut self.stats);
        }
    }
}

/// Like [`redact`], but with [`RedactOptions`]; also returns [`RedactStats`].
///
/// # Scope
///
/// Scrubbed: the title and stored system prompt; every message (all text/thinking/tool blocks via [`redact_message`], plus
/// each message's harness-specific `extra` map and complete replay record); the session-level `extra` map; and `git.remote` —
/// remotes can embed credentials (`https://x-access-token:ghs_…@github.com/o/r.git`). Note that an
/// scp-style `git@host:…` remote has its `user@host` scrubbed as an email; that's cosmetic, not a
/// leak.
///
/// Not scrubbed: `cwd` and `source_path` (filesystem paths — a placeholder would corrupt them, and
/// path privacy is a separate concern from secret redaction), `model`, ids, timestamps, and
/// `git.branch`/`git.commit` (user-visible refs, not credential carriers). `principal` is also
/// preserved like other identifiers: it is an opaque identity, and its arbitrary object fields
/// carry no cv-defined semantics. Consumers must apply their own identity privacy policy before
/// sharing a principal; secret redaction does not anonymize it.
pub fn redact_with(session: &Session, opts: &RedactOptions) -> anyhow::Result<(Session, RedactStats)> {
    let mut out = session.clone();
    let resolver = session.resolver();
    let mut stats = RedactStats::default();
    let mut seen = opts.hashed.then(std::collections::BTreeMap::new);

    if let Some(title) = out.title.as_mut() {
        *title = scrub_cow_tracking(title, opts, &mut stats, seen.as_mut()).into_owned();
    }
    if let Some(prompt) = out.system_prompt.as_mut() {
        if let Cow::Owned(scrubbed) = scrub_cow_tracking(prompt, opts, &mut stats, seen.as_mut()) {
            *prompt = scrubbed;
        }
    }
    if let Some(remote) = out.git.as_mut().and_then(|g| g.remote.as_mut()) {
        if let Cow::Owned(scrubbed) = scrub_cow_tracking(remote, opts, &mut stats, seen.as_mut()) {
            *remote = scrubbed;
        }
    }
    for (_k, v) in out.extra.iter_mut() {
        scrub_value_tracking(v, opts, &mut stats, seen.as_mut());
    }
    for msg in &mut out.messages {
        redact_message_tracking(msg, opts, &mut stats, &resolver, seen.as_mut())?;
    }

    // Pass 2 (cross-copy echo propagation): any secret discovered anywhere in Pass 1 (for example,
    // `SIG_POD_REMOTE_TOKEN=<val>` inside a cross-read `.env` tool result) is also scrubbed if
    // echoed bare elsewhere in prompt, thinking, assistant text, tool inputs, or tool outputs.
    if let Some(seen) = seen.filter(|s| !s.is_empty()) {
        let ordered = ordered_seen(&seen);
        if let Some(title) = out.title.as_mut() {
            if let Cow::Owned(scrubbed) = propagate_seen_cow(title, &ordered, &mut stats) {
                *title = scrubbed;
            }
        }
        if let Some(prompt) = out.system_prompt.as_mut() {
            if let Cow::Owned(scrubbed) = propagate_seen_cow(prompt, &ordered, &mut stats) {
                *prompt = scrubbed;
            }
        }
        if let Some(remote) = out.git.as_mut().and_then(|g| g.remote.as_mut()) {
            if let Cow::Owned(scrubbed) = propagate_seen_cow(remote, &ordered, &mut stats) {
                *remote = scrubbed;
            }
        }
        for (_k, v) in out.extra.iter_mut() {
            propagate_value(v, &ordered, &mut stats);
        }
        for msg in &mut out.messages {
            propagate_message(msg, &ordered, &mut stats)?;
        }
    }

    Ok((out, stats))
}

/// Redact secrets from a single string. The core building block.
pub fn redact_text(s: &str) -> String {
    let mut stats = RedactStats::default();
    scrub(s, &RedactOptions::default(), &mut stats)
}

/// Redact secrets from a single string using deterministic `<REDACTED:rule:sha8>` placeholders.
pub fn redact_text_hashed(s: &str) -> String {
    let mut stats = RedactStats::default();
    scrub(s, &RedactOptions::hashed(), &mut stats)
}

/// Redact all string leaves of a JSON document (preserving path/id/cwd/model/branch keys) using
/// deterministic `<REDACTED:rule:sha8>` placeholders plus two-pass value propagation.
pub fn redact_json_hashed(v: &mut Value) {
    let opts = RedactOptions::hashed();
    let mut stats = RedactStats::default();
    let mut seen = std::collections::BTreeMap::new();
    scrub_api_json(v, &opts, &mut stats, &mut seen);
    if !seen.is_empty() {
        let ordered = ordered_seen(&seen);
        propagate_api_json(v, &ordered, &mut stats);
    }
}

fn is_structural_json_key(k: &str) -> bool {
    matches!(
        k,
        "id" | "session_id"
            | "parent_id"
            | "agent_id"
            | "tool_use_id"
            | "harness"
            | "path"
            | "source_path"
            | "cwd"
            | "model"
            | "role"
            | "kind"
            | "origin"
            | "type"
            | "created_at"
            | "updated_at"
            | "timestamp"
            | "last_ts"
            | "branch"
            | "commit"
            | "workflow"
            | "cursor"
    )
}

fn scrub_api_json(
    v: &mut Value,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    seen: &mut std::collections::BTreeMap<String, String>,
) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(scrubbed) = scrub_cow_tracking(s, opts, stats, Some(seen)) {
                *s = scrubbed;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_api_json(item, opts, stats, seen);
            }
        }
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_structural_json_key(k) {
                    continue;
                }
                scrub_api_json(val, opts, stats, seen);
            }
        }
        _ => {}
    }
}

fn propagate_api_json(v: &mut Value, ordered: &[(String, String)], stats: &mut RedactStats) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(scrubbed) = propagate_seen_cow(s, ordered, stats) {
                *s = scrubbed;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                propagate_api_json(item, ordered, stats);
            }
        }
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_structural_json_key(k) {
                    continue;
                }
                propagate_api_json(val, ordered, stats);
            }
        }
        _ => {}
    }
}

/// Redact secrets from a single message in place. Public so streaming consumers (dataset export) can
/// scrub one message at a time without materializing the whole session.
pub fn redact_message(
    msg: &mut Message,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    resolver: &crate::Resolver,
) -> anyhow::Result<()> {
    redact_message_tracking(msg, opts, stats, resolver, None)
}

fn redact_message_tracking(
    msg: &mut Message,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    resolver: &crate::Resolver,
    mut seen: Option<&mut std::collections::BTreeMap<String, String>>,
) -> anyhow::Result<()> {
    // Resolve fallibly before changing text or statistics. Earlier successful fields may
    // become owned, but a late failed source must never become a successful scrub.
    msg.try_materialize_text(resolver)?;
    if let Some(record) = &mut msg.complete_record {
        let record = std::sync::Arc::make_mut(record);
        record.value.make_owned()?;
        // Redaction invalidates the original replay witness; the scrubbed owned native record
        // remains available to codec comparison without retaining secret-bearing descriptors.
        record.projection.clear();
    }
    // Validate every demanded input before changing text or statistics. A failed read leaves
    // source descriptors intact; earlier successful values may become owned without changing data.
    for block in &mut msg.content {
        if let Block::ToolUse { input, .. } = block {
            input.make_owned()?;
        }
    }
    for block in &mut msg.content {
        match block {
            Block::Text { text } | Block::Thinking { text, .. } => {
                // `scrub_cow` borrows when clean, so untouched text (the common case) is not
                // reallocated — only actually-redacted fields are rewritten.
                if let Cow::Owned(scrubbed) = scrub_cow_tracking(text, opts, stats, seen.as_deref_mut()) {
                    *text = scrubbed.into();
                }
            }
            Block::ToolUse { id, input, .. } => {
                let before = stats.total();
                scrub_value_tracking(input.make_owned()?, opts, stats, seen.as_deref_mut());
                if stats.total() > before {
                    stats.redacted_tool_use_ids.insert(id.clone());
                }
            }
            Block::ToolResult {
                tool_use_id,
                content,
                details,
                ..
            } => {
                let before = stats.total();
                if let Cow::Owned(scrubbed) = scrub_cow_tracking(content, opts, stats, seen.as_deref_mut()) {
                    *content = scrubbed.into();
                }
                if let Some(d) = details {
                    scrub_value_tracking(d, opts, stats, seen.as_deref_mut());
                }
                if stats.total() > before {
                    stats.redacted_tool_use_ids.insert(tool_use_id.clone());
                }
            }
            Block::Image { .. } | Block::File { .. } => {}
        }
    }
    if let Some(record) = &mut msg.complete_record {
        scrub_value_tracking(
            std::sync::Arc::make_mut(record).value.make_owned()?,
            opts,
            stats,
            seen.as_deref_mut(),
        );
    }
    // Harness-specific passthrough fields can carry anything the harness logged (env snapshots,
    // request metadata) — scrub their string leaves too.
    for (_k, v) in msg.extra.iter_mut() {
        scrub_value_tracking(v, opts, stats, seen.as_deref_mut());
    }
    Ok(())
}

/// Recursively scrub string values inside a JSON value, leaving keys and structure intact.
/// Crate-visible so per-block redacting renderers (dataset export) can scrub a tool input without
/// going through [`redact_message`] on a cloned message.
pub(crate) fn scrub_value(v: &mut Value, opts: &RedactOptions, stats: &mut RedactStats) {
    scrub_value_tracking(v, opts, stats, None);
}

fn scrub_value_tracking(
    v: &mut Value,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    mut seen: Option<&mut std::collections::BTreeMap<String, String>>,
) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(scrubbed) = scrub_cow_tracking(s, opts, stats, seen) {
                *s = scrubbed;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_value_tracking(item, opts, stats, seen.as_deref_mut());
            }
        }
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                scrub_value_tracking(val, opts, stats, seen.as_deref_mut());
            }
        }
        // numbers, bools, null: nothing to scrub.
        _ => {}
    }
}

const PLACEHOLDER_OPEN: &str = "[REDACTED:";
const HASHED_PLACEHOLDER_OPEN: &str = "<REDACTED:";

fn is_placeholder_str(s: &str) -> bool {
    s.starts_with(PLACEHOLDER_OPEN) || s.starts_with(HASHED_PLACEHOLDER_OPEN)
}

fn ordered_seen(seen: &std::collections::BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = seen.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    pairs
}

fn placeholder_span_at(s: &str, idx: usize) -> Option<usize> {
    let rest = &s[idx..];
    if rest.starts_with(PLACEHOLDER_OPEN) {
        return rest.find(']').map(|end| idx + end + 1);
    }
    if rest.starts_with(HASHED_PLACEHOLDER_OPEN) {
        return rest.find('>').map(|end| idx + end + 1);
    }
    None
}

fn propagate_seen_cow<'a>(s: &'a str, ordered: &[(String, String)], stats: &mut RedactStats) -> Cow<'a, str> {
    if ordered.is_empty() || s.len() < 8 {
        return Cow::Borrowed(s);
    }
    if !ordered.iter().any(|(secret, _)| s.contains(secret.as_str())) {
        return Cow::Borrowed(s);
    }
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut cur = 0usize;
    while cur < s.len() {
        if let Some(after_ph) = placeholder_span_at(s, cur) {
            cur = after_ph;
            continue;
        }
        let mut earliest: Option<(usize, &str, &str)> = None;
        for (secret, ph) in ordered {
            if let Some(rel) = s[cur..].find(secret.as_str()) {
                let pos = cur + rel;
                // Do not match inside an existing placeholder.
                if s[..pos]
                    .rfind(HASHED_PLACEHOLDER_OPEN)
                    .is_some_and(|open| placeholder_span_at(s, open).is_some_and(|close| pos < close))
                    || s[..pos]
                        .rfind(PLACEHOLDER_OPEN)
                        .is_some_and(|open| placeholder_span_at(s, open).is_some_and(|close| pos < close))
                {
                    continue;
                }
                if earliest.is_none_or(|(best_pos, best_sec, _)| {
                    pos < best_pos || (pos == best_pos && secret.len() > best_sec.len())
                }) {
                    earliest = Some((pos, secret.as_str(), ph.as_str()));
                    if pos == cur {
                        break;
                    }
                }
            }
        }
        let Some((pos, secret, ph)) = earliest else {
            break;
        };
        let o = out.get_or_insert_with(|| String::with_capacity(s.len()));
        o.push_str(&s[copied..pos]);
        o.push_str(ph);
        stats.blobs += 1;
        copied = pos + secret.len();
        cur = copied;
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut o) => {
            o.push_str(&s[copied..]);
            Cow::Owned(o)
        }
    }
}

fn propagate_value(v: &mut Value, ordered: &[(String, String)], stats: &mut RedactStats) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(scrubbed) = propagate_seen_cow(s, ordered, stats) {
                *s = scrubbed;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                propagate_value(item, ordered, stats);
            }
        }
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                propagate_value(val, ordered, stats);
            }
        }
        _ => {}
    }
}

fn propagate_message(msg: &mut Message, ordered: &[(String, String)], stats: &mut RedactStats) -> anyhow::Result<()> {
    for block in &mut msg.content {
        match block {
            Block::Text { text } | Block::Thinking { text, .. } => {
                if let Cow::Owned(scrubbed) = propagate_seen_cow(text, ordered, stats) {
                    *text = scrubbed.into();
                }
            }
            Block::ToolUse { id, input, .. } => {
                let before = stats.total();
                propagate_value(input.make_owned()?, ordered, stats);
                if stats.total() > before {
                    stats.redacted_tool_use_ids.insert(id.clone());
                }
            }
            Block::ToolResult {
                tool_use_id,
                content,
                details,
                ..
            } => {
                let before = stats.total();
                if let Cow::Owned(scrubbed) = propagate_seen_cow(content, ordered, stats) {
                    *content = scrubbed.into();
                }
                if let Some(d) = details {
                    propagate_value(d, ordered, stats);
                }
                if stats.total() > before {
                    stats.redacted_tool_use_ids.insert(tool_use_id.clone());
                }
            }
            Block::Image { .. } | Block::File { .. } => {}
        }
    }
    if let Some(record) = &mut msg.complete_record {
        propagate_value(std::sync::Arc::make_mut(record).value.make_owned()?, ordered, stats);
    }
    for (_k, v) in msg.extra.iter_mut() {
        propagate_value(v, ordered, stats);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Core scanner
// ---------------------------------------------------------------------------

/// Walk `s` once, replacing every recognized secret span with a placeholder.
fn scrub(s: &str, opts: &RedactOptions, stats: &mut RedactStats) -> String {
    scrub_cow(s, opts, stats).into_owned()
}

/// [`scrub`], allocation-aware: returns `Cow::Borrowed(s)` when nothing matched (the overwhelmingly
/// common case for ordinary transcript text), and copies clean stretches between matches in bulk.
///
/// Each enabled [`Rule`] keeps its earliest guarded hit at or after the cursor; the leftmost hit
/// wins, and at a tie the earlier rule (the order of [`rules`]) wins. An existing placeholder is a
/// rule too (the first), consumed verbatim, which is what makes the transform idempotent.
pub(crate) fn scrub_cow<'a>(s: &'a str, opts: &RedactOptions, stats: &mut RedactStats) -> Cow<'a, str> {
    scrub_cow_tracking(s, opts, stats, None)
}

fn canonical_secret_value(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() > 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        let rest = trimmed[7..].trim();
        if !rest.is_empty() {
            return rest;
        }
    }
    trimmed
}

fn classify_hashed_rule(secret: &str, fallback_rule: &'static str) -> &'static str {
    if secret.starts_with("-----BEGIN ") && secret.contains("PRIVATE KEY") {
        return "private_key";
    }
    if (secret.starts_with("AKIA") || secret.starts_with("ASIA"))
        && secret.len() >= 20
        && secret[4..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return "aws";
    }
    for (prefix, min_tail, alnum, rule_name) in PREFIXES {
        if let Some(tail) = secret.strip_prefix(prefix) {
            let n = if *alnum {
                tail.bytes().take_while(u8::is_ascii_alphanumeric).count()
            } else {
                tail.bytes().take_while(|b| is_token_char(*b)).count()
            };
            if n >= *min_tail {
                return rule_name;
            }
        }
    }
    if secret.starts_with("eyJ") && secret.len() >= 40 && secret.matches('.').count() == 2 {
        return "jwt";
    }
    fallback_rule
}

fn format_hashed_placeholder(secret: &str, fallback_rule: &'static str) -> String {
    let rule = classify_hashed_rule(secret, fallback_rule);
    let digest = crate::digest::sha256_hex(secret.as_bytes());
    format!("<REDACTED:{rule}:{}>", &digest[..8])
}

fn scrub_cow_tracking<'a>(
    s: &'a str,
    opts: &RedactOptions,
    stats: &mut RedactStats,
    mut seen: Option<&mut std::collections::BTreeMap<String, String>>,
) -> Cow<'a, str> {
    let rules: Vec<&Rule> = rules().iter().filter(|r| r.kind.enabled(opts)).collect();
    let mut next: Vec<Option<Hit>> = rules.iter().map(|r| r.next_hit(s, 0)).collect();
    // The redacted output, allocated lazily on the first match; `copied` is how much of `s` has
    // already been flushed into it.
    let (mut out, mut copied, mut cur) = (None::<String>, 0, 0);
    loop {
        for (r, h) in rules.iter().zip(next.iter_mut()) {
            if h.as_ref().is_some_and(|h| h.start < cur) {
                *h = r.next_hit(s, cur);
            }
        }
        let Some(hit) = next.iter().flatten().min_by_key(|h| h.start) else {
            break;
        };
        let Hit {
            start,
            keep,
            end,
            secret_start,
            kind,
            rule_name,
        } = *hit;
        cur = end.max(start + 1);
        let Some(label) = kind.label() else { continue }; // a placeholder: leave it in place
        let o = out.get_or_insert_with(String::new);
        if opts.hashed {
            // For `Bearer <tok>` in hashed mode, keep `Bearer ` (`s[copied..secret_start]`) when
            // `secret_start > keep` so headers stay readable while hashing `<tok>` directly.
            let prefix_end = if rule_name == "bearer" && secret_start > keep {
                secret_start
            } else {
                keep
            };
            let raw_secret = canonical_secret_value(&s[secret_start..end]);
            let ph = format_hashed_placeholder(raw_secret, rule_name);
            o.push_str(&s[copied..prefix_end]);
            o.push_str(&ph);
            if let Some(seen) = seen.as_deref_mut() {
                if raw_secret.len() >= 8 && kind != Kind::Email {
                    seen.entry(raw_secret.to_string()).or_insert(ph);
                }
            }
        } else {
            // The clean stretch plus any prefix the rule preserves (e.g. `Authorization: `).
            o.push_str(&s[copied..keep]);
            o.push_str(PLACEHOLDER_OPEN);
            o.push_str(label);
            o.push(']');
        }
        kind.bump(stats);
        copied = end;
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut o) => {
            o.push_str(&s[copied..]);
            Cow::Owned(o)
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// An existing `[REDACTED:…]` placeholder, kept as is.
    Placeholder,
    ApiKey,
    PrivateKey,
    Jwt,
    Email,
    Blob,
    Assignment,
}

impl Kind {
    fn enabled(self, opts: &RedactOptions) -> bool {
        let c = &opts.classes;
        match self {
            Kind::Placeholder => true,
            Kind::PrivateKey => c.private_keys,
            Kind::Assignment => c.assignments,
            Kind::ApiKey => c.api_keys,
            Kind::Jwt => c.jwts,
            Kind::Email => c.emails,
            Kind::Blob => c.blobs,
        }
    }
    fn label(self) -> Option<&'static str> {
        Some(match self {
            Kind::Placeholder => return None,
            Kind::ApiKey => "api_key",
            Kind::PrivateKey => "private_key",
            Kind::Jwt => "jwt",
            Kind::Email => "email",
            Kind::Blob | Kind::Assignment => "secret",
        })
    }
    fn bump(self, stats: &mut RedactStats) {
        match self {
            Kind::Placeholder => {}
            Kind::ApiKey => stats.api_keys += 1,
            Kind::PrivateKey => stats.private_keys += 1,
            Kind::Jwt => stats.jwts += 1,
            Kind::Email => stats.emails += 1,
            Kind::Blob => stats.blobs += 1,
            Kind::Assignment => stats.assignments += 1,
        }
    }
}

/// A guarded match: replace `[keep, end)` with a placeholder, keeping `[start, keep)` verbatim.
#[derive(Clone, Copy)]
struct Hit {
    start: usize,
    keep: usize,
    end: usize,
    secret_start: usize,
    kind: Kind,
    rule_name: &'static str,
}

type GuardMatch = (usize, usize, usize, &'static str);

/// A candidate pattern plus the guard that accepts or rejects each candidate. A rejected
/// candidate is retried one character later, so a guard sees every start a char-by-char scan
/// would have tried.
struct Rule {
    kind: Kind,
    re: Regex,
    /// `Some((keep, end, secret_start, rule_name))` (absolute offsets in `s`) accepts the candidate.
    guard: fn(&str, &Captures) -> Option<GuardMatch>,
}

impl Rule {
    /// The earliest accepted hit at or after `from`.
    fn next_hit(&self, s: &str, mut from: usize) -> Option<Hit> {
        if self.kind == Kind::Blob {
            return next_blob(&self.re, s, from);
        }
        while from <= s.len() {
            let c = self.re.captures_at(s, from)?;
            let start = c.get(0)?.start();
            if let Some((keep, end, secret_start, rule_name)) = (self.guard)(s, &c) {
                return Some(Hit {
                    start,
                    keep,
                    end,
                    secret_start,
                    kind: self.kind,
                    rule_name,
                });
            }
            from = start + s[start..].chars().next().map_or(1, char::len_utf8);
        }
        None
    }
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_base64_std_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

/// True if the byte just before `i` is not part of a token (so we match at a boundary).
fn boundary_before(s: &str, i: usize) -> bool {
    i == 0 || !is_token_char(s.as_bytes()[i - 1])
}

/// The whole match, at a token boundary.
fn whole(s: &str, c: &Captures, rule_name: &'static str) -> Option<GuardMatch> {
    let m = c.get(0)?;
    boundary_before(s, m.start()).then_some((m.start(), m.end(), m.start(), rule_name))
}

/// Group `g` as the redacted value (the rest of the match is kept), unless already redacted.
fn value(c: &Captures, g: usize, rule_name: &'static str) -> Option<GuardMatch> {
    let v = c.get(g)?;
    (!is_placeholder_str(v.as_str())).then_some((v.start(), v.end(), v.start(), rule_name))
}

/// Known credential prefixes with the minimum tail each needs, whether the tail is strictly
/// alphanumeric, and the rule family name used in `<REDACTED:rule:sha8>`.
/// Order matters only for prefix-of-prefix pairs (`sk-ant-` before `sk-`).
const PREFIXES: &[(&str, usize, bool, &str)] = &[
    ("sk-ant-", 10, false, "anthropic"),
    ("sk-", 10, false, "openai"),
    ("sk_live_", 20, true, "stripe"), // Stripe secret key (live)
    ("rk_live_", 20, true, "stripe"), // Stripe restricted key (live)
    ("xoxb-", 8, false, "slack"),
    ("xoxp-", 8, false, "slack"),
    ("xoxa-", 8, false, "slack"),
    ("xoxr-", 8, false, "slack"),
    ("xoxc-", 8, false, "slack"), // Slack client token
    ("xoxs-", 8, false, "slack"), // Slack session token
    ("xapp-", 8, false, "slack"), // Slack app-level token
    ("ghp_", 20, false, "github"),
    ("gho_", 20, false, "github"),
    ("ghs_", 20, false, "github"),
    ("ghu_", 20, false, "github"),
    ("ghr_", 20, false, "github"),
    ("github_pat_", 20, false, "github"),
    ("glpat-", 20, false, "gitlab"), // GitLab personal access token
    ("lin_api_", 20, false, "linear"),
    ("kgm_", 16, false, "kagi"),
    ("sbp_", 20, false, "supabase"),
    ("pypi-AgEI", 30, false, "pypi"),
    ("tskey-", 16, false, "tailscale"),
    ("AGE-SECRET-KEY-1", 30, true, "age"),
    ("npm_", 36, true, "npm"),        // npm: npm_ + 36 base62
    ("hf_", 30, true, "huggingface"), // Hugging Face: hf_ + 34 base62
    ("gsk_", 40, true, "groq"),       // Groq: gsk_ + 52 base62
    ("xai-", 40, true, "xai"),        // xAI: xai- + ~80 base62
    ("dop_v1_", 40, true, "digitalocean"),
    ("shpat_", 32, true, "shopify"),
    ("shpss_", 32, true, "shopify"),
    ("AIza", 30, false, "google"),
];

/// The recognizers, in priority order at a shared start: placeholders, private keys (they swallow
/// a block), assignments and URL passwords (which keep a prefix), headers and bare tokens, JWTs,
/// emails, then conservative blobs.
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let rule = |kind, pat: &str, guard| Rule {
            kind,
            re: Regex::new(pat).expect("redact pattern"),
            guard,
        };
        let prefixes: Vec<String> = PREFIXES.iter().map(|(p, _, _, _)| regex::escape(p)).collect();
        vec![
            rule(
                Kind::Placeholder,
                r"\[REDACTED:[^\]]*\]|<REDACTED:[A-Za-z0-9_]+:[0-9a-f]{8}>",
                |_, c| {
                    let m = c.get(0)?;
                    Some((m.start(), m.end(), m.start(), "placeholder"))
                },
            ),
            // `-----BEGIN … PRIVATE KEY-----` through its END marker. Transcripts routinely clip
            // tool output, losing the END line; a truncated key is still a key, so without one
            // the header plus the base64 body that follows is redacted.
            rule(Kind::PrivateKey, r"-----BEGIN ", |s, c| {
                let start = c.get(0)?.start();
                let after = &s[start..];
                let close = after[5..].find("-----")? + 10;
                after[..close].contains("PRIVATE KEY").then_some(())?;
                let from = start + close;
                let footer = s[from..].match_indices("-----END ").find_map(|(offset, _)| {
                    let marker = &s[from + offset..];
                    let close = marker[5..].find("-----")? + 10;
                    marker[..close].contains("PRIVATE KEY").then_some(from + offset + close)
                });
                let end = match footer {
                    Some(end) => end,
                    None => truncated_pem_body_end(s, from)?,
                };
                Some((start, end, start, "private_key"))
            }),
            // `key = "value"` / `key: value` for an allowlist of secret-ish keys; the value goes,
            // `key = "` and the closing quote stay. Skips calls (`token = get_token()`), short
            // bare words (`password: use a strong one`), an unterminated quote, and values under
            // three characters.
            rule(
                Kind::Assignment,
                r#"(?i)(?:password|passwd|secret|api_key|apikey|api-key|access_token|secret_key|client_secret|auth_token|token)[ \t]*[=:][ \t]*(?:"([^"]*)"|'([^']*)'|([^ \t\r\n,;)]+))"#,
                |s, c| {
                    boundary_before(s, c.get(0)?.start()).then_some(())?;
                    let v = match c.get(1).or_else(|| c.get(2)) {
                        Some(quoted) => quoted,
                        None => {
                            let v = c.get(3)?;
                            let t = v.as_str().as_bytes();
                            if t.contains(&b'(') || t[0] == b'"' || t[0] == b'\'' || t[0] == b'$' {
                                return None;
                            }
                            if t.len() < 8 && t.iter().all(u8::is_ascii_alphabetic) {
                                return None;
                            }
                            v
                        }
                    };
                    (v.len() >= 3 && !is_placeholder_str(v.as_str())).then_some((
                        v.start(),
                        v.end(),
                        v.start(),
                        "secret",
                    ))
                },
            ),
            // Compound uppercase environment variable secret assignments (`SIG_POD_REMOTE_TOKEN=…`,
            // `SUPABASE_SERVICE_KEY=…`, `MY_CLIENT_SECRET=…`). Keeps the variable name and `=`;
            // scrubs values ≥ 8 chars while skipping code expressions and `$VAR` references.
            rule(
                Kind::Assignment,
                r#"[A-Z][A-Z0-9_]*(?:_SECRET|_TOKEN|_PASSWORD|_PASSWD|_API_KEY|_APIKEY|_PRIVATE_KEY|_ACCESS_KEY|_CLIENT_SECRET)[ \t]*[=:][ \t]*(?:"([^"\r\n]*)"|'([^'\r\n]*)'|([^ \t\r\n,;)}"'`]+))"#,
                |s, c| {
                    boundary_before(s, c.get(0)?.start()).then_some(())?;
                    let v = match c.get(1).or_else(|| c.get(2)) {
                        Some(quoted) => {
                            if quoted.as_str().starts_with('$') {
                                return None;
                            }
                            quoted
                        }
                        None => {
                            let v = c.get(3)?;
                            let t = v.as_str().as_bytes();
                            if t.contains(&b'(')
                                || t.contains(&b'[')
                                || v.as_str().contains("::")
                                || matches!(t[0], b'"' | b'\'' | b'$')
                            {
                                return None;
                            }
                            v
                        }
                    };
                    (v.len() >= 8 && !is_placeholder_str(v.as_str())).then_some((
                        v.start(),
                        v.end(),
                        v.start(),
                        "secret",
                    ))
                },
            ),
            // `scheme://user:password@host`: the password goes, scheme, user and host stay. A port
            // (`https://host:8080/path`) never matches, and an env-var reference (`${DB_PASS}`)
            // is not a secret.
            rule(
                Kind::Assignment,
                r#"[A-Za-z][A-Za-z0-9+.-]*://[^:@/ \t\r\n"']*:([^$@/ \t\r\n"'][^@/ \t\r\n"']*)@"#,
                |s, c| {
                    boundary_before(s, c.get(0)?.start()).then_some(())?;
                    value(c, 1, "secret")
                },
            ),
            // `Authorization: <value>` to end of line; `Proxy-Authorization:` too, not
            // `FooAuthorization:`.
            rule(Kind::ApiKey, r"(?i)authorization:[ \t]*([^ \t\r\n][^\r\n]*)", |s, c| {
                let i = c.get(0)?.start();
                (i == 0 || !matches!(s.as_bytes()[i - 1], b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_'))
                    .then_some(())?;
                value(c, 1, "bearer")
            }),
            // `Bearer <token>`, any case (RFC 7235). Prose guard: "bearer instruments are…" —
            // real tokens are long or contain digits/symbols.
            rule(Kind::ApiKey, r"(?i)bearer +([A-Za-z0-9_.-]{8,})", |s, c| {
                let tok = c.get(1)?;
                let t = tok.as_str();
                (t.len() >= 16 || !t.bytes().all(|b| b.is_ascii_alphabetic())).then_some(())?;
                let m = c.get(0)?;
                boundary_before(s, m.start()).then_some((m.start(), m.end(), tok.start(), "bearer"))
            }),
            // AWS access key ids (the whole uppercase run, so an overlong key leaves no tail),
            // then the prefix table.
            rule(
                Kind::ApiKey,
                &format!(r"(?:AKIA|ASIA)[A-Z0-9]{{16,}}|(?:{})[A-Za-z0-9_-]*", prefixes.join("|")),
                |s, c| {
                    let m = c.get(0)?;
                    if m.as_str().starts_with("AKIA") || m.as_str().starts_with("ASIA") {
                        return whole(s, c, "aws");
                    }
                    let (p, min, alnum, rule_name) =
                        PREFIXES.iter().find(|(p, _, _, _)| m.as_str().starts_with(p))?;
                    let tail = &m.as_str()[p.len()..];
                    let n = if *alnum {
                        tail.bytes().take_while(u8::is_ascii_alphanumeric).count()
                    } else {
                        tail.len()
                    };
                    let end = m.start() + p.len() + n;
                    (n >= *min && boundary_before(s, m.start())).then_some((m.start(), end, m.start(), *rule_name))
                },
            ),
            // JWT: three base64url segments (the header starts `eyJ`, base64 of `{"`), 40+ chars.
            rule(
                Kind::Jwt,
                r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}",
                |s, c| {
                    let m = c.get(0)?;
                    (m.len() >= 40 && m.as_str().find('.')? >= 4).then_some(())?;
                    whole(s, c, "jwt")
                },
            ),
            // localpart@domain.tld, the TLD two or more letters.
            rule(Kind::Email, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]*\.[A-Za-z0-9.-]*", |s, c| {
                let m = c.get(0)?;
                let local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
                (m.start() == 0 || !local(s.as_bytes()[m.start() - 1])).then_some(())?;
                let t = m.as_str().trim_end_matches('.');
                let tld = t.rsplit('.').next()?;
                let end = m.start() + t.len();
                (tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()))
                    .then_some((m.start(), end, m.start(), "email"))
            }),
            // Runs of base64/hex characters; see [`next_blob`].
            rule(Kind::Blob, r"[A-Za-z0-9+/=_-]{32,}", |_, _| None),
        ]
    })
}

/// A long, *pure* hex (≥ 32) or base64 (≥ 40) run with a secret-ish keyword within ~48 chars
/// before it — commit hashes, identifiers and base64 data that isn't a credential survive. Every
/// token boundary inside a run is a candidate start (after a `/`, `+` or `=`), so the purity of
/// each suffix is computed once per run: a per-start re-measure was quadratic, 21 s on one 8 MB
/// transcript.
fn next_blob(re: &Regex, s: &str, mut from: usize) -> Option<Hit> {
    let b = s.as_bytes();
    loop {
        let m = re.find_at(s, from)?;
        let (start, end) = (m.start(), m.end());
        // Each predicate stays true for every suffix beginning after its last invalid byte.
        // Retain three offsets, not a tuple for every byte of a potentially giant clean token.
        let mut pure_from = [start; 3];
        for (q, &c) in b.iter().enumerate().take(end).skip(start) {
            for (index, valid) in [c.is_ascii_hexdigit(), is_token_char(c), is_base64_std_char(c)]
                .into_iter()
                .enumerate()
            {
                if !valid {
                    pure_from[index] = q + 1;
                }
            }
        }
        for p in start..end.saturating_sub(31) {
            let [h, u, d] = pure_from.map(|from| p >= from);
            if (h || u || d) && end - p >= if h { 32 } else { 40 } && boundary_before(s, p) && keyword_before(s, p, 48)
            {
                return Some(Hit {
                    start: p,
                    keep: p,
                    end,
                    secret_start: p,
                    kind: Kind::Blob,
                    rule_name: "secret",
                });
            }
        }
        from = end;
    }
}

/// End offset of the contiguous PEM body following a truncated `-----BEGIN …-----` header at
/// `body_start`, or `None` if no body follows (a bare marker in prose stays untouched).
///
/// A body line is a whole line of base64 characters. To avoid eating prose after a marker
/// mention, the first body lines must be ≥ 16 chars; once one real body line is seen, a single
/// shorter trailing base64 fragment (the clipped tail) is also consumed. Encrypted-PEM header
/// lines (`Proc-Type: …`, `DEK-Info: …` — the latter carries the IV) before the body are
/// consumed too.
fn truncated_pem_body_end(s: &str, body_start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut pos = body_start;
    let mut end: Option<usize> = None;
    let mut seen_body = false;
    while pos < bytes.len() {
        // Skip line terminators between lines.
        while pos < bytes.len() && (bytes[pos] == b'\n' || bytes[pos] == b'\r') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let line_start = pos;
        let mut line_end = pos;
        while line_end < bytes.len() && bytes[line_end] != b'\n' && bytes[line_end] != b'\r' {
            line_end += 1;
        }
        let line = s[line_start..line_end].trim();
        if line.is_empty() {
            pos = line_end;
            continue;
        }
        if line.bytes().all(is_base64_std_char) {
            if line.len() >= 16 {
                seen_body = true;
                end = Some(line_end);
                pos = line_end;
                continue;
            }
            if seen_body {
                // A short base64 fragment right after real body lines: the clipped tail.
                end = Some(line_end);
            }
            break;
        }
        // Encrypted-PEM header line ("Proc-Type: 4,ENCRYPTED" / "DEK-Info: AES-128-CBC,…")
        // before the body: key has no spaces, then ": ".
        if !seen_body {
            if let Some(colon) = line.find(": ") {
                if !line[..colon].contains(' ') && line.len() < 64 {
                    end = Some(line_end);
                    pos = line_end;
                    continue;
                }
            }
        }
        break;
    }
    if seen_body {
        end
    } else {
        None
    }
}

/// Does a secret-ish keyword appear in the `window` chars (roughly) before `i`?
fn keyword_before(s: &str, i: usize, window: usize) -> bool {
    let mut start = i.saturating_sub(window);
    // Snap `start` back to a char boundary so slicing never panics on multi-byte UTF-8.
    while start > 0 && !s.is_char_boundary(start) {
        start -= 1;
    }
    let ctx = s[start..i].to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "key",
        "credential",
        "auth",
    ];
    NEEDLES.iter().any(|n| ctx.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, GitInfo, Harness, Message, Role, Session};

    fn empty_session() -> Session {
        Session {
            ..Session::new("t", Harness::Claude)
        }
    }

    fn redact_one(s: &str) -> String {
        redact_text(s)
    }

    #[test]
    fn redacts_openai_key() {
        let out = redact_one("my key is sk-abcDEF1234567890ghijkl done");
        assert!(!out.contains("sk-abcDEF1234567890ghijkl"), "got: {out}");
        assert!(out.contains("[REDACTED:api_key]"), "got: {out}");
        assert!(out.starts_with("my key is "));
        assert!(out.ends_with(" done"));
    }

    #[test]
    fn redacts_anthropic_key() {
        let out = redact_one("sk-ant-api03-aaaaaaaaaaaaaaaaaaaabbbb");
        assert_eq!(out, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_slack_tokens() {
        for t in ["xoxb-1234-5678-abcdEFGHijklmnop", "xoxp-1111-2222-zzzzzzzzzzzz"] {
            let out = redact_one(t);
            assert_eq!(out, "[REDACTED:api_key]", "token {t}");
        }
    }

    #[test]
    fn redacts_github_tokens() {
        let out = redact_one("ghp_0123456789abcdefABCDEF0123456789abcd");
        assert_eq!(out, "[REDACTED:api_key]");
        let out2 = redact_one("gho_0123456789abcdefABCDEF0123456789abcd");
        assert_eq!(out2, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_aws_access_key() {
        let out = redact_one("AKIAIOSFODNN7EXAMPLE rest");
        assert!(out.starts_with("[REDACTED:api_key]"), "got: {out}");
        assert!(out.ends_with(" rest"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_google_key() {
        let out = redact_one("AIzaSyA1234567890abcdefghijklmnopqrstuv");
        assert_eq!(out, "[REDACTED:api_key]");
    }

    #[test]
    fn redacts_bearer_and_authorization() {
        let out = redact_one("Bearer abcdef1234567890XYZ");
        assert!(out.contains("[REDACTED:api_key]"), "got: {out}");
        assert!(!out.contains("abcdef1234567890XYZ"));
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_one(jwt);
        assert_eq!(out, "[REDACTED:jwt]", "got: {out}");
    }

    #[test]
    fn redacts_email() {
        let out = redact_one("contact me at jane.doe+spam@example.co.uk please");
        assert!(!out.contains("jane.doe+spam@example.co.uk"), "got: {out}");
        assert!(out.contains("[REDACTED:email]"));
        assert!(out.starts_with("contact me at "));
        assert!(out.ends_with(" please"));
    }

    #[test]
    fn emails_class_off_keeps_emails() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "ping a@b.com".into(),
        });
        sess.messages.push(m);
        let (out, _stats) = redact_with(
            &sess,
            &RedactOptions {
                classes: RedactClasses {
                    emails: false,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let t = out.messages[0].text().unwrap();
        assert!(t.contains("a@b.com"), "got: {t}");
    }

    #[test]
    fn scoped_classes_only_private_keys() {
        // The exact ask: scrub PEM blocks, leave email/identity and api-key fixtures untouched.
        let opts = RedactOptions {
            classes: RedactClasses::parse_only("private_key").unwrap(),
            ..Default::default()
        };
        let text = "mail me@example.com key sk-abcDEF1234567890ghijkl\n-----BEGIN PRIVATE KEY-----\nMIIBVgIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\ndone";
        let mut stats = RedactStats::default();
        let out = scrub_cow(text, &opts, &mut stats);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(out.contains("me@example.com"), "email must survive: {out}");
        assert!(out.contains("sk-abcDEF1234567890ghijkl"), "api key must survive: {out}");
        assert_eq!(stats.private_keys, 1);
        assert_eq!(stats.emails, 0);
        assert_eq!(stats.api_keys, 0);
    }

    #[test]
    fn parse_only_rejects_garbage_and_trims_plurals() {
        assert_eq!(
            RedactClasses::parse_only("private_keys, emails").unwrap(),
            RedactClasses {
                private_keys: true,
                emails: true,
                ..RedactClasses::none()
            }
        );
        assert!(RedactClasses::parse_only("nope").is_err());
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890\nabcdefghijklmnopqrstuvwxyz\n-----END RSA PRIVATE KEY-----\nafter";
        let out = redact_one(pem);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "got: {out}");
        assert!(!out.contains("BEGIN RSA PRIVATE KEY"), "got: {out}");
        assert!(out.starts_with("before\n"));
        assert!(out.ends_with("\nafter"));
    }

    #[test]
    fn redacts_assignment_value() {
        let out = redact_one(r#"password = "hunter2supersecret""#);
        assert!(!out.contains("hunter2supersecret"), "got: {out}");
        assert!(out.contains("[REDACTED:"), "got: {out}");

        let out2 = redact_one(r#"api_key: "abcdEFGH12345678""#);
        assert!(!out2.contains("abcdEFGH12345678"), "got: {out2}");
    }

    #[test]
    fn redacts_keyworded_blob() {
        let out = redact_one("secret: 0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(out.contains("[REDACTED:"), "got: {out}");
        assert!(
            !out.contains("0123456789abcdef0123456789abcdef0123456789abcdef"),
            "got: {out}"
        );
    }

    #[test]
    fn preserves_ordinary_prose_and_code() {
        let inputs = [
            "The quick brown fox jumps over the lazy dog.",
            "let x = compute_hash(); // returns a u64",
            "fn main() { println!(\"hello, world\"); }",
            "Visit https://example.com/path?q=1 for docs.",
            "The commit is abc123 and the file is src/main.rs",
            "tokenize the input and return tokens",
        ];
        for input in inputs {
            let out = redact_one(input);
            assert_eq!(out, input, "ordinary text was altered");
        }
    }

    #[test]
    fn preserves_plain_hex_without_keyword() {
        // A long hex string (e.g. a git blob hash) with NO secret keyword nearby must survive.
        let input = "object 0123456789abcdef0123456789abcdef0123456789abcdef committed";
        let out = redact_one(input);
        assert_eq!(out, input, "got: {out}");
    }

    /// Join a token prefix to its fixture tail at runtime so secret-shaped literals never appear
    /// in the source blob — GitHub push protection (and every downstream clone's scanner) would
    /// flag them as real credentials.
    fn fixture(prefix: &str, tail: &str) -> String {
        format!("{prefix}{tail}")
    }

    #[test]
    fn idempotent() {
        let inputs = [
            "sk-abcDEF1234567890ghijkl".to_string(),
            "email a@b.com and key sk-zzzzzzzzzzzzzzzzzzzz".to_string(),
            "-----BEGIN PRIVATE KEY-----\nAAAA\nBBBB\n-----END PRIVATE KEY-----".to_string(),
            "-----BEGIN PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwx".to_string(), // truncated
            r#"password = "hunter2supersecret""#.to_string(),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c".to_string(),
            "postgres://app_user:hunter2secret@db.example.com:5432/prod".to_string(),
            "npm_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            fixture("glpat-", "abcDEF1234567890ghij"),
            "Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==".to_string(),
            "bearer abcdef1234567890XYZ".to_string(),
            "key AKIAIOSFODNN7EXAMPLEEXTRA9 tail".to_string(),
        ];
        for input in &inputs {
            let once = redact_one(input);
            let twice = redact_one(&once);
            assert_eq!(once, twice, "not idempotent for {input}");
        }
    }

    #[test]
    fn redacts_tool_use_input_values_keeping_structure() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::Assistant);
        let input = serde_json::json!({
            "command": "curl",
            "headers": {
                "Authorization": "Bearer abcdef1234567890XYZ",
                "X-Api-Key": "sk-abcDEF1234567890ghijkl"
            },
            "count": 5,
            "nested": ["sk-ant-api03-aaaaaaaaaaaaaaaaaaaabbbb", "plain text"]
        });
        m.content.push(Block::ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: input.into(),
            namespace: None,
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        let block = &out.messages[0].content[0];
        let Block::ToolUse { input, .. } = block else {
            panic!("expected tool use");
        };
        let input = input.resolve().unwrap();
        // structure + keys intact
        assert!(input.get("command").is_some());
        assert_eq!(input["command"], "curl");
        assert_eq!(input["count"], 5);
        assert!(input["headers"].get("Authorization").is_some());
        // secret values gone
        let serialized = input.to_string();
        assert!(!serialized.contains("sk-abcDEF1234567890ghijkl"), "got: {serialized}");
        assert!(!serialized.contains("sk-ant-api03"), "got: {serialized}");
        assert!(serialized.contains("[REDACTED:api_key]"), "got: {serialized}");
        // non-secret string preserved
        assert!(serialized.contains("plain text"));
    }

    #[test]
    fn redacts_tool_result_content_and_details() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: "1".into(),
            content: "leaked: sk-abcDEF1234567890ghijkl".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: Some(serde_json::json!({ "stderr": "token ghp_0123456789abcdefABCDEF0123456789abcd" })),
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        let Block::ToolResult { content, details, .. } = &out.messages[0].content[0] else {
            panic!();
        };
        assert!(!content.contains("sk-abcDEF1234567890ghijkl"), "got: {content}");
        assert!(content.contains("[REDACTED:api_key]"));
        let d = details.as_ref().unwrap().to_string();
        assert!(!d.contains("ghp_0123456789abcdef"), "got: {d}");
    }

    #[test]
    fn redacts_title_and_thinking() {
        let mut sess = empty_session();
        sess.title = Some("debugging sk-abcDEF1234567890ghijkl".into());
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Thinking {
            text: "the user's key is sk-zzzzzzzzzzzzzzzzzzzz hmm".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        sess.messages.push(m);

        let out = redact(&sess).unwrap();
        assert!(!out.title.as_ref().unwrap().contains("sk-abcDEF"));
        let Block::Thinking { text, .. } = &out.messages[0].content[0] else {
            panic!();
        };
        assert!(!text.contains("sk-zzzz"), "got: {text}");
        assert!(text.starts_with("the user's key is "));
        assert!(text.ends_with(" hmm"));
    }

    #[test]
    fn stats_count() {
        let mut sess = empty_session();
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text {
            text: "sk-abcDEF1234567890ghijkl and a@b.com".into(),
        });
        sess.messages.push(m);
        let (_out, stats) = redact_with(&sess, &RedactOptions::default()).unwrap();
        assert_eq!(stats.api_keys, 1);
        assert_eq!(stats.emails, 1);
        assert_eq!(stats.total(), 2);
    }

    /// `scrub_cow` is the zero-copy core: clean text comes back `Borrowed` (no allocation),
    /// and dirty text produces exactly what `scrub`/`redact_text` produce.
    #[test]
    fn scrub_cow_borrows_clean_and_matches_scrub_when_dirty() {
        let mut stats = RedactStats::default();
        let clean = "ordinary prose, a commit abc123, and [REDACTED:api_key] already placed";
        match scrub_cow(clean, &RedactOptions::default(), &mut stats) {
            Cow::Borrowed(s) => assert_eq!(s, clean),
            Cow::Owned(s) => panic!("clean text must not allocate, got owned {s:?}"),
        }
        assert_eq!(stats.total(), 0);

        let dirty = "prefix sk-abcDEF1234567890ghijkl middle a@b.com suffix";
        let cow = scrub_cow(dirty, &RedactOptions::default(), &mut stats);
        assert!(matches!(cow, Cow::Owned(_)));
        assert_eq!(cow.as_ref(), redact_text(dirty));
        assert_eq!(stats.total(), 2);
    }

    // --- R1: truncated PEM ---------------------------------------------------

    #[test]
    fn redacts_truncated_private_key() {
        // Tool-output clipping loses the END marker; the body must still be scrubbed.
        let pem = "output:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdefMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwxyzABCDEF0123456789abcdefqrstuvwxyzABCDEF0123456789abcdef\nZZZZclipped\n[output clipped]";
        let out = redact_one(pem);
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "line 1 leaked: {out}");
        assert!(!out.contains("qrstuvwxyzABCDEF"), "line 2 leaked: {out}");
        assert!(!out.contains("ZZZZclipped"), "clipped tail leaked: {out}");
        assert!(out.starts_with("output:\n"));
        assert!(out.ends_with("\n[output clipped]"));
    }

    #[test]
    fn redacts_truncated_encrypted_private_key_headers() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,A1B2C3D4E5F60718\n\nMIIEowIBAAKCAQEA1234567890abcdefMIIEowIBAAKCAQEA1234567890abcdef";
        let out = redact_one(pem);
        assert!(!out.contains("DEK-Info"), "IV header leaked: {out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "body leaked: {out}");
        assert!(out.contains("[REDACTED:private_key]"), "got: {out}");
    }

    #[test]
    fn bare_begin_marker_in_prose_survives() {
        // A mention of the marker with no base64 body following is prose, not a key.
        let input = "PEM blocks start with -----BEGIN PRIVATE KEY----- and look like\nthis line";
        assert_eq!(redact_one(input), input);
    }

    // --- R2: new token families + connection strings -------------------------

    #[test]
    fn redacts_new_token_families() {
        let tokens = [
            fixture("sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"),
            fixture("rk_live_", "4eC39HqLyjWDarjtT1zdp7dc"),
            fixture("glpat-", "abcDEF1234567890ghij"),
            fixture("xoxc-", "1234567890-abcdefghij"),
            fixture("xoxs-", "1234567890-abcdefghij"),
            fixture("xapp-", "1-A0123456789-abcdefghijklmnop"),
            fixture("npm_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            fixture("hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"),
            fixture("gsk_", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKL"),
            fixture(
                "xai-",
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd",
            ),
            fixture(
                "dop_v1_",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
            fixture("shpat_", "0123456789abcdef0123456789abcdef"),
            fixture("shpss_", "0123456789abcdef0123456789abcdef"),
        ];
        for t in &tokens {
            let out = redact_one(&format!("deploy with {t} now"));
            assert!(!out.contains(t.as_str()), "{t} leaked: {out}");
            assert!(out.contains("[REDACTED:api_key]"), "{t}: got {out}");
            assert!(out.starts_with("deploy with "), "{t}: got {out}");
            assert!(out.ends_with(" now"), "{t}: got {out}");
        }
    }

    #[test]
    fn short_prefix_families_skip_identifiers() {
        let inputs = [
            "npm_config_registry points at the default registry",
            "npm_package_version is set by npm",
            "use hf_hub_download to fetch the model",
            "hf_transfer speeds things up",
            "the sk_live_key variable name",
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "identifier was eaten");
        }
    }

    #[test]
    fn redacts_connection_string_password() {
        let out = redact_one("DATABASE_URL=postgres://app_user:sup3rS3cret!@db.internal:5432/prod");
        assert!(!out.contains("sup3rS3cret!"), "got: {out}");
        assert_eq!(
            out,
            "DATABASE_URL=postgres://app_user:[REDACTED:secret]@db.internal:5432/prod"
        );

        // Empty user (redis style) still counts; user stays visible in the general case.
        let out2 = redact_one("redis://:opensesame123@cache.local:6379");
        assert_eq!(out2, "redis://:[REDACTED:secret]@cache.local:6379");

        // Git remote with an embedded installation token.
        let out3 = redact_one("https://x-access-token:ghs_0123456789abcdefABCD@github.com/o/r.git");
        assert_eq!(out3, "https://x-access-token:[REDACTED:secret]@github.com/o/r.git");
    }

    #[test]
    fn connection_string_false_positives_survive() {
        let inputs = [
            "https://example.com:8080/path?q=1",
            "see http://localhost:3000 for the dev server",
            "mongodb://replica1.example.com:27017,replica2.example.com:27017/db",
            "postgres://app:$DB_PASSWORD@db/prod",   // env ref, not a secret
            "postgres://app:${DB_PASSWORD}@db/prod", // env ref, not a secret
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "false positive");
        }
    }

    // --- R3: header/bearer case + compound header names ----------------------

    #[test]
    fn redacts_compound_authorization_headers() {
        let out = redact_one("Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==");
        assert_eq!(out, "Proxy-Authorization: [REDACTED:api_key]");

        let out2 = redact_one("X-Authorization: t0ken-value-1234");
        assert_eq!(out2, "X-Authorization: [REDACTED:api_key]");
    }

    #[test]
    fn redacts_lowercase_bearer() {
        let out = redact_one("sending bearer abcdef1234567890XYZ along");
        assert!(!out.contains("abcdef1234567890XYZ"), "got: {out}");
        assert_eq!(out, "sending [REDACTED:api_key] along");
    }

    #[test]
    fn bearer_prose_survives() {
        let inputs = ["bearer instruments are negotiable", "the bearer of this note"];
        for input in inputs {
            assert_eq!(redact_one(input), input, "prose was eaten");
        }
    }

    // --- R4: assignment code/prose false positives ---------------------------

    #[test]
    fn assignment_skips_code_and_prose() {
        let inputs = [
            "let token = get_token();",
            "let token = client.fetch_token(scope);",
            "password: use a strong one",
            "# token: set via the environment",
        ];
        for input in inputs {
            assert_eq!(redact_one(input), input, "code/prose was corrupted");
        }
    }

    // --- R5: wider keyword window + hex floor ---------------------------------

    #[test]
    fn redacts_hex_secret_behind_wider_keyword_window() {
        // 39-hex value, keyword ~26 chars back (old 24-char window missed it).
        let input = r#""credentials": {"value": "0123456789abcdef0123456789abcdef0123456"}"#;
        let out = redact_one(input);
        assert!(
            !out.contains("0123456789abcdef0123456789abcdef0123456"),
            "39-hex leaked: {out}"
        );

        // 32-hex secret with the keyword >24 but <48 chars before it.
        let input2 = "the api key for that environment is 0123456789abcdef0123456789abcdef";
        let out2 = redact_one(input2);
        assert!(
            !out2.contains("0123456789abcdef0123456789abcdef"),
            "32-hex leaked: {out2}"
        );
    }

    #[test]
    fn preserves_multiline_base64_without_keyword() {
        // Certificate-style base64 (public data, no secret keyword nearby) must survive.
        let cert = "MIIDdzCCAlgAwIBAgIEbGRkbDANBgcshgiG9w0BAQsFADBsMQswCQYDVQQGEwJV\nUzEQMA4GA1UECBMHQXJpem9uYTETMBEGA1UEBxMSU2NvdHRzZGFsZTEaMBgGA1UE\n";
        assert_eq!(redact_one(cert), cert);
    }

    // --- R6: scope beyond title + message blocks -----------------------------

    /// Documents the redaction scope of [`redact_with`]: `git.remote`, `Session.extra`, and
    /// `Message.extra` are scrubbed; `cwd`/`source_path` deliberately are not (see the fn docs).
    #[test]
    fn scrubs_git_remote_and_extra_maps() {
        let mut sess = empty_session();
        sess.git = Some(GitInfo {
            branch: Some("main".into()),
            commit: Some("abc1234".into()),
            remote: Some("https://x-access-token:ghs_0123456789abcdefABCD@github.com/o/r.git".into()),
        });
        sess.extra
            .insert("apiKey".into(), serde_json::json!("sk-abcDEF1234567890ghijkl"));
        let mut m = Message::new(Role::User);
        m.content.push(Block::Text { text: "hi".into() });
        m.extra.insert(
            "env".into(),
            serde_json::json!({ "GITHUB_TOKEN": "ghp_0123456789abcdefABCDEF0123456789abcd" }),
        );
        sess.messages.push(m);

        let (out, stats) = redact_with(&sess, &RedactOptions::default()).unwrap();
        let remote = out.git.as_ref().unwrap().remote.as_deref().unwrap();
        assert_eq!(remote, "https://x-access-token:[REDACTED:secret]@github.com/o/r.git");
        assert_eq!(out.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        let extra = serde_json::Value::Object(out.extra.clone()).to_string();
        assert!(!extra.contains("sk-abcDEF"), "session extra leaked: {extra}");
        let mextra = serde_json::Value::Object(out.messages[0].extra.clone()).to_string();
        assert!(!mextra.contains("ghp_"), "message extra leaked: {mextra}");
        assert!(stats.total() >= 3, "stats: {stats:?}");

        // Idempotent across the whole session too.
        let (again, again_stats) = redact_with(&out, &RedactOptions::default()).unwrap();
        assert_eq!(again.git.as_ref().unwrap().remote, out.git.as_ref().unwrap().remote);
        assert_eq!(again.extra, out.extra);
        assert_eq!(again.messages[0].extra, out.messages[0].extra);
        assert_eq!(again_stats.total(), 0);
    }

    // --- R7: AKIA over-long runs ----------------------------------------------

    #[test]
    fn akia_overlong_run_fully_redacted() {
        // Slicing exactly 20 chars out of a longer run would leave a tail of the key behind.
        let out = redact_one("key AKIAIOSFODNN7EXAMPLEEXTRA9 tail");
        assert!(!out.contains("AKIA"), "got: {out}");
        assert!(!out.contains("EXTRA9"), "tail leaked: {out}");
        assert_eq!(out, "key [REDACTED:api_key] tail");
    }

    #[test]
    fn no_panic_on_weird_input() {
        let weird = [
            "",
            "@",
            "@@@@",
            "sk-",
            "-----BEGIN PRIVATE KEY-----",
            "Bearer ",
            "café résumé naïve 日本語 sk-abcDEF1234567890ghijkl",
            "....",
            "eyJ",
        ];
        for w in weird {
            let _ = redact_one(w); // must not panic
        }
    }

    /// Property-ish: every matcher must survive arbitrary truncation/suffixing of real-looking
    /// secrets (clipped tool output produces exactly these shapes), including through multi-byte
    /// UTF-8, and redacting any fragment must stay idempotent.
    #[test]
    fn no_panic_on_mutated_inputs() {
        let corpus = [
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nMIIEowIBAAKCAQEA1234567890abcdef\nqrstuvwx\n-----END RSA PRIVATE KEY-----".to_string(),
            "postgres://app_user:sup3rS3cret!@db.example.com:5432/prod".to_string(),
            "Proxy-Authorization: bearer abcdef1234567890XYZ".to_string(),
            "password = \"hunter2supersecret\" et café 日本語".to_string(),
            fixture("npm_", "abcdefghijklmnopqrstuvwxyz0123456789 ") + &fixture("hf_", "abcdefghijklmnopqrstuvwxyzABCDEFGH"),
            "key AKIAIOSFODNN7EXAMPLEEXTRA9 [REDACTED:api_key] eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig".to_string(),
        ];
        for base in &corpus {
            for cut in 0..=base.len() {
                if !base.is_char_boundary(cut) {
                    continue;
                }
                for frag in [&base[..cut], &base[cut..]] {
                    let once = redact_one(frag);
                    let twice = redact_one(&once);
                    assert_eq!(once, twice, "not idempotent for fragment {frag:?}");
                }
            }
        }
    }

    #[test]
    fn redaction_resolves_lazy_text_and_scrubs_system_prompt_and_complete_record() -> anyhow::Result<()> {
        let world = crate::test_world("redact-checked-record");
        let path = world.base.join("record.json");
        let secret = fixture("sk-", "testabcdefghijklmnopqrstuvwxyz");
        let body = format!("{} {secret}", "ordinary ".repeat(2000));
        let value = serde_json::json!({"type":"assistant","text":body,"unknown":{"credential":secret}});
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let text = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                view.get("text")?.unwrap().source_text()
            })?
            .unwrap();
        let raw = crate::Json::source(&path, 0, source.len())?;
        let mut session = Session::new("redaction", crate::Harness::OpenCode);
        session.system_prompt = Some(format!("Authorization: Bearer {secret}"));
        let mut message = Message::new(crate::Role::Assistant);
        message.content.push(Block::Text { text });
        session.messages.push(message);
        crate::ir::CompleteRecord::attach_replay(
            &mut session.messages,
            crate::Harness::OpenCode,
            raw,
            Default::default(),
        );
        let original = session.messages[0].complete_record.as_ref().unwrap();
        assert!(!original.projection.is_empty());
        let (redacted, stats) = redact_with(&session, &RedactOptions::default())?;
        assert!(!serde_json::to_string(&redacted)?.contains(&secret));
        assert!(serde_json::to_string(&session)?.contains(&secret));
        assert!(
            !original.projection.is_empty(),
            "copy-on-write must not alter the input witness"
        );
        assert!(redacted.messages[0]
            .complete_record
            .as_ref()
            .unwrap()
            .projection
            .is_empty());
        assert!(stats.total() >= 3);
        assert_eq!(std::fs::read(&path)?, serde_json::to_vec(&value)?);
        // Direct message calls propagate stale text instead of panicking or returning clean data.
        let mut stale = session.messages[0].clone();
        std::fs::write(&path, b"replacement")?;
        let mut failed_stats = RedactStats::default();
        assert!(redact_message(
            &mut stale,
            &RedactOptions::default(),
            &mut failed_stats,
            &session.resolver()
        )
        .is_err());
        assert_eq!(failed_stats.total(), 0);
        Ok(())
    }

    #[test]
    fn redaction_uses_session_resolver_for_implicit_spans() -> anyhow::Result<()> {
        let world = crate::test_world("redact-implicit-span");
        let path = world.base.join("body.txt");
        let secret = fixture("sk-", "testabcdefghijklmnopqrstuvwxyz");
        std::fs::write(&path, serde_json::to_string(&secret)?)?;
        let mut session = Session::new("implicit", crate::Harness::Claude);
        session.source_path = Some(path);
        let mut message = Message::new(crate::Role::User);
        message.content.push(Block::Text {
            text: crate::Text::Span(crate::lazy::Span {
                source: None,
                offset: 1,
                len: secret.len() as u64,
                escaped: false,
            }),
        });
        session.messages.push(message);
        assert!(!serde_json::to_string(&redact(&session)?)?.contains(&secret));
        let resolver = session.resolver();
        let mut message = session.messages[0].clone();
        redact_message(
            &mut message,
            &RedactOptions::default(),
            &mut RedactStats::default(),
            &resolver,
        )?;
        assert!(!serde_json::to_string(&message)?.contains(&secret));
        Ok(())
    }

    #[test]
    fn a_second_pem_header_is_never_mistaken_for_the_private_key_footer() {
        let body = "ABCDEFGHIJKLMNOP";
        let input = format!(
            "-----BEGIN PRIVATE KEY-----\n{body}\n-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----"
        );
        let output = redact_one(&input);
        assert!(!output.contains(body));
        assert!(output.contains("[REDACTED:private_key]"));
        assert_eq!(redact_one(&output), output);
    }

    #[test]
    fn blob_suffix_offsets_match_the_recorded_table() {
        let rule = rules().iter().find(|r| r.kind == Kind::Blob).unwrap();
        let mut frozen = crate::emit::frozen::Frozen::new("redact-blob-offsets");
        for prefix in ["", "key ", "secret = ", "ordinary ", "clé key "] {
            for stem in ["abcdef", "abcdefghi", "abc-def_", "abc/def+", "abc==="] {
                for repeats in [1, 5, 8, 20, 1000] {
                    let input = format!("{prefix}{}", stem.repeat(repeats));
                    let actual = next_blob(&rule.re, &input, 0).map(|h| (h.start, h.end));
                    frozen.insert(format!("{prefix:?} {stem} x{repeats}"), serde_json::json!(actual));
                }
            }
        }
        frozen.check().unwrap();
        assert!(matches!(
            scrub_cow(
                &"x".repeat(1_000_000),
                &RedactOptions::default(),
                &mut RedactStats::default()
            ),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn hashed_redaction_covers_high_risk_families_with_exact_sha8_tags() {
        let expected_tag = |rule: &str, secret: &str| {
            let digest = crate::digest::sha256_hex(secret.as_bytes());
            format!("<REDACTED:{rule}:{}>", &digest[..8])
        };

        let ant = fixture("sk-ant-", "api03-TEST0123456789abcdefghijkl");
        let oai = fixture("sk-", "projTEST0123456789abcdefghijkl");
        let ghp = fixture("ghp_", "0123456789abcdefABCDEF0123456789abcd");
        let ghs = fixture("ghs_", "9876543210fedcbaFEDCBA9876543210dcba");
        let pat = fixture("github_pat_", "11ABCDEF0123456789_abcdefghijklmnop");
        let lin = fixture("lin_api_", "0123456789abcdefABCDEF01234567");
        let kgm = fixture("kgm_", "0123456789abcdefABCD");
        let sbp = fixture("sbp_", "0123456789abcdefABCDEF0123456789");
        let aws = fixture("AKIA", "IOSFODNN7EXAMPLE");
        // Independent SHA-256 sanity check against FIPS 180-4 ("abc" -> ba7816bf...) and aws fixture.
        assert_eq!(&crate::digest::sha256_hex(b"abc")[..8], "ba7816bf");
        assert_eq!(&crate::digest::sha256_hex(aws.as_bytes())[..8], "1a5d44a2");
        let jwt = format!(
            "{}.{}.{}",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwicm9sZSI6InNpZyJ9",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        );
        let bearer_tok = "sigBearerToken9876543210xyz";
        let pod_tok = "podRemoteSecretValue99887766";
        let pem = format!(
            "-----BEGIN {} PRIVATE KEY-----\nMIIEpAIBAAKCAQEA0123456789abcdef\n-----END {} PRIVATE KEY-----",
            "RSA", "RSA"
        );

        for (raw, rule, hashed_val) in [
            (ant.as_str(), "anthropic", ant.as_str()),
            (oai.as_str(), "openai", oai.as_str()),
            (ghp.as_str(), "github", ghp.as_str()),
            (ghs.as_str(), "github", ghs.as_str()),
            (pat.as_str(), "github", pat.as_str()),
            (lin.as_str(), "linear", lin.as_str()),
            (kgm.as_str(), "kagi", kgm.as_str()),
            (sbp.as_str(), "supabase", sbp.as_str()),
            (aws.as_str(), "aws", aws.as_str()),
            (jwt.as_str(), "jwt", jwt.as_str()),
            (pem.as_str(), "private_key", pem.as_str()),
        ] {
            let out = redact_text_hashed(raw);
            assert_eq!(out, expected_tag(rule, hashed_val), "rule {rule}");
            // Idempotent when re-fed through hashed or default redaction.
            assert_eq!(redact_text_hashed(&out), out, "idempotent {rule}");
        }

        let bearer_in = format!("Header: Bearer {bearer_tok}!");
        assert_eq!(
            redact_text_hashed(&bearer_in),
            format!("Header: Bearer {}!", expected_tag("bearer", bearer_tok))
        );

        let pod_in = format!("env SIG_POD_REMOTE_TOKEN={pod_tok} active");
        assert_eq!(
            redact_text_hashed(&pod_in),
            format!("env SIG_POD_REMOTE_TOKEN={} active", expected_tag("secret", pod_tok))
        );
    }

    #[test]
    fn two_pass_session_redactor_scrubs_multi_copy_and_bare_echoes_across_tool_blocks() -> anyhow::Result<()> {
        let pod_val = "XyZ987654321unprefixedPodSecret";
        let ant_key = fixture("sk-ant-", "api03-MultiCopySecretValue123456");
        let gh_key = fixture("ghp_", "MultiCopyGithubSecret0123456789ab");

        let mut session = empty_session();
        session.title = Some(format!("investigating {ant_key} leak"));
        session.git = Some(GitInfo {
            branch: Some("main".into()),
            commit: Some("b4674049b04300d7be61c3003f6329357bd9083f".into()),
            remote: Some("https://bot:superSecretGitPass123@github.com/org/repo.git".into()),
        });

        let mut m0 = Message::new(Role::User);
        m0.content.push(Block::Text {
            text: format!("Check SIG_POD_REMOTE_TOKEN={pod_val} and rotate {ant_key}").into(),
        });

        let mut m1 = Message::new(Role::Assistant);
        m1.content.push(Block::ToolUse {
            id: "tu_cross_read_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({
                "command": format!("curl -H 'X-Key: {gh_key}' https://example.test"),
                "note": "safe_identifier_stays_intact",
            })
            .into(),
            namespace: None,
        });

        let mut m2 = Message::new(Role::User);
        m2.content.push(Block::ToolResult {
            tool_use_id: "tu_cross_read_1".into(),
            // Bare unprefixed echo of `pod_val` (no `SIG_POD_REMOTE_TOKEN=` prefix here!) + `gh_key`.
            content: format!("stdout echoed bare value {pod_val} and {gh_key} during test").into(),
            is_error: false,
            tool_name: Some("Bash".into()),
            status: None,
            details: Some(serde_json::json!({"echoed": pod_val})),
        });

        let mut m3 = Message::new(Role::Assistant);
        m3.content.push(Block::Text {
            text: format!(
                "Confirmed {pod_val} and {ant_key} were rotated; commit b4674049b04300d7be61c3003f6329357bd9083f and bearer instruments prose stay."
            )
            .into(),
        });

        session.messages = vec![m0, m1, m2, m3];

        let (clean, stats) = redact_with(&session, &RedactOptions::hashed())?;
        let serialized = serde_json::to_string(&clean)?;

        for forbidden in [pod_val, ant_key.as_str(), gh_key.as_str(), "superSecretGitPass123"] {
            assert!(
                !serialized.contains(forbidden),
                "secret {forbidden} survived in:\n{serialized}"
            );
        }
        assert!(
            stats.redacted_tool_use_ids.contains("tu_cross_read_1"),
            "expected tu_cross_read_1 in {:?}",
            stats.redacted_tool_use_ids
        );

        let pod_tag = redact_text_hashed(&format!("SIG_POD_REMOTE_TOKEN={pod_val}"))
            .strip_prefix("SIG_POD_REMOTE_TOKEN=")
            .unwrap()
            .to_string();
        let ant_tag = redact_text_hashed(&ant_key);
        let gh_tag = redact_text_hashed(&gh_key);

        // Every copy — including the bare unprefixed echoes in m2 (`content` + `details`) and m3 —
        // carries the identical deterministic `<REDACTED:rule:sha8>` placeholder.
        assert_eq!(serialized.matches(&pod_tag).count(), 4, "serialized:\n{serialized}");
        assert_eq!(serialized.matches(&ant_tag).count(), 3, "serialized:\n{serialized}");
        assert_eq!(serialized.matches(&gh_tag).count(), 2, "serialized:\n{serialized}");

        // Non-secret prose, identifiers, and git commit SHA survive verbatim.
        assert!(serialized.contains("safe_identifier_stays_intact"));
        assert!(serialized.contains("b4674049b04300d7be61c3003f6329357bd9083f"));
        assert!(serialized.contains("bearer instruments prose stay."));

        // Idempotent under a second pass.
        let (clean_twice, _) = redact_with(&clean, &RedactOptions::hashed())?;
        assert_eq!(serde_json::to_string(&clean_twice)?, serialized);
        Ok(())
    }
}
