import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FIXTURE, makeFixtureReport } from "../fixture";
import { serverMock } from "./serverFixture";
import { t } from "./i18n";
import type { Bridge, PanelSettings, QuotaOrder, PeriodKey, Report, ThemeKey, TrayMode, TrayState, UpdateStatus, DownloadProgress, MachineScope, ServerView, ServerProbeReq, ServerProbeOutcome, ServerInstallReq, ServerInstallOutcome, ServerInstallProgress, ServerUpdateReq, ServerRemoveOutcome } from "../types";

/** True inside the Tauri webview; in a plain browser the fixture drives everything. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** The dismiss control is a Windows tray-panel affordance; macOS uses the
 * native non-activating panel behavior and should not show a duplicate close. */
export const isWindows = inTauri && /Windows/i.test(navigator.userAgent);

/** `system` hands the media query back to the OS; a pin sets `data-theme`. */
export function applyTheme(theme: ThemeKey): void {
  // QA/screenshot override, same channel as ?lang= / ?page=: the URL pins the
  // theme so a browser render can be frozen to light or dark.
  const pin = new URLSearchParams(location.search).get("theme");
  const effective: ThemeKey = pin === "light" || pin === "dark" ? pin : theme;
  if (effective === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = effective;
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

/** Subscribe with the same cancel-safe unlisten dance `onReport` spells out. */
function once<T>(event: string, handler: (payload: T) => void): () => void {
  let unlisten: (() => void) | null = null;
  let cancelled = false;
  void listen<T>(event, (e) => handler(e.payload)).then((fn) => {
    if (cancelled) fn();
    else unlisten = fn;
  });
  return () => {
    cancelled = true;
    unlisten?.();
  };
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

  async refreshPricing(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("refresh_pricing");
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

  onTrayPeriod(handler: (period: PeriodKey) => void): () => void {
    if (!inTauri) return () => {};
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    void listen<PeriodKey>("tray-period", (event) => handler(event.payload)).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  },

  /** The browser preview has no engine to re-fold the numbers, so this is a
   *  no-op there; the panel only reacts to the report's own scope echo. */
  async setReportScope(scope: MachineScope): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("set_report_scope", { scope });
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

  async setUiLang(lang: string): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("set_ui_lang", { lang });
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

  async setHostExitPause(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.host_exit_pause = on;
      return;
    }
    await invoke<void>("set_host_exit_pause", { on });
  },

  async setAutoUpdateCheck(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.auto_update_check = on;
      return;
    }
    await invoke<void>("set_auto_update_check", { on });
  },

  async checkUpdate(): Promise<UpdateStatus> {
    if (!inTauri) return { phase: "uptodate", message: t("update.dev.none"), version: null };
    return invoke<UpdateStatus>("check_update");
  },

  async downloadUpdate(): Promise<UpdateStatus> {
    if (!inTauri) return { phase: "unsupported", message: t("update.dev.none"), version: null };
    return invoke<UpdateStatus>("download_update");
  },

  async installUpdate(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("install_update");
  },

  onUpdateProgress(handler: (progress: DownloadProgress) => void): () => void {
    if (!inTauri) return () => {};
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    void listen<DownloadProgress>("update-download-progress", (event) => handler(event.payload)).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  },

  async setShowEmptyTools(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.show_empty_tools = on;
      return;
    }
    await invoke<void>("set_show_empty_tools", { on });
  },

  async setBubbleEnabled(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.bubble_enabled = on;
      return;
    }
    await invoke<void>("set_bubble_enabled", { on });
  },

  async showPanel(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("show_panel");
  },

  async setKeyboardMode(on: boolean): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("panel_keyboard", { on });
  },

  async beginBubbleDrag(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("begin_bubble_drag");
  },

  async quit(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("quit_app");
  },

  async openExternal(url: string): Promise<void> {
    if (!inTauri) {
      window.open(url, "_blank", "noopener");
      return;
    }
    await invoke<void>("open_external", { url });
  },

  async openLogDir(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("open_log_dir");
  },

  /* 远程服务器（拉取式采集）。浏览器预览由 `serverFixture.ts` 的内存假件驱动，
     与真实命令同形，QA 截图走的也是同一条组件路径。 */
  async servers(): Promise<ServerView[]> {
    if (!inTauri) return serverMock.servers();
    return invoke<ServerView[]>("get_servers");
  },

  async serverPublicKey(): Promise<{ path: string; line: string }> {
    if (!inTauri) return serverMock.publicKey();
    return invoke<{ path: string; line: string }>("server_public_key");
  },

  async serverProbe(req: ServerProbeReq): Promise<ServerProbeOutcome> {
    if (!inTauri) return serverMock.probe(req);
    return invoke<ServerProbeOutcome>("server_probe", { req });
  },

  async serverInstall(req: ServerInstallReq): Promise<ServerInstallOutcome> {
    if (!inTauri) return serverMock.install(req);
    return invoke<ServerInstallOutcome>("server_install", { req });
  },

  async serverAbortSession(session: number): Promise<void> {
    if (!inTauri) return serverMock.abort(session);
    await invoke<void>("server_abort_session", { session });
  },

  async serverSyncNow(id: number): Promise<void> {
    if (!inTauri) return serverMock.syncNow(id);
    await invoke<void>("server_sync_now", { id });
  },

  async serverUpdate(req: ServerUpdateReq): Promise<ServerView[]> {
    if (!inTauri) return serverMock.update(req);
    return invoke<ServerView[]>("server_update", { req });
  },

  async serverRemove(id: number, cleanupRemote: boolean): Promise<ServerRemoveOutcome> {
    if (!inTauri) return serverMock.remove(id, cleanupRemote);
    return invoke<ServerRemoveOutcome>("server_remove", { id, cleanupRemote });
  },

  onServersUpdated(handler: (servers: ServerView[]) => void): () => void {
    if (!inTauri) return serverMock.onServersUpdated(handler);
    return once("servers-updated", handler);
  },

  onInstallProgress(handler: (progress: ServerInstallProgress) => void): () => void {
    if (!inTauri) return serverMock.onInstallProgress(handler);
    return once("server-install-progress", handler);
  },
};

/** The browser preview has nothing to persist; the sheet still works in memory. */
const browserSettings: PanelSettings = {
  autostart: false,
  refresh_secs: 30,
  theme: "system",
  show_money: false,
  show_empty_tools: false,
  bubble_enabled: true,
  host_exit_pause: true,
  auto_update_check: true,
  version: "dev",
};
