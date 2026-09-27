import { RELEASE_PAGE_URL, UPDATE_MANIFEST_URL } from "./about";

export interface UpdateInfo {
  latest: string;
  url: string;
}

/** x.y.z compare; a current version that is not a release (dev) never updates. */
export function isNewer(latest: string, current: string): boolean {
  const parse = (v: string) => v.trim().replace(/^v/, "").split(".").map((n) => Number.parseInt(n, 10) || 0);
  const [a, b, c] = parse(latest);
  const [x, y, z] = parse(current);
  if ([a, b, c, x, y, z].some((n) => Number.isNaN(n))) return false;
  return a !== x ? a > x : b !== y ? b > y : c > z;
}

/**
 * One quiet GET at panel boot. The manifest is three fields —
 * `{"version":"x.y.z","url":"https://…"}` — hosted anywhere a URL can point
 * at (GitHub raw, Gitea raw, a Qiniu bucket for mainland reachability); any
 * failure answers null and the panel stays silent about it.
 */
export async function checkForUpdate(current: string): Promise<UpdateInfo | null> {
  try {
    const res = await fetch(UPDATE_MANIFEST_URL, {
      signal: AbortSignal.timeout(4000),
      headers: { accept: "application/json" },
    });
    if (!res.ok) return null;
    const body = (await res.json()) as { version?: string; url?: string };
    const latest = body.version?.trim() ?? "";
    if (!latest || !isNewer(latest, current)) return null;
    return { latest, url: body.url || RELEASE_PAGE_URL };
  } catch {
    return null;
  }
}
