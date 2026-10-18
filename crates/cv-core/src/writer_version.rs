//! Writer versions observed during normal discovery, compared with frozen corpus coverage.
use std::cmp::Ordering;

/// Only harnesses whose discovery reads observe a writer declaration are listed here.
/// The roster lives inside the published crate; its test checks the corpus manifests.
pub const VERIFIED: &[(&str, &str)] = &[("claude", "2.1.296"), ("codex", "0.162.1")];

pub struct Observed {
    pub harness: &'static str,
    pub newest: Option<String>,
    pub sessions: usize,
    pub unknown: usize,
}

pub(crate) fn empty_observations() -> Vec<Observed> {
    VERIFIED
        .iter()
        .map(|(harness, _)| Observed {
            harness,
            newest: None,
            sessions: 0,
            unknown: 0,
        })
        .collect()
}

fn numeric(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn parts(version: &str) -> Option<(Vec<&str>, Option<Vec<&str>>)> {
    let mut build = version.split('+');
    let version = build.next()?;
    if let Some(build) = build.next() {
        if !identifiers(build) || build.is_empty() {
            return None;
        }
    }
    if build.next().is_some() {
        return None;
    }
    let (core, pre) = version.split_once('-').map_or((version, None), |(c, p)| (c, Some(p)));
    let core: Vec<_> = core.split('.').collect();
    if core.len() != 3
        || core
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let pre = match pre {
        Some(p) if identifiers(p) => Some(p.split('.').collect()),
        Some(_) => return None,
        None => None,
    };
    Some((core, pre))
}

fn identifiers(text: &str) -> bool {
    text.split('.')
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Numeric release components, then SemVer prerelease ordering; build metadata has no precedence.
/// Unsupported declarations are unknown rather than lexicographically newer releases.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let (a, ap) = parts(a)?;
    let (b, bp) = parts(b)?;
    for (a, b) in a.iter().zip(&b) {
        let order = numeric(a, b);
        if order != Ordering::Equal {
            return Some(order);
        }
    }
    Some(match (ap, bp) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let mut order = Ordering::Equal;
            for (a, b) in a.iter().zip(&b) {
                order = match (
                    a.bytes().all(|c| c.is_ascii_digit()),
                    b.bytes().all(|c| c.is_ascii_digit()),
                ) {
                    (true, true) => numeric(a, b),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => a.cmp(b),
                };
                if order != Ordering::Equal {
                    break;
                }
            }
            if order == Ordering::Equal {
                a.len().cmp(&b.len())
            } else {
                order
            }
        }
    })
}

/// Fold an actual declaration into historical knowledge. Returns whether it is understood.
pub(crate) fn observe(newest: &mut Option<String>, declaration: Option<&str>) -> bool {
    let Some(version) = declaration.filter(|v| compare(v, v).is_some()) else {
        return false;
    };
    if newest
        .as_deref()
        .is_none_or(|old| compare(version, old).is_none_or(|o| o.is_gt()))
    {
        *newest = Some(version.to_owned());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_prerelease_and_unknown() {
        for (a, b) in [
            ("2.10.0", "2.9.99"),
            ("0.1.0", "0.1.0-rc.9"),
            ("0.1.0-rc.10", "0.1.0-rc.2"),
            ("0.1.0-beta", "0.1.0-9"),
        ] {
            assert_eq!(compare(a, b), Some(Ordering::Greater));
        }
        assert_eq!(compare("1.2.3+a", "1.2.3+b"), Some(Ordering::Equal));
        assert_eq!(compare("unknown", "1.0.0"), None);
        let mut newest = None;
        for v in [Some("2.10.0"), None, Some("unknown"), Some("2.9.0")] {
            observe(&mut newest, v);
        }
        assert_eq!(newest.as_deref(), Some("2.10.0"));
    }

    #[test]
    fn packaged_roster_matches_newest_corpus_manifests() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        for &(harness, verified) in VERIFIED {
            let mut newest = None;
            for entry in std::fs::read_dir(corpus.join(harness)).unwrap() {
                let path = entry.unwrap().path().join("MANIFEST.toml");
                if !path.exists() {
                    continue;
                }
                let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                assert_eq!(manifest["harness"].as_str(), Some(harness));
                assert!(observe(&mut newest, manifest["version"].as_str()));
            }
            assert_eq!(newest.as_deref(), Some(verified), "update {harness}'s packaged roster");
        }
    }
}
