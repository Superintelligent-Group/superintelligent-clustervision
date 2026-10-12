#!/usr/bin/env python3
"""Process-isolated live Rust heap gates; no timing thresholds or native SQLite heap claims.

Run with a release store_probe built with probe-heap. The second binary must also disable mmap.
Only generated scratch inputs are read, and every subprocess uses an isolated cv catalog.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


MIB = 1 << 20
SIZES = (16 * MIB, 64 * MIB)
STORE_CEILING = 8 * MIB
STORE_GROWTH = 2 * MIB
CHUNK_CEILING = 2 * MIB
CHUNK_GROWTH = MIB // 2
CHUNK_BYTES = 64 << 10


def probe(binary, args, env, mmap):
    result = subprocess.run([str(binary), *map(str, args)], env=env, text=True, capture_output=True)
    if result.returncode:
        raise AssertionError(f"probe failed ({result.returncode}): {result.stderr[-2000:]}")
    try:
        row = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError(f"probe did not return JSON: {result.stdout[-1000:]}") from error
    assert row.get("build_features") == {"mmap": mmap, "probe_heap": True}, "wrong probe features; heap instrumentation is required"
    heap = row.get("requested_rust_heap")
    assert isinstance(heap, dict), "probe-heap counters missing; a clean timing binary cannot pass this gate"
    peak = heap.get("peak_above_baseline_bytes")
    assert type(peak) is int and peak >= 0, "invalid live Rust heap counter"
    return row, peak


def bound(peaks, ceiling, growth, case):
    assert max(peaks) <= ceiling, f"{case}: live heap {peaks} exceeds {ceiling} bytes"
    assert peaks[1] <= peaks[0] + growth, f"{case}: heap grows with source size: {peaks} (allowance {growth})"


def stores(binary, root, env):
    generator = Path(__file__).with_name("perf-fixture.py")
    # Store promises here are for mmap builds. The no-mmap guarantee below applies to the
    # positioned-read chunk resolver, not adapters that must own giant source records.
    for harness, shape in (("gemini", "giant"), ("claude", "fixed"), ("claude", "giant"), ("codex", "fixed")):
        for escaped in (False, True):
            peaks = []
            for size in SIZES:
                # Fixed records refute whole-history retention; giant records refute temporary
                # field/line ownership in mmap-backed lazy Gemini and Claude parsing.
                records = 4 if shape == "giant" else size // (256 << 10)
                out = root / f"{harness}-{shape}-{size}-{escaped}"
                command = [sys.executable, str(generator), harness, str(out), "--payload-bytes", str(size), "--records", str(records)]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                row, peak = probe(binary, ["count", harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--mode", "lazy"], env, True)
                assert row["messages"] == manifest["expected_messages"], f"{harness}: truncated or skipped source"
                peaks.append(peak)
                print(json.dumps({"case": f"{harness}-{shape}-{'escaped' if escaped else 'plain'}", "decoded_payload_bytes": size,
                                  "physical_input_bytes": manifest["physical_input_bytes"], "messages": row["messages"], "peak_live_rust_bytes": peak}), flush=True)
            bound(peaks, STORE_CEILING, STORE_GROWTH, f"{harness} shape={shape} escaped={escaped}")


def raw_body(path, repeats, malformed):
    block_repeats = 16 << 10
    block = b"x\\n" * block_repeats
    with path.open("wb") as out:
        blocks, tail = divmod(repeats, block_repeats)
        for _ in range(blocks):
            out.write(block)
        out.write(b"x\\n" * tail)
        if malformed:
            out.write(b"\\q")


def chunks(binary, root, env, mmap):
    for malformed in (False, True):
        peaks = []
        for size in SIZES:
            repeats = (size + 2) // 3
            path = root / f"raw-{size}-{malformed}.txt"
            raw_body(path, repeats, malformed)
            raw_bytes = path.stat().st_size
            row, peak = probe(binary, ["chunks", "gemini", "unused", path, "--span", f"0:{raw_bytes}", "--escaped", "--chunk-bytes", CHUNK_BYTES], env, mmap)
            expected = raw_bytes if malformed else repeats * 2
            assert row["detail"]["decoded_bytes"] == expected, "decoder dropped or duplicated bytes"
            assert row["detail"]["largest_chunk_bytes"] <= CHUNK_BYTES + 3, "decoder output chunk is unbounded"
            peaks.append(peak)
            print(json.dumps({"case": f"chunks-{'mmap' if mmap else 'ranged-read'}-{'malformed-tail' if malformed else 'valid'}",
                              "raw_bytes": raw_bytes, "decoded_bytes": expected, "peak_live_rust_bytes": peak}), flush=True)
        bound(peaks, CHUNK_CEILING, CHUNK_GROWTH, f"chunks mmap={mmap} malformed={malformed}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=Path)
    parser.add_argument("--no-mmap-probe", required=True, type=Path)
    parser.add_argument("--only", choices=("stores", "chunks"), help="focused local check; CI runs both")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cv-heap-budget-") as temporary:
        root = Path(temporary)
        catalog = root / "catalog"
        catalog.mkdir()
        env = dict(os.environ, CLUSTERVISION_HOME=str(catalog))
        if args.only != "chunks":
            stores(args.probe.resolve(), root, env)
        if args.only != "stores":
            chunks(args.probe.resolve(), root, env, True)
            chunks(args.no_mmap_probe.resolve(), root, env, False)
    print("live Rust heap budgets passed", flush=True)


if __name__ == "__main__":
    main()
