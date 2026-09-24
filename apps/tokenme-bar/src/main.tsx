import React from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { inTauri } from "./lib/bridge";
import "./styles/theme.css";
import "./styles/panel.css";

document.body.dataset.env = inTauri ? "tauri" : "web";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
