# Selected payload regression gate

Run `tools/check-heap-budget.py PROBE --no-mmap-probe PORTABLE --only selected-payloads`.
Both probes must have `probe-heap`; this selector measures the native mmap probe.
`--only portable-selected-payloads` runs the same cases, oracles, and budgets against PORTABLE.
That binary must enable SQLite without mmap:
`cargo build --release -p clustervision-core --example store_probe --no-default-features --features sqlite,probe-heap`.
The older no-SQLite portable gates keep their separate build; selected SQLite stores require this feature.
Use repeated `--format` to focus Kimi Code, OpenCode files/SQLite, Cursor CLI/IDE, or Goose.

The shared fixture generator creates native-shaped synthetic stores in new scratch directories.
Each case has a giant selected record and a growing 64→256-turn history at 16→64 MiB,
with plain and escaped text. Payloads occupy selected text, reasoning where supported,
structured arguments, and tool results. A tiny final tool call supplies the late query sentinel.

Each isolated probe exercises lazy discard, Facts doctor, accepted deferred dataset collection,
and a missing-tool deferred query. Doctor reports must exactly equal a separate full-mode
report; accepted dataset bytes and SHA256 must equal a separate full-mode collector.
Each probe runs with a 64-descriptor limit; each generated store is removed after its oracles.
This catches lost or altered payloads as well as skipped facts/messages. Missing queries must
emit no record. Every partial consumer has an 8 MiB live Rust ceiling and 2 MiB growth allowance.
Full references intentionally have no bounded-memory claim.

These counters exclude SQLite native allocations, mapped pages, filesystem caches, and spill
storage. Arbitrary record/file counts, rich full metadata, complete mode, images/encrypted URLs,
and unrepresented malformed/legacy fallbacks are outside this selected-payload promise.
The fixtures are synthetic and never regenerate committed harness corpora or write user stores.

`--only metadata-facts` separately checks Claude, Cursor CLI, and Goose unused rich sidecars.
Its doctor Facts report must equal full mode and remain unchanged across 16/64 MiB at fixed
message count. Its stricter budget is 1 MiB live Rust heap and 256 KiB growth. Goose puts the
payload in native `metadata_json.operations` while retaining tiny successful tool messages.
