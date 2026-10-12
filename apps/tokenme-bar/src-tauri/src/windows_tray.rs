//! Windows paints the PNG literally; AppKit's template recoloring is macOS-only.
//! Follow the taskbar's system theme, independently of the panel/app theme.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::image::Image;
use tauri::{AppHandle, Manager, Runtime};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP, WM_NCDESTROY, WM_SETTINGCHANGE, WM_THEMECHANGED};

const SUBCLASS_ID: usize = 0x544D_5452; // "TMTR", separate from windows_surface
const WM_TRAY_THEME: u32 = WM_APP + 0x54;

struct Assets {
    light: Image<'static>,
    dark: Image<'static>,
    is_dark: AtomicBool,
}

impl Assets {
    fn image(&self, dark: bool) -> Image<'static> {
        if dark { self.dark.clone() } else { self.light.clone() }
    }
}

fn dark_taskbar() -> bool {
    // AppsUseLightTheme (and the panel's pinned theme) can disagree with the
    // taskbar. A missing setting uses Windows' original dark taskbar default.
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let name: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
    let mut light = 0u32;
    let mut bytes = std::mem::size_of_val(&light) as u32;
    let result = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD,
            std::ptr::null_mut(), (&mut light as *mut u32).cast(), &mut bytes)
    };
    result != ERROR_SUCCESS || light == 0
}

fn white_glyph(black: &Image<'_>) -> Image<'static> {
    // Only the ink changes: exact-size coverage and antialiased edges survive.
    let mut rgba = black.rgba().to_vec();
    for pixel in rgba.chunks_exact_mut(4) { pixel[..3].fill(255); }
    Image::new_owned(rgba, black.width(), black.height())
}

pub(crate) fn init(app: &AppHandle, light: Image<'static>) -> Image<'static> {
    let dark = dark_taskbar();
    let assets = Assets { dark: white_glyph(&light), light, is_dark: AtomicBool::new(dark) };
    let icon = assets.image(dark);
    crate::logging::info(&format!("tray: {} taskbar, {}px glyph", if dark { "dark" } else { "light" }, icon.width()));
    app.manage(assets);
    icon
}

pub(crate) fn image<R: Runtime>(app: &AppHandle<R>) -> Image<'static> {
    let assets = app.state::<Assets>();
    assets.image(assets.is_dark.load(Ordering::Relaxed))
}

/// Called on the UI thread by the native notification and by normal tray
/// paints (a fallback if installing the notification hook ever fails).
pub(crate) fn refresh<R: Runtime>(app: &AppHandle<R>) {
    let Some(assets) = app.try_state::<Assets>() else { return };
    let dark = dark_taskbar();
    if assets.is_dark.load(Ordering::Relaxed) == dark { return; }
    let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) else { return };
    match tray.set_icon(Some(assets.image(dark))) {
        Ok(()) => {
            assets.is_dark.store(dark, Ordering::Relaxed);
            crate::logging::info(&format!("tray: switched to {} taskbar glyph", if dark { "dark" } else { "light" }));
        }
        Err(e) => crate::logging::error(&format!("tray: theme update failed: {e}")),
    }
}

unsafe extern "system" fn theme_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, id: usize, data: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        // Remove before forwarding: the reference cannot be reused by a
        // nested notification while the HWND tears down.
        RemoveWindowSubclass(hwnd, Some(theme_proc), id);
        drop(Box::from_raw(data as *mut AppHandle));
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }
    if msg == WM_SETTINGCHANGE || msg == WM_THEMECHANGED {
        // Post rather than run_on_main_thread: Tauri executes that inline
        // when already on the UI thread, inside this native callback.
        if PostMessageW(hwnd, WM_TRAY_THEME, 0, 0) == 0 {
            crate::logging::error("tray: theme notification could not be queued");
        }
    } else if msg == WM_TRAY_THEME {
        refresh(&*(data as *const AppHandle));
        return 0;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// A hidden top-level HWND still receives Windows' settings broadcasts.
/// Install during setup, on the thread that owns the panel's HWND.
pub(crate) fn watch(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
    let Ok(native) = window.hwnd() else { return };
    let hwnd = native.0 as HWND;
    let mut existing = 0;
    if unsafe { GetWindowSubclass(hwnd, Some(theme_proc), SUBCLASS_ID, &mut existing) } != 0 { return; }
    let data = Box::into_raw(Box::new(app.clone()));
    if unsafe { SetWindowSubclass(hwnd, Some(theme_proc), SUBCLASS_ID, data as usize) } == 0 {
        unsafe { drop(Box::from_raw(data)); }
        crate::logging::error("tray: taskbar theme hook failed; periodic refresh remains active");
    }
    // Close the gap between startup's registry read and hook installation.
    refresh(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_glyph_keeps_exact_size_and_alpha_at_every_tray_dpi() {
        for black in [
            tauri::include_image!("icons/tray-icon-16.png"),
            tauri::include_image!("icons/tray-icon-20.png"),
            tauri::include_image!("icons/tray-icon-24.png"),
            tauri::include_image!("icons/tray-icon-28.png"),
            tauri::include_image!("icons/tray-icon-32.png"),
        ] {
            let white = white_glyph(&black);
            assert_eq!((white.width(), white.height()), (black.width(), black.height()));
            assert!(black.rgba().chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255));
            for (before, after) in black.rgba().chunks_exact(4).zip(white.rgba().chunks_exact(4)) {
                assert_eq!(&after[..3], &[255, 255, 255]);
                assert_eq!(after[3], before[3]);
            }
        }
    }
}
