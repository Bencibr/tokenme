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
from pathlib import Path
import shutil
import sqlite3
import struct
import sys
import time

START = time.time()
from collections import defaultdict

HOME = os.path.expanduser("~")


def data_dir():
    """Mirror `dirs::data_dir()` for the CLI's default index location."""
    if os.name == "nt":
        return os.environ.get("LOCALAPPDATA") or os.path.join(HOME, "AppData", "Local")
    if sys.platform == "darwin":
        return os.path.join(HOME, "Library", "Application Support")
    return os.environ.get("XDG_DATA_HOME") or os.path.join(HOME, ".local", "share")


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


def open_ro(path: str):
    """Open a database for reading however it will be read.

    A WAL database whose writer has exited cleanly keeps its `-shm` deleted, and
    SQLite refuses a read-only handle that cannot rebuild it — the same failure
    the Rust adapters walk a ladder around. `immutable` is only ever tried when no
    `-wal` sidecar exists, so there are never uncheckpointed rows to silently miss.
    """
    base = Path(path).resolve().as_uri()
    sidecar = Path(str(path) + "-wal")
    uris = [f"{base}?mode=ro", f"{base}?mode=ro&nolock=1"]
    if not sidecar.exists():
        uris.append(f"{base}?immutable=1")
    last = None
    for uri in uris:
        conn = None
        try:
            conn = sqlite3.connect(uri, uri=True)
            # `connect` is lazy: the open that fails is the first statement, so
            # probe here or the rung below would never be reached.
            conn.execute("select count(*) from pragma_database_list").fetchone()
            return conn
        except sqlite3.Error as exc:  # try the next rung
            last = exc
            if conn is not None:
                conn.close()
    raise last


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
    out = defaultdict(lambda: [0.0] * 5)
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
                # Thinking is a sub-split of output_tokens, so it rides along the
                # same last-write rule and never joins the billed total.
                num((u.get("output_tokens_details") or {}).get("thinking_tokens")),
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
    # The gate is on the four billable stages only: a thinking-only row would
    # otherwise look billable while the adapter's `total()` ignores it.
    billed = {k: v for k, v in out.items() if sum(v[:4]) > 0}
    totals = [0.0] * 5
    for v in billed.values():
        for i in range(5):
            totals[i] += v[i]
    return {
        "n": len(billed),
        "in": totals[0],
        "cc": totals[1],
        "cr": totals[2],
        "out": totals[3],
        "reason": totals[4],
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
    paths = [p for p in files("~/.codex/sessions/**/rollout-*.jsonl") if "archived_sessions" not in Path(p).parts]
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
        # Crow5 ships a desktop build that keeps its own store in the Electron
        # userData root while the CLI's directory goes quiet — the adapter reads
        # both, so this reader has to as well or the two can never agree.
        "crow5": [
            "~/.local/share/crow5/*.db",
            "~/Library/Application Support/com.crow5.desktop/crow5/*.db",
        ],
        "mimocode": ["~/.local/share/mimocode/mimocode.db"],
    }
    result = {}
    for tool, patterns in products.items():
        agg = defaultdict(float)
        n = broken = rows_total = zero = 0
        for pattern in patterns:
            for path in files(pattern):
                try:
                    conn = open_ro(path)
                except sqlite3.Error:
                    continue
                # Which record table this store writes: the candidate with the most
                # rows, ties to `message` — the same rule the Rust adapter runs,
                # reimplemented here from the wire up so a disagreement is a finding.
                candidates = []
                for rank, name in enumerate(("message", "session_message")):
                    try:
                        high = conn.execute(f"select coalesce(max(rowid), 0) from {name}").fetchone()[0]
                    except sqlite3.Error:
                        continue
                    candidates.append((-high, rank, name))
                if not candidates:
                    conn.close()
                    continue
                table = min(candidates)[2]
                role_in_data = table == "message"
                try:
                    got = conn.execute(
                        f"select data from {table}"
                        + ("" if role_in_data else " where type = 'assistant'")
                    ).fetchall()
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
                    if d.get("role") not in (None, "", "assistant"):
                        continue
                    if role_in_data and d.get("role") != "assistant":
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
            "notes": f"{table} rows={rows_total}, identity breaks={broken} (must be 0), "
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


def _ts_ms(raw):
    """`usage_core::parse_ts_ms`'s ladder: RFC3339, naive ISO read as local, then
    an integer with 10 digits read as seconds (13 as milliseconds). Only used to
    decide whether a row is old enough that the index must already hold it."""
    if isinstance(raw, (int, float)) and not isinstance(raw, bool):
        n = int(raw)
        return n * 1000 if n < 10_000_000_000 else n
    if not isinstance(raw, str):
        return None
    s = raw.strip()
    if not s:
        return None
    try:
        parsed = dt.datetime.fromisoformat(s.replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            parsed = parsed.astimezone()
        return int(parsed.timestamp() * 1000)
    except ValueError:
        pass
    try:
        n = int(s)
    except ValueError:
        return None
    return n * 1000 if n < 10_000_000_000 else n


def cline():
    """Cline rewrites one JSON array per session; `metrics.inputTokens` already
    contains the cache (its own source says not to add it back), so the net input
    is `inputTokens - cacheReadTokens`.

    The rewrite can *prune* rows the index already imported: measured 2026-10-05,
    session_1790752678127_io4gd came back with 8 fewer billable assistant rows
    (1,662,464 cache-read tokens) than the index holds, file otherwise identical.
    Like zcode, the aggregate is compared against the index *minus* the
    quantified deletions, and the per-row identity below proves the survivors.
    The caliber mirrors the adapter exactly: role=assistant, `metrics` present as
    an object, a parseable `ts`/`timestamp`/`created_at`.

    Cline also keeps its own per-session ledger in the sibling `<dir>.json`:
    `metadata.usage` for the orchestrator transcript and `metadata.aggregateUsage`
    for that transcript *plus every sub-agent's* — measured 2026-10-06 on the live
    92.9M-token session `session_1791191498435_ti6bi`, where `aggregateUsage −
    usage` equalled the ten agent transcripts to the token. Two readers of one
    transcript can agree by construction; agreeing with the product's own
    accounting is what proves the *mapping* of the stages, so those accumulators
    come back as `_vendor`. A session whose files changed while this function read
    it drops out of that sum (`live=` in the notes) rather than becoming a phantom
    mismatch."""
    agg = defaultdict(float)
    n = sessions = negative = 0
    keyed = {}
    # Wire numbers per session dir (`inputTokens` still holds the cached prefix,
    # exactly as the app's own accumulator counts them), over every row carrying
    # `metrics` — including one the adapter could not date, because the app billed
    # that call too.
    per_dir = defaultdict(lambda: {"gross": 0.0, "cr": 0.0, "cw": 0.0, "out": 0.0, "paths": {}, "files": 0})
    for path in files("~/.cline/data/sessions/*/*.messages.json"):
        try:
            before = os.stat(path).st_mtime_ns
            records = json.load(open(path, encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        sid = None
        if isinstance(records, dict):  # {version, updated_at, agent, messages: [...]}
            sid = records.get("sessionId")
            records = records.get("messages")
        if not isinstance(records, list):
            continue
        sessions += 1
        slot = per_dir[os.path.dirname(path)]
        slot["paths"][path] = before
        slot["files"] += 1
        session = sid if isinstance(sid, str) and sid else os.path.basename(path)[: -len(".messages.json")]
        for idx, rec in enumerate(records):
            if not isinstance(rec, dict) or rec.get("role") != "assistant":
                continue
            m = rec.get("metrics") if isinstance(rec.get("metrics"), dict) else None
            if m is None:
                continue
            gross, cr = num(m.get("inputTokens")), num(m.get("cacheReadTokens"))
            cw, out = num(m.get("cacheWriteTokens")), num(m.get("outputTokens"))
            if gross - cr < 0:
                negative += 1
            slot["gross"] += gross
            slot["cr"] += cr
            slot["cw"] += cw
            slot["out"] += out
            ts = None
            for k in ("ts", "timestamp", "created_at"):
                ts = _ts_ms(rec.get(k))
                if ts is not None:
                    break
            if ts is None:
                continue
            mid = rec.get("id")
            key = f"{session}#{mid}" if isinstance(mid, str) and mid else f"{session}#idx{idx}"
            keyed[key] = (max(0.0, gross - cr), cw, cr, out, 0.0, ts)
            agg["in"] += max(0.0, gross - cr)
            agg["cr"] += cr
            agg["cc"] += cw
            agg["out"] += out
            n += 1
    vendor = defaultdict(float)
    live = lagged = no_ledger = settled = 0
    worst = (0.0, "")
    for d, got in sorted(per_dir.items()):
        name = os.path.basename(d)
        meta = os.path.join(d, name + ".json")
        try:
            doc = json.load(open(meta, encoding="utf-8"))
            churn = any(os.stat(p).st_mtime_ns != stamp for p, stamp in got["paths"].items())
            meta_at = os.stat(meta).st_mtime_ns
        except (OSError, json.JSONDecodeError):
            no_ledger += 1
            continue
        if churn:
            live += 1
            continue
        if meta_at < max(got["paths"].values()):
            # The ledger predates a transcript it cannot describe: the app writes
            # the meta after the transcript, so a session that is still running
            # always looks short here. Measured 2026-10-06 on
            # `session_1791191498435_ti6bi`: 7.4M of tokens landed in the seven
            # minutes between its meta and its transcript.
            lagged += 1
            continue
        md = doc.get("metadata") if isinstance(doc, dict) else None
        agg_of = md.get("aggregateUsage") if isinstance(md, dict) else None
        own = md.get("usage") if isinstance(md, dict) else None
        acc = agg_of if isinstance(agg_of, dict) else own
        if not isinstance(acc, dict):
            no_ledger += 1
            continue
        if got["files"] > 1 and not isinstance(agg_of, dict):
            # Sub-agent spend lives in this dir, and a `usage`-only ledger counts
            # just the orchestrator — comparing them would be unfair, not true.
            no_ledger += 1
            continue
        theirs = {
            "in": num(acc.get("inputTokens")) - num(acc.get("cacheReadTokens")),
            "cc": num(acc.get("cacheWriteTokens")),
            "cr": num(acc.get("cacheReadTokens")),
            "out": num(acc.get("outputTokens")),
        }
        ours = {"in": got["gross"] - got["cr"], "cc": got["cw"], "cr": got["cr"], "out": got["out"]}
        for f in ("in", "cc", "cr", "out"):
            vendor[f] += theirs[f]
        settled += 1
        delta = theirs["in"] - ours["in"]
        if abs(delta) > abs(worst[0]):
            worst = (delta, name)
    note = f"sessions={sessions}, rows where cache>input={negative} (clamped)"
    if vendor:
        note += f", vendor ledger over {settled} settled session(s)"
        if worst[1]:
            note += f", largest input residue {worst[0]:+,.0f} in {worst[1]}"
    if live:
        note += f", live={live} (rewritten while read, excluded)"
    if lagged:
        note += f", ledger predates its transcript={lagged} (excluded)"
    if no_ledger:
        note += f", no session ledger={no_ledger}"
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "_keyed": keyed,
        "_vendor": dict(vendor),
        "notes": note,
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
    if not os.path.exists(path):
        return {"n": 0, "notes": "no zcode model_usage database here"}
    conn = open_ro(path)
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
    if not os.path.exists(path):
        return {"n": 0, "notes": "no agnes sessions database here"}
    conn = open_ro(path)
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
    conn = open_ro(path)
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


def pb_stages(buf):
    """The varint stages of one usage message, plus its `#11` responseId string."""
    stages = {}
    for sub, sw, sval in pb_walk(buf):
        if sw == 0:
            stages[sub] = sval
        elif sub == 11 and sw == 2:
            stages[11] = bytes(sval).decode("utf-8", "replace")
    return stages


def antigravity():
    agg = defaultdict(float)
    calls = {}
    broken_identity = blobs = bad_parse = retries = 0
    roots = ["antigravity-cli", "antigravity", "antigravity-ide", "antigravity-backup"]
    paths = [p for r in roots for p in files(f"~/.gemini/{r}/conversations/*.db")]
    for path in paths:
        try:
            conn = open_ro(path)
            got = conn.execute("select data from gen_metadata").fetchall()
            conn.close()
        except sqlite3.Error:
            continue
        for (blob,) in got:
            if not isinstance(blob, (bytes, bytearray)):
                continue
            blobs += 1
            usage, attempts = None, []
            for field, wire, val in pb_walk(blob):
                if field == 1 and wire == 2:
                    for sub, sw, sval in pb_walk(val):
                        if sub == 4 and sw == 2 and usage is None:
                            usage = sval
                        elif sub == 17 and sw == 2:
                            # The attempt box: its own `#2` is that attempt's usage.
                            for f2, w2, v2 in pb_walk(sval):
                                if f2 == 2 and w2 == 2:
                                    attempts.append(v2)
                if usage is not None:
                    break
            if usage is None:
                continue
            try:
                entries = [pb_stages(usage)]
            except Exception:
                bad_parse += 1
                continue
            parent_id = entries[0].get(11)
            for attempt in attempts:
                try:
                    st = pb_stages(attempt)
                except Exception:
                    bad_parse += 1
                    continue
                # An attempt is a separate call only when it carries a responseId
                # of its own. Measured here: 81,260 of 81,727 boxes repeat `#4`
                # under the parent's id and 435 carry no id at all (all of them
                # zero-token); only 17 have an id that differs, and those are the
                # calls the vendor actually served.
                if st.get(11) and st[11] != parent_id:
                    entries.append(st)
                    retries += 1
            for stages in entries:
                inp = num(stages.get(2, 0))
                cr = num(stages.get(5, 0))
                out = num(stages.get(3, 0))
                text = num(stages.get(9, 0))
                reason = num(stages.get(10, 0))
                if out and text + reason and abs(out - (text + reason)) > 1:
                    broken_identity += 1
                rid = stages.get(11) or f"{path}#{blobs}#{inp:.0f}{out:.0f}"
                key = (path, rid)
                snap = (inp, cr, out, reason)
                prev = calls.get(key)
                if prev is None:
                    calls[key] = snap
                else:  # a streamed repeat of one call keeps the largest snapshot
                    calls[key] = tuple(max(p, v) for p, v in zip(prev, snap))
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
                 f"{broken_identity}, unparsable={bad_parse} (must be 0), "
                 f"retry calls={retries}",
    }


# -------------------------------------------------------------------- harness


# -------------------------------------------------------------- workbuddy
#
# Two independent readers of the same vendor payload: the Rust adapter and this
# decoder. The rules come from the vendor's own shape, cross-checked against
# `ChanningYuan/usageBar`'s WorkBuddyDetailScanner: `prompt_tokens` already
# contains the cache hit, `completion_thinking_tokens` is a sub-split of
# `completion_tokens`, and only `prompt_cache_hit_tokens` carries the hit — the
# Anthropic-named twins next to it sit at 0 for this provider.


def workbuddy():
    """One event per `providerData.rawUsage` line of the app's transcripts."""
    n = 0
    tot = dict(in_tok=0.0, cc=0.0, cr=0.0, out=0.0, reason=0.0, credits=0.0)
    hit_identity = inclusive_violation = zero_credit_rows = 0
    roots = [Path(HOME) / ".workbuddy-ai" / "projects", Path(HOME) / ".workbuddy" / "projects"]
    seen = set()
    for root in roots:
        if not root.is_dir():
            continue
        for path in sorted(root.glob("**/*.jsonl")):
            if str(path) in seen:
                continue
            seen.add(str(path))
            for rec in rows(str(path)):
                usage = (rec.get("providerData") or {}).get("rawUsage")
                if not isinstance(usage, dict):
                    continue
                prompt = max(num(usage.get("prompt_tokens")), 0.0)
                out = max(num(usage.get("completion_tokens")), 0.0)
                if prompt == 0.0 and out == 0.0:
                    continue
                hit = min(max(num(usage.get("prompt_cache_hit_tokens")), 0.0), prompt)
                write = min(max(num(usage.get("prompt_cache_write_tokens")) or num(usage.get("cache_creation_input_tokens")), 0.0), prompt - hit)
                miss = num(usage.get("prompt_cache_miss_tokens"))
                if prompt and abs(hit + miss - prompt) <= 1.0:
                    hit_identity += 1
                if prompt < hit + write:
                    inclusive_violation += 1
                n += 1
                tot["in_tok"] += prompt - hit - write
                tot["cc"] += write
                tot["cr"] += hit
                tot["out"] += out
                tot["reason"] += max(num(usage.get("completion_thinking_tokens")), 0.0)
                credit = num(usage.get("credit"))
                tot["credits"] += max(credit, 0.0)
                if credit == 0.0:
                    zero_credit_rows += 1
    return {
        "n": n,
        "in": tot["in_tok"],
        "cc": tot["cc"],
        "cr": tot["cr"],
        "out": tot["out"],
        "reason": tot["reason"],
        "credits": tot["credits"],
        "notes": (
            f"hit+miss==prompt held for {hit_identity}/{n} rows; "
            f"{zero_credit_rows} rows billed 0 credit (free model); "
            f"{inclusive_violation} rows where prompt < hit+write; "
            f"reason is a sub-split of out and never added to it"
        ),
    }


def index_totals(db):
    conn = open_ro(db)
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


# -------------------------------------------------------------- kimi code
#
# Kimi Code keeps one durable event log per agent -- `sessions/**/wire.jsonl` --
# and a `usage.record` line is exactly one LLM call. The vendor writes it at
# `llmRequesterService.ts:477` with that call's own stages, and `usageScope` names
# the request *source* ('turn' for a user turn, 'session' for compaction, plan and
# title requests), not a running total: `usageAgentModel.ts` adds every record into
# the state `/status` prints as `Session total`. So both scopes are billed here, as
# they are in the adapter, and a reader that filters to 'turn' is the one that is
# wrong.
#
# The stages are mutually exclusive by the vendor's own helper
# (`human/llm/usage.ts`: inputTotal = inputOther + inputCacheRead +
# inputCacheCreation, grandTotal adds output), and the wire carries no reasoning
# split at all, so `reason` stays empty rather than being invented.
#
# Identity follows the adapter's rule so a disagreement points at the keying: the
# legacy payload's `message_id`, else the writing agent, the stamp, the model and
# all four stages --
# deliberately WITHOUT the session directory, because `migration-legacy` copies a
# v1 session into a v2 directory under a new id and both roots are read.


def kimicode():
    roots = []
    for var in ("KIMI_CODE_HOME", "KIMI_DATA_DIR"):
        value = (os.environ.get(var) or "").strip()
        if value:
            roots = [part.strip() for part in value.split(",") if part.strip()]
            break
    if not roots:
        roots = [os.path.join(HOME, ".kimi-code"), os.path.join(HOME, ".kimi")]
        # The desktop client (Kimi.app) provisions a whole kimi-code home for its
        # embedded runtime; its wire logs are this product's usage too, and the
        # adapter reads them (paths::desktop_home).
        if sys.platform == "darwin":
            roots.append(
                os.path.join(
                    data_dir(), "kimi-desktop", "daimon-share", "daimon", "runtime", "kimi-code", "home"
                )
            )

    agg = defaultdict(float)
    seen = set()
    n = logs = zero = repeats = unstamped = 0
    for root in roots:
        for path in files(os.path.join(root, "sessions", "**", "wire.jsonl")):
            logs += 1
            for ordinal, rec in enumerate(rows(path)):
                model = message_id = None
                agent = ""
                if rec.get("type") == "usage.record":
                    usage = rec.get("usage")
                    ms = millis(rec.get("time"))
                    model = rec.get("model")
                    agent = rec.get("agentId") or ""
                    if isinstance(model, str) and model.startswith("kimi-code/"):
                        model = model[len("kimi-code/"):]
                else:
                    message = rec.get("message") or {}
                    if message.get("type") != "StatusUpdate":
                        continue
                    payload = message.get("payload") or {}
                    usage = payload.get("token_usage")
                    ms = millis(rec.get("timestamp"))
                    model = payload.get("model")
                    message_id = payload.get("message_id")
                    agent = payload.get("agent_id") or ""
                if not isinstance(usage, dict):
                    continue
                inp = num(usage.get("input_other", usage.get("inputOther")))
                out = num(usage.get("output"))
                cr = num(usage.get("input_cache_read", usage.get("inputCacheRead")))
                cc = num(usage.get("input_cache_creation", usage.get("inputCacheCreation")))
                if inp + out + cr + cc <= 0:
                    zero += 1
                    continue
                if message_id:
                    ident = f"id:{message_id}"
                elif ms > 0:
                    ident = f"t:{agent}:{ms:.0f}:{model}:{inp:.0f}:{cc:.0f}:{cr:.0f}:{out:.0f}"
                else:
                    ident = f"p:{path}:{ordinal}"
                    unstamped += 1
                if ident in seen:
                    repeats += 1
                    continue
                seen.add(ident)
                n += 1
                agg["in"] += inp
                agg["cc"] += cc
                agg["cr"] += cr
                agg["out"] += out
    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "reason": 0.0,
        "credits": 0.0,
        "notes": f"{logs} wire logs, zero-usage rows={zero}, repeats collapsed={repeats} "
                 f"(a migrated copy or a retraction), unstamped rows={unstamped}",
    }


def minimaxcode():
    """`v2/sessions/**/messages.jsonl` — one assistant record per billed call.

    MiniMax Code is the vendored pi-mono coding agent behind its own envelope:
    `{message_id, turn_id, message:{role, model, usage{input, output, cacheRead,
    cacheWrite, totalTokens, cost}, timestamp}}`. `usage.input` is the fresh input
    *after* the last cache breakpoint (the vendor's own
    `packages/local-runtime/src/usage/api.ts` says so), so the four stages are
    peers and `totalTokens` is their sum — asserted per record below, and the only
    reason `cacheRead` is not subtracted from `input` here either.

    The store beside it, `local_runtime_token_usage`, is the vendor's OWN projection
    of these very records (`recordLocalTokenUsageFromPiMessages`). Its numbers are
    returned as `_vendor` and compared against ours in `main()`: two readers of one
    file can agree by construction, but a reader and the product's own accounting
    agreeing is a fact.
    """
    roots = []
    for var in ("MINIMAX_DATA_DIR", "MAVIS_DATA_DIR"):
        value = (os.environ.get(var) or "").strip()
        if value:
            roots = [value]
            break
    if not roots:
        profile = (os.environ.get("MAVIS_PROFILE") or "").strip()
        tail = f"-{profile}" if profile else ""
        seen = set()
        for base in (".minimax", ".mavis"):
            path = os.path.join(HOME, base + tail)
            if not os.path.isdir(path):
                continue
            # The migration leaves `~/.mavis` as a symlink to `~/.minimax`: one
            # directory behind two names is one install, not two.
            real = os.path.realpath(path)
            if real in seen:
                continue
            seen.add(real)
            roots.append(path)

    agg = defaultdict(float)
    seen = set()
    n = logs = zero = repeats = unstamped = broken = idless = 0
    for root in roots:
        for path in files(os.path.join(root, "v2", "sessions", "**", "messages.jsonl")):
            logs += 1
            for ordinal, rec in enumerate(rows(path)):
                message = rec.get("message") or {}
                if message.get("role") != "assistant":
                    continue
                usage = message.get("usage")
                if not isinstance(usage, dict):
                    continue
                inp, out = num(usage.get("input")), num(usage.get("output"))
                cr, cc = num(usage.get("cacheRead")), num(usage.get("cacheWrite"))
                reason = num(usage.get("reasoning"))
                total = num(usage.get("totalTokens"))
                if total > 0 and abs(total - (inp + out + cr + cc)) > 0.5:
                    broken += 1
                if inp + out + cr + cc <= 0:
                    zero += 1
                    continue
                ms = millis(message.get("timestamp"))
                mid = rec.get("message_id")
                if isinstance(mid, str) and mid:
                    ident = f"id:{mid}"
                elif ms > 0:
                    ident = (
                        f"t:{rec.get('turn_id')}:{ms:.0f}:{message.get('model')}"
                        f":{inp:.0f}:{cc:.0f}:{cr:.0f}:{out:.0f}"
                    )
                else:
                    ident = f"p:{path}:{ordinal}"
                    unstamped += 1
                if not isinstance(mid, str) or not mid:
                    idless += 1
                if ident in seen:
                    repeats += 1
                    continue
                seen.add(ident)
                n += 1
                agg["in"] += inp
                agg["cc"] += cc
                agg["cr"] += cr
                agg["out"] += out
                agg["reason"] += reason

    vendor = {}
    vendor_note = ""
    for root in roots:
        db = os.path.join(root, "v2", "sqlite", "runtime-state.sqlite")
        if not os.path.isfile(db):
            continue
        try:
            conn = open_ro(db)
            got = conn.execute(
                "select count(*), coalesce(sum(input_tokens), 0), coalesce(sum(cache_write_tokens), 0),"
                " coalesce(sum(cache_read_tokens), 0), coalesce(sum(output_tokens), 0),"
                " coalesce(sum(reasoning_tokens), 0) from local_runtime_token_usage"
            ).fetchone()
            conn.close()
        except sqlite3.Error as exc:
            vendor_note = f" · runtime store unreadable ({exc})"
            break
        vendor = dict(zip(("n", "in", "cc", "cr", "out", "reason"), (float(x) for x in got)))
        break

    return {
        "n": n,
        "in": agg["in"],
        "cc": agg["cc"],
        "cr": agg["cr"],
        "out": agg["out"],
        "reason": agg["reason"],
        "credits": 0.0,
        "_vendor": vendor,
        "notes": f"{logs} transcripts, totalTokens identity breaks={broken} (must be 0), "
                 f"zero-usage rows={zero}, repeats collapsed={repeats} "
                 f"(a rewritten history is re-read whole), records without a message id={idless}, "
                 f"unstamped={unstamped}" + vendor_note,
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
        "workbuddy": workbuddy(),
        "antigravity": antigravity(),
        "ccswitch": ccswitch(),
        "kimicode": kimicode(),
        "minimaxcode": minimaxcode(),
    }


FIELDS = ["n", "in", "cc", "cr", "out", "reason", "credits"]


def row_identity(index_db, tool, keyed, tolerance):
    """Per-row identity for a source whose vendor prunes history (zcode's table
    rebuilds, Cline's transcripts rewrite in place).

    Every row the source still holds must appear in the index once with the
    same net numbers (a row newer than the index's newest event may simply not
    be imported yet — the live-lag `--bracket` handles everywhere else). Events
    only the index has are the vendor's deletions: quantified and handed back
    so the aggregate comparison can subtract them, never trusted as numbers.
    Returns `(failures, deleted_field_sums, note)`."""
    conn = open_ro(index_db)
    try:
        ev = conn.execute(
            "select dedupe_key, ts_ms, in_tok, cc_tok, cr_tok, out_tok, reason_tok"
            " from event where tool = ?1 and dedupe_key is not null",
            (tool,),
        ).fetchall()
    finally:
        conn.close()
    if not ev:
        return ([f"{tool}: no keyed events in the index"], {}, "identity not checkable")
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
        failures.append(f"{tool}: {len(missing)} row(s) the source has but the index never saw, e.g. {missing[0]}")
    if wrong:
        failures.append(f"{tool}: {len(wrong)} row(s) whose numbers differ, e.g. {wrong[0]}: {idx[wrong[0]]} != {must[wrong[0]]}")
    tokens = sum(v for f, v in sums.items() if f != "n")
    note = (
        f"per-row identity: {len(must)} checked, {len(deleted)} vendor-deleted "
        f"({tokens:,.0f} tok) kept as history"
    )
    return failures, sums, note


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bracket", action="store_true", help="re-read everything after rebuilding the index")
    ap.add_argument("--db", default=os.path.join(data_dir(), "tokenme", "index.db"))
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
        repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        binary = os.path.join(
            repo,
            "target",
            "release",
            "tokenme.exe" if os.name == "nt" else "tokenme",
        )
        if os.path.isfile(binary):
            command = [binary, "--db", args.db, "--offline", "index", "--rebuild"]
        else:
            cargo = shutil.which("cargo")
            if not cargo:
                raise SystemExit("tokenme release binary and cargo are both unavailable; build the CLI first")
            command = [
                cargo,
                "run",
                "--quiet",
                "--release",
                "-p",
                "usage-cli",
                "--",
                "--db",
                args.db,
                "--offline",
                "index",
                "--rebuild",
            ]
        subprocess.run(command, check=True, cwd=repo)
        stored = index_totals(args.db)
        high = compute_all()
        print(f"[bracket] pass 2 done in {time.time() - START:.0f}s", flush=True)

    failures = []
    # Sources whose vendor prunes history out from under an append-only index:
    # the per-row identity proves the survivors and quantifies the deletions.
    deleted_by_tool = {}
    identity_notes = {}
    for tool in ("zcode", "cline"):
        if "_keyed" in low.get(tool, {}):
            id_failures, sums, note = row_identity(args.db, tool, low[tool]["_keyed"], args.tolerance)
            failures.extend(id_failures)
            deleted_by_tool[tool] = sums
            identity_notes[tool] = note
    # Sources that keep their own tally of these same records (MiniMax Code's
    # runtime table, Cline's per-session accumulators). Two readings of one
    # transcript can agree by construction; agreeing with the product's accounting
    # is the check that the *mapping* of the stages is right. Only the fields the
    # vendor actually publishes are compared — a ledger that counts no rows must
    # not be scored on `n`.
    for tool in sorted(low):
        ven = low[tool].get("_vendor")
        if not isinstance(ven, dict) or not ven:
            continue
        for f in ven:
            mine, theirs = low[tool].get(f, 0.0), ven[f]
            if mine == theirs:
                continue
            rel = abs(mine - theirs) / max(abs(theirs), 1.0)
            if rel > args.tolerance:
                failures.append(
                    f"{tool}.{f}: the vendor's own ledger says {theirs:,.0f}, "
                    f"the transcripts say {mine:,.0f}"
                )
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
            # The vendor prunes rows the index already keeps as history
            # (zcode's rebuilding table, Cline's rewriting transcripts); the
            # per-row identity proved the survivors, so the aggregate is
            # compared against the index *minus* the quantified deletions.
            gone = deleted_by_tool.get(tool, {}).get(f, 0.0)
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
        if tool in identity_notes:
            print(f"{'':<12}  note: {identity_notes[tool]}")
    print("-" * 88)
    print(f"{len(failures)} mismatched field(s) at tolerance {args.tolerance * 100:.2f}%")
    for f in failures:
        print(f"  ! {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
