//! Bounded text projections shared by search and API consumers.
use crate::{Resolver, Text};

/// A decoded UTF-8 byte prefix. Once a scalar does not fit, later writes cannot skip it.
/// All text, punctuation, paths, names and streamed JSON formatting share this one budget.
pub struct Head {
    text: String,
    limit: usize,
    stopped: bool,
}
impl Head {
    pub fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit),
            limit,
            stopped: false,
        }
    }
    pub fn remaining(&self) -> usize {
        if self.stopped {
            0
        } else {
            self.limit - self.text.len()
        }
    }
    pub fn full(&self) -> bool {
        self.remaining() == 0
    }
    pub fn push(&mut self, text: &str) {
        if self.full() {
            return;
        }
        let mut end = text.len().min(self.remaining());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.stopped |= end < text.len();
    }
    pub fn push_text(&mut self, text: &Text, resolver: &Resolver) {
        if self.full() {
            return;
        }
        // Preserve the existing raw-prefix selection for spans. Decoded malformed UTF-8 may
        // expand beyond this raw budget, so still apply the final decoded-byte bound.
        let prefix = text.resolve_prefix(resolver, self.remaining() as u64);
        self.push(&prefix);
    }
    pub fn push_json(&mut self, json: &crate::json::Json) -> anyhow::Result<()> {
        if !self.full() {
            let (prefix, truncated) = json.canonical_prefix(self.remaining())?;
            self.push(&prefix);
            self.stopped |= truncated;
        }
        Ok(())
    }
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn len(&self) -> usize {
        self.text.len()
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.text.capacity()
    }
    pub fn into_string(self) -> String {
        self.text
    }
}
