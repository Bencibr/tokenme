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

export function IconMissing(p: IconProps) {
  return (
    <Svg {...p}>
      <path d="M4 7.5h16v12H4z" />
      <path d="M4 7.5l3-3h10l3 3" />
      <path d="M9.5 13h5" />
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
