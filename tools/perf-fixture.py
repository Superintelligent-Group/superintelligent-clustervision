#!/usr/bin/env python3
"""Create deterministic synthetic sessions in a NEW explicit directory, never a corpus version.

Payload size is decoded UTF-8 bytes across variable result/reply records or unused metadata
sidecars (--shape metadata). Encoded JSON file size
is reported separately. Generation is deliberately outside the measured probe process.
"""

import argparse
import json
from pathlib import Path
from perf_fixtures import FORMATS, generate
from perf_fixture_zed import generate_zed
from perf_fixture_sqlite import FORMATS as SQLITE_FORMATS, generate_sqlite
from perf_fixture_usage import generate_usage
from perf_fixture_search import generate_sweep


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", choices=["claude", "codex", "gemini", "continue", "lmstudio", "zed", *FORMATS, *SQLITE_FORMATS])
    parser.add_argument("out", type=Path)
    parser.add_argument("--payload-bytes", type=int, default=100_000_000)
    parser.add_argument("--records", type=int, default=1)
    parser.add_argument("--escaped", action="store_true")
    parser.add_argument("--entropy", action="store_true", help="Zed only: deterministic low-compressibility payloads")
    parser.add_argument("--shape", choices=["history", "unrelated", "sidecar-unmatched", "sidecar-selected", "tool-arguments", "metadata", "unselected", "prompt-only", "inline", "legacy", "parts", "usage", "sweep-arguments", "encoded-arguments", "selected", "selected-v2", "selected-ide"], default="history")
    args = parser.parse_args()
    if args.payload_bytes < 0 or args.records < 1:
        parser.error("payload bytes must be nonnegative and records positive")
    if args.entropy and args.harness != "zed":
        parser.error("--entropy applies only to Zed compressed fixtures")
    if args.shape == "usage" and (args.escaped or args.records < 1):
        parser.error("usage fixtures use tiny literal fields; --records controls call count")
    if args.shape == "sweep-arguments" and (args.harness != "claude" or args.records != 1):
        parser.error("sweep-arguments is one Claude tool argument; use --payload-bytes for scale")
    args.out.mkdir(parents=True, exist_ok=False)
    root = args.out.resolve()
    if args.shape == "sweep-arguments":
        manifest = generate_sweep(root, args.payload_bytes, args.escaped)
        (root / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(json.dumps(manifest))
        return
    if args.shape == "usage":
        if args.harness not in ("claude", "codex"):
            parser.error("usage fixtures support Claude and Codex")
        manifest = generate_usage(args.harness, root, args.records)
        (root / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(json.dumps(manifest))
        return
    if args.harness in (*FORMATS, *SQLITE_FORMATS, "zed"):
        if args.harness == "zed":
            if args.shape != "history":
                parser.error("Zed supports only --shape history")
            manifest = generate_zed(root, args.payload_bytes, args.records, args.escaped, args.entropy)
        elif args.harness in SQLITE_FORMATS:
            manifest = generate_sqlite(args.harness, root, args.payload_bytes, args.records, args.escaped, args.shape)
        else:
            manifest = generate(args.harness, root, args.payload_bytes, args.records, args.escaped, args.shape)
        (root / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(manifest))
        return
    if args.shape != "history" and not (
        args.shape == "tool-arguments" and args.harness in ("claude", "codex")
        or args.shape == "metadata" and args.harness == "claude"
        or args.shape == "encoded-arguments" and args.harness == "continue"
    ):
        parser.error("unsupported harness/shape combination")
    sid = "00000000-0000-4000-8000-000000000001"
    stamp = "2026-01-01T00:00:00Z"
    if args.harness == "claude":
        path = root / "projects" / "-perf" / f"{sid}.jsonl"
    elif args.harness == "codex":
        path = root / "sessions" / "2026" / "01" / "01" / f"rollout-{stamp.replace(':', '-')}-{sid}.jsonl"
    elif args.harness == "continue":
        path = root / "sessions" / f"{sid}.json"
    elif args.harness == "lmstudio":
        path = root / "conversations" / f"{sid}.conversation.json"
    else:
        path = root / "tmp" / "perf" / "chats" / "session-perf.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    # Both patterns are ASCII; requested decoded byte counts are exact, including short tails.
    pattern = 'line\n"quoted"\t\\end' if args.escaped else "plain payload "

    def payload(i):
        n = args.payload_bytes // args.records + (i < args.payload_bytes % args.records)
        return (pattern * ((n + len(pattern) - 1) // len(pattern)))[:n]

    def line(f, value):
        f.write(json.dumps(value, ensure_ascii=False, separators=(",", ":")))
        f.write("\n")

    with path.open("w", encoding="utf-8", newline="\n") as f:
        if args.harness == "continue":
            f.write('{"title":"perf fixture","workspaceDirectory":"/perf","history":[')
            for i in range(args.records):
                if i:
                    f.write(",")
                if args.shape == "encoded-arguments":
                    argument = json.dumps({"payload": payload(i)}, ensure_ascii=False, separators=(",", ":"))
                    message = {"role": "assistant", "toolCalls": [
                        {"id": str(i), "function": {"name": "read", "arguments": argument}}]}
                    # A model query cannot be settled from earlier turns or the SessionRef.
                    if i == args.records - 1:
                        message["usage"] = {"model": "present"}
                else:
                    message = {"role": "assistant", "content": payload(i)}
                json.dump({"id": f"m{i}", "message": message}, f,
                          ensure_ascii=False, separators=(",", ":"))
            f.write("]}\n")
        elif args.harness == "lmstudio":
            f.write('{"name":"perf fixture","createdAt":1767225600000,"messages":[')
            json.dump({"currentlySelected": 0, "versions": [{"role": "user", "type": "singleStep", "content": "prompt"}]}, f,
                      separators=(",", ":"))
            for i in range(args.records):
                f.write(",")
                text = payload(i)
                split = len(text) // 2
                json.dump({"currentlySelected": 0, "versions": [
                    {"role": "assistant", "type": "singleStep", "content": text[:split]},
                    {"role": "assistant", "type": "singleStep", "content": text[split:]},
                ]}, f, ensure_ascii=False, separators=(",", ":"))
            f.write("]}\n")
        elif args.harness == "gemini":
            header = {"sessionId": sid, "projectHash": "perf", "startTime": stamp, "lastUpdated": stamp}
            f.write(json.dumps(header, separators=(",", ":"))[:-1] + ',"messages":[')
            for i in range(args.records):
                if i:
                    f.write(",")
                for j, (kind, content) in enumerate([("user", f"prompt {i}"), ("gemini", payload(i))]):
                    if j:
                        f.write(",")
                    json.dump({"id": f"m{i}-{j}", "timestamp": stamp, "type": kind, "content": content}, f,
                              ensure_ascii=False, separators=(",", ":"))
            f.write("]}\n")
        elif args.harness == "codex":
            line(f, {"type": "session_meta", "timestamp": stamp, "payload": {"id": sid, "timestamp": stamp}})
            if args.shape == "tool-arguments":
                line(f, {"type": "turn_context", "timestamp": stamp, "payload": {"cwd": "/perf", "model": "test"}})
            for i in range(args.records):
                if args.shape == "tool-arguments":
                    arguments = {"file_path": f"/perf/file-{i}.rs", "content": payload(i)}
                    line(f, {"type": "response_item", "timestamp": stamp, "payload": {
                        "type": "function_call", "name": "Edit", "call_id": f"c{i}",
                        "arguments": json.dumps(arguments, separators=(",", ":"))}})
                    continue
                for kind, fields in [
                    ("function_call", {"name": "shell", "call_id": f"c{i}", "arguments": '{"command":"true"}'}),
                    ("function_call_output", {"call_id": f"c{i}", "output": payload(i)}),
                ]:
                    line(f, {"type": "response_item", "timestamp": stamp, "payload": {"type": kind, **fields}})
        else:
            parent = None
            for i in range(args.records):
                if args.shape == "tool-arguments":
                    uuid = f"m{i}"
                    line(f, {"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": sid,
                             "cwd": "/perf", "timestamp": stamp, "message": {"role": "assistant", "model": "test",
                             "content": [{"type": "tool_use", "id": f"c{i}", "name": "Edit",
                                          "input": {"file_path": f"/perf/file-{i}.rs", "content": payload(i)}}]}})
                    parent = uuid
                    continue
                for j, (kind, content) in enumerate([
                    ("user", f"prompt {i}"),
                    ("assistant", [{"type": "tool_use", "id": f"c{i}", "name": "Bash", "input": {"command": "true"}}]),
                    ("user", [{"type": "tool_result", "tool_use_id": f"c{i}", "content": f"result {i}" if args.shape == "metadata" else payload(i)}]),
                ]):
                    uuid = f"m{i}-{j}"
                    row = {"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": sid,
                           "timestamp": stamp, "message": {"role": kind, "content": content}}
                    if args.shape == "metadata":
                        if kind == "assistant":
                            row["message"].update(id=f"response-{i}", model="test",
                                                  usage={"input_tokens": 100, "output_tokens": 2})
                        if j == 2:
                            text = payload(i)
                            row["toolUseResult"] = {"stdout": text[:len(text) // 3]}
                    line(f, row)
                    parent = uuid
                if args.shape == "metadata":
                    text = payload(i)
                    split, end = len(text) // 3, 2 * len(text) // 3
                    boundary = f"boundary-{i}"
                    line(f, {"type": "system", "uuid": boundary, "parentUuid": parent,
                             "timestamp": stamp, "subtype": "compact_boundary",
                             "compactMetadata": {"trigger": "auto", "preTokens": 42 + i,
                                                 "durationMs": 7, "unusedPayload": text[split:end]}})
                    parent = f"attachment-{i}"
                    line(f, {"type": "attachment", "uuid": parent, "parentUuid": boundary,
                             "timestamp": stamp, "attachment": {"type": "hook_success",
                             "unusedPayload": text[end:]}, "rendered": [{"content": "hook output"}]})
    manifest = {
        "harness": args.harness, "id": sid, "path": str(path),
        "root": str(path.parent if args.harness == "lmstudio" else root),
        "decoded_payload_bytes": args.payload_bytes, "physical_input_bytes": path.stat().st_size,
        "records": args.records, "escaped": args.escaped, "shape": args.shape,
        "expected_messages": (args.records * 5 if args.shape == "metadata" else
                              args.records if args.shape == "tool-arguments" else
                              args.records + 1 if args.harness == "lmstudio" else args.records
                              if args.harness == "continue" else args.records * (3 if args.harness == "claude" else 2)),
    }
    if args.shape == "metadata":
        manifest["decoded_metadata_payload_bytes"] = args.payload_bytes
        manifest["payload_location"] = "metadata"
        manifest["expected_compactions"] = args.records
    (root / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(manifest))


if __name__ == "__main__":
    main()
