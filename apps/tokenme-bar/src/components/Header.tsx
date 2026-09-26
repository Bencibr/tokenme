import { useCallback } from "react";
import type { PageKey, PeriodKey, Report } from "../types";
import {
  PERIOD_PREV,
  PERIOD_SHORT,
  compactTokens,
  credits as creditText,
  deltaDirection,
  money,
  percent,
  count,
  signedPercent,
  splitTokens,
} from "../lib/format";
import { useTweenNumber } from "../lib/hooks";
import { useMoney } from "../lib/display";
import { IconArrowDown, IconArrowUp, IconClose, IconFlat } from "./Icons";

const ORDER: PeriodKey[] = ["day", "week", "month", "year"];

const PAGES: { key: PageKey; label: string; hint: string }[] = [
  { key: "overview", label: "概览", hint: "活动热力与配额" },
  { key: "tools", label: "工具", hint: "各工具的用量与花费" },
  { key: "ranks", label: "排行", hint: "模型 / 项目 / MCP / Skill 的排序" },
  { key: "detail", label: "明细", hint: "最近会话与数据来源" },
];

interface Props {
  report: Report;
  period: PeriodKey;
  onPeriod: (period: PeriodKey) => void;
  page: PageKey;
  onPage: (page: PageKey) => void;
  onClose?: () => void;
}

function DeltaChip({ pct, caption }: { pct: number; caption: string }) {
  const dir = deltaDirection(pct);
  const Glyph = dir === "up" ? IconArrowUp : dir === "down" ? IconArrowDown : IconFlat;
  return (
    <span className="chip" data-dir={dir} title={caption}>
      <Glyph size={11} />
      <span className="chip-v">{signedPercent(pct)}</span>
      <span className="chip-cap">{caption}</span>
    </span>
  );
}

export function Header({ report, period, onPeriod, page, onPage, onClose }: Props) {
  const showMoney = useMoney();
  const win = report[period];
  const { summary } = win;
  const hero = splitTokens(useTweenNumber(summary.total_tokens));
  const prevLabel = PERIOD_PREV[period];

  const onKey = useCallback(
    (e: React.KeyboardEvent) => {
      const delta = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (!delta) return;
      e.preventDefault();
      const i = (ORDER.indexOf(period) + delta + ORDER.length) % ORDER.length;
      onPeriod(ORDER[i]);
    },
    [onPeriod, period],
  );

  return (
    <header className="hdr">
      <div className="hdr-bar">
        <span className="brand">TokenMe</span>
        <div className="hdr-actions">
          <div className="seg" role="radiogroup" aria-label="统计周期" onKeyDown={onKey}>
            {ORDER.map((key) => (
              <button
                key={key}
                type="button"
                role="radio"
                className="seg-btn"
                aria-checked={key === period}
                tabIndex={key === period ? 0 : -1}
                onClick={() => onPeriod(key)}
              >
                {PERIOD_SHORT[key]}
              </button>
            ))}
          </div>
          {onClose ? (
            <button type="button" className="panel-close" onClick={onClose} title="关闭面板" aria-label="关闭面板">
              <IconClose size={13} />
            </button>
          ) : null}
        </div>
      </div>

      <div className="hero" aria-live="polite">
        <div className="hero-main">
          <span className="hero-value" aria-hidden="true">
            {hero.value}
          </span>
          <span className="hero-unit" aria-hidden="true">
            {hero.unit}
          </span>
          <span className="hero-unit-word">tokens</span>
        </div>
        <span className="sr">
          {showMoney
            ? `${win.label} ${compactTokens(summary.total_tokens)} tokens，花费 ${money(summary.cost)}，较${prevLabel} ${signedPercent(win.delta_cost_pct)}`
            : `${win.label} ${compactTokens(summary.total_tokens)} tokens，较${prevLabel} ${signedPercent(win.delta_tokens_pct)}`}
        </span>
        <div className="hero-sub">
          {showMoney ? (
            <span
              className="hero-cost"
              title={summary.credit_cost > 0 ? `含 ${money(summary.credit_cost)} 由 credits 按官方方案价折算` : undefined}
            >
              {summary.credit_cost > 0 ? "≈" : ""}
              {money(summary.cost)}
            </span>
          ) : null}
          <DeltaChip pct={showMoney ? win.delta_cost_pct : win.delta_tokens_pct} caption={prevLabel} />
        </div>
      </div>

      <div className="hdr-stats">
        <span>{win.label}</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(summary.requests)}</span>
        <span>次</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(summary.sessions)}</span>
        <span>会话</span>
        <span className="dot-sep" aria-hidden="true" />
        <span>缓存 {percent(summary.cached_pct, 1)}</span>
        {summary.credits > 0 ? (
          <>
            <span className="dot-sep" aria-hidden="true" />
            <span>{creditText(summary.credits)}</span>
            {showMoney && summary.credit_cost > 0 ? (
              <span className="hint" title="credits 按厂商公布的方案价折算，非账单">
                ≈{money(summary.credit_cost)} 已计入
              </span>
            ) : null}
          </>
        ) : null}
      </div>
      <div className="page-tabs" role="tablist" aria-label="页面">
        {PAGES.map((p, i) => (
          <button
            key={p.key}
            type="button"
            role="tab"
            className="page-tab"
            aria-selected={page === p.key}
            tabIndex={page === p.key ? 0 : -1}
            title={p.hint}
            onClick={() => onPage(p.key)}
            onKeyDown={(e) => {
              const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
              if (!d) return;
              e.preventDefault();
              onPage(PAGES[(i + d + PAGES.length) % PAGES.length].key);
            }}
          >
            {p.label}
          </button>
        ))}
      </div>
    </header>
  );
}
