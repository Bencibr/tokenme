import { t } from "../lib/i18n";
import { IconChevron } from "./Icons";

interface SectionProps {
  label: string;
  meta?: string;
  /** A compact control (e.g. the sort toggle) sitting beside the meta text. */
  trail?: React.ReactNode;
  /** Replaces the label/meta pair outright, for a section whose header is a
   *  control (the 活动 section's 今日/活动 view seg). */
  head?: React.ReactNode;
  children: React.ReactNode;
  /** Default true. False drops the section's bottom hairline — for a section
      whose body is a transient wait, where the line would strand in blank
      panel space instead of separating two blocks of content. */
  divider?: boolean;
}

/** A hairline-separated block. Deliberately not a card: no radius, no shadow. */
export function Section({ label, meta, trail, head, children, divider = true }: SectionProps) {
  return (
    <section className={divider ? "sec" : "sec sec-nodiv"}>
      <div className="sec-hd">
        {head ?? (
          <>
            <h2 className="sec-label">{label}</h2>
            {meta || trail ? (
              <span className="sec-right">
                {trail}
                {meta ? <span className="sec-meta num">{meta}</span> : null}
              </span>
            ) : null}
          </>
        )}
      </div>
      {children}
    </section>
  );
}

interface MoreProps {
  open: boolean;
  /** Everything the list has, including what the preview hides. */
  total: number;
  /** How many the collapsed list shows. */
  preview: number;
  onToggle: () => void;
  /** Non-count semantics: the 数据源 tail hides "undetected", not "more items". */
  closedLabel?: string;
  openLabel?: string;
}

/**
 * The one affordance for a truncated list, so no section ever quietly withholds
 * rows. It carries the hidden count before the click — "另 13" answers *how much*
 * is behind it, which a bare "更多" would not.
 */
export function MoreRow({ open, total, preview, onToggle, closedLabel, openLabel }: MoreProps) {
  if (total <= preview) return null;
  return (
    <button type="button" className="more-row" aria-expanded={open} onClick={onToggle}>
      <IconChevron size={12} className="more-icon" />
      <span>{open ? openLabel ?? t("more.open", { n: preview }) : closedLabel ?? t("more.close", { n: total })}</span>
      {open ? null : <span className="more-n num">+{total - preview}</span>}
    </button>
  );
}
