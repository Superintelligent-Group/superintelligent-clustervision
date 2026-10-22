# Search (full-text & semantic)

clustervision indexes every session you've ever run — across every harness — so you
can find that one conversation again. There are two ways to look: **full-text**
search (you remember a word that was said) and **semantic** search (you only
remember what it was *about*). 🔮

- **Full-text** is a real inverted index ([tantivy], Lucene-style): tokenization,
  BM25 ranking, fielded filters, phrase and boolean operators, highlighted
  snippets. Fast and exact — it finds documents that contain your words.
- **Semantic** is meaning-based: every session is embedded into a vector, your
  query is embedded too, and results are ranked by cosine similarity. It finds
  the *right* session even when it shares no words with your query.

Both are exposed through the everyday [`cv`](cli.md) CLI (`cv index`, `cv search`); the
`cv-search` crate is the library behind them.

## Building the index

The index is a cache and the transcripts are the truth: `cv search` answers from the
index plus a bounded live read of whatever the index has not seen (see
[How `cv search` resolves](#how-cv-search-resolves)). Building and refreshing it pre-pays
that read, so build it once and refresh it when convenient; no timer or cron job is needed
for fresh results:

```sh
cv index              # build/refresh the full-text index
cv index --semantic   # also build semantic embeddings
```

```text
✦ building full-text index…
indexed 412 session(s) → /Users/you/.clustervision/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 412 session(s) → /Users/you/.clustervision/embeddings.bin
```

`cv index` discovers every session from every supported harness and indexes the
ones that changed — it is **incremental** by default, re-reading only new or
modified sessions and reaping vanished ones, so routine refreshes are cheap. Pass
`--rebuild` to clear and rebuild from scratch. Add `--semantic` to *also* compute
embeddings; that step downloads a small embedding model (~30 MB) the first time
it runs, then caches it. `--subagents` folds the sub-agent/workflow forest in too
(off by default — it can add hundreds of MB).

A session that cannot be read (its transcript changed under the pass twice, or a
read failed) stays out of the index until a later pass; `cv index` warns with the
count and still exits 0 once the rest is committed. `--json` lists each such
session's id and reason; `--strict` makes any of them exit 1. A persisted tool
output the indexer cannot open (another user's file) only drops that output: the
session is indexed with a note naming it.

### What gets indexed

Each session is indexed from its **entire textual content**, not just metadata:
the title, every message's text and thinking blocks, tool-call names and their JSON
inputs, tool results, and referenced file paths — all stored in searchable chunks and
ranked as one session. Alongside the body, the index stores the session `id`, its
`harness`, the working directory (`cwd`, tokenized so path fragments are searchable),
the `title`, and created/updated timestamps.

The text is kept in two parts. The **exchange** is what the person asked, what the model
replied and ran, what came back, and what sub-agents were handed and returned. The
**context** is what surrounds it: injected instruction files (`CLAUDE.md`, `AGENTS.md`),
system prompts, skill bodies, hook output, harness notices and compaction summaries. The
context is near-identical across hundreds of sessions, so a query matches the exchange by
default and the context only when asked (`--kind context`, or `--kind all` for both). A
turn a fork copied from its source (a Claude `--fork-session` copy) is matched in the
source alone, so one exchange is one hit.

### Where the index lives

Both indexes live under `$CLUSTERVISION_HOME` (default `~/.clustervision`):

| Index            | Path                                  |
| ---------------- | ------------------------------------- |
| Full-text        | `~/.clustervision/tantivy/`            |
| Semantic vectors | `~/.clustervision/embeddings.bin`      |

Set `CLUSTERVISION_HOME` to relocate them.

## Full-text search

```sh
cv search "tantivy bm25"
cv search "kubernetes" --harness claude --limit 50
```

```text
claude    a1f3c2d9  2026-05-21  Rust tantivy index
          … We built a full text search engine with <b>tantivy</b> and BM25 scoring.
```

Bare terms search across the title, the exchange, and cwd (`--kind context` searches the
context instead, `--kind all` both). The query supports tantivy's full syntax:

- **Fielded filters** — `harness:claude foo` restricts to a harness.
- **Phrases** — `"formal verification"` matches the exact phrase.
- **Booleans** — `lean AND proof`, `rust OR zig`.

Terms are **conjunctive by default**: every term must match (better precision
than OR). Results are ranked by BM25, with a highlighted snippet drawn from the
matching region of the body. `--limit` caps the number of rows shown (default
**20**); `--harness` restricts candidates before the top results are selected, in both
full-text and semantic mode.

### How `cv search` resolves

`cv search` answers from the tantivy index **plus the unindexed suffix**: every session the
index does not describe as it is now (grown, rewritten or new since `cv index`) is read live,
newest first, into a private in-memory index and ranked by the same BM25 scorer, with corpus
statistics taken over both. A session that grew is read from where the index stopped (its
resume mark, taken every 4 MiB of indexed text), so it costs only its new bytes; a smaller,
rewritten or new one is read whole, exactly as `cv index` would read it. The read is
bounded by `--live-bytes` (default 256 MiB, about a day of new transcript on a busy machine):
a session that does not fit is left to what the index last saw of it. The footer says what was
read live and what the budget left out:

```text
(live: read 3 session(s) the index has not seen live (2.1 MiB of the 256.0 MiB --live-bytes budget, 2 from where the index stopped))
(note: the index is ~2h behind for 1 session(s) not read live: 1 over the --live-bytes budget (412.0 MiB) — run `cv index`)
```

With no index at all every session is unindexed: the newest are read live within the budget
and the footer counts the rest. Global `--frozen` (or `--live-bytes 0`) answers from the index
alone and says how far behind it is; with no index, `--frozen` is an error naming the index path
and the fix (`cv index`). An index that exists but is unusable is always an error. Nothing is
written by the live read (no index, catalog or mark), so a home the process may not write (a
sandbox's deny-write rule, a read-only copy) is searched, listed and shown from the index and
catalog already there, its suffix included. The same index-plus-suffix answers a `-q text:`
predicate; without an index there it must cover every session, or the query exits 2.

`--scan` asks for the live scan instead, and says so on stderr: case-insensitive substring
matching over only the first 256 KiB of exchange content per session, in discovery order,
with no Tantivy operators and no BM25 ranking. Invalid query syntax is an error, never a
reason to scan. `--kind` requires indexed full-text search; it cannot be combined with `--scan`.

> Older releases also kept a SQLite full-text index as a middle tier; it's
> retired (tantivy is canonical). If a stale `index.sqlite` lingers in your
> clustervision home, `cv search` prints a note that it's safe to delete.

## Semantic / meaning search

Keyword search only finds documents that contain your words. Semantic search
finds documents that *mean* what you asked, even with **zero word overlap**.

> 🔮 Suppose you once spent an afternoon with Lean proving a theorem, but the
> word "formalizing" never appears in that transcript. A full-text search for
> `"formalizing proofs"` finds nothing. A *semantic* search for the same phrase
> ranks that Lean session right at the top — because "formalizing proofs" and
> "proving theorems in Lean" live near each other in meaning-space.

Select semantic ranking on the same command:

```sh
cv search "formalizing proofs" --semantic   # semantic mode of the normal search
```

`cv search --semantic` embeds your query, ranks every stored session vector by
cosine similarity, and prints the same row format as full-text search. It needs
embeddings to exist — run `cv index --semantic` first.

> **No silent degradation.** `cv search --semantic` does **not** quietly fall
> back to keyword ranking when the embeddings store is missing: it errors and
> tells you to run `cv index --semantic`. A result set that says "semantic" is
> always actually semantic.

### From a session row to the material you wanted

`cv search` answers *which* sessions are relevant. When what you actually want is
the **material** — the relevant past spans, compiled into something you can hand
a fresh agent — that's [`cv pack`](pack.md), which runs the same fused full-text
+ semantic ranking and then excerpts each hit:

```sh
cv pack "how did we handle retry backoff"        # a CLAUDE.md-style context bundle
cv pack "auth token refresh" --limit 3
```

`pack` is where the old `cv recall` went in 0.11.0: one verb for
"build context from the corpus." It degrades honestly too — no index means a live
scan with a stderr note, and no embeddings means full-text-only ranking.

## How semantic embeddings work

Semantic search uses [model2vec] *static* embeddings (model
`minishlab/potion-base-8M`, ~30 MB, 256-dim). "Static" means there's no
transformer forward pass and no ONNX runtime: text is tokenized, per-token
vectors are looked up in a distilled table and mean-pooled. It's tiny and
CPU-fast, so clustervision just embeds every session up front and does a
brute-force in-memory cosine scan at query time — no approximate-nearest-neighbor
index needed at a few thousand sessions (one 256-float vector per session).

A session's vector embeds the head of its own exchange, as the full-text index
matches it: never the injected instructions, system prompts or hook output around
it, and never turns a fork copied from its source. `--kind context` and
`--kind all` are full-text only.

The model is fetched from the HuggingFace Hub on first use and cached under
`~/.cache/huggingface`. To pin a local copy and run fully offline, point
`CV_SEARCH_MODEL` at a model directory:

```sh
export CV_SEARCH_MODEL=/path/to/potion-base-8M
cv index --semantic
```

## Quick reference

| Command                          | What it does                                        | Limit flag (default) |
| -------------------------------- | --------------------------------------------------- | -------------------- |
| `cv index`                       | Build/refresh the full-text index                   | —                    |
| `cv index --semantic`            | Also build semantic embeddings                      | —                    |
| `cv search <query>`              | Full-text search (BM25), index plus unindexed suffix | `--limit` (20), `--live-bytes` (256 MiB) |
| `cv search <query> --semantic`   | Semantic search (no keyword fallback)               | `--limit` (20)       |
| `cv pack <task>`                 | Fused ranking → relevant spans, as a context bundle | `--limit` (8)        |

See also: [the CLI](cli.md) for the full command surface, and
[MCP](mcp.md) for exposing search to a running agent.

[tantivy]: https://github.com/quickwit-oss/tantivy
[model2vec]: https://github.com/MinishLab/model2vec
