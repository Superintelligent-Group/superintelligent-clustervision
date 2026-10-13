# Cold SQLite snapshot lifetime

An actual scratch `cv doctor goose:<id> --json` read of 8,192 ordinary records (8,000,000
decoded payload bytes) requires a private cold-WAL backing. The producer database has a WAL
header but no WAL/SHM, so opening it directly could create auxiliaries in a read-only store.
Its main file is 11,718,656 bytes. The existing fixture generator created it before any control
process started. No real harness/catalog/home was written.

The independent installed baseline (`927be0c8`, binary SHA in `baseline-cancellation.json`)
removes its copy after normal completion, but SIGINT leaves 11,718,656 logical bytes and
SIGKILL leaves 11,751,424 bytes including SHM. These are named file extents; APFS clones may
share physical blocks. They are not heap, RSS, timing or physical-I/O measurements.

The repaired binary's report is `candidate-lifecycle.json`. Both signals still terminate
without Rust cleanup; the next ordinary doctor operation reclaims all copied DB/WAL/SHM
payloads. Every normal run has the exact baseline output SHA
`264789a069e38d1f208d2ec6d887d5af02c0362704b4f7cd4b761078d3fd99c0`.
Original source SHA, inode/stat identity and absent producer auxiliaries remain unchanged.

The actual generated MCP doctor child was observed with a complete backing and then stopped
with SIGSTOP, preventing natural completion. Closing server stdout and sending ping produces
a writer EPIPE and disconnect cancellation. The composed MCP server kills/reaps that child,
joins handlers and exits zero; no copied payload or child remains. Final TMPDIR contains only
one recognized namespace and its 75-byte gate. The final binary contains the settled lease
repair and the already accepted MCP admission/disconnect change (`debf6b63`). No full gate
suite or additional timing/format matrix ran in this lane.

Reproduce on POSIX with an independent legacy binary and a newly owned output directory:

```sh
python3 tools/check-sqlite-lifecycle.py \
  --binary /absolute/candidate/cv --baseline /absolute/legacy/cv \
  --out /new/owned/scratch/sqlite-lifecycle
```

This creates the same synthetic schema/shape through `perf_fixture_sqlite.py`, switches the
owned fixture to cold WAL and freezes normal legacy output before the candidate controls.
`--reuse-state` reruns a previously created control directory without regenerating its source
or replacing its oracle. The final recorded run used the previously preserved owned state:

```sh
python3 tools/check-sqlite-lifecycle.py \
  --binary /Users/ember/dev/cv-worktrees/native-sqlite-heap-target/debug/cv \
  --out /Users/ember/dev/cv-worktrees/sqlite-lifecycle-evidence --reuse-state
```

21 focused SQLite tests pass with default features and with `--no-default-features --features
sqlite`; both lib/tests clippy checks pass. Tests cover cross-process live/dead leases,
creation/reaper exclusion, malformed/unmarked state, directory/lock replacement, symlinks,
Unix privacy, interrupted payload deletion, cross-thread nested scope drop, independent
long-poll boundaries, backing ownership beyond the connection/cache, LRU reuse/eviction and
original DB/WAL/journal freshness failures. An independent source review cleared publication,
reaper/cache ordering and the Windows handle-ID declaration. Windows runtime behavior was not
executed here; the implementation uses existing fs4 locks and the 128-bit
[FILE_ID_INFO API](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info).

The limit is explicit: standalone abrupt death needs a later operation in the same recognized
namespace; busy gates and filesystem errors defer reclamation. There is no daemon or claim of
immediate SIGKILL cleanup. Only marked, identity-bound unlocked owners are reclaimed; legacy,
foreign, malformed and symlink-substituted paths are left alone. Normal completion leaves only
fixed gate bookkeeping. An interruption before the owner record is published, or after the
last payload is removed, can leave tiny incomplete metadata; copied payload is created only
after publication and its recognition marker survives until every payload is gone.
