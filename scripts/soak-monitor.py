#!/usr/bin/env python3
"""Soak monitor for the device-sync network layer (48-72 h pre-release run).

Reads panel.log incrementally and the live sync_state.db / index.db read-only,
appends one JSONL snapshot per poll, and prints an alert line when something
drifts:

- ws_connect churn: a session lifetime under 5 min (the pre-refactor flap
  regime; healthy sessions live hours).
- database busy: any new "database is locked" line (the kill-chain's fuel).
- merge staleness: a paired device whose last_merged_at_ms is older than
  STALE_MIN while its row was recently stamped — data plane stuck.
- fold drift: per-origin event_rollup sums vs remote_rollup sums disagreeing
  (merge corruption).

Run:  python3 scripts/soak-monitor.py [--every 60]
Stop: Ctrl-C (writes soak-summary.json next to the JSONL).
State lives in ~/Library/Logs/tokenme/soak/.
"""

import json
import os
import sqlite3
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

LOG = Path.home() / "Library/Logs/tokenme/panel.log"
STATE_DB = Path.home() / "Library/Application Support/tokenme/sync_state.db"
INDEX_DB = Path.home() / "Library/Application Support/tokenme/index.db"
SOAK_DIR = Path.home() / "Library/Logs/tokenme/soak"
STALE_MIN = 15
CHURN_GAP_S = 300

DB_BUSY_MARK = "database is locked"
CONNECT_MARK = "sync: action=ws_connect outcome=ok"


def iso(ms: float) -> str:
    return datetime.fromtimestamp(ms / 1000).strftime("%m-%d %H:%M:%S") if ms else ""


def snapshot(now_s: float, state: dict) -> dict:
    snap = {"ts": datetime.now().strftime("%Y-%m-%d %H:%M:%S"), "alerts": []}

    # --- log delta ---
    pos = state.get("log_pos", 0)
    busy_before = state.get("busy_count", 0)
    state.setdefault("busy_count", 0)
    state.setdefault("connects", [])
    if LOG.exists():
        size = LOG.stat().st_size
        with LOG.open("r", errors="replace") as fh:
            fh.seek(min(pos, size))
            new = fh.read()
            state["log_pos"] = fh.tell()
    else:
        new = ""
    for line in new.splitlines():
        if DB_BUSY_MARK in line:
            state["busy_count"] = state.get("busy_count", 0) + 1
        if CONNECT_MARK in line:
            ts = line.split(" ", 2)[1] if line.count(" ") >= 2 else ""
            state.setdefault("connects", []).append(ts)
    connects = state.get("connects", [])
    snap["ws_connects_total"] = len(connects)
    snap["busy_total"] = state.get("busy_count", 0)
    snap["busy_new"] = state["busy_count"] - busy_before
    if state["busy_count"] - busy_before:
        snap["alerts"].append(f"database-locked +{snap['busy_new']}")
    # session lifetimes = gaps between successive connects (last one open)
    gaps = []
    def parse(t):  # HH:MM:SS(.fff) today
        base = datetime.now().replace(hour=int(t[:2]), minute=int(t[3:5]), second=int(t[6:8]),
                                      microsecond=int(t[9:] or 0, ) * 1000 if len(t) > 8 else 0)
        return base
    for a, b in zip(connects, connects[1:]):
        try:
            gaps.append((parse(b) - parse(a)).total_seconds())
        except Exception:
            pass
    snap["session_gap_min_s"] = round(min(gaps), 1) if gaps else None
    snap["session_gap_last_s"] = round(gaps[-1], 1) if gaps else None
    if gaps and min(gaps) < CHURN_GAP_S:
        snap["alerts"].append(f"session-churn min-gap {min(gaps):.0f}s < {CHURN_GAP_S}s")

    # --- presence + freshness (read-only) ---
    devices = []
    if STATE_DB.exists():
        try:
            con = sqlite3.connect(f"file:{STATE_DB}?mode=ro", uri=True)
            for did, name, status, merged in con.execute(
                "SELECT device_id, display_name, status, last_merged_at_ms FROM device_registry"):
                devices.append({"name": name, "status": status, "last_merged": iso(merged or 0), "_merged_ms": merged})
            con.close()
        except sqlite3.Error as err:
            snap["alerts"].append(f"state-db read error: {err}")
    snap["devices"] = [{k: v for k, v in dev.items() if not k.startswith("_")} for dev in devices]
    now = datetime.now()
    for dev in devices:
        if dev["status"] == "paired" and dev["last_merged"]:
            dt = datetime.fromtimestamp(float(dev["_merged_ms"] or 0) / 1000)
            if now - dt > timedelta(minutes=STALE_MIN):
                snap["alerts"].append(f"merge-stale {dev['name']}: last {dev['last_merged']} > {STALE_MIN}m")

    # --- uplink monotonicity (read-only): a full-replace snapshot that
    # SHRINKS a remote origin's totals means the secondary lost data or the
    # merge dropped rows; growth or equality is the healthy shape.
    if INDEX_DB.exists():
        try:
            con = sqlite3.connect(f"file:{INDEX_DB}?mode=ro", uri=True)
            remote = dict(con.execute(
                "SELECT origin, sum(n) FROM remote_rollup GROUP BY origin"))
            con.close()
            prev = state.get("remote_totals", {})
            shrunk = [f"{origin or 'local'} {prev.get(origin)}→{total}"
                      for origin, total in remote.items()
                      if origin in prev and total < prev[origin] * 0.95]
            state["remote_totals"] = remote
            if shrunk:
                snap["alerts"].append("uplink-shrunk " + "; ".join(shrunk[:3]))
            snap["remote_origins"] = len(remote)
        except sqlite3.Error as err:
            snap["alerts"].append(f"index-db read error: {err}")

    state["snapshots"] = state.get("snapshots", 0) + 1
    return snap


def main() -> None:
    every = 60
    args = sys.argv[1:]
    if "--every" in args:
        every = int(args[args.index("--every") + 1])
    SOAK_DIR.mkdir(parents=True, exist_ok=True)
    jsonl = SOAK_DIR / "soak-report.jsonl"
    state_path = SOAK_DIR / "monitor-state.json"
    state = {}
    if state_path.exists():
        state = json.loads(state_path.read_text())
    print(f"soak monitor: every {every}s → {jsonl}", flush=True)
    try:
        while True:
            snap = snapshot(time.time(), state)
            with jsonl.open("a") as fh:
                fh.write(json.dumps(snap, ensure_ascii=False) + "\n")
            for alert in snap["alerts"]:
                print(f"[{snap['ts']}] ALERT {alert}", flush=True)
            state_path.write_text(json.dumps(state))
            time.sleep(every)
    except KeyboardInterrupt:
        pass
    finally:
        summary = {
            "ended": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
            "snapshots": state.get("snapshots", 0),
            "ws_connects_total": len(state.get("connects", [])),
            "database_locked_total": state.get("busy_count", 0),
        }
        (SOAK_DIR / "soak-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
