# Continue corpus generator

```sh
tools/harness-corpus/continue/install.sh 1.5.47
tools/harness-corpus/continue/generate.sh 1.5.47 crates/cv-core/tests/corpus/continue/1.5.47
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** npm, python3, git, and `~/.zai-key` (override with `ZAI_KEY_FILE`).

**The writer.** Continue's VS Code and JetBrains extensions cannot run headless. The cn CLI (npm
`@continuedev/cli`) can, with `cn -p`, and it writes the same store the extensions write:
`$CONTINUE_GLOBAL_DIR/sessions/{sessions.json,<id>.json}`. The generator points `CONTINUE_GLOBAL_DIR`
and `HOME` at a throwaway dir. ember has no Continue installed, so there is no "installed version" to
capture first; 1.5.47 is the latest release.

**The model path.** `config.yaml` names an `openai` provider whose `apiBase` is `key_proxy.py` on a
free local port, with a dummy key. The proxy adds the Z.ai key and forwards to the coding endpoint
(`glm-5.3-flash`). A second model with the `subagent` role is defined because the Subagent tool only
offers such models.

## Scenarios

| scenario | how |
|---|---|
| chat, resume | `cn -p` once, then `cn -p --resume` twice |
| read/edit/shell, parallel calls, tool error | one session: two parallel reads, an Edit plus `cat`, then `cat` of a missing file |
| MCP | the `add` tool of `../codex/mcp_server.py`, configured under `mcpServers` |
| sub-agent | `--beta-subagent-tool`. In `-p` mode cn 1.5.47 fails the call ("No active session found"). The call and its error persist. |
| fork | `--fork <id>` copies a session into a new one and continues it |
| plan mode, checklist | `--readonly` (it refuses `--auto`) and the Checklist tool |
| web fetch | the Fetch tool on example.com |

**Not covered, and why** (also in `MANIFEST.toml`):

- Image input: `cn -p` has no attachment flag.
- Reasoning: cn does not persist GLM's `reasoning_content`.
- Compaction: `cn -p /compact` compacts in memory and exits without saving. A small `contextLength`
  made cn drop the truncated reply instead of auto-compacting.

**What is copied:** `sessions/` (each session file and the `sessions.json` index). `config.yaml`,
`permissions.yaml` and `logs/` are not copied.
