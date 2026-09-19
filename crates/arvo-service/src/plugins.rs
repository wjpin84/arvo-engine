//! The plugins Arvo runs, and what they serve (ADR-0029).
//!
//! The registry probes them, the supervisor starts and stops them, and
//! whatever they serve joins [`crate::source::all`]. All of it belongs to the
//! process that outlives the window, for two reasons that were bugs before it
//! did: a session looks its venue up in `source::all()`, which is per-process
//! state the window used to be the only one to fill; and the supervisor spawns
//! with `kill_on_drop`, so providers used to die when the window closed.
//!
//! The credential granter is here for the same reason it is not in the window
//! any more: after ADR-0028 the engine is the only process holding a
//! credential to hand out, one call at a time (ADR-0022 point 4).

use std::path::Path;
use std::sync::Arc;

use arvo_plugin_host::registry::{PluginRegistry, PluginStatus};
use arvo_plugin_host::supervisor::{Launch, Restart, Supervisor};
use arvo_views::EventView;

use arvo_schedule::Jobs;

/// The registry and the supervisor, held together because nothing wants one
/// without the other.
#[derive(Clone)]
pub struct Plugins {
    registry: Arc<PluginRegistry>,
    supervisor: Arc<Supervisor>,
}

/// What a plugin-served source may use, per call, by vendor (ADR-0022 point
/// 4): this process reads the keychain, the plugin never does. Robinhood's
/// bearer token joins here with #149.
fn granter() -> arvo_plugin_host::source::Granter {
    Arc::new(|vendor| match vendor {
        "alpaca" => crate::source::alpaca::stored_keys().ok().flatten().map(|keys| {
            arvo_plugin_host::source::v1::Grant { key_id: keys.key_id, secret: keys.secret, bearer: String::new() }
        }),
        _ => None,
    })
}

impl Plugins {
    /// Connects to what `plugins.toml` names, starts what the installed
    /// extensions contribute, and registers the probe that keeps both honest.
    ///
    /// `raise` carries a status change to whoever is listening; the engine
    /// broadcasts it, so every front end hears the same wording.
    pub async fn start(
        config_dir: &Path,
        jobs: &Jobs,
        raise: impl Fn(EventView) + Send + Sync + 'static,
    ) -> Self {
        let config_path = config_dir.join("plugins.toml");
        let config = match arvo_core::config::load(&config_path) {
            Ok(config) => config,
            Err(err) => {
                // A hand-edited file must not stop the engine coming up: it is
                // logged loudly and read as no plugins at all.
                eprintln!("arvo-engine: {} is unreadable, continuing with no plugins: {err}", config_path.display());
                arvo_core::config::PluginsConfig::default()
            }
        };
        let registry = Arc::new(PluginRegistry::connect_with(&config, granter()).await);
        // Whatever the plugins serve joins `source::all()` (ADR-0022), in the
        // process that reads it.
        crate::source::plug(registry.served_sources().await);

        let supervisor = Arc::new(Supervisor::new(registry.clone()));
        let mut wanted = crate::extensions::launches();
        for entry in &config.plugin {
            let Some(command) = &entry.command else { continue };
            let mut words = command.split_whitespace().map(ToOwned::to_owned);
            let Some(program) = words.next() else { continue };
            wanted.push((
                entry.id.clone(),
                Launch {
                    program,
                    args: words.collect(),
                    cwd: config_dir.to_path_buf(),
                    restart: Restart::UpTo(3),
                },
            ));
        }
        supervisor.reconcile(wanted).await;

        // Every transition, to whoever is listening. A lagged subscriber keeps
        // going: this is a low-volume stream and a missed probe is not worth
        // dying over.
        let mut transitions = registry.subscribe();
        tokio::spawn(async move {
            loop {
                match transitions.recv().await {
                    Ok(event) => raise(crate::events::plugin(&event)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let plugins = Self { registry, supervisor };
        // Automatic registry refresh. 30s is a starting default, not
        // user-configurable yet.
        let probing = plugins.clone();
        jobs.every("plugins", "Probe plugins", std::time::Duration::from_secs(30), move || {
            let plugins = probing.clone();
            async move {
                let all = plugins.refresh().await;
                let reachable = all
                    .iter()
                    .filter(|view| matches!(view.status, arvo_views::PluginStatusView::Reachable { .. }))
                    .count();
                Ok(format!("{reachable} of {} reachable", all.len()))
            }
        });
        plugins
    }

    /// Every plugin, as a front end shows it.
    pub async fn snapshot(&self) -> Vec<arvo_views::PluginView> {
        self.registry.snapshot().await.iter().map(view_of).collect()
    }

    /// Probes every plugin now, re-plugs what they serve, and answers with the
    /// new list.
    pub async fn refresh(&self) -> Vec<arvo_views::PluginView> {
        self.registry.refresh().await;
        crate::source::plug(self.registry.served_sources().await);
        self.snapshot().await
    }

    /// Makes what runs match what is installed, enabled and built, and plugs
    /// what the plugins serve. A plugin just started says its address a moment
    /// later; what it serves arrives with the next probe.
    pub async fn reconcile(&self) {
        self.supervisor.reconcile(crate::extensions::launches()).await;
        crate::source::plug(self.registry.served_sources().await);
    }

    /// Stops everything this started.
    pub async fn stop_all(&self) {
        self.supervisor.stop_all().await;
    }

    /// Every signal the plugins declare or publish.
    ///
    /// A name published without being declared is shown rather than dropped: a
    /// rule could read it, so a person should be able to see it.
    pub async fn signals(&self) -> Vec<arvo_views::SignalView> {
        use arvo_plugin_host::signal;

        let providers = self.registry.served_signals().await;
        let declared = signal::declared(&providers);
        let published = signal::publish_into(&providers).await;
        let mut views: Vec<arvo_views::SignalView> = declared
            .values()
            .map(|declaration| {
                let held = published.get(declaration.name.as_str());
                arvo_views::SignalView {
                    name: declaration.name.to_string(),
                    description: declaration.description.clone(),
                    causal: declaration.causal,
                    because: declaration.because.clone(),
                    engine: declaration.engine.clone(),
                    at: held.map_or_else(String::new, |signal| signal.at.to_string()),
                    value: held.and_then(|signal| signal.value),
                }
            })
            .collect();
        for name in published.names() {
            if !declared.contains_key(name) {
                let held = published.get(name.as_str());
                views.push(arvo_views::SignalView {
                    name: name.to_string(),
                    description: "published without being declared".to_owned(),
                    causal: false,
                    because: "its provider did not say how it is computed".to_owned(),
                    engine: String::new(),
                    at: held.map_or_else(String::new, |signal| signal.at.to_string()),
                    value: held.and_then(|signal| signal.value),
                });
            }
        }
        views.sort_by(|left, right| left.name.cmp(&right.name));
        views
    }
}

/// One plugin, as a front end sees it.
///
/// A free function rather than a `From` impl: [`arvo_views::PluginView`] and
/// [`arvo_plugin_host::registry::PluginEntry`] are both foreign here, so the
/// orphan rule forbids the impl. That is the rule doing its job.
fn view_of(entry: &arvo_plugin_host::registry::PluginEntry) -> arvo_views::PluginView {
    let status = match &entry.status {
        PluginStatus::Reachable(manifest) => arvo_views::PluginStatusView::Reachable {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            capabilities: manifest.capabilities.iter().map(|capability| capability.name.clone()).collect(),
        },
        PluginStatus::Unreachable(reason) => arvo_views::PluginStatusView::Unreachable { reason: reason.clone() },
    };
    arvo_views::PluginView { id: entry.id.clone(), address: entry.address.clone(), status }
}
