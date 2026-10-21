//! The adapter-boundary fence (the `dependency_fence.rs` shape): a shared module names a harness
//! only inside an exhaustive `match`. Everywhere else the fact belongs to the `Adapter` (a method
//! with a default), so a `Harness::<Name>` literal in product code outside `src/harness/` and
//! `src/formats.rs` is a leak, and any one fails this test.
//!
//! The scan is textual and approximate, on purpose: `#[cfg(test)]`/`#[test]` items are skipped to
//! their closing brace (or `;`), `tests.rs`/`*_tests.rs` files and `tests/` directories are not
//! read, `//` comment lines are ignored, string (raw ones included, one line at a time) and char
//! literals are masked before braces are counted, and a `match` is exhaustive when the literals
//! inside its brace-matched body name every variant of [`Harness::ALL`] (a `_` arm beside a
//! partial list does not count). A file's `use Harness as H` (or `type H = Harness`) makes `H::Name`
//! a literal too, and importing the variants themselves (`Harness::*`, `Harness::{…}`) is a breach,
//! since bare variant names cannot be told from any other. Rust's own exhaustiveness checking is
//! the real judge; this fence only has to tell a per-harness table from a special case.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use cv_core::Harness;
use regex::Regex;

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` under `src/` the fence reads: not an adapter, not the format manifests, not a test
/// file.
fn fenced_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap();
        if path.is_dir() {
            if name != "harness" && name != "tests" {
                fenced_files(&path, out);
            }
        } else if name.ends_with(".rs") && name != "formats.rs" && name != "tests.rs" && !name.ends_with("_tests.rs") {
            out.push(path);
        }
    }
}

/// The line with its comments, string and char literals blanked, so braces and `match` are read
/// from code only. A `//` comment line is blanked whole.
fn code_only(line: &str, strings: &Regex) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return String::new();
    }
    strings
        .replace_all(line, |m: &regex::Captures| " ".repeat(m[0].len()))
        .into_owned()
}

/// Per line: is it product code (not a `#[cfg(test)]`/`#[test]` item)?
fn product_mask(code: &[String]) -> Vec<bool> {
    let mut keep = vec![true; code.len()];
    let mut i = 0;
    while i < code.len() {
        let t = code[i].trim_start();
        if t.starts_with("#[cfg(test)]") || t.starts_with("#[cfg(all(test") || t.starts_with("#[test]") {
            keep[i] = false;
            i += 1;
            while i < code.len() && code[i].trim_start().starts_with("#[") {
                keep[i] = false;
                i += 1;
            }
            // The item: to the brace that closes it, or the `;` that ends a braceless one.
            let mut depth = 0i32;
            let mut opened = false;
            while i < code.len() {
                keep[i] = false;
                for c in code[i].chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                let line = code[i].trim_end();
                i += 1;
                if (opened && depth <= 0) || (!opened && line.ends_with(';')) {
                    break;
                }
            }
            continue;
        }
        i += 1;
    }
    keep
}

/// Line ranges (inclusive) of every `match` body whose literals name every harness variant.
fn exhaustive_ranges(code: &[String], keep: &[bool], literal: &Regex, all: &BTreeSet<String>) -> Vec<(usize, usize)> {
    let word = Regex::new(r"\bmatch\b").unwrap();
    let mut ranges = Vec::new();
    for (start, line) in code.iter().enumerate() {
        if !keep[start] {
            continue;
        }
        for m in word.find_iter(line) {
            // The body opens at the first `{` after `match`, on this line or a later one.
            let mut depth = 0i32;
            let mut opened = false;
            let mut end = None;
            let mut offset = m.end();
            let mut i = start;
            'scan: while i < code.len() {
                for c in code[i][offset..].chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => {
                            depth -= 1;
                            if opened && depth == 0 {
                                end = Some(i);
                                break 'scan;
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
                offset = 0;
            }
            let Some(end) = end else { continue };
            let named: BTreeSet<String> = (start..=end)
                .flat_map(|i| literal.captures_iter(&code[i]).map(|v| v[1].to_string()))
                .collect();
            if all.is_subset(&named) {
                ranges.push((start, end));
            }
        }
    }
    ranges
}

#[test]
fn harness_names_stay_inside_exhaustive_matches() {
    let root = source_root();
    let mut files = Vec::new();
    fenced_files(&root, &mut files);
    assert!(files.len() > 20, "the fence read {} files: wrong root?", files.len());
    // Raw strings first (`r"…"`, `r#"…"#`, `r##"…"##`): a JSON fixture inside one holds quotes
    // and braces that would otherwise end the plain-string match early and unbalance the count.
    let strings =
        Regex::new(r####"\bb?r##".*?"##|\bb?r#".*?"#|\bb?r"[^"]*"|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])'"####).unwrap();
    let alias = Regex::new(r"\bHarness\s+as\s+(\w+)|\btype\s+(\w+)\s*=\s*(?:\w+::)*Harness\s*;").unwrap();
    let variants_import = Regex::new(r"\bHarness::(?:\*|\{)").unwrap();
    let all: BTreeSet<String> = Harness::ALL.iter().map(|h| format!("{h:?}")).collect();

    // A file a parent declares under `#[cfg(test)] mod name;` is a test file by another name.
    let mod_decl = Regex::new(r"\bmod\s+(\w+)\s*;").unwrap();
    let mut test_only = BTreeSet::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        let code: Vec<String> = text.lines().map(|l| code_only(l, &strings)).collect();
        let keep = product_mask(&code);
        let dir = match path.file_name().and_then(|n| n.to_str()) {
            Some("mod.rs") | Some("lib.rs") => path.parent().unwrap().to_path_buf(),
            _ => path.with_extension(""),
        };
        for (i, line) in code.iter().enumerate() {
            if let Some(m) = mod_decl.captures(line).filter(|_| !keep[i]) {
                test_only.insert(dir.join(format!("{}.rs", &m[1])));
                test_only.insert(dir.join(&m[1]).join("mod.rs"));
            }
        }
    }

    let mut sites = Vec::new();
    for path in files.iter().filter(|p| !test_only.contains(*p)) {
        let text = fs::read_to_string(path).unwrap();
        let code: Vec<String> = text.lines().map(|l| code_only(l, &strings)).collect();
        let keep = product_mask(&code);
        let rel = path.strip_prefix(&root).unwrap().to_str().unwrap().to_string();
        // `Harness` and every name this file's product code aliases it to.
        let mut names = vec!["Harness".to_string()];
        for (i, line) in code.iter().enumerate().filter(|(i, _)| keep[*i]) {
            for c in alias.captures_iter(line) {
                names.push(c.get(1).or(c.get(2)).unwrap().as_str().to_string());
            }
            if variants_import.is_match(line) {
                sites.push(format!("{rel}:{}: {}", i + 1, text.lines().nth(i).unwrap().trim()));
            }
        }
        let literal = Regex::new(&format!(r"\b(?:{})::([A-Z][A-Za-z]*)", names.join("|"))).unwrap();
        let fenced = exhaustive_ranges(&code, &keep, &literal, &all);
        for (i, line) in code.iter().enumerate() {
            if !keep[i] || fenced.iter().any(|(a, b)| (*a..=*b).contains(&i)) {
                continue;
            }
            for m in literal.captures_iter(line) {
                let name = &m[1];
                if name == "ALL" || name == "COUNT" {
                    continue;
                }
                sites.push(format!("{rel}:{}: {}", i + 1, text.lines().nth(i).unwrap().trim()));
            }
        }
    }

    assert!(
        sites.is_empty(),
        "adapter boundary fence breached:\n  {}\n\
         The law: a shared module names a harness only inside an exhaustive `match` (a per-harness \
         table). A special case for one harness is an `Adapter` method with a default. If the site \
         is a table, make the match name every variant.",
        sites.join("\n  ")
    );
}
