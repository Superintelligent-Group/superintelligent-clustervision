#!/usr/bin/env python3
"""Small process-isolated whole-operation Rust heap gates, without timing thresholds.

Synthetic inputs only. Output validation runs in this driver after the measured operation.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import sys
from pathlib import Path
import subprocess
import tempfile

MIB = 1 << 20
PAYLOAD = 64 * MIB
CEILING = 4 * MIB
NEW_ID = "22222222-2222-4222-8222-222222222222"


def checked_peak(heap):
    assert isinstance(heap, dict), "heap instrumentation/counters missing"
    peak = heap.get("peak_above_baseline_bytes")
    assert type(peak) is int and peak > 0, "invalid or inactive live Rust heap counter"
    assert peak <= CEILING, f"whole-operation live Rust heap {peak} exceeds {CEILING} bytes"
    for name in ("allocation_requests", "requested_bytes"):
        value = heap.get(name)
        assert type(value) is int and value > 0, f"invalid or inactive {name} counter"
    return peak


def file_digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(64 << 10), b""):
            h.update(chunk)
    return h.hexdigest()


def prune_fixture(path, case):
    def row(role, uuid, content, **extra):
        return dict(type=role, uuid=uuid, sessionId="old", **extra,
                    message=dict(role=role, content=content))
    rows = [row("user", "u", "prompt"), None, row("user", "v", "recent"),
            row("assistant", "z", [dict(type="text", text="recent answer")])]
    field = "thinking" if case == "thinking" else "text"
    block = dict(type=field, **{field: "MARKER"})
    if case == "thinking":
        block["signature"] = "sig"
    rows[1] = row("assistant", "a", [block, dict(type="text", text="conclusion")], parentUuid="u")
    if case == "thinking":
        rows[1]["message"]["content"].append(dict(type="tool_use", id="linked", name="Bash",
            input=dict(command="printf useful", description="linked control")))
        for index, tokens in ((1, 1000), (3, 2000)):
            rows[index]["message"]["usage"] = dict(input_tokens=tokens,
                cache_read_input_tokens=0, cache_creation_input_tokens=0)
        rows.insert(2, row("user", "result", [dict(type="tool_result", tool_use_id="linked",
            content="small result\n" * 512)]))
    write_rows(path, rows, "" if case == "thinking" else "ALPHA beta ")


def write_rows(path, rows, payload_prefix="", row_prefix=b"", row_suffix=b"\n"):
    with path.open("wb") as output:
        for row in rows:
            output.write(row_prefix)
            wire = json.dumps(row).encode()
            if b'"MARKER"' in wire:
                before, after = wire.split(b'MARKER')
                output.write(before)
                output.write(payload_prefix.encode())
                for _ in range(PAYLOAD // (64 << 10)):
                    output.write(b"x" * (64 << 10))
                output.write(after)
            else:
                output.write(wire)
            output.write(row_suffix)


def prune_gate(binary, root, env, cases=("thinking", "ascii-declass")):
    # Frozen native sidecars from PRUNE-TURN/PRUNE-LINKED-INPUT-EVIDENCE.json. Hashes cover the
    # full decoded payload and metadata, without building a second giant owned Value tree.
    expected = {
        "thinking": ("a#think0", 67116376, "68c8fcaf6c42fa65fcd367f4eca3e8334dfb412f08637b2a206a97cfa3ad4822"),
        "ascii-declass": ("a#declass0", 67109028, "e02a472d3c4aa721b5199b991ef3c3aa1b1c73420f1885adaa73d1574a9c0b1f"),
    }
    for case in cases:
        stash_id, sidecar_size, sidecar_hash = expected[case]
        directory = root / case
        directory.mkdir()
        source = directory / "source.jsonl"
        prune_fixture(source, case)
        original = file_digest(source)
        if case == "thinking":
            assert source.stat().st_size == 67117078
            assert original == "aa73756329ab751759a30b8c22fd0113728c9153b27a092634a3523cbb60407b"
        flags = ["thinking", "revive"] if case == "thinking" else ["declassify"]
        child = subprocess.run([str(binary), str(source), *flags], env=env,
                               text=True, capture_output=True)
        assert child.returncode == 0, f"prune probe failed: {child.stderr[-2000:]}"
        report = json.loads(child.stdout)
        peak = checked_peak(report.get("heap"))
        result = report["result"]
        assert result["snipped_payloads"] == (2 if case == "thinking" else 1)
        assert result["tokens_freed"] > PAYLOAD // 8
        assert result["before_bytes"] > PAYLOAD and result["after_bytes"] < 2048
        if case == "thinking":
            assert report["revive_tokens"] == 168 and report["revive_old_tokens"] == 2000
            assert report["usage_rewritten"] == 2
        output_path = Path(result["new_path"])
        if case == "thinking":
            assert output_path.stat().st_size == 1521
            assert file_digest(output_path) == "a22fa06f173a2222433de567d4548e1d62fc8c09133759e8070a8ea3c9d943b5"
        output = [json.loads(line) for line in output_path.read_text().splitlines()]
        assert all(row["sessionId"] == NEW_ID for row in output)
        assert [row["uuid"] for row in output] == (["u", "a", "result", "v", "z"]
                                                 if case == "thinking" else ["u", "a", "v", "z"])
        assert output[0]["message"]["content"] == "prompt"
        assert output[-2]["message"]["content"] == "recent"
        assert output[-1]["message"]["content"] == [dict(type="text", text="recent answer")]
        changed = output[1]["message"]["content"]
        assert len(changed) == (3 if case == "thinking" else 2)
        assert changed[1] == dict(type="text", text="conclusion")
        if case == "thinking":
            assert changed[2] == dict(type="tool_use", id="linked", name="Bash",
                input=dict(command="printf useful", description="linked control"))
            result_block = output[2]["message"]["content"]
            assert len(result_block) == 1 and result_block[0]["type"] == "tool_result"
            assert result_block[0]["tool_use_id"] == "linked"
            assert "id=linked tool=Bash command=printf useful, description=linked control |" in result_block[0]["content"]
        assert f"id={stash_id} " in changed[0]["text"] and "[PRUNED " in changed[0]["text"]
        sidecar = Path(result["sidecar_path"])
        assert sidecar.stat().st_size == sidecar_size and file_digest(sidecar) == sidecar_hash
        assert file_digest(source) == original, "prune modified its source"
        print(json.dumps(dict(case=case, peak_live_rust_bytes=peak,
                              sidecar_bytes=sidecar_size, sidecar_sha256=sidecar_hash)), flush=True)
        shutil.rmtree(directory)




def usage_gate(binary, root, env):
    directory = root / "usage-rewrite"
    directory.mkdir()
    source = directory / "source.jsonl"
    usage = dict(input_tokens=999999999, cache_read_input_tokens=11,
                 cache_creation_input_tokens=22, output_tokens=3,
                 cache_creation=dict(ephemeral_1h_input_tokens=5,
                                     ephemeral_5m_input_tokens=6, other="keep"),
                 iterations=[dict(type="advisor_message", input_tokens=9999),
                             dict(type="message", input_tokens=999999999,
                                  cache_read_input_tokens=11, cache_creation_input_tokens=22,
                                  output_tokens=3)])
    def row(role, uuid, content, **extra):
        return dict(type=role, uuid=uuid, sessionId="old", message=dict(role=role, content=content, **extra))
    rows = [dict(type="system", subtype="compact_boundary", sessionId="old"),
            row("user", "u", "prompt"),
            dict(row("assistant", "a", [dict(type="text", text="small result")], usage=usage), ignored="MARKER"),
            row("user", "v", "recent"),
            row("assistant", "z", [dict(type="text", text="recent answer")])]
    write_rows(source, rows, row_prefix=b"  ", row_suffix=b"  \r\n")
    original = file_digest(source)
    child = subprocess.run([str(binary), str(source), "revive"], env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"usage prune probe failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(report.get("heap"))
    result = report["result"]
    assert result["snipped_payloads"] == 0 and result["sidecar_path"] is None
    assert report["usage_rewritten"] == 1 and report["revive_tokens"] == 12
    assert report["revive_old_tokens"] == 1000000032
    output = Path(result["new_path"])
    assert output.stat().st_size == 67110008
    # Frozen owned operation: checks complete giant body preservation and exact usage rewrite.
    assert file_digest(output) == "835a2a22a52b94ca4e33db8a2b6c4d5ad0af9488abe176ec775d42b46a3c6e0e"
    assert file_digest(source) == original
    print(json.dumps(dict(case="unchanged-giant-usage-rewrite", peak_live_rust_bytes=peak,
                          usage_rewritten=1, output_bytes=output.stat().st_size)), flush=True)
    shutil.rmtree(directory)



def claude_output_digest(path):
    """Hash validated canonical probe JSONL as an array, normalizing only top-level UUIDs.

    The emitter's uuid/parentUuid prefix is bounded; all remaining bytes are hashed unchanged.
    This is an output check for this fixed probe, not a general JSON parser/canonicalizer.
    """
    uuid = rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"
    prefix_pattern = re.compile(rb'^\{"uuid":"(' + uuid + rb')","parentUuid":(null|"' + uuid + rb'"),')
    names = {}
    def name(value):
        if value not in names:
            names[value] = f"generated-{len(names)}".encode()
        return b'"' + names[value] + b'"'
    digest = hashlib.sha256()
    digest.update(b"[")
    rows = 0
    with path.open("rb") as source:
        while prefix := source.readline(160):
            match = prefix_pattern.match(prefix)
            assert match, "unexpected Claude probe record prefix"
            normalized_uuid = name(match[1])
            parent = b"null" if match[2] == b"null" else name(match[2][1:-1])
            if rows:
                digest.update(b",")
            prefix = b'{"uuid":' + normalized_uuid + b',"parentUuid":' + parent + b',' + prefix[match.end():]
            while True:
                if prefix.endswith(b"\n"):
                    digest.update(prefix[:-1])
                    break
                digest.update(prefix)
                prefix = source.readline(64 << 10)
                assert prefix, "unterminated Claude probe record"
            rows += 1
    digest.update(b"]")
    return digest.hexdigest(), rows


def claude_port_gate(binary, root, env):
    directory = root / "claude-port"
    directory.mkdir()
    source = directory / "source.jsonl"
    generator = Path(__file__).parent / "profiling/claude-port-fixture.py"
    subprocess.run([sys.executable, "-I", str(generator), str(source), "--characters", str(PAYLOAD)],
                   env=env, check=True)
    # Frozen fixture/output contract from claude-port-heap-native.json, native64M characters.
    original = file_digest(source)
    assert source.stat().st_size == 89479544
    assert original == "0ce7431d3a56ccfeb3c5a565551557380e9eb04ff02f5de1dc1472210301584d"
    child = subprocess.run([str(binary), "codex", str(source), str(directory / "output"), "native"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified Claude export failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    assert report["messages"] == 4 and report["fidelity"] == dict(deltas=[])
    output = Path(report["path"])
    assert output.stat().st_size == 74566782
    digest, rows = claude_output_digest(output)
    assert digest == "bb1364b6a0e47516873453e41eee4700f8c472f017b79820daf5489b5ae34119"
    assert rows == 4
    assert file_digest(source) == original, "Claude export modified its source"
    print(json.dumps(dict(case="verified-claude-export", peak_live_rust_bytes=peak,
                          messages=4, output_bytes=output.stat().st_size,
                          normalized_sha256=digest)), flush=True)
    shutil.rmtree(directory)


def window_binary(report_path):
    artifacts = [json.loads(line) for line in report_path.read_text().splitlines()]
    candidates = [item["executable"] for item in artifacts
                  if item.get("reason") == "compiler-artifact"
                  and item.get("target", {}).get("name") == "cv"
                  and item.get("profile", {}).get("test") and item.get("executable")]
    assert len(candidates) == 1, "expected exactly one compiled cv test executable"
    return Path(candidates[0]).resolve(strict=True)


def window_gate(binary, root, env):
    spec = importlib.util.spec_from_file_location("window_extra", Path(__file__).parent / "profiling/window-extra.py")
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)
    for harness in ("claude", "cursor"):
        directory = root / harness
        directory.mkdir()
        path = fixtures.generate_fixture(directory, harness, "unselected", 64)
        assert path.stat().st_size > PAYLOAD
        original = file_digest(path)
        child_env = dict(env, CLUSTERVISION_HOME=str(directory / "catalog"),
                         CV_WINDOW_PROBE_SOURCE=str(path), CV_WINDOW_PROBE_HARNESS=harness,
                         CV_WINDOW_PROBE_RANGE="0..1", CV_WINDOW_PROBE_ORACLE="0")
        child = subprocess.run([str(binary), fixtures.TEST, "--exact", "--ignored",
                                "--nocapture", "--test-threads=1"], env=child_env,
                               text=True, capture_output=True)
        assert child.returncode == 0, f"window probe failed: {child.stderr[-2000:]}"
        match = re.search(r"CV_WINDOW_HEAP (\{[^\n]+\})", child.stderr)
        assert match, "whole-command heap counters missing (test hook must actually run)"
        peak = checked_peak(json.loads(match[1]))
        start = child.stdout.index("{\n")
        output, _ = json.JSONDecoder().raw_decode(child.stdout[start:])
        message = dict(id="u", role="user", kind="prompt", origin="human",
                       content=[dict(type="text", text="selected prompt")])
        expected = dict(id="window-extra", harness=harness, messages=[message], source_path=str(path))
        if harness == "claude":
            message["timestamp"] = "2026-01-01T00:00:00Z"
            expected.update(cwd="/tmp/window", title="final title",
                            created_at="2026-01-01T00:00:00Z", updated_at="2026-01-01T00:00:00Z",
                            git=dict(branch="late-main"), extra=dict(claude=dict(custom_title="final title")))
        else:
            message["extra"] = dict(cursor=dict(unified_mode_raw=99))
            expected.update(title="EOF composer title", model="latest-model",
                            created_at="1970-01-01T00:00:01Z", updated_at="1970-01-01T00:00:02Z",
                            extra=dict(cursor=dict(unified_mode="agent")))
        assert output == expected, "selected message or late metadata changed"
        assert file_digest(path) == original, "window command modified its source"
        print(json.dumps(dict(case=f"{harness}-unselected-rich-window",
                              peak_live_rust_bytes=peak, messages=1)), flush=True)
        shutil.rmtree(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prune-probe", type=Path, required=True)
    parser.add_argument("--window-build-report", type=Path, required=True)
    parser.add_argument("--claude-port-probe", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cv-operation-heap-") as directory:
        root = Path(directory)
        (root / "home").mkdir()
        env = dict(os.environ, HOME=str(root / "home"),
                   CLUSTERVISION_HOME=str(root / "catalog"), RAYON_NUM_THREADS="1")
        binary = args.prune_probe.resolve(strict=True)
        prune_gate(binary, root, env)
        usage_gate(binary, root, env)
        window_gate(window_binary(args.window_build_report), root, env)
        claude_port_gate(args.claude_port_probe.resolve(strict=True), root, env)


if __name__ == "__main__":
    main()
