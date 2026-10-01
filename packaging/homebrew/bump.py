#!/usr/bin/env python3
"""Rewrites the homebrew tap's formula and cask to a released version.

Usage: bump.py <version> <sha256-file> <tap-dir>

The version and the two sha256 pairs live in the tap's Formula/Casks copies
(mirrored verbatim from packaging/homebrew/ in the main repo); this script
stamps the freshly published version and the hashes from the release's own
sha256.txt, keyed by filename suffix so the version-stamped DMG name follows.
"""

import re
import sys
import pathlib

def main() -> None:
    version, sha_file, tap = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
    hashes = {}
    for line in sha_file.read_text().splitlines():
        if line.strip():
            digest, name = line.split()
            hashes[name] = digest

    formula = tap / "Formula" / "tokenme-cli.rb"
    text = re.sub(r'version "[^"]+"', f'version "{version}"', formula.read_text(), count=1)
    for target in ("aarch64", "x86_64"):
        name = f"tokenme-cli-{target}-apple-darwin.tar.gz"
        text = re.sub(
            rf'(tokenme-cli-{target}-apple-darwin\.tar\.gz"\n\s+sha256 ")[0-9a-f]{{64}}',
            lambda m, h=hashes[name]: m.group(1) + h,
            text,
        )
    formula.write_text(text)

    cask = tap / "Casks" / "tokenme.rb"
    text = re.sub(r'version "[^"]+"', f'version "{version}"', cask.read_text(), count=1)
    arm = next(h for name, h in hashes.items() if name.endswith("_arm64.dmg"))
    uni = next(h for name, h in hashes.items() if name.endswith("universal.dmg"))
    text = re.sub(
        r'(tokenme_[^"]+_arm64\.dmg"\n\s+sha256 ")[0-9a-f]{64}',
        lambda m: m.group(1) + arm,
        text,
    )
    text = re.sub(
        r'(tokenme_universal\.dmg"\n\s+sha256 ")[0-9a-f]{64}',
        lambda m: m.group(1) + uni,
        text,
    )
    cask.write_text(text)
    print(f"tap bumped to {version}")

if __name__ == "__main__":
    main()
