#!/usr/bin/env bash
# Generate the Roo Code corpus for one installed Roo CLI version: tasks the bundled Roo Code
# extension wrote itself (tasks/<id>/api_conversation_history.json, ui_messages.json, ...), in a
# throwaway home, driven by Z.ai GLM through key_proxy.py on Z.ai's Anthropic-compatible endpoint.
#
#   tools/harness-corpus/roo/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# The CLI hosts the extension in a mock VS Code whose globalStorage is ~/.vscode-mock/global-storage,
# so the tasks land in ~/.vscode-mock/global-storage/tasks/ with the extension's own layout.
# Needs: install.sh <version> first, python3, git, ~/.zai-key. Env: ZAI_KEY_FILE, CORPUS_MODEL
# (default glm-5.3-flash), MAX_CALLS (default 60).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"
. "$HERE/../drive.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools image todo subtask mode ask mcp condense"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="$(ls "${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/roo/$V"/roo-cli-*/bin/roo 2>/dev/null | head -1)"
[ -x "$BIN" ] || { echo "roo $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"
export MAX_CALLS="${MAX_CALLS:-60}"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME ANTHROPIC_API_KEY ANTHROPIC_BASE_URL \
  OPENAI_API_KEY OPENROUTER_API_KEY ROO_API_KEY ROO_CLI_ROOT ROO_EXTENSION_PATH
corpus_guard HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
STORE="$CORPUS_HOME/.vscode-mock/global-storage"
corpus_project

# Roo's anthropic provider takes no base URL from the CLI, but the Anthropic SDK reads
# ANTHROPIC_BASE_URL: point it at the proxy, which adds the real key for Z.ai.
corpus_proxy "https://api.z.ai/api/anthropic" x-api-key "$MODEL"
export ANTHROPIC_BASE_URL="$PROXY_URL"

# MCP: the extension reads globalStorage/settings/mcp_settings.json.
mkdir -p "$STORE/settings"
cat >"$STORE/settings/mcp_settings.json" <<JSON
{"mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"], "alwaysAllow": ["add"]}}}
JSON

cd "$PROJECT"
# One roo process per task, driven through its NDJSON stdin protocol by stream.py: turn 1 is a
# `start` (which may carry extension settings as `configuration`), later turns are `message`s that
# continue the same task. Each turn is one JSON line on stream.py's stdin.
run() {
  cat >"$WORK/turns.jsonl"; echo "--- roo task: $(head -1 "$WORK/turns.jsonl" | cut -c1-110)"
  python3 -I "$HERE/stream.py" --log "$WORK/stream.log" -- "$BIN" -p --output-format stream-json \
    --stdin-prompt-stream --exit-on-error --provider anthropic -k corpus-local-proxy -m claude-sonnet-4-5 \
    -w "$PROJECT" "$@" <"$WORK/turns.jsonl" || echo "    (exit $?)"
}
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=("reasoning")
SKIP=()
# Checkpoints are off by default in the CLI; the start command turns them on for every task.
CKPT='"configuration": {"enableCheckpoints": true}'

if want chat; then   # 1 multi-turn chat + 9 resume (a message continues the completed task)
  run <<JSON
{"prompt": "Say hello in five words.", $CKPT}
{"prompt": "Now say it in French."}
{"prompt": "Which language did you use first? One word."}
JSON
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, checkpoints after edits
  run <<JSON
{"prompt": "Read a.txt and b.txt at the same time (two parallel tool calls), then reply with their combined line count.", $CKPT}
{"prompt": "Replace TODO with DONE in notes.txt, then run the command: cat notes.txt"}
{"prompt": "Run the command: cat missing-file.txt   and tell me the error in one line."}
JSON
  SCEN+=("tools" "parallel_tools" "tool_error" "checkpoints")
fi
if want image; then  # 6 image input (a data URL on the start command)
  img="data:image/png;base64,$(base64 <"$PROJECT/dot.png" | tr -d '\n')"
  run <<JSON
{"prompt": "An image is attached. Do not use any tools except attempt_completion. Reply with the single word: received.", "images": ["$img"]}
JSON
  SCEN+=("image")
fi
if want todo; then   # update_todo_list, Roo's plan/todo tracker
  run <<JSON
{"prompt": "Use the update_todo_list tool to record a two-item todo list for renaming b.txt, then update it to mark the first item done, then finish without editing files."}
JSON
  SCEN+=("todo_list")
fi
if want subtask; then # 7 sub-agents: new_task starts a child task (its own task dir, parentTaskId)
  run <<JSON
{"prompt": "Use the new_task tool to start a subtask in ask mode with the message 'reply with the word ok'. When it returns, report what it said."}
JSON
  SCEN+=("subagent")
fi
if want mode; then   # switch_mode, in a task started in architect mode
  run --mode architect <<JSON
{"prompt": "Use the switch_mode tool to switch to code mode, then finish with the single word: switched."}
JSON
  SCEN+=("mode_switch")
fi
if want ask; then    # ask_followup_question; the next message is its answer
  run <<JSON
{"prompt": "Use the ask_followup_question tool to ask me which file to read (a.txt or b.txt), then read the one I choose and finish."}
{"prompt": "b.txt"}
JSON
  SCEN+=("followup_question")
fi
if want mcp; then    # 10 MCP tool call (use_mcp_tool) through the stdio server shared with Codex
  run <<JSON
{"prompt": "Use the corpus MCP server's add tool to add 2 and 3, then say the result."}
JSON
  SCEN+=("mcp")
fi
if want condense; then # 8 compaction: auto-condense at the 5% minimum. Z.ai's Anthropic stream
  # reports 0 input tokens, so Roo's estimate is only the last message's own tiktoken count: a
  # padded second message (~11K tokens, over 5% of the 200K window) trips the condense.
  pad="$(python3 -I -c 'print("x " * 11000)')"
  run <<JSON
{"prompt": "List three prime numbers.", "configuration": {"autoCondenseContext": true, "autoCondenseContextPercent": 5}}
{"prompt": "List three more. Ignore this padding: $pad"}
JSON
  SCEN+=("compaction")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"
corpus_proxy_stop

# Copy the store as the extension wrote it: tasks/ (each task dir and the _index.json), minus each
# task's checkpoints/ (a shadow git repository of the workspace; ui_messages.json keeps the
# checkpoint_saved records that point into it). Not global-state.json, secrets.json, settings/,
# cache/ or the machine id.
mkdir -p "$OUT/.vscode-mock/global-storage"
cp -R "$STORE/tasks" "$OUT/.vscode-mock/global-storage/"
rm -rf "$OUT"/.vscode-mock/global-storage/tasks/*/checkpoints
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"roo\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "writer=\"Roo Code CLI $V (GitHub cli-v$V), which runs the bundled Roo Code extension in a mock VS Code host\"" \
  "model=\"$MODEL via Z.ai Anthropic-compatible endpoint, through key_proxy.py (Roo told: anthropic / claude-sonnet-4-5)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
