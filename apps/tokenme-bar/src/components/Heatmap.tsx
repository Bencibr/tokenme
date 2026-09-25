import { useMemo, useState } from "react";
import type { HeatCell } from "../types";
import { compactTokens, count, formatDateLabel, money } from "../lib/format";
import { useMoney } from "../lib/display";
import { Section } from "./Section";

const CELL = 5;
const GAP = 1.5;
const PITCH = CELL + GAP;
const WEEKS_SHOWN = 53;
const LEVELS = [1, 2, 3, 4];

/** Monday-anchored columns; the leading partial week is trimmed. */
function toWeeks(cells: HeatCell[]): HeatCell[][] {
  if (cells.length === 0) return [];
  const offset = (new Date(`${cells[0].date}T00:00:00`).getDay() + 6) % 7;
  const aligned = cells.slice(offset);
  const cols: HeatCell[][] = [];
  for (let i = 0; i + 7 <= aligned.length; i += 7) cols.push(aligned.slice(i, i + 7));
  return cols.slice(-WEEKS_SHOWN);
}

/** Levels are quantiles of *active* days, so a quiet month still reads as quiet. */
function levelOf(value: number, thresholds: number[]): number {
  if (value <= 0) return 0;
  let level = 1;
  for (const t of thresholds) if (value > t) level += 1;
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
      out.push({ index, label: `${month}月` });
      last = month;
    } else if (month !== last) {
      last = month;
    }
  });
  return out;
}

export function Heatmap({ cells, today }: { cells: HeatCell[]; today: string }) {
  const [hover, setHover] = useState<HeatCell | null>(null);
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
  const width = weeks.length * PITCH - GAP;
  const height = 7 * PITCH - GAP;

  return (
    <Section label="活动" meta={`${weeks.length} 周`}>
      <div className="heat">
        <div className="heat-months" style={{ width }} aria-hidden="true">
          {monthMarkers(weeks).map((m) => (
            <span key={`${m.label}-${m.index}`} style={{ left: m.index * PITCH }}>
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
              ? `近 ${weeks.length} 周活动，共 ${compactTokens(totals.tokens)} tokens，花费 ${money(totals.cost)}`
              : `近 ${weeks.length} 周活动，共 ${compactTokens(totals.tokens)} tokens`
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
                x={col * PITCH}
                y={row * PITCH}
                width={CELL}
                height={CELL}
                rx={1}
                onMouseEnter={() => setHover(cell)}
              />
            )),
          )}
        </svg>
        {/* A fixed readout row: no overlay, no layout shift, and the totals stay
            visible even when nothing is hovered. */}
        <div className="heat-foot">
          <span className="heat-read num" role="status" aria-live="off">
            {hover ? (
              <>
                {formatDateLabel(hover.date)} · {compactTokens(hover.total_tokens)} tokens
                {showMoney ? <> · {money(hover.cost)}</> : null} · {count(hover.requests)} 次
              </>
            ) : (
              <>
                共 {compactTokens(totals.tokens)} tokens
                {showMoney ? <> · {money(totals.cost)}</> : null}
              </>
            )}
          </span>
          <span className="heat-legend" aria-hidden="true">
            少
            {LEVELS.map((level) => (
              <i key={level} data-level={level} />
            ))}
            多
          </span>
        </div>
      </div>
    </Section>
  );
}
