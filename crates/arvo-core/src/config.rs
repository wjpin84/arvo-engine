use serde::Deserialize;
use std::fmt;
use std::path::Path;

/// A plugin is a process speaking gRPC at `address`.
///
/// There used to be a second, optional `path` for an in-process WASM tier, and
/// the pair had to be exactly-one-of. Both the field and the validation went
/// with the tier: a required field enforces the same rule with no code, and
/// serde reports a missing one better than a hand-written check did.
#[derive(Debug, Deserialize, Clone)]
pub struct PluginConfigEntry {
    pub id: String,
    pub address: String,
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
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => write!(f, "failed to read {path}: {source}"),
            ConfigError::Parse { path, source } => write!(f, "failed to parse {path}: {source}"),
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

    toml::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
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
        assert_eq!(config.plugin[1].address, "http://127.0.0.1:50052");
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
    fn an_entry_with_no_address_is_a_parse_error_rather_than_a_silent_skip() {
        // Used to be a hand-written InvalidPluginEntry check. serde enforces
        // it now that `address` is required, and says which field is missing.
        let file = write_temp(
            r#"
            [[plugin]]
            id = "empty"
            "#,
        );

        assert!(matches!(load(file.path()), Err(ConfigError::Parse { .. })));
    }
}
