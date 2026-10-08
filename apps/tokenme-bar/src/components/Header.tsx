import { useCallback } from "react";
import type { MachineScope, PageKey, PeriodKey, Report } from "../types";
import {
  PERIOD_LABEL,
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
import { t } from "../lib/i18n";
import { IconArrowDown, IconArrowUp, IconClose, IconFlat, IconRefresh } from "./Icons";
import { ScopeDropdown } from "./ScopeDropdown";

const ORDER: PeriodKey[] = ["day", "week", "month", "year"];

const PAGES: { key: PageKey; label: () => string; hint: () => string }[] = [
  { key: "overview", label: () => t("page.overview"), hint: () => t("page.overview.hint") },
  { key: "tools", label: () => t("page.tools"), hint: () => t("page.tools.hint") },
  { key: "ranks", label: () => t("page.ranks"), hint: () => t("page.ranks.hint") },
  { key: "detail", label: () => t("page.detail"), hint: () => t("page.detail.hint") },
];

interface Props {
  report: Report;
  period: PeriodKey;
  onPeriod: (period: PeriodKey) => void;
  page: PageKey;
  onPage: (page: PageKey) => void;
  onClose?: () => void;
  /** Machine-sync health: the newest bundle merged from each origin machine.
   *  Null when no bundle was ever imported (most users). */
  sync?: {
    latest: { origin: string; rows: number };
    age: string;
    more: number;
    /** Warn hue: the newest merge is not from today. */
    stale: boolean;
    /** Nothing merged today → the badge says so instead of yesterday's rows. */
    mergedToday: boolean;
    rows: { origin: string; file: string; rows: number; window: string; age: string }[];
  } | null;
  /** The scope the current report was folded under (its echoed value). */
  scope: MachineScope;
  onScope: (scope: MachineScope) => void;
  /** The scope menu's open state lives in App so Escape can order
   *  menu → settings sheet → panel. */
  scopeMenuOpen: boolean;
  onScopeMenuOpen: (open: boolean) => void;
  /** Origins backed by a configured server. The scope menu tags every other
   *  imported origin as a manual export/import. Empty = no tags at all. */
  serverOrigins?: Set<string>;
  /** The shared 30 s tick, so the menu's sync ages stay honest. */
  now: number;
}

function DeltaChip({ pct, caption }: { pct: number; caption: string }) {
  const dir = deltaDirection(pct);
  const Glyph = dir === "up" ? IconArrowUp : dir === "down" ? IconArrowDown : IconFlat;
  // Naked text on the hero's baseline, glued to the figure it qualifies — a
  // boxed chip floating beside the cost read as an unrelated second row.
  return (
    <span className="chip" data-dir={dir} title={caption}>
      <Glyph size={11} />
      <span className="chip-v num">{signedPercent(pct)}</span>
      <span className="chip-cap">{t("hdr.vs", { p: caption })}</span>
    </span>
  );
}

/** One line per origin for the badge tooltip: window and file included so a
 *  wrong or half-transported bundle is identifiable without opening the folder. */
function syncTip(sync: NonNullable<Props["sync"]>): string {
  const lines = [sync.mergedToday ? t("hdr.sync.tip") : t("hdr.sync.idle.tip")];
  for (const r of sync.rows) {
    lines.push(
      `${r.origin} · ${r.age} · ${t("hdr.sync.rows", { n: count(r.rows) })} · ${r.window} · ${r.file}`,
    );
  }
  return lines.join("\n");
}

export function Header({ report, period, onPeriod, page, onPage, onClose, sync, scope, onScope, scopeMenuOpen, onScopeMenuOpen, serverOrigins, now }: Props) {
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
          <div className="seg" role="radiogroup" aria-label={t("hdr.period.a11y")} onKeyDown={onKey}>
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
            <button type="button" className="panel-close" onClick={onClose} title={t("hdr.close")} aria-label={t("hdr.close")}>
              <IconClose size={13} />
            </button>
          ) : null}
        </div>
      </div>

      <div className="hero" aria-live="polite">
        {/* the delta lives on the big number's baseline, right-aligned — a
            figure and its change read as one line, not two stacked rows; when
            the line is too tight the side wraps below, still right-aligned */}
        <div className="hero-main">
          <span className="hero-value" aria-hidden="true">
            {hero.value}
          </span>
          <span className="hero-unit" aria-hidden="true">
            {hero.unit}
          </span>
          <span className="hero-unit-word">tokens</span>
          <span className="hero-side">
            {showMoney ? (
              <span
                className="hero-cost"
                title={summary.credit_cost > 0 ? t("hdr.credit.title", { c: money(summary.credit_cost) }) : undefined}
              >
                {summary.credit_cost > 0 ? "≈" : ""}
                {money(summary.cost)}
              </span>
            ) : null}
            <DeltaChip pct={showMoney ? win.delta_cost_pct : win.delta_tokens_pct} caption={prevLabel} />
          </span>
        </div>
        <span className="sr">
          {showMoney
            ? t("hdr.sr.money", {
                label: PERIOD_LABEL[period],
                tokens: compactTokens(summary.total_tokens),
                cost: money(summary.cost),
                prev: prevLabel,
                pct: signedPercent(win.delta_cost_pct),
              })
            : t("hdr.sr.tokens", {
                label: PERIOD_LABEL[period],
                tokens: compactTokens(summary.total_tokens),
                prev: prevLabel,
                pct: signedPercent(win.delta_tokens_pct),
              })}
        </span>
      </div>

      <div className="hdr-stats">
        <span>{PERIOD_LABEL[period]}</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(summary.requests)}</span>
        <span>{t("hdr.reqs")}</span>
        <span className="dot-sep" aria-hidden="true" />
        <span className="num">{count(summary.sessions)}</span>
        <span>{t("hdr.sessions")}</span>
        <span className="dot-sep" aria-hidden="true" />
        <span>{t("hdr.cache", { p: percent(summary.cached_pct, 1) })}</span>
        {summary.credits > 0 ? (
          <>
            <span className="dot-sep" aria-hidden="true" />
            <span>{creditText(summary.credits)}</span>
            {showMoney && summary.credit_cost > 0 ? (
              <span className="hint" title={t("hdr.credit.note")}>
                {t("hdr.credit.included", { c: money(summary.credit_cost) })}
              </span>
            ) : null}
          </>
        ) : null}
      </div>
      {/* Its own line, not another item in the stats: the badge names a machine,
          how fresh its merge is and how many rows came in, so it wraps to a
          second row of numbers all by itself — and it is silent until a bundle
          has actually been imported. */}
      {sync ? (
        <div className="hdr-sync">
          <span className="sync" data-stale={sync.stale || undefined} title={syncTip(sync)}>
            <IconRefresh size={11} />
            {t("hdr.sync")}
            <span className="dot-sep" aria-hidden="true" />
            {sync.mergedToday ? (
              <>
                <span>{sync.latest.origin}</span>
                <span className="dot-sep" aria-hidden="true" />
                <span>{sync.age}</span>
                <span className="dot-sep" aria-hidden="true" />
                <span className="num">{t("hdr.sync.rows", { n: count(sync.latest.rows) })}</span>
                {sync.more > 0 ? <span className="sync-more">{t("hdr.sync.more", { n: sync.more })}</span> : null}
              </>
            ) : (
              <span>{t("hdr.sync.idle")}</span>
            )}
          </span>
        </div>
      ) : null}
      {/* The scope dropdown shares the tabs' line and hugs the far right; with
          no imported origins it renders nothing and the row is unchanged. */}
      <div className="tabs-row">
        <div className="page-tabs" role="tablist" aria-label={t("page.overview")}>
          {PAGES.map((p, i) => (
            <button
              key={p.key}
              type="button"
              role="tab"
              className="page-tab"
              aria-selected={page === p.key}
              tabIndex={page === p.key ? 0 : -1}
              title={p.hint()}
              onClick={() => onPage(p.key)}
              onKeyDown={(e) => {
                const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
                if (!d) return;
                e.preventDefault();
                onPage(PAGES[(i + d + PAGES.length) % PAGES.length].key);
              }}
            >
              {p.label()}
            </button>
          ))}
        </div>
        <ScopeDropdown
          scope={scope}
          onScope={onScope}
          machines={report.machines ?? []}
          syncs={report.syncs}
          now={now}
          open={scopeMenuOpen}
          onOpen={onScopeMenuOpen}
          serverOrigins={serverOrigins}
        />
      </div>
    </header>
  );
}
