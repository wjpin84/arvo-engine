//! What installed extensions contribute to research, read from disk.
//!
//! Only the part the service needs: the strategy documents of enabled
//! extensions. Installing, building, enabling and the rest of the manifest
//! live with the window (`arvo-runtime`), which shares these constants so
//! the two read one layout.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// Under the app data directory: one folder per installed extension.
pub const SUBDIR: &str = "extensions";
/// The manifest at the root of each extension folder.
pub const MANIFEST: &str = "arvo-extension.json";
/// What is installed, pinned and disabled, beside the folders.
pub const LOCKFILE: &str = "installed.json";
/// Build scratch, never an extension.
pub const CACHE: &str = ".cache";

#[derive(Debug, Deserialize)]
struct Manifest {
    id: String,
    #[serde(default)]
    contributes: Contributes,
}

#[derive(Debug, Default, Deserialize)]
struct Contributes {
    #[serde(default)]
    strategies: Vec<serde_json::Value>,
    #[serde(default)]
    providers: Vec<ProviderContribution>,
}

/// A process the supervisor may run. Only the parts needed to launch one: the
/// window reads the rest of this shape when it installs and builds.
#[derive(Debug, Clone, Deserialize)]
struct ProviderContribution {
    id: String,
    #[serde(default)]
    run: String,
}

#[derive(Debug, Default, Deserialize)]
struct Lock {
    #[serde(default)]
    disabled: BTreeSet<String>,
    #[serde(default)]
    built: std::collections::BTreeMap<String, Built>,
}

/// What a build produced, as the window recorded it (ADR-0025). Read here so
/// the supervisor runs the artifact rather than the recipe's bare program name.
#[derive(Debug, Clone, Deserialize)]
struct Built {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    artifacts: std::collections::BTreeMap<String, String>,
}

/// A contributed strategy's name in the picker: the extension's id first, so
/// two extensions can both ship a `crossover`.
#[must_use]
pub fn namespaced(extension: &str, name: &str) -> String {
    format!("{extension}.{name}")
}

/// What the installed, enabled extensions contribute to the picker. The
/// project's own rulesets join these in `crate::rulesets::refresh`. Without
/// an app handle, because the headless engine asks too: the folder is the
/// app data directory's, which `project::app_data_root` names the same way.
#[must_use]
pub fn contributed() -> Vec<(String, arvo_research::StrategyDocument)> {
    let Ok(dir) = crate::project::app_data_root().map(|root| root.join(SUBDIR)) else { return Vec::new() };
    let lock: Lock = std::fs::read_to_string(dir.join(LOCKFILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    contributed_in(&dir, &lock.disabled)
}

fn contributed_in(dir: &Path, disabled: &BTreeSet<String>) -> Vec<(String, arvo_research::StrategyDocument)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found = Vec::new();
    for folder in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.file_name().is_some_and(|name| name != CACHE))
    {
        let Ok(text) = std::fs::read_to_string(folder.join(MANIFEST)) else { continue };
        let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else { continue };
        // A disabled extension contributes nothing, the way its theme does
        // not apply.
        if disabled.contains(&manifest.id) {
            continue;
        }
        for value in &manifest.contributes.strategies {
            if let Ok(document) = serde_json::from_value::<arvo_research::StrategyDocument>(value.clone()) {
                found.push((namespaced(&manifest.id, &document.name), document));
            }
        }
    }
    found
}

/// One contributed strategy document, as the extensions page shows it (#162).
///
/// A document that cannot be parsed, or that this build cannot offer, is
/// listed with the reason rather than dropped: a catalog one entry short with
/// no explanation is the failure shape this project keeps paying for.
///
/// Here rather than in the window because the answer is `offerable`'s: what
/// the picker can run is this side's question.
#[must_use]
pub fn strategy_view(extension: &str, value: &serde_json::Value) -> arvo_api::StrategyContributionView {
    let unreadable = |reason: String| arvo_api::StrategyContributionView {
        id: format!("{extension}.?"),
        label: String::new(),
        premise: String::new(),
        kind: String::new(),
        interval: String::new(),
        configurations: 0,
        problem: Some(reason),
    };
    let document: arvo_research::StrategyDocument = match serde_json::from_value(value.clone()) {
        Ok(document) => document,
        Err(err) => return unreadable(format!("not a strategy document: {err}")),
    };
    let (kind, configurations) = match &document.kind {
        arvo_research::StrategyKind::Grid(grid) => ("grid", grid.configurations()),
        arvo_research::StrategyKind::Rules(_) => ("rules", 0),
    };
    arvo_api::StrategyContributionView {
        // Namespaced by its extension, as a theme is: two catalogs may both
        // ship a `fast-cross`.
        id: namespaced(extension, &document.name),
        label: if document.label.is_empty() { document.name.clone() } else { document.label.clone() },
        premise: document.premise.clone(),
        kind: kind.to_owned(),
        interval: document.interval.to_string(),
        configurations: arvo_api::count(configurations),
        problem: crate::research::offerable(&document).err(),
    }
}

/// The extensions directory, under the app data directory.
///
/// # Errors
///
/// `APPDATA` is not set.
pub fn root() -> Result<std::path::PathBuf, String> {
    crate::project::app_data_root().map(|root| root.join(SUBDIR))
}

/// Splits a recipe into a program and its arguments.
///
/// Whitespace, and no shell: a manifest can name a program and its arguments
/// and cannot smuggle a second command through a semicolon.
fn argv(recipe: &str) -> Option<(String, Vec<String>)> {
    let mut words = recipe.split_whitespace().map(ToOwned::to_owned);
    let program = words.next()?;
    Some((program, words.collect()))
}

/// Every provider that should be running: installed, enabled, built, with an
/// artifact to run.
///
/// Read from disk on every call rather than cached, because the window writes
/// this directory while the engine is up.
#[must_use]
pub fn launches() -> Vec<(String, arvo_plugin_host::supervisor::Launch)> {
    use arvo_plugin_host::supervisor::{Launch, Restart};

    let mut wanted = Vec::new();
    let Ok(dir) = root() else { return wanted };
    let lock: Lock = std::fs::read_to_string(dir.join(LOCKFILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(&dir) else { return wanted };
    for folder in entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()) {
        let Ok(text) = std::fs::read_to_string(folder.join(MANIFEST)) else { continue };
        let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else { continue };
        if lock.disabled.contains(&manifest.id) {
            continue;
        }
        let Some(built) = lock.built.get(&manifest.id).filter(|built| built.ok) else { continue };
        for provider in &manifest.contributes.providers {
            let Some((program, args)) = argv(&provider.run) else { continue };
            let program = built.artifacts.get(&provider.id).cloned().unwrap_or(program);
            wanted.push((
                format!("{}/{}", manifest.id, provider.id),
                Launch { program, args, cwd: folder.clone(), restart: Restart::UpTo(3) },
            ));
        }
    }
    wanted
}

/// What each installed extension contributes to the picker, by extension id,
/// with any reason this build cannot offer it.
///
/// Whether a document can be run is `offerable`'s answer, and that lives on
/// this side of the wire. A front end renders the rows; it does not decide
/// which of them work.
#[must_use]
pub fn contributed_views() -> std::collections::BTreeMap<String, Vec<arvo_api::StrategyContributionView>> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(dir) = root() else { return out };
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    for folder in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.file_name().is_some_and(|name| name != CACHE))
    {
        let Ok(text) = std::fs::read_to_string(folder.join(MANIFEST)) else { continue };
        let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else { continue };
        let views = manifest
            .contributes
            .strategies
            .iter()
            .map(|value| strategy_view(&manifest.id, value))
            .collect();
        out.insert(manifest.id, views);
    }
    out
}
