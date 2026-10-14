//! Allocation-free checks that serde's ignored/raw values omit. Call only after JSON syntax
//! validation; these checks preserve Value's finite numbers, UTF-8/UTF-16 and depth acceptance.

/// RawValue validates JSON syntax but deliberately skips finite-number and surrogate checks,
/// and its ignored containers do not consume Value's recursion budget. Match Value acceptance
/// before applying replay controls, without decoding or allocating the potentially giant strings.
pub(crate) fn value_compatible(b: &[u8]) -> bool {
    value_compatible_with(b, |_, _| {})
}

/// As `value_compatible`, reporting validated string boundaries for source-slice indexes.
pub(crate) fn value_compatible_with(b: &[u8], mut string: impl FnMut(usize, usize)) -> bool {
    value_compatible_at_depth_with(b, 0, &mut string)
}

pub(crate) fn value_compatible_at_depth_with(b: &[u8], depth: usize, string: impl FnMut(usize, usize)) -> bool {
    let Ok(text) = std::str::from_utf8(b) else {
        return false;
    };
    value_compatible_str_at_depth_with(text, depth, string)
}

/// The source view already owns a UTF-8-validated str. Keep byte-slice entry points strict,
/// without rescanning the whole borrowed document at this stronger boundary. `span` sees the
/// bounds of every string and of every container closed within `text`.
pub(crate) fn value_compatible_str_at_depth_with(
    text: &str,
    mut depth: usize,
    mut span: impl FnMut(usize, usize),
) -> bool {
    let b = text.as_bytes();
    let mut opened = [usize::MAX; 128];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                let Some(end) = string_end(b, i) else {
                    return false;
                };
                if !valid_unicode_escapes(&b[start + 1..end - 1]) {
                    return false;
                }
                span(start, end);
                i = end;
                continue;
            }
            b'{' | b'[' => {
                opened[depth] = i;
                depth += 1;
                if depth >= 128 {
                    return false;
                }
            }
            b'}' | b']' => {
                let Some(next) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next;
                if opened[depth] != usize::MAX {
                    span(opened[depth], i + 1);
                    opened[depth] = usize::MAX;
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while b
                    .get(i)
                    .is_some_and(|c| !c.is_ascii_whitespace() && !b",]}".contains(c))
                {
                    i += 1;
                }
                if serde_json::from_slice::<serde_json::Number>(&b[start..i]).is_err() {
                    return false;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    true
}

/// One pass that accepts exactly what `serde_json::from_str::<Value>` accepts: JSON grammar,
/// strings without control characters or invalid escapes (including lone surrogates), finite
/// numbers, Value's nesting limit, nothing after the value. `span` sees the bounds of every
/// string and container, so a caller can index them for later structural selection. Replaces
/// a serde syntax pass followed by [`value_compatible`] for a record a codec will select from.
pub(crate) fn validate_value(text: &str, mut span: impl FnMut(usize, usize)) -> bool {
    let b = text.as_bytes();
    // Open containers: (start, is_object). Value rejects the 128th level, as value_compatible does.
    let mut open = [(0usize, false); 128];
    let mut depth = 0;
    let mut i = 0;
    loop {
        // A value starts here.
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b'{' | b'[') => {
                let object = b[i] == b'{';
                open[depth] = (i, object);
                depth += 1;
                if depth >= 128 {
                    return false;
                }
                i = skip_ws(b, i + 1);
                if b.get(i) == Some(if object { &b'}' } else { &b']' }) {
                    depth -= 1;
                    span(open[depth].0, i + 1);
                    i += 1;
                } else {
                    if object {
                        let Some(next) = key(b, i, &mut span) else {
                            return false;
                        };
                        i = next;
                    }
                    continue;
                }
            }
            Some(b'"') => {
                let Some(end) = string(b, i) else {
                    return false;
                };
                span(i, end);
                i = end;
            }
            Some(b't') if b[i..].starts_with(b"true") => i += 4,
            Some(b'f') if b[i..].starts_with(b"false") => i += 5,
            Some(b'n') if b[i..].starts_with(b"null") => i += 4,
            Some(b'-' | b'0'..=b'9') => {
                let start = i;
                while b
                    .get(i)
                    .is_some_and(|c| !c.is_ascii_whitespace() && !b",]}".contains(c))
                {
                    i += 1;
                }
                if serde_json::from_slice::<serde_json::Number>(&b[start..i]).is_err() {
                    return false;
                }
            }
            _ => return false,
        }
        // A value ended: close containers until another value is due.
        loop {
            i = skip_ws(b, i);
            if depth == 0 {
                return i == b.len();
            }
            let (start, object) = open[depth - 1];
            match b.get(i) {
                Some(b',') => {
                    i += 1;
                    if object {
                        let Some(next) = key(b, skip_ws(b, i), &mut span) else {
                            return false;
                        };
                        i = next;
                    }
                    break;
                }
                Some(b'}') if object => {}
                Some(b']') if !object => {}
                _ => return false,
            }
            depth -= 1;
            span(start, i + 1);
            i += 1;
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\n' | b'\t' | b'\r')) {
        i += 1;
    }
    i
}

/// An object key and its colon; the offset after the colon.
fn key(b: &[u8], i: usize, span: &mut impl FnMut(usize, usize)) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let end = string(b, i)?;
    span(i, end);
    let i = skip_ws(b, end);
    (b.get(i) == Some(&b':')).then_some(i + 1)
}

/// The end of a valid JSON string starting at the quote `i`.
fn string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        j = special(b, j);
        match *b.get(j)? {
            b'"' => return Some(j + 1),
            b'\\' => match *b.get(j + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => j += 2,
                b'u' => {
                    let code = hex_quad(b.get(j + 2..j + 6))?;
                    j += 6;
                    if (0xd800..=0xdbff).contains(&code) {
                        if b.get(j..j + 2) != Some(b"\\u") {
                            return None;
                        }
                        let low = hex_quad(b.get(j + 2..j + 6))?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        j += 6;
                    } else if (0xdc00..=0xdfff).contains(&code) {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None, // A control character.
        }
    }
}

/// The first quote, backslash or control byte at or after `j`, eight bytes at a time.
fn special(b: &[u8], mut j: usize) -> usize {
    const ONES: u64 = u64::from_ne_bytes([1; 8]);
    const HIGH: u64 = u64::from_ne_bytes([0x80; 8]);
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & HIGH;
    while let Some(chunk) = b.get(j..j + 8) {
        let w = u64::from_le_bytes(chunk.try_into().unwrap());
        let found = zero(w ^ (ONES * b'"' as u64))
            | zero(w ^ (ONES * b'\\' as u64))
            | (w.wrapping_sub(ONES * 0x20) & !w & HIGH);
        if found != 0 {
            return j + (found.trailing_zeros() / 8) as usize;
        }
        j += 8;
    }
    while b.get(j).is_some_and(|&c| c != b'"' && c != b'\\' && c >= 0x20) {
        j += 1;
    }
    j
}

fn valid_unicode_escapes(b: &[u8]) -> bool {
    let mut i = 0;
    while let Some(off) = memchr::memmem::find(&b[i..], b"\\u") {
        i += off;
        let mut slash = i;
        while slash > 0 && b[slash - 1] == b'\\' {
            slash -= 1;
        }
        if !(i - slash).is_multiple_of(2) {
            i += 2;
            continue; // A literal backslash followed by 'u', rather than a JSON Unicode escape.
        }
        let Some(code) = hex_quad(b.get(i + 2..i + 6)) else {
            return false;
        };
        i += 6;
        if (0xd800..=0xdbff).contains(&code) {
            if b.get(i..i + 2) != Some(b"\\u") {
                return false;
            }
            let Some(low) = hex_quad(b.get(i + 2..i + 6)) else {
                return false;
            };
            if !(0xdc00..=0xdfff).contains(&low) {
                return false;
            }
            i += 6;
        } else if (0xdc00..=0xdfff).contains(&code) {
            return false;
        }
    }
    true
}

fn hex_quad(b: Option<&[u8]>) -> Option<u16> {
    b?.iter()
        .try_fold(0u16, |value, c| Some(value * 16 + char::from(*c).to_digit(16)? as u16))
}

pub(crate) fn string_end(b: &[u8], mut i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    loop {
        i += memchr::memchr(b'"', b.get(i..)?)?;
        let mut slash = i;
        while slash > 0 && b[slash - 1] == b'\\' {
            slash -= 1;
        }
        if (i - slash).is_multiple_of(2) {
            return Some(i + 1);
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preflight_matches_value_validation() {
        let mut cases: Vec<String> = [
            "0",
            "-0",
            "1.5",
            "1e-400",
            "1e400",
            "18446744073709551616",
            "-99999999999999999999999",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        for input in cases {
            let raw: &crate::lazy::RawValue = serde_json::from_str(&input).unwrap();
            assert_eq!(
                value_compatible(raw.get().as_bytes()),
                serde_json::from_str::<serde_json::Value>(&input).is_ok(),
                "{input}"
            );
            assert_eq!(
                value_compatible_str_at_depth_with(raw.get(), 0, |_, _| {}),
                serde_json::from_str::<serde_json::Value>(&input).is_ok(),
                "validated str: {input}"
            );
        }
    }

    /// The one-pass validator is the record acceptance boundary: it must agree with Value on
    /// every input, including near-misses of valid documents.
    #[test]
    fn validate_value_accepts_exactly_what_value_accepts() {
        let mut cases: Vec<String> = [
            "",
            " ",
            "0",
            "-0",
            "01",
            "-",
            "1.",
            ".5",
            "1e",
            "1e+",
            "1E5",
            "-1.5e-3",
            "1e400",
            "18446744073709551616",
            "true",
            "tru",
            "truex",
            "null",
            "nul",
            "false ",
            " \t\r\n{}\n",
            "{}",
            "[]",
            "[,]",
            "[1,]",
            "{,}",
            r#"{"a":}"#,
            r#"{"a"}"#,
            r#"{"a":1,}"#,
            r#"{"a":1 "b":2}"#,
            r#"{1:2}"#,
            r#"[1 2]"#,
            "[1}",
            "{]",
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud83c\udf19""#,
            r#""\\ud800""#,
            r#""\\\ud800""#,
            r#""\ud800\u0041""#,
            r#""\x""#,
            r#""\u12""#,
            r#""\u12g4""#,
            "\"tab\tinside\"",
            "\"nl\ninside\"",
            "\"\u{7f}\"",
            "\"é🌙\"",
            r#"{"\ud800":0}"#,
            r#"{"a":1e400,"a":0}"#,
            r#"{"a":1,"a":2}"#,
            "[true,false,null]",
            "[]]",
            "[[]",
            "\"",
            r#""unterminated"#,
            r#""a"x"#,
            "{} {}",
            "[1] x",
            "\u{a0}1",
            "[-]",
            "[--1]",
            "[1.5.5]",
            "[0x10]",
            "[+1]",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        cases.push("9".repeat(1000));
        for depth in 125..=130 {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
            cases.push(format!("{{\"a\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)));
        }
        // Mutations of a realistic record: every single-byte deletion, and replacements by bytes
        // that matter to the grammar.
        let record = r#"{"type":"user","n":-12.5e2,"ok":true,"z":null,"m":{"c":[{"t":"a\"b\\c\u00e9\ud83c\udf19","x":[1,2,[]]}],"e":{}}}"#;
        for at in 0..record.len() {
            let mut deleted = record.as_bytes().to_vec();
            deleted.remove(at);
            cases.extend(String::from_utf8(deleted).ok());
            for byte in [
                b'"', b'\\', b',', b':', b'{', b'}', b'[', b']', b' ', b'0', b'e', b'-', b'u', 0x01,
            ] {
                let mut replaced = record.as_bytes().to_vec();
                replaced[at] = byte;
                cases.extend(String::from_utf8(replaced).ok());
            }
        }
        for input in cases {
            assert_eq!(
                validate_value(&input, |_, _| {}),
                serde_json::from_str::<serde_json::Value>(&input).is_ok(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn rejects_non_utf8_ignored_text() {
        assert!(!value_compatible(b"{\"ignored\":\"\xff\"}"));
    }
}
