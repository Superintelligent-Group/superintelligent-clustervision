#!/usr/bin/env python3
"""Focused SQLite row-scale heap gate using an explicitly supplied probe-heap binary.

Writes new synthetic sources and scratch catalogs only. Holds row payload size near128KiB,
so increasing logical session size measures row/navigation retention rather than giant rows.
Optional baseline checks preserved-order message and metadata hashes in separate processes.
Native RSS/footprint and physical temporary I/O are NOT Rust heap measurements.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
from perf_fixture_sqlite import generate_sqlite

CASES = (("devin", "history"), ("devin", "metadata"), ("devin", "unselected"),
         ("hermes", "history"), ("hermes", "prompt-only"), ("goose", "history"),
         ("opencode", "history"), ("cursor", "history"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--sizes", type=int, nargs="+", default=[8_000_000, 32_000_000])
    args = parser.parse_args()
    if any(size <= 0 for size in args.sizes): parser.error("sizes must be positive")
    args.out.mkdir(parents=True, exist_ok=False)
    root = args.out.resolve()
    scratch = root / "catalog"
    scratch.mkdir()
    temporary = root / "temporary"
    temporary.mkdir()
    env = dict(os.environ, CLUSTERVISION_HOME=str(scratch), TMPDIR=str(temporary))
    results = []
    failed = []
    for harness, shape in CASES:
        for size in args.sizes:
            destination = root / f"{harness}-{shape}-{size}"
            destination.mkdir()
            records = max(1, (size + 127999) // 128000)
            manifest = generate_sqlite(harness, destination, size, records, False, shape)
            (destination / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n")
            common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--mode", "lazy"]
            def run(binary, operation, extra=()):
                result = subprocess.run([str(binary.resolve()), operation, *common, *extra], env=env,
                                        text=True, capture_output=True, check=True)
                return json.loads(result.stdout)
            result = run(args.probe, "discard")
            if not result["build_features"]["probe_heap"]:
                raise ValueError("--probe must be built with --features probe-heap")
            peak = result["requested_rust_heap"]["peak_above_baseline_bytes"]
            # Allows compact Devin forest and Cursor header descriptors to grow by row count.
            # Does not allow payload-size memory growth (old32MB Devin/Hermes fail decisively).
            limit = 2 * 1024 * 1024 + records * 2048
            leftover = list(temporary.iterdir())
            entry = {"fixture": manifest, "probe": result, "heap_limit_bytes": limit,
                     "temporary_files_after": [str(path) for path in leftover]}
            if result["messages"] != manifest["expected_messages"] or peak > limit or leftover:
                failed.append(f"{harness}/{shape}/{size}")
            if args.baseline:
                before = run(args.baseline, "hash", ["--raw-order"])
                after = run(args.probe, "hash", ["--raw-order"])
                entry["baseline_hash"] = before["detail"]
                entry["candidate_hash"] = after["detail"]
                if before["detail"] != after["detail"]:
                    failed.append(f"{harness}/{shape}/{size}: fidelity")
            results.append(entry)
            print(f"{harness}/{shape}: {size} logical payload bytes, {records} rows, {peak} peak requested Rust bytes", flush=True)
    (root / "SQLITE-SCALE-REPORT.json").write_text(json.dumps({"results": results, "failures": failed}, indent=2) + "\n")
    if failed:
        raise SystemExit("SQLite scale gate failed: " + ", ".join(failed))


if __name__ == "__main__":
    main()
