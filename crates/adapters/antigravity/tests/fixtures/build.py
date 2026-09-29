#!/usr/bin/env python3
"""Rebuild the Antigravity fixtures from the real databases on disk.

Dev-time only: no test ever reads `~/.gemini` — the committed files below are
what the tests consume. Run as `python3 crates/adapters/antigravity/tests/fixtures/build.py`.

What gets committed, and what does not:

* `gen_metadata.hex`  — real `gen_metadata.data` blobs, trimmed to the wire
  fields the mapping reads: the chat-model message keeps `#3`, `#4` (usage),
  `#9` (timing), `#11`, `#12`, `#19`, `#21`, each byte-for-byte as stored. The
  dropped siblings (`#7`, `#15`, `#17`, and the request/response messages) are
  what carry tool schemas and prompt text, so no transcript content lands here.
* `steps_metadata.hex` — the `step_type = 15` row for the same turn, trimmed to
  `#1` (Timestamp), `#9` (the same usage message) and `#20`, with the trajectory
  and conversation UUIDs inside `#20` replaced by synthetic ones.
* `expected.tsv`      — the ground truth a *second, independent* decoder (this
  script) measured off the same rows, so the Rust tests assert against numbers
  derived elsewhere rather than against this crate's own output.

Empty `steps_metadata.hex` line = the row had no matching step (the turns with
no usage never got one).
"""
import os
import re
import sqlite3

FIX = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.expanduser("~/.gemini/antigravity-cli/conversations")
KEEP_CM = [3, 4, 9, 11, 12, 19, 21]
KEEP_STEP = [1, 9, 20]
# Every UUID in the committed bytes is replaced by a stable synthetic one: the
# step rows carry trajectory and conversation ids that identify the author's own
# sessions, and nothing in the mapping depends on their values.
UUID_RE = re.compile(rb"[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}")
SEEN: dict = {}


def synth(match):
    if match not in SEEN:
        SEEN[match] = b"00000000-0000-4000-8000-%012d" % len(SEEN)
    return SEEN[match]
# Chosen for shape: cache read present and absent, an empty usage message, and a
# row from each of two other conversations.
PICKS = [
    ("c2d7ca81-51e5-4551-a630-c292bf7806ae", 0),
    ("c2d7ca81-51e5-4551-a630-c292bf7806ae", 1),
    ("c2d7ca81-51e5-4551-a630-c292bf7806ae", 2),
    ("c2d7ca81-51e5-4551-a630-c292bf7806ae", 41),
    ("432adb81-3c1e-4a03-8020-a7a0fba14f73", 0),
    ("8668382c-b7e3-4d13-9e09-ebfdc4dac029", 0),
    ("292391de-b992-4c47-a9af-d098e481823f", 0),
    ("f7536091-f8be-4f50-b164-74b4f25b424e", 0),
]


def rv(b, i):
    out = 0
    shift = 0
    while True:
        x = b[i]
        i += 1
        out |= (x & 0x7F) << shift
        if not x & 0x80:
            return out, i
        shift += 7


def fields(b):
    out = []
    i = 0
    while i < len(b):
        tag, i = rv(b, i)
        fn, wt = tag >> 3, tag & 7
        if wt == 0:
            v, i = rv(b, i)
            out.append((fn, "V", v))
        elif wt == 2:
            ln, i = rv(b, i)
            out.append((fn, "L", b[i:i + ln]))
            i += ln
        elif wt == 1:
            out.append((fn, "8", int.from_bytes(b[i:i + 8], "little")))
            i += 8
        elif wt == 5:
            out.append((fn, "4", int.from_bytes(b[i:i + 4], "little")))
            i += 4
        else:
            raise SystemExit("unsupported wire type %d at %d" % (wt, i))
    return out


def get(fs, fn, kind=None):
    for a, k, c in fs:
        if a == fn and (kind is None or k == kind):
            return c
    return None


def enc(value):
    out = bytearray()
    while True:
        b = value & 0x7F
        value >>= 7
        out.append(b | (0x80 if value else 0))
        if not value:
            return bytes(out)


def put_v(o, fn, value):
    o += enc(fn << 3) + enc(value)


def put_l(o, fn, payload):
    o += enc((fn << 3) | 2) + enc(len(payload)) + payload


def rebuild(src, keep):
    """A new message holding verbatim every occurrence of each field in `keep`."""
    o = bytearray()
    parsed = fields(src)
    for fn in sorted(keep):
        for a, k, c in parsed:
            if a == fn:
                put_v(o, fn, c) if k == "V" else put_l(o, fn, c)
    return bytes(o)


def encode_fields(pairs):
    """Re-emit an already-decoded field list, unchanged."""
    o = bytearray()
    for fn, kind, value in pairs:
        put_v(o, fn, value) if kind == "V" else put_l(o, fn, value)
    return bytes(o)


def scrub_strings(blob):
    """Replace every UUID this machine's rows happen to carry."""
    out = []
    for fn, kind, value in fields(blob):
        if kind == "L":
            value = UUID_RE.sub(synth, value)
        out.append((fn, kind, value))
    return encode_fields(out)


def scrub_step(meta):
    """Verbatim, except inside the fields kept from `#1`, `#9` and `#20`."""
    return encode_fields(
        [(fn, kind, scrub_strings(value) if kind == "L" else value) for fn, kind, value in fields(meta) if fn in KEEP_STEP]
    )


def main():
    gen, steps, tsv = [], [], []
    for fidx, (uuid, idx) in enumerate(PICKS):
        conn = sqlite3.connect("file:%s/%s.db?mode=ro" % (SRC, uuid), uri=True)
        blob = conn.execute("SELECT data FROM gen_metadata WHERE idx = ?", (idx,)).fetchone()[0]
        cm = get(fields(blob), 1, "L")
        outer = bytearray()
        put_l(outer, 1, rebuild(cm, KEEP_CM))
        gen.append(bytes(outer).hex())

        uf = fields(get(fields(cm), 4, "L") or b"")
        stage = lambda f: get(uf, f)
        resp = (get(uf, 11, "L") or b"").decode()
        stamp = get(fields(get(fields(cm), 9, "L") or b""), 4, "L")
        ts = ""
        if stamp:
            sf = fields(stamp)
            ts = get(sf, 1) * 1000 + (get(sf, 2) or 0) // 1_000_000

        step = ""
        if resp:
            for (meta,) in conn.execute(
                "SELECT metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL"
            ):
                if meta is None:
                    continue
                joined = get(fields(get(fields(meta), 9, "L") or b""), 11, "L")
                if joined and joined.decode() == resp:
                    step = scrub_step(meta).hex()
                    break
        steps.append(step)
        tsv.append("\t".join(str(x if x is not None else "") for x in
                             [fidx, uuid[:8], idx, stage(1), stage(2), stage(3), stage(5),
                              stage(9), stage(10), resp, ts]))

    with open(os.path.join(FIX, "gen_metadata.hex"), "w") as fh:
        fh.write("\n".join(gen) + "\n")
    with open(os.path.join(FIX, "steps_metadata.hex"), "w") as fh:
        fh.write("\n".join(steps) + "\n")
    with open(os.path.join(FIX, "expected.tsv"), "w") as fh:
        fh.write("\n".join(tsv) + "\n")
    print("wrote %d gen rows, %d step rows" % (len(gen), sum(1 for s in steps if s)))


if __name__ == "__main__":
    main()
