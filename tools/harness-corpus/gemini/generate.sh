#!/usr/bin/env bash
# Generate the Gemini CLI corpus for one installed version: real chat recordings Gemini CLI wrote
# itself, in a throwaway home, driven by a cheap real model (Z.ai GLM through generate_content_shim.py,
# reached with API-key auth and GOOGLE_GEMINI_BASE_URL).
#
#   tools/harness-corpus/gemini/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/gemini/<version>; it must not exist yet. Needs:
# install.sh <version> run first, python3, git, ~/.zai-key. Env: ZAI_KEY_FILE, CORPUS_MODEL (default
# glm-5.3-flash), GEMINI_MODEL_PROFILE (the model name Gemini CLI is told; default gemini-2.5-flash),
# PORT (default: a free one), MAX_CALLS (default 70).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ONLY="chat tools background image todos tracker plan mcp subagent skill command hook web model compress chatsave"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/gemini/$V/node_modules/.bin/gemini"
[ -x "$BIN" ] || { echo "gemini $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"
PROFILE="${GEMINI_MODEL_PROFILE:-gemini-2.5-flash}"; ALT_PROFILE="gemini-2.5-flash-lite"
PORT="${PORT:-$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')}"
UPSTREAM="https://api.z.ai/api/coding/paas/v4"

corpus_work
SHIM_PID=""
cleanup() { [ -z "$SHIM_PID" ] || kill "$SHIM_PID" 2>/dev/null || true; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" GEMINI_CLI_HOME="$CORPUS_HOME" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME GEMINI_API_KEY GOOGLE_API_KEY \
  GOOGLE_CLOUD_PROJECT GOOGLE_CLOUD_LOCATION GOOGLE_GENAI_USE_VERTEXAI GOOGLE_GENAI_USE_GCA \
  GOOGLE_APPLICATION_CREDENTIALS SANDBOX GEMINI_SANDBOX
corpus_guard HOME GEMINI_CLI_HOME TMPDIR
PROJECT="$CORPUS_HOME/project"
GDIR="$CORPUS_HOME/.gemini"
mkdir -p "$GDIR/skills/corpus-greeting" "$GDIR/commands"

# The scratch project: a git repo with a few small files.
cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

# A hook that injects context only when CORPUS_HOOK=1 (the `hook` scenario), a skill, a custom command.
cat >"$GDIR/corpus_hook.sh" <<'SH'
#!/bin/sh
cat >/dev/null
[ "${CORPUS_HOOK:-}" = 1 ] || { echo '{}'; exit 0; }
echo '{"hookSpecificOutput":{"hookEventName":"BeforeAgent","additionalContext":"corpus hook: the magic word is teal."}}'
SH
chmod +x "$GDIR/corpus_hook.sh"
cat >"$GDIR/skills/corpus-greeting/SKILL.md" <<'MD'
---
name: corpus-greeting
description: How to greet the corpus user. Use when asked to greet with the corpus greeting.
---
Greet with exactly: "Greetings from the corpus skill."
MD
cat >"$GDIR/commands/corpus-hello.toml" <<'TOML'
description = "Say hello the corpus way"
prompt = "Reply with exactly: hello from a custom command. {{args}}"
TOML
cat >"$GDIR/settings.json" <<JSON
{
  "security": {"auth": {"selectedType": "gemini-api-key"}, "folderTrust": {"enabled": false}},
  "privacy": {"usageStatisticsEnabled": false},
  "telemetry": {"enabled": false},
  "general": {"enableAutoUpdate": false, "enableAutoUpdateNotification": false,
              "checkpointing": {"enabled": true}},
  "model": {"name": "$PROFILE"},
  "useWriteTodos": true,
  "experimental": {"enableAgents": true},
  "skills": {"enabled": true},
  "mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"], "trust": true}},
  "hooksConfig": {"enabled": true},
  "hooks": {"BeforeAgent": [{"hooks": [{"type": "command", "command": "$GDIR/corpus_hook.sh", "name": "corpus-hook"}]}]}
}
JSON

# Only the shim sees the real key; Gemini CLI gets a dummy API key and a base URL pointing at the shim.
corpus_load_key UPSTREAM_KEY "${ZAI_KEY_FILE:-$REAL_HOME/.zai-key}"
python3 -I "$HERE/generate_content_shim.py" --port "$PORT" --upstream "$UPSTREAM" --model "$MODEL" \
  --log "$WORK/calls.jsonl" --max-calls "${MAX_CALLS:-70}" &
SHIM_PID=$!
unset UPSTREAM_KEY
export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:$PORT" GEMINI_API_KEY="corpus-local-shim"
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
kill -0 "$SHIM_PID" 2>/dev/null || { echo "the shim did not start" >&2; exit 1; }
touch "$WORK/calls.jsonl"

# ws JSON: a workspace settings file for the next scenario only; ws with no argument removes it.
ws() { if [ $# -gt 0 ]; then mkdir -p "$PROJECT/.gemini"; printf '%s\n' "$1" >"$PROJECT/.gemini/settings.json"; else rm -rf "$PROJECT/.gemini"; fi; }
run() { echo "--- gemini $*" | cut -c1-150; timeout "${RUN_TIMEOUT:-300}" "$BIN" --skip-trust "$@" </dev/null >>"$WORK/run.log" 2>&1 || echo "    (exit $?)"; }
newid() { python3 -I -c 'import uuid; print(uuid.uuid4())'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=()
# What gemini-cli cannot do headless (AFFORDANCES.md): recorded in MANIFEST.toml.
SKIP=("rewind (/rewind is interactive)" "ask_user (needs a person)" "info/warning/error records (written by the interactive UI)" "restore checkpoints (TUI only)")

if want chat; then   # 1 plain multi-turn chat + 9 resume (--resume <uuid> appends to the same recording)
  id="$(newid)"
  run --session-id "$id" -p "Say hello in five words."
  run -r "$id" -p "Now say it in French."
  run -r "$id" -p "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error, 5 thoughts (every turn)
  id="$(newid)"
  run --session-id "$id" -y -p "Read a.txt and b.txt at the same time (two parallel read_file calls), then reply with their combined line count."
  run -r "$id" -y -p "Use the replace tool to change TODO to DONE in notes.txt, then run the shell command: cat notes.txt"
  run -r "$id" -y -p "Run the shell command: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error" "thoughts" "file_checkpoint")
fi
if want background; then  # background shell (run_shell_command is_background)
  run -y -p "Run the shell command 'sleep 1; echo bg-done' in the background (run_shell_command with is_background set to true), then say started."
  SCEN+=("background_shell")
fi
if want image; then  # 6 image input: @file inlines it as inlineData
  run -p "@dot.png An image is attached. Do not use any tools. Reply with the single word: received."
  SCEN+=("image")
fi
if want todos; then  # write_todos (planning)
  run -y -p "Use the write_todos tool to record a plan with three steps (read a.txt, read b.txt, summarize), marking the first in_progress. Then stop."
  SCEN+=("write_todos")
fi
if want tracker; then  # tracker_* (experimental.taskTracker), stored under tmp/<project>/<session>/tracker
  ws '{"experimental": {"taskTracker": true}}'
  run -y -p "Use tracker_create_task to create a task titled 'corpus task' with description 'check the corpus', then use tracker_list_tasks and say how many tasks exist."
  ws
  SCEN+=("task_tracker")
fi
if want plan; then  # plan mode: enter/exit_plan_mode, plans under tmp/<project>/<session>/plans
  run --approval-mode plan -p "Plan (do not implement) how to add a file c.txt containing delta. Write the plan, then call exit_plan_mode."
  SCEN+=("plan_mode")
fi
if want mcp; then    # 10 MCP tool call through the stdio server
  run -y -p "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want subagent; then # 7 sub-agents: the agent tool runs a local subagent with its own recording
  run -y -p "Delegate to the codebase_investigator subagent: ask it which file in this project contains the word TODO. Then report its answer in one line."
  SCEN+=("subagent")
fi
if want skill; then  # skills: activate_skill
  run -y -p "Use the corpus-greeting skill (activate it first) and greet me with it."
  SCEN+=("skill")
fi
if want command; then  # a custom slash command (~/.gemini/commands/*.toml) expanded headless
  run -p "/corpus-hello please"
  SCEN+=("custom_command")
fi
if want hook; then   # a BeforeAgent hook's additionalContext (recorded as <hook_context>)
  CORPUS_HOOK=1 run -p "What is the magic word? One word."
  SCEN+=("hook")
fi
if want web; then    # web_fetch (the model answers through the shim; the call is what persists)
  run -y -p "Use web_fetch to fetch https://example.com and tell me its title in five words or fewer."
  run -y -p "Use google_web_search to search for: example domain. Then answer in five words."
  SCEN+=("web_fetch" "web_search")
fi
if want model; then  # a model switch inside one session (per-message model differs from the session's)
  id="$(newid)"
  run --session-id "$id" -p "Name one color."
  run -r "$id" -m "$ALT_PROFILE" -p "Name another color."
  SCEN+=("model_switch")
fi
if want compress; then # 8 compression: /compress is interactive-only (headless sends it to the model as
  # text), so a tiny workspace compressionThreshold forces the automatic one on the resumed turn.
  id="$(newid)"
  run --session-id "$id" -p "List three prime numbers."
  ws '{"model": {"compressionThreshold": 0.0001}}'
  run -r "$id" -p "List three more."
  ws
  SCEN+=("compress")
fi
if want chatsave; then # /chat save <tag> writes tmp/<project>/checkpoint-<tag>.json (then exits 42:
  # its confirm-on-overwrite result is unsupported headless, but the file is already written)
  id="$(newid)"
  run --session-id "$id" -p "Say the word saved."
  run -r "$id" -p "/chat save corpus"
  SCEN+=("chat_save")
fi

CALLS="$(grep -c '"refused": false' "$WORK/calls.jsonl" || true)"
echo "model calls: $CALLS"

# Copy the store as Gemini CLI wrote it: everything under tmp/ (recordings, checkpoints, logs,
# plans, tracker), projects.json, and history/*/.project_root. Not settings, oauth, installation_id,
# the shadow git repos under history/, or our hook/skill/command inputs.
mkdir -p "$OUT/.gemini"
[ -d "$GDIR/tmp" ] && cp -R "$GDIR/tmp" "$OUT/.gemini/"
rm -rf "$OUT/.gemini/tmp/bin"
[ -e "$GDIR/projects.json" ] && cp "$GDIR/projects.json" "$OUT/.gemini/"
if [ -d "$GDIR/history" ]; then
  for d in "$GDIR"/history/*/; do
    [ -e "$d/.project_root" ] || continue
    mkdir -p "$OUT/.gemini/history/$(basename "$d")"; cp "$d/.project_root" "$OUT/.gemini/history/$(basename "$d")/"
  done
fi
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"gemini\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through generate_content_shim.py (gemini-api-key auth with GOOGLE_GEMINI_BASE_URL)\"" \
  "gemini_model_profile=\"$PROFILE (the model name Gemini CLI recorded; $ALT_PROFILE for the switch)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[${SKIP[*]:+$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')}]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
