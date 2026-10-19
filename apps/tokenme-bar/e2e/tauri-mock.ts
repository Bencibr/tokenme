/**
 * A fake Tauri IPC backend for the browser E2E run.
 *
 * The bubble is a Windows-only native window, so the real thing cannot be
 * driven on a Linux CI host. What CAN be reproduced faithfully is the exact
 * condition the drag bug lived in: WebView2 delivers pointer events whose
 * *client* coordinates stay pinned while the OS window travels with the
 * cursor, so `event.clientX` never changes during a drag. This mock supplies
 * the window/monitor/event commands `BubbleApp` reaches for, and the spec
 * dispatches synthetic pointer events with a fixed client point and a moving
 * screen point — the invariant the frontend must not ignore.
 *
 * The Rust drag loop itself is modelled by the spec: `begin_bubble_drag`
 * emits `bubble-drag-started`, and the spec emits `bubble-drag-ended` with
 * the drop's verdict, exactly the payload `bubble::begin_drag` now sends.
 */

export interface MockPayload {
  report: unknown;
  settings: unknown;
}

/**
 * Installed with `page.addInitScript`, before any app module runs, so the
 * `inTauri` sniff in `lib/bridge.ts` sees a real (mock) bridge. The function
 * is serialized into the page: it must not close over anything but its arg.
 */
export function installTauriMock(payload: MockPayload) {
  const callbacks = new Map<number, (message: unknown) => void>();
  const listeners = new Map<string, number[]>();
  const calls: { cmd: string; args: unknown }[] = [];
  let nextId = 0;

  const win = { pos: { x: 0, y: 0 }, size: { width: 144, height: 144 }, scale: 1 };
  const cursor = { x: 100, y: 100 };
  const monitor = {
    name: "e2e",
    size: { width: 1920, height: 1080 },
    position: { x: 0, y: 0 },
    scaleFactor: 1,
    workArea: { position: { x: 0, y: 0 }, size: { width: 1920, height: 1040 } },
  };

  const emit = (event: string, eventPayload?: unknown) => {
    for (const id of listeners.get(event) ?? []) {
      callbacks.get(id)?.({ event, id, payload: eventPayload });
    }
  };

  const invoke = (cmd: string, args?: Record<string, unknown>) => {
    calls.push({ cmd, args: args ? (JSON.parse(JSON.stringify(args)) as unknown) : args });
    switch (cmd) {
      case "plugin:event|listen": {
        const event = String(args?.event);
        listeners.set(event, [...(listeners.get(event) ?? []), Number(args?.handler)]);
        return Promise.resolve(args?.handler);
      }
      case "plugin:event|unlisten":
        return Promise.resolve();
      case "plugin:window|outer_position":
        return Promise.resolve({ ...win.pos });
      case "plugin:window|outer_size":
        return Promise.resolve({ ...win.size });
      case "plugin:window|scale_factor":
        return Promise.resolve(win.scale);
      case "plugin:window|current_monitor":
      case "plugin:window|primary_monitor":
        return Promise.resolve(monitor);
      case "plugin:window|available_monitors":
        return Promise.resolve([monitor]);
      case "plugin:window|cursor_position":
        return Promise.resolve({ ...cursor });
      case "plugin:window|set_position": {
        const raw = args?.value as { x?: number; y?: number; Physical?: { x: number; y: number } };
        const value = raw.Physical ?? raw;
        win.pos = { x: Number(value.x), y: Number(value.y) };
        return Promise.resolve();
      }
      case "plugin:window|set_size": {
        const raw = args?.value as { width?: number; height?: number; Physical?: { width: number; height: number } };
        const size = raw.Physical ?? raw;
        win.size = { width: Number(size.width), height: Number(size.height) };
        return Promise.resolve();
      }
      case "plugin:window|start_dragging":
        return Promise.resolve();
      case "get_report":
        return Promise.resolve(payload.report);
      case "get_panel_settings":
        return Promise.resolve(payload.settings);
      case "pet_activity":
        return Promise.resolve({ idle_ms: 0, locked: false, fullscreen: false, visible: true });
      case "get_pet_notice":
        return Promise.resolve(null);
      case "panel_page_signal":
        return Promise.resolve();
      case "begin_bubble_drag":
        // bubble::begin_drag emits this the instant the loop arms, before the
        // pointer ever travels. The spec plays the release side.
        emit("bubble-drag-started");
        return Promise.resolve();
      default:
        return Promise.resolve(null);
    }
  };

  Object.assign(window, {
    __TAURI_INTERNALS__: {
      metadata: {
        currentWindow: { label: "bubble" },
        currentWebview: { label: "bubble", windowLabel: "bubble" },
      },
      transformCallback(callback: (message: unknown) => void, once = false) {
        const id = ++nextId;
        callbacks.set(id, once ? (message: unknown) => { callbacks.delete(id); callback(message); } : callback);
        return id;
      },
      convertFileSrc: (path: string) => path,
      invoke,
    },
    __TAURI_EVENT_PLUGIN_INTERNALS__: {
      unregisterListener(event: string, id: number) {
        listeners.set(event, (listeners.get(event) ?? []).filter((candidate) => candidate !== id));
        callbacks.delete(id);
      },
    },
    __tauriMock: {
      calls,
      win,
      cursor,
      monitor,
      emit,
      setWindow(x: number, y: number) {
        win.pos = { x, y };
      },
      setCursor(x: number, y: number) {
        cursor.x = x;
        cursor.y = y;
      },
    },
  });
}

/** Commands the mock logged, in call order. */
export type MockCall = { cmd: string; args: unknown };

export interface MockHandle {
  calls: MockCall[];
  win: { pos: { x: number; y: number }; size: { width: number; height: number }; scale: number };
  cursor: { x: number; y: number };
  monitor: unknown;
  emit: (event: string, payload?: unknown) => void;
  setWindow: (x: number, y: number) => void;
  setCursor: (x: number, y: number) => void;
}
