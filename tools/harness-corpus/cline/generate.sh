#!/usr/bin/env bash
# Generate the Cline corpus for one installed Cline CLI version: sessions the Cline SDK wrote
# itself into ~/.cline/data (sessions/<id>/<id>.json + <id>.messages.json, db/sessions.db), in a
# throwaway home, driven by Z.ai GLM through key_proxy.py. The 4.x VS Code extension writes the
# same store; the 3.x extension's globalStorage tasks/ layout is the older format.
#
#   tools/harness-corpus/cline/generate.sh <version> <out-dir> [--keep] [--only "chat tools ..."]
#
# Needs: install.sh <version> first, python3, git, sqlite3, ~/.zai-key. Env: ZAI_KEY_FILE,
# CORPUS_MODEL (default glm-5.3-flash), MAX_CALLS (default 60).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/../lib.sh"
. "$HERE/../drive.sh"

V="${1:?usage: generate.sh <version> <out-dir>}"; OUT="${2:?usage: generate.sh <version> <out-dir>}"; shift 2
KEEP=0; ONLY="chat tools subagent team plan mcp fetch ask"
while [ $# -gt 0 ]; do
  case "$1" in --keep) KEEP=1 ;; --only) ONLY="$2"; shift ;; *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift
done
mkdir -p "$(dirname "$OUT")"; OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
[ ! -e "$OUT" ] || { echo "refusing: $OUT exists (add a new version instead of regenerating)" >&2; exit 1; }
BIN="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/cline/$V/node_modules/.bin/cline"
[ -x "$BIN" ] || { echo "cline $V not installed: run $HERE/install.sh $V" >&2; exit 1; }
MODEL="${CORPUS_MODEL:-glm-5.3-flash}"
export MAX_CALLS="${MAX_CALLS:-60}"

corpus_work
cleanup() { corpus_proxy_stop; [ "$KEEP" = 1 ] && echo "kept $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT
export HOME="$CORPUS_HOME" CLINE_DIR="$CORPUS_HOME/.cline" TMPDIR="$WORK/tmp"
# local: no background hub daemon (it would outlive the run); no self-update.
export CLINE_SESSION_BACKEND_MODE=local CLINE_NO_AUTO_UPDATE=1
mkdir -p "$TMPDIR"
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME CLINE_DATA_DIR CLINE_API_KEY \
  CLINE_PROVIDER CLINE_MODEL OPENAI_API_KEY ANTHROPIC_API_KEY ZHIPU_API_KEY ZAI_API_KEY
corpus_guard HOME CLINE_DIR TMPDIR
PROJECT="$CORPUS_HOME/project"
DATA="$CLINE_DIR/data"
corpus_project

corpus_proxy "https://api.z.ai/api/coding/paas/v4" bearer "$MODEL"
# The provider gets a dummy key and the proxy's URL; the proxy adds the real key.
"$BIN" auth -p openai-compatible -k corpus-local-proxy -m "$MODEL" -b "$PROXY_URL/v1" >/dev/null
mkdir -p "$DATA/settings"
cat >"$DATA/settings/cline_mcp_settings.json" <<JSON
{"mcpServers": {"corpus": {"command": "python3", "args": ["-I", "$HERE/../codex/mcp_server.py"]}}}
JSON

cd "$PROJECT"
run() { echo "--- cline $*" | cut -c1-140; "$BIN" --json -c "$PROJECT" "$@" </dev/null >>"$WORK/exec.log" 2>&1 || echo "    (exit $?)"; }
want() { case " $ONLY " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
SCEN=(); SKIP=("multi-turn chat and resume: --id opens the TUI, which needs a TTY (headless runs are single-prompt)"
  "image: the CLI has no attachment flag"
  "compaction: /compact is a TUI command (a -p/--json prompt of /compact is rejected); the ACP mode needs a Cline account sign-in"
  "checkpoints and /undo, /fork: TUI only")

# --id (resume) always opens the TUI, which needs a TTY, and under a pty the TUI neither sends the
# prompt nor honours CLINE_SESSION_BACKEND_MODE=local; so every scenario is one prompt.
if want chat; then   # 1 chat (one user turn; see above) + 5 reasoning
  run --thinking low "Say hello in five words, then the same in French."
  SCEN+=("chat" "reasoning")
fi
if want tools; then  # 2 read/edit/shell, 3 parallel calls, 4 a tool error
  run "Read a.txt and b.txt at the same time (two parallel tool calls), then edit notes.txt to replace TODO with DONE, then run the command: cat notes.txt missing-file.txt   and report the line count of a.txt plus b.txt and the error in one line."
  SCEN+=("tools" "parallel_tools" "tool_error")
fi
if want subagent; then # 7 sub-agents: spawn_agent (a child session with parent_session_id)
  run "Use the spawn_agent tool once with the task 'reply with the word ok', then report what it said."
  SCEN+=("subagent")
fi
if want team; then   # agent teams: a teammate, a task run, its result, cleanup (teams.db)
  run "Use team_spawn_teammate to add a teammate named helper, then team_run_task to ask helper to reply with the word ok, wait for it with team_await_runs, report the reply, and finish with team_cleanup."
  SCEN+=("team")
fi
if want plan; then   # plan mode (read-only)
  run -p "Plan, without editing anything, how to rename b.txt to c.txt. Two steps."
  SCEN+=("plan_mode")
fi
if want mcp; then    # 10 MCP tool call through the stdio server shared with the Codex generator
  run "Use the corpus MCP server's add tool to add 2 and 3, then say the result."
  SCEN+=("mcp")
fi
if want fetch; then  # web fetch
  run "Use fetch_web_content on https://example.com and tell me the page title."
  SCEN+=("web_fetch")
fi
if want ask; then    # ask_question (non-interactive: Cline answers with the first option)
  run "Use the ask_question tool to ask me whether to read a.txt or b.txt (two options), then read the file I pick."
  SCEN+=("ask_question")
fi

CALLS="$(corpus_calls)"
echo "model calls: $CALLS"
corpus_proxy_stop

# Copy the store as the SDK wrote it: sessions/ (each session's manifest and messages, plus any
# index or spawn queue) and the session and team databases (backup API: they run in WAL mode).
# Not settings/ (providers.json holds the provider config), logs/, cache/, locks/, or the
# connectors, cron and tasks databases (empty here).
mkdir -p "$OUT/.cline/data/db"
cp -R "$DATA/sessions" "$OUT/.cline/data/"
for db in sessions teams; do
  [ -e "$DATA/db/$db.db" ] || continue
  sqlite3 "$DATA/db/$db.db" ".backup '$OUT/.cline/data/db/$db.db'"
  sqlite3 "$OUT/.cline/data/db/$db.db" "PRAGMA journal_mode=DELETE" >/dev/null
done
corpus_scrub "$OUT"
corpus_manifest "$OUT" \
  "harness=\"cline\"" "version=\"$V\"" "generated=\"$(date -u +%Y-%m-%d)\"" \
  "writer=\"Cline CLI $V (npm cline) on the Cline SDK; the 4.x VS Code extension writes the same ~/.cline/data store\"" \
  "model=\"$MODEL via Z.ai coding endpoint, through key_proxy.py (provider openai-compatible)\"" \
  "model_calls=$CALLS" \
  "scenarios=[$(printf '"%s",' "${SCEN[@]}" | sed 's/,$//')]" \
  "skipped=[$(printf '"%s",' "${SKIP[@]}" | sed 's/,$//')]" \
  "generator_commit=\"$(git -C "$HERE" rev-parse --short HEAD)\""
corpus_size_check "$OUT"
