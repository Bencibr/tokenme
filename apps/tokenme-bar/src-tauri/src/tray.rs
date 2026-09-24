//! Menu-bar presence: the icon, the live title, and the right-click menu.

use tauri::menu::{CheckMenuItem, MenuBuilder, MenuEvent, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Rect};
use tauri_plugin_autostart::ManagerExt;
use usage_core::{QuotaView, Report};

use crate::commands::TrayState;
use crate::engine::{EngineChannel, Msg, Shared};
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

    let title = match mode {
        TrayMode::Quiet => None,
        TrayMode::Cost => Some(match quota {
            Some(q) => format!("{} · {:.0}%", money(day.summary.cost), q.used_percent),
            None => money(day.summary.cost),
        }),
        TrayMode::Tokens => Some(format!("⬡ {}", compact(day.summary.total_tokens))),
    };

    let mut tooltip = format!(
        "tokenme · {}\n{} · {} tokens · {} 次",
        day.label,
        money(day.summary.cost),
        compact(day.summary.total_tokens),
        day.summary.requests
    );
    if day.summary.credits > 0.0 {
        tooltip.push_str(&format!(" · {:.2} credits", day.summary.credits));
    }
    if let Some(q) = quota {
        tooltip.push_str(&format!(
            "\n{} 配额 {:.0}%{}",
            q.tool,
            q.used_percent,
            if q.resets_at_ms > 0 {
                format!(" · 重置 in {}", countdown(q.resets_at_ms, now_ms))
            } else {
                String::new()
            }
        ));
    }
    tooltip.push_str(if report.pricing.stale {
        "\n价格快照已过期，成本为估算"
    } else {
        "\n价格为实时估算，非账单"
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
        modes: TrayMode::ALL.to_vec(),
    })
}

/// Pushes the current numbers onto the status bar. Runs on the engine thread.
pub fn refresh(app: &AppHandle, report: &Report, mode: TrayMode) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let (title, tooltip) = label_for(report, mode);
    // `set_title` is a no-op on Windows, so the tooltip always carries the text.
    let _ = tray.set_title(title.as_deref());
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Keeps the checkable autostart row in sync with the settings file.
struct MenuItems {
    autostart: CheckMenuItem<tauri::Wry>,
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "autostart", "开机自启", true, false, None::<&str>)?;
    let open_data = MenuItem::with_id(app, "data", "打开数据目录", true, None::<&str>)?;
    let week = MenuItem::with_id(app, "week", "本周", true, None::<&str>)?;
    let today = MenuItem::with_id(app, "today", "今日花费", true, None::<&str>)?;
    let refresh_item = MenuItem::with_id(app, "refresh", "刷新", true, None::<&str>)?;

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
    app.manage(MenuItems {
        autostart: autostart.clone(),
    });

    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("tokenme")
        .icon(tauri::include_image!("icons/tray-icon.png"))
        .icon_as_template(true)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event)
        .build(app)?;
    Ok(())
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
        }
        | TrayIconEvent::DoubleClick { rect, .. } => panel::toggle(&app, Some(rect)),
        _ => {}
    }
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "refresh" => {
            if let Some(channel) = app.try_state::<EngineChannel>() {
                let _ = channel.0.send(Msg::Refresh);
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
pub fn set_autostart(app: &AppHandle, on: bool) {
    let plugin = app.autolaunch();
    if on {
        plugin.enable().ok();
    } else {
        plugin.disable().ok();
    }
    if let Some(items) = app.try_state::<MenuItems>() {
        items.autostart.set_checked(on).ok();
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
        items.autostart.set_checked(want).ok();
    }
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
