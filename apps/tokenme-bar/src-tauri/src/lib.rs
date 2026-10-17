mod commands;
mod diagnostics;
mod bubble;
mod lang;
mod logging;
mod engine;
mod notify;
mod scan_log;
mod panel;
mod servers;
mod settings;
mod pet_notice;
mod claude_otel_config;
mod claude_otel_ttft;
mod ttft;
mod active_speed;
mod snapshot;
mod tray;
mod updater;
#[cfg(target_os = "windows")]
mod windows_surface;
#[cfg(target_os = "windows")]
mod windows_tray;

pub use settings::{BubbleSkin, Settings, TrayMode};

use std::sync::mpsc::Receiver;

use tauri::Manager;

use crate::engine::EngineChannel;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Start the cold-start clock before anything else can read it.
    panel::mark_launch();
    let (tx, rx) = std::sync::mpsc::channel::<engine::Msg>();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            panel::show(app, None);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_positioner::init())
        .manage(EngineChannel(tx))
        .manage(engine::Shared::new(Settings::load()))
        .manage(commands::SyncControl::default())
        .manage(diagnostics::DiagnosticsState::default())
        .manage(servers::state())
        .manage(pet_notice::NoticeState::default())
        .invoke_handler(tauri::generate_handler![
            commands::get_report,
            commands::get_tray_state,
            commands::set_ui_lang,
            commands::panel_page_signal,
            commands::set_tray_mode,
            commands::get_quota_order,
            commands::set_quota_order,
            commands::refresh_pricing,
            commands::tool_icons,
            commands::get_panel_settings,
            commands::set_autostart,
            commands::set_refresh_secs,
            commands::checkin_now,
            commands::set_theme,
            commands::set_show_money,
            commands::set_quota_display_mode,
            commands::set_show_cycle_tokens,
            commands::get_ttft_snapshot,
            commands::set_ttft_enabled,
            active_speed::run_active_speed_test,
            commands::set_hide_unused_tools,
            commands::set_hide_unused_quota,
            commands::set_hide_unused_estimation,
            commands::set_report_scope,
            commands::sync_devices,
            commands::get_mcp_client_config,
            diagnostics::set_diagnostics_enabled,
            diagnostics::get_diagnostics_status,
            diagnostics::revoke_diagnostics_sessions,
            diagnostics::diagnostics_session,
            servers::commands::get_servers,
            servers::commands::server_public_key,
            servers::commands::server_probe,
            servers::commands::server_install,
            servers::commands::server_abort_session,
            servers::commands::server_sync_now,
            servers::commands::server_update,
            servers::commands::server_remove,
            updater::check_update,
            updater::download_update,
            updater::install_update,
            updater::get_auto_update_check,
            updater::set_auto_update_check,
            commands::set_host_exit_pause,
            commands::set_auto_checkin,
            commands::set_quota_polling,
            commands::set_tool_polling,
            commands::probe_tools,
            commands::set_notify_tiers,
            commands::set_plan_expiry_notify_days,
            commands::set_tool_muted,
            commands::set_bubble_enabled,
            commands::set_bubble_skin,
            commands::set_pet_preferences,
            pet_notice::pet_activity,
            pet_notice::get_pet_notice,
            pet_notice::show_pet_notice,
            pet_notice::preview_pet_notice,
            pet_notice::pet_notice_ready,
            pet_notice::respond_pet_notice,
            pet_notice::reposition_pet_notice,
            notify::notify_status,
            notify::notify_enable,
            commands::begin_bubble_drag,
            commands::show_panel,
            commands::hide_panel,
            commands::panel_keyboard,
            commands::quit_app,
            commands::open_external,
            commands::open_log_dir
        ]);

    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());

    builder
        .setup(move |app| {
            // Menu-bar only: no Dock tile, no app-switcher entry.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            logging::init();
            notify::init(app.handle());
            // The check-in ledger rides the panel's own log dir; the probes
            // themselves call through this hook, so usage-quota stays
            // host-agnostic.
            usage_quota::set_checkin_log(std::sync::Arc::new(|msg| crate::logging::checkin(msg)));

            let handle = app.handle().clone();
            diagnostics::DiagnosticsState::restore_on_launch(&handle);
            let ttft_dir = dirs::data_local_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("tokenme").join("ttft.json");
            let ttft_state = ttft::TtftState::new(handle.clone(), ttft_dir, handle.state::<engine::Shared>().settings().ttft_enabled);
            ttft_state.start();
            app.manage(ttft_state);
            tray::build(&handle)?;
            panel::configure(&handle);
            // The panel is created hidden (visible: false in tauri.conf.json)
            // and is only ever revealed through panel::show / panel::toggle, so
            // setup has nothing to hide: a hide here would bypass
            // request_visibility and leave the native window and panel_hidden()
            // telling different stories.
            bubble::configure(&handle)?;
            engine_start(handle.clone(), rx);
            // Restore a user-enabled sync role before the first WebView
            // snapshot; an explicit Stop remains persisted as disabled.
            commands::restore_sync_on_launch(&handle);
            servers::hub::start(
                handle.clone(),
                handle.state::<std::sync::Arc<servers::ServersState>>().inner().clone(),
            );
            tray::reconcile_autostart(&handle);
            // Screenshots and manual QA need the panel up without a tray click.
            if std::env::var_os("TOKENME_SHOW_PANEL").is_some() {
                panel::show(&handle, None);
            }
            // A one-shot banner for manual QA of the permission flow
            // (`TOKENME_TEST_NOTIFY=1`), posted through the real gate.
            if std::env::var_os("TOKENME_TEST_NOTIFY").is_some() {
                notify::send_test();
            }
            Ok(())
        })
        .on_window_event(panel::on_window_event)
        .on_page_load(|webview, payload| {
            // `TOKENME_PIN_PAGE=tools` deep-links the real panel the way `?page=`
            // does in a browser — the config window loads index.html bare, and a
            // non-activating panel takes no synthetic clicks (same QA family as
            // TOKENME_SHOW_PANEL). The guard keeps this navigation's own second
            // load from looping.
            if payload.event() != tauri::webview::PageLoadEvent::Finished || webview.label() != "main" {
                return;
            }
            if let Some(p) = std::env::var_os("TOKENME_PIN_PAGE") {
                let _ = webview.eval(&format!(
                    "if (!location.search) location.replace('index.html?page={}')",
                    p.to_string_lossy()
                ));
            }
        })
        .build(tauri::generate_context!())
        .expect("tokenme failed to start")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit) {
                if let Some(state) = app.try_state::<diagnostics::DiagnosticsState>() {
                    state.revoke_all();
                    state.stop_local_http();
                }
            }
        })
}

fn engine_start(app: tauri::AppHandle, rx: Receiver<engine::Msg>) {
    let sender = app.state::<EngineChannel>().0.clone();
    engine::start(app, rx, sender);
}
