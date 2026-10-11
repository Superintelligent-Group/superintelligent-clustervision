#!/usr/bin/env python3
"""Copy an opencode.db for the corpus: `thin.py <src.db> <dst.db>`.

The sqlite backup API folds the WAL in; then the rows of two non-session tables are deleted (their
schema stays): `kv` entries under `models-dev:` (the models.dev catalog 2.x caches, ~6 MB) and the
`event` sync log (a replay of the same message/part updates). Journal mode is set to DELETE and the
file VACUUMed, so the committed store is one self-contained file.
"""
import sqlite3
import sys

src, dst = sqlite3.connect(sys.argv[1]), sqlite3.connect(sys.argv[2])
src.backup(dst)
src.close()
tables = {r[0] for r in dst.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
if "kv" in tables:
    dst.execute("DELETE FROM kv WHERE key LIKE 'models-dev:%'")
if "event" in tables:
    dst.execute("DELETE FROM event")
dst.commit()
dst.execute("PRAGMA journal_mode=DELETE")
dst.execute("VACUUM")
dst.close()
