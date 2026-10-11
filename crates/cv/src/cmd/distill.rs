//! `cv distill` — make a transcript a reshapeable lane context.
//!
//! `distill` compresses a session (typically a long sub-agent lane) into a context pack plus a
//! verbatim tail (see [`cv_core::distill`]) and emits it three ways:
//!  * the markdown pack on stdout or `--pack FILE` — a fresh agent's first message;
//!  * `--session`: a new resumable Claude session (`claude -p --resume <id> "<next quest>"`);
//!  * `--agent-of <root>`: a new sub-agent transcript inside a root session's `subagents/` dir, which
//!    that root resumes by sending it a message (`SendMessage` to the printed agent id).
//!
//! The same two destinations serve `cv splice … --session | --agent-of` (a verbatim prefix,
//! `<id>:..N`, is the fork of a lane): [`Target`], [`target`] and [`emit_to`] are shared. Both
//! destinations are one `cv_core::emit` of the IR to Claude — the emitter owns resumability
//! (threading, open calls, sidechain identity), the selectors here only choose the messages.
//!
//! Neither command ever writes to the source session. Elided tool outputs go to a sidecar next to
//! the new transcript (`<stem>.flat.jsonl`), which `cv cat <new> <tool_use_id>` reads.

use crate::util::{parse_harness, report, resolve, short_id, usage};
use anyhow::{bail, Context, Result};
use cv_core::distill::{distill, findings, resumable_session, DistillOptions, Distilled};
use cv_core::ir::{Harness, Session, SessionRef};
use cv_core::EmitOptions;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where a reshaped session goes.
pub(crate) enum Target {
    /// Only the pack (stdout / `--pack`).
    PackOnly,
    /// A new main-thread session under the harness store (or `--out`), rehomed to `cwd` if given.
    Session { cwd: Option<PathBuf>, out: Option<PathBuf> },
    /// A new sub-agent transcript of this root session.
    AgentOf { root: String },
}

/// The destination the flags name.
pub(crate) fn target(
    session: bool,
    agent_of: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<Target> {
    match (session, agent_of) {
        (_, Some(root)) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out apply to --session; --agent-of writes next to the root transcript");
            }
            Ok(Target::AgentOf { root })
        }
        (true, None) => Ok(Target::Session { cwd, out }),
        (false, None) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out need --session");
            }
            Ok(Target::PackOnly)
        }
    }
}

/// Resolve a session spec: an existing transcript path (read as Claude unless `--harness` says
/// otherwise), else an id / `harness:id` / agent id through the normal resolver.
pub(crate) fn load(spec: &str, harness: Option<Harness>) -> Result<(Session, PathBuf)> {
    let p = Path::new(spec);
    let (r, adapter) = if p.is_file() {
        let h = harness.unwrap_or(Harness::Claude);
        let a = cv_core::harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?;
        (SessionRef::from_path(p, h), a)
    } else {
        resolve(spec, harness)?
    };
    let mut s = adapter.parse(&r)?;
    s.materialize();
    // A sub-agent transcript read by path: its label lives in the sibling `<stem>.meta.json`
    // (the resolver path reads it through the parent's sub-agent tree instead).
    if p.is_file() {
        if let Some(meta) = std::fs::read_to_string(p.with_extension("meta.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            let bag = s.harness_extra_mut(Harness::Claude);
            for (k, key) in [("description", "agent_description"), ("agentType", "agent_type")] {
                if let Some(v) = meta.get(k).and_then(Value::as_str) {
                    bag.entry(key).or_insert_with(|| json!(v));
                }
            }
        }
    }
    let path = r.path.clone();
    Ok((s, path))
}

fn new_agent_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("a{}", &u[..16])
}

/// What an emit produced.
#[derive(Default, serde::Serialize)]
pub(crate) struct Emitted {
    pub(crate) session_id: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) sidecar_path: Option<PathBuf>,
    pub(crate) resume: Option<String>,
}

fn write_sidecar(transcript: &Path, elided: &[cv_core::distill::Elided]) -> Result<Option<PathBuf>> {
    if elided.is_empty() {
        return Ok(None);
    }
    let stem = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    let p = transcript.with_file_name(format!("{stem}.flat.jsonl"));
    let mut body = String::new();
    for e in elided {
        body.push_str(&cv_core::prune::sidecar_line(
            &e.tool_use_id,
            &e.tool_name,
            &e.input,
            &e.content,
        ));
        body.push('\n');
    }
    std::fs::write(&p, body).with_context(|| format!("writing sidecar {}", p.display()))?;
    Ok(Some(p))
}

/// Emit an IR session as Claude JSONL to `target`, with the elided outputs as its sidecar.
pub(crate) fn emit_to(
    ir: &Session,
    target: &Target,
    elided: &[cv_core::distill::Elided],
    source_path: &Path,
    description: &str,
) -> Result<Emitted> {
    match target {
        Target::PackOnly => Ok(Emitted::default()),
        Target::Session { cwd, out } => {
            let out_dir = match out {
                Some(d) => d.clone(),
                None => cv_core::harness::for_harness(Harness::Claude)
                    .and_then(|a| a.root())
                    .context("Claude Code's store was not found; pass --out <dir>")?,
            };
            let opts = EmitOptions {
                new_cwd: cwd.clone(),
                ..Default::default()
            };
            let res = emit_resumable(ir, &out_dir, &opts)?;
            let sidecar = write_sidecar(&res.path, elided)?;
            let dir = cwd.clone().or_else(|| ir.cwd.clone());
            let resume = format!(
                "{}claude -p --resume {} \"<next instruction>\"",
                dir.map(|d| format!("cd {} && ", d.display())).unwrap_or_default(),
                res.new_id
            );
            Ok(Emitted {
                session_id: Some(res.new_id),
                agent_id: None,
                path: Some(res.path),
                sidecar_path: sidecar,
                resume: Some(resume),
            })
        }
        Target::AgentOf { root } => {
            let root = root_transcript(root)?;
            let sub = root
                .path
                .parent()
                .context("root transcript has no parent dir")?
                .join(&root.id)
                .join("subagents");
            let agent = new_agent_id();
            let dest = sub.join(format!("agent-{agent}.jsonl"));
            if dest.exists() {
                bail!("{} already exists — refusing to overwrite", dest.display());
            }
            // The emitter writes the sub-agent transcript in place: the root's id on every record,
            // the sidechain markers, no title record (`EmitOptions::sidechain_of`).
            let opts = EmitOptions {
                sidechain_of: Some((root.id.clone(), agent.clone())),
                ..Default::default()
            };
            let res = emit_resumable(ir, &sub, &opts)?;
            debug_assert_eq!(res.path, dest);
            // The meta sidecar is optional for resume, but carries the model and the label a
            // harness shows; inherit the source's when it has one.
            let src_meta = source_path.with_extension("meta.json");
            let mut meta: Value = std::fs::read_to_string(&src_meta)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({"agentType": "general-purpose"}));
            if let Some(o) = meta.as_object_mut() {
                o.insert("description".into(), json!(description));
                o.remove("toolUseId");
            }
            std::fs::write(sub.join(format!("agent-{agent}.meta.json")), meta.to_string())?;
            let sidecar = write_sidecar(&res.path, elided)?;
            Ok(Emitted {
                session_id: Some(root.id),
                agent_id: Some(agent.clone()),
                path: Some(res.path),
                sidecar_path: sidecar,
                resume: Some(format!(
                    "from the root session: SendMessage to=\"{agent}\" with the next instruction"
                )),
            })
        }
    }
}

/// Emit `ir` as a Claude transcript under `out_dir` and verify what was written: the fidelity
/// read-back (anything the IR carried that the file does not comes back as a warning) and the
/// resumability rules ([`cv_core::emit::verify_claude_resumable`], an error — it means a bug in cv).
fn emit_resumable(ir: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<cv_core::EmitResult> {
    let (res, warnings) = cv_core::emit::emit_verified(ir, Harness::Claude, out_dir, opts)?;
    for w in &warnings {
        eprintln!("  ⚠ lossy: {w}");
    }
    Ok(res)
}

/// A root session's transcript, from a path or an id.
fn root_transcript(root: &str) -> Result<SessionRef> {
    let p = Path::new(root);
    if p.is_file() {
        return Ok(SessionRef::from_path(p, Harness::Claude));
    }
    let (r, _) = resolve(root, Some(Harness::Claude))?;
    if r.id.starts_with("agent-") {
        return usage(format!(
            "{root} is a sub-agent; --agent-of takes the ROOT session that should own the new agent"
        ));
    }
    Ok(r)
}

/// The human report (stderr side of [`report`]).
fn report_text(d: &Distilled, e: &Emitted) -> String {
    let mut h = String::new();
    let s = &d.stats;
    let before = s.recorded_context_tokens.unwrap_or(s.source_est_tokens);
    let ratio = if s.distilled_est_tokens > 0 {
        before as f64 / s.distilled_est_tokens as f64
    } else {
        0.0
    };
    let _ = writeln!(
                h,
        "✦ distilled {} · {} msg, {} tool calls · context {}{} → ~{} tokens (pack ~{}, tail ~{} over {} calls) · {:.1}× smaller",
        short_id(&s.source_id),
        s.messages,
        s.tool_calls,
        before,
        if s.recorded_context_tokens.is_some() { " (recorded)" } else { " (est.)" },
        s.distilled_est_tokens,
        s.pack_est_tokens,
        s.tail_est_tokens,
        s.tail_tool_calls,
        ratio,
    );
    let _ = writeln!(
        h,
        "  ↳ kept: brief, {} timeline entries, {} commits, {} errors; elided {} tool outputs",
        s.timeline_entries, s.commits, s.errors, s.elided_outputs
    );
    if let Some(p) = &e.path {
        let _ = writeln!(h, "  ↳ wrote {}", p.display());
    }
    if let Some(p) = &e.sidecar_path {
        let _ = writeln!(h, "  ↳ sidecar {}", p.display());
    }
    if let Some(r) = &e.resume {
        let _ = writeln!(h, "  ↳ resume: {r}");
    }
    h
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_distill(
    spec: &str,
    harness: Option<String>,
    opts: DistillOptions,
    upto: Option<usize>,
    with: &[String],
    pack: Option<PathBuf>,
    target: Target,
    json_out: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (mut s, src_path) = load(spec, want)?;
    if let Some(n) = upto {
        if n == 0 || n > s.messages.len() {
            return usage(format!("--upto {n}: the session has {} messages", s.messages.len()));
        }
        s.messages.truncate(n);
    }
    let mut d = distill(&s, &opts);
    for other in with {
        let (o, _) = load(other, None)?;
        d.head_md.push_str(&findings(&o, &opts));
        d.head_md.push('\n');
    }
    if !with.is_empty() {
        d.stats.pack_est_tokens = (d.head_md.len() as f64 / 3.5).ceil() as u64;
        d.stats.distilled_est_tokens = d.stats.pack_est_tokens + d.stats.tail_est_tokens;
    }
    let description = format!(
        "distilled: {}",
        s.extra
            .get("claude")
            .and_then(|b| b.get("agent_description"))
            .and_then(Value::as_str)
            .unwrap_or(&s.label())
    );
    let emitted = match &target {
        Target::PackOnly => Emitted::default(),
        _ => {
            let ir = resumable_session(&s, &d, &opts);
            emit_to(&ir, &target, &d.elided, &src_path, &description)?
        }
    };
    let mut h = report_text(&d, &emitted);
    if let Some(p) = &pack {
        std::fs::write(p, d.pack()).with_context(|| format!("writing {}", p.display()))?;
        let _ = writeln!(h, "  ↳ pack {}", p.display());
    }
    report(
        json_out,
        &h,
        &json!({"stats": d.stats, "emitted": emitted, "pack_path": pack}),
    )?;
    if !json_out && pack.is_none() && matches!(target, Target::PackOnly) {
        print!("{}", d.pack());
    }
    Ok(())
}
