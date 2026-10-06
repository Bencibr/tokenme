import { useEffect, useRef, type ReactNode } from "react";
import type { MachineScope, MachineView, SyncRecord } from "../types";
import { compactTokens, relativeTime } from "../lib/format";
import { t } from "../lib/i18n";
import { IconChevron, IconWarn } from "./Icons";

/** Remote colour slots, keyed by name order (machines arrive name-sorted).
 *  The local machine keeps the accent; a colour never carries meaning alone —
 *  every dot rides next to the machine's own name. */
const SLOTS = ["var(--cat-2)", "var(--cat-4)", "var(--cat-7)", "var(--cat-8)", "var(--cat-3)", "var(--cat-6)"];

/** The line the header badge draws: an import older than this reads amber. */
const STALE_MS = 24 * 3_600_000;

/** No server feature in use: nothing to contrast, so no manual-import tags. */
const NO_ORIGINS: Set<string> = new Set();

function share(part: number, whole: number): string {
  return whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : "0%";
}

interface MachineRow {
  /** Raw origin; `""` is this machine and never collides with an origin. */
  origin: string;
  /** What the row shows: "本机" for the local row, the origin itself otherwise. */
  name: string;
  local: boolean;
  tokens: number;
  color: string;
  importedAt: number | null;
  age: string | null;
  stale: boolean;
}

interface Props {
  /** The scope the current report was folded under (its echoed value). */
  scope: MachineScope;
  onScope: (scope: MachineScope) => void;
  /** `Report.machines` — `""` first (this machine), then origins by name. */
  machines: MachineView[];
  syncs?: SyncRecord[];
  now: number;
  /** Open state lives in App so Escape closes menu → sheet → panel in order. */
  open: boolean;
  onOpen: (open: boolean) => void;
  /** Origins backed by a configured server. Origins outside a non-empty set
   *  arrived by a manual export/import and are tagged; an empty set shows no
   *  tags at all. */
  serverOrigins?: Set<string>;
}

/** The one new control: switching scope re-folds the whole panel. Renders
 *  nothing when the index holds no imported origins — the panel then looks
 *  exactly the way it did before this feature existed. */
export function ScopeDropdown({
  scope,
  onScope,
  machines,
  syncs,
  now,
  open,
  onOpen,
  serverOrigins = NO_ORIGINS,
}: Props) {
  const rootRef = useRef<HTMLDivElement | null>(null);

  // Outside click closes; Escape is ordered by App.tsx.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) onOpen(false);
    };
    window.addEventListener("pointerdown", onDown);
    return () => window.removeEventListener("pointerdown", onDown);
  }, [open, onOpen]);

  const newest = new Map<string, SyncRecord>();
  for (const r of syncs ?? []) {
    const cur = newest.get(r.origin);
    if (!cur || r.imported_at_ms > cur.imported_at_ms) newest.set(r.origin, r);
  }

  let slot = 0;
  const rows: MachineRow[] = machines.map((m) => {
    const local = m.origin === "";
    const rec = local ? undefined : newest.get(m.origin);
    const importedAt = rec ? rec.imported_at_ms : null;
    return {
      origin: m.origin,
      name: local ? t("scope.local") : m.origin,
      local,
      tokens: m.today_tokens,
      color: local ? "var(--accent)" : SLOTS[slot++ % SLOTS.length],
      importedAt,
      age: importedAt === null ? null : relativeTime(importedAt, now),
      stale: importedAt !== null && now - importedAt > STALE_MS,
    };
  });
  const remotes = rows.filter((r) => !r.local);
  if (remotes.length === 0) return null;

  const total = rows.reduce((a, r) => a + r.tokens, 0);
  const remoteTokens = remotes.reduce((a, r) => a + r.tokens, 0);
  const remotePct = total > 0 ? (remoteTokens / total) * 100 : 0;
  const remoteShare = share(remoteTokens, total);
  const anyStale = remotes.some((r) => r.stale);
  const selected = scope.kind === "origin" ? remotes.find((r) => r.origin === scope.name) : undefined;
  // 最近同步 = the most recent successful import among the remotes.
  const lastSync = remotes.reduce<MachineRow | null>(
    (best, r) => (r.importedAt !== null && (best === null || (best.importedAt ?? 0) < r.importedAt) ? r : best),
    null,
  );

  const pick = (next: MachineScope) => {
    onScope(next);
    onOpen(false);
  };

  const cluster = (
    <span className="o-cluster" aria-hidden="true">
      {rows.slice(0, 3).map((r) => (
        <i key={r.origin} style={{ background: r.color }} />
      ))}
    </span>
  );

  let body: ReactNode;
  if (scope.kind === "all") {
    body = (
      <>
        {cluster}
        <span className="scope-name">{t("scope.all")}</span>
        <span className="scope-supp">
          {t("scope.remote")} <b className="num">{Math.round(remotePct)}%</b>
        </span>
        {anyStale ? (
          <span className="o-warn" title={t("scope.warn.tip")}>
            <IconWarn size={10} />
          </span>
        ) : null}
      </>
    );
  } else if (scope.kind === "local") {
    const me = rows.find((r) => r.local);
    body = (
      <>
        <span className="o-dot" style={{ background: "var(--accent)" }} aria-hidden="true" />
        <span className="scope-name">{t("scope.local")}</span>
        <span className="scope-supp">{share(me?.tokens ?? 0, total)}</span>
      </>
    );
  } else {
    body = (
      <>
        <span className="o-dot" style={{ background: selected?.color ?? "var(--ink-3)" }} aria-hidden="true" />
        <span className="scope-name">{scope.name}</span>
        <span className="scope-supp" data-warn={selected?.stale || undefined}>
          {selected ? `${share(selected.tokens, total)} · ${selected.age ?? "—"}` : "—"}
        </span>
      </>
    );
  }

  return (
    <div className="scope-row" ref={rootRef}>
      <button
        type="button"
        className="scope-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        data-open={open || undefined}
        onClick={() => onOpen(!open)}
      >
        {body}
        <IconChevron size={9} className="scope-chev" />
      </button>

      {open ? (
        <div className="scope-menu" role="menu" aria-label={t("scope.a11y")}>
          <button
            type="button"
            role="menuitemradio"
            aria-checked={scope.kind === "all"}
            className="scope-item"
            onClick={() => pick({ kind: "all" })}
          >
            <span className="si-check">{scope.kind === "all" ? "✓" : ""}</span>
            {cluster}
            <span className="si-name">
              <span className="scope-name">{t("scope.all")}</span>
            </span>
            <span className="si-vals">
              <span className="si-tok num">{compactTokens(total)}</span>
            </span>
          </button>

          {rows.map((r) => {
            const checked = r.local ? scope.kind === "local" : scope.kind === "origin" && scope.name === r.origin;
            const manual = !r.local && serverOrigins.size > 0 && !serverOrigins.has(r.origin);
            return (
              <button
                key={r.origin}
                type="button"
                role="menuitemradio"
                aria-checked={checked}
                className="scope-item"
                onClick={() => pick(r.local ? { kind: "local" } : { kind: "origin", name: r.origin })}
              >
                <span className="si-check">{checked ? "✓" : ""}</span>
                <span className="o-dot" style={{ background: r.color }} aria-hidden="true" />
                <span className="si-name">
                  <span className="scope-name">{r.name}</span>
                  {manual ? <span className="si-tag">{t("scope.manual")}</span> : null}
                </span>
                <span className="si-vals">
                  <span className="si-tok num">{compactTokens(r.tokens)}</span>
                  <span className="si-share num">{share(r.tokens, total)}</span>
                  {r.local ? null : (
                    <span className="si-age num" data-stale={r.stale || undefined}>
                      {r.age ?? "—"}
                    </span>
                  )}
                </span>
              </button>
            );
          })}

          <div className="scope-foot">
            <span>
              {t("scope.foot.remote", { n: remotes.length })} · {t("scope.foot.total")}{" "}
              <b className="num">{remoteShare}</b>
            </span>
            <span className="num">{t("scope.foot.last", { age: lastSync?.age ?? "—" })}</span>
          </div>
        </div>
      ) : null}
    </div>
  );
}
