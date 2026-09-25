import { IconChevron } from "./Icons";

interface SectionProps {
  label: string;
  meta?: string;
  children: React.ReactNode;
}

/** A hairline-separated block. Deliberately not a card: no radius, no shadow. */
export function Section({ label, meta, children }: SectionProps) {
  return (
    <section className="sec">
      <div className="sec-hd">
        <h2 className="sec-label">{label}</h2>
        {meta ? <span className="sec-meta num">{meta}</span> : null}
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
      <span>{open ? openLabel ?? `只看前 ${preview} 项` : closedLabel ?? `查看全部 ${total} 项`}</span>
      {open ? null : <span className="more-n num">+{total - preview}</span>}
    </button>
  );
}
