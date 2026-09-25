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

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, Position, Rect, Size, WebviewWindow, Window,
    WindowEvent,
};

use crate::engine::PERIOD_EVENT;
use crate::tray;

pub const LABEL: &str = "main";
const WIDTH: f64 = 400.0;
const HEIGHT: f64 = 660.0;
/// Breathing room between the tray/taskbar and the panel edge.
const GAP: f64 = 8.0;

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

pub fn toggle(app: &AppHandle, rect: Option<Rect>) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        show(app, rect);
    }
}

pub fn show(app: &AppHandle, rect: Option<Rect>) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    anchor(&window, rect.or_else(|| tray::last_rect(app)));
    let _ = window.show();
    let _ = window.set_focus();
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
    let size = window
        .outer_size()
        .map(|s| (s.width as f64, s.height as f64))
        .unwrap_or((WIDTH, HEIGHT));

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
    #[cfg(target_os = "windows")]
    if matches!(event, WindowEvent::Resized(_)) {
        // The panel is fixed-size today, but Windows can change its physical
        // size when it crosses a monitor with a different DPI. Rebuild the
        // region so the native clip continues to match the CSS radius.
        if let Some(panel) = window.app_handle().get_webview_window(LABEL) {
            apply_windows_round_clip(&panel);
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
            let _ = panel.hide();
        }
    }
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
    }
    // If the conversion fails the window stays a plain `alwaysOnTop` surface:
    // the tray toggle still works, it just cannot follow into full screen.

    observe_context_switches(app);
}

/// Hides the panel when the user switches Space or activates another app.
#[cfg(target_os = "macos")]
fn observe_context_switches(app: &AppHandle) {
    use std::ffi::CString;

    use tauri_nspanel::block::ConcreteBlock;
    use tauri_nspanel::cocoa::base::{id, nil};
    use tauri_nspanel::objc::{class, msg_send, sel, sel_impl};

    let handle = app.clone();
    let block = ConcreteBlock::new(move |_note: id| {
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
        apply_windows_round_clip(&window);
    }
}
