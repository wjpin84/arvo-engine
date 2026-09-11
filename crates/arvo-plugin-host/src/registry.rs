use crate::plugin::plugin_client::PluginClient;
use crate::plugin::{GetManifestRequest, Manifest};
use arvo_core::config::PluginsConfig;
use arvo_core::events::{Event, StatusKind};
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone)]
pub enum PluginStatus {
    Reachable(Manifest),
    Unreachable(String),
}

#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub id: String,
    pub address: String,
    pub status: PluginStatus,
}

/// Registered != Reachable — see the entry's `status`. A plugin being
/// Unreachable is expected (it's an external process this app doesn't control
/// the existence of) and must never block startup or panic.
///
/// # Why there is only one tier
///
/// There was an in-process WASM tier beside this one. It was removed rather
/// than extended, because its sandbox policy and its purpose had come into
/// direct conflict: components were instantiated with an empty `Linker` and no
/// WASI context, so a component importing *anything* failed to instantiate.
/// That is a sound way to run untrusted arithmetic and a structurally
/// impossible way to run a data source, which needs a socket and a credential.
/// A plugin worth having is one that can fetch bars; that is a process, and a
/// process is this tier.
pub struct PluginRegistry {
    entries: RwLock<Vec<PluginEntry>>,
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
        let (events, _) = broadcast::channel(16);

        // No events published here — there's no prior state for anything to
        // have transitioned *from*. "Everything just started" isn't a
        // meaningful status change. See ticket 02 of the arvo-core map.
        let mut entries = Vec::with_capacity(config.plugin.len());
        for plugin in &config.plugin {
            let status = probe(&plugin.address).await;
            entries.push(PluginEntry {
                id: plugin.id.clone(),
                address: plugin.address.clone(),
                status,
            });
        }

        Self {
            entries: RwLock::new(entries),
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
            let new_status = probe(&old.address).await;

            if status_kind(&new_status) != status_kind(&old.status) {
                let _ = self.events.send(Event::PluginStatusChanged {
                    id: old.id.clone(),
                    status: status_kind(&new_status),
                });
            }

            refreshed.push(PluginEntry {
                id: old.id,
                address: old.address,
                status: new_status,
            });
        }

        *self.entries.write().await = refreshed;
    }

    pub async fn snapshot(&self) -> Vec<PluginEntry> {
        self.entries.read().await.clone()
    }
}

async fn probe(address: &str) -> PluginStatus {
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
                address: "http://127.0.0.1:50061".into(),
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
                address: "http://127.0.0.1:50062".into(),
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
                address: "http://127.0.0.1:50063".into(),
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
