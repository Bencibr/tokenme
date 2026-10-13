#!/usr/bin/env bash
# Build the Tauri desktop panel; build-linux.sh builds the headless collector.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Linux ]] || { echo "Run the desktop build on Linux." >&2; exit 1; }
FAST=0
case "${1:-}" in
  "") ;;
  --fast) FAST=1 ;;
  *) echo "usage: $0 [--fast]" >&2; exit 1 ;;
esac
for TOOL in cargo pnpm python3 pkg-config; do
  command -v "$TOOL" >/dev/null || { echo "$TOOL is required." >&2; exit 1; }
done
pkg-config --exists gtk+-3.0 webkit2gtk-4.1 ayatana-appindicator3-0.1
python3 scripts/check-theme-parity.py
python3 scripts/check-i18n-parity.py
if [[ ! -s apps/tokenme-bar/src-tauri/resources/collector/tokenme-x86_64 || ! -s apps/tokenme-bar/src-tauri/resources/collector/tokenme-aarch64 ]]; then
  bash scripts/bundle-collector.sh
fi
if [[ "$FAST" == 0 ]]; then
  cargo test --workspace --locked
  (cd apps/tokenme-bar/src-tauri && cargo test --locked)
fi
cd apps/tokenme-bar
pnpm install --frozen-lockfile
pnpm tauri build --bundles deb
sha256sum src-tauri/target/release/bundle/deb/*.deb
