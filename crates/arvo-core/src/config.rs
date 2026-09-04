use serde::Deserialize;
use std::fmt;
use std::path::Path;

/// Exactly one of `address` (gRPC subprocess tier) or `path` (WASM tier)
/// must be set — that shape, not a separate field, is what a plugin's
/// entry declares about which execution tier it uses. Enforced by `load`.
#[derive(Debug, Deserialize, Clone)]
pub struct PluginConfigEntry {
    pub id: String,
    pub address: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PluginsConfig {
    #[serde(default)]
    pub plugin: Vec<PluginConfigEntry>,
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        source: toml::de::Error,
    },
    InvalidPluginEntry {
        id: String,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => write!(f, "failed to read {path}: {source}"),
            ConfigError::Parse { path, source } => write!(f, "failed to parse {path}: {source}"),
            ConfigError::InvalidPluginEntry { id, reason } => {
                write!(f, "invalid plugin entry {id:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Loads the plugin registry config. A missing file means zero plugins
/// configured, not an error. A malformed file is an error the caller must
/// surface, never a panic — this is a trust boundary (hand-edited file).
pub fn load(path: &Path) -> Result<PluginsConfig, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginsConfig::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };

    let config: PluginsConfig =
        toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;

    for entry in &config.plugin {
        let reason = match (&entry.address, &entry.path) {
            (Some(_), None) | (None, Some(_)) => continue,
            (Some(_), Some(_)) => "both address and path set; a plugin is exactly one of \
                gRPC (address) or WASM (path)",
            (None, None) => "neither address nor path set",
        };
        return Err(ConfigError::InvalidPluginEntry {
            id: entry.id.clone(),
            reason: reason.into(),
        });
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    #[test]
    fn parses_two_plugin_entries() {
        let file = write_temp(
            r#"
            [[plugin]]
            id = "stub"
            address = "http://127.0.0.1:50051"

            [[plugin]]
            id = "retirement"
            address = "http://127.0.0.1:50052"
            "#,
        );

        let config = load(file.path()).unwrap();
        assert_eq!(config.plugin.len(), 2);
        assert_eq!(config.plugin[0].id, "stub");
        assert_eq!(
            config.plugin[1].address.as_deref(),
            Some("http://127.0.0.1:50052")
        );
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let config = load(Path::new("does/not/exist/plugins.toml")).unwrap();
        assert!(config.plugin.is_empty());
    }

    #[test]
    fn malformed_file_errors_without_panicking() {
        let file = write_temp("this is not valid toml [[[");
        let result = load(file.path());
        assert!(result.is_err());
    }

    #[test]
    fn parses_mixed_address_and_path_entries() {
        let file = write_temp(
            r#"
            [[plugin]]
            id = "stub"
            address = "http://127.0.0.1:50051"

            [[plugin]]
            id = "wasm-stub"
            path = "../target/wasm32-unknown-unknown/debug/arvo_plugin_wasm_stub.wasm"
            "#,
        );

        let config = load(file.path()).unwrap();
        assert_eq!(config.plugin.len(), 2);

        assert_eq!(
            config.plugin[0].address.as_deref(),
            Some("http://127.0.0.1:50051")
        );
        assert_eq!(config.plugin[0].path, None);

        assert_eq!(config.plugin[1].address, None);
        assert!(config.plugin[1].path.is_some());
    }

    #[test]
    fn entry_with_both_address_and_path_errors() {
        let file = write_temp(
            r#"
            [[plugin]]
            id = "ambiguous"
            address = "http://127.0.0.1:50051"
            path = "some/plugin.wasm"
            "#,
        );

        let result = load(file.path());
        assert!(matches!(
            result,
            Err(ConfigError::InvalidPluginEntry { id, .. }) if id == "ambiguous"
        ));
    }

    #[test]
    fn entry_with_neither_address_nor_path_errors() {
        let file = write_temp(
            r#"
            [[plugin]]
            id = "empty"
            "#,
        );

        let result = load(file.path());
        assert!(matches!(
            result,
            Err(ConfigError::InvalidPluginEntry { id, .. }) if id == "empty"
        ));
    }
}
