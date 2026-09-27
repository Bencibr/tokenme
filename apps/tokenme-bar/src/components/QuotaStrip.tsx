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
  // Click a truncated window label to read it whole: the native hover title
  // exists, but nobody waits out its delay in a non-activating panel.
  const [tipKey, setTipKey] = useState<string | null>(null);
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
  // A plan suffix every window of a tool repeats ("5 小时 · GLM Coding Lite")
  // is group-level information: it rides the tool header once, and the rows
  // keep only their window names.
  const planOf = (rows: QuotaView[]): string => {
    // Ledger rows (今日额度 · 已用 X/Y) are their own tail and never vote —
    // a Start Plan bucket lives in every ZCode group, so requiring unanimous
    // tails disabled the hoist forever.
    const tails = rows
      .filter((q) => !windowName(q).includes("已用 "))
      .map((q) => {
        const label = windowName(q);
        const cut = label.indexOf(" · ");
        return cut === -1 ? "" : label.slice(cut + 3);
      });
    if (tails.length === 0) return "";
    const first = tails[0];
    if (!first || tails.some((t) => t !== first)) return "";
    return first;
  };
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
        <span className="quota-legend" aria-hidden="true">
          <span><i data-stage="low" />安稳</span>
          <span><i data-stage="mid" />注意</span>
          <span><i data-stage="high" />告急</span>
        </span>
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
          const plan = planOf(g.rows);
          // Origins are noise when every row answers the same way — the common
          // case is everything probed live. A group badge appears only when a
          // row's number came from the tool's own records (日志), exactly the
          // rows a reader must not mistake for a vendor meter; budget rows
          // carry their own per-row badge already.
          const logged = g.rows.some((r) => r.origin === "log");
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
                {plan ? <span className="quota-plan">{plan}</span> : null}
                <span className="quota-origins">
                  {logged ? (
                    <span className="quota-badge" data-origin="log">
                      {ORIGIN.log}
                    </span>
                  ) : null}
                </span>
              </div>
              {g.rows.map((q) => {
                const pct = q.used_percent;
                const width = Math.max(0, Math.min(100, pct));
                // One colour per stage, the whole bar: <50% mint, 50-79% amber,
                // ≥80% red — a gauge whose hue answers "how urgent", never a
                // gradient that leaves two colour semantics on one bar.
                const stage = pct > 100 ? "over" : pct >= 80 ? "high" : pct >= 50 ? "mid" : "low";
                // A source with no metered limit at all keeps its row (so the
                // tool block never loses a line) but its track hatches: nothing
                // here is being measured, and a 0% bar would claim otherwise.
                const unlimited = (q.label ?? "").includes("无可查限额");
                const mine = (q.origin ?? "probe") === "budget";
                // The window name leads at full ink; a plan suffix the source
                // appended ("5 小时 · GLM Coding Lite") follows dimmed, so ten
                // rows of the same plan stop shouting it.
                const label = windowName(q);
                const cut = label.indexOf(" · ");
                const head = cut === -1 ? label : label.slice(0, cut);
                const tail = cut === -1 ? "" : label.slice(cut);
                const rowTail = tail === ` · ${plan}` ? "" : tail;
                return (
                  <div
                    className="quota-row"
                    key={rowKey(q)}
                    data-qrow={rowKey(q)}
                    data-dragging={drag?.kind === "row" && drag.key === rowKey(q) ? "" : undefined}
                    onPointerDown={(e) => beginPress(e, { kind: "row", key: rowKey(q) })}
                  >
                    <span
                      className="quota-name"
                      title={label}
                      onClick={() => setTipKey(tipKey === rowKey(q) ? null : rowKey(q))}
                      onMouseLeave={() => tipKey === rowKey(q) && setTipKey(null)}
                    >
                      <span className="quota-text">
                        <span className="quota-win">{head}</span>
                        {rowTail ? <span className="quota-sub">{rowTail}</span> : null}
                      </span>
                      {tipKey === rowKey(q) ? <span className="rank-tip">{label}</span> : null}
                      {mine && (
                        <span className="quota-badge" data-origin="budget">
                          {ORIGIN.budget}
                        </span>
                      )}
                    </span>
                    {unlimited ? (
                      <span className="track track-inf" aria-hidden="true" />
                    ) : (
                      <span
                        className="track"
                        role="progressbar"
                        aria-valuenow={Math.round(pct)}
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-label={`${toolDisplay(q.tool)} ${label} 已用 ${pct.toFixed(1)}%`}
                      >
                        <i
                          style={{
                            width: `${width}%`,
                            // A used window at 3% must still read as *started*;
                            // below ~4 px the fill is invisible on a 84 px track.
                            minWidth: pct > 0 ? 4 : undefined,
                          }}
                          data-stage={stage}
                        />
                      </span>
                    )}
                    {unlimited ? (
                      <span className="quota-pct quota-pct--na">不限</span>
                    ) : (
                      <span className="quota-pct num" data-stage={stage}>
                        {pct > 100 ? Math.round(pct) : pct < 10 ? pct.toFixed(1) : Math.round(pct)}
                        <em>%</em>
                      </span>
                    )}
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
