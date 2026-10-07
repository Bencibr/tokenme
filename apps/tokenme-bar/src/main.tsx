import React from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { bridge, inTauri } from "./lib/bridge";
import "./styles/theme.css";
import "./styles/panel.css";
import "./styles/bubble.css";

document.body.dataset.env = inTauri ? "tauri" : "web";

// Cold-start telemetry, stage 1: how long the page took to get here. The second
// stage (real figures) is reported from App.tsx, where the note is retired.
void bridge.reportBoot();

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
