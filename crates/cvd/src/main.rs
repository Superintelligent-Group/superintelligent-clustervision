//! `cvd` — a one-release shim. The daemon is `cv` subcommands now: `cvd sync|ls|path` is
//! `cv archive sync|ls|path`, `cvd watch` is `cv watch`, `cvd serve` is `cv serve`. This binary
//! prints that on stderr and execs `cv` (the one beside it, else `cv` on `PATH`) with the mapped
//! arguments, so hooks and launchd jobs written against `cvd` keep working for one release.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mapped = map_args(args);
    let shown: Vec<String> = mapped.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    eprintln!(
        "cvd is deprecated and goes in the next release: running `cv {}`",
        shown.join(" ")
    );
    std::process::exit(exec(cv_path(), mapped));
}

/// `cvd [--home DIR] <sub> …` → the `cv` argv. `--home` (a global flag on cvd) moves after the
/// subcommand, where `cv archive` / `cv watch` take it; `serve` never used it.
fn map_args(args: Vec<OsString>) -> Vec<OsString> {
    let mut home: Vec<OsString> = Vec::new();
    let mut rest: Vec<OsString> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--home") => {
                home.push(a);
                home.extend(it.next());
            }
            Some(s) if s.starts_with("--home=") => home.push(a),
            _ => rest.push(a),
        }
    }
    let sub = rest.first().and_then(|s| s.to_str()).map(str::to_string);
    let mut out: Vec<OsString> = match sub.as_deref() {
        Some("sync" | "ls" | "path") => vec!["archive".into()],
        Some("watch" | "serve") | None => Vec::new(),
        Some(_) => Vec::new(), // `--help`, `--version`, a typo: `cv` answers it
    };
    out.extend(rest);
    if sub.as_deref() != Some("serve") {
        out.extend(home);
    }
    out
}

/// The `cv` installed beside this binary (the pair ships together), else `cv` on `PATH`.
fn cv_path() -> PathBuf {
    let name = if cfg!(windows) { "cv.exe" } else { "cv" };
    std::env::current_exe()
        .ok()
        .and_then(|me| me.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(unix)]
fn exec(bin: PathBuf, args: Vec<OsString>) -> i32 {
    use std::os::unix::process::CommandExt;
    let err = Command::new(&bin).args(args).exec();
    eprintln!("cvd: could not run {}: {err}", bin.display());
    127
}

#[cfg(not(unix))]
fn exec(bin: PathBuf, args: Vec<OsString>) -> i32 {
    match Command::new(&bin).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cvd: could not run {}: {e}", bin.display());
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::map_args;

    fn m(args: &[&str]) -> Vec<String> {
        map_args(args.iter().map(Into::into).collect())
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn archive_verbs_gain_their_noun_and_home_follows_the_subcommand() {
        assert_eq!(m(&["sync"]), ["archive", "sync"]);
        assert_eq!(m(&["--home", "/a", "ls"]), ["archive", "ls", "--home", "/a"]);
        assert_eq!(m(&["path", "--home=/a"]), ["archive", "path", "--home=/a"]);
        assert_eq!(m(&["watch", "--interval", "5"]), ["watch", "--interval", "5"]);
        assert_eq!(m(&["--home", "/a", "watch"]), ["watch", "--home", "/a"]);
        assert_eq!(m(&["serve", "--port", "0"]), ["serve", "--port", "0"]);
        assert_eq!(m(&["--version"]), ["--version"]);
    }
}
