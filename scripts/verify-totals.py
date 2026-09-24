#!/usr/bin/env python3
"""Independent recomputation of tokenme's per-source totals.

Nothing here shares code with the Rust adapters: each source is re-read from its
raw files with the stage convention that source's own data proves (the identities
are asserted in-line, not assumed), and the result is diffed against the SQLite
index tokenme built. A match means two independent implementations agree; a
mismatch is a bug in one of them and gets reported with its size.

Usage:  scripts/verify-totals.py [--db PATH] [--tolerance 0.005]
"""
from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import os
import sqlite3
import struct
import sys
import time

START = time.time()
from collections import defaultdict

HOME = os.path.expanduser("~")


def rows(path: str):
    """Every parseable JSONL record of a file; a torn last line is skipped."""
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    yield json.loads(line)
                except json.JSONDecodeError:
                    continue
    except OSError:
        return


def num(v) -> float:
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else 0.0


def millis(v) -> float:
    """The wire is seconds or milliseconds; make it always milliseconds."""
    v = num(v)
    return v * 1000 if 0 < v < 100_000_000_000 else v


def files(pattern, recursive=True):
    return sorted(glob.glob(os.path.expanduser(pattern), recursive=recursive))


# ------------------------------------------------------------------ sources


def claude():
    """One event per (file, message.id), keeping the largest snapshot.

    Claude Code writes one line per streamed content block and repeats the same
    call's `usage` on each. The last line is the call's final figure, because the
    stages are not independent: measured here on 321 groups, `input_tokens` and
    `cache_read_input_tokens` move in opposite directions while their sum (plus
    cache_creation) only grows — the provider re-resolves how much of the prompt
    came from cache. So a per-stage maximum double-bills the cached prefix
    (1,432,576 tokens on this machine) and only the final snapshot is correct.
    """
    out = defaultdict(lambda: [0.0] * 4)
    dup = monotonic = 0
    seen = {}
    for path in files("~/.claude/projects/**/*.jsonl"):
        for rec in rows(path):
            msg = rec.get("message") or {}
            u = msg.get("usage")
            if not isinstance(u, dict):
                continue
            mid = msg.get("id") or f"noid-{id(rec)}"
            key = (path, mid)
            cur = (
                num(u.get("input_tokens")),
                num(u.get("cache_creation_input_tokens")),
                num(u.get("cache_read_input_tokens")),
                num(u.get("output_tokens")),
            )
            if key in seen:
                dup += 1
                prev = seen[key]
                # The quantity that must never shrink is the whole prompt plus the
                # whole completion, not any single stage.
                if sum(cur) < sum(prev):
                    monotonic += 1
            seen[key] = cur
            out[key] = cur  # last write wins
    billed = {k: v for k, v in out.items() if sum(v) > 0}
    totals = [0.0] * 4
    for v in billed.values():
        for i in range(4):
            totals[i] += v[i]
    return {
        "n": len(billed),
        "in": totals[0],
        "cc": totals[1],
        "cr": totals[2],
        "out": totals[3],
        "notes": f"repeated ids={dup}, groups whose prompt+completion shrank: {monotonic} (must be 0)",
    }


def codex():
    """Per-call sum, checked against the file's own cumulative counter.

    Codex stamps `total_token_usage` after every call. Two independent readings
    of the same history: (a) sum `last_token_usage` over the calls that changed
    the cumulative counter (what the adapter bills), and (b) take each file's
    final cumulative value. If (a) != (b) the per-call summation is wrong.
    `archived_sessions/` mirrors live sessions, so it is excluded by design.
    """
    per_call = defaultdict(float)
    last_snapshot = {}
    calls = 0
    skipped_identical = 0
    deltas = delta_agree = delta_disagree = zero_echo = 0
    delta_tokens = first_tokens = first_calls = 0
    paths = [p for p in files("~/.codex/sessions/**/rollout-*.jsonl") if "/archived_sessions/" not in p]
    for path in paths:
        prev_total = None
        tot_prev = None
        for rec in rows(path):
            payload = rec.get("payload") or {}
            if payload.get("type") != "token_count":
                continue
            info = payload.get("info") or {}
            tot = info.get("total_token_usage") or {}
            last = info.get("last_token_usage") or {}
            if not tot:
                continue
            last_snapshot[path] = tot
            # Two rules, both from the vendor's own shape:
            #  * a record whose netted stages are all zero is a context-size echo,
            #    not a billed call (measured here: ~1.7k of 204k records);
            #  * a repeat of the previous (accumulator, stages, reported total) is
            #    the same snapshot re-emitted, so it is counted once.
            net_in = num(last.get("input_tokens")) - num(last.get("cached_input_tokens"))
            stages = (
                max(0.0, net_in),
                num(last.get("cache_write_input_tokens")),
                num(last.get("cached_input_tokens")),
                num(last.get("output_tokens")),
                num(last.get("reasoning_output_tokens")),
            )
            if sum(stages) == 0:
                zero_echo += 1
                continue
            sig = (num(tot.get("total_tokens")), stages, num(last.get("total_tokens")))
            if sig == prev_total and sig[0] > 0:
                skipped_identical += 1
                continue
            prev_total = sig
            deltas += 1
            if last and tot_prev is not None:
                agree = all(
                    abs(num(last.get(k)) - (num(tot.get(k)) - num(tot_prev.get(k)))) <= 1
                    for k in ("input_tokens", "cached_input_tokens", "output_tokens")
                )
                if agree:
                    delta_agree += 1
                else:
                    delta_disagree += 1
                    delta_tokens += num(tot.get("total_tokens")) - num(last.get("total_tokens"))
            elif last:
                first_calls += 1
                first_tokens += num(last.get("total_tokens"))
            tot_prev = dict(tot)
            calls += 1
            # `input_tokens` already contains `cached_input_tokens`: the file's own
            # identity total == input + output says the cache is a subset, not a peer.
            per_call["in"] += stages[0]
            per_call["cc"] += stages[1]
            per_call["cr"] += stages[2]
            per_call["out"] += stages[3]
            per_call["reason"] += stages[4]
    # A resumed session inherits its parent's cumulative counter, so Σ of the final
    # counters is an upper bound, not a check. The per-record check is: `last` equals
    # the counter's own step, i.e. every billed call is a real increment.
    return {
        "n": calls,
        "in": per_call["in"],
        "cc": per_call["cc"],
        "cr": per_call["cr"],
        "out": per_call["out"],
        "reason": per_call["reason"],
        "notes": f"files={len(paths)}, context-echo rows skipped={zero_echo:,}, verbatim repeats skipped={skipped_identical:,}; "
                 f"counter-step matches last: {delta_agree} yes / {delta_disagree} no / {first_calls} first-call "
                 f"({delta_tokens:,.0f} + {first_tokens:,.0f} = {delta_tokens + first_tokens:,.0f} tokens outside the identity)",
    }


def opencode_family():
    """OpenCode and its forks: `message.data` JSON with a `tokens` object.

    `tokens.input` is the net prompt and `tokens.cache.read/write` are peers, so
    the identity `total == input + output + reasoning + cache.read + cache.write`
    is asserted per row; a row that breaks it is reported, not silently summed.
    """
    products = {
        "opencode": ["~/.local/share/opencode/opencode.db"],
        "crow5": ["~/.local/share/crow5/*.db"],
        "mimocode": ["~/.local/share/mimocode/mimocode.db"],
    }
    result = {}
    for tool, patterns in products.items():
        agg = defaultdict(float)
        n = broken = rows_total = zero = 0
        for pattern in patterns:
            for path in files(pattern):
                try:
                    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
                except sqlite3.Error:
                    continue
                try:
                    got = conn.execute("select data from message").fetchall()
                except sqlite3.Error:
                    conn.close()
                    continue
                for (data,) in got:
                    rows_total += 1
                    try:
                        d = json.loads(data)
                    except (TypeError, json.JSONDecodeError):
                        continue
                    t = d.get("tokens")
                    if not isinstance(t, dict):
                        continue
                    if d.get("role") != "assistant":
                        continue
                    cache = t.get("cache") or {}
                    if d.get("tokens") and not any(
                        isinstance(t.get(k), (int, float)) and t.get(k)
                        for k in ("input", "output", "total", "reasoning")
                    ) and not any(cache.get(k) for k in ("read", "write")):
                        zero += 1
                        continue
                    net_in = num(t.get("input"))
                    out = num(t.get("output"))
                    reason = num(t.get("reasoning"))
                    cr = num(cache.get("read"))
                    cw = num(cache.get("write"))
                    total = num(t.get("total"))
                    if total and abs(total - (net_in + out + reason + cr + cw)) > 1:
                        broken += 1
                    agg["in"] += net_in
                    agg["cr"] += cr
                    agg["cc"] += cw
                    agg["out"] += out + reason
                    agg["reason"] += reason
                    n += 1
                conn.close()
        result[tool] = {
            "n": n,
            "in": agg["in"],
            "cc": agg["cc"],
            "cr": agg["cr"],
            "out": agg["out"],
            "reason": agg["reason"],
            "notes": f"message rows={rows_total}, identity breaks={broken} (must be 0), "
                     f"all-zero rows excluded={zero}",
        }
    return result


def pi_family():
    """Pi and Cola share one record shape: `message.usage` with named stages.

    Pi's own `totalTokens` is the referee: it must equal
    cacheRead + cacheWrite + input + output on every record (asserted here), and
    `cacheWrite1h` is a sub-breakdown of `cacheWrite`, never an extra stage.
    """
    roots = {"pi": "~/.pi/agent/sessions/*/*.jsonl", "cola": "~/.cola/sessions/*/*.jsonl"}
    result = {}
    for tool, pattern in roots.items():
        agg = defaultdict(float)
        n = bad_identity = zero = 0
        for path in files(pattern):
            for rec in rows(path):
                if rec.get("type") != "message":
                    continue
                u = (rec.get("message") or {}).get("usage")
                if not isinstance(u, dict):
                    continue
                inp, out = num(u.get("input")), num(u.get("output"))
                cr, cw = num(u.get("cacheRead")), num(u.get("cacheWrite"))
                total = num(u.get("totalTokens"))
                if abs(total - (inp + out + cr + cw)) > 1:
                    bad_identity += 1
                if inp + out + cr + cw == 0:
                    zero += 1  # a real record, but nothing billed: not an event
                    continue
                agg["in"] += inp
                agg["cr"] += cr
                agg["cc"] += cw
                agg["out"] += out
                agg["reason"] += num(u.get("reasoning"))
                n += 1
        result[tool] = {
            "n": n,
            "in": agg["in"],
            "cc": agg["cc"],
            "cr": agg["cr"],
            "out": agg["out"],
            "reason": agg["reason"],
            "notes": f"totalTokens identity breaks={bad_identity} (must be 0), "
                     f"all-zero rows excluded={zero}; reasoning is a sub-breakdown of output",
        }
    return result


def cline():
    """Cline rewrites one JSON array per session; `metrics.inputTokens` already
    contains the cache (its own source says not to add it back), so the net input
    is `inputTokens - cacheReadTokens`."""
    agg = defaultdict(float)
    n = sessions = negative = 0
    for path in files("~/.cline/data/sessions/*/*.messages.json"):
        try:
            records = json.load(open(path, encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(records, dict):  # {version, updated_at, agent, messages: [...]}
            records = records.get("messages")
        if not isinstance(records, list):
            continue
        sessions += 1
        for rec in records:
            if not isinstance(rec, dict):
                continue
            m = rec.get("metrics") if isinstance(rec.get("metrics"), dict) else None
            if not m:
                continue
            gross, cr = num(m.get("inputTokens")), num(m.get("cacheReadTokens"))
            cw, out = num(m.get("cacheWriteTokens")), num(m.get("outputTokens"))
            if gross - cr < 0:
                negative += 1
            agg["in"] += max(0.0, gross - cr)
            agg["cr"] += cr
            agg["cc"] += cw
            agg["out"] += out
            n += 1
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "notes": f"sessions={sessions}, rows where cache>input={negative} (clamped)",
    }


def zcode():
    """`model_usage` stores the cached prefix inside `input_tokens`; the identity
    `input + output == computed_total_tokens` is what proves the netting.

    The table is append-only *except that ZCode prunes whole sessions*: measured
    2026-09-24, one session's 32 rows (60,004 output tokens) vanished after the
    index had imported them, leaving no rowid gap — a rebuild, not a rotation.
    An append-only index keeps that spend as history, so the aggregate identity
    "index == current table" is wrong by construction here. What the table can
    still prove, per row, is stronger: every surviving row must appear in the
    index exactly once with identical numbers. Rows only the index has are the
    vendor's deletions, quantified in `_deleted` and reported, not counted
    against the survivors."""
    path = os.path.expanduser("~/.zcode/cli/db/db.sqlite")
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    got = conn.execute(
        "select input_tokens, cache_read_input_tokens, cache_creation_input_tokens,"
        " output_tokens, reasoning_tokens, computed_total_tokens, session_id,"
        " logical_request_id, attempt_index, completed_at, started_at from model_usage"
    ).fetchall()
    conn.close()
    agg = defaultdict(float)
    n = broken = keyless = 0
    keyed = {}
    for inp, cr, cw, out, reason, total, sess, req, attempt, done, started in got:
        inp, cr, cw, out, reason = (num(inp), num(cr), num(cw), num(out), num(reason))
        if inp + out != num(total):
            broken += 1
        # The same netting the adapter applies: the stored input holds the cached
        # prefix, and a row can carry more cache-read than input.
        row = (
            max(inp - cr, 0.0),
            cw,
            cr,
            out,
            min(reason, out),
        )
        ts = millis(done if num(done) else started)
        if req and not (inp == 0 and cr == 0 and cw == 0 and out == 0):
            # The adapter imports no event for an all-zero row, so the identity
            # cannot ask the index about one.
            keyed[f"{sess or ''}#{req}#{int(attempt or 0)}"] = row + (ts,)
        elif not req:
            keyless += 1
        if inp + out + cr + cw == 0:
            continue
        agg["in"] += inp - cr
        agg["cr"] += cr
        agg["cc"] += cw
        agg["out"] += out
        agg["reason"] += reason
        n += 1
    notes = f"table rows={len(got)}, input+output!=total on {broken} (must be 0)"
    if keyless:
        notes += f", rows without request id (identity cannot see them)={keyless}"
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "reason": agg["reason"],
        "notes": notes,
        "_keyed": keyed,
    }


def agnes():
    """`usage_ledger.input_tokens` contains the cache: the file's own identity is
    `input + output == total_tokens`, so the net prompt is `input - read - write`."""
    path = os.path.expanduser("~/.agnes/data/sessions/sessions.db")
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    got = conn.execute(
        "select input_tokens, output_tokens, total_tokens, cache_read_tokens, cache_write_tokens"
        " from usage_ledger"
    ).fetchall()
    conn.close()
    agg = defaultdict(float)
    n = broken = 0
    for inp, out, total, cr, cw in got:
        inp, out, cr, cw = num(inp), num(out), num(cr), num(cw)
        if inp + out != num(total):
            broken += 1
        if inp + out == 0:
            continue
        agg["in"] += inp - cr - cw
        agg["cr"] += cr
        agg["cc"] += cw
        agg["out"] += out
        n += 1
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "notes": f"ledger rows={len(got)}, identity breaks={broken} (must be 0)",
    }


def atomcode():
    """Top-level `usage.{prompt,completion,cached}`; `prompt` is the total input and
    `cached` is its cache-read subset (upstream's own comment says so)."""
    agg = defaultdict(float)
    n = negative = 0
    for path in files("~/.atomcode/sessions/*/*.jsonl"):
        for rec in rows(path):
            u = rec.get("usage")
            if not isinstance(u, dict):
                continue
            prompt, comp, cached = num(u.get("prompt")), num(u.get("completion")), num(u.get("cached"))
            if prompt == 0 and comp == 0:
                continue
            if prompt - cached < 0:
                negative += 1
            agg["in"] += max(0.0, prompt - cached)
            agg["cr"] += cached
            agg["out"] += comp
            n += 1
    return {
        "n": n,
        "in": agg["in"],
        "cr": agg["cr"],
        "out": agg["out"],
        "notes": f"rows with cached>prompt={negative} (clamped)",
    }


# CC Switch's own enums, re-typed from `src-tauri/src/services/sql_helpers.rs`:
# `:29-32` for the semantics stamp, `:25` for the cache-inclusive app types.
CCSWITCH_MIRRORED = {"session_log", "codex_session", "opencode_session", "pi_session"}
CCSWITCH_NATIVE = {"claude", "codex", "opencode", "pi"}
CCSWITCH_INCLUSIVE = {"codex", "gemini", "grokbuild"}


def ccswitch():
    """The gateway's `proxy_request_logs`, with the double-count gate recomputed.

    Two independent channels of duplication, and the gate has to hold on live data
    or this source bills the same calls twice: rows CC Switch re-imported from a
    tool's own session file (`data_source`), and rows it proxied for a tool tokenme
    reads directly (`data_source = proxy` plus `app_type`). Here the whole 92,754-row
    ledger is one or the other, so the correct contribution is *nothing*.
    """
    path = os.path.expanduser("~/.cc-switch/cc-switch.db")
    if not os.path.exists(path):
        return {"n": 0, "notes": "no cc-switch.db here"}
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    got = conn.execute(
        "select coalesce(nullif(data_source, ''), 'proxy'), app_type, input_token_semantics,"
        " input_tokens, cache_read_tokens, cache_creation_tokens, output_tokens, created_at"
        " from proxy_request_logs"
    ).fetchall()
    conn.close()

    agg = defaultdict(float)
    n = mirrored = proxied_native = zero = no_ts = guard = 0
    mix = defaultdict(int)
    for ds, app, sem, inp, cr, cw, out, ts in got:
        sem, inp, cr, cw, out = num(sem), max(num(inp), 0.0), max(num(cr), 0.0), max(num(cw), 0.0), max(num(out), 0.0)
        mix[(ds, app)] += 1
        if ds in CCSWITCH_MIRRORED:
            mirrored += 1
            continue
        if ds == "proxy" and app in CCSWITCH_NATIVE:
            proxied_native += 1
            continue
        # `fresh_input_sql`: net the cached prefix off the prompt, but only for a
        # cache-inclusive app type, and never below zero — a failed guard leaves
        # the raw column alone, which is what the Rust does and reports.
        net = inp
        if app in CCSWITCH_INCLUSIVE and sem != 2:
            want = cr + cw if sem == 1 else cr if sem == 0 else None
            if want is None:
                guard += 1
            elif inp >= want:
                net = inp - want
            else:
                guard += 1
        if net + cr + cw + out == 0:
            zero += 1
            continue
        if num(ts) <= 0:
            no_ts += 1
            continue
        agg["in"] += net
        agg["cr"] += cr
        agg["cc"] += cw
        agg["out"] += out
        n += 1

    top = ", ".join(f"{ds}/{app}×{k:,}" for (ds, app), k in sorted(mix.items(), key=lambda kv: -kv[1])[:4])
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "notes": f"ledger rows={len(got):,} [{top}]; gated as re-imported session={mirrored:,}, "
        f"as proxied traffic of a tool tokenme reads itself={proxied_native:,}; "
        f"zero-usage={zero:,}, no timestamp={no_ts:,}, netting guard failed={guard:,}",
    }


def qoder():
    """The transcript's token fields are always 0; `usage.credits` is the charge.
    Streaming repeats one call across lines, so the largest credits snapshot per
    (file, message.id) is the call."""
    best = {}
    zero_tokens = nonzero_tokens = 0
    for path in files("~/.qoder/projects/**/*.jsonl"):
        for rec in rows(path):
            msg = rec.get("message") or {}
            u = msg.get("usage")
            if not isinstance(u, dict) or "credits" not in u:
                continue
            toks = sum(num(u.get(k)) for k in ("input_tokens", "cache_read_input_tokens", "output_tokens"))
            if toks:
                nonzero_tokens += 1
            else:
                zero_tokens += 1
            key = (path, msg.get("id") or f"noid-{len(best)}")
            best[key] = max(best.get(key, 0.0), num(u.get("credits")))
    return {
        "n": len(best),
        "credits": sum(best.values()),
        "notes": f"records with tokens==0: {zero_tokens}, tokens>0: {nonzero_tokens} (must be 0)",
    }


# -------------------------------------------------------------- antigravity
#
# The Antigravity CLI is closed-source and keeps usage only in protobuf blobs, so
# this is the one source where "independent" means a second decoder. The field
# numbers below are the ones four open-source decoders agree on
# (tokscale, TokenTracker, ccusage, 1yayaye); the `#4.3 == #4.9 + #4.10` identity
# is asserted per blob here, which is what catches a mis-numbered field.


def pb_walk(buf):
    """Yield `(field_number, wire_type, value)` from a protobuf message."""
    i = 0
    n = len(buf)
    while i < n:
        tag = 0
        shift = 0
        while i < n:
            b = buf[i]
            i += 1
            tag |= (b & 0x7F) << shift
            if not (b & 0x80):
                break
            shift += 7
        field, wire = tag >> 3, tag & 7
        if wire == 0:
            val = 0
            shift = 0
            while i < n:
                b = buf[i]
                i += 1
                val |= (b & 0x7F) << shift
                if not (b & 0x80):
                    break
                shift += 7
            yield field, wire, val
        elif wire == 2:
            ln = 0
            shift = 0
            while i < n:
                b = buf[i]
                i += 1
                ln |= (b & 0x7F) << shift
                if not (b & 0x80):
                    break
                shift += 7
            yield field, wire, buf[i : i + ln]
            i += ln
        elif wire == 5:
            yield field, wire, struct.unpack_from("<I", buf, i)[0]
            i += 5
        elif wire == 1:
            yield field, wire, struct.unpack_from("<Q", buf, i)[0]
            i += 8
        else:
            return


def antigravity():
    agg = defaultdict(float)
    calls = {}
    broken_identity = blobs = bad_parse = 0
    roots = ["antigravity-cli", "antigravity", "antigravity-ide", "antigravity-backup"]
    paths = [p for r in roots for p in files(f"~/.gemini/{r}/conversations/*.db")]
    for path in paths:
        try:
            conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
            got = conn.execute("select data from gen_metadata").fetchall()
            conn.close()
        except sqlite3.Error:
            continue
        for (blob,) in got:
            if not isinstance(blob, (bytes, bytearray)):
                continue
            blobs += 1
            usage = None
            for field, wire, val in pb_walk(blob):
                if field == 1 and wire == 2:
                    for sub, sw, sval in pb_walk(val):
                        if sub == 4 and sw == 2:
                            usage = sval
                            break
                if usage is not None:
                    break
            if usage is None:
                continue
            stages = {}
            try:
                for sub, sw, sval in pb_walk(usage):
                    if sw == 0:
                        stages[sub] = sval
                    elif sub == 11 and sw == 2:
                        stages[11] = bytes(sval).decode("utf-8", "replace")
            except Exception:
                bad_parse += 1
                continue
            inp = num(stages.get(2, 0))
            cr = num(stages.get(5, 0))
            out = num(stages.get(3, 0))
            text = num(stages.get(9, 0))
            reason = num(stages.get(10, 0))
            if out and text + reason and abs(out - (text + reason)) > 1:
                broken_identity += 1
            rid = stages.get(11) or f"{path}#{blobs}#{inp:.0f}{out:.0f}"
            key = (path, rid)
            prev = calls.get(key)
            reason = num(stages.get(10, 0))
            if prev is None:
                calls[key] = (inp, cr, out, reason)
            else:  # a streamed repeat of one call keeps the largest snapshot
                calls[key] = tuple(max(p, v) for p, v in zip(prev, (inp, cr, out, reason)))
    for entry in calls.values():
        inp, cr, out, reason = entry
        agg["in"] += inp
        agg["cr"] += cr
        agg["out"] += out
        agg["reason"] += reason
    return {
        "n": len(calls),
        "in": agg["in"],
        "cr": agg["cr"],
        "out": agg["out"],
        "reason": agg["reason"],
        "notes": f"blobs={blobs} in {len(paths)} dbs, output!=text+reasoning on "
                 f"{broken_identity}, unparsable={bad_parse} (must be 0)",
    }


# -------------------------------------------------------------------- harness


def index_totals(db):
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        got = conn.execute(
            "select tool, count(*), coalesce(sum(in_tok),0), coalesce(sum(cc_tok),0),"
            " coalesce(sum(cr_tok),0), coalesce(sum(out_tok),0), coalesce(sum(reason_tok),0),"
            " coalesce(sum(credits),0) from event group by tool"
        ).fetchall()
    finally:
        conn.close()
    return {
        r[0]: {"n": r[1], "in": r[2], "cc": r[3], "cr": r[4], "out": r[5], "reason": r[6], "credits": r[7]}
        for r in got
    }


def compute_all():
    """Every source, read once, from scratch."""
    return {
        "claude": claude(),
        "codex": codex(),
        **opencode_family(),
        **pi_family(),
        "cline": cline(),
        "zcode": zcode(),
        "agnes": agnes(),
        "atomcode": atomcode(),
        "qoder": qoder(),
        "antigravity": antigravity(),
        "ccswitch": ccswitch(),
    }


FIELDS = ["n", "in", "cc", "cr", "out", "reason", "credits"]


def zcode_row_identity(index_db, keyed, tolerance):
    """Per-row identity for the source whose vendor deletes history.

    Every surviving `model_usage` row must appear in the index once with the
    same net numbers (a row newer than the index's newest event may simply not
    be imported yet — the live-lag `--bracket` handles everywhere else). Events
    only the index has are the vendor's deletions: quantified and handed back
    so the aggregate comparison can subtract them, never trusted as numbers.
    Returns `(failures, deleted_field_sums, note)`."""
    conn = sqlite3.connect(f"file:{index_db}?mode=ro", uri=True)
    try:
        ev = conn.execute(
            "select dedupe_key, ts_ms, in_tok, cc_tok, cr_tok, out_tok, reason_tok"
            " from event where tool='zcode' and dedupe_key is not null"
        ).fetchall()
    finally:
        conn.close()
    if not ev:
        return (["zcode: no keyed events in the index"], {}, "identity not checkable")
    newest = max(e[1] for e in ev)
    idx = {e[0]: (e[2], e[3], e[4], e[5], e[6]) for e in ev}
    FIELDS5 = ["in", "cc", "cr", "out", "reason"]
    must = {k: v[:5] for k, v in keyed.items() if v[5] <= newest}
    missing = sorted(k for k in must if k not in idx)
    wrong = sorted(k for k in must if k in idx and tuple(idx[k]) != tuple(must[k]))
    deleted = [k for k in idx if k not in keyed]
    sums = {f: sum(idx[k][i] or 0 for k in deleted) for i, f in enumerate(FIELDS5)}
    sums["n"] = float(len(deleted))
    failures = []
    if missing:
        failures.append(f"zcode: {len(missing)} row(s) the table has but the index never saw, e.g. {missing[0]}")
    if wrong:
        failures.append(f"zcode: {len(wrong)} row(s) whose numbers differ, e.g. {wrong[0]}: {idx[wrong[0]]} != {must[wrong[0]]}")
    note = (
        f"per-row identity: {len(must)} checked, {len(idx) - len(must) + len(missing)} vendor-deleted "
        f"({sum(sums.values()):,.0f} tok) kept as history"
    )
    return failures, sums, note


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bracket", action="store_true", help="re-read everything after rebuilding the index")
    ap.add_argument("--db", default=os.path.expanduser("~/Library/Application Support/tokenme/index.db"))
    ap.add_argument("--tolerance", type=float, default=0.005, help="relative tolerance for live-growing sources")
    args = ap.parse_args()

    # The sources are live: this very session appends to Claude's and Codex's logs
    # while we read them. So an exact match against one snapshot is not the honest
    # test — `--bracket` reads everything, rebuilds the index, reads everything
    # again, and accepts any index figure that falls inside the two passes.
    low = compute_all()
    stored = index_totals(args.db)
    high = None
    if args.bracket:
        import subprocess

        print(f"[bracket] pass 1 done in {time.time() - START:.0f}s; rebuilding the index…", flush=True)
        subprocess.run(
            ["cargo", "run", "--quiet", "--release", "-p", "usage-cli", "--", "index", "--rebuild", "--offline"],
            check=True,
            cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        )
        stored = index_totals(args.db)
        high = compute_all()
        print(f"[bracket] pass 2 done in {time.time() - START:.0f}s", flush=True)

    failures = []
    zcode_note = ""
    zcode_deleted = {}
    if "_keyed" in low.get("zcode", {}):
        id_failures, zcode_deleted, zcode_note = zcode_row_identity(args.db, low["zcode"]["_keyed"], args.tolerance)
        failures.extend(id_failures)
    print(f"{'tool':<12}{'field':<8}{'independent':>17}{'index':>17}{'delta':>13}  verdict")
    print("-" * 88)
    for tool in sorted(low):
        idx = stored.get(tool)
        if idx is None:
            # A source with nothing to contribute has no rows in the index at all,
            # and that is a correct answer, not a missing one: `ccswitch` gates
            # every row of this machine's ledger as another source's duplicate.
            idx = {}
            if any(low[tool].get(f, 0.0) for f in FIELDS):
                failures.append(f"{tool}: missing from the index")
                print(f"{tool:<12}{'-':<8}{'':>17}{'':>17}{'':>13}  MISSING from the index")
                continue
        for f in FIELDS:
            a, b = low[tool].get(f, 0.0), idx.get(f, 0.0)
            if a == 0.0 and b == 0.0:
                continue
            # The vendor deletes zcode rows the index already keeps as history;
            # the per-row identity proved the survivors, so the aggregate is
            # compared against the index *minus* the quantified deletions.
            gone = zcode_deleted.get(f, 0.0) if tool == "zcode" else 0.0
            rel = abs(a - (b - gone)) / max(abs(b - gone), 1.0)
            flag = "ok" if rel <= args.tolerance else f"MISMATCH {rel * 100:.2f}%"
            if gone and rel <= args.tolerance:
                flag = f"ok (−{gone:,.0f} vendor-deleted)"
            if high is not None:
                lo = min(a, high[tool].get(f, 0.0))
                hi = max(a, high[tool].get(f, 0.0))
                if rel > args.tolerance and lo - 1.0 <= b - gone <= hi + 1.0:
                    flag = f"ok (live: {lo:,.0f}…{hi:,.0f})"
            if flag.startswith("MISMATCH"):
                failures.append(f"{tool}.{f}: {flag} (independent {a:,.0f} vs index {b:,.0f})")
            print(f"{tool:<12}{f:<8}{a:>17,.0f}{b:>17,.0f}{a - b:>+13,.0f}  {flag}")
        if low[tool].get("notes"):
            print(f"{'':<12}  note: {low[tool]['notes']}")
        if tool == "zcode" and zcode_note:
            print(f"{'':<12}  note: {zcode_note}")
    print("-" * 88)
    print(f"{len(failures)} mismatched field(s) at tolerance {args.tolerance * 100:.2f}%")
    for f in failures:
        print(f"  ! {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
