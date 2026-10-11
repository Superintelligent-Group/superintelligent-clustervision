#!/usr/bin/env bash
# Install one exact Continue CLI (`cn`, npm @continuedev/cli) into
# ~/tools/harness-versions/continue/<version>/ (persistent, idempotent).
#
#   tools/harness-corpus/continue/install.sh <version>      # e.g. 1.5.47
#
# Prints the path of the installed `cn` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/continue/$V"
BIN="$PREFIX/node_modules/.bin/cn"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qxF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "@continuedev/cli@$V" >&2
fi
"$BIN" --version | grep -qxF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
