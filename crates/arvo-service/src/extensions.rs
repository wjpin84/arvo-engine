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

/// One contributed strategy document, as the extensions page shows it (#162).
///
/// A document that cannot be parsed, or that this build cannot offer, is
/// listed with the reason rather than dropped: a catalog one entry short with
/// no explanation is the failure shape this project keeps paying for.
///
/// Here rather than in the window because the answer is `offerable`'s: what
/// the picker can run is this side's question.
#[must_use]
pub fn strategy_view(extension: &str, value: &serde_json::Value) -> arvo_views::StrategyContributionView {
    let unreadable = |reason: String| arvo_views::StrategyContributionView {
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
    arvo_views::StrategyContributionView {
        // Namespaced by its extension, as a theme is: two catalogs may both
        // ship a `fast-cross`.
        id: namespaced(extension, &document.name),
        label: if document.label.is_empty() { document.name.clone() } else { document.label.clone() },
        premise: document.premise.clone(),
        kind: kind.to_owned(),
        interval: document.interval.to_string(),
        configurations,
        problem: crate::research::offerable(&document).err(),
    }
}
