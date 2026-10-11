# Plan: Add `c.txt` containing `delta`

## Context
The project directory (`~/project`) contains text files following a simple naming/content pattern:
- `a.txt` → `alpha\nbeta\n`
- `b.txt` → `gamma\n`

The task is to add `c.txt` with the content `delta`, continuing this pattern.

## Steps
1. Create `~/project/c.txt` with the exact content `delta\n` (trailing newline, matching the convention of `a.txt` and `b.txt`).
2. Verify by reading `c.txt` back and confirming its contents are exactly `delta`.

## Verification
- `read_file` on `~/project/c.txt` returns `delta\n`.

## Notes
- No changes to existing files; `c.txt` is a new file only.
- Per repo rules, no git staging/commit unless explicitly requested.
