#!/usr/bin/env python3
"""Measure what the panel actually shows, frame by frame, on a cold start.

Why this exists: the cold-start complaint ("1-2 s of white panel") is a claim
about the first composited frame, and nothing else on this machine can answer
it. `screencapture -l<windowid>` returns a pure black backing store for a window
that has never been composited; a still screenshot is one sample and cannot show
a sequence; `panel.log` says when the *page* reported content and where the code
asked the window to go — which is not the same as where the pixels appeared.
Display-rate video of the whole screen is the only instrument that sees the frame
the user sees, so this script drives that instrument and prints the timeline.

It quits the app, records the display, cold-starts the app with its panel opening
itself, then finds the panel by pixels: the largest changed region per frame, its
bounding box in points (which is also the anchor proof), and inside it whether the
frame carries the brand ring, nothing, or a full report. Frames at every state
change are saved as PNGs for a human to look at — the numbers locate the moment,
the pictures settle what it was.

Usage:
  scripts/verify-cold-start.py                       # whole display, auto bbox
  scripts/verify-cold-start.py --app dist/TokenMe.app --duration 10
  scripts/verify-cold-start.py --region 1500,30,420,700   # points, top-left origin

`--region` is in POINTS with a top-left origin (what `screencapture -R` takes) and
only narrows the recording; the panel is still located by its pixels. A Retina
display reports physical pixels to the app and points to the capture API — this
script works in points throughout, so its numbers are comparable to the ones in
System Settings, not to the ones in the log.
"""

import argparse
import glob
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

import cv2
import numpy as np

ACCENT_HUE_RANGE = (76, 94)  # the brand teal, in OpenCV's 0-180 hue scale
ACCENT_S_MIN = 80
ACCENT_V_MIN = 60
CHANGED_DELTA = 28  # per-channel mean-abs difference that counts as "not wallpaper"
BLANK_INK = 1.5  # % of the panel's own area that differs from its background
REPORT_INK = 4.5  # measured here: a loading card sits near 1 %, a full report 6–10 %


def run(cmd):
    return subprocess.run(cmd, check=False)


def parse_region(text):
    parts = [int(p) for p in text.split(",")]
    if len(parts) != 4:
        raise argparse.ArgumentTypeError("--region wants x,y,w,h in points")
    return parts


def start_movie(region, out):
    if os.path.exists(out):
        os.remove(out)
    cmd = ["screencapture", "-v", "-x"]
    if region:
        cmd += ["-R", f"{region[0]},{region[1]},{region[2]},{region[3]}"]
    return subprocess.Popen(
        cmd + [out], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )


def stop_movie(proc, out, timeout=15.0):
    for sig in (signal.SIGINT, signal.SIGTERM):
        proc.send_signal(sig)
        deadline = time.time() + timeout
        while time.time() < deadline and proc.poll() is None:
            time.sleep(0.1)
        if proc.poll() is not None:
            break
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    deadline = time.time() + timeout
    while time.time() < deadline and not os.path.exists(out):
        time.sleep(0.2)
    return out


def bundle_processes(app):
    """Every name this bundle's executable could be running as.

    `mainBinaryName` has been `tokenme-bar`, `tokenme` and `TokenMe`, and copying a
    new bundle over an old one on a case-insensitive volume keeps the earlier
    spelling on disk — so the historical names are included, and matching is exact
    (`pgrep -x`) rather than a substring that could hit an unrelated process.
    """
    found = {p.name for p in (Path(app) / "Contents" / "MacOS").glob("*") if p.is_file()}
    return found | {"tokenme", "TokenMe", "tokenme-bar"}


def launch(app):
    """Cold-start the app with its panel opening itself.

    `TOKENME_SHOW_PANEL` is the app's own QA hook (see lib.rs): the window is
    ordered front as early as the process can manage, which is the worst case for
    the page and therefore the case worth measuring. The bundle's binary is exec'd
    directly because `open -a` does not pass the environment through. It is found
    by listing `Contents/MacOS`, not by guessing its name: `mainBinaryName` has
    been `tokenme`, `TokenMe`, and `tokenme-bar` across this product's life, and a
    hardcoded name here would silently measure nothing after a rename.
    """
    binaries = sorted(
        p for p in (Path(app) / "Contents" / "MacOS").glob("*") if p.is_file()
    )
    if not binaries:
        sys.exit(f"verify-cold-start: {app} is not a built app bundle")
    if len(binaries) > 1:
        sys.exit(f"verify-cold-start: {app} has more than one executable: {binaries}")
    binary = binaries[0]
    if not binary.exists():
        sys.exit(f"verify-cold-start: {binary} is not a built app bundle")
    env = dict(os.environ, TOKENME_SHOW_PANEL="1")
    return subprocess.Popen(
        [str(binary)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env
    )


def to_points(frame, region, native_width):
    """Downscale a recorded frame so one analysis pixel is one point.

    The display is captured at its native width; dividing that out and multiplying
    by the logical width the user sees gives a 1:1 point grid, which is what every
    number in the report is in.
    """
    if region:
        # A region capture is already in points at native scale; the frame is
        # native pixels, so shrink it by the display scale.
        scale = frame.shape[1] / float(region[2])
    else:
        scale = frame.shape[1] / float(native_width)
    if scale > 1.0:
        size = (int(round(frame.shape[1] / scale)), int(round(frame.shape[0] / scale)))
        return cv2.resize(frame, size, interpolation=cv2.INTER_AREA)
    return frame


def movie_baseline(mov, region, native_width, preroll, fps):
    """Mean of the pre-launch frames — the wallpaper the panel appears over.

    Deliberately not a separate `screencapture` still: a still and the video
    frames are encoded differently enough (colour profile, sharpening) that every
    pixel "changes" against a still baseline, and the whole desktop reads as the
    panel. That mistake made one run report a full-screen panel at t=-0.8s.
    """
    cap = cv2.VideoCapture(mov)
    frames = max(3, int(preroll * fps * 0.7))
    acc, seen = None, 0
    for _ in range(frames):
        ok, frame = cap.read()
        if not ok:
            break
        frame = to_points(frame, region, native_width)
        acc = frame.astype(np.float32) if acc is None else acc + frame.astype(np.float32)
        seen += 1
    cap.release()
    if seen < 3:
        sys.exit("verify-cold-start: the recording has no pre-launch frames to measure against")
    return (acc / seen).astype(np.uint8), seen


def panel_size_from_config():
    """The panel's configured window size in points, straight out of tauri.conf.json.

    The blob finder needs it: an unrelated white window crossing the screen reads
    as a bigger change than the panel does, and one run really did report a
    394×650 "blank panel" that turned out to be somebody's ChatGPT window.
    """
    conf = Path(__file__).resolve().parent.parent / "apps/tokenme-bar/src-tauri/tauri.conf.json"
    text = conf.read_text(encoding="utf-8")
    m = re.search(r'"width":\s*([0-9.]+)', text)
    n = re.search(r'"height":\s*([0-9.]+)', text)
    if not (m and n):
        sys.exit(f"verify-cold-start: {conf} carries no width/height for the panel window")
    return float(m.group(1)), float(n.group(1))


def panel_bbox(changed, want, tolerance=0.04, menu_band=100):
    """The panel's own blob: its configured size, hanging in the menu-bar band.

    Size alone is not enough — a ChatGPT window measured 394×650 against the
    panel's 400×660, well inside tolerance, and one run reported it as a blank
    panel. So a blob must also start within `menu_band` points of the top of the
    screen, which is the acceptance criterion for the anchor itself. Anything the
    size gate let through but the band rejected comes back as a rejected candidate
    so a regression reads as "the panel is somewhere it should not be", never as
    "the panel never appeared".
    """
    lo, hi = 1.0 - tolerance, 1.0 + tolerance
    mask = (changed.astype(np.uint8)) * 255
    mask = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, np.ones((7, 7), np.uint8))
    count, labels, stats, _ = cv2.connectedComponentsWithStats(mask, 8)
    best, best_y, rejected = None, None, []
    for i in range(1, count):
        x, y, w, h, area = stats[i]
        if not (w <= want[0] * hi and w >= want[0] * lo and h <= want[1] * hi and h >= want[1] * lo):
            continue
        box = (int(x), int(y), int(w), int(h))
        if int(y) > menu_band:
            rejected.append(box)
            continue
        if best is None or int(y) < best_y:
            best, best_y = box, int(y)
    return best, rejected


def ink_fraction(crop):
    """How much of the panel differs from its own background.

    The panel is a dark translucent sheet: comparing it to the wallpaper says
    only "a panel is there", which is true from its very first composited frame
    and useless for "has it drawn anything yet". So measure inside: the median
    colour of the sheet is the sheet, and everything else is content. A blank
    panel scores 0–1 %, a loading card a little more, a full report 6–10 %.
    """
    ground = np.median(crop.reshape(-1, 3), axis=0)
    dist = np.abs(crop.astype(np.float32) - ground).mean(axis=2)
    return 100.0 * float((dist > 24.0).mean())


def accent_count(crop):
    """Teal pixels — the brand ring's colour, so a shell or a bar chart is present."""
    hsv = cv2.cvtColor(crop, cv2.COLOR_BGR2HSV)
    lo, hi = ACCENT_HUE_RANGE
    m = (
        (hsv[..., 0] >= lo)
        & (hsv[..., 0] <= hi)
        & (hsv[..., 1] >= ACCENT_S_MIN)
        & (hsv[..., 2] >= ACCENT_V_MIN)
    )
    return int(m.sum())


def session_guard():
    """Refuse to measure a locked or sleeping display, and say why.

    Not a nicety: `screencapture -v` writes nothing at all while the screen is
    locked (one run produced a 4.9 s movie and then an empty file, and the
    analysis would have reported "the panel never appeared"), and window geometry
    read while parked is the window server's placeholder, not the panel's. Exit
    code 2 means "no measurement was taken", never "the measurement failed".
    """
    try:
        import Quartz
    except ImportError:
        print("verify-cold-start: pyobjc/Quartz unavailable — continuing unguarded")
        return
    display = Quartz.CGMainDisplayID()
    locked = bool(
        dict(Quartz.CGSessionCopyCurrentDictionary() or {}).get("CGSSessionScreenIsLocked", 0)
    )
    asleep = bool(Quartz.CGDisplayIsAsleep(display))
    # Not every Quartz build exports CGDisplayIsStarted; a missing probe must not
    # be read as "the display is off".
    started = bool(getattr(Quartz, "CGDisplayIsStarted", lambda _: 1)(display))
    if locked or asleep or not started:
        print(
            f"verify-cold-start: refusing to measure — locked={locked} asleep={asleep} "
            f"started={started}. Wake the display and re-run: a recording taken behind a "
            "lock is empty, and an empty recording is not evidence the panel never showed."
        )
        sys.exit(2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--region", type=parse_region, help="x,y,w,h in points")
    ap.add_argument("--app", default="/Applications/TokenMe.app")
    ap.add_argument("--duration", type=float, default=9.0)
    ap.add_argument("--native-width", type=int, default=1920, help="logical display width in points")
    ap.add_argument("--panel-size", help="override the panel's WxH in points (default: tauri.conf.json)")
    ap.add_argument("--out-dir", default="/tmp/tokenme-coldstart")
    ap.add_argument("--no-quit", action="store_true")
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    session_guard()
    out_dir.mkdir(parents=True, exist_ok=True)
    for stale in glob.glob(str(out_dir / "*.png")) + glob.glob(str(out_dir / "*.mov")):
        os.remove(stale)

    if args.panel_size:
        w, h = [float(v) for v in args.panel_size.lower().split("x")]
        want = (w, h)
    else:
        want = panel_size_from_config()
    print(f"verify-cold-start: looking for the panel at {want[0]:.0f}×{want[1]:.0f} points")

    if not args.no_quit:
        # Kill by the names this bundle can actually produce, not by one hardcoded
        # spelling: `mainBinaryName` has been tokenme-bar, tokenme and TokenMe, and a
        # case-insensitive copy-over keeps the old one running. A surviving instance
        # does not fail this run — it quietly measures a warm open and reports the
        # panel as never appearing, which is the exact wrong verdict twice over.
        names = bundle_processes(args.app)
        for name in sorted(names):
            run(["pkill", "-x", name])
        deadline = time.time() + 10
        while time.time() < deadline:
            alive = [n for n in sorted(names) if run(["pgrep", "-x", n]).returncode == 0]
            if not alive:
                break
            time.sleep(0.2)
        else:
            sys.exit(
                "verify-cold-start: "
                + ", ".join(alive)
                + " refused to quit — a running instance owns the window, so this "
                "recording would measure a warm open. Quit it and re-run."
            )
        # A process still holding the window would put the panel on screen before
        # the recording starts, and the whole run would measure a warm open.
        time.sleep(1.5)

    mov = str(out_dir / "cold.mov")
    proc = start_movie(args.region, mov)
    preroll = 0.8  # frames must be flowing before anything moves
    time.sleep(preroll)
    launch(args.app)
    time.sleep(max(0.2, args.duration - preroll))
    stop_movie(proc, mov)

    cap = cv2.VideoCapture(mov)
    fps = cap.get(cv2.CAP_PROP_FPS) or 0.0
    if not cap.isOpened() or fps <= 0:
        sys.exit(f"verify-cold-start: cannot read {mov} (empty recording? screen-recording permission?)")
    got = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))
    cap.release()
    session_guard()
    if got < args.duration * fps * 0.7:
        # The display went to sleep mid-run: the recorder stops writing and the
        # remaining seconds simply are not there. Reading that as "the panel never
        # appeared" is how a measurement becomes a fiction.
        sys.exit(
            f"verify-cold-start: the recording covers {got / fps:.1f}s of the {args.duration:.1f}s "
            "asked for — the display slept or locked mid-run. No verdict."
        )
    baseline, pre = movie_baseline(mov, args.region, args.native_width, preroll, fps)
    cap.release()
    frames = int(cv2.VideoCapture(mov).get(cv2.CAP_PROP_FRAME_COUNT))
    print(
        f"verify-cold-start: {frames} frames @ {fps:.1f} fps; baseline = mean of the {pre} "
        f"pre-launch frames, {baseline.shape[1]}×{baseline.shape[0]} points "
        f"(region {args.region or 'whole display'}); t=0 is the launch"
    )

    cap = cv2.VideoCapture(mov)
    prev_state, index, anchors, first, rejected = None, -1, [], {}, []
    while True:
        ok, frame = cap.read()
        if not ok:
            break
        index += 1
        frame = to_points(frame, args.region, args.native_width)
        if frame.shape != baseline.shape:
            frame = cv2.resize(frame, (baseline.shape[1], baseline.shape[0]))
        diff = np.abs(frame.astype(np.int16) - baseline.astype(np.int16)).mean(axis=2)
        bbox, misses = panel_bbox(diff > CHANGED_DELTA, want)
        for box in misses:
            if box not in rejected:
                rejected.append(box)
        t = index / fps - preroll
        if bbox is None:
            state, ink, acc = "gone", 0.0, 0
        else:
            x, y, w, h = bbox
            crop = frame[y + 8 : y + h - 8, x + 8 : x + w - 8]
            ink = ink_fraction(crop)
            acc = accent_count(frame[y : y + h, x : x + w])
            # The native boot note is brand teal on an otherwise empty sheet, so
            # it reads as accent pixels long before it reads as ink; the HTML
            # shell would look the same and the report covers the panel.
            if ink >= REPORT_INK:
                state = "content"
            elif acc >= 40:
                state = "note"
            elif ink >= BLANK_INK:
                state = "shell"
            else:
                state = "blank"
            anchors.append((t, x, y, w, h, ink))
        first.setdefault(state, (index, t, ink, acc, bbox))
        if state != prev_state:
            name = f"{index:04d}-{t:+.2f}s-{state}.png"
            cv2.imwrite(str(out_dir / name), frame)
            print(
                f"  frame {index:4d} t={t:+6.2f}s  {state:7s}  ink={ink:5.1f}%  accent={acc:6d}  "
                f"bbox={bbox if bbox else '— panel not on screen'}  [saved {name}]"
            )
            prev_state = state
    cap.release()

    print(f"\nverify-cold-start: {index + 1} frames analysed, pictures in {out_dir}")
    print("  ink = the share of the panel's own area that differs from its background")
    for state in ("gone", "blank", "note", "shell", "content"):
        if state in first:
            i, t, ink, acc, bbox = first[state]
            print(f"  first {state:7s}: frame {i:4d} at t={t:+6.2f}s  ink={ink:.1f}%  accent={acc}")
        else:
            print(f"  first {state:7s}: never")
    if anchors:
        settled = anchors[-1]
        moved = [a for a in anchors if a[1] != settled[1] or a[2] != settled[2]]
        first_on_screen = anchors[0]
        print(
            f"  panel on screen from t={first_on_screen[0]:+.2f}s, landed at "
            f"x={settled[1]},y={settled[2]} {settled[3]}×{settled[4]} pt, "
            f"moved {len(moved)} time(s) while open"
        )
    else:
        print("  the panel never appeared in the menu-bar band during the recording")
    if rejected:
        print(
            f"  {len(rejected)} same-size blob(s) were rejected for not hanging off the menu "
            f"bar: {rejected[:4]} — another window, not the panel"
        )


if __name__ == "__main__":
    main()
