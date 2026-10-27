import React from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { bridge, inTauri, isWindows } from "./lib/bridge";
import "./styles/theme.css";
import "./styles/panel.css";
import "./styles/bubble.css";
import { getCurrentWindow } from "@tauri-apps/api/window";

document.body.dataset.env = inTauri ? "tauri" : "web";
document.body.dataset.platform = isWindows ? "windows" : "other";

// TokenMe is a utility surface, not a browser. WebView2's native context menu
// exposes Save As/Print/Refresh on the panel and the Windows bubble; neither
// surface has a user-facing right-click action.
if (isWindows) {
  document.addEventListener("contextmenu", (event) => event.preventDefault(), { capture: true });
}

// Cold-start telemetry, stage 1: how long the page took to get here. The second
// stage (real figures) is reported from App.tsx, where the note is retired.
if (inTauri && getCurrentWindow().label === "pet-notice") document.body.dataset.surface = "pet-notice";
else void bridge.reportBoot();

// A render-phase exception unmounts React and re-reveals the boot shell — from
// the outside that is indistinguishable from "still indexing". Route uncaught
// errors and rejections into panel.log so the crash names itself. A surface
// crash-looping here would spam the log at its own pace; the throttle keeps
// one line per distinct message per second.
const reported = new Map<string, number>();
const reportFrontendError = (kind: string, message: string) => {
  if (!inTauri) return;
  const key = `${kind}:${message}`;
  const last = reported.get(key) ?? 0;
  const now = Date.now();
  if (now - last < 1000) return;
  reported.set(key, now);
  void bridge.frontendError(`${kind}: ${message}`);
};
window.addEventListener("error", (e) => reportFrontendError("error", e.message || String(e.error)));
window.addEventListener("unhandledrejection", (e) => reportFrontendError("rejection", String(e.reason)));

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
