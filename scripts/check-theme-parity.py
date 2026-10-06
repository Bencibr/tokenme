#!/usr/bin/env python3
"""Gate: the dark token blocks in theme.css must stay mechanically identical.

theme.css declares the dark palette twice — once under the OS media query
(follow-system mode) and once under the explicit 深色 pin — because plain CSS
cannot share a block between them. The file's own comments make that identity
a contract, and drift is silent: 2026-10-05 the media block was missing
--today/--scrim, so follow-system + OS-dark fell back to the light scrim (a
dark veil on the dark wallpaper: the sheet lost its mask), and the pin block
had kept a stale --cat-18 after the other two blocks were repainted.

This parses the three colour blocks (light :root, media, pin) and fails on:
  - a name the light block lacks but the dark blocks have (or vice versa),
  - token order drift between light and the dark blocks,
  - any value drift between media and pin (whitespace-normalized, since the
    two blocks indent their multi-line values differently by design).

Usage: check-theme-parity.py [path-to-theme.css]
"""

import re
import sys
from pathlib import Path

DEFAULT_CSS = (
    Path(__file__).resolve().parent.parent / "apps/tokenme-bar/src/styles/theme.css"
)
LIGHT = ":root {"
MEDIA = "@media (prefers-color-scheme: dark)"
PIN = ':root[data-theme="dark"] {'
TOKEN = re.compile(r"--([a-zA-Z0-9-]+)\s*:\s*([^;]+);")


def die(msg):
    print(f"theme parity: FAIL — {msg}")
    sys.exit(1)


def rule_body(text, marker):
    start = text.find(marker)
    if start < 0:
        die(f"block marker not found: {marker!r} — did theme.css get restructured?")
    end = text.find("}", start)
    if end < 0:
        die(f"unterminated block at {marker!r}")
    return text[start:end]


def tokens(body):
    order, values = [], {}
    for match in TOKEN.finditer(body):
        name = match.group(1)
        if name not in values:
            order.append(name)
        values[name] = " ".join(match.group(2).split())
    return order, values


def main():
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_CSS
    text = path.read_text(encoding="utf-8")

    light_o, light_v = tokens(rule_body(text, LIGHT))
    media_o, media_v = tokens(rule_body(text, MEDIA))
    pin_o, pin_v = tokens(rule_body(text, PIN))

    problems = []
    for a_name, a_o, a_v, b_name, b_o, b_v in (
        ("media", media_o, media_v, "pin", pin_o, pin_v),
        ("light", light_o, light_v, "pin", pin_o, pin_v),
    ):
        missing = [k for k in b_o if k not in a_v]
        extra = [k for k in a_o if k not in b_v]
        if missing:
            problems.append(f"{a_name} is missing tokens that {b_name} has: {missing}")
        if extra:
            problems.append(f"{a_name} has tokens that {b_name} lacks: {extra}")
        if not missing and not extra and a_o != b_o:
            i = next(i for i, (x, y) in enumerate(zip(a_o, b_o)) if x != y)
            problems.append(
                f"{a_name}/{b_name} token order diverges at index {i}: "
                f"--{a_o[i]} vs --{b_o[i]}"
            )

    for k in pin_o:
        if k in media_v and media_v[k] != pin_v[k]:
            problems.append(
                f"value drift for --{k}: media={media_v[k]!r} pin={pin_v[k]!r}"
            )

    if problems:
        die("the dark token blocks have drifted:\n  - " + "\n  - ".join(problems))

    print(
        f"theme parity: OK — {len(light_o)} tokens; light/media/pin names aligned, "
        f"the two dark blocks mechanically identical ({path})"
    )


if __name__ == "__main__":
    main()
