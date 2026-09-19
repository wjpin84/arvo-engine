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
}

#[derive(Debug, Default, Deserialize)]
struct Lock {
    #[serde(default)]
    disabled: BTreeSet<String>,
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
