import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { availableMonitors, currentMonitor, cursorPosition, getCurrentWindow } from "@tauri-apps/api/window";
import type { Monitor } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import type { BubbleSkin, Report } from "../types";
import { bridge, inTauri } from "../lib/bridge";
import { ballTokens } from "../lib/format";
import { t } from "../lib/i18n";
import { getPetSkinConfig, isPetSkinName } from "../lib/petSkinRegistry";
import { PetSkin } from "./PetSkin";

type Dock = "left" | "right" | "top";

type BubbleGeometry = {
  windowLogical: number;
  contentLogical: number;
  marginLogical: number;
  peekLogical: number;
};

type DockAnchor = {
  monitor: Monitor;
  // Physical distance along the edge, relative to this monitor's origin.
  center: number;
};

// The native window needs a size before the webview has rendered a skin. This
// is only that bootstrap size; the running window is negotiated from the
// rendered `.pet-skin` box below. The 34px ring is the shadow/transparent
// headroom that the waterdrop used to reserve around its 76px core.
const BOOTSTRAP_WINDOW = 144;
const DEFAULT_CONTENT = 76;
const WINDOW_MARGIN = 34;
const DEFAULT_PEEK = 44;
const EDGE_THRESHOLD = 48;

const DEFAULT_GEOMETRY: BubbleGeometry = {
  windowLogical: BOOTSTRAP_WINDOW,
  contentLogical: DEFAULT_CONTENT,
  marginLogical: WINDOW_MARGIN,
  peekLogical: DEFAULT_PEEK,
};

function dockPosition(edge: Dock, anchor: DockAnchor, layout: BubbleGeometry, size: PhysicalSize) {
  const { position: p, size: s, scaleFactor: scale } = anchor.monitor;
  const margin = Math.round(layout.marginLogical * scale);
  const peek = Math.round(layout.peekLogical * scale);
  const gap = Math.round(8 * scale);
  const along = (origin: number, length: number, extent: number) => {
    const low = origin + gap;
    const high = origin + length - extent - gap;
    return high < low ? origin + (length - extent) / 2
      : Math.min(Math.max(origin + anchor.center - extent / 2, low), high);
  };
  return new PhysicalPosition(
    Math.round(edge === "left" ? p.x - (size.width - margin - peek)
      : edge === "right" ? p.x + s.width - margin - peek : along(p.x, s.width, size.width)),
    Math.round(edge === "top" ? p.y - (size.height - margin - peek) : along(p.y, s.height, size.height)),
  );
}

export function BubbleApp() {
  const window = useMemo(() => getCurrentWindow(), []);
  const [report, setReport] = useState<Report | null>(null);
  const [dock, setDock] = useState<Dock | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [gaze, setGaze] = useState<{ x: number; y: number } | null>(null);
  const [skin, setSkin] = useState<BubbleSkin>("waterdrop");
  const [hovered, setHovered] = useState(false);
  const [petDragging, setPetDragging] = useState(false);
  const [petAssetFailed, setPetAssetFailed] = useState(false);
  const onPetAssetError = useCallback(() => setPetAssetFailed(true), []);
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
  // The pet owns its layout size. Keep the latest measured geometry in refs so
  // native hover/drag callbacks never use a stale render value.
  const geometry = useRef<BubbleGeometry>(DEFAULT_GEOMETRY);
  const geometrySync = useRef<Promise<void> | null>(null);
  const geometryDirty = useRef(false);
  const dockAnchor = useRef<DockAnchor | null>(null);
  const geometryErrorReported = useRef(false);

  const reportGeometryError = (error: unknown) => {
    if (geometryErrorReported.current) return;
    geometryErrorReported.current = true;
    console.error("Bubble geometry update failed", error);
  };

  useEffect(() => {
    let alive = true;
    let changed = false;
    const unlisten = bridge.onBubbleSkin((next) => {
      changed = true;
      if (alive) setSkin(next === "waterdrop" || isPetSkinName(next) ? next : "waterdrop");
    });
    void bridge.panelSettings().then((settings) => {
      if (alive && !changed) {
        const next = settings.bubble_skin;
        setSkin(next === "waterdrop" || isPetSkinName(next) ? next : "waterdrop");
      }
    }).catch(() => {});
    return () => { alive = false; unlisten(); };
  }, []);

  useEffect(() => setPetAssetFailed(false), [skin]);

  useEffect(() => {
    let alive = true;
    void bridge.fetchReport(false).then((value) => alive && setReport(value)).catch(() => {});
    const unlisten = bridge.onReport((value) => setReport(value));
    // A native window is born at the bootstrap size. Once the first frame is
    // ready this measures the current skin and switches to its real geometry.
    void ensureWindowGeometry().catch(() => {});
    return () => {
      alive = false;
      unlisten();
    };
  }, []);

  useLayoutEffect(() => {
    // A skin can be taller/wider than the waterdrop. Resize after React has
    // committed the new `.pet-skin`; retain its dock and along-edge center.
    // Mark an in-flight resize dirty before waiting for the next frame.
    geometryDirty.current = true;
    let alive = true;
    let frame = 0;
    const sync = () => {
      if (frame) cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        if (!alive) return;
        void ensureWindowGeometry();
      });
    };
    const pet = document.querySelector<HTMLElement>(".pet-skin");
    const observer = pet && typeof ResizeObserver !== "undefined"
      ? new ResizeObserver(sync)
      : null;
    if (observer && pet) observer.observe(pet);
    sync();
    return () => {
      alive = false;
      if (frame) cancelAnimationFrame(frame);
      observer?.disconnect();
    };
    // Geometry is deliberately refreshed only when the rendered skin/fallback
    // changes; the 150ms dock poll handles external DPI/window drift.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skin, petAssetFailed]);

  const clearLeaveTimer = () => {
    if (leaveTimer.current) {
      clearTimeout(leaveTimer.current);
      leaveTimer.current = null;
    }
  };

  const dockMonitor = async () => {
    const saved = dockAnchor.current?.monitor;
    if (!dockRef.current || !saved) return currentMonitor();
    // A mostly off-screen HWND can be assigned to a neighbouring display.
    // Resolve the recorded display again to pick up layout and DPI changes.
    const monitors = await availableMonitors();
    const samePosition = (candidate: Monitor) => candidate.position.x === saved.position.x
      && candidate.position.y === saved.position.y;
    return monitors.find((candidate) => candidate.name === saved.name && samePosition(candidate))
      ?? (saved.name ? monitors.find((candidate) => candidate.name === saved.name) : undefined)
      ?? monitors.find(samePosition)
      ?? await currentMonitor();
  };

  const monitor = async () => {
    const current = await dockMonitor();
    if (!current) return null;
    const size = await window.outerSize();
    return { current, size };
  };

  const measureGeometry = () => {
    const pet = document.querySelector<HTMLElement>(".pet-skin");
    // offsetWidth/offsetHeight intentionally ignore dock translations. They
    // describe the skin's own box, which is the source of truth for the
    // window size and remains stable while the pet peeks from an edge.
    const content = pet ? Math.max(pet.offsetWidth, pet.offsetHeight) : DEFAULT_CONTENT;
    const contentLogical = Math.max(1, Math.ceil(content));
    const next: BubbleGeometry = {
      contentLogical,
      marginLogical: WINDOW_MARGIN,
      windowLogical: contentLogical + WINDOW_MARGIN * 2,
      // Keep the visible dock sliver proportional when a larger character is
      // selected, instead of exposing only a thin strip of its face.
      peekLogical: Math.round(DEFAULT_PEEK * contentLogical / DEFAULT_CONTENT),
    };
    geometry.current = next;
    return next;
  };

  // A DPI/scale event or an external nudge can resize the HWND independently
  // of CSS. Reconcile it with the currently rendered pet. Concurrent hover,
  // drag and gaze callbacks share one promise. New requests dirty the run so
  // a skin/fallback committed during IPC is measured before it resolves.
  const ensureWindowGeometry = () => {
    geometryDirty.current = true;
    if (geometrySync.current) return geometrySync.current;
    const run = (async () => {
      do {
        geometryDirty.current = false;
        // The native drag owns position until its drop requests a new pass.
        if (dragging.current) return;
        const layout = measureGeometry();
        const edge = dockRef.current;
        const anchor = dockAnchor.current;
        const current = edge && anchor ? await dockMonitor() : null;
        const scale = current?.scaleFactor ?? await window.scaleFactor();
        const size = await window.outerSize();
        const pos = await window.outerPosition();
        if (dragging.current) return;
        if (geometryDirty.current || edge !== dockRef.current || anchor !== dockAnchor.current) {
          geometryDirty.current = true;
          continue;
        }
        const want = Math.round(layout.windowLogical * scale);
        const resized = size.width !== want || size.height !== want;
        const nextSize = resized ? new PhysicalSize(want, want) : size;
        if (resized) await window.setSize(nextSize);
        if (dragging.current) return;
        if (edge !== dockRef.current || anchor !== dockAnchor.current) {
          geometryDirty.current = true;
          continue;
        }
        let target: PhysicalPosition | null = null;
        if (edge && anchor && current) {
          anchor.monitor = current;
          target = dockPosition(edge, anchor, layout, nextSize);
        } else if (resized) {
          target = new PhysicalPosition(
            Math.round(pos.x + (size.width - want) / 2),
            Math.round(pos.y + (size.height - want) / 2),
          );
        }
        if (target && (target.x !== pos.x || target.y !== pos.y)) await window.setPosition(target);
      } while (geometryDirty.current);
    })().catch(reportGeometryError);
    geometrySync.current = run;
    void run.then(() => { if (geometrySync.current === run) geometrySync.current = null; });
    return run;
  };

  const expand = async () => {
    clearLeaveTimer();
    if (dragging.current) return;
    await ensureWindowGeometry();
    if (dragging.current) return;
    const layout = geometry.current;
    setExpanded(true);
    // One pull per hover: claim the dock synchronously so a second trigger
    // (mouseenter + the Rust hover event racing) reads `null` and bails
    // instead of pulling the window again from where the first pull left it.
    const currentDock = dockRef.current;
    if (!currentDock) return;
    const anchor = dockAnchor.current;
    dockRef.current = null;
    dockAnchor.current = null;
    // Use the reconciled monitor edge, rather than inferring it from a HWND
    // whose size or position may have changed during a DPI transition.
    const scale = anchor?.monitor.scaleFactor ?? await window.scaleFactor();
    const pos = await window.outerPosition();
    const cursor = await cursorPosition().catch(() => null);
    const gap = 8 * scale;
    let x = pos.x;
    let y = pos.y;
    if (currentDock === "right") {
        const monitorEdge = anchor ? anchor.monitor.position.x + anchor.monitor.size.width
          : pos.x + (layout.marginLogical + layout.peekLogical) * scale;
        let edge = monitorEdge - gap;
        // A cursor parked at the very screen edge ends up outside the fully
        // pulled window (its edge stops `gap` short of the monitor edge) —
        // hover-leave then re-docks and hover-enter re-expands, flashing the
        // pet in a loop. When the cursor sits in that band, stop the pull just
        // past it so the cursor stays inside and the hover state holds.
        if (cursor && cursor.x > edge - 4 * scale) {
            edge = Math.min(monitorEdge, cursor.x + 4 * scale);
        }
        x = edge - layout.windowLogical * scale;
    } else if (currentDock === "left") {
        const monitorEdge = anchor ? anchor.monitor.position.x
          : pos.x + (layout.windowLogical - layout.marginLogical - layout.peekLogical) * scale;
        let edge = monitorEdge + gap;
        if (cursor && cursor.x < edge + 4 * scale) {
            edge = Math.max(monitorEdge, cursor.x - 4 * scale);
        }
        x = edge;
    } else {
        const monitorEdge = anchor ? anchor.monitor.position.y
          : pos.y + (layout.windowLogical - layout.marginLogical - layout.peekLogical) * scale;
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
    if (dragging.current) return;
    if (afterDrag) {
      dockRef.current = null;
      dockAnchor.current = null;
    }
    await ensureWindowGeometry();
    if (dragging.current) return;
    const data = await monitor();
    if (!data) return;
    const { current, size } = data;
    const p = current.position;
    const s = current.size;
    // The monitor math is physical pixels; the constants above are CSS pixels.
    const threshold = Math.round(EDGE_THRESHOLD * current.scaleFactor);
    const pos = await window.outerPosition();
    if (dragging.current) return;
    const right = pos.x + size.width;
    let next = dockRef.current;
    if (!next) {
      if (pos.x <= p.x + threshold) next = "left";
      else if (right >= p.x + s.width - threshold) next = "right";
      else if (pos.y <= p.y + threshold) next = "top";
    }
    if (next && !dockAnchor.current) {
      dockAnchor.current = {
        monitor: current,
        center: next === "top" ? pos.x + size.width / 2 - p.x : pos.y + size.height / 2 - p.y,
      };
    }
    dockRef.current = next;
    // The same correction owns sizing and placement, including a target
    // monitor whose DPI has not yet reached the window's cached scale factor.
    if (next) await ensureWindowGeometry();
    if (dragging.current) return;
    setDock(next);
    setExpanded(!next);
    // A drop parked the pet back under the cursor: hover-enter would
    // immediately pop it out again — one jump right after another. Hold it
    // docked until the cursor genuinely leaves the bubble once. (A plain
    // mouse-leave dock runs with the cursor elsewhere; nothing to suppress.)
    if (afterDrag && next !== null) {
      const cursor = await cursorPosition().catch(() => null);
      const position = await window.outerPosition();
      const actualSize = await window.outerSize();
      const cursorInside =
        cursor !== null &&
        cursor.x >= position.x && cursor.x < position.x + actualSize.width &&
        cursor.y >= position.y && cursor.y < position.y + actualSize.height;
      suppressExpand.current = cursorInside;
    }
  };

  const scheduleDock = () => {
    clearLeaveTimer();
    leaveTimer.current = setTimeout(() => {
      if (!dragging.current) void dockToEdge(false).catch(reportGeometryError);
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
      setPetDragging(true);
      clearLeaveTimer();
      press.current = null;
    });
    void listen("bubble-drag-ended", () => {
      dragging.current = false;
      setPetDragging(false);
      press.current = null;
      void dockToEdge(true).catch(reportGeometryError);
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
      setHovered(event.payload);
      if (event.payload) {
        if (!suppressExpand.current) void expand().catch(reportGeometryError);
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
    if ((!dock && skin === "waterdrop") || !inTauri) {
      setGaze(null);
      return;
    }
    let alive = true;
    void (async () => {
      while (alive) {
        const pos = await window.outerPosition().catch(() => null);
        const size = await window.outerSize().catch(() => null);
        const scale = await window.scaleFactor().catch(() => 1);
        if (!pos || !size) break;
        const eyes = Array.from(document.querySelectorAll(".pet-eye"))
          .map((eye) => eye.getBoundingClientRect()).filter((eye) => eye.width > 0);
        const centerX = pos.x + (eyes.length ? eyes.reduce((sum, eye) => sum + eye.left + eye.width / 2, 0) / eyes.length * scale : size.width / 2);
        const centerY = pos.y + (eyes.length ? eyes.reduce((sum, eye) => sum + eye.top + eye.height / 2, 0) / eyes.length * scale : size.height / 2);
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
    })().catch(reportGeometryError);
    return () => {
      alive = false;
    };
  }, [dock, skin]);

  const onPointerDown = async (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    clearLeaveTimer();
    moved.current = false;
    press.current = { x: event.clientX, y: event.clientY };
    dragging.current = true;
    setPetDragging(true);
    await expand();
    try {
      // Rust owns the move: the native startDragging loop refuses to drag a
      // non-activating window. Resolves at once; bubble-drag-ended reports the
      // drop (and this webview's own pointer events cover the failed case).
      await bridge.beginBubbleDrag();
    } catch {
      dragging.current = false;
      setPetDragging(false);
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
    setPetDragging(false);
    press.current = null;
    if (wasMoved) {
      setTimeout(() => void dockToEdge(true).catch(reportGeometryError), 80);
    } else {
      // Non-activating Windows utility windows can swallow the synthetic
      // click that follows startDragging(). Treat an unmoved pointer-up as the
      // click action so opening the panel is deterministic.
      void bridge.showPanel();
      setExpanded(false);
    }
  };

  const tokens = report?.day.summary.total_tokens ?? 0;
  const petSkinName = isPetSkinName(skin) ? skin : null;
  const petSkin = petSkinName ? getPetSkinConfig(petSkinName) : undefined;
  return (
    <div
      className="bubble-app"
      data-dock={dock ?? "none"}
      data-expanded={expanded}
      data-skin={skin}
      onMouseEnter={() => { setHovered(true); if (!suppressExpand.current) void expand().catch(reportGeometryError); }}
      onMouseLeave={() => { setHovered(false); scheduleDock(); }}
      onPointerDown={(event) => void onPointerDown(event)}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      role="button"
      tabIndex={0}
      aria-label={t("bubble.aria", { t: ballTokens(tokens) })}
    >
      {petSkin && petSkinName && !petAssetFailed ? (
        <PetSkin key={petSkinName} skin={petSkinName} dock={dock} hovered={hovered} dragging={petDragging}
          tokens={tokens} gaze={gaze} onAssetError={onPetAssetError} />
      ) : <>
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
      </>}
    </div>
  );
}
