"""Scrub a generated corpus in place: throwaway paths -> /home/corpus, then refuse anything secret.

    python3 -I scrub.py --work WORK --home CORPUS_HOME --dir OUT [--also FROM=TO ...]

Rewrites text files and every TEXT/BLOB cell of every sqlite table (then VACUUMs). Afterwards it
scans every file for key material (the literal keys named in $CORPUS_KEY_FILES, common token
shapes), the real home path, and the real user name, and exits 1 naming the file — never the match.
"""
import argparse
import os
import re
import sqlite3
import sys
from urllib.parse import quote

PLACEHOLDER_HOME = "/home/corpus"
EMAIL = "corpus@example.invalid"
TOKEN_RES = [re.compile(p) for p in (
    rb"sk-[A-Za-z0-9_-]{20,}", rb"sk-or-v1-[0-9a-f]{20,}", rb"gh[pousr]_[A-Za-z0-9]{30,}", rb"github_pat_[A-Za-z0-9_]{30,}",
    rb"xox[abpr]-[A-Za-z0-9-]{10,}", rb"AKIA[0-9A-Z]{16}", rb"AIza[0-9A-Za-z_-]{35}", rb"[Bb]earer [A-Za-z0-9._~+/-]{20,}",
    rb"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.", rb"[0-9a-f]{32}\.[A-Za-z0-9]{16}",
)]
# A JSON escape is not part of the address (`\n@dot.png` is a newline and an @-mention; in
# `\nme@x.com` the address starts at `m`), and a file name (`me@dot.png`) is not one.
EMAIL_RE = re.compile(
    rb"(?:(?<=\\[nrt])|(?<![\\A-Za-z0-9._%+-]))[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+(?![A-Za-z0-9-]|\.[A-Za-z0-9])"
    rb"(?<!\.png)(?<!\.jpg)(?<!\.jpeg)(?<!\.gif)(?<!\.txt)(?<!\.md)(?<!\.json)(?<!\.rs)(?<!\.py)"
)


def replacements(work, home):
    reps = []
    for base in {work, os.path.realpath(work), work.replace("/private/", "/", 1)}:
        h = home.replace(work, base, 1)
        reps += [(h, PLACEHOLDER_HOME), (base, PLACEHOLDER_HOME + "/.work")]
    # The harness's own install dir (a system prompt can list paths inside it, e.g. bundled skills),
    # then the rest of the real home, when a model writes it out (it saw the user name in `ls -l`, or
    # read it off a scratch path); the longer install-dir rule is applied first.
    real_home = os.environ.get("CORPUS_REAL_HOME", "")
    if real_home:
        reps.append((os.path.join(real_home, "tools", "harness-versions"), "/opt/harness-versions"))
    if len(real_home) > 3:
        reps.append((real_home, PLACEHOLDER_HOME))
    # file:// URLs and JSON-escaped slashes carry the same paths
    reps += [(a.replace("/", "\\/"), b.replace("/", "\\/")) for a, b in list(reps)]
    # Claude Code (and others) name a project dir by its path with every non-alphanumeric made `-`
    reps += [(dashed(a), dashed(b)) for a, b in list(reps) if "\\" not in a]
    # ... and Grok by its percent-encoded cwd (`%2Fhome%2Fcorpus%2Fproject`)
    reps += [(quote(a, safe=""), quote(b, safe="")) for a, b in list(reps) if a.startswith("/")]
    return sorted(set(reps), key=lambda r: -len(r[0]))


def dashed(path):
    return re.sub(r"[^A-Za-z0-9]", "-", path)


def rename_paths(top, reps):
    """File and dir names carry the throwaway path too (an encoded-cwd project dir); rename them."""
    for d, dirs, fs in os.walk(top, topdown=False):
        for name in dirs + fs:
            new = name
            for a, z in reps:
                if "/" not in a:
                    new = new.replace(a, z)
            if new != name:
                os.rename(os.path.join(d, name), os.path.join(d, new))


ACCOUNT_RE = re.compile(rb'("(?:creator_user_id|creator_account_id|account_id|user_id|email)"\s*:\s*)"[^"]*"')


USER = os.path.basename(os.environ.get("CORPUS_REAL_HOME", "")).encode()


def fix(b, reps):
    for a, z in reps:
        b = b.replace(a.encode(), z.encode())
        # A model retyping a path can change its case (`corpus-ide` -> `corpus-IDE`).
        b = re.sub(re.escape(a.encode()), z.replace("\\", "\\\\").encode(), b, flags=re.IGNORECASE)
    if len(USER) > 2:  # tool output like `ls -l` carries the owner name
        b = re.sub(rb"\b" + re.escape(USER) + rb"\b", b"corpus", b)
    b = ACCOUNT_RE.sub(lambda m: m.group(1) + b'"' + EMAIL.encode() + b'"', b)
    return EMAIL_RE.sub(lambda m: m.group(0) if m.group(0).endswith((b".invalid", b".example")) or b"@openai" in m.group(0) else EMAIL.encode(), b)


ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"


def zstd():
    """Python 3.14's compression.zstd; zstd cells (OpenClaw's event_zstd) cannot be scrubbed without it."""
    try:
        from compression import zstd as z
    except ImportError:
        sys.exit("scrub: this store has zstd-compressed sqlite cells; run scrub.py with python >= 3.14")
    return z


def fix_blob(v, reps):
    """A zstd frame is scrubbed inside (decompress, rewrite, recompress at level 1 with a checksum)."""
    if not v.startswith(ZSTD_MAGIC):
        return fix(v, reps), None
    z = zstd()
    raw = z.decompress(v)
    new = fix(raw, reps)
    if new == raw:
        return v, None
    return z.compress(new, options={z.CompressionParameter.compression_level: 1,
                                    z.CompressionParameter.checksum_flag: 1}), len(new)


def sqlite_expanded(path):
    """The decompressed contents of every zstd cell, so the secret scan sees inside them."""
    con = sqlite3.connect(path)
    out = []
    for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
        for row in con.execute(f'SELECT * FROM "{t}"'):
            out += [zstd().decompress(v) for v in row if isinstance(v, bytes) and v.startswith(ZSTD_MAGIC)]
    con.close()
    return b"\n".join(out)


def is_sqlite(path):
    with open(path, "rb") as f:
        return f.read(16) == b"SQLite format 3\x00"


def fts_tables(con):
    """(FTS5 virtual tables, their shadow tables) in an open database."""
    master = con.execute("SELECT name, sql FROM sqlite_master WHERE type='table'").fetchall()
    fts = [n for n, sql in master if re.search(r"CREATE VIRTUAL TABLE.*USING\s+fts5", sql or "", re.I | re.S)]
    return fts, {f"{v}_{s}" for v in fts for s in ("data", "idx", "config", "docsize", "content")}


def sqlite_cells(path):
    """Every cell of every table except FTS5 index segments, as bytes. Index terms are prefix-
    compressed ('remember' after 'rem...' is stored as 'ember'), so a word check on the raw file
    hits fragments of other words; the indexed text itself is checked through the content."""
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    _, shadow = fts_tables(con)
    out = []
    for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall():
        if t in shadow and not t.endswith("_content"):
            continue
        try:
            rows = con.execute(f'SELECT * FROM "{t}"').fetchall()
        except sqlite3.Error:
            continue
        for row in rows:
            out += [(zstd().decompress(v) if v.startswith(ZSTD_MAGIC) else v) if isinstance(v, bytes) else str(v).encode()
                    for v in row if v is not None]
    con.close()
    return b"\n".join(out)


def scrub_sqlite(path, reps):
    """Rewrite every TEXT/BLOB cell. FTS5 virtual tables are skipped along with their shadow tables
    (index segments are not text) and rebuilt from their scrubbed content afterwards; a WITHOUT
    ROWID table is addressed by its primary key. A zstd cell is scrubbed inside."""
    con = sqlite3.connect(path)
    master = con.execute("SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall()
    fts, shadow = fts_tables(con)
    for t, sql in master:
        if t in fts or t in shadow or (sql or "").upper().startswith("CREATE VIRTUAL TABLE"):
            continue
        info = list(con.execute(f'PRAGMA table_info("{t}")'))
        cols = [r[1] for r in info]
        if re.search(r"WITHOUT\s+ROWID\s*$", sql or "", re.I):
            key = [r[1] for r in sorted(info, key=lambda r: r[5]) if r[5]]
        else:
            key = ["rowid"]
        sel = ", ".join(f'"{c}"' if c != "rowid" else c for c in key)
        rows = con.execute(f'SELECT {sel}, * FROM "{t}"').fetchall()
        where = " AND ".join(f'{c} = ?' if c == "rowid" else f'"{c}" = ?' for c in key)
        for row in rows:
            k, vals = row[:len(key)], row[len(key):]
            new = list(vals)
            for i, v in enumerate(vals):
                if isinstance(v, str):
                    new[i] = fix(v.encode(), reps).decode()
                elif isinstance(v, bytes):
                    new[i], raw_len = fix_blob(v, reps)
                    # `<x>_zstd` keeps its uncompressed size in `<x>_utf8_bytes` (OpenClaw transcript_events)
                    size_col = cols[i].removesuffix("_zstd") + "_utf8_bytes"
                    if raw_len is not None and cols[i].endswith("_zstd") and size_col in cols:
                        new[cols.index(size_col)] = raw_len
            if list(vals) != new:
                sets = ", ".join(f'"{c}" = ?' for c in cols)
                con.execute(f'UPDATE "{t}" SET {sets} WHERE {where}', [*new, *k])
    for v in fts:
        con.execute(f'INSERT INTO "{v}"("{v}") VALUES (\'rebuild\')')
    con.commit()
    con.execute("VACUUM")
    # A WAL-mode file makes every reader (the golden test included) create -wal/-shm beside it.
    con.execute("PRAGMA journal_mode=DELETE")
    con.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--work", required=True)
    p.add_argument("--home", required=True)
    p.add_argument("--dir", required=True)
    p.add_argument("--also", action="append", default=[], metavar="FROM=TO",
                   help="an extra literal rewrite, e.g. a harness install prefix the store records")
    a = p.parse_args()
    reps = replacements(a.work, a.home)
    for kv in a.also:
        frm, _, to = kv.partition("=")
        reps += [(frm, to), (frm.replace("/", "\\/"), to.replace("/", "\\/"))]
    reps = sorted(set(reps), key=lambda r: -len(r[0]))
    rename_paths(a.dir, reps)
    files = [os.path.join(d, f) for d, _, fs in os.walk(a.dir) for f in fs]
    for path in files:
        if is_sqlite(path):
            scrub_sqlite(path, reps)
        else:
            with open(path, "rb") as f:
                b = f.read()
            nb = fix(b, reps)
            if nb != b:
                with open(path, "wb") as f:
                    f.write(nb)
    keys = []
    for kf in filter(None, os.environ.get("CORPUS_KEY_FILES", "").split(":")):
        with open(kf, "rb") as f:
            k = f.read().strip()
        if k:
            keys.append(k)
    user = USER
    bad = []
    for path in files:
        with open(path, "rb") as f:
            b = f.read()
        words = sqlite_cells(path) if is_sqlite(path) else b
        if is_sqlite(path):
            b += sqlite_expanded(path)
        why = [n for n, hit in (
            ("a literal key", any(k in b for k in keys)),
            ("a token shape", any(r.search(b) for r in TOKEN_RES)),
            ("/Users/", b"/Users/" in b),
            ("the real user name", len(user) > 2 and re.search(rb"\b" + re.escape(user) + rb"\b", words) is not None),
            ("the throwaway dir", any(x.encode() in b for x in (a.work, dashed(a.work), quote(a.work, safe="")))),
        ) if hit]
        if any(x in path for x in (a.work, dashed(a.work), quote(a.work, safe=""))):
            why.append("the throwaway dir in its name")
        if why:
            bad.append(f"{os.path.relpath(path, a.dir)}: {', '.join(why)}")
    if bad:
        print("scrub: refusing, these files still carry something they must not:", *bad, sep="\n  ", file=sys.stderr)
        sys.exit(1)
    print(f"scrub: {len(files)} files clean")


if __name__ == "__main__":
    main()
