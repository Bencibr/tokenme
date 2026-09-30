/**
 * The brand spinner: two concentric half-rings spinning against each other —
 * the outer arc (bright) turns counter-clockwise while the inner arc (faint)
 * turns clockwise, both about the shared center. The panel's dual-ring mark
 * set in motion. CSS rotation; `prefers-reduced-motion` freezes both.
 *
 * Drop it wherever a wait is visible: the boot screen, a tab waiting on its
 * first data, anything that would otherwise read as a blank panel.
 */
export function Loading({ size = 30, label }: { size?: number; label?: string }) {
  return (
    <span className="loading" role="status" aria-label={label ?? "加载中"}>
      <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden="true">
        {/* outer half-ring (r=20, upper 160°) and inner half-ring (r=11, lower
            160°): concentric, offset half a turn, gaps at the sides. The CSS
            spins the outer CCW and the inner CW about the viewBox center. */}
        <path
          className="loading-arc-hi"
          d="M 4.3 20.53 A 20 20 0 0 1 43.7 20.53"
          strokeWidth="4"
          strokeLinecap="round"
        />
        <path
          className="loading-arc-lo"
          d="M 34.84 25.91 A 11 11 0 0 1 13.16 25.91"
          strokeWidth="3.5"
          strokeLinecap="round"
        />
      </svg>
      {label ? <span className="loading-label">{label}</span> : null}
    </span>
  );
}
