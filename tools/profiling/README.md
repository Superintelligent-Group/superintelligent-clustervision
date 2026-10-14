# Profiling tools

Method only. Recorded results and the per-change evidence files that used to live under
`results/` are in git history; current numbers are in `docs/design/PERF.md`. Every script
creates its own new scratch directory and never touches real stores.

## Instruction-count control

`cachegrind.py` runs an existing `store_probe` binary natively, then twice under
Cachegrind, with a new scratch catalog before every invocation. It compares the
complete JSON outputs except the probe's `wall_seconds` instrumentation field,
records `Ir` (executed instructions), and emits function attribution reports.
No timings are compared. `RAYON_NUM_THREADS` is pinned to one by default;
`--threads N` changes and records it. The output directory must not exist.

Use Linux ARM64 on Apple Silicon, explicitly selecting the Docker platform.
Task-owned container and volume names below are examples; choose unique names.
The checkout is read-only and all compilation, inputs and catalogs live in the
new volume. No harness credentials or real stores are mounted.

```sh
docker run -d --platform linux/arm64 --name cv-cachegrind-control \
  --cpus=3 --memory=8g -v "$PWD:/work:ro" \
  -v cv-cachegrind-control:/scratch debian:trixie-slim sleep infinity
docker exec cv-cachegrind-control sh -c '
  apt-get update -qq &&
  apt-get install -y -qq curl ca-certificates build-essential python3 valgrind &&
  curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs |
    RUSTUP_HOME=/scratch/rustup CARGO_HOME=/scratch/cargo sh -s -- \
    -y --profile minimal --default-toolchain 1.99.0'
docker exec -e RUSTUP_HOME=/scratch/rustup -e CARGO_HOME=/scratch/cargo \
  cv-cachegrind-control sh -c 'cd /work &&
    CARGO_TARGET_DIR=/scratch/target CARGO_PROFILE_RELEASE_DEBUG=1 \
    CARGO_BUILD_JOBS=2 CLUSTERVISION_HOME=/scratch/build-home \
    /scratch/cargo/bin/cargo build -p clustervision-core --release --example store_probe'
docker exec cv-cachegrind-control python3 /work/tools/perf-fixture.py \
  claude /scratch/fixture --payload-bytes 65536 --records 16
docker exec cv-cachegrind-control python3 /work/tools/profiling/cachegrind.py \
  --input /scratch/fixture/projects/-perf/00000000-0000-4000-8000-000000000001.jsonl \
  /scratch/profile -- /scratch/target/release/examples/store_probe \
  count claude 00000000-0000-4000-8000-000000000001 \
  /scratch/fixture/projects/-perf/00000000-0000-4000-8000-000000000001.jsonl \
  --root /scratch/fixture --mode lazy
```

Default core features retain mmap, SQLite and parallel support. The recipe sets
no `RUSTFLAGS`; record any flags used at build time separately from the runner
environment (the binary cannot recover its build flags). `count` avoids
hashing the transcript as part of the profiled workload. For content-fidelity
checks, run a separate native `hash` operation with `--hash-detail`; count parity
alone proves only this operation's output. Archive the compiler version, image
identity, features, binary and input SHA256 alongside `summary.json`.

The [Cachegrind manual](https://valgrind.org/docs/manual/cg-manual.html) documents
instruction counts and attribution. Both cache and branch simulations are
explicitly disabled. These are ARM64 Linux guest instruction counts, not native
macOS instructions, Apple Silicon cache behavior or elapsed-time predictions.
The total includes process startup, dynamic loader, parsing, catalog work and
output serialization. Attribution helps identify which work a change removes.

Instructions can still vary because allocator addresses, ASLR, hash seeds,
filesystem/catalog state, syscall outcomes and threads change control flow.
Keep input, binary, command, path lengths and initial catalog state fixed; report
the observed difference rather than declaring the metric perfectly deterministic.

## Whole CLI controls

`--output bytes` compares stdout and stderr byte for byte against the native
child. `--harness-home` supplies an isolated synthetic HOME/XDG environment;
`--seed-catalog` restores a frozen catalog/index before each child. Fixtures and
seeds are hashed before and after the three invocations to reject input mutation.
A directory hash covers sorted relative filenames, lengths and bytes, without
following symlinks. Setup/generation/copying/hashing are outside the profiled child.

Index provenance includes its canonical path (`fts:<index path>`). Relocating a
seed causes legitimate reindexing. Use `--catalog-home` to restore the original
canonical path; it must initially be absent and becomes runner-owned. Prepare the
seed at that path, checkpoint SQLite, then move it aside before profiling. Do not
rewrite the seed's provenance strings to pretend a relocated index is unchanged.
`--expect-stderr TEXT` can require operation diagnostics in each child.

Example after building the actual CLI in the same exclusive Linux target:

```sh
# In the container; setup is outside Cachegrind.
python3 /scratch/cli-work/tools/profiling/cli-fixture.py /scratch/cli-fixture
export HOME=/scratch/cli-fixture/home RAYON_NUM_THREADS=1
export XDG_CONFIG_HOME=/scratch/seed-config XDG_DATA_HOME=/scratch/seed-data
export XDG_CACHE_HOME=/scratch/seed-cache
CV=/scratch/target/release/cv
RUNNER=/scratch/cli-work/tools/profiling/cachegrind.py
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  /scratch/cli-ls-cold -- "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/cli-seed "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/cli-seed "$CV" index
# Checkpoint all seed *.db files with sqlite3's PRAGMA wal_checkpoint(TRUNCATE).
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  --seed-catalog /scratch/cli-seed /scratch/cli-ls-warm -- "$CV" ls --json --limit 20
CLUSTERVISION_HOME=/scratch/profiled-index-catalog "$CV" index
CLUSTERVISION_HOME=/scratch/profiled-index-catalog "$CV" index
# Require the second setup pass to say 0 changed/new, 8 unchanged, 0 errors;
# checkpoint SQLite, then move this task-owned directory to /scratch/index-seed-fixed.
python3 "$RUNNER" --input "$HOME" --harness-home "$HOME" --output bytes \
  --seed-catalog /scratch/index-seed-fixed --catalog-home /scratch/profiled-index-catalog \
  --expect-stderr '0 changed/new' --expect-stderr '8 unchanged, 0 errors' \
  /scratch/cli-index-fixed -- "$CV" index
```

The warm seed's full-sync stamp must remain within the configured freshness
backstop (default 900 seconds); confirm that the intended warm route was measured.
For before/after controls retain the exact input inodes, seed bytes, canonical
catalog/index paths, auxiliary paths, commands and environment. Preserve old
reports before reusing their paths. Count only child instructions, not setup.

`compare.py BEFORE.json AFTER.json` rejects mismatched runtime controls, source
state and output. When both summaries carry `build` metadata, it also compares
compiler/LLVM/profile/features/flags/image/resources while allowing source commit
and binary hash to differ. Raw runner summaries cannot recover their build flags:
when build metadata is absent the helper explicitly reports that it was not
verified.

## First-use scoped discovery cache allocations

`discovery-cache.py --before PROBE --after PROBE --output NEW_DIRECTORY` compares production
`cv_core::sessions()` using two `store_probe` binaries with `--features probe-heap`. It creates
one synthetic Claude inventory, warms its persisted catalog during setup, then appends a user
record. Every measured child starts with an empty process-local cache and the same restored
catalog path and original transcript identity. Adding 4,096 unrelated serialized cache rows
changes no affected inventory. Setup and result reporting are outside the measured child stage;
elapsed-time fields are discarded. The listing must remain one exact session reference with message count two.

## Existing index compaction geometry

`compaction-existing.py` clones a stable index using macOS APFS copies, verifies its source
inventory before and after copying, then runs the ignored
`fts::tests::existing_index_compaction_work_control` in a compiled cv-search test executable.
Pass `--index`, a new `--scratch` directory and `--test-binary`; the binary can be obtained
with `cargo test -p cv-search --lib existing_index_compaction_work_control --no-run` using
the usual isolated target and scratch `CLUSTERVISION_HOME`. The source is never opened for
index writing. A clone marker is required before the test will mutate anything.

Sixteen direct tiny document commits use the production writer and byte-tier selector. The
report records logical merge input/output lengths, retained large segments, query ID-multiset
parity, source identities and binary provenance. The test contains assertions for preserved
queries and append terms; it records rather than assumes large-segment retention, since a
different input can legitimately need deletion cleanup or higher-tier merges.

## Verified Claude port ownership

`claude_port_probe` measures the actual `parse_for_emit` → `emit_report` composition,
including resumability verification and fidelity reparse. Build with `--features probe-heap`
and default core features; stderr is Rust allocation counters, stdout is the result/fidelity.
Measure with a debug build and one Rayon worker. It excludes native allocations/mmap/stacks and makes no CPU or RSS claim.

```sh
python3 tools/profiling/claude-port-fixture.py /owned/new-source.jsonl
CARGO_TARGET_DIR=/owned/target CLUSTERVISION_HOME=/owned/build-home CARGO_BUILD_JOBS=2 \
  cargo build -p clustervision-core --example claude_port_probe --features probe-heap
CLUSTERVISION_HOME=/owned/new-catalog RAYON_NUM_THREADS=1 \
  /owned/target/debug/examples/claude_port_probe codex /owned/new-source.jsonl \
  /owned/new-output native
```

Use fresh scratch paths and repeat with `text`/`drop` for mode controls. Fixture generation
is outside the measured child and produces synthetic data, not a genuine corpus capture.
`--characters` counts Unicode characters; record actual source/decoded UTF-8 byte sizes.

## Other controls

- `distill-command.py`: whole `cv distill` allocation/output controls in isolated Rust test
  processes, baseline against candidate.
- `window-extra.py`: whole `cv show` extra-demand control; `tools/check-operation-heap.py` uses
  it in CI.
