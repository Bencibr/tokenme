#!/usr/bin/env bash
# Build the macOS menu-bar app: .app, ad-hoc signature, drag-install .dmg.
# No developer certificate by design (see README) — Gatekeeper is cleared once,
# with a right-click open.
#
# ./scripts/build-macos.sh --fast skips the test gate and the DMG for
# frontend-style iteration — relink the .app and stop. The default path stays
# the release gate: tests always run unless the crate sources are unchanged
# since the last green run (see the stamp below).
set -euo pipefail

cd "$(dirname "$0")/.."
APP="apps/tokenme-bar"
BUNDLE="$APP/src-tauri/target/release/bundle"
APP_DIR="$BUNDLE/macos"
DMG_DIR="$BUNDLE/dmg"
FAST=0

if [[ "${1:-}" == "--dev" ]]; then
  cd "$APP" && exec pnpm tauri dev
fi
[[ "${1:-}" == "--fast" ]] && FAST=1

command -v pnpm >/dev/null || { echo "pnpm is required (https://pnpm.io)"; exit 1; }
command -v cargo >/dev/null || { echo "a Rust toolchain is required"; exit 1; }

# The build id: bumped on every full build and printed, so "which build is
# running" is one glance at the settings footer or the tray tooltip.
BUILD_ID_FILE="$APP/src-tauri/BUILD_ID"
BUILD=$(( $(cat "$BUILD_ID_FILE" 2>/dev/null || echo 0) + 1 ))
echo "$BUILD" > "$BUILD_ID_FILE"
echo "==> tokenme build id: $BUILD"
T0=$SECONDS

# A stale dist/ would be bundled silently, so build it here rather than trusting it.
t=$SECONDS
(cd "$APP" && pnpm install --frozen-lockfile --prefer-offline && pnpm build)
echo "==> frontend: $((SECONDS - t))s"

# theme.css declares the dark palette twice over (the OS media query and the
# 深色 pin) and drift between them is silent: follow-system once shipped a
# settings sheet with no mask at all. Sub-second, and it guards a source file
# the cargo gate below never looks at, so it runs even with --fast.
python3 scripts/check-theme-parity.py

# A console child of a GUI process gets a visible console window on Windows, and
# the cargo gate cannot see that class at all: it is a spawn flag, not a type
# error. The Copilot probe shipped exactly that bug (a black box stealing focus
# once per TTL). Sub-second, so it runs even with --fast.
python3 scripts/check-no-window-spawns.py

# The test gate. A workspace test pass recompiles every crate in debug —
# minutes that a frontend-only iteration pays for nothing. Hash everything
# under the crates and the app's Rust tree (sources, fixtures, manifests);
# BUILD_ID is excluded: it churns every build but only reaches the app crate
# via build.rs, never the test targets. The stamp survives only a green run.
TEST_STAMP=".cargo-test-stamp"
rust_hash() {
  find crates "$APP/src-tauri" -type f \
    ! -path '*/target/*' ! -name BUILD_ID -print0 \
    | sort -z | xargs -0 shasum -a 256 | shasum -a 256 | cut -d' ' -f1
}
t=$SECONDS
if (( FAST )); then
  echo "==> tests: skipped (--fast)"
elif [[ -f "$TEST_STAMP" && "$(cat "$TEST_STAMP")" == "$(rust_hash)" ]]; then
  echo "==> tests: skipped (crate sources unchanged since the last green run)"
else
  cargo test --workspace --all-targets
  # The panel's Rust lives in its own workspace (root Cargo.toml excludes
  # apps/tokenme-bar), so the run above never compiles it — and the shipped
  # binary is the one it produces. The anchor and engine-loop tests caught real
  # bugs and are invisible to the gate unless asked for directly.
  (cd "$APP/src-tauri" && cargo test)
  rust_hash > "$TEST_STAMP"
  echo "==> tests: $((SECONDS - t))s"
fi

t=$SECONDS
# The panel bundles the two static linux collectors it uploads to servers;
# stage them when missing so a fresh checkout builds a working app unassisted.
if [[ ! -x "$APP/src-tauri/resources/collector/tokenme-x86_64" \
   || ! -x "$APP/src-tauri/resources/collector/tokenme-aarch64" ]]; then
  echo "==> collector resources missing; staging them"
  ./scripts/bundle-collector.sh
fi
(cd "$APP" && pnpm tauri build)
echo "==> tauri build: $((SECONDS - t))s"

# Tauri leaves the bundle carrying only the linker's ad-hoc signature, and
# `codesign -v` rejects that ("code has no resources but signature indicates they
# must be present", because Info.plist is not bound) while the bundle id reads
# `tokenme-<hash>` instead of `dev.tokenme.bar`. Re-signing ad-hoc binds the
# plist and gives the app a stable identity — which is also what a keychain ACL
# (e.g. Qoder's `Safe Storage` item) is matched against.
codesign --force --sign - "$APP_DIR/TokenMe.app"
codesign -v "$APP_DIR/TokenMe.app"

if (( ! FAST )); then
  # The drag-install image, built with hdiutil rather than Tauri's bundle_dmg.sh:
  # that script drives Finder through AppleScript to arrange icons, so it fails
  # anywhere without Accessibility permission and leaves a half-written rw-*.dmg.
  hdiutil detach /Volumes/tokenme -quiet 2>/dev/null || true
  VERSION=$(grep -m1 '^version = ' "$APP/src-tauri/Cargo.toml" | cut -d'"' -f2)
  STAGE=$(mktemp -d)
  trap 'rm -rf "$STAGE"' EXIT
  cp -R "$APP_DIR/TokenMe.app" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  mkdir -p "$DMG_DIR"
  rm -f "$DMG_DIR"/*.dmg "$APP_DIR"/rw.*.dmg
  hdiutil create -quiet -volname tokenme -srcfolder "$STAGE" -ov -format UDZO \
    "$DMG_DIR/tokenme_${VERSION}_$(uname -m).dmg"
fi

echo
echo "==> total: $((SECONDS - T0))s"
echo "artifacts:"
find "$BUNDLE" -maxdepth 2 \( -name '*.app' -o -name '*.dmg' \) -print | sed "s|^|  |"
