import { useState } from "react";
import type { SourceStatus, Summary } from "../types";
import { compactTokens, count, money } from "../lib/format";
import { IconMissing } from "./Icons";
import { MoreRow, Section } from "./Section";
import { ToolIcon } from "./ToolIcon";
import { useMoney } from "../lib/display";
import { t } from "../lib/i18n";

/**
 * Which logs were actually read. Undetected sources stay reachable rather than
 * deleted — hiding them would hide the only hint about how to make them appear —
 * but they move behind a toggle, because on a machine with nine of fourteen tools
 * installed the dead rows were the majority of the section's height.
 */
export function Sources({ sources, allTime }: { sources: SourceStatus[]; allTime: Summary }) {
  const [showMissing, setShowMissing] = useState(false);
  // One tooltip at a time, keyed on the row: the root paths truncate hard, and
  // hover titles exist but a non-activating panel is exactly where nobody
  // waits for a hover delay — same click-to-reveal idiom as the rank rows.
  const [tipKey, setTipKey] = useState<string | null>(null);
  const showMoney = useMoney();
  const detected = sources.filter((s) => s.detected);
  const missing = sources.filter((s) => !s.detected);
  const events = sources.reduce((acc, s) => acc + s.events_ingested, 0);
  const listed = showMissing ? [...detected, ...missing] : detected;

  return (
    <Section
      label={t("src.section")}
      meta={t("src.meta", { d: detected.length, s: sources.length, e: count(events) })}
    >
      <ol className="src-list">
        {listed.map((s, idx) => (
          <li
            className="src"
            key={s.id}
            data-off={!s.detected || undefined}
            onClick={() => setTipKey(tipKey === s.id ? null : s.id)}
            onMouseLeave={() => tipKey === s.id && setTipKey(null)}
          >
            {s.detected ? <ToolIcon tool={s.id} size={20} /> : <IconMissing size={12} className="src-icon" />}
            <span className="src-name">{s.display}</span>
            <span className="src-events num">{s.detected ? t("src.rows", { n: count(s.events_ingested) }) : t("src.missing")}</span>
            <div className="src-meta">
              <span className="src-root" title={s.roots.join("\n")}>
                {s.roots[0] ?? "—"}
                {s.roots.length > 1 ? ` +${s.roots.length - 1}` : ""}
              </span>
              {s.hint ? (
                <>
                  <span className="dot-sep" aria-hidden="true" />
                  <span>{s.hint}</span>
                </>
              ) : null}
            </div>
            {/* the tip anchors to the row, not to .src-root — src-meta clips
                its overflow and would behead the tip it carries. The bottom
                rows flip above the row so the tip clears the panel edge. */}
            {tipKey === s.id ? (
              <span className={`src-tip${idx >= listed.length - 2 ? " up" : ""}`}>
                {s.roots.join("\n") || "—"}
              </span>
            ) : null}
          </li>
        ))}
      </ol>
      <MoreRow
        open={showMissing}
        total={sources.length}
        preview={detected.length}
        onToggle={() => setShowMissing((v) => !v)}
        closedLabel={t("src.more.show", { n: missing.length })}
        openLabel={t("src.more.hide")}
      />
      <p className="src-total">
        {t("src.total")}
        <span className="num"> {compactTokens(allTime.total_tokens)}</span>
        <span> tokens</span>
        {showMoney ? (
          <>
            <span> ·</span>
            <span className="num"> {money(allTime.cost)}</span>
          </>
        ) : null}
        {allTime.credits > 0 ? (
          <>
            <span> · </span>
            <span className="num">{allTime.credits.toFixed(2)}</span>
            <span> credits</span>
          </>
        ) : null}
      </p>
    </Section>
  );
}
