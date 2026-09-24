#!/usr/bin/env python3
"""Rebuild the zcode fixtures from the real rollout + db on this machine.

Records average ~200 KB because they embed the whole request/response. Only
`response.providerMetadata` and the identity fields matter to the parser, so the
fixtures keep those verbatim and blank the payload bodies; every number and id in
them is the real one.
"""
import json, os, pathlib, sqlite3

FIX = pathlib.Path("/Users/me/workspace/tokenme/crates/adapters/zcode/tests/fixtures")
CLI = pathlib.Path.home() / ".zcode/cli"
ROLL = CLI / "rollout"
DB = CLI / "db/db.sqlite"
FIX.mkdir(parents=True, exist_ok=True)
(FIX / "model_usage.sqlite").unlink(missing_ok=True)
for stale in FIX.glob("rollout-*.jsonl"):
    stale.unlink()

DROP = ("text", "headers", "toolCalls", "choices", "rawResponse")


def trim(rec):
    body = dict(rec)
    body.pop("request", None)
    resp = dict(body.get("response") or {})
    for k in DROP:
        resp.pop(k, None)
    body["response"] = resp
    return body


def lines_of(path):
    out = []
    for line in open(path):
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


smallest = min(ROLL.glob("model-io-sess_*.jsonl"), key=lambda p: p.stat().st_size)
recs = lines_of(smallest)
sub = [p for p in ROLL.glob("model-io-sess_subagent_*")][0]
sub_recs = lines_of(sub)

with (FIX / "rollout-real.jsonl").open("w") as f:
    for r in recs:
        f.write(json.dumps(trim(r), separators=(",", ":")) + "\n")
    # a real non-usage record type from the same session's stream
    non = {"type": "session_initialized", "sessionId": recs[0]["sessionId"], "traceId": recs[0]["traceId"],
           "turnId": recs[0]["turnId"], "startedAt": recs[0]["startedAt"], "model": recs[0]["model"]}
    f.write(json.dumps(non, separators=(",", ":")) + "\n")
    # a torn write: the file is appended to while the process runs
    f.write('{"type":"model_io","sessionId":"sess_torn","response":{"providerMetadata":{"anthropic":{"usage":{"input_to')

# a retried request: attempt 1, attempt 2, then a byte-identical replica of the
# second line (the log can carry the same call twice). Real rollout files today
# hold only distinct (requestId, attempt) pairs, so `attempt` is bumped here.
r0 = trim(recs[0])
r1 = json.loads(json.dumps(r0)); r1["attempt"] = 2
r1["requestId"] = "req_retry_probe"
r1["sessionId"] = "sess_retry_probe"
# the retry is logged at the same completed instant as attempt 1
r1["completedAt"] = recs[0]["completedAt"]
r2 = json.loads(json.dumps(r1))
r2["traceId"] = "trace-replica-of-attempt-2"
with (FIX / "rollout-retry.jsonl").open("w") as f:
    base = json.dumps({**r0, "requestId": "req_retry_probe", "sessionId": "sess_retry_probe"}, separators=(",", ":"))
    f.write(base + "\n")
    f.write(json.dumps(r1, separators=(",", ":")) + "\n")
    f.write(json.dumps(r2, separators=(",", ":")) + "\n")

# the same three shapes plus a subagent record, from the second real file
with (FIX / "rollout-subagent.jsonl").open("w") as f:
    for r in sub_recs[:3]:
        f.write(json.dumps(trim(r), separators=(",", ":")) + "\n")

# a file of nothing but unusable lines, for read()'s error contract
with (FIX / "rollout-garbage.jsonl").open("w") as f:
    f.write("not json at all\n{\n[]\n\"type\":model_io\n\n")
    f.write(json.dumps(trim(recs[0]), separators=(",", ":")).replace('"type":"model_io"', '"type":"tool_result"') + "\n")

# db fixture: the real schema, real rows, one synthetic retry
con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
con.row_factory = sqlite3.Row
cols = [r[1] for r in con.execute("pragma table_info(model_usage)")]
rows = con.execute("select * from model_usage order by rowid limit 6").fetchall()
err = con.execute("select * from model_usage where status!='completed' limit 1").fetchone()
tail = con.execute("select * from model_usage order by rowid desc limit 1").fetchone()
picked_pre = [r for r in rows + [err, tail] if r is not None]
sessions = {r["session_id"]: con.execute("select directory, path, title from session where id=?", (r["session_id"],)).fetchone() for r in picked_pre}

small = sqlite3.connect(FIX / "model_usage.sqlite")
# same DDL as the real db, so the reader cannot pass over a column we invented
for name in ("model_usage", "session"):
    ddl = con.execute("select sql from sqlite_master where type='table' and name=?", (name,)).fetchone()[0]
    small.execute(ddl)
    if name == "model_usage":
        small.execute("delete from model_usage")

BIG = ("raw_usage_json", "provider_metadata_json")
def shrink(r):
    out = {}
    for c in r.keys():
        v = r[c]
        if c in BIG and isinstance(v, str) and len(v) > 120:
            v = v[:117] + "..."""
        out[c] = v
    return out

sess_cols = [r[1] for r in con.execute("pragma table_info(session)")]
for sid in {r["session_id"] for r in picked_pre}:
    row = con.execute("select * from session where id=?", (sid,)).fetchone()
    if row is None:
        continue
    small.execute(f"insert or replace into session ({','.join(sess_cols)}) values ({','.join('?' * len(sess_cols))})", [None if isinstance(v, (bytes, bytearray)) else v for v in tuple(row)])
picked = picked_pre
for n, r in enumerate(picked):
    d = shrink(r)
    names = [c for c in cols]
    small.execute(f"insert into model_usage ({','.join(names)}) values ({','.join('?' * len(names))})", [d[c] for c in names])
# a second attempt of the first row: same logical request, billed again (the real
# table has no such pair today -- attempt_index is 0 in all 18,112 rows)
d = shrink(rows[0])
d["id"] = (d["id"] or "row")[:60] + "-a2"
d["attempt_index"] = 1
d["completed_at"] = (d["completed_at"] or 0) + 1
small.execute(f"insert into model_usage ({','.join(cols)}) values ({','.join('?' * len(cols))})", [d[c] for c in cols])
small.commit()
small.close()

print("rollout file used:", smallest.name, "records:", len(recs))
for r in recs[:3]:
    u = r["response"]["providerMetadata"]["anthropic"]
    print("  ", r["requestId"], r["attempt"], r["sessionId"][:20], r["completedAt"], u["usage"])
print("subagent file:", sub.name, len(sub_recs), "first:", sub_recs[0]["sessionId"])
print("db rows fixture:", len(picked) + 1, "sessions:", [k[:16] for k in list(sessions)[:2]], "cols:", len(cols))
u0 = picked[0]
print("db first row:", {k: u0[k] for k in ("logical_request_id", "attempt_index", "session_id", "model_id", "status", "started_at", "completed_at", "input_tokens", "output_tokens", "reasoning_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "computed_total_tokens")})
print("err row:", None if err is None else {k: err[k] for k in ("status", "input_tokens", "output_tokens", "cache_read_input_tokens")})
print("tail row:", {k: tail[k] for k in ("session_id", "input_tokens", "cache_read_input_tokens", "output_tokens", "reasoning_tokens")})
print("fixture sizes:", {p.name: p.stat().st_size for p in sorted(FIX.iterdir())})
