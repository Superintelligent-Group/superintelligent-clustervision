# One codec per wire format

"Never materialize" was built as extra decoders beside each adapter's owned one (Claude had
five: owned Value, typed borrowed, positioned "portable", lean, facts), plus parity tests that
those paths agree. The rule now: **a wire format has one decoder, parameterized by demand; a
build differs only in its byte source.** Discovery must share that classification too; remaining
count-only scans are tracked in [`DATAPATHS.md`](DATAPATHS.md).

**Byte source.** `store::jsonl::for_each_line(source, file, from, to, visit)` yields each
physical line once as a `Line { at, end, complete, view }`, with `view: Option<JsonView>` (None
for blank, non-UTF-8 or rejected lines). Mapped builds borrow lines from successive 8 MiB windows (grown for a longer line); positioned
builds (and a failed mapping) read lines up to 64 KiB into one buffer and leave a giant line in
place as a positioned view. That `cfg` is the only one a reader needs; `JsonView` has the same
API with two arms: a source view over `RangeSource` (mapped/borrowed bytes or positioned reads),
and an owned `Value` view (`(&Value).into()`, for strings with no file).
Each line is validated once by `store::json::validate_value` (exactly serde `Value` acceptance,
property-tested against serde). A bounded endpoint index lets cached long strings and containers
be skipped in O(1); an evicted endpoint can require rescanning. Lines are trimmed of JSON whitespace (space, tab, LF, CR) only.

**Decoder.** One function per format, `record(view, demand) -> session facts + Option<Message>`
(Claude: `harness/claude/record.rs`). It selects the fields any record kind reads in one pass
(`Fields::parse`), then resolves only what the demand asks for: `ParseOptions` plus the sink's
`extra_projection`. Content goes through the family's block codec (`wire::anthropic::
block_from_view_with`) with the shared text rule `JsonView::lazy_text(spans)`: decoded length
over `INLINE_MAX` stays in the source: primary transcript text uses `Span` in both builds;
independent files, cells and decoded backing use checked `Text::Source`. Large tool arguments
become `Json::Source`. Lean, facts, usage, rich and
complete are which fields get resolved, never which code runs. Discovery, fork heads and
prefix counts are demands on the same reader. The driver rechecks source identity after a
record that published a source descriptor and at completion; the identity is
`source::FileId`, and a `.jsonl` that grew under the read is admitted by content
(`FileId::admits`: same inode, at least as long, sampled prefix unchanged), with the trailing
partial line held back. Every span a reader hands out resolves through one checked resolver in
`lazy`, so a replaced or shrunk file fails the first content read instead of returning the
replacement's bytes. Claude checks the opened descriptor after each record's message/checkpoint
callback group before mapped iteration resumes. Codex checks its message, metadata and checkpoint
callbacks, including held EOF output. Grok guards pending/current output and checkpoint groups
before advancing the framer, and rechecks held reasoning after EOF delivery. Kimi guards each
message/checkpoint boundary and validates final pending output against its own segment, plus
admitted wire enrichment. Completion rechecks the pathname. These guards reject
synchronous callback truncation; they do not make mappings immutable snapshots against arbitrary
concurrent external writes or truncation.

**Heap budgets by construction.** Materialization follows demand, while validation can still
traverse unselected bytes. A reducer retains the fields it reads; a giant line is never copied on positioned builds; spans and source arguments
defer payloads to the consumer. The CI heap gate (`check-heap-budget.py`, both builds) and the
operation gates stay the proof. Still owned, and so still output-sized: complete-mode carriers
and metadata, rich `toolUseResult` sidecars, array tool results under 64 KiB (coerced inline).

**Tests.** Per format: the corpus golden summaries (frozen per harness version) and the heap
gate, plus behavior tests where the rule lives (classification, checkpoints and resume,
source-change rejection, offset stamps). Shared primitives (`validate_value`, the streaming
classifiers) are tested against simple oracles. No test compares two decoders: there are none.
Before deleting an old path, compare hashes of canonical outputs, resolved payloads, checkpoints
and session metadata against an independent baseline over recorded sessions in each affected
demand. Stream hashes rather than retaining transcript dumps; diagnostic tools stay uncommitted.
The byte-identity gate is `tools/dump-hash.sh --cv BIN --probe store_probe [--probe-positioned
store_probe] --scratch DIR [--home DIR]`: over `tools/dump-hash.manifest` (every corpus version
staged as a fake HOME, four synthetic Claude edges, any external home of real sessions) it prints
one sorted `sha256  command  session  demand` line per `ls`/`show`/`export`/`search`/`events`/
`tools`/`stats`/`doctor`/`pack` form, per reshape output and sidecar, and per `store_probe hash`
demand in both builds. Two runs of one binary print equal sets; a commit that changes output
pastes the lines it moved into its message. Results are never committed.

**Byte sources by family.** JSONL (Claude, the OpenAI family, Gemini's jsonl era): `for_each_line`.
JSON documents (Gemini's document era, cline, roo, cline_sdk, lmstudio, opensession, the exports):
`JsonSource::read_document`/`read_archive`, with an owned view (`(&Value).into()`) for string entry
points. SQLite stores (opencode, cursor, devin, goose, zed, openclaw, hermes): `store::sqlite::Cells`,
each cell copied once into the source's arena, validated once, then viewed. `codex`, `cursor` and
`goose` `legacy` modules are readers of older formats, not oracles. Test-only legacy emitters are
retired to frozen goldens (`tests/fixtures/emit-frozen/`, `Frozen` in `emit/grok_qwen_tests.rs`).
A change whose output differs must explain each difference in its commit.

**Writing shared wire formats.** Anthropic blocks use one grammar with explicit leaf publication:
checked streams resolve source text; the public owned API preserves Span descriptors and resolves
Source text. Only callers requesting an owned Value buffer that value. Qwen and Gemini share
GenAI part selection and text/media fields, preserving their native malformed-field qualification;
consumer-specific tool IDs, wrappers, signatures and demand remain in the adapters. Independent
frozen outputs check both contracts without retaining a second production codec.

**Store selections.** `store::sqlite::read` brackets an operation with the existing descriptor
and native read-set checks. Private `Selection` databases hold derived membership, never write
the native store, and belong to its operation inventory. OpenClaw promotes from native SQL
after another query cache miss; Hermes indexes normalized digests and confirms collisions by
exact source comparison. Native ordering, affinities and collations remain authoritative.
Portable `selection::Positions` indexes Qwen task times and export IDs without copying payloads.
Goose retains its already indexed scalar family selection. Page-cache settings and inventory
admission charges do not bound individual SQLite cells or demanded candidate vectors.
