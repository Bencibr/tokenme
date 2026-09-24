#!/usr/bin/env python3
"""Rebuild the cline fixtures from the real transcript on disk."""
import json, os, pathlib

FIX = pathlib.Path(__file__).resolve().parent
SRC = pathlib.Path.home() / ".cline/data/sessions/1787490736874_djat1"
doc = json.load(open(SRC / "1787490736874_djat1.messages.json"))
meta = json.load(open(SRC / "1787490736874_djat1.json"))
asst = [m for m in doc["messages"] if m.get("role") == "assistant" and m.get("metrics")]

# The same transcript, with the bodies and every local coordinate stripped:
# token counts, ids and timestamps are what the parser reads, the prose is
# another project's session and has no business in this repository.
def redact_text(value):
    return str(value).replace(str(pathlib.Path.home()), "/Users/dev")

def redact(node):
    if isinstance(node, dict):
        return {k: ("…" if k in ("prompt", "title", "text", "system_prompt") and not isinstance(v, (int, float))
                    else "0" * 40 if k == "ref" and isinstance(v, str)
                    else "https://git.example/apppty.git" if k == "url"
                    else redact(v)) for k, v in node.items()}
    if isinstance(node, list):
        return [redact(v) for v in node]
    if isinstance(node, str):
        return redact_text(node)
    return node

whole = redact(doc)
whole["messages"] = [trim(m) for m in whole["messages"]]
(FIX / "cline-djat1.messages.json").write_text(json.dumps(whole, indent=2, ensure_ascii=False) + "\n")
(FIX / "cline-djat1.json").write_text(json.dumps(redact(meta), indent=2, ensure_ascii=False) + "\n")

# the same file as Cline rewrites it: snapshot before the last message lands.
# Assistant-bearing rows only, content arrays trimmed, one whole-file snapshot
# per line so a test can feed the two generations separately.
def trim(m):
    m = dict(m)
    if isinstance(m.get("content"), list):
        m["content"] = [{"type": c.get("type", "text"), "text": "…"} if isinstance(c, dict) else c for c in m["content"]]
    return m

msgs = [trim(m) for m in doc["messages"]]
head = {"version": doc["version"], "updated_at": doc["updated_at"], "agent": "lead",
        "sessionId": "1787490736874_djat1", "messages": msgs[:-1]}
tail = dict(head, messages=msgs)
(FIX / "rewritten-twice.jsonl").write_text(
    json.dumps(head, separators=(",", ":")) + "\n" + json.dumps(tail, separators=(",", ":")) + "\n")

# one assistant row per shape read() must cope with, in the real envelope
def row(id_, ts, metrics=None, role="assistant", model="deepseek/deepseek-v4-flash"):
    m = {"id": id_, "role": role, "ts": ts, "version": 1,
         "modelInfo": {"id": model, "provider": "cline"},
         "content": [{"type": "text", "text": "ok"}]}
    if metrics is not None:
        m["metrics"] = metrics
    return m

mixed = {"version": 1, "agent": "lead", "sessionId": "3_ts_mixed", "messages": [
    row("user_0", 1788497041733, None, "user"),
    row("msg_ok", 1788497041736, {"inputTokens": 6222, "outputTokens": 122,
                                  "cacheReadTokens": 0, "cacheWriteTokens": 0, "totalCost": 0.031}),
    row("msg_null_cw", 1788497042000, {"inputTokens": 8000, "outputTokens": 30,
                                       "cacheReadTokens": None, "cacheWriteTokens": None}),
    {"id": "msg_no_metrics", "role": "assistant", "ts": 1788497042100, "content": []},
    row("msg_dup", 1788497042200, {"inputTokens": 9000, "outputTokens": 40,
                                   "cacheReadTokens": 6000, "cacheWriteTokens": 0}),
    row("msg_dup", 1788497042300, {"inputTokens": 9500, "outputTokens": 41,
                                   "cacheReadTokens": 9000, "cacheWriteTokens": 0}),
    row("msg_no_ts", None, {"inputTokens": 1, "outputTokens": 1, "cacheReadTokens": 0, "cacheWriteTokens": 0}),
    {"id": "msg_str_ts", "role": "assistant", "ts": "1788497042400",
     "modelId": "cline-deepseek-v4-flash",
     "metrics": {"inputTokens": 1200, "outputTokens": 5, "cacheReadTokens": 0, "cacheWriteTokens": 0}},
]}
(FIX / "mixed-rows.messages.json").write_text(json.dumps(mixed, indent=2) + "\n")

# same message id in two sessions -> a bare `msg_dup` key would collide globally
d = FIX / "duplicate-id-across-sessions"
d.mkdir(exist_ok=True)
for s in ("1_ts_a", "2_ts_b"):
    body = {"version": 1, "agent": "lead", "sessionId": s,
            "messages": [row("msg_dup", 1788497041736, {"inputTokens": 100, "outputTokens": 2,
                                                        "cacheReadTokens": 0, "cacheWriteTokens": 0})]}
    (d / f"{s}.messages.json").write_text(json.dumps(body, indent=2) + "\n")

(FIX / "malformed.messages.json").write_text('{"version":"1.0","messages":[{"id":"msg_x",')
(FIX / "not-an-object.messages.json").write_text("[1, 2, 3]\n")
(FIX / "null-messages.messages.json").write_text('{"messages": null}\n')

for stale in ("cline-session-real.messages.json", "empty-object.messages.json"):
    p = FIX / stale
    if p.exists():
        p.unlink()

print("assistant rows in real transcript:", len(asst))
print("wire sums:", (sum(m['metrics']['inputTokens'] for m in asst),
                     sum(m['metrics']['cacheReadTokens'] for m in asst),
                     sum(m['metrics']['cacheWriteTokens'] for m in asst),
                     sum(m['metrics']['outputTokens'] for m in asst)))
print("meta usage:", meta["metadata"]["usage"], "| aggregate:", meta["metadata"]["aggregateUsage"])
print("first3:", [(m['metrics']['inputTokens'], m['metrics']['cacheReadTokens'], m['metrics']['outputTokens'], m['id'], m['ts']) for m in asst[:3]])
print("cwd:", meta.get("cwd"))
print("events expected in mixed-rows:", 5, "(msg_no_ts skipped, no_metrics/user skipped)")
os.system(f"du -sk {FIX} && ls {FIX}")
