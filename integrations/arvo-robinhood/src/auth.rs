//! Signing in, staying signed in, and handing out a connected client.
//!
//! OAuth 2.1 with PKCE and dynamic client registration, via [`arvo_oauth`], and
//! a client id plus token pair in the OS keychain. The access token is
//! refreshed on use when it has run out and the refreshed pair is written back,
//! so an unattended sync survives an expiry.
//!
//! Also the two error mappings, because both failures start here: an MCP
//! `Unauthorized` and an OAuth `Denied` are dead sessions, and everything else
//! either way is the network wearing an auth error's clothing.

use arvo_data::source::SourceError;

use crate::source::SOURCE_ID;

/// Robinhood's MCP endpoint.
const ENDPOINT: &str = "https://agent.robinhood.com/mcp/trading";

/// What is kept in the keychain once someone has signed in.
///
/// The client id travels with the tokens rather than being registered afresh
/// each time: dynamic registration works on every sign-in, but it leaves one
/// abandoned client on the server per attempt, and reusing the id is what makes
/// a re-authorisation look like the same app coming back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Connection {
    client_id: String,
    tokens: arvo_oauth::Tokens,
}

/// An MCP client with a fresh token, already through `initialize`.
pub(crate) async fn connect() -> Result<arvo_mcp::McpClient, SourceError> {
    let token = access_token().await?;
    let client = arvo_mcp::McpClient::new(ENDPOINT, token);
    client.connect().await.map_err(transport)?;
    Ok(client)
}

/// An MCP failure as a [`SourceError`].
///
/// The one classification that matters happens here: `Unauthorized` is a dead
/// session and everything else is the network. Doing it at the boundary is why
/// no caller has to know what an `arvo_mcp::ClientError` is.
pub(crate) fn transport(err: arvo_mcp::ClientError) -> SourceError {
    match err {
        arvo_mcp::ClientError::Unauthorized { .. } => SourceError::NoSession { vendor: SOURCE_ID },
        other => SourceError::Transport {
            vendor: SOURCE_ID,
            detail: other.to_string(),
        },
    }
}

/// An OAuth failure as a [`SourceError`].
///
/// `Denied` means the authorization server refused the grant, which a person
/// fixes by signing in again. Every other OAuth error is a refresh that could
/// not *reach* the server — a network problem wearing an auth error's clothing,
/// and reporting it as a dead session would sign someone out over flaky wifi.
fn oauth(err: arvo_oauth::OAuthError) -> SourceError {
    match err {
        arvo_oauth::OAuthError::Denied { .. } => SourceError::NoSession { vendor: SOURCE_ID },
        other => SourceError::Transport {
            vendor: SOURCE_ID,
            detail: other.to_string(),
        },
    }
}

/// Whether a connection is stored, without revealing anything about it.
///
/// A getter for the token itself would put a bearer credential on the wire to
/// the web view for no reason a UI actually has — the only thing a UI needs to
/// know is whether to offer sign-in or sign-out.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain cannot be read.
pub fn is_connected() -> Result<bool, SourceError> {
    Ok(stored()?.is_some())
}

/// Forgets the stored connection.
///
/// Local only. Whether the tokens are also revoked at the server is the
/// server's business and there is no revocation endpoint in its metadata, so
/// this does not claim to have done more than it did.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the delete.
pub fn disconnect() -> Result<(), SourceError> {
    arvo_core::secrets::delete_token(SOURCE_ID).map_err(credential)
}

/// Starts a sign-in: discovers the server, registers, and returns the URL to
/// open along with the pending flow.
///
/// Returning the URL rather than opening it keeps `arvo-oauth` free of a
/// browser dependency, and keeps *this* function testable up to the point a
/// person is actually required.
///
/// # Errors
///
/// Returns [`SourceError`] if discovery or registration fails.
pub async fn begin_sign_in() -> Result<arvo_oauth::Pending, SourceError> {
    // Reuse the client id from a previous sign-in when there is one, even if
    // its tokens have since expired or been revoked — the registration is still
    // good, and re-registering would strand it.
    let client_id = stored().ok().flatten().map(|held| held.client_id);
    arvo_oauth::begin(&arvo_oauth::AuthConfig {
        resource: ENDPOINT.to_owned(),
        client_name: "Arvo".to_owned(),
        // Empty: take whatever the server advertises rather than asserting a
        // scope name that may not exist. Asking for one it does not know is a
        // refusal, and asking for more than it offers is worse.
        scopes: Vec::new(),
        client_id,
    })
    .await
    .map_err(oauth)
}

/// Waits for the browser redirect and stores what comes back.
///
/// # Errors
///
/// Returns [`SourceError`] if consent is refused or nobody finishes.
pub async fn complete_sign_in(pending: arvo_oauth::Pending) -> Result<(), SourceError> {
    let client_id = pending.client_id.clone();
    let tokens = pending
        .finish(arvo_oauth::DEFAULT_TIMEOUT)
        .await
        .map_err(oauth)?;
    store(&Connection { client_id, tokens })
}

/// A usable access token, refreshed if the stored one has run out.
///
/// The refreshed pair is written back before it is used. Refreshing without
/// storing works exactly once and then asks for a browser again, which is the
/// sort of bug that only shows up an hour after someone stops watching.
///
/// # Errors
///
/// Returns [`SourceError::NoSession`] if nobody has signed in or the refresh
/// token has been revoked — in which case a person has to sign in again.
pub async fn access_token() -> Result<String, SourceError> {
    let held = stored()?.ok_or(SourceError::NoSession { vendor: SOURCE_ID })?;
    if !held.tokens.is_expired(std::time::SystemTime::now()) {
        return Ok(held.tokens.access_token);
    }

    let refresh_token = held
        .tokens
        .refresh_token
        .as_deref()
        .ok_or(SourceError::NoSession { vendor: SOURCE_ID })?;

    // The token endpoint again from discovery rather than remembered: an
    // endpoint cached at sign-in and moved since would fail every refresh with
    // no way to recover but a reinstall.
    let metadata_url = arvo_oauth::metadata_url(ENDPOINT).map_err(oauth)?;
    let document: serde_json::Value = reqwest::Client::new()
        .get(metadata_url)
        .send()
        .await
        .map_err(|err| oauth(arvo_oauth::OAuthError::Http(err)))?
        .json()
        .await
        .map_err(|err| oauth(arvo_oauth::OAuthError::Http(err)))?;
    let metadata = arvo_oauth::ServerMetadata::parse(&document).map_err(oauth)?;

    let tokens = arvo_oauth::refresh(
        &metadata.token_endpoint,
        &held.client_id,
        refresh_token,
        ENDPOINT,
    )
    .await
    .map_err(oauth)?;

    let access = tokens.access_token.clone();
    store(&Connection {
        client_id: held.client_id,
        tokens,
    })?;
    Ok(access)
}

fn credential(err: impl std::fmt::Display) -> SourceError {
    SourceError::Credential {
        vendor: SOURCE_ID,
        detail: err.to_string(),
    }
}

fn stored() -> Result<Option<Connection>, SourceError> {
    let Some(text) = arvo_core::secrets::get_token(SOURCE_ID).map_err(credential)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(credential)
}

fn store(connection: &Connection) -> Result<(), SourceError> {
    let text = serde_json::to_string(connection).map_err(credential)?;
    arvo_core::secrets::store_token(SOURCE_ID, &text).map_err(credential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_token_is_a_dead_session_and_a_reset_connection_is_not() {
        // The distinction the whole error mapping exists for: signing someone
        // out over flaky wifi would make them re-authorize in a browser to fix
        // a network blip.
        assert!(transport(arvo_mcp::ClientError::Unauthorized { status: 401 }).needs_sign_in());
        assert!(!oauth(arvo_oauth::OAuthError::Metadata("token_endpoint"))
            .needs_sign_in());
        assert!(oauth(arvo_oauth::OAuthError::Denied {
            error: "invalid_grant".into(),
            description: None,
        })
        .needs_sign_in());
    }
}
