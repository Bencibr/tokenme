//! Local desktop-pet notices, separate from the pet's drag/dock geometry.
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

pub const LABEL: &str = "pet-notice";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Morning, Noon, Evening, Night, Rest, Hello }

#[derive(Debug, Clone, Serialize)]
pub struct Notice { pub id: u64, pub kind: Kind, pub minutes: u32 }

#[derive(Default)]
pub struct NoticeState(pub std::sync::Mutex<Option<Notice>>);

#[derive(Serialize)]
pub struct Activity { idle_ms: Option<u64>, locked: bool, fullscreen: bool, visible: bool }

#[tauri::command]
pub fn pet_activity(app: AppHandle) -> Activity {
    let (idle_ms, locked, fullscreen) = input_state();
    let enabled = app.state::<crate::engine::Shared>().settings().bubble_enabled;
    let visible = enabled && app.get_webview_window(crate::bubble::LABEL)
        .is_some_and(|w| w.is_visible().unwrap_or(false));
    // A user-requested hello is an interaction, so fullscreen suppression is
    // reserved for automatic delivery. Lock/hidden still closes every notice.
    if locked || !visible || (fullscreen && get_pet_notice(app.clone()).is_some_and(|n| n.kind != Kind::Hello)) { dismiss(&app, None); }
    Activity { idle_ms, locked, fullscreen, visible }
}

#[cfg(target_os = "windows")]
fn input_state() -> (Option<u64>, bool, bool) {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{POINT, RECT};
    #[repr(C)] struct Input { size:u32, tick:u32 }
    #[repr(C)] struct Monitor { size:u32, screen:RECT, work:RECT, flags:u32 }
    #[link(name = "user32")]
    extern "system" {
        fn GetLastInputInfo(info:*mut Input) -> i32;
        fn OpenInputDesktop(flags:u32, inherit:i32, access:u32) -> *mut c_void;
        fn CloseDesktop(desktop:*mut c_void) -> i32;
        fn GetUserObjectInformationW(object:*mut c_void, index:i32, info:*mut c_void, size:u32, needed:*mut u32) -> i32;
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowRect(window:*mut c_void, rect:*mut RECT) -> i32;
        fn GetClassNameW(window:*mut c_void, name:*mut u16, size:i32) -> i32;
        fn MonitorFromPoint(point:POINT, flags:u32) -> *mut c_void;
        fn GetMonitorInfoW(monitor:*mut c_void, info:*mut Monitor) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" { fn GetTickCount() -> u32; }
    unsafe {
        let mut info = Input { size:std::mem::size_of::<Input>() as u32, tick:0 };
        let idle = (GetLastInputInfo(&mut info) != 0).then(|| u64::from(GetTickCount().wrapping_sub(info.tick)));
        let desktop = OpenInputDesktop(0, 0, 1); // DESKTOP_READOBJECTS, never switch desktops.
        let mut name = [0u16; 64]; let mut needed = 0;
        let unlocked = !desktop.is_null() && GetUserObjectInformationW(desktop, 2, name.as_mut_ptr().cast(), 128, &mut needed) != 0
            && String::from_utf16_lossy(&name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())]) == "Default";
        if !desktop.is_null() { CloseDesktop(desktop); }
        let window = GetForegroundWindow(); let mut rect = RECT::default(); let mut class = [0u16; 64];
        let count = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
        let desktop_window = matches!(String::from_utf16_lossy(&class[..count]).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd");
        let mut monitor = Monitor { size:std::mem::size_of::<Monitor>() as u32, screen:RECT::default(), work:RECT::default(), flags:0 };
        let fullscreen = !desktop_window && GetWindowRect(window, &mut rect) != 0
            && GetMonitorInfoW(MonitorFromPoint(POINT { x:rect.left + (rect.right - rect.left)/2, y:rect.top + (rect.bottom - rect.top)/2 }, 2), &mut monitor) != 0
            && rect.left <= monitor.screen.left && rect.top <= monitor.screen.top
            && rect.right >= monitor.screen.right && rect.bottom >= monitor.screen.bottom;
        (idle, !unlocked, fullscreen)
    }
}

#[cfg(not(target_os = "windows"))]
fn input_state() -> (Option<u64>, bool, bool) { (None, true, false) }

#[tauri::command]
pub fn get_pet_notice(app: AppHandle) -> Option<Notice> {
    app.state::<NoticeState>().0.lock().ok().and_then(|n| n.clone())
}

#[tauri::command]
pub async fn show_pet_notice(app: AppHandle, kind: Kind, minutes: u32) -> Result<Option<Notice>, String> {
    show(&app, kind, minutes, false)
}

#[tauri::command]
pub async fn preview_pet_notice(app: AppHandle) -> Result<Option<Notice>, String> { show(&app, Kind::Hello, 0, true) }

#[cfg(target_os = "windows")]
fn show(app: &AppHandle, kind: Kind, minutes: u32, preview: bool) -> Result<Option<Notice>, String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    let activity = pet_activity(app.clone());
    let prefs = app.state::<crate::engine::Shared>().settings().pet_companion;
    if !activity.visible || activity.locked || (!preview && activity.fullscreen) || (!preview &&
        (if kind == Kind::Rest { !prefs.rest_reminders } else { !prefs.greetings })) { return Ok(None); }
    if app.get_webview_window(LABEL).is_some_and(|w| w.is_visible().unwrap_or(false)) { return Ok(None); }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let notice = Notice { id:NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), kind, minutes:minutes.min(1440) };
    *app.state::<NoticeState>().0.lock().map_err(|_| "pet notice busy")? = Some(notice.clone());
    if app.get_webview_window(LABEL).is_none() {
        let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
            .title("").inner_size(280.0, 188.0).decorations(false).transparent(true).shadow(false)
            .always_on_top(true).skip_taskbar(true).resizable(false).maximizable(false).minimizable(false)
            .visible(false).focused(false).build().map_err(|e| e.to_string())?;
        let _ = window.set_focusable(false);
        // WebView2 creation runs outside synchronous command handlers; the
        // HWND subclass itself must still be installed on its owner UI thread.
        let surface = window.clone();
        let _ = app.run_on_main_thread(move || crate::windows_surface::configure(&surface));
    }
    app.emit("pet-notice-changed", &notice).map_err(|e| e.to_string())?;
    // The page reads the same snapshot on first load and reveals itself only
    // after the translated card has rendered, so the loading shell never flashes.
    crate::logging::info(&format!("pet: notice queued {:?} ({})", kind, notice.id));
    Ok(Some(notice))
}

#[cfg(not(target_os = "windows"))]
fn show(_app: &AppHandle, _kind: Kind, _minutes:u32, _preview:bool) -> Result<Option<Notice>, String> { Ok(None) }

#[tauri::command]
pub fn pet_notice_ready(app: AppHandle, id: u64) {
    let Some(notice) = get_pet_notice(app.clone()).filter(|n| n.id == id) else { return };
    let activity = pet_activity(app.clone());
    if !activity.visible || activity.locked || (activity.fullscreen && notice.kind != Kind::Hello) { dismiss(&app, Some(id)); return; }
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if get_pet_notice(handle.clone()).is_none_or(|n| n.id != id) { return; }
        reposition(&handle);
        let _ = window.set_focusable(false);
        let _ = window.show();
        #[cfg(target_os = "windows")] crate::panel::set_webview_visible(&window, true);
    });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(if notice.kind == Kind::Rest { 18 } else { 9 }));
        dismiss(&app, Some(id));
    });
}

#[derive(Clone, Serialize)]
struct Response { id:u64, action:String }

#[tauri::command]
pub fn respond_pet_notice(app: AppHandle, id:u64, action:String) -> Result<(), String> {
    if !matches!(action.as_str(), "dismiss" | "break" | "snooze" | "hello") { return Err("unknown pet response".into()); }
    if get_pet_notice(app.clone()).is_some_and(|n| n.id == id) {
        dismiss(&app, Some(id));
        app.emit("pet-response", Response { id, action }).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn dismiss(app:&AppHandle, id:Option<u64>) {
    let state = app.state::<NoticeState>();
    let Ok(mut current) = state.0.lock() else { return };
    if current.is_none() { return; }
    if id.is_some_and(|id| current.as_ref().is_none_or(|n| n.id != id)) { return; }
    *current = None; drop(current);
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
        #[cfg(target_os = "windows")] crate::panel::set_webview_visible(&window, false);
    }
    let _ = app.emit("pet-notice-changed", Option::<Notice>::None);
}

#[tauri::command]
pub fn reposition_pet_notice(app:AppHandle) { reposition(&app); }

fn reposition(app:&AppHandle) {
    let Some(note) = app.get_webview_window(LABEL) else { return };
    let Some(pet) = app.get_webview_window(crate::bubble::LABEL) else { return };
    let (Ok(pos), Ok(pet_size), Ok(Some(monitor)), Ok(size)) = (pet.outer_position(), pet.outer_size(), pet.current_monitor(), note.outer_size()) else { return };
    let left = monitor.position().x; let top = monitor.position().y;
    let right = left + monitor.size().width as i32; let bottom = top + monitor.size().height as i32;
    let gap = (12.0 * monitor.scale_factor()) as i32;
    let width = size.width as i32; let height = size.height as i32;
    let x = if pos.x + pet_size.width as i32 + gap + width <= right { pos.x + pet_size.width as i32 + gap } else { pos.x - width - gap };
    let x = x.clamp(left + gap, (right - width - gap).max(left + gap));
    let y = (pos.y + pet_size.height as i32 / 2 - height / 2).clamp(top + gap, (bottom - height - gap).max(top + gap));
    let _ = note.set_position(tauri::PhysicalPosition::new(x,y));
}

#[cfg(target_os = "windows")]
pub fn contains_point(app:&AppHandle, x:i32, y:i32) -> bool {
    let Some(window) = app.get_webview_window(LABEL) else { return false };
    let Ok(hwnd) = window.hwnd() else { return false };
    let mut rect = windows_sys::Win32::Foundation::RECT::default();
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd.0) != 0
            && windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd.0, &mut rect) != 0
            && x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
    }
}
