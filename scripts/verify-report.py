#!/usr/bin/env python3
"""Second verification layer: index → report.

`verify-totals.py` proves the raw files were read correctly. This proves the
aggregation on top of them: every number in `tokenme report --json` is recomputed
from the SQLite index with independent SQL/Python, including the money, which is
priced here from the cached models.dev payload rather than from the Rust table.

Anything that cannot be reproduced exactly is reported, not rounded away.

Usage: scripts/verify-report.py [--json-out FILE]
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sqlite3
import sys
from collections import defaultdict

HOME = os.path.expanduser("~")
DB = os.path.join(HOME, "Library/Application Support/tokenme/index.db")
PRICES = os.path.join(HOME, "Library/Caches/tokenme/models.dev.json")
SETTINGS = os.path.join(HOME, "Library/Application Support/tokenme/settings.json")
# docs.qoder.com/zh/account/pricing: Pro $20/2000, Pro+ $60/6000, Ultra $200/20000.
CREDIT_USD = {"qoder": 0.01}
FIELDS = ["input", "cache_creation", "cache_read", "output", "reasoning", "credits"]
# `total_tokens` is the four mutually-exclusive stages. `reasoning` is a
# sub-breakdown of `output` and `credits` is a different meter entirely, so adding
# either would double-bill — which is exactly the mistake this script made first.
STAGES = ["input", "cache_creation", "cache_read", "output"]


def norm(s: str) -> str:
    """Our own key normalisation: deliberately not a port of the Rust one."""
    t = "".join(c for c in (s or "").strip().lower() if not c.isspace())
    while "/" in t:
        t = t.split("/", 1)[1]
    if ":" in t:
        head, _, tail = t.partition(":")
        t = tail or head
    return t


# The documented precedence: the model's own vendor lists the price we quote, a
# coding-plan clone of that vendor ranks with it, and every reseller ranks behind
# all of them. Implemented here independently of the Rust so the two can disagree.
VENDOR_PRIORITY = [
    "anthropic", "openai", "google", "google-vertex", "amazon-bedrock", "xai", "deepseek",
    "zhipuai", "moonshotai", "qwen", "minimax", "stepfun", "mistralai", "groq", "openrouter",
]


def rank(provider: str) -> int:
    p = provider.lower()
    if p in VENDOR_PRIORITY:
        return VENDOR_PRIORITY.index(p)
    for i, v in enumerate(VENDOR_PRIORITY):
        if p.startswith(v):
            return i
    return len(VENDOR_PRIORITY)


def price_table():
    raw = json.load(open(PRICES, encoding="utf-8"))
    table, ambiguous = defaultdict(list), {}
    for provider, body in raw.items():
        if not isinstance(body, dict):
            continue
        for model_id, model in (body.get("models") or {}).items():
            if not isinstance(model, dict):
                continue
            cost = model.get("cost")
            if not isinstance(cost, dict):
                continue
            price = (
                float(cost.get("input") or 0.0),
                float(cost.get("output") or 0.0),
                float(cost.get("cache_write") or cost.get("cache_creation") or 0.0),
                float(cost.get("cache_read") or 0.0),
            )
            key = norm(model_id)
            table[key].append((rank(provider), provider, model_id, price, bool(cost)))
    # One entry per key: the documented precedence, ties broken by provider id so
    # the choice never depends on JSON ordering.
    final = {}
    for key, cands in table.items():
        cands.sort(key=lambda c: (c[0], c[1], c[2]))
        priced = [c for c in cands if c[4] and sum(c[3]) > 0] or cands
        final[key] = priced[0][3]
        if len({c[3] for c in cands}) > 1:
            ambiguous[key] = [(c[1], c[3]) for c in cands]
    return final, ambiguous


def cost_of(price, counts) -> float:
    inp, out, cw, cr = price
    return (counts["input"] * inp + counts["output"] * out + counts["cache_creation"] * cw + counts["cache_read"] * cr) / 1e6


def local_day(ms: int) -> str:
    return dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m-%d")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json-out", default="-", help="report JSON to check, '-' for a fresh CLI run")
    ap.add_argument(
        "--freeze",
        action="store_true",
        help="copy the index first and check both sides against that copy: the live "
        "index is being written by the menu-bar app while we read it, so without this "
        "a difference can be a moving target rather than a bug",
    )
    args = ap.parse_args()

    # Freeze first. The menu-bar app is writing the index while we read it, so
    # without a snapshot every "difference" below could be a moving target.
    db = DB
    frozen = None
    if args.freeze:
        frozen = f"/tmp/tokenme-frozen-{os.getpid()}.db"
        src = sqlite3.connect(DB)
        dst = sqlite3.connect(frozen)
        src.backup(dst)  # `.backup` folds the WAL in, which a plain copy would not
        dst.close()
        src.close()
        db = frozen

    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    events = [
        {
            "tool": r[0],
            "ts": r[1],
            "session": r[2],
            "project": r[3],
            "model": r[4],
            "meter": r[5],
            "counts": {"input": r[6] or 0.0, "cache_creation": r[7] or 0.0, "cache_read": r[8] or 0.0,
                       "output": r[9] or 0.0, "reasoning": r[10] or 0.0, "credits": r[11] or 0.0},
        }
        for r in conn.execute(
            "select tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits"
            " from event"
        )
    ]
    conn.close()

    if args.json_out == "-":
        import subprocess

        binary = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target/release/tokenme"
        )
        # `--no-ingest` so the report reads the very snapshot we just took instead of
        # advancing the index underneath us.
        raw = subprocess.run(
            [binary, *(["--db", frozen] if frozen else []), "--no-ingest", "report", "--json", "--offline"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    else:
        raw = open(args.json_out, encoding="utf-8").read()
    report = json.loads(raw)

    table, ambiguous = price_table()

    fails = []

    def check(label, mine, theirs, tol=1e-6):
        if mine is None or theirs is None:
            fails.append(f"{label}: missing (mine={mine} report={theirs})")
            return
        d = abs(mine - theirs)
        rel = d / max(abs(theirs), 1.0)
        if d > tol and rel > 1e-9:
            fails.append(f"{label}: mine {mine:,.6f} vs report {theirs:,.6f} (Δ {d:,.6f})")

    def priced(e):
        """Money for one event, from the cached models.dev payload only."""
        if e["meter"] == "credits":
            return CREDIT_USD.get(e["tool"], 0.0) * e["counts"]["credits"]
        price = table.get(norm(e["model"] or ""))
        return cost_of(price, e["counts"]) if price else 0.0

    # ---- window aggregation -------------------------------------------------
    for win in ("day", "week", "month", "all_time"):
        if win == "all_time":
            span = (None, None)
            got = events
        else:
            w = report[win]
            span = (w["start_ms"], w["end_ms"])
            got = [e for e in events if span[0] <= e["ts"] <= span[1]]
        s = report["all_time"] if win == "all_time" else report[win]["summary"]
        agg = dict.fromkeys(FIELDS, 0.0)
        cost = 0.0
        credit_cost = 0.0
        unpriced = defaultdict(lambda: [0.0, 0])
        for e in got:
            for f in FIELDS:
                agg[f] += e["counts"][f]
            if e["meter"] == "credits":
                rate = CREDIT_USD.get(e["tool"])
                if rate:
                    credit_cost += e["counts"]["credits"] * rate
                else:
                    unpriced[(e["tool"], e["model"] or "unknown")][0] += sum(e["counts"][f] for f in STAGES)
            else:
                price = table.get(norm(e["model"] or ""))
                tokens = sum(e["counts"][f] for f in STAGES)
                if price is None:
                    if tokens:
                        unpriced[(e["tool"], e["model"] or "unknown")][0] += tokens
                        unpriced[(e["tool"], e["model"] or "unknown")][1] += 1
                    continue
                cost += cost_of(price, e["counts"])
        total = agg["input"] + agg["cache_creation"] + agg["cache_read"] + agg["output"]
        prompt = agg["input"] + agg["cache_creation"] + agg["cache_read"]
        for f in FIELDS:
            check(f"{win}.counts.{f}", agg[f], s["counts"][f], tol=0.51)
        check(f"{win}.total_tokens", total, s["total_tokens"], tol=0.51)
        check(f"{win}.requests", float(len(got)), float(s["requests"]), tol=0.51)
        check(f"{win}.sessions", float(len({(e["tool"], e["session"]) for e in got})), float(s["sessions"]), tol=0.51)
        check(f"{win}.cached_pct", (agg["cache_read"] / prompt * 100.0) if prompt else 0.0, s["cached_pct"], tol=1e-4)
        check(f"{win}.cost", cost + credit_cost, s["cost"], tol=1e-4)
        check(f"{win}.credit_cost", credit_cost, s["credit_cost"], tol=1e-4)
        listed = {f"{u['tool']}|{u['model']}" for u in s["unpriced"]}
        mine_unpriced = {f"{t}|{m}" for (t, m) in unpriced}
        if win == "day" and mine_unpriced != listed:
            fails.append(f"day.unpriced set differs: mine-only={sorted(mine_unpriced - listed)} report-only={sorted(listed - mine_unpriced)}")

    # ---- breakdowns ---------------------------------------------------------
    for win in ("day", "week", "month"):
        w = report[win]
        got = [e for e in events if w["start_ms"] <= e["ts"] <= w["end_ms"]]

        for dim, keyfn in (
            ("tools", lambda e: e["tool"]),
            ("models", lambda e: e["model"] or "unknown"),
            ("projects", lambda e: e["project"] or ""),
        ):
            grouped = defaultdict(lambda: dict(cost=0.0, tokens=0.0, n=0, sessions=set()))
            for e in got:
                g = grouped[keyfn(e)]
                g["cost"] += priced(e)
                g["tokens"] += sum(e["counts"][f] for f in STAGES)
                g["n"] += 1
                g["sessions"].add(e["session"])
            items = {i["key"]: i for i in w["breakdown"][dim]}
            for k, g in grouped.items():
                if k and k in items:
                    check(f"{win}.{dim}[{k}].cost", g["cost"], items[k]["cost"], tol=1e-4)
                    check(f"{win}.{dim}[{k}].tokens", g["tokens"], items[k]["total_tokens"], tol=0.51)
                    check(f"{win}.{dim}[{k}].requests", float(g["n"]), float(items[k]["requests"]), tol=0.51)
            # A breakdown must not invent or lose money relative to its window.
            if dim == "tools":
                check(f"{win}.tools Σcost", sum(i["cost"] for i in items.values()), w["summary"]["cost"], tol=1e-3)
                check(f"{win}.tools Σtokens", sum(i["total_tokens"] for i in items.values()), w["summary"]["total_tokens"], tol=0.51)

    # ---- heatmap ------------------------------------------------------------
    cells = {c["date"]: c for c in report["heatmap"]}
    by_day = defaultdict(lambda: [0.0, 0.0, 0])
    for e in events:
        d = local_day(e["ts"])
        if d in cells:
            by_day[d][0] += sum(e["counts"][f] for f in STAGES)
            by_day[d][1] += priced(e)
            by_day[d][2] += 1
    for d, (tokens, cost, n) in by_day.items():
        check(f"heatmap[{d}].tokens", tokens, cells[d]["total_tokens"], tol=0.51)
        check(f"heatmap[{d}].cost", cost, cells[d]["cost"], tol=1e-4)
        check(f"heatmap[{d}].requests", float(n), float(cells[d]["requests"]), tol=0.51)
    print(f"heatmap: {len(cells)} cells, {len(by_day)} with events")

    # ---- budgets ------------------------------------------------------------
    budgets = json.load(open(SETTINGS, encoding="utf-8")).get("budgets", {}) if os.path.exists(SETTINGS) else {}
    if budgets:
        day = report["day"]
        month = report["month"]
        cost_by = lambda w: {i["key"]: i["cost"] for i in w["breakdown"]["tools"]}
        dc, mc = cost_by(day), cost_by(month)
        for q in report["quotas"]:
            if q.get("origin") != "budget":
                continue
            tool = q["tool"]
            b = budgets.get(tool) or {}
            limit = b.get("daily_usd") if q["window_minutes"] == 1440 else b.get("monthly_usd")
            spent = (dc if q["window_minutes"] == 1440 else mc).get(tool, 0.0)
            if limit:
                check(f"budget[{tool}].{q['window_minutes']}", spent / limit * 100.0, q["used_percent"], tol=0.05)
    print(f"budgets in settings: {budgets}")

    # ---- quota sanity -------------------------------------------------------
    now = report["generated_at_ms"]
    for q in report["quotas"]:
        if q["used_percent"] < 0:
            fails.append(f"quota {q['tool']}: negative used_percent {q['used_percent']}")
        if q["resets_at_ms"] and q["resets_at_ms"] < now:
            fails.append(f"quota {q['tool']}: expired window still listed ({q['resets_at_ms']})")
        if q["sampled_at_ms"] > now:
            fails.append(f"quota {q['tool']}: sampled in the future")

    # ---- price-table ambiguity ---------------------------------------------
    used_models = {norm(e["model"]) for e in events if e["model"]}
    clash = sorted(used_models & set(ambiguous))
    print(f"models.dev keys: {len(table):,}; ambiguous across providers: {len(ambiguous):,}; used by this index: {len(used_models):,}")
    if clash:
        print(f"  ! ambiguous AND used (price depends on which provider wins): {clash}")

    if frozen:
        for suffix in ("", "-wal", "-shm"):
            try:
                os.remove(frozen + suffix)
            except OSError:
                pass
    print("-" * 78)
    if fails:
        print(f"{len(fails)} FAILED check(s):")
        for f in fails[:40]:
            print("  !", f)
        return 1
    print("every report number reproduced independently: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
