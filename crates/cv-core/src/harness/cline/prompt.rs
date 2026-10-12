//! Prompt metadata without retaining the prompt. Marker positions use decoded UTF-8 bytes;
//! a second bounded traversal selects the same first prose/task line as the owned codec.

use anyhow::Result;
use std::path::PathBuf;

#[derive(Default, Debug)]
pub(super) struct Summary {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub task: bool,
    pub environment: bool,
}

struct Marker {
    needle: &'static [u8],
    tail: Vec<u8>,
    found: Option<u64>,
}
impl Marker {
    fn new(needle: &'static [u8]) -> Self {
        Self {
            needle,
            tail: Vec::new(),
            found: None,
        }
    }
    fn feed(&mut self, chunk: &[u8], position: u64, after: u64) {
        if self.found.is_some() {
            return;
        }
        let mut boundary = self.tail.clone();
        boundary.extend_from_slice(&chunk[..chunk.len().min(self.needle.len() - 1)]);
        let base = position - self.tail.len() as u64;
        self.found = memchr::memmem::find_iter(&boundary, self.needle)
            .map(|i| base + i as u64)
            .find(|&i| i >= after);
        if self.found.is_none() {
            let start = after.saturating_sub(position).min(chunk.len() as u64) as usize;
            self.found = memchr::memmem::find(&chunk[start..], self.needle).map(|i| position + (start + i) as u64);
        }
        if chunk.len() >= self.needle.len() - 1 {
            self.tail.clear();
            self.tail
                .extend_from_slice(&chunk[chunk.len() - (self.needle.len() - 1)..]);
        } else {
            self.tail.extend_from_slice(chunk);
            let keep = self.needle.len() - 1;
            if self.tail.len() > keep {
                self.tail.drain(..self.tail.len() - keep);
            }
        }
    }
}

#[derive(Default)]
struct FirstLine {
    prefix: String,
    chars: u64,
    last_nonwhite: u64,
    title: Option<String>,
}
impl FirstLine {
    fn feed(&mut self, c: char) {
        if self.title.is_some() {
            return;
        }
        if c == '\n' {
            self.finish_line();
        } else if self.chars != 0 || !c.is_whitespace() {
            if self.chars < 81 {
                self.prefix.push(c);
            }
            self.chars = self.chars.saturating_add(1);
            if !c.is_whitespace() {
                self.last_nonwhite = self.chars;
            }
        }
    }
    fn finish_line(&mut self) {
        if self.title.is_some() {
            return;
        }
        if self.last_nonwhite != 0 {
            let text = if self.last_nonwhite <= 81 {
                self.prefix.trim_end()
            } else {
                &self.prefix
            };
            self.title = Some(crate::ir::truncate(text, 80));
        } else {
            self.prefix.clear();
            self.chars = 0;
        }
    }
}

const CWD_PREFIX: &str = "# Current Working Directory";
#[derive(Default)]
struct Cwd {
    matched: usize,
    // 0: prefix; 1: whitespace before '('; 2: path; 3: ignore line; 4: terminal.
    state: u8,
    path: String,
    result: Option<PathBuf>,
}
impl Cwd {
    fn feed(&mut self, c: char) {
        if self.state == 4 {
            return;
        }
        if c == '\n' {
            if self.state == 1 || self.state == 2 {
                // The owned extractor returns at the first prefix with malformed parentheses.
                self.state = 4;
            } else {
                self.state = 0;
                self.matched = 0;
            }
            return;
        }
        match self.state {
            0 => {
                if self.matched == 0 && c.is_whitespace() {
                    return;
                }
                if CWD_PREFIX.as_bytes().get(self.matched).copied() == Some(c as u8) && c.is_ascii() {
                    self.matched += 1;
                    if self.matched == CWD_PREFIX.len() {
                        self.state = 1;
                    }
                } else {
                    self.state = 3;
                }
            }
            1 if c.is_whitespace() => {}
            1 if c == '(' => self.state = 2,
            1 => self.state = 4,
            2 if c == ')' => {
                let path = self.path.trim();
                if !path.is_empty() {
                    self.result = Some(PathBuf::from(path));
                    self.state = 4;
                } else {
                    self.path.clear();
                    self.state = 3;
                }
            }
            2 => self.path.push(c),
            _ => {}
        }
    }
}

pub(super) fn summarize(mut visit: impl FnMut(&mut dyn FnMut(&str)) -> Result<()>) -> Result<Summary> {
    let mut task = Marker::new(b"<task>");
    let mut task_end = Marker::new(b"</task>");
    let mut environment = Marker::new(b"<environment_details>");
    let mut environment_end = Marker::new(b"</environment_details>");
    let mut cwd = Cwd::default();
    let mut position = 0;
    visit(&mut |chunk| {
        task.feed(chunk.as_bytes(), position, 0);
        if let Some(start) = task.found {
            task_end.feed(chunk.as_bytes(), position, start + task.needle.len() as u64);
        }
        environment.feed(chunk.as_bytes(), position, 0);
        environment_end.feed(chunk.as_bytes(), position, 0);
        for c in chunk.chars() {
            cwd.feed(c);
        }
        position += chunk.len() as u64;
    })?;
    let task_range = task
        .found
        .zip(task_end.found)
        .map(|(start, end)| (start + task.needle.len() as u64, end));
    let environment_range = environment
        .found
        .zip(environment_end.found)
        .filter(|(start, end)| end >= start)
        .map(|(start, end)| (start, end + environment_end.needle.len() as u64));
    let mut explicit = FirstLine::default();
    let mut prose = FirstLine::default();
    position = 0;
    visit(&mut |chunk| {
        for (i, c) in chunk.char_indices() {
            let at = position + i as u64;
            if task_range.is_some_and(|(start, end)| start <= at && at < end) {
                explicit.feed(c);
            }
            if !environment_range.is_some_and(|(start, end)| start <= at && at < end) {
                prose.feed(c);
            }
        }
        position += chunk.len() as u64;
    })?;
    explicit.finish_line();
    prose.finish_line();
    Ok(Summary {
        title: explicit.title.or(prose.title),
        cwd: cwd.result,
        task: task.found.is_some(),
        environment: environment.found.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_prompt_scan_matches_the_owned_metadata_codec_at_every_split() {
        let cases = [
            "plain\nsecond".to_string(),
            "雪  \r\n# Current Working Directory (/a b) Files".to_string(),
            "before\n<task>  \nexplicit é  \nsecond</task>after".to_string(),
            "pre<environment_details>hidden\nmore</environment_details>suffix".to_string(),
            "</environment_details>before<environment_details>hidden".to_string(),
            "<task>  </task>\nplain".to_string(),
            "# Current Working Directory missing\n# Current Working Directory (/late)".to_string(),
            "# Current Working Directory (  )\n# Current Working Directory (/later)".to_string(),
            "<task>".to_string(),
            "x".repeat(100) + "  \n<task>late title</task>",
            "x".to_string() + &" ".repeat(200) + "end",
        ];
        for text in cases {
            for at in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
                let result = summarize(|feed| {
                    feed(&text[..at]);
                    feed(&text[at..]);
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    result.title,
                    crate::harness::cline::clean_user_title(&text).map(|s| crate::ir::truncate(&s, 80)),
                    "at {at}: {text:?}"
                );
                assert_eq!(
                    result.cwd,
                    crate::harness::cline::extract_cwd(&text),
                    "at {at}: {text:?}"
                );
                assert_eq!(result.task, text.contains("<task>"));
                assert_eq!(result.environment, text.contains("<environment_details>"));
            }
        }
    }
}
