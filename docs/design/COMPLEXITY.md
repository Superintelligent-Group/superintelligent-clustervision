# Complexity: what each path costs, and the no-mode-switch rule

The 0.18 complexity audit, done by reading the code. Use this page to check a change: find the
row for the path, and make sure its cost after the change is still what the row says.

Variables: s is the session's size, n the number of sessions, S the store's size, k the
sub-agents, e the events, and L the lane-ledger rows. "One change" means one session appended
or rewritten. Paths are relative to `crates/`.

## The rule

1. **Cost is set by the question.** A listing is O(n). `tools --across` is O(messages it
   folds). One change rereads exactly the session that changed, so its cost is O(s) or the
   appended suffix. Nothing is O(n²), O(n·k), O(k²) or O(n·S) unless the question itself is.
   Fork inference over siblings is not inherently n².
2. **Shared state is read once per store state.** Something one file, table or directory
   holds is parsed once per `source::FileId` (or per store state) and then reused. It is never
   reread inside a loop over sessions. See `lanes::read_ledger`, `lanes::ledger_index`,
   `claude::configured_roots` and `fork::head`.
3. **No mode switches.** No cache or fast path may stop applying past N bytes or N items while
   the work silently becomes more expensive. Every threshold falls into one of three classes:
   - **budget**: bounds the work and says so in the output or on stderr. Fine.
   - **safety cap**: bounds memory. Fine only if the fallback is also bounded.
   - **mode switch**: silently changes complexity. Not allowed. Replace it with an
     always-on structure, or with a budget that reports itself.
4. **Gate a fix with a test of its shape**: one change produces one reread, or a counter
   assertion. Byte identity is shown by `tools/dump-hash.sh` base vs branch.

## Cost per path, as landed

| Path | Cold | Warm | One change | Set by |
|---|---|---|---|---|
| `discover_all` | O(S_meta + n) | O(n) stats + scan-cache hits | O(n_h) refresh reads, O(changed rows) writes | `lib.rs` discover; `catalog/order.rs` |
| `sessions()` / `cv ls` | as above | O(n) probe + rows, O(n log n) sort | as above | `catalog.rs` `probe_stale_in` |
| Claude discovery | O(S) | O(n) | O(appended) through the continuation | `claude.rs` `cached_metadata` |
| Codex discovery | O(Σ min(s, 8 MiB)) + zst decode | O(n) | O(min(s, 8 MiB)) | `codex.rs` `scan_rollout` |
| Claude sub-agent forest, any demand | O(Σ s_k) | O(k) stats | O(appended) for the changed agent | `claude.rs` `agents_in_dir` |
| Codex hook forest | O(L) index + that forest's rows | same | same | `codex.rs` `hook_forest`, `lanes::ledger_index` |
| Edit guard, per edit | O(touching sessions × hops) lookups | same | same | `hooks.rs` `lineage_root` |
| `lanes_of` / `cv lanes` | O(the parent's rows + Σ s_k) | same | same | `lanes.rs` `lanes_of` |
| `cv index`, unchanged | — | O(n + dependencies) stats | O(n) plan + one session read | `fts.rs` `PlanState`; `sweep.rs` |
| `cv index` sweep queue | O(Z log Z) for Z groups | — | — | `sweep.rs` `Queue` |
| `cv search` | O(postings) + `limit` snippets | same | + suffix read within `--live-bytes` | `cv-search` `search_suffixed` |
| `stats --tokens` | O(S) | O(n) cache loads + a bounded sweep | one source reparsed | `stats/usage_cache.rs` |
| `cv scry` / watch poll | O(n) stats per tick | O(n) | O(appended) from the anchor | `watch.rs`, `watch/read.rs` |
| `cv watch` archive | O(S) | O(n) baseline checks + 2n stats | O(appended) | `watch.rs` `offer_sources`, `archive/reuse.rs` |
| `events ingest` | O(S) | O(n) | one reparse | `events.rs` `ingest_all` |
| Workflow listing | O(r) state parses + r stats | same | same | `claude_workflow.rs` |
| distill | O(s) | — | — | `distill.rs` (commit set) |
| prune / rewind / emit / dataset | O(s) per session | — | — | linear passes, hashed sets |
| Task log, per command | O(E) replay | O(E) | O(E) | `task/store.rs` (open, below) |

## Open (structural, not yet fixed)

- **Exports roster cache** (`harness/export/native.rs` `CACHE_BYTES` 8 MiB, 8 entries): a mode
  switch. Past it every conversation read re-parses its whole archive, O(n_a·S_a). Owned by the
  open-cliffs lane (the exports offset index), as are that lane's other items.

- **Task log replay**: every command, prompt hook and API request replays all E events.
  E appends cost O(E²) in total. Needs a persisted reducer snapshot plus a suffix fold
  (about 150 lines).
- **API read cache** (`api/read.rs`): limits of 32 entries FIFO, 256 KiB per entry and 2 MiB
  total. Past them a page costs O(start) and a head costs O(s). Needs per-session offset
  certificates in the catalog (about 200 lines).
- **MCP observe cursor**: keeps reader state for only 8 sessions. Every other changed session
  is reparsed on each call. Needs the reader state kept server-side as a `source::Record`
  (about 120 lines).
- **`agent-…` lookups**: rediscover the Claude harness on every call, O(n). Needs an
  agent → parent index (about 100 lines).
- **`show --tree`**: indents by chain depth, so output is O(e²). Fixing it changes output.

## Surviving thresholds

Budgets (they bound work and say so):
- `--live-bytes` (search suffix): sessions over the budget are named in the stale note.
- `LIVE_HAY_BYTES` 256 KiB (`--scan`): stderr says results are head-capped.
- `PIPE_GUARD_*` (show), MCP `SHOW_DEFAULT_*`, `observe_stream` caps: stated in output.
- `source/scope.rs` `RETAINED_BYTES` 16 MiB: per namespace. When a namespace passes it,
  stderr says so once.
- Codex `FULL_SCAN_CAP` 8 MiB: listing metadata for larger rollouts comes from 1 MiB head and
  tail samples. It bounds work and is stated in DATAPATHS. Making it say so per session would
  change output.
- `hooks.rs` `COMPACT_FULL_SCAN_MAX` / `COMPACT_TAIL_BYTES` / `COMMIT_TAIL_BYTES`: the hook text
  names the command to run in full.
- `EMBED_HEAD_BYTES` 64 KiB: by definition, the vector is computed from the session's head.
- `usage_cache_floor` 64 KiB: sources under it are re-streamed. That is bounded at 64 KiB per
  session and is a deliberate CPU trade.

Safety caps (they bound memory, and the fallback is bounded too):
- `IN_FLIGHT_BYTES` / `DECODED_WINDOW` 2 GiB: an oversized job runs alone.
  Compressed jobs are charged their decoded size.
- `DOCUMENT_BYTES`, `COMMIT_EVERY` (drain at 8×): a document is admitted alone.
- `spill/` `MEMORY_BYTES`, `store/bytes.rs` `MEMORY_LIMIT`, `Selection::RETAINED_BYTES`:
  memory spills to disk, with the same complexity.
- `MAP_WINDOW` 8 MiB, `MAPPED_RECORD` 64 KiB, `document/range.rs` `ENDPOINTS`: they cost at
  most one extra read per traversal.
- HTTP `MAX_LINE` / `MAX_HEAD` / `BODY_LIMIT`: a request over the limit is rejected.
- `catalog/reader.rs` Probe reset at 256 refs: one extra stat per 256 refs.

Removed mode switches:
- Usage cache: the FIFO eviction at 256 MiB / 65,536 sources, and the 16 MiB per-source limit.
- Watch: the 64 KiB anchor limit.
- Archive: the 1 MiB resume-state limit.
- The `.zst` whole-guard charge.
- The shared inventory budget.
