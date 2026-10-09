//! Windows non-client painting policy for the borderless utility surfaces.
//!
//! tao and Wry both install their own window subclasses.  This module adds a
//! small, independent subclass to the top-level Tauri HWND and keeps those
//! handlers in the chain.  The windows have no native caption, so forwarding
//! an activation change with `lParam = -1` lets tao update its activation state
//! without allowing `DefWindowProc` to repaint a cached non-client caption.

#![cfg(target_os = "windows")]

use tauri::WebviewWindow;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsIconic, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
    STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCPAINT, WM_NCDESTROY,
    WM_STYLECHANGING, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
    WS_THICKFRAME,
};

// The subclass is identified by the callback and this id.  Re-running
// configure updates the existing registration instead of adding a duplicate.
const SUBCLASS_ID: usize = 0x544D_4E43; // "TMNC"
const NATIVE_FRAME: u32 = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

unsafe extern "system" fn surface_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match msg {
        WM_STYLECHANGING if wparam as i32 == GWL_STYLE => {
            // tao writes WS_CAPTION | WS_SYSMENU for every top-level window,
            // even when its decorations marker is false. Keep its visibility
            // and focus updates, but enforce the utility host's native style
            // before Windows/DWM can observe or paint a caption again.
            if let Some(styles) = (lparam as *mut STYLESTRUCT).as_mut() {
                styles.styleNew &= !NATIVE_FRAME;
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }

        WM_NCCALCSIZE => {
            // Both RECT (wParam=0) and NCCALCSIZE_PARAMS (wParam=1) keep the
            // entire host as client area. There is no native title/frame.
            0
        }

        WM_NCACTIVATE => {
            // Keep tao/Wry's activation bookkeeping in the subclass chain.
            // For a live borderless window, -1 tells DefWindowProc not to
            // repaint the non-client area. Microsoft documents the original
            // lParam path for minimized windows, so preserve it there.
            let next_lparam = if IsIconic(hwnd) != 0 { lparam } else { -1 };
            let result = DefSubclassProc(hwnd, msg, wparam, next_lparam);

            // For deactivation Windows requires TRUE to continue the state
            // transition; for activation the return value is ignored.
            if wparam == 0 { 1 } else { result }
        }

        WM_NCPAINT => {
            // There is no native frame to paint.  Do not call the next
            // handler, because DefWindowProc is the component that paints the
            // ghost caption strip.
            0
        }

        WM_NCDESTROY => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            let _ = RemoveWindowSubclass(hwnd, Some(surface_proc), subclass_id);
            result
        }

        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

/// Install the non-client paint policy on one top-level Tauri window.
///
/// This must run on the thread that owns the HWND. `SetWindowSubclass` does
/// not support cross-thread subclassing. The operation is idempotent for this
/// callback/id pair, so setup can safely call it more than once.
pub(crate) fn configure(window: &WebviewWindow) {
    let Ok(native_hwnd) = window.hwnd() else { return };
    let hwnd = native_hwnd.0 as HWND;

    let installed = unsafe { SetWindowSubclass(hwnd, Some(surface_proc), SUBCLASS_ID, 0) } != 0;
    if installed {
        // Remove the frame that tao wrote before this subclass was installed.
        // Future state changes pass through WM_STYLECHANGING above, so this
        // initialization never needs a right-click/resize/show-time repair.
        unsafe {
            let current = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            SetWindowLongPtrW(hwnd, GWL_STYLE, (current & !NATIVE_FRAME) as isize);
            let _ = SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);
        }
        crate::logging::info(&format!("surface: borderless non-client policy installed ({})", window.label()));
    } else {
        crate::logging::error(&format!("surface: non-client policy installation failed ({})", window.label()));
    }
}
