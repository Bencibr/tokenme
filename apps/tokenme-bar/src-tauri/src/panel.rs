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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Position, Rect, Size,
    WebviewWindow, Window, WindowEvent,
};

use crate::engine::PERIOD_EVENT;
use crate::tray;
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenArea {
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
    if window.is_visible().unwrap_or(false) {
        request_visibility(&window, false);
    } else if accept_visibility_request(true) {
        show_window(&window, rect.or_else(|| tray::last_rect(app)));
    }
}

pub fn show(app: &AppHandle, rect: Option<Rect>) {
    if !accept_visibility_request(true) {
        return;
    }
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    show_window(&window, rect.or_else(|| tray::last_rect(app)));
}

fn show_window(window: &WebviewWindow, rect: Option<Rect>) {
    anchor(window, rect);
    let _ = window.show();

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

fn request_visibility(window: &WebviewWindow, visible: bool) {
    if !accept_visibility_request(visible) {
        return;
    }
    if visible {
        let _ = window.show();
    } else {
        let _ = window.hide();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
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
    let rect = rect.or_else(|| tray::last_rect(window.app_handle()));

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

    if let Some(rect) = rect {
        let (x, y) = rect_origin(&rect);
        let (w, h) = rect_size(&rect);
        let (x, y) = clamp_point(window, x + w / 2.0 - WIDTH / 2.0, y + h + GAP);
        let _ = window.set_position(PhysicalPosition::new(x, y));
    } else {
        clamp(window);
    }
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

#[cfg(target_os = "windows")]
fn anchor_windows(window: &WebviewWindow, rect: Option<&Rect>) -> bool {
    let (x, y) = if let Some(rect) = rect {
        let (x, y) = rect_origin(rect);
        let (w, h) = rect_size(rect);
        (x + w / 2.0, y + h / 2.0)
    } else if let Ok(pos) = window.outer_position() {
        (pos.x as f64, pos.y as f64)
    } else {
        return false;
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
    } else if let Ok(pos) = window.outer_position() {
        let (x, y) = clamp_in_bounds(pos.x as f64, pos.y as f64, size, area.work, GAP);
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

    // Focus loss is the only reliable "clicked elsewhere" signal for a panel
    // that never activates the app.
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
    let Some(rect) = tray::last_rect(app) else { return false };
    let (left, top) = rect_origin(&rect);
    let (width, height) = rect_size(&rect);
    let right = left + width;
    let bottom = top + height;
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
}

#[cfg(target_os = "macos")]
pub fn configure(app: &AppHandle) {
    use tauri_nspanel::cocoa::appkit::{NSMainMenuWindowLevel, NSWindowCollectionBehavior};
    use tauri_nspanel::WebviewWindowExt as _;

    /// `NSWindowStyleMask` bits the cocoa bindings leave out or name for the
    /// pre-10.12 era: borderless (0), nonactivating panel (1<<7), full-size
    /// content view (1<<15).
    const STYLE: i32 = (1 << 7) | (1 << 15);

    /// Keeps the swizzled panel alive for the lifetime of the app.
    struct PanelHandle(#[allow(dead_code)] tauri_nspanel::Panel);

    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    // Opaque backing, not frosted glass: the under-window material let
    // whatever sat behind the panel bleed through the translucent surface —
    // a dark IDE turned the sheet's bottom edge into a mismatched black
    // smear. The theme-coloured backing below (apply_window_background) is
    // the single ground truth now; the page's translucent surface composites
    // onto it into exactly --surface-solid.
    if let Ok(panel) = window.to_panel() {
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
        apply_window_background(&window, None);
    }
    // If the conversion fails the window stays a plain `alwaysOnTop` surface:
    // the tray toggle still works, it just cannot follow into full screen.

    observe_context_switches(app);
}

/// Hides the panel when the user switches Space or activates another app.
/// The window's own backing shows wherever the webview viewport rounds a few
/// pixels short of the window height (fractional-scale rounding) — a dark
/// strip under the settings sheet on a light theme. Paint the panel backing
/// with the theme's own surface so the strip and the sheet are one colour.
#[cfg(target_os = "macos")]
pub fn apply_window_background(window: &tauri::WebviewWindow, theme: Option<crate::settings::Theme>) {
    use tauri_nspanel::cocoa::base::{id, YES};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::WebviewWindowExt as _;

    let resolved = theme.or_else(|| Some(crate::settings::Settings::load().theme));
    // system follows the OS appearance the window itself reports
    let dark = match resolved {
        Some(crate::settings::Theme::Light) => false,
        Some(crate::settings::Theme::Dark) => true,
        _ => window.theme().ok() == Some(tauri::Theme::Dark),
    };
    // --surface-solid: light #f9fafc · dark #19212d
    let (fr, fg, fb) = if dark { (0.098, 0.129, 0.176) } else { (0.976, 0.980, 0.988) };

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
    if let Ok(panel) = window.to_panel() {
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
            }
        }
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
            if window.is_visible().unwrap_or(false) {
                let _ = window.hide();
            }
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

#[cfg(not(target_os = "macos"))]
pub fn configure(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window(LABEL) {
        // A tray panel must be visible without taking foreground focus. tao
        // maps a non-focusable window to WS_EX_NOACTIVATE, while mouse clicks
        // still reach the WebView controls.
        let _ = window.set_focusable(false);
        apply_windows_round_clip(&window);
        start_click_away_monitor(app);
    }
}
