//! The Tauri desktop host.
//!
//! Owns the window, the async runtime and the wiring between them: it loads
//! config via [`arvo_core`], stands up the registry from
//! [`arvo_plugin_host`], and exposes both to the workbench UI as commands.
//!
//! The scheduler lives here rather than in `arvo-core` because its only
//! implementation is bound to `tauri::async_runtime` — see [`scheduler`].

pub mod commands;
pub mod events;
pub mod feed;
pub mod portfolio;
pub mod research;
pub mod scheduler;
pub mod session;
pub mod stream;

use arvo_core::config;
use arvo_plugin_host::registry;
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;

/// Sets up diagnostics, writing to both stderr and a file under Tauri's app
/// log directory.
///
/// The file sink is the point. `main.rs` sets `windows_subsystem = "windows"`
/// for release builds, so a released app has no console and anything written
/// to stderr goes nowhere — which is precisely the situation where a user
/// hits a malformed `plugins.toml` and needs to know why it was ignored.
///
/// Truncates on each launch rather than rolling: the interesting log is the
/// one for the session that just misbehaved, and this keeps the file from
/// growing without bound. Swap in `tracing-appender` if retention across
/// sessions is ever wanted.
fn init_tracing(log_dir: std::path::PathBuf) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    // RUST_LOG wins if set; otherwise info for our own crates, warn elsewhere.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,arvo_runtime=info,arvo_core=info,arvo_plugin_host=info")
    });

    let file_layer = std::fs::create_dir_all(&log_dir)
        .and_then(|()| std::fs::File::create(log_dir.join("arvo.log")))
        .map(|file| fmt::layer().with_ansi(false).with_writer(file))
        .ok();

    // If the log file cannot be opened, carry on with stderr only — losing
    // diagnostics is not a reason to refuse to start.
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer);

    // `set_global_default`, deliberately, NOT `SubscriberInitExt::init()`.
    //
    // `init()` additionally installs `tracing-log`'s `LogTracer` as the `log`
    // crate's global logger. NautilusTrader's kernel then cannot install its
    // own, and rather than shrugging it fails the whole engine construction
    // with "A non-Nautilus logger is already registered" — so every backtest
    // died at startup while the tests, which install no subscriber, all
    // passed. This cost the `log` bridge, which only mattered for
    // dependencies that log through `log` rather than `tracing`; Nautilus is
    // the significant one and `arvo-nautilus` silences it anyway.
    if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("could not install the tracing subscriber: {err}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Before anything that can fail, so config errors are captured.
            init_tracing(app.path().app_log_dir()?);

            let config_path = app.path().app_config_dir()?.join("plugins.toml");
            let plugins_config = match config::load(&config_path) {
                Ok(config) => config,
                Err(err) => {
                    // ponytail: a malformed plugins.toml has no UI surface yet —
                    // the plugin list only shows what the registry probed. Log
                    // loudly and continue with zero plugins rather than blocking
                    // startup on a hand-edited file.
                    tracing::error!(
                        error = %err,
                        path = %config_path.display(),
                        "plugin config unreadable, continuing with no plugins"
                    );
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

            // Subscribe before the registry moves into app.manage(). The window
            // hears about every transition through the same seam that raises the
            // OS notification — see `events`.
            let mut transitions = plugin_registry.subscribe();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match transitions.recv().await {
                        Ok(event) => events::emit(&app_handle, events::plugin(&event)),
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

            // Daily bars live under the app data dir so a user can drop CSV
            // exports in without touching the install. The directory is
            // created up front, because an empty folder that exists is a
            // clearer instruction than a path in an error message.
            let data_dir = app.path().app_data_dir()?.join(research::DATA_SUBDIR);
            if let Err(err) = std::fs::create_dir_all(&data_dir) {
                tracing::warn!(
                    error = %err,
                    path = %data_dir.display(),
                    "could not create the research data directory; studies will find no instruments"
                );
            }
            // Findings live beside the data they were produced from, under the
            // app data dir, so a user can back up or inspect both together.
            let evidence_dir = app.path().app_data_dir()?.join(research::EVIDENCE_SUBDIR);
            app.manage(research::ResearchService::new(
                data_dir.clone(),
                evidence_dir,
            ));

            // Holdings you export yourself. Created up front so the empty
            // folder is the instruction, rather than a path in an error.
            let portfolio_dir = app.path().app_data_dir()?.join(portfolio::PORTFOLIO_SUBDIR);
            if let Err(err) = std::fs::create_dir_all(&portfolio_dir) {
                tracing::warn!(
                    error = %err,
                    path = %portfolio_dir.display(),
                    "could not create the portfolios directory"
                );
            }
            let snapshot_dir = app.path().app_data_dir()?.join(portfolio::SNAPSHOT_SUBDIR);
            app.manage(portfolio::PortfolioService::new(
                portfolio_dir,
                data_dir,
                snapshot_dir,
            ));

            // The live price stream. Started here and held for the life of
            // the app: it is a socket, not a request, and the thing that
            // decides what it carries is the watchlist command.
            app.manage(stream::start(app.handle().clone()));

            app.manage(plugins_config);
            app.manage(plugin_registry);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_plugins,
            commands::refresh_plugins,
            commands::load_session,
            commands::save_session,
            research::list_instruments,
            research::run_study,
            research::run_walk_forward,
            research::export_trades,
            research::list_strategies,
            research::feed_connected,
            research::connect_feed,
            research::disconnect_feed,
            research::fetch_bars,
            research::search_instruments,
            research::watchlist,
            research::compare_records,
            research::run_panel,
            research::list_history,
            research::open_record,
            research::replay_record,
            research::run_book,
            portfolio::list_portfolios
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    /// Guards the exact failure that shipped: `init_tracing` used to install
    /// `tracing-log`'s bridge as the `log` crate's global logger, and
    /// NautilusTrader's kernel then refuses to build an engine at all, with
    /// "A non-Nautilus logger is already registered". Every backtest in the
    /// desktop app failed while every test passed, because tests install no
    /// subscriber.
    ///
    /// The invariant is one-directional and belongs here rather than in
    /// `arvo-nautilus`: whoever sets up diagnostics must leave the `log`
    /// global free for the engine to claim.
    #[test]
    fn tracing_setup_leaves_the_log_global_free_for_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::init_tracing(dir.path().to_path_buf());

        struct Discard;
        impl log::Log for Discard {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                false
            }
            fn log(&self, _: &log::Record<'_>) {}
            fn flush(&self) {}
        }

        // Succeeds only while nothing else holds the `log` global — which is
        // precisely what Nautilus needs to be true when it starts up.
        assert!(
            log::set_boxed_logger(Box::new(Discard)).is_ok(),
            "init_tracing claimed the `log` global; Nautilus cannot install its logger and will \
             refuse to build an engine"
        );
    }
}
