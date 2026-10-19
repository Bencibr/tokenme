interface IconProps {
  size?: number;
  className?: string;
}

/** Stroke 1.5, no icon fonts, no CDN. 24-unit grid, scaled by `size`. */
function Svg({ size = 14, className, children }: IconProps & { children: React.ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export function IconRefresh(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M20 11a8 8 0 1 0-2.3 6.2" />
      <path d="M20 5v6h-6" />
    </Svg>
  );
}

export function IconArrowUp(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M12 19V5" />
      <path d="M6 11l6-6 6 6" />
    </Svg>
  );
}

/** Header's "back to the previous page". */
/** Points down when a list is folded; `.more-row[aria-expanded="true"]` turns it. */
export function IconChevron(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M6 9l6 6 6-6" />
    </Svg>
  );
}

export function IconArrowDown(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M12 5v14" />
      <path d="M6 13l6 6 6-6" />
    </Svg>
  );
}

export function IconFlat(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M5 12h14" />
    </Svg>
  );
}

export function IconWarn(p: IconProps) {
  return (
    <Svg {...p}>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 8v5" />
      <path d="M12 16h.01" />
    </Svg>
  );
}

/** The quota section's notification guide. */
export function IconBell(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M12 4.5a5 5 0 0 0-5 5V13l-1.6 2.6h13.2L17 13V9.5a5 5 0 0 0-5-5z" />
      <path d="M10.4 18.4a1.6 1.6 0 0 0 3.2 0" />
    </Svg>
  );
}

export function IconMissing(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M4 7.5h16v12H4z" />
      <path d="M4 7.5l3-3h10l3 3" />
      <path d="M9.5 13h5" />
    </Svg>
  );
}
/** The speed page's about badge: an exclamation alone — the button's own
 *  circular border is the circle, so the glyph stays one mark, not two. */
export function IconInfo(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M12 5.5v8" />
      <path d="M12 17.5v.2" />
    </Svg>
  );
}

/** Status bar's settings gear. */
export function IconSettings(p: IconProps) {
  return (
    <Svg {...p}>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h0a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h0a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v0a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </Svg>
  );
}

/** The settings sheet's close affordance. */
export function IconClose(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M6 6l12 12" />
      <path d="M18 6L6 18" />
    </Svg>
  );
}

/** 排序三态:未排序(上下双箭头)。 */
export function IconSortDefault(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="m21 16-4 4-4-4" />
      <path d="M17 20V4" />
      <path d="m3 8 4-4 4 4" />
      <path d="M7 2v16" />
    </Svg>
  );
}

/** 升序(向上箭头 + 递减横线)。 */
export function IconSortAsc(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="m3 8 4-4 4 4" />
      <path d="M7 4v16" />
      <path d="M13 6h8" />
      <path d="M13 12h6" />
      <path d="M13 18h4" />
    </Svg>
  );
}

/** 降序(向下箭头 + 递减横线)。 */
export function IconSortDesc(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="m3 16 4 4 4-4" />
      <path d="M7 4v16" />
      <path d="M13 6h8" />
      <path d="M13 12h6" />
      <path d="M13 18h4" />
    </Svg>
  );
}

/** Status bar's remote-server entry: a stacked 1U-box rack. */
export function IconServer(p: IconProps) {
  return (
    <Svg {...p}>
      <rect x="4" y="4" width="16" height="7" rx="2" />
      <rect x="4" y="13" width="16" height="7" rx="2" />
      <circle cx="8" cy="7.5" r="0.7" fill="currentColor" stroke="none" />
      <circle cx="8" cy="16.5" r="0.7" fill="currentColor" stroke="none" />
    </Svg>
  );
}

/** Status bar's local / remote device management entry. */
export function IconDevices(p: IconProps) {
  return (
    <Svg {...p}>
      <rect x="3.5" y="5" width="10.5" height="14" rx="1.8" />
      <rect x="14" y="8" width="6.5" height="10" rx="1.5" />
      <path d="M7 16.5h3.5" />
      <path d="M16 15h2.5" />
    </Svg>
  );
}

/** List rows' "open detail" affordance: a forward chevron. */
export function IconNext(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M9 6l6 6-6 6" />
    </Svg>
  );
}

/** Wizard back-step / detail-sheet return: a backward chevron. */
export function IconBack(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M15 6l-6 6 6 6" />
    </Svg>
  );
}

/** Progress rows: the check inside a filled disc. */
export function IconCheck(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M5 12.5l4.5 4.5L19 7.5" />
    </Svg>
  );
}

/** Device sync setup: the primary role card (lucide radio-tower). */
export function IconRadioTower(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M4.9 16.1C1 12.2 1 5.8 4.9 1.9" />
      <path d="M7.8 4.7a6.14 6.14 0 0 0-.8 7.5" />
      <circle cx="12" cy="9" r="2" />
      <path d="M16.2 4.8c2 2 2.26 5.11.8 7.47" />
      <path d="M19.1 1.9a9.96 9.96 0 0 1 0 14.1" />
      <path d="M9.5 18h5" />
      <path d="m8 22 4-11 4 11" />
    </Svg>
  );
}

/** Device sync setup: the secondary role card (lucide monitor-down). */
export function IconMonitorDown(p: IconProps) {
  return (
    <Svg {...p}>
      <rect width="20" height="14" x="2" y="3" rx="2" />
      <path d="M12 13V7" />
      <path d="m15 10-3 3-3-3" />
      <path d="M12 17v4" />
      <path d="M8 21h8" />
    </Svg>
  );
}

/** A compact OS mark for paired-device rows. */
export function IconDevicePlatform({ os, ...p }: IconProps & { os: string }) {
  const platform = os.toLowerCase();
  if (platform.includes("windows")) {
    return (
      <Svg {...p}>
        <path d="m3 5.2 7.5-1.1v7H3z" />
        <path d="m12 3.9 9-1.3v8.5h-9z" />
        <path d="M3 12.9h7.5v7L3 18.8z" />
        <path d="m12 12.9 9 .1v8.4l-9-1.3z" />
      </Svg>
    );
  }
  if (platform.includes("mac") || platform.includes("darwin")) {
    return (
      <Svg {...p}>
        <path d="M14.8 8c-1.1 0-1.8.6-2.8.6S10.2 8 9.2 8c-2.3 0-4.4 1.9-4.4 5.1 0 3.6 2.2 7.5 4.4 7.5.9 0 1.4-.6 2.8-.6s1.9.6 2.8.6c2.2 0 4.4-3.8 4.4-7.5 0-3.2-2.1-5.1-4.4-5.1z" />
        <path d="M12.6 6.8c.1-1.9 1.5-3.2 3.4-3.4.1 1.7-1.2 3.2-3.4 3.4z" />
      </Svg>
    );
  }
  if (platform.includes("linux")) {
    return (
      <Svg {...p}>
        <path d="M12 3.5c-2.4 0-4.2 2.1-4.2 5v4.4c-2.1 1.1-3.4 3.1-3.4 5.2 0 1 1 1.5 1.9.9l2-1.2h7.4l2 1.2c.9.6 1.9.1 1.9-.9 0-2.1-1.3-4.1-3.4-5.2V8.5c0-2.9-1.8-5-4.2-5z" />
        <path d="M9.5 8.5h.01m4.98 0h.01m-4.7 3 2.2 1.2 2.2-1.2m-4.5 5.8-.8 2m5.1-2 .8 2" />
      </Svg>
    );
  }
  return (
    <Svg {...p}>
      <rect width="18" height="13" x="3" y="3" rx="1.5" />
      <path d="M9 20h6m-3-4v4" />
    </Svg>
  );
}
