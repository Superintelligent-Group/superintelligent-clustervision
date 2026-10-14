#!/usr/bin/env python3
"""Record the SHAPE of a harness store that cannot be driven headless, without its content.

    schema_snapshot.py sqlite DB [--tables T1,T2] [--json-column TABLE.COLUMN[:zstd]]...  > schema.txt
    schema_snapshot.py json GLOB...                                      > shape.txt

sqlite: the DDL from sqlite_master (table, index and trigger statements; DDL carries no rows), plus
`PRAGMA user_version`, plus, per --json-column, the merged shape of the JSON stored in that column
(decompressed first when marked :zstd and the row says so).
json: the merged shape of every matching file.

A shape is key paths and value types, never values: `messages[].role: str = assistant|user`. Only
the values of tag-like fields (type, role, kind, status, ...) that look like identifiers are
listed, because those enumerations are what a format change alters. Any object whose keys are not
all plain identifiers (ids, paths, names used as keys) is collapsed to `{<key>: ...}`. Rows are
opened read-only and nothing is written next to the store.
"""
import argparse
import glob
import json
import re
import sqlite3
import sys
import shutil
import tempfile
from collections import defaultdict
from contextlib import contextmanager
from pathlib import Path

TAGS = {"type", "role", "kind", "status", "version",
        "variant", "mode", "state", "source", "format", "stopReason", "stop_reason", "finish_reason",
        "data_type", "fileType"}
IDENT = re.compile(r"^[A-Za-z][A-Za-z0-9_]{0,40}$")
TAGVAL = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,40}$")


def sqlite_files(path):
    return [path, Path(str(path) + "-wal"), Path(str(path) + "-journal")]


def fingerprint(path):
    try:
        stat = path.stat()
    except FileNotFoundError:
        return None
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


@contextmanager
def sqlite_snapshot(path):
    """SQLite touches only an owned scratch copy, including committed WAL content.

    immutable=1 skips WAL entirely; mode=ro alone may create SHM at the source. Copy
    stable DB/WAL/journal tuples instead and refuse observed concurrent changes rather
    than report a mixed or stale store shape. No source content is retained afterwards.
    """
    path = Path(path).resolve(strict=True)
    files = sqlite_files(path)
    captured = [fingerprint(file) for file in files]

    def check():
        if [fingerprint(file) for file in files] != captured:
            raise RuntimeError("SQLite source changed during schema snapshot; retry after the writer is quiet")

    with tempfile.TemporaryDirectory(prefix="cv-schema-snapshot.") as scratch:
        database = Path(scratch) / "store.db"
        for source, stamp, suffix in zip(files, captured, ("", "-wal", "-journal")):
            if stamp is not None:
                target = Path(str(database) + suffix)
                shutil.copyfile(source, target)
                target.chmod(0o600)
        check()
        connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)
        try:
            connection.execute("BEGIN")
            yield connection
            check()
        finally:
            connection.close()


class Shape:
    def __init__(self):
        self.types = defaultdict(set)
        self.values = defaultdict(set)

    def add(self, v, path="$"):
        if isinstance(v, dict):
            self.types[path].add("obj")
            keys = list(v)
            if len(keys) > 60 or not all(IDENT.match(k) and not re.search(r"\d{3}", k) for k in keys):
                for x in v.values():
                    self.add(x, path + ".<key>")
                return
            for k, x in v.items():
                if k in TAGS and isinstance(x, (str, int)) and not isinstance(x, bool) and TAGVAL.match(str(x)):
                    self.values[path + "." + k].add(str(x))
                self.add(x, path + "." + k)
        elif isinstance(v, list):
            self.types[path].add("list")
            for x in v:
                self.add(x, path + "[]")
        else:
            self.types[path].add({str: "str", bool: "bool", int: "int", float: "float", type(None): "null"}.get(type(v), "?"))

    def lines(self):
        out = []
        for p in sorted(self.types):
            vals = sorted(self.values.get(p, ()))
            tail = " = " + "|".join(vals[:40]) + (" ..." if len(vals) > 40 else "") if vals else ""
            out.append(f"{p}: {'|'.join(sorted(self.types[p]))}{tail}")
        return out


def decode(cell, zstd):
    if cell is None:
        return None
    if isinstance(cell, str):
        cell = cell.encode()
    if zstd and cell[:4] == b"\x28\xb5\x2f\xfd":
        from compression import zstd as z  # Python 3.14+
        cell = z.decompress(cell)
    try:
        return json.loads(cell)
    except ValueError:
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("kind", choices=["sqlite", "json"])
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--json-column", action="append", default=[])
    ap.add_argument("--tables", help="comma-separated: only these tables' DDL (and their indexes)")
    a = ap.parse_args()
    out = []
    if a.kind == "sqlite":
        with sqlite_snapshot(a.paths[0]) as c:
            out.append(f"-- PRAGMA user_version = {c.execute('PRAGMA user_version').fetchone()[0]}")
            keep = set(a.tables.split(",")) if a.tables else None
            for typ, name, tbl, sql in c.execute(
                    "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name"):
                if keep is None or tbl in keep:
                    out.append(sql.strip() + ";")
            for spec in a.json_column:
                col, _, codec = spec.partition(":")
                table, column = col.split(".")
                sh = Shape()
                n = 0
                for (cell,) in c.execute(f'SELECT "{column}" FROM "{table}"'):
                    v = decode(cell, codec == "zstd")
                    if v is not None:
                        sh.add(v)
                        n += 1
                out.append(f"\n-- JSON shape of {table}.{column} (merged over {n} rows)")
                out += sh.lines()
    else:
        sh = Shape()
        files = sorted({f for g in a.paths for f in glob.glob(g)})
        for f in files:
            with open(f, "rb") as fh:
                try:
                    sh.add(json.load(fh))
                except ValueError:
                    pass
        out.append(f"-- JSON shape merged over {len(files)} files")
        out += sh.lines()
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
