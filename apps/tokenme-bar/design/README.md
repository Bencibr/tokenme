# icon sources

The shipped icons are generated from these SVGs, not drawn by hand in the
editor:

- `app-icon.svg` — dual usage rings (outer = 7-day window, inner = 5-hour
  window) on the brand teal gradient. Regenerate the full set with
  `pnpm tauri icon design/render/app-icon.png` from a 1024px PNG render of
  this file (chrome headless at 1024×1024 works fine).
- `tray-icon.svg` — the menu-bar glyph: the app icon's two progress arcs
  (outer 72%, inner 42%) on a 22pt grid. **Render with Chrome** (headless
  screenshot at 44×44, transparent background) — `rsvg-convert` ignores SVG2
  `pathLength`, so the dasharrays collapse into eight dot segments and the
  glyph stops being the logo. A 22×22 bitmap also renders blurry on retina;
  44×44 is the size muda expects and it is sized correctly in the menu bar.
  The Windows tray sizes (16/20/24/28/32, one per DPI step) come from this
  same SVG through `scripts/gen-tray-icons.py`, which injects absolute
  width/height into a temp copy per size — Edge lays an attribute-less SVG
  document out as if the viewport were wider than the window and clips the
  right side otherwise.

The teal in `src/styles/theme.css` tracks this gradient: light mode uses the
deep end `#0c7f6c`, dark mode the bright end `#2fc6a4`.
