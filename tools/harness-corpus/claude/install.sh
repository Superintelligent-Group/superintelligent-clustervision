#!/usr/bin/env bash
# Install one exact Claude Code version into ~/tools/harness-versions/claude/<version>/ (persistent,
# idempotent). Never touches ~/.local/share/claude or the global install.
#
#   tools/harness-corpus/claude/install.sh <version>      # e.g. 2.1.296
#
# Prints the path of the installed `claude` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/claude/$V"
BIN="$PREFIX/node_modules/.bin/claude"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  # --prefix keeps it out of the global node_modules; --no-save/--no-package-lock keep the dir clean.
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "@anthropic-ai/claude-code@$V" >&2
fi
"$BIN" --version | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
