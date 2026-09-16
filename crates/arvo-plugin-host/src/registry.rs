use crate::plugin::plugin_client::PluginClient;
use crate::plugin::{GetManifestRequest, Manifest};
use crate::source::{no_grants, GrpcSource, Granter, SERVICE};
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
    /// What it serves behind `arvo.source.v1.Source`, read when it was last
    /// reachable and its manifest named the service (ADR-0022 point 2). Empty
    /// for a plugin that serves something else, or is down.
    pub sources: Vec<GrpcSource>,
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
    /// What a plugin-served source may use per call (ADR-0022 point 4).
    granter: Granter,
}

fn status_kind(status: &PluginStatus) -> StatusKind {
    match status {
        PluginStatus::Reachable(_) => StatusKind::Reachable,
        PluginStatus::Unreachable(_) => StatusKind::Unreachable,
    }
}

impl PluginRegistry {
    pub async fn connect(config: &PluginsConfig) -> Self {
        Self::connect_with(config, no_grants()).await
    }

    /// [`Self::connect`], with the granter every plugin-served source asks
    /// for what its calls may use. The app installs one that reads the
    /// keychain; a host with no keychain, or a test, installs none.
    pub async fn connect_with(config: &PluginsConfig, granter: Granter) -> Self {
        let (events, _) = broadcast::channel(16);

        // No events published here — there's no prior state for anything to
        // have transitioned *from*. "Everything just started" isn't a
        // meaningful status change. See ticket 02 of the arvo-core map.
        let mut entries = Vec::with_capacity(config.plugin.len());
        for plugin in &config.plugin {
            // A `command` entry is the supervisor's to start; it registers
            // itself here once it has said where it listens.
            let Some(address) = plugin.address.clone() else { continue };
            let (status, sources) = probe(&address, &granter).await;
            entries.push(PluginEntry { id: plugin.id.clone(), address, status, sources });
        }

        Self {
            entries: RwLock::new(entries),
            events,
            granter,
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
            // A supervised plugin that is down has no address to probe; what
            // the supervisor said about it stands until it is back.
            if old.address.is_empty() {
                refreshed.push(old);
                continue;
            }
            let (new_status, sources) = probe(&old.address, &self.granter).await;

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
                sources,
            });
        }

        *self.entries.write().await = refreshed;
    }

    pub async fn snapshot(&self) -> Vec<PluginEntry> {
        self.entries.read().await.clone()
    }

    /// Every source every reachable plugin serves, in registry order. What
    /// the app appends to its compiled-in sources.
    pub async fn served_sources(&self) -> Vec<GrpcSource> {
        self.entries.read().await.iter().flat_map(|entry| entry.sources.iter().cloned()).collect()
    }

    /// Registers a plugin that has just said where it listens, probing it.
    /// Replaces an entry of that id: a supervised plugin restarted lands on a
    /// new port and is the same plugin.
    pub async fn add(&self, id: &str, address: String) {
        let (status, sources) = probe(&address, &self.granter).await;
        let kind = status_kind(&status);
        let mut entries = self.entries.write().await;
        let before = entries.iter().position(|entry| entry.id == id);
        let changed = before.is_none_or(|at| status_kind(&entries[at].status) != kind);
        let entry = PluginEntry { id: id.to_owned(), address, status, sources };
        match before {
            Some(at) => entries[at] = entry,
            None => entries.push(entry),
        }
        drop(entries);
        if changed {
            let _ = self.events.send(Event::PluginStatusChanged { id: id.to_owned(), status: kind });
        }
    }

    /// Forgets a plugin. One that was reachable going away is a change.
    pub async fn remove(&self, id: &str) {
        let mut entries = self.entries.write().await;
        let Some(at) = entries.iter().position(|entry| entry.id == id) else { return };
        let was = status_kind(&entries[at].status);
        entries.remove(at);
        drop(entries);
        if was == StatusKind::Reachable {
            let _ = self.events.send(Event::PluginStatusChanged { id: id.to_owned(), status: StatusKind::Unreachable });
        }
    }

    /// Records why a supervised plugin is not answering, from the one that
    /// knows. Its sources are gone with it; its address is kept so a
    /// person can see where it was.
    pub async fn set_unreachable(&self, id: &str, reason: String) {
        let mut entries = self.entries.write().await;
        let status = PluginStatus::Unreachable(reason);
        let changed = match entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                let changed = status_kind(&entry.status) != StatusKind::Unreachable;
                entry.status = status;
                entry.sources.clear();
                changed
            }
            None => {
                entries.push(PluginEntry { id: id.to_owned(), address: String::new(), status, sources: Vec::new() });
                false
            }
        };
        drop(entries);
        if changed {
            let _ = self.events.send(Event::PluginStatusChanged { id: id.to_owned(), status: StatusKind::Unreachable });
        }
    }
}

/// The plugin's status, and its sources when its manifest names the service.
///
/// Discovery failing is not the plugin being unreachable: it answered its
/// manifest. It is a plugin that claims a service it cannot describe, which
/// is reported as reachable with no sources, and logged, rather than hidden
/// behind an Unreachable that would send someone to check a process that is
/// running.
async fn probe(address: &str, granter: &Granter) -> (PluginStatus, Vec<GrpcSource>) {
    let mut client = match PluginClient::connect(address.to_string()).await {
        Ok(client) => client,
        Err(err) => return (PluginStatus::Unreachable(err.to_string()), Vec::new()),
    };

    let manifest = match client.get_manifest(GetManifestRequest {}).await {
        Ok(response) => response.into_inner(),
        Err(status) => return (PluginStatus::Unreachable(status.to_string()), Vec::new()),
    };

    let sources = if manifest.capabilities.iter().any(|capability| capability.name == SERVICE) {
        match GrpcSource::discover_with(address, granter.clone()).await {
            Ok(sources) => sources,
            Err(why) => {
                eprintln!("plugin {} names {SERVICE} but could not describe it: {why}", manifest.id);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    (PluginStatus::Reachable(manifest), sources)
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
        // A manifest and nothing behind it: all a probe reads.
        tokio::spawn(crate::source::serve(addr, crate::source::Served::new("stub", "Stub Plugin", "0.1.0", Vec::new())));
        wait_until_serving("http://127.0.0.1:50061").await;

        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "stub".into(),
                address: Some("http://127.0.0.1:50061".into()),
                command: None,
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
                command: None,
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
        let handle = tokio::spawn(crate::source::serve(addr, crate::source::Served::new("stub", "Stub Plugin", "0.1.0", Vec::new())));
        wait_until_serving("http://127.0.0.1:50063").await;

        let config = PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: "events-stub".into(),
                address: Some("http://127.0.0.1:50063".into()),
                command: None,
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
