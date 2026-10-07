#!/usr/bin/env python3
"""Gate: nothing personal reaches the public mirror.

The public repo github.com/Bencibr/tokenme mirrors this repository's history
from v0.1.6 on (docs/internal/PUBLIC_DELIVERY.md), so what the hand-built
snapshot commit once guaranteed by construction — the tree is safe to publish
— has to hold for every commit, messages included. This scans a revision
range and fails on the mechanical version of those invariants:

  - identity paths: /Users/<account>, /home/<account> and C:\\Users\\<account>
    for any real account name. The names are derived, never hardcoded: the
    owner component of every non-public remote URL, falling back to the local
    account name when there is no such remote. Placeholders (/Users/demo,
    /Users/dev, ...) pass because they are not on the derived list.
  - private hosts: the hostname of every non-public remote (github.com is
    the only public host), anywhere in blob contents or messages.
  - machine paths: Windows drive paths whose root component is not Users,
    Windows, Program Files or ProgramData — a tooling root on E:\\ or a
    fixture cwd on D:\\ is this machine's layout, not product text.
  - oversized blobs: anything over 1 MiB. The largest legal blob today is
    pricing_fallback.json at 930,861 B.
  - credential shapes: openai/github/gitlab/aws/slack keys and PEM private
    keys, with a boundary so `task-<uuid>` never reads as an `sk-` key.
  - author identity: commit author/committer emails outside ALLOWED_EMAILS.
    Every published snapshot carries agent@local; anything else is a
    deliberate decision, not a default.
  - internal paths: `AGENTS.md` at the root and everything under
    `docs/internal/`, plus the local agent tool state under `.bugx/`. These
    are working notes, never product: the switch deletes the paths from
    every revision and afterwards they stay untracked on disk, so a tracked
    appearance — any revision, any range — is a finding. Until the switch
    the findings are the recorded transitional state, not a surprise.

Findings collapse per file (count + first hit); --verbose prints every match.
Exit 0 clean / 1 findings.

Scope: exactly what a push can carry — every commit under refs/heads and
refs/tags. Local-only refs are deliberately out: Cline's checkpoints
(refs/cline/*), remote-tracking refs, editor backups. Pass an explicit range
for the steady-state scan instead, e.g. `github/main..HEAD` plus any tags.

Usage: scripts/scan-public.py [--verbose] [<rev-range>]
"""

import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlparse

BLOB_LIMIT = 1 << 20
CONTENT_CAP = 16 << 20  # bigger blobs are reported by size; pattern scan skipped
ALLOWED_EMAILS = {"agent@local"}  # the identity every published snapshot carries
PUBLIC_HOSTS = {"github.com"}
DRIVE_ROOTS_OK = {"users", "windows", "program files", "programdata"}

# Working notes that must never publish — deleted from every revision at the
# switch and untracked afterwards; any tracked appearance is the regression
# this catches.
INTERNAL_PATHS = (
    re.compile(r"^AGENTS\.md$"),
    re.compile(r"^docs/internal/"),
    re.compile(r"^\.bugx/"),
)

CRED_PATTERNS = [
    (re.compile(r"(?<![A-Za-z0-9])sk-[A-Za-z0-9_-]{20,}"), "openai-style key"),
    (re.compile(r"(?<![A-Za-z0-9])ghp_[A-Za-z0-9]{20,}"), "github token"),
    (re.compile(r"github_pat_[A-Za-z0-9_]{20,}"), "github fine-grained token"),
    (re.compile(r"(?<![A-Za-z0-9])glpat-[A-Za-z0-9_-]{20,}"), "gitlab token"),
    (re.compile(r"(?<![A-Za-z0-9])AKIA[0-9A-Z]{16}"), "aws access key id"),
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"), "private key"),
    (re.compile(r"(?<![A-Za-z0-9])xox[baprs]-[A-Za-z0-9-]{10,}"), "slack token"),
]

DRIVE_RE = re.compile(r"(?<![A-Za-z0-9_])([A-Za-z]):[\\/]{1,2}([A-Za-z0-9][A-Za-z0-9 ._-]*)")


def git_out(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True).stdout


def sensitive_context():
    """(private_hosts, account_names) derived from the configured remotes."""
    hosts, names = set(), set()
    for remote in git_out("remote").split():
        url = git_out("remote", "get-url", remote).strip()
        host, path = "", ""
        if "://" in url:
            u = urlparse(url)
            host, path = u.hostname or "", u.path
        else:  # scp form: git@host:owner/repo.git
            head, _, path = url.partition(":")
            host = head.rsplit("@", 1)[-1]
        if not host or host.lower() in PUBLIC_HOSTS:
            continue
        hosts.add(host)
        owner = path.strip("/").split("/")[0]
        if owner:
            names.add(owner)
    if not hosts:
        names.add(Path.home().name)
    return hosts, names


def context_patterns(hosts, names):
    pats = []
    for host in sorted(hosts):
        pats.append((re.compile(rf"(?<![A-Za-z0-9.-]){re.escape(host)}(?![A-Za-z0-9.-])", re.I), "private host"))
    for name in sorted(names):
        e = re.escape(name)
        pats.append((re.compile(rf"/(?:Users|home)/{e}(?=[/\"'\\s]|$)"), "identity path"))
        pats.append((re.compile(rf"(?<![A-Za-z0-9_])[A-Za-z]:[\\/]{{1,2}}Users[\\/]{{1,2}}{e}(?=[\\/\"'\\s]|$)", re.I), "identity path"))
    return pats


def snippet(text, start, end):
    s = re.sub(r"[\x00-\x1f\x7f]+", " ", text[max(0, start - 30):end + 30]).strip()
    return f"…{s}…" if start > 30 else f"{s}…"


def line_of(text, offset):
    return text.count("\n", 0, offset) + 1


def drive_hits(text):
    for m in DRIVE_RE.finditer(text):
        root = m.group(2).lower()
        # Single-letter roots (C:\\w, C:/x) are invented placeholders; real
        # machine layout names a directory (tokenme-tools, workspace, hermes).
        if len(root) < 2 or root in DRIVE_ROOTS_OK:
            continue
        # Compressed bytes occasionally spell `<letter>:/<two chars>`; a real
        # path sits in printable text, binary noise sits next to control bytes.
        window = text[max(0, m.start() - 6):m.end() + 6]
        if any(c < " " and c not in "\t\n\r" or c == "\x7f" for c in window):
            continue
        yield m


class Findings:
    def __init__(self):
        self.matches = []  # (kind, subject, where, detail)
        self.rows = {}     # (kind, subject) -> [count, first where, first detail]

    def add(self, kind, subject, where, detail):
        self.matches.append((kind, subject, where, detail))
        if (kind, subject) in self.rows:
            self.rows[(kind, subject)][0] += 1
        else:
            self.rows[(kind, subject)] = [1, where, detail]

    def scan(self, text, where, subject, pats):
        for pat, label in pats:
            for m in pat.finditer(text):
                self.add(label, subject, f"{where}:{line_of(text, m.start())}", snippet(text, m.start(), m.end()))
        for m in drive_hits(text):
            self.add("machine path", subject, f"{where}:{line_of(text, m.start())}", snippet(text, m.start(), m.end()))
        for pat, label in CRED_PATTERNS:
            for m in pat.finditer(text):
                self.add("credential", subject, f"{where}:{line_of(text, m.start())}", f"{label} — {snippet(text, m.start(), m.end())}")

    def report(self, verbose):
        if verbose:
            for kind, _, where, detail in sorted(set(self.matches), key=lambda r: (r[0], r[2])):
                print(f"FAIL  {kind}  {where}  {detail}")
        else:
            for (kind, subject), (count, where, detail) in sorted(self.rows.items(), key=lambda kv: (kv[0][0], kv[0][1])):
                times = f"  ×{count}" if count > 1 else ""
                print(f"FAIL  {kind}  {subject}{times}  first {where} — {detail}")
        kinds = {}
        for kind, _, _, _ in self.matches:
            kinds[kind] = kinds.get(kind, 0) + 1
        summary = ", ".join(f"{v} {k}" for k, v in sorted(kinds.items())) or "nothing"
        print(f"scan-public: {'FAIL — ' if self.matches else 'CLEAN — '}{summary}")
        return 1 if self.matches else 0


def batch_check(shas):
    p = subprocess.run(["git", "cat-file", "--batch-check"],
                       input="\n".join(shas) + "\n", capture_output=True, text=True)
    info = {}
    for line in p.stdout.splitlines():
        f = line.split()
        if len(f) >= 3:
            info[f[0]] = (f[1], int(f[2]))
    return info


def batch_read(shas):
    p = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    p.stdin.write(("\n".join(shas) + "\n").encode())
    p.stdin.close()
    out = {}
    f = p.stdout
    for _ in shas:
        header = f.readline().decode("utf-8", "replace").strip().split()
        if len(header) < 3 or header[1] != "blob":
            continue
        data = f.read(int(header[2]))
        f.readline()
        out[header[0]] = data
    p.wait()
    return out


def blob_paths(range_args):
    """sha -> first path, for every object that carries one in the range."""
    paths = {}
    for line in git_out("rev-list", "--objects", *range_args).splitlines():
        sha, _, path = line.partition(" ")
        if path:
            paths.setdefault(sha, path)
    return paths


def commit_messages(range_args):
    raw = subprocess.run(["git", "log", "-z", "--format=%H%x1f%B", *range_args],
                         capture_output=True, text=True).stdout
    for rec in raw.split("\0"):
        sha, _, body = rec.strip("\n").partition("\x1f")
        if sha:
            yield sha[:8], body


def main():
    args = [a for a in sys.argv[1:] if a != "--verbose"]
    verbose = len(args) != len(sys.argv[1:])
    default_scope = not args
    range_args = args or ["--branches", "--tags"]
    scope = " ".join(range_args)
    hosts, names = sensitive_context()
    pats = context_patterns(hosts, names)
    findings = Findings()

    paths = blob_paths(range_args)
    info = batch_check(list(paths))
    blob_info = {s: v for s, v in info.items() if v[0] == "blob"}
    largest = max((size for _, size in blob_info.values()), default=0)
    commits = len(git_out("rev-list", *range_args).split())
    print(f"scan-public: scope {scope} — {commits} commits, {len(blob_info)} blobs, largest {largest:,} B")

    for sha, (_, size) in blob_info.items():
        if size > BLOB_LIMIT:
            findings.add("oversize blob", paths[sha], f"(blob {sha[:8]})", f"{size:,} B")

    for sha in blob_info:
        path = paths[sha]
        if any(pat.search(path) for pat in INTERNAL_PATHS):
            findings.add("internal path", path, f"(blob {sha[:8]})", path)

    scannable = [s for s, (_, size) in blob_info.items() if size <= CONTENT_CAP]
    skipped = len(blob_info) - len(scannable)
    contents = batch_read(scannable)
    for sha in scannable:
        if sha in contents:
            findings.scan(contents[sha].decode("utf-8", "replace"), paths[sha], paths[sha], pats)
    if skipped:
        print(f"scan-public: {skipped} blob(s) over {CONTENT_CAP:,} B not pattern-scanned")

    seen_emails = {}
    log = git_out("log", "--format=%H%x1f%ae%x1f%ce", *range_args)
    for line in log.splitlines():
        f = line.split("\x1f")
        if len(f) != 3:
            continue
        sha, ae, ce = f
        for who, email in (("author", ae), ("committer", ce)):
            if email not in ALLOWED_EMAILS:
                first, count = seen_emails.get((who, email), (sha[:8], 0))
                seen_emails[(who, email)] = (first, count + 1)

    for sha, body in commit_messages(range_args):
        findings.scan(body, f"message {sha}", "commit messages", pats)

    if default_scope:
        refs = git_out("for-each-ref", "refs/tags", "--format=%(objecttype) %(objectname) %(refname:short)")
        for line in refs.splitlines():
            typ, sha, name = line.split()
            if typ == "tag":
                findings.scan(git_out("cat-file", "-p", sha), f"tag {name}", f"tag {name}", pats)

    for (who, email), (first, count) in seen_emails.items():
        findings.add("author identity", f"{who} {email}", first, f"{count} commit(s)")

    return findings.report(verbose)


if __name__ == "__main__":
    sys.exit(main())
