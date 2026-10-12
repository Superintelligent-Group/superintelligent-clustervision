//! Tool projections retain command prefixes and patch target lines, never unrelated argument trees.
use crate::affordance::input::{self, Prefix};
use crate::json::JsonView;
use anyhow::Result;

const HEADERS: &[&str] = &["*** Update File:", "*** Add File:", "*** Delete File:", "*** Move to:"];

#[derive(Default)]
struct Header {
    probe: String,
    value: Option<String>,
    ignored: bool,
    started: bool,
}
impl Header {
    fn push(&mut self, mut text: &str, prefixes: &[&str], trim_start: bool, capture: bool) {
        if self.ignored {
            return;
        }
        if let Some(value) = &mut self.value {
            value.push_str(text);
            return;
        }
        if trim_start && !self.started {
            text = text.trim_start();
            self.started = !text.is_empty();
        }
        for (offset, ch) in text.char_indices() {
            self.probe.push(ch);
            if prefixes.contains(&self.probe.as_str()) {
                self.value = Some(if capture {
                    text[offset + ch.len_utf8()..].to_string()
                } else {
                    String::new()
                });
                return;
            }
            if !prefixes.iter().any(|prefix| prefix.starts_with(&self.probe)) {
                self.ignored = true;
                self.probe.clear();
                return;
            }
        }
    }
}

/// A non-header line is discarded as soon as its prefix differs. Only actual candidate path
/// lines and the deduplicated output paths are retained; a demanded giant path can still be giant.
#[derive(Default)]
pub(super) struct Patch {
    codex: Header,
    unified: Header,
    old: Header,
    primary: Vec<String>,
    fallback: Vec<String>,
    has_old: bool,
}
impl Patch {
    pub(super) fn push(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            let line = part.strip_suffix('\n').unwrap_or(part);
            self.codex.push(line, HEADERS, true, true);
            self.unified.push(line, &["+++ "], false, true);
            self.old.push(line, &["--- "], false, false);
            self.has_old |= self.old.value.is_some();
            // The --- marker demands no rest of the line.
            if self.has_old {
                self.old.ignored = true;
                self.old.value = None;
            }
            if part.ends_with('\n') {
                self.end_line();
            }
        }
    }
    fn end_line(&mut self) {
        fn add(out: &mut Vec<String>, value: &str) {
            let value = value.trim();
            if !value.is_empty() && value != "/dev/null" && !out.iter().any(|path| path == value) {
                out.push(value.to_string());
            }
        }
        if let Some(value) = self.codex.value.take() {
            add(&mut self.primary, &value);
        }
        if let Some(value) = self.unified.value.take() {
            add(&mut self.fallback, value.trim().trim_start_matches("b/"));
        }
        self.codex = Header::default();
        self.unified = Header::default();
        self.old = Header::default();
    }
    pub(super) fn finish(mut self) -> Vec<String> {
        self.end_line();
        if !self.primary.is_empty() {
            self.primary
        } else if self.has_old {
            self.fallback
        } else {
            Vec::new()
        }
    }
}

fn command(view: JsonView<'_>) -> Result<Option<JsonView<'_>>> {
    for key in ["command", "cmd", "script"] {
        if let Some(value) = view.get(key)? {
            return Ok(Some(value));
        }
    }
    view.get("action")?
        .map(|action| action.get("command"))
        .transpose()
        .map(Option::flatten)
}

/// Visit the exact old string/argv join. Non-string argv entries are ignored; empty strings still
/// occupy a position and therefore introduce separators. Present invalid command keys mask later keys.
fn command_chunks(view: JsonView<'_>, mut chunk: impl FnMut(&str) -> Result<()>) -> Result<bool> {
    let Some(value) = command(view)? else {
        return Ok(false);
    };
    if value.is_string()? {
        return value.for_each_str_chunk(chunk);
    }
    let Some(parts) = value.array()? else {
        return Ok(false);
    };
    let mut found = false;
    for part in parts {
        let part = part?;
        if !part.is_string()? {
            continue;
        }
        if found {
            chunk(" ")?;
        }
        found = true;
        part.for_each_str_chunk(&mut chunk)?;
    }
    Ok(found)
}

pub(super) fn command_string(view: JsonView<'_>) -> Result<Option<String>> {
    let mut text = String::new();
    Ok(command_chunks(view, |chunk| {
        text.push_str(chunk);
        Ok(())
    })?
    .then_some(text))
}

/// Small rolling prefix/substring state; the command itself is never assembled for event rows.
#[derive(Default)]
struct Trigger {
    tail: Vec<u8>,
    marker: bool,
    head: String,
    started: bool,
    decided: bool,
    apply: bool,
}
impl Trigger {
    fn push(&mut self, text: &str) {
        const MARKER: &[u8] = b"*** Begin Patch";
        if !self.marker {
            // Only a marker-sized boundary copy; search the borrowed chunk directly.
            let boundary_len = text.len().min(MARKER.len() - 1);
            self.tail.extend_from_slice(&text.as_bytes()[..boundary_len]);
            self.marker = self.tail.windows(MARKER.len()).any(|window| window == MARKER)
                || memchr::memmem::find(text.as_bytes(), MARKER).is_some();
            if text.len() >= MARKER.len() - 1 {
                self.tail.clear();
                let start = text.len() - (MARKER.len() - 1);
                self.tail.extend_from_slice(&text.as_bytes()[start..]);
            } else {
                let excess = self.tail.len().saturating_sub(MARKER.len() - 1);
                self.tail.drain(..excess);
            }
        }
        if self.decided {
            return;
        }
        let text = if !self.started { text.trim_start() } else { text };
        self.started |= !text.is_empty();
        for ch in text.chars() {
            self.head.push(ch);
            if !"apply_patch".starts_with(&self.head) {
                self.decided = true;
                break;
            }
            if self.head == "apply_patch" {
                self.apply = true;
                self.decided = true;
                break;
            }
        }
    }
}

pub(super) fn command_rows(view: JsonView<'_>, cap: usize) -> Result<Option<(String, Vec<String>)>> {
    let mut prefix = Prefix::new(cap);
    let mut trigger = Trigger::default();
    let exists = command_chunks(view, |chunk| {
        prefix.push(chunk);
        trigger.push(chunk);
        Ok(())
    })?;
    if !exists {
        return Ok(None);
    }
    // Establish whether patch targets are demanded before retaining any candidate path line.
    // Otherwise a giant fake header in an ordinary command would allocate despite emitting no edit.
    let files = if trigger.marker || trigger.apply {
        let mut patch = Patch::default();
        command_chunks(view, |chunk| {
            patch.push(chunk);
            Ok(())
        })?;
        patch.finish()
    } else {
        Vec::new()
    };
    Ok(Some((prefix.finish(), files)))
}

pub(super) fn patch_rows(view: JsonView<'_>) -> Result<Vec<String>> {
    let mut body = view.is_string()?.then_some(view);
    if body.is_none() {
        for key in ["input", "patch"] {
            if let Some(value) = view.get(key)? {
                if value.is_string()? {
                    body = Some(value);
                    break;
                }
            }
        }
    }
    let mut patch = Patch::default();
    if let Some(body) = body {
        body.for_each_str_chunk(|chunk| {
            patch.push(chunk);
            Ok(())
        })?;
    }
    Ok(patch.finish())
}

pub(super) fn file_key(view: JsonView<'_>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        if let Some(value) = view.get(key)? {
            if let Some(path) = input::string(value)? {
                return Ok((!path.is_empty()).then_some(path));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trigger_matches_whole_string_across_every_boundary() {
        for text in [
            "λ *** Begin Patch",
            "  apply_patch",
            "apply_patch_suffix",
            "apply_patc",
            "ordinary",
            "\u{2003}apply_patch",
        ] {
            let expected = text.contains("*** Begin Patch") || text.trim_start().starts_with("apply_patch");
            for split in text.char_indices().map(|(at, _)| at).chain(std::iter::once(text.len())) {
                let mut trigger = Trigger::default();
                trigger.push(&text[..split]);
                trigger.push(&text[split..]);
                assert_eq!(trigger.marker || trigger.apply, expected, "{text:?}, split {split}");
            }
            let mut trigger = Trigger::default();
            for ch in text.chars() {
                trigger.push(ch.encode_utf8(&mut [0; 4]));
            }
            assert_eq!(trigger.marker || trigger.apply, expected);
        }
    }

    #[test]
    fn chunked_patch_matches_owned_oracle() {
        for text in [
            "--- a/a\n+++ b/a\n*** Add File: later\n+body\n*** Move to: renamed\n*** Add File: later\n",
            "--- a/a\n+++ b/a\n+++ b/b\n+++ /dev/null\n",
            "+++ b/not-a-patch\n+irrelevant\n",
            "\u{2003}*** Update File: λ.rs\r\n*** Delete File: /dev/null\n",
        ] {
            let expected = super::super::patch_targets(text);
            for width in 1..32 {
                let mut patch = Patch::default();
                let mut start = 0;
                for (offset, _) in text.char_indices() {
                    if offset - start >= width {
                        patch.push(&text[start..offset]);
                        start = offset;
                    }
                }
                patch.push(&text[start..]);
                assert_eq!(patch.finish(), expected, "width {width}: {text:?}");
            }
        }
    }
}
