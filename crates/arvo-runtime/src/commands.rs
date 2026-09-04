use arvo_plugin_host::registry::{PluginEntry, PluginRegistry, PluginStatus};
use serde::{Serialize, Serializer};
use std::sync::Arc;

/// Error surface for Tauri commands.
///
/// Tauri requires an async command that borrows `State<'_, _>` to return a
/// `Result`, so these signatures cannot simply return `T` — but `()` as the
/// error type means a future failure reaches the UI carrying nothing at all.
/// Both commands happen to be infallible today; this exists so that the first
/// one that isn't has somewhere to put the reason.
///
/// Serialises as a plain string, because that is what the front end can
/// actually render.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("{0}")]
    Failed(String),
}

impl Serialize for CommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Serialize)]
pub struct PluginView {
    pub id: String,
    pub address: Option<String>,
    pub status: PluginStatusView,
}

#[derive(Serialize)]
#[serde(tag = "state")]
pub enum PluginStatusView {
    Reachable {
        name: String,
        version: String,
        capabilities: Vec<String>,
    },
    Unreachable {
        reason: String,
    },
}

impl From<&PluginEntry> for PluginView {
    fn from(entry: &PluginEntry) -> Self {
        let status = match &entry.status {
            PluginStatus::Reachable(manifest) => PluginStatusView::Reachable {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                capabilities: manifest
                    .capabilities
                    .iter()
                    .map(|capability| capability.name.clone())
                    .collect(),
            },
            PluginStatus::Unreachable(reason) => PluginStatusView::Unreachable {
                reason: reason.clone(),
            },
        };

        Self {
            id: entry.id.clone(),
            address: entry.address.clone(),
            status,
        }
    }
}

#[tauri::command]
pub async fn list_plugins(
    registry: tauri::State<'_, Arc<PluginRegistry>>,
) -> Result<Vec<PluginView>, CommandError> {
    Ok(registry.snapshot().await.iter().map(PluginView::from).collect())
}

#[tauri::command]
pub async fn refresh_plugins(
    registry: tauri::State<'_, Arc<PluginRegistry>>,
) -> Result<Vec<PluginView>, CommandError> {
    registry.refresh().await;
    Ok(registry.snapshot().await.iter().map(PluginView::from).collect())
}
