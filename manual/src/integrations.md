# Hooking a harness in

clustervision reads every harness's own on-disk store, so nothing has to be wired for it to
*see* your sessions. Two small hooks and one MCP registration make a harness *use* it while it
runs. This page lists only what works today, in the one harness we have checked against its
documentation and binary (Claude Code 2.1.x). A `cv hooks install <harness>` / `cv hooks remove`
command that writes these entries idempotently is planned; until it lands, paste them by hand.

## Claude Code

Hooks live under the top-level `hooks` key of `~/.claude/settings.json` (every project) or
`<project>/.claude/settings.json` (one project). Each hook is a shell command; it reads a JSON
object on stdin (`session_id`, `transcript_path`, `cwd`, `hook_event_name`, …) and, for
`SessionStart`, whatever it prints to stdout is injected into the session as context.
`$CLAUDE_PROJECT_DIR` is exported to every hook. Use absolute paths to the binaries.

```json
{
  "hooks": {
    "SessionStart": [
      { "matcher": "startup|resume|clear",
        "hooks": [ { "type": "command",
                     "command": "/abs/path/to/cv ls --cwd \"$CLAUDE_PROJECT_DIR\" --limit 8 2>/dev/null || true" } ] }
    ],
    "SessionEnd": [
      { "hooks": [ { "type": "command",
                     "command": "/abs/path/to/cvd sync >/dev/null 2>&1 || true" } ] }
    ]
  }
}
```

- **`SessionStart` → `cv ls --cwd`.** The agent starts with the list of this project's prior
  sessions, every harness, newest first, so it can `show` the relevant one. `--limit 8` keeps the
  injected text to a few lines. (See [the CLI](cli.md#cv-ls).)
- **`SessionEnd` → `cvd sync`.** Every harness's sessions are snapshotted into the archive under
  `~/.clustervision` when a session ends. It is idempotent and exits quickly when nothing changed.
  (See [the daemon](daemon.md#cvd-sync--snapshot-everything-once).)

Both commands are `|| true`-guarded so a missing binary never blocks the harness.

### The MCP server

```sh
claude mcp add clustervision -- /abs/path/to/cv-mcp
```

`cv-mcp` finds `cv` next to itself, then on `$PATH` (`$CV_BIN` overrides), and generates one tool
per CLI command from `cv schema --commands --json`: `ls`, `show`, `cat`, `search`, `pack`,
`doctor`, `lanes`, `prune`, `rewind`, `distill`, … plus the hand-written `await_omen`,
`observe_stream`, `task_*` and `board_*` tools. The roster, with arguments, is in
[MCP](mcp.md); the live `tools/list` reply is the authority.

A typical first move for an agent that has the server: `ls(cwd=<project>)` to see what happened
here before, `search(query=…)` or `pack(task=…)` to pull prior work into context, `doctor(id=<own
session>)` when its own context keeps filling.

## Every other harness

Nothing to install. `cvd watch` follows every harness's store and archives sessions as they change;
`cv ls`, `cv search` and the MCP tools read the stores directly. Harnesses with their own hook or
plugin systems (Codex `~/.codex/hooks.json`, Gemini CLI extensions, OpenCode plugins, Hermes
plugins, OpenClaw hooks) can run the same two commands at the same two moments; clustervision
ships no configuration for them until `cv hooks install` does it from one tested source.
