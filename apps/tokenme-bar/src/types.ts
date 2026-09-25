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

export interface Report {
  generated_at_ms: number;
  utc_offset: string;
  day: Window;
  week: Window;
  month: Window;
  heatmap: HeatCell[];
  quotas: QuotaView[];
  sources: SourceStatus[];
  pricing: PricingMeta;
  recent_sessions: SessionRow[];
  all_time: Summary;
}

export type PeriodKey = "day" | "week" | "month";

/** The panel is four pages rather than one long scroll; see `App.tsx`. */
export type PageKey = "overview" | "tools" | "ranks" | "detail";

/** Tray label modes, mirrored from `tokenme_bar::TrayMode`. */
export type TrayMode = "cost" | "tokens" | "quiet";

export interface TrayState {
  label: string;
  tooltip: string;
  mode: TrayMode;
  modes: TrayMode[];
}

export interface Bridge {
  /** `window.__TAURI_INTERNALS__` present ⇒ real app; otherwise the dev fixture. */
  live: boolean;
  fetchReport: (force: boolean) => Promise<Report>;
  trayState: () => Promise<TrayState | null>;
  setTrayMode: (mode: TrayMode) => Promise<TrayState | null>;
  onReport: (handler: (report: Report) => void) => () => void;
  /** A tray menu entry ("本周"/"今日") asks the panel to focus a period. */
  onTrayPeriod: (handler: (period: PeriodKey) => void) => () => void;
  /** `tool id → data:image/png;base64,…` for the tools that ship a macOS app. */
  toolIcons: () => Promise<Record<string, string>>;
  /** The saved drag order of the quota section (empty lists ⇒ report order). */
  quotaOrder: () => Promise<QuotaOrder>;
  setQuotaOrder: (order: QuotaOrder) => Promise<void>;
  /** The settings sheet: autostart, fallback poll cadence, build version. */
  panelSettings: () => Promise<PanelSettings>;
  setAutostart: (on: boolean) => Promise<void>;
  setRefreshSecs: (secs: number) => Promise<void>;
  setTheme: (theme: ThemeKey) => Promise<void>;
  setShowMoney: (on: boolean) => Promise<void>;
}

/** `system` defers to the OS media query; `light`/`dark` pin the panel. */
export type ThemeKey = "system" | "light" | "dark";

export interface PanelSettings {
  autostart: boolean;
  refresh_secs: number;
  theme: ThemeKey;
  show_money: boolean;
  version: string;
}
