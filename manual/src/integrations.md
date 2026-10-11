# Hooking a harness in

clustervision reads every harness's own on-disk store, so nothing has to be wired for it to
*see* your sessions. Hooks make it *act* inside a running session: a compaction becomes visible
and reversible from the model's own context, a new session starts knowing what happened in this
directory before, lanes land in a ledger with no model action, owed decisions surface when you
type, and an edit to a file another live session is working on asks first.

One command sets it up, and an agent can run it for itself:

```sh
cv hooks install claude                 # ~/.claude/settings.json (or $CLAUDE_CONFIG_DIR)
cv hooks install claude --scope project # ./.claude/settings.json
cv hooks show claude                    # what install would change; --json for the before/after
cv hooks remove claude                  # exactly cv's entries and the skill, nothing else
```

`install` is idempotent: a second run writes nothing. It merges six entries (five hooks; `lane`
serves both sub-agent events) into the settings file's `hooks` key, beside whatever is already
there, and copies the [`cv` skill](#the-skill) to `skills/cv/SKILL.md` under the same `.claude`
dir. Each entry's command is this `cv` binary's absolute path plus `hook <name>`. That shape is how
`remove` and a reinstall recognize cv's entries, so a reinstall from a moved binary replaces
them instead of adding more. A settings file that is not valid JSON is left untouched and the
command errors. The hooks take effect in new sessions. `cv doctor` prints whether they are
installed and which compaction policy is in force.

Claude Code (checked against 2.1.296 and its [hooks reference](https://code.claude.com/docs/en/hooks))
is the one harness with a verified contract. `cv hooks install codex` (or any other name) errors
rather than writing a guess.

## The five hooks

Each `cv hook <name>` reads the hook's JSON on stdin and prints one line of hook JSON, or nothing.
Every hook exits 0, so a hook can never block the harness: bad input or an unreadable store goes
to stderr. Each one reads the catalog, the event index or one transcript's tail. None of them
ever scans the fleet, and each finishes in 10–30 ms on a real store (one compaction case is
slower; measurements below).

| event · matcher | command | prints |
|---|---|---|
| `SessionStart` · `compact` | `cv hook compact` | `additionalContext`: `context compacted (auto, 180,412 tokens) at message N — compaction #k`, then `cv show <id> --pre-compaction k`, `cv prompts <id> --pre-compaction k` and `cv rewind <id> --at N-1` |
| `SessionStart` · `startup\|resume` | `cv hook start` | `additionalContext`: up to six of this directory's earlier sessions (id, age, messages, title, the last commit sha its transcript's tail shows) and the count of decisions owed; capped at 4,000 characters |
| `SubagentStart`, `SubagentStop` (`async`) | `cv hook lane` | nothing; appends `{ts, event, session_id, agent_id, agent_type, transcript_path, agent_transcript_path, cwd, last_text}` to `$CLUSTERVISION_HOME/lanes.jsonl` |
| `UserPromptSubmit` | `cv hook decisions` | `additionalContext` only when `cv task inbox` has decisions owed to `$CV_ENDPOINT` (else `cv`): the count and the commands that answer them |
| `PreToolUse` · `Edit\|Write` | `cv hook edit` | `permissionDecision: "ask"` with the session id and minutes since, when another session edited this exact path and its transcript changed in the last 30 minutes; nothing (allow) otherwise |

Details that matter:

- **compact** streams the transcript the way `cv compaction` does, giving the exact message
  index and ordinal, for transcripts up to 8 MiB. Above that it reads only the last 4 MiB for the
  newest boundary's trigger and size, and points at `cv compaction <id>` for the index. The
  pre-compaction records never leave the jsonl: [`cv show --pre-compaction`](cli.md#cv-show)
  reads them and [`cv rewind --at`](cli.md#cv-rewind) resumes from before them.
- **start** reads the catalog as it stands, without a freshness probe. A session newer than the
  last `cv ls` / `cvd sync` is missing until one runs. The commit sha comes from the last
  `[branch sha]` line in each transcript's final 512 KiB.
- **lane** writes the ledger [`cv lanes`](swarm.md) reads. A `stop` row at or after a lane's last
  turn makes it read `stopped` with `status_source: hook_ledger`, even before the transcript
  records the `SubagentStop` attachment.
- **edit** reads the event index (`cv touched`'s table) by exact path. Sessions it has not
  ingested (`cv index`, `cv events <id>`) are invisible to it. "Live" means the transcript's mtime
  falls within the last 30 minutes. The hook skips the calling session and, inside a sub-agent,
  its own agent id.

Measured on the maintainer's store (6,704 sessions, 2.7M events, 37 GB of Claude transcripts),
release build, five runs each: `start` 20–30 ms, `decisions` 10 ms, `edit` 10–30 ms, `lane`
under 10 ms, `compact` 10 ms on a 1 MB transcript, 70–90 ms at 16.5 MB (why the full scan stops
at 8 MiB), and 10 ms on the tail path at 190 MB.

## The skill

`skills/cv/SKILL.md` in the repository, which `install` copies into place: when `rewind`,
`show --pre-compaction`, `distill`, `fork`, `adopt` and `lanes` apply, and the table above. The
copy is byte-for-byte what this `cv` binary embeds, so `cv hooks show` marks a stale copy and
`install` rewrites it.

## The compaction policy

`cv doctor` prints a `Policy:` line built from the user and project settings (project wins) and
the environment, which wins over a settings `env` block: `autoCompactEnabled` (default on),
`idleCompaction` (default off; when the server enables it, it fires at 90% of a 200K-token floor),
`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and `DISABLE_AUTO_COMPACT`.
No hook can supply or replace the compaction summary (`PreCompact` can only block it), so the
`compact` hook makes the loss visible and reversible rather than preventing it.

## The MCP server

```sh
claude mcp add clustervision -- /abs/path/to/cv-mcp
```

`cv-mcp` finds `cv` next to itself, then on `$PATH` (`$CV_BIN` overrides), and generates one tool
per CLI command from `cv schema --commands --json`, plus the hand-written `await_omen`,
`observe_stream`, `task_*` and `board_*` tools. The roster, with arguments, is in
[MCP](mcp.md); the live `tools/list` reply is the authority.

## Every other harness

Nothing to install for reading: `cvd watch` follows every harness's store, and `cv ls`, `cv
search` and the MCP tools read the stores directly. cv ships no hook configuration for any other
harness until one has been checked against that harness's own documentation the way Claude Code's
was.
