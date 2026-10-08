#!/usr/bin/env python3
"""Gate: a page section's hairline is drawn by the section *below* it.

`.sec` used to own a `border-bottom`. That is a line with a claim to make — it
separates two blocks — only while a block follows. On the last section of a page
the claim is void and the pixels stay: the 工具 page with one tool showed the row,
a full-width hairline, then a screenful of blank panel before the footer, and the
line read as a rendering defect rather than as design. All four pages had one
(概览 / 工具 / 排行 / 明细, measured 2026-10-08 at 400x1500 over the fixture:
y=580 / 489 / 979 / 811, each with 620-950 px of blank beneath it).

The per-page answer (`.page > .sec:last-child { border-bottom: none }`) fixes the
four pages and nothing else: a new page, a new section order, or a section that
stops being last brings the line back. So the separator moved instead of being
suppressed — `.sec + .sec { border-top }` — which makes the defect
unrepresentable: a line exists exactly where two sections meet, nowhere else.

This parses panel.css and fails on:
  - a section-targeting rule that draws a bottom edge (`border-bottom`, or the
    `border` shorthand that implies one),
  - the absence of a sibling-combinator rule carrying the section `border-top`
    (a divider nobody draws is a panel with no separators at all).

What it cannot see: a stranded line drawn by something that is not a section (a
row rule inside a sheet, say). That surface is out of this file's scope.

Usage: check-section-dividers.py [path-to-panel.css]
"""

import re
import sys
from pathlib import Path

DEFAULT_CSS = (
    Path(__file__).resolve().parent.parent
    / "apps/tokenme-bar/src/styles/panel.css"
)

# `.sec` as its own compound — `.sec-hd` / `.sec-fill` are a section's parts,
# not the section box that carries the divider.
SECTION = re.compile(r"\.sec\b(?![-\w])")
DECL = re.compile(r"([a-z-]+)\s*:\s*([^;]+)")


def blocks(css: str, context: str = ""):
    """Yield (selector, body) for every rule block, comments already gone.

    Depth-aware, not a `[^{}]+` regex: the two @media blocks in this file nest
    rules, and a regex reads an at-rule's selector as a rule and its inner body
    as declarations. A nested selector comes back prefixed with its at-rule so a
    `.sec { border-bottom }` hiding under `@media` is still attributable.
    """
    start = 0
    while True:
        brace = css.find("{", start)
        if brace < 0:
            return
        selector = css[start:brace].strip()
        depth, i = 1, brace + 1
        while i < len(css) and depth:
            if css[i] == "{":
                depth += 1
            elif css[i] == "}":
                depth -= 1
            i += 1
        body = css[brace + 1 : i - 1]
        if selector.startswith("@"):
            if selector.startswith(("@media", "@supports", "@layer")):
                yield from blocks(body, f"{context} {selector}".strip())
        else:
            yield (f"{context} {selector}".strip() if context else selector), body
        start = i


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_CSS
    raw = path.read_text(encoding="utf-8")
    css = re.sub(r"/\*.*?\*/", "", raw, flags=re.S)

    findings = []
    divider = None
    for selector, body in blocks(css):
        if not SECTION.search(selector):
            continue
        for prop, value in DECL.findall(body):
            if prop in ("border-bottom", "border") and value.strip() != "none":
                findings.append(
                    f"{selector} {{ {prop}: {value.strip()} }} — a section that "
                    "owns its bottom edge strands a hairline under the page's "
                    "last section"
                )
            if prop == "border-top" and "+" in selector and value.strip() != "none":
                divider = selector
    if divider is None:
        findings.append(
            "no `.sec + .sec { border-top: … }` rule — the section divider has "
            "no owner, so sections run into each other"
        )

    if findings:
        print(f"FAIL {path}: section dividers")
        for f in findings:
            print(f"  - {f}")
        return 1
    print(f"OK {path.name}: section hairlines belong to the section below")
    return 0


if __name__ == "__main__":
    sys.exit(main())
