import { useMemo, useState } from "react";
import type { HeatCell, HourCell } from "../types";
import { compactTokens, count, formatDateLabel, money } from "../lib/format";
import { useMoney } from "../lib/display";
import { monthLabel, t } from "../lib/i18n";
import { Section } from "./Section";

/* The two pitches are decoupled on purpose: 7 rows can never fill a 96px body
 * with square cells, so the cells go very slightly rectangular (5.5 × 6.6) and
 * both views end at exactly the same height — switching never jumps. */
const CELL_W = 5.5;
const CELL_H = 6.6;
const PITCH_X = 6.94;
const PITCH_Y = 8.5667;
const WEEKS_SHOWN = 53;
const HOUR_SLOTS = 24;
const LEVELS = [1, 2, 3, 4];

const VIEWS = ["hours", "heat"] as const;
type ViewMode = (typeof VIEWS)[number];
const VIEW_LABEL: Record<ViewMode, () => string> = { hours: () => t("view.today"), heat: () => t("view.heat") };

const pad2 = (n: number) => String(n).padStart(2, "0");

/** Monday-anchored columns; the leading partial week is trimmed and the
 * trailing one is kept short — the graph must reach today, or "活动" reads as
 * silently stale every day of the week after Sunday. */
function toWeeks(cells: HeatCell[]): HeatCell[][] {
  if (cells.length === 0) return [];
  const offset = (new Date(`${cells[0].date}T00:00:00`).getDay() + 6) % 7;
  const aligned = cells.slice(offset);
  const cols: HeatCell[][] = [];
  for (let i = 0; i < aligned.length; i += 7) cols.push(aligned.slice(i, i + 7));
  return cols.slice(-WEEKS_SHOWN);
}

/** Levels are quantiles of *active* days, so a quiet month still reads as quiet. */
function levelOf(value: number, thresholds: number[]): number {
  if (value <= 0) return 0;
  let level = 1;
  for (const th of thresholds) if (value > th) level += 1;
  return Math.min(level, 4);
}

function monthMarkers(weeks: HeatCell[][]) {
  const out: Array<{ index: number; label: string }> = [];
  let last = -1;
  weeks.forEach((week, index) => {
    const month = Number(week[0].date.slice(5, 7));
    const dayOfMonth = Number(week[0].date.slice(8, 10));
    // A label needs 4 clear columns, or "10月" and "11月" collide.
    if (month !== last && dayOfMonth <= 7 && index - last >= 0 && (!out.length || index - out[out.length - 1].index >= 4)) {
      out.push({ index, label: monthLabel(month) });
      last = month;
    } else if (month !== last) {
      last = month;
    }
  });
  return out;
}

export function Heatmap({ cells, today, hours }: { cells: HeatCell[]; today: string; hours: HourCell[] }) {
  const [hover, setHover] = useState<HeatCell | null>(null);
  const [hoverHour, setHoverHour] = useState<number | null>(null);
  // An old snapshot reports no hourly vec; the section then stays heat-only.
  const [mode, setMode] = useState<ViewMode>(() => (hours.length === HOUR_SLOTS ? "hours" : "heat"));
  const showMoney = useMoney();

  const { weeks, thresholds, totals } = useMemo(() => {
    const grid = toWeeks(cells);
    const flat = grid.flat();
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
  }, [cells]);

  if (weeks.length === 0) return null;

  const hasHours = hours.length === HOUR_SLOTS;
  // The day total reads all 24 slots, so a report generated minutes ago still
  // agrees with the chart's own bars.
  const dayTotals = hours.reduce(
    (acc, h) => ({ tokens: acc.tokens + h.total_tokens, cost: acc.cost + h.cost }),
    { tokens: 0, cost: 0 },
  );
  const maxHour = Math.max(...hours.map((h) => h.total_tokens), 1);
  const nowHour = new Date().getHours();

  // A full pitch per column: the trailing gap is the right margin where the
  // today ring's stroke bleeds — a width hugging the last cell clips it.
  const width = weeks.length * PITCH_X;
  const height = 6 * PITCH_Y + CELL_H;

  const onViewKey = (e: React.KeyboardEvent) => {
    const delta = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!delta || !hasHours) return;
    e.preventDefault();
    const i = (VIEWS.indexOf(mode) + delta + VIEWS.length) % VIEWS.length;
    setMode(VIEWS[i]);
  };

  const hourRead = (h: HourCell) =>
    showMoney ? (
      <>
        {" "}
        · {money(h.cost)} · {count(h.requests)} {t("unit.times")}
      </>
    ) : null;

  return (
    <Section
      label={t("view.heat")}
      meta={hasHours ? undefined : t("heat.weeks", { n: weeks.length })}
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
                  {VIEW_LABEL[v]()}
                </button>
              ))}
            </div>
            <span className="sec-meta num">
              {mode === "hours" ? formatDateLabel(today) : t("heat.weeks", { n: weeks.length })}
            </span>
          </>
        ) : undefined
      }
    >
      {hasHours && mode === "hours" ? (
        <div className="view view-hours">
          <div className="chart-body">
            <div className="hours-chart" role="radiogroup" aria-label={t("hours.a11y")} onMouseLeave={() => setHoverHour(null)}>
              {hours.map((h, i) => (
                <div
                  key={i}
                  className="hb-col"
                  role="img"
                  aria-label={t("hour.aria", { h: pad2(i), t: compactTokens(h.total_tokens) })}
                  data-now={i === nowHour || undefined}
                  data-future={i > nowHour || undefined}
                  onMouseEnter={() => setHoverHour(i)}
                >
                  <div className="hb-barw">
                    <span
                      className="hb-bar"
                      style={{ height: h.total_tokens > 0 ? `${Math.max(4, Math.round((h.total_tokens / maxHour) * 100))}%` : 0 }}
                    />
                  </div>
                  <span className="hb-lab">{i % 3 === 0 ? pad2(i) : ""}</span>
                </div>
              ))}
            </div>
          </div>
          <div className="chart-foot">
            <span className="chart-read num" role="status" aria-live="off">
              {hoverHour !== null ? (
                <>
                  {t("read.hour", { h: pad2(hoverHour), t: compactTokens(hours[hoverHour].total_tokens) })}
                  {hours[hoverHour].total_tokens > 0 ? hourRead(hours[hoverHour]) : null}
                </>
              ) : (
                <>
                  {t("read.day", { t: compactTokens(dayTotals.tokens) })}
                  {showMoney ? <> · {money(dayTotals.cost)}</> : null}
                </>
              )}
            </span>
            <span className="chart-side hours-now num">{t("now.hour", { h: pad2(nowHour) })}</span>
          </div>
        </div>
      ) : (
        <div className="view view-heat">
          <div className="chart-body">
            <div className="heat-months" style={{ width }} aria-hidden="true">
              {monthMarkers(weeks).map((m) => (
                <span key={`${m.label}-${m.index}`} style={{ left: m.index * PITCH_X }}>
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
                  ? t("heat.aria.money", { n: weeks.length, t: compactTokens(totals.tokens), c: money(totals.cost) })
                  : t("heat.aria.plain", { n: weeks.length, t: compactTokens(totals.tokens) })
              }
              onMouseLeave={() => setHover(null)}
            >
              {weeks.map((week, col) =>
                week.map((cell, row) => (
                  <rect
                    key={cell.date}
                    className="heat-cell"
                    data-level={levelOf(cell.total_tokens, thresholds)}
                    data-today={cell.date === today || undefined}
                    x={col * PITCH_X}
                    y={row * PITCH_Y}
                    width={CELL_W}
                    height={CELL_H}
                    rx={1}
                    onMouseEnter={() => setHover(cell)}
                  />
                )),
              )}
            </svg>
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
