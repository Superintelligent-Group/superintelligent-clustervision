#!/usr/bin/env python3
"""Extract native bootstrap DDL and its matching migration frontier, without copying store data.
Usage: extract_bootstrap.py CORPUS_DB OUTPUT_DIRECTORY VERSION
Update the emitter's versioned include_str! paths and rerun native checks when upgrading.
"""
from pathlib import Path
import sqlite3
import sys

source, output = map(Path, sys.argv[1:3])
version = sys.argv[3]
with sqlite3.connect(f"file:{source}?mode=ro", uri=True) as connection:
    rows = connection.execute("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END,name").fetchall()
    ids = connection.execute("SELECT id FROM migration ORDER BY id").fetchall()
output.mkdir(parents=True, exist_ok=True)
(output / f"schema-v{version}.sql").write_text(f"-- Native OpenCode v{version} empty-store schema, captured from immutable harness corpus.\n-- No session/project/kv data or migration.v1-v2 marker is copied.\n" + "\n\n".join(row[0] + ";" for row in rows) + "\n")
(output / f"migrations-v{version}.txt").write_text("".join(row[0] + "\n" for row in ids))
