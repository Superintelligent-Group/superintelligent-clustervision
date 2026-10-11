#!/usr/bin/env bash
# Generate the Continue corpus for one installed `cn` version: sessions the Continue CLI wrote
# itself into ~/.continue/sessions (the same store and file shape the VS Code / JetBrains
# extension writes), in a throwaway home, driven by Z.ai GLM through key_proxy.py.
#
#   tools/harness-corpus/continue/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# Needs: install.sh <version> first, python3, git, ~/.zai-key. Env: ZAI_KEY_FILE, CORPUS_MODEL
# (default glm-5.3-flash), MAX_CALLS (default 40).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"
. "$HERE/../drive.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools mcp subagent fork plan fetch"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/continue/$V/node_modules/.bin/cn"
[ -x "$BIN" ] || { echo "cn $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" CONTINUE_GLOBAL_DIR="$CORPUS_HOME/.continue" TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME OPENAI_API_KEY OPENAI_BASE_URL CONTINUE_API_KEY
corpus_guard HOME CONTINUE_GLOBAL_DIR TMPDIR
PROJECT="$CORPUS_HOME/project"
corpus_project

corpus_proxy "https://api.z.ai/api/coding/paas/v4" bearer "$MODEL"
# The harness gets a dummy key for a provider at the local proxy; the proxy adds the real one.
cat >"$CONTINUE_GLOBAL_DIR/config.yaml" <<YAML
name: corpus
version: 1.0.0
schema: v1
models:
  - name: glm
    provider: openai
    model: $MODEL
    apiBase: $PROXY_URL/v1
    apiKey: corpus-local-proxy
    roles: [chat, edit, apply, summarize]
    capabilities: [tool_use]
  # The Subagent tool only offers models with the subagent role and a baseSystemMessage.
  - name: helper
    provider: openai
    model: $MODEL
    apiBase: $PROXY_URL/v1
    apiKey: corpus-local-proxy
    roles: [subagent]
    capabilities: [tool_use]
    chatOptions:
      baseSystemMessage: "You are a helper sub-agent. Do the task in one short reply."
mcpServers:
  - name: corpus
    command: python3
    args: ["-I", "$HERE/../codex/mcp_server.py"]
YAML
cd "$PROJECT"
CONFIG="$CONTINUE_GLOBAL_DIR/config.yaml"; MODE=--auto
run() { echo "--- cn $*" | cut -c1-140; "$BIN" --config "$CONFIG" $MODE -p "$@" </dev/null >>"$WORK/exec.log" 2>&1 || echo "    (exit $?)"; }
last_id() { ls -t "$CONTINUE_GLOBAL_DIR"/sessions/*.json | grep -v '/sessions.json$' | head -1 | sed -E 's#.*/([^/]+)\.json$#\1#'; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=("image: cn -p has no attachment flag"
  "reasoning: cn does not persist GLM reasoning_content"
  "compaction: headless -p /compact compacts in memory and exits without saving; a small contextLength made cn drop the truncated reply instead of auto-compacting")

if want chat; then   # 1 multi-turn chat + 9 resume (--resume continues the most recent session)
  run "Say hello in five words."
  run --resume "Now say it in French."
  run --resume "Which language did you use first? One word."
  SCEN+=("chat" "resume")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error
  run "Read a.txt and b.txt at the same time (two parallel tool calls), then reply with their combined line count."
  run --resume "Edit notes.txt to replace TODO with DONE, then run the shell command: cat notes.txt"
  run --resume "Run the shell command: cat missing-file.txt   and tell me the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error")
fi
if want mcp; then    # 10 MCP tool call through the stdio server shared with the Codex generator
  run "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want subagent; then # 7 sub-agents: the beta Subagent tool with the helper model as the agent type.
  # In -p mode cn 1.5.47 fails the call ("No active session found"); the call and its error persist.
  run --beta-subagent-tool "Use the Subagent tool once (subagent_name helper) with the task 'reply with the word ok', then report what it said."
  SCEN+=("subagent")
fi
if want fork; then   # --fork copies a session into a new one and continues it
  src="$(last_id)"
  run --fork "$src" "Say the word forked."
  SCEN+=("fork")
fi
if want plan; then   # plan mode (read-only tools; it refuses --auto) and the Checklist tool
  MODE=""
  run --readonly "Use the Checklist tool to write a two-item checklist for renaming b.txt. Do not edit anything."
  MODE=--auto
  SCEN+=("plan_mode" "checklist")
fi
if want fetch; then  # web fetch
  run "Use the Fetch tool on https://example.com and tell me the page title."
  SCEN+=("web_fetch")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"
corpus_proxy_stop

# Copy the store as cn wrote it: sessions/ (the per-session json and the sessions.json index).
# Not config.yaml, permissions.yaml, or logs.
mkdir -p "$OUT/.continue"
cp -R "$CONTINUE_GLOBAL_DIR/sessions" "$OUT/.continue/"
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"continue\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "writer=\"Continue CLI (cn, npm @continuedev/cli); the IDE extensions write the same ~/.continue/sessions\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py\"" \
  "model_calls=$CALLS" \
  "scenarios=$(toml_list ${SCEN[@]+"${SCEN[@]}"})" \
  "skipped=$(toml_list ${SKIP[@]+"${SKIP[@]}"})" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
