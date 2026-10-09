//! The complete JS-facing surface; none of these commands computes a number.

use std::collections::BTreeMap;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use usage_core::pricing::PricingOptions;
use usage_core::{origin_ok, MachineScope, PricingMap, PricingMeta, Report};

use crate::engine::{EngineChannel, Msg, Shared};
use crate::{bubble, panel};
use crate::settings::{NotifyTier, Theme, TrayMode};
use crate::tray;

/// Terminates the tray application, rather than merely hiding its panel.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Mirrors the tray's live state so the panel can show what the bar shows.
#[derive(Debug, Clone, Serialize)]
pub struct TrayState {
    pub label: String,
    pub tooltip: String,
    pub mode: TrayMode,
}

/// Returns the last report; `force` asks the engine for an immediate re-index.
/// The re-index never happens on this thread — the fresh report arrives through
/// the `report-updated` event.
#[tauri::command]
pub async fn get_report(app: AppHandle, force: bool) -> Result<Report, String> {
    if force {
        if let Some(channel) = app.try_state::<EngineChannel>() {
            let _ = channel.0.send(Msg::Refresh);
        }
    }
    app.state::<Shared>()
        .report()
        .ok_or_else(|| "indexing".to_string())
}

/// The webview's detected UI language (system locale; the `?lang=` override
/// wins for QA). The Rust chrome — tray menu, tooltip, updater copy — follows
/// it; the menu rebuilds here because muda items carry their labels from
/// construction.
#[tauri::command]
pub async fn set_ui_lang(app: AppHandle, lang: String) -> Result<(), String> {
    let Some(parsed) = crate::lang::parse(&lang) else {
        return Err(format!("unknown lang: {lang}"));
    };
    if crate::lang::get() != parsed {
        crate::lang::set(parsed);
        tray::apply_lang(&app);
    }
    Ok(())
}

#[tauri::command]
pub async fn get_tray_state(app: AppHandle) -> Result<TrayState, String> {
    tray::tray_state(&app).ok_or_else(|| "indexing".to_string())
}

#[tauri::command]
pub async fn set_tray_mode(app: AppHandle, mode: TrayMode) -> Result<(), String> {
    let report = app.state::<Shared>().report();
    if let Err(e) = app.state::<Shared>().set_tray_mode(mode) {
        return Err(e);
    }
    // The menubar repaints before anything touches disk: the switch must feel
    // instant, and a slow save must never sit between the click and the pixel.
    if let Some(report) = report {
        tray::refresh(&app, &report, mode);
    }
    // Persist after the repaint: disk IO never sits between the click and the
    // menubar, and a failed write still leaves this session working.
    let settings = app.state::<Shared>().settings();
    settings.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// The saved drag order of the quota section, as the panel persists it.
#[tauri::command]
pub async fn get_quota_order(app: AppHandle) -> Result<QuotaOrder, String> {
    let shared = app.state::<Shared>();
    let Ok(settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    Ok(QuotaOrder { tools: settings.quota_tools.clone(), rows: settings.quota_rows.clone() })
}

/// Persist a new drag order. The panel applies the reorder locally first, so
/// this only has to survive the next launch; a failure costs the arrangement,
/// never a report.
#[tauri::command]
pub async fn set_quota_order(app: AppHandle, order: QuotaOrder) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_tools = order.tools;
    settings.quota_rows = order.rows;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The drag order of the quota section: tool ids for the groups,
/// `<tool>/<row id>` for the bars inside them.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct QuotaOrder {
    pub tools: Vec<String>,
    pub rows: Vec<String>,
}

/// Drops the price cache and re-downloads models.dev off the main thread, then
/// hands the new table to the engine so it re-summarizes without re-reading logs.
#[tauri::command]
pub async fn refresh_pricing(app: AppHandle) -> Result<PricingMeta, String> {
    let sender = app
        .try_state::<EngineChannel>()
        .map(|channel| channel.0.clone())
        .ok_or_else(|| "engine unavailable".to_string())?;
    let meta = tauri::async_runtime::spawn_blocking(move || {
        let map = PricingMap::refresh(&PricingOptions {
            offline: false,
            cache_dir: PricingOptions::default_cache_dir(),
            overrides: Default::default(),
        });
        let meta = map.meta().clone();
        let _ = sender.send(Msg::Pricing(map));
        meta
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(meta)
}

/// The macOS application icon of each tool that ships one, as a `data:` URL.
///
/// Filesystem reads only, and only ever decoration: an unreadable icon costs the
/// panel a coloured monogram, never a figure.
#[tauri::command]
pub async fn tool_icons() -> Result<BTreeMap<String, String>, String> {
    Ok(usage_core::icons::icon_data_urls())
}

/// What the settings sheet shows: the switches plus the build version.
#[derive(Debug, Clone, Serialize)]
pub struct PanelSettings {
    pub autostart: bool,
    pub refresh_secs: u64,
    pub theme: Theme,
    pub show_money: bool,
    pub show_empty_tools: bool,
    pub bubble_enabled: bool,
    pub host_exit_pause: bool,
    pub quota_polling: bool,
    pub quota_probes_off: Vec<String>,
    pub notify_tiers: NotifyTier,
    pub notify_muted: Vec<String>,
    pub auto_update_check: bool,
    pub version: String,
    /// The same number `main.rs` logs at startup ("build N starting"), so the
    /// 关于 page and the log agree about which artifact is actually running.
    pub build: &'static str,
}

/// One row of the settings sheet's per-tool polling page, straight from the
/// probe registry — the page has no list of its own to fall out of step with.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeRow {
    pub id: &'static str,
    pub host_gated: bool,
    pub answered_here: bool,
    /// What the composed gate answers right now: master switch ∧ not excluded.
    pub polling: bool,
}

#[tauri::command]
pub async fn probe_tools(app: AppHandle) -> Result<Vec<ProbeRow>, String> {
    let settings = app.state::<Shared>().settings();
    Ok(usage_quota::inventory()
        .into_iter()
        .map(|p| ProbeRow {
            polling: settings.probe_allowed(p.tool),
            id: p.tool,
            host_gated: p.host_gated,
            answered_here: p.answered_here,
        })
        .collect())
}

/// Open a release page / mailto link in the user's browser. The scheme
/// whitelist is the whole of it: a panel that reads local files has no business
/// being told to launch arbitrary URLs.
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    crate::logging::info(&format!("open_external: {url}"));
    let allowed = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("mailto:");
    if !allowed {
        return Err(format!("unsupported url scheme: {url}"));
    }
    // The mail draft takes a beat to appear; a second click on the same
    // button while it does reads as "nothing happened" and opens a second
    // draft. Same URL within 1.5s is the same intent — swallow it.
    static LAST: std::sync::OnceLock<std::sync::Mutex<Option<(String, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    let last = LAST.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = last.lock() {
        let duplicate = slot
            .as_ref()
            .is_some_and(|(prev, at)| *prev == url && at.elapsed() < std::time::Duration::from_millis(1500));
        *slot = Some((url.clone(), std::time::Instant::now()));
        if duplicate {
            return Ok(());
        }
    }
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = {
        // `cmd` is a console binary, so without this every panel link (发邮件 /
        // Releases / 日志) opens through a visible console window that the default
        // terminal paints for a moment and steals focus with.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(&url).spawn();
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_panel_settings(app: AppHandle) -> Result<PanelSettings, String> {
    let settings = app.state::<Shared>().settings();
    Ok(PanelSettings {
        autostart: settings.autostart,
        refresh_secs: settings.refresh_secs,
        theme: settings.theme,
        show_money: settings.show_money,
        show_empty_tools: settings.show_empty_tools,
        bubble_enabled: settings.bubble_enabled,
        host_exit_pause: settings.host_exit_pause,
        quota_polling: settings.quota_polling,
        quota_probes_off: settings.quota_probes_off.clone(),
        notify_tiers: settings.notify_tiers,
        notify_muted: settings.notify_muted.clone(),
        auto_update_check: settings.auto_update_check,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: env!("TOKENME_BUILD_ID"),
    })
}

#[tauri::command]
pub async fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    tray::set_autostart(&app, on);
    Ok(())
}

/// Purely a webview concern — the panel applies it as `data-theme` itself;
/// persisting it here is what makes the choice survive a relaunch.
#[tauri::command]
pub async fn set_theme(app: AppHandle, theme: Theme) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.theme = theme;
    settings.clone().save().map_err(|e| e.to_string())?;
    // the native backing must follow, or the window strip under the sheet
    // flashes the old theme's colour for the lifetime of the panel
    #[cfg(target_os = "macos")]
    crate::panel::apply_window_background(&app, Some(theme));
    Ok(())
}

/// The page's two cold-start milestones, reported by name because they answer
/// different questions: `boot` is when the bundle executed (index.html's shell is
/// on screen), `content` is when it has figures — and only the second one retires
/// the native boot note. See `panel::mark_page_boot` / `panel::mark_content_ready`
/// for why neither may gate the window.
#[tauri::command]
pub fn panel_page_signal(app: tauri::AppHandle, stage: String) {
    match stage.as_str() {
        "boot" => crate::panel::mark_page_boot(),
        "content" => crate::panel::mark_content_ready(&app),
        other => crate::logging::error(&format!("panel: unknown page signal {other:?}")),
    }
}

/// Reveal the diagnostic log directory in the OS file manager.
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let dir = crate::logging::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(&dir).spawn();
    spawned.map(|_| ()).map_err(|e| e.to_string())
}

/// Whether the tools page also lists tools with zero sessions this period.
/// Off keeps the page to what actually ran; a quiet tool reappears the day it
/// bills again. Same shape as the other panel switches: persist, then let the
/// webview re-render from its own state.
#[tauri::command]
pub async fn set_show_empty_tools(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.show_empty_tools = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The host-exit pause: stop probing a tool's quota once its application has
/// exited; the last known answer stays on screen until it runs again.
#[tauri::command]
pub async fn set_host_exit_pause(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".to_string());
    };
    settings.host_exit_pause = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// The master switch: off, no vendor request leaves on the next pass. It is
/// deliberately *not* wired to the manual refresh — that path clears the quota
/// cache, and a switch whose promise is "the numbers stay where they were" must
/// not be the thing that erases them. The pass cache is 60 s at most, so the
/// stop lands within one cadence tick; the label appears at once because the
/// frontend reads it from settings, not from the report.
#[tauri::command]
pub async fn set_quota_polling(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_polling = on;
    settings.clone().save().map_err(|e| e.to_string())
}

/// One tool's own switch — the only lever for the probes with no host mapping.
/// The list is stored as exclusions (an allow-list would have to be rewritten
/// every time the registry gains a probe).
#[tauri::command]
pub async fn set_tool_polling(app: AppHandle, tool: String, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.quota_probes_off.retain(|t| *t != tool);
    if !on {
        settings.quota_probes_off.push(tool);
        settings.quota_probes_off.sort();
    }
    settings.clone().save().map_err(|e| e.to_string())
}

/// Which banner lines fire. Delivery policy only: the tier machine keeps
/// recording where each window stands, so turning this back on does not
/// retro-fire a line crossed while it was off.
#[tauri::command]
pub async fn set_notify_tiers(app: AppHandle, tiers: NotifyTier) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.notify_tiers = tiers;
    settings.clone().save().map_err(|e| e.to_string())
}

/// One tool's banner mute, same exclusion-list shape as the polling switches.
#[tauri::command]
pub async fn set_tool_muted(app: AppHandle, tool: String, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.notify_muted.retain(|t| *t != tool);
    if on {
        settings.notify_muted.push(tool);
        settings.notify_muted.sort();
    }
    settings.clone().save().map_err(|e| e.to_string())
}

/// Dollar figures render at all — the switch the settings sheet flips.
#[tauri::command]
pub async fn set_show_money(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.show_money = on;
    settings.clone().save().map_err(|e| e.to_string())
}


/// Windows-only edge bubble. The command remains available on every target so
/// the frontend bridge stays platform-neutral; non-Windows is a no-op.
#[tauri::command]
pub async fn set_bubble_enabled(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        settings.bubble_enabled = on;
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    bubble::set_enabled(&app, on);
    Ok(())
}

/// Called by the Windows bubble when it is clicked.
#[tauri::command]
pub fn show_panel(app: AppHandle) {
    panel::show(&app, None);
}

/// Dismisses the panel while keeping the native and Rust visibility state in
/// sync, so the next tray click opens it again.
#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    panel::hide(&app);
}

/// While a text field holds focus the frontend asks for a keyboard session —
/// the non-activating tray panel otherwise never owns the keys on Windows.
#[tauri::command]
pub fn panel_keyboard(app: AppHandle, on: bool) {
    panel::set_keyboard_mode(&app, on);
}

/// Hands the press to the Rust-side drag loop. `window.startDragging()` cannot
/// move this non-activating window (see `bubble::begin_drag`); non-Windows is a
/// no-op so the bridge stays platform-neutral.
#[tauri::command]
pub fn begin_bubble_drag(app: AppHandle) {
    bubble::begin_drag(&app);
}

/// The check-in button of a checkin-capable tool (Trae CN / Qoder): one
/// forced claim right now — the automatic pass may already have tried and
/// failed today; a manual click is the user asking again. Returns a
/// user-facing message.
#[tauri::command]
pub async fn checkin_now(tool: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || match tool.as_str() {
        "trae_cn" => usage_quota::trae_manual_checkin(),
        "qoder" => usage_quota::qoder_manual_checkin(),
        other => Err(format!("{other} 没有签到活动")),
    })
    .await
    .map_err(|e| format!("签到任务失败: {e}"))?
}

/// Persist the new fallback cadence, then wake the engine so the next wait
/// uses it instead of the old value's remaining timeout.
#[tauri::command]
pub async fn set_refresh_secs(app: AppHandle, secs: u64) -> Result<(), String> {
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        settings.refresh_secs = secs.clamp(10, 3600);
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    if let Some(channel) = app.try_state::<EngineChannel>() {
        let _ = channel.0.send(Msg::Refresh);
    }
    Ok(())
}

/// Switches the machine scope the panel folds with. The command returns as
/// soon as the engine has the message — the new report arrives through the
/// ordinary `report-updated` event, so the UI never blocks on a refold.
#[tauri::command]
pub fn set_report_scope(app: AppHandle, scope: MachineScope) -> Result<(), String> {
    // The scope normally comes from a menu built out of the report itself, but
    // this is a boundary: an origin name that `origin_of` cannot parse back
    // would silently select nothing, so reject it loudly instead.
    if let MachineScope::Origin { name } = &scope {
        if !origin_ok(name) {
            return Err(format!(
                "origin {name:?} contains ':', '/' or '\\' or a control character"
            ));
        }
    }
    app.state::<Shared>().set_scope(scope.clone())?;
    if let Some(channel) = app.try_state::<EngineChannel>() {
        let _ = channel.0.send(Msg::Scope(scope));
    }
    Ok(())
}
