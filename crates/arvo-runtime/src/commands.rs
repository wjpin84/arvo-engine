use arvo_plugin_host::registry::{PluginEntry, PluginRegistry, PluginStatus};
use serde::Serialize;
use std::sync::Arc;

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
) -> Result<Vec<PluginView>, ()> {
    Ok(registry.snapshot().await.iter().map(PluginView::from).collect())
}

#[tauri::command]
pub async fn refresh_plugins(
    registry: tauri::State<'_, Arc<PluginRegistry>>,
) -> Result<Vec<PluginView>, ()> {
    registry.refresh().await;
    Ok(registry.snapshot().await.iter().map(PluginView::from).collect())
}
