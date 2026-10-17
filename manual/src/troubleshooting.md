# Troubleshooting & FAQ

### The app shows the sample data, not my real sessions

The desktop app reads your local sessions through a native `api` command and runs a `cv serve` for the Fleet dashboard. If you see
the bundled sample, the app couldn't reach them — make sure you're running the **desktop app** (not
the plain browser build, which is zip-drop only), and that nothing else is occupying its port.

### "Showing 0 sessions" / nothing found

clustervision only sees harnesses installed in their standard locations (see
[Harnesses](harnesses.md)). Run `cv ls` in a terminal — if that's empty too, no supported harness
data was found under `$HOME`.

### First `cv ls` is slow, then fast

Discovery parallelizes across harnesses and caches metadata by file identity (length, mtime,
device, inode, ctime), so the first run does the real work (82 s wall for 7,000 sessions and 95 GB
on a busy host, 2026-10-16) and later runs take 0.1–0.2 s. A huge Codex rollout (hundreds of MB) is
sampled head+tail during discovery, never fully read. See [Architecture](architecture.md).

### `cv port` warns about lost content

`cv port` re-parses its own output and diffs it against the source, message by message, then
reports anything that didn't survive. Each loss is classified: **expected** when the target format
genuinely cannot hold it (LM Studio has no tool structures on disk, so tool calls become readable
text), **unexpected** when it could have and didn't. Only the unexpected ones fail `cv port
--strict`. A `⚠ lost` line about an expected loss is a format limitation, not a bug. See
[Cross-harness conversion](conversion.md).

### Sub-agents

Claude Code's Task tool spawns sub-agents whose transcripts are normally invisible. clustervision
finds them and nests them under the parent session in the app's transcript view (lazy-loaded,
labeled by task prompt). There can be thousands, so they're never dumped into the main list. See
[The app](viewing-and-sharing.md).

### The MCP server isn't responding

`cv mcp` speaks JSON-RPC over **stdio**, and **stdout is the protocol channel** — all diagnostics go
to stderr. Register it with `claude mcp add clustervision -- /abs/path/to/cv mcp`. See [MCP](mcp.md).

### Still stuck?

Open an issue: <https://github.com/emberian/cv/issues>. Weird old transcripts especially
welcome. 🔮
