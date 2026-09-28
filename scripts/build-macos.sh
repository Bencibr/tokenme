#!/usr/bin/env bash
# Build the macOS menu-bar app: .app, ad-hoc signature, drag-install .dmg.
# No developer certificate by design (see README) — Gatekeeper is cleared once,
# with a right-click open.
set -euo pipefail

cd "$(dirname "$0")/.."
APP="apps/tokenme-bar"
BUNDLE="$APP/src-tauri/target/release/bundle"
APP_DIR="$BUNDLE/macos"
DMG_DIR="$BUNDLE/dmg"

if [[ "${1:-}" == "--dev" ]]; then
  cd "$APP" && exec pnpm tauri dev
fi

command -v pnpm >/dev/null || { echo "pnpm is required (https://pnpm.io)"; exit 1; }
command -v cargo >/dev/null || { echo "a Rust toolchain is required"; exit 1; }

# The build id: bumped on every full build and printed, so "which build is
# running" is one glance at the settings footer or the tray tooltip.
BUILD_ID_FILE="$APP/src-tauri/BUILD_ID"
BUILD=$(( $(cat "$BUILD_ID_FILE" 2>/dev/null || echo 0) + 1 ))
echo "$BUILD" > "$BUILD_ID_FILE"
echo "==> tokenme build id: $BUILD"

# A stale dist/ would be bundled silently, so build it here rather than trusting it.
(cd "$APP" && pnpm install --frozen-lockfile --prefer-offline && pnpm build)
cargo test --workspace --all-targets
(cd "$APP" && pnpm tauri build)

# Tauri leaves the bundle carrying only the linker's ad-hoc signature, and
# `codesign -v` rejects that ("code has no resources but signature indicates they
# must be present", because Info.plist is not bound) while the bundle id reads
# `tokenme-<hash>` instead of `dev.tokenme.bar`. Re-signing ad-hoc binds the
# plist and gives the app a stable identity — which is also what a keychain ACL
# (e.g. Qoder's `Safe Storage` item) is matched against.
codesign --force --sign - "$APP_DIR/tokenme.app"
codesign -v "$APP_DIR/tokenme.app"

# The drag-install image, built with hdiutil rather than Tauri's bundle_dmg.sh:
# that script drives Finder through AppleScript to arrange icons, so it fails
# anywhere without Accessibility permission and leaves a half-written rw-*.dmg.
hdiutil detach /Volumes/tokenme -quiet 2>/dev/null || true
VERSION=$(grep -m1 '^version = ' "$APP/src-tauri/Cargo.toml" | cut -d'"' -f2)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP_DIR/tokenme.app" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p "$DMG_DIR"
rm -f "$DMG_DIR"/*.dmg "$APP_DIR"/rw.*.dmg
hdiutil create -quiet -volname tokenme -srcfolder "$STAGE" -ov -format UDZO \
  "$DMG_DIR/tokenme_${VERSION}_$(uname -m).dmg"

echo
echo "artifacts:"
find "$BUNDLE" -maxdepth 2 \( -name '*.app' -o -name '*.dmg' \) -print | sed "s|^|  |"
