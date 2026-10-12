//! Isolate serial catalog publication from parsing, Tantivy and final segment merging.
#[cfg(feature = "sqlite")]
mod probe {
    use anyhow::{bail, Context, Result};
    use cv_core::{Block, Harness, Message, MessageSink, Role, Session, SessionRef};
    use std::path::PathBuf;
    use std::time::Instant;

    pub fn run() -> Result<()> {
        let mut args = std::env::args().skip(1);
        let mode = args.next().context("MODE SESSIONS SOURCE; MODE is legacy|writer")?;
        let sessions: usize = args.next().context("SESSIONS required")?.parse()?;
        let path = std::fs::canonicalize(args.next().context("existing scratch SOURCE required")?)?;
        if args.next().is_some() || !matches!(mode.as_str(), "legacy" | "writer") {
            bail!("MODE SESSIONS SOURCE; MODE is legacy|writer");
        }
        let home = std::fs::canonicalize(
            std::env::var_os("CLUSTERVISION_HOME").context("scratch CLUSTERVISION_HOME required")?,
        )?;
        if let Some(real) = dirs::home_dir().map(|p| p.join(".clustervision")) {
            let real = std::fs::canonicalize(&real).unwrap_or(real);
            if home.starts_with(real) {
                bail!("real catalog is forbidden");
            }
        }
        if home.join("catalog.db").exists() {
            bail!("use a new scratch catalog for each comparison");
        }
        let mut message = Message::new(Role::Assistant);
        for i in 0..16 {
            message.content.push(Block::ToolUse {
                id: format!("t{i}"),
                name: "Edit".into(),
                input: serde_json::json!({"file_path":format!("/repo/file-{i}.rs")}),
                namespace: None,
            });
        }
        let session = Session::new("probe", Harness::Claude);
        let start = Instant::now();
        let mut writer = if mode == "writer" {
            Some(cv_core::catalog::Writer::open().context("catalog unavailable")?)
        } else {
            None
        };
        for i in 0..sessions {
            let mut r = SessionRef::new(format!("probe-{i}"), Harness::Claude, &path);
            r.message_count = 1;
            let source = cv_core::source::Probe::default()
                .snapshot(&r)
                .with_reads(&Default::default());
            let mut sink = cv_core::events::SpoolingSink::new(Some(PathBuf::from("/repo")));
            sink.message(message.clone());
            let spool = sink.into_spool()?;
            let mut counter = cv_core::affordance::Counter::new(&r);
            counter.note(&message);
            let counted = cv_core::stream::Counted {
                turns: 1,
                counts: counter.finish(&session, &[]),
                dependencies: Default::default(),
            };
            match &mut writer {
                Some(writer) => {
                    writer.record_events(&r, 0, spool, &source, &Default::default())?;
                    writer.record_counted(&r, &source, counted);
                }
                None => {
                    cv_core::events::record_spooled_with_source(&r, 0, spool, &source, &Default::default())?;
                    counted.record_with_source(&r, &source);
                }
            }
        }
        drop(writer); // Include the final connection close/checkpoint in the measured stage.
        let wall = start.elapsed().as_secs_f64();
        let conn = rusqlite::Connection::open(home.join("catalog.db"))?;
        let events: usize = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
        let counts: usize = conn.query_row("SELECT count(*) FROM affordance_counts", [], |r| r.get(0))?;
        let baselines: usize = conn.query_row("SELECT count(*) FROM source_dependencies", [], |r| r.get(0))?;
        anyhow::ensure!(
            events == 16 * sessions && counts == sessions && baselines == 2 * sessions,
            "incomplete publication"
        );
        println!(
            "{}",
            serde_json::json!({"mode":mode,"sessions":sessions,"events":events,"counts":counts,"baselines":baselines,"wall_seconds":wall,"catalog_bytes":std::fs::metadata(home.join("catalog.db"))?.len()})
        );
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "sqlite")]
    {
        probe::run()
    }
    #[cfg(not(feature = "sqlite"))]
    {
        anyhow::bail!("catalog_writer_probe requires sqlite")
    }
}
