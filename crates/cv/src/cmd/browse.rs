//! `cv ls` / `cv stats` — browsing the discovered corpus.
//!
//! These read [`cv_core::sessions`] (the probed catalog — milliseconds warm, transparently a full
//! discovery when cold/stale); `cv ls --fresh` forces [`cv_core::discover_all`]'s full scan.

use crate::util::{dim_cwd, parse_harness, session_row, short_id};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};
use cv_core::sanitize::sanitize_line;

/// Apply a parsed query to a ref list in place. The two-phase filter itself is
/// [`cv_core::query::filter_refs`] — shared with `cv serve`'s `/api/stats?q=`, so `ls`/`stats` and
/// the daemon all speak exactly the same `-q`; the CLI only supplies the `text:`
/// resolution, which needs the tantivy index cv-core cannot depend on.
fn apply_query(refs: &mut Vec<SessionRef>, query: &Option<cv_core::SessionQuery>) -> Result<()> {
    let Some(q) = query else { return Ok(()) };
    crate::cmd::query::filter_refs(refs, q, &crate::cmd::query::text_sets(q))
}

/// Session rows for `--json` listings: the SAME refs the table would print (the `exists()` guard —
/// a row whose file vanished since the probe is dropped — comes free from the `stat` that yields
/// `size_bytes`), capped at `limit`.
fn json_rows<'a>(refs: impl Iterator<Item = &'a SessionRef>, limit: usize) -> Vec<(&'a SessionRef, serde_json::Value)> {
    refs.filter_map(|r| std::fs::metadata(&r.path).ok().map(|m| (r, m.len())))
        .take(limit)
        .map(|(r, size)| (r, session_row(r, Some(size))))
        .collect()
}

/// What `ls --enrich` reads from a transcript: the session metadata (`git`, `title`) and its user
/// turns, the only messages `synth_title` looks at. A lazy stream that keeps just those holds a
/// few prompts instead of the whole session (assistant turns and tool output are most of it).
fn enrich_head(adapter: &dyn cv_core::Adapter, r: &SessionRef) -> Option<cv_core::Session> {
    struct Users(Vec<cv_core::Message>);
    impl cv_core::MessageSink for Users {
        fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
            if m.role == cv_core::Role::User {
                self.0.push(m);
            }
            cv_core::Flow::Continue
        }
    }
    let mut users = Users(Vec::new());
    let mut session = cv_core::stream::stream_counted(adapter, r, &cv_core::ParseOptions::lazy(), &mut users).ok()?;
    session.messages = users.0;
    Some(session)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_ls(
    harness: Option<String>,
    cwd: Option<String>,
    query: Option<String>,
    limit: usize,
    sort_by: &str,
    asc: bool,    // print the shown window oldest-first (the feed order)
    fresh: bool,  // force a full re-discovery instead of trusting the probed catalog
    json: bool,   // emit the rows as one JSON array of session rows instead of the table
    enrich: bool, // --json only: add transcript-derived git + display_title (one parse per emitted row)
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    let all = if fresh {
        cv_core::discover_all()
    } else {
        cv_core::sessions()
    };
    let discovered = all.len();
    let mut refs: Vec<SessionRef> = all
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r.cwd.as_ref().is_some_and(|p| p.to_string_lossy().contains(c)),
        })
        .collect();
    apply_query(&mut refs, &query)?;

    match sort_by {
        "created" => refs.sort_by_key(|r| std::cmp::Reverse(r.created_at.or(r.updated_at))),
        "messages" => refs.sort_by_key(|r| std::cmp::Reverse(r.message_count)),
        // "updated" (the default; clap's value_parser admits nothing else)
        _ => refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at))),
    }

    let total = refs.len();
    if asc {
        // The SAME newest `limit` rows, read oldest → newest like a feed: select first, then flip,
        // so `--asc` never changes which sessions are shown — only the order they print in.
        refs.truncate(limit);
        refs.reverse();
    }
    if json {
        // Machine-readable listing: the SAME rows the table below would print (same filters, sort,
        // exists() guard, and limit), as one JSON array on stdout — no header/footer, so it pipes
        // cleanly. Transcript-derived text (title) stays raw here — sanitizing is the terminal
        // seam's job (G5); JSON is the machine contract.
        //
        // `--enrich` (git + display_title) is opt-in because it costs one transcript parse per
        // emitted row — O(limit), not O(fleet), but not the catalog-cheap default `ls --json`
        // promises. See the perf note in the flag's help.
        let rows: Vec<serde_json::Value> = json_rows(refs.iter(), limit)
            .into_iter()
            .map(|(r, mut obj)| {
                if enrich {
                    // Same transcript source `cv show --json` reads. A lazy parse leaves giant
                    // content on disk (memory-safe on the multi-MB "single-exchange giants" sesh
                    // warns about) while still carrying the session's git metadata and enough of the
                    // first user turn to synthesize a title.
                    if let Some(session) =
                        cv_core::harness::for_harness(r.harness).and_then(|a| enrich_head(a.as_ref(), r))
                    {
                        let map = obj.as_object_mut().expect("json object");
                        // `git`: the same object `cv show --json` emits (branch/commit/remote,
                        // skip-none), omitted entirely when the transcript records no git context.
                        if let Some(git) = &session.git {
                            if let Ok(g) = serde_json::to_value(git) {
                                map.insert("git".into(), g);
                            }
                        }
                        // `display_title`: `title` with a first-real-user-text fallback. Explicit
                        // null (not absent) when a session has neither, so consumers can tell "no
                        // title anywhere" from "not enriched" (the key is absent without --enrich).
                        map.insert("display_title".into(), serde_json::json!(session.synth_title()));
                    }
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if total < discovered {
        println!("{total} session(s) (of {discovered} discovered; filtered)\n");
    } else {
        println!("{total} session(s)\n");
    }
    // The exists() check guards the catalog read path's one residual lie — a session deleted since
    // the probe last looked — and is bounded by `limit`, not the fleet (it stats only listed rows).
    for r in refs.iter().filter(|r| r.path.exists()).take(limit) {
        // Titles/cwds are transcript-derived (untrusted, replayed forever) — sanitize at the
        // terminal seam (G5), exactly like task/board rows.
        println!(
            "{:8}  {:8}  {}  {:>4} msg  {}",
            r.harness.as_str(),
            short_id(&r.id),
            crate::util::fmt_span(r.created_at, r.updated_at),
            r.message_count,
            r.title
                .as_deref()
                .map(|t| truncate(&sanitize_line(t), 60))
                .unwrap_or_else(|| sanitize_line(&dim_cwd(r.cwd.as_deref())).into_owned()),
        );
    }
    if total > limit {
        println!("\n… {} more (use --limit)", total - limit);
    }
    Ok(())
}

// ---------- stats ----------

pub(crate) fn cmd_stats(query: Option<String>, json: bool, tokens: bool) -> Result<()> {
    let query = crate::cmd::query::build(query)?;
    let mut refs = cv_core::sessions();
    apply_query(&mut refs, &query)?;
    // The aggregation and the `--json` payload both live in `cv_core::stats`, so `cv stats --json`
    // and `cv serve`'s `/api/stats` are the same six numbers computed once — the dashboard used to
    // accumulate its own from whichever sessions the user had happened to open.
    let s = cv_core::stats::CorpusStats::compute(&refs);
    let total = s.sessions;
    let t = tokens.then(|| cv_core::stats::TokenStats::compute(&refs));

    if json {
        let mut v = s.to_json();
        if let Some(t) = &t {
            v["tokens"] = t.to_json();
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    if total == 0 {
        let scope = if query.is_some() {
            " match the query"
        } else {
            " discovered"
        };
        println!("no sessions{scope}.");
        return Ok(());
    }

    println!("✦ clustervision fleet stats\n");
    println!("{total} session(s) · {} message(s)\n", s.messages);

    println!("by harness:");
    for (h, n) in &s.by_harness {
        println!("  {h:12} {n:>5}");
    }

    println!("\ntop cwds:");
    for (c, n) in s.top_cwds.iter().take(cv_core::stats::TOP_CWDS) {
        println!("  {n:>5}  {}", truncate(c, 70));
    }

    println!("\ndate range:");
    println!(
        "  earliest created: {}",
        s.earliest_created
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  latest updated:   {}",
        s.latest_updated
            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "?".into())
    );
    if let Some(t) = &t {
        print_token_stats(t);
    }
    Ok(())
}

/// `1.23B` / `45.6M` / `7.8K` / `12`: token counts span nine orders of magnitude across a fleet.
fn si(n: u64) -> String {
    let f = n as f64;
    match n {
        1_000_000_000.. => format!("{:.2}B", f / 1e9),
        1_000_000.. => format!("{:.1}M", f / 1e6),
        1_000.. => format!("{:.1}K", f / 1e3),
        _ => n.to_string(),
    }
}

fn print_token_stats(t: &cv_core::stats::TokenStats) {
    use cv_core::stats::TokenRow;
    println!(
        "
tokens ({} session(s) + {} sub-agent transcript(s){}{}):",
        t.sessions,
        t.subagents,
        if t.failed > 0 {
            format!(", {} unparseable", t.failed)
        } else {
            String::new()
        },
        if t.duplicates > 0 {
            format!(", {} repeated usage record(s) skipped", t.duplicates)
        } else {
            String::new()
        },
    );
    println!(
        "  {:8} {:30} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "harness", "model", "calls", "input", "c.write", "c.read", "output", "uncached", "total"
    );
    let line = |r: &TokenRow| {
        let cost = r.cost_usd.map(|c| format!("  ${c:.2}")).unwrap_or_default();
        println!(
            "  {:8} {:30} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}{cost}",
            r.harness,
            truncate(&r.model, 30),
            r.calls,
            si(r.input),
            si(r.cache_write),
            si(r.cache_read),
            si(r.output),
            si(r.uncached()),
            si(r.total()),
        )
    };
    for r in &t.rows {
        line(r);
    }
    let by_h = t.by_harness();
    if by_h.len() > 1 {
        println!();
        for r in &by_h {
            line(r);
        }
    }
    println!();
    line(&t.total());
    println!("  (input excludes cache; uncached = input + cache writes + output)");
}
