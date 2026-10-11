#!/usr/bin/env bash
# Generate the legacy kimi-cli corpus for one installed version: real sessions kimi-cli wrote itself,
# in a throwaway KIMI_SHARE_DIR, driven by Kimi's coding model through passthrough_proxy.py.
#
#   tools/harness-corpus/kimi/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# <out-dir> is normally crates/cv-core/tests/corpus/kimi/<version>; it must not exist yet.
# Needs: install.sh <version> run first, python3, git, ~/.kimikey (a Kimi Code membership key for
# https://api.kimi.com/coding/v1). Env: KIMI_KEY_FILE, CORPUS_MODEL (default kimi-for-coding),
# MAX_CALLS (default 60).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0
ALL="chat tools image todo plan background skill mcp fetch ask subagent ralph compact clear"
ONLY="$ALL"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/kimi/$V/bin/kimi"
[ -x "$BIN" ] || { echo "kimi-cli $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-kimi-for-coding}"
UPSTREAM="https://api.kimi.com/coding/v1"

corpus_work
PROXY_PID=""
cleanup() { [ -z "$PROXY_PID" ] || kill "$PROXY_PID" 2>/dev/null || true; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" KIMI_SHARE_DIR="$CORPUS_HOME/.kimi" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR" "$KIMI_SHARE_DIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME KIMI_API_KEY KIMI_BASE_URL KIMI_MODEL_NAME \
  KIMI_CODE_HOME OPENAI_API_KEY OPENAI_BASE_URL ANTHROPIC_API_KEY
corpus_guard HOME KIMI_SHARE_DIR TMPDIR
PROJECT="$CORPUS_HOME/project"

cd "$PROJECT"
printf 'alpha\nbeta\n' >a.txt; printf 'gamma\n' >b.txt; printf 'TODO: fix\n' >notes.txt
python3 -I -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="))' >dot.png
printf '# Project notes\nThis is a tiny test repository. Keep answers short.\n' >AGENTS.md
git init -q -b main . && git -c user.name=corpus -c user.email=corpus@example.invalid add -A \
  && git -c user.name=corpus -c user.email=corpus@example.invalid commit -qm init

corpus_load_key UPSTREAM_KEY "${KIMI_KEY_FILE:-$REAL_HOME/.kimikey}"
python3 -I "$HERE/../passthrough_proxy.py" --port-file "$WORK/port" --upstream "$UPSTREAM" --strip-prefix /v1 \
  --log "$WORK/calls.jsonl" --max-calls "${MAX_CALLS:-60}" &
PROXY_PID=$!
unset UPSTREAM_KEY
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
PORT="$(cat "$WORK/port")"

# Config and MCP config live in $WORK, outside the share dir: kimi-cli only gets a dummy key.
cp "$HERE/../codex/mcp_server.py" "$WORK/mcp_server.py"
cat >"$WORK/config.toml" <<TOML
default_model = "corpus"
default_thinking = true
telemetry = false

[providers.corpus]
type = "kimi"
base_url = "http://127.0.0.1:$PORT/v1"
api_key = "corpus-local-proxy"

[models.corpus]
provider = "corpus"
model = "$MODEL"
max_context_size = 262144
capabilities = ["thinking", "image_in"]

[[hooks]]
event = "PostToolUse"
matcher = "Shell"
command = "echo corpus-hook-saw-shell"

[[hooks]]
event = "UserPromptSubmit"
command = "echo corpus-hook-saw-prompt"
TOML
cat >"$WORK/mcp.json" <<JSON
{"mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$WORK/mcp_server.py"]}}}
JSON
mkdir -p "$KIMI_SHARE_DIR/skills/greet"
cat >"$KIMI_SHARE_DIR/skills/greet/SKILL.md" <<'MD'
---
name: greet
description: Greet the user in the corpus house style. Use when asked to greet.
---
Reply with exactly: "Greetings from the corpus skill."
MD

LAST=""
run() {
  echo "--- kimi $*" | cut -c1-140
  "$BIN" --print --afk --config-file "$WORK/config.toml" --mcp-config-file "$WORK/mcp.json" -w "$PROJECT" "$@" \
    </dev/null >"$WORK/run.out" 2>&1 || echo "    (exit $?)"
  cat "$WORK/run.out" >>"$WORK/exec.log"
  LAST="$(grep -oE 'kimi -r [0-9a-f-]{36}' "$WORK/run.out" | tail -1 | cut -d' ' -f3 || true)"
  [ -n "$LAST" ] || echo "    (no session id in output)"
}
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=()
# One session, resumed (-S) for every scenario, like a long real session. --afk: no user present,
# tool calls auto-approved, AskUserQuestion auto-dismissed.
A=""
on_a() { if [ -z "$A" ]; then run -p "$1" "${@:2}"; A="$LAST"; else run -S "$A" -p "$1" "${@:2}"; fi; }

if want chat; then      # 1 multi-turn chat + 9 resume
  on_a "Say hello in five words."
  on_a "Now say it in French."
  on_a "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then     # 2 read/edit/shell, 3 parallel, 4 tool error, 5 thinking, hooks on every turn
  on_a "Read a.txt and b.txt at the same time (two parallel ReadFile calls), then reply with their combined line count."
  on_a "Use StrReplaceFile to replace TODO with DONE in notes.txt, then run with Shell: cat notes.txt"
  on_a "Run with Shell: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error" "thinking" "hooks" "checkpoints")
fi
if want image; then     # 6 image input through ReadMediaFile
  on_a "Use ReadMediaFile on dot.png and tell me its main colour in one word."
  SCEN+=("image")
fi
if want todo; then      # planning: SetTodoList
  on_a "Use SetTodoList to record three todos: read a.txt, read b.txt, summarize, with the first one done. Do nothing else."
  SCEN+=("todo")
fi
if want plan; then      # plan mode through its tools
  on_a "Call EnterPlanMode. Then plan (do not execute) how to rename b.txt to c.txt in two bullet points, and call ExitPlanMode with that plan. Do not rename anything."
  SCEN+=("plan_mode")
fi
if want background; then # a background shell and its completion notification
  on_a "Start this with Shell in the background (run_in_background): sleep 2; echo bg-done   Then use TaskOutput to wait for it and report its output in one line."
  SCEN+=("background_task")
fi
if want skill; then     # a user skill, invoked as a slash command
  on_a "/skill:greet"
  SCEN+=("skill")
fi
if want mcp; then       # 10 MCP tool call through the stdio server
  on_a "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want fetch; then     # web fetch
  on_a "Use FetchURL on https://example.com and give me the page title in one line."
  SCEN+=("fetch_url")
fi
if want ask; then       # AskUserQuestion, auto-dismissed in afk mode
  on_a "Use AskUserQuestion to ask me whether to proceed (options yes/no). If no answer comes, say 'no answer'."
  SCEN+=("ask_user")
fi
if want subagent; then  # 7 sub-agents: the Agent tool
  on_a "Use the Agent tool once to spawn a sub-agent with the task 'reply with the word ok'. Then report what it said."
  SCEN+=("subagent")
fi
if want ralph; then     # the Ralph loop: one extra automatic iteration after the turn
  on_a "Say done." --max-ralph-iterations 1
  SCEN+=("ralph_loop")
fi
if want compact; then   # 8 compaction: the soul handles /compact in print mode too
  on_a "/compact"
  on_a "List three prime numbers."
  SCEN+=("compaction")
fi
if want clear; then     # /clear rotates context.jsonl into context_N.jsonl
  on_a "/clear"
  on_a "Say ok."
  SCEN+=("clear")
fi
SKIP+=("approvals: --afk auto-approves every tool call"
  "web_search: SearchWeb needs the Moonshot search service (OAuth login)"
  "interrupt: print mode has no interrupt; the Kimi Code corpus covers it")

CALLS="$(wc -l <"$WORK/calls.jsonl" | tr -d ' ')"
echo "model calls: $CALLS"

# Copy what kimi-cli wrote: the session dirs (context*.jsonl, wire.jsonl, state.json, sidecars) and
# kimi.json (the cwd registry cv reads). Not config, skills, device_id, logs, or telemetry.
mkdir -p "$OUT/.kimi"
cp -R "$KIMI_SHARE_DIR/sessions" "$OUT/.kimi/"
[ -e "$KIMI_SHARE_DIR/kimi.json" ] && cp "$KIMI_SHARE_DIR/kimi.json" "$OUT/.kimi/"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"kimi\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "model=\"$MODEL via api.kimi.com/coding/v1, through passthrough_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
