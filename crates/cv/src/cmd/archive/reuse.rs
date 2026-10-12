//! Consumer source stamps and bounded temporary serialization for the durable archive.
use super::*;
use std::io::{BufReader, BufWriter, Read};

const STATE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "/archive-full1");
const STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Artifact {
    bytes: u64,
    modified_ns: u64,
    identity: Option<(u64, u64, i64, i64)>,
    hash: Option<String>,
}
impl Artifact {
    fn of(path: &Path) -> Option<Self> {
        let md = fs::metadata(path).ok()?;
        let modified_ns = md
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as u64;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            Some((md.dev(), md.ino(), md.ctime(), md.ctime_nsec()))
        };
        #[cfg(not(unix))]
        let identity = None;
        let hash = if identity.is_none() {
            let mut input = fs::File::open(path).ok()?;
            let mut digest = cv_core::digest::Sha256::new();
            let mut bytes = [0; 64 * 1024];
            loop {
                let n = input.read(&mut bytes).ok()?;
                if n == 0 {
                    break;
                }
                digest.update(&bytes[..n]);
            }
            Some(digest.finish_hex())
        } else {
            None
        };
        Some(Self {
            bytes: md.len(),
            modified_ns,
            identity,
            hash,
        })
    }
    fn matches(&self, path: &Path) -> bool {
        Self::of(path).as_ref() == Some(self)
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: String,
    total: usize,
    artifact: Artifact,
    read: cv_core::watch::ReadState,
}

pub(super) struct StoredPass {
    pub outcome: StoreOutcome,
    pub total: usize,
    pub read: cv_core::watch::ReadState,
    #[cfg(test)]
    pub parsed_messages: usize,
    #[cfg(test)]
    pub resumed: bool,
    #[cfg(test)]
    pub suffix_bytes: u64,
}

struct Temporary {
    path: PathBuf,
    file: Option<fs::File>,
}
impl Temporary {
    fn new(dest: &Path) -> Result<Self> {
        let parent = dest.parent().context("archive path has no parent")?;
        fs::create_dir_all(parent)?;
        let path = parent.join(format!(".cv-archive-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self { path, file: Some(file) })
    }
    fn write_json(&mut self, value: &impl Serialize, pretty: bool) -> Result<()> {
        let mut writer = BufWriter::with_capacity(64 * 1024, self.file.as_mut().unwrap());
        if pretty {
            serde_json::to_writer_pretty(&mut writer, value)?;
        } else {
            serde_json::to_writer(&mut writer, value)?;
        }
        writer.flush()?;
        Ok(())
    }
    fn publish(mut self, dest: &Path) -> Result<()> {
        drop(self.file.take());
        fs::rename(&self.path, dest).with_context(|| format!("finalizing {}", dest.display()))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn equal_files(left: &Path, right: &Path) -> bool {
    fn compare(left: &Path, right: &Path) -> std::io::Result<bool> {
        let mut left = fs::File::open(left)?;
        let mut right = fs::File::open(right)?;
        if left.metadata()?.len() != right.metadata()?.len() {
            return Ok(false);
        }
        let mut a = [0; 64 * 1024];
        let mut b = [0; 64 * 1024];
        loop {
            let n = left.read(&mut a)?;
            right.read_exact(&mut b[..n])?;
            if a[..n] != b[..n] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
        }
    }
    compare(left, right).unwrap_or(false)
}

fn entry(session: &Session) -> CatalogEntry {
    CatalogEntry {
        harness: session.harness.as_str().into(),
        id: session.id.clone(),
        cwd: session.cwd.clone(),
        title: session.title.clone().or_else(|| Some(session.label())),
        message_count: session.messages.len(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        archived_at: Utc::now(),
        path: Archive::rel_session_path(session.harness, &session.id),
    }
}

impl Archive {
    fn source_path(&self, r: &SessionRef) -> PathBuf {
        self.home
            .join("archive-source")
            .join(r.harness.as_str())
            .join(format!("{}.json", r.id))
    }

    fn stored(&self, r: &SessionRef) -> Option<Stored> {
        let path = self.source_path(r);
        if fs::metadata(&path).ok()?.len() > STATE_BYTES {
            return None;
        }
        let stored: Stored = serde_json::from_reader(BufReader::new(fs::File::open(path).ok()?)).ok()?;
        (stored.version == STATE_VERSION && stored.artifact.matches(&self.session_path(r.harness, &r.id)))
            .then_some(stored)
    }

    /// Serialize without a whole JSON/comparison Vec. Artifact rename and catalog append share
    /// the existing cross-process archive lock; a failure never exposes a partial JSON file.
    #[cfg(test)]
    pub(super) fn store_streaming(&self, session: &Session) -> Result<StoreOutcome> {
        self.store_artifact(session, false).map(|(outcome, _)| outcome)
    }

    fn store_artifact(&self, session: &Session, repair_catalog: bool) -> Result<(StoreOutcome, Artifact)> {
        let dest = self.session_path(session.harness, &session.id);
        let mut temporary = Temporary::new(&dest)?;
        temporary
            .write_json(session, true)
            .with_context(|| format!("serializing session {}", session.id))?;
        let _lock = self.lock_catalog()?;
        let outcome = if equal_files(&temporary.path, &dest) {
            // Repair an artifact whose bytes were published before a crash/error prevented the
            // catalog append. The identity comparison and repair share the writer lock.
            if repair_catalog {
                self.upsert_catalog_locked(entry(session))?;
            }
            StoreOutcome::Skipped
        } else {
            temporary.publish(&dest)?;
            self.upsert_catalog_locked(entry(session))?;
            StoreOutcome::Archived
        };
        let artifact = Artifact::of(&dest).context("archive artifact disappeared during publication")?;
        Ok((outcome, artifact))
    }

    /// Skip unchanged sources before parsing. A verified append replaces the archived suffix
    /// after the checkpoint, preserving held EOF messages and exact final session metadata.
    pub(super) fn store_ref(&self, adapter: &dyn cv_core::Adapter, r: &SessionRef) -> Result<StoredPass> {
        let previous = self.stored(r);
        if let Some(stored) = previous.as_ref().filter(|stored| stored.read.fresh_with(adapter, r)) {
            return Ok(StoredPass {
                outcome: StoreOutcome::Skipped,
                total: stored.total,
                read: stored.read.clone(),
                #[cfg(test)]
                parsed_messages: 0,
                #[cfg(test)]
                resumed: false,
                #[cfg(test)]
                suffix_bytes: 0,
            });
        }
        let mut pass = cv_core::watch::read_session(
            adapter,
            r,
            previous.as_ref().map(|stored| &stored.read),
            previous.as_ref().map_or(0, |stored| stored.total),
        )?;
        let mut prefix = Vec::new();
        if pass.resumed {
            let dest = self.session_path(r.harness, &r.id);
            let loaded = (|| -> Option<Session> {
                let stored = previous.as_ref()?;
                if !stored.artifact.matches(&dest) {
                    return None;
                }
                let session: Session = serde_json::from_reader(BufReader::new(fs::File::open(&dest).ok()?)).ok()?;
                (session.id == r.id
                    && session.harness == r.harness
                    && session.messages.len() == stored.total
                    && pass.prefix <= session.messages.len()
                    && stored.artifact.matches(&dest))
                .then_some(session)
            })();
            match loaded {
                Some(mut session) => {
                    session.messages.truncate(pass.prefix);
                    prefix = session.messages;
                }
                None => {
                    pass = cv_core::watch::read_session(adapter, r, None, 0)?;
                }
            }
        }
        let read = pass.accepted(pass.total);
        let total = pass.total;
        #[cfg(test)]
        let (parsed_messages, resumed) = (pass.parsed_messages, pass.resumed);
        #[cfg(test)]
        let suffix_bytes = pass.suffix_bytes;
        prefix.append(&mut pass.messages);
        let mut session = pass.session;
        session.messages = prefix;
        anyhow::ensure!(
            session.messages.len() == total,
            "archive prefix length does not match the completed read"
        );
        let (outcome, artifact) = self.store_artifact(&session, previous.is_none())?;
        if read.fits_cache(STATE_BYTES as usize - 1024) && read.fresh_with(adapter, r) {
            let stored = Stored {
                version: STATE_VERSION.into(),
                total,
                artifact,
                read: read.clone(),
            };
            let dest = self.source_path(r);
            let mut temporary = Temporary::new(&dest)?;
            temporary.write_json(&stored, false)?;
            let _lock = self.lock_catalog()?;
            if stored.artifact.matches(&self.session_path(r.harness, &r.id)) && read.fresh_with(adapter, r) {
                temporary.publish(&dest)?;
            }
        }
        Ok(StoredPass {
            outcome,
            total,
            read,
            #[cfg(test)]
            parsed_messages,
            #[cfg(test)]
            resumed,
            #[cfg(test)]
            suffix_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(harness: Harness) -> (PathBuf, Archive, SessionRef, String) {
        let root = std::env::temp_dir().join(format!("cv-archive-reuse-{}", uuid::Uuid::new_v4()));
        let input = root.join("input");
        fs::create_dir_all(&input).unwrap();
        let r = SessionRef::new("read", harness, input.join("read.jsonl"));
        let a = Archive::resolve(Some(root.join("archive"))).unwrap();
        let mut body = String::new();
        for i in 0..8192 {
            let line = match harness {
                Harness::Claude => {
                    serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read", "message":{"role":"user","content":format!("{i} {}", "x".repeat(256))}})
                }
                Harness::Codex => {
                    serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":format!("{i} {}", "x".repeat(256))}})
                }
                _ => unreachable!(),
            };
            body.push_str(&line.to_string());
            body.push('\n');
        }
        fs::write(&r.path, &body).unwrap();
        (root, a, r, body)
    }
    fn exact(a: &Archive, adapter: &dyn cv_core::Adapter, r: &SessionRef) {
        let expected = serde_json::to_vec_pretty(&adapter.parse(r).unwrap()).unwrap();
        assert_eq!(fs::read(a.session_path(r.harness, &r.id)).unwrap(), expected);
    }

    #[test]
    fn streamed_artifact_write_is_idempotent_and_owned_temps_clean_up() {
        let (root, a, r, _) = setup(Harness::Claude);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let session = adapter.parse(&r).unwrap();
        assert_eq!(a.store(&session).unwrap(), StoreOutcome::Archived);
        assert_eq!(a.store(&session).unwrap(), StoreOutcome::Skipped);
        exact(&a, adapter.as_ref(), &r);
        let dest = a.session_path(r.harness, &r.id);
        let temporary = Temporary::new(&dest).unwrap();
        let own = temporary.path.clone();
        assert!(own.exists());
        drop(temporary);
        assert!(!own.exists());
        assert!(fs::read_dir(dest.parent().unwrap()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_sync_appends_rewrites_dependencies_and_artifact_repairs_are_exact() {
        let (root, a, r, mut body) = setup(Harness::Claude);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let first = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!((first.outcome, first.parsed_messages), (StoreOutcome::Archived, 8192));
        for _ in 0..4 {
            let cold = Archive::resolve(Some(a.home.clone())).unwrap();
            let idle = cold.store_ref(adapter.as_ref(), &r).unwrap();
            assert_eq!(
                (idle.outcome, idle.parsed_messages, idle.suffix_bytes),
                (StoreOutcome::Skipped, 0, 0)
            );
        }
        for i in 8192..8196 {
            body.push_str(&serde_json::json!({"type":"user","uuid":format!("u{i}"),"sessionId":"read","message":{"role":"user","content":format!("append {i}")}}).to_string());
            body.push('\n');
        }
        fs::write(&r.path, &body).unwrap();
        let appended = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(appended.resumed);
        assert!(appended.parsed_messages < 1024);
        assert!(appended.suffix_bytes < 300 * 1024);
        exact(&a, adapter.as_ref(), &r);
        eprintln!("archive repeated polls: 0 parses; append: {} messages/{} primary suffix bytes instead of {} messages/{} bytes", appended.parsed_messages, appended.suffix_bytes, appended.total, body.len());

        // A modified/deleted artifact cannot be blessed by an unchanged source stamp.
        let dest = a.session_path(r.harness, &r.id);
        fs::write(&dest, b"{\"user modification\":true}").unwrap();
        assert_eq!(a.store_ref(adapter.as_ref(), &r).unwrap().parsed_messages, 8196);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_file(&dest).unwrap();
        assert_eq!(a.store_ref(adapter.as_ref(), &r).unwrap().parsed_messages, 8196);
        exact(&a, adapter.as_ref(), &r);

        // Auxiliary creation and replacement identity force whole reads, even at the same count.
        let outputs = r.path.with_extension("").join("tool-results");
        fs::create_dir_all(&outputs).unwrap();
        fs::write(outputs.join("new.txt"), "sidecar changed").unwrap();
        assert!(!a.store_ref(adapter.as_ref(), &r).unwrap().resumed);
        exact(&a, adapter.as_ref(), &r);
        let replacement = r.path.with_extension("new");
        let old_modified = fs::metadata(&r.path).unwrap().modified().unwrap();
        fs::write(&replacement, &body).unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old_modified))
            .unwrap();
        fs::rename(replacement, &r.path).unwrap();
        assert!(!a.store_ref(adapter.as_ref(), &r).unwrap().resumed);
        exact(&a, adapter.as_ref(), &r);

        let shorter: String = body.lines().take(2).map(|line| format!("{line}\n")).collect();
        fs::write(&r.path, shorter).unwrap();
        let truncated = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(
            (truncated.total, truncated.parsed_messages, truncated.resumed),
            (2, 2, false)
        );
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_archive_replaces_held_eof_suffix_after_usage_arrives() {
        let (root, a, r, mut body) = setup(Harness::Codex);
        body.push_str(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"held at EOF\"}}\n",
        );
        fs::write(&r.path, &body).unwrap();
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        let first = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(first.total, 8193);
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":7,\"output_tokens\":3}}}}\n");
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new prompt\"}}\n");
        fs::write(&r.path, &body).unwrap();
        let append = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert!(append.resumed);
        assert!(append.parsed_messages < 1024);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_catalog_publication_keeps_the_old_stamp_and_retry_repairs_it() {
        let (root, a, r, mut body) = setup(Harness::Claude);
        let adapter = cv_core::harness::for_harness(r.harness).unwrap();
        a.store_ref(adapter.as_ref(), &r).unwrap();
        let state = a.source_path(&r);
        let old_state = fs::read(&state).unwrap();
        let catalog = a.catalog_path();
        let backup = root.join("owned-catalog-backup");
        fs::rename(&catalog, &backup).unwrap();
        fs::create_dir(&catalog).unwrap();
        body.push_str("{\"type\":\"user\",\"uuid\":\"new\",\"sessionId\":\"read\",\"message\":{\"role\":\"user\",\"content\":\"new prompt\"}}\n");
        fs::write(&r.path, body).unwrap();
        assert!(a.store_ref(adapter.as_ref(), &r).is_err());
        assert_eq!(fs::read(&state).unwrap(), old_state);
        assert!(a.stored(&r).is_none(), "old stamp cannot bless new artifact");
        fs::remove_dir(&catalog).unwrap();
        fs::rename(backup, &catalog).unwrap();
        let repaired = a.store_ref(adapter.as_ref(), &r).unwrap();
        assert_eq!(repaired.total, 8193);
        assert!(a.stored(&r).unwrap().read.fresh(&r));
        assert_eq!(a.load_catalog().unwrap()[0].message_count, 8193);
        exact(&a, adapter.as_ref(), &r);
        fs::remove_dir_all(root).unwrap();
    }
}
