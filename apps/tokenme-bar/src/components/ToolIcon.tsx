import { createContext, useContext, useEffect, useState } from "react";
import { bridge } from "../lib/bridge";
import { toolColor, toolDisplay } from "../lib/format";

const Icons = createContext<Record<string, string>>({});

/**
 * The provider owns the one fetch: reading every `.app` bundle's icon off disk is
 * file I/O worth doing once per panel rather than once per row. An empty map is a
 * normal answer — on a machine with none of these apps installed, or outside
 * macOS, every row simply takes the monogram branch.
 */
export function IconProvider({ children }: { children: React.ReactNode }) {
  const [icons, setIcons] = useState<Record<string, string>>({});
  useEffect(() => {
    let live = true;
    bridge
      .toolIcons()
      .then((map) => live && setIcons(map))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  return <Icons.Provider value={icons}>{children}</Icons.Provider>;
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
  const src = useContext(Icons)[tool];
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
