#!/usr/bin/env python3
"""Render the tray glyph at the exact pixel sizes the Windows taskbar draws it.

Explorer paints a tray icon at GetSystemMetrics(SM_CXSMICON) physical pixels
(16 at 100% scaling, 20 at 125%, 24 at 150% ...), but the tray used to ship
the macOS 44px menu-bar asset on every platform, so Explorer did the downscale
itself — a non-integer 44→20 squash that broke the thin arcs into uneven
strokes on high-DPI screens. This script pre-renders design/tray-icon.svg
natively at each size Windows asks for (icons/tray-icon-<size>.png); tray.rs
picks the nearest one at startup and the glyph is never rescaled on screen.

Same pipeline as the 44px macOS asset (see design/README.md): headless Edge
— the one renderer on every Windows dev box here, and the one that honors
SVG2 pathLength, which the dasharray arcs depend on — at 8x the target size,
then area-averaged down, so each delivered size is a native anti-aliased
render rather than a resample of another raster. Black + alpha, like the
macOS asset: the glyph has no color of its own.

The 44px tray-icon.png stays untouched — that is the macOS menu-bar size and
it already renders correctly there. --check verifies the committed PNGs
without writing (exit 1 on drift).
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SVG = ROOT / "apps" / "tokenme-bar" / "design" / "tray-icon.svg"
ICON_DIR = ROOT / "apps" / "tokenme-bar" / "src-tauri" / "icons"

SIZES = (16, 20, 24, 28, 32)
SS = 8  # capture at size*SS, area-average down to size

# Edge headless is the one renderer guaranteed on every Windows dev box here;
# the install roots arrive via the environment, never spelled as drive paths.
EDGE_CANDIDATES = [
    Path(os.environ.get("ProgramFiles(x86)", "")) / "Microsoft/Edge/Application/msedge.exe",
    Path(os.environ.get("ProgramFiles", "")) / "Microsoft/Edge/Application/msedge.exe",
]


def find_edge() -> Path:
    edge = next((e for e in EDGE_CANDIDATES if e.exists()), None)
    if edge is None:
        sys.exit("msedge.exe not found; install Edge or extend EDGE_CANDIDATES")
    return edge


def render(edge: Path, size: int) -> Image.Image:
    tmp = Path(tempfile.mkdtemp(prefix="tray-render-"))
    big = tmp / "glyph.png"
    try:
        # Edge lays an SVG document out as if the viewport were wider than the
        # window and clips the right side — unless the root element carries
        # absolute width/height (percentages do not help; hero renders pass
        # through unshaped only because their canvas is full-bleed). The
        # capture size is injected into a temp copy; the design source stays
        # as committed.
        sized = re.sub(r"<svg ", f'<svg width="{size * SS}" height="{size * SS}" ', SVG.read_text(encoding="utf-8"), count=1)
        source = tmp / "sized.svg"
        source.write_text(sized, encoding="utf-8")
        subprocess.run(
            [str(edge), "--headless", "--disable-gpu", "--hide-scrollbars",
             "--no-first-run", "--no-default-browser-check",
             "--default-background-color=00000000",
             f"--user-data-dir={tmp}",
             f"--window-size={size * SS},{size * SS}",
             f"--screenshot={big}",
             source.resolve().as_uri()],
            capture_output=True, timeout=60, check=True,
        )
        captured = Image.open(big)
        captured.load()  # pixels out before the tmp dir goes away
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    if captured.size != (size * SS, size * SS):
        sys.exit(f"edge rendered {captured.size} for size {size}, expected {(size * SS, size * SS)}")
    # The glyph is pure black, so the whole render is carried by the alpha
    # channel: average the coverage down and rebuild black + alpha.
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.putalpha(captured.convert("RGBA").getchannel("A").resize((size, size), Image.BOX))
    return out


def main() -> int:
    check = "--check" in sys.argv
    edge = find_edge()
    drift = []
    for size in SIZES:
        fresh = render(edge, size)
        path = ICON_DIR / f"tray-icon-{size}.png"
        if check:
            if not path.exists():
                drift.append(f"{path}: missing")
            elif Image.open(path).convert("RGBA").tobytes() != fresh.tobytes():
                drift.append(f"{path}: drifted")
            continue
        fresh.save(path)
        print(path)
    if check:
        for line in drift:
            print(line, file=sys.stderr)
        return 1 if drift else 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
