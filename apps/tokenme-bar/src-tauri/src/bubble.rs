//! Windows-only token waterdrop window.
//!
//! This is created at runtime instead of being declared in tauri.conf.json so
//! macOS and Linux never receive a second window or a second tray surface.

pub const LABEL: &str = "bubble";

/// The window is larger than the ball it shows: the core is 76 logical px
/// (`--bubble-core` in bubble.css) and its box-shadow reaches ~31 px past the
/// core edge (9 px offset + 22 px blur), so a window the size of the ball clips
/// the shadow into a hard square edge. 144 leaves a 34 px margin on every side.
/// BubbleApp.tsx keeps the same two numbers and derives its dock/peek math
/// from them.
#[cfg(target_os = "windows")]
pub const WINDOW_LOGICAL: f64 = 144.0;

#[cfg(target_os = "windows")]
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[cfg(target_os = "windows")]
pub fn configure(app: &AppHandle) -> tauri::Result<()> {
    let window = match app.get_webview_window(LABEL) {
        Some(window) => window,
        None => WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
            .title("")
            .inner_size(WINDOW_LOGICAL, WINDOW_LOGICAL)
            .min_inner_size(WINDOW_LOGICAL, WINDOW_LOGICAL)
            .max_inner_size(WINDOW_LOGICAL, WINDOW_LOGICAL)
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

#[cfg(not(target_os = "windows"))]
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

#[cfg(not(target_os = "windows"))]
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


#[cfg(target_os = "windows")]
fn position_initial(window: &WebviewWindow) {
    let Ok(Some(monitor)) = window.current_monitor() else { return };
    let position = monitor.position();
    let monitor_size = monitor.size();
    let size = window
        .outer_size()
        .unwrap_or_else(|_| tauri::PhysicalSize::new(WINDOW_LOGICAL as u32, WINDOW_LOGICAL as u32));
    let x = position.x + monitor_size.width as i32 - size.width as i32 - 24;
    let y = position.y + 128;
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

#[cfg(not(target_os = "windows"))]
pub fn contains_point(_app: &tauri::AppHandle, _x: i32, _y: i32) -> bool {
    false
}
