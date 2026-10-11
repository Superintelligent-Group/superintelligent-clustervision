//! `cv-mcp` — a one-release shim. The MCP server is `cv mcp` now; this binary says so on stderr
//! (stdout is the protocol channel) and execs `cv mcp` — the `cv` beside it, else `cv` on `PATH` —
//! so hosts registered against `/path/to/cv-mcp` keep working for one release.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    eprintln!("cv-mcp is deprecated and goes in the next release: register `cv mcp` instead");
    let name = if cfg!(windows) { "cv.exe" } else { "cv" };
    let bin = std::env::current_exe()
        .ok()
        .and_then(|me| me.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name));
    let mut cmd = Command::new(&bin);
    cmd.arg("mcp").args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("cv-mcp: could not run {}: {err}", bin.display());
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("cv-mcp: could not run {}: {e}", bin.display());
            std::process::exit(127);
        }
    }
}
