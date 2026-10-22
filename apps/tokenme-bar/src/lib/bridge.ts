import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FIXTURE, makeFixtureReport } from "../fixture";
import { serverMock } from "./serverFixture";
import { t } from "./i18n";
import { DEFAULT_PET_PREFERENCES } from "./petCompanion";
import type { PetNotice, PetPreferences, PetResponse } from "./petCompanion";
import type { ActiveSpeedTest, Bridge, BubbleSkin, CheckinResult, DiagnosticsSnapshot, PanelSettings, ProbeTool, NotifyTierKey, QuotaDisplayMode, QuotaOrder, PeriodKey, Report, ThemeKey, TrayMode, TrayState, UpdateStatus, DownloadProgress, MachineScope, ServerView, ServerProbeReq, ServerProbeOutcome, ServerInstallReq, ServerInstallOutcome, ServerInstallProgress, ServerUpdateReq, ServerRemoveOutcome, NotifyState, TtftSnapshot, SyncDevicesSnapshot, SyncManagementRequest, DiagnosticsManagementRequest, DiagnosticsResponse, McpClientConfig } from "../types";

/** True inside the Tauri webview; in a plain browser the fixture drives everything. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Last answers, handed back to the QA stubs so a scripted state machine can
 *  branch on the current snapshot instead of re-deriving it. */
let syncCurrent: SyncDevicesSnapshot = { primary_id: null, primary_display_name: null, primary_os: null, role: "unconfigured", primary_auto_approve_enabled: false, service_running: false, primary_starting: false, primary_data_sharing_enabled: false, service_health: null, endpoint: null, certificate_pin: null, stream_id: null, data_epoch: null, upload_complete: null, download_complete: null, pending: 0, in_flight: 0, pending_bytes: 0, last_error: null, discovery_error: null, last_connected_at_ms: null, operation_state: "idle", last_checked_at_ms: null, invite_url: null, invite_expires_at_ms: null, pairing_pending: false, discovered_primaries: [], devices: [], pairing_requests: [] };
let diagCurrent: DiagnosticsResponse = { enabled: false, lan_control_enabled: false, lan_control_expires_at_ms: null, sessions: [], retained_operations: 0, last_operation: null };

/** The dismiss control is a Windows tray-panel affordance; macOS uses the
 * native non-activating panel behavior and should not show a duplicate close. */
export const isWindows = inTauri && /Windows/i.test(navigator.userAgent);
export const isMac = inTauri && /Macintosh|Mac OS X/i.test(navigator.userAgent);

function petListen<T>(name: string, handler: (payload: T) => void): () => void {
  if (!inTauri) return () => {};
  let cancelled = false;
  let off: (() => void) | undefined;
  void listen<T>(name, (event) => { if (!cancelled) handler(event.payload); }).then((fn) => { if (cancelled) fn(); else off = fn; }).catch(() => {});
  return () => { cancelled = true; off?.(); };
}

/** `system` hands the media query back to the OS; a pin sets `data-theme`. */
export function applyTheme(theme: ThemeKey): void {
  // QA/screenshot override, same channel as ?lang= / ?page=: the URL pins the
  // theme so a browser render can be frozen to light or dark.
  const pin = new URLSearchParams(location.search).get("theme");
  const effective: ThemeKey = pin === "light" || pin === "dark" ? pin : theme;
  if (effective === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = effective;
  // index.html's boot shell paints before this can run, and it reads the last
  // applied theme synchronously: a shell in the wrong appearance would flip
  // colour the moment the sheet mounts. `system` is stored as it is, which lets
  // the shell fall through to the media query — what 跟随系统 means anyway.
  try {
    localStorage.setItem("tokenme:theme", effective);
  } catch {
    /* private mode / disabled storage: the shell just follows the OS */
  }
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
    /** Browser-QA seams: scripted sync/diagnostics state machines for Playwright
     *  DOM runs against the preview build; never consulted inside the app. */
    __TOKENME_SYNC_STUB__?: (request: SyncManagementRequest | null, current: SyncDevicesSnapshot) => SyncDevicesSnapshot | Promise<SyncDevicesSnapshot>;
    __TOKENME_DIAG_STUB__?: (request: DiagnosticsManagementRequest, current: DiagnosticsSnapshot) => DiagnosticsResponse | Promise<DiagnosticsResponse>;
    __TOKENME_MCP_STUB__?: () => McpClientConfig | Promise<McpClientConfig>;
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

const browserBubbleSkinHandlers = new Set<(skin: BubbleSkin) => void>();

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

  /** The check-in button of a checkin-capable tool (Trae CN / Qoder): one
   *  forced claim right now. `ok` alone turns the button into the done
   *  badge; `message` is the reason (未到签到时间 / 排队中) or the success
   *  copy, and the button shows it until the next sample refresh. */
  async checkinNow(tool: string): Promise<CheckinResult> {
    if (!inTauri) return { ok: true, message: "签到成功，+30 积分" };
    return invoke<CheckinResult>("checkin_now", { tool });
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

  async syncDevices(request?: SyncManagementRequest): Promise<SyncDevicesSnapshot> {
    if (!inTauri) {
      const stub = window.__TOKENME_SYNC_STUB__;
      if (stub) { syncCurrent = await stub(request ?? null, syncCurrent); return syncCurrent; }
      return { primary_id: null, primary_display_name: null, primary_os: null, role: "unconfigured", primary_auto_approve_enabled: false, service_running: false, primary_starting: false, primary_data_sharing_enabled: false, service_health: null, endpoint: null, certificate_pin: null, stream_id: null, data_epoch: null, upload_complete: null, download_complete: null, pending: 0, in_flight: 0, pending_bytes: 0, last_error: null, discovery_error: null, last_connected_at_ms: null, invite_url: null, invite_expires_at_ms: null, pairing_pending: false, operation_state: "idle", last_checked_at_ms: null, discovered_primaries: [], devices: [], pairing_requests: [] };
    }
    syncCurrent = await invoke<SyncDevicesSnapshot>("sync_devices", { request: request ?? null });
    return syncCurrent;
  },

  async diagnosticsSession(request: DiagnosticsManagementRequest): Promise<DiagnosticsResponse> {
    if (!inTauri) {
      const stub = window.__TOKENME_DIAG_STUB__;
      if (stub) { diagCurrent = await stub(request, diagCurrent); return diagCurrent; }
      return request.action === "status"
        ? { enabled: false, lan_control_enabled: false, lan_control_expires_at_ms: null, sessions: [], retained_operations: 0, last_operation: null }
        : { enabled: false, lan_control_enabled: false, lan_control_expires_at_ms: null, sessions: [], retained_operations: 0, last_operation: null, error: { code: "preview_only", message: "Remote diagnostics is available in the TokenMe app." } };
    }
    diagCurrent = await invoke<DiagnosticsResponse>("diagnostics_session", { request });
    return diagCurrent;
  },

  async mcpClientConfig(): Promise<McpClientConfig> {
    if (!inTauri) {
      const stub = window.__TOKENME_MCP_STUB__;
      if (stub) return await stub();
      return {
        config_json: "",
        endpoint: null,
        ready: false,
        reason: "MCP HTTP 配置只会在 TokenMe 桌面应用中生成",
        expires_at_ms: null,
        control: false,
      };
    }
    return invoke<McpClientConfig>("get_mcp_client_config");
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

  /** The browser preview has no OS permission to read; `?notify=denied` (or
   *  `not_determined`) pins the state so the guide row can be reviewed, same
   *  QA-pin family as `?stale=` / `?showempty=`. */
  async notifyStatus(): Promise<NotifyState> {
    if (!inTauri) return pinnedNotifyState();
    return invoke<NotifyState>("notify_status");
  },

  async notifyEnable(): Promise<NotifyState> {
    if (!inTauri) return pinnedNotifyState();
    return invoke<NotifyState>("notify_enable");
  },

  async setUiLang(lang: string): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("set_ui_lang", { lang });
  },

  /** Cold-start telemetry, stage 1: the bundle executed and index.html's boot
   *  shell is on screen. Called from `main.tsx`. Fire-and-forget. */
  async reportBoot(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("panel_page_signal", { stage: "boot" });
  },

  /** Cold-start telemetry, stage 2, and the handover: the page has real figures,
   *  so the native boot note can go away. Called once from `App.tsx` when the
   *  first report lands — reporting at bundle-execution time retired the note
   *  seconds before anything was drawn, which the cold-start recording measured
   *  as a bare sheet from +3.2 s to +8.1 s. Fire-and-forget. */
  async reportContent(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("panel_page_signal", { stage: "content" });
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

  async setQuotaDisplayMode(mode: QuotaDisplayMode): Promise<void> {
    if (!inTauri) {
      browserSettings.quota_display_mode = mode;
      return;
    }
    await invoke<void>("set_quota_display_mode", { mode });
  },

  async setShowCycleTokens(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.show_cycle_tokens = on;
      return;
    }
    await invoke<void>("set_show_cycle_tokens", { on });
  },

  async ttftSnapshot(): Promise<TtftSnapshot> {
    if (!inTauri) {
      // QA samples so the 访问速度 page reviews with rows on it.
      const now = Date.now();
      const samples = [
        { provider: "codex" as const, granularity: "turn" as const, metric: "ttft" as const, ttft_ms: 1240, sampled_at_ms: now - 90_000, model: "gpt-5.2-codex", tps: 96.4 },
        { provider: "codex" as const, granularity: "turn" as const, metric: "ttft" as const, ttft_ms: 1120, sampled_at_ms: now - 300_000, model: "gpt-5.2-codex", tps: 88.2 },
        { provider: "claude_code" as const, granularity: "request" as const, metric: "first_content" as const, ttft_ms: 980, sampled_at_ms: now - 60_000, model: "glm-5.2" },
        { provider: "dsh" as const, granularity: "request" as const, metric: "ttft" as const, ttft_ms: 740, sampled_at_ms: now - 45_000, model: "deepseek-flash", tps: 82.5, output_tokens: 660, decode_ms: 8_000 },
      ];
      return { revision: 0, enabled: browserSettings.ttft_enabled, claude_receiver: "disabled", claude_config_status: "disabled", samples, hosts: { codex: true, claude_code: true, dsh: true } };
    }
    return invoke<TtftSnapshot>("get_ttft_snapshot");
  },

  async runActiveSpeedTest(provider: ActiveSpeedTest["provider"]): Promise<ActiveSpeedTest> {
    if (!inTauri) {
      const now = Date.now();
      return { provider, model: provider === "codex" ? "gpt-5.6-codex" : "claude-sonnet", ttftMs: 820, tps: 72.4, outputTokens: 42, durationMs: 1420, sampledAtMs: now, measurement: provider === "codex" ? "codex_stream" : "claude_otel" };
    }
    return invoke<ActiveSpeedTest>("run_active_speed_test", { provider });
  },

  async setTtftEnabled(on: boolean): Promise<void> {
    if (!inTauri) { browserSettings.ttft_enabled = on; return; }
    await invoke<void>("set_ttft_enabled", { on });
  },

  async setPerformanceMonitoring(on: boolean): Promise<void> {
    if (!inTauri) { browserSettings.performance_monitoring = on; return; }
    await invoke<void>("set_performance_monitoring", { on });
  },

  onTtftUpdated(handler) {
    return inTauri ? once<TtftSnapshot>("ttft-updated", handler) : () => {};
  },

  async setHostExitPause(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.host_exit_pause = on;
      return;
    }
    await invoke<void>("set_host_exit_pause", { on });
  },

  async setAutoCheckin(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.auto_checkin = on;
      return;
    }
    await invoke<void>("set_auto_checkin", { on });
  },

  async setQuotaPolling(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.quota_polling = on;
      return;
    }
    await invoke<void>("set_quota_polling", { on });
  },

  async setToolPolling(tool: string, on: boolean): Promise<void> {
    if (!inTauri) {
      const off = new Set(browserSettings.quota_probes_off);
      if (on) off.delete(tool);
      else off.add(tool);
      browserSettings.quota_probes_off = [...off].sort();
      return;
    }
    await invoke<void>("set_tool_polling", { tool, on });
  },

  async probeTools(): Promise<ProbeTool[]> {
    if (!inTauri) return browserProbes.map((p) => ({ ...p, polling: browserSettings.quota_polling && !browserSettings.quota_probes_off.includes(p.id) }));
    return invoke<ProbeTool[]>("probe_tools");
  },

  async setNotifyTiers(tiers: NotifyTierKey): Promise<void> {
    if (!inTauri) {
      browserSettings.notify_tiers = tiers;
      return;
    }
    await invoke<void>("set_notify_tiers", { tiers });
  },

  async setPlanExpiryNotifyDays(days: number | null): Promise<void> {
    if (!inTauri) {
      browserSettings.plan_expiry_notify_days = days;
      return;
    }
    await invoke<void>("set_plan_expiry_notify_days", { days });
  },

  async setToolMuted(tool: string, on: boolean): Promise<void> {
    if (!inTauri) {
      const muted = new Set(browserSettings.notify_muted);
      if (on) muted.add(tool);
      else muted.delete(tool);
      browserSettings.notify_muted = [...muted].sort();
      return;
    }
    await invoke<void>("set_tool_muted", { tool, on });
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

  async setPetPreferences(prefs: PetPreferences): Promise<void> {
    if (!inTauri) { browserSettings.pet_companion = prefs; return; }
    await invoke<void>("set_pet_preferences", { prefs });
  },
  onPetPreferences: (handler: (prefs: PetPreferences) => void) => petListen("pet-preferences-changed", handler),
  async petActivity() {
    return inTauri ? invoke<import("./petCompanion").PetActivity>("pet_activity")
      : { idle_ms: 0, locked:false, fullscreen:false, visible:true };
  },
  async showPetNotice(kind: import("./petCompanion").PetNoticeKind, minutes: number) {
    return inTauri ? invoke<PetNotice | null>("show_pet_notice", { kind, minutes }) : { id:Date.now(), kind, minutes };
  },
  async previewPetNotice() {
    return inTauri ? invoke<PetNotice | null>("preview_pet_notice") : { id:Date.now(), kind:"hello" as const, minutes:0 };
  },
  async getPetNotice() { return inTauri ? invoke<PetNotice | null>("get_pet_notice") : null; },
  onPetNotice: (handler: (notice: PetNotice | null) => void) => petListen("pet-notice-changed", handler),
  async petNoticeReady(id: number) { if (inTauri) await invoke<void>("pet_notice_ready", { id }); },
  async respondPetNotice(id: number, action: PetResponse["action"]) {
    if (inTauri) await invoke<void>("respond_pet_notice", { id, action });
  },
  onPetResponse: (handler: (response: PetResponse) => void) => petListen("pet-response", handler),
  async repositionPetNotice() { if (inTauri) await invoke<void>("reposition_pet_notice"); },

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

  async setHideUnusedTools(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.hide_unused_tools = on;
      return;
    }
    await invoke<void>("set_hide_unused_tools", { on });
  },

  async setHideUnusedQuota(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.hide_unused_quota = on;
      return;
    }
    await invoke<void>("set_hide_unused_quota", { on });
  },

  async setHideUnusedEstimation(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.hide_unused_estimation = on;
      return;
    }
    await invoke<void>("set_hide_unused_estimation", { on });
  },

  async setBubbleEnabled(on: boolean): Promise<void> {
    if (!inTauri) {
      browserSettings.bubble_enabled = on;
      return;
    }
    await invoke<void>("set_bubble_enabled", { on });
  },

  async setBubbleSkin(skin: BubbleSkin): Promise<void> {
    if (!inTauri) {
      browserSettings.bubble_skin = skin;
      browserBubbleSkinHandlers.forEach((handler) => handler(skin));
      return;
    }
    await invoke<void>("set_bubble_skin", { skin });
  },

  onBubbleSkin(handler: (skin: BubbleSkin) => void): () => void {
    if (!inTauri) {
      browserBubbleSkinHandlers.add(handler);
      return () => { browserBubbleSkinHandlers.delete(handler); };
    }
    return once<BubbleSkin>("bubble-skin-changed", handler);
  },

  async showPanel(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("show_panel");
  },

  async hidePanel(): Promise<void> {
    if (!inTauri) return;
    await invoke<void>("hide_panel");
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

/** The browser preview's notification pin: `?notify=denied|not_determined|unknown`. */
function pinnedNotifyState(): NotifyState {
  const pin = new URLSearchParams(location.search).get("notify");
  return pin === "denied" || pin === "not_determined" || pin === "unknown" ? pin : "granted";
}

/** The browser preview has nothing to persist; the sheet still works in memory. */
const browserSettings: PanelSettings = {
  autostart: false,
  refresh_secs: 30,
  theme: "dark",
  show_money: false,
  quota_display_mode: "used",
  show_cycle_tokens: false,
  ttft_enabled: false,
  performance_monitoring: false,
  hide_unused_tools: true,
  hide_unused_quota: false,
  hide_unused_estimation: false,
  bubble_enabled: true,
  bubble_skin: "waterdrop",
  pet_companion: { ...DEFAULT_PET_PREFERENCES },
  host_exit_pause: true,
  auto_checkin: false,
  quota_polling: true,
  quota_probes_off: [],
  notify_tiers: "both",
  notify_muted: [],
  plan_expiry_notify_days: 3,
  auto_update_check: true,
  version: "dev",
  build: "dev",
};

/** The same 20 rows the Rust registry answers, so a browser review of the
 *  settings sheet shows the real page instead of an empty one. `answered_here`
 *  mirrors the ten tools with a cached answer on this machine. */
const browserProbes: ProbeTool[] = [
  "claude", "agnes", "antigravity", "antigravity_ide", "trae_cn", "atomcode", "cola", "kimicode", "minimaxcode", "funide",
  "cline", "opencode", "codex", "copilot", "gemini", "joycode", "qoder", "qoder_cn", "workbuddy", "workbuddy_cn", "catpaw",
  "zcode", "dsh", "trae",
].map((id) => ({
  id,
  host_gated: !["kimicode", "minimaxcode", "copilot", "gemini"].includes(id),
  answered_here: ["antigravity", "cline", "codex", "copilot", "gemini", "kimicode", "minimaxcode", "opencode", "qoder", "zcode"].includes(id),
  polling: true,
}));
