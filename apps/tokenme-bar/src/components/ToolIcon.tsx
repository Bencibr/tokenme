import { createContext, useContext, useEffect, useState } from "react";
import { bridge } from "../lib/bridge";
import { toolColor, toolDisplay } from "../lib/format";

type IconSet = { sources: Record<string, string>; tints: Record<string, string> };
const Icons = createContext<IconSet>({ sources: {}, tints: {} });

function dominantTint(src: string): Promise<string | null> {
  return new Promise((resolve) => {
    const image = new Image();
    image.onload = () => {
      try {
        const canvas = document.createElement("canvas");
        canvas.width = canvas.height = 32;
        const context = canvas.getContext("2d", { willReadFrequently: true });
        if (!context) { resolve(null); return; }
        context.drawImage(image, 0, 0, 32, 32);
        const pixels = context.getImageData(0, 0, 32, 32).data;
        const hues = Array.from({ length: 12 }, () => ({ weight: 0, r: 0, g: 0, b: 0 }));
        let opaque = 0;
        let chromatic = 0;
        for (let i = 0; i < pixels.length; i += 4) {
          const alpha = pixels[i + 3] / 255;
          if (alpha < 0.25) continue;
          opaque++;
          const r = pixels[i], g = pixels[i + 1], b = pixels[i + 2];
          const high = Math.max(r, g, b), low = Math.min(r, g, b), delta = high - low;
          if (high < 72 || delta < 48 || delta / Math.max(1, high) < 0.3) continue;
          chromatic++;
          let hue = high === r
            ? 60 * (((g - b) / delta) % 6)
            : high === g ? 60 * ((b - r) / delta + 2) : 60 * ((r - g) / delta + 4);
          if (hue < 0) hue += 360;
          const bin = hues[Math.floor(hue / 30) % hues.length];
          const weight = alpha * (delta / high);
          bin.weight += weight;
          bin.r += r * weight;
          bin.g += g * weight;
          bin.b += b * weight;
        }
        if (!opaque || chromatic / opaque < 0.12) { resolve(null); return; }
        const dominant = hues.reduce((best, entry) => entry.weight > best.weight ? entry : best);
        if (!dominant.weight) { resolve(null); return; }
        const hex = (value: number) => Math.round(value / dominant.weight).toString(16).padStart(2, "0");
        resolve(`#${hex(dominant.r)}${hex(dominant.g)}${hex(dominant.b)}`);
      } catch {
        // Tainted or unsupported images keep the established tool palette.
        resolve(null);
      }
    };
    image.onerror = () => resolve(null);
    image.src = src;
  });
}

/**
 * The provider owns the one fetch: reading every `.app` bundle's icon off disk is
 * file I/O worth doing once per panel rather than once per row. An empty map is a
 * normal answer — on a machine with none of these apps installed, or outside
 * macOS, every row simply takes the monogram branch.
 */
export function IconProvider({ children }: { children: React.ReactNode }) {
  const [icons, setIcons] = useState<IconSet>({ sources: {}, tints: {} });
  useEffect(() => {
    let live = true;
    bridge
      .toolIcons()
      .then((map) => {
        if (!live) return;
        setIcons({ sources: map, tints: {} });
        void Promise.all(Object.entries(map).map(async ([tool, src]) => [tool, await dominantTint(src)] as const))
          .then((entries) => {
            if (!live) return;
            setIcons({ sources: map, tints: Object.fromEntries(entries.filter((entry): entry is readonly [string, string] => entry[1] !== null)) });
          });
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  return <Icons.Provider value={icons}>{children}</Icons.Provider>;
}

export function useToolIconTints(): Record<string, string> {
  return useContext(Icons).tints;
}

/**
 * A tool's own application icon, or a coloured monogram where there is none.
 *
 * Codex borrows the ChatGPT app's icon, Cline ships one inside the panel, and
 * the rest of the CLI-only tools (OpenCode, Pi, AtomCode…) have no bundle on
 * disk, so they keep a letter. The row still reads its own name: this is
 * decoration on top of the label, never in place of it, and a data URL that
 * will not decode falls back through `onError`.
 */
export function ToolIcon({ tool, size = 22 }: { tool: string; size?: number }) {
  const src = useContext(Icons).sources[tool];
  const [broken, setBroken] = useState(false);
  const style = { "--c": toolColor(tool) } as React.CSSProperties;
  if (src && !broken) {
    return (
      <img
        className="tool-ic"
        src={src}
        width={size}
        height={size}
        loading="lazy"
        decoding="async"
        alt=""
        onError={() => setBroken(true)}
      />
    );
  }
  return (
    <span
      className="tool-mono"
      style={{ ...style, width: size, height: size, fontSize: Math.round(size * 0.5) }}
      aria-hidden="true"
    >
      {toolDisplay(tool).charAt(0).toUpperCase()}
    </span>
  );
}
