#!/usr/bin/env bash
# Install one exact Devin CLI (npm `devin`, a closed-source native binary) into
# ~/tools/harness-versions/devin/<version>/ (persistent, idempotent). Never touches the copy ember
# runs (~/.local/bin/devin -> ~/.local/share/devin/cli/_versions/).
#
#   tools/harness-corpus/devin/install.sh <version>      # e.g. 3000.11.3
#
# Prints the path of the installed `devin` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/devin/$V"
BIN="$PREFIX/node_modules/.bin/devin"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "devin $V "; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "devin@$V" >&2
fi
"$BIN" --version | grep -qF "devin $V " || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
