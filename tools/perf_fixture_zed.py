"""Zed's corpus-backed thread schema, compressed with its documented zstd level 3."""

import base64
import json
import random
import shutil
import sqlite3
import subprocess


def generate_zed(root, size, records, escaped, entropy=False):
    codec = shutil.which("zstd")
    if not codec:
        raise RuntimeError("Zed fixture generation needs zstd on PATH; generation is outside the probe")
    sid = "00000000-0000-4000-8000-000000000001"
    stamp = "2026-01-01T00:00:00Z"
    pattern = 'line\n"quoted"\t\\end' if escaped else "plain payload "
    rng = random.Random(20260101)
    decoded = root / "generation-thread.json"
    with decoded.open("w", encoding="utf-8") as out:
        out.write('{"version":"0.2.0","summary":"perf fixture","updated_at":' + json.dumps(stamp) + ',"messages":[')
        for index in range(records):
            length = size // records + (index < size % records)
            if entropy:
                payload = base64.b64encode(rng.randbytes((length * 3 + 3) // 4)).decode("ascii")[:length]
                if escaped:
                    payload = payload.translate(str.maketrans({"A": "\n", "B": '"', "C": "\\", "D": "\t"}))
            else:
                payload = (pattern * ((length + len(pattern) - 1) // len(pattern)))[:length]
            for role, text in (("user", f"prompt {index}"), ("assistant", payload)):
                if index or role == "assistant":
                    out.write(",")
                json.dump({"id": index * 2 + (role == "assistant"), "role": role,
                           "segments": [{"type": "text", "text": text}], "tool_uses": [], "tool_results": [],
                           "context": "", "creases": [], "is_hidden": False}, out,
                          ensure_ascii=False, separators=(",", ":"))
        out.write('],"model":{"provider":"fixture","model":"test"},"profile":"write"}\n')
    decoded_bytes = decoded.stat().st_size
    compressed = root / "generation-thread.zstd"
    with compressed.open("wb") as output:
        subprocess.run([codec, "-3", "--quiet", "--stdout", str(decoded)], stdout=output, check=True)
    blob_bytes = compressed.stat().st_size
    path = root / "threads.db"
    with sqlite3.connect(path) as connection:
        connection.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, summary TEXT NOT NULL, updated_at TEXT NOT NULL, "
                           "data_type TEXT NOT NULL, data BLOB NOT NULL, parent_id TEXT, folder_paths TEXT, "
                           "folder_paths_order TEXT, created_at TEXT)")
        connection.execute("INSERT INTO threads (id,summary,updated_at,data_type,data,created_at) VALUES (?,?,?,?,?,?)",
                           (sid, "perf fixture", stamp, "zstd", compressed.read_bytes(), stamp))
    # Only delete our two generation intermediates; the measured source is the SQLite blob.
    decoded.unlink()
    compressed.unlink()
    return {"harness": "zed", "format": "zed", "shape": "history", "id": sid, "path": str(path), "root": str(root),
            "decoded_payload_bytes": size, "selected_payload_bytes": size, "decoded_document_bytes": decoded_bytes,
            "compressed_blob_bytes": blob_bytes, "physical_input_bytes": path.stat().st_size,
            "files": {path.name: path.stat().st_size}, "records": records, "escaped": escaped,
            "expected_messages": records * 2, "expected_discovered_sessions": 1, "codec": "zstd level 3",
            "entropy": entropy, "entropy_seed": 20260101 if entropy else None}
