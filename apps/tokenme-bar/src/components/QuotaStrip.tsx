import { useEffect, useRef, useState } from "react";
import { Loading } from "./Loading";
import type { NotifyState, QuotaOrder, QuotaView } from "../types";
import { bridge } from "../lib/bridge";
import { toolColor, toolDisplay, until, freshness } from "../lib/format";
import { t } from "../lib/i18n";
import { Section } from "./Section";
import { ToolIcon } from "./ToolIcon";
import { IconBell, IconClose } from "./Icons";

/** Windows are named by length unless the source gave a better bucket name.
 *  A source-provided label passes through verbatim — it is the vendor's text. */
function windowName(q: QuotaView): string {
  if (q.label && q.label.length > 0) return q.label;
  if (q.window_minutes >= 43_200) return t("quota.win.month");
  if (q.window_minutes >= 1_440) return t("quota.win.day", { n: Math.round(q.window_minutes / 1440) });
  if (q.window_minutes >= 60) return t("quota.win.hour", { n: Math.round(q.window_minutes / 60) });
  return q.window_minutes > 0 ? t("quota.win.min", { n: q.window_minutes }) : t("quota.win.unknown");
}

/** Tools whose quota probe owns a daily check-in: the group header grows the
 *  badge/button for exactly these. */
const CHECKIN_TOOLS = ["trae_cn", "qoder", "minimaxcode"];

/** The check-in state rides the report as zero-window marker rows. They are
 *  control channel, not data: the badge and the button read them off the raw
 *  report, the row list never shows them. Trae's entitlement window
 *  (`checkin_…`, a real gauge) is not one of these and stays visible. */
const CHECKIN_STATE_IDS = ["checkin", "checkin-wait", "checkin-gated"];

const ORIGIN_KEY: Record<string, Parameters<typeof t>[0]> = { probe: "quota.origin.probe", log: "quota.origin.log", budget: "quota.origin.budget" };
const ORIGIN = (k: string): string => t(ORIGIN_KEY[k] ?? "quota.origin.probe");

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

/** The guide's dismissal is remembered per permission state: observing the
 *  state change re-arms it, so a grant now and a denial later still speaks. */
const GUIDE_KEY = "tokenme:notifyHint";

function readDismissed(): NotifyState | null {
  try {
    const raw = localStorage.getItem(GUIDE_KEY);
    return raw === "denied" || raw === "not_determined" ? raw : null;
  } catch {
    return null;
  }
}

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
export function QuotaStrip({
  quotas,
  now,
  pending,
  scoped,
  polling = true,
}: {
  quotas: QuotaView[];
  now: number;
  /** True while the boot publish has not run the probes yet. */
  pending?: boolean;
  /** True when a machine scope other than "all" is active. Quotas are probed
   *  on this machine only and never follow the scope, so the section says so
   *  instead of silently mixing calibers. */
  scoped?: boolean;
  /** False when the user turned vendor polling off: the rows keep their last
   *  answer and the section says when it was taken. */
  polling?: boolean;
}) {
  // The check-in marker rows are state, not data (CHECKIN_STATE_IDS): every
  // count and row on screen reads this filtered list; the badge and button
  // below read the raw report instead.
  const live = quotas
    .filter((q) => q.resets_at_ms === 0 || q.resets_at_ms > now)
    .filter((q) => !CHECKIN_STATE_IDS.includes(q.id ?? ""));
  // Trae CN / Qoder 的每日签到：状态按工具各管各的——busy/claimed/msg 都是
  // 以工具 id 为键，点 Trae 的按钮不能把 Qoder 的按钮带成"签到中"，更不能把
  // 另一个工具误标成已签。状态与点击都在这里，而不是配额分组之后——那一段
  // 有早退（无窗口时），hooks 挪过去就会条件化调用，首次渲染即崩（白屏）。
  const [checkinBusy, setCheckinBusy] = useState<string | null>(null);
  const [checkinClaimed, setCheckinClaimed] = useState<Set<string>>(new Set());
  const [checkinMsg, setCheckinMsg] = useState<Record<string, string | undefined>>({});
  /** The backend stamps a claimed day into the checkin pack's window: its id
   *  starts with "checkin" (Trae's entitlement row, Qoder's marker row) — the
   *  wait row (`checkin-wait`, the day's window not open yet) and the gate row
   *  (`checkin-gated`, Qoder's device-identity refusal) are not claims. These
   *  read the raw report: the marker rows are filtered out of the visible
   *  list, so a group's rendered rows cannot answer for them. */
  const groupCheckedIn = (tool: string): boolean =>
    quotas.some((q) => q.tool === tool && q.id?.startsWith("checkin") && q.id !== "checkin-wait" && q.id !== "checkin-gated");
  /** The backend says this tool's day has not opened yet (Qoder 10:00): the
   *  button greys and names the hour instead of claiming into the void. */
  const checkinWaiting = (tool: string): boolean =>
    quotas.some((q) => q.tool === tool && q.id === "checkin-wait");
  /** The vendor refuses third-party claims for this row (Qoder's SAME_PERSON
   *  device gate): the button greys and names the client that can claim it. */
  const checkinGated = (tool: string): boolean =>
    quotas.some((q) => q.tool === tool && q.id === "checkin-gated");
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

  // The system-notification guide, shown while the OS would not deliver a
  // banner. The permission is a fact about this machine, not the report, but
  // the report is the panel's heartbeat: re-asking on each publish means a
  // grant made in System Settings shows up within one cadence.
  const [notify, setNotify] = useState<NotifyState | null>(null);
  const [dismissedFor, setDismissedFor] = useState<NotifyState | null>(readDismissed);
  const guideWatch = useRef<number | null>(null);

  const stopGuideWatch = () => {
    if (guideWatch.current !== null) {
      window.clearInterval(guideWatch.current);
      guideWatch.current = null;
    }
  };

  const adoptNotify = (s: NotifyState) => {
    setNotify(s);
    if (s === "granted") {
      // A real grant re-arms a dismissal: if the user turns notifications off
      // again later, the guide may speak once more.
      try {
        localStorage.removeItem(GUIDE_KEY);
      } catch {
        /* nothing to clear */
      }
      setDismissedFor(null);
    }
  };

  useEffect(() => {
    let alive = true;
    void bridge
      .notifyStatus()
      .then((s) => {
        if (alive) adoptNotify(s);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [quotas]);

  useEffect(() => stopGuideWatch, []);

  const enableNotify = () => {
    void bridge
      .notifyEnable()
      .then((s) => {
        adoptNotify(s);
        stopGuideWatch();
        // The answer can take a while (the prompt sits on screen; System
        // Settings needs the toggle flipped), so watch for it a bounded time.
        if (s !== "not_determined" && s !== "denied") return;
        let tries = 0;
        guideWatch.current = window.setInterval(() => {
          tries += 1;
          void bridge
            .notifyStatus()
            .then((now) => {
              if (now !== s) {
                adoptNotify(now);
                stopGuideWatch();
              }
            })
            .catch(() => {});
          if (tries >= 20) stopGuideWatch();
        }, 2_000);
      })
      .catch(() => {});
  };

  const dismissGuide = () => {
    if (!notify) return;
    setDismissedFor(notify);
    try {
      localStorage.setItem(GUIDE_KEY, notify);
    } catch {
      /* the row just comes back on the next publish */
    }
  };

  if (live.length === 0) {
    // 探测未跑（boot 的首次发布故意跳过配额）：给一个可见的等待，
    // 而不是让配额区块无声消失。pending 为假且为空 = 真没有配额。
    if (pending) {
      return (
        // fill: 探测没跑完时这块替整列配额占位，转圈落在那列行的中间。
        <Section label={t("quota.section")} fill>
          <div className="quota-loading">
            <Loading size={22} label={t("quota.pending")} />
          </div>
        </Section>
      );
    }
    return null;
  }

  // The guide only speaks to a vendor-window audience — budget rows are this
  // app's own caps, no OS banner is involved — and only while the OS would not
  // deliver one at all.
  const vendorLive = live.some((q) => (q.origin ?? "probe") !== "budget");
  const guide =
    notify !== null && notify !== "granted" && notify !== "unknown" && dismissedFor !== notify && vendorLive
      ? notify
      : null;

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
  // The strip rides the bottom of the overview page, so its last two rows flip
  // the click tip above the row — a below-tip there would leave the panel
  // (same rule as the ranks and sources lists).
  const flipTips = new Set(groups.flatMap((g) => g.rows).slice(-2).map(rowKey));
  // A plan suffix every window of a tool repeats ("5 小时 · GLM Coding Lite")
  // is group-level information: it rides the tool header once, and the rows
  // keep only their window names.
  const planOf = (rows: QuotaView[]): string => {
    // Ledger rows (今日额度 · 已用 X/Y) are their own tail and never vote —
    // a Start Plan bucket lives in every ZCode group, so requiring unanimous
    // tails disabled the hoist forever. The bucket carries a stable id, and
    // its gauge tail (X/Y, no 已用 verb) would otherwise poison the vote.
    const tails = rows
      .filter((q) => !windowName(q).includes("已用 ") && !q.id?.startsWith("start-plan:"))
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
      label={t("quota.section")}
      meta={scoped ? t("quota.meta.local") : t("quota.meta", { w: live.length, g: groups.length })}
      trail={
        <span className="quota-legend" aria-hidden="true">
          <span><i data-stage="low" />{t("quota.legend.low")}</span>
          <span><i data-stage="mid" />{t("quota.legend.mid")}</span>
          <span><i data-stage="high" />{t("quota.legend.high")}</span>
        </span>
      }
    >
      {!polling && live.length ? (
        <p className="quota-stopped" role="status">
          {t("quota.stopped", { age: freshness(live.reduce((m, q) => Math.max(m, q.sampled_at_ms), 0), now) })}
        </p>
      ) : null}
      {guide ? (
        <div className="quota-notify" role="status">
          <IconBell size={13} />
          <span className="q-note">{guide === "denied" ? t("quota.notify.denied") : t("quota.notify.ask")}</span>
          <button type="button" className="btn-quiet" onClick={enableNotify}>
            {guide === "denied" ? t("quota.notify.settings") : t("quota.notify.enable")}
          </button>
          <button
            type="button"
            className="sheet-close quota-notify-x"
            aria-label={t("quota.notify.dismiss")}
            onClick={dismissGuide}
          >
            <IconClose size={10} />
          </button>
        </div>
      ) : null}
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
                title={t("quota.drag")}
                onPointerDown={(e) => beginPress(e, { kind: "tool", key: g.tool })}
              >
                <ToolIcon tool={g.tool} size={18} />
                <span className="quota-tool">{toolDisplay(g.tool)}</span>
                {plan ? <span className="quota-plan">{plan}</span> : null}
                <span className="quota-origins">
                  {logged ? (
                    <span className="quota-badge" data-origin="log">
                      {ORIGIN("log")}
                    </span>
                  ) : null}
                </span>
                {CHECKIN_TOOLS.includes(g.tool) ? (
                  <span className="quota-checkin">
                    {groupCheckedIn(g.tool) || checkinClaimed.has(g.tool) ? (
                      <span className="checkin-badge">{t("quota.checkin.done")}</span>
                    ) : (
                      <button
                        type="button"
                        className="checkin-btn"
                        disabled={checkinBusy === g.tool || checkinWaiting(g.tool) || checkinGated(g.tool)}
                        title={checkinMsg[g.tool]}
                        onClick={async (e) => {
                          // The head is a drag handle; the button must not
                          // start a drag while it claims.
                          e.stopPropagation();
                          setCheckinBusy(g.tool);
                          try {
                            const { ok, message } = await bridge.checkinNow(g.tool);
                            setCheckinMsg((m) => ({ ...m, [g.tool]: message }));
                            // Only a real claim turns the button into the done
                            // badge; a reason (未到签到时间, 排队中) flashes on
                            // the button itself until the next sample refresh.
                            if (ok) setCheckinClaimed((done) => new Set(done).add(g.tool));
                            else
                              window.setTimeout(
                                () => setCheckinMsg((m) => ({ ...m, [g.tool]: undefined })),
                                5000,
                              );
                          } catch (err) {
                            setCheckinMsg((m) => ({ ...m, [g.tool]: String(err) }));
                          } finally {
                            setCheckinBusy((busy) => (busy === g.tool ? null : busy));
                          }
                        }}
                      >
                        {checkinBusy === g.tool
                          ? t("quota.checkin.busy")
                          : checkinGated(g.tool)
                            ? t("quota.checkin.gated")
                            : checkinWaiting(g.tool)
                              ? t("quota.checkin.wait")
                              : (checkinMsg[g.tool] ?? t("quota.checkin.btn"))}
                      </button>
                    )}
                  </span>
                ) : null}
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
                // A prepaid balance names its figure's unit in the row head
                // and drops the figure from the label. Where the figure sits
                // differs per row: DSH's balance replaces the percent (and its
                // track runs dashed — no cap is measured); FunIDE's points
                // keep the real percent and take the reset slot instead.
                // Values are the ENGINE-side label prefixes (the vendor text is
                // zh-authored) — they match, they do not display; the row head
                // renders the translated word via the dict below.
                const BALANCE_HEADS: Record<string, { mark: string; key: "quota.balance.dsh" | "quota.balance.funide" }> = {
                    "dsh-balance": { mark: "余额", key: "quota.balance.dsh" },
                    "funide-points": { mark: "积分", key: "quota.balance.funide" },
                };
                const balanceId = q.id ?? "";
                const balance = balanceId in BALANCE_HEADS;
                const figureInPct = balanceId === "dsh-balance";
                const figure = balance
                    ? (q.label ?? "").replace(new RegExp(`^${BALANCE_HEADS[balanceId].mark}\\s*`), "")
                    : null;
                const mine = (q.origin ?? "probe") === "budget";
                // The reset slot: value/unit pairs (see `until`) laid into the
                // column's fixed sub-columns, or a plain string for the two
                // non-grid shapes (a prepaid figure, the no-reset dash).
                let resetNode: React.ReactNode;
                if (balance && !figureInPct) {
                  resetNode = figure;
                } else if (q.resets_at_ms > 0) {
                  const u = until(q.resets_at_ms, now);
                  resetNode =
                    typeof u === "string"
                      ? u
                      : u.map(([v, unit], i) => (
                          <span className="rq" key={i}>
                            <span className="rq-v">{v}</span>
                            <span className="rq-u">{unit}</span>
                          </span>
                        ));
                } else {
                  // No countdown: a drawn bar, not the "—" glyph — the glyph's
                  // ink sits ~2px inside its advance width and read as an
                  // indent against the flush-right digits, however the cell
                  // was aligned. An element's edge is exact by construction.
                  resetNode = <span className="rq-dash" aria-label="无重置时间" />;
                }
                // The window name leads at full ink; a plan suffix the source
                // appended ("5 小时 · GLM Coding Lite") follows dimmed, so ten
                // rows of the same plan stop shouting it.
                const label = windowName(q);
                const cut = label.indexOf(" · ");
                const head = balance ? t(BALANCE_HEADS[balanceId].key) : cut === -1 ? label : label.slice(0, cut);
                const tail = balance ? "" : cut === -1 ? "" : label.slice(cut);
                // 已用 X/Y tails (the credit packs' spent side) live in the
                // tooltip only — rendered on every row they read as clutter
                // the user already asked to remove; `title={label}` keeps them.
                const rowTail =
                    tail === ` · ${plan}` || tail.startsWith(" · 已用") ? "" : tail;
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
                      {tipKey === rowKey(q) ? (
                        <span className={`rank-tip${flipTips.has(rowKey(q)) ? " up" : ""}`}>{label}</span>
                      ) : null}
                      {mine && (
                        <span className="quota-badge" data-origin="budget">
                          {ORIGIN("budget")}
                        </span>
                      )}
                    </span>
                    {unlimited ? (
                      <span className="track track-inf" aria-hidden="true" />
                    ) : figureInPct ? (
                      <span className="track track-balance" aria-hidden="true" />
                    ) : (
                      <span
                        className="track"
                        role="progressbar"
                        aria-valuenow={Math.round(pct)}
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-label={t("quota.bar.a11y", { tool: toolDisplay(q.tool), label, p: pct.toFixed(1) })}
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
                      <span className="quota-pct quota-pct--na">{t("quota.unlimited")}</span>
                    ) : figureInPct ? (
                      <span className="quota-pct quota-pct--na num">{figure}</span>
                    ) : (
                      <span className="quota-pct num" data-stage={stage}>
                        {pct > 100 ? Math.round(pct) : pct < 10 ? pct.toFixed(1) : Math.round(pct)}
                        <em>%</em>
                      </span>
                    )}
                    <span className="quota-reset num">{resetNode}</span>
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
