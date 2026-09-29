#!/usr/bin/env bash
# Bump the version in its two homes, refresh latest.json, commit and tag.
# Usage: ./scripts/release.sh 0.2.0
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "${1:-}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: $0 x.y.z"; exit 1; }
V="$1"

sed -i '' "s/^version = \".*\"/version = \"$V\"/" apps/tokenme-bar/src-tauri/Cargo.toml
sed -i '' "s/\"version\": \".*\"/\"version\": \"$V\"/" apps/tokenme-bar/src-tauri/tauri.conf.json
cat > latest.json <<JSON
{
  "version": "$V",
  "url": "https://github.com/Bencibr/tokenme/releases/latest",
  "notes": ""
}
JSON

git add apps/tokenme-bar/src-tauri/Cargo.toml apps/tokenme-bar/src-tauri/tauri.conf.json latest.json
git commit -m "release: v$V"
git tag "v$V"
echo "tagged v$V — push with: git push && git push origin v$V"
