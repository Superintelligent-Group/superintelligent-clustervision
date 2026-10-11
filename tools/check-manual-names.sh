#!/usr/bin/env bash
# Every `cv <cmd>` and every MCP tool name the manual mentions must exist: commands in
# `cv schema --commands --json`, tools in a live `tools/list` from cv-mcp. The manual is the one
# place the roster can go stale (the MCP tools are generated from the clap tree, so the binary
# cannot), and it did: a `formats` tool that is really `formats_census`/`formats_check`, and no
# `task_stats`.
#
#   tools/check-manual-names.sh [path/to/cv] [path/to/cv-mcp]      (default: target/debug/*)
#
# Same shape as ci.yml's `.kind ===` grep: a regex over the prose, a list from the binary, exit 1
# with every offender named. Names the manual documents AS removed (the 0.11 rename tables) and
# backticked JSON keys that happen to share a prefix are allowlisted below, each with its reason.
set -euo pipefail
cd "$(dirname "$0")/.."
CV="${1:-target/debug/cv}"
CV_MCP="${2:-target/debug/cv-mcp}"

# `cv <name>` where the manual says the name is gone (the binary errors on it with a pointer), and
# clap's built-in `help`.
REMOVED_COMMANDS="convert query recall help"
# Backticked snake_case tokens that are JSON keys, not tools.
NOT_TOOLS="task_id"

fail=0

# ---- commands --------------------------------------------------------------------------------
known_cmds=$("$CV" schema --commands --json | python3 -I -c '
import json, sys
print(" ".join(c["name"] for c in json.load(sys.stdin) if " " not in c["name"]))')
mentioned_cmds=$(grep -ohE '`cv [a-z][a-z-]*' manual/src/*.md | sed 's/`cv //' | sort -u)
for name in $mentioned_cmds; do
  case " $known_cmds $REMOVED_COMMANDS " in
    *" $name "*) ;;
    *) echo "::error::manual names \`cv $name\`, which is not in \`cv schema --commands\`:"
       grep -nE "\`cv $name( |\`)" manual/src/*.md | head -5
       fail=1 ;;
  esac
done

# ---- MCP tools -------------------------------------------------------------------------------
known_tools=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | CV_BIN="$(cd "$(dirname "$CV")" && pwd)/$(basename "$CV")" "$CV_MCP" 2>/dev/null \
  | python3 -I -c '
import json, sys
for line in sys.stdin:
    d = json.loads(line)
    if d.get("id") == 2:
        print(" ".join(t["name"] for t in d["result"]["tools"]))')
if [ -z "$known_tools" ]; then
  echo "::error::cv-mcp returned no tools/list (is $CV_MCP built, and does it find $CV?)"
  exit 1
fi
# A tool name is a whole backticked token (or the head of a `tool(args)` call): the hand-written
# families by prefix, plus the two blocking tools.
mentioned_tools=$(grep -ohE '`((task|board)_[a-z_]+|await_omen|observe_stream)[`(]' manual/src/*.md \
  | sed 's/^`//; s/[`(]$//' | sort -u)
for name in $mentioned_tools; do
  case " $known_tools $NOT_TOOLS " in
    *" $name "*) ;;
    *) echo "::error::manual names MCP tool \`$name\`, which cv-mcp does not serve:"
       grep -nE "\`$name[\`(]" manual/src/*.md | head -5
       fail=1 ;;
  esac
done

if [ "$fail" -eq 0 ]; then
  echo "manual names: $(echo "$mentioned_cmds" | wc -w | tr -d ' ') commands and $(echo "$mentioned_tools" | wc -w | tr -d ' ') MCP tools all exist"
fi
exit "$fail"
