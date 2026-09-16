//! The key pair, and getting an authenticated request out.
//!
//! An API key id and secret in the OS keychain, sent as headers. No OAuth —
//! Alpaca issues keys directly, so there is no flow to run and nothing to
//! refresh, which is the whole difference from `arvo_robinhood::auth`.
//!
//! The environment is read as a fallback so the headless examples work before
//! any UI exists to paste a key into. `APCA_API_KEY_ID` and
//! `APCA_API_SECRET_KEY` are Alpaca's own names, which is what every one of
//! their SDKs already reads.

use arvo_data::source::SourceError;
use serde_json::Value;

/// One Alpaca login owns two accounts: paper and live. Each has its own key
/// pair, from its own dashboard, and each only works against its own endpoint
/// — a paper key gets a 403 from the live API and the reverse. So they are two
/// credentials, and which one is used is part of what a call *is*, the same
/// way [`crate::AlpacaExecutor::paper`] and `::live` are two constructors.
///
/// Which money is at stake is the whole difference, so it is a type rather
/// than a boolean anyone could pass the wrong way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    /// Simulated money. Where a person should start.
    Paper,
    /// Real money.
    Live,
}

impl Env {
    /// The spelling the window uses: `paper` or `live`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Live => "live",
        }
    }

    /// The window's spelling back to an `Env`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "paper" => Some(Self::Paper),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    /// Where this environment's key pair lives in the OS keychain.
    const fn credential_id(self) -> &'static str {
        match self {
            Self::Paper => "alpaca-paper",
            Self::Live => "alpaca-live",
        }
    }

    /// The trading API host for this environment. Paper and live are the same
    /// API against different money, at different addresses.
    #[must_use]
    pub const fn trading_host(self) -> &'static str {
        match self {
            Self::Paper => "https://paper-api.alpaca.markets",
            Self::Live => "https://api.alpaca.markets",
        }
    }

    /// The venue a holding imported from this account is filed under: the
    /// same names the executor trades as, so paper and live never mix.
    #[must_use]
    pub const fn venue(self) -> &'static str {
        match self {
            Self::Paper => "ALPACA-PAPER",
            Self::Live => "ALPACA",
        }
    }
}

/// The single keychain entry Alpaca keys used to live under, before paper and
/// live were told apart. Read as paper, so a key pair stored by an earlier
/// build keeps working; moved to `alpaca-paper` the next time paper keys are
/// stored, and cleared when paper keys are forgotten.
pub const LEGACY_CREDENTIAL_ID: &str = "alpaca";

/// An Alpaca key pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Keys {
    pub key_id: String,
    pub secret: String,
}

/// The key pair stored for one environment, or `None`. Paper falls back to
/// the pre-split `alpaca` entry, so nothing a person stored before is lost.
pub(crate) fn keys_for(env: Env) -> Result<Option<Keys>, SourceError> {
    if let Some(text) = arvo_core::secrets::get_token(env.credential_id()).map_err(credential)? {
        return serde_json::from_str(&text).map(Some).map_err(credential);
    }
    if env == Env::Paper {
        if let Some(text) = arvo_core::secrets::get_token(LEGACY_CREDENTIAL_ID).map_err(credential)? {
            return serde_json::from_str(&text).map(Some).map_err(credential);
        }
    }
    Ok(None)
}

/// The key pair a data call uses: paper first, then live, then the
/// environment. Data does not care which account it is — bars and option
/// quotes come off the same shared feed either pair can reach — so it uses
/// whichever exists, preferring the one with no real money behind it.
///
/// Keychain first: the environment is a convenience for headless runs, not the
/// intended home for a credential.
pub(crate) fn keys() -> Result<Option<Keys>, SourceError> {
    if let Some(keys) = keys_for(Env::Paper)? {
        return Ok(Some(keys));
    }
    if let Some(keys) = keys_for(Env::Live)? {
        return Ok(Some(keys));
    }
    let (Ok(key_id), Ok(secret)) = (
        std::env::var("APCA_API_KEY_ID"),
        std::env::var("APCA_API_SECRET_KEY"),
    ) else {
        return Ok(None);
    };
    Ok(Some(Keys { key_id, secret }))
}

/// Whether an environment has a key pair stored.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain cannot be read.
pub fn has(env: Env) -> Result<bool, SourceError> {
    Ok(keys_for(env)?.is_some())
}

/// Stores a key pair for one environment.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the write.
pub fn store(env: Env, keys: &Keys) -> Result<(), SourceError> {
    let text = serde_json::to_string(keys).map_err(credential)?;
    arvo_core::secrets::store_token(env.credential_id(), &text).map_err(credential)?;
    // The keys are under their own entry now; the shared one it may have been
    // read from would otherwise shadow a later change.
    if env == Env::Paper {
        let _ = arvo_core::secrets::delete_token(LEGACY_CREDENTIAL_ID);
    }
    Ok(())
}

/// Forgets one environment's key pair, and the pre-split entry when it is
/// paper's fallback.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the delete.
pub fn forget(env: Env) -> Result<(), SourceError> {
    arvo_core::secrets::delete_token(env.credential_id()).map_err(credential)?;
    if env == Env::Paper {
        let _ = arvo_core::secrets::delete_token(LEGACY_CREDENTIAL_ID);
    }
    Ok(())
}

fn credential(err: impl std::fmt::Display) -> SourceError {
    SourceError::Credential {
        vendor: "alpaca",
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::Env;

    #[test]
    fn an_environment_round_trips_through_its_id_and_owns_its_own_keychain_entry() {
        assert_eq!(Env::from_id("paper"), Some(Env::Paper));
        assert_eq!(Env::from_id("live"), Some(Env::Live));
        assert_eq!(Env::from_id("demo"), None);
        assert_eq!(Env::Paper.id(), "paper");
        // The two never share an entry: that is what keeps paper keys off the
        // live endpoint and the reverse.
        assert_ne!(Env::Paper.credential_id(), Env::Live.credential_id());
        assert_ne!(Env::Live.credential_id(), super::LEGACY_CREDENTIAL_ID);
    }
}

/// One authenticated GET with the data keys (paper first, then live).
pub(crate) async fn get(url: &str) -> Result<Value, SourceError> {
    let response = authorized(reqwest::Client::new().get(url), keys()?).await?;
    response.json().await.map_err(transport)
}

/// One authenticated GET with a specific environment's keys, for execution:
/// an order against the paper endpoint is signed with the paper pair, never
/// whichever pair a data call happened to prefer.
pub(crate) async fn get_env(env: Env, url: &str) -> Result<Value, SourceError> {
    let response = authorized(reqwest::Client::new().get(url), keys_for(env)?).await?;
    response.json().await.map_err(transport)
}

/// One authenticated POST for one environment, returning what the venue said.
///
/// Separate from [`get_env`] rather than a `method` parameter, because the two
/// differ in the one way that matters: a failed GET can be retried freely and
/// a failed POST cannot. Whoever calls this has to decide what a lost reply
/// means, and a shared helper that hid the verb would make that easy to forget.
pub(crate) async fn post_env(env: Env, url: &str, body: &Value) -> Result<Value, SourceError> {
    let response = authorized(reqwest::Client::new().post(url).json(body), keys_for(env)?).await?;
    response.json().await.map_err(transport)
}

/// One authenticated DELETE for one environment. Alpaca answers 204 with no
/// body.
pub(crate) async fn delete_env(env: Env, url: &str) -> Result<(), SourceError> {
    authorized(reqwest::Client::new().delete(url), keys_for(env)?).await.map(|_| ())
}

/// Signs a request with `keys`, sends it, and turns the status into the one
/// distinction a caller can act on.
///
/// Every call in this crate goes through here, so the rule about what counts as
/// a dead session is stated once: the keys are wrong, missing, or not entitled
/// to what was asked for — which is what a free plan gets for asking
/// `feed=sip`, and what a paper key gets for asking the live endpoint.
async fn authorized(request: reqwest::RequestBuilder, keys: Option<Keys>) -> Result<reqwest::Response, SourceError> {
    let keys = keys.ok_or(SourceError::NoSession { vendor: "alpaca" })?;
    let response = request
        .header("APCA-API-KEY-ID", &keys.key_id)
        .header("APCA-API-SECRET-KEY", &keys.secret)
        .send()
        .await
        .map_err(transport)?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(SourceError::NoSession { vendor: "alpaca" });
    }
    if !status.is_success() {
        // The body, not just the code. Alpaca says *why* it refused an order —
        // insufficient buying power, a symbol it will not trade, a duplicate
        // client id — and a bare "HTTP 422" would throw away the only part of
        // that a person can act on.
        let code = status.as_u16();
        let detail = response.text().await.unwrap_or_default();
        return Err(SourceError::Transport {
            vendor: "alpaca",
            detail: format!("HTTP {code}: {detail}"),
        });
    }
    Ok(response)
}

pub(crate) fn transport(err: reqwest::Error) -> SourceError {
    SourceError::Transport {
        vendor: "alpaca",
        detail: err.to_string(),
    }
}
