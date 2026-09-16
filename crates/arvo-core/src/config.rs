use serde::Deserialize;
use std::fmt;
use std::path::Path;

/// A plugin is a process speaking gRPC. `address` means a person started it
/// and Arvo probes it; `command` means Arvo starts it and owns it (ADR-0023
/// point 3). An entry needs one of the two, which [`load`] checks.
#[derive(Debug, Deserialize, Clone)]
pub struct PluginConfigEntry {
    pub id: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
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
    /// Well-formed, and wrong: an entry that says neither where a plugin is
    /// nor how to start it.
    Invalid {
        path: String,
        detail: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => write!(f, "failed to read {path}: {source}"),
            ConfigError::Parse { path, source } => write!(f, "failed to parse {path}: {source}"),
            ConfigError::Invalid { path, detail } => write!(f, "{path}: {detail}"),
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

    let config: PluginsConfig = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    if let Some(entry) = config.plugin.iter().find(|entry| entry.address.is_none() && entry.command.is_none()) {
        return Err(ConfigError::Invalid {
            path: path.display().to_string(),
            detail: format!("plugin {:?} has neither an address to probe nor a command to run", entry.id),
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
        assert_eq!(config.plugin[1].address.as_deref(), Some("http://127.0.0.1:50052"));
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
    fn an_entry_with_neither_address_nor_command_is_refused_rather_than_silently_skipped() {
        let file = write_temp(
            r#"
            [[plugin]]
            id = "empty"
            "#,
        );
        assert!(matches!(load(file.path()), Err(ConfigError::Invalid { .. })));

        // Either one is enough: a command entry is the supervisor's to start.
        let file = write_temp(
            r#"
            [[plugin]]
            id = "mine"
            command = "target/release/my-plugin --verbose"
            "#,
        );
        let config = load(file.path()).unwrap();
        assert!(config.plugin[0].address.is_none());
        assert_eq!(config.plugin[0].command.as_deref(), Some("target/release/my-plugin --verbose"));
    }
}
