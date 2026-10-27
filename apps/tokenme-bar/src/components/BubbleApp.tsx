import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { availableMonitors, currentMonitor, cursorPosition, getCurrentWindow } from "@tauri-apps/api/window";
import { isMac } from "../lib/bridge";
import type { Monitor } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { PhysicalPosition, PhysicalSize } from "@tauri-apps/api/dpi";
import type { BubbleSkin, Report } from "../types";
import { bridge, inTauri } from "../lib/bridge";
import { ballTokens } from "../lib/format";
import { WaterdropPet } from "./WaterdropPet";
import { t } from "../lib/i18n";
import { getPetSkinConfig, isPetSkinName } from "../lib/petSkinRegistry";
import { PetSkin } from "./PetSkin";
import { usePetCompanion } from "../lib/usePetCompanion";

type Dock = "left" | "right" | "top";

type AlphaMap = { w: number; h: number; data: Uint8ClampedArray };

// Decoded alpha maps for the pet's sprite assets, keyed by asset URL. The
// selected skin warms its current pose before the native window hit-test uses
// it, so transparent pixels can pass clicks through from the first hover.
const alphaCache = new Map<string, AlphaMap>();
const alphaPending = new Map<string, Promise<AlphaMap | null>>();

function loadAlpha(src: string): Promise<AlphaMap | null> {
  const cached = alphaCache.get(src);
  if (cached) return Promise.resolve(cached);
  const pending = alphaPending.get(src);
  if (pending) return pending;
  const loaded = new Promise<AlphaMap | null>((resolve) => {
    const image = new Image();
    image.onload = () => {
      let entry: AlphaMap | null = null;
      try {
        const canvas = document.createElement("canvas");
        canvas.width = image.naturalWidth;
        canvas.height = image.naturalHeight;
        const ctx = canvas.getContext("2d", { willReadFrequently: true });
        if (ctx) {
          ctx.drawImage(image, 0, 0);
          entry = {
            w: image.naturalWidth, h: image.naturalHeight,
            data: ctx.getImageData(0, 0, image.naturalWidth, image.naturalHeight).data,
          };
          alphaCache.set(src, entry);
        }
      } catch {
        // A failed alpha read must leave the surface click-through, never make
        // the whole transparent window the pet's hit target.
      } finally {
        alphaPending.delete(src);
        resolve(entry);
      }
    };
    image.onerror = () => {
      alphaPending.delete(src);
      resolve(null);
    };
    image.src = src;
  });
  alphaPending.set(src, loaded);
  return loaded;
}

function urlOf(backgroundImage: string): string | null {
  const m = /url\(["']?([^"')]+)["']?\)/.exec(backgroundImage);
  return m ? m[1] : null;
}

function pxOf(value: string | null | undefined): number | null {
  if (!value) return null;
  const m = /(-?\d+(?:\.\d+)?)px/.exec(value);
  return m ? parseFloat(m[1]) : null;
}

function artAlphaSampler(element: HTMLElement, entry: AlphaMap) {
  const r = element.getBoundingClientRect();
  let sample: (x: number, y: number) => [number, number];
  if (element instanceof HTMLImageElement) {
    const scaleX = entry.w / r.width;
    const scaleY = entry.h / r.height;
    sample = (x, y) => [Math.floor((x - r.left) * scaleX), Math.floor((y - r.top) * scaleY)];
  } else {
    const cs = getComputedStyle(element);
    const sizeW = pxOf(cs.backgroundSize) ?? entry.w;
    const sizeH = pxOf(cs.backgroundSize?.split(" ")[1]) ?? entry.h;
    const posX = pxOf(cs.backgroundPosition) ?? 0;
    const posY = pxOf(cs.backgroundPosition?.split(" ").slice(1).join(" ")) ?? 0;
    const scaleX = entry.w / sizeW;
    const scaleY = entry.h / sizeH;
    sample = (x, y) => [Math.floor(((x - r.left) - posX) * scaleX), Math.floor(((y - r.top) - posY) * scaleY)];
  }
  return (x: number, y: number) => {
    if (x < r.left || x >= r.right || y < r.top || y >= r.bottom) return false;
    const [ix, iy] = sample(x, y);
    return ix >= 0 && iy >= 0 && ix < entry.w && iy < entry.h
      && entry.data[(iy * entry.w + ix) * 4 + 3] > 16;
  };
}

function artAlphaAt(element: HTMLElement, x: number, y: number, entry: AlphaMap) {
  return artAlphaSampler(element, entry)(x, y);
}

type BubbleGeometry = {
  windowWidthLogical: number;
  windowHeightLogical: number;
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
const BOOTSTRAP_WINDOW = 110;
const DEFAULT_CONTENT = 76;
const WATERDROP_CONTENT = 70;
const WINDOW_MARGIN = 34;
const WATERDROP_WINDOW_MARGIN = 20;
const DEFAULT_PEEK = 44;
const EDGE_THRESHOLD = 48;

const DEFAULT_GEOMETRY: BubbleGeometry = {
  windowWidthLogical: BOOTSTRAP_WINDOW,
  windowHeightLogical: BOOTSTRAP_WINDOW,
  marginLogical: WATERDROP_WINDOW_MARGIN,
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
  const companion = usePetCompanion();
  const window = useMemo(() => getCurrentWindow(), []);
  const [report, setReport] = useState<Report | null>(null);
  const [dock, setDock] = useState<Dock | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [gaze, setGaze] = useState<{ x: number; y: number } | null>(null);
  const [skin, setSkin] = useState<BubbleSkin>("waterdrop");
  const [hovered, setHovered] = useState(false);
  const hovering = useRef(false);
  hovering.current = hovered;
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
  // mac: the native drag session arms on the first real move, never on the
  // press — a press that ran startDragging raced the webview's own pointerup
  // and the click contract ran twice (panel flashes open, then closes).
  const macDragStarted = useRef(false);
  // The pet owns its layout size. Keep the latest measured geometry in refs so
  // native hover/drag callbacks never use a stale render value.
  const geometry = useRef<BubbleGeometry>(DEFAULT_GEOMETRY);
  const geometrySync = useRef<Promise<void> | null>(null);
  const geometryDirty = useRef(false);
  const dockAnchor = useRef<DockAnchor | null>(null);
  const geometryErrorReported = useRef(false);
  const cursorIgnored = useRef<boolean | null>(null);
  const cursorIgnoreSupported = useRef<boolean | null>(null);

  const setCursorIgnored = useCallback(async (ignore: boolean) => {
    if (cursorIgnoreSupported.current === false) return;
    if (cursorIgnored.current === ignore) return;
    cursorIgnored.current = ignore;
    try {
      await window.setIgnoreCursorEvents(ignore);
      cursorIgnoreSupported.current = true;
    } catch {
      cursorIgnoreSupported.current = false;
      cursorIgnored.current = null;
    }
  }, [window]);

  // Do not leave a hidden/unmounted bubble intercepting input when the native
  // window is recreated or the user switches to a different skin.
  useEffect(() => () => {
    cursorIgnored.current = false;
    void window.setIgnoreCursorEvents(false).catch(() => {});
  }, [window]);

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
  }, [skin, dock, petAssetFailed]);

  const clearLeaveTimer = () => {
    if (leaveTimer.current) {
      clearTimeout(leaveTimer.current);
      leaveTimer.current = null;
    }
  };

  const dockMonitor = async () => {
    const saved = dockAnchor.current?.monitor;
    if (!dockRef.current || !saved) {
      // macOS: currentMonitor() answers null for a window hanging past the
      // screen edge — exactly where a pet docks. Resolve the display that
      // holds the window's centre, then the first known one.
      const current = await currentMonitor();
      if (current) return current;
      const [pos, size, monitors] = await Promise.all([
        window.outerPosition(), window.outerSize(), availableMonitors(),
      ]);
      const cx = pos.x + size.width / 2;
      const cy = pos.y + size.height / 2;
      return (
        monitors.find((m) => cx >= m.position.x && cx < m.position.x + m.size.width
          && cy >= m.position.y && cy < m.position.y + m.size.height)
        ?? monitors[0]
      );
    }
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
    // window size and remains stable while the pet peeks from an edge. The
    // quota badge hangs BELOW that box, so the window's height must cover it
    // too — a summary line clipped by the window's bottom edge was exactly
    // what the first hover showed.
    const badge = document.querySelector<HTMLElement>(".pet-token-badge");
    const badgeDrop = badge ? badge.offsetTop + badge.offsetHeight - (pet?.offsetHeight ?? 0) : 0;
    const waterdrop = !isPetSkinName(skin) || petAssetFailed;
    const baseContent = waterdrop ? WATERDROP_CONTENT : DEFAULT_CONTENT;
    const baseMargin = waterdrop ? WATERDROP_WINDOW_MARGIN : WINDOW_MARGIN;
    const content = pet
      ? Math.max(pet.offsetWidth, pet.offsetHeight + Math.max(0, badgeDrop))
      : baseContent;
    const skinConfig = skin === "waterdrop" ? undefined : getPetSkinConfig(skin);
    const artworkCrop = pet && !dockRef.current ? skinConfig?.layout.crop : undefined;
    const cropToArtwork = Boolean(artworkCrop);
    let windowWidthLogical = Math.ceil(content + baseMargin * 2);
    let windowHeightLogical = windowWidthLogical;
    let marginLogical = baseMargin;
    if (cropToArtwork && pet && artworkCrop && skinConfig) {
      const centerX = artworkCrop.left + artworkCrop.width / 2;
      const centerY = artworkCrop.top + artworkCrop.height / 2;
      let left = artworkCrop.left;
      let top = artworkCrop.top;
      let right = artworkCrop.left + artworkCrop.width;
      let bottom = artworkCrop.top + artworkCrop.height;
      if (badge) {
        const style = getComputedStyle(badge);
        if (style.display !== "none" && style.visibility !== "hidden" && Number(style.opacity) > 0.02) {
          // Once the sprite viewport is cropped, the badge's offsets are
          // relative to that smaller box. Convert them back into the original
          // sprite layout before combining its bounds with the artwork.
          const badgeLeft = badge.offsetLeft + (pet.dataset.cropped === "true" ? artworkCrop.left : 0);
          const badgeTop = badge.offsetTop + (pet.dataset.cropped === "true" ? artworkCrop.top : 0);
          left = Math.min(left, badgeLeft);
          top = Math.min(top, badgeTop);
          right = Math.max(right, badgeLeft + badge.offsetWidth);
          bottom = Math.max(bottom, badgeTop + badge.offsetHeight);
        }
      }
      const padding = skinConfig.layout.windowPadding;
      windowWidthLogical = Math.ceil(2 * Math.max(
        centerX - (left - padding.left), right + padding.right - centerX,
      ));
      windowHeightLogical = Math.ceil(2 * Math.max(
        centerY - (top - padding.top), bottom + padding.bottom - centerY,
      ));
      marginLogical = Math.min(padding.left, padding.top, padding.right, padding.bottom);
    }
    const next: BubbleGeometry = {
      windowWidthLogical,
      windowHeightLogical,
      marginLogical,
      // Keep the visible dock sliver proportional when a larger character is
      // selected, instead of exposing only a thin strip of its face.
      peekLogical: Math.round(DEFAULT_PEEK * Math.max(windowWidthLogical, windowHeightLogical) / baseContent),
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
        const wantWidth = Math.round(layout.windowWidthLogical * scale);
        const wantHeight = Math.round(layout.windowHeightLogical * scale);
        const resized = size.width !== wantWidth || size.height !== wantHeight;
        const nextSize = resized ? new PhysicalSize(wantWidth, wantHeight) : size;
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
            Math.round(pos.x + (size.width - wantWidth) / 2),
            Math.round(pos.y + (size.height - wantHeight) / 2),
          );
        }
        if (target && (target.x !== pos.x || target.y !== pos.y)) await window.setPosition(target);
      } while (geometryDirty.current);
    })().catch(reportGeometryError);
    geometrySync.current = run;
    void run.then(() => { if (geometrySync.current === run) geometrySync.current = null; });
    if (companion.noticeVisible.current) void run.then(() => bridge.repositionPetNotice()).catch(() => {});
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
        x = edge - layout.windowWidthLogical * scale;
    } else if (currentDock === "left") {
        const monitorEdge = anchor ? anchor.monitor.position.x
          : pos.x + (layout.windowWidthLogical - layout.marginLogical - layout.peekLogical) * scale;
        let edge = monitorEdge + gap;
        if (cursor && cursor.x < edge + 4 * scale) {
            edge = Math.max(monitorEdge, cursor.x - 4 * scale);
        }
        x = edge;
    } else {
        const monitorEdge = anchor ? anchor.monitor.position.y
          : pos.y + (layout.windowHeightLogical - layout.marginLogical - layout.peekLogical) * scale;
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
    if (isMac || dragging.current) return;
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
    if (isMac) return; // dragging into an edge trips the system's stage gesture
    clearLeaveTimer();
    if (companion.noticeVisible.current) return;
    leaveTimer.current = setTimeout(() => {
      if (!dragging.current) void dockToEdge(false).catch(reportGeometryError);
    }, 260);
  };

  useEffect(() => bridge.onPetNotice((notice) => {
    if (notice && !dragging.current) void expand().catch(reportGeometryError);
    else if (!notice && !dragging.current && !hovering.current) scheduleDock();
    // Native position is reconciled after expansion and again by the gaze poll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }), []);

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
      companion.play("settle");
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
      void (async () => {
        let isHovering = event.payload;
        if (isHovering && skin !== "waterdrop" && !petAssetFailed) {
          const [cursor, pos, scale] = await Promise.all([
            cursorPosition().catch(() => null),
            window.outerPosition().catch(() => null),
            window.scaleFactor().catch(() => 1),
          ]);
          isHovering = Boolean(cursor && pos && withinArt((cursor.x - pos.x) / scale, (cursor.y - pos.y) / scale));
        }
        hovering.current = isHovering;
        setHovered(isHovering);
        void setCursorIgnored(!isHovering);
        if (isHovering) {
          if (!suppressExpand.current) void expand().catch(reportGeometryError);
        } else {
          suppressExpand.current = false;
          scheduleDock();
        }
      })();
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

  // Route native input against the visible pet pixels, not the square webview
  // that holds transparent headroom around a tall skin. Tauri's ignore setting
  // passes clicks through to the application underneath that headroom.
  useEffect(() => {
    if (!inTauri) {
      setGaze(null);
      return;
    }
    const petAware = skin !== "waterdrop" && !petAssetFailed;
    if ((!dock && !petAware)) {
      setGaze(null);
      void setCursorIgnored(false);
      return;
    }
    let alive = true;
    void (async () => {
      if (petAware) await setCursorIgnored(true);
      let lastGeometryCheck = 0;
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
        // The mac pet has no DOM mouseenter to lean on (a non-activating
        // window sees no mouse-moved events): this poll IS the hover — the
        // cursor over its visible pixels means greet mode, outside means idle.
        const inWindow = cursor.x >= pos.x && cursor.x < pos.x + size.width
          && cursor.y >= pos.y && cursor.y < pos.y + size.height;
        const overPet = petAware
          ? withinArt((cursor.x - pos.x) / scale, (cursor.y - pos.y) / scale)
          : inWindow;
        const interactive = dragging.current || overPet;
        const wasInteractive = hovering.current;
        hovering.current = interactive;
        setHovered(interactive);
        await setCursorIgnored(!interactive);
        if (interactive && !wasInteractive && !suppressExpand.current) {
          void expand().catch(reportGeometryError);
        }
        const dx = cursor.x - centerX;
        const dy = cursor.y - centerY;
        const dist = Math.hypot(dx, dy);
        const reach = Math.min(dist / 500, 1) * 2.4;
        setGaze(dist > 4 ? { x: (dx / dist) * reach, y: (dy / dist) * reach } : { x: 0, y: 0 });
        // A docked pet lives at the screen edge for hours: this poll is also
        // where a drifted window gets snapped back without waiting for a
        // dock/expand round trip.
        if (Date.now() - lastGeometryCheck >= 150) {
          lastGeometryCheck = Date.now();
          await ensureWindowGeometry().catch(() => {});
        }
        await new Promise((resolve) => setTimeout(resolve, petAware ? 50 : 150));
      }
    })().catch(reportGeometryError);
    return () => {
      alive = false;
    };
  }, [dock, skin, petAssetFailed]);

  // Pixel-accurate hit testing: the sprite frames carry transparent margins
  // (the standing pose is narrow inside its own frame), so "clickable exactly
  // where the picture shows" means reading the alpha channel at the cursor.
  const withinArt = (x: number, y: number) => {
    const visible = (element: Element) => {
      const style = getComputedStyle(element);
      return style.display !== "none" && style.visibility !== "hidden" && Number(style.opacity) > 0.02;
    };
    // Visible chrome that is part of the pet counts regardless of pixels.
    let sawSurface = false;
    for (const sel of [".pet-token-badge", ".bubble-core"]) {
      const element = document.querySelector(sel);
      if (!element) continue;
      sawSurface = true;
      if (!visible(element)) continue;
      const r = element.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) continue;
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return true;
    }
    const edge = document.querySelector<HTMLImageElement>(".pet-edge-art");
    if (edge?.src) {
      sawSurface = true;
      const r = edge.getBoundingClientRect();
      if (visible(edge) && r.width > 0 && r.height > 0
        && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) {
        const src = edge.currentSrc || edge.src;
        const entry = alphaCache.get(src);
        if (!entry) { void loadAlpha(src); return !inTauri; }
        return artAlphaAt(edge, x, y, entry);
      }
    }
    const sprite = document.querySelector<HTMLElement>(".pet-sprite");
    if (sprite) {
      sawSurface = true;
      const r = sprite.getBoundingClientRect();
      if (visible(sprite) && r.width > 0 && r.height > 0
        && x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) {
        const cs = getComputedStyle(sprite);
        const url = urlOf(cs.backgroundImage);
        const entry = url ? alphaCache.get(url) : undefined;
        if (url && !entry) { void loadAlpha(url); return !inTauri; }
        if (url && entry) return artAlphaAt(sprite, x, y, entry);
        return !url;
      }
    }
    // A recognized pet surface exists but the point missed every visible box:
    // this is transparent window headroom, so the native window must let the
    // click reach the application underneath. Unknown skin shapes retain the
    // previous fallback behavior.
    return !sawSurface;
  };

  // Per-skin crop geometry is declared in the manifest. Warm the current
  // artwork alpha only for pixel-level hit testing, independent of its decode
  // timing so it cannot hold the native resize at the bootstrap size.
  useEffect(() => {
    const frame = requestAnimationFrame(() => {
      const edge = document.querySelector<HTMLImageElement>(".pet-edge-art");
      const sprite = document.querySelector<HTMLElement>(".pet-sprite");
      const url = sprite ? urlOf(getComputedStyle(sprite).backgroundImage) : null;
      const sources = [edge?.src ? edge.currentSrc || edge.src : null, url].filter((value): value is string => Boolean(value));
      void Promise.all(sources.map(loadAlpha));
    });
    return () => cancelAnimationFrame(frame);
  }, [skin, dock, petAssetFailed, companion.motion, companion.emotion, hovered]);

  const onPointerDown = async (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    if (!withinArt(event.clientX, event.clientY)) {
      // A press on the transparent margin: arm nothing, so the release's
      // click contract finds no press and stays silent.
      moved.current = false;
      press.current = null;
      return;
    }
    clearLeaveTimer();
    moved.current = false;
    press.current = { x: event.clientX, y: event.clientY };
    if (isMac) {
      // mac: the press arms nothing. The click contract (open the panel)
      // runs ONCE, from pointerup; the drag session arms on the first real
      // move in onPointerMove. Running the contract from both the press
      // continuation and the release is what toggled twice per click and
      // flashed the panel (2026-10-11). The pet keeps its skin — no expand
      // on the press, whose collapse read as a state flicker.
      dragging.current = false;
      macDragStarted.current = false;
      return;
    }
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
    if (!start) return;
    if (Math.abs(event.clientX - start.x) > 6 || Math.abs(event.clientY - start.y) > 6) {
      moved.current = true;
      if (isMac && !macDragStarted.current) {
        // The move is real: hand the pointer to the native drag session. The
        // webview sees no further pointer events until release, and mac has
        // no edge dock to return to (scheduleDock bows out on mac).
        macDragStarted.current = true;
        dragging.current = true;
        setPetDragging(true);
        void getCurrentWindow().startDragging().catch(() => {
          dragging.current = false;
          setPetDragging(false);
        });
      }
    }
  };

  const onPointerUp = () => {
    // A release with no armed press belongs to a margin press the down gate
    // already refused: stay silent instead of toggling the panel.
    if (!press.current) return;
    if (isMac) {
      // The single click contract on mac: a press that never became a drag
      // toggles the panel — once, here, and nowhere else.
      const dragged = macDragStarted.current;
      macDragStarted.current = false;
      dragging.current = false;
      setPetDragging(false);
      press.current = null;
      if (!dragged) {
        void bridge.showPanel();
        setExpanded(false);
      }
      return;
    }
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
      companion.play("cheer");
      setExpanded(false);
    }
  };

  const tokens = report?.day.summary.total_tokens ?? 0;
  const petSkinName = isPetSkinName(skin) ? skin : null;
  const petSkin = petSkinName ? getPetSkinConfig(petSkinName) : undefined;
  const activeArtworkCrop = petSkin && dock === null && !petAssetFailed ? petSkin.layout.crop : null;
  return (
    <div
      className="bubble-app"
      data-platform={isMac ? "mac" : "windows"}
      data-dock={dock ?? "none"}
      data-expanded={expanded}
      data-skin={skin}
      data-action={companion.motion}
      data-emotion={companion.emotion === "idle" ? companion.motion : companion.emotion}
      data-animations={companion.motions}
      onMouseEnter={(event) => {
        const hit = skin === "waterdrop" || petAssetFailed || withinArt(event.clientX, event.clientY);
        hovering.current = hit;
        setHovered(hit);
        if (hit && !suppressExpand.current) void expand().catch(reportGeometryError);
      }}
      onMouseLeave={() => { hovering.current = false; setHovered(false); scheduleDock(); }}
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
          tokens={tokens} gaze={gaze} onAssetError={onPetAssetError} action={companion.motion} emotion={companion.emotion} motions={companion.motions}
          crop={activeArtworkCrop ?? undefined} centerInWindow />
      ) : <WaterdropPet tokens={tokens} gaze={gaze} />}
    </div>
  );
}
