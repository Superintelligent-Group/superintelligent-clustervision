//! `cv compaction` — every compaction boundary in a session: when, why (trigger), the
//! pre-compaction context size, and the summary that seeded the next window.

use crate::util::{fmt_duration, fmt_int, short_id};
use anyhow::Result;
use cv_core::ir::truncate;

/// `cv compaction <id>`: list every compaction boundary in a session — when it happened, why
/// (trigger), the pre-compaction context size, and the summary that seeded the next window. With
/// `--summaries`, print each summary's full text.
pub(crate) fn cmd_compaction(id: &str, harness: Option<String>, summaries: bool, json: bool) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let (r, _adapter) = crate::util::resolve(id, want)?;

    let comps = cv_core::compaction::detect(&r, true)?;

    if json {
        // Attach each boundary's pre-compaction span for machine consumers.
        let rows: Vec<serde_json::Value> = comps
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut v = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    if let Some((s, e)) = cv_core::compaction::pre_compaction_span(&comps, i) {
                        obj.insert("pre_compaction_span".into(), serde_json::json!([s, e]));
                    }
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if comps.is_empty() {
        println!("{} never compacted (0 boundaries)", short_id(&r.id));
        return Ok(());
    }

    println!("# {} compacted {} time(s)\n", short_id(&r.id), comps.len());
    for (i, c) in comps.iter().enumerate() {
        let trig = c.trigger.as_deref().unwrap_or("?");
        let pre = c
            .pre_tokens
            .map(|t| format!("{} tokens", fmt_int(t)))
            .unwrap_or_else(|| "? tokens".into());
        let dur = c
            .duration_ms
            .map(|ms| format!(" · took {}", fmt_duration(ms)))
            .unwrap_or_default();
        let span = cv_core::compaction::pre_compaction_span(&comps, i)
            .map(|(s, e)| format!("  · pre-span msgs {s}-{e}"))
            .unwrap_or_default();
        println!(
            "── #{} · {} · {} (pre) · @msg {}{}{}",
            i + 1,
            trig,
            pre,
            c.boundary_msg_idx,
            dur,
            span,
        );
        match (&c.summary, summaries) {
            (Some(s), true) => println!("\n{s}\n"),
            (Some(s), false) => println!("   summary: {}\n", truncate(s, 200)),
            (None, _) => println!("   (no summary recorded)\n"),
        }
    }
    if !summaries {
        println!(
            "→ `--summaries` for full summary text · `cv show {} --pre-compaction` to read the lost span",
            short_id(&r.id)
        );
    }
    Ok(())
}
