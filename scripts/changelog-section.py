#!/usr/bin/env python3
"""Print the CHANGELOG.md section for one version — the GitHub release body.

release.yml's `notes` job runs this first and publishes the output as the
release page body: GitHub's own generated notes are a bare "Full Changelog"
link on a repo without pull requests, which is all the v0.1.0–v0.1.5 pages
ever said. A missing `## [<version>]` section is exit 1 — the notes job fails
before any build starts — and the same question is a test in
crates/adapters/all/src/lib.rs, so it is answered before the tag exists.
`v` prefixes are accepted.

Usage: scripts/changelog-section.py <version> [path/to/CHANGELOG.md]
Exit 0 with the section on stdout / 1 when it is missing / 2 on usage.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HEADING = re.compile(r"^## \[([^\]]+)\]")


def section(text: str, version: str) -> str | None:
    lines = text.splitlines()
    start = None
    for i, line in enumerate(lines):
        m = HEADING.match(line)
        if m and m.group(1) == version:
            start = i
            break
    if start is None:
        return None
    end = len(lines)
    for j in range(start + 1, len(lines)):
        if HEADING.match(lines[j]):
            end = j
            break
    block = lines[start:end]
    while block and not block[-1].strip():
        block.pop()
    return "\n".join(block) + "\n"


def main() -> int:
    if not 2 <= len(sys.argv) <= 3:
        print("usage: changelog-section.py <version> [path/to/CHANGELOG.md]", file=sys.stderr)
        return 2
    version = sys.argv[1].removeprefix("v")
    path = Path(sys.argv[2]) if len(sys.argv) > 2 else ROOT / "CHANGELOG.md"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        print(f"changelog-section: cannot read {path}: {exc}", file=sys.stderr)
        return 1
    block = section(text, version)
    if block is None:
        print(
            f"changelog-section: no '## [{version}]' section in {path} — the release page "
            "would have no story, so this fails the release instead of publishing an "
            "empty body",
            file=sys.stderr,
        )
        return 1
    sys.stdout.write(block)
    return 0


if __name__ == "__main__":
    sys.exit(main())
