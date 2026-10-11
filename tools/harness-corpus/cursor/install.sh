#!/usr/bin/env bash
# Install one exact Cursor agent CLI build into ~/tools/harness-versions/cursor/<build>/ (persistent,
# idempotent). Never touches ~/.local/share/cursor-agent or ~/.local/bin.
#
#   tools/harness-corpus/cursor/install.sh <build>      # e.g. 2026.10.01-e373342
#
# Fetches the tarball the official installer (https://cursor.com/install) names:
# https://downloads.cursor.com/lab/<build>/<os>/<arch>/agent-cli-package.tar.gz. Prints the path of
# the `cursor-agent` entry point on stdout.
set -euo pipefail
V="${1:?usage: install.sh <build>}"
case "$V" in *[!0-9a-z.-]*) echo "bad build: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/cursor/$V"
BIN="$PREFIX/cursor-agent"
if [ ! -x "$BIN" ]; then
  case "$(uname -s)" in Darwin) OS=darwin ;; Linux) OS=linux ;; *) echo "unsupported OS" >&2; exit 1 ;; esac
  case "$(uname -m)" in arm64|aarch64) ARCH=arm64 ;; x86_64) ARCH=x64 ;; *) echo "unsupported arch" >&2; exit 1 ;; esac
  TMP="$(dirname "$PREFIX")/.download-$V.$$"
  mkdir -p "$TMP"
  trap 'rm -rf "$TMP"' EXIT
  curl -fsSL "https://downloads.cursor.com/lab/$V/$OS/$ARCH/agent-cli-package.tar.gz" | tar --strip-components=1 -xzf - -C "$TMP"
  [ -x "$TMP/cursor-agent" ] || { echo "no cursor-agent in the $V package" >&2; exit 1; }
  rm -rf "$PREFIX" && mv "$TMP" "$PREFIX"
fi
echo "$BIN"
