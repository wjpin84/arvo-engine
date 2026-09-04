//! The Tauri desktop host.
//!
//! Owns the window, the async runtime and the wiring between them: it loads
//! config via [`arvo_core`], stands up the registry from
//! [`arvo_plugin_host`], and exposes both to the workbench UI as commands.
//!
//! The scheduler lives here rather than in `arvo-core` because its only
//! implementation is bound to `tauri::async_runtime` — see [`scheduler`].

pub mod commands;
pub mod scheduler;

use arvo_core::{config, notifications};
use arvo_plugin_host::registry;
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;
use tauri_plugin_notification::NotificationExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config_path = app.path().app_config_dir()?.join("plugins.toml");
            let plugins_config = match config::load(&config_path) {
                Ok(config) => config,
                Err(err) => {
                    // ponytail: a malformed plugins.toml has no UI surface yet —
                    // the plugin list only shows what the registry probed. Log
                    // loudly and continue with zero plugins rather than blocking
                    // startup on a hand-edited file.
                    eprintln!("plugin config error, continuing with no plugins: {err}");
                    config::PluginsConfig::default()
                }
            };
            // Arc, not a bare PluginRegistry: the scheduled-refresh task
            // below runs outside Tauri's command-invocation system (which
            // only hands out State inside command handlers), so it needs
            // its own cheap handle to the same registry.
            let plugin_registry = Arc::new(tauri::async_runtime::block_on(
                registry::PluginRegistry::connect(&plugins_config),
            ));

            // Subscribe before the registry moves into app.manage().
            let mut events = plugin_registry.subscribe();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let notification = notifications::event_to_notification(&event);
                            if let Err(err) = app_handle
                                .notification()
                                .builder()
                                .title(notification.title)
                                .body(notification.body)
                                .show()
                            {
                                eprintln!("failed to show notification: {err}");
                            }
                        }
                        // A slow subscriber missed some events — keep going,
                        // don't die over a lag on a low-volume stream.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            // First real scheduled job: automatic registry refresh. 30s is
            // a starting default, not user-configurable yet.
            let scheduled_registry = plugin_registry.clone();
            scheduler::every(Duration::from_secs(30), move || {
                let registry = scheduled_registry.clone();
                async move { registry.refresh().await }
            });

            app.manage(plugins_config);
            app.manage(plugin_registry);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_plugins,
            commands::refresh_plugins
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
