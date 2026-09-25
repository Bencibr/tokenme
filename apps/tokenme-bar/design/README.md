# icon sources

The shipped icons are generated from these SVGs, not drawn by hand in the
editor:

- `app-icon.svg` — dual usage rings (outer = 7-day window, inner = 5-hour
  window) on the brand teal gradient. Regenerate the full set with
  `pnpm tauri icon design/render/app-icon.png` from a 1024px PNG render of
  this file (chrome headless at 1024×1024 works fine).
- `tray-icon.svg` — the menu-bar glyph: the same dual usage rings as the app
  icon, drawn on a 22pt grid (outer 72% arc, inner 42% arc). Render at 352px
  and downscale to exactly 22×22; a larger file would render oversized in the
  menu bar.

The teal in `src/styles/theme.css` tracks this gradient: light mode uses the
deep end `#0c7f6c`, dark mode the bright end `#2fc6a4`.
