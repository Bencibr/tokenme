import type {
  Breakdown,
  HeatCell,
  Item,
  PricingMeta,
  QuotaView,
  Report,
  SessionRow,
  SourceStatus,
  Summary,
  TokenCounts,
  UnpricedModel,
  Window,
} from "./types";

/**
 * A realistic, hand-authored `usage_core::Report`, used when the panel runs in a
 * plain browser (`pnpm dev`, no Rust). Every per-item number is literal; the
 * enclosing `Summary` is *derived by summing those same items*, which is what
 * `report.rs::summarize` does, so two numbers in here can never disagree.
 */

const DAY = 86_400_000;
/** `usage_core::report::HEATMAP_DAYS` — a whole number of weeks ending today. */
const HEATMAP_DAYS = 371;

function counts(
  input: number,
  cache_creation: number,
  cache_read: number,
  output: number,
  reasoning = 0,
  credits = 0,
): TokenCounts {
  return { input, cache_creation, cache_read, output, reasoning, credits };
}

/** Mirrors `TokenCounts::total` / `cached_pct`, which are Rust methods. */
const total = (c: TokenCounts) => c.input + c.cache_creation + c.cache_read + c.output;
const cachedPct = (c: TokenCounts) => {
  const prompt = c.input + c.cache_creation + c.cache_read;
  return prompt <= 0 ? 0 : (c.cache_read / prompt) * 100;
};

const round2 = (n: number) => Math.round(n * 100) / 100;
const round1 = (n: number) => Math.round(n * 10) / 10;
const pad = (n: number) => String(n).padStart(2, "0");

interface ItemSeed {
  key: string;
  c: TokenCounts;
  cost: number;
  requests: number;
  sessions: number;
  priced?: boolean;
}

function item(seed: ItemSeed): Item {
  return {
    key: seed.key,
    label: seed.key,
    counts: seed.c,
    total_tokens: total(seed.c),
    cost: seed.cost,
    requests: seed.requests,
    sessions: seed.sessions,
    priced: seed.priced ?? true,
  };
}

/** Mirrors `summarize_events`. `sessions` is summed per group, as report.rs does. */
function summaryOf(items: Item[], unpriced: UnpricedModel[] = []): Summary {
  const acc = counts(0, 0, 0, 0);
  let cost = 0;
  let creditMoney = 0;
  let requests = 0;
  let sessions = 0;
  for (const i of items) {
    acc.input += i.counts.input;
    acc.cache_creation += i.counts.cache_creation;
    acc.cache_read += i.counts.cache_read;
    acc.output += i.counts.output;
    acc.reasoning += i.counts.reasoning;
    acc.credits += i.counts.credits;
    cost += i.cost;
    if (i.counts.credits > 0 && total(i.counts) === 0) creditMoney += i.cost;
    requests += i.requests;
    sessions += i.sessions;
  }
  return {
    counts: acc,
    total_tokens: total(acc),
    cost: round2(cost),
    credits: acc.credits,
    credit_cost: round2(creditMoney),
    requests,
    sessions,
    cached_pct: round1(cachedPct(acc)),
    unpriced,
  };
}

function pctDelta(cur: number, prev: number): number {
  if (Math.abs(prev) < 1e-9) return Math.abs(cur) < 1e-9 ? 0 : 100;
  return round1(((cur - prev) / prev) * 100);
}

/* --------------------------------------------------------------- date utils */

function dayKey(d: Date): string {
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function localMidnight(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

/** `Date#setDate` keeps local calendar days correct across DST. */
function shiftDay(midnight: Date, by: number): Date {
  const d = new Date(midnight.getTime());
  d.setDate(d.getDate() + by);
  return d;
}

function mondayOf(d: Date): Date {
  return shiftDay(localMidnight(d), -((d.getDay() + 6) % 7));
}

/** ISO-8601 week key, resolved from the week's Thursday like `chrono::iso_week`. */
function isoWeekKey(monday: Date): string {
  const thursday = shiftDay(monday, 3);
  const ordinal = Math.floor((thursday.getTime() - new Date(thursday.getFullYear(), 0, 1).getTime()) / DAY) + 1;
  return `${thursday.getFullYear()}-W${pad(Math.floor((ordinal + 6) / 7))}`;
}

/* ------------------------------------------------------------- item factory */

/**
 * Money split per stage, per tool: Claude Code leans hard on prompt caching,
 * Codex barely uses it, Qoder is credit-metered and therefore has no tokens.
 * Each row sums to exactly 1.0 so derived totals stay literal.
 */
const SPLIT: Record<string, [number, number, number, number]> = {
  claude: [0.16, 0.09, 0.71, 0.04],
  codex: [0.62, 0.0, 0.3, 0.08],
  opencode: [0.44, 0.04, 0.46, 0.06],
};

function toolItem(tool: string, tokens: number, cost: number, requests: number, sessions: number): Item {
  const [i, cc, cr, o] = SPLIT[tool] ?? [0.55, 0.05, 0.33, 0.07];
  return item({
    key: tool,
    c: counts(
      Math.round(tokens * i),
      Math.round(tokens * cc),
      Math.round(tokens * cr),
      Math.round(tokens * o),
      Math.round(tokens * o * 0.22),
    ),
    cost,
    requests,
    sessions,
  });
}

function flatItem(key: string, tokens: number, cost: number, requests: number, sessions: number, priced = true): Item {
  return item({
    key,
    c: counts(Math.round(tokens * 0.55), Math.round(tokens * 0.05), Math.round(tokens * 0.33), Math.round(tokens * 0.07)),
    cost,
    requests,
    sessions,
    priced,
  });
}

function creditItem(credits: number, requests: number, sessions: number): Item {
  return item({ key: "qoder", c: counts(0, 0, 0, 0, 0, credits), cost: 0, requests, sessions });
}

interface PeriodSpec {
  tools: Array<[string, number, number, number, number]>;
  /** Trailing tag: "unpriced" ⇒ no known price, "credit" ⇒ credit-metered, zero tokens. */
  models: Array<[string, number, number, number, ("unpriced" | "credit")?]>;
  projects: Array<[string, number, number, number]>;
  mcps: Array<[string, number, number]>;
  skills: Array<[string, number, number]>;
  unpriced?: UnpricedModel[];
  prev: Summary;
}

function buildWindow(now: Date, kind: "day" | "week" | "month", spec: PeriodSpec): Window {
  const tools = spec.tools.map(([tool, tokens, cost, requests, sessions]) =>
    // For the credit-metered tool the `tokens` slot carries credits instead.
    tool === "qoder" ? creditItem(tokens, requests, sessions) : toolItem(tool, tokens, cost, requests, sessions),
  );
  const models = spec.models.map(([m, tokens, cost, requests, mode]) =>
    mode === "credit"
      ? item({ key: m, c: counts(0, 0, 0, 0, 0, 0.42), cost: 0, requests, sessions: 3 })
      : flatItem(m, tokens, cost, requests, 3, mode !== "unpriced"),
  );
  const projects = spec.projects.map(([p, tokens, cost, requests]) => flatItem(p, tokens, cost, requests, 5));
  const mcps = spec.mcps.map(([m, tokens, requests]) => flatItem(m, tokens, round2((tokens / 1_000_000) * 0.34), requests, 2));
  const skills = spec.skills.map(([s, tokens, requests]) => flatItem(s, tokens, round2((tokens / 1_000_000) * 0.31), requests, 2));
  const summary = summaryOf(tools, spec.unpriced ?? []);
  const today = localMidnight(now);
  const monday = mondayOf(now);
  let start = today;
  let key: string;
  let label: string;
  if (kind === "day") {
    key = dayKey(today);
    label = `${pad(today.getMonth() + 1)}-${pad(today.getDate())}`;
  } else if (kind === "week") {
    start = monday;
    key = isoWeekKey(monday);
    label = `${pad(monday.getMonth() + 1)}-${pad(monday.getDate())} ~ ${pad(today.getMonth() + 1)}-${pad(today.getDate())}`;
  } else {
    start = new Date(today.getFullYear(), today.getMonth(), 1);
    key = `${today.getFullYear()}-${pad(today.getMonth() + 1)}`;
    label = key;
  }

  const breakdown: Breakdown = { tools, models, projects, mcps, skills };
  return {
    key,
    label,
    start_ms: start.getTime(),
    end_ms: now.getTime(),
    summary,
    prev: spec.prev,
    delta_cost_pct: pctDelta(summary.cost, spec.prev.cost),
    delta_tokens_pct: pctDelta(summary.total_tokens, spec.prev.total_tokens),
    breakdown,
  };
}

/* ------------------------------------------------------------------ heatmap */

/** [tokens, cost, requests] for the 28 days ending today, oldest first. */
const RECENT_DAYS: Array<[number, number, number]> = [
  [1_842_000, 0.71, 46], [3_118_000, 1.18, 71], [2_604_000, 0.94, 63], [4_411_000, 1.62, 88], [1_190_000, 0.4, 21],
  [0, 0, 0], [486_000, 0.16, 9], [5_248_000, 1.94, 102], [3_902_000, 1.41, 79], [6_884_000, 2.53, 141],
  [2_275_000, 0.83, 51], [982_000, 0.31, 18], [0, 0, 0], [1_614_000, 0.52, 33], [7_336_000, 2.71, 158],
  [8_109_000, 3.06, 173], [5_772_000, 2.14, 118], [3_268_000, 1.19, 74], [0, 0, 0], [741_000, 0.24, 14],
  [6_118_000, 2.28, 131], [9_427_000, 3.58, 204], [7_851_000, 2.91, 166], [4_930_000, 1.77, 98],
  [2_146_000, 0.79, 44], [0, 0, 0], [1_328_000, 0.44, 27], [12_380_000, 4.21, 331],
];

/** The back catalogue: busier usage as the tool became the daily driver, then a gap before install. */
const HISTORY_BANDS: Array<{ days: number; tokens: number; rate: number; perReq: number }> = [
  { days: 84, tokens: 2_640_000, rate: 0.28, perReq: 26_400 },
  { days: 84, tokens: 1_180_000, rate: 0.21, perReq: 21_000 },
  { days: 84, tokens: 420_000, rate: 0.17, perReq: 17_400 },
  { days: 91, tokens: 0, rate: 0, perReq: 1 },
];

/** Deterministic jitter, so the fixture looks identical on every reload. */
function noise(i: number): number {
  const x = Math.sin(i * 12.9898) * 43758.5453;
  return x - Math.floor(x);
}

/** Bands run from the newest history day outwards, i.e. busier → quieter. */
function bandFor(daysSinceHistory: number) {
  let from = 0;
  for (const band of HISTORY_BANDS) {
    if (daysSinceHistory < from + band.days) return band;
    from += band.days;
  }
  return HISTORY_BANDS[HISTORY_BANDS.length - 1];
}

function buildHeatmap(today: Date): HeatCell[] {
  const history = HEATMAP_DAYS - RECENT_DAYS.length;
  if (HISTORY_BANDS.reduce((a, b) => a + b.days, 0) !== history) {
    throw new Error(`fixture heatmap bands must cover exactly ${history} days, not ${HISTORY_BANDS.reduce((a, b) => a + b.days, 0)}`);
  }

  const cells: HeatCell[] = [];
  for (let daysAgo = HEATMAP_DAYS - 1; daysAgo >= RECENT_DAYS.length; daysAgo -= 1) {
    const band = bandFor(daysAgo - RECENT_DAYS.length);
    const date = shiftDay(today, -daysAgo);
    const weekend = date.getDay() === 0 || date.getDay() === 6;
    const n = noise(daysAgo);
    const idle = n < (band.tokens === 0 ? 1 : weekend ? 0.55 : 0.16);
    const tokens = idle ? 0 : Math.round(band.tokens * (0.25 + n * 1.35) * (weekend ? 0.4 : 1));
    cells.push({
      date: dayKey(date),
      total_tokens: tokens,
      cost: round2((tokens / 1_000_000) * band.rate),
      requests: tokens === 0 ? 0 : Math.max(1, Math.round(tokens / band.perReq)),
    });
  }

  RECENT_DAYS.forEach(([total_tokens, cost, requests], i) => {
    cells.push({
      date: dayKey(shiftDay(today, i - RECENT_DAYS.length + 1)),
      total_tokens,
      cost,
      requests,
    });
  });
  return cells;
}

/* ------------------------------------------------------------------- extras */

const SOURCES: SourceStatus[] = [
  {
    id: "claude",
    display: "Claude Code",
    detected: true,
    roots: ["~/.claude/projects"],
    hint: "2.4 GB · 增量 tail",
    events_ingested: 8_241,
  },
  {
    id: "codex",
    display: "Codex",
    detected: true,
    roots: ["~/.codex/sessions"],
    hint: "上报 rate_limits",
    events_ingested: 12_604,
  },
  {
    id: "opencode",
    display: "OpenCode",
    detected: true,
    roots: ["~/.local/share/opencode/opencode.db"],
    hint: null,
    events_ingested: 1_877,
  },
  {
    id: "qoder",
    display: "Qoder",
    detected: true,
    roots: ["~/Library/Application Support/Qoder/usage"],
    hint: "credits 计量，无 token",
    events_ingested: 412,
  },
  {
    id: "gemini",
    display: "Gemini CLI",
    detected: false,
    roots: ["~/.gemini/tmp"],
    hint: "未发现会话目录",
    events_ingested: 0,
  },
];

function quotasOf(now: number): QuotaView[] {
  return [
    { tool: "codex", used_percent: 12, window_minutes: 300, resets_at_ms: now + 3 * 3_600_000 + 12 * 60_000, sampled_at_ms: now - 90_000 },
    { tool: "claude", used_percent: 8.5, window_minutes: 10_080, resets_at_ms: now + 2 * DAY + 6 * 3_600_000, sampled_at_ms: now - 4 * 60_000 },
    { tool: "workbuddy", used_percent: 58.1, window_minutes: 0, resets_at_ms: now + 18 * DAY + 2 * 3_600_000, sampled_at_ms: now - 60_000, label: "Bonus Pack · 剩 180/430" },
  ];
}

function sessionsOf(now: number): SessionRow[] {
  const rows: Array<[string, string, string, string, number, number, number, number]> = [
    ["claude", "7f3ac19e", "tokenme", "glm-5.3-flash", 26 * 60_000, 4_118_000, 1.24, 86],
    ["codex", "c0d4f21a", "tokenscope-rs", "gpt-5", 74 * 60_000, 2_264_000, 0.61, 41],
    ["claude", "1a9bb0de", "wallet-web", "claude-sonnet-4-6", 3 * 3_600_000, 6_902_000, 2.48, 133],
    ["opencode", "e44d0a7c", "infra", "step-5-preview", 6 * 3_600_000, 812_000, 0.14, 22],
    ["qoder", "90c1ff23", "docs-site", "qmodel-latest", 21 * 3_600_000, 0, 0, 17],
    ["claude", "b7d3e155", "tokenme", "glm-5.3-flash", 26 * 3_600_000, 3_544_000, 1.02, 74],
    ["codex", "5590aa71", "cli", "gpt-5-mini", 30 * 3_600_000, 1_286_000, 0.19, 38],
  ];
  return rows.map(([tool, session, project, model, ago, tokens, cost, requests]) => ({
    tool,
    session,
    project: `/Users/me/work/${project}`,
    model,
    first_ms: now - ago - 12 * 60_000,
    last_ms: now - ago,
    total_tokens: tokens,
    cost,
    requests,
  }));
}

const PRICING: PricingMeta = {
  source: "models_dev",
  fetched_at_ms: 0,
  stale: false,
  key_count: 4_213,
  cache_dir: "~/Library/Caches/tokenme",
};

/* -------------------------------------------------------------------- build */

export function makeFixtureReport(nowMs = Date.now()): Report {
  const now = new Date(nowMs - (nowMs % 60_000));
  const nowMs2 = now.getTime();

  const day = buildWindow(now, "day", {
    tools: [
      ["claude", 8_102_000, 2.98, 214, 6],
      ["codex", 3_214_000, 0.96, 78, 3],
      ["opencode", 1_064_000, 0.27, 22, 2],
      ["qoder", 0.42, 0, 17, 1],
    ],
    models: [
      ["glm-5.3-flash", 6_118_000, 1.94, 148],
      ["claude-sonnet-4-6", 2_264_000, 1.42, 61],
      ["gpt-5", 2_418_000, 0.81, 54],
      ["step-5-preview", 1_064_000, 0.27, 22],
      ["gpt-5-mini", 802_000, 0.12, 24],
      ["kimi-k2-turbo", 118_400, 0, 9, "unpriced"],
      ["qmodel-latest", 0, 0, 17, "credit"],
    ],
    projects: [
      ["/Users/me/work/tokenme", 6_902_000, 2.31, 168],
      ["/Users/me/work/wallet-web", 3_148_000, 1.14, 87],
      ["/Users/me/work/tokenscope-rs", 1_604_000, 0.52, 44],
      ["/Users/me/work/infra", 548_000, 0.18, 21],
      ["/Users/me/work/docs-site", 178_000, 0.06, 11],
    ],
    mcps: [
      ["bugx", 2_864_000, 64],
      ["codebase-index", 1_902_000, 41],
      ["gh", 688_000, 19],
      ["context7", 214_000, 7],
    ],
    skills: [
      ["review", 1_684_000, 28],
      ["investigate", 942_000, 17],
      ["ship", 486_000, 9],
    ],
    unpriced: [{ tool: "codex", model: "kimi-k2-turbo", total_tokens: 118_400, requests: 9 }],
    prev: summaryOf([
      toolItem("claude", 6_104_000, 2.24, 178, 5),
      toolItem("codex", 3_018_000, 1.02, 82, 3),
      toolItem("opencode", 798_000, 0.42, 27, 1),
    ]),
  });

  const week = buildWindow(now, "week", {
    tools: [
      ["claude", 41_286_000, 14.62, 1_042, 24],
      ["codex", 18_402_000, 5.14, 388, 14],
      ["opencode", 6_118_000, 1.48, 141, 9],
      ["qoder", 1.9, 0, 96, 6],
    ],
    models: [
      ["glm-5.3-flash", 31_884_000, 9.42, 728],
      ["claude-sonnet-4-6", 12_402_000, 6.88, 296],
      ["gpt-5", 14_118_000, 4.02, 311],
      ["step-5-preview", 6_118_000, 1.48, 141],
      ["gpt-5-mini", 4_902_000, 0.71, 158],
    ],
    projects: [
      ["/Users/me/work/tokenme", 34_118_000, 11.84, 842],
      ["/Users/me/work/wallet-web", 18_264_000, 6.12, 428],
      ["/Users/me/work/tokenscope-rs", 8_146_000, 2.24, 214],
      ["/Users/me/work/infra", 3_402_000, 0.72, 108],
      ["/Users/me/work/docs-site", 1_876_000, 0.32, 75],
    ],
    mcps: [
      ["bugx", 14_802_000, 318],
      ["codebase-index", 9_618_000, 224],
      ["gh", 4_286_000, 112],
      ["context7", 1_402_000, 38],
    ],
    skills: [
      ["review", 8_146_000, 148],
      ["investigate", 5_402_000, 96],
      ["ship", 2_864_000, 54],
    ],
    unpriced: [{ tool: "codex", model: "kimi-k2-turbo", total_tokens: 986_400, requests: 74 }],
    prev: summaryOf([
      toolItem("claude", 32_408_000, 11.84, 942, 22),
      toolItem("codex", 20_112_000, 3.62, 421, 15),
      toolItem("opencode", 5_086_000, 0.48, 65, 5),
    ]),
  });

  const month = buildWindow(now, "month", {
    tools: [
      ["claude", 186_402_000, 64.28, 4_618, 108],
      ["codex", 82_118_000, 22.84, 1_742, 61],
      ["opencode", 28_486_000, 6.92, 648, 34],
      ["qoder", 7.4, 0, 412, 27],
    ],
    models: [
      ["glm-5.3-flash", 142_068_000, 41.22, 3_214],
      ["claude-sonnet-4-6", 58_264_000, 31.04, 1_386],
      ["gpt-5", 44_118_000, 12.88, 988],
      ["step-5-preview", 28_486_000, 6.92, 648],
      ["gpt-5-mini", 16_402_000, 2.44, 538],
    ],
    projects: [
      ["/Users/me/work/tokenme", 154_286_000, 52.84, 3_728],
      ["/Users/me/work/wallet-web", 78_142_000, 26.44, 1_842],
      ["/Users/me/work/tokenscope-rs", 34_028_000, 9.88, 812],
      ["/Users/me/work/infra", 18_402_000, 3.72, 486],
      ["/Users/me/work/docs-site", 12_148_000, 2.08, 348],
    ],
    mcps: [
      ["bugx", 68_142_000, 1_428],
      ["codebase-index", 42_806_000, 988],
      ["gh", 21_402_000, 548],
      ["context7", 8_118_000, 214],
    ],
    skills: [
      ["review", 38_264_000, 688],
      ["investigate", 24_118_000, 442],
      ["ship", 12_806_000, 248],
    ],
    unpriced: [{ tool: "codex", model: "kimi-k2-turbo", total_tokens: 4_186_000, requests: 312 }],
    prev: summaryOf([
      toolItem("claude", 154_408_000, 52.12, 4_182, 96),
      toolItem("codex", 62_112_000, 16.44, 1_528, 54),
      toolItem("opencode", 24_086_000, 2.86, 148, 18),
    ]),
  });

  const allTime = summaryOf([
    toolItem("claude", 1_284_018_000, 442.86, 31_408, 812),
    toolItem("codex", 518_264_000, 141.22, 8_942, 468),
    toolItem("opencode", 148_062_000, 34.08, 3_412, 214),
    creditItem(24.6, 4_182, 268),
  ]);

  return {
    generated_at_ms: nowMs2,
    utc_offset: fmtOffset(now),
    day,
    week,
    month,
    heatmap: buildHeatmap(localMidnight(now)),
    quotas: quotasOf(nowMs2),
    sources: SOURCES,
    pricing: { ...PRICING, fetched_at_ms: nowMs2 - 42 * 60_000 },
    recent_sessions: sessionsOf(nowMs2),
    all_time: allTime,
  };
}

function fmtOffset(d: Date): string {
  const mins = -d.getTimezoneOffset();
  const abs = Math.abs(mins);
  return `${mins < 0 ? "-" : "+"}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`;
}

export const FIXTURE = makeFixtureReport();
