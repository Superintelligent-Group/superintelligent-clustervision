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

TEST = "cmd::view::tests::whole_command_window_extra_control"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def identity(path):
    stat = path.stat()
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binaries = {name: path.resolve(strict=True) for name, path in
                [("before", args.before), ("after", args.after)]}
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    fixtures = []

    def row(kind, uuid, content, **extra):
        return dict(type=kind, uuid=uuid, sessionId="window-extra",
                    timestamp="2026-01-01T00:00:00Z", cwd="/tmp/window",
                    message=dict(role=kind, content=content), **extra)

    for kind, mib in [("unselected", 16), ("unselected", 64), ("selected", 0), ("malformed", 0)]:
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
        fixtures.append((kind, mib, path))

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
                       "RAYON_NUM_THREADS": "1", "CV_WINDOW_PROBE_SOURCE": str(path),
                       "CV_WINDOW_PROBE_RANGE": "2..3" if kind == "selected" else "0..1",
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
    report = {"before_source": "7e8d2cd8 production + isolated command hook",
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
