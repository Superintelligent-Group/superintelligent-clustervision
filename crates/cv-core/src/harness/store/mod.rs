//! Store helpers shared by every adapter: the on-disk plumbing that is the same whichever harness
//! wrote the files. An adapter imports these and keeps only what is genuinely its own — where its
//! files are and what its records mean.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// `$XDG_DATA_HOME` when set to an absolute path (the spec says a relative value is ignored), else
/// `~/.local/share`. The base of every XDG-shaped store (opencode, devin, goose, zed); `None` only
/// when there is no home directory at all.
pub(crate) fn xdg_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
}

/// Write `lines` as JSONL (one compact JSON value per line) to `path`, replacing the file.
pub(crate) fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut buf = String::new();
    for v in lines {
        buf.push_str(&serde_json::to_string(v)?);
        buf.push('\n');
    }
    std::fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
