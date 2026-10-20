import { t } from "../lib/i18n";
import { IconRefresh } from "./Icons";

/**
 * The quiet gauge that fronts the empty sheet. The brand dual-ring at rest
 * (same geometry as <Loading />, frozen: bright outer arc, faint inner) sits
 * inside a dotted orbit — the circle the first data points will complete —
 * with a few waiting dots on a wider orbit. Ambient, not busy: the dotted
 * ring creeps and the dots twinkle far slower than the spinner, and
 * `prefers-reduced-motion` freezes all of it. Purely decorative.
 */
function EmptyArt() {
  return (
    <div className="empty-art" aria-hidden="true">
      <svg width="180" height="150" viewBox="0 0 180 150" fill="none">
        <radialGradient id="empty-glow" gradientUnits="userSpaceOnUse" cx="90" cy="66" r="70">
          <stop className="empty-glow-stop" offset="0" stopOpacity="0.07" />
          <stop className="empty-glow-stop" offset="1" stopOpacity="0" />
        </radialGradient>
        <circle cx="90" cy="66" r="70" fill="url(#empty-glow)" />
        {/* the outer circle the first data will complete (dotted remainder
            under the solid brand arc); 28 dots close seamlessly around
            r=42, so the slow dashoffset march never pops at the seam */}
        <circle className="empty-orbit" cx="90" cy="66" r="42" />
        <path className="empty-arc-hi" d="M 48.64 58.71 A 42 42 0 0 1 131.36 58.71" />
        <path className="empty-arc-lo" d="M 112.75 70.01 A 23.1 23.1 0 0 1 67.25 70.01" />
        <circle className="empty-dot dot-1" cx="139.4" cy="37.5" r="2.1" />
        <circle className="empty-dot dot-2" cx="121" cy="12.3" r="1.7" />
        <circle className="empty-dot dot-3" cx="42.5" cy="32.7" r="2.2" />
        <circle className="empty-dot dot-4" cx="33.6" cy="86.5" r="1.6" />
        <circle className="empty-dot dot-5" cx="145.4" cy="86.2" r="1.8" />
      </svg>
    </div>
  );
}

/**
 * Nothing on this machine is readable yet: one quiet hero — ring, why it's
 * empty, and the one action out. Every tab renders this same screen; the
 * rescan swaps it for the loading line and back, empty or not.
 */
export function EmptyState({ onRefresh }: { onRefresh: () => void }) {
  return (
    <div className="empty">
      <EmptyArt />
      <p className="empty-title">{t("empty.title")}</p>
      <p className="empty-body">{t("empty.body")}</p>
      <button type="button" className="empty-cta" onClick={onRefresh}>
        <IconRefresh size={12} />
        {t("empty.cta")}
      </button>
    </div>
  );
}
