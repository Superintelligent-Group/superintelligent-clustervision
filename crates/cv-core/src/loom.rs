//! 🧵 Splice — compose a NEW session from spans of existing ones.
//!
//! Because every harness parses into one IR, a transcript is just an ordered list of messages you can
//! cut and recombine. [`splice`] concatenates message spans from several sessions into a fresh
//! [`Session`] with provenance; a verbatim prefix (`a:..N`) forks a lane at message N, and two spans
//! (`base:..N other:M..`) graft one continuation onto another's head. The result is a normal session
//! you `emit` into any harness.
//!
//! Splice is a **selector**: it chooses messages and nothing more. Threading, open tool calls and
//! sidechain identity are the Claude emitter's rules (`harness::claude::emit::make_resumable`): a span's source
//! parents that still resolve are kept, the cut points are re-linked, a span lifted from a sub-agent
//! joins the main thread. `cv splice` calls this.

use crate::ir::{Harness, Message, Session};

/// A contiguous span of messages from a source session: `[start, end)` (None end → through the last).
pub struct Span<'a> {
    pub source: &'a Session,
    pub start: usize,
    pub end: Option<usize>,
}

impl<'a> Span<'a> {
    /// The clamped, panic-free `[start, end)` range into `source.messages`.
    ///
    /// Out-of-range indices saturate to the message count; an inverted or empty range
    /// (`end <= start`) yields an empty range so iteration produces nothing.
    fn range(&self) -> std::ops::Range<usize> {
        let len = self.source.messages.len();
        let start = self.start.min(len);
        let end = self.end.unwrap_or(len).min(len);
        // Treat end < start (and end == start) as an empty span.
        start..end.max(start)
    }

    /// The messages this span actually selects, in order. Never panics.
    fn messages(&self) -> &'a [Message] {
        &self.source.messages[self.range()]
    }
}

/// Concatenate `spans` (in order) into a new [`Session`] with `harness` and a fresh id (or
/// `new_id`). Messages are cloned as they are — ids and parents included — and the first span's
/// session provides default cwd/title/model.
pub fn splice(spans: &[Span<'_>], new_id: Option<String>, harness: Harness) -> Session {
    let id = new_id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // Defaults come from the first span's source session (if any).
    let first_source = spans.first().map(|s| s.source);
    let cwd = first_source.and_then(|s| s.cwd.clone());
    let title = first_source.and_then(|s| s.title.clone());
    let model = first_source.and_then(|s| s.model.clone());
    let git = first_source.and_then(|s| s.git.clone());

    // Concatenate the selected messages (cloned) in span order, and record provenance per span.
    let mut messages: Vec<Message> = Vec::new();
    let mut provenance: Vec<serde_json::Value> = Vec::with_capacity(spans.len());
    for span in spans {
        let range = span.range();
        provenance.push(serde_json::json!({
            "source_id": span.source.id,
            "harness": span.source.harness.as_str(),
            "start": range.start,
            "end": range.end,
        }));
        messages.extend(span.messages().iter().cloned());
    }

    // created_at = min, updated_at = max of the included messages' timestamps (when present).
    let created_at = messages.iter().filter_map(|m| m.timestamp).min();
    let updated_at = messages.iter().filter_map(|m| m.timestamp).max();

    // Provenance is cv's own fact about a session cv synthesized, not something read out of a
    // harness store, so it lives in the `cv` namespace — `Session::extra` has no flat keys
    // (docs/INTERFACE-V2.md §4).
    let mut extra = serde_json::Map::new();
    extra.insert(
        crate::ir::CV_NAMESPACE.into(),
        serde_json::json!({ "loom": serde_json::Value::Array(provenance) }),
    );

    Session {
        id,
        harness,
        cwd,
        title,
        created_at,
        updated_at,
        model,
        git,
        messages,
        source_path: None,
        extra,
        system_prompt: None,
        lineage: crate::ir::Lineage::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Message, Role};
    use chrono::{TimeZone, Utc};

    /// Build a session with `n` messages, each tagged with a deterministic id, parent chain, text,
    /// and an increasing timestamp so we can assert created/updated bounds.
    fn session(id: &str, harness: Harness, n: usize) -> Session {
        let mut messages = Vec::new();
        let mut prev: Option<String> = None;
        for i in 0..n {
            let mid = format!("{id}-m{i}");
            let mut m = Message::new(if i % 2 == 0 { Role::User } else { Role::Assistant });
            m.id = Some(mid.clone());
            m.parent_id = prev.take();
            m.timestamp = Some(Utc.timestamp_opt(1_000 + i as i64, 0).unwrap());
            m.content = vec![Block::Text {
                text: format!("{id}#{i}").into(),
            }];
            prev = Some(mid);
            messages.push(m);
        }
        Session {
            id: id.to_string(),
            harness,
            cwd: Some("/tmp/a".into()),
            title: Some(format!("{id} title")),
            created_at: None,
            updated_at: None,
            model: Some("m-1".into()),
            git: None,
            messages,
            source_path: Some("/tmp/orig.jsonl".into()),
            extra: serde_json::Map::new(),
            system_prompt: None,
            lineage: crate::ir::Lineage::default(),
        }
    }

    fn texts(s: &Session) -> Vec<String> {
        s.messages.iter().filter_map(|m| m.text()).collect()
    }

    #[test]
    fn splice_count_order_and_defaults() {
        let a = session("A", Harness::Claude, 4);
        let b = session("B", Harness::Codex, 3);

        // Take A[1..3] then B[0..2].
        let spliced = splice(
            &[
                Span {
                    source: &a,
                    start: 1,
                    end: Some(3),
                },
                Span {
                    source: &b,
                    start: 0,
                    end: Some(2),
                },
            ],
            None,
            Harness::Gemini,
        );

        // (a) count + order.
        assert_eq!(spliced.messages.len(), 4);
        assert_eq!(texts(&spliced), vec!["A#1", "A#2", "B#0", "B#1"]);

        // (b) a selector copies messages as they are: source ids and parents ride along, and the
        // emitter re-links the cut points (`harness::claude::emit::make_resumable`).
        assert_eq!(spliced.messages[0].id.as_deref(), Some("A-m1"));
        assert_eq!(spliced.messages[0].parent_id.as_deref(), Some("A-m0"));
        assert_eq!(spliced.messages[2].parent_id, None);

        // Fresh id generated, harness honored, defaults from first span's source.
        assert!(!spliced.id.is_empty());
        assert_ne!(spliced.id, "A");
        assert_eq!(spliced.harness, Harness::Gemini);
        assert_eq!(spliced.cwd, Some("/tmp/a".into()));
        assert_eq!(spliced.title, Some("A title".into()));
        assert_eq!(spliced.model, Some("m-1".into()));
        assert_eq!(spliced.source_path, None);

        // created/updated bounds from included timestamps:
        // A#1=1001, A#2=1002, B#0=1000, B#1=1001 → min 1000, max 1002.
        assert_eq!(spliced.created_at, Some(Utc.timestamp_opt(1_000, 0).unwrap()));
        assert_eq!(spliced.updated_at, Some(Utc.timestamp_opt(1_002, 0).unwrap()));
    }

    #[test]
    fn provenance_recorded_in_extra() {
        let a = session("A", Harness::Claude, 4);
        let b = session("B", Harness::Codex, 3);
        let spliced = splice(
            &[
                Span {
                    source: &a,
                    start: 1,
                    end: Some(3),
                },
                Span {
                    source: &b,
                    start: 0,
                    end: None,
                },
            ],
            Some("fixed-id".into()),
            Harness::Claude,
        );

        assert_eq!(spliced.id, "fixed-id");
        // A synthesized session obeys the same `extra` rule as a parsed one: no flat keys, every
        // top-level key a namespace object. `loom` used to sit flat here, which quietly made the
        // contract's "a session's extra has no flat keys at all" false for our own output.
        crate::harness::assert_no_flat_keys(&spliced);
        let loom = spliced
            .cv_extra()
            .expect("cv namespace present")
            .get("loom")
            .expect("loom provenance");
        let arr = loom.as_array().expect("loom is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["source_id"], "A");
        assert_eq!(arr[0]["harness"], "claude");
        assert_eq!(arr[0]["start"], 1);
        assert_eq!(arr[0]["end"], 3);
        assert_eq!(arr[1]["source_id"], "B");
        assert_eq!(arr[1]["harness"], "codex");
        assert_eq!(arr[1]["start"], 0);
        assert_eq!(arr[1]["end"], 3); // None → clamped to len.
    }

    #[test]
    fn out_of_range_spans_clamp_without_panic() {
        let a = session("A", Harness::Claude, 3);
        let empty = session("E", Harness::Claude, 0);

        let spliced = splice(
            &[
                // start past the end → empty.
                Span {
                    source: &a,
                    start: 99,
                    end: Some(200),
                },
                // end < start → empty span, not a panic.
                Span {
                    source: &a,
                    start: 2,
                    end: Some(1),
                },
                // end past the end → clamps to 3, yields A#1, A#2.
                Span {
                    source: &a,
                    start: 1,
                    end: Some(999),
                },
                // splicing from an empty session → nothing.
                Span {
                    source: &empty,
                    start: 0,
                    end: None,
                },
            ],
            None,
            Harness::Claude,
        );

        assert_eq!(texts(&spliced), vec!["A#1", "A#2"]);

        // Empty result is fine, too.
        let none = splice(&[], None, Harness::Claude);
        assert!(none.messages.is_empty());
        assert_eq!(none.created_at, None);
        assert_eq!(none.updated_at, None);
        assert!(!none.id.is_empty());
    }
}
