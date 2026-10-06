#!/usr/bin/env bash
# Build the Linux CLI: fully static musl binaries for x86_64 and aarch64,
# bundled with install-linux.sh and a sha256 sums file.
#
# Cross-compiled with cargo-zigbuild (zig as the linker), so no Linux machine
# or rustup target-specific toolchain is needed. The resulting binary runs on
# any distro — no glibc, no Rust, no Python on the collector.
#
#   ./scripts/build-linux.sh            # both architectures
#   ./scripts/build-linux.sh x86_64     # one of: x86_64, aarch64
set -euo pipefail
cd "$(dirname "$0")/.."

ARCHES=("x86_64" "aarch64")
if [[ -n "${1:-}" ]]; then
  case "$1" in
    x86_64|aarch64) ARCHES=("$1") ;;
    *) echo "usage: $0 [x86_64|aarch64]"; exit 1 ;;
  esac
fi

command -v cargo >/dev/null || { echo "a Rust toolchain is required"; exit 1; }
command -v cargo-zigbuild >/dev/null || {
  echo "cargo-zigbuild is required: cargo install cargo-zigbuild"; exit 1
}
command -v zig >/dev/null || { echo "zig is required (https://ziglang.org)"; exit 1; }

V=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST="dist/linux"
mkdir -p "$DIST"

for ARCH in "${ARCHES[@]}"; do
  TARGET="${ARCH}-unknown-linux-musl"
  echo "==> tokenme $V for $TARGET"
  rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
  cargo zigbuild --release -p usage-cli --target "$TARGET"

  BIN="target/$TARGET/release/tokenme"
  PKG="tokenme-cli-$V-$ARCH-linux-musl"
  STAGE="$DIST/$PKG"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  cp "$BIN" "$STAGE/tokenme"
  cp scripts/install-linux.sh "$STAGE/install-linux.sh"
  chmod +x "$STAGE/tokenme" "$STAGE/install-linux.sh"

  TAR="$DIST/$PKG.tar.gz"
  # COPYFILE_DISABLE + --no-xattrs: without them macOS tar carries AppleDouble
  # `._*` siblings and xattr header lines (com.apple.provenance), which GNU tar
  # then warns about on every extract.
  COPYFILE_DISABLE=1 tar --no-xattrs -C "$DIST" -czf "$TAR" "$PKG"
  rm -rf "$STAGE"
  echo "    $(du -h "$TAR" | cut -f1)  $TAR"
done

( cd "$DIST" && shasum -a 256 ./*.tar.gz > sha256.txt )
echo "==> sums:"
cat "$DIST/sha256.txt"
