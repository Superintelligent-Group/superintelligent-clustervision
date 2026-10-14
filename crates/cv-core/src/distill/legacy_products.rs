//! Frozen pre-product-sharing orchestration, retained only as an exact-output/work oracle.
use super::*;

pub(super) fn distill(session: &Session, opts: &DistillOptions) -> anyhow::Result<Distilled> {
    let session = crate::emit::materialized_session(session)?;
    let session = session.as_ref();
    let msgs = &session.messages;
    let retrieve_ref = opts.retrieve_ref.clone().unwrap_or_else(|| session.id.clone());

    let uses = tool_index(msgs);
    let tool_calls = uses.len();

    let tail_start = tail_cut(msgs, opts.keep_last, &uses);

    // Documents named in the brief or in any message to the agent are instructions by reference:
    // when the agent reads one, its latest read is kept (capped) in the timeline.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in msgs {
        let incoming = (m.role == Role::User && m.kind == MessageKind::Prompt)
            || (m.kind == MessageKind::InjectedContext && matches!(attachment_type(m), Some("queued_command")));
        if !incoming {
            continue;
        }
        let t = m.text().unwrap_or_default();
        for c in path_re().captures_iter(&t) {
            referenced.insert(c[1].trim_end_matches(['.', ',', ';', ':', ')', '`', '\'']).to_string());
        }
    }
    // tool_use_id → path, for reads of a referenced document (Read, or a plain `cat`/`sed -n` of it).
    let mut doc_reads: HashMap<&str, String> = HashMap::new();
    for m in &msgs[..tail_start] {
        for b in &m.content {
            if let Block::ToolUse { id, name, input, .. } = b {
                let input = input.resolve()?;
                let path = match name.as_str() {
                    "Read" => input.get("file_path").and_then(Value::as_str).map(String::from),
                    _ => command_of(&input).and_then(|c| {
                        let c = c.trim();
                        let simple = c.starts_with("cat ") || c.starts_with("sed -n");
                        let p = c.split_whitespace().last()?.trim_matches(['\'', '"']);
                        (simple && !c.contains('|') && !c.contains(';') && !c.contains("&&")).then(|| p.to_string())
                    }),
                };
                if let Some(p) = path.filter(|p| referenced.contains(p)) {
                    doc_reads.insert(id.as_str(), p);
                }
            }
        }
    }
    let mut last_read_of: HashMap<String, &str> = HashMap::new();
    for m in &msgs[..tail_start] {
        for b in &m.content {
            if let Block::ToolResult {
                tool_use_id,
                is_error: false,
                ..
            } = b
            {
                if let Some(p) = doc_reads.get(tool_use_id.as_str()) {
                    last_read_of.insert(p.clone(), tool_use_id.as_str());
                }
            }
        }
    }
    let tail_tool_calls = msgs[tail_start..]
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, Block::ToolUse { .. }))
        .count();

    // ── pass: brief, timeline, facts, ledger ──
    let mut brief: Option<(usize, String)> = None;
    let mut timeline: Vec<Entry> = Vec::new();
    let mut facts = Facts::default();
    let mut ledger: Vec<(usize, usize, String, String)> = Vec::new(); // first idx, last idx, label, outcome
    let mut ledger_runs: Vec<usize> = Vec::new(); // repeat count per ledger row
    let mut elided: Vec<Elided> = Vec::new();

    for (i, m) in msgs.iter().enumerate() {
        let in_tail = i >= tail_start;
        match (m.role, m.kind) {
            (Role::User, MessageKind::Prompt) => {
                let text = m.text().unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                if brief.is_none() {
                    brief = Some((i, text));
                } else if !in_tail {
                    let who = if m.origin == Origin::Human {
                        "user"
                    } else {
                        "message to this agent"
                    };
                    timeline.push(Entry {
                        idx: i,
                        when: when(m),
                        heading: who.into(),
                        body: text,
                    });
                }
            }
            (_, MessageKind::CompactionSummary) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "compaction summary (the harness's own digest of what came before)".into(),
                    body: m.text().unwrap_or_default(),
                });
            }
            (_, MessageKind::SubagentReturn) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "sub-agent returned".into(),
                    body: head_tail(
                        &m.text().unwrap_or_default(),
                        opts.note_max,
                        "the full return is in the source transcript",
                    ),
                });
            }
            (Role::System, MessageKind::Error) if !in_tail => {
                timeline.push(Entry {
                    idx: i,
                    when: when(m),
                    heading: "harness error".into(),
                    body: truncate(m.text().unwrap_or_default().trim(), 300),
                });
            }
            (_, MessageKind::InjectedContext) if !in_tail => {
                // A message delivered mid-run (peer agent, coordinator, queued user input).
                if matches!(attachment_type(m), Some("queued_command")) {
                    timeline.push(Entry {
                        idx: i,
                        when: when(m),
                        heading: "message delivered while working".into(),
                        body: strip_reminder(&m.text().unwrap_or_default()),
                    });
                }
            }
            (Role::Assistant, _) => {
                for b in &m.content {
                    match b {
                        Block::Text { text } if !in_tail && !text.trim().is_empty() => {
                            timeline.push(Entry {
                                idx: i,
                                when: when(m),
                                heading: "agent said".into(),
                                body: text.to_string(),
                            });
                        }
                        Block::ToolUse { name, input, .. } => {
                            let input = input.resolve()?;
                            scan_input_facts(name, &input, &mut facts);
                            if !in_tail {
                                if let Some(e) = own_words_from_tool(i, m, name, &input, opts) {
                                    timeline.push(e);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            (Role::Tool, _) | (Role::User, MessageKind::ToolResult) => {
                for b in &m.content {
                    let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        tool_name,
                        ..
                    } = b
                    else {
                        continue;
                    };
                    let (name, input) = uses
                        .get(tool_use_id.as_str())
                        .map(|c| Ok::<_, anyhow::Error>((c.name.to_string(), c.input.resolve()?.into_owned())))
                        .transpose()?
                        .unwrap_or_else(|| (tool_name.clone().unwrap_or_default(), Value::Null));
                    let label = tool_label(&name, &input);
                    let out: &str = content;
                    let outcome = scan_result_facts(i, &name, &input, &label, out, *is_error, &mut facts);
                    if let Some(p) = doc_reads.get(tool_use_id.as_str()) {
                        if !in_tail && last_read_of.get(p) == Some(&tool_use_id.as_str()) {
                            timeline.push(Entry {
                                idx: i,
                                when: when(m),
                                heading: format!("agent read {p} (a document named in its instructions)"),
                                body: format!(
                                    "```\n{}\n```",
                                    head_tail(out, opts.note_max, &format!("cv cat {retrieve_ref} {tool_use_id}"))
                                ),
                            });
                        }
                    }
                    if (name == "Agent" || name == "Task") && !in_tail {
                        timeline.push(Entry {
                            idx: i,
                            when: when(m),
                            heading: format!(
                                "sub-agent returned ({})",
                                truncate(
                                    input.get("description").and_then(Value::as_str).unwrap_or("").trim(),
                                    80
                                )
                            ),
                            body: head_tail(out, opts.note_max, &format!("cv cat {retrieve_ref} {tool_use_id}")),
                        });
                    }
                    let keep_whole = in_tail && out.len() <= opts.tail_result_max;
                    if !keep_whole {
                        note_work(|work| work.elided_copy_bytes += out.len());
                        elided.push(Elided {
                            tool_use_id: tool_use_id.clone(),
                            tool_name: name.clone(),
                            input: input.clone(),
                            content: out.to_string(),
                        });
                    }
                    if !in_tail && opts.ledger {
                        let use_idx = uses.get(tool_use_id.as_str()).map(|c| c.msg).unwrap_or(i);
                        match ledger.last_mut() {
                            Some(last) if last.2 == label => {
                                last.1 = use_idx;
                                last.3 = outcome;
                                *ledger_runs.last_mut().unwrap() += 1;
                            }
                            _ => {
                                ledger.push((use_idx, use_idx, label, outcome));
                                ledger_runs.push(1);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // Paths the agent itself names in prose count toward the path index too.
        if m.role == Role::Assistant {
            for b in &m.content {
                if let Block::Text { text } = b {
                    for c in path_re().captures_iter(text) {
                        let p = c[1].trim_end_matches(['.', ',', ';', ':', ')', '`']);
                        if note_paths(p) {
                            bump(&mut facts.paths, p);
                        }
                    }
                }
            }
        }
    }

    // ── render the head ──
    let label = session_label(session);
    let mut h = String::new();
    let _ = writeln!(h, "# Distilled context — {label}\n");
    let _ = writeln!(
        h,
        "Source: `{}:{}`{} · {} messages · {} tool calls. Distilled by `cv distill` (deterministic \
         extraction, no model). Anything elided is retrievable: `cv cat {retrieve_ref} <tool_use_id>`; \
         the full source stays readable with `cv show {} --range A..B` (bracketed numbers below are \
         its message indices).\n",
        session.harness.as_str(),
        session.id,
        session
            .lineage
            .parent
            .as_deref()
            .map(|p| format!(" (sub-agent of `{p}`)"))
            .unwrap_or_default(),
        msgs.len(),
        tool_calls,
        session.id,
    );
    if let Some((i, text)) = &brief {
        let _ = writeln!(h, "## 1. Brief (verbatim) [{i}]\n\n{}\n", text.trim());
    }
    let _ = writeln!(
        h,
        "## 2. Timeline — everything said to this agent, and everything it said or wrote itself (verbatim)\n"
    );
    if timeline.is_empty() {
        let _ = writeln!(h, "_(nothing before the verbatim tail)_\n");
    }
    for e in &timeline {
        let _ = writeln!(h, "### [{}] {} · {}\n\n{}\n", e.idx, e.when, e.heading, e.body.trim());
    }
    let _ = writeln!(h, "## 3. Facts (mined from tool inputs and outputs)\n");
    render_facts(&mut h, &facts);
    if opts.ledger && !ledger.is_empty() {
        let _ = writeln!(
            h,
            "## 4. Ledger — every tool call before the tail, one line each (`[index] label → outcome`; ×N = repeated)\n"
        );
        for ((a, b, label, outcome), n) in ledger.iter().zip(&ledger_runs) {
            let idx = if a == b {
                format!("[{a}]")
            } else {
                format!("[{a}–{b}]")
            };
            let rep = if *n > 1 { format!(" ×{n}") } else { String::new() };
            let _ = writeln!(h, "- {idx} {label}{rep} → {outcome}");
        }
        h.push('\n');
    }

    let tail_md = render_tail(msgs, tail_start, opts, &retrieve_ref, &session.resolver())?;

    let source_est_tokens: u64 = msgs
        .iter()
        .try_fold(0u64, |sum, message| Ok::<_, anyhow::Error>(sum + message_est(message)?))?;
    let pack_est_tokens = est(&h);
    let tail_est_tokens: u64 = resumable_tail(msgs, tail_start, opts, &retrieve_ref, &session.resolver())?
        .iter()
        .flat_map(|m| &m.content)
        .try_fold(0u64, |sum, block| Ok::<_, anyhow::Error>(sum + block_est(block)?))?;
    let recorded_context_tokens = msgs
        .iter()
        .rev()
        .find_map(|m| m.usage.as_ref().map(|u| u.prompt_tokens()))
        .filter(|t| *t > 0);

    let stats = DistillStats {
        source_id: session.id.clone(),
        messages: msgs.len(),
        tool_calls,
        recorded_context_tokens,
        source_est_tokens,
        pack_est_tokens,
        tail_est_tokens,
        distilled_est_tokens: pack_est_tokens + tail_est_tokens,
        tail_start,
        tail_tool_calls,
        timeline_entries: timeline.len(),
        commits: facts.commits.len(),
        errors: facts.errors.len(),
        elided_outputs: elided.len(),
    };
    Ok(Distilled {
        head_md: h,
        tail_md,
        tail_start,
        elided,
        stats,
        label,
        retrieve_ref,
    })
}

fn render_tail(
    msgs: &[Message],
    tail_start: usize,
    opts: &DistillOptions,
    retrieve_ref: &str,
    resolver: &crate::Resolver,
) -> anyhow::Result<String> {
    let tail = resumable_tail(msgs, tail_start, opts, retrieve_ref, resolver)?;
    if tail.is_empty() {
        return Ok(String::new());
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "## 5. The last {} tool calls, verbatim (from message [{tail_start}] on)\n",
        opts.keep_last
    );
    for m in &tail {
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    let who = if m.role == Role::Assistant {
                        "agent"
                    } else {
                        "message to this agent"
                    };
                    let _ = writeln!(s, "**{who}** ({}):\n\n{}\n", when(m), text.trim());
                }
                Block::ToolUse { id, name, input, .. } => {
                    let input = input.resolve()?;
                    let body = match command_of(&input) {
                        Some(c) => c.to_string(),
                        None => serde_json::to_string_pretty(&*input)?,
                    };
                    let _ = writeln!(
                        s,
                        "▶ **{name}** `{id}` — {}\n```\n{}\n```\n",
                        tool_label(name, &input),
                        body.trim()
                    );
                }
                Block::ToolResult { content, is_error, .. } => {
                    let tag = if *is_error { "◀ error" } else { "◀ result" };
                    let _ = writeln!(s, "{tag}\n```\n{}\n```\n", content.trim());
                }
                _ => {}
            }
        }
    }
    Ok(s)
}

pub(super) fn resumable_session(session: &Session, d: &Distilled, opts: &DistillOptions) -> anyhow::Result<Session> {
    let mut first = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
    first.timestamp = session
        .messages
        .get(d.tail_start)
        .and_then(|m| m.timestamp)
        .or(session.updated_at);
    first.content.push(Block::Text {
        text: format!("{}{}", resume_preamble(d, opts.keep_last), d.head_md).into(),
    });
    let mut tail = resumable_tail(
        &session.messages,
        d.tail_start,
        opts,
        &d.retrieve_ref,
        &session.resolver(),
    )?;
    let mut messages = vec![first];
    messages.append(&mut tail);
    let output = Session {
        id: session.id.clone(),
        harness: session.harness,
        cwd: session.cwd.clone(),
        title: Some(format!("distilled: {}", d.label)),
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: session.model.clone(),
        git: session.git.clone(),
        system_prompt: None,
        lineage: crate::ir::Lineage {
            forked_from: Some(session.id.clone()),
            ..Default::default()
        },
        messages,
        source_path: None,
        extra: serde_json::Map::new(),
    };
    // The selected tail was already materialized; earlier inputs were never demanded.
    Ok(output)
}

pub(super) fn findings(session: &Session, opts: &DistillOptions) -> anyhow::Result<String> {
    let o = DistillOptions {
        keep_last: 0,
        ledger: false,
        ..opts.clone()
    };
    let d = distill(session, &o)?;
    // Keep only the agent's own entries from the timeline and the facts section.
    let mut out = String::new();
    let _ = writeln!(out, "## Findings from another lane — {} (`{}`)\n", d.label, session.id);
    let mut keep = false;
    let mut in_facts = false;
    for block in d.head_md.split("\n### ") {
        if block.starts_with("## 3. Facts") || block.contains("\n## 3. Facts") {
            in_facts = true;
        }
        let head = block.lines().next().unwrap_or("");
        let own = head.contains("· agent said") || head.contains("· agent sent") || head.contains("· agent wrote");
        if own && !in_facts {
            let section = block.split("\n## ").next().unwrap_or(block);
            let _ = writeln!(out, "### {}\n", section.trim());
            keep = true;
        }
    }
    if let Some(f) = d
        .head_md
        .split("## 3. Facts (mined from tool inputs and outputs)\n")
        .nth(1)
    {
        let facts = f.split("\n## ").next().unwrap_or(f);
        let _ = writeln!(out, "### Facts\n\n{}", facts.trim());
        keep = true;
    }
    if !keep {
        let _ = writeln!(out, "_(no findings)_");
    }
    Ok(out)
}
