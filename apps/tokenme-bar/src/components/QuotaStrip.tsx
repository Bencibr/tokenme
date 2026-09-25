import { useEffect, useRef, useState } from "react";
import type { QuotaOrder, QuotaView } from "../types";
import { bridge } from "../lib/bridge";
import { toolColor, toolDisplay, until } from "../lib/format";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";

/** Windows are named by length unless the source gave a better bucket name. */
function windowName(q: QuotaView): string {
  if (q.label && q.label.length > 0) return q.label;
  if (q.window_minutes >= 43_200) return "月窗口";
  if (q.window_minutes >= 1_440) return `${Math.round(q.window_minutes / 1440)} 天窗口`;
  if (q.window_minutes >= 60) return `${Math.round(q.window_minutes / 60)} 小时窗口`;
  return q.window_minutes > 0 ? `${q.window_minutes} 分钟窗口` : "窗口未知";
}

const ORIGIN: Record<string, string> = { probe: "实测", log: "日志", budget: "预算" };

/** The app icon's dual rings, live: the outer arc is the worst live long
 *  window (7 天级), the inner the worst short one (5 小时级); arc length is the
 *  used percent and the colour steps green → amber → red as the window fills.
 *  A glanceable echo of the rows underneath — exact numbers stay in the rows. */
function QuotaRings({ short, long }: { short: number; long: number }) {
  const tier = (pct: number) => (pct >= 85 ? "var(--bad)" : pct >= 60 ? "var(--warn)" : "var(--ok)");
  const arc = (pct: number) => Math.max(0, Math.min(100, pct));
  const ring = (r: number, pct: number, key: string) => (
    <>
      <circle cx="22" cy="22" r={r} fill="none" stroke="var(--track)" strokeWidth="5" pathLength={100} />
      {pct > 0 ? (
        <circle
          cx="22"
          cy="22"
          r={r}
          fill="none"
          stroke={tier(pct)}
          strokeWidth="5"
          strokeLinecap="round"
          pathLength={100}
          strokeDasharray={`${arc(pct)} ${100 - arc(pct)}`}
          className="ring-arc"
          data-ring={key}
        />
      ) : null}
    </>
  );
  return (
    <svg
      className="quota-rings"
      width={30}
      height={30}
      viewBox="0 0 44 44"
      role="img"
      aria-label={`配额环：外环 7 天窗口已用 ${Math.round(long)}%，内环 5 小时窗口已用 ${Math.round(short)}%`}
    >
      <title>外环 7 天窗口 · 内环 5 小时窗口</title>
      <g transform="rotate(-90 22 22)">
        {ring(18.5, long, "long")}
        {ring(10.5, short, "short")}
      </g>
    </svg>
  );
}

/** Row identity, and the one thing a saved drag order may key on. A probe with
 *  a natural window name carries an id (`zcode/mcp`); the rest derive one from
 *  nominal length + label, neither of which changes between refreshes. Rows
 *  whose numbers move — spent dollars, a reset gap shrinking every poll — are
 *  exactly the rows the sources give an id for. */
function rowKey(q: QuotaView): string {
  return `${q.tool}/${q.id ?? `w${q.window_minutes}:${q.label ?? ""}`}`;
}

/** Saved entries keep their slot; everything new keeps the report's own order
 *  behind them (the sort is stable, so equal ranks never shuffle). */
function ordered<T>(items: T[], saved: string[], key: (t: T) => string): T[] {
  const rank = (k: string) => {
    const i = saved.indexOf(k);
    return i === -1 ? saved.length : i;
  };
  return [...items].sort((a, b) => rank(key(a)) - rank(key(b)));
}

/** One drag step: lift `from` to sit where `to` is now. */
function moved(keys: string[], from: string, to: string): string[] {
  const out = [...keys];
  const a = out.indexOf(from);
  const b = out.indexOf(to);
  if (a === -1 || b === -1 || a === b) return keys;
  out.splice(out.indexOf(to), 0, ...out.splice(a, 1));
  return out;
}

type Drag = { kind: "row" | "tool"; key: string };

/** Survives the component unmounting (the section disappears whenever no
 *  window is live), so a drag started before an empty spell keeps its order. */
let savedOrder: QuotaOrder = { tools: [], rows: [] };
let orderRequested = false;

/** How long a still press is held before it becomes a drag. Movement beyond
 *  {@link MOVE_CANCEL_PX} first means a scroll, never a reorder. */
const LONG_PRESS_MS = 320;
const MOVE_CANCEL_PX = 8;

/**
 * Quota windows the tools report about themselves, plus the caps set in
 * `tokenme budget`.
 *
 * Grouped by tool rather than one row per window: Codex and Antigravity each
 * answer with a 5-hour and a weekly window, and a flat list read as four
 * unrelated tools.
 *
 * The order is fixed, not a ranking: an earlier build re-sorted on
 * `used_percent` (a number that changes on every refresh) and on window length
 * (whose implied variant — a reset gap — shrinks continuously), so the bars
 * jumped around while being read. The report now ships a rank-stable order and
 * the panel renders it as-is; the arrangement is the user's to change, by
 * long-pressing a bar (or a tool header) and dragging. The order persists in
 * settings.json; windows seen for the first time join at the end.
 *
 * `origin` is shown because the three kinds mean different things: 实测 is the
 * vendor's own number, 日志 came out of its records, 预算 is a cap this app set
 * and measured against its own cost. Budget rows carry their own 预算 badge,
 * because a "日/月" cap sitting between vendor windows reads as one of them
 * otherwise.
 */
export function QuotaStrip({ quotas, now }: { quotas: QuotaView[]; now: number }) {
  const live = quotas.filter((q) => q.resets_at_ms === 0 || q.resets_at_ms > now);
  const [order, setOrderState] = useState<QuotaOrder>(savedOrder);
  const [drag, setDrag] = useState<Drag | null>(null);
  const dragRef = useRef<Drag | null>(null);
  // The authoritative order: pointer moves arrive faster than renders, so the
  // drag handlers swap through the ref and never through stale state.
  const orderRef = useRef(order);
  const pressRef = useRef<{ timer: number; x: number; y: number } | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const keysRef = useRef<{ tools: string[]; rows: string[] }>({ tools: [], rows: [] });

  const applyOrder = (next: QuotaOrder) => {
    orderRef.current = next;
    setOrderState(next);
  };

  useEffect(() => {
    if (orderRequested) return;
    orderRequested = true;
    void bridge
      .quotaOrder()
      .then((o) => {
        savedOrder = o;
        applyOrder(o);
      })
      .catch(() => {});
  }, []);

  if (live.length === 0) return null;

  const byTool = new Map<string, QuotaView[]>();
  for (const q of live) {
    const rows = byTool.get(q.tool) ?? [];
    rows.push(q);
    byTool.set(q.tool, rows);
  }
  const groups = ordered([...byTool.keys()], order.tools, (t) => t).map((tool) => ({
    tool,
    rows: ordered(byTool.get(tool) ?? [], order.rows, rowKey),
  }));
  // The drag handlers run outside render, so the key lists they reorder from
  // must be the ones on screen right now.
  keysRef.current = {
    tools: groups.map((g) => g.tool),
    rows: groups.flatMap((g) => g.rows.map(rowKey)),
  };

  const beginPress = (e: React.PointerEvent, target: Drag) => {
    if (e.button !== 0) return;
    const { clientX: x, clientY: y, pointerId } = e;
    pressRef.current = {
      x,
      y,
      timer: window.setTimeout(() => {
        pressRef.current = null;
        dragRef.current = target;
        setDrag(target);
        // Keep move/up events flowing even when the pointer leaves the rows.
        try {
          listRef.current?.setPointerCapture(pointerId);
        } catch {
          /* a released pointer simply never drags */
        }
      }, LONG_PRESS_MS),
    };
  };

  const cancelPress = () => {
    if (pressRef.current) {
      window.clearTimeout(pressRef.current.timer);
      pressRef.current = null;
    }
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const active = dragRef.current;
    if (!active) {
      // A still hold becomes a drag; a moving one was a scroll all along.
      if (pressRef.current) {
        const { x, y, timer } = pressRef.current;
        if (Math.abs(e.clientX - x) > MOVE_CANCEL_PX || Math.abs(e.clientY - y) > MOVE_CANCEL_PX) {
          window.clearTimeout(timer);
          pressRef.current = null;
        }
      }
      return;
    }
    const over =
      active.kind === "row"
        ? document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>("[data-qrow]")
        : document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>("[data-qtool]");
    if (!over) return;
    const overKey = (active.kind === "row" ? over.dataset.qrow : over.dataset.qtool) ?? "";
    if (!overKey || overKey === active.key) return;
    if (active.kind === "row" && !overKey.startsWith(`${active.key.split("/")[0]}/`)) {
      return; // a bar stays inside its own tool's block
    }
    const o = orderRef.current;
    const base = {
      tools: o.tools.length ? o.tools : keysRef.current.tools,
      rows: o.rows.length ? o.rows : keysRef.current.rows,
    };
    if (active.kind === "row") base.rows = moved(base.rows, active.key, overKey);
    else base.tools = moved(base.tools, active.key, overKey);
    applyOrder(base);
  };

  const finish = () => {
    cancelPress();
    if (!dragRef.current) return;
    dragRef.current = null;
    setDrag(null);
    const final = orderRef.current;
    savedOrder = final;
    void bridge.setQuotaOrder(final).catch(() => {});
  };

  return (
    <Section
      label="配额"
      meta={`${live.length} 个窗口 · ${groups.length} 个工具`}
      trail={
        // window_minutes 0 = unknown window; it belongs to neither ring.
        <QuotaRings
          short={Math.max(0, ...live.filter((q) => q.window_minutes > 0 && q.window_minutes < 1440).map((q) => q.used_percent))}
          long={Math.max(0, ...live.filter((q) => q.window_minutes >= 1440).map((q) => q.used_percent))}
        />
      }
    >
      <div
        className="quota-list"
        ref={listRef}
        data-dragging={drag ? "" : undefined}
        onPointerMove={onPointerMove}
        onPointerUp={finish}
        onPointerCancel={finish}
      >
        {groups.map((g) => {
          const origins = [...new Set(g.rows.map((r) => r.origin ?? "probe"))];
          return (
            <div
              className="quota-group"
              key={g.tool}
              data-qtool={g.tool}
              data-dragging={drag?.kind === "tool" && drag.key === g.tool ? "" : undefined}
              style={{ "--c": toolColor(g.tool) } as React.CSSProperties}
            >
              <div
                className="quota-head"
                title="长按拖动可调整顺序"
                onPointerDown={(e) => beginPress(e, { kind: "tool", key: g.tool })}
              >
                <ToolIcon tool={g.tool} size={18} />
                <span className="quota-tool">{toolDisplay(g.tool)}</span>
                <span className="quota-origins">
                  {origins.map((o) => (
                    <span key={o} className="quota-badge" data-origin={o}>
                      {ORIGIN[o] ?? o}
                    </span>
                  ))}
                </span>
              </div>
              {g.rows.map((q) => {
                const pct = q.used_percent;
                const width = Math.max(0, Math.min(100, pct));
                const hot = pct >= 80;
                const mine = (q.origin ?? "probe") === "budget";
                return (
                  <div
                    className="quota-row"
                    key={rowKey(q)}
                    data-qrow={rowKey(q)}
                    data-dragging={drag?.kind === "row" && drag.key === rowKey(q) ? "" : undefined}
                    onPointerDown={(e) => beginPress(e, { kind: "row", key: rowKey(q) })}
                  >
                    <span className="quota-name" title={windowName(q)}>
                      <span className="quota-text">{windowName(q)}</span>
                      {mine && (
                        <span className="quota-badge" data-origin="budget">
                          {ORIGIN.budget}
                        </span>
                      )}
                    </span>
                    <span
                      className="track"
                      role="progressbar"
                      aria-valuenow={Math.round(pct)}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-label={`${toolDisplay(q.tool)} ${windowName(q)} 已用 ${pct.toFixed(1)}%`}
                    >
                      <i
                        style={{
                          width: `${width}%`,
                          // A used window at 3% must still read as *started*;
                          // below ~4 px the fill is invisible on a 84 px track.
                          minWidth: pct > 0 ? 4 : undefined,
                          // The gradient paints the whole window, the fill only
                          // reveals it: the bar's leading edge is the color of
                          // "how far from unusable" — red by 100%, never a full
                          // bar that could be read as untouched allowance.
                          backgroundSize: width > 0 ? `${10000 / width}% 100%` : "0% 100%",
                        }}
                        data-hot={hot || undefined}
                        data-over={pct > 100 || undefined}
                      />
                    </span>
                    <span className="quota-pct num" data-hot={hot || undefined}>
                      {pct > 100 ? Math.round(pct) : pct < 10 ? pct.toFixed(1) : Math.round(pct)}
                      <em>%</em>
                    </span>
                    <span className="quota-reset num">
                      {q.resets_at_ms > 0 ? until(q.resets_at_ms, now) : "—"}
                    </span>
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>
    </Section>
  );
}
