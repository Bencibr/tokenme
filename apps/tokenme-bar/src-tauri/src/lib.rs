mod commands;
mod bubble;
mod lang;
mod logging;
mod engine;
mod notify;
mod scan_log;
mod panel;
mod servers;
mod settings;
mod snapshot;
mod tray;
mod updater;
#[cfg(target_os = "windows")]
mod windows_surface;

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
        .manage(servers::state())
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
            commands::set_show_empty_tools,
            commands::set_report_scope,
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
            commands::set_tool_muted,
            commands::set_bubble_enabled,
            commands::set_bubble_skin,
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

            let handle = app.handle().clone();
            tray::build(&handle)?;
            panel::configure(&handle);
            // The panel is created hidden (visible: false in tauri.conf.json)
            // and is only ever revealed through panel::show / panel::toggle, so
            // setup has nothing to hide: a hide here would bypass
            // request_visibility and leave the native window and panel_hidden()
            // telling different stories.
            bubble::configure(&handle)?;
            engine_start(handle.clone(), rx);
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
        .build(tauri::generate_context!())
        .expect("tokenme failed to start")
        .run(|_app, _event| {})
}

fn engine_start(app: tauri::AppHandle, rx: Receiver<engine::Msg>) {
    let sender = app.state::<EngineChannel>().0.clone();
    engine::start(app, rx, sender);
}
