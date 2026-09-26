import type { PeriodKey, TokenCounts, TrayMode } from "../types";

/** Fixed 14-slot categorical ramp; index order is the order tools appear. */
const TOOL_ORDER = [
  "claude",
  "codex",
  "opencode",
  "pi",
  "cline",
  "zcode",
  "qoder",
  "antigravity",
  "ccswitch",
  "agnes",
  "atomcode",
  "workbuddy",
  "crow5",
  "mimocode",
  "cola",
  "catpaw",
  "dsh",
];

const DISPLAY: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  pi: "Pi",
  cline: "Cline",
  zcode: "ZCode",
  qoder: "Qoder",
  antigravity: "Antigravity",
  ccswitch: "CC Switch",
  agnes: "AgnesCode",
  atomcode: "AtomCode",
  workbuddy: "WorkBuddy",
  crow5: "Crow5",
  mimocode: "Mimocode",
  cola: "Cola",
  gemini: "Gemini CLI",
  cursor: "Cursor",
};

export function toolDisplay(id: string): string {
  return DISPLAY[id] ?? id.charAt(0).toUpperCase() + id.slice(1);
}

/** Colour is assigned by id, never by rank, so a tool keeps its colour across periods. */
export function toolSlot(id: string): number {
  const exact = TOOL_ORDER.indexOf(id);
  if (exact >= 0) return exact;
  let hash = 0;
  for (const ch of id) hash = (hash * 31 + ch.charCodeAt(0)) % 997;
  return hash % TOOL_ORDER.length;
}

export function toolColor(id: string): string {
  return `var(--cat-${toolSlot(id) + 1})`;
}

/** 14.02M / 842.4K / 96 — the unit is a separate node so it can be styled smaller. */
export function splitTokens(n: number): { value: string; unit: string } {
  const abs = Math.abs(n);
  if (abs >= 1e9) return { value: (n / 1e9).toFixed(2), unit: "B" };
  if (abs >= 1e6) return { value: (n / 1e6).toFixed(2), unit: "M" };
  if (abs >= 1e4) return { value: (n / 1e3).toFixed(1), unit: "K" };
  return { value: Math.round(n).toLocaleString("en-US"), unit: "" };
}

export function compactTokens(n: number): string {
  const { value, unit } = splitTokens(n);
  return unit ? `${value}${unit}` : value;
}

/**
 * The bubble's text room: ~5 characters of number plus one unit letter inside a
 * 76 px ball. Precision folds as the magnitude grows (9.87M / 12.3M / 100M) and
 * rounding always carries into the next unit (999.6M → 1B) — it may never
 * widen the string. The panel keeps `splitTokens`' precision; only the ball
 * truncates.
 */
export function ballTokens(n: number): string {
  const abs = Math.abs(n);
  if (abs < 1e4) return Math.round(n).toLocaleString("en-US");
  const thresholds = [1e4, 1e6, 1e9, 1e12];
  const units = ["K", "M", "B", "T"];
  let i = 0;
  for (let k = thresholds.length - 1; k >= 0; k--) {
    if (abs >= thresholds[k]) {
      i = k;
      break;
    }
  }
  let v = abs / thresholds[i];
  v = Number(v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2));
  if (v >= 1000 && i < units.length - 1) {
    i += 1;
    v = Number((v / 1000).toFixed(2));
  }
  return `${n < 0 ? "-" : ""}${v}${units[i]}`;
}

export function money(n: number): string {
  if (n === 0) return "$0";
  if (n > 0 && n < 0.01) return "<$0.01";
  return `$${n.toFixed(2)}`;
}

/** Credit-metered sources are not money and must never render as "$0". */
export function credits(n: number): string {
  return `${n < 10 ? n.toFixed(2) : n.toFixed(0)} credits`;
}

export function count(n: number): string {
  return n.toLocaleString("en-US");
}

export function percent(n: number, digits = 0): string {
  return `${n.toFixed(digits)}%`;
}

export function signedPercent(n: number): string {
  if (!Number.isFinite(n)) return "—";
  const rounded = Math.abs(n) < 0.05 ? 0 : n;
  return `${rounded > 0 ? "+" : ""}${rounded.toFixed(0)}%`;
}

/** Delta is directional, not decorative: the sign and arrow are always present. */
export function deltaDirection(n: number): "up" | "down" | "flat" {
  if (!Number.isFinite(n) || Math.abs(n) < 0.5) return "flat";
  return n > 0 ? "up" : "down";
}

export function relativeTime(ms: number, nowMs: number): string {
  const diff = Math.max(0, nowMs - ms);
  const min = Math.floor(diff / 60_000);
  if (min < 1) return "刚刚";
  if (min < 60) return `${min} 分钟前`;
  const hours = Math.floor(min / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days === 1) return "昨天";
  if (days < 7) return `${days} 天前`;
  return `${Math.floor(days / 7)} 周前`;
}

/** "in 3h 12m" style countdown, dropping the unit pair that adds no information. */
export function until(resetsAtMs: number, nowMs: number): string {
  const diff = resetsAtMs - nowMs;
  if (diff <= 0) return "即将重置";
  const totalMin = Math.floor(diff / 60_000);
  const days = Math.floor(totalMin / 1440);
  const hours = Math.floor((totalMin % 1440) / 60);
  const min = totalMin % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${String(min).padStart(2, "0")}m`;
  return `${min}m`;
}

export const PERIOD_LABEL: Record<PeriodKey, string> = {
  day: "今日",
  week: "本周",
  month: "本月",
  year: "今年",
};

export const PERIOD_PREV: Record<PeriodKey, string> = {
  day: "昨日同期",
  week: "上周同期",
  month: "上月同期",
  year: "去年同期",
};

export const PERIOD_SHORT: Record<PeriodKey, string> = {
  day: "日",
  week: "周",
  month: "月",
  year: "年",
};

/** `TokenCounts::total` and `cached_pct` are Rust methods; mirror them here. */
export function totalOf(c: TokenCounts): number {
  return c.input + c.cache_creation + c.cache_read + c.output;
}

export function cachedOf(c: TokenCounts): number {
  const prompt = c.input + c.cache_creation + c.cache_read;
  return prompt <= 0 ? 0 : (c.cache_read / prompt) * 100;
}

export const TRAY_MODE_LABEL: Record<TrayMode, string> = {
  tray_only: "仅托盘",
  tokens_only: "仅Token",
  cost_only: "仅花费",
  tray_tokens: "托盘·Token",
  tray_cost: "托盘·花费",
  tray_tokens_cost: "托盘·Token·花费",
};

/** Local `YYYY-MM-DD` for an epoch-ms value, matching how report.rs buckets days. */
export function localDate(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

export function formatDateLabel(iso: string): string {
  const [, m, d] = iso.split("-");
  return `${Number(m)} 月 ${Number(d)} 日`;
}

export function weekdayInitial(iso: string): string {
  const [y, m, d] = iso.split("-").map(Number);
  return ["日", "一", "二", "三", "四", "五", "六"][new Date(y, m - 1, d).getDay()];
}
