#!/usr/bin/env python3
"""Gate: no spawn from the Windows GUI process may paint a console window.

Why this exists: on Windows a console-subsystem child started by a GUI process
(the tray app, which has no console of its own) gets a console allocated for it,
and the default terminal paints that console — a black window that steals focus
for as long as the child lives. `gh auth token` in the Copilot quota probe did
exactly that once every TTL pass; the panel's own link buttons did it through
`cmd /c start`. Both are silent in code review and loud on the user's screen,
so this checks the shape instead of trusting eyes.

The rule, per `Command::new(...)` site that can run in a Windows build:
  - the program must be one whose spawn cannot allocate a console, or
  - the call must be inside a platform guard that excludes Windows, or
  - the call must carry `creation_flags(CREATE_NO_WINDOW)` within a few lines.

Exemptions are lists of names, not per-line waivers, so a new offender cannot
quietly join them by editing a comment:
  - CONSOLE_EXEMPT: GUI-subsystem binaries. Explorer never allocates a console,
    and adding the flag to it would be noise.
  - UNIX_ONLY_NAMES: programs that do not exist on Windows. Their spawn fails
    before any window can appear, so the flag would be dead code. A name here
    must be a program the Windows PATH cannot plausibly resolve.

Usage: scripts/check-no-window-spawns.py [--verbose] [<path>]
Exit 0 clean / 1 findings. <path> audits one file or directory (the control).

What it cannot see, stated plainly: a program named by a variable (`Command::new(bin)`)
is checked for the flag but not for which binary it resolves to, and a runtime OS
switch (`match env::consts::OS`) is trusted to keep its Windows arm honest. Both
shapes still need the flag within 16 lines, so the class this gate exists for — an
unflagged console spawn in a Windows-reachable path — cannot slip past either way.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCAN = ["crates", "apps"]
SKIP_DIR_PARTS = {"tests", "target", "node_modules"}

# GUI-subsystem: no console is ever allocated for these.
CONSOLE_EXEMPT = {"explorer", "explorer.exe", "explorer.EXE"}
# Cannot resolve on Windows, so the spawn errors out before a window exists.
UNIX_ONLY_NAMES = {
    "open", "xdg-open", "sh", "/bin/sh", "/usr/bin/security", "security",
    "ps", "lsof", "pgrep", "osascript", "hdiutil", "codesign", "ditto",
}

NEW_RE = re.compile(r"Command::new\(\s*(?:&?str::from_utf8\(\s*)?\"([^\"]+)\"|Command::new\(\s*([A-Za-z_][\w:]*)")
# Attributes and runtime guards that keep the call out of a Windows build.
NOT_WINDOWS_RE = re.compile(r'#\[cfg\((?:unix|target_os\s*=\s*"macos"|all\([^)]*unix|not\(target_os\s*=\s*"windows"\))')
RUNTIME_GUARD_RE = re.compile(r'cfg!\(\s*target_os\s*=\s*"macos"|cfg!\(\s*unix|env::consts::OS')
FLAGS_RE = re.compile(r"creation_flags")
FN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+")

WINDOW = 16  # lines after the spawn in which the flag must appear


def audit_file(path: Path):
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    findings = []
    for i, line in enumerate(lines):
        m = NEW_RE.search(line)
        if not m:
            continue
        program = m.group(1) or m.group(2) or "?"
        base = program.replace("\\", "/").rsplit("/", 1)[-1]
        if base in CONSOLE_EXEMPT or program in UNIX_ONLY_NAMES or base in UNIX_ONLY_NAMES:
            continue
        before = "\n".join(lines[max(0, i - 8):i])
        around = "\n".join(lines[max(0, i - 8):min(len(lines), i + WINDOW)])
        if NOT_WINDOWS_RE.search(before):
            continue
        # A runtime OS switch (`match env::consts::OS`, `if !cfg!(..macos) { return }`)
        # keeps the arm out of the Windows path; the arm's own line carries no cfg.
        if RUNTIME_GUARD_RE.search(before) and not re.search(r'"windows"\s*=>', line):
            continue
        if FLAGS_RE.search("\n".join(lines[i:min(len(lines), i + WINDOW)])):
            continue
        findings.append((path, i + 1, program, "no CREATE_NO_WINDOW within %d lines" % WINDOW, around))
    return findings


def main(argv):
    verbose = "--verbose" in argv
    args = [a for a in argv[1:] if not a.startswith("--")]
    targets = []
    if args:
        p = Path(args[0])
        targets = [p] if p.is_file() else sorted(p.rglob("*.rs"))
    else:
        for part in SCAN:
            targets.extend(sorted((ROOT / part).rglob("*.rs")))
    targets = [t for t in targets if not SKIP_DIR_PARTS & set(t.parts) and ".git" not in t.parts]

    findings = []
    for t in targets:
        findings.extend(audit_file(t))

    for path, ln, program, why, _ctx in findings:
        rel = path.relative_to(path.resolve().anchor) if not path.is_absolute() else path
        try:
            rel = path.resolve().relative_to(ROOT)
        except ValueError:
            rel = path
        print(f"console spawn: {rel}:{ln} — Command::new(\"{program}\") {why}")
        if verbose:
            print("    " + "\n    ".join(_ctx.strip() for _ctx in _ctx.splitlines() if _ctx.strip()))

    scope = args[0] if args else "crates/ + apps/"
    if findings:
        print(f"scan-public-style gate: {len(findings)} finding(s) over {len(targets)} files in {scope}")
        return 1
    print(f"no-window-spawn: CLEAN — {len(targets)} files in {scope}, every Windows-reachable spawn is silent")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
