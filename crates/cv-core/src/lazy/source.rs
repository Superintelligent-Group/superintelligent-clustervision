//! Retained JSON-string backing with original source provenance and bounded decoded demands.
use crate::json::{Json, JsonRange};
use anyhow::{Context, Result};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SourceText {
    pub(crate) range: Arc<JsonRange>,
    pub(crate) skip: usize,
}
impl PartialEq for SourceText {
    fn eq(&self, other: &Self) -> bool {
        self.skip == other.skip && self.range.same_range(&other.range)
    }
}
impl Eq for SourceText {}
#[derive(Debug)]
struct PrefixComplete;
impl std::fmt::Display for PrefixComplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source text prefix complete")
    }
}
impl std::error::Error for PrefixComplete {}
impl SourceText {
    pub(crate) fn new(range: Arc<JsonRange>) -> Self {
        Self { range, skip: 0 }
    }
    pub fn path(&self) -> &std::path::Path {
        self.range.path()
    }
    pub fn dependencies(&self) -> Option<&crate::source::ReadSet> {
        self.range.dependencies()
    }
    pub(crate) fn json(&self) -> Json {
        Json::Source(self.range.clone())
    }
    pub fn raw_len(&self) -> usize {
        usize::try_from(self.range.len().saturating_sub(2))
            .unwrap_or(usize::MAX)
            .saturating_sub(self.skip)
    }
    pub fn try_for_each_chunk(&self, chunk: usize, mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
        let mut skip = self.skip;
        let mut scratch = String::new();
        self.range.with_view(|view| {
            anyhow::ensure!(
                view.for_each_str_chunk(|mut part| {
                    if skip >= part.len() {
                        skip -= part.len();
                        return Ok(());
                    }
                    part = part.get(skip..).context("source text prefix is not a UTF8 boundary")?;
                    skip = 0;
                    while !part.is_empty() {
                        let mut end = part.len().min(chunk.max(1));
                        while !part.is_char_boundary(end) {
                            end += 1;
                        }
                        // Stable scratch remains valid if a callback mutates/truncates the original.
                        scratch.clear();
                        scratch.push_str(&part[..end]);
                        let result = f(&scratch);
                        self.range.check()?;
                        result?;
                        part = &part[end..];
                    }
                    Ok(())
                })?,
                "source text backing is not a JSON string"
            );
            anyhow::ensure!(skip == 0, "source text prefix is past end");
            Ok(())
        })
    }
    pub fn try_resolve(&self) -> Result<String> {
        let mut output = String::new();
        self.try_for_each_chunk(64 * 1024, |part| {
            output.push_str(part);
            Ok(())
        })?;
        Ok(output)
    }
    /// A decoded UTF8 byte prefix; unlike raw Span caps, this carrier has strict JSON semantics.
    pub fn try_prefix(&self, cap: usize) -> Result<String> {
        let mut output = String::with_capacity(cap.min(64 * 1024));
        let result = self.try_for_each_chunk(64 * 1024, |part| {
            let mut end = part.len().min(cap - output.len());
            while !part.is_char_boundary(end) {
                end -= 1;
            }
            let required = output.len() + end;
            if required > output.capacity() {
                output.reserve_exact(required - output.len());
            }
            output.push_str(&part[..end]);
            if end < part.len() {
                return Err(PrefixComplete.into());
            }
            Ok(())
        });
        match result {
            Err(error) if error.downcast_ref::<PrefixComplete>().is_some() => {}
            other => other?,
        }
        Ok(output)
    }
    pub fn try_decoded_len(&self) -> Result<usize> {
        let mut len = 0usize;
        self.try_for_each_chunk(64 * 1024, |part| {
            len = len.checked_add(part.len()).context("source text length overflow")?;
            Ok(())
        })?;
        Ok(len)
    }
}
