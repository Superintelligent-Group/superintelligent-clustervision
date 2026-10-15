#!/usr/bin/env python3
"""Parsimony gate: product code per crate stays within a checked-in budget, and the tree holds no
committed evidence. Growth is allowed only by editing tools/size-budget.txt in the same change, so
it is a decision a reviewer sees, not drift. The budget also ratchets down: one more than
SLACK above the measured size fails, so freed room is not silently refilled.

Product lines: non-blank lines of crates/<crate>/src/**/*.rs, excluding tests.rs files, `tests`
directories, and everything from a file's first `#[cfg(test)]` item onward.
Run from the repository root: python3 -I tools/check-size.py [--write]"""

import pathlib
import re
import subprocess
import sys

BUDGET = pathlib.Path("tools/size-budget.txt")
SLACK = 0.05
DOC_MAX_LINES = 250
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]")


def product_lines(crate: pathlib.Path) -> int:
    total = 0
    for path in sorted((crate / "src").rglob("*.rs")):
        if path.name == "tests.rs" or "tests" in path.relative_to(crate / "src").parts[:-1]:
            continue
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if CFG_TEST.match(line):
                break
            if line.strip():
                total += 1
    return total


def measure() -> dict[str, int]:
    return {
        crate.name: product_lines(crate)
        for crate in sorted(pathlib.Path("crates").iterdir())
        if (crate / "src").is_dir()
    }


def tracked(*patterns: str) -> list[str]:
    out = subprocess.run(["git", "ls-files", "--", *patterns], capture_output=True, text=True, check=True)
    return [line for line in out.stdout.splitlines() if line]


def main() -> int:
    sizes = measure()
    if "--write" in sys.argv:
        BUDGET.write_text(
            "# Product lines per crate (tools/check-size.py). Raise a number only as a reviewed decision.\n"
            + "".join(f"{name} {lines}\n" for name, lines in sizes.items())
        )
        print(f"wrote {BUDGET}")
        return 0

    failures = []
    budget = {}
    for line in BUDGET.read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            name, lines = line.split()
            budget[name] = int(lines)
    for name, lines in sizes.items():
        limit = budget.get(name)
        if limit is None:
            failures.append(f"{name}: {lines} product lines and no budget (add it to {BUDGET})")
        elif lines > limit:
            failures.append(f"{name}: {lines} product lines exceed the budget of {limit} by {lines - limit}")
        elif lines < limit / (1 + SLACK):
            failures.append(f"{name}: budget {limit} is more than {SLACK:.0%} above the {lines} measured; lower it")
        print(f"{name:12} {lines:7} / {limit if limit is not None else '-'}")
    for name in budget.keys() - sizes.keys():
        failures.append(f"{name}: budgeted but no such crate; remove it from {BUDGET}")

    for path in tracked("docs/*.json", "docs/**/*.json", "tools/profiling/results/*"):
        failures.append(f"{path}: measurements belong in commit messages, not committed files")
    for path in tracked("docs/design/*.md"):
        n = len(pathlib.Path(path).read_text().splitlines())
        if n > DOC_MAX_LINES:
            failures.append(f"{path}: {n} lines; design docs stay under {DOC_MAX_LINES} (history lives in git)")

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
