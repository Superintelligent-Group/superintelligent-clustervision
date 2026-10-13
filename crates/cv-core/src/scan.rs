//! The no-index live scan: substring search over the corpus with no search index at all.
//!
//! This is the *degraded* path — `cv search` takes it before `cv index` has ever run, and `cv serve`'s
//! `/api/search` takes it when the tantivy index is missing or unreadable, so a fresh machine and
//! a broken index both still answer a query instead of erroring. It is slower than the index by
//! orders of magnitude (it streams every session) and it only sees each session's capped head, so
//! callers say so: the CLI prints a `(no index yet — scanning live…)` note, the daemon sets
//! `X-Cv-Search-Source: live`.
//!
//! Streaming discipline: each session streams into [`CapSink`], which keeps at most
//! [`LIVE_HAY_BYTES`] of searchable text and drops every message as it passes. Peak memory per
//! session is O(cap), never O(session) — an unbounded haystack here once meant multi-GB RSS on a
//! real corpus.

use crate::ir::{truncate, Block, Harness, Message, Role, SessionRef};
use crate::lazy::{Resolver, Text};
use crate::projection::Head;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use std::path::Path;

/// Searchable-haystack cap per session for the no-index live scan.
pub const LIVE_HAY_BYTES: usize = 256 * 1024;

/// Builds a capped searchable head (`hay`, at most [`LIVE_HAY_BYTES`]) as messages stream past
/// (each dropped immediately), grabbing the first user text for the label fallback; stops the
/// parse once the cap is hit and a first-user label candidate exists. Stream it under
/// [`ParseOptions::lazy`] so large content arrives as spans resolved only up to the remaining
/// budget — peak per session is O(cap), never O(session).
///
/// The no-index live path for `cv pack`, `cv search` and `cv serve`'s `/api/search` alike.
pub struct CapSink {
    pub hay: Head,
    pub first_user: Option<String>,
    resolver: Resolver,
    error: Option<anyhow::Error>,
    failed: bool,
}

impl CapSink {
    /// A fresh sink for one session at `path` (the resolver reads lazy spans back from it).
    pub fn new(path: &Path) -> Self {
        CapSink {
            hay: Head::new(LIVE_HAY_BYTES),
            first_user: None,
            resolver: Resolver::new(Some(path.to_path_buf())),
            error: None,
            failed: false,
        }
    }

    fn push_text(&mut self, text: &Text) -> Result<()> {
        self.hay.try_push_text(text, &self.resolver)?;
        self.hay.push("\n");
        Ok(())
    }
}

impl MessageSink for CapSink {
    fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error
            .take()
            .or_else(|| self.failed.then(|| anyhow::anyhow!("live search source read failed")))
    }
    fn message(&mut self, m: Message) -> Flow {
        if self.failed {
            return Flow::Stop;
        }
        if self.first_user.is_none() && m.role == Role::User {
            for b in &m.content {
                if let Block::Text { text } = b {
                    // Inline text keeps its full non-whitespace test; lazy labels select the
                    // same checked 512-raw-byte head used by indexed and semantic search.
                    let t = match text.try_resolve_prefix(&self.resolver, 512) {
                        Ok(text) => text,
                        Err(error) => {
                            self.error = Some(error);
                            self.failed = true;
                            return Flow::Stop;
                        }
                    };
                    if !t.trim().is_empty() {
                        self.first_user = Some(t.chars().take(73).collect());
                        break;
                    }
                }
            }
        }
        for b in &m.content {
            let result = match b {
                Block::Text { text } | Block::Thinking { text, .. } => self.push_text(text),
                Block::ToolUse { name, input, .. } => {
                    self.hay.push(name);
                    self.hay.push(" ");
                    self.hay.push_json(input).map(|()| self.hay.push("\n"))
                }
                Block::ToolResult { content, .. } => self.push_text(content),
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        self.hay.push(p);
                        self.hay.push("\n");
                    }
                    Ok(())
                }
                Block::Image { .. } => Ok(()),
            };
            if let Err(error) = result {
                self.error = Some(error);
                self.failed = true;
                return Flow::Stop;
            }
        }
        if self.hay.full() && self.first_user.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// One live-scan match: the discovered ref, the label to show for it, and the excerpt around the
/// first occurrence of the needle.
pub struct LiveHit {
    pub session: SessionRef,
    pub title: String,
    pub snippet: String,
}

/// The result of a live scan: the hits in discovery order, and whether the scan stopped early
/// because it filled `limit` (so a caller can say "there may be more").
pub struct LiveSearch {
    pub hits: Vec<LiveHit>,
    pub truncated: bool,
}

/// Case-insensitive substring search across every session's capped head, stopping at `limit` hits.
///
/// Trade-off, stated plainly: a match beyond a session's capped head is missed here; finding those
/// is the index's job (`cv index`).
pub fn live_search(query: &str, want: Option<Harness>, limit: usize) -> Result<LiveSearch> {
    let needle = query.to_lowercase();
    let mut out = LiveSearch {
        hits: Vec::new(),
        truncated: false,
    };
    for adapter in crate::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.roots().is_empty() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = CapSink::new(&r.path);
            let meta = adapter
                .try_stream(&r, &ParseOptions::lazy(), &mut sink)
                .with_context(|| format!("scanning {}:{}", r.harness, r.id))?;
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.as_str().to_lowercase();
            let Some(pos) = low.find(&needle) else { continue };
            out.hits.push(LiveHit {
                title: crate::label_from(meta.title.as_deref(), sink.first_user.as_deref()),
                snippet: snippet(sink.hay.as_str(), pos.min(sink.hay.len()), needle.len()),
                session: r,
            });
            if out.hits.len() >= limit {
                out.truncated = true;
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// A one-line excerpt of `hay` around `pos`, with ~40 bytes of lead-in and lead-out.
pub fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = (pos + len + 40).min(hay.len());
    let s = &hay[floor_char(hay, start)..ceil_char(hay, end)];
    truncate(&s.replace('\n', " "), 120)
}

/// Round `i` down/up to the nearest char boundary of `s` (both clamp to `s.len()` first).
pub fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// See [`floor_char`].
pub fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_head_caps_arguments_paths_and_labels_together() {
        let mut sink = CapSink::new(Path::new("unused"));
        let input = serde_json::json!({"content": "🌙\n".repeat(LIVE_HAY_BYTES)});
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text { text: "prompt".into() },
            Block::ToolUse {
                id: "t".into(),
                name: "Edit".into(),
                input: input.clone().into(),
                namespace: None,
            },
            Block::File {
                mime: None,
                path: Some("later".repeat(LIVE_HAY_BYTES)),
                source: None,
            },
        ];
        assert_eq!(sink.message(message), Flow::Stop);
        let complete = format!("prompt\nEdit {input}\nlater");
        assert_eq!(sink.hay.as_str(), &complete[..floor_char(&complete, LIVE_HAY_BYTES)]);
        assert_eq!(sink.first_user.as_deref(), Some("prompt"));
        assert!(sink.hay.capacity() <= LIVE_HAY_BYTES);
    }

    #[test]
    fn failed_text_demand_stays_latched_for_body_and_label() {
        let dir = cv_testkit::temp_dir("live-head-text-error");
        let path = dir.join("absent");
        for role in [Role::User, Role::Assistant] {
            let mut sink = CapSink::new(&path);
            let mut message = Message::new(role);
            message.content.push(Block::Text {
                text: Text::Span(crate::Span {
                    source: Some(path.clone()),
                    offset: 0,
                    len: 8192,
                    escaped: false,
                }),
            });
            assert!(sink.try_message(message).is_err());
            assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
            assert!(sink.take_error().is_some());
            assert!(sink.hay.is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lazy_label_uses_prefix_policy_and_inline_checks_full_whitespace() {
        let dir = cv_testkit::temp_dir("live-head-lazy-label");
        let path = dir.join("text");
        let content = format!("{}late meaningful text", " ".repeat(1024));
        std::fs::write(&path, &content).unwrap();
        let mut lazy = CapSink::new(&path);
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text {
            text: Text::Span(crate::Span {
                source: Some(path.clone()),
                offset: 0,
                len: content.len() as u64,
                escaped: false,
            }),
        });
        assert_eq!(lazy.message(message), Flow::Continue);
        assert!(lazy.first_user.is_none());
        assert!(lazy.hay.as_str().contains("late meaningful text"));
        let mut inline = CapSink::new(&path);
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text { text: content.into() });
        inline.message(message);
        assert_eq!(inline.first_user.as_deref(), Some(" ".repeat(73).as_str()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_source_is_not_a_successful_short_head() {
        let dir = cv_testkit::temp_dir("live-head-source-error");
        let path = dir.join("input.json");
        std::fs::write(&path, "{}").unwrap();
        let input = crate::json::Json::source(&path, 0, 2).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut sink = CapSink::new(&path);
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "t".into(),
            name: "Edit".into(),
            input,
            namespace: None,
        });
        assert!(sink.try_message(message).is_err());
        assert_eq!(sink.message(Message::new(Role::User)), Flow::Stop);
        assert!(sink.take_error().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snippets_window_the_match_and_stay_on_char_boundaries() {
        let hay = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
        let s = snippet(&hay, 100, 6);
        assert!(s.contains("NEEDLE"), "{s}");
        assert!(s.starts_with(&"a".repeat(40)) && s.len() <= 120, "{s}");
        // Multi-byte text must not be sliced mid-codepoint.
        let uni = "日本語テキスト NEEDLE 日本語テキスト";
        let pos = uni.find("NEEDLE").unwrap();
        assert!(snippet(uni, pos, 6).contains("NEEDLE"));
        // An out-of-range position clamps to an empty window instead of panicking.
        assert_eq!(snippet("short", 900, 6), "");
    }
}
