import { useMemo, useState } from "react";
import type { SessionRow } from "../types";
import { compactTokens, count, money, relativeTime } from "../lib/format";
import { MoreRow, Section } from "./Section";
import { nextSortState, SortToggle, type SortState } from "./SortToggle";
import { ToolIcon } from "./ToolIcon";
import { useMoney } from "../lib/display";
import { t } from "../lib/i18n";

const BASENAME = (p: string) => p.replace(/\/+$/, "").split("/").pop() || p;

/** Six rows by default — "what did I just spend on", not a session browser — with
 *  the tail reachable rather than silently cut off. The sort toggle re-orders
 *  by tokens; time stays visible on every row either way. */
export function Sessions({ rows, now }: { rows: SessionRow[]; now: number }) {
  const [all, setAll] = useState(false);
  const [sort, setSort] = useState<SortState>("default");
  const showMoney = useMoney();
  const ordered = useMemo(() => {
    if (sort === "default") return rows;
    const dir = sort === "asc" ? 1 : -1;
    return [...rows].sort((a, b) => (a.total_tokens - b.total_tokens) * dir);
  }, [rows, sort]);
  const shown = ordered.slice(0, all ? ordered.length : 6);
  return (
    <Section
      label={t("sess.section")}
      meta={rows.length > shown.length ? t("sess.meta", { a: shown.length, b: rows.length }) : undefined}
      trail={rows.length > 1 ? <SortToggle state={sort} onCycle={() => setSort((v) => nextSortState(v))} /> : undefined}
    >
      {shown.length === 0 ? (
        <p className="empty-row">{t("sess.empty")}</p>
      ) : (
        <ol className="sess-list">
          {shown.map((s) => {
            const project = s.project ? BASENAME(s.project) : s.session.slice(0, 8);
            const billed =
              s.cost > 0 ? (showMoney ? money(s.cost) : "") : s.total_tokens === 0 ? null : t("sess.unpriced");
            return (
              <li className="sess" key={`${s.tool}/${s.session}`}>
                <ToolIcon tool={s.tool} size={20} />
                <div className="sess-main">
                  <div className="sess-line">
                    <span className="sess-name" title={s.project ?? s.session}>
                      {project}
                    </span>
                    <span
                      className="sess-cost num"
                      data-unpriced={billed === t("sess.unpriced") || undefined}
                      data-credit={billed === null || undefined}
                      title={billed === null ? t("sess.credit.tip") : undefined}
                    >
                      {billed ?? "credits"}
                    </span>
                  </div>
                  <div className="sess-meta">
                    <span className="sess-model" title={s.model}>
                      {s.model}
                    </span>
                    <span className="dot-sep" aria-hidden="true" />
                    <span className="num">{count(s.requests)}</span>
                    <span>{t("unit.times")}</span>
                    <span className="dot-sep" aria-hidden="true" />
                    <span className="num">{compactTokens(s.total_tokens)}</span>
                    <span className="grow" />
                    <span className="sess-when">{relativeTime(s.last_ms, now)}</span>
                  </div>
                </div>
              </li>
            );
          })}
        </ol>
      )}
      <MoreRow
        open={all}
        total={rows.length}
        preview={6}
        onToggle={() => setAll((v) => !v)}
        closedLabel={t("sess.more.all", { n: rows.length })}
        openLabel={t("sess.more.recent")}
      />
    </Section>
  );
}
