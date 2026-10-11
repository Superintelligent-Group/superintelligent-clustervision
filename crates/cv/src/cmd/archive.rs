//! `cv archive sync|ls|path` and `cv watch` — the durable, harness-agnostic store (what `cvd` was).
//!
//! The archive is a per-harness directory of serialized IR sessions plus an idempotent JSONL
//! catalog, so a user can centralize agent logs from many machines/harnesses for search and
//! safekeeping. `sync` snapshots once and exits; `watch` follows live activity and re-archives
//! each session as it changes, built on cv-core's live-watch engine.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cv_core::ir::{home_rel, truncate};
use cv_core::watch::{EventKind, Filter, Watcher};
use cv_core::{Harness, Session, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{parse_harness, short_id};

/// One row of the catalog (`catalog.jsonl`). Cheap metadata for listings/search; the full
/// transcript lives in the per-session JSON under `archive/<harness>/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CatalogEntry {
    pub harness: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub message_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub archived_at: DateTime<Utc>,
    /// Path to the serialized session JSON, relative to the archive home.
    pub path: PathBuf,
}

impl CatalogEntry {
    fn key(&self) -> (String, String) {
        (self.harness.clone(), self.id.clone())
    }
}

/// Whether a `store` call actually wrote (the session was new/changed) or was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreOutcome {
    Archived,
    Skipped,
}

/// Handle to the archive root (`$CLUSTERVISION_HOME` or `~/.clustervision`, overridable by `--home`).
pub(crate) struct Archive {
    home: PathBuf,
}

impl Archive {
    /// Resolve the archive home: explicit `--home`, else `$CLUSTERVISION_HOME`, else `~/.clustervision`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(p) = explicit {
            p
        } else if let Some(env) = std::env::var_os("CLUSTERVISION_HOME") {
            PathBuf::from(env)
        } else {
            let base = crate::util::dirs_home().context("could not determine home directory")?;
            base.join(".clustervision")
        };
        Ok(Archive { home })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn catalog_path(&self) -> PathBuf {
        self.home.join("catalog.jsonl")
    }

    /// Serialize catalog writers across processes (parallel `sync` / `watch` on one archive): an
    /// exclusive advisory flock, released when the returned handle drops. It lives on a dedicated
    /// lockfile rather than the catalog itself, because compaction rename-replaces the catalog and
    /// a lock on the replaced inode would guard nothing.
    fn lock_catalog(&self) -> Result<fs::File> {
        let path = self.home.join("catalog.jsonl.lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false) // the lockfile is a pure rendezvous point; contents are irrelevant
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        f.lock().with_context(|| format!("locking {}", path.display()))?;
        Ok(f)
    }

    /// Relative (to home) path where a session's JSON lives.
    fn rel_session_path(harness: Harness, id: &str) -> PathBuf {
        PathBuf::from("archive")
            .join(harness.as_str())
            .join(format!("{id}.json"))
    }

    fn session_path(&self, harness: Harness, id: &str) -> PathBuf {
        self.home.join(Self::rel_session_path(harness, id))
    }

    /// Persist a session to `archive/<harness>/<id>.json` and update the catalog. Idempotent:
    /// if the serialized bytes are byte-identical to what's already on disk, returns `Skipped`
    /// without rewriting anything.
    pub fn store(&self, session: &Session) -> Result<StoreOutcome> {
        let json = serde_json::to_vec_pretty(session).with_context(|| format!("serializing session {}", session.id))?;

        let dest = self.session_path(session.harness, &session.id);
        if let Ok(existing) = fs::read(&dest) {
            if existing == json {
                return Ok(StoreOutcome::Skipped);
            }
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&dest, &json).with_context(|| format!("writing {}", dest.display()))?;

        let entry = CatalogEntry {
            harness: session.harness.as_str().to_string(),
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            title: session.title.clone().or_else(|| Some(session.label())),
            message_count: session.messages.len(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            archived_at: Utc::now(),
            path: Self::rel_session_path(session.harness, &session.id),
        };
        self.upsert_catalog(entry)?;
        Ok(StoreOutcome::Archived)
    }

    /// Read the catalog, deduped by (harness, id) with the newest `archived_at` winning.
    pub fn load_catalog(&self) -> Result<Vec<CatalogEntry>> {
        let path = self.catalog_path();
        let mut map: BTreeMap<(String, String), CatalogEntry> = BTreeMap::new();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<CatalogEntry>(line) {
                Ok(entry) => {
                    let key = entry.key();
                    match map.get(&key) {
                        Some(prev) if prev.archived_at >= entry.archived_at => {}
                        _ => {
                            map.insert(key, entry);
                        }
                    }
                }
                Err(e) => eprintln!("cv archive: skipping corrupt catalog line: {e}"),
            }
        }
        Ok(map.into_values().collect())
    }

    /// Record an entry by **appending** one line — O(1) per archive instead of rewriting the whole
    /// catalog every time (which made N archives O(N²)). `load_catalog` already dedups by key with
    /// the newest `archived_at` winning, so superseded lines are harmless until we compact. When the
    /// raw line count grows well past the number of unique entries, rewrite once to reclaim space.
    fn upsert_catalog(&self, entry: CatalogEntry) -> Result<()> {
        // Held across the append *and* the possible compaction: a compactor's read-rewrite-rename
        // would otherwise silently drop a line another process appended in between.
        let _lock = self.lock_catalog()?;
        let path = self.catalog_path();
        let line = serde_json::to_string(&entry)?;
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {} for append", path.display()))?;
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.maybe_compact_catalog()?;
        Ok(())
    }

    /// Rewrite the catalog to one line per unique entry, but only when the on-disk line count has
    /// drifted far enough past the unique count to be worth the full rewrite (amortizes to O(1)).
    /// Caller must hold the catalog lock (see [`Self::lock_catalog`]).
    fn maybe_compact_catalog(&self) -> Result<()> {
        let path = self.catalog_path();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let raw_lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
        let entries = self.load_catalog()?;
        // Compact only once we're carrying ~2x dead weight (and past a small floor).
        if raw_lines < 64 || raw_lines < entries.len() * 2 {
            return Ok(());
        }
        let tmp = path.with_extension(format!("{}.jsonl.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            for e in &entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.flush()?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("finalizing {}", path.display()))?;
        Ok(())
    }
}

/// A cwd for logs/listings: `~`-relative, or `-` when unknown.
fn pretty_cwd(cwd: Option<&Path>) -> String {
    cwd.map(home_rel).unwrap_or_else(|| "-".to_string())
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// `cv archive sync`: discover and archive every session once, then exit.
pub(crate) fn cmd_sync(archive: &Archive) -> Result<()> {
    let refs = cv_core::discover_all();
    let total = refs.len();
    let mut archived = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        match adapter.parse(&r) {
            Ok(session) => match archive.store(&session) {
                Ok(StoreOutcome::Archived) => archived += 1,
                Ok(StoreOutcome::Skipped) => skipped += 1,
                Err(e) => {
                    failed += 1;
                    eprintln!("cv archive: failed to archive {} {}: {e:#}", r.harness, r.id);
                }
            },
            Err(e) => {
                failed += 1;
                eprintln!("cv archive: failed to parse {} {}: {e:#}", r.harness, r.id);
            }
        }
    }

    if failed > 0 {
        eprintln!("cv archive: {failed} session(s) failed (logged above)");
    }
    println!("sync complete: {archived} archived, {skipped} skipped (unchanged) of {total} discovered");
    println!("archive: {}", archive.home().display());
    Ok(())
}

/// `cv watch`: follow live activity and archive sessions as they change, forever.
pub(crate) fn cmd_watch(archive: &Archive, interval: u64, harness: Option<String>, cwd: Option<String>) -> Result<()> {
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    // Emit existing sessions on the first poll so a fresh watcher archives what's already there
    // before following live activity (`sync` is the dedicated one-shot, but watch is self-sufficient).
    let mut watcher = Watcher::new(filter, true);
    let interval = Duration::from_secs(interval.max(1));

    eprintln!(
        "cv watch: watching (interval {}s) -> {}",
        interval.as_secs(),
        archive.home().display()
    );

    // Not `Watcher::run` (which never returns and takes the closure by value): a manual loop keeps
    // ownership of `archive` and reads identically.
    loop {
        for ev in watcher.poll() {
            archive_event(archive, &ev.reference, ev.kind, ev.new_messages.len());
        }
        std::thread::sleep(interval);
    }
}

/// Re-parse and archive the session behind an event, logging the outcome to stderr.
fn archive_event(archive: &Archive, r: &SessionRef, kind: EventKind, delta: usize) {
    let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
        return;
    };
    let session = match adapter.parse(r) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[parse-fail] {} {}: {e:#}", r.harness, short_id(&r.id));
            return;
        }
    };
    match archive.store(&session) {
        Ok(StoreOutcome::Archived) => {
            let verb = match kind {
                EventKind::New => "new",
                EventKind::Updated => "archived",
            };
            eprintln!(
                "[{verb}] {} {}  +{delta} msgs  {}",
                r.harness,
                short_id(&r.id),
                pretty_cwd(r.cwd.as_deref())
            );
        }
        Ok(StoreOutcome::Skipped) => {}
        Err(e) => eprintln!("[store-fail] {} {}: {e:#}", r.harness, short_id(&r.id)),
    }
}

/// `cv archive ls`: what's in the archive, newest activity first.
pub(crate) fn cmd_ls(archive: &Archive) -> Result<()> {
    let mut entries = archive.load_catalog()?;
    if entries.is_empty() {
        println!("(archive empty — run `cv archive sync`)");
        println!("archive: {}", archive.home().display());
        return Ok(());
    }
    entries.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(b.archived_at)
            .cmp(&a.updated_at.unwrap_or(a.archived_at))
    });

    for e in &entries {
        let title = e.title.clone().unwrap_or_else(|| "(untitled)".to_string());
        let archived = e.archived_at.format("%Y-%m-%d %H:%M");
        println!(
            "{:<9} {:<10} {:>4} msgs  {}  [{}]  {}",
            e.harness,
            short_id(&e.id),
            e.message_count,
            archived,
            pretty_cwd(e.cwd.as_deref()),
            truncate(&title.replace('\n', " "), 60),
        );
    }
    println!("\n{} session(s) in {}", entries.len(), archive.home().display());
    Ok(())
}

/// `cv archive path`: print the archive location.
pub(crate) fn cmd_path(archive: &Archive) -> Result<()> {
    println!("{}", archive.home().display());
    Ok(())
}
