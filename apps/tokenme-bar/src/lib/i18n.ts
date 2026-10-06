/**
 * 面板文案的中英双语：跟随系统语言，非英文系统一律回落中文。
 * `?lang=en|zh` 可强制指定（QA / 预览截图用）。
 *
 * 字典是扁平的点分 key；`zh` 是唯一事实源，`en` 用类型钉住同样的 key 集，
 * 漏译在编译期报错。带 `{var}` 占位符的由 `t(key, vars)` 填充。
 */

export type Lang = "zh" | "en";

function detect(): Lang {
  if (typeof location !== "undefined") {
    const q = new URLSearchParams(location.search).get("lang");
    if (q === "en" || q === "zh") return q;
  }
  const sys = (typeof navigator !== "undefined" && navigator.language) || "zh";
  return sys.toLowerCase().startsWith("en") ? "en" : "zh";
}

export const lang: Lang = detect()

// Accessibility and translation tools read the document language; keep it in
// step with the dictionary actually rendering.
document.documentElement.lang = lang === "en" ? "en" : "zh-CN";;

const zh = {
  /* 周期与相对时间 -------------------------------------------------------- */
  "period.day": "今日",
  "period.week": "本周",
  "period.month": "本月",
  "period.year": "今年",
  "period.short.day": "日",
  "period.short.week": "周",
  "period.short.month": "月",
  "period.short.year": "年",
  "prev.day": "昨日同期",
  "prev.week": "上周同期",
  "prev.month": "上月同期",
  "prev.year": "去年同期",
  "rel.now": "刚刚",
  "rel.min": "{n} 分钟前",
  "rel.hour": "{n} 小时前",
  "rel.yesterday": "昨天",
  "rel.day": "{n} 天前",
  "rel.week": "{n} 周前",
  "until.soon": "即将重置",

  /* 托盘显示模式 ---------------------------------------------------------- */
  "tray.tray_only": "仅托盘",
  "tray.tokens_only": "仅Token",
  "tray.cost_only": "仅花费",
  "tray.tray_tokens": "托盘·Token",
  "tray.tray_cost": "托盘·花费",
  "tray.tray_tokens_cost": "托盘·Token·花费",

  /* 顶部 ------------------------------------------------------------------ */
  "hdr.period.a11y": "统计周期",
  "hdr.close": "关闭面板",
  "hdr.credit.title": "含 {c} 由 credits 按官方方案价折算",
  "hdr.credit.included": "≈{c} 已计入",
  "hdr.credit.note": "credits 按厂商公布的方案价折算，非账单",
  "hdr.vs": "较{p}",
  "hdr.sr.money": "{label} {tokens} tokens，花费 {cost}，较{prev} {pct}",
  "hdr.sr.tokens": "{label} {tokens} tokens，较{prev} {pct}",
  "hdr.reqs": "次",
  "hdr.sessions": "会话",
  "hdr.cache": "缓存 {p}",
  "hdr.frozen": "数据已停止更新",
  "hdr.frozen.tip": "引擎超过 {s} 秒没有发布新数据，以上是最后一次快照。点右下角“刷新”可立刻重试。",
  "hdr.sync": "Linux 同步",
  "hdr.sync.tip": "每台机器最后一次成功合并（哈希校验与对账全部通过才会记录）",
  "hdr.sync.rows": "{n} 行",
  "hdr.sync.more": "+{n} 台",

  /* 页签 ------------------------------------------------------------------ */
  "page.overview": "概览",
  "page.overview.hint": "活动热力与配额",
  "page.tools": "工具",
  "page.tools.hint": "各工具的用量与花费",
  "page.ranks": "排行",
  "page.ranks.hint": "模型 / 项目 / MCP / Skill 的排序",
  "page.detail": "明细",
  "page.detail.hint": "最近会话与数据来源",

  /* 活动双视图 ------------------------------------------------------------ */
  "view.today": "今日",
  "view.heat": "活动",
  "view.a11y": "活动视图",
  "heat.weeks": "{n} 周",
  "hours.a11y": "今日按小时消耗",
  "hour.aria": "{h} 时 {t} tokens",
  "read.hour": "{h} 时 · {t} tokens",
  "read.hour.tail": " · {c} · {n} 次",
  "read.day": "今日共 {t} tokens",
  "read.week": "本周共 {t} tokens",
  "read.month": "本月共 {t} tokens",
  "read.year": "今年共 {t} tokens",
  "bars.a11y.week": "本周按日消耗",
  "bars.a11y.month": "本月按日消耗",
  "bars.a11y.year": "今年按月消耗",
  "now.hour": "现在 {h} 时",
  "heat.aria.money": "近 {n} 周活动，共 {t} tokens，花费 {c}",
  "heat.aria.plain": "近 {n} 周活动，共 {t} tokens",
  "read.heat.total": "共 {t} tokens",
  "legend.less": "少",
  "legend.more": "多",
  "unit.times": "次",

  /* 配额 ------------------------------------------------------------------ */
  "quota.section": "配额",
  "quota.pending": "配额探测中…",
  "quota.meta": "{w} 个窗口 · {g} 个工具",
  "quota.legend.low": "安稳",
  "quota.legend.mid": "注意",
  "quota.legend.high": "告急",
  "quota.drag": "长按拖动可调整顺序",
  "quota.win.month": "月窗口",
  "quota.win.day": "{n} 天窗口",
  "quota.win.hour": "{n} 小时窗口",
  "quota.win.min": "{n} 分钟窗口",
  "quota.win.unknown": "窗口未知",
  "quota.origin.probe": "实测",
  "quota.origin.log": "日志",
  "quota.origin.budget": "预算",
  "quota.unlimited": "不限",
  "quota.balance.dsh": "余额",
  "quota.balance.funide": "积分",
  "quota.bar.a11y": "{tool} {label} 已用 {p}%",

  /* 状态栏 ---------------------------------------------------------------- */
  "status.source.cache": "缓存",
  "status.source.bundled": "内置快照",
  "status.source.none": "无价格源",
  "status.prices": "价格",
  "status.events": "事件",
  "status.stale.tip": "价格快照超过 24 小时未更新，成本为估算值",
  "status.stale": "价格已过期",
  "status.update.chip": "新版本 v{v}",
  "status.update.go": "前往下载 {v}",
  "status.tray.tip": "菜单栏显示内容",
  "status.settings": "设置",
  "status.refresh": "刷新",
  "status.refreshing": "刷新中",

  /* 工具页 ---------------------------------------------------------------- */
  "tools.section": "工具",
  "tools.empty": "本期没有用量记录",
  "tools.meta": "{n} 个工具",
  "tools.unpriced": "无价格",
  "tools.credit.priced": "次 · {c}，按方案价折算",
  "tools.credit.free": "次 · credits 计量，不计入美元",
  "tools.sessions": "会话",

  /* 会话 ------------------------------------------------------------------ */
  "sess.section": "最近会话",
  "sess.meta": "显示 {a} / {b}",
  "sess.empty": "还没有会话记录",
  "sess.unpriced": "无价格",
  "sess.credit.tip": "credits 计量，不计入美元",
  "sess.more.all": "查看全部 {n} 个会话",
  "sess.more.recent": "只看最近 6 个会话",

  /* 数据源 ---------------------------------------------------------------- */
  "src.section": "数据源",
  "src.meta": "{d}/{s} 已检测 · {e} 事件",
  "src.rows": "{n} 条",
  "src.missing": "未检测到",
  "src.more.show": "展开 {n} 个未检测到的源",
  "src.more.hide": "收起未检测到的源",
  "src.total": "全部历史",

  /* 排行与调用 ------------------------------------------------------------ */
  "rank.all": "{label} · 全部",
  "rank.meta.all": "{n} 项",
  "rank.meta.more": "另 {n} 项",
  "rank.empty": "本期无数据",
  "rank.unpriced.foot": "{n} 个模型暂无价格：{list}",
  "call.section": "调用来源",
  "call.meta": "{n} 项",
  "call.a11y": "调用类型",
  "call.empty.mcp": "本期没有 MCP 调用",
  "call.empty.skill": "本期没有 Skill 调用",
  "call.panel.mcp": "MCP 调用",
  "call.panel.skill": "Skill 调用",

  /* 排序与更多 ------------------------------------------------------------ */
  "sort.default": "默认",
  "sort.asc": "升序",
  "sort.desc": "降序",
  "sort.title.default": "默认排序",
  "sort.title.asc": "按用量升序",
  "sort.title.desc": "按用量降序",
  "sort.a11y": "排序：{l}",
  "more.close": "查看全部 {n} 项",
  "more.open": "只看前 {n} 项",

  /* 空态与启动 ------------------------------------------------------------ */
  "empty.title": "未检测到任何数据源",
  "empty.body":
    "tokenme 只读本机已有的 AI 命令行日志并统计用量计数。安装下列任意一个并运行一次会话后，点右下角刷新即可看到数据。",
  "empty.cta": "重新扫描",
  "boot.indexing": "正在索引本机用量…",
  "boot.failed": "读取失败：{e}",
  "boot.retry": "重试",
  "loading.default": "加载中",

  /* 设置面板 -------------------------------------------------------------- */
  "set.title": "设置",
  "set.close.a11y": "关闭设置",
  "set.refresh": "自动刷新",
  "set.refresh.hint": "文件变化时仍会立即刷新",
  "set.interval.a11y": "自动刷新间隔",
  "set.15s": "15 秒",
  "set.30s": "30 秒",
  "set.1m": "1 分钟",
  "set.5m": "5 分钟",
  "set.bubble": "边缘悬浮水滴",
  "set.bubble.hint": "显示今日 Token 数，靠近屏幕边缘时自动吸附",
  "set.theme": "外观",
  "set.theme.a11y": "外观主题",
  "set.theme.system": "跟随系统",
  "set.theme.light": "浅色",
  "set.theme.dark": "深色",
  "set.money": "金额折算",
  "set.money.hint": "关闭后只显示 tokens 与 credits",
  "set.showempty": "显示零会话工具",
  "set.showempty.hint": "开启后，本期没有会话的工具也会列在工具页",
  "set.pause": "退出后暂停配额",
  "set.pause.hint": "工具退出后停止探测其配额，保留最后数值",
  "set.autoupdate": "自动检查更新",
  "set.autoupdate.hint": "开启后启动时和每天各静默检查一次，发现新版本在底部提示",
  "set.autostart": "开机自动启动",
  "set.contact": "联系我们",
  "set.contact.hint": "问题反馈 · 版本发布 · 交流群见 README",
  "set.mail": "发邮件",
  "set.logs": "日志",
  "set.logs.tip": "崩溃与异常都记录在这里，反馈问题时附上",
  "set.quit": "退出程序",
  "set.quit.hint": "关闭面板并退出托盘进程",
  "set.quit.btn": "退出",
  "set.dl": "下载中 {p}%",
  "set.busy": "处理中…",
  "set.download": "下载并安装",
  "set.reboot": "重启到新版本",
  "update.dev.none": "dev 无更新",

  /* 悬浮球 ---------------------------------------------------------------- */
  "bubble.today": "今日",
  "bubble.aria": "今日 {t} tokens",

  /* 页面区块标题（App 直传的 Section label） ------------------------------ */
  "sec.models": "模型",
  "sec.projects": "项目",
};

export type StrKey = keyof typeof zh;

const en: Record<StrKey, string> = {
  "period.day": "Today",
  "period.week": "This week",
  "period.month": "This month",
  "period.year": "This year",
  "period.short.day": "D",
  "period.short.week": "W",
  "period.short.month": "M",
  "period.short.year": "Y",
  "prev.day": "yesterday",
  "prev.week": "last week",
  "prev.month": "last month",
  "prev.year": "last year",
  "rel.now": "just now",
  "rel.min": "{n} min ago",
  "rel.hour": "{n} h ago",
  "rel.yesterday": "yesterday",
  "rel.day": "{n} d ago",
  "rel.week": "{n} w ago",
  "until.soon": "resets soon",

  "tray.tray_only": "Tray only",
  "tray.tokens_only": "Tokens",
  "tray.cost_only": "Cost",
  "tray.tray_tokens": "Tray·Tokens",
  "tray.tray_cost": "Tray·Cost",
  "tray.tray_tokens_cost": "Tray·Tokens·Cost",

  "hdr.period.a11y": "Usage period",
  "hdr.close": "Close panel",
  "hdr.credit.title": "includes {c} converted from credits at plan price",
  "hdr.credit.included": "≈{c} included",
  "hdr.credit.note": "credits converted at the vendor's published plan price — not a bill",
  "hdr.vs": "vs {p}",
  "hdr.sr.money": "{label}: {tokens} tokens, {cost}, {pct} vs {prev}",
  "hdr.sr.tokens": "{label}: {tokens} tokens, {pct} vs {prev}",
  "hdr.reqs": "reqs",
  "hdr.sessions": "sessions",
  "hdr.cache": "Cache {p}",
  "hdr.frozen": "Updates stopped",
  "hdr.frozen.tip": "The engine has not published for over {s} s — these are the last known numbers. Refresh retries now.",
  "hdr.sync": "Linux sync",
  "hdr.sync.tip": "Newest successful merge from each machine (recorded only after the hash check and reconciliation pass)",
  "hdr.sync.rows": "{n} rows",
  "hdr.sync.more": "+{n} more",

  "page.overview": "Overview",
  "page.overview.hint": "Activity heat & quotas",
  "page.tools": "Tools",
  "page.tools.hint": "Usage and cost by tool",
  "page.ranks": "Rankings",
  "page.ranks.hint": "Top models, projects, MCP, skills",
  "page.detail": "Details",
  "page.detail.hint": "Sessions & data sources",

  "view.today": "Today",
  "view.heat": "Activity",
  "view.a11y": "Activity view",
  "heat.weeks": "{n} w",
  "hours.a11y": "Today by hour",
  "hour.aria": "{h}:00 · {t} tokens",
  "read.hour": "{h}:00 · {t} tokens",
  "read.hour.tail": " · {c} · {n} reqs",
  "read.day": "Today · {t} tokens",
  "read.week": "This week · {t} tokens",
  "read.month": "This month · {t} tokens",
  "read.year": "This year · {t} tokens",
  "bars.a11y.week": "This week by day",
  "bars.a11y.month": "This month by day",
  "bars.a11y.year": "This year by month",
  "now.hour": "Now {h}:00",
  "heat.aria.money": "{n}-week activity · {t} tokens · {c}",
  "heat.aria.plain": "{n}-week activity · {t} tokens",
  "read.heat.total": "Total {t} tokens",
  "legend.less": "Less",
  "legend.more": "More",
  "unit.times": "reqs",

  "quota.section": "Quotas",
  "quota.pending": "Probing quotas…",
  "quota.meta": "{w} windows · {g} tools",
  "quota.legend.low": "OK",
  "quota.legend.mid": "Watch",
  "quota.legend.high": "Critical",
  "quota.drag": "Long-press & drag to reorder",
  "quota.win.month": "Month window",
  "quota.win.day": "{n}-day window",
  "quota.win.hour": "{n}-hour window",
  "quota.win.min": "{n}-min window",
  "quota.win.unknown": "Unknown window",
  "quota.origin.probe": "Probe",
  "quota.origin.log": "Log",
  "quota.origin.budget": "Budget",
  "quota.unlimited": "No cap",
  "quota.balance.dsh": "Balance",
  "quota.balance.funide": "Points",
  "quota.bar.a11y": "{tool} {label}: {p}% used",

  "status.source.cache": "Cache",
  "status.source.bundled": "Bundled",
  "status.source.none": "No prices",
  "status.prices": "prices",
  "status.events": "events",
  "status.stale.tip": "Price snapshot older than 24 h — costs are estimates",
  "status.stale": "Prices stale",
  "status.update.chip": "New v{v}",
  "status.update.go": "Get {v}",
  "status.tray.tip": "Menu bar display",
  "status.settings": "Settings",
  "status.refresh": "Refresh",
  "status.refreshing": "Refreshing",

  "tools.section": "Tools",
  "tools.empty": "No usage this period",
  "tools.meta": "{n} tools",
  "tools.unpriced": "No price",
  "tools.credit.priced": "reqs · {c} at plan price",
  "tools.credit.free": "reqs · credit-metered, not in USD",
  "tools.sessions": "sessions",

  "sess.section": "Recent sessions",
  "sess.meta": "{a} / {b}",
  "sess.empty": "No sessions yet",
  "sess.unpriced": "No price",
  "sess.credit.tip": "credit-metered, not in USD",
  "sess.more.all": "Show all {n} sessions",
  "sess.more.recent": "Recent 6 only",

  "src.section": "Data sources",
  "src.meta": "{d}/{s} detected · {e} events",
  "src.rows": "{n}",
  "src.missing": "Not found",
  "src.more.show": "Show {n} undetected sources",
  "src.more.hide": "Hide undetected sources",
  "src.total": "All time",

  "rank.all": "{label} · All",
  "rank.meta.all": "{n} items",
  "rank.meta.more": "+{n} more",
  "rank.empty": "No data this period",
  "rank.unpriced.foot": "{n} models without prices: {list}",
  "call.section": "Call sources",
  "call.meta": "{n} items",
  "call.a11y": "Call type",
  "call.empty.mcp": "No MCP calls this period",
  "call.empty.skill": "No Skill calls this period",
  "call.panel.mcp": "MCP calls",
  "call.panel.skill": "Skill calls",

  "sort.default": "Default",
  "sort.asc": "Asc",
  "sort.desc": "Desc",
  "sort.title.default": "Default order",
  "sort.title.asc": "Usage, ascending",
  "sort.title.desc": "Usage, descending",
  "sort.a11y": "Sort: {l}",
  "more.close": "Show all {n}",
  "more.open": "First {n} only",

  "empty.title": "No data sources detected",
  "empty.body":
    "tokenme only reads the AI CLI logs already on this machine and counts usage. Install any tool below, run one session, then hit rescan.",
  "empty.cta": "Rescan",
  "boot.indexing": "Indexing local usage…",
  "boot.failed": "Failed to read: {e}",
  "boot.retry": "Retry",
  "loading.default": "Loading",

  "set.title": "Settings",
  "set.close.a11y": "Close settings",
  "set.refresh": "Auto refresh",
  "set.refresh.hint": "Still refreshes the moment files change",
  "set.interval.a11y": "Refresh interval",
  "set.15s": "15 s",
  "set.30s": "30 s",
  "set.1m": "1 min",
  "set.5m": "5 min",
  "set.bubble": "Edge bubble",
  "set.bubble.hint": "Shows today's tokens; docks to the screen edge",
  "set.theme": "Appearance",
  "set.theme.a11y": "Theme",
  "set.theme.system": "System",
  "set.theme.light": "Light",
  "set.theme.dark": "Dark",
  "set.money": "Cost in USD",
  "set.money.hint": "Off shows tokens and credits only",
  "set.showempty": "Show zero-session tools",
  "set.showempty.hint": "On, tools without sessions this period stay listed",
  "set.pause": "Pause when tool exits",
  "set.pause.hint": "Stops probing a tool's quotas after it exits, keeping the last values",
  "set.autoupdate": "Auto update check",
  "set.autoupdate.hint": "Checks quietly at boot and daily; a new version appears at the bottom",
  "set.autostart": "Launch at login",
  "set.contact": "Contact",
  "set.contact.hint": "Feedback · releases · community — see the README",
  "set.mail": "Email",
  "set.logs": "Logs",
  "set.logs.tip": "Crashes and errors land here — attach them when reporting",
  "set.quit": "Quit",
  "set.quit.hint": "Closes the panel and exits the tray process",
  "set.quit.btn": "Quit",
  "set.dl": "Downloading {p}%",
  "set.busy": "Working…",
  "set.download": "Download & install",
  "set.reboot": "Restart to update",
  "update.dev.none": "dev: no updates",

  "bubble.today": "Today",
  "bubble.aria": "Today {t} tokens",

  "sec.models": "Models",
  "sec.projects": "Projects",
};

const DICTS: Record<Lang, Record<StrKey, string>> = { zh, en };

/** 取文案；`{var}` 占位符按传入的 vars 填充。 */
export function t(key: StrKey, vars?: Record<string, string | number>): string {
  const s = DICTS[lang][key];
  if (!vars) return s;
  return s.replace(/\{(\w+)\}/g, (_, name: string) => String(vars[name] ?? `{${name}}`));
}

/** 月份短名：热力图的月份刻度（zh "9月"，en "Sep"）。 */
const MONTHS_EN = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
export function monthLabel(month: number): string {
  return lang === "en" ? MONTHS_EN[month - 1] ?? String(month) : `${month}月`;
}

/** 周一锚定的星期短名：周期联动柱状图的 7 根轴标（zh 单字 "一"，en 三字母 "Mon"）。 */
const DAYS_EN = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const DAYS_ZH = "一二三四五六日";
export function weekdayLabel(mondayIndex: number): string {
  return lang === "en" ? DAYS_EN[mondayIndex] ?? "" : DAYS_ZH[mondayIndex] ?? "";
}

/** 系统语言代码（Rust 侧托盘/更新文案用同一份语言判定）。 */
export function systemLang(): Lang {
  return lang;
}
