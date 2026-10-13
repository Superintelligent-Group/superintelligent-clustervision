#!/usr/bin/env python3
"""Whole cmd_show extra demand: exact owned oracle and requested allocation work.

Use isolated test executables built with probe-heap. No real stores or timing campaign.
The before executable must contain the whole_command_window_extra_control hook.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sqlite3

TEST = "cmd::view::tests::whole_command_window_extra_control"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def identity(path):
    stat = path.stat()
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def generate_fixture(root, harness, kind, mib):
    """Create one owned synthetic fixture, shared by comparison controls and focused CI gates."""
    root = Path(root)
    if harness not in ("claude", "cursor") or kind not in ("unselected", "selected", "malformed") or mib < 0:
        raise ValueError("unsupported window fixture")
    suffix = "jsonl" if harness == "claude" else "vscdb"
    if (root / f"{kind}-{mib}.{suffix}").exists():
        raise FileExistsError("window fixture already exists")

    def row(kind, uuid, content, **extra):
        return dict(type=kind, uuid=uuid, sessionId="window-extra",
                    timestamp="2026-01-01T00:00:00Z", cwd="/tmp/window",
                    message=dict(role=kind, content=content), **extra)

    if harness == "claude":
        path = root / f"{kind}-{mib}.jsonl"
        rows = [row("user", "u", "selected prompt"),
                row("assistant", "a", [dict(type="tool_use", id="call", name="Read",
                                            input=dict(file_path="/tmp/example"))])]
        tail = row("user", "last", "late prompt", gitBranch="late-main")
        with path.open("wb") as output:
            for record in rows:
                output.write((json.dumps(record, separators=(",", ":")) + "\n").encode())
            result = row("user", "r", [dict(type="tool_result", tool_use_id="call", content="tiny result")])
            prefix = json.dumps(result, separators=(",", ":"))[:-1]
            output.write((prefix + ',"toolUseResult":{"filePath":"/tmp/example","stdout":"').encode())
            if mib:
                chunk = b"x\\n" * 65536
                for _ in range(mib * 1024 * 1024 // (2 * 65536)):
                    output.write(chunk)
            elif kind == "malformed":
                output.write(b"bad\\ud800")
            else:
                output.write(b'owned rich output \\n \\" tail')
            output.write(b'"}}\n')
            output.write((json.dumps(tail, separators=(",", ":")) + "\n").encode())
            output.write(b'{"type":"custom-title","customTitle":"final title"}\n')
    else:
        path = root / f"{kind}-{mib}.vscdb"
        composer = {"name":"EOF composer title","createdAt":1,"lastUpdatedAt":2,"unifiedMode":"agent",
                    "modelConfig":{"modelName":"fallback-model"},
                    "fullConversationHeadersOnly":[{"bubbleId":"u"},{"bubbleId":"a"},{"bubbleId":"last"}]}
        user = {"bubbleId":"u","type":1,"text":"selected prompt","unifiedMode":99}
        assistant = {"bubbleId":"a","type":2,"text":"late answer","thinking":{"text":"thought","signature":"sig"},
                     "modelInfo":{"modelName":"latest-model"},"tokenCount":{"inputTokens":10,"outputTokens":8},
                     "unifiedMode":1,"toolFormerData":{"name":"run_terminal_cmd","toolCallId":"call",
                     "rawArgs":'{"command":"true"}',"status":"success","result":"x\n" * (mib*1024*1024//2) if mib else "rich result"}}
        final = {"bubbleId":"last","type":1,"text":"final prompt","unifiedMode":5}
        assistant_wire = json.dumps(assistant)
        if kind == "malformed":
            assistant_wire = assistant_wire[:-1] + ',"providerOptions":{"ignored":"\\ud800"}}'
        with sqlite3.connect(path) as db:
            db.execute("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
            for key, value in [("composerData:window-extra",json.dumps(composer)),
                               ("bubbleId:window-extra:u",json.dumps(user)),
                               ("bubbleId:window-extra:a",assistant_wire),
                               ("bubbleId:window-extra:last",json.dumps(final))]:
                db.execute("INSERT INTO cursorDiskKV VALUES(?,?)",(key,value))
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--harness", choices=["claude", "cursor"], default="claude")
    args = parser.parse_args()
    binaries = {name: path.resolve(strict=True) for name, path in
                [("before", args.before), ("after", args.after)]}
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    fixtures = [(kind, mib, generate_fixture(root, args.harness, kind, mib))
                for kind, mib in [("unselected", 16), ("unselected", 64), ("selected", 0), ("malformed", 0)]]

    results = {}
    for kind, mib, path in fixtures:
        pair = {}
        source_hash = digest(path.read_bytes())
        source_identity = identity(path)
        for label, binary in binaries.items():
            for oracle in [True, False]:
                case = root / f'{kind}-{mib}-{label}-{"oracle" if oracle else "actual"}'
                case.mkdir()
                env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C",
                       "HOME": str(case / "home"), "CLUSTERVISION_HOME": str(case / "catalog"),
                       "RAYON_NUM_THREADS": "1", "CV_WINDOW_PROBE_SOURCE": str(path), "CV_WINDOW_PROBE_HARNESS": args.harness,
                       "CV_WINDOW_PROBE_RANGE": ("1..2" if args.harness == "cursor" else "2..3") if kind == "selected" else "0..1",
                       "CV_WINDOW_PROBE_ORACLE": "1" if oracle else "0"}
                child = subprocess.run([str(binary), TEST, "--exact", "--ignored", "--nocapture",
                                        "--test-threads=1"], env=env, capture_output=True, check=True)
                text = child.stdout.decode()
                start = text.index("{\n")
                _, length = json.JSONDecoder().raw_decode(text[start:])
                output = text[start:start + length] + "\n"
                (case / "output.json").write_text(output)
                item = {"stdout_sha256": digest(output.encode()), "stdout_bytes": len(output.encode()),
                        "messages": len(json.loads(output)["messages"])}
                if not oracle:
                    item["heap"] = json.loads(re.search(rb"CV_WINDOW_HEAP (\{[^\n]+\})", child.stderr)[1])
                pair[f'{label}-{"oracle" if oracle else "actual"}'] = item
        assert len({item["stdout_sha256"] for item in pair.values()}) == 1, pair
        assert digest(path.read_bytes()) == source_hash
        assert identity(path) == source_identity
        pair["source_sha256"] = source_hash
        results[f"{kind}-{mib}"] = pair
    report = {"before_source": "f08377be production + isolated command hook" if args.harness == "cursor" else "7e8d2cd8 production + isolated command hook",
              "harness": args.harness,
              "operation": "actual cmd_show forward JSON window; synthetic catalog setup outside allocation interval",
              "binary_sha256": {name: digest(path.read_bytes()) for name, path in binaries.items()},
              "results": results,
              "limits": ["requested Rust allocations exclude SQLite/C, stacks, mmap and allocator overhead",
                         "test/fixture/catalog setup and full owned oracle are outside measured command interval",
                         "native default-feature diagnostic; no timing/CPU or total-memory claim",
                         "selected-rich fixture is small; demanded large extras and unknown Last tails remain owned",
                         "malformed/unusual producer fallbacks remain full for compatibility"]}
    (root / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
