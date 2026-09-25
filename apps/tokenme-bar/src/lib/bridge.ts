import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FIXTURE, makeFixtureReport } from "../fixture";
import type { Bridge, PanelSettings, QuotaOrder, Report, ThemeKey, TrayMode, TrayState } from "../types";

/** True inside the Tauri webview; in a plain browser the fixture drives everything. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** `system` hands the media query back to the OS; a pin sets `data-theme`. */
export function applyTheme(theme: ThemeKey): void {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
}

/** Dollar figures off => the components read the flag from context; the
 *  attribute exists for anything style-level that ever needs it. */
export function applyMoney(on: boolean): void {
  if (on) delete document.documentElement.dataset.money;
  else document.documentElement.dataset.money = "off";
}

/**
 * A real `Report` as produced by `tokenme report --json`, for reviewing the panel
 * in a browser with this machine's actual numbers instead of the fixture. Set it
 * from the console before the first fetch; absent in the app, where Rust answers.
 */
declare global {
  interface Window {
    __TOKENME_REPORT__?: Report;
  }
}
let real: Report | null | undefined;
/** `pnpm dev` + http://127.0.0.1:1420/?real=1 renders this machine's numbers. */
async function injected(): Promise<Report | null> {
  if (typeof window === "undefined") return null;
  if (window.__TOKENME_REPORT__) return window.__TOKENME_REPORT__;
  if (!window.location.search.includes("real")) return null;
  if (real === undefined) {
    real = await fetch("/report.json")
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
  }
  return real ?? null;
}

/**
 * The only seam between the UI and Rust. Commands are exactly the four the
 * core exposes: `get_report`, `get_tray_state`, `set_tray_mode`, `refresh_pricing`.
 */
export const bridge: Bridge = {
  live: inTauri,

  async fetchReport(force: boolean): Promise<Report> {
    if (!inTauri) return (await injected()) ?? (force ? makeFixtureReport() : FIXTURE);
    return invoke<Report>("get_report", { force });
  },

  async trayState(): Promise<TrayState | null> {
    if (!inTauri) return null;
    return invoke<TrayState>("get_tray_state");
  },

  async setTrayMode(mode: TrayMode): Promise<TrayState | null> {
    if (!inTauri) return null;
    await invoke<void>("set_tray_mode", { mode });
    return invoke<TrayState>("get_tray_state");
  },

  onReport(handler: (report: Report) => void): () => void {
    if (!inTauri) return () => {};
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    void listen<Report>("report-updated", (event) => handler(event.payload)).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  },

  /**
   * Icons come from `/Applications`, so the browser review path reads the same
   * file the `?real=1` report comes from: `tokenme icons --json > icons.json`.
   * Without it the panel falls back to coloured monograms, which is also what a
   * machine with none of these apps installed would show.
   */
  async toolIcons(): Promise<Record<string, string>> {
    if (inTauri) return invoke<Record<string, string>>("tool_icons");
    return fetch("/icons.json")
      .then((r) => (r.ok ? r.json() : {}))
      .catch(() => ({}));
  },

  /** The drag order lives in settings.json next to the budgets; a browser
   *  review has nothing to persist, so it answers empty and accepts silently. */
  async quotaOrder(): Promise<QuotaOrder> {
    if (!inTauri) return { tools: [], rows: [] };
    return invoke<QuotaOrder>("get_quota_order");
  },

  async setQuotaOrder(order: QuotaOrder): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("set_quota_order", { order });
  },

  async panelSettings(): Promise<PanelSettings> {
    if (!inTauri) return { ...browserSettings };
    return invoke<PanelSettings>("get_panel_settings");
  },

  async setAutostart(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.autostart = on;
      return;
    }
    await invoke<void>("set_autostart", { on });
  },

  async setRefreshSecs(secs: number): Promise<void> {
    if (!inTauri) {
      browserSettings.refresh_secs = secs;
      return;
    }
    await invoke<void>("set_refresh_secs", { secs });
  },

  async setTheme(theme: ThemeKey): Promise<void> {
    applyTheme(theme);
    if (!inTauri) {
      browserSettings.theme = theme;
      return;
    }
    await invoke<void>("set_theme", { theme });
  },

  async setShowMoney(on: boolean): Promise<void> {
    applyMoney(on);
    if (!inTauri) {
      browserSettings.show_money = on;
      return;
    }
    await invoke<void>("set_show_money", { on });
  },
};

/** The browser preview has nothing to persist; the sheet still works in memory. */
const browserSettings: PanelSettings = {
  autostart: false,
  refresh_secs: 30,
  theme: "system",
  show_money: true,
  version: "dev",
};
