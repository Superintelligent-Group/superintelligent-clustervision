# Roo Code corpus generator

```sh
tools/harness-corpus/roo/install.sh 0.1.17
tools/harness-corpus/roo/generate.sh 0.1.17 crates/cv-core/tests/corpus/roo/0.1.17
CV_REGEN_CORPUS=1 cargo test -p clustervision-core --test corpus
```

**Needs:** gh (to download the release), npm, node >= 20, python3, git, and `~/.zai-key`.

**The writer.** The Roo Code VS Code extension is not drivable headless, but the Roo Code CLI is
(GitHub releases `cli-v<version>` of RooCodeInc/Roo-Code; it is not on npm). The CLI bundles the
extension and runs it in a mock VS Code host whose globalStorage is `~/.vscode-mock/global-storage`.
So the tasks are the extension's own `tasks/<uuid>/` files, written by the extension's own code.
`install.sh` does what upstream's `apps/cli/install.sh` does: download, check the sha256, run
`npm install --production`, restore the bundled ripgrep. It skips the `~/.local/bin` symlink. ember
has no Roo installed, so 0.1.17, the latest CLI release (2026-03-04), is the only version.

**The model path.** The CLI's `anthropic` provider takes no base URL, but the Anthropic SDK inside it
reads `ANTHROPIC_BASE_URL`. That variable points at `key_proxy.py`, which adds the key and forwards
to Z.ai's Anthropic-compatible endpoint. The proxy rewrites the model to `glm-5.3-flash`; Roo is told
`claude-sonnet-4-5`. Z.ai's stream reports 0 input tokens, so Roo's own token estimate stays near zero.

**Driving it.** `-p` takes one prompt and refuses a prompt together with `--session-id`. So every
task runs through the CLI's NDJSON stdin protocol (`--stdin-prompt-stream`), driven by `stream.py`:
a `start` command, then one `message` per later turn, each answering the completion ask so the same
task continues, then `shutdown`. A `start` may carry extension settings as `configuration`. That is
how checkpoints (`enableCheckpoints`) and auto-condense (`autoCondenseContext`) get turned on.

## Scenarios

| scenario | how |
|---|---|
| chat, resume | one task, three turns |
| read/edit/shell, parallel, tool error, checkpoints | one task with checkpoints on: two reads, `apply_diff` and `execute_command`, then a missing file |
| image | a data-URL image on the `start` command |
| todo list | `update_todo_list` twice |
| sub-agent | `new_task` in ask mode: a child task dir whose `history_item.json` names its `parentTaskId` |
| mode switch | a task started in architect mode that calls `switch_mode` |
| follow-up question | `ask_followup_question`, answered by the next `message` |
| MCP | `mcp--corpus--add` (`settings/mcp_settings.json` with `alwaysAllow`) |
| compaction | auto-condense at the 5% minimum, tripped by a padded second message (Roo counts the last message's tokens itself) |
| reasoning | every turn (GLM thinking → Roo `reasoning` blocks) |

**What is copied:** `tasks/` (each task's `api_conversation_history.json`, `ui_messages.json`,
`history_item.json`, `task_metadata.json`, and the `_index.json`). Each task's `checkpoints/` (a
shadow git repository of the workspace) is left out. Also not copied: `global-state.json`,
`secrets.json` (it holds the dummy key), `settings/`, `cache/` and the machine id.
