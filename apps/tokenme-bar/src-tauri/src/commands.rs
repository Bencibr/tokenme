//! The complete JS-facing surface: five commands, none of which computes a number.

use std::collections::BTreeMap;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use usage_core::pricing::PricingOptions;
use usage_core::{PricingMap, PricingMeta, Report};

use crate::engine::{EngineChannel, Msg, Shared};
use crate::settings::{Theme, TrayMode};
use crate::tray;

/// Mirrors the tray's live state so the panel can show what the bar shows.
#[derive(Debug, Clone, Serialize)]
pub struct TrayState {
    pub label: String,
    pub tooltip: String,
    pub mode: TrayMode,
    pub modes: Vec<TrayMode>,
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

#[tauri::command]
pub async fn get_tray_state(app: AppHandle) -> Result<TrayState, String> {
    tray::tray_state(&app).ok_or_else(|| "indexing".to_string())
}

#[tauri::command]
pub async fn set_tray_mode(app: AppHandle, mode: TrayMode) -> Result<(), String> {
    let report = app.state::<Shared>().report();
    {
        let shared = app.state::<Shared>();
        let Ok(mut settings) = shared.settings.lock() else {
            return Err("settings busy".into());
        };
        settings.tray_mode = mode;
        settings.clone().save().map_err(|e| e.to_string())?;
    }
    if let Some(report) = report {
        tray::refresh(&app, &report, mode);
    }
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
    pub version: String,
}

#[tauri::command]
pub async fn get_panel_settings(app: AppHandle) -> Result<PanelSettings, String> {
    let settings = app.state::<Shared>().settings();
    Ok(PanelSettings {
        autostart: settings.autostart,
        refresh_secs: settings.refresh_secs,
        theme: settings.theme,
        version: env!("CARGO_PKG_VERSION").to_string(),
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
    settings.clone().save().map_err(|e| e.to_string())
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
