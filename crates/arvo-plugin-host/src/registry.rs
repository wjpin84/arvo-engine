//! Every plugin Arvo knows about, and what it last said.
//!
//! An entry arrives one of two ways. An `address` in `plugins.toml` is a
//! process a person started, which the registry probes at launch and every
//! refresh. A supervised process (ADR-0023) registers itself through [`add`]
//! once it has said where it listens, and the supervisor keeps its entry
//! honest through [`remove`] and [`set_unreachable`] as it comes and goes.
//! Either way the entry is the same shape, and the Extensions view lists
//! both under Providers.
//!
//! Registered is not reachable: a plugin being down is expected of an
//! external process and must never block startup or panic. What a reachable
//! plugin serves behind `arvo.source.v1.Source` is discovered at probe time
//! and kept on the entry, so `source::all()` can list it beside the
//! compiled-in sources.
//!
//! One event, [`Event::PluginStatusChanged`], and only on a change: most
//! probes change nothing, and an event per probe would be noise the moment a
//! second plugin existed.
//!
//! [`add`]: PluginRegistry::add
//! [`remove`]: PluginRegistry::remove
//! [`set_unreachable`]: PluginRegistry::set_unreachable

use crate::plugin::plugin_client::PluginClient;
use crate::plugin::{GetManifestRequest, Manifest};
use crate::source::{no_grants, GrpcSource, Granter, SERVICE};
use arvo_core::config::PluginsConfig;
use arvo_core::events::{Event, StatusKind};
use tokio::sync::{broadcast, RwLock};

/// What a plugin last said, or why it could not be asked.
#[derive(Debug, Clone)]
pub enum PluginStatus {
    Reachable(Manifest),
    Unreachable(String),
}

impl PluginStatus {
    fn kind(&self) -> StatusKind {
        match self {
            Self::Reachable(_) => StatusKind::Reachable,
            Self::Unreachable(_) => StatusKind::Unreachable,
        }
    }
}

/// One plugin: where it is, what it said, and what it serves.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub id: String,
    /// Where it listens. Empty for a supervised plugin that is down, whose
    /// next address is not known until it says so.
    pub address: String,
    pub status: PluginStatus,
    /// What it serves behind `arvo.source.v1.Source`, read when it was last
    /// reachable and its manifest named the service (ADR-0022 point 2). Empty
    /// for a plugin that serves something else, or is down.
    pub sources: Vec<GrpcSource>,
}

/// The one tier of plugins: processes speaking gRPC.
///
/// There was an in-process WASM tier beside this one. It was removed rather
/// than extended, because its sandbox policy and its purpose had come into
/// direct conflict: components were instantiated with an empty `Linker` and no
/// WASI context, so a component importing *anything* failed to instantiate.
/// That is a sound way to run untrusted arithmetic and a structurally
/// impossible way to run a data source, which needs a socket and a credential.
/// A plugin worth having is one that can fetch bars; that is a process, and a
/// process is this tier (ADR-0012).
pub struct PluginRegistry {
    entries: RwLock<Vec<PluginEntry>>,
    events: broadcast::Sender<Event>,
    /// What a plugin-served source may use per call (ADR-0022 point 4).
    granter: Granter,
}

impl PluginRegistry {
    /// Probes every `address` entry in `config` once. Nothing is published:
    /// there is no earlier state for anything to have changed from.
    pub async fn connect(config: &PluginsConfig) -> Self {
        Self::connect_with(config, no_grants()).await
    }

    /// [`Self::connect`], with the granter every plugin-served source asks
    /// for what its calls may use. The app installs one that reads the
    /// keychain; a host with no keychain, or a test, installs none.
    pub async fn connect_with(config: &PluginsConfig, granter: Granter) -> Self {
        let (events, _) = broadcast::channel(16);
        let mut entries = Vec::with_capacity(config.plugin.len());
        for plugin in &config.plugin {
            // A `command` entry is the supervisor's to start; it registers
            // itself here once it has said where it listens.
            let Some(address) = plugin.address.clone() else { continue };
            let (status, sources) = probe(&address, &granter).await;
            entries.push(PluginEntry { id: plugin.id.clone(), address, status, sources });
        }
        Self { entries: RwLock::new(entries), events, granter }
    }

    /// Every status change, as it happens. This is the registry's bus, not a
    /// `PluginStatusChanged`-only one, though that is the only producer today.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Re-probes every plugin with an address, so one started after the app
    /// was is picked up without a restart. A supervised plugin that is down
    /// has no address to probe; what the supervisor said about it stands
    /// until it is back.
    pub async fn refresh(&self) {
        let known = self.entries.read().await.clone();
        let mut refreshed = Vec::with_capacity(known.len());
        for old in known {
            if old.address.is_empty() {
                refreshed.push(old);
                continue;
            }
            let (status, sources) = probe(&old.address, &self.granter).await;
            self.announce(&old.id, Some(old.status.kind()), status.kind());
            refreshed.push(PluginEntry { id: old.id, address: old.address, status, sources });
        }
        *self.entries.write().await = refreshed;
    }

    /// Every entry, as it stands.
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
        let now = status.kind();
        let mut entries = self.entries.write().await;
        let entry = PluginEntry { id: id.to_owned(), address, status, sources };
        let was = match entries.iter().position(|entry| entry.id == id) {
            Some(at) => Some(std::mem::replace(&mut entries[at], entry).status.kind()),
            None => {
                entries.push(entry);
                None
            }
        };
        drop(entries);
        self.announce(id, was, now);
    }

    /// Forgets a plugin. One that was reachable going away is a change.
    pub async fn remove(&self, id: &str) {
        let mut entries = self.entries.write().await;
        let Some(at) = entries.iter().position(|entry| entry.id == id) else { return };
        let was = entries.remove(at).status.kind();
        drop(entries);
        self.announce(id, Some(was), StatusKind::Unreachable);
    }

    /// Records why a supervised plugin is not answering, from the one that
    /// knows. Its sources go with it; its address is kept so a person can see
    /// where it was.
    pub async fn set_unreachable(&self, id: &str, reason: String) {
        let status = PluginStatus::Unreachable(reason);
        let mut entries = self.entries.write().await;
        let was = match entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                let was = entry.status.kind();
                entry.status = status;
                entry.sources.clear();
                Some(was)
            }
            None => {
                entries.push(PluginEntry { id: id.to_owned(), address: String::new(), status, sources: Vec::new() });
                None
            }
        };
        drop(entries);
        self.announce(id, was, StatusKind::Unreachable);
    }

    /// Publishes a change of kind, and only a change: `was` is what the entry
    /// was before, `None` for one that did not exist, which counts as a change
    /// only if it is now reachable.
    fn announce(&self, id: &str, was: Option<StatusKind>, now: StatusKind) {
        let changed = match was {
            Some(was) => was != now,
            None => now == StatusKind::Reachable,
        };
        if changed {
            let _ = self.events.send(Event::PluginStatusChanged { id: id.to_owned(), status: now });
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
                tracing::warn!(plugin = %manifest.id, why, "names {SERVICE} but could not describe it");
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
    use crate::source::{serve, Served};
    use arvo_core::config::PluginConfigEntry;
    use std::time::Duration;
    use tokio::task::JoinHandle;

    /// A plugin that answers its manifest and serves nothing: all a probe
    /// reads, and all these tests need.
    async fn manifest_only(port: u16) -> JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let addr = format!("127.0.0.1:{port}").parse().expect("an address");
        let handle = tokio::spawn(serve(addr, Served::new("manifest-only", "Manifest Only", "0.1.0", Vec::new())));
        for _ in 0..50 {
            if PluginClient::connect(format!("http://127.0.0.1:{port}")).await.is_ok() {
                return handle;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the plugin never came up on {port}");
    }

    fn configured(id: &str, port: u16) -> PluginsConfig {
        PluginsConfig {
            plugin: vec![PluginConfigEntry {
                id: id.into(),
                address: Some(format!("http://127.0.0.1:{port}")),
                command: None,
            }],
        }
    }

    #[tokio::test]
    async fn reachable_plugin_returns_its_manifest() {
        let _serving = manifest_only(50061).await;
        let registry = PluginRegistry::connect(&configured("one", 50061)).await;
        let snapshot = registry.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        match &snapshot[0].status {
            PluginStatus::Reachable(manifest) => assert_eq!(manifest.id, "manifest-only"),
            PluginStatus::Unreachable(reason) => panic!("expected reachable, got: {reason}"),
        }
    }

    #[tokio::test]
    async fn dead_address_is_unreachable_not_an_error() {
        // ponytail: a port picked to almost certainly have nothing listening;
        // a collision would fail loudly, not silently.
        let registry = PluginRegistry::connect(&configured("nothing-here", 50062)).await;
        let snapshot = registry.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert!(matches!(snapshot[0].status, PluginStatus::Unreachable(_)));
    }

    #[tokio::test]
    async fn refresh_publishes_only_on_status_change() {
        use tokio::sync::broadcast::error::TryRecvError;

        let serving = manifest_only(50063).await;
        let registry = PluginRegistry::connect(&configured("events", 50063)).await;
        let mut events = registry.subscribe();

        // connect() itself must not have published anything.
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // No status change yet: refresh publishes nothing.
        registry.refresh().await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // Kill the plugin (abort its in-process task, the same effect as
        // killing a real process here) and refresh: exactly one event.
        serving.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
        registry.refresh().await;
        match events.try_recv() {
            Ok(Event::PluginStatusChanged { id, status }) => {
                assert_eq!(id, "events");
                assert_eq!(status, StatusKind::Unreachable);
            }
            other => panic!("expected a PluginStatusChanged event, got {other:?}"),
        }

        // Still down, no further change: refresh publishes nothing new.
        registry.refresh().await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    }

    /// The supervisor's three calls, and what each announces: a plugin
    /// appearing reachable is a change, one reported down is a change, one
    /// reported down again is not, and one removed while down is not.
    #[tokio::test]
    async fn a_supervised_plugin_announces_each_change_once() {
        use tokio::sync::broadcast::error::TryRecvError;

        let registry = PluginRegistry::connect(&PluginsConfig::default()).await;
        let mut events = registry.subscribe();

        registry.set_unreachable("mine", "starting".into()).await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "not yet up is not a change");
        assert_eq!(registry.snapshot().await[0].address, "", "no address until it says so");

        let _serving = manifest_only(50064).await;
        registry.add("mine", "http://127.0.0.1:50064".into()).await;
        assert!(matches!(events.try_recv(), Ok(Event::PluginStatusChanged { status: StatusKind::Reachable, .. })));

        registry.set_unreachable("mine", "exited".into()).await;
        assert!(matches!(events.try_recv(), Ok(Event::PluginStatusChanged { status: StatusKind::Unreachable, .. })));
        registry.set_unreachable("mine", "exited again".into()).await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "down twice is one change");
        assert_eq!(registry.snapshot().await.len(), 1, "replaced, not duplicated");

        registry.remove("mine").await;
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "removing a down plugin changes nothing");
        assert!(registry.snapshot().await.is_empty());
    }
}
