#!/usr/bin/env bash
# Stage the two static musl collector binaries where Tauri bundles them:
# apps/tokenme-bar/src-tauri/resources/collector/tokenme-{x86_64,aarch64}.
#
# Builds them first (via build-linux.sh) when dist/linux is missing, so a
# release build from a fresh checkout works without manual steps.
set -euo pipefail
cd "$(dirname "$0")/.."

V=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT="apps/tokenme-bar/src-tauri/resources/collector"
mkdir -p "$OUT"

for ARCH in x86_64 aarch64; do
  TAR="dist/linux/tokenme-cli-$V-$ARCH-linux-musl.tar.gz"
  if [[ ! -f "$TAR" ]]; then
    echo "==> $TAR missing; building it"
    ./scripts/build-linux.sh "$ARCH"
  fi
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  tar -xzf "$TAR" -C "$TMP"
  BIN=$(find "$TMP" -type f -name tokenme | head -1)
  [[ -n "$BIN" ]] || { echo "no tokenme binary inside $TAR" >&2; exit 1; }
  install -m 0755 "$BIN" "$OUT/tokenme-$ARCH"
  rm -rf "$TMP"
  trap - EXIT
  echo "bundled: $OUT/tokenme-$ARCH ($(du -h "$OUT/tokenme-$ARCH" | cut -f1))"
done
