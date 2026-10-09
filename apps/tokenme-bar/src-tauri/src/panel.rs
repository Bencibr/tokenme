//! The panel window: where it appears under the tray icon, and how it leaves.
//!
//! macOS converts the webview window into a non-activating `NSPanel` so it can
//! float over full-screen spaces without stealing focus, and anchors it with the
//! tray rect from the click event — the positioner plugin cannot position a
//! swizzled panel, so that anchor math lives here. Windows uses the tray rect
//! plus the OS work area, and both platforms clamp back onto a connected
//! monitor.

// The cocoa/objc bindings `tauri-nspanel` re-exports are superseded by objc2 but
// are still the only thing that crate speaks; the noise is not ours to fix.
#![allow(deprecated)]
#![allow(unexpected_cfgs)]

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, Position, Rect, Size,
    WebviewWindow, Window, WindowEvent,
};
#[cfg(target_os = "windows")]
use tauri::PhysicalSize;

use crate::engine::PERIOD_EVENT;
use crate::tray;
#[cfg(target_os = "windows")]
use crate::bubble;

pub const LABEL: &str = "main";
const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 660.0;
/// The panel never shrinks below this, even on a tiny work area: the header,
/// a few rows and the status bar stay usable, and the page scrolls for the rest.
const MIN_HEIGHT: f64 = 420.0;
/// Breathing room between the tray/taskbar and the panel edge.
const GAP: f64 = 8.0;
/// Ignore repeated visibility requests while the native window state catches up.
const VISIBILITY_DEBOUNCE_MS: u64 = 160;
/// A focus-loss event can be emitted as the panel is being shown. Do not let it
/// immediately undo the show request.
const SHOW_FOCUS_GRACE_MS: u64 = 220;

static LAST_VISIBILITY_REQUEST_MS: AtomicU64 = AtomicU64::new(0);
static SHOW_FOCUS_GRACE_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "windows")]
static CLICK_AWAY_MONITOR_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

#[cfg(target_os = "windows")]
fn last_tray_bounds() -> &'static std::sync::Mutex<Option<Bounds>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<Bounds>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

/// Keep the exact physical tray rectangle from the click event. The Windows
/// click-away poll runs on mouse-down, before Tauri delivers the tray
/// mouse-up event, and `TrayIcon::rect()` can still be stale or empty there.
/// Without this cache one tray click is misclassified as an outside click and
/// toggles the panel twice.
#[cfg(target_os = "windows")]
pub(crate) fn remember_tray_rect(rect: &Rect) {
    let (left, top) = rect_origin(rect);
    let (width, height) = rect_size(rect);
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    if let Ok(mut slot) = last_tray_bounds().lock() {
        *slot = Some(Bounds {
            left,
            top,
            right: left + width,
            bottom: top + height,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenArea {
    monitor: Bounds,
    work: Bounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Rapid tray clicks deliver Click + DoubleClick events that would toggle the
/// panel two-plus times per gesture; toggles inside this window are swallowed.
const TOGGLE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

fn last_toggle() -> &'static std::sync::Mutex<Option<std::time::Instant>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

/// True when this toggle was swallowed by the debounce.
fn debounced() -> bool {
    let mut slot = last_toggle().lock().unwrap();
    let now = std::time::Instant::now();
    let skip = slot.map(|t| now.duration_since(t) < TOGGLE_DEBOUNCE).unwrap_or(false);
    if !skip {
        *slot = Some(now);
    }
    skip
}

pub fn toggle(app: &AppHandle, rect: Option<Rect>) {
    if debounced() {
        return;
    }
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    // On Windows the native surface is the source of truth. The WebView2 child
    // can be hidden while the Tauri host HWND still reports visible, so use
    // both the native state and our visibility flag before choosing a branch.
    let hidden = {
        #[cfg(target_os = "windows")]
        {
            panel_hidden().load(Ordering::Acquire) || !native_window_visible(&window)
        }
        #[cfg(not(target_os = "windows"))]
        {
            panel_hidden().load(Ordering::Acquire)
        }
    };
    if hidden {
        if accept_visibility_request(true) {
            show_window(&window, rect.or_else(|| tray::last_rect(app)));
        }
    } else {
        request_visibility(&window, false);
    }
}

pub fn show(app: &AppHandle, rect: Option<Rect>) {
    crate::logging::info(&format!("panel: show requested (rect present: {})", rect.is_some()));
    if !accept_visibility_request(true) {
        crate::logging::info("panel: show refused by the visibility debounce");
        return;
    }
    let Some(window) = app.get_webview_window(LABEL) else {
        crate::logging::error("panel: show has no main window to reveal");
        return;
    };
    show_window(&window, rect.or_else(|| tray::last_rect(app)));
}

/// Hide the panel through the same state path as click-away dismissal. The
/// frontend must not call `WebviewWindow::hide` directly: that leaves the
/// Windows native visibility and `panel_hidden()` disagreeing.
pub fn hide(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    hide_window(&window);
}

fn show_window(window: &WebviewWindow, rect: Option<Rect>) {
    #[cfg(target_os = "windows")]
    {
        let surface = window.clone();
        let _ = window.run_on_main_thread(move || show_window_on_ui(&surface, rect));
    }
    #[cfg(not(target_os = "windows"))]
    show_window_on_ui(window, rect);
}

fn show_window_on_ui(window: &WebviewWindow, rect: Option<Rect>) {
    let was_hidden = panel_hidden().swap(false, Ordering::Release);
    // `set_focusable` is Windows-only, so on macOS and Linux nothing reads the
    // flag this swap returns — the warning is the proof, not the bug.
    #[cfg(not(target_os = "windows"))]
    let _ = was_hidden;
    #[cfg(target_os = "windows")]
    let was_hidden = was_hidden || !native_window_visible(window);
    #[cfg(target_os = "windows")]
    if was_hidden {
        // A keyboard session can outlive the panel's hide path (the webview
        // does not always see a blur when the window goes away under it), so
        // every fresh open starts non-activating again — with the window
        // focusable, tao's post-first-show `SW_SHOW` would foreground it.
        let _ = window.set_focusable(false);
    }
    // Tauri owns both visibility transitions. A native SWP_SHOWWINDOW used to
    // leave tao's VISIBLE flag false, so window.hide() became a no-op and only
    // the WebView disappeared. Native positioning is geometry-only: it keeps
    // the verified physical-pixel placement without creating a second owner.
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_TOPMOST, SWP_NOCOPYBITS, SWP_NOACTIVATE,
        };
        if let Err(error) = window.show() {
            panel_hidden().store(true, Ordering::Release);
            crate::logging::error(&format!("panel: host show failed: {error}"));
            return;
        }
        match window.hwnd() {
            Ok(hwnd) => {
                let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd.0) }
                    .max(96) as f64;
                let scale = dpi / 96.0;
                let (ax, ay) = anchor_point_for(rect.or_else(|| tray::last_rect(window.app_handle())), window);
                let area = windows_screen_area(ax, ay).unwrap_or_else(|| crate::panel::fallback_area());
                let height = panel_height_for_work_area(area.work.bottom - area.work.top, scale);
                let w = WIDTH * scale;
                let h = height * scale;
                let (x, y) = clamp_in_bounds(ax - w / 2.0, ay + GAP, (w, h), area.work, GAP);
                let ok = unsafe {
                    SetWindowPos(
                        hwnd.0,
                        HWND_TOPMOST,
                        x.round() as i32,
                        y.round() as i32,
                        w.round() as i32,
                        h.round() as i32,
                        SWP_NOACTIVATE | SWP_NOCOPYBITS,
                    )
                };
                crate::logging::info(&format!(
                    "panel: native position — SetWindowPos({x}, {y}, {w}x{h}) ok={ok}"
                ));
            }
            Err(_) => {
                let _ = window.show();
                anchor(window, rect);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        anchor(window, rect);
        let anchored_at = window.outer_position().ok();
        #[cfg(not(target_os = "windows"))]
        let _ = window.show();
        #[cfg(target_os = "macos")]
        {
            // Re-sync the native backing with the persisted theme on every open: a
            // theme switch the app missed (label typo'd away once) or an OS
            // appearance change while hidden otherwise leaves a stale layer under
            // the translucent page until relaunch.
            apply_window_background(window.app_handle(), None);
            let _ = window.show();
        }
        let _ = anchored_at;
    }

    #[cfg(target_os = "windows")]
    {
        set_webview_visible(window, true);
        if let Ok(hwnd) = window.hwnd() {
            let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd.0) };
            let native = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
            let mut rect = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let got = unsafe { native(hwnd.0, &mut rect) };
            let native_pos = if got != 0 {
                format!("{}x{} at {},{}", rect.right - rect.left, rect.bottom - rect.top, rect.left, rect.top)
            } else {
                "<err>".into()
            };
            let vis = window.is_visible().unwrap_or(false);
            crate::logging::info(&format!(
                "panel: show complete — dpi={dpi}, native {native_pos}, visible={vis}"
            ));
        }
    }

    // The panel is on screen before its page can paint, and the native layer is
    // the only one that can say something during that stretch — an AppKit note
    // on macOS, a GDI overlay child on Windows.
    if !content_is_ready() {
        show_boot_note(window.app_handle());
    }

    // Windows tray panels are intentionally non-activating. Calling set_focus
    // here invokes tao's foreground-window recovery (including Alt-key input),
    // which is both visible to the user and a source of popup latency. The
    // Windows configure hook applies WS_EX_NOACTIVATE through set_focusable.
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window.set_focus();
    }

    SHOW_FOCUS_GRACE_UNTIL_MS.store(
        now_ms().saturating_add(SHOW_FOCUS_GRACE_MS),
        Ordering::Release,
    );
}

/// Keeps the WebView2 controller in sync with the host window.
///
/// The panel is created hidden, so Runtime 154 can retain an invisible
/// controller after the host HWND is shown. Re-asserting the controller on the
/// UI thread restores the DirectComposition surface before the first frame.
#[cfg(target_os = "windows")]
pub(crate) fn set_webview_visible(window: &WebviewWindow, visible: bool) {
    let label = window.label().to_string();
    let _ = window.with_webview(move |webview| {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            // The panel and the edge bubble are utility surfaces, not browser
            // documents. Disable WebView2's native context menu at the
            // controller level as well as in the page: the non-activating
            // bubble can bypass the DOM `contextmenu` event and otherwise
            // expose Save As / Print / Refresh on a right-click.
            let context_menu = webview
                .controller()
                .CoreWebView2()
                .and_then(|core| core.Settings())
                .and_then(|settings| settings.SetAreDefaultContextMenusEnabled(false));
            // Keep the controller in sync with the host without navigating the
            // page. A reload here races Wry's initial navigation and can leave
            // the native boot note up forever before the content milestone.
            let visibility = webview.controller().SetIsVisible(visible);
            match visibility {
                Ok(()) => match context_menu {
                    Ok(()) => format!("visible={visible} ok, context-menu=off"),
                    Err(error) => format!("visible={visible} ok, context-menu failed ({error})"),
                },
                Err(error) if visible => {
                    let hidden = webview.controller().SetIsVisible(false).is_ok();
                    let shown = webview.controller().SetIsVisible(true).is_ok();
                    let menu = if context_menu.is_ok() { "off" } else { "failed" };
                    format!("visible=true failed ({error}) — forced false→true: hidden={hidden} shown={shown}, context-menu={menu}")
                }
                Err(error) => {
                    let menu = if context_menu.is_ok() { "off" } else { "failed" };
                    format!("visible=false failed ({error}), context-menu={menu}")
                }
            }
        }))
        .unwrap_or_else(|_| "panicked".to_string());
        crate::logging::info(&format!("panel: controller re-assert — {outcome} ({label})"));
    });
}

/// Text entry needs real keyboard focus, which the non-activating panel never
/// has: `WS_EX_NOACTIVATE` keeps the tray flyout off the foreground, and with
/// it the WebView receives no key events at all. The frontend turns this on
/// while a text field holds focus and off when it blurs; macOS panels route
/// keys without activating the app, so they need none of this.
#[cfg(target_os = "windows")]
pub fn set_keyboard_mode(app: &AppHandle, on: bool) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.set_focusable(on);
        if on {
            let _ = window.set_focus();
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_keyboard_mode(_app: &AppHandle, _on: bool) {}

fn request_visibility(window: &WebviewWindow, visible: bool) {
    if !accept_visibility_request(visible) {
        return;
    }
    if visible {
        show_window(window, None);
    } else {
        hide_window(window);
    }
}

fn hide_window(window: &WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        let surface = window.clone();
        let _ = window.run_on_main_thread(move || hide_window_on_ui(&surface));
    }
    #[cfg(not(target_os = "windows"))]
    hide_window_on_ui(window);
}

fn hide_window_on_ui(window: &WebviewWindow) {
    if panel_hidden().swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = window.hide() {
        panel_hidden().store(false, Ordering::Release);
        crate::logging::error(&format!("panel: host hide failed: {error}"));
        return;
    }
    #[cfg(target_os = "windows")]
    {
        set_webview_visible(window, false);
        crate::logging::info(&format!("panel: host hide complete — visible={}", native_window_visible(window)));
    }
}

/// The panel's real visibility, recorded here because neither tauri's cached
/// `is_visible` nor the converted NSPanel itself (RawNSPanel lacks NSWindow
/// selectors) can answer reliably. The window is created hidden
/// (`visible: false` in tauri.conf.json).
fn panel_hidden() -> &'static std::sync::atomic::AtomicBool {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::atomic::AtomicBool::new(true))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// When the page first had something to show — the cold-start budget line.
///
/// Deliberately telemetry, not a gate on showing the window. Measured on this
/// machine: with nothing ever asking to show the panel, the page reported
/// content in 1.1-1.6 s; with a gate that held the window back until the page
/// reported, it took 3.5-4.3 s — WebKit does not load a page for a window that
/// is never ordered front. Waiting on the thing the wait itself causes is a
/// stall, not a fix.
fn content_ready_flag() -> &'static std::sync::atomic::AtomicBool {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::atomic::AtomicBool::new(false))
}

/// Whether the page has ever reported content — the native boot note keys off
/// this so it never reappears on a panel that is already drawing.
fn content_is_ready() -> bool {
    content_ready_flag().load(Ordering::Acquire)
}

/// Stage 1: the bundle ran, so `index.html`'s boot shell is on screen. Kept apart
/// from the first report because the two are seconds apart on a cold start, and
/// that gap is exactly where the native note has to live.
pub fn mark_page_boot() {
    static BOOT: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    let booted = BOOT.get_or_init(|| std::sync::atomic::AtomicBool::new(false));
    if !booted.swap(true, Ordering::AcqRel) {
        crate::logging::info(&format!(
            "panel: the page booted {} ms after launch",
            launch_instant().elapsed().as_millis()
        ));
    }
}

/// The page has its first figures: say so once, and take the native note away.
pub fn mark_content_ready(app: &AppHandle) {
    if !content_ready_flag().swap(true, Ordering::AcqRel) {
        crate::logging::info(&format!(
            "panel: the page reported content {} ms after launch",
            launch_instant().elapsed().as_millis()
        ));
    }
    hide_boot_note(app);
}

/// The panel sits on screen seconds before its page can paint. Measured here:
/// the window is up at +1.5 s and every frame until the first report (~+8 s) is
/// the bare native backing — a white sheet under 浅色 — with the web layer
/// contributing nothing at all. Three builds of markup in `index.html` proved
/// that interval is unreachable from the page, so the note is native on both
/// platforms — AppKit on macOS, a GDI overlay child on Windows — and is
/// removed the moment the page reports content.
pub fn show_boot_note(app: &AppHandle) {
    let text = crate::lang::get()
        .str("正在索引本机用量…", "Indexing this machine…")
        .to_string();
    #[cfg(target_os = "macos")]
    {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || set_boot_note(&handle, Some(&text)));
    }
    #[cfg(target_os = "windows")]
    boot_note_windows::show(app, &text);
}

#[cfg(target_os = "macos")]
pub fn hide_boot_note(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || set_boot_note(&handle, None));
}

#[cfg(target_os = "windows")]
pub fn hide_boot_note(app: &AppHandle) {
    boot_note_windows::hide(app);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn hide_boot_note(_app: &AppHandle) {}

/// The label's tag, so an open finds and replaces its own note instead of
/// stacking one per click.
#[cfg(target_os = "macos")]
const BOOT_NOTE_TAG: isize = 0x746b_426f;

/// The palette the native note paints with — the settings pin first, the OS
/// appearance for 系统. The same resolution `apply_window_background` uses for
/// the sheet under it, so the note and its surface can never disagree, and the
/// same one `index.html`'s shell follows through its `[data-theme]` pin.
#[cfg(target_os = "macos")]
fn boot_note_is_dark() -> bool {
    match crate::settings::Settings::load().theme {
        crate::settings::Theme::Light => false,
        crate::settings::Theme::Dark => true,
        crate::settings::Theme::System => os_appearance_is_dark(),
    }
}

#[cfg(target_os = "macos")]
fn set_boot_note(app: &AppHandle, text: Option<&str>) {
    use tauri_nspanel::cocoa::base::id;
    use tauri_nspanel::cocoa::foundation::{NSPoint, NSRect, NSSize};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let Some(handle) = app.try_state::<PanelHandle>() else {
        return; // no converted panel: there is no content view to hang a note on
    };
    let panel = handle.inner().0.clone();
    unsafe {
        let content: id = panel.content_view();
        if content.is_null() {
            return;
        }
        let subviews: id = msg_send![content, subviews];
        let count: usize = msg_send![subviews, count];
        for index in 0..count {
            let view: id = msg_send![subviews, objectAtIndex: index];
            // `tag` reads on every NSView; only NSControl can be *given* one, which
            // is why both pieces of the note are controls (the label, and the ring's
            // host). Sending `setTag:` to a plain NSView is an unrecognized selector,
            // and objc's msg_send does not verify selectors: the exception unwinds
            // into this main-thread callback — tao's `did_finish_launching`, a frame
            // that cannot unwind — and aborts the whole app, logged only as "panic in
            // a function that cannot unwind" (build 98 died this way on first open).
            let tag: isize = msg_send![view, tag];
            if tag == BOOT_NOTE_TAG {
                let _: () = msg_send![view, removeFromSuperview];
            }
        }
        let Some(text) = text else { return };
        let bounds: NSRect = msg_send![content, bounds];

        // The page's own loading card speaks theme.css: label --ink-3, arcs
        // --accent — light #66748a / #0d8f74 · dark #8a99ad / #30d9aa, the same
        // values index.html's shell carries. Resolved by the settings pin, not
        // assumed dark: a hardcoded dark palette on the light sheet washed the
        // label out and sank the 28 % inner ring into the white (build 155).
        let dark = boot_note_is_dark();
        let (ink_rgb, accent_rgb) = if dark {
            ([0x8a, 0x99, 0xad], [0x30, 0xd9, 0xaa])
        } else {
            ([0x66, 0x74, 0x8a], [0x0d, 0x8f, 0x74])
        };
        let srgb = |rgb: [u8; 3], alpha: f64| -> id {
            msg_send![class!(NSColor),
                colorWithSRGBRed: (rgb[0] as f64) / 255.0
                green: (rgb[1] as f64) / 255.0
                blue: (rgb[2] as f64) / 255.0 alpha: alpha]
        };
        let ink_ns = srgb(ink_rgb, 1.0);
        let accent_ns = srgb(accent_rgb, 1.0);
        let faint_ns = srgb(accent_rgb, 0.28); // color-mix 28 %, the .loading-arc-lo recipe

        // Motion, not just words. A static line on an empty sheet reads as a dead
        // panel — reported that way on 2026-10-07, and again on 2026-10-08 with the
        // system spinner already in the code: a spinning NSProgressIndicator paints
        // nothing until its animation runs, and on a non-activating panel belonging
        // to an accessory app (no dock icon, never active) it never runs. The
        // recorded frame settled it — label pixels, no ring. So the motion is
        // CoreAnimation, which composites whether or not the app is active, and it
        // draws the panel's own dual half-ring: the same mark `index.html`'s shell
        // and React's loading card spin, so native → page hands over one object
        // keeping time rather than swapping one spinner for another.
        let ring_side = 34.0;
        let ring: id = msg_send![class!(NSControl), alloc];
        let ring: id = msg_send![ring, initWithFrame: NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize { width: ring_side, height: ring_side },
        }];
        let _: () = msg_send![ring, setTag: BOOT_NOTE_TAG];
        // Attached before boot_ring runs: the ring asks its host for a backing
        // layer, and a view already in the window's layer tree is the one state
        // where that layer is guaranteed to exist. Its frame lands below.
        let _: () = msg_send![content, addSubview: ring];
        if let Err(step) = boot_ring(ring, ring_side, accent_ns, faint_ns) {
            // No ring beats a crash: the label still names the wait, and the page
            // paints within seconds either way. The refusal speaks now — build 155
            // shipped a bare label because this branch was silent while the guard
            // refused a selector no AppKit class answers.
            crate::logging::info(&format!("panel: boot note ring refused at {step}"));
            let _: () = msg_send![ring, removeFromSuperview];
        }

        let c_text = match std::ffi::CString::new(text) {
            Ok(value) => value,
            Err(_) => return, // interior NUL: no label, and the page will paint anyway
        };
        let ns_text: id = msg_send![class!(NSString), stringWithUTF8String: c_text.as_ptr()];
        if ns_text.is_null() {
            return;
        }
        let label: id = msg_send![class!(NSTextField), labelWithString: ns_text];
        if label.is_null() {
            return;
        }
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![label, setBackgroundColor: clear];
        // --ink-3 for the words (the page's .loading-label colour), resolved by
        // the settings pin above — the note and the card it hands over to speak
        // one palette.
        let _: () = msg_send![label, setTextColor: ink_ns];
        let font: id = msg_send![class!(NSFont), systemFontOfSize: 11.0];
        let _: () = msg_send![label, setFont: font];
        let _: () = msg_send![label, setTag: BOOT_NOTE_TAG];

        // The pair is centred as a group, spinner above, the same 10 pt apart as
        // the page's own loading card — the handover between the two layers should
        // not move the words.
        let size: NSSize = msg_send![label, fittingSize];
        let gap = 10.0;
        let group_top = (bounds.size.height + ring_side + gap + size.height) / 2.0;
        let ring_frame = NSRect {
            origin: NSPoint {
                x: (bounds.size.width - ring_side) / 2.0,
                y: group_top - ring_side,
            },
            size: NSSize { width: ring_side, height: ring_side },
        };
        let _: () = msg_send![ring, setFrame: ring_frame];
        let label_frame = NSRect {
            origin: NSPoint {
                x: (bounds.size.width - size.width) / 2.0,
                y: group_top - ring_side - gap - size.height,
            },
            size,
        };
        let _: () = msg_send![label, setFrame: label_frame];
        // Last subviews, so they sit above the web view: the page is transparent
        // until it paints, and the note has to survive exactly that interval.
        // The ring is already attached (that order is its layer's birthright);
        // the label lands on top of it here.
        let _: () = msg_send![content, addSubview: label];
    }
}

/// The brand's two half-rings, stroked from bezier paths on CoreAnimation layers
/// and counter-rotated: outer 160° counter-clockwise in 1.3 s, inner 160° (offset
/// half a turn) clockwise in 0.9 s — the same geometry and cadence as
/// `.loading-arc-hi` / `.loading-arc-lo` in panel.css and the boot shell's copy.
/// The stroke colours arrive resolved (`accent` full, `faint` at its 28 % mix —
/// the palette lives beside the label in `set_boot_note`, so ring and words can
/// never disagree).
///
/// Every selector is asked before it is sent, and the first one this OS does not
/// answer ends the attempt with its name: an unrecognized selector here would
/// unwind through `did_finish_launching`, a frame that cannot unwind, and abort the
/// app with no class name in the log. `NSBezierPath`'s `CGPath` is the young API in
/// this list (macOS 14+), which is exactly the kind of thing not to assume.
#[cfg(target_os = "macos")]
unsafe fn boot_ring(
    host: tauri_nspanel::cocoa::base::id,
    side: f64,
    accent: tauri_nspanel::cocoa::base::id,
    faint: tauri_nspanel::cocoa::base::id,
) -> Result<(), &'static str> {
    use std::ffi::c_char;
    use tauri_nspanel::cocoa::base::id;
    use tauri_nspanel::cocoa::foundation::{NSPoint, NSRect, NSSize};
    use tauri_nspanel::objc::runtime::Sel;
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let can = |target: id, selector: Sel| -> bool {
        let answered: bool = msg_send![target, respondsToSelector: selector];
        answered
    };
    // Each guard takes a whole `sel!(…)` expression: a selector is not a single
    // `tt` in macro input (`setPath:` lexes as two tokens), and `$sel:expr` is how
    // `sel!`'s own declaration accepts it.
    macro_rules! need {
        ($target:expr, $($selector:expr => $name:literal),+ $(,)?) => {
            $( if !can($target, $selector) { return Err($name); } )+
        };
    }
    let text = |bytes: &[u8]| -> id {
        let value: id = msg_send![class!(NSString), stringWithUTF8String: bytes.as_ptr() as *const c_char];
        value
    };

    let path_class = class!(NSBezierPath);
    let shape_class = class!(CAShapeLayer);
    let anim_class = class!(CABasicAnimation);
    need!(host, sel!(setWantsLayer:) => "NSView.setWantsLayer:");
    let _: () = msg_send![host, setWantsLayer: true];
    let layer: id = msg_send![host, layer];
    if layer.is_null() {
        return Err("NSView.layer");
    }
    need!(layer, sel!(addSublayer:) => "CALayer.addSublayer:");
    // `respondsToSelector:` on a class answers for instance methods, which is what
    // is being asked here; the constructors (`bezierPath`, `layer`) are class
    // methods and are checked by trying them.
    let path_probe: id = msg_send![path_class, bezierPath];
    if path_probe.is_null() {
        return Err("NSBezierPath.bezierPath");
    }
    need!(
        path_probe,
        sel!(appendBezierPathWithArcWithCenter:radius:startAngle:endAngle:) =>
            "NSBezierPath.appendBezierPathWithArcWithCenter:radius:startAngle:endAngle:",
        sel!(CGPath) => "NSBezierPath.CGPath"
    );
    let shape_probe: id = msg_send![shape_class, layer];
    if shape_probe.is_null() {
        return Err("CAShapeLayer.layer");
    }
    need!(
        shape_probe,
        sel!(setPath:) => "CAShapeLayer.setPath:",
        sel!(setStrokeColor:) => "CAShapeLayer.setStrokeColor:",
        sel!(setFillColor:) => "CAShapeLayer.setFillColor:",
        sel!(setLineWidth:) => "CAShapeLayer.setLineWidth:",
        sel!(setLineCap:) => "CAShapeLayer.setLineCap:",
        sel!(setFrame:) => "CALayer.setFrame:",
        sel!(addAnimation:forKey:) => "CALayer.addAnimation:forKey:"
    );
    let anim_probe: id = msg_send![anim_class, alloc];
    let anim_probe: id = msg_send![anim_probe, init];
    need!(
        anim_probe,
        sel!(setFromValue:) => "CABasicAnimation.setFromValue:",
        sel!(setToValue:) => "CABasicAnimation.setToValue:",
        sel!(setDuration:) => "CABasicAnimation.setDuration:",
        sel!(setRepeatCount:) => "CABasicAnimation.setRepeatCount:"
    );

    let clear: id = msg_send![class!(NSColor), clearColor];
    need!(accent, sel!(CGColor) => "NSColor.CGColor");
    need!(faint, sel!(CGColor) => "NSColor.CGColor");
    let accent_cg: id = msg_send![accent, CGColor];
    let faint_cg: id = msg_send![faint, CGColor];
    let clear_cg: id = msg_send![clear, CGColor];

    // viewBox 48 scaled to the host: the web card's r=20 outer and r=11 inner arcs,
    // each 160° and offset half a turn, at the same 4 / 3.5 stroke widths.
    let scale = side / 48.0;
    let center = NSPoint { x: side / 2.0, y: side / 2.0 };
    let arc = |radius: f64, from: f64, to: f64| -> id {
        let path: id = msg_send![path_class, bezierPath];
        let sweep = radius * scale;
        let _: () = msg_send![path,
            appendBezierPathWithArcWithCenter: center radius: sweep startAngle: from endAngle: to];
        msg_send![path, CGPath]
    };
    let frame = NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: side, height: side } };
    let turn = std::f64::consts::TAU;
    for (cg_path, stroke, width, seconds, direction) in
        [(arc(20.0, 20.0, 200.0), accent_cg, 4.0 * scale, 1.3, -turn), (arc(11.0, 200.0, 380.0), faint_cg, 3.5 * scale, 0.9, turn)]
    {
        if cg_path.is_null() {
            return Err("NSBezierPath.CGPath answered NULL");
        }
        let shape: id = msg_send![shape_class, layer];
        let _: () = msg_send![shape, setPath: cg_path];
        let _: () = msg_send![shape, setFillColor: clear_cg];
        let _: () = msg_send![shape, setStrokeColor: stroke];
        let _: () = msg_send![shape, setLineWidth: width];
        // kCALineCapRound is the string @"round"; the constant is not a symbol to
        // link against from here.
        let round = text(b"round\0");
        let _: () = msg_send![shape, setLineCap: round];
        let _: () = msg_send![shape, setFrame: frame];
        let _: () = msg_send![layer, addSublayer: shape];

        let key_path = text(b"transform.rotation.z\0");
        let anim: id = msg_send![anim_class, animationWithKeyPath: key_path];
        let zero: id = msg_send![class!(NSNumber), numberWithDouble: 0.0];
        let end: id = msg_send![class!(NSNumber), numberWithDouble: direction];
        let _: () = msg_send![anim, setFromValue: zero];
        let _: () = msg_send![anim, setToValue: end];
        let _: () = msg_send![anim, setDuration: seconds];
        let _: () = msg_send![anim, setRepeatCount: f64::INFINITY];
        let name = text(b"boot-spin\0");
        let _: () = msg_send![shape, addAnimation: anim forKey: name];
    }
    Ok(())
}

/// Windows: the same note, native. A child HWND covering the panel while its
/// WebView is still blank — the opaque surface the boot shows anyway, the
/// brand's dual half-ring (the exact macOS geometry: 34 pt host, outer r=20 of
/// a 48-box at 4 px counter-clockwise in 1.3 s, inner r=11 at 3.5 px clockwise
/// in 0.9 s, both 160° and half a turn apart), and the wait's name under it.
/// A 30 Hz repaint timer drives the phase; the note is destroyed at the content
/// milestone, exactly like its AppKit sibling. Plain GDI, no exceptions to
/// unwind — the AppKit selector-verification discipline does not apply here.
#[cfg(target_os = "windows")]
mod boot_note_windows {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::time::Instant;

    use tauri::{AppHandle, Manager};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        Arc, BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
        ExtCreatePen, FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor,
        DT_CALCRECT, DT_CENTER, DT_WORDBREAK, LOGBRUSH, PS_ENDCAP_ROUND, PS_GEOMETRIC, PS_SOLID,
        PAINTSTRUCT, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
        KillTimer, RegisterClassW, SetTimer, SetWindowLongPtrW, ShowWindow, GWLP_USERDATA,
        SW_SHOWNA, WM_ERASEBKGND, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_CHILD,
    };

    const LABEL: &str = super::LABEL;

    /// The live overlay's HWND, 0 when none. Main-thread only.
    static OVERLAY: AtomicIsize = AtomicIsize::new(0);

    struct BootNote {
        text: Vec<u16>,
        scale: f64,
        dark: bool,
        hfont: isize,
        start: Instant,
    }

    pub(super) fn show(app: &AppHandle, text: &str) {
        let handle = app.clone();
        let text = text.to_string();
        let _ = app.run_on_main_thread(move || unsafe { create(&handle, &text) });
    }

    pub(super) fn hide(app: &AppHandle) {
        let _ = app.run_on_main_thread(move || unsafe {
            let hwnd = OVERLAY.swap(0, Ordering::AcqRel);
            if hwnd != 0 {
                KillTimer(hwnd as _, 1);
                DestroyWindow(hwnd as _);
            }
        });
    }

    unsafe fn create(app: &AppHandle, text: &str) {
        let Some(window) = app.get_webview_window(LABEL) else { return };
        let Ok(hwnd) = window.hwnd() else { return };
        if OVERLAY.load(Ordering::Acquire) != 0 {
            return; // an open note is recycled by the hide/show pair, never stacked
        }
        let Ok(scale) = window.scale_factor() else { return };
        let dark = match app.try_state::<crate::engine::Shared>() {
            Some(shared) => match shared.settings().theme {
                crate::settings::Theme::Dark => true,
                crate::settings::Theme::Light => false,
                crate::settings::Theme::System => !matches!(window.theme(), Ok(tauri::Theme::Light)),
            },
            None => !matches!(window.theme(), Ok(tauri::Theme::Light)),
        };
        ensure_class();

        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetClientRect(hwnd.0 as _, &mut rc) == 0 {
            return;
        }
        let class = class_name();
        let mut wide = text.encode_utf16().collect::<Vec<_>>();
        wide.push(0);
        let note = Box::new(BootNote {
            text: wide,
            scale,
            dark,
            hfont: make_font(scale) as isize,
            start: Instant::now(),
        });
        let overlay = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            WS_CHILD, // shown only after the state is attached
            0,
            0,
            rc.right,
            rc.bottom,
            hwnd.0 as _,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if overlay.is_null() {
            return;
        }
        SetWindowLongPtrW(overlay, GWLP_USERDATA, Box::into_raw(note) as isize);
        OVERLAY.store(overlay as isize, Ordering::Release);
        // 30 Hz keeps the ring smooth at a cost a cold boot never notices; the
        // timer dies with the overlay, so an idle panel schedules nothing.
        SetTimer(overlay, 1, 33, None);
        ShowWindow(overlay, SW_SHOWNA);
        InvalidateRect(overlay, std::ptr::null(), 0);
    }

    fn class_name() -> &'static [u16] {
        static CLASS: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
        CLASS.get_or_init(|| "TokenMeBootNote\0".encode_utf16().collect())
    }

    unsafe fn ensure_class() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(boot_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(), // the paint fills everything
                lpszMenuName: std::ptr::null_mut(),
                lpszClassName: class_name().as_ptr(),
            };
            RegisterClassW(&wc);
        });
    }

    unsafe extern "system" fn boot_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        match msg {
            WM_TIMER => {
                InvalidateRect(hwnd, std::ptr::null(), 0);
                0
            }
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_ERASEBKGND => 1, // the paint fills everything; erasing only flickers
            WM_NCDESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let note = Box::from_raw(ptr as *mut BootNote);
                    DeleteObject(note.hfont as _);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    unsafe fn paint(hwnd: HWND) {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_null() {
            return;
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if ptr == 0 {
            EndPaint(hwnd, &ps);
            return;
        }
        let note = &*(ptr as *const BootNote);
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut rc);

        // The opaque sheet the boot shows anyway: pure black under dark, pure
        // white under light — the panel surface's own two colours. The ring
        // speaks --accent per theme; the words speak --ink-3 (#8a99ad dark ·
        // #66748a light), the page's own .loading-label colour.
        let (bg, teal, faint, ink) = if note.dark {
            (
                0x0000_0000,
                0x00A8_D931, // (49,217,168)
                0x002F_3D0E, // 28 % of it on black
                0x00AD_998Au32, // (138,153,173)
            )
        } else {
            (
                0x00FF_FFFF,
                0x0074_8F0D, // (13,143,116)
                0x00E7_F4C5, // 28 % of it on white
                0x008A_7466u32, // (102,116,138)
            )
        };
        let brush = CreateSolidBrush(bg);
        FillRect(hdc, &rc, brush);
        DeleteObject(brush as _);

        let s = note.scale;
        let side = 34.0 * s;
        let gap = 10.0 * s;
        let old_font = SelectObject(hdc, note.hfont as _);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, ink);

        let mut trc =
            RECT { left: (16.0 * s) as i32, top: 0, right: rc.right - (16.0 * s) as i32, bottom: 0 };
        DrawTextW(
            hdc,
            note.text.as_ptr(),
            note.text.len() as i32,
            &mut trc,
            DT_CALCRECT | DT_CENTER | DT_WORDBREAK,
        );
        let text_h = trc.bottom - trc.top;
        let total = side + gap + text_h as f64;
        let top = (rc.bottom as f64 - total) / 2.0;
        let cx = rc.right as f64 / 2.0;
        let cy = top + side / 2.0;

        // The dual half-ring. GDI's Arc runs from a start point to an end point
        // counter-clockwise, so the phases are plain angle offsets: the outer
        // arc's start drifts forward through its 1.3 s turn, the inner arc's
        // drifts backward through its 0.9 s one.
        let ms = note.start.elapsed().as_millis() as f64;
        let outer_start = 20.0 + (ms % 1300.0) / 1300.0 * 360.0;
        let inner_start = 200.0 - (ms % 900.0) / 900.0 * 360.0;
        stroke_arc(hdc, cx, cy, 20.0 / 48.0 * side, outer_start, 160.0, teal, (4.0 * s).round().max(1.0) as u32);
        stroke_arc(hdc, cx, cy, 11.0 / 48.0 * side, inner_start, 160.0, faint, (3.5 * s).round().max(1.0) as u32);

        let mut tr = RECT {
            left: trc.left,
            top: (top + side + gap) as i32,
            right: trc.right,
            bottom: (top + side + gap + text_h as f64) as i32,
        };
        DrawTextW(hdc, note.text.as_ptr(), note.text.len() as i32, &mut tr, DT_CENTER | DT_WORDBREAK);
        SelectObject(hdc, old_font);
        EndPaint(hwnd, &ps);
    }

    /// One 160° arc, round-capped, centred on (cx, cy). Angles are the AppKit
    /// convention the brand's mark was measured in: degrees, 0° at 3 o'clock,
    /// counter-clockwise positive — hence the y negation against GDI's
    /// downward axis.
    unsafe fn stroke_arc(
        hdc: windows_sys::Win32::Graphics::Gdi::HDC,
        cx: f64,
        cy: f64,
        radius: f64,
        start_deg: f64,
        sweep_deg: f64,
        color: u32,
        width: u32,
    ) {
        let brush = LOGBRUSH { lbStyle: PS_SOLID as u32, lbColor: color, lbHatch: 0 };
        let pen = ExtCreatePen(
            (PS_GEOMETRIC | PS_ENDCAP_ROUND) as u32,
            width,
            &brush,
            0,
            std::ptr::null(),
        );
        if pen.is_null() {
            return;
        }
        let old = SelectObject(hdc, pen as _);
        let a0 = start_deg.to_radians();
        let a1 = (start_deg + sweep_deg).to_radians();
        let (x1, y1) = (cx + radius * a0.cos(), cy - radius * a0.sin());
        let (x2, y2) = (cx + radius * a1.cos(), cy - radius * a1.sin());
        Arc(
            hdc,
            (cx - radius).round() as i32,
            (cy - radius).round() as i32,
            (cx + radius).round() as i32,
            (cy + radius).round() as i32,
            x1.round() as i32,
            y1.round() as i32,
            x2.round() as i32,
            y2.round() as i32,
        );
        SelectObject(hdc, old);
        DeleteObject(pen);
    }

    unsafe fn make_font(scale: f64) -> windows_sys::Win32::Graphics::Gdi::HFONT {
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        CreateFontW(
            (-(12.0 * scale)).round() as i32,
            0,
            0,
            0,
            400, // FW_NORMAL
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0,
            0,
            4, // ANTIALIASED_QUALITY
            0,
            face.as_ptr(),
        )
    }
}

/// start at its first use and report every latency as 0 ms.
fn launch_instant() -> std::time::Instant {
    static CELL: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *CELL.get_or_init(std::time::Instant::now)
}

pub fn mark_launch() {
    launch_instant();
}

fn accept_visibility_request(visible: bool) -> bool {
    let now = now_ms();
    if !visible && now < SHOW_FOCUS_GRACE_UNTIL_MS.load(Ordering::Acquire) {
        return false;
    }

    loop {
        let previous = LAST_VISIBILITY_REQUEST_MS.load(Ordering::Acquire);
        if now.saturating_sub(previous) < VISIBILITY_DEBOUNCE_MS {
            return false;
        }
        if LAST_VISIBILITY_REQUEST_MS
            .compare_exchange(previous, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if visible {
                SHOW_FOCUS_GRACE_UNTIL_MS.store(
                    now.saturating_add(SHOW_FOCUS_GRACE_MS),
                    Ordering::Release,
                );
            }
            return true;
        }
    }
}


/// Opens the panel and tells the frontend which period to focus.
pub fn open_at(app: &AppHandle, period: &str) {
    show(app, tray::last_rect(app));
    let _ = app.emit(PERIOD_EVENT, period);
}

fn anchor(window: &WebviewWindow, rect: Option<Rect>) {
    // A tray rectangle is only an anchor if it describes a place on screen.
    // Until the status item has been laid out — which, measured on this machine,
    // is every launch until the first click — muda reports `(0, 2160, 68, 0)`:
    // zero height, pinned to the bottom edge of the display. Centring the panel
    // on that puts it in the bottom-left corner, so such a rect means "not yet
    // known" and the menu-bar fallback below takes over.
    let rect = rect
        .or_else(|| tray::last_rect(window.app_handle()))
        .filter(|rect| {
            let (w, h) = rect_size(rect);
            usable_tray_size(w, h)
        });

    #[cfg(target_os = "windows")]
    {
        // The positioner plugin constrains against the full monitor rectangle
        // on Windows. That puts a bottom-taskbar panel flush with the physical
        // screen edge. Use the tray rectangle plus the OS work area instead,
        // so the panel opens on the correct side of the taskbar and keeps an
        // intentional gap from the usable desktop edge.
        if anchor_windows(window, rect.as_ref()) {
            return;
        }
    }

    let placed = match rect.as_ref() {
        Some(rect) => {
            let (rx, ry) = rect_origin(rect);
            let (rw, rh) = rect_size(rect);
            let (x, y) = clamp_point(window, rx + rw / 2.0 - WIDTH / 2.0, ry + rh + GAP);
            Some((x, y, "tray rect"))
        }
        // Where the panel belongs when no tray rectangle is known: a launch-time
        // or QA open (`TOKENME_SHOW_PANEL`), or a single-instance hand-off. The
        // menu bar's right end is where the icon actually lives.
        None => match menu_bar_origin(window) {
            Some((x, y)) => Some((x, y, "menu bar")),
            None => {
                // A locked or waking session exposes no monitors: the panel then
                // opens wherever the window server put it, and the next real
                // open fixes it. Said out loud because "the panel appeared in the
                // wrong corner" is otherwise undiagnosable from the log.
                #[cfg(target_os = "macos")]
                crate::logging::error(
                    "panel: no monitor to anchor to — opening where the window server put it",
                );
                None
            }
        },
    };
    let Some((x, y, source)) = placed else { return };
    let _ = window.set_position(PhysicalPosition::new(x, y));
    // The first open of the process is the cold-start one, and it is the only
    // one worth a log line: an anchor that computes the right number and still
    // lands in a corner is otherwise invisible from the outside.
    //
    // The read-back is not a confirmation of `y`. AppKit refuses to place a
    // window over the menu bar — a requested top edge of 0 comes back about 60
    // pt lower — and `outer_position` reports the frame origin, shadow included.
    // The two numbers are therefore never equal by design. The read-back is here
    // to catch "2000 px away from the menu bar", not to settle a pixel; on-screen
    // truth comes from a capture, as it did for the bottom-left-corner bug.
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::AcqRel) {
        let back = window.outer_position().ok();
        crate::logging::info(&format!(
            "panel: first open anchored to {source} at ({x:.0}, {y:.0}) from rect {:?}, frame origin afterwards {:?} (AppKit clamps below the menu bar; not a pixel check)",
            rect.as_ref().map(|r| {
                let (rx, ry) = rect_origin(r);
                let (rw, rh) = rect_size(r);
                (rx as i64, ry as i64, rw as i64, rh as i64)
            }),
            back.map(|p| (p.x, p.y)),
        ));
    }
}

/// Whether a tray rectangle of `w × h` can be trusted as an anchor. See
/// [`anchor`]: the status item reports a zero-height rect at the bottom of the
/// display until it has been laid out, and centring the panel on that is the
/// bottom-left-corner bug.
fn usable_tray_size(w: f64, h: f64) -> bool {
    w > 0.0 && h > 0.0
}

/// Anywhere on screen beats nowhere: `anchor_windows` already had its chance at
/// the tray rectangle, so a missing one here just gets pulled back inside the
/// work area.
#[cfg(not(target_os = "macos"))]
fn menu_bar_origin(window: &WebviewWindow) -> Option<(f64, f64)> {
    clamp(window);
    None
}

/// The origin that hangs the panel off the right end of the menu bar's
/// available area. `available_monitors` already stops below the menu bar, so its
/// top edge is exactly where a menu-bar panel should start.
#[cfg(target_os = "macos")]
fn menu_bar_origin(window: &WebviewWindow) -> Option<(f64, f64)> {
    let monitor = window.available_monitors().ok()?.into_iter().next()?;
    let width = window
        .outer_size()
        .map(|s| s.width as f64)
        .unwrap_or(WIDTH);
    let (pos, size) = (monitor.position(), monitor.size());
    Some(top_right_origin(
        Bounds {
            left: pos.x as f64,
            top: pos.y as f64,
            right: pos.x as f64 + size.width as f64,
            bottom: pos.y as f64 + size.height as f64,
        },
        width,
    ))
}

/// The panel's origin when it hangs off the right end of the menu bar.
#[cfg(target_os = "macos")]
fn top_right_origin(area: Bounds, width: f64) -> (f64, f64) {
    (
        (area.right - width - GAP).max(area.left),
        area.top,
    )
}

fn clamp_in_bounds(x: f64, y: f64, size: (f64, f64), bounds: Bounds, margin: f64) -> (f64, f64) {
    let min_x = bounds.left + margin;
    let min_y = bounds.top + margin;
    let max_x = (bounds.right - size.0 - margin).max(min_x);
    let max_y = (bounds.bottom - size.1 - margin).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

fn edge_for_tray(tray: Bounds, area: ScreenArea) -> TrayEdge {
    // A normal taskbar makes the tray rectangle fall outside the work area.
    if tray.top >= area.work.bottom {
        return TrayEdge::Bottom;
    }
    if tray.bottom <= area.work.top {
        return TrayEdge::Top;
    }
    if tray.right <= area.work.left {
        return TrayEdge::Left;
    }
    if tray.left >= area.work.right {
        return TrayEdge::Right;
    }

    // Auto-hide taskbars leave the work area equal to the monitor. In that
    // case, use the nearest monitor edge rather than guessing from Windows'
    // current taskbar setting.
    let distances = [
        (TrayEdge::Top, (tray.top - area.monitor.top).abs()),
        (TrayEdge::Bottom, (area.monitor.bottom - tray.bottom).abs()),
        (TrayEdge::Left, (tray.left - area.monitor.left).abs()),
        (TrayEdge::Right, (area.monitor.right - tray.right).abs()),
    ];
    distances
        .into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(edge, _)| edge)
        .unwrap_or(TrayEdge::Bottom)
}

fn windows_anchor_point(tray: Bounds, area: ScreenArea, size: (f64, f64)) -> (f64, f64) {
    let edge = edge_for_tray(tray, area);
    let tray_center_x = (tray.left + tray.right) / 2.0;
    let tray_center_y = (tray.top + tray.bottom) / 2.0;
    let (x, y) = match edge {
        TrayEdge::Bottom => (tray_center_x - size.0 / 2.0, tray.top - size.1 - GAP),
        TrayEdge::Top => (tray_center_x - size.0 / 2.0, tray.bottom + GAP),
        TrayEdge::Left => (tray.right + GAP, tray_center_y - size.1 / 2.0),
        TrayEdge::Right => (tray.left - size.0 - GAP, tray_center_y - size.1 / 2.0),
    };
    clamp_in_bounds(x, y, size, area.work, GAP)
}

/// The anchor point for a tray-less open: the cursor, not the window — a
/// never-shown window answers no reliable position, while GetCursorPos always
/// knows where the user is, and a tray click leaves the cursor at the icon.
#[cfg(target_os = "windows")]
fn anchor_point_for(rect: Option<Rect>, window: &WebviewWindow) -> (f64, f64) {
    if let Some(rect) = rect {
        let (x, y) = rect_origin(&rect);
        let (w, h) = rect_size(&rect);
        if w > 0.0 && h > 0.0 {
            return (x + w / 2.0, y + h);
        }
    }
    let _ = window;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut point) };
    (point.x as f64, point.y as f64)
}

/// The primary monitor when the point-based lookup has nothing to work with.
#[cfg(target_os = "windows")]
pub fn fallback_area() -> ScreenArea {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    unsafe {
        let right = GetSystemMetrics(SM_CXSCREEN) as f64;
        let bottom = GetSystemMetrics(SM_CYSCREEN) as f64;
        ScreenArea {
            monitor: Bounds { left: 0.0, top: 0.0, right, bottom },
            work: Bounds { left: 0.0, top: 0.0, right, bottom },
        }
    }
}

#[cfg(target_os = "windows")]
fn anchor_windows(window: &WebviewWindow, rect: Option<&Rect>) -> bool {
    // The anchor point for a tray-less open comes from the cursor, not from
    // the window: a never-shown window answers no reliable position (measured:
    // it sat at an off-screen OS default while tauri's monitor list came back
    // empty, and both clamp paths no-op'd — the panel "opened" 1200 px past
    // the screen edge), while GetCursorPos always knows where the user is,
    // and a tray click leaves the cursor at the icon.
    let (x, y) = if let Some(rect) = rect {
        let (x, y) = rect_origin(rect);
        let (w, h) = rect_size(rect);
        (x + w / 2.0, y + h / 2.0)
    } else {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut point) };
        (point.x as f64, point.y as f64)
    };
    let Some(area) = windows_screen_area(x, y) else {
        return false;
    };

    // The work area decides the height. A resolution or scaling change leaves
    // the hidden window carrying a stale physical size (measured: 990 px tall
    // on a 720 px work area, its bottom buried behind the taskbar), so the
    // anchor never trusts that size: it fits a fresh one and resizes to it.
    let scale = window
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or_else(|| window.scale_factor().unwrap_or(1.0))
        .max(1.0);
    let height = panel_height_for_work_area(area.work.bottom - area.work.top, scale);
    let current_height = window
        .outer_size()
        .map(|s| s.height as f64 / scale)
        .unwrap_or(HEIGHT);
    if (current_height - height).abs() > 1.0 {
        let _ = window.set_size(Size::Physical(PhysicalSize::new(
            (WIDTH * scale).round() as u32,
            (height * scale).round() as u32,
        )));
    }
    let size = (WIDTH * scale, height * scale);

    if let Some(rect) = rect {
        let (x, y) = rect_origin(rect);
        let (w, h) = rect_size(rect);
        let tray = Bounds {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        let (x, y) = windows_anchor_point(tray, area, size);
        let _ = window.set_position(PhysicalPosition::new(x, y));
    } else {
        let (x, y) = clamp_in_bounds(x, y, size, area.work, GAP);
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
    true
}

/// How tall the panel may be on this work area: the default height when it
/// fits, otherwise shrunk to the space between the screen edges minus the
/// anchoring gap (never below [`MIN_HEIGHT`] — the page scrolls for the rest).
/// Division and multiplication use the same scale, so even a stale scale
/// factor cannot push the physical result past the work area.
fn panel_height_for_work_area(work_height: f64, scale: f64) -> f64 {
    let max_height = ((work_height - 2.0 * GAP) / scale).max(MIN_HEIGHT);
    HEIGHT.min(max_height)
}

#[cfg(target_os = "windows")]
fn windows_screen_area(x: f64, y: f64) -> Option<ScreenArea> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let point = POINT {
        x: x.round() as i32,
        y: y.round() as i32,
    };
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let bounds = |r: windows_sys::Win32::Foundation::RECT| Bounds {
        left: r.left as f64,
        top: r.top as f64,
        right: r.right as f64,
        bottom: r.bottom as f64,
    };
    Some(ScreenArea {
        monitor: bounds(info.rcMonitor),
        work: bounds(info.rcWork),
    })
}

/// The tray rect arrives physical on both platforms; a logical one is rare but
/// must not be silently dropped, so both arms are handled.
fn rect_origin(rect: &Rect) -> (f64, f64) {
    match rect.position {
        Position::Physical(p) => (p.x as f64, p.y as f64),
        Position::Logical(l) => (l.x, l.y),
    }
}

fn rect_size(rect: &Rect) -> (f64, f64) {
    match rect.size {
        Size::Physical(s) => (s.width as f64, s.height as f64),
        Size::Logical(s) => (s.width, s.height),
    }
}

/// Pulls the panel back onto a connected monitor without moving it if it fits.
fn clamp(window: &WebviewWindow) {
    if let Ok(pos) = window.outer_position() {
        let (x, y) = clamp_point(window, pos.x as f64, pos.y as f64);
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

fn clamp_point(window: &WebviewWindow, x: f64, y: f64) -> (f64, f64) {
    let monitors = window.available_monitors().unwrap_or_default();
    let size = window
        .outer_size()
        .map(|s| (s.width as f64, s.height as f64))
        .unwrap_or((WIDTH, HEIGHT));
    let Some(monitor) = monitors
        .iter()
        .find(|m| {
            let p = m.position();
            let s = m.size();
            x >= p.x as f64 && x < p.x as f64 + s.width as f64
        })
        .or_else(|| monitors.first())
    else {
        return (x, y);
    };
    let p = monitor.position();
    let s = monitor.size();
    let min_x = p.x as f64;
    let min_y = p.y as f64;
    let max_x = (p.x as f64 + s.width as f64 - size.0).max(min_x);
    let max_y = (p.y as f64 + s.height as f64 - size.1).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() != LABEL {
        return;
    }

    #[cfg(target_os = "windows")]
    if matches!(event, WindowEvent::Resized(_)) {
        // The panel is fixed-size today, but Windows can change its physical
        // size when it crosses a monitor with a different DPI. Rebuild the
        // region so the native clip continues to match the CSS radius.
        if let Some(panel) = window.app_handle().get_webview_window(LABEL) {
            apply_windows_round_clip(&panel);
            // A display change (resolution, scaling, monitor layout) also
            // resizes or rescales the window while it is open; re-anchor so it
            // lands back inside the new work area instead of straddling the
            // taskbar. Stable after one pass: fitting a fitting size is a
            // no-op, and positioning does not emit further Resized events.
            if panel.is_visible().unwrap_or(false) {
                anchor(&panel, None);
            }
        }
    }

    // Windows panels are deliberately non-activating. Their Focused(false)
    // notification is therefore also emitted while the panel is being shown,
    // not only when the user clicks elsewhere; treating it as dismissal makes
    // the tray popup disappear immediately after a successful show. The
    // Windows click-away monitor above uses the actual HWND and pointer state,
    // so it remains the authoritative outside-click path there.
    #[cfg(not(target_os = "windows"))]
    {
        if !matches!(event, WindowEvent::Focused(false)) {
            return;
        }
        let app = window.app_handle().clone();
        if let Some(panel) = app.get_webview_window(LABEL) {
            if panel.is_visible().unwrap_or(false) {
                request_visibility(&panel, false);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn start_click_away_monitor(app: &AppHandle) {
    if CLICK_AWAY_MONITOR_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("tokenme-panel-click-away".into())
        .spawn(move || {
            use windows_sys::Win32::Foundation::POINT;
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

            let mut was_down = false;
            let mut was_hover = false;
            let mut bubble_press: Option<(i32, i32)> = None;
            loop {
                let down = unsafe { (GetAsyncKeyState(0x01) as u16 & 0x8000) != 0 };
                let mut point = POINT { x: 0, y: 0 };
                let has_point = unsafe { GetCursorPos(&mut point) != 0 };
                let hovering = has_point && bubble::contains_point(&app, point.x, point.y);
                if hovering != was_hover {
                    was_hover = hovering;
                    // WebView2 does not reliably deliver hover state for this
                    // window on every monitor, so the frontend's mouseenter is
                    // nothing to lean on. This poll is the source of truth;
                    // the pet's expand-on-hover hangs off it.
                    let _ = app.emit("bubble-hover", hovering);
                }
                if down && !was_down {
                    if let Some(panel) = app.get_webview_window(LABEL) {
                        if has_point && bubble::contains_point(&app, point.x, point.y) {
                            bubble_press = Some((point.x, point.y));
                        } else if native_window_visible(&panel)
                            && has_point
                            && !window_contains_point(&panel, point.x, point.y)
                            && !tray_contains_point(&app, point.x, point.y)
                        {
                            // Tauri's visibility cache can lag for a
                            // non-activating window immediately after a native
                            // show. Use the HWND state here so a desktop click
                            // always closes the surface that is actually visible.
                            request_visibility(&panel, false);
                        }
                    }
                }
                // A bubble press that moves is a drag. Waiting for the movement
                // lets the hover-expand land first, so the drag loop grabs the
                // expanded geometry instead of the docked one — and plain
                // clicks never spawn the drag thread at all.
                if down {
                    if let Some((start_x, start_y)) = bubble_press {
                        if (point.x - start_x).abs() > 6 || (point.y - start_y).abs() > 6 {
                            bubble_press = None;
                            bubble::begin_drag(&app);
                        }
                    }
                }
                if !down && was_down {
                    if let Some((start_x, start_y)) = bubble_press.take() {
                        let stayed = has_point
                            && (point.x - start_x).abs() <= 6
                            && (point.y - start_y).abs() <= 6
                            && bubble::contains_point(&app, point.x, point.y);
                        if stayed {
                            if let Some(panel) = app.get_webview_window(LABEL) {
                                if !native_window_visible(&panel) {
                                    show(&app, None);
                                }
                            }
                        } else {
                            // A moved bubble press was a drag, and the native
                            // move loop consumed the pointer events the webview
                            // was waiting for: it never saw the drop, so its
                            // dock-to-edge never ran. Tell it the drag is over.
                            let _ = app.emit("bubble-drag-ended", ());
                        }
                    }
                }
                was_down = down;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        })
        .ok();
}

#[cfg(target_os = "windows")]
fn window_contains_point(window: &WebviewWindow, x: i32, y: i32) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let Ok(hwnd) = window.hwnd() else { return false };
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd.0, &mut rect) != 0
            && x >= rect.left
            && x < rect.right
            && y >= rect.top
            && y < rect.bottom
    }
}

#[cfg(target_os = "windows")]
fn native_window_visible(window: &WebviewWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let Ok(hwnd) = window.hwnd() else { return false };
    unsafe { IsWindowVisible(hwnd.0) != 0 }
}

#[cfg(target_os = "windows")]
fn tray_contains_point(app: &AppHandle, x: i32, y: i32) -> bool {
    let bounds = last_tray_bounds()
        .lock()
        .ok()
        .and_then(|slot| *slot)
        .or_else(|| {
            let rect = tray::last_rect(app)?;
            let (left, top) = rect_origin(&rect);
            let (width, height) = rect_size(&rect);
            (width > 0.0 && height > 0.0).then_some(Bounds {
                left,
                top,
                right: left + width,
                bottom: top + height,
            })
        });
    let Some(Bounds { left, top, right, bottom }) = bounds else { return false };
    let x = x as f64;
    let y = y as f64;
    x >= left && x < right && y >= top && y < bottom
}

#[cfg(target_os = "windows")]
fn apply_windows_round_clip(window: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateRoundRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ,
    };

    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };

    let width = size.width.min(i32::MAX as u32) as i32;
    let height = size.height.min(i32::MAX as u32) as i32;
    if width <= 0 || height <= 0 {
        return;
    }

    // CSS uses a 12 logical-pixel radius. The region is expressed in physical
    // pixels, so scale it with the window rather than hard-coding 24px.
    let scale = window.scale_factor().unwrap_or(1.0).max(1.0);
    let diameter = (12.0 * scale).round().max(1.0) as i32 * 2;
    let region = unsafe { CreateRoundRectRgn(0, 0, width, height, diameter, diameter) };
    if region.is_null() {
        return;
    }

    // SetWindowRgn takes ownership of a successful region handle. Only free it
    // on failure; freeing it after success makes the clip intermittently turn
    // square when Windows repaints the transparent WebView.
    if unsafe { SetWindowRgn(hwnd.0, region, 1) } == 0 {
        unsafe {
            let _ = DeleteObject(region as HGDIOBJ);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Bounds = Bounds {
        left: 0.0,
        top: 0.0,
        right: 1920.0,
        bottom: 1080.0,
    };

    #[test]
    fn bottom_taskbar_keeps_the_panel_above_the_work_area() {
        let area = ScreenArea {
            monitor: MONITOR,
            work: Bounds {
                bottom: 1040.0,
                ..MONITOR
            },
        };
        let tray = Bounds {
            left: 1500.0,
            top: 1040.0,
            right: 1532.0,
            bottom: 1080.0,
        };
        let (x, y) = windows_anchor_point(tray, area, (400.0, 660.0));
        assert_eq!(x, 1316.0);
        assert_eq!(y, 372.0);
        assert!(y + 660.0 <= area.work.bottom - GAP);
    }

    #[test]
    fn top_taskbar_opens_below_the_tray() {
        let area = ScreenArea {
            monitor: MONITOR,
            work: Bounds {
                top: 40.0,
                ..MONITOR
            },
        };
        let tray = Bounds {
            left: 1500.0,
            top: 0.0,
            right: 1532.0,
            bottom: 40.0,
        };
        let (_, y) = windows_anchor_point(tray, area, (400.0, 660.0));
        assert_eq!(y, 48.0);
    }

    /// A resolution or scaling change must never again leave the panel taller
    /// than the work area: full height when it fits, shrunk when it does not,
    /// and a floor so a tiny work area keeps the panel usable.
    #[test]
    fn panel_height_fits_the_work_area() {
        // Roomy desktop: the default height stands.
        assert_eq!(panel_height_for_work_area(1040.0, 1.0), HEIGHT);
        // A 1366×768 screen at 125 % scaling: work area 720 physical px is
        // 576 logical — the panel must shrink to it (minus the anchor gap).
        assert_eq!(panel_height_for_work_area(720.0, 1.25), (720.0 - 16.0) / 1.25);
        // A tiny work area floors at MIN_HEIGHT instead of collapsing.
        assert_eq!(panel_height_for_work_area(200.0, 2.0), MIN_HEIGHT);
        // A stale (too large) scale factor divides and multiplies away: the
        // physical result still fits the work area.
        let scale = 1.5;
        let height = panel_height_for_work_area(720.0, scale);
        assert!(height * scale <= 720.0 - 2.0 * GAP + 0.001);
    }

    /// An open with no tray rectangle (launch, QA, single-instance hand-off)
    /// belongs at the right end of the menu bar, where the icon lives. It used
    /// to stay wherever the window was created — the bottom-left corner.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_open_without_a_tray_rect_hangs_off_the_menu_bar_s_right_end() {
        // The available area starts below the menu bar; the panel top sits on it.
        let screen = Bounds {
            left: 0.0,
            top: 37.0,
            right: 1920.0,
            bottom: 1080.0,
        };
        let (x, y) = top_right_origin(screen, WIDTH);
        assert_eq!(y, 37.0);
        assert_eq!(x, 1920.0 - WIDTH - GAP);
        assert!(x + WIDTH <= screen.right);
        // A work area narrower than the panel still starts at its left edge
        // rather than running off the screen.
        let tiny = Bounds {
            left: 0.0,
            top: 0.0,
            right: 300.0,
            bottom: 400.0,
        };
        assert_eq!(top_right_origin(tiny, WIDTH).0, 0.0);
        // A secondary monitor to the left of the main one keeps its own edge.
        let second = Bounds {
            left: -1920.0,
            top: 37.0,
            right: 0.0,
            bottom: 1080.0,
        };
        assert_eq!(top_right_origin(second, WIDTH).0, -WIDTH - GAP);
    }

    /// The tray rectangle reported at launch on this machine is
    /// `(0, 2160, 68, 0)` — a status item that has not been laid out yet.
    /// Centring the panel on it is exactly what put it in the bottom-left
    /// corner, so a zero-sized one has to read as "no anchor".
    #[test]
    fn an_unlaid_out_tray_rect_is_no_anchor_at_all() {
        assert!(usable_tray_size(68.0, 74.0), "a real status item anchors the panel");
        assert!(
            !usable_tray_size(68.0, 0.0),
            "zero height means the item has no place on screen yet"
        );
        assert!(!usable_tray_size(0.0, 74.0), "same for zero width");
    }
}

#[cfg(target_os = "macos")]
pub fn configure(app: &AppHandle) {
    use tauri_nspanel::cocoa::appkit::{NSMainMenuWindowLevel, NSWindowCollectionBehavior};
    use tauri_nspanel::WebviewWindowExt as _;

    /// `NSWindowStyleMask` bits the cocoa bindings leave out or name for the
    /// pre-10.12 era: borderless (0), nonactivating panel (1<<7), full-size
    /// content view (1<<15).
    const STYLE: i32 = (1 << 7) | (1 << 15);

    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    // Opaque backing, not frosted glass: the under-window material let
    // whatever sat behind the panel bleed through the translucent surface —
    // a dark IDE turned the sheet's bottom edge into a mismatched black
    // smear. The page surface itself is opaque since the same fix (the grey
    // mask was always this layer and the page disagreeing), so this
    // theme-coloured paint is only the corner seam's fallback; it still
    // follows the theme so even that seam can never read as a wash.
    if let Ok(panel) = window.to_panel() {
        // The panel object must never deallocate: tauri-nspanel's
        // RawNSPanel::dealloc calls [NSObject dealloc] directly — skipping
        // NSPanel/NSWindow teardown — so any deallocation after the class
        // swap is fatal (the pool-drain SIGBUS of builds 25-27). The crate's
        // from_window also burns one retain per call (Id::from_retained_ptr
        // takes ownership of a borrowed pointer), and to_panel used to run on
        // every show. The window lives for the whole process anyway: pin it
        // with one retain that is deliberately never released.
        unsafe {
            use tauri_nspanel::cocoa::base::id;
            use tauri_nspanel::objc::{msg_send, sel, sel_impl};
            let _: id = msg_send![panel, retain];
        }
        panel.set_style_mask(STYLE);
        panel.set_collection_behaviour(
            NSWindowCollectionBehavior::NSWindowCollectionBehaviorCanJoinAllSpaces
                | NSWindowCollectionBehavior::NSWindowCollectionBehaviorStationary
                | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary
                | NSWindowCollectionBehavior::NSWindowCollectionBehaviorIgnoresCycle,
        );
        panel.set_level(NSMainMenuWindowLevel + 1);
        panel.set_becomes_key_only_if_needed(false);
        panel.set_works_when_modal(true);
        panel.set_hides_on_deactivate(false);
        panel.set_floating_panel(true);
        panel.set_has_shadow(false);
        // Rounded corners: a borderless panel gets none natively, and the CSS
        // radius alone cannot deliver them — the webview's own backing layer
        // stays square behind the transparent page. Clipping the content
        // view's layer rounds what actually reaches the screen.
        unsafe {
            use tauri_nspanel::cocoa::base::{id, YES};
            use tauri_nspanel::objc::{msg_send, sel, sel_impl};
            /// Matches `[data-env="tauri"] .panel` in the panel stylesheet.
            const CORNER: f64 = 12.0;
            let content: id = panel.content_view();
            let _: () = msg_send![content, setWantsLayer: YES];
            let layer: id = msg_send![content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: CORNER];
                let _: () = msg_send![layer, setMasksToBounds: YES];
            }
        }
        app.manage(PanelHandle(panel));
        apply_window_background(app, None);
    }
    // If the conversion fails the window stays a plain `alwaysOnTop` surface:
    // the tray toggle still works, it just cannot follow into full screen.

    observe_context_switches(app);
}

/// Keeps the converted panel alive (and reachable) for the lifetime of the
/// app: acquired once at setup, never re-converted — see the pinning note in
/// `configure`.
#[cfg(target_os = "macos")]
struct PanelHandle(tauri_nspanel::Panel);

/// Hides the panel when the user switches Space or activates another app.
/// The window's own backing shows wherever the webview viewport rounds a few
/// pixels short of the window height (fractional-scale rounding) — a dark
/// strip under the settings sheet on a light theme. Paint the panel backing
/// with the theme's own surface so the strip and the sheet are one colour.
#[cfg(target_os = "macos")]
pub fn apply_window_background(app: &AppHandle, theme: Option<crate::settings::Theme>) {
    use tauri_nspanel::cocoa::base::{id, YES};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let resolved = theme.or_else(|| Some(crate::settings::Settings::load().theme));
    // "system" must resolve the way the page's media query does — against the
    // OS appearance, not tauri's window.theme(), which reports Aqua for a
    // window created without an explicit theme even on a dark system (that
    // mismatch painted a light layer under a dark page: a grey wash).
    let dark = match resolved {
        Some(crate::settings::Theme::Light) => false,
        Some(crate::settings::Theme::Dark) => true,
        _ => os_appearance_is_dark(),
    };
    // --surface: light #ffffff · dark #000000 — the page's own flat surface
    // is opaque now, so this layer only shows through the rounded-corner
    // anti-aliasing seam; matching colours keep that seam invisible.
    let (fr, fg, fb) = if dark { (0.0, 0.0, 0.0) } else { (1.0, 1.0, 1.0) };

    // Build the CGColor through CoreGraphics itself. NSColor's -CGColor bridge
    // answers nil for calibrated colours (what colorWithCalibratedRed returns),
    // and a nil passed to setBackgroundColor silently leaves the layer clear —
    // the black ring around the rounded panel on a light theme. A CGColorCreate
    // in sRGB has no such failure mode.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        static kCGColorSpaceSRGB: id;
        fn CGColorSpaceCreateWithName(name: id) -> id;
        fn CGColorCreate(space: id, components: *const f64) -> id;
    }
    // The panel handle was acquired once at setup (to_panel also burns a
    // retain per call — see the pinning note in configure); reuse it here.
    let Some(handle) = app.try_state::<PanelHandle>() else {
        return; // conversion failed at setup: the window stays a plain surface
    };
    // Clone (a balanced objc retain/release pair — the object is pinned
    // immortal anyway): the msg_send receiver needs the owned Id type.
    let panel = handle.inner().0.clone();
    unsafe {
        let space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        let comps = [fr, fg, fb, 1.0f64];
        let cg = CGColorCreate(space, comps.as_ptr());
        if !cg.is_null() {
            // The window backing must stay CLEAR — an opaque window
            // background is square and would fill the corners outside the
            // radius, defeating the rounded clip. The rounded content
            // layer is the only painter: corners, the 1-2px band the
            // page's own rounded rect leaves, and any sub-pixel seam are
            // the panel's own colour; outside the radius is desktop.
            let clear: id = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![panel, setBackgroundColor: clear];
            let content: id = panel.content_view();
            let _: () = msg_send![content, setWantsLayer: YES];
            let layer: id = msg_send![content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setBackgroundColor: cg];
            }
            // The webview is the one painter the page cannot reach: its own
            // base colour is white whatever the theme says, so any area the
            // page leaves uncovered (overscroll bounce, a frame composed
            // before the sheet's own background is drawn) shows white through
            // a dark panel. Paint it with the panel's surface instead.
            paint_webview_backing(content, cg, fr, fg, fb);
        }
    }
}

/// Paint the WKWebView's own backing with the panel's surface colour.
///
/// Only the webview's class answers `setUnderPageBackgroundColor:`, so that
/// selector doubles as the identity test — no class-name coupling to wry — and
/// every message is guarded: this runs on every show and on each theme switch,
/// and a nil layer or an older WebKit has to degrade to "nothing repainted"
/// rather than to an unrecognized-selector trap.
#[cfg(target_os = "macos")]
unsafe fn paint_webview_backing(
    content: tauri_nspanel::cocoa::base::id,
    surface: tauri_nspanel::cocoa::base::id,
    r: f64,
    g: f64,
    b: f64,
) {
    use tauri_nspanel::cocoa::base::{id, YES};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let subviews: id = msg_send![content, subviews];
    if subviews.is_null() {
        return;
    }
    let count: usize = msg_send![subviews, count];
    for i in 0..count {
        let view: id = msg_send![subviews, objectAtIndex: i];
        if view.is_null() {
            continue;
        }
        let answers: i8 = msg_send![view, respondsToSelector: sel!(setUnderPageBackgroundColor:)];
        if answers == 0 {
            continue; // a chrome subview, not the webview
        }
        let mut layer: id = msg_send![view, layer];
        if layer.is_null() {
            let _: () = msg_send![view, setWantsLayer: YES];
            layer = msg_send![view, layer];
        }
        if !layer.is_null() {
            let _: () = msg_send![layer, setBackgroundColor: surface];
        }
        let color: id = msg_send![class!(NSColor), colorWithSRGBRed: r green: g blue: b alpha: 1.0f64];
        if !color.is_null() {
            let _: () = msg_send![view, setUnderPageBackgroundColor: color];
        }
        return;
    }
}


#[cfg(target_os = "macos")]
fn observe_context_switches(app: &AppHandle) {
    use std::ffi::CString;

    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let handle = app.clone();
    // Our own process id: clicking the tray icon activates this very app,
    // which used to fire DidActivateApplication and hide the panel right
    // after showing it — the "first click does nothing" bug.
    let our_pid = std::process::id();
    let block = ConcreteBlock::new(move |note: id| {
        unsafe {
            let user_info: id = msg_send![note, userInfo];
            if !user_info.is_null() {
                let key: id = msg_send![class!(NSString), stringWithUTF8String: "NSWorkspaceApplicationKey"];
                let activated: id = msg_send![user_info, objectForKey: key];
                let pid: i32 = if activated.is_null() { 0 } else { msg_send![activated, processIdentifier] };
                if pid == our_pid as i32 || pid == 0 {
                    return; // our own activation, or an event without an app
                }
            }
        }
        if let Some(window) = handle.get_webview_window(LABEL) {
            // Through the shared visibility choke point, not a raw hide: the
            // focus-loss handler fires for the very same activation, and a
            // double orderOut on the converted panel is the crash path.
            request_visibility(&window, false);
        }
    });
    let block = block.copy();

    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return;
        }
        let center: id = msg_send![workspace, notificationCenter];
        for name in [
            "NSWorkspaceActiveSpaceDidChangeNotification",
            "NSWorkspaceDidActivateApplicationNotification",
        ] {
            let c_name = CString::new(name).expect("static notification name");
            let ns_name: id = msg_send![class!(NSString), stringWithUTF8String: c_name.as_ptr()];
            // Both `RcBlock` and `&ConcreteBlock` deref to the block itself and
            // are leaked on purpose: the observer lives for the process.
            let observer: id = &*block as *const _ as id;
            let _: id = msg_send![center, addObserverForName: ns_name object: nil queue: nil usingBlock: observer];
        }
    }
}

/// The OS appearance as NSApplication resolves it — the same source the
/// page's `prefers-color-scheme` media query follows.
#[cfg(target_os = "macos")]
fn os_appearance_is_dark() -> bool {
    use tauri_nspanel::cocoa::base::id;
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let appearance: id = msg_send![app, effectiveAppearance];
        let name: id = msg_send![appearance, name];
        let utf8: *const std::ffi::c_char = msg_send![name, UTF8String];
        std::ffi::CStr::from_ptr(utf8).to_bytes() == b"NSAppearanceNameDarkAqua"
    }
}

#[cfg(not(target_os = "macos"))]
pub fn configure(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window(LABEL) {
        // A tray panel must be visible without taking foreground focus. tao
        // maps a non-focusable window to WS_EX_NOACTIVATE, while mouse clicks
        // still reach the WebView controls.
        let _ = window.set_focusable(false);
        crate::windows_surface::configure(&window);
        // The panel is created with `visible: false`. On current WebView2
        // Runtime builds, a controller created invisible can remain
        // pixel-less after the host HWND is later shown, even though a direct
        // SetIsVisible(true) succeeds. Keep the controller's compositor alive
        // while the parent stays hidden; show_window still controls the native
        // surface and the Rust visibility flag.
        set_webview_visible(&window, true);
        apply_windows_round_clip(&window);
        start_click_away_monitor(app);
    }
}
