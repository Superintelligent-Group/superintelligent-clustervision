//! Adapter-enforced project-root allowlist (`ADR-067` Step 0).
//!
//! Configured via `CV_ALLOWED_ROOTS` (colon- or semicolon-separated project root paths) and/or
//! repeatable `--allowed-root <PATH>` CLI flags, plus `CV_REQUIRE_SCOPE=1` for fail-closed
//! deployment gating.
//!
//! Invariants:
//! 1. When `CV_REQUIRE_SCOPE=1` is set and the allowlist is empty, [`check_configured`] fails
//!    closed (`Err(ScopeError::Unconfigured)`) and [`allows_cwd`] returns `false` (fail-closed).
//! 2. When the allowlist is non-empty, a session is visible/readable iff its `cwd` is present and
//!    its canonicalized path lies at or under at least one canonicalized allowed root (component
//!    boundary: `/foo/bar` never admits `/foo/bar-evil`, `..` traversals and symlinks are resolved).
//! 3. `--allowed-root` narrows an existing `CV_ALLOWED_ROOTS` allowlist and refuses to widen it.

use std::path::{Component, Path, PathBuf};

use crate::ir::SessionRef;

/// Errors raised by project-root scope enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    /// `CV_REQUIRE_SCOPE=1` is set, but no allowed project roots were configured.
    Unconfigured,
    /// `--allowed-root` attempted to widen an active `CV_ALLOWED_ROOTS` allowlist.
    WideningRefused { requested: PathBuf },
    /// Direct lookup or stream targeted a session outside the allowed project roots.
    OutsideAllowedRoots { id: String, cwd: Option<PathBuf> },
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeError::Unconfigured => write!(
                f,
                "CV_REQUIRE_SCOPE=1 is set, but no allowed project roots are configured \
                 (set CV_ALLOWED_ROOTS or pass --allowed-root)"
            ),
            ScopeError::WideningRefused { requested } => write!(
                f,
                "refusing --allowed-root {}: outside active CV_ALLOWED_ROOTS allowlist",
                requested.display()
            ),
            ScopeError::OutsideAllowedRoots { id, cwd: Some(cwd) } => write!(
                f,
                "refused: session {id:?} (project root {}) is outside allowed project roots",
                cwd.display()
            ),
            ScopeError::OutsideAllowedRoots { id, cwd: None } => write!(
                f,
                "refused: session {id:?} has no project cwd and project-root allowlist is active"
            ),
        }
    }
}

impl std::error::Error for ScopeError {}

/// Resolved project-root scope attached to [`crate::env::Env`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    roots: Vec<PathBuf>,
    require_scope: bool,
}

impl Scope {
    /// Resolve project-root scope from raw environment values.
    pub(crate) fn from_env_values(
        raw_roots: Option<&str>,
        require_scope: bool,
        home: Option<&Path>,
        base_cwd: Option<&Path>,
    ) -> Self {
        let roots = raw_roots
            .map(|raw| split_allowed_roots(raw, home))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| canonicalize_scope_path(&p, base_cwd))
            .collect();
        Scope { roots, require_scope }
    }

    /// Whether scope filtering/enforcement is active (either roots are configured or
    /// `CV_REQUIRE_SCOPE=1` demands a scope).
    pub fn is_active(&self) -> bool {
        self.require_scope || !self.roots.is_empty()
    }

    /// Canonicalized allowed roots.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Fail closed when `CV_REQUIRE_SCOPE=1` is set without any configured allowed roots.
    pub fn check_configured(&self) -> Result<(), ScopeError> {
        if self.require_scope && self.roots.is_empty() {
            return Err(ScopeError::Unconfigured);
        }
        Ok(())
    }

    /// Narrow (or, when unrestricted, establish) allowed roots from CLI `--allowed-root` flags.
    pub fn narrow_with_cli(
        &self,
        cli_roots: &[PathBuf],
        home: Option<&Path>,
        base_cwd: Option<&Path>,
    ) -> Result<Vec<PathBuf>, ScopeError> {
        let mut resolved = Vec::with_capacity(cli_roots.len());
        for raw in cli_roots {
            let expanded = expand_tilde(raw, home);
            let Some(canon) = canonicalize_scope_path(&expanded, base_cwd) else {
                return Err(ScopeError::WideningRefused { requested: raw.clone() });
            };
            if !self.roots.is_empty() && !self.allows_canonical(&canon) {
                return Err(ScopeError::WideningRefused { requested: raw.clone() });
            }
            if !resolved.contains(&canon) {
                resolved.push(canon);
            }
        }
        Ok(resolved)
    }

    /// Whether a session `cwd` is admitted by this scope.
    pub fn allows_cwd(&self, cwd: Option<&Path>, base_cwd: Option<&Path>) -> bool {
        if !self.is_active() {
            return true;
        }
        if self.roots.is_empty() {
            // `require_scope` with empty roots: fail closed even for infallible callers.
            return false;
        }
        let Some(cwd) = cwd.filter(|p| !p.as_os_str().is_empty()) else {
            return false;
        };
        let Some(canon) = canonicalize_scope_path(cwd, base_cwd) else {
            return false;
        };
        self.allows_canonical(&canon)
    }

    fn allows_canonical(&self, candidate: &Path) -> bool {
        self.roots.iter().any(|root| path_within(candidate, root))
    }
}

/// Whether project-root scope is active in the current [`crate::env::Env`].
pub fn is_active() -> bool {
    crate::env::Env::current().scope().is_active()
}

/// Fail closed when `CV_REQUIRE_SCOPE=1` is set with an empty allowlist.
pub fn check_configured() -> anyhow::Result<()> {
    crate::env::Env::current().scope().check_configured()?;
    Ok(())
}

/// Whether `cwd` is inside the current environment's project-root allowlist.
pub fn allows_cwd(cwd: Option<&Path>) -> bool {
    let env = crate::env::Env::current();
    env.scope().allows_cwd(cwd, crate::env::current_dir().as_deref())
}

/// Whether session ref `r` is inside the current environment's project-root allowlist.
pub fn allows_ref(r: &SessionRef) -> bool {
    allows_cwd(r.cwd.as_deref())
}

/// Retain only sessions inside the active project-root allowlist.
pub fn filter_refs(mut refs: Vec<SessionRef>) -> Vec<SessionRef> {
    let env = crate::env::Env::current();
    let scope = env.scope();
    if !scope.is_active() {
        return refs;
    }
    let base = crate::env::current_dir();
    refs.retain(|r| scope.allows_cwd(r.cwd.as_deref(), base.as_deref()));
    refs
}

/// Adapter-level guard: verify scope configuration and ensure `r` lies within allowed roots.
pub fn ensure_ref_allowed(r: &SessionRef) -> anyhow::Result<()> {
    let env = crate::env::Env::current();
    let scope = env.scope();
    scope.check_configured()?;
    if !scope.allows_cwd(r.cwd.as_deref(), crate::env::current_dir().as_deref()) {
        return Err(ScopeError::OutsideAllowedRoots {
            id: r.id.clone(),
            cwd: r.cwd.clone(),
        }
        .into());
    }
    Ok(())
}

/// Split `CV_ALLOWED_ROOTS` on `;` and OS path separators (`:` on Unix, `;` on Windows).
pub fn split_allowed_roots(raw: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for semi_part in raw.split(';') {
        let trimmed = semi_part.trim();
        if trimmed.is_empty() {
            continue;
        }
        for p in std::env::split_paths(trimmed) {
            let s = p.to_string_lossy();
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            let expanded = expand_tilde(Path::new(s), home);
            if !out.contains(&expanded) {
                out.push(expanded);
            }
        }
    }
    out
}

/// Format resolved roots back into an env-var string suitable for `CV_ALLOWED_ROOTS`.
pub(crate) fn join_allowed_roots(roots: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(h)) if rest.as_os_str().is_empty() => h.to_path_buf(),
        (Ok(rest), Some(h)) => h.join(rest),
        _ => path.to_path_buf(),
    }
}

/// Canonicalize `path` for prefix containment, resolving symlinks on existing prefixes (and
/// dangling symlinks) as well as `.` and `..` components on non-existent tails.
pub fn canonicalize_scope_path(path: &Path, base_cwd: Option<&Path>) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let abs = if path.is_absolute() || path.to_string_lossy().starts_with('/') {
        path.to_path_buf()
    } else {
        base_cwd
            .map(|b| b.join(path))
            .or_else(|| std::env::current_dir().ok().map(|b| b.join(path)))?
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return Some(canon);
    }
    resolve_components(&abs, 0)
}

fn resolve_components(path: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 32 {
        return None;
    }
    let mut acc = PathBuf::new();
    #[cfg(windows)]
    let normalized = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    let iter_path = Path::new(&normalized);
    #[cfg(not(windows))]
    let iter_path = path;

    for comp in iter_path.components() {
        match comp {
            Component::Prefix(p) => acc.push(p.as_os_str()),
            Component::RootDir => acc.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(acc.components().next_back(), Some(Component::Normal(_))) {
                    acc.pop();
                }
            }
            Component::Normal(seg) => {
                acc.push(seg);
                #[cfg(not(target_arch = "wasm32"))]
                {
                    match std::fs::symlink_metadata(&acc) {
                        Ok(md) if md.file_type().is_symlink() => match std::fs::canonicalize(&acc) {
                            Ok(canon) => acc = canon,
                            Err(_) => {
                                let target = std::fs::read_link(&acc).ok()?;
                                acc.pop();
                                let joined = if target.is_absolute() { target } else { acc.join(target) };
                                acc = resolve_components(&joined, depth + 1)?;
                            }
                        },
                        Ok(_) => {
                            if let Ok(canon) = std::fs::canonicalize(&acc) {
                                acc = canon;
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }
    (!acc.as_os_str().is_empty()).then_some(acc)
}

#[cfg(not(windows))]
fn path_within(candidate: &Path, root: &Path) -> bool {
    candidate.starts_with(root)
}

#[cfg(windows)]
fn path_within(candidate: &Path, root: &Path) -> bool {
    let mut c_iter = candidate.components();
    for r_comp in root.components() {
        let Some(c_comp) = c_iter.next() else {
            return false;
        };
        if !c_comp
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&r_comp.as_os_str().to_string_lossy())
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_collision_dotdot_traversal_and_missing_cwd_are_enforced() {
        let scope = Scope::from_env_values(Some("/foo/bar;/opt/sig"), false, None, None);
        assert!(scope.is_active());
        assert!(scope.check_configured().is_ok());

        for allowed in [
            "/foo/bar",
            "/foo/bar/",
            "/foo/bar/sub",
            "/foo/bar/a/../sub",
            "/opt/sig/crate",
        ] {
            assert!(
                scope.allows_cwd(Some(Path::new(allowed)), None),
                "expected allowed: {allowed}"
            );
        }
        for rejected in [
            "/foo/bar-evil",
            "/foo/bar_other",
            "/foo/ba",
            "/foo/bar/../bar-evil",
            "/foo/bar/missing/../../bar-evil",
            "/elsewhere",
            "",
        ] {
            assert!(
                !scope.allows_cwd(Some(Path::new(rejected)), None),
                "expected rejected: {rejected}"
            );
        }
        assert!(!scope.allows_cwd(None, None), "missing cwd must fail closed");
    }

    #[test]
    fn colon_and_semicolon_lists_and_require_scope_fail_closed() {
        let unconfigured = Scope::from_env_values(Some(" : ; "), true, None, None);
        assert!(unconfigured.is_active());
        assert_eq!(unconfigured.check_configured(), Err(ScopeError::Unconfigured));
        assert!(!unconfigured.allows_cwd(Some(Path::new("/foo/bar")), None));

        let configured = Scope::from_env_values(Some("/a/one:/b/two;/c/three"), true, None, None);
        assert!(configured.check_configured().is_ok());
        assert_eq!(configured.roots().len(), 3);
        assert!(configured.allows_cwd(Some(Path::new("/b/two/src")), None));
        assert!(!configured.allows_cwd(Some(Path::new("/b/two-evil")), None));
    }

    #[test]
    fn cli_roots_may_narrow_env_allowlist_but_never_widen() {
        let env_scope = Scope::from_env_values(Some("/sig/repo"), false, None, None);
        let narrowed = env_scope
            .narrow_with_cli(&[PathBuf::from("/sig/repo/crates/cv")], None, None)
            .unwrap();
        assert_eq!(narrowed.len(), 1);

        let widened = env_scope.narrow_with_cli(&[PathBuf::from("/sig/repo-other")], None, None);
        assert!(matches!(widened, Err(ScopeError::WideningRefused { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_dangling_symlink_escape_are_rejected() {
        let base = std::env::temp_dir().join(format!("cv-scope-symlink-{}", uuid::Uuid::new_v4()));
        let allowed = base.join("allowed");
        let outside = base.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("dangling-target"), allowed.join("dangling")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), allowed.join("sublink")).unwrap();

        let scope = Scope::from_env_values(Some(&allowed.to_string_lossy()), false, None, None);
        assert!(scope.allows_cwd(Some(&allowed.join("real-nonexistent-child")), None));
        assert!(!scope.allows_cwd(Some(&allowed.join("escape")), None));
        assert!(!scope.allows_cwd(Some(&allowed.join("escape/nonexistent")), None));
        assert!(!scope.allows_cwd(Some(&allowed.join("dangling/child")), None));
        assert!(!scope.allows_cwd(Some(&allowed.join("sublink/..")), None));

        std::fs::remove_dir_all(&base).ok();
    }
}
