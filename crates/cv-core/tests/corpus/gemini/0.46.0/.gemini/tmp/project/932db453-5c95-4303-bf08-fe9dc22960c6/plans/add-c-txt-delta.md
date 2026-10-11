# Plan: Add c.txt containing "delta"

## Objective
Create a new file `c.txt` in the project root containing the text `delta`, following the existing file conventions in the workspace.

## Key Context
- Existing sibling files follow the pattern of a single word followed by a trailing newline:
  - `a.txt` → `alpha\n`
  - `b.txt` → `gamma\n`
- Therefore `c.txt` should contain `delta\n` to match convention.

## Changes
- Create `/home/corpus/project/c.txt` with the exact content:
  ```
  delta
  ```
  (the word `delta` followed by a trailing newline).

## Verification & Testing
- Run `cat c.txt` (or `xxd c.txt`) in the project root to confirm the file exists and contains exactly `delta\n`.
- Confirm no other files were modified.

## Notes
- Per the request, this is plan-only; no implementation will be done until the plan is approved.
- Do not stage or commit unless explicitly requested later.
