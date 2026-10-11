#!/usr/bin/env bash
# Install one exact Goose CLI release into ~/tools/harness-versions/goose/<version>/ (persistent,
# idempotent), from the official GitHub release tarball (block/goose, goose-<target>.tar.bz2).
# Never touches a system-wide goose.
#
#   tools/harness-corpus/goose/install.sh <version>      # e.g. 1.54.0
#
# Prints the path of the installed `goose` binary on stdout. Needs gh (or curl) and tar.
set -euo pipefail
V="${1:?usage: install.sh <version>}"
case "$V" in *[!0-9a-z.-]*) echo "bad version: $V" >&2; exit 2 ;; esac
PREFIX="${HARNESS_VERSIONS:-$HOME/tools/harness-versions}/goose/$V"
BIN="$PREFIX/bin/goose"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  Linux-aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  *) echo "no goose release for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
if [ ! -x "$BIN" ] || ! "$BIN" --version 2>/dev/null | grep -qF "$V"; then
  DL="$(mktemp -d)"; trap 'rm -rf "$DL"' EXIT
  ASSET="goose-$TARGET.tar.bz2"
  if command -v gh >/dev/null; then
    gh release download "v$V" -R block/goose -p "$ASSET" -D "$DL" >&2
  else
    curl -fsSL -o "$DL/$ASSET" "https://github.com/block/goose/releases/download/v$V/$ASSET"
  fi
  mkdir -p "$DL/x" "$PREFIX/bin"
  tar -xjf "$DL/$ASSET" -C "$DL/x"
  found="$(find "$DL/x" -type f -name goose -perm -u+x | head -1)"
  [ -n "$found" ] || { echo "no goose binary in $ASSET" >&2; exit 1; }
  install -m 755 "$found" "$BIN"
fi
"$BIN" --version | grep -qF "$V" || { echo "installed $BIN does not report $V" >&2; exit 1; }
echo "$BIN"
