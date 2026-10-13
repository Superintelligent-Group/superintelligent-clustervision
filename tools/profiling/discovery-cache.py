#!/usr/bin/env python3
"""Compare first-process scoped listing allocations with 0/4096 unrelated cache rows.

All inputs/catalogs are synthetic and created under a new --output directory. Each
child gets a minimal environment and the same restored catalog path/input identity.
The probe's elapsed-time field is discarded: this control measures allocation work.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", required=True, type=Path)
    parser.add_argument("--after", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binaries = {name: path.resolve(strict=True) for name, path in
                [("before", args.before), ("after", args.after)]}
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("probes must be executable files")
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    home, catalog, seed = [root / name for name in ("home", "catalog", "seed")]
    source = home / ".claude/projects/-fixed/alpha.jsonl"
    source.parent.mkdir(parents=True)
    catalog.mkdir()
    (root / "tmp").mkdir()
    # Do not inherit credentials, harness root overrides or another installation's stores.
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C",
           "HOME": str(home), "CLUSTERVISION_HOME": str(catalog),
           "XDG_CACHE_HOME": str(home / ".cache"),
           "XDG_CONFIG_HOME": str(home / ".config"),
           "XDG_DATA_HOME": str(home / ".local/share"), "TMPDIR": str(root / "tmp"),
           "RAYON_NUM_THREADS": "1", "CLUSTERVISION_MAX_STALE_SECS": "3600"}

    def invoke(binary):
        child = subprocess.run([str(binary), "sessions", "claude", "alpha", str(source),
                                "--root", str(home / ".claude"), "--hash-output"],
                               env=env, cwd=root, capture_output=True, text=True, check=True)
        result = json.loads(child.stdout)
        result.pop("wall_seconds", None)
        if not result.get("build_features", {}).get("probe_heap"):
            raise ValueError("both probes require --features probe-heap")
        return result, child.stderr

    def record(i):
        return json.dumps({"type": "user", "uuid": f"u{i}", "sessionId": "alpha",
                           "timestamp": "2026-01-01T00:00:00Z",
                           "message": {"role": "user", "content": f"hello {i}"}}) + "\n"

    source.write_text(record(1))
    for directory in source.parents:
        if not directory.is_relative_to(home):
            break
        os.utime(directory, (1700000000, 1700000000))
    initial, stderr = invoke(binaries["before"])
    assert initial["detail"]["discovered_sessions"] == 1 and not stderr
    with sqlite3.connect(catalog / "catalog.db") as db:
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    seed.mkdir()
    shutil.copyfile(catalog / "catalog.db", seed / "catalog.db")
    with source.open("a") as file:
        file.write(record(2))
    input_hash, seed_hash = digest(source), digest(seed / "catalog.db")
    def identity(path):
        stat = path.stat()
        return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns
    input_identity = identity(source)
    results = {}
    for label, binary in binaries.items():
        for rows in (0, 4096):
            # Only this runner-created catalog is removed; transcript and seed remain frozen.
            shutil.rmtree(catalog)
            catalog.mkdir()
            shutil.copyfile(seed / "catalog.db", catalog / "catalog.db")
            with sqlite3.connect(catalog / "catalog.db") as db:
                original = db.execute("SELECT mtime_ns,size,refs,source_reads,interpretation_version "
                                      "FROM scan_cache LIMIT 1").fetchone()
                assert original is not None
                for i in range(rows):
                    fake = str(root / "unrelated" / f"{i}.jsonl")
                    refs = json.loads(original[2])
                    refs[0].update(path=fake, id=f"unrelated-{i}", harness="codex",
                                   title="metadata " + "x" * 1024)
                    reads = original[3].replace(str(source), fake)
                    db.execute("INSERT INTO scan_cache(path,mtime_ns,size,refs,source_reads,"
                               "interpretation_version) VALUES(?,?,?,?,?,?)",
                               (fake, original[0], original[1],
                                json.dumps(refs, separators=(",", ":")), reads, original[4]))
                db.commit()
                db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            result, stderr = invoke(binary)
            assert not stderr and result["detail"]["discovered_sessions"] == 1
            assert result["build_features"] == initial["build_features"]
            assert digest(source) == input_hash and identity(source) == input_identity
            assert digest(seed / "catalog.db") == seed_hash
            with sqlite3.connect(catalog / "catalog.db") as db:
                assert db.execute("SELECT message_count FROM sessions WHERE id='alpha'").fetchone() == (2,)
            results[f"{label}-{rows}"] = {"heap": result["requested_rust_heap"],
                                           "output": result["detail"], "stderr": stderr}
    outputs = [case["output"] for case in results.values()]
    assert all(output == outputs[0] for output in outputs), "listing bytes/counts differ"
    report = {"operation": "first-process sessions(): one stale Claude primary, warmed persisted catalog",
              "unrelated_cache_rows": [0, 4096], "unrelated_title_bytes": 1033,
              "rayon_workers": 1, "fixture_sha256": input_hash, "seed_sha256": seed_hash,
              "binary_sha256": {name: digest(path) for name, path in binaries.items()},
              "build_features": initial["build_features"], "results": results,
              "limits": ["requested Rust allocations exclude SQLite/C, stacks and mmap",
                         "synthetic cache metadata; not genuine corpus or native elapsed-time evidence",
                         "bulk SQL string copying and serialized cache retention remain fleet-sized"]}
    (root / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
