//! Desktop token waterdrop window.
//!
//! This is created at runtime instead of being declared in tauri.conf.json so
//! macOS and Linux never receive a second window or a second tray surface.

pub const LABEL: &str = "bubble";

/// Fallback size for the classic waterdrop and an unknown skin ID.
pub const BOOTSTRAP_WINDOW_LOGICAL: f64 = 110.0;

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[cfg(target_os = "windows")]
use tauri::Emitter;
use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const PET_SKINS_MANIFEST: &str = include_str!("../../src/assets/pets/pet-skins.json");

fn logical_size_for_skin(skin: &str) -> (f64, f64) {
    logical_size_from_manifest(skin).unwrap_or((BOOTSTRAP_WINDOW_LOGICAL, BOOTSTRAP_WINDOW_LOGICAL))
}

fn logical_size_from_manifest(skin: &str) -> Option<(f64, f64)> {
    use std::sync::OnceLock;
    static MANIFEST: OnceLock<serde_json::Value> = OnceLock::new();
    let manifest = MANIFEST.get_or_init(|| serde_json::from_str(PET_SKINS_MANIFEST).unwrap_or_default());
    let layout = manifest.get("skins")?.get(skin)?.get("layout")?;
    let crop = layout.get("crop")?;
    let padding = layout.get("windowPadding")?;
    let number = |object: &serde_json::Value, field: &str| object.get(field)?.as_f64();
    let width = number(crop, "width")? + number(padding, "left")? + number(padding, "right")?;
    let height = number(crop, "height")? + number(padding, "top")? + number(padding, "bottom")?;
    (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0).then_some((width, height))
}

/// Resize the transparent native surface from the selected skin's own
/// manifest entry. Preserve its center so switching a tall portrait doesn't
/// jump away from the point the user was looking at.
pub fn resize_for_skin(app: &AppHandle, skin: &str) -> Result<(), String> {
    let Some(window) = app.get_webview_window(LABEL) else { return Ok(()) };
    let (width, height) = logical_size_for_skin(skin);
    let scale = window.scale_factor().unwrap_or(1.0).max(1.0);
    let wanted = PhysicalSize::new((width * scale).round() as u32, (height * scale).round() as u32);
    let old_size = window.outer_size().ok();
    if old_size == Some(wanted) { return Ok(()) }
    let old_position = window.outer_position().ok();
    window.set_size(LogicalSize::new(width, height)).map_err(|error| error.to_string())?;
    if let (Some(position), Some(old_size)) = (old_position, old_size) {
        let centered = PhysicalPosition::new(
            position.x + (old_size.width as i32 - wanted.width as i32) / 2,
            position.y + (old_size.height as i32 - wanted.height as i32) / 2,
        );
        let _ = window.set_position(centered);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn configure(app: &AppHandle) -> tauri::Result<()> {
    let skin = app.state::<crate::engine::Shared>().settings().bubble_skin;
    let (skin_width, skin_height) = logical_size_for_skin(skin.as_str());
    let window = match app.get_webview_window(LABEL) {
        Some(window) => {
            let _ = window.set_size(LogicalSize::new(skin_width, skin_height));
            window
        },
        None => WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
            .title("")
            .inner_size(skin_width, skin_height)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .visible(false)
            .focused(false)
            .build()?,
    };

    // A hoverable utility surface must not become the foreground application.
    let _ = window.set_focusable(false);
    crate::windows_surface::configure(&window);
    position_initial(&window);

    if app.state::<crate::engine::Shared>().settings().bubble_enabled {
        let _ = window.show();
        crate::panel::set_webview_visible(&window, true);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn configure(app: &AppHandle) -> tauri::Result<()> {
    let skin = app.state::<crate::engine::Shared>().settings().bubble_skin;
    let (skin_width, skin_height) = logical_size_for_skin(skin.as_str());
    let window = match app.get_webview_window(LABEL) {
        Some(window) => {
            let _ = window.set_size(LogicalSize::new(skin_width, skin_height));
            window
        },
        None => WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
            .title("")
            .inner_size(skin_width, skin_height)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .visible(false)
            .focused(false)
            .build()?,
    };

    // A hoverable utility surface must not become the foreground application.
    // tauri maps this to a non-activating panel on macOS — the main panel's
    // own semantics, already proven on this machine.
    let _ = window.set_focusable(false);
    position_initial(&window);

    if app.state::<crate::engine::Shared>().settings().bubble_enabled {
        let _ = window.show();
    }
    Ok(())
}

/// The Windows-only pieces of the Windows configure (the WebView2 visibility
/// re-assert and the non-client subclass) have no macOS counterpart: WKWebView
/// composites a transparent window without help.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn configure(_app: &tauri::AppHandle) -> tauri::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn set_enabled(app: &AppHandle, enabled: bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let _ = app.run_on_main_thread(move || {
        if enabled {
            let _ = window.show();
            crate::panel::set_webview_visible(&window, true);
        } else {
            let _ = window.hide();
            crate::panel::set_webview_visible(&window, false);
        }
    });
}

#[cfg(target_os = "macos")]
pub fn set_enabled(app: &AppHandle, enabled: bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let _ = app.run_on_main_thread(move || {
        if enabled {
            let _ = window.show();
        } else {
            let _ = window.hide();
        }
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn set_enabled(_app: &tauri::AppHandle, _enabled: bool) {}

/// Moves the bubble under the cursor until the left button is released.
///
/// `window.startDragging()` is unusable here: tao hands the drag to the native
/// `WM_NCLBUTTONDOWN` move loop, which refuses to move a `WS_EX_NOACTIVATE`
/// window — exactly what a hover bubble must be (see `configure`). So the
/// bubble drags itself: poll the cursor, pin the window's grab offset, watch
/// for the release on the same key state the panel's click-away monitor uses.
#[cfg(target_os = "windows")]
pub fn begin_drag(app: &AppHandle) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DRAGGING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let Some(window) = app.get_webview_window(LABEL) else {
        DRAGGING.store(false, std::sync::atomic::Ordering::Release);
        return;
    };
    let app = app.clone();
    crate::pet_notice::dismiss(&app, None);
    // The frontend must know a Rust-side drag is running: its hover timers
    // would otherwise dock the window out from under the drag loop the moment
    // the cursor outruns the window for a frame.
    let _ = app.emit("bubble-drag-started", ());
    std::thread::Builder::new()
        .name("tokenme-bubble-drag".into())
        .spawn(move || {
            let mut cursor = POINT { x: 0, y: 0 };
            let mut grab = (0, 0);
            let mut started = unsafe { GetCursorPos(&mut cursor) } != 0;
            if started {
                match window.outer_position() {
                    Ok(pos) => grab = (cursor.x - pos.x, cursor.y - pos.y),
                    Err(_) => started = false,
                }
            }
            if started {
                let mut moved = false;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                    let mut point = POINT { x: 0, y: 0 };
                    if unsafe { GetCursorPos(&mut point) } == 0 {
                        break;
                    }
                    let released = unsafe { (GetAsyncKeyState(0x01) as u16 & 0x8000) == 0 };
                    let target = PhysicalPosition::new(point.x - grab.0, point.y - grab.1);
                    if window.outer_position().is_ok_and(|p| p != target) {
                        let _ = window.set_position(target);
                        moved = true;
                    }
                    if released {
                        break;
                    }
                }
                if moved {
                    // The webview's pointer stream ends once the window starts
                    // moving under it; report the drop so it can dock.
                    let _ = app.emit("bubble-drag-ended", ());
                }
            }
            DRAGGING.store(false, std::sync::atomic::Ordering::Release);
        })
        .ok();
}

#[cfg(not(target_os = "windows"))]
pub fn begin_drag(_app: &tauri::AppHandle) {}


#[cfg(any(target_os = "windows", target_os = "macos"))]
fn position_initial(window: &WebviewWindow) {
    let Ok(Some(monitor)) = window.current_monitor() else { return };
    let position = monitor.position();
    let monitor_size = monitor.size();
    // Monitor coordinates are physical; bootstrap size and offsets are logical.
    let scale = monitor.scale_factor();
    let bootstrap = (BOOTSTRAP_WINDOW_LOGICAL * scale).round() as u32;
    let size = window
        .outer_size()
        .unwrap_or_else(|_| tauri::PhysicalSize::new(bootstrap, bootstrap));
    let right = (position.x + monitor_size.width as i32 - size.width as i32).max(position.x);
    let bottom = (position.y + monitor_size.height as i32 - size.height as i32).max(position.y);
    let x = (right - (24.0 * scale).round() as i32).max(position.x);
    let y = (position.y + (128.0 * scale).round() as i32).min(bottom);
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// Used by the panel click-away monitor so the bubble is not mistaken for a
/// desktop click while the panel is open.
#[cfg(target_os = "windows")]
pub fn contains_point(app: &AppHandle, x: i32, y: i32) -> bool {
    let Some(window) = app.get_webview_window(LABEL) else { return false };
    let Ok(hwnd) = window.hwnd() else { return false };
    let mut rect = windows_sys::Win32::Foundation::RECT::default();
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd.0) != 0
            && windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd.0, &mut rect) != 0
            && x >= rect.left
            && x < rect.right
            && y >= rect.top
            && y < rect.bottom
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn contains_point(_app: &tauri::AppHandle, _x: i32, _y: i32) -> bool {
    false
}
