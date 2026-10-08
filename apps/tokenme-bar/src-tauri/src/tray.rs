//! Menu-bar presence: the icon, the live title, and the right-click menu.

use tauri::menu::{CheckMenuItem, MenuBuilder, MenuEvent, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Rect, Runtime};
use tauri_plugin_autostart::ManagerExt;
use usage_core::pricing::PricingOptions;
use usage_core::{PricingMap, QuotaView, Report};

use crate::commands::TrayState;
use crate::engine::{EngineChannel, Msg, Shared};
use crate::lang::{self, Lang};
use crate::panel;
use crate::settings::{Settings, TrayMode};

pub const TRAY_ID: &str = "tokenme";

/// Fixed-ish width: the status bar must not jitter on every refresh.
fn compact(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if abs >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{:.0}", n)
    }
}

fn money(n: f64) -> String {
    if n > 0.0 && n < 0.01 {
        "<$0.01".into()
    } else {
        format!("${:.2}", n)
    }
}

fn countdown(ms: i64, now_ms: i64) -> String {
    let diff = (ms - now_ms).max(0) / 60_000;
    let (d, h, m) = (diff / 1440, (diff % 1440) / 60, diff % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

/// The newest still-open quota window with the highest usage.
///
/// `resets_at_ms == 0` means the source never reports a reset (Qoder's credit
/// plan), which is "always open" — a plan that has run out is the one bar that
/// belongs in the menu bar, so dropping it for lack of a countdown is wrong.
fn live_quota(report: &Report, now_ms: i64) -> Option<&QuotaView> {
    report
        .quotas
        .iter()
        .filter(|q| q.resets_at_ms == 0 || q.resets_at_ms > now_ms)
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
}

/// `(title, tooltip)`. Windows ignores the title, so the tooltip repeats it.
pub fn label_for(report: &Report, mode: TrayMode) -> (Option<String>, String) {
    let now_ms = report.generated_at_ms;
    let day = &report.day;
    let quota = live_quota(report, now_ms);

    let tokens_seg = compact(day.summary.total_tokens);
    let cost_seg = money(day.summary.cost);
    let title = match mode {
        TrayMode::TrayOnly => None,
        TrayMode::TokensOnly | TrayMode::TrayTokens => Some(tokens_seg.clone()),
        TrayMode::CostOnly | TrayMode::TrayCost => Some(cost_seg.clone()),
        TrayMode::TrayTokensCost => Some(format!("{tokens_seg} {cost_seg}")),
    };

    let l = lang::get();
    let mut tooltip = format!(
        "TokenMe · {}\n{} · {} tokens · {} {}",
        l.str("今日", "Today"),
        money(day.summary.cost),
        compact(day.summary.total_tokens),
        day.summary.requests,
        l.str("次", "requests"),
    );
    if day.summary.credits > 0.0 {
        tooltip.push_str(&format!(" · {:.2} credits", day.summary.credits));
    }
    tooltip.push_str(&format!(" · v{}", env!("CARGO_PKG_VERSION")));
    if let Some(q) = quota {
        let reset = if q.resets_at_ms > 0 {
            match l {
                Lang::Zh => format!(" · {} 后重置", countdown(q.resets_at_ms, now_ms)),
                Lang::En => format!(" · resets in {}", countdown(q.resets_at_ms, now_ms)),
            }
        } else {
            String::new()
        };
        tooltip.push_str(&format!(
            "\n{} {} {:.0}%{}",
            q.tool,
            l.str("配额", "quota"),
            q.used_percent,
            reset
        ));
    }
    tooltip.push_str(if report.pricing.stale {
        l.str("\n价格快照已过期，成本为估算", "\nPrice snapshot is stale; costs are estimates")
    } else {
        l.str("\n价格为实时估算，非账单", "\nPrices are live estimates, not billing")
    });
    (title, tooltip)
}

pub fn tray_state(app: &AppHandle) -> Option<TrayState> {
    let shared = app.state::<Shared>();
    let settings = shared.settings();
    let report = shared.report()?;
    let (title, tooltip) = label_for(&report, settings.tray_mode);
    Some(TrayState {
        label: title.unwrap_or_default(),
        tooltip,
        mode: settings.tray_mode,
    })
}

/// AppKit mutations (NSStatusItem, muda menu items) are main-thread-only on
/// macOS, but the tray is poked from the engine thread and from async commands
/// too. Every tray-touching body goes through here; calling this from the main
/// thread itself is fine — the body then runs after the current handler
/// returns. Skipping the update (instead of crashing the pool drain) when the
/// event loop is already gone, i.e. during shutdown.
fn on_main<R: Runtime, F: FnOnce() + Send + 'static>(app: &AppHandle<R>, body: F) {
    if let Err(e) = app.run_on_main_thread(body) {
        crate::logging::error(&format!("tray update dropped, event loop is gone: {e}"));
    }
}

/// Pushes the current numbers onto the status bar. Callable from any thread;
/// the painting itself is marshalled onto the main thread.
pub fn refresh<R: Runtime>(app: &AppHandle<R>, report: &Report, mode: TrayMode) {
    let handle = app.clone();
    let report = report.clone();
    on_main(app, move || paint(&handle, &report, mode));
}

fn paint<R: Runtime>(app: &AppHandle<R>, report: &Report, mode: TrayMode) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let (title, tooltip) = label_for(report, mode);
    // Re-setting the image repaints the status item — visible as a flicker on
    // every engine cycle. The image only depends on the mode, so touch it
    // when the mode actually changes and leave it alone otherwise.
    let changed = match app.state::<LastMode>().0.lock() {
        Ok(mut last) => {
            let changed = *last != Some(mode);
            if changed {
                *last = Some(mode);
            }
            changed
        }
        Err(_) => true, // poisoned: repaint rather than risk a stale image
    };
    if changed {
        // 仅Token/仅花费 hide the icon entirely: a text-only status item.
        let icon = if mode.shows_icon() {
            app.try_state::<TrayAssets>().map(|a| a.0.clone())
        } else {
            None
        };
        let _ = tray.set_icon(icon);
        // set_icon resets the template flag; without it macOS stops recoloring
        // the glyph and the black PNG ships as-is — invisible on a dark bar.
        let _ = tray.set_icon_as_template(true);
    }
    // tray-icon's set_title(None) is a no-op, so clearing means an empty
    // string — otherwise 仅托盘 keeps the previous mode's text forever.
    // Same-value writes are skipped entirely: see LastPaint.
    let title_str = title.clone().unwrap_or_default();
    let mut changed = true;
    if let Some(paint) = app.try_state::<LastPaint>() {
        if let Ok(mut last) = paint.0.lock() {
            if last.0 == title_str && last.1 == tooltip {
                changed = false;
            } else {
                *last = (title_str.clone(), tooltip.clone());
            }
        }
    }
    if changed {
        let _ = tray.set_title(Some(title_str.as_str()));
        let _ = tray.set_tooltip(Some(tooltip));
    }
}

/// Keeps the checkable autostart row in sync with the settings file. The
/// option inside the mutex is swapped when the language changes rebuilds the
/// menu — a second `manage` would be ignored, leaving sync pointed at a dead
/// row.
struct MenuItems(std::sync::Mutex<Option<CheckMenuItem<tauri::Wry>>>);

impl MenuItems {
    fn set_checked(&self, on: bool) {
        if let Ok(guard) = self.0.lock() {
            if let Some(item) = guard.as_ref() {
                item.set_checked(on).ok();
            }
        }
    }
}

/// The dual-ring icon, kept so 仅Token/仅花费 can hide it and the 托盘 modes
/// can put it back without re-reading the file.
struct TrayAssets(tauri::image::Image<'static>);

/// The last mode the status item was painted with — the guard that keeps
/// periodic refreshes from re-setting the image (and flickering) on no-op.
struct LastMode(std::sync::Mutex<Option<TrayMode>>);

/// The strings the status item currently carries. Re-writing the same title
/// and tooltip through AppKit on every engine cycle (the watcher can fire
/// several times a second) churns NSStatusItem internals for zero visual
/// change — and that churn path is where the pool drain meets KVO state.
/// Only real changes are pushed; a same-value write is skipped.
struct LastPaint(std::sync::Mutex<(String, String)>);

fn build_menu(app: &AppHandle) -> tauri::Result<(tauri::menu::Menu<tauri::Wry>, CheckMenuItem<tauri::Wry>)> {
    let l = lang::get();
    let quit = MenuItem::with_id(app, "quit", l.str("退出", "Quit"), true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", l.str("开机自启", "Launch at Login"), true, false, None::<&str>)?;
    let open_data = MenuItem::with_id(app, "data", l.str("打开数据目录", "Open Data Folder"), true, None::<&str>)?;
    let week = MenuItem::with_id(app, "week", l.str("本周", "This Week"), true, None::<&str>)?;
    let today = MenuItem::with_id(app, "today", l.str("今日花费", "Today's Cost"), true, None::<&str>)?;
    let refresh_item = MenuItem::with_id(app, "refresh", l.str("刷新", "Refresh"), true, None::<&str>)?;

    autostart
        .set_checked(app.state::<Shared>().settings().autostart)
        .ok();

    let menu = MenuBuilder::new(app)
        .item(&refresh_item)
        .separator()
        .item(&today)
        .item(&week)
        .separator()
        .item(&open_data)
        .separator()
        .item(&autostart)
        .separator()
        .item(&quit)
        .build()?;
    Ok((menu, autostart))
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let (menu, autostart) = build_menu(app)?;
    app.manage(MenuItems(std::sync::Mutex::new(Some(autostart))));
    let icon = tauri::include_image!("icons/tray-icon.png");
    app.manage(TrayAssets(icon.clone()));
    // The builder already painted the icon for the startup mode's default;
    // seeding None makes the first publish apply the real mode's imagery.
    app.manage(LastMode(std::sync::Mutex::new(None)));
    // Empty strings: build() painted icon-only, so the first publish always
    // writes whatever the mode resolves to.
    app.manage(LastPaint(std::sync::Mutex::new((String::new(), String::new()))));

    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("TokenMe")
        .icon(icon)
        .icon_as_template(true)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event)
        .build(app)?;
    #[cfg(target_os = "windows")]
    promote_taskbar_icon();
    Ok(())
}

/// Win11 files every tray icon under HKCU\Control Panel\NotifyIconSettings,
/// keyed by a hash of the exe path, and a fresh entry starts hidden inside the
/// overflow flyout (IsPromoted absent) — every new install path would surface
/// its tray icon only after a manual trip through taskbar settings. The tray
/// is this app's whole interface, so an absent value is flipped to promoted;
/// a value that is present was set by the user (shown or hidden) and is never
/// touched. Explorer creates the entry only after the icon is first shown, so
/// the lookup retries on its own thread and stops at the first decision.
#[cfg(target_os = "windows")]
fn promote_taskbar_icon() {
    std::thread::Builder::new()
        .name("tokenme-tray-promote".into())
        .spawn(|| {
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(_) => return,
            };
            for _ in 0..24 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if promote_taskbar_icon_once(&exe) {
                    return;
                }
            }
        })
        .ok();
}

/// One lookup pass. `true` = decided (promoted, or the user already chose).
#[cfg(target_os = "windows")]
fn promote_taskbar_icon_once(exe: &std::path::Path) -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe {
        let mut root = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide("Control Panel\\NotifyIconSettings").as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut root,
        ) != 0
        {
            return false;
        }
        let want: Vec<u16> = exe.to_string_lossy().encode_utf16().collect();
        let mut decided = false;
        let mut index: u32 = 0;
        loop {
            let mut name = [0u16; 256];
            let mut len = 256u32;
            if RegEnumKeyExW(
                root,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) != 0
            {
                break;
            }
            index += 1;
            let mut sub = std::ptr::null_mut();
            if RegOpenKeyExW(root, name.as_ptr(), 0, KEY_READ | KEY_SET_VALUE, &mut sub) != 0 {
                continue;
            }
            let mut path = [0u16; 520];
            let mut path_len = std::mem::size_of_val(&path) as u32;
            let mut vtype: u32 = 0;
            let got = RegGetValueW(
                sub,
                std::ptr::null(),
                wide("ExecutablePath").as_ptr(),
                RRF_RT_REG_SZ,
                &mut vtype,
                path.as_mut_ptr() as _,
                &mut path_len,
            );
            if got == 0 {
                let entry = &path[..path.len() >> 1];
                let entry = &entry[..entry.iter().position(|&c| c == 0).unwrap_or(entry.len())];
                if entry.eq_ignore_ascii_case(&want[..want.len() - 1]) {
                    let mut current: u32 = 0;
                    let mut cur_len = std::mem::size_of::<u32>() as u32;
                    let present = RegGetValueW(
                        sub,
                        std::ptr::null(),
                        wide("IsPromoted").as_ptr(),
                        RRF_RT_REG_DWORD,
                        std::ptr::null_mut(),
                        &mut current as *mut u32 as _,
                        &mut cur_len,
                    ) == 0;
                    if !present {
                        let one: u32 = 1;
                        RegSetValueExW(
                            sub,
                            wide("IsPromoted").as_ptr(),
                            0,
                            REG_DWORD,
                            &one as *const u32 as _,
                            4,
                        );
                        crate::logging::info("tray: promoted the taskbar icon (fresh entry)");
                    }
                    decided = true;
                }
            }
            RegCloseKey(sub);
            if decided {
                break;
            }
        }
        RegCloseKey(root);
        decided
    }
}

fn on_tray_event(tray: &TrayIcon<tauri::Wry>, event: TrayIconEvent) {
    let app = tray.app_handle().clone();
    // The positioner plugin tracks the tray rect for the Windows anchor.
    #[cfg(target_os = "windows")]
    tauri_plugin_positioner::on_tray_event(&app, &event);

    match event {
        // Right-click is served by the native context menu attached above.
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            rect,
            ..
        } => panel::toggle(&app, Some(rect)),
        // On Windows a double click is preceded by the first left-button-up
        // click. Toggling here as well would immediately undo the first toggle.
        TrayIconEvent::DoubleClick { .. } => {}
        _ => {}
    }
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "refresh" => {
            // The full refresh the panel's 刷新 runs: a fresh models.dev price
            // table first (the engine re-summarizes when it lands), then the
            // manual pass — the engine drops the quota cache on Refresh, so
            // every vendor is re-probed for real. The fetch blocks on network,
            // so it runs off the menu-event thread.
            if let Some(channel) = app.try_state::<EngineChannel>() {
                let sender = channel.0.clone();
                std::thread::spawn(move || {
                    let map = PricingMap::refresh(&PricingOptions {
                        offline: false,
                        cache_dir: PricingOptions::default_cache_dir(),
                        overrides: Default::default(),
                    });
                    let _ = sender.send(Msg::Pricing(map));
                    let _ = sender.send(Msg::Refresh);
                });
            }
        }
        "today" => panel::open_at(app, "day"),
        "week" => panel::open_at(app, "week"),
        "data" => open_data_dir(),
        "autostart" => toggle_autostart(app),
        "quit" => app.exit(0),
        _ => {}
    }
}

fn toggle_autostart(app: &AppHandle) {
    let next = !app.autolaunch().is_enabled().unwrap_or(false);
    set_autostart(app, next);
}

/// The one writer for the login-item switch: tray menu and panel command both
/// land here, so the plugin, the menu checkmark and the file can never drift.
/// Marshalled onto the main thread — the checkmark is an AppKit mutation and
/// the async command reaches this from the runtime thread.
pub fn set_autostart(app: &AppHandle, on: bool) {
    let handle = app.clone();
    on_main(app, move || apply_autostart(&handle, on));
}

fn apply_autostart(app: &AppHandle, on: bool) {
    let plugin = app.autolaunch();
    if on {
        plugin.enable().ok();
    } else {
        plugin.disable().ok();
    }
    if let Some(items) = app.try_state::<MenuItems>() {
        items.set_checked(on);
    }
    persist(app, |s| s.autostart = on);
}

/// Applies the persisted switch, and repairs drift if login items changed.
pub fn reconcile_autostart(app: &AppHandle) {
    let want = app.state::<Shared>().settings().autostart;
    let plugin = app.autolaunch();
    if plugin.is_enabled().unwrap_or(false) == want {
        return;
    }
    if want {
        plugin.enable().ok();
    } else {
        plugin.disable().ok();
    }
    if let Some(items) = app.try_state::<MenuItems>() {
        items.set_checked(want);
    }
}

/// Rebuild the context menu in the current UI language. muda items carry
/// their labels from construction, so a language switch means new items;
/// the autostart row moves into the fresh menu before the old one drops.
pub fn apply_lang(app: &AppHandle) {
    let handle = app.clone();
    on_main(app, move || {
        let Ok((menu, autostart)) = build_menu(&handle) else { return };
        if let Some(tray) = handle.tray_by_id(TRAY_ID) {
            let _ = tray.set_menu(Some(menu));
        }
        if let Some(items) = handle.try_state::<MenuItems>() {
            *items.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(autostart);
        }
    });
}

fn persist(app: &AppHandle, edit: impl FnOnce(&mut Settings)) {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else { return };
    edit(&mut settings);
    settings.clone().save().ok();
}

fn open_data_dir() {
    let Some(path) = usage_index::Index::default_path() else { return };
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
    };
    let mut command = match std::env::consts::OS {
        "macos" => {
            let mut c = std::process::Command::new("open");
            c.arg(&dir);
            c
        }
        "windows" => {
            let mut c = std::process::Command::new("explorer");
            c.arg(dir.to_string_lossy().to_string());
            c
        }
        _ => {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(&dir);
            c
        }
    };
    // Spawning is best-effort: a missing file manager must not kill the app.
    command.spawn().ok();
}

/// Exposed for the panel's fallback anchor when no tray rect is available.
pub fn last_rect(app: &AppHandle) -> Option<Rect> {
    app.tray_by_id(TRAY_ID).and_then(|t| t.rect().ok().flatten())
}
