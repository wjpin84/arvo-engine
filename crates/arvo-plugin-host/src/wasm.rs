use anyhow::Context as _;
use std::path::Path;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

wasmtime::component::bindgen!({
    path: "../../wit/plugin.wit",
    world: "plugin",
});

/// One shared `Engine` for the app — expensive to create, cheap to share
/// (it's internally `Arc`-backed), so this is built once and held by the
/// registry rather than per plugin.
pub struct WasmHost {
    engine: Engine,
}

impl Default for WasmHost {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmHost {
    /// Infallible: `Engine::default()` cannot fail, so this deliberately does
    /// not return a `Result`. It used to, which forced an `.expect()` at the
    /// single call site — the only thing that could panic during app startup,
    /// in direct contradiction of `PluginRegistry`'s own "must never panic"
    /// invariant. Removing the ceremonial `Result` removes the panic outright
    /// rather than handling it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::default(),
        }
    }

    /// Loads and instantiates the component fresh, calls `get-manifest`,
    /// and drops it — no persistent `Store` per plugin. See the
    /// plugin-execution-tiers map's Not-yet-specified: revisit only if
    /// probing gets expensive enough to matter.
    ///
    /// No WASI context, no host functions on the `Linker` — the component
    /// gets exactly nothing beyond what it exports. If a component actually
    /// imports something, instantiation fails here, which is correct: the
    /// zero-WASI sandbox policy is enforced by giving it nothing to import,
    /// not by inspecting what it asked for.
    /// # Errors
    ///
    /// Returns an error if the component cannot be read, cannot be
    /// instantiated (which includes a component asking for *any* import —
    /// see the zero-WASI note above), or if its `get-manifest` export traps.
    ///
    /// wasmtime reports failures as `anyhow::Error`, so this propagates that
    /// rather than flattening to a `String` at the point of failure. Each step
    /// adds context, so the caller can render the whole chain — "why did this
    /// component fail to load?" is the entire diagnostic question on this
    /// tier, and a bare `err.to_string()` discards the causes that answer it.
    pub fn get_manifest(&self, path: &Path) -> anyhow::Result<crate::plugin::Manifest> {
        let component = Component::from_file(&self.engine, path)
            .with_context(|| format!("failed to read component {}", path.display()))?;
        let linker = Linker::new(&self.engine);
        let mut store = Store::new(&self.engine, ());

        let instance = Plugin::instantiate(&mut store, &component, &linker)
            .context("failed to instantiate component (does it import anything?)")?;

        let manifest = instance
            .arvo_plugin_manifest_api()
            .call_get_manifest(&mut store)
            .context("component's get-manifest export failed")?;

        Ok(into_proto_manifest(manifest))
    }
}

fn into_proto_manifest(manifest: exports::arvo::plugin::manifest_api::Manifest) -> crate::plugin::Manifest {
    crate::plugin::Manifest {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        api_version: manifest.api_version,
        capabilities: manifest
            .capabilities
            .into_iter()
            .map(|capability| crate::plugin::Capability {
                name: capability.name,
                description: capability.description,
            })
            .collect(),
    }
}
