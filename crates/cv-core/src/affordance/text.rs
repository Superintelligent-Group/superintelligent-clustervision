//! Bounded reductions over complete decoded content, shared by affordance counts and rows.
use crate::ir::Harness;
use crate::lazy::{Resolver, Text};

const CHUNK: usize = 64 * 1024;
const SUMMARY_CHARS: usize = 120;

/// Keep enough of a fully trimmed string to reproduce `truncate(trim(text), 120)` exactly.
/// A long whitespace tail must not turn a 120-character summary into an ellipsis.
#[derive(Default)]
struct Trimmed {
    prefix: String,
    chars: usize,
    started: bool,
    later_content: bool,
}

impl Trimmed {
    fn push(&mut self, mut text: &str) {
        if !self.started {
            text = text.trim_start();
            self.started = !text.is_empty();
        }
        if self.chars <= SUMMARY_CHARS {
            let mut end = 0;
            for (offset, ch) in text.char_indices().take(SUMMARY_CHARS + 1 - self.chars) {
                self.prefix.push(ch);
                self.chars += 1;
                end = offset + ch.len_utf8();
            }
            text = &text[end..];
        }
        self.later_content |= !text.trim().is_empty();
    }

    fn summary(&self) -> String {
        let prefix = if self.later_content {
            self.prefix.as_str()
        } else {
            self.prefix.trim_end()
        };
        crate::ir::truncate(prefix, SUMMARY_CHARS)
    }
}

/// The longest suffix that could continue an ASCII delimiter in the next decoded chunk.
fn partial_suffix(text: &str, delimiter: &str) -> usize {
    (1..delimiter.len())
        .rev()
        .find(|&n| text.ends_with(&delimiter[..n]))
        .unwrap_or(0)
}

struct Tag {
    open: &'static str,
    close: &'static str,
    opened: bool,
    complete: bool,
    pending: String,
    scratch: String,
    text: Trimmed,
}

impl Tag {
    fn new(open: &'static str, close: &'static str) -> Self {
        Self {
            open,
            close,
            opened: false,
            complete: false,
            pending: String::new(),
            scratch: String::new(),
            text: Trimmed::default(),
        }
    }

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
            if let Some(start) = body.find(self.open) {
                self.opened = true;
                body = &body[start + self.open.len()..];
            } else {
                let n = partial_suffix(body, self.open);
                self.pending.push_str(&body[body.len() - n..]);
                return;
            }
        }
        if let Some(end) = body.find(self.close) {
            self.text.push(&body[..end]);
            self.complete = true;
        } else {
            let n = partial_suffix(body, self.close);
            self.text.push(&body[..body.len() - n]);
            self.pending.push_str(&body[body.len() - n..]);
        }
    }
}

struct Contains {
    needle: &'static str,
    found: bool,
    pending: String,
}

impl Contains {
    fn new(needle: &'static str) -> Self {
        Self {
            needle,
            found: false,
            pending: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) {
        if self.found {
            return;
        }
        if chunk.contains(self.needle) {
            self.found = true;
            return;
        }
        let mut end = chunk.len().min(self.needle.len());
        while !chunk.is_char_boundary(end) {
            end += 1;
        }
        let mut bridge = std::mem::take(&mut self.pending);
        bridge.push_str(&chunk[..end]);
        if bridge.contains(self.needle) {
            self.found = true;
            return;
        }
        let tail = if end == chunk.len() { bridge.as_str() } else { chunk };
        let n = partial_suffix(tail, self.needle);
        self.pending.push_str(&tail[tail.len() - n..]);
    }
}

pub(super) struct Facts {
    plain: Trimmed,
    summary: Tag,
    reminder: Tag,
    #[cfg(test)]
    denied: Contains,
    #[cfg(test)]
    approval: Contains,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            plain: Trimmed::default(),
            summary: Tag::new("<summary>", "</summary>"),
            reminder: Tag::new("<system-reminder>", "</system-reminder>"),
            #[cfg(test)]
            denied: Contains::new(" denied"),
            #[cfg(test)]
            approval: Contains::new("requires approval"),
        }
    }
}

impl Facts {
    fn push(&mut self, chunk: &str) {
        self.plain.push(chunk);
        self.summary.push(chunk);
        self.reminder.push(chunk);
        #[cfg(test)]
        {
            self.denied.push(chunk);
            self.approval.push(chunk);
        }
    }

    #[cfg(test)]
    fn starts_with(&self, prefix: &str) -> bool {
        self.plain.prefix.starts_with(prefix)
    }

    #[cfg(test)]
    fn requires_approval(&self) -> bool {
        self.approval.found
    }

    #[cfg(test)]
    fn denied(&self, harness: Harness) -> bool {
        match harness {
            Harness::Claude => {
                (self.starts_with("Permission to use ") && self.denied.found)
                    || self.starts_with("The user doesn't want to proceed with this tool use")
            }
            Harness::OpenCode => {
                self.starts_with("The user has specified a rule which prevents you from using this specific tool call")
                    || self.starts_with("The user rejected permission to use this specific tool call")
            }
            _ => false,
        }
    }

    pub(super) fn summary(&self) -> String {
        if self.summary.complete {
            self.summary.text.summary()
        } else if self.reminder.complete {
            self.reminder.text.summary()
        } else {
            self.plain.summary()
        }
    }

    #[cfg(test)]
    fn plain_summary(&self) -> String {
        self.plain.summary()
    }
}

fn chunks(text: &Text, resolver: &Resolver, mut visit: impl FnMut(&str)) {
    match text {
        Text::Span(span) => resolver.for_each_chunk(span, CHUNK, visit),
        Text::Inline(text) => {
            let mut rest = text.as_str();
            while !rest.is_empty() {
                let mut end = rest.len().min(CHUNK);
                while !rest.is_char_boundary(end) {
                    end += 1;
                }
                visit(&rest[..end]);
                rest = &rest[end..];
            }
        }
    }
}

pub(super) fn facts(text: &Text, resolver: &Resolver) -> Facts {
    let mut facts = Facts::default();
    chunks(text, resolver, |chunk| facts.push(chunk));
    facts
}

/// Most record heads decide from a small source prefix. Only a long whitespace run or an
/// incomplete decoded prefix needs the chunk path; it retains only the wanted ASCII head.
pub(super) fn starts_with(text: &Text, resolver: &Resolver, wanted: &str) -> bool {
    if let Text::Inline(text) = text {
        return text.trim_start().starts_with(wanted);
    }
    let span = text.as_span().unwrap();
    let prefix = text.resolve_prefix(resolver, 256);
    if span.len <= 256 {
        return prefix.trim_start().starts_with(wanted);
    }
    let decoded = prefix_decision(&prefix, wanted, span.escaped);
    // Full resolution of a malformed escaped body falls back to its *entire raw text*. A valid
    // head alone cannot prove that decoding succeeds later. Early answers must agree with both
    // interpretations; otherwise the bounded decoder validates and selects the whole-body mode.
    let raw = crate::lazy::Span {
        escaped: false,
        ..span.clone()
    };
    let raw_prefix = resolver.resolve_prefix(&raw, 256);
    if let Some(answer) = decoded {
        if prefix_decision(&raw_prefix, wanted, false) == Some(answer) {
            return answer;
        }
    }
    let mut head = String::with_capacity(wanted.len());
    let mut remaining = wanted.chars().count();
    let mut started = false;
    chunks(text, resolver, |mut chunk| {
        if remaining == 0 {
            return;
        }
        if !started {
            chunk = chunk.trim_start();
            started = !chunk.is_empty();
        }
        for ch in chunk.chars().take(remaining) {
            head.push(ch);
            remaining -= 1;
        }
    });
    head.starts_with(wanted)
}

fn prefix_decision(prefix: &str, wanted: &str, escaped: bool) -> Option<bool> {
    // A UTF-8 cut can produce a replacement character; a failed prefix unescape can expose
    // backslashes. The text before that uncertainty can still prove an answer.
    let uncertain = prefix
        .find('\u{fffd}')
        .into_iter()
        .chain(escaped.then(|| prefix.find('\\')).flatten())
        .min();
    let known = prefix[..uncertain.unwrap_or(prefix.len())].trim_start();
    if known.starts_with(wanted) {
        Some(true)
    } else if !wanted.starts_with(known) {
        Some(false)
    } else {
        None
    }
}

pub(super) fn contains(text: &Text, resolver: &Resolver, needle: &'static str) -> bool {
    if let Text::Inline(text) = text {
        return text.contains(needle);
    }
    let mut search = Contains::new(needle);
    chunks(text, resolver, |chunk| search.push(chunk));
    search.found
}

pub(super) fn denied(text: &Text, resolver: &Resolver, harness: Harness) -> bool {
    match harness {
        Harness::Claude => {
            (starts_with(text, resolver, "Permission to use ") && contains(text, resolver, " denied"))
                || starts_with(text, resolver, "The user doesn't want to proceed with this tool use")
        }
        Harness::OpenCode => {
            starts_with(
                text,
                resolver,
                "The user has specified a rule which prevents you from using this specific tool call",
            ) || starts_with(
                text,
                resolver,
                "The user rejected permission to use this specific tool call",
            )
        }
        _ => false,
    }
}

pub(super) fn plain_summary(text: &Text, resolver: &Resolver) -> String {
    let mut summary = Trimmed::default();
    chunks(text, resolver, |chunk| summary.push(chunk));
    summary.summary()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original_summary(text: &str) -> String {
        let inner = |tag: &str| {
            let open = format!("<{tag}>");
            let start = text.find(&open)? + open.len();
            let end = text[start..].find(&format!("</{tag}>"))? + start;
            Some(&text[start..end])
        };
        crate::ir::truncate(
            inner("summary")
                .or_else(|| inner("system-reminder"))
                .unwrap_or(text)
                .trim(),
            120,
        )
    }

    #[test]
    fn reductions_match_full_text_across_every_chunk_boundary() {
        let examples = [
            String::new(),
            format!("{}late first text{}", "\u{2003}".repeat(8200), "\n".repeat(9000)),
            format!("{}{}", "x".repeat(120), " \n\t".repeat(9000)),
            format!("{}{}z", "x".repeat(120), " \n\t".repeat(9000)),
            format!(
                "{}<summary>{}late summary{}</summary>",
                "x".repeat(9000),
                "\n".repeat(8200),
                "\u{2003}".repeat(9000)
            ),
            "<system-reminder>fallback</system-reminder><summary>first<summary>second</summary>ignored</summary>"
                .into(),
            "<summary>unclosed <system-reminder>fallback</system-reminder>".into(),
            "<summary></summary><summary>second</summary>".into(),
            format!(
                "{}Permission to use Bash {} denied{}",
                " \n".repeat(9000),
                "x".repeat(9000),
                "\n".repeat(9000)
            ),
            format!(
                "{}The user rejected permission to use this specific tool call",
                "\u{2003}".repeat(9000)
            ),
            format!("{}requires approval", "x".repeat(9000)),
            "😀</summary><summar<summary>α\nβ😀</summary><summary>ignored</summary>".into(),
        ];
        for text in examples {
            for chunk in [1, 2, 3, 7, 13, 63, 1024, 4096, 65536] {
                let mut facts = Facts::default();
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let mut end = chunk.min(rest.len());
                    while !rest.is_char_boundary(end) {
                        end += 1;
                    }
                    facts.push(&rest[..end]);
                    rest = &rest[end..];
                }
                assert_eq!(facts.summary(), original_summary(&text), "chunk={chunk}, text={text:?}");
                assert_eq!(facts.plain_summary(), crate::ir::truncate(text.trim(), 120));
                assert_eq!(
                    facts.requires_approval(),
                    text.trim_start().contains("requires approval")
                );
                let t = text.trim_start();
                assert_eq!(
                    facts.denied(Harness::Claude),
                    (t.starts_with("Permission to use ") && t.contains(" denied"))
                        || t.starts_with("The user doesn't want to proceed with this tool use")
                );
                assert_eq!(
                    facts.denied(Harness::OpenCode),
                    t.starts_with(
                        "The user has specified a rule which prevents you from using this specific tool call"
                    ) || t.starts_with("The user rejected permission to use this specific tool call")
                );
            }
        }
    }
    #[test]
    fn escaped_and_literal_spans_match_full_inline_reductions() {
        let dir = std::env::temp_dir().join(format!("cv-affordance-text-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let examples = [
            format!("{}<task-notification>requires approval", "\u{2003}".repeat(100)),
            format!(
                "{}<summary>{}α\nβ😀{}</summary>",
                "p".repeat(70000),
                "\u{2003}".repeat(70000),
                "\n".repeat(70000)
            ),
            format!(
                "{}Permission to use Bash {} denied{}",
                "\t".repeat(70000),
                "x".repeat(70000),
                "\n".repeat(70000)
            ),
            format!(
                "{}<task-notification>{}requires approval",
                "\n".repeat(70000),
                "x".repeat(70000)
            ),
            format!("{}{}", "😀".repeat(120), "\u{2003}".repeat(70000)),
        ];
        for (i, body) in examples.iter().enumerate() {
            for escaped in [false, true] {
                let path = dir.join(format!("{i}-{escaped}"));
                let raw = if escaped {
                    serde_json::to_string(body).unwrap()
                } else {
                    body.clone()
                };
                std::fs::write(&path, &raw).unwrap();
                let resolver = Resolver::new(Some(path));
                let span = Text::Span(crate::lazy::Span {
                    source: None,
                    offset: u64::from(escaped),
                    len: raw.len() as u64 - 2 * u64::from(escaped),
                    escaped,
                });
                let reduced = facts(&span, &resolver);
                assert_eq!(reduced.summary(), original_summary(body));
                assert_eq!(reduced.plain_summary(), crate::ir::truncate(body.trim(), 120));
                assert_eq!(reduced.requires_approval(), body.contains("requires approval"));
                let full = facts(&Text::Inline(body.clone()), &resolver);
                assert_eq!(reduced.denied(Harness::Claude), full.denied(Harness::Claude));
                assert_eq!(
                    reduced.starts_with("<task-notification>"),
                    full.starts_with("<task-notification>")
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn malformed_escaped_tail_uses_the_full_resolution_fallback_for_heads() {
        let dir = std::env::temp_dir().join(format!("cv-affordance-malformed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let examples = [
            format!("\\n<task-notification>{}\\q", "x".repeat(70000)),
            format!("\\n<task-notification>{}\\uD800", "x".repeat(70000)),
            format!("\\nPermission to use Bash {} denied\\q", "x".repeat(70000)),
            format!(
                "\\nThe user rejected permission to use this specific tool call{}\\q",
                "x".repeat(70000)
            ),
            format!("\\n<task-notification>{}", "x".repeat(70000)),
        ];
        for (i, raw) in examples.iter().enumerate() {
            let path = dir.join(i.to_string());
            std::fs::write(&path, raw).unwrap();
            let resolver = Resolver::new(Some(path));
            let span = Text::Span(crate::lazy::Span {
                source: None,
                offset: 0,
                len: raw.len() as u64,
                escaped: true,
            });
            let full = span.resolve(&resolver);
            let inline = Text::Inline(full.to_string());
            assert_eq!(
                starts_with(&span, &resolver, "<task-notification>"),
                full.trim_start().starts_with("<task-notification>")
            );
            for harness in [Harness::Claude, Harness::OpenCode] {
                assert_eq!(denied(&span, &resolver, harness), denied(&inline, &resolver, harness));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
