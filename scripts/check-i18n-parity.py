#!/usr/bin/env python3
"""Gate: the zh and en dictionaries must carry the same keys AND the same
placeholders, every key referenced from source must exist in both, and no key
may sit in the dictionary unreachable.

TypeScript already pins the two key sets against each other — `en` is declared
`Record<StrKey, string>` with `StrKey = keyof typeof zh` — so a missing or an
extra translation is a compile error. What that type cannot see:

  - **placeholders that diverge between the two sides**: `{n}` in zh against
    `{count}` in en typechecks, and `t()` substitutes only the names it was
    handed, so the English line renders a literal `{count}` on screen;
  - **a call site handing the wrong name**: `t("quota.bar.a11y", { lab })`
    against a string that says `{label}` is a plain object with an unexpected
    key — TypeScript is happy, the token prints raw;
  - **dead copy**: delete a control and its label/hint stay in both
    dictionaries. Nothing errors, and the next reader cannot tell missing UI
    from missing translation;
  - **which row went wrong**: `t()` answers `undefined` for a missing key
    instead of echoing it, so the whole symptom is one blank row.

Keys reached through an expression (`t(probing ? "a" : "b")`) or carried as
data (`{ key: "set.tab.about" }`) count as referenced: the check matches the
literal anywhere under `src/`, which is exactly how those two shapes reach it.
The same shapes are beyond the call-site check — it reads only a `t()` whose key
is a literal and whose vars are a single-level object on one line, so a composed
key or a spread never gets compared. That is the honest limit, not a gap to
count on.

Usage: check-i18n-parity.py [path-to-i18n.ts]
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_TS = ROOT / "apps/tokenme-bar/src/lib/i18n.ts"
SRC = ROOT / "apps/tokenme-bar/src"

DICT_START = re.compile(r"^const (zh|en)(?::[^=]+)? = \{$")
STARTS = re.compile(r'^\s*"((?:[^"\\]|\\.)*)"\s*:\s*(.*)$')
CLOSED = re.compile(r'",?\s*$')
SKIPPABLE = re.compile(r"^\s*(/\*|\*|//|$)")
PLACEHOLDER = re.compile(r"\{(\w+)\}")
LITERAL = re.compile(r'"((?:[a-z0-9]+[._-])+[a-z0-9]+)"')
# `t("key", { n: …, off: … })` — the names handed to the filler must be exactly
# the placeholders the string asks for. A call site that renames one (`{t:` while
# the string says `{age}`) renders the token literally, which is what the
# placeholder-rename that motivated this file was at risk of.
CALL = re.compile(r'\bt\(\s*"((?:[^"\\]|\\.)*)"\s*,\s*\{([^{}]*)\}')
# A property name is a word followed by `:` (a real pair) or a word on its own
# (the shorthand `{ rows }`, which hands the local's name). A fragment of a
# value that contained a comma — ` now)` from `relativeTime(a, now)` — is
# neither, and is dropped rather than read as a name.
PAIR = re.compile(r"^\s*(\w+)\s*:")
SHORTHAND = re.compile(r"^\s*(\w+)\s*$")


def parse(text):
    """{dict: {key: placeholders}} plus the lines inside a dictionary the
    reader could not place — the gate refuses to skip what it cannot read."""
    dicts, dangling, name, depth, block = {}, [], None, 0, []
    for lineno, line in enumerate(text.splitlines(), 1):
        if name is None:
            hit = DICT_START.match(line)
            if hit:
                name, depth, block = hit.group(1), 1, []
            continue
        depth += line.count("{") - line.count("}")
        if depth == 0:
            if line.strip() not in {"};", "}"}:
                dangling.append((lineno, line))
            dicts[name] = collect(block, dangling, name)
            name = None
            continue
        block.append((lineno, line))
    if name is not None:
        raise SystemExit(f"i18n-parity: the `{name}` dictionary never closes")
    return dicts, dangling


def collect(block, dangling, name):
    # One record per entry, where a record is a key line plus however many
    # continuation lines it takes to close the value — two entries write their
    # text across a line break.
    out, open_at, parts = {}, None, []
    for lineno, line in block:
        if SKIPPABLE.match(line):
            continue
        hit = STARTS.match(line)
        if open_at is None:
            # A record opens with a quoted value, or with nothing at all when
            # the text continues on the next line. Anything else (an unquoted
            # value) is reported, never absorbed into the entry below it.
            if not hit or (hit.group(2).strip() not in {""} and not hit.group(2).lstrip().startswith('"')):
                dangling.append((lineno, line))
                continue
            open_at, parts = lineno, [hit.group(1), hit.group(2)]
        else:
            parts[1] += "\n" + line.strip()
        if CLOSED.search(line) or (parts and CLOSED.search(parts[1])):
            if open_at in out:
                dangling.append((open_at, f"duplicate key in {name}"))
            out[open_at] = (parts[0], set(PLACEHOLDER.findall(parts[1])))
            open_at, parts = None, []
    if open_at is not None:
        dangling.append((open_at, "value never closed"))
    return out


def referenced():
    """Every dotted string literal outside the dictionaries themselves."""
    hit = set()
    for path in sorted(SRC.rglob("*.ts")) + sorted(SRC.rglob("*.tsx")):
        if path.name == "i18n.ts":
            continue
        hit.update(LITERAL.findall(path.read_text(encoding="utf-8")))
    # The pet selector reads labelKey from its JSON packages. These labels are
    # live UI copy even though their keys no longer occur in a TS switch.
    manifest = SRC / "assets/pets/pet-skins.json"
    if manifest.exists():
        skins = json.loads(manifest.read_text(encoding="utf-8"))["skins"]
        hit.update(skin["labelKey"] for skin in skins.values())
    return hit


def call_vars():
    """(key, the names handed to `t()`, line, file) for every `t("key", { … })`.
    Only a single-level object literal is read; anything else is left alone
    rather than guessed at. The names must equal the string's placeholders:
    hand it `{t:` while the string says `{age}` and the row renders the token
    literally, which is exactly what a placeholder rename invites."""
    out = []
    for path in sorted(SRC.rglob("*.ts")) + sorted(SRC.rglob("*.tsx")):
        if path.name == "i18n.ts":
            continue
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for key, body in CALL.findall(line):
                names = set()
                for piece in body.split(","):
                    hit = PAIR.match(piece) or SHORTHAND.match(piece)
                    if hit:
                        names.add(hit.group(1))
                if names:
                    out.append((key, names, lineno, path))
    return out


def main() -> int:
    target = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_TS
    text = target.read_text(encoding="utf-8")
    dicts, dangling = parse(text)
    if sorted(dicts) != ["en", "zh"]:
        print(f"i18n-parity: expected `zh` and `en` dictionaries, found {sorted(dicts)}")
        return 1

    problems = []
    if dangling:
        problems.append("dictionary lines the reader could not place:")
        problems += [f"    {target}:{line} {body.strip()[:72]}" for line, body in dangling]

    def flat(which):
        table = {}
        for _, (key, ph) in dicts[which].items():
            table.setdefault(key, set()).update(ph)
        return table

    zh, en = flat("zh"), flat("en")
    only_zh, only_en = sorted(set(zh) - set(en)), sorted(set(en) - set(zh))
    if only_zh:
        problems.append(f"{len(only_zh)} key(s) with no English side: {', '.join(only_zh[:8])}")
    if only_en:
        problems.append(f"{len(only_en)} key(s) zh never had: {', '.join(only_en[:8])}")
    swaps = [k for k in sorted(set(zh) & set(en)) if zh[k] != en[k]]
    if swaps:
        problems.append(f"{len(swaps)} key(s) whose `{{var}}` placeholders differ between the two sides:")
        problems += [f"    {k}: zh {sorted(zh[k]) or '—'} / en {sorted(en[k]) or '—'}" for k in swaps[:10]]

    mismatched = [
        (k, names, path, line)
        for (k, names, line, path) in call_vars()
        if (k in zh or k in en) and names != (zh.get(k) or en.get(k))
    ]
    if mismatched:
        problems.append(f"{len(mismatched)} call site(s) handing `t()` other names than the string asks for:")
        problems += [
            f"    {path.relative_to(ROOT)}:{line} {k}: passes {sorted(names)}, "
            f"the string wants {sorted(zh.get(k) or en.get(k))}"
            for k, names, path, line in mismatched[:10]
        ]

    used = referenced()
    half = sorted(k for k in used if (k in zh or k in en) and not (k in zh and k in en))
    if half:
        problems.append(f"{len(half)} key(s) referenced from source but present in only one dictionary: {', '.join(half[:10])}")
    dead = sorted((set(zh) | set(en)) - used)
    if dead:
        problems.append(
            f"{len(dead)} key(s) no source file references — a deleted control leaves its copy "
            f"behind, so remove the key or wire the control back: {', '.join(dead[:14])}"
        )

    if problems:
        print("i18n-parity: FAIL")
        for line in problems:
            print(f"  - {line}")
        return 1
    print(
        f"i18n-parity: OK — {len(zh)} keys in both dictionaries, placeholders aligned, "
        f"every one reachable from source"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
