#!/usr/bin/env python3
"""Exact whole cmd_distill allocation/output controls using isolated Rust test processes.

Compile the identical whole_command_heap_control test against baseline and candidate.
Synthetic fixtures only; source preparation, analysis, emission, report and pack writes
are measured together. Test harness startup and fixture/report inspection are excluded.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

TEST = "cmd::distill::tests::whole_command_heap_control"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def normalize(value, replacements):
    if isinstance(value, dict):
        return {k: normalize(v, replacements) for k, v in value.items()}
    if isinstance(value, list):
        return [normalize(v, replacements) for v in value]
    if isinstance(value, str):
        for old, new in replacements:
            value = value.replace(old, new)
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", help="run only named cases")
    args = parser.parse_args()
    binaries = {label: p.resolve(strict=True) for label, p in
                [("before", args.before), ("after", args.after)]}
    root = args.output.resolve()
    root.mkdir(exist_ok=False, parents=True)
    home = root / "home"
    home.mkdir()
    (root / "tmp").mkdir()
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C",
           "HOME": str(home), "CLUSTERVISION_HOME": str(root / "catalog"),
           "TMPDIR": str(root / "tmp"), "RAYON_NUM_THREADS": "1"}
    results = {}
    pattern = 'code λ\n"\\'

    def fixture(kind, mib):
        source = root / f"{kind}-{mib}" / "source.jsonl"
        source.parent.mkdir()
        payload = pattern * (mib * 1024 * 1024 // len(pattern.encode()))
        rows = []
        def row(role, content):
            i = len(rows)
            rows.append({"type": role, "uuid": f"m{i}", "parentUuid": f"m{i-1}" if i else None,
                         "sessionId": "source", "timestamp": "2026-01-01T00:00:00Z",
                         "cwd": "/tmp/project", "message": {"role": role, "content": content}})
        row("user", "Implement /tmp/generated.rs; keep notes in /tmp/notes.md.")
        if kind == "thinking":
            for _ in range(8):
                row("assistant", [{"type": "thinking", "thinking": payload, "signature": "signed"}])
        else:
            row("assistant", [{"type": "thinking", "thinking": "small thought"},
                              {"type": "tool_use", "id": "write", "name": "Write",
                               "input": {"file_path": "/tmp/generated.rs", "content": payload,
                                         "unknown": {"complete": "sidecar"}}}])
            row("user", [{"type": "tool_result", "tool_use_id": "write", "content": "File written"}])
            row("user", "Review /tmp/generated.rs after the earlier Write.")
            row("assistant", [{"type": "tool_use", "id": "note", "name": "Write",
                               "input": {"file_path": "/tmp/notes.md", "content": 'note é\n"\\'}}])
            row("user", [{"type": "tool_result", "tool_use_id": "note", "content": "File written"}])
            row("assistant", [{"type": "tool_use", "id": "failure", "name": "Bash",
                               "input": {"command": "false"}}])
            row("user", [{"type": "tool_result", "tool_use_id": "failure", "content": "error: failed", "is_error": True}])
        row("assistant", "Finished λ.")
        rows.append({"type": "custom-title", "customTitle": "EOF title"})
        source.write_text("".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) + "\n" for r in rows))
        source.with_suffix(".meta.json").write_text('{"description":"path label","agentType":"reviewer"}')
        return source, sha(source.read_bytes()), len(payload.encode())

    fixtures = {(kind, mib): fixture(kind, mib) for kind in ("thinking", "write") for mib in (2, 8)}
    cases = []
    for kind, mib in fixtures:
        for mode, keep in [("pack", 0), ("resume", 0), ("pack", 100), ("resume", 100)]:
            cases.append((f"{kind}-{mib}-{mode}-tail{keep}", kind, mib, mode, keep, None, "json"))
    cases.extend([("thinking-kept", "thinking", 2, "thinking-tail", 100, None, "json")])
    cases.extend([(f"write-2-{mode}", "write", 2, mode, 0, None, "human")
                  for mode in ("stdout", "agent")])
    cases.extend([(f"prefix-{n}", "write", 8, "pack", 0, n, "json") for n in (1, 3, 0, 99)])
    cases.extend([(f"with-options-{mode}", "write", 2, mode, 2, None, "human")
                  for mode in ("pack", "resume")])

    for case, kind, mib, mode, keep, upto, report in cases:
        if args.case and case not in args.case:
            continue
        source, source_sha, payload_bytes = fixtures[kind, mib]
        exact = []
        for label, binary in binaries.items():
            destination = root / case / label
            destination.mkdir(parents=True)
            run_env = dict(env, CV_DISTILL_PROBE_SOURCE=str(source),
                           CV_DISTILL_PROBE_DESTINATION=str(destination), CV_DISTILL_PROBE_MODE=mode,
                           CV_DISTILL_PROBE_KEEP_LAST=str(keep), CV_DISTILL_PROBE_REPORT=report)
            if case.startswith("with-options"):
                run_env.update(CV_DISTILL_PROBE_WITH=str(source), CV_DISTILL_PROBE_TAIL_MAX="3",
                               CV_DISTILL_PROBE_NOTE_MAX="8", CV_DISTILL_PROBE_LEDGER="0")
            if upto is not None:
                run_env["CV_DISTILL_PROBE_UPTO"] = str(upto)
            child = subprocess.run([str(binary), TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"],
                                   env=run_env, cwd=root, capture_output=True, text=True)
            (destination / "stdout").write_text(child.stdout)
            (destination / "stderr").write_text(child.stderr)
            heap = json.loads(re.search(r"CV_DISTILL_HEAP (\{[^\n]+\})", child.stderr)[1])
            decoder = json.JSONDecoder()
            report_json = None
            if report == "json" and child.returncode == 0:
                start = child.stdout.index('{\n')
                report_json, _ = decoder.raw_decode(child.stdout[start:])
            replacements = [(str(destination), "<DEST>")]
            emitted = report_json["emitted"] if report_json else {}
            transcript = None
            if mode == "resume" and child.returncode == 0:
                path = Path(emitted["path"]) if emitted else next(
                    p for p in (destination / "resume").rglob("*.jsonl")
                    if not p.name.endswith(".flat.jsonl"))
                replacements.append((path.stem, "<SESSION>"))
            elif mode == "agent" and child.returncode == 0:
                path = next(p for p in source.parent.glob("source/subagents/agent-*.jsonl")
                            if not p.name.endswith(".flat.jsonl"))
                replacements.append((path.stem.removeprefix("agent-"), "<AGENT>"))
            else:
                path = None
            sidecar_bytes = b""
            agent_meta = None
            if path:
                records = [json.loads(line) for line in path.read_text().splitlines()]
                replacements.extend((record["uuid"], f"<MESSAGE-{i}>")
                                    for i, record in enumerate(records) if "uuid" in record)
                transcript = normalize(records, replacements)
                sidecar = path.with_suffix(".flat.jsonl")
                if sidecar.exists():
                    sidecar_bytes = sidecar.read_bytes()
                if mode == "agent":
                    agent_meta = json.loads(path.with_suffix(".meta.json").read_text())
                    # Remove only the artifacts created by this runner, keeping the source intact.
                    for artifact in path.parent.glob(path.stem + ".*"):
                        artifact.unlink()
            pack_path = destination / "pack.md"
            pack = pack_path.read_bytes() if pack_path.exists() else b""
            human = re.sub(r"CV_DISTILL_HEAP \{[^\n]+\}\n", "", child.stderr)
            human = normalize(human, replacements)
            error = human if child.returncode else None
            control = {"report": normalize(report_json, replacements), "pack_sha256": sha(pack),
                       "pack_bytes": len(pack), "sidecar_sha256": sha(sidecar_bytes),
                       "sidecar_bytes": len(sidecar_bytes), "agent_meta": agent_meta, "resume_sha256": sha(json.dumps(transcript).encode()),
                       "human_report": human if report == "human" else None,
                       "stdout_pack_sha256": sha(child.stdout.split("# Distilled context", 1)[1].split("ok\n", 1)[0].encode()) if mode == "stdout" and child.returncode == 0 else None,
                       "error": error, "success": child.returncode == 0}
            exact.append(control)
            assert sha(source.read_bytes()) == source_sha
            results[f"{case}-{label}"] = {"heap": heap, "output": control,
                                           "fixture_sha256": source_sha, "payload_bytes": payload_bytes}
        assert exact[0] == exact[1], (case, exact)
        print(f"{case}: exact outputs; peak {results[case+'-before']['heap']['peak_above_baseline_bytes']} -> {results[case+'-after']['heap']['peak_above_baseline_bytes']}", flush=True)
    summary = {"baseline_revision": "1377fdc9", "operation": "whole production cmd_distill caller",
               "binary_sha256": {k: sha(v.read_bytes()) for k, v in binaries.items()}, "results": results,
               "limits": ["requested Rust heap excludes mmap, SQLite/C, stacks and allocator overhead",
                          "Thinking fixtures have eight payloads; Write fixtures have one",
                          "test harness/clap startup excluded; no timing claim",
                          "validation and token accounting still scan source bytes",
                          "tail, timeline notes, emitted transcript and complete sidecar input remain demanded ownership"]}
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
