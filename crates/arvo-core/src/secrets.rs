use keyring::Entry;

const SERVICE: &str = "com.arvo.desktop";

/// Keeps the underlying `keyring::Error` as a `#[source]` rather than
/// flattening it to a `String`. A keychain failure is almost always about
/// *why* the platform refused — locked keyring, no backend, denied by
/// policy — and that detail lives in the cause, not the outer message.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("failed to open keychain entry for plugin {plugin_id:?}")]
    Entry {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to store token for plugin {plugin_id:?}")]
    Store {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to read token for plugin {plugin_id:?}")]
    Read {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to delete token for plugin {plugin_id:?}")]
    Delete {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
}

fn entry(plugin_id: &str) -> Result<Entry, SecretsError> {
    Entry::new(SERVICE, &format!("plugin.{plugin_id}.token")).map_err(|source| {
        SecretsError::Entry {
            plugin_id: plugin_id.to_owned(),
            source,
        }
    })
}

/// Desktop→plugin auth tokens only — a plugin's own upstream credentials
/// (a brokerage API key, etc.) are that plugin's own concern, not stored
/// here. See the arvo-core map's Notes.
///
/// # Errors
///
/// Returns an error if the platform keychain cannot be opened or written.
pub fn store_token(plugin_id: &str, token: &str) -> Result<(), SecretsError> {
    entry(plugin_id)?
        .set_password(token)
        .map_err(|source| SecretsError::Store {
            plugin_id: plugin_id.to_owned(),
            source,
        })
}

/// A token that was never stored is `Ok(None)`, not an error — same
/// "absence isn't failure" shape as `config::load` on a missing file.
///
/// # Errors
///
/// Returns an error if the keychain cannot be opened or read. A token that
/// was never stored is `Ok(None)`, not an error.
pub fn get_token(plugin_id: &str) -> Result<Option<String>, SecretsError> {
    match entry(plugin_id)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(source) => Err(SecretsError::Read {
            plugin_id: plugin_id.to_owned(),
            source,
        }),
    }
}

/// # Errors
///
/// Returns an error if the keychain cannot be opened, or the entry cannot be
/// deleted.
pub fn delete_token(plugin_id: &str) -> Result<(), SecretsError> {
    entry(plugin_id)?
        .delete_credential()
        .map_err(|source| SecretsError::Delete {
            plugin_id: plugin_id.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_token() {
        let plugin_id = "test-plugin-secrets-round-trip";
        // Never assume a clean slate from a prior failed run.
        let _ = delete_token(plugin_id);

        assert_eq!(get_token(plugin_id).unwrap(), None);

        store_token(plugin_id, "s3cr3t").unwrap();
        assert_eq!(get_token(plugin_id).unwrap(), Some("s3cr3t".to_string()));

        delete_token(plugin_id).unwrap();
        assert_eq!(get_token(plugin_id).unwrap(), None);
    }

    #[test]
    fn getting_a_never_stored_token_is_not_an_error() {
        let result = get_token("test-plugin-secrets-never-stored");
        assert_eq!(result.unwrap(), None);
    }
}
