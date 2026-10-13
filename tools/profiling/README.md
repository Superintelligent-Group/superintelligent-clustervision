# Instruction-count control

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

## Initial control (2026-10-12)

Source `26867544`, Debian trixie ARM64 image manifest
`sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f`;
container verified `aarch64`, 3 CPUs and 8 GiB memory. Rust 1.99.0
`b940084d7eb6a299eb4bfeb8e34901bc051e7ac4`, LLVM 23.1.1, Valgrind 3.24.0-3,
glibc 2.41-12+deb13u4 and GCC 14.2.0. Build: opt-level 3, ThinLTO,
debuginfo 1, default core features, no RUSTFLAGS. Runtime: one Rayon worker.

The deterministic synthetic history fixture has 16 records, 65,536 decoded
payload bytes, 76,259 physical bytes and 48 emitted messages. Its SHA256 and
the binary SHA256 are in [the result](results/claude-count-arm64.json).
Instruction-only runs counted **1,871,091** and **1,870,995** instructions:
96 fewer in the second run, **0.0051%** variation. Both complete count outputs
matched the native output after removing only `wall_seconds`. This profiles one
parser/count operation, not the entire `cv` CLI command boundary or fleet query.
A separate unprofiled native content hash oracle produced message SHA256
`11300364c11606b79e254d84c0b8d230047620977a6f67d20bcce9145378cfdb`;
it is not included in instruction counts.

Top function attribution in run one (inclusive of inlined source locations):

| Function | Ir | Share |
|---|---:|---:|
| serde_json SliceRead::skip_to_escape | 407,540 | 21.8% |
| dynamic loader relocation | 168,905 | 9.0% |
| core::str::from_utf8 | 119,081 | 6.4% |
| store::json::valid_unicode_escapes | 102,363 | 5.5% |
| store::json::value_compatible | 91,768 | 4.9% |

The nonzero run diff is in hashbrown/indexmap table entry/control and comparison
functions, consistent with randomized hash seeds. Parser attribution above is
unchanged between the runs. This is a useful low-variance work metric, not an
assertion that all process-level instruction totals are deterministic.

Raw Cachegrind files and both attribution reports are retained in the task-owned
Docker volume at `/scratch/profile-pinned`; compiler/package evidence is at
`/scratch/compiler.txt` and `/scratch/packages.txt`. The container is stopped
after capture; restarting it preserves the exclusive Linux target and results.
