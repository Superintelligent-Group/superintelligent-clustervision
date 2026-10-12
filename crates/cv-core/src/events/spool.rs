//! Bounded event retention between a source pass and atomic catalog publication.
use super::{Event, EventSink};
use crate::stream::{Checkpoint, Flow, MessageSink};
use crate::{Message, Session};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Seek, Write};
use std::path::PathBuf;

/// Small sessions avoid temporary IO. Large sessions retain only one message's extracted rows
/// and a fixed writer buffer, independent of the number of messages or distinct targets.
const MEMORY_BYTES: usize = 256 * 1024;
const IO_BYTES: usize = 64 * 1024;

#[derive(serde::Deserialize)]
struct DiskEvent {
    msg_idx: usize,
    ts: Option<i64>,
    kind: String,
    tool: Option<String>,
    target: Option<String>,
    detail: Option<String>,
    call: bool,
}

fn decode(line: &str) -> Result<Event> {
    let row: DiskEvent = serde_json::from_str(line).context("decoding event spool row")?;
    let kind = match row.kind.as_str() {
        "file_edit" => "file_edit",
        "file_read" => "file_read",
        "command" => "command",
        "tool" => "tool",
        "error" => "error",
        _ => anyhow::bail!("unknown event kind"),
    };
    Ok(Event {
        msg_idx: row.msg_idx,
        ts: row.ts,
        kind,
        tool: row.tool,
        target: row.target,
        detail: row.detail,
        call: row.call,
    })
}

fn cost(event: &Event) -> usize {
    std::mem::size_of::<Event>()
        + [&event.tool, &event.target, &event.detail]
            .into_iter()
            .flatten()
            .map(String::capacity)
            .sum::<usize>()
}

/// A completed ordered event batch. Its temporary file, when needed, is owned by this value and
/// removed on consumption, abandoned passes, channel closure and persistence failures.
#[derive(Default)]
pub struct EventSpool {
    memory: VecDeque<Event>,
    memory_bytes: usize,
    spill: Option<Spill>,
    len: usize,
}

struct Spill {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
}

impl Drop for Spill {
    fn drop(&mut self) {
        // This path was exclusively created by us. Never remove another operation's files.
        self.writer.take();
        let _ = fs::remove_file(&self.path);
    }
}

impl EventSpool {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Logical temporary bytes written, including the pending writer buffer. Zero for a batch
    /// retained in memory. Successful publication reads these bytes once before removing the file.
    pub fn temporary_bytes(&self) -> Result<u64> {
        match &self.spill {
            None => Ok(0),
            Some(spill) => {
                let writer = spill.writer.as_ref().context("event spool already consumed")?;
                Ok(writer.get_ref().metadata()?.len() + writer.buffer().len() as u64)
            }
        }
    }

    fn push(&mut self, event: Event) -> Result<()> {
        if self.spill.is_none() && self.memory_bytes.saturating_add(cost(&event)) > MEMORY_BYTES {
            let path = std::env::temp_dir().join(format!("cv-event-spool-{}.jsonl", uuid::Uuid::new_v4()));
            let mut options = fs::OpenOptions::new();
            options.write(true).read(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&path).context("creating event spool")?;
            self.spill = Some(Spill {
                path,
                writer: Some(BufWriter::with_capacity(IO_BYTES, file)),
            });
            let spill = self.spill.as_mut().unwrap();
            for event in self.memory.drain(..) {
                write_event(spill.writer.as_mut().unwrap(), &event)?;
            }
            self.memory = VecDeque::new();
            self.memory_bytes = 0;
        }
        match &mut self.spill {
            Some(spill) => write_event(spill.writer.as_mut().unwrap(), &event)?,
            None => {
                self.memory_bytes += cost(&event);
                self.memory.push_back(event);
            }
        }
        self.len += 1;
        Ok(())
    }

    pub(super) fn rows(mut self) -> Result<Rows> {
        let reader = if let Some(spill) = &mut self.spill {
            let mut writer = spill.writer.take().unwrap();
            writer.flush().context("flushing event spool")?;
            writer.get_mut().rewind().context("rewinding event spool")?;
            let file = writer
                .into_inner()
                .map_err(|e| e.into_error())
                .context("opening event spool reader")?;
            Some(BufReader::with_capacity(IO_BYTES, file))
        } else {
            None
        };
        Ok(Rows {
            spool: self,
            reader,
            line: String::new(),
            failed: false,
            read_count: 0,
        })
    }
}

fn write_event(writer: &mut BufWriter<File>, event: &Event) -> Result<()> {
    serde_json::to_writer(&mut *writer, event).context("serializing event spool row")?;
    writer.write_all(b"\n").context("writing event spool row")
}

pub(super) struct Rows {
    spool: EventSpool,
    reader: Option<BufReader<File>>,
    line: String,
    failed: bool,
    read_count: usize,
}

impl Drop for Rows {
    fn drop(&mut self) {
        // Close the reader before the spool's ownership guard removes its file (also on Windows).
        self.reader.take();
    }
}

impl Iterator for Rows {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Some(reader) = &mut self.reader else {
            return self.spool.memory.pop_front().map(Ok);
        };
        self.line.clear();
        let result = match reader.read_line(&mut self.line) {
            Ok(0) if self.read_count == self.spool.len => return None,
            Ok(0) => Err(anyhow::anyhow!("truncated event spool")),
            Ok(_) if self.read_count >= self.spool.len => Err(anyhow::anyhow!("extra event spool row")),
            Ok(_) => decode(&self.line),
            Err(e) => Err(e).context("reading event spool row"),
        };
        self.failed = result.is_err();
        self.read_count += usize::from(result.is_ok());
        Some(result)
    }
}

/// Event extraction with bounded retention. Checkpoints have the same events layer as EventSink;
/// the rows are unpublished until the source pass completes successfully.
pub struct SpoolingSink {
    inner: EventSink,
    spool: EventSpool,
    error: Option<anyhow::Error>,
}

impl SpoolingSink {
    pub fn new(cwd: Option<PathBuf>) -> Self {
        Self::with_inner(EventSink::new(cwd))
    }

    pub fn resume(cp: &Checkpoint) -> Option<Self> {
        EventSink::resume(cp).map(Self::with_inner)
    }

    fn with_inner(inner: EventSink) -> Self {
        Self {
            inner,
            spool: EventSpool::default(),
            error: None,
        }
    }

    /// Keep event checkpoints and dependency reads without extracting further event rows.
    pub fn without_events(mut self) -> Self {
        self.inner = self.inner.without_events();
        self
    }

    pub fn inner(&self) -> &EventSink {
        &self.inner
    }

    pub fn into_spool(self) -> Result<EventSpool> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.spool),
        }
    }
}

impl MessageSink for SpoolingSink {
    fn dependency(&mut self, path: &std::path::Path) {
        self.inner.dependency(path);
    }

    fn meta(&mut self, session: &Session) {
        self.inner.meta(session);
    }

    fn message(&mut self, message: Message) -> Flow {
        if self.error.is_some() {
            return Flow::Stop;
        }
        self.inner.message(message);
        for event in self.inner.events.drain(..) {
            if let Err(error) = self.spool.push(event) {
                self.error = Some(error);
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        self.error.is_none() && self.inner.wants_checkpoint(offset)
    }

    fn checkpoint(&mut self, cp: &mut Checkpoint) {
        self.inner.checkpoint(cp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(idx: usize, target: Option<String>) -> Event {
        Event {
            msg_idx: idx,
            ts: Some(idx as i64),
            kind: "file_edit",
            tool: Some("Edit".into()),
            target,
            detail: Some("unicode \u{1f9ed} \"quoted\"\nsecond line".into()),
            call: true,
        }
    }

    fn large_spool(start: usize, len: usize) -> EventSpool {
        let mut spool = EventSpool::default();
        for i in start..start + len {
            spool.push(event(i, Some(format!("/repo/distinct/{i}.rs")))).unwrap();
        }
        spool
    }

    #[test]
    fn spill_roundtrip_retains_bounded_memory_and_removes_owned_file() {
        let small = large_spool(0, 2);
        assert_eq!(small.temporary_bytes().unwrap(), 0);
        let rows: Vec<_> = small.rows().unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(
            rows,
            (0..2)
                .map(|i| event(i, Some(format!("/repo/distinct/{i}.rs"))))
                .collect::<Vec<_>>()
        );
        for count in [20_000, 200_000] {
            let spool = large_spool(0, count);
            assert_eq!(spool.len(), count);
            assert_eq!(spool.memory.capacity(), 0);
            assert_eq!(spool.memory_bytes, 0);
            let spill = spool.spill.as_ref().unwrap();
            let path = spill.path.clone();
            assert_eq!(spill.writer.as_ref().unwrap().capacity(), IO_BYTES);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            }
            let bytes = spool.temporary_bytes().unwrap();
            let mut rows = spool.rows().unwrap();
            assert_eq!(fs::metadata(&path).unwrap().len(), bytes);
            for i in 0..count {
                assert_eq!(
                    rows.next().unwrap().unwrap(),
                    event(i, Some(format!("/repo/distinct/{i}.rs")))
                );
            }
            assert!(rows.next().is_none());
            drop(rows);
            assert!(!path.exists());
        }
    }

    #[test]
    fn abandoned_and_corrupt_batches_remove_their_files() {
        let spool = large_spool(0, 3000);
        let path = spool.spill.as_ref().unwrap().path.clone();
        drop(spool);
        assert!(!path.exists());
        let spool = large_spool(0, 3000);
        let path = spool.spill.as_ref().unwrap().path.clone();
        let mut rows = spool.rows().unwrap();
        fs::write(&path, b"{broken\n").unwrap();
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
        drop(rows);
        assert!(!path.exists());
        let spool = large_spool(0, 3000);
        let path = spool.spill.as_ref().unwrap().path.clone();
        let mut rows = spool.rows().unwrap();
        let first = serde_json::to_string(&event(0, Some("/repo/distinct/0.rs".into()))).unwrap();
        fs::write(&path, format!("{first}\n")).unwrap();
        assert!(rows.next().unwrap().is_ok());
        assert!(
            rows.next().unwrap().is_err(),
            "valid truncated prefix cannot be published fresh"
        );
        drop(rows);
        assert!(!path.exists());
        assert!(decode(r#"{"msg_idx":0,"ts":null,"kind":"unknown","call":true}"#).is_err());
    }

    #[test]
    fn extraction_and_resume_share_the_legacy_event_checkpoint() {
        use crate::{Block, Role};
        let mut legacy = EventSink::new(Some("/repo".into()));
        let mut bounded = SpoolingSink::new(Some("/repo".into()));
        let dependency = std::env::temp_dir().join(format!("cv-absent-dependency-{}", uuid::Uuid::new_v4()));
        legacy.dependency(&dependency);
        bounded.dependency(&dependency);
        assert_eq!(
            serde_json::to_value(legacy.dependencies()).unwrap(),
            serde_json::to_value(bounded.inner().dependencies()).unwrap()
        );
        let mut message = Message::new(Role::Assistant);
        message.content.push(Block::ToolUse {
            id: "edit".into(),
            name: "Edit".into(),
            input: serde_json::json!({"file_path":"a.rs"}),
            namespace: None,
        });
        legacy.message(message.clone());
        bounded.message(message.clone());
        let mut expected = Checkpoint::default();
        let mut actual = Checkpoint::default();
        legacy.checkpoint(&mut expected);
        bounded.checkpoint(&mut actual);
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(&actual).unwrap()
        );
        let mut resumed = SpoolingSink::resume(&actual).unwrap();
        resumed.message(message.clone());
        legacy.message(message);
        let full: Vec<_> = bounded
            .into_spool()
            .unwrap()
            .rows()
            .unwrap()
            .chain(resumed.into_spool().unwrap().rows().unwrap())
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(full, legacy.into_events());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn standalone_ingest_reports_sql_failure_and_keeps_prior_publication() {
        use crate::events::{events_for, ingest_ref, needs_ingest};
        use crate::{Harness, SessionRef};
        let world = cv_testkit::World::in_process("event-spool-ingest-error");
        let record = serde_json::json!({"type":"assistant","uuid":"a1","sessionId":"ingest", "message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/repo/old.rs"}}]}});
        let path = world.write_session("ingest", &[record]);
        let r = SessionRef::new("ingest", Harness::Claude, &path);
        assert_eq!(ingest_ref(&r).unwrap(), 1);
        let before = format!("{:?}", events_for("claude", "ingest", None));
        let content = fs::read_to_string(&path).unwrap().replace("old.rs", "new-longer.rs");
        fs::write(&path, content).unwrap();
        let conn = crate::catalog::open_db().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )
        .unwrap();
        assert!(ingest_ref(&r).is_err());
        assert_eq!(format!("{:?}", events_for("claude", "ingest", None)), before);
        assert!(needs_ingest(&r, crate::events::file_mtime_ns(&r.path)));
        conn.execute_batch("DROP TRIGGER reject_event;").unwrap();
        assert_eq!(ingest_ref(&r).unwrap(), 1);
        assert_eq!(
            events_for("claude", "ingest", None)[0].target.as_deref(),
            Some("/repo/new-longer.rs")
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn ordered_publication_preserves_suffix_dedup_and_rolls_back_failures() {
        use crate::events::{
            events_for, needs_ingest, record_from_with_source, record_spooled_with_source, Provenance,
        };
        use crate::{Harness, SessionRef};
        let world = cv_testkit::World::in_process("event-spool-transaction");
        let path = world.proj.join("events.jsonl");
        fs::write(&path, b"original source").unwrap();
        let r = SessionRef::new("events", Harness::Claude, &path);
        let source = crate::source::Probe::default()
            .snapshot(&r)
            .with_reads(&Default::default());
        // The public slice API accepts arbitrary order and dedups across nonadjacent rows.
        let prefix = event(0, None);
        let original = vec![
            event(2, Some("old".into())),
            prefix.clone(),
            event(2, Some("old".into())),
        ];
        record_from_with_source(&r, 0, &original, &source, &Provenance::default());
        assert_eq!(events_for("claude", "events", None).len(), 2);
        let mut spool = EventSpool::default();
        spool.push(event(2, None)).unwrap();
        let mut duplicate = event(2, Some(String::new()));
        duplicate.detail = Some("later duplicate must lose".into());
        spool.push(duplicate).unwrap();
        spool.push(event(2, Some("new".into()))).unwrap();
        spool.push(event(3, Some("new".into()))).unwrap();
        record_spooled_with_source(&r, 2, spool, &source, &Provenance::default()).unwrap();
        let rows = events_for("claude", "events", None);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].msg_idx, 0);
        assert_eq!(rows[1].target, None);
        let before = format!("{rows:?}");
        assert!(!needs_ingest(&r, source.primary.0));
        fs::write(&path, b"rewritten source, deliberately longer").unwrap();
        let changed = crate::source::Probe::default()
            .snapshot(&r)
            .with_reads(&Default::default());
        // An out-of-order row fails after the DELETE and an INSERT; neither rows nor freshness
        // may commit. A below-resume index fails by the same transactional path.
        for indices in [[4, 3], [1, 5]] {
            let mut spool = EventSpool::default();
            for i in indices {
                spool.push(event(i, Some("invalid".into()))).unwrap();
            }
            assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
            assert_eq!(format!("{:?}", events_for("claude", "events", None)), before);
            assert!(needs_ingest(&r, changed.primary.0));
        }
        // Force a disk decoding failure after a valid row; all earlier INSERTs still roll back.
        let mut spool = large_spool(2, 3000);
        let spill = spool.spill.as_mut().unwrap();
        spill.writer.as_mut().unwrap().flush().unwrap();
        let path = spill.path.clone();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{broken\n")
            .unwrap();
        assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
        assert!(!path.exists());
        assert_eq!(format!("{:?}", events_for("claude", "events", None)), before);
        assert!(needs_ingest(&r, changed.primary.0));
        // Force an SQL INSERT failure, again after the suffix DELETE.
        let conn = crate::catalog::open_db().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )
        .unwrap();
        let spool = large_spool(2, 3000);
        let path = spool.spill.as_ref().unwrap().path.clone();
        assert!(record_spooled_with_source(&r, 2, spool, &changed, &Provenance::default()).is_err());
        assert!(!path.exists());
        assert_eq!(format!("{:?}", events_for("claude", "events", None)), before);
        assert!(needs_ingest(&r, changed.primary.0));
    }
}
