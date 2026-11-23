//! `#tooth` integration tests for `ADR-067` Step 0:
//! 1. Adapter-enforced project-root allowlist (`--allowed-root` + `CV_ALLOWED_ROOTS` + `CV_REQUIRE_SCOPE=1`).
//! 2. Multi-copy deterministic value-hashed redaction (`--redact` + `CV_REDACT=1`).
//! 3. End-to-end enforcement across CLI (`ls`, `show`, `cat`, `search`, `events`, `touched`,
//!    `doctor`, `pack`) and `cv mcp` (`ls`, `show`, `observe_stream`).

use cv_testkit::{claude, World};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

fn tok(prefix: &str, tail: &str) -> String {
    format!("{prefix}{tail}")
}

fn write_session_at_cwd(w: &World, sid: &str, cwd: Option<&str>, title: &str, user_text: &str, blocks: Value) {
    let mut u = json!({
        "type": "user",
        "sessionId": sid,
        "uuid": format!("{sid}-u1"),
        "timestamp": "2026-06-01T10:00:00Z",
        "message": {"role": "user", "content": user_text},
    });
    let mut a = claude::assistant(&format!("{sid}-a1"), "2026-06-01T10:01:00Z", "claude-test-1", blocks);
    if let Some(c) = cwd {
        u["cwd"] = json!(c);
        a["cwd"] = json!(c);
    }
    w.write_session(sid, &[json!({"type": "ai-title", "aiTitle": title}), u, a]);
}

#[test]
fn allowlist_hides_outside_sessions_and_refuses_direct_lookup_fail_closed() {
    let w = World::new("adr067-scope");

    let allowed = w.base.join("sig/repo");
    let allowed_sub = allowed.join("crates/cv");
    let prefix_evil = w.base.join("sig/repo-evil");
    let personal = w.base.join("personal/notes");
    for d in [&allowed, &allowed_sub, &prefix_evil, &personal] {
        fs::create_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    let symlink_escape = {
        let link = allowed.join("escape-link");
        std::os::unix::fs::symlink(&personal, &link).unwrap();
        link
    };

    let allowed_str = allowed.to_str().unwrap();
    let allowed_sub_str = allowed_sub.to_str().unwrap();
    let prefix_evil_str = prefix_evil.to_str().unwrap();
    let personal_str = personal.to_str().unwrap();
    let dotdot_evil = format!("{allowed_str}/../repo-evil");
    let missing_dotdot_evil = format!("{allowed_str}/nonexistent/../../repo-evil");

    // Allowed SIG sessions.
    write_session_at_cwd(
        &w,
        "sigroot1",
        Some(allowed_str),
        "sig root work",
        "implement axolotl scope check",
        json!([
            {"type": "text", "text": "axolotl root implementation complete"},
            {"type": "tool_use", "id": "tu_sig_1", "name": "Edit",
             "input": {"file_path": format!("{allowed_str}/src/lib.rs"), "old_string": "a", "new_string": "b"}}
        ]),
    );
    write_session_at_cwd(
        &w,
        "sigsub1",
        Some(allowed_sub_str),
        "sig crate work",
        "refine axolotl subcrate",
        json!([{"type": "text", "text": "axolotl subcrate verified"}]),
    );
    write_session_at_cwd(
        &w,
        "sharedprefix_sig",
        Some(allowed_str),
        "shared prefix allowed",
        "allowed prefix session",
        json!([{"type": "text", "text": "allowed_shared_payload"}]),
    );

    // Out-of-allowlist sessions covering prefix collision, `..` traversal, missing-dir `..`
    // traversal, missing `cwd`, personal root, and (on Unix) symlink escape.
    write_session_at_cwd(
        &w,
        "evilprefix1",
        Some(prefix_evil_str),
        "evil prefix collision",
        "axolotl topsecret_canary_prefix_collision",
        json!([
            {"type": "text", "text": "axolotl topsecret_canary_prefix_collision"},
            {"type": "tool_use", "id": "tu_evil_1", "name": "Edit",
             "input": {"file_path": format!("{prefix_evil_str}/evil.rs"), "old_string": "x", "new_string": "y"}}
        ]),
    );
    write_session_at_cwd(
        &w,
        "evildotdot1",
        Some(&dotdot_evil),
        "evil dotdot traversal",
        "topsecret_canary_dotdot",
        json!([{"type": "text", "text": "topsecret_canary_dotdot"}]),
    );
    write_session_at_cwd(
        &w,
        "evilmissing1",
        Some(&missing_dotdot_evil),
        "evil missing dotdot",
        "topsecret_canary_missing_dotdot",
        json!([{"type": "text", "text": "topsecret_canary_missing_dotdot"}]),
    );
    write_session_at_cwd(
        &w,
        "evilnocwd1",
        None,
        "evil missing cwd",
        "topsecret_canary_nocwd",
        json!([{"type": "text", "text": "topsecret_canary_nocwd"}]),
    );
    write_session_at_cwd(
        &w,
        "sharedprefix_out",
        Some(personal_str),
        "shared prefix outside",
        "topsecret_canary_shared_outside",
        json!([{"type": "text", "text": "topsecret_canary_shared_outside"}]),
    );
    #[cfg(unix)]
    write_session_at_cwd(
        &w,
        "evilsymlink1",
        Some(symlink_escape.to_str().unwrap()),
        "evil symlink escape",
        "topsecret_canary_symlink",
        json!([{"type": "text", "text": "topsecret_canary_symlink"}]),
    );

    // Sub-agent under an out-of-scope parent vs under an allowed parent.
    w.write_agent(
        "evilprefix1",
        "evilsub99",
        &json!({"agentType": "general-purpose", "description": "secret agent"}),
        &[json!({
            "type": "assistant",
            "uuid": "es1",
            "timestamp": "2026-06-01T10:02:00Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "topsecret_canary_subagent"}]}
        })],
    );
    w.write_agent(
        "sigroot1",
        "sigsub99",
        &json!({"agentType": "general-purpose", "description": "allowed agent"}),
        &[json!({
            "type": "assistant",
            "uuid": "ss1",
            "timestamp": "2026-06-01T10:02:00Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "allowed_subagent_body"}]}
        })],
    );

    // Pre-populate BOTH the Tantivy FTS index and the SQLite catalog/events store UNSCOPED first,
    // reproducing an existing workstation index before entering a scoped SIG invocation.
    w.cv_ok(&["index", "--subagents"]);
    w.cv_ok(&["events", "evilprefix1"]);
    w.cv_ok(&["events", "sigroot1"]);

    // 1. `cv ls --json` under `--allowed-root` and `CV_ALLOWED_ROOTS` shows ONLY allowed sessions.
    for (args, env) in [
        (vec!["--allowed-root", allowed_str, "ls", "--json"], vec![]),
        (vec!["ls", "--json"], vec![("CV_ALLOWED_ROOTS", allowed_str)]),
    ] {
        let (ok, code, out, err) = w.cv_env(&args, &env);
        assert!(ok, "exit {code}: {err}");
        let rows: Vec<Value> = serde_json::from_str(&out).unwrap();
        let mut ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["sharedprefix_sig", "sigroot1", "sigsub1"], "rows: {out}");
        assert!(!out.contains("topsecret_canary"), "leak in ls: {out}");
    }

    // 2. `cv search` (both indexed FTS and `--scan`) hides all out-of-scope hits and counts.
    for extra in [
        vec!["search", "topsecret_canary", "--json"],
        vec!["search", "--scan", "topsecret_canary", "--json"],
    ] {
        let mut args = vec!["--allowed-root", allowed_str];
        args.extend(extra);
        let (out, _) = w.cv_ok(&args);
        let found: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            found["total"],
            json!(0),
            "expected total=0 for out-of-scope search, got: {out}"
        );
        assert!(
            found["hits"].as_array().unwrap().is_empty(),
            "expected 0 out-of-scope search hits, got: {out}"
        );
    }
    let (ax_out, _) = w.cv_ok(&["--allowed-root", allowed_str, "search", "axolotl", "--json"]);
    let ax_found: Value = serde_json::from_str(&ax_out).unwrap();
    assert_eq!(ax_found["total"], json!(2), "expected total=2 in FTS: {ax_out}");
    assert_eq!(
        ax_found["hits"].as_array().unwrap().len(),
        2,
        "expected both allowed sessions in FTS: {ax_out}"
    );

    // 3. `cv touched` and `cv pack` never surface out-of-scope files or sessions.
    let (touched_evil, _) = w.cv_ok(&["--allowed-root", allowed_str, "touched", "evil.rs", "--json"]);
    let touched_rows: Vec<Value> = serde_json::from_str(&touched_evil).unwrap();
    assert!(touched_rows.is_empty(), "evil.rs leaked in touched: {touched_evil}");

    let (touched_sig, _) = w.cv_ok(&["--allowed-root", allowed_str, "touched", "src/lib.rs", "--json"]);
    let sig_touched_rows: Vec<Value> = serde_json::from_str(&touched_sig).unwrap();
    assert_eq!(sig_touched_rows.len(), 1, "expected sigroot1 in touched: {touched_sig}");

    let (pack_out, _) = w.cv_ok(&["--allowed-root", allowed_str, "pack", "axolotl"]);
    assert!(
        pack_out.contains("axolotl"),
        "pack should include allowed content:\n{pack_out}"
    );
    assert!(
        !pack_out.contains("topsecret_canary"),
        "pack leaked out-of-scope session:\n{pack_out}"
    );
    assert!(
        !pack_out.contains("evil.rs"),
        "pack leaked out-of-scope file:\n{pack_out}"
    );

    // 4. Direct lookup (`show`, `cat`, `events`, `doctor`) on every out-of-scope session fails
    //    closed (exit 2, empty stdout, clear scope refusal on stderr, zero canary leak).
    let mut blocked_ids = vec![
        "evilprefix1",
        "evildotdot1",
        "evilmissing1",
        "evilnocwd1",
        "sharedprefix_out",
        "agent-evilsub99",
    ];
    #[cfg(unix)]
    blocked_ids.push("evilsymlink1");

    for bad_id in blocked_ids {
        let (ok, code, out, err) = w.cv_env(&["show", bad_id], &[("CV_ALLOWED_ROOTS", allowed_str)]);
        assert!(
            !ok && code == 2,
            "show {bad_id} expected exit 2, got {code}\nout:{out}\nerr:{err}"
        );
        assert!(out.is_empty(), "show {bad_id} must emit nothing on stdout, got: {out}");
        assert!(
            err.contains("refused:"),
            "show {bad_id} expected refusal on stderr, got: {err}"
        );
        assert!(
            !err.contains("topsecret_canary"),
            "stderr leaked content for {bad_id}: {err}"
        );
    }

    for cmd in [
        vec!["cat", "evilprefix1", "tu_evil_1", "--input"],
        vec!["events", "evilprefix1"],
        vec!["doctor", "evilprefix1"],
    ] {
        let (ok, code, out, err) = w.cv_env(&cmd, &[("CV_ALLOWED_ROOTS", allowed_str)]);
        assert!(
            !ok && code == 2,
            "{cmd:?} expected exit 2, got {code}\nout:{out}\nerr:{err}"
        );
        assert!(out.is_empty(), "{cmd:?} leaked stdout: {out}");
        assert!(
            err.contains("refused:"),
            "{cmd:?} expected refusal on stderr, got: {err}"
        );
    }

    // 5. Allowed sub-agent and shared-prefix disambiguation succeed cleanly.
    let (sub_out, _) = w.cv_ok(&["--allowed-root", allowed_str, "show", "agent-sigsub99"]);
    assert!(sub_out.contains("allowed_subagent_body"), "{sub_out}");

    let (shared_out, _) = w.cv_ok(&["--allowed-root", allowed_str, "show", "sharedprefix"]);
    assert!(shared_out.contains("allowed_shared_payload"), "{shared_out}");
    assert!(!shared_out.contains("topsecret_canary"), "{shared_out}");
}

#[test]
fn require_scope_fails_closed_when_empty_and_cli_cannot_widen_env_scope() {
    let w = World::new("adr067-require-scope");
    let root_a = w.base.join("sig/a");
    let root_a_sub = root_a.join("nested");
    let root_b = w.base.join("sig/b");
    let outside = w.base.join("personal");
    for d in [&root_a, &root_a_sub, &root_b, &outside] {
        fs::create_dir_all(d).unwrap();
    }

    write_session_at_cwd(
        &w,
        "siga1",
        root_a.to_str(),
        "sig a",
        "pangolin alpha",
        json!([{"type": "text", "text": "pangolin alpha"}]),
    );
    write_session_at_cwd(
        &w,
        "sigasub1",
        root_a_sub.to_str(),
        "sig a nested",
        "pangolin nested",
        json!([{"type": "text", "text": "pangolin nested"}]),
    );
    write_session_at_cwd(
        &w,
        "sigb1",
        root_b.to_str(),
        "sig b",
        "pangolin beta",
        json!([{"type": "text", "text": "pangolin beta"}]),
    );
    write_session_at_cwd(
        &w,
        "pers1",
        outside.to_str(),
        "personal",
        "pangolin outside",
        json!([{"type": "text", "text": "pangolin outside"}]),
    );

    // 1. CV_REQUIRE_SCOPE=1 with unset or empty/separator-only CV_ALLOWED_ROOTS fails closed.
    for env in [
        vec![("CV_REQUIRE_SCOPE", "1")],
        vec![("CV_REQUIRE_SCOPE", "1"), ("CV_ALLOWED_ROOTS", "")],
        vec![("CV_REQUIRE_SCOPE", "1"), ("CV_ALLOWED_ROOTS", " : ; ")],
    ] {
        for args in [
            &["ls", "--json"][..],
            &["search", "--scan", "pangolin"],
            &["show", "siga1"],
            &["pack", "pangolin"],
        ] {
            let (ok, code, out, err) = w.cv_env(args, &env);
            assert!(
                !ok && code == 2,
                "{args:?} under {env:?} expected exit 2, got {code}\nout:{out}\nerr:{err}"
            );
            assert!(out.is_empty(), "{args:?} must not emit stdout on fail-closed: {out}");
            assert!(
                err.contains("CV_REQUIRE_SCOPE=1"),
                "stderr should explain fail-closed: {err}"
            );
        }
    }

    // 2. Semicolon/colon multi-root list satisfies CV_REQUIRE_SCOPE=1 and admits both SIG roots.
    let combined = format!("{};{}", root_a.display(), root_b.display());
    let (ok, code, out, err) = w.cv_env(
        &["ls", "--json"],
        &[("CV_REQUIRE_SCOPE", "1"), ("CV_ALLOWED_ROOTS", &combined)],
    );
    assert!(ok, "exit {code}: {err}");
    let rows: Vec<Value> = serde_json::from_str(&out).unwrap();
    let mut ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["siga1", "sigasub1", "sigb1"]);

    // 3. `--allowed-root` narrows an active `CV_ALLOWED_ROOTS`, and refuses to widen it.
    let (ok, code, narrowed_out, err) = w.cv_env(
        &["--allowed-root", root_a_sub.to_str().unwrap(), "ls", "--json"],
        &[("CV_ALLOWED_ROOTS", &combined)],
    );
    assert!(ok, "narrowing failed ({code}): {err}");
    let narrowed_rows: Vec<Value> = serde_json::from_str(&narrowed_out).unwrap();
    assert_eq!(narrowed_rows.len(), 1);
    assert_eq!(narrowed_rows[0]["id"], "sigasub1");

    let (ok, code, widen_out, widen_err) = w.cv_env(
        &["--allowed-root", outside.to_str().unwrap(), "ls", "--json"],
        &[("CV_ALLOWED_ROOTS", &combined)],
    );
    assert!(!ok && code == 2, "widening must fail closed with exit 2, got {code}");
    assert!(widen_out.is_empty());
    assert!(widen_err.contains("outside active CV_ALLOWED_ROOTS"), "{widen_err}");
}

#[test]
fn redact_gate_scrubs_multi_copy_secrets_deterministically_across_surfaces() {
    let w = World::new("adr067-redact");
    let allowed = w.base.join("sig/repo");
    fs::create_dir_all(&allowed).unwrap();
    let allowed_str = allowed.to_str().unwrap();

    // Assemble credential fixtures dynamically so static scanners never match test source.
    let pod_tok = "PodSecretValue99887766Unprefixed";
    let ant_tok = tok("sk-ant-", "api03-RedactionGateSecret0123456789");
    let oai_tok = tok("sk-", "projRedactionGateOpenAI0123456789ab");
    let ghp_tok = tok("ghp_", "0123456789abcdefABCDEF0123456789abcd");
    let ghs_tok = tok("ghs_", "fedcba9876543210FEDCBA9876543210dcba");
    let pat_tok = tok("github_pat_", "11ABCDEF0123456789_abcdefghijklmnop");
    let lin_tok = tok("lin_api_", "0123456789abcdefABCDEF01234567");
    let kgm_tok = tok("kgm_", "0123456789abcdefABCD");
    let sbp_tok = tok("sbp_", "0123456789abcdefABCDEF0123456789");
    let aws_tok = tok("AKIA", "IOSFODNN7EXAMPLE");
    let bearer_val = "BearerSecretToken9876543210ABC";
    let jwt_tok = format!(
        "{}.{}.{}",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        "eyJzdWIiOiJzdXAtMzQ0NyIsInNjb3BlIjoic2lnIn0",
        "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    );
    let pem_body = "MIIEpAIBAAKCAQEA99887766554433221100abcdefABCDEF";
    let pem_block = format!(
        "-----BEGIN {} PRIVATE KEY-----\n{pem_body}\n-----END {} PRIVATE KEY-----",
        "OPENSSH", "OPENSSH"
    );

    w.write_session(
        "crossread1",
        &[
            json!({"type": "ai-title", "aiTitle": format!("narwhal audit {ant_tok}")}),
            json!({
                "type": "user",
                "sessionId": "crossread1",
                "uuid": "cr-u1",
                "timestamp": "2026-07-01T10:00:00Z",
                "cwd": allowed_str,
                "message": {
                    "role": "user",
                    "content": format!(
                        "Verify narwhal adapter with SIG_POD_REMOTE_TOKEN={pod_tok} and Authorization: Bearer {bearer_val}"
                    )
                }
            }),
            claude::with(
                claude::assistant(
                    "cr-a1",
                    "2026-07-01T10:01:00Z",
                    "claude-test-1",
                    json!([
                        {"type": "text", "text": format!("Checking narwhal env keys {ant_tok} and {oai_tok}")},
                        {"type": "tool_use", "id": "tu_cross_1", "name": "Bash", "input": {
                            "command": format!(
                                "env GH={ghp_tok} GHS={ghs_tok} PAT={pat_tok} LIN={lin_tok} KGM={kgm_tok} SBP={sbp_tok} AWS={aws_tok} JWT={jwt_tok}"
                            ),
                            "safe_flag": "--narwhal-safe-mode"
                        }}
                    ]),
                ),
                json!({"cwd": allowed_str}),
            ),
            json!({
                "type": "user",
                "sessionId": "crossread1",
                "uuid": "cr-u2",
                "timestamp": "2026-07-01T10:02:00Z",
                "cwd": allowed_str,
                "message": {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "tu_cross_1",
                        "content": format!(
                            "Cross-read /Users/dev/.env:\n{pem_block}\nechoed_pod={pod_tok}\nechoed_gh={ghp_tok}\nsafe_struct: struct NarwhalScope {{ enforced: bool }}"
                        ),
                        "is_error": false
                    }]
                }
            }),
            claude::with(
                claude::assistant(
                    "cr-a2",
                    "2026-07-01T10:03:00Z",
                    "claude-test-1",
                    json!([{
                        "type": "text",
                        "text": format!(
                            "Summary for narwhal ADR-067: rotated {pod_tok} and {ghp_tok}; keep struct NarwhalScope {{ enforced: bool }} intact."
                        )
                    }]),
                ),
                json!({"cwd": allowed_str}),
            ),
        ],
    );

    w.cv_ok(&["index"]);

    let all_secrets = [
        pod_tok,
        ant_tok.as_str(),
        oai_tok.as_str(),
        ghp_tok.as_str(),
        ghs_tok.as_str(),
        pat_tok.as_str(),
        lin_tok.as_str(),
        kgm_tok.as_str(),
        sbp_tok.as_str(),
        aws_tok.as_str(),
        bearer_val,
        jwt_tok.as_str(),
        pem_body,
    ];
    let assert_scrubbed = |label: &str, text: &str| {
        for s in all_secrets {
            assert!(!text.contains(s), "{label} leaked secret {s:?} in:\n{text}");
        }
    };

    // 1. `cv --redact show` (rendered text & `--json`) scrubs every copy with identical `<REDACTED:rule:sha8>`.
    let (show_txt, _) = w.cv_ok(&["--allowed-root", allowed_str, "--redact", "show", "crossread1"]);
    assert_scrubbed("show text", &show_txt);
    assert!(
        show_txt.contains("struct NarwhalScope { enforced: bool }"),
        "{show_txt}"
    );
    assert!(show_txt.contains("<REDACTED:anthropic:"), "{show_txt}");
    assert!(show_txt.contains("<REDACTED:private_key:"), "{show_txt}");

    let (show_json, _) = w.cv_ok(&[
        "--allowed-root",
        allowed_str,
        "--redact",
        "show",
        "crossread1",
        "--json",
    ]);
    assert_scrubbed("show json", &show_json);
    for expected_prefix in [
        "<REDACTED:anthropic:",
        "<REDACTED:openai:",
        "<REDACTED:github:",
        "<REDACTED:linear:",
        "<REDACTED:kagi:",
        "<REDACTED:supabase:",
        "<REDACTED:aws:",
        "<REDACTED:jwt:",
        "<REDACTED:bearer:",
        "<REDACTED:private_key:",
        "<REDACTED:secret:",
    ] {
        assert!(
            show_json.contains(expected_prefix),
            "missing {expected_prefix} in:\n{show_json}"
        );
    }

    // Extract the exact `<REDACTED:secret:xxxxxxxx>` tag from `SIG_POD_REMOTE_TOKEN=` and verify
    // it appears in all 3 messages (prompt assignment, tool_result bare echo, final summary bare echo).
    let marker = "SIG_POD_REMOTE_TOKEN=";
    let idx = show_json.find(marker).expect("SIG_POD_REMOTE_TOKEN= preserved") + marker.len();
    let pod_tag = &show_json[idx..idx + "<REDACTED:secret:00000000>".len()];
    assert!(
        pod_tag.starts_with("<REDACTED:secret:") && pod_tag.ends_with('>'),
        "bad tag: {pod_tag}"
    );
    assert_eq!(
        show_json.matches(pod_tag).count(),
        3,
        "pod_tag {pod_tag} must match across assignment + tool_result + assistant summary:\n{show_json}"
    );

    // 2. Windowed `show --last 1 --json` under `CV_REDACT=1` still scrubs the bare echo in cr-a2
    //    even though `SIG_POD_REMOTE_TOKEN=` appeared outside the window in cr-u1!
    let (ok, code, win_json, err) = w.cv_env(
        &["show", "crossread1", "--last", "1", "--json"],
        &[("CV_ALLOWED_ROOTS", allowed_str), ("CV_REDACT", "1")],
    );
    assert!(ok, "exit {code}: {err}");
    assert_scrubbed("windowed show json", &win_json);
    assert!(
        win_json.contains(pod_tag),
        "windowed show must carry same {pod_tag}:\n{win_json}"
    );

    // 3. `cv --redact cat` scrubs both `--input` (`tool_use`) and output (`tool_result`) with the same tag.
    let (cat_in, _) = w.cv_ok(&[
        "--allowed-root",
        allowed_str,
        "--redact",
        "cat",
        "crossread1",
        "tu_cross_1",
        "--input",
    ]);
    assert_scrubbed("cat --input", &cat_in);
    assert!(cat_in.contains("--narwhal-safe-mode"), "{cat_in}");

    let (cat_out, _) = w.cv_ok(&[
        "--allowed-root",
        allowed_str,
        "--redact",
        "cat",
        "crossread1",
        "tu_cross_1",
    ]);
    assert_scrubbed("cat output", &cat_out);
    assert!(
        cat_out.contains(pod_tag),
        "cat output must use deterministic {pod_tag}:\n{cat_out}"
    );
    assert!(cat_out.contains("struct NarwhalScope { enforced: bool }"), "{cat_out}");

    // 4. `cv --redact pack`, `search`, `events`, and `ls --json` scrub all secrets while preserving prose.
    let (pack_md, _) = w.cv_ok(&["--allowed-root", allowed_str, "--redact", "pack", "narwhal ADR-067"]);
    assert_scrubbed("pack md", &pack_md);
    assert!(pack_md.contains("NarwhalScope"), "{pack_md}");

    let (search_fts, _) = w.cv_ok(&["--allowed-root", allowed_str, "--redact", "search", "narwhal", "--json"]);
    assert_scrubbed("search fts", &search_fts);

    let (search_scan, _) = w.cv_ok(&[
        "--allowed-root",
        allowed_str,
        "--redact",
        "search",
        "--scan",
        "narwhal",
        "--json",
    ]);
    assert_scrubbed("search scan", &search_scan);

    let (events_json, _) = w.cv_ok(&[
        "--allowed-root",
        allowed_str,
        "--redact",
        "events",
        "crossread1",
        "--json",
    ]);
    assert_scrubbed("events json", &events_json);

    let (ls_json, _) = w.cv_ok(&["--allowed-root", allowed_str, "--redact", "ls", "--json", "--enrich"]);
    assert_scrubbed("ls --json --enrich", &ls_json);
}

#[test]
fn mcp_enforces_scope_and_redaction_across_generated_and_streaming_tools() {
    let w = World::new("adr067-mcp");
    let allowed = w.base.join("sig/repo");
    let outside = w.base.join("personal");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&outside).unwrap();

    let ant_tok = tok("sk-ant-", "api03-McpStreamSecretToken01234567");
    write_session_at_cwd(
        &w,
        "mcpsig1",
        allowed.to_str(),
        "mcp allowed",
        "hello from sig",
        json!([{"type": "text", "text": format!("sig stream carries {ant_tok} safely")}]),
    );
    write_session_at_cwd(
        &w,
        "mcppers1",
        outside.to_str(),
        "mcp personal",
        "personal secret diary",
        json!([{"type": "text", "text": "mcp_outside_canary_payload"}]),
    );

    let mut child = w
        .cv_command()
        .args(["--allowed-root", allowed.to_str().unwrap(), "--redact", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cv mcp should spawn");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut call = |req: Value| -> Option<Value> {
        let expects_reply = req.get("id").is_some();
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
        if !expects_reply {
            return None;
        }
        let line = rx.recv_timeout(Duration::from_secs(60)).expect("mcp reply");
        Some(serde_json::from_str(&line).unwrap())
    };

    call(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}));
    call(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // `ls` via MCP sees only `mcpsig1`.
    let ls_resp = call(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "ls", "arguments": {}}
    }))
    .unwrap();
    let ls_text = ls_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        ls_text.contains("mcpsig1") && !ls_text.contains("mcppers1"),
        "{ls_text}"
    );

    // `show` on allowed session succeeds with `<REDACTED:anthropic:...>` and zero raw secret.
    let show_ok = call(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "show", "arguments": {"id": "mcpsig1"}}
    }))
    .unwrap();
    let show_text = show_ok["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!show_text.contains(&ant_tok), "MCP show leaked secret: {show_text}");
    assert!(
        show_text.contains("<REDACTED:anthropic:"),
        "MCP show missing hash tag: {show_text}"
    );

    // `show` on out-of-scope session fails closed (`isError: true`, zero canary leak).
    let show_bad = call(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "show", "arguments": {"id": "mcppers1"}}
    }))
    .unwrap();
    assert_eq!(show_bad["result"]["isError"], json!(true), "{show_bad}");
    let bad_text = show_bad["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!bad_text.contains("mcp_outside_canary_payload"), "{bad_text}");

    // Passing `allowed_root` attempting to widen scope via MCP tool args is refused fail-closed.
    let widen_bad = call(json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "ls", "arguments": {"allowed_root": [Path::new(&outside).to_str().unwrap()]}}
    }))
    .unwrap();
    assert_eq!(widen_bad["result"]["isError"], json!(true), "{widen_bad}");

    // `observe_stream` (in-process MCP streaming tool) enforces both allowlist and redaction:
    // baseline cursor omits out-of-scope `mcppers1`, and subsequent delta poll returns only
    // `mcpsig1`'s appended message with `<REDACTED:anthropic:...>` scrubbed.
    let obs_base = call(json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "observe_stream", "arguments": {}}
    }))
    .unwrap();
    let obs_base_text = obs_base["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !obs_base_text.contains("mcppers1"),
        "baseline cursor leaked out-of-scope id: {obs_base_text}"
    );
    let obs_base_val: Value = serde_json::from_str(obs_base_text).unwrap();
    let cursor = obs_base_val["cursor"].as_str().unwrap().to_string();

    for (sid, cwd, body) in [
        (
            "mcpsig1",
            allowed.to_str().unwrap(),
            format!("delta carries {ant_tok} safely"),
        ),
        (
            "mcppers1",
            outside.to_str().unwrap(),
            "mcp_outside_canary_delta".to_string(),
        ),
    ] {
        let mut f = fs::OpenOptions::new().append(true).open(w.transcript(sid)).unwrap();
        writeln!(
            f,
            "{}",
            claude::with(
                claude::assistant(
                    &format!("{sid}-a2"),
                    "2026-06-01T10:05:00Z",
                    "claude-test-1",
                    json!([{"type": "text", "text": body}]),
                ),
                json!({"cwd": cwd}),
            )
        )
        .unwrap();
    }

    let obs = call(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "observe_stream", "arguments": {"since_cursor": cursor}}
    }))
    .unwrap();
    let obs_text = obs["result"]["content"][0]["text"].as_str().unwrap();
    let obs_val: Value = serde_json::from_str(obs_text).unwrap();
    assert_eq!(obs_val["count"], json!(1), "{obs_text}");
    assert!(obs_text.contains("mcpsig1"), "{obs_text}");
    assert!(
        !obs_text.contains("mcppers1") && !obs_text.contains("mcp_outside_canary"),
        "{obs_text}"
    );
    assert!(!obs_text.contains(&ant_tok), "{obs_text}");
    assert!(obs_text.contains("<REDACTED:anthropic:"), "{obs_text}");

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
