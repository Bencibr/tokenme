#!/usr/bin/env python3
"""Latency harness for sync actions, read off the panel log.

The panel emits one line per sync action it completes —

    2026-10-20 18:20:54.686 [info] [pid=68829] [thread=tokio-rt-worker] \
sync: action=set_primary_data_sharing total_ms=10262 outcome=ok

— and, while `start_primary` runs, one line per stage it finishes —

    2026-10-20 16:05:02.279 [info] [pid=68829] [thread=main] \
sync: action=start_primary stage=store_open elapsed_ms=12350

Field order after `sync:` is NOT stable (outcome can precede total_ms,
error_code/gzip_bytes/rows interleave freely), so records are parsed as
generic key=value tokens, never by position.

What it prints:

  - per action: sample count, p50 and max of total_ms, error count, and the
    timestamp of the last occurrence;
  - for start_primary stages: the same columns on elapsed_ms, so the slow
    stage inside a slow start is visible next to the start's own total.

`--last N` is a per-action/per-stage tail window: the newest N samples of
each series feed the stats, so a chatty series (`connection`, every ~15 min)
cannot starve a sparse one (`stop`, once a day).  The default of 20 is a
whole day's worth of connection probes.  `--action` is a substring filter
and may be repeated: `--action start_primary` also catches
`start_primary_worker` should it ever appear.

Exit code is 0 always — including for a missing or unreadable log — because
this is a measuring stick, not a gate; a measurement of nothing is a fact,
not a failure.

Usage:
    scripts/measure-sync-latency.py [LOG] [--action NAME]... [--last N]
"""

import argparse
import os
import re
import sys
from collections import OrderedDict

DEFAULT_LOG = "~/Library/Logs/tokenme/panel.log"

LINE_RE = re.compile(
    r"^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)?)\s+.*?\bsync:\s(?P<rest>.*)$"
)
KV_RE = re.compile(r"(\w+)=([^\s]+)")


def parse_records(text):
    """Yield (timestamp, action, kind, value_ms, outcome) tuples.

    kind is "total" for a completed action line, "stage" for a stage line
    (value_ms is then elapsed_ms and outcome is None).  A line carrying both
    total_ms and stage/elapsed_ms would yield one of each — the tolerant
    reading; the panel never emits that shape today.
    """
    for line in text.splitlines():
        m = LINE_RE.match(line)
        if not m:
            continue
        kv = dict(KV_RE.findall(m.group("rest")))
        action = kv.get("action")
        if not action:
            continue
        if "total_ms" in kv:
            try:
                yield (m.group("ts"), action, "total", float(kv["total_ms"]),
                       kv.get("outcome"))
            except ValueError:
                pass
        stage = kv.get("stage")
        if stage and "elapsed_ms" in kv:
            try:
                yield (m.group("ts"), action, "stage:" + stage,
                       float(kv["elapsed_ms"]), None)
            except ValueError:
                pass


def median(values):
    s = sorted(values)
    n = len(s)
    if n == 0:
        return 0.0
    mid = n // 2
    return s[mid] if n % 2 else (s[mid - 1] + s[mid]) / 2.0


def fmt_ms(v):
    return str(int(v)) if float(v).is_integer() else f"{v:.1f}"


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Per-action sync latency stats from the panel log.")
    ap.add_argument("log", nargs="?", default=DEFAULT_LOG,
                    help="panel log to read (default: %(default)s)")
    ap.add_argument("--action", action="append", default=[], metavar="NAME",
                    help="substring filter on the action name; repeatable")
    ap.add_argument("--last", type=int, default=20, metavar="N",
                    help="newest N samples per action/stage feed the stats "
                         "(default: %(default)s)")
    args = ap.parse_args(argv)

    path = os.path.expanduser(args.log)
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
    except OSError as e:
        print(f"sync latency: cannot read {path}: {e}")
        return 0

    wanted = [a.lower() for a in args.action]

    def keep(action):
        return not wanted or any(w in action.lower() for w in wanted)

    # series key -> list of (ts, ms, outcome|None), in file (chronological) order
    totals = OrderedDict()   # action -> samples
    stages = OrderedDict()   # (action, stage) -> samples
    n_records = 0
    for ts, action, kind, ms, outcome in parse_records(text):
        if not keep(action):
            continue
        n_records += 1
        if kind == "total":
            totals.setdefault(action, []).append((ts, ms, outcome))
        else:
            stages.setdefault((action, kind[6:]), []).append((ts, ms, None))

    window = max(1, args.last)
    print(f"sync action latency — {path}")
    print(f"records parsed: {n_records} (tail window: newest {window} "
          f"per action/stage){', action filter: ' + ' | '.join(args.action) if args.action else ''}")
    if n_records == 0:
        print("no matching sync records found")
        return 0

    print()
    print(f"{'action':<28}{'count':>6}{'p50 ms':>10}{'max ms':>10}"
          f"{'errors':>8}  {'last seen'}")
    for action in sorted(totals):
        samples = totals[action][-window:]
        values = [ms for _, ms, _ in samples]
        errors = sum(1 for _, _, o in samples if o == "error")
        last_ts = samples[-1][0]
        print(f"{action:<28}{len(samples):>6}{fmt_ms(median(values)):>10}"
              f"{fmt_ms(max(values)):>10}{errors:>8}  {last_ts}")

    stage_rows = [k for k in stages if k[0] == "start_primary"]
    if stage_rows:
        print()
        print("start_primary stages (elapsed per stage)")
        print(f"{'  stage':<28}{'count':>6}{'p50 ms':>10}{'max ms':>10}"
              f"{'':>8}  {'last seen'}")
        for _, stage in sorted(stage_rows):
            samples = stages[("start_primary", stage)][-window:]
            values = [ms for _, ms, _ in samples]
            print(f"  {stage:<26}{len(samples):>6}"
                  f"{fmt_ms(median(values)):>10}{fmt_ms(max(values)):>10}"
                  f"{'':>8}  {samples[-1][0]}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
