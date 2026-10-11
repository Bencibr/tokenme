import { useEffect, useMemo, useRef, useState } from "react";
import type { HeatCell, HourCell, PeriodKey } from "../types";
import { compactTokens, count, formatDateLabel, money } from "../lib/format";
import { useMoney } from "../lib/display";
import { monthLabel, systemLang, t, weekdayLabel, type StrKey } from "../lib/i18n";
import { Section } from "./Section";

const HOUR_SLOTS = 24;
const LEVELS = [1, 2, 3, 4];
/* The heat grid fills the same box the hours chart uses, at GitHub's
 * contribution-calendar proportions. GitHub itself lays a fixed 10×10 cell on
 * a 3px table pitch — 56 square columns, forever — and just clips the overflow
 * on narrow screens. Our columns follow the season (Jan 1 → today), so the
 * pitch is width ÷ columns and every cell scales with it: squares grow through
 * spring and shrink as December packs 53 columns in. The 13px cap is GitHub's
 * own pitch, so a young year tops out at GitHub-sized squares instead of
 * ballooning into slabs. */
const CONTENT_W = 366;
const GUTTER_W = 28;
const GRID_W = CONTENT_W - GUTTER_W;
const BODY_H = 92;
/* .heat-months' height; the gutter's row labels sit below it. Keep in step. */
const MONTHS_ROW_H = 14;
const PITCH_CAP = 13;
const CELL_RATIO = 10 / 13;
const PITCH_Y = (BODY_H - MONTHS_ROW_H) / 7;

const VIEWS = ["hours", "heat"] as const;
type ViewMode = (typeof VIEWS)[number];
const VIEW_HEAT = () => t("view.heat");
/* The bars view's tab reads with the top period: 今日 / 本周 / 本月 / 今年. */
const PERIOD_VIEW: Record<PeriodKey, () => string> = {
  day: () => t("view.today"),
  week: () => t("period.week"),
  month: () => t("period.month"),
  year: () => t("period.year"),
};
/* The back page's tab reads one unit up the same ladder. */
const PERIOD_PREV: Record<PeriodKey, () => string> = {
  day: () => t("view.yesterday"),
  week: () => t("period.pweek"),
  month: () => t("period.pmonth"),
  year: () => t("period.pyear"),
};
/* Swipe hints per period, both directions. */
const SWIPE_PREV: Record<PeriodKey, StrKey> = {
  day: "hours.swipe.prev",
  week: "hours.swipe.prev.week",
  month: "hours.swipe.prev.month",
  year: "hours.swipe.prev.year",
};
const SWIPE_CUR: Record<PeriodKey, StrKey> = {
  day: "hours.swipe.today",
  week: "hours.swipe.cur.week",
  month: "hours.swipe.cur.month",
  year: "hours.swipe.cur.year",
};
/* The bar pages' chart labels; the day pages have their own hours.* pair. */
const BARS_A11Y: Record<Exclude<PeriodKey, "day">, StrKey> = {
  week: "bars.a11y.week",
  month: "bars.a11y.month",
  year: "bars.a11y.year",
};
const BARS_A11Y_PREV: Record<Exclude<PeriodKey, "day">, StrKey> = {
  week: "bars.a11y.pweek",
  month: "bars.a11y.pmonth",
  year: "bars.a11y.pyear",
};

/** A month anchored to its year: the back page's "12月" must not read as this
 * year's December. zh glues the year in front, en appends it. */
const yearMonthLabel = (y: number, m: number) =>
  systemLang() === "en" ? `${monthLabel(m)} ${y}` : `${y}年${monthLabel(m)}`;

const pad2 = (n: number) => String(n).padStart(2, "0");

/** One bar of the week/month/year views, derived from the heatmap's daily
 * cells — never from the period summary. The readout sums exactly these bars,
 * so chart and total agree even minutes after a publish (the hours view's
 * dayTotals contract, extended). */
interface Bar {
  tokens: number;
  cost: number;
  requests: number;
  future: boolean;
  now: boolean;
  axis: string;
  hover: string;
}

const isoDate = (d: Date) => `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;

/** Monday-anchored columns over the calendar year — Jan 1 through today, the
 * GitHub pitch: months run across, weekdays stack down the rows. Days before
 * Jan 1 stay blank slots in the first column. The engine's 742-day window
 * always covers the year start; a day it somehow lacks synthesizes as an
 * empty cell, never a missing column. */
function toWeeks(cells: HeatCell[], today: string): (HeatCell | null)[][] {
  if (cells.length === 0) return [];
  const year = today.slice(0, 4);
  const byDate = new Map(
    cells
      .filter((c) => c.date >= `${year}-01-01` && c.date <= today)
      .map((c) => [c.date, c]),
  );
  const start = new Date(`${year}-01-01T00:00:00`);
  const end = new Date(`${today}T00:00:00`);
  const cols: (HeatCell | null)[][] = [];
  let col: (HeatCell | null)[] = Array.from({ length: (start.getDay() + 6) % 7 }, () => null);
  for (const d = start; d <= end; d.setDate(d.getDate() + 1)) {
    const date = isoDate(d);
    col.push(byDate.get(date) ?? { date, total_tokens: 0, cost: 0, requests: 0 });
    if (col.length === 7) {
      cols.push(col);
      col = [];
    }
  }
  if (col.length > 0) cols.push(col);
  return cols;
}

/** Levels are quantiles of *active* days, so a quiet month still reads as quiet. */
function levelOf(value: number, thresholds: number[]): number {
  if (value <= 0) return 0;
  let level = 1;
  for (const th of thresholds) if (value > th) level += 1;
  return Math.min(level, 4);
}

function monthMarkers(weeks: (HeatCell | null)[][]) {
  const out: Array<{ index: number; label: string }> = [];
  weeks.forEach((week, index) => {
    // GitHub pins the label to the column that contains the 1st, not to the
    // next Monday; a column can hold at most one month's 1st, so each month
    // marks at most once. 4 clear columns still keep "10月" and "11月" apart.
    const firstOf = week.find((c) => c !== null && Number(c.date.slice(8, 10)) === 1);
    if (!firstOf) return;
    if (!out.length || index - out[out.length - 1].index >= 4) {
      out.push({ index, label: monthLabel(Number(firstOf.date.slice(5, 7))) });
    }
  });
  return out;
}

export function Heatmap({ cells = [], today, hours = [], hoursPrev, period }: { cells: HeatCell[]; today: string; hours?: HourCell[]; hoursPrev?: HourCell[]; period: PeriodKey }) {
  const [hover, setHover] = useState<HeatCell | null>(null);
  const [hoverHour, setHoverHour] = useState<number | null>(null);
  const [hoverBar, setHoverBar] = useState<Bar | null>(null);
  // An old snapshot reports no hourly vec; the section then stays heat-only.
  const [mode, setMode] = useState<ViewMode>(() => (hours.length === HOUR_SLOTS ? "hours" : "heat"));
  // The pager's swipe history: 0 = the current period, 1 = one unit back —
  // yesterday / last week / last month / last year, never further (older
  // history belongs to the heat grid). The day page needs the engine's
  // `hourly_prev`; the other periods page over the daily cells, so they
  // always have a back page. An old snapshot hides the day swipe entirely.
  const [page, setPage] = useState(0);
  const pagerRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<{ id: number; x0: number; y0: number; dx: number; moved: boolean } | null>(null);
  const [nudge, setNudge] = useState(0);
  const [dragging, setDragging] = useState(false);
  const showMoney = useMoney();

  const hasPrev = (hoursPrev?.length ?? 0) === HOUR_SLOTS;
  const pager = period === "day" ? hasPrev : true;
  const off = pager ? Math.min(page, 1) : 0;
  // A period switch lands back on the current page: yesterday is a day-chart
  // state, not a standing choice the week view should inherit. The hover
  // follows the same rule — the index belongs to whichever page is on screen.
  useEffect(() => setPage(0), [period]);
  useEffect(() => {
    setHoverHour(null);
    setHoverBar(null);
  }, [off]);

  // Trackpad / tilt-wheel paging on the chart. Two fingers right — the
  // browser-back gesture, deltaX < 0 under natural scrolling — opens the back
  // page; left returns to the current one. A native non-passive listener:
  // React's wheel delegate cannot preventDefault, and an unprevented
  // horizontal wheel scrolls the page instead of paging.
  useEffect(() => {
    const el = pagerRef.current;
    if (!el || !pager) return;
    let acc = 0;
    let idle = 0;
    const onWheel = (e: WheelEvent) => {
      if (Math.abs(e.deltaX) <= Math.abs(e.deltaY)) return;
      e.preventDefault();
      acc += e.deltaX;
      window.clearTimeout(idle);
      idle = window.setTimeout(() => (acc = 0), 180);
      if (acc > 40) {
        acc = 0;
        setPage(0);
      } else if (acc < -40) {
        acc = 0;
        setPage(1);
      }
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      window.clearTimeout(idle);
    };
  }, [pager]);

  const { weeks, thresholds, totals } = useMemo(() => {
    const grid = toWeeks(cells, today);
    const flat = grid.flat().filter((c): c is HeatCell => c !== null);
    const active = flat
      .map((c) => c.total_tokens)
      .filter((v) => v > 0)
      .sort((a, b) => a - b);
    const at = (q: number) => (active.length === 0 ? 0 : active[Math.floor((active.length - 1) * q)]);
    return {
      weeks: grid,
      thresholds: [at(0.5), at(0.78), at(0.94)],
      totals: flat.reduce((acc, c) => ({ tokens: acc.tokens + c.total_tokens, cost: acc.cost + c.cost }), {
        tokens: 0,
        cost: 0,
      }),
    };
  }, [cells, today]);

  if (weeks.length === 0) return null;

  const hasHours = hours.length === HOUR_SLOTS;
  // The viewed day's own slots: the readout sums exactly the bars on screen,
  // and each day scales to its own peak — a quiet yesterday still reads.
  const dayHours = off === 0 || !hoursPrev || hoursPrev.length !== HOUR_SLOTS ? hours : hoursPrev;
  const viewDate = (() => {
    if (off === 0) return today;
    const d = new Date(`${today}T00:00:00`);
    d.setDate(d.getDate() - 1);
    return isoDate(d);
  })();
  const dayTotals = dayHours.reduce(
    (acc, h) => ({ tokens: acc.tokens + h.total_tokens, cost: acc.cost + h.cost }),
    { tokens: 0, cost: 0 },
  );
  const maxDay = Math.max(...dayHours.map((h) => h.total_tokens), 1);
  const nowHour = new Date().getHours();

  const dayColumns = (list: HourCell[], isToday: boolean) =>
    list.map((h, i) => (
      <div
        key={i}
        className="hb-col"
        role="img"
        aria-label={t("hour.aria", { h: pad2(i), t: compactTokens(h.total_tokens) })}
        data-now={(isToday && i === nowHour) || undefined}
        data-future={(isToday && i > nowHour) || undefined}
        onMouseEnter={() => setHoverHour(i)}
      >
        <div className="hb-barw"><span className="hb-bar" style={{ height: h.total_tokens > 0 ? `${Math.max(4, Math.round((h.total_tokens / maxDay) * 100))}%` : 0 }} /></div>
        <span className="hb-lab">{i % 3 === 0 ? pad2(i) : ""}</span>
      </div>
    ));

  // The pager's drag: pointer-follow with a rubber band past either end, and
  // a horizontal bias — a mostly-vertical gesture scrolls the page as usual.
  const onDragStart = (e: React.PointerEvent<HTMLDivElement>) => {
    if (dragRef.current || (e.pointerType === "mouse" && e.button !== 0)) return;
    dragRef.current = { id: e.pointerId, x0: e.clientX, y0: e.clientY, dx: 0, moved: false };
    setDragging(true);
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onDragMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = dragRef.current;
    if (!d || e.pointerId !== d.id) return;
    d.dx = e.clientX - d.x0;
    const dy = e.clientY - d.y0;
    if (!d.moved && Math.abs(d.dx) < 6 && Math.abs(dy) < 6) return;
    d.moved = true;
    const pinned = (off === 0 && d.dx > 0) || (off === 1 && d.dx < 0);
    setNudge(pinned ? d.dx * 0.25 : Math.max(-180, Math.min(180, d.dx)));
  };
  const onDragEnd = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = dragRef.current;
    if (!d || e.pointerId !== d.id) return;
    dragRef.current = null;
    setDragging(false);
    setNudge(0);
    if (d.moved && Math.abs(d.dx) > 48 && Math.abs(d.dx) > Math.abs(e.clientY - d.y0) * 1.4) {
      if (d.dx > 0 && off === 0) setPage(1);
      if (d.dx < 0 && off === 1) setPage(0);
    }
  };

  /* Week/month/year bars from the same daily cells the heatmap renders. Each
   * period pages one unit back — last week, last month, last year — built by
   * this same code with the period's base shifted; a back page never carries
   * the future hatch or the now ring because all of its dates lose to today.
   * The engine's 742-day cell window keeps last year's months full. */
  const cellMap = new Map(cells.map((c) => [c.date, c]));
  const barOf = (date: string, axis: string, hover: string): Bar => {
    const cell = cellMap.get(date);
    return {
      tokens: cell?.total_tokens ?? 0,
      cost: cell?.cost ?? 0,
      requests: cell?.requests ?? 0,
      future: date > today,
      now: date === today,
      axis,
      hover,
    };
  };
  const barsOf = (shift: number): Bar[] => {
    if (period === "week") {
      const t0 = new Date(`${today}T00:00:00`);
      const monday = new Date(t0);
      monday.setDate(t0.getDate() - ((t0.getDay() + 6) % 7) - shift * 7);
      return Array.from({ length: 7 }, (_, i) => {
        const d = new Date(monday);
        d.setDate(monday.getDate() + i);
        const date = isoDate(d);
        return barOf(date, weekdayLabel(i), formatDateLabel(date));
      });
    }
    if (period === "month") {
      const base = new Date(Number(today.slice(0, 4)), Number(today.slice(5, 7)) - 1 - shift, 1);
      const y = base.getFullYear();
      const m = base.getMonth() + 1;
      const days = new Date(y, m, 0).getDate();
      return Array.from({ length: days }, (_, i) => {
        const d = i + 1;
        const date = `${y}-${pad2(m)}-${pad2(d)}`;
        // 31 labels never fit under these columns; every fifth dates the axis.
        return barOf(date, d === 1 || d % 5 === 0 ? String(d) : "", formatDateLabel(date));
      });
    }
    const y = Number(today.slice(0, 4)) - shift;
    return Array.from({ length: 12 }, (_, i) => {
      const m = i + 1;
      const prefix = `${y}-${pad2(m)}`;
      let tokens = 0;
      let cost = 0;
      let requests = 0;
      for (const c of cells) {
        if (c.date.startsWith(prefix)) {
          tokens += c.total_tokens;
          cost += c.cost;
          requests += c.requests;
        }
      }
      return {
        tokens,
        cost,
        requests,
        future: prefix > today.slice(0, 7),
        now: prefix === today.slice(0, 7),
        axis: monthLabel(m),
        hover: shift ? yearMonthLabel(y, m) : monthLabel(m),
      };
    });
  };
  const barsCur = period === "day" ? [] : barsOf(0);
  const barsPrev = period === "day" ? [] : barsOf(1);
  // The viewed page's own bars: the readout sums exactly the bars on screen,
  // and each page scales to its own peak — a quiet last month still reads.
  const shownBars = off === 1 ? barsPrev : barsCur;
  const barTotals = shownBars.reduce(
    (acc, b) => ({ tokens: acc.tokens + b.tokens, cost: acc.cost + b.cost }),
    { tokens: 0, cost: 0 },
  );
  const maxBar = Math.max(...shownBars.map((b) => b.tokens), 1);
  const barCols = (list: Bar[]) =>
    list.map((b, i) => (
      <div
        key={i}
        className="hb-col"
        role="img"
        aria-label={`${b.hover} · ${compactTokens(b.tokens)} tokens`}
        data-now={b.now || undefined}
        data-future={b.future || undefined}
        onMouseEnter={() => setHoverBar(b)}
      >
        <div className="hb-barw">
          <span
            className="hb-bar"
            style={{ height: b.tokens > 0 ? `${Math.max(4, Math.round((b.tokens / maxBar) * 100))}%` : 0 }}
          />
        </div>
        <span className="hb-lab">{b.axis}</span>
      </div>
    ));

  /* The week tab's range label and the month tab's month label, per page —
   * the back month names its year, or January's "12月" reads as this year's. */
  const weekRange = (shift: number) => {
    const t0 = new Date(`${today}T00:00:00`);
    const monday = new Date(t0);
    monday.setDate(t0.getDate() - ((t0.getDay() + 6) % 7) - shift * 7);
    const sunday = new Date(monday);
    sunday.setDate(monday.getDate() + 6);
    return `${monday.getMonth() + 1}/${monday.getDate()} – ${sunday.getMonth() + 1}/${sunday.getDate()}`;
  };
  const monthMeta = (shift: number) => {
    if (shift === 0) return monthLabel(Number(today.slice(5, 7)));
    const base = new Date(Number(today.slice(0, 4)), Number(today.slice(5, 7)) - 1 - shift, 1);
    return yearMonthLabel(base.getFullYear(), base.getMonth() + 1);
  };

  // The pitch is width ÷ columns, capped: the grid fills the card from about
  // the 27th week of the year on, and before that the squares stop growing at
  // GitHub's size and the graph simply ends where the year has reached. The
  // trailing gap is still the today ring's bleed margin.
  const pitchX = Math.min(PITCH_CAP, GRID_W / weeks.length);
  const cellW = pitchX * CELL_RATIO;
  const cellH = PITCH_Y * CELL_RATIO;
  const width = weeks.length * pitchX;
  const height = 6 * PITCH_Y + cellH;

  const onViewKey = (e: React.KeyboardEvent) => {
    const delta = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!delta || !hasHours) return;
    e.preventDefault();
    const i = (VIEWS.indexOf(mode) + delta + VIEWS.length) % VIEWS.length;
    setMode(VIEWS[i]);
  };

  const tail = (cost: number, requests: number) =>
    showMoney ? (
      <>
        {" "}
        · {money(cost)} · {count(requests)} {t("unit.times")}
      </>
    ) : null;

  return (
    <Section
      label={t("view.heat")}
      meta={hasHours ? undefined : today.slice(0, 4)}
      head={
        hasHours ? (
          <>
            <div className="seg" role="radiogroup" aria-label={t("view.a11y")} onKeyDown={onViewKey}>
              {VIEWS.map((v) => (
                <button
                  key={v}
                  type="button"
                  role="radio"
                  className="seg-btn"
                  aria-checked={v === mode}
                  tabIndex={v === mode ? 0 : -1}
                  onClick={() => setMode(v)}
                >
                  {v === "heat" ? VIEW_HEAT() : off === 1 ? PERIOD_PREV[period]() : PERIOD_VIEW[period]()}
                </button>
              ))}
            </div>
            <span className="sec-meta num">
              {mode === "heat"
                ? today.slice(0, 4)
                : period === "day"
                  ? formatDateLabel(viewDate)
                  : period === "week"
                    ? weekRange(off)
                    : period === "month"
                      ? monthMeta(off)
                      : String(Number(today.slice(0, 4)) - off)}
            </span>
          </>
        ) : undefined
      }
    >
      {hasHours && mode === "hours" ? (
        <div className="view view-hours">
          <div className="chart-body">
            {/* Every period is the same two-page pager: the current stretch,
             * and one swipe back — yesterday, last week, last month, last
             * year. Drag right (or two fingers right) opens the back page;
             * left returns. The day page needs `hourly_prev`; the others page
             * over the daily cells and always have a back page. */}
            <div
              ref={pagerRef}
              className={pager ? "hours-scroll pager" : "hours-scroll"}
              title={pager ? t(off === 0 ? SWIPE_PREV[period] : SWIPE_CUR[period]) : undefined}
              onPointerDown={pager ? onDragStart : undefined}
              onPointerMove={pager ? onDragMove : undefined}
              onPointerUp={pager ? onDragEnd : undefined}
              onPointerCancel={pager ? onDragEnd : undefined}
            >
              <div
                className={"hours-track" + (dragging ? " dragging" : "")}
                style={{ transform: `translateX(calc(${off * -100}% + ${nudge}px))` }}
              >
                {period === "day" ? (
                  <>
                    <div className="hours-chart" role="group" aria-label={t("hours.a11y")} onMouseLeave={() => setHoverHour(null)}>
                      {dayColumns(hours, true)}
                    </div>
                    {pager ? (
                      <div className="hours-chart" role="group" aria-label={t("hours.a11y.yday")} onMouseLeave={() => setHoverHour(null)}>
                        {dayColumns(hoursPrev ?? [], false)}
                      </div>
                    ) : null}
                  </>
                ) : (
                  <>
                    <div
                      className="hours-chart"
                      role="group"
                      aria-label={t(BARS_A11Y[period])}
                      onMouseLeave={() => {
                        setHoverHour(null);
                        setHoverBar(null);
                      }}
                    >
                      {barCols(barsCur)}
                    </div>
                    <div
                      className="hours-chart"
                      role="group"
                      aria-label={t(BARS_A11Y_PREV[period])}
                      onMouseLeave={() => {
                        setHoverHour(null);
                        setHoverBar(null);
                      }}
                    >
                      {barCols(barsPrev)}
                    </div>
                  </>
                )}
              </div>
            </div>
          </div>
          <div className="chart-foot">
            <span className="chart-read num" role="status" aria-live="off">
              {period === "day" ? (
                hoverHour !== null ? (
                  <>
                    {t("read.hour", { h: pad2(hoverHour), t: compactTokens(dayHours[hoverHour].total_tokens) })}
                    {dayHours[hoverHour].total_tokens > 0 ? tail(dayHours[hoverHour].cost, dayHours[hoverHour].requests) : null}
                  </>
                ) : (
                  <>
                    {t(off === 0 ? "read.day" : "read.yday", { t: compactTokens(dayTotals.tokens) })}
                    {showMoney ? <> · {money(dayTotals.cost)}</> : null}
                  </>
                )
              ) : hoverBar ? (
                <>
                  {hoverBar.hover} · {compactTokens(hoverBar.tokens)} tokens
                  {hoverBar.tokens > 0 ? tail(hoverBar.cost, hoverBar.requests) : null}
                </>
              ) : (
                <>
                  {period === "week"
                    ? t(off === 0 ? "read.week" : "read.pweek", { t: compactTokens(barTotals.tokens) })
                    : period === "month"
                      ? t(off === 0 ? "read.month" : "read.pmonth", { t: compactTokens(barTotals.tokens) })
                      : t(off === 0 ? "read.year" : "read.pyear", { t: compactTokens(barTotals.tokens) })}
                  {showMoney ? <> · {money(barTotals.cost)}</> : null}
                </>
              )}
            </span>
            {period === "day" && off === 0 ? (
              <span className="chart-side hours-now num">{t("now.hour", { h: pad2(nowHour) })}</span>
            ) : null}
          </div>
        </div>
      ) : (
        <div className="view view-heat">
          <div className="chart-body">
            <div className="heat-wrap">
              {/* The GitHub-style row gutter: Mon/Wed/Fri only, right-aligned
                  against the grid, centered on their rows below the months
                  strip. */}
              <div className="heat-days" aria-hidden="true">
                <span style={{ top: MONTHS_ROW_H + cellH / 2 }}>{t("heat.row.mon")}</span>
                <span style={{ top: MONTHS_ROW_H + 2 * PITCH_Y + cellH / 2 }}>{t("heat.row.wed")}</span>
                <span style={{ top: MONTHS_ROW_H + 4 * PITCH_Y + cellH / 2 }}>{t("heat.row.fri")}</span>
              </div>
              <div className="heat-main">
                <div className="heat-months" style={{ width }} aria-hidden="true">
                  {monthMarkers(weeks).map((m) => (
                    <span key={`${m.label}-${m.index}`} style={{ left: m.index * pitchX }}>
                      {m.label}
                    </span>
                  ))}
                </div>
                <svg
                  className="heat-svg"
                  width={width}
                  height={height}
                  viewBox={`0 0 ${width} ${height}`}
                  role="img"
                  aria-label={
                    showMoney
                      ? t("heat.aria.money", { t: compactTokens(totals.tokens), c: money(totals.cost) })
                      : t("heat.aria.plain", { t: compactTokens(totals.tokens) })
                  }
                  onMouseLeave={() => setHover(null)}
                >
                  {weeks.map((week, col) =>
                    week.map((cell, row) =>
                      cell ? (
                        <rect
                          key={cell.date}
                          className="heat-cell"
                          data-level={levelOf(cell.total_tokens, thresholds)}
                          data-today={cell.date === today || undefined}
                          x={col * pitchX}
                          y={row * PITCH_Y}
                          width={cellW}
                          height={cellH}
                          rx={1}
                          onMouseEnter={() => setHover(cell)}
                        />
                      ) : null,
                    ),
                  )}
                </svg>
              </div>
            </div>
          </div>
          {/* A fixed readout row: no overlay, no layout shift, and the totals stay
              visible even when nothing is hovered. Identical box to the hours
              view's foot — switching views moves no text. */}
          <div className="chart-foot">
            <span className="chart-read num" role="status" aria-live="off">
              {hover ? (
                <>
                  {formatDateLabel(hover.date)} · {compactTokens(hover.total_tokens)} tokens
                  {showMoney ? <> · {money(hover.cost)}</> : null} · {count(hover.requests)} {t("unit.times")}
                </>
              ) : (
                <>
                  {t("read.heat.total", { t: compactTokens(totals.tokens) })}
                  {showMoney ? <> · {money(totals.cost)}</> : null}
                </>
              )}
            </span>
            <span className="chart-side heat-legend" aria-hidden="true">
              {t("legend.less")}
              {LEVELS.map((level) => (
                <i key={level} data-level={level} />
              ))}
              {t("legend.more")}
            </span>
          </div>
        </div>
      )}
    </Section>
  );
}
