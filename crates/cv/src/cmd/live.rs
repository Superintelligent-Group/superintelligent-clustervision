//! `cv scry` — live activity across harnesses (`tail -f` for sessions).

use crate::util::{home_rel, parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::truncate;
use cv_core::watch::{Filter, Watcher};
use std::time::Duration;

pub(crate) fn cmd_scry(harness: Option<String>, cwd: Option<String>, interval: f64, existing: bool) -> Result<()> {
    let interval = poll_interval(interval)?;
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let mut watcher = Watcher::new(filter, existing);
    eprintln!("✦ scrying for agent activity… (Ctrl-C to stop)");
    watcher.run(interval, |ev| {
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

fn poll_interval(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() {
        return crate::util::usage("--interval must be finite");
    }
    Duration::try_from_secs_f64(seconds.max(0.25))
        .map_err(|_| crate::util::UsageError("--interval exceeds the supported duration".into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scry_interval_rejects_nonfinite_and_overflow_without_panicking() {
        for seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(poll_interval(seconds)
                .unwrap_err()
                .downcast_ref::<crate::util::UsageError>()
                .is_some());
        }
        assert_eq!(poll_interval(0.0).unwrap(), Duration::from_millis(250));
        assert_eq!(poll_interval(-1.0).unwrap(), Duration::from_millis(250));
        assert_eq!(poll_interval(1.5).unwrap(), Duration::from_millis(1500));
    }
}
