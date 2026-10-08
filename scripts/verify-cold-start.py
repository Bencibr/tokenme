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
  scripts/verify-cold-start.py --movie old.mov       # re-read a recording, no relaunch

Exit codes: 0 measured and the note moved; 1 measured and the note was frozen (the
loading animation is missing); 2 no measurement (locked or sleeping display).

`--movie` analyses a recording this script already made — which is how the motion
gate got its negative control: build 113's own movie, the one that started the
complaint, must come back "frozen".

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
MOTION_DELTA = 24  # per-pixel change between two note frames that counts as movement
MOTION_PAIR_S = 0.3  # note frames this far apart are compared (a ring must have swept)
MOTION_PIXELS = 40  # the frozen note's own noise floor measured here is 12 (build 113's
# movie, the recording that started this: label pixels, no ring, 127 note frames, and
# 12 teal pixels still "moved" from video encoding on the translucent sheet). A turning
# ring is not near that floor: the drawn control gives 332. So the gate sits at 40 —
# above the floor by 3×, below a real sweep by 8×.

FAINT_S_MIN = 35  # the ring's inner arc is stroked at 28 % alpha, so the strict accent
FAINT_V_MIN = 45  # gate (ACCENT_S/V_MIN) never sees it; motion needs the loose one


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


def tealish(crop):
    """The loose teal gate: hue only, plus a floor low enough for 28 %-alpha strokes.

    `accent_count` asks for a saturated, bright teal, which is right for the shell
    mark and the chart bars and wrong for the ring's faint inner arc — that arc is
    drawn at 0.28 alpha over a translucent sheet and lands near hue 84, sat 40. The
    motion gate wants every pixel the ring touches, so it asks loosely and leans on
    the *change* test to reject the wallpaper, which never changes.
    """
    hsv = cv2.cvtColor(crop, cv2.COLOR_BGR2HSV)
    lo, hi = ACCENT_HUE_RANGE
    return (
        (hsv[..., 0] >= lo) & (hsv[..., 0] <= hi)
        & (hsv[..., 1] >= FAINT_S_MIN) & (hsv[..., 2] >= FAINT_V_MIN)
    )


def note_rect(bbox, high=34, low=8, wide=20):
    """The ring's footprint in *screen* points, from one panel bbox.

    `set_boot_note` centres the ring+label group on the content view's own bounds and
    the ring is the group's top piece, so the ring occupies roughly the panel centre
    minus 31 pt to plus 4 pt vertically and ±17 pt horizontally. This rectangle is
    computed once — from the first note frame — and then sliced out of every frame
    unchanged, because the blob finder re-derives the panel's edges per frame and
    they wobble by a point: re-centring on each frame slides a static label through
    the sample and its teal glyph edges then read as motion. That is how the first
    version of this gate scored build 113 — the recording with no ring at all — as
    "animating". The bottom edge stops above the label's own glyphs where it can.
    If the note stops being centred, or the ring moves within the group, this
    rectangle has to move with it.
    """
    x, y, w, h = bbox
    cy, cx = y + h // 2, x + w // 2
    return cy - high, cy + low, cx - wide, cx + wide


def swept_pixels(a, b, jitter=2):
    """Fewest moved teal pixels between two note frames, over ±jitter pt alignment.

    A window that really does nudge — or a blob whose edges the finder re-derives a
    point apart — carries every static pixel of the label along with it, and those
    glyph edges count as "moved". So each pair is scored at every candidate offset
    and the *quietest* offset wins: a frozen note finds one that makes the two frames
    agree (≈0 pixels), a ring that swept 97° finds none. Skipping this step is how
    the first version of this gate called build 113 — the recording with no ring at
    all — "animating", on 15 pixels of label jitter.
    """
    h, w = a.shape[:2]
    base = a[jitter : h - jitter, jitter : w - jitter]
    base_teal = tealish(base)
    best = None
    for dy in range(-jitter, jitter + 1):
        for dx in range(-jitter, jitter + 1):
            other = b[jitter + dy : h - jitter + dy, jitter + dx : w - jitter + dx]
            changed = (
                np.abs(base.astype(np.int16) - other.astype(np.int16)).mean(axis=2)
                > MOTION_DELTA
            )
            count = int((changed & (base_teal | tealish(other))).sum())
            best = count if best is None else min(best, count)
    return best


def note_motion(samples, drift=3):
    """Did the boot note move? Samples are (t, band crop, (panel x, panel y)).

    The complaint this answers is precise: the note *painted* (label pixels, teal
    hue, ink where a human can read it) and nothing else changed for the whole wait.
    So presence is not enough, and neither is "some frame differs" — the ring turns
    once every 1.3 s, which at 60 fps is a ~4.6° step between neighbours: too small
    to separate from video encoding noise. Frames MOTION_PAIR_S apart are a 97° step
    on the outer arc, which is ~140 swept pixels against a static label's zero.

    Only the note's own rectangle is compared (see `note_rect`), each pair is allowed
    to align within ±2 pt (see `swept_pixels`), and a pair whose panel origin moved by
    more than `drift` points is dropped — past that the screen-fixed rectangle is not
    looking at the same sheet and says nothing either way.
    """
    pairs, best = 0, 0
    origin = samples[0]
    for t, band, (px, py) in samples[1:]:
        if t - origin[0] < MOTION_PAIR_S or band.shape != origin[1].shape:
            continue
        if abs(px - origin[2][0]) > drift or abs(py - origin[2][1]) > drift:
            continue
        pairs += 1
        best = max(best, swept_pixels(origin[1], band))
    return pairs, best


def self_test():
    """The two cases this gate must never get wrong, drawn rather than recorded.

    The real negative control is build 113's own movie, but that needs a display and
    a recording; these two run anywhere and are what the metric is *for*: a ring that
    turns must read as motion, and the same ring slid one point sideways — the exact
    artefact that fooled the first version — must read as frozen.
    """
    teal = (170, 217, 48)  # #30d9aa in BGR
    def ring(angle):
        img = np.zeros((42, 40, 3), np.uint8)
        cv2.ellipse(img, (20, 21), (14, 14), angle, 0, 200, teal, 3)
        return img
    frames = [ring(0), ring(97), ring(194)]
    turned = note_motion([(i * (MOTION_PAIR_S + 0.1), f, (0, 0)) for i, f in enumerate(frames)])
    nudged = note_motion(
        [(i * (MOTION_PAIR_S + 0.1), np.roll(frames[0], (i, i), (0, 1)), (0, 0)) for i in range(3)]
    )
    ok = turned[1] >= MOTION_PIXELS and nudged[1] < MOTION_PIXELS
    print(
        f"self-test: turning ring → {turned[0]} pair(s), {turned[1]} swept px (want "
        f"≥{MOTION_PIXELS}); same ring shifted 1 pt → {nudged[1]} swept px "
        f"(want <{MOTION_PIXELS}): {'PASS' if ok else 'FAIL'}"
    )
    sys.exit(0 if ok else 1)


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
    ap.add_argument(
        "--movie",
        help="re-analyse a recording this script already made; no relaunch, no capture",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run the motion gate on two drawn frames and exit (no display needed)",
    )
    args = ap.parse_args()
    if args.self_test:
        self_test()

    out_dir = Path(args.out_dir)
    if not args.movie:
        session_guard()
    out_dir.mkdir(parents=True, exist_ok=True)
    if not args.movie:
        for stale in glob.glob(str(out_dir / "*.png")) + glob.glob(str(out_dir / "*.mov")):
            os.remove(stale)

    if args.panel_size:
        w, h = [float(v) for v in args.panel_size.lower().split("x")]
        want = (w, h)
    else:
        want = panel_size_from_config()
    print(f"verify-cold-start: looking for the panel at {want[0]:.0f}×{want[1]:.0f} points")

    if not (args.no_quit or args.movie):
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
    preroll = 0.8  # frames must be flowing before anything moves
    if args.movie:
        if not Path(args.movie).exists():
            sys.exit(f"verify-cold-start: no recording at {args.movie}")
        mov = args.movie
    else:
        proc = start_movie(args.region, mov)
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
    if not args.movie:
        # Only a capture that is still running can lose frames to a sleeping display;
        # a recording already on disk is a fixed object and gets judged as one.
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
    note_frames = []
    rect = None  # the screen-fixed note rectangle, taken from the first note frame
    # A re-analysis must not overwrite the frames that were the evidence for the run
    # it re-reads, so its pictures land beside them under a different name.
    shots = out_dir if not args.movie else out_dir / "recheck"
    shots.mkdir(parents=True, exist_ok=True)
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
            if state == "note":
                if rect is None:
                    rect = note_rect(bbox)
                note_frames.append(
                    (t, frame[rect[0] : rect[1], rect[2] : rect[3]], (x, y))
                )
            anchors.append((t, x, y, w, h, ink))
        first.setdefault(state, (index, t, ink, acc, bbox))
        if state != prev_state:
            name = f"{index:04d}-{t:+.2f}s-{state}.png"
            cv2.imwrite(str(shots / name), frame)
            print(
                f"  frame {index:4d} t={t:+6.2f}s  {state:7s}  ink={ink:5.1f}%  accent={acc:6d}  "
                f"bbox={bbox if bbox else '— panel not on screen'}  [saved {name}]"
            )
            prev_state = state
    cap.release()

    print(f"\nverify-cold-start: {index + 1} frames analysed, pictures in {shots}")
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

    # The animation gate. Ink and accent counts prove the note *painted*; they said
    # nothing about build 113, whose recorded frames carry the label and a dead
    # spinner — which is precisely the defect the user reported by eye and no number
    # on this page could see. So the note is now also measured for motion, and a
    # frozen note fails the run.
    pairs, swept = note_motion(note_frames) if len(note_frames) > 1 else (0, 0)
    span = (
        f"{note_frames[0][0]:+.2f}s…{note_frames[-1][0]:+.2f}s"
        if note_frames
        else "—"
    )
    print(f"  boot note: {len(note_frames)} frame(s) over {span}, {pairs} pair(s) "
          f"≥{MOTION_PAIR_S}s apart")
    frozen = bool(note_frames) and pairs > 0 and swept < MOTION_PIXELS
    if not note_frames:
        print("  note motion: not measured — the native note never appeared as a frame "
              "state (the page may have painted inside one frame interval)")
    elif pairs == 0:
        print(f"  note motion: inconclusive — the note was on screen for under "
              f"{MOTION_PAIR_S}s, too short to tell a sweep from a still")
    elif frozen:
        print(f"  note motion: FROZEN — the largest sweep between any two note frames is "
              f"{swept} moved teal pixel(s) in the note band, under the {MOTION_PIXELS} a "
              "turning ring gives. The label painted and nothing spun.")
    else:
        print(f"  note motion: ANIMATING — {swept} teal pixel(s) changed place between "
              f"note frames {MOTION_PAIR_S}s apart; that is the ring sweeping, not noise")
    if frozen:
        sys.exit(1)


if __name__ == "__main__":
    main()
