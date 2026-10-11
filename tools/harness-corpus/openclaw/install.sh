#!/usr/bin/env bash
# Install one exact OpenClaw version into ~/tools/harness-versions/openclaw/<version>/ (persistent,
# idempotent). Never touches the Homebrew/global install or ~/.openclaw.
#
#   tools/harness-corpus/openclaw/install.sh <version>      # e.g. 2026.10.1
#
# Prints the path of the installed `openclaw` binary on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/openclaw/$V"
BIN="$PREFIX/node_modules/.bin/openclaw"
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "openclaw@$V" >&2
fi
"$BIN" --version | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
