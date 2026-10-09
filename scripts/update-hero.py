#!/usr/bin/env python3
"""Regenerate the hero banners from the tree's own data.

The banner in assets/ carries three facts that drift: the version (root
Cargo.toml), the tool count (TOOL_IDS in crates/adapters/all/src/lib.rs —
the same registry the README test holds the tool tables to), and the pill
layout that has to grow with its labels. This patches both SVGs, then
renders a 2x PNG beside each so the shipped pixels can be eyeballed without
opening a browser; rendering is skipped when the SVG text did not change and
the PNG already exists, so a no-drift run touches nothing.

Used by scripts/public-deliver.sh before the gate; also fine by hand.
--force re-renders the PNGs even when the SVG text did not drift.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
SVGS = [ASSETS / "tokenme-hero-dark.svg", ASSETS / "tokenme-hero-light.svg"]

# Edge headless is the one renderer guaranteed on every Windows dev box here;
# the install roots arrive via the environment, never spelled as drive paths.
EDGE_CANDIDATES = [
    Path(os.environ.get("ProgramFiles(x86)", "")) / "Microsoft/Edge/Application/msedge.exe",
    Path(os.environ.get("ProgramFiles", "")) / "Microsoft/Edge/Application/msedge.exe",
]

# Pill geometry: text sits 28px in, ~7.28px/char at 13px semibold, 17px tail.
# Reproduces today's 125/147/117 exactly, so an unchanged label is a no-op.
CHAR_PX, TEXT_INSET, TAIL = 7.28, 28, 17
PILL_GAP = 12


def tree_version() -> str:
    for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        m = re.match(r'^version = "(.+)"', line)
        if m:
            return m.group(1)
    sys.exit("no version = line in Cargo.toml [workspace.package]")


def tree_tool_count() -> int:
    src = (ROOT / "crates" / "adapters" / "all" / "src" / "lib.rs").read_text(encoding="utf-8")
    block = re.search(r"pub const TOOL_IDS: &\[&str\] = &\[(.*?)\];", src, re.S)
    if not block:
        sys.exit("TOOL_IDS not found in crates/adapters/all/src/lib.rs")
    ids = re.findall(r'"([^"]+)"', block.group(1))
    return len(ids)


def pill_width(label: str) -> int:
    return TEXT_INSET + round(len(label) * CHAR_PX) + TAIL


def relayout_group(group: str) -> str:
    pat = re.compile(
        r'<rect x="(\d+)" y="0" width="(\d+)"( rx="8"[^/]*)/>'
        r'(\s*<circle[^/]*/>\s*<text x=")(\d+)(" y="22"[^>]*>)([^<]+)(</text>)'
    )

    x = 0
    out = []
    cursor = 0
    for m in pat.finditer(group):
        label = m.group(7)
        width = pill_width(label)
        out.append(group[cursor : m.start()])
        out.append(
            f'<rect x="{x}" y="0" width="{width}"{m.group(3)}'
            f'{m.group(4)}{x}{m.group(6)}{label}{m.group(8)}'
        )
        cursor = m.end()
        x += width + PILL_GAP
    out.append(group[cursor:])
    return "".join(out)


def patch_svg(path: Path, version: str, count: int) -> bool:
    s = path.read_text(encoding="utf-8")
    before = s
    s = re.sub(r">v\d+\.\d+\.\d+ · MIT License<", f">v{version} · MIT License<", s)
    s = re.sub(r">\d+ AI Tools<", f">{count} AI Tools<", s)
    group_pat = re.compile(r'(<g transform="translate\(64, 210\)">)(.*?)(</g>)', re.S)
    s = group_pat.sub(lambda m: m.group(1) + relayout_group(m.group(2)) + m.group(3), s)
    if s == before:
        return False
    path.write_text(s, encoding="utf-8", newline="\n")
    return True


def render(path: Path) -> Path:
    png = path.with_suffix(".png")
    edge = next((e for e in EDGE_CANDIDATES if e.exists()), None)
    if edge is None:
        sys.exit("msedge.exe not found; install Edge or extend EDGE_CANDIDATES")
    tmp = tempfile.mkdtemp(prefix="hero-render-")
    subprocess.run(
        [str(edge), "--headless", "--disable-gpu", "--hide-scrollbars",
         "--no-first-run", "--no-default-browser-check",
         "--default-background-color=00000000",
         f"--user-data-dir={tmp}",
         "--window-size=1920,600",
         f"--screenshot={png}",
         path.resolve().as_uri()],
        capture_output=True, timeout=60, check=True,
    )
    shutil.rmtree(tmp, ignore_errors=True)
    return png


def main() -> None:
    force = "--force" in sys.argv
    version, count = tree_version(), tree_tool_count()
    changed = force
    for svg in SVGS:
        if patch_svg(svg, version, count):
            print(f"patched {svg.name}: v{version} · {count} AI Tools")
            changed = True
    if not changed:
        for svg in SVGS:
            if not svg.with_suffix(".png").exists():
                changed = True
    if changed:
        for svg in SVGS:
            out = render(svg)
            print(f"rendered {out.name}: {out.stat().st_size:,} B")


if __name__ == "__main__":
    main()
