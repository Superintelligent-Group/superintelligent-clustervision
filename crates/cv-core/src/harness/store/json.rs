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

pub(crate) fn value_compatible_at_depth_with(b: &[u8], mut depth: usize, mut string: impl FnMut(usize, usize)) -> bool {
    if std::str::from_utf8(b).is_err() {
        return false;
    }
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
                string(start, end);
                i = end;
                continue;
            }
            b'{' | b'[' => {
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
        }
    }

    #[test]
    fn rejects_non_utf8_ignored_text() {
        assert!(!value_compatible(b"{\"ignored\":\"\xff\"}"));
    }
}
