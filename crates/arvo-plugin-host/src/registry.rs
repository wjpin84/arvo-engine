use crate::plugin::plugin_client::PluginClient;
use crate::plugin::{GetManifestRequest, Manifest};
use crate::wasm::WasmHost;
use arvo_core::config::PluginsConfig;
use arvo_core::events::{Event, StatusKind};
use std::path::Path;
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone)]
pub enum PluginStatus {
    Reachable(Manifest),
    Unreachable(String),
}

#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub id: String,
    pub address: Option<String>,
    pub path: Option<String>,
    pub status: PluginStatus,
}

/// Registered != Reachable — see the entry's `status`. A plugin being
/// Unreachable is expected (it's an external process, or a WASM file, this
/// app doesn't control the existence of) and must never block startup or
/// panic. Two execution tiers (gRPC subprocess, WASM in-process), one
/// registry — callers never need to know which tier an entry is on.
pub struct PluginRegistry {
    entries: RwLock<Vec<PluginEntry>>,
    wasm_host: WasmHost,
    events: broadcast::Sender<Event>,
}

fn status_kind(status: &PluginStatus) -> StatusKind {
    match status {
        PluginStatus::Reachable(_) => StatusKind::Reachable,
        PluginStatus::Unreachable(_) => StatusKind::Unreachable,
    }
}

impl PluginRegistry {
    pub async fn connect(config: &PluginsConfig) -> Self {
        let wasm_host = WasmHost::new();
        let (events, _) = broadcast::channel(16);

        // No events published here — there's no prior state for anything to
        // have transitioned *from*. "Everything just started" isn't a
        // meaningful status change. See ticket 02 of the arvo-core map.
        let mut entries = Vec::with_capacity(config.plugin.len());
        for plugin in &config.plugin {
            let status = probe(
                plugin.address.as_deref(),
                plugin.path.as_deref(),
                &wasm_host,
            )
            .await;
            entries.push(PluginEntry {
                id: plugin.id.clone(),
                address: plugin.address.clone(),
                path: plugin.path.clone(),
                status,
            });
        }

        Self {
            entries: RwLock::new(entries),
            wasm_host,
            events,
        }
    }

    /// Events other than the plugin registry's own can subscribe here too —
    /// this is `PluginRegistry`'s bus, not a `PluginStatusChanged`-only one,
    /// even though that's the only producer today.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Re-probes every configured plugin, so one started after the app was
    /// can be picked up without a restart. Publishes a `PluginStatusChanged`
    /// only when a plugin's status actually changed — most refreshes change
    /// nothing, and an event on every probe would make "event" noise the
    /// moment a second plugin exists.
    pub async fn refresh(&self) {
        let known: Vec<PluginEntry> = {
            let entries = self.entries.read().await;
            entries.clone()
        };

        let mut refreshed = Vec::with_capacity(known.len());
        for old in known {
            let new_status =
                probe(old.address.as_deref(), old.path.as_deref(), &self.wasm_host).await;

            if status_kind(&new_status) != status_kind(&old.status) {
                let _ = self.events.send(Event::PluginStatusChanged {
                    id: old.id.clone(),
                    status: status_kind(&new_status),
                });
            }

            refreshed.push(PluginEntry {
                id: old.id,
                address: old.address,
                path: old.path,
                status: new_status,
            });
        }

        *self.entries.write().await = refreshed;
    }

    pub async fn snapshot(&self) -> Vec<PluginEntry> {
        self.entries.read().await.clone()
    }
}

async fn probe(address: Option<&str>, path: Option<&str>, wasm_host: &WasmHost) -> PluginStatus {
    if let Some(address) = address {
        return probe_process(address).await;
    }

    if let Some(path) = path {
        // ponytail: instantiating our stub-sized components is near-instant
        // (no I/O, pure computation), so this runs directly on the async
        // task rather than via spawn_blocking. Revisit if a real plugin's
        // instantiation is ever slow enough to matter — see the map's
        // Not-yet-specified on wasmtime lifecycle.
        return match wasm_host.get_manifest(Path::new(path)) {
            Ok(manifest) => PluginStatus::Reachable(manifest),
            // `{:#}` is anyhow's alternate form: the whole cause chain on one
            // line, not just the outermost message. `Unreachable` holds a
            // String because it is a display value bound for the UI — but it
            // should carry every cause, not only the last one.
            Err(err) => PluginStatus::Unreachable(format!("{err:#}")),
        };
    }

    // Unreachable in practice — config::load validates exactly one of
    // address/path is set — but name the state instead of panicking on it.
    PluginStatus::Unreachable("plugin entry has neither address nor path".into())
}

async fn probe_process(address: &str) -> PluginStatus {
    let mut client = match PluginClient::connect(address.to_string()).await {
        Ok(client) => client,
        Err(err) => return PluginStatus::Unreachable(err.to_string()),
    };

    match client.get_manifest(GetManifestRequest {}).await {
        Ok(response) => PluginStatus::Reachable(response.into_inner()),
        Err(status) => PluginStatus::Unreachable(status.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arvo_core::config::PluginConfigEntry;
    use std::time::Duration;

    async fn wait_until_serving(address: &str) {
        for _ in 0..50 {
            if PluginClient::connect(address.to_string()).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("stub plugin never came up at {address}");
    }

    #[tokio::test]
    async fn reachable_plugin_returns_its_manifest() {
        let addr = "127.0.0.1:50061".parse().unwrap();
        tokio::spawn(arvo_plugin_stub::serve(addr));
        wait_until_serving("http://127.0.0.1:50061").await;

        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "stub".into(),
                address: Some("http://127.0.0.1:50061".into()),
                path: None,
            }],
        };

        let registry = PluginRegistry::connect(&config).await;
        let snapshot = registry.snapshot().await;

        assert_eq!(snapshot.len(), 1);
        match &snapshot[0].status {
            PluginStatus::Reachable(manifest) => assert_eq!(manifest.id, "stub"),
            PluginStatus::Unreachable(reason) => panic!("expected reachable, got: {reason}"),
        }
    }

    #[tokio::test]
    async fn dead_address_is_unreachable_not_an_error() {
        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "nothing-here".into(),
                // ponytail: port picked to almost certainly have nothing
                // listening; a flaky collision would fail loudly, not silently.
                address: Some("http://127.0.0.1:50062".into()),
                path: None,
            }],
        };

        let registry = PluginRegistry::connect(&config).await;
        let snapshot = registry.snapshot().await;

        assert_eq!(snapshot.len(), 1);
        assert!(matches!(snapshot[0].status, PluginStatus::Unreachable(_)));
    }

    fn wasm_stub_path() -> String {
        // Built by plugin-execution-tiers ticket 02; must be built with
        // --target wasm32-unknown-unknown (not cargo-component's default
        // wasm32-wasip1) to get zero WASI imports. Relative to this crate,
        // since that's where `cargo test` runs from.
        "../../target/wasm32-unknown-unknown/debug/arvo_plugin_wasm_stub.wasm".to_string()
    }

    #[tokio::test]
    async fn reachable_wasm_plugin_returns_its_manifest() {
        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "wasm-stub".into(),
                address: None,
                path: Some(wasm_stub_path()),
            }],
        };

        let registry = PluginRegistry::connect(&config).await;
        let snapshot = registry.snapshot().await;

        assert_eq!(snapshot.len(), 1);
        match &snapshot[0].status {
            PluginStatus::Reachable(manifest) => assert_eq!(manifest.id, "wasm-stub"),
            PluginStatus::Unreachable(reason) => panic!(
                "expected reachable (did you `cargo component build --target \
                 wasm32-unknown-unknown` in plugins/wasm-stub first?): {reason}"
            ),
        }
    }

    #[tokio::test]
    async fn missing_wasm_file_is_unreachable_not_an_error() {
        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "ghost".into(),
                address: None,
                path: Some("does/not/exist.wasm".into()),
            }],
        };

        let registry = PluginRegistry::connect(&config).await;
        let snapshot = registry.snapshot().await;

        assert_eq!(snapshot.len(), 1);
        assert!(matches!(snapshot[0].status, PluginStatus::Unreachable(_)));
    }

    #[tokio::test]
    async fn refresh_publishes_only_on_status_change() {
        use tokio::sync::broadcast::error::TryRecvError;

        let addr = "127.0.0.1:50063".parse().unwrap();
        let handle = tokio::spawn(arvo_plugin_stub::serve(addr));
        wait_until_serving("http://127.0.0.1:50063").await;

        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "events-stub".into(),
                address: Some("http://127.0.0.1:50063".into()),
                path: None,
            }],
        };

        let registry = PluginRegistry::connect(&config).await;
        let mut events = registry.subscribe();

        // connect() itself must not have published anything.
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // No status change yet -> refresh publishes nothing.
        registry.refresh().await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // Kill the plugin (abort its in-process task, same effect as killing
        // a real process for this test) and refresh -> exactly one event.
        handle.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
        registry.refresh().await;

        match events.try_recv() {
            Ok(Event::PluginStatusChanged { id, status }) => {
                assert_eq!(id, "events-stub");
                assert_eq!(status, StatusKind::Unreachable);
            }
            other => panic!("expected a PluginStatusChanged event, got {other:?}"),
        }

        // Still down, no further change -> refresh publishes nothing new.
        registry.refresh().await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    }
}
