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
import tempfile
from perf_fixture_sqlite import generate_sqlite

CASES = (("devin", "history"), ("devin", "metadata"), ("devin", "unselected"),
         ("hermes", "history"), ("hermes", "prompt-only"), ("goose", "history"),
         ("opencode", "history"), ("cursor", "history"),
         ("cursor", "inline"), ("cursor", "legacy"))


def checked_peak(result):
    if result.get("build_features") != {"mmap": True, "probe_heap": True}:
        raise ValueError("--probe must be a native mmap binary built with --features probe-heap")
    heap = result.get("requested_rust_heap")
    peak = heap.get("peak_above_baseline_bytes") if isinstance(heap, dict) else None
    if type(peak) is not int or peak < 0:
        raise ValueError("valid live Rust heap counters are required")
    return peak


def campaign(args, root):
    scratch = root / "catalog"
    scratch.mkdir()
    temporary = root / "temporary"
    temporary.mkdir()
    env = dict(os.environ, CLUSTERVISION_HOME=str(scratch), TMPDIR=str(temporary))
    results = []
    failed = []
    for harness, shape in CASES:
        if args.format and harness not in args.format:
            continue
        for size in args.sizes:
            destination = root / f"{harness}-{shape}-{size}"
            destination.mkdir()
            records = max(1, (size + 127999) // 128000)
            manifest = generate_sqlite(harness, destination, size, records, args.escaped, shape)
            (destination / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n")
            common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--mode", "lazy"]
            def run(binary, operation, extra=()):
                result = subprocess.run([str(binary.resolve()), operation, *common, *extra], env=env,
                                        text=True, capture_output=True, check=True)
                return json.loads(result.stdout)
            result = run(args.probe, "discard")
            peak = checked_peak(result)
            # Allows compact Devin forest and Cursor header descriptors to grow by row count.
            # Payloads stay ~128KiB/row; 2KiB/row cannot conceal whole-payload retention.
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--out", type=Path, help="retain generated fixtures/report; otherwise all inputs are operation-owned temporary files")
    parser.add_argument("--sizes", type=int, nargs="+", default=[8_000_000, 32_000_000])
    parser.add_argument("--format", action="append", choices=("devin", "hermes", "goose", "opencode", "cursor"))
    parser.add_argument("--escaped", action="store_true", help="use escaped payloads for a focused decoder scale check")
    args = parser.parse_args()
    if len(args.sizes) < 2 or any(size <= 0 for size in args.sizes):
        parser.error("at least two positive sizes are required")
    if args.out:
        args.out.mkdir(parents=True, exist_ok=False)
        campaign(args, args.out.resolve())
    else:
        with tempfile.TemporaryDirectory(prefix="cv-sqlite-budget-") as temporary:
            campaign(args, Path(temporary))


if __name__ == "__main__":
    main()
