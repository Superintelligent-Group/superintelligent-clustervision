#!/usr/bin/env python3
"""Clone a stable macOS index and run the opt-in production byte-tier control.

The source is read-only. The scratch directory must not exist; all mutations are
confined to its clone. Pass a compiled cv-search test executable, not cargo.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def inventory(root):
    result = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"index contains symlink: {path}")
        if path.is_file():
            stat = path.stat()
            result[str(path.relative_to(root))] = [
                stat.st_dev, stat.st_ino, stat.st_size,
                stat.st_mtime_ns, stat.st_ctime_ns,
            ]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--index", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--test-binary", type=Path, required=True)
    args = parser.parse_args()
    source = args.index.resolve(strict=True)
    scratch = args.scratch.resolve()
    if scratch == source or source in scratch.parents or scratch in source.parents:
        parser.error("scratch and source must be separate trees")
    binary = args.test_binary.resolve(strict=True)
    scratch.mkdir(parents=True, exist_ok=False)
    before = inventory(source)
    meta = (source / "meta.json").read_bytes()

    def clone_file(src, dst):
        subprocess.run(["/bin/cp", "-c", src, dst], check=True)
        return dst

    clone = scratch / "index"
    shutil.copytree(source, clone, copy_function=clone_file)
    if inventory(source) != before or (clone / "meta.json").read_bytes() != meta:
        raise RuntimeError("source changed while copying; clone was not tested")
    for name, stat in before.items():
        if (clone / name).stat().st_size != stat[2]:
            raise RuntimeError(f"clone length mismatch: {name}")
    (clone / "cv-compaction-control-copy").write_text("disposable clone\n")
    env = dict(os.environ, CLUSTERVISION_HOME=str(scratch / "catalog"),
               CV_COMPACTION_CONTROL_INDEX=str(clone),
               CV_COMPACTION_CONTROL_REPORT=str(scratch / "result.json"),
               RAYON_NUM_THREADS="1")
    with (scratch / "test.log").open("wb") as log:
        subprocess.run([str(binary), "fts::tests::existing_index_compaction_work_control",
                        "--exact", "--ignored", "--nocapture"],
                       env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    report = json.loads((scratch / "result.json").read_text())
    report["source_snapshot"] = {
        "files": len(before), "logical_bytes": sum(s[2] for s in before.values()),
        "meta_sha256": hashlib.sha256(meta).hexdigest(),
        "stable_across_clone": True,
        "unchanged_at_control_end": inventory(source) == before,
    }
    with binary.open("rb") as executable:
        report["test_binary_sha256"] = hashlib.file_digest(executable, "sha256").hexdigest()
    report["limits"] = [
        "Segment file lengths are logical merge input/output, not physical I/O.",
        "Sixteen direct tiny document commits exercise production compaction, not discovery or an index sweep.",
        "An older-schema input tests existing geometry, not current-schema fleet rebuild or migration.",
        "Query checks preserve matching stored-ID multisets; they do not compare BM25 scores or snippets.",
        "This does not establish general merge-fanout, deleted-byte, heap or latency bounds.",
    ]
    (scratch / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
