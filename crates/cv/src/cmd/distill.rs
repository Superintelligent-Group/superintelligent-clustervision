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
use cv_core::distill::{distill, findings, prepare_distill, DistillOptions, Distilled, PreparedDistillation};
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
    /// A new sub-agent transcript of this root session; `model` overrides its meta's `model`.
    AgentOf { root: String, model: Option<String> },
}

/// The destination the flags name.
pub(crate) fn target(
    session: bool,
    agent_of: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<Target> {
    if model.is_some() && agent_of.is_none() {
        // clap's `requires` yields to `--session`'s conflict with `--agent-of`, so say it here.
        return usage("--model sets the new sub-agent's meta; it needs --agent-of");
    }
    match (session, agent_of) {
        (_, Some(root)) => {
            if cwd.is_some() || out.is_some() {
                return usage("--cwd/--out apply to --session; --agent-of writes next to the root transcript");
            }
            Ok(Target::AgentOf { root, model })
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

pub(crate) fn resolve_source(
    spec: &str,
    harness: Option<Harness>,
) -> Result<(SessionRef, Box<dyn cv_core::harness::Adapter>)> {
    let p = Path::new(spec);
    let resolved = if p.is_file() {
        let h = harness.unwrap_or(Harness::Claude);
        let a = cv_core::harness::for_harness(h).with_context(|| format!("no adapter for {h}"))?;
        (SessionRef::from_path(p, h), a)
    } else {
        resolve(spec, harness)?
    };
    Ok(resolved)
}

/// Resolve a session spec: an existing transcript path (read as Claude unless `--harness` says
/// otherwise), else an id / `harness:id` / agent id through the normal resolver.
pub(crate) fn load(spec: &str, harness: Option<Harness>) -> Result<(Session, PathBuf)> {
    let (r, adapter) = resolve_source(spec, harness)?;
    let p = Path::new(spec);
    let mut s = adapter.parse(&r)?;
    s.materialize()?;
    enrich_path_meta(&mut s, p);
    let path = r.path.clone();
    Ok((s, path))
}

/// Selected prefix with full EOF metadata and original-count validation. Other load callers
/// retain their existing full-session contract.
fn load_upto(spec: &str, harness: Option<Harness>, upto: usize) -> Result<(Session, PathBuf)> {
    let (reference, adapter) = resolve_source(spec, harness)?;
    let (mut session, total) = cv_core::emit::prepare_selected(adapter.as_ref(), &reference, 0, Some(upto))?;
    if upto == 0 || upto > total {
        return usage(format!("--upto {upto}: the session has {total} messages"));
    }
    session.materialize()?;
    enrich_path_meta(&mut session, Path::new(spec));
    Ok((session, reference.path))
}

fn enrich_path_meta(s: &mut Session, p: &Path) {
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
    cv_core::distill::write_sidecar(&p, elided).with_context(|| format!("writing sidecar {}", p.display()))?;
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
        Target::AgentOf { root, model } => {
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
                if let Some(m) = model {
                    o.insert("model".into(), json!(m));
                }
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
/// resumability rules ([`cv_core::harness::claude::verify_claude_resumable`], an error — it means a bug in cv).
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
    let (s, src_path) = match upto {
        Some(n) => load_upto(spec, want, n)?,
        None => load(spec, want)?,
    };
    enum Products {
        Pack(Distilled),
        Resume(PreparedDistillation),
    }
    let mut products = if matches!(target, Target::PackOnly) {
        Products::Pack(distill(&s, &opts)?)
    } else {
        Products::Resume(prepare_distill(&s, &opts)?)
    };
    let d = match &mut products {
        Products::Pack(d) => d,
        Products::Resume(prepared) => &mut prepared.distilled,
    };
    for other in with {
        let (o, _) = load(other, None)?;
        d.head_md.push_str(&findings(&o, &opts)?);
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
    let (d, emitted) = match products {
        Products::Pack(d) => (d, Emitted::default()),
        Products::Resume(prepared) => {
            let (d, ir) = prepared.into_resumable_session(&s);
            let emitted = emit_to(&ir, &target, &d.elided, &src_path, &description)?;
            (d, emitted)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distill_sidecar_empty_roster_leaves_existing_artifact_untouched() -> Result<()> {
        let world = cv_testkit::World::new("distill-sidecar-empty");
        let transcript = world.base.join("session.jsonl");
        let sidecar = world.base.join("session.flat.jsonl");
        std::fs::write(&sidecar, b"existing")?;
        assert!(write_sidecar(&transcript, &[])?.is_none());
        assert_eq!(std::fs::read(sidecar)?, b"existing");
        Ok(())
    }

    #[test]
    fn distill_prefix_inherits_codex_and_opensession_full_parity() -> Result<()> {
        let world = cv_testkit::World::new("distill-prefix-other-harnesses");
        let codex = world.base.join("codex.jsonl");
        let rows = [
            json!({"timestamp":"2025-01-01T00:00:00Z","type":"session_meta","payload":{"id":"codex","cwd":"/tmp/project","source":"cli"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt λ"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"}}),
            json!({"type":"turn_context","payload":{"cwd":"/tmp/final","model":"model","approval_policy":"on-request"}}),
        ];
        std::fs::write(&codex, rows.iter().map(|row| format!("{row}\n")).collect::<String>())?;
        let opensession = world.base.join("wrapped.opensession.json");
        let mut original = Session::new("wrapped", Harness::Codex);
        for text in ["prompt λ", "reply", "tail"] {
            let mut message = cv_core::Message::new(cv_core::Role::User);
            message.content.push(cv_core::Block::Text { text: text.into() });
            original.messages.push(message);
        }
        std::fs::write(
            &opensession,
            serde_json::to_vec(&cv_core::harness::opensession::document(&original))?,
        )?;
        for (path, harness) in [(codex, Harness::Codex), (opensession, Harness::OpenSession)] {
            let spec = path.to_str().unwrap();
            let (full, _) = load(spec, Some(harness))?;
            assert!(!full.messages.is_empty());
            for n in [1, full.messages.len()] {
                let mut expected = full.clone();
                expected.messages.truncate(n);
                let (actual, _) = load_upto(spec, Some(harness), n)?;
                assert_eq!(
                    serde_json::to_value(actual)?,
                    serde_json::to_value(expected)?,
                    "{harness} prefix"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn distill_prefix_matches_full_load_truncate_and_eof_path_meta() -> Result<()> {
        let world = cv_testkit::World::new("distill-prefix");
        let path = world.base.join("agent.jsonl");
        let mut bytes = Vec::new();
        for index in 0..9 {
            serde_json::to_writer(
                &mut bytes,
                &json!({
                    "type":"user", "uuid":format!("u{index}"), "cwd":format!("/tmp/eof-{index}"),
                    "timestamp":format!("2025-01-01T00:00:0{index}Z"), "permissionMode":format!("mode-{index}"),
                    "message":{"role":"user", "content":if index == 0 { "selected λ".to_string() } else { "discarded".repeat(128 * 1024) }}
                }),
            )?;
            bytes.push(b'\n');
        }
        std::fs::write(&path, bytes)?;
        std::fs::write(
            path.with_extension("meta.json"),
            r#"{"description":"path agent","agentType":"reviewer"}"#,
        )?;
        let spec = path.to_str().unwrap();
        let (full, original_path) = load(spec, None)?;
        for n in [1, 3, 9] {
            let mut expected = full.clone();
            expected.messages.truncate(n);
            let (actual, actual_path) = load_upto(spec, None, n)?;
            assert_eq!(actual_path, original_path);
            assert_eq!(serde_json::to_value(&actual)?, serde_json::to_value(&expected)?);
            let opts = DistillOptions::default();
            let old = distill(&expected, &opts)?;
            let new = distill(&actual, &opts)?;
            assert_eq!(new.pack(), old.pack());
            assert_eq!(serde_json::to_value(new.stats)?, serde_json::to_value(old.stats)?);
        }
        for n in [0, 10] {
            let error = load_upto(spec, None, n).unwrap_err();
            assert_eq!(error.to_string(), format!("--upto {n}: the session has 9 messages"));
            assert!(error.downcast_ref::<crate::util::UsageError>().is_some());
        }
        Ok(())
    }
}
