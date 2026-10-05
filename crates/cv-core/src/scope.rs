//! Project visibility shared by discovery and persisted search indexes.
//!
//! CLI roots override CV_ALLOW_ROOTS (an OS path list: ";" on Windows, ":" on Unix),
//! which overrides config allow_roots. Matching is lexical, on directory boundaries;
//! it does not require the project to still exist on this machine.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::SessionRef;

static CLI_ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Install a CLI override before starting discovery or worker threads.
pub fn set_roots(roots: Vec<PathBuf>) {
    CLI_ROOTS.set(roots).expect("scope already initialized");
}

/// The effective project allowlist for this process.
pub fn roots() -> Vec<PathBuf> {
    if let Some(roots) = CLI_ROOTS.get() {
        return roots.clone();
    }
    if let Some(value) = std::env::var_os("CV_ALLOW_ROOTS") {
        return std::env::split_paths(&value)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
    }
    crate::config::load().allow_roots
}

/// Whether a known project directory is visible. Unknown directories fail closed when scoped.
pub fn allows(cwd: Option<&Path>, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return true;
    }
    let Some(cwd) = cwd.filter(|p| !p.as_os_str().is_empty()) else {
        return false;
    };
    let cwd = normalized(cwd);
    roots.iter().filter(|r| !r.as_os_str().is_empty()).any(|root| {
        let root = normalized(root);
        cwd == root || cwd.starts_with(&format!("{}/", root.trim_end_matches('/')))
    })
}

fn normalized(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    // Root-relative Windows paths (e.g. /work) retain their lexical root.
    if !path.is_absolute() && !value.starts_with('/') {
        if let Ok(base) = std::env::current_dir() {
            value = format!("{}/{}", base.to_string_lossy().replace('\\', "/"), value);
        }
    }
    let unc = value.starts_with("//");
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p: &&str| !p.ends_with(':')) {
                    parts.pop();
                }
            }
            _ => parts.push(part),
        }
    }
    let prefix = if unc {
        "//"
    } else if value.starts_with('/') {
        "/"
    } else {
        ""
    };
    let result = format!("{prefix}{}", parts.join("/"));
    if cfg!(windows) {
        result.to_lowercase()
    } else {
        result
    }
}

pub(crate) fn filter(mut refs: Vec<SessionRef>) -> Vec<SessionRef> {
    let roots = roots();
    refs.retain(|r| allows(r.cwd.as_deref(), &roots));
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_boundaries_and_unknown_cwd() {
        let roots = vec![PathBuf::from("/work/proj/")];
        for cwd in [
            "/work/proj",
            "/work/proj/src",
            "/work//proj/./src",
            "/work/proj/a/../src",
        ] {
            assert!(allows(Some(Path::new(cwd)), &roots), "{cwd}");
        }
        for cwd in ["/work/project", "/work/proj-other", "/work/proj/../other", ""] {
            assert!(!allows(Some(Path::new(cwd)), &roots), "{cwd}");
        }
        assert!(!allows(None, &roots));
        assert!(allows(None, &[]));
        assert!(allows(Some(Path::new("/elsewhere")), &[]));
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_and_separators() {
        let roots = vec![PathBuf::from(r"C:\Work\Project\")];
        assert!(allows(Some(Path::new("c:/work/PROJECT/src")), &roots));
        assert!(!allows(Some(Path::new("C:/Work/Project-other")), &roots));
    }
}
