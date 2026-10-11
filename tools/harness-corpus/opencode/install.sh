#!/usr/bin/env bash
# Install one exact OpenCode version into ~/tools/harness-versions/opencode/<version>/ (persistent,
# idempotent). Never touches ~/.opencode or the Homebrew install.
#
#   tools/harness-corpus/opencode/install.sh <version>      # e.g. 1.18.20, 2.0.25
#
# 1.x ships on npm as `opencode-ai`; 2.x as `@opencode/cli` (not as a GitHub release). Both are a
# wrapper whose postinstall links the platform binary. Prints the binary path on stdout.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
case "$V" in 0.*|1.*) PKG="opencode-ai" ;; *) PKG="@opencode/cli" ;; esac
# Even `--version` writes a log under the data dir (2.x), so every probe runs in a throwaway home.
PROBE="$(mktemp -d "${TMPDIR:-/tmp}/cv-opencode-probe.XXXXXX")"
trap 'rm -rf "$PROBE"' EXIT
ver() { env HOME="$PROBE" XDG_DATA_HOME="$PROBE/data" XDG_CONFIG_HOME="$PROBE/config" XDG_STATE_HOME="$PROBE/state" XDG_CACHE_HOME="$PROBE/cache" "$BIN" --version 2>/dev/null; }
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/opencode/$V"
BIN="$PREFIX/node_modules/.bin/opencode"
if [ ! -x "$BIN" ] || ! ver | grep -qF "$V"; then
  command -v npm >/dev/null || { echo "npm not on PATH" >&2; exit 1; }
  mkdir -p "$PREFIX"
  npm install --prefix "$PREFIX" --no-audit --no-fund --loglevel=error "$PKG@$V" >&2
fi
ver | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
