use keyring::Entry;

const SERVICE: &str = "com.arvo.desktop";

/// How much of a secret goes in one keychain entry.
///
/// Windows' credential store caps a secret at 2560 *bytes* of UTF-16 — 1280
/// characters of ASCII, not the 2560 the platform error claims. An OAuth
/// token pair serialised as JSON is comfortably past that, so signing in to a
/// broker failed on Windows at the moment it tried to save what the browser
/// had just handed it, every time, with the reason buried in a `#[source]`
/// nothing printed.
///
/// Counted in UTF-16 code units rather than bytes or `char`s because that is
/// what the platform counts. 1200 leaves headroom under the 1280 ceiling; the
/// other platforms' limits are far higher and are not the binding constraint.
const CHUNK_UTF16_UNITS: usize = 1200;

/// Keeps the underlying `keyring::Error` as a `#[source]` rather than
/// flattening it to a `String`. A keychain failure is almost always about
/// *why* the platform refused — locked keyring, no backend, denied by
/// policy — and that detail lives in the cause.
///
/// # Why the cause is also in the message
///
/// Convention says a `Display` should not repeat its `#[source]`, because a
/// reporter walks the chain and printing it twice is noise. Nothing here
/// walks it. Both consumers — `tracing::error!(error = %err)` and the string
/// a Tauri command hands the window — call `Display` and stop.
///
/// That cost a real diagnosis. "failed to store token for plugin
/// \"robinhood\"" was all anyone saw, in the log and on screen, while the
/// actual sentence — that the value was past the platform's size limit — sat
/// one level down where nothing looked. The doc comment above promised the
/// detail was in the cause; it was, and it was unreachable.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("failed to open keychain entry for plugin {plugin_id:?}: {source}")]
    Entry {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to store token for plugin {plugin_id:?}: {source}")]
    Store {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to read token for plugin {plugin_id:?}: {source}")]
    Read {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
    #[error("failed to delete token for plugin {plugin_id:?}: {source}")]
    Delete {
        plugin_id: String,
        #[source]
        source: keyring::Error,
    },
}

/// One entry of a stored secret.
///
/// Index 0 keeps the original un-suffixed name, so a token written before
/// this crate learned to split is still found by the code that reads it now.
fn entry(plugin_id: &str, index: usize) -> Result<Entry, SecretsError> {
    let key = if index == 0 {
        format!("plugin.{plugin_id}.token")
    } else {
        format!("plugin.{plugin_id}.token.{index}")
    };
    Entry::new(SERVICE, &key).map_err(|source| SecretsError::Entry {
        plugin_id: plugin_id.to_owned(),
        source,
    })
}

/// Splits a secret at boundaries the platform will accept.
///
/// Always yields at least one piece, so an empty secret still round-trips as
/// stored-and-empty rather than as never-stored.
fn split(value: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut units = 0;

    for character in value.chars() {
        let width = character.len_utf16();
        // Split before the character that would overflow, never mid-character
        // — a piece cut through a surrogate pair is not valid UTF-16 and the
        // platform would reject it for a different reason than the one being
        // avoided.
        if units + width > CHUNK_UTF16_UNITS && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(character);
        units += width;
    }
    pieces.push(current);
    pieces
}

/// Removes entries from `from` onwards, stopping at the first gap.
///
/// The reason a rewrite cannot just overwrite: a secret that used to need
/// five entries and now needs three would otherwise leave two behind, and the
/// next read — which stops at the first gap — would splice them onto the end
/// and hand back a corrupted token.
fn delete_from(plugin_id: &str, from: usize) -> Result<(), SecretsError> {
    for index in from.. {
        match entry(plugin_id, index)?.delete_credential() {
            Ok(()) => {}
            Err(keyring::Error::NoEntry) => return Ok(()),
            Err(source) => {
                return Err(SecretsError::Delete {
                    plugin_id: plugin_id.to_owned(),
                    source,
                })
            }
        }
    }
    Ok(())
}

/// Desktop→plugin auth tokens only — a plugin's own upstream credentials
/// (a brokerage API key, etc.) are that plugin's own concern, not stored
/// here. See the arvo-core map's Notes.
///
/// # Errors
///
/// Returns an error if the platform keychain cannot be opened or written.
pub fn store_token(plugin_id: &str, token: &str) -> Result<(), SecretsError> {
    let pieces = split(token);
    for (index, piece) in pieces.iter().enumerate() {
        entry(plugin_id, index)?
            .set_password(piece)
            .map_err(|source| SecretsError::Store {
                plugin_id: plugin_id.to_owned(),
                source,
            })?;
    }
    // Anything a longer previous token left behind.
    delete_from(plugin_id, pieces.len())
}

/// A token that was never stored is `Ok(None)`, not an error — same
/// "absence isn't failure" shape as `config::load` on a missing file.
///
/// # Errors
///
/// Returns an error if the keychain cannot be opened or read. A token that
/// was never stored is `Ok(None)`, not an error.
pub fn get_token(plugin_id: &str) -> Result<Option<String>, SecretsError> {
    let mut token = String::new();

    for index in 0.. {
        match entry(plugin_id, index)?.get_password() {
            Ok(piece) => token.push_str(&piece),
            // The first gap ends the secret. On index 0 that means nobody
            // ever stored one.
            Err(keyring::Error::NoEntry) if index == 0 => return Ok(None),
            Err(keyring::Error::NoEntry) => break,
            Err(source) => {
                return Err(SecretsError::Read {
                    plugin_id: plugin_id.to_owned(),
                    source,
                })
            }
        }
    }

    Ok(Some(token))
}

/// # Errors
///
/// Returns an error if the keychain cannot be opened, or the entry cannot be
/// deleted.
pub fn delete_token(plugin_id: &str) -> Result<(), SecretsError> {
    delete_from(plugin_id, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the real platform keychain.
    ///
    /// These do not use a fake: the whole point of them is that the *platform*
    /// accepts what is written, which is exactly what a fake would assume. The
    /// cost is that they share one mutable store, and Rust runs tests in
    /// parallel by default.
    ///
    /// Left unserialised they fail intermittently and misleadingly. The
    /// oversized-token test reported its five pieces stored and then read back
    /// `None` — a result that reads as a splitting bug and is nothing of the
    /// kind. Distinct plugin ids are not enough; the store itself is the
    /// shared resource.
    static KEYCHAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the keychain lock, ignoring poisoning.
    ///
    /// A test that panics while holding it has already failed and reported
    /// why. Propagating the poison would fail every other keychain test too,
    /// burying the one real failure under four unrelated ones.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        KEYCHAIN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many credentials a secret actually occupies, asked of the
    /// platform rather than of [`get_token`] — which stops at the first gap
    /// and so cannot see pieces orphaned past one.
    fn stored_pieces(plugin_id: &str) -> usize {
        (0..16)
            .filter(|index| entry(plugin_id, *index).unwrap().get_password().is_ok())
            .count()
    }

    #[test]
    fn round_trips_a_token() {
        let _guard = exclusive();
        let plugin_id = "test-plugin-secrets-round-trip";
        // Never assume a clean slate from a prior failed run.
        let _ = delete_token(plugin_id);

        assert_eq!(get_token(plugin_id).unwrap(), None);

        store_token(plugin_id, "s3cr3t").unwrap();
        assert_eq!(get_token(plugin_id).unwrap(), Some("s3cr3t".to_string()));

        delete_token(plugin_id).unwrap();
        assert_eq!(get_token(plugin_id).unwrap(), None);
    }

    /// The failure that shipped: an OAuth token pair as JSON is a few
    /// thousand characters, Windows' credential store takes 2560 *bytes* of
    /// UTF-16 — 1280 ASCII characters — and signing in to a broker therefore
    /// could not succeed on Windows at all. It failed at the moment it tried
    /// to save what the browser had just handed it.
    ///
    /// Sized well past a single entry so it stays a real test of the
    /// splitting rather than of one entry that happens to fit.
    #[test]
    fn a_token_far_larger_than_one_keychain_entry_round_trips() {
        let _guard = exclusive();
        let plugin_id = "test-plugin-secrets-oversized";
        let _ = delete_token(plugin_id);

        // Not a repeated character: a splice at the wrong offset, or two
        // pieces reassembled out of order, is invisible in "xxxx…".
        let token: String = (0..6000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        store_token(plugin_id, &token).unwrap();

        assert!(stored_pieces(plugin_id) > 1, "the point of the test is a split");
        assert_eq!(get_token(plugin_id).unwrap().as_deref(), Some(token.as_str()));

        delete_token(plugin_id).unwrap();
        assert_eq!(get_token(plugin_id).unwrap(), None);
        // Not `get_token` alone: it stops at the first gap, so it reports
        // "nothing stored" the moment piece 0 is gone and would pass a
        // delete that left four credentials behind in the user's keychain.
        assert_eq!(stored_pieces(plugin_id), 0, "every piece must be gone");
    }

    /// A rewrite that needs fewer entries than last time has to remove the
    /// ones it no longer uses. Leaving them behind means the next read — which
    /// stops at the first gap — splices the old tail onto the new token and
    /// returns a credential that was never stored.
    #[test]
    fn shrinking_a_token_does_not_leave_a_tail_behind() {
        let _guard = exclusive();
        let plugin_id = "test-plugin-secrets-shrink";
        let _ = delete_token(plugin_id);

        store_token(plugin_id, &"x".repeat(6000)).unwrap();
        store_token(plugin_id, "short").unwrap();

        assert_eq!(get_token(plugin_id).unwrap().as_deref(), Some("short"));
        assert_eq!(stored_pieces(plugin_id), 1, "the long token's tail must be gone");

        delete_token(plugin_id).unwrap();
        assert_eq!(stored_pieces(plugin_id), 0);
    }

    /// Splitting counts UTF-16 code units because that is what the platform
    /// counts, and it never cuts through a character — half a surrogate pair
    /// is not valid UTF-16 and would be refused for a different reason than
    /// the one being avoided.
    #[test]
    fn pieces_fit_the_platform_limit_and_reassemble_exactly() {
        // Emoji are two UTF-16 units each, so this is twice the length it
        // looks and exercises the boundary the ASCII case cannot.
        for original in ["", "short", &"a".repeat(5000), &"🙂".repeat(2000)] {
            let pieces = split(original);
            assert!(!pieces.is_empty(), "always at least one piece");
            for piece in &pieces {
                assert!(
                    piece.encode_utf16().count() <= CHUNK_UTF16_UNITS,
                    "a piece of {} units is past the platform limit",
                    piece.encode_utf16().count()
                );
            }
            assert_eq!(pieces.concat(), original, "reassembly must be exact");
        }
    }

    #[test]
    fn getting_a_never_stored_token_is_not_an_error() {
        let _guard = exclusive();
        let result = get_token("test-plugin-secrets-never-stored");
        assert_eq!(result.unwrap(), None);
    }
}

