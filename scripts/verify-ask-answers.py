#!/usr/bin/env python3
"""Verify what the user actually answered, from the session log on disk.

The agent's in-context copy of an AskUserQuestion result is not authoritative:
on 2026-10-09 it disagreed with the on-disk record and a wrong reading was
repeated to the user as "you decided X" before a command was planned from it.
This script pairs each AskUserQuestion call with its recorded result, resolves
the answer back to that question's own option list, and exits non-zero on any
unmatched or missing answer — so a decision with side effects can never rest on
an unverified echo.

Usage:
  scripts/verify-ask-answers.py [<session-id-or-jsonl>] [--last N] [--call <tool-use-id>]

Default session: the newest .jsonl under ~/.qoder/projects/-<cwd-with-slashes>/
"""

import json
import pathlib
import sys


def session_log(arg: str | None) -> pathlib.Path:
    if arg and pathlib.Path(arg).suffix == ".jsonl" and pathlib.Path(arg).exists():
        return pathlib.Path(arg)
    root = pathlib.Path.home() / ".qoder" / "projects"
    if arg:
        for p in root.glob(f"*/{arg}.jsonl"):
            return p
        sys.exit(f"session {arg} not found under {root}")
    cwd_tag = "-" + str(pathlib.Path.cwd()).lstrip("/").replace("/", "-")
    cands = sorted((root / cwd_tag).glob("*.jsonl"), key=lambda p: p.stat().st_mtime)
    if not cands:
        sys.exit(f"no session logs for {cwd_tag} under {root}")
    return cands[-1]


def blocks(rows):
    for r in rows:
        content = (r.get("message") or {}).get("content")
        if isinstance(content, list):
            yield from (b for b in content if isinstance(b, dict))
        elif isinstance(content, str):
            yield {"type": "tool_result", "content": content}


def collect(path: pathlib.Path):
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    uses, results = {}, {}
    for b in blocks(rows):
        if b.get("type") == "tool_use" and b.get("name") == "AskUserQuestion":
            uses[b["id"]] = b.get("input") or {}
        elif b.get("type") == "tool_result" and b.get("tool_use_id") in uses:
            txt = b.get("content")
            if isinstance(txt, list):
                txt = " ".join(x.get("text", "") for x in txt if isinstance(x, dict))
            results[b["tool_use_id"]] = str(txt or "")
    return uses, results


def parse_echo(text: str):
    """Turn '... "question"="answer", "question"="answer". You can ...' into pairs."""
    import re

    marker = "answered: "
    body = text.split(marker, 1)[1] if marker in text else text
    pairs = {}
    for q, a in re.findall(r'"((?:[^"\\]|\\.)*)"\s*=\s*"((?:[^"\\]|\\.)*)"', body, re.S):
        pairs[q] = a
    return pairs


def main() -> int:
    import argparse

    ap = argparse.ArgumentParser(description="Read user answers back from the session log.")
    ap.add_argument("session", nargs="?", help="session id or .jsonl path (default: newest for cwd)")
    ap.add_argument("--last", type=int, default=0, help="check only the last N AskUserQuestion calls")
    ap.add_argument("--call", help="check only this tool-use id")
    ns = ap.parse_args()

    path = session_log(ns.session)
    uses, results = collect(path)
    ids = [i for i in uses if (ns.call is None or i == ns.call)]
    if ns.call is not None and not ids:
        print(f"{path}\ncall {ns.call} is not an AskUserQuestion in this session")
        return 1
    if ns.last:
        ids = ids[-ns.last:]
    if not ids:
        print(f"{path}\nno AskUserQuestion calls selected")
        return 1

    bad = 0
    print(f"session log: {path}")
    for n, tid in enumerate(ids, 1):
        echo = results.get(tid)
        print(f"\n=== call {n}  id={tid}")
        if echo is None:
            print("  NO RESULT RECORDED — treat as unanswered")
            bad += 1
            continue
        pairs = parse_echo(echo)
        for qi, q in enumerate(uses[tid].get("questions", []), 1):
            text = q.get("question", "")
            labels = [o.get("label", "") for o in q.get("options", [])]
            hit = next((v for k, v in pairs.items() if k == text), None)
            if hit is None:
                fuzzy = next((v for k, v in pairs.items() if k[:24] == text[:24]), None)
                hit = fuzzy
            if hit is None:
                print(f"  Q{qi} [{q.get('header')}] -> MISSING (asked, never answered)")
                bad += 1
                continue
            idx = labels.index(hit) + 1 if hit in labels else 0
            if idx:
                print(f"  Q{qi} [{q.get('header')}] -> option {idx}/{len(labels)}: {hit}")
            else:
                print(f"  Q{qi} [{q.get('header')}] -> OTHER (free text): {hit}")
    print("\nVERDICT:", "ALL ANSWERS MATCH THEIR OWN QUESTION" if not bad else f"{bad} PROBLEM(S) — do not act on these answers")
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
