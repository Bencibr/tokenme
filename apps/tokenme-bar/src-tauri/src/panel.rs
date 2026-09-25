//! The panel window: where it appears under the tray icon, and how it leaves.
//!
//! macOS converts the webview window into a non-activating `NSPanel` so it can
//! float over full-screen spaces without stealing focus, and anchors it with the
//! tray rect from the click event — the positioner plugin cannot position a
//! swizzled panel, so that anchor math lives here. Windows uses the positioner's
//! `TrayBottomCenter`, and both platforms clamp back onto a connected monitor.

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
/// Breathing room between the menu bar and the panel's top edge.
const GAP: f64 = 6.0;

pub fn toggle(app: &AppHandle, rect: Option<Rect>) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        show(app, rect);
    }
}

pub fn show(app: &AppHandle, rect: Option<Rect>) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
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
    #[cfg(target_os = "windows")]
    {
        use tauri_plugin_positioner::{Position as TrayPosition, WindowExt as _};
        // The plugin already recorded the tray geometry from the tray event.
        if window
            .move_window_constrained(TrayPosition::TrayBottomCenter)
            .is_ok()
        {
            clamp(window);
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

    let Some(window) = app.get_webview_window(LABEL) else { return };
    // Frosted glass: the native under-window material blurs the desktop
    // behind the panel wherever the page paints translucent surface. Set
    // through tauri's supported API before the NSPanel conversion (raw
    // addSubview calls here raise during app setup); the corner clip below
    // rounds the material.
    let _ = window.set_effects(Some(tauri::utils::config::WindowEffectsConfig {
        effects: vec![tauri::window::Effect::UnderWindowBackground],
        state: Some(tauri::window::EffectState::Active),
        radius: None,
        color: None,
    }));
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
pub fn configure(_app: &AppHandle) {}
