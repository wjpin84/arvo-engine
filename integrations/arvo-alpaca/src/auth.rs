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

/// Where the key pair lives in the OS keychain.
pub const CREDENTIAL_ID: &str = "alpaca";

/// An Alpaca key pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Keys {
    pub key_id: String,
    pub secret: String,
}

/// The stored key pair, or the environment, or nothing.
///
/// Keychain first: the environment is a convenience for headless runs, not the
/// intended home for a credential.
pub(crate) fn keys() -> Result<Option<Keys>, SourceError> {
    if let Some(text) = arvo_core::secrets::get_token(CREDENTIAL_ID).map_err(credential)? {
        return serde_json::from_str(&text).map(Some).map_err(credential);
    }
    let (Ok(key_id), Ok(secret)) = (
        std::env::var("APCA_API_KEY_ID"),
        std::env::var("APCA_API_SECRET_KEY"),
    ) else {
        return Ok(None);
    };
    Ok(Some(Keys { key_id, secret }))
}

/// Stores a key pair.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the write.
pub fn store(keys: &Keys) -> Result<(), SourceError> {
    let text = serde_json::to_string(keys).map_err(credential)?;
    arvo_core::secrets::store_token(CREDENTIAL_ID, &text).map_err(credential)
}

/// Forgets the stored key pair.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the delete.
pub fn forget() -> Result<(), SourceError> {
    arvo_core::secrets::delete_token(CREDENTIAL_ID).map_err(credential)
}

fn credential(err: impl std::fmt::Display) -> SourceError {
    SourceError::Credential {
        vendor: "alpaca",
        detail: err.to_string(),
    }
}

/// One authenticated GET.
pub(crate) async fn get(url: &str) -> Result<Value, SourceError> {
    let response = authorized(reqwest::Client::new().get(url)).await?;
    response.json().await.map_err(transport)
}

/// One authenticated POST, returning what the venue said.
///
/// Separate from [`get`] rather than a `method` parameter, because the two
/// differ in the one way that matters: a failed GET can be retried freely and
/// a failed POST cannot. Whoever calls this has to decide what a lost reply
/// means, and a shared helper that hid the verb would make that easy to forget.
pub(crate) async fn post(url: &str, body: &Value) -> Result<Value, SourceError> {
    let response = authorized(reqwest::Client::new().post(url).json(body)).await?;
    response.json().await.map_err(transport)
}

/// One authenticated DELETE. Alpaca answers 204 with no body.
pub(crate) async fn delete(url: &str) -> Result<(), SourceError> {
    authorized(reqwest::Client::new().delete(url)).await.map(|_| ())
}

/// Signs a request, sends it, and turns the status into the one distinction a
/// caller can act on.
///
/// Every call in this crate goes through here, so the rule about what counts as
/// a dead session is stated once: the keys are wrong, missing, or not entitled
/// to what was asked for — which is what a free plan gets for asking
/// `feed=sip`, and what a paper key gets for asking the live endpoint.
async fn authorized(request: reqwest::RequestBuilder) -> Result<reqwest::Response, SourceError> {
    let keys = keys()?.ok_or(SourceError::NoSession { vendor: "alpaca" })?;
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
