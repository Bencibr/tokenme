/**
 * The wire contract with `usage_core::Report`.
 *
 * Field names are verbatim serde output (snake_case, no `rename_all` on the
 * structs), so this file is the single place the UI is allowed to assume
 * anything about Rust. `fixture.ts` produces the same shape.
 */

export type Meter = "tokens" | "credits";

/** `usage_core::TokenCounts` — the four Anthropic-style stages plus reasoning/credits. */
export interface TokenCounts {
  input: number;
  cache_creation: number;
  cache_read: number;
  output: number;
  reasoning: number;
  credits: number;
}

export interface UnpricedModel {
  tool: string;
  model: string;
  total_tokens: number;
  requests: number;
}

export interface Summary {
  counts: TokenCounts;
  total_tokens: number;
  cost: number;
  credits: number;
  /** Part of `cost` converted from credits at a published plan price: ">0" means
      the total is partly an estimate, which is why the UI prefixes it with ≈. */
  credit_cost: number;
  requests: number;
  sessions: number;
  cached_pct: number;
  unpriced: UnpricedModel[];
}

export interface Item {
  key: string;
  label: string;
  counts: TokenCounts;
  total_tokens: number;
  cost: number;
  requests: number;
  sessions: number;
  /** `false` ⇒ no price known: render "无价格", never "$0.00". */
  priced: boolean;
}

export interface Breakdown {
  tools: Item[];
  models: Item[];
  projects: Item[];
  mcps: Item[];
  skills: Item[];
}

export interface Window {
  key: string;
  label: string;
  start_ms: number;
  end_ms: number;
  summary: Summary;
  prev: Summary;
  delta_cost_pct: number;
  delta_tokens_pct: number;
  breakdown: Breakdown;
}

export interface HeatCell {
  /** `YYYY-MM-DD`, local. */
  date: string;
  total_tokens: number;
  cost: number;
  requests: number;
}

/** One local-hour bucket of today; the hour is the array index. */
export interface HourCell {
  total_tokens: number;
  cost: number;
  requests: number;
}

export interface QuotaView {
  tool: string;
  used_percent: number;
  window_minutes: number;
  resets_at_ms: number;
  sampled_at_ms: number;
  /** Bucket name when one tool reports several windows at once. */
  label?: string | null;
  /** Stable identity across polls, when the source has one. Labels carry live
   *  numbers and implied lengths drift, so a saved ordering keys on this. */
  id?: string | null;
  /** `probe` = read live from the tool, `log` = embedded in its own records,
   *  `budget` = a cap set in tokenme and measured against our own cost. */
  origin?: "log" | "probe" | "budget";
}

/** The saved drag order of the quota section: tool ids for the groups,
 *  `<tool>/<row id>` for the bars inside them. Absent entries keep the
 *  report's own rank-stable order. */
export interface QuotaOrder {
  tools: string[];
  rows: string[];
}

/** What the OS says about TokenMe's notification permission, plus `unknown`
 *  where the platform has no answer (Windows). Mirrors the Rust vocabulary in
 *  `notify::permission_state`. */
export type NotifyState = "granted" | "denied" | "not_determined" | "unknown";

export interface SourceStatus {
  id: string;
  display: string;
  detected: boolean;
  roots: string[];
  hint: string | null;
  events_ingested: number;
}

export type PricingSource = "models_dev" | "cache" | "bundled" | "unavailable";

export interface PricingMeta {
  source: PricingSource;
  fetched_at_ms: number;
  stale: boolean;
  key_count: number;
  cache_dir: string | null;
}

export interface SessionRow {
  tool: string;
  session: string;
  project: string | null;
  model: string;
  first_ms: number;
  last_ms: number;
  total_tokens: number;
  cost: number;
  requests: number;
}

/** `usage_core::SyncRecord` — one bundle from another machine, merged into
    this index. The newest import per origin wins in the badge; anything older
    than 24h per origin makes the badge warn. */
export interface SyncRecord {
  origin: string;
  imported_at_ms: number;
  window_lo_ms: number;
  window_hi_ms: number;
  rows: number;
  file: string;
  sha256?: string;
}

/** Which machines a report's numbers cover. Mirrors Rust `MachineScope`
    (`{"kind":"all"}` / `{"kind":"local"}` / `{"kind":"origin","name":"ops-box"}`). */
export type MachineScope = { kind: "all" } | { kind: "local" } | { kind: "origin"; name: string };

/* ── 远程服务器（拉取式采集） --------------------------------------------- */
/* 与 `src-tauri/src/servers/` 的 serde 输出逐字对应；命令面见 Bridge。 */

/** A classified backend failure (`SshError`): `kind` picks the title, `detail` is the raw text. */
export interface SshError {
  kind: string;
  detail: string;
}

export type ServerStatus = "ok" | "warn" | "error" | "syncing" | "none";

/** One completed pull attempt, newest last (capped server-side). */
export interface ServerSyncEvent {
  at_ms: number;
  rows: number;
  took_ms: number;
  ok: boolean;
}

/** `ServerView` — the record plus live scheduling facts. Never carries a password. */
export interface ServerView {
  id: number;
  name: string;
  host: string;
  port: number;
  user: string;
  fingerprint: string;
  every_secs: number;
  days: number;
  enabled: boolean;
  status: ServerStatus;
  last_ok_ms: number | null;
  last_error: SshError | null;
  last_rows: number;
  last_took_ms: number;
  next_due_ms: number | null;
  tools: string[];
  history: ServerSyncEvent[];
}

/** `AuthReq` (tagged by `kind`). The password lives only in a wizard session. */
export type ServerAuth =
  | { kind: "password"; password: string }
  | { kind: "key"; path: string; passphrase?: string | null }
  | { kind: "default" };

export interface ServerProbeReq {
  name: string;
  host: string;
  port?: number;
  user: string;
  auth: ServerAuth;
}

export interface ServerProbeOutcome {
  ok: boolean;
  /** The wizard session that install must present; null on failure. */
  session: number | null;
  fingerprint: string | null;
  /** What `servers.json` already pins for this host:port. */
  known_fingerprint: string | null;
  /** Presented key differs from the pin — the UI must ask for explicit re-confirmation. */
  mismatch: boolean;
  arch: string | null;
  hostname: string | null;
  error: SshError | null;
}

/** Mirrors the Rust step keys in order: keygen…merge. */
export type InstallStepState = "pending" | "active" | "done" | "warn" | "error";

export interface InstallStep {
  key: string;
  state: InstallStepState;
  detail: string | null;
}

export interface ServerInstallProgress {
  session: number;
  steps: InstallStep[];
  done: boolean;
  ok: boolean;
  error: SshError | null;
}

export interface ServerInstallReq {
  session: number;
  every_secs: number;
  days: number;
  fingerprint: string;
}

export interface ServerInstallOutcome {
  ok: boolean;
  server: ServerView | null;
  detected: string[];
  rows: number;
  /** The bundle is on disk but the engine had not published its merge yet. */
  merge_pending: boolean;
  error: SshError | null;
}

export interface ServerRemoveOutcome {
  removed: boolean;
  /** `true` cleanup ran ok; `false` it was wanted but failed; `null` not requested. */
  cleaned: boolean | null;
  cleanup_error: SshError | null;
}

export interface ServerUpdateReq {
  id: number;
  every_secs?: number;
  days?: number;
  enabled?: boolean;
}

/** One machine row of the scope switcher; `origin: ""` is this machine. */
export interface MachineView {
  origin: string;
  today_tokens: number;
}

export interface Report {
  generated_at_ms: number;
  utc_offset: string;
  day: Window;
  week: Window;
  month: Window;
  year: Window;
  heatmap: HeatCell[];
  /** Today by local hour, 24 slots. Older snapshots may omit it (empty). */
  hourly: HourCell[];
  quotas: QuotaView[];
  /** True until the quota probes have run (boot publishes without them). */
  quotas_pending?: boolean;
  /** The engine published this last run and this run restored it from disk
   *  before folding anything: same machine and scope, older than the moment. */
  from_previous_run?: boolean;
  sources: SourceStatus[];
  pricing: PricingMeta;
  recent_sessions: SessionRow[];
  all_time: Summary;
  /** The scope these numbers were folded under — echoed so a report that
      arrives after a switch is identifiable. Absent on older snapshots. */
  scope?: MachineScope;
  /** Every machine in the index, unscoped: `""` (this machine) first, then
      origins by name. Absent on older snapshots. */
  machines?: MachineView[];
  /** Machine-sync imports, newest first. Absent on snapshots taken before
      this field existed (older engines), hence optional. */
  syncs?: SyncRecord[];
}

export type PeriodKey = "day" | "week" | "month" | "year";

/** The panel is four pages rather than one long scroll; see `App.tsx`. */
export type PageKey = "overview" | "tools" | "ranks" | "detail";

/** Tray label modes, mirrored from `tokenme::TrayMode` (snake_case). */
export type TrayMode =
  | "tray_only"
  | "tokens_only"
  | "cost_only"
  | "tray_tokens"
  | "tray_cost"
  | "tray_tokens_cost";

export interface TrayState {
  label: string;
  tooltip: string;
  mode: TrayMode;
}

export interface Bridge {
  checkinNow: (tool: string) => Promise<string>;
  /** `window.__TAURI_INTERNALS__` present ⇒ real app; otherwise the dev fixture. */
  live: boolean;
  fetchReport: (force: boolean) => Promise<Report>;
  refreshPricing: () => Promise<void>;
  trayState: () => Promise<TrayState | null>;
  setTrayMode: (mode: TrayMode) => Promise<TrayState | null>;
  onReport: (handler: (report: Report) => void) => () => void;
  /** A tray menu entry ("本周"/"今日") asks the panel to focus a period. */
  onTrayPeriod: (handler: (period: PeriodKey) => void) => () => void;
  /** Switch the whole panel's machine scope; the engine re-folds and the next
   *  published report echoes it in `Report.scope`. */
  setReportScope: (scope: MachineScope) => Promise<void>;
  /** `tool id → data:image/png;base64,…` for the tools that ship a macOS app. */
  toolIcons: () => Promise<Record<string, string>>;
  /** The saved drag order of the quota section (empty lists ⇒ report order). */
  quotaOrder: () => Promise<QuotaOrder>;
  setQuotaOrder: (order: QuotaOrder) => Promise<void>;
  /** The notification permission as the OS reports it, for the quota section's
   *  guide. `?notify=` pins it in the browser preview, same family as `?stale=`. */
  notifyStatus: () => Promise<NotifyState>;
  /** The guide's one button: raises the system prompt (undecided) or opens the
   *  notification settings pane (denied). Returns the state it routed on. */
  notifyEnable: () => Promise<NotifyState>;
  /** Report the webview's detected UI language to the native chrome. */
  setUiLang: (lang: string) => Promise<void>;
  /** Cold-start telemetry, stage 1: the bundle ran and index.html's shell is up. */
  reportBoot: () => Promise<void>;
  /** Cold-start telemetry, stage 2: the page has figures. This is what retires the
   *  native boot note, so it must not fire earlier than real content. */
  reportContent: () => Promise<void>;
  /** The settings sheet: autostart, fallback poll cadence, build version. */
  panelSettings: () => Promise<PanelSettings>;
  setAutostart: (on: boolean) => Promise<void>;
  setRefreshSecs: (secs: number) => Promise<void>;
  setTheme: (theme: ThemeKey) => Promise<void>;
  setShowMoney: (on: boolean) => Promise<void>;
  setShowEmptyTools: (on: boolean) => Promise<void>;
  setBubbleEnabled: (on: boolean) => Promise<void>;
  setHostExitPause: (on: boolean) => Promise<void>;
  /** The master switch: off, the engine asks no vendor for anything. */
  setQuotaPolling: (on: boolean) => Promise<void>;
  /** One probe's own switch — the only lever with no host mapping. */
  setToolPolling: (tool: string, on: boolean) => Promise<void>;
  /** The registry's own list, with what this machine has answered. */
  probeTools: () => Promise<ProbeTool[]>;
  setNotifyTiers: (tiers: NotifyTierKey) => Promise<void>;
  setToolMuted: (tool: string, on: boolean) => Promise<void>;
  setAutoUpdateCheck: (on: boolean) => Promise<void>;
  checkUpdate: () => Promise<UpdateStatus>;
  downloadUpdate: () => Promise<UpdateStatus>;
  installUpdate: () => Promise<void>;
  onUpdateProgress: (handler: (progress: DownloadProgress) => void) => () => void;
  showPanel: () => Promise<void>;
  hidePanel: () => Promise<void>;
  /** Windows only: a text field can only receive keys while the window owns
   *  the keyboard, and the non-activating tray panel never does. No-op in the
   *  browser fixture and on macOS. */
  setKeyboardMode: (on: boolean) => Promise<void>;
  /** Hands the press to the Rust drag loop; the native move loop cannot move a
   *  non-activating window. Resolves immediately; the drag runs in Rust. */
  beginBubbleDrag: () => Promise<void>;
  quit: () => Promise<void>;
  /** Release page / mailto — the Rust side whitelists http(s) and mailto only. */
  openExternal: (url: string) => Promise<void>;
  /** Reveal the diagnostic log directory in the file manager. */
  openLogDir: () => Promise<void>;
  /** Configured remote servers with live scheduling facts (empty most of the time). */
  servers: () => Promise<ServerView[]>;
  /** The dedicated key pair's public half; generates on first call. */
  serverPublicKey: () => Promise<{ path: string; line: string }>;
  serverProbe: (req: ServerProbeReq) => Promise<ServerProbeOutcome>;
  serverInstall: (req: ServerInstallReq) => Promise<ServerInstallOutcome>;
  /** Drop a wizard session (zeroizes its password); safe to call twice. */
  serverAbortSession: (session: number) => Promise<void>;
  serverSyncNow: (id: number) => Promise<void>;
  serverUpdate: (req: ServerUpdateReq) => Promise<ServerView[]>;
  serverRemove: (id: number, cleanupRemote: boolean) => Promise<ServerRemoveOutcome>;
  onServersUpdated: (handler: (servers: ServerView[]) => void) => () => void;
  onInstallProgress: (handler: (progress: ServerInstallProgress) => void) => () => void;
}

/** `system` defers to the OS media query; `light`/`dark` pin the panel. */
export type ThemeKey = "system" | "light" | "dark";

/** Which banner lines fire: both lines, only exhaustion, or nothing at all. */
export type NotifyTierKey = "both" | "exhausted" | "off";

export interface PanelSettings {
  autostart: boolean;
  refresh_secs: number;
  theme: ThemeKey;
  show_money: boolean;
  show_empty_tools: boolean;
  bubble_enabled: boolean;
  host_exit_pause: boolean;
  /** The master switch: off, not one vendor request leaves the process. */
  quota_polling: boolean;
  /** Tools the user stopped by hand — the only lever for probes with no host. */
  quota_probes_off: string[];
  notify_tiers: NotifyTierKey;
  /** Tools whose windows never post a banner. */
  notify_muted: string[];
  auto_update_check: boolean;
  version: string;
  /** The build number the panel logs at startup — the same digits a support
   *  reply asks for, shown where the user can copy it. */
  build: string;
}

/** One row of the 逐工具探测 list: what the registry actually holds, not a
 *  hand-written list. `answered_here` is the "本机答过" judge the settings
 *  sub-page filters on — `detected` is true for every source on a full machine,
 *  so it cannot separate anything. Names come from the report's own
 *  `sources[].display`, falling back to `toolDisplay` for the probes with no
 *  adapter (copilot, gemini). */
export interface ProbeTool {
  id: string;
  /** Whether `host_exit_pause` can reach this probe at all. */
  host_gated: boolean;
  answered_here: boolean;
  /** What the composed gate says right now: master switch ∧ not excluded. */
  polling: boolean;
}

export interface UpdateStatus {
  phase: string;
  message: string;
  version: string | null;
}

/** Streamed while the update artifact downloads: `下载中 {percent}%`. */
export interface DownloadProgress {
  percent: number;
  downloaded: number;
  total: number;
}
