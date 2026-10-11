#!/usr/bin/env bash
# Install one exact Cline CLI (npm `cline`, the 3.x CLI built on the Cline SDK; the 4.x VS Code
# extension is built on the same SDK and session store) into
# ~/tools/harness-versions/cline/<version>/ (persistent, idempotent).
#
#   tools/harness-corpus/cline/install.sh <version>      # e.g. 3.0.70
#
# Prints the path of the installed `cline` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/cline/$V"
BIN="$PREFIX/node_modules/.bin/cline"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qxF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "cline@$V" >&2
fi
"$BIN" --version | grep -qxF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
