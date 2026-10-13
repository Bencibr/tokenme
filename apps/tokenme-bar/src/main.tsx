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

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
