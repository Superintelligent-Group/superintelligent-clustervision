#!/usr/bin/env python3
"""Create deterministic synthetic sessions in a NEW explicit directory, never a corpus version.

Payload size is decoded UTF-8 bytes across all result/reply records. Encoded JSON file size
is reported separately. Generation is deliberately outside the measured probe process.
"""

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", choices=["claude", "codex", "gemini", "continue", "lmstudio"])
    parser.add_argument("out", type=Path)
    parser.add_argument("--payload-bytes", type=int, default=100_000_000)
    parser.add_argument("--records", type=int, default=1)
    parser.add_argument("--escaped", action="store_true")
    args = parser.parse_args()
    if args.payload_bytes < 0 or args.records < 1:
        parser.error("payload bytes must be nonnegative and records positive")
    args.out.mkdir(parents=True, exist_ok=False)
    root = args.out.resolve()
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
                json.dump({"id": f"m{i}", "message": {"role": "assistant", "content": payload(i)}}, f,
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
            for i in range(args.records):
                for kind, fields in [
                    ("function_call", {"name": "shell", "call_id": f"c{i}", "arguments": '{"command":"true"}'}),
                    ("function_call_output", {"call_id": f"c{i}", "output": payload(i)}),
                ]:
                    line(f, {"type": "response_item", "timestamp": stamp, "payload": {"type": kind, **fields}})
        else:
            parent = None
            for i in range(args.records):
                for j, (kind, content) in enumerate([
                    ("user", f"prompt {i}"),
                    ("assistant", [{"type": "tool_use", "id": f"c{i}", "name": "Bash", "input": {"command": "true"}}]),
                    ("user", [{"type": "tool_result", "tool_use_id": f"c{i}", "content": payload(i)}]),
                ]):
                    uuid = f"m{i}-{j}"
                    line(f, {"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": sid,
                             "timestamp": stamp, "message": {"role": kind, "content": content}})
                    parent = uuid
    manifest = {
        "harness": args.harness, "id": sid, "path": str(path),
        "root": str(path.parent if args.harness == "lmstudio" else root),
        "decoded_payload_bytes": args.payload_bytes, "physical_input_bytes": path.stat().st_size,
        "records": args.records, "escaped": args.escaped,
        "expected_messages": (args.records + 1 if args.harness == "lmstudio" else args.records
                              if args.harness == "continue" else args.records * (3 if args.harness == "claude" else 2)),
    }
    (root / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(manifest))


if __name__ == "__main__":
    main()
