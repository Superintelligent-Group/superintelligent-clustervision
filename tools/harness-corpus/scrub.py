"""Scrub a generated corpus in place: throwaway paths -> /home/corpus, then refuse anything secret.

    python3 -I scrub.py --work WORK --home CORPUS_HOME --dir OUT

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


def is_sqlite(path):
    with open(path, "rb") as f:
        return f.read(16) == b"SQLite format 3\x00"


def scrub_sqlite(path, reps):
    con = sqlite3.connect(path)
    for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
        cols = [r[1] for r in con.execute(f'PRAGMA table_info("{t}")')]
        rows = con.execute(f'SELECT rowid, * FROM "{t}"').fetchall()
        for row in rows:
            new = []
            for v in row[1:]:
                if isinstance(v, str):
                    v = fix(v.encode(), reps).decode()
                elif isinstance(v, bytes):
                    v = fix(v, reps)
                new.append(v)
            if list(row[1:]) != new:
                sets = ", ".join(f'"{c}" = ?' for c in cols)
                con.execute(f'UPDATE "{t}" SET {sets} WHERE rowid = ?', [*new, row[0]])
    con.commit()
    con.execute("VACUUM")
    con.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--work", required=True)
    p.add_argument("--home", required=True)
    p.add_argument("--dir", required=True)
    a = p.parse_args()
    reps = replacements(a.work, a.home)
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
        why = [n for n, hit in (
            ("a literal key", any(k in b for k in keys)),
            ("a token shape", any(r.search(b) for r in TOKEN_RES)),
            ("/Users/", b"/Users/" in b),
            ("the real user name", len(user) > 2 and re.search(rb"\b" + re.escape(user) + rb"\b", b) is not None),
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
