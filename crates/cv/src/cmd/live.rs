//! `cv scry` — live activity across harnesses (`tail -f` for sessions).

use crate::util::{home_rel, parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::truncate;
use cv_core::watch::{Filter, Watcher};
use std::time::Duration;

pub(crate) fn cmd_scry(harness: Option<String>, cwd: Option<String>, interval: f64, existing: bool) -> Result<()> {
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let mut watcher = Watcher::new(filter, existing);
    eprintln!("✦ scrying for agent activity… (Ctrl-C to stop)");
    watcher.run(Duration::from_secs_f64(interval.max(0.25)), |ev| {
        let r = &ev.reference;
        let tag = match ev.kind {
            cv_core::watch::EventKind::New => "✷ new",
            cv_core::watch::EventKind::Updated => "   +",
        };
        let where_ = r.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into());
        println!(
            "{tag}  {:8} {:8}  {}  ({} msg)",
            r.harness.as_str(),
            short_id(&r.id),
            where_,
            ev.new_messages.len()
        );
        for m in &ev.new_messages {
            if let Some(t) = m.text() {
                println!("      {} {}", cv_core::render::role_label(m.role), truncate(&t, 100));
            }
        }
    });
}
