import { useEffect, useMemo, useRef, useState } from "react";
import { currentMonitor, cursorPosition, getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import type { Report } from "../types";
import { bridge, inTauri } from "../lib/bridge";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";

type Dock = "left" | "right" | "top";

// Keep these two in sync with bubble.rs (WINDOW_LOGICAL) and `.bubble-core` in
// bubble.css: the window is bigger than the ball so the shadow fits, which puts
// an invisible MARGIN px ring around the visible core. Every dock offset below
// is measured from the core, never from the window edge — docking by window
// edge would hide the ball behind the monitor border.
const WINDOW = 144;
const CORE = 76;
const MARGIN = (WINDOW - CORE) / 2;
// Docked, the ball becomes a robot pet peeking over the edge, and a face needs
// more than a sliver: ~half the head stays visible, eyes forward.
const PEEK = 44;
const EDGE_THRESHOLD = 48;

export function BubbleApp() {
  const window = useMemo(() => getCurrentWindow(), []);
  const [report, setReport] = useState<Report | null>(null);
  const [dock, setDock] = useState<Dock | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [gaze, setGaze] = useState<{ x: number; y: number } | null>(null);
  const moved = useRef(false);
  const press = useRef<{ x: number; y: number } | null>(null);
  // After a drop the pet may sit right under the cursor; hover-enter stays
  // suppressed until the cursor has left the bubble once (see dockToEdge).
  const suppressExpand = useRef(false);
  const leaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Synchronous claim flag for expand(): mouseenter and the Rust hover event
  // can both fire for one hover, and the second expand must not re-pull the
  // window from its already-expanded position — that double pull is what made
  // the pop-out distance jumpy and once left the ball too far from the edge to
  // dock back. Kept in step ONLY where `dock` itself changes: a render mirror
  // would be reset by the setExpanded re-render that fires mid-hover, which is
  // exactly the race this exists to close.
  const dockRef = useRef<Dock | null>(null);
  // Set while a drag is running (whichever side of the bridge drives it) so
  // hover timers and expand can never fight the drag loop for the window.
  const dragging = useRef(false);

  useEffect(() => {
    let alive = true;
    void bridge.fetchReport(false).then((value) => alive && setReport(value)).catch(() => {});
    const unlisten = bridge.onReport((value) => setReport(value));
    // A window that came back the wrong size (DPI event, external nudge)
    // snaps back at boot — the dock math below assumes the intended geometry.
    void ensureWindowGeometry().catch(() => {});
    return () => {
      alive = false;
      unlisten();
    };
  }, []);

  const clearLeaveTimer = () => {
    if (leaveTimer.current) {
      clearTimeout(leaveTimer.current);
      leaveTimer.current = null;
    }
  };

  const monitor = async () => {
    const current = await currentMonitor();
    if (!current) return null;
    const size = await window.outerSize();
    return { current, size };
  };

  // The window is born WINDOW×scale, but a DPI/scale event or an external
  // nudge can shrink it afterwards — measured 73×73 on a 144-logical install,
  // which clipped the round pet on all four sides. Re-assert the intended
  // size whenever the geometry is consulted; a correct window costs one
  // outerSize read, a drifted one snaps back instead of staying broken.
  const ensureWindowGeometry = async () => {
    const scale = await window.scaleFactor().catch(() => 1);
    const size = await window.outerSize().catch(() => null);
    const want = Math.round(WINDOW * scale);
    if (size && (Math.abs(size.width - want) > 2 || Math.abs(size.height - want) > 2)) {
      await window.setSize(new PhysicalSize(want, want));
    }
  };

  const expand = async () => {
    clearLeaveTimer();
    if (dragging.current) return;
    await ensureWindowGeometry();
    setExpanded(true);
    // One pull per hover: claim the dock synchronously so a second trigger
    // (mouseenter + the Rust hover event racing) reads `null` and bails
    // instead of pulling the window again from where the first pull left it.
    const currentDock = dockRef.current;
    if (!currentDock) return;
    dockRef.current = null;
    // Derived from the docked geometry itself: docking tucked the window
    // (WINDOW - MARGIN - PEEK) logical px past the monitor edge, so pulling it
    // back out lands it fully on screen. No monitor enumeration needed — it
    // answered wrong for a window that is mostly off-screen anyway.
    const scale = await window.scaleFactor().catch(() => 1);
    const pos = await window.outerPosition();
    const cursor = await cursorPosition().catch(() => null);
    const gap = 8 * scale;
    let x = pos.x;
    let y = pos.y;
    if (currentDock === "right") {
        const monitorEdge = pos.x + (MARGIN + PEEK) * scale;
        let edge = monitorEdge - gap;
        // A cursor parked at the very screen edge ends up outside the fully
        // pulled window (its edge stops `gap` short of the monitor edge) —
        // hover-leave then re-docks and hover-enter re-expands, flashing the
        // pet in a loop. When the cursor sits in that band, stop the pull just
        // past it so the cursor stays inside and the hover state holds.
        if (cursor && cursor.x > edge - 4 * scale) {
            edge = Math.min(monitorEdge, cursor.x + 4 * scale);
        }
        x = edge - WINDOW * scale;
    } else if (currentDock === "left") {
        const monitorEdge = pos.x + (WINDOW - MARGIN - PEEK) * scale;
        let edge = monitorEdge + gap;
        if (cursor && cursor.x < edge + 4 * scale) {
            edge = Math.max(monitorEdge, cursor.x - 4 * scale);
        }
        x = edge;
    } else {
        const monitorEdge = pos.y + (WINDOW - MARGIN - PEEK) * scale;
        let edge = monitorEdge + gap;
        if (cursor && cursor.y < edge + 4 * scale) {
            edge = Math.max(monitorEdge, cursor.y - 4 * scale);
        }
        y = edge;
    }
    await window.setPosition(new PhysicalPosition(Math.round(x), Math.round(y)));
    setDock(null);
  };

  const dockToEdge = async (afterDrag = false) => {
    await ensureWindowGeometry();
    const data = await monitor();
    if (!data) return;
    const { current, size } = data;
    const p = current.position;
    const s = current.size;
    // The monitor math is physical pixels; the constants above are CSS pixels.
    const scale = await window.scaleFactor().catch(() => 1);
    const margin = Math.round(MARGIN * scale);
    const peek = Math.round(PEEK * scale);
    // How much of the window hides past the monitor edge when the core keeps
    // only its PEEK sliver visible: the margin ring plus the covered core.
    const tuck = margin + Math.round(CORE * scale) - peek;
    const pos = await window.outerPosition();
    const right = pos.x + size.width;
    let next: Dock | null = null;
    let x = pos.x;
    let y = pos.y;

    if (pos.x <= p.x + EDGE_THRESHOLD) {
      next = "left";
      x = p.x - tuck;
      y = Math.min(Math.max(pos.y, p.y + 8), p.y + s.height - size.height - 8);
    } else if (right >= p.x + s.width - EDGE_THRESHOLD) {
      next = "right";
      x = p.x + s.width - margin - peek;
      y = Math.min(Math.max(pos.y, p.y + 8), p.y + s.height - size.height - 8);
    } else if (pos.y <= p.y + EDGE_THRESHOLD) {
      next = "top";
      x = Math.min(Math.max(pos.x, p.x + 8), p.x + s.width - size.width - 8);
      y = p.y - tuck;
    }
    await window.setPosition(new PhysicalPosition(Math.round(x), Math.round(y)));
    dockRef.current = next;
    setDock(next);
    setExpanded(!next);
    // A drop parked the pet back under the cursor: hover-enter would
    // immediately pop it out again — one jump right after another. Hold it
    // docked until the cursor genuinely leaves the bubble once. (A plain
    // mouse-leave dock runs with the cursor elsewhere; nothing to suppress.)
    if (afterDrag && next !== null) {
      const cursor = await cursorPosition().catch(() => null);
      const cursorInside =
        cursor !== null &&
        cursor.x >= Math.round(x) &&
        cursor.x < Math.round(x) + WINDOW * scale &&
        cursor.y >= Math.round(y) &&
        cursor.y < Math.round(y) + WINDOW * scale;
      suppressExpand.current = cursorInside;
    }
  };

  const scheduleDock = () => {
    clearLeaveTimer();
    leaveTimer.current = setTimeout(() => {
      if (!dragging.current) void dockToEdge(false);
    }, 260);
  };

  useEffect(() => {
    // The native move loop consumes the pointer events while a drag is in
    // progress, so pointer-up never reaches this webview; the Windows
    // click-away monitor in panel.rs reports the drop instead. Off-Tauri (and
    // non-Windows) nothing emits the event and the pointer-up path covers it.
    if (!inTauri) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen("bubble-drag-started", () => {
      // The Rust monitor can start a drag without any pointer event reaching
      // this webview; mark it so hover timers stand down until the drop.
      dragging.current = true;
      clearLeaveTimer();
      press.current = null;
    });
    void listen("bubble-drag-ended", () => {
      dragging.current = false;
      press.current = null;
      void dockToEdge(true);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Hover state, as seen by the Rust click-away monitor. On some setups the
  // webview never learns the cursor entered this window (it is half off-screen
  // while docked), so mouseenter alone cannot be trusted to expand the pet.
  useEffect(() => {
    if (!inTauri) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen<boolean>("bubble-hover", (event) => {
      if (event.payload) {
        if (!suppressExpand.current) void expand();
      } else {
        suppressExpand.current = false;
        scheduleDock();
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
    // Re-subscribed per dock state: the handler must see the current `dock`
    // to decide between expanding and re-docking.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dock]);

  // The docked pet watches the cursor: poll it and point the pupils that way.
  // The window doesn't move while docked, so only the cursor needs polling.
  useEffect(() => {
    if (!dock || !inTauri) {
      setGaze(null);
      return;
    }
    let alive = true;
    void (async () => {
      const pos = await window.outerPosition().catch(() => null);
      const size = await window.outerSize().catch(() => null);
      if (!pos || !size) return;
      const centerX = pos.x + size.width / 2;
      const centerY = pos.y + size.height / 2;
      while (alive) {
        const cursor = await cursorPosition().catch(() => null);
        if (!cursor) break;
        const dx = cursor.x - centerX;
        const dy = cursor.y - centerY;
        const dist = Math.hypot(dx, dy);
        const reach = Math.min(dist / 500, 1) * 2.4;
        setGaze(dist > 4 ? { x: (dx / dist) * reach, y: (dy / dist) * reach } : { x: 0, y: 0 });
        // A docked pet lives at the screen edge for hours: this poll is also
        // where a drifted window gets snapped back without waiting for a
        // dock/expand round trip.
        await ensureWindowGeometry().catch(() => {});
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
    })();
    return () => {
      alive = false;
    };
  }, [dock]);

  const onPointerDown = async (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    clearLeaveTimer();
    moved.current = false;
    press.current = { x: event.clientX, y: event.clientY };
    dragging.current = true;
    await expand();
    try {
      // Rust owns the move: the native startDragging loop refuses to drag a
      // non-activating window. Resolves at once; bubble-drag-ended reports the
      // drop (and this webview's own pointer events cover the failed case).
      await bridge.beginBubbleDrag();
    } catch {
      dragging.current = false;
    }
  };

  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const start = press.current;
    if (start && (Math.abs(event.clientX - start.x) > 4 || Math.abs(event.clientY - start.y) > 4)) {
      moved.current = true;
    }
  };

  const onPointerUp = () => {
    if (!dragging.current) return;
    const wasMoved = moved.current;
    dragging.current = false;
    press.current = null;
    if (wasMoved) {
      setTimeout(() => void dockToEdge(true), 80);
    } else {
      // Non-activating Windows utility windows can swallow the synthetic
      // click that follows startDragging(). Treat an unmoved pointer-up as the
      // click action so opening the panel is deterministic.
      void bridge.showPanel();
      setExpanded(false);
    }
  };

  const tokens = report?.day.summary.total_tokens ?? 0;
  return (
    <div
      className="bubble-app"
      data-dock={dock ?? "none"}
      data-expanded={expanded}
      onMouseEnter={() => void expand()}
      onMouseLeave={scheduleDock}
      onPointerDown={(event) => void onPointerDown(event)}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      role="button"
      tabIndex={0}
      aria-label={t("bubble.aria", { t: ballTokens(tokens) })}
    >
      <span className="bubble-ripple" aria-hidden="true" />
      <span className="bubble-core">
        <span className="bubble-value">{ballTokens(tokens)}</span>
        <span className="bubble-unit">{t("bubble.today")} tokens</span>
        {/* Docked, the ball turns into a robot pet peeking over the edge; the
            pupils point at the cursor via the --gx/--gy vars set on the face. */}
        <span
          className="bubble-face"
          aria-hidden="true"
          style={gaze ? ({ "--gx": `${gaze.x}px`, "--gy": `${gaze.y}px` } as React.CSSProperties) : undefined}
        >
          <span className="bubble-eye"><span className="bubble-pupil" /></span>
          <span className="bubble-eye"><span className="bubble-pupil" /></span>
          <span className="bubble-mouth" />
        </span>
      </span>
    </div>
  );
}
