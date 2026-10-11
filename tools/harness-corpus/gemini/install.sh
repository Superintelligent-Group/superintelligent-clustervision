#!/usr/bin/env bash
# Install one exact Gemini CLI version into ~/tools/harness-versions/gemini/<version>/ (persistent,
# idempotent). Never touches the Homebrew install.
#
#   tools/harness-corpus/gemini/install.sh <version>      # e.g. 0.63.0
#
# Prints the path of the installed `gemini` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/gemini/$V"
BIN="$PREFIX/node_modules/.bin/gemini"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "@google/gemini-cli@$V" >&2
fi
"$BIN" --version | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
