//! Fallible projections of tool arguments, shared by affordance and event reducers.
use crate::json::JsonView;
use anyhow::Result;

/// Enough decoded text to reproduce `truncate(text, cap)`, across arbitrary chunk boundaries.
pub(crate) struct Prefix {
    text: String,
    chars: usize,
    cap: usize,
}
impl Prefix {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            cap,
        }
    }
    pub(crate) fn push(&mut self, text: &str) {
        for ch in text.chars().take((self.cap + 1).saturating_sub(self.chars)) {
            self.text.push(ch);
            self.chars += 1;
        }
    }
    pub(crate) fn finish(self) -> String {
        crate::ir::truncate(&self.text, self.cap)
    }
}

/// Materialize one demanded string, never its sibling argument fields or a Value tree.
pub(crate) fn string(view: JsonView<'_>) -> Result<Option<String>> {
    let mut out = String::new();
    Ok(view
        .for_each_str_chunk(|chunk| {
            out.push_str(chunk);
            Ok(())
        })?
        .then_some(out))
}

pub(crate) fn field_string(view: JsonView<'_>, key: &str) -> Result<Option<String>> {
    view.get(key)?.map(string).transpose().map(Option::flatten)
}

pub(crate) fn prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let mut out = Prefix::new(cap);
    Ok(view
        .for_each_str_chunk(|chunk| {
            out.push(chunk);
            Ok(())
        })?
        .then(|| out.finish()))
}

pub(crate) fn scalar_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    if let Some(text) = prefix(view, cap)? {
        return Ok(Some(text));
    }
    Ok(view
        .as_number()?
        .and_then(|number| number.as_u64())
        .map(|number| number.to_string()))
}

/// Scan before allocating so an all-whitespace string emits no owned item. Trimming uses decoded
/// byte bounds; a huge ignored whitespace tail does not become a transient String allocation.
pub(crate) fn item_string(view: JsonView<'_>, trim: bool) -> Result<Option<String>> {
    let mut offset = 0;
    let mut start = None;
    let mut end = 0;
    if !view.for_each_str_chunk(|chunk| {
        for (at, ch) in chunk.char_indices() {
            if !ch.is_whitespace() {
                start.get_or_insert(offset + at);
                end = offset + at + ch.len_utf8();
            }
        }
        offset += chunk.len();
        Ok(())
    })? {
        return Ok(None);
    }
    let Some(start) = start else {
        return Ok(None);
    };
    if !trim {
        return string(view);
    }
    let mut out = String::new();
    offset = 0;
    view.for_each_str_chunk(|chunk| {
        let lo = start.saturating_sub(offset).min(chunk.len());
        let hi = end.saturating_sub(offset).min(chunk.len());
        if lo < hi {
            out.push_str(&chunk[lo..hi]);
        }
        offset += chunk.len();
        Ok(())
    })?;
    Ok(Some(out))
}

/// Exact membership without allocating an arbitrarily long action/status string.
pub(crate) fn word(view: JsonView<'_>, words: &[&str]) -> Result<Option<usize>> {
    let cap = words.iter().map(|word| word.len()).max().unwrap_or(0);
    let mut text = String::new();
    let mut bytes = 0;
    if !view.for_each_str_chunk(|chunk| {
        bytes += chunk.len();
        if text.len() <= cap {
            for ch in chunk.chars() {
                text.push(ch);
                if text.len() > cap {
                    break;
                }
            }
        }
        Ok(())
    })? {
        return Ok(None);
    }
    Ok(words.iter().position(|word| word.len() == bytes && *word == text))
}

/// Trim both ends before truncating, preserving a long whitespace tail's non-ellipsis behavior.
pub(crate) fn trimmed_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let mut prefix = String::new();
    let mut chars = 0;
    let mut started = false;
    let mut later_content = false;
    let is_string = view.for_each_str_chunk(|mut chunk| {
        if !started {
            chunk = chunk.trim_start();
            started = !chunk.is_empty();
        }
        let mut end = 0;
        for (offset, ch) in chunk.char_indices().take((cap + 1usize).saturating_sub(chars)) {
            prefix.push(ch);
            chars += 1;
            end = offset + ch.len_utf8();
        }
        later_content |= !chunk[end..].trim().is_empty();
        Ok(())
    })?;
    Ok(is_string.then(|| crate::ir::truncate(if later_content { &prefix } else { prefix.trim_end() }, cap)))
}

/// The untrimmed prefix of a nonblank string, for object item text whose whitespace is retained.
pub(crate) fn nonblank_prefix(view: JsonView<'_>, cap: usize) -> Result<Option<String>> {
    let mut out = Prefix::new(cap);
    let mut nonblank = false;
    let is_string = view.for_each_str_chunk(|chunk| {
        out.push(chunk);
        nonblank |= !chunk.trim().is_empty();
        Ok(())
    })?;
    Ok((is_string && nonblank).then(|| out.finish()))
}
