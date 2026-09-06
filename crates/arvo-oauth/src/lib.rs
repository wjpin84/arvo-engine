//! OAuth 2.1 for a desktop app, with PKCE and dynamic client registration.
//!
//! # Why this is its own crate
//!
//! It is not part of being an MCP client. `arvo-mcp` is a transport — JSON-RPC
//! framing over HTTP — and it takes a bearer token as an input. Welding the
//! authorization flow into it would make both harder to test, because this
//! half needs a TCP listener, a browser and a clock, and none of that belongs
//! behind a `call_tool`.
//!
//! It is not part of `arvo-core` either. That crate is deliberately
//! dependency-light and everything depends on it; giving it an HTTP stack, a
//! hash and a random number generator to serve one feature would be paid for
//! by every other crate in the graph.
//!
//! Nothing here mentions Robinhood, or MCP. Every endpoint comes from the
//! server's own discovery document.
//!
//! # The shape of the flow, and why it is split in two
//!
//! [`begin`] does discovery, registration, binds a loopback listener and
//! returns the URL to send someone to. The caller opens it. [`Pending::finish`]
//! waits for the redirect and exchanges the code.
//!
//! Split there deliberately: opening a browser is a platform concern with a
//! different answer on every OS, and a crate that did it would need a
//! dependency it has no other use for and could not be tested without one.
//!
//! # What makes a public client safe
//!
//! There is no client secret. There is nowhere in a distributed binary to keep
//! one, so a desktop app that claims to have a secret is lying about its own
//! security. PKCE is the replacement: the client proves it is the same party
//! that started the flow by presenting the pre-image of a hash it sent up
//! front, so an attacker who intercepts the authorization code cannot redeem
//! it.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// How early a token is treated as expired.
///
/// A token that expires during the request that used it fails in a way that
/// looks like a permissions problem. Refreshing a minute early costs nothing.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// How long to wait for someone to finish in the browser.
///
/// Long enough to find a password manager and pass a second factor, short
/// enough that an abandoned flow does not hold a socket for the life of the
/// process.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("{0} is not a URL this can derive a discovery address from: {1}")]
    BadResource(String, String),
    #[error("the authorization server's metadata is missing {0}")]
    Metadata(&'static str),
    #[error("this server does not support {0}, which a desktop client cannot work without")]
    Unsupported(&'static str),
    #[error("could not reach the authorization server: {0}")]
    Http(#[from] reqwest::Error),
    #[error("the authorization server said: {error}{}", .description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default())]
    Denied {
        error: String,
        description: Option<String>,
    },
    #[error("could not listen for the redirect on localhost: {0}")]
    Listener(#[source] std::io::Error),
    #[error("nobody completed the sign-in within {0:?}")]
    TimedOut(Duration),
    #[error("the redirect came back with the wrong state; treating it as forged and stopping")]
    StateMismatch,
    #[error("the token response was missing {0}")]
    Token(&'static str),
}

// --------------------------------------------------------------- discovery ---

/// Where an authorization server publishes its metadata.
///
/// RFC 8414's **suffix** form: the well-known segment goes after the host and
/// the resource's path goes after that, so `https://host/mcp/trading` is
/// described at `https://host/.well-known/oauth-authorization-server/mcp/trading`.
///
/// Not the nested form (`https://host/mcp/trading/.well-known/...`). That is
/// the intuitive guess, it is what a reader expects, and against this server
/// it 404s — which is worth a test rather than a comment alone.
///
/// # Errors
///
/// Returns [`OAuthError::BadResource`] if `resource` is not an absolute URL.
pub fn metadata_url(resource: &str) -> Result<String, OAuthError> {
    let parsed = url::Url::parse(resource)
        .map_err(|err| OAuthError::BadResource(resource.to_owned(), err.to_string()))?;
    let mut out = parsed.clone();
    out.set_query(None);
    out.set_fragment(None);

    let path = parsed.path().trim_matches('/');
    let suffix = if path.is_empty() {
        "/.well-known/oauth-authorization-server".to_owned()
    } else {
        format!("/.well-known/oauth-authorization-server/{path}")
    };
    out.set_path(&suffix);
    Ok(out.to_string())
}

/// What the authorization server says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// `None` when the server has no dynamic registration, which for a
    /// desktop client means somebody has to register it by hand.
    pub registration_endpoint: Option<String>,
    pub scopes: Vec<String>,
}

impl ServerMetadata {
    /// # Errors
    ///
    /// Returns [`OAuthError::Metadata`] if a required endpoint is absent, or
    /// [`OAuthError::Unsupported`] if the server will not accept S256 PKCE.
    pub fn parse(document: &Value) -> Result<Self, OAuthError> {
        let field = |name: &'static str| -> Result<String, OAuthError> {
            document
                .get(name)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or(OAuthError::Metadata(name))
        };

        // Checked rather than assumed. Without S256 the only alternative is
        // `plain`, which is PKCE in name only — the verifier travels in the
        // clear on the first leg and protects nothing.
        let methods = strings(document, "code_challenge_methods_supported");
        if !methods.is_empty() && !methods.iter().any(|method| method == "S256") {
            return Err(OAuthError::Unsupported("PKCE with S256"));
        }

        Ok(Self {
            authorization_endpoint: field("authorization_endpoint")?,
            token_endpoint: field("token_endpoint")?,
            registration_endpoint: document
                .get("registration_endpoint")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            scopes: strings(document, "scopes_supported"),
        })
    }
}

fn strings(document: &Value, name: &str) -> Vec<String> {
    document
        .get(name)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// -------------------------------------------------------------------- pkce ---

/// A PKCE verifier and the challenge derived from it.
///
/// The verifier never leaves this process until the token request; only its
/// SHA-256 goes out with the authorization request. That is the whole
/// mechanism, and it is what lets a client with no secret redeem a code
/// safely.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    #[must_use]
    pub fn generate() -> Self {
        // 32 bytes base64url-encoded is 43 characters, the shortest the spec
        // allows and already 256 bits of entropy.
        let verifier = random_token();
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            challenge: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ------------------------------------------------------------------ tokens ---

/// What the token endpoint handed back.
///
/// Serializable because it has to survive a restart — a flow that made someone
/// sign in again on every launch would be one nobody leaves connected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// `None` when the server issues no refresh token, which means the next
    /// expiry needs a person again.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Absolute, not a duration. A duration is only meaningful next to the
    /// instant it was received, and that instant is exactly what is lost when
    /// this is written to a keychain and read back a day later.
    #[serde(default)]
    pub expires_at: Option<SystemTime>,
}

impl Tokens {
    /// # Errors
    ///
    /// Returns [`OAuthError::Token`] if no access token came back, or
    /// [`OAuthError::Denied`] if the server returned an error object.
    pub fn parse(document: &Value, now: SystemTime) -> Result<Self, OAuthError> {
        if let Some(error) = document.get("error").and_then(Value::as_str) {
            return Err(OAuthError::Denied {
                error: error.to_owned(),
                description: document
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }

        Ok(Self {
            access_token: document
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or(OAuthError::Token("access_token"))?
                .to_owned(),
            refresh_token: document
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            expires_at: document
                .get("expires_in")
                .and_then(Value::as_u64)
                .map(|seconds| now + Duration::from_secs(seconds)),
        })
    }

    /// Whether this needs refreshing before use.
    ///
    /// A token with no stated expiry is treated as usable: the server chose
    /// not to say, and guessing an expiry would throw away a working token.
    #[must_use]
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.expires_at
            .is_some_and(|at| now + EXPIRY_MARGIN >= at)
    }
}

// ------------------------------------------------------------------- flow ---

/// What a client needs to say about itself to start.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The protected resource being authorized for — the MCP endpoint itself.
    /// Used to find the discovery document and sent as the `resource`
    /// parameter so the token is minted for this audience and not another.
    pub resource: String,
    /// Shown to the user on the consent screen.
    pub client_name: String,
    /// Left empty to ask for whatever the server advertises.
    pub scopes: Vec<String>,
    /// Reuses a client id from a previous registration. Registering afresh on
    /// every sign-in works but litters the server with one client per attempt.
    pub client_id: Option<String>,
}

/// A flow waiting for someone to finish in their browser.
#[derive(Debug)]
pub struct Pending {
    /// Send the user here.
    pub url: String,
    /// The registered client, worth keeping so the next sign-in can skip
    /// registration.
    pub client_id: String,
    metadata: ServerMetadata,
    listener: tokio::net::TcpListener,
    redirect_uri: String,
    state: String,
    pkce: Pkce,
    resource: String,
}

/// Discovers the server, registers this client, and prepares the redirect.
///
/// The loopback socket is bound *before* registration on purpose: the port is
/// whatever the OS hands out, and the redirect URI containing it has to be the
/// one registered or the server will reject it.
///
/// # Errors
///
/// Returns [`OAuthError`] if discovery, registration or binding fails.
pub async fn begin(config: &AuthConfig) -> Result<Pending, OAuthError> {
    let http = reqwest::Client::new();

    let document: Value = http
        .get(metadata_url(&config.resource)?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let metadata = ServerMetadata::parse(&document)?;

    // 127.0.0.1 rather than `localhost`: the name can resolve to ::1 or to
    // something a hosts file decided, and the redirect has to arrive at this
    // process.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(OAuthError::Listener)?;
    let port = listener
        .local_addr()
        .map_err(OAuthError::Listener)?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let scopes = if config.scopes.is_empty() {
        metadata.scopes.clone()
    } else {
        config.scopes.clone()
    };

    let client_id = match &config.client_id {
        Some(existing) => existing.clone(),
        None => {
            let endpoint = metadata
                .registration_endpoint
                .as_ref()
                .ok_or(OAuthError::Unsupported("dynamic client registration"))?;
            register(&http, endpoint, config, &redirect_uri, &scopes).await?
        }
    };

    let pkce = Pkce::generate();
    let state = random_token();
    let url = authorization_url(
        &metadata.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &state,
        &pkce.challenge,
        &scopes,
        &config.resource,
    );

    Ok(Pending {
        url,
        client_id,
        metadata,
        listener,
        redirect_uri,
        state,
        pkce,
        resource: config.resource.clone(),
    })
}

impl Pending {
    /// Waits for the browser redirect and exchanges the code for tokens.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthError::TimedOut`] if nobody finishes,
    /// [`OAuthError::StateMismatch`] if the redirect does not carry the state
    /// this flow issued, or [`OAuthError::Denied`] if consent was refused.
    pub async fn finish(self, timeout: Duration) -> Result<Tokens, OAuthError> {
        let code = tokio::time::timeout(timeout, self.wait_for_code())
            .await
            .map_err(|_| OAuthError::TimedOut(timeout))??;

        let document: Value = post_form(
            &self.metadata.token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("client_id", self.client_id.as_str()),
                ("code_verifier", self.pkce.verifier.as_str()),
                ("resource", self.resource.as_str()),
            ],
        )
        .await?;

        Tokens::parse(&document, SystemTime::now())
    }

    /// Accepts connections until one carries this flow's state.
    ///
    /// A loop rather than a single accept: browsers open speculative
    /// connections and fetch `/favicon.ico` off the redirect page, and taking
    /// the first thing that connects as the answer would abandon the flow.
    async fn wait_for_code(&self) -> Result<String, OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(OAuthError::Listener)?;
            let Some(line) = read_request_line(&mut stream).await else {
                continue;
            };

            match Callback::parse(&line) {
                None => {
                    respond(&mut stream, "Waiting for the sign-in to finish…").await;
                }
                Some(Callback::Error { error, description }) => {
                    respond(&mut stream, "Sign-in was refused. You can close this tab.").await;
                    return Err(OAuthError::Denied { error, description });
                }
                Some(Callback::Code { code, state }) => {
                    if state != self.state {
                        // Not this flow's redirect. Refusing rather than
                        // proceeding is the entire purpose of `state`.
                        respond(&mut stream, "That request did not come from this app.").await;
                        return Err(OAuthError::StateMismatch);
                    }
                    // Deliberately not "Connected". The token exchange has not
                    // happened yet and can still fail, and a browser tab
                    // claiming success while the app reports failure is worse
                    // than either message alone.
                    respond(
                        &mut stream,
                        "Signed in. Returning to Arvo — you can close this tab.",
                    )
                    .await;
                    return Ok(code);
                }
            }
        }
    }
}

/// Trades a refresh token for a fresh access token.
///
/// # Errors
///
/// Returns [`OAuthError::Denied`] if the refresh token has been revoked or has
/// itself expired, which means a person has to sign in again.
pub async fn refresh(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    resource: &str,
) -> Result<Tokens, OAuthError> {
    let document: Value = post_form(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("resource", resource),
        ],
    )
    .await?;

    let mut tokens = Tokens::parse(&document, SystemTime::now())?;
    // Servers may rotate the refresh token or may not send one back at all.
    // Dropping the old one in the second case would turn a working connection
    // into one that needs a person at the next expiry.
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(tokens)
}

/// Posts a form-encoded body and reads JSON back.
///
/// The response body is read whatever the status says: OAuth returns its
/// failures as JSON with a 400, and that body is the only thing that
/// distinguishes an expired grant from a rejected redirect from a parameter
/// the server does not accept. Throwing it away for the status code would
/// leave "400 Bad Request" as the whole diagnosis.
///
/// Hand-rolled rather than reqwest's `form`, which is behind a feature this
/// crate would otherwise not need; the encoder is already here for building
/// the authorization URL.
///
/// The response is read without `error_for_status`: OAuth returns its failures
/// as a JSON body with a 400, and that body says *which* failure — an expired
/// grant reads very differently from a wrong client id. Throwing it away for
/// the status code alone would leave "400 Bad Request" as the whole diagnosis.
async fn post_form(endpoint: &str, fields: &[(&str, &str)]) -> Result<Value, OAuthError> {
    // Encoded and finished in its own scope. `Serializer` holds a `dyn Fn`
    // that is neither `Send` nor `Sync`, and a future that merely has one in
    // scope across an await is itself not `Send` — which a Tauri command
    // requires, several layers up, with an error naming none of this.
    let body = {
        let mut encoder = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in fields {
            encoder.append_pair(name, value);
        }
        encoder.finish()
    };

    let response = reqwest::Client::new()
        .post(endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;
    serde_json::from_str(&text).map_or_else(
        // Not JSON at all — an HTML error page, a proxy, a gateway. Carrying
        // the first of it through is the difference between a diagnosis and a
        // shrug.
        |_| {
            Err(OAuthError::Denied {
                error: format!("HTTP {status}"),
                description: Some(text.chars().take(300).collect()),
            })
        },
        Ok,
    )
}

async fn register(
    http: &reqwest::Client,
    endpoint: &str,
    config: &AuthConfig,
    redirect_uri: &str,
    scopes: &[String],
) -> Result<String, OAuthError> {
    let document: Value = http
        .post(endpoint)
        .json(&json!({
            "client_name": config.client_name,
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            // A public client. Saying so is not a limitation to apologise for:
            // there is nowhere in a distributed binary to keep a secret, and
            // claiming one would be a false statement about this app's
            // security posture.
            "token_endpoint_auth_method": "none",
            "scope": scopes.join(" "),
        }))
        .send()
        .await?
        .json()
        .await?;

    if let Some(error) = document.get("error").and_then(Value::as_str) {
        return Err(OAuthError::Denied {
            error: error.to_owned(),
            description: document
                .get("error_description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        });
    }

    document
        .get("client_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or(OAuthError::Token("client_id"))
}

fn authorization_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    scopes: &[String],
    resource: &str,
) -> String {
    let mut url = endpoint.to_owned();
    url.push(if endpoint.contains('?') { '&' } else { '?' });
    url.push_str(
        &url::form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("scope", &scopes.join(" "))
            // RFC 8707. Says which API the token is for, so a token minted
            // here cannot be replayed against a different service the same
            // authorization server protects.
            .append_pair("resource", resource)
            .finish(),
    );
    url
}

// --------------------------------------------------------------- callback ---

/// What came back on the redirect.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Code {
        code: String,
        state: String,
    },
    Error {
        error: String,
        description: Option<String>,
    },
}

impl Callback {
    /// Reads an HTTP request line: `GET /callback?code=…&state=… HTTP/1.1`.
    ///
    /// `None` for anything that is not a redirect — a browser's favicon
    /// request, a probe, a health check. Those are ordinary noise on a
    /// listening socket, not failures.
    fn parse(request_line: &str) -> Option<Self> {
        let target = request_line.split_whitespace().nth(1)?;
        // A relative target has no base, so give it one purely to reuse a
        // tested query parser rather than writing a fourth one.
        let parsed = url::Url::parse("http://127.0.0.1").ok()?.join(target).ok()?;
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if let Some(error) = query.get("error") {
            return Some(Self::Error {
                error: error.clone(),
                description: query.get("error_description").cloned(),
            });
        }
        Some(Self::Code {
            code: query.get("code")?.clone(),
            state: query.get("state").cloned().unwrap_or_default(),
        })
    }
}

async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt as _;

    // The request line only. A cap because this socket is reachable by
    // anything on the machine, and an unbounded read from an untrusted peer is
    // a way to be handed a gigabyte.
    let mut buffer = [0u8; 4096];
    let read = stream.read(&mut buffer).await.ok()?;
    let text = String::from_utf8_lossy(&buffer[..read]);
    text.lines().next().map(ToOwned::to_owned)
}

async fn respond(stream: &mut tokio::net::TcpStream, message: &str) {
    use tokio::io::AsyncWriteExt as _;

    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Arvo</title>\
         <body style=\"font:16px system-ui;padding:3rem\">{message}</body>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Best effort: the flow's outcome is already decided by this point, and a
    // browser that closed the tab early must not fail the sign-in.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_uses_the_suffix_form_not_the_nested_one() {
        // The nested form is the intuitive guess and it 404s against a real
        // server. Read off RFC 8414 and confirmed against the live endpoint.
        assert_eq!(
            metadata_url("https://agent.robinhood.com/mcp/trading").expect("absolute"),
            "https://agent.robinhood.com/.well-known/oauth-authorization-server/mcp/trading"
        );
    }

    #[test]
    fn a_resource_with_no_path_gets_the_bare_well_known_address() {
        assert_eq!(
            metadata_url("https://example.com").expect("absolute"),
            "https://example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn a_resource_that_is_not_a_url_is_refused() {
        assert!(metadata_url("agent.robinhood.com/mcp").is_err());
    }

    #[test]
    fn metadata_needs_the_endpoints_it_claims_to_have() {
        let missing = json!({ "authorization_endpoint": "https://example.com/authorize" });
        assert!(matches!(
            ServerMetadata::parse(&missing),
            Err(OAuthError::Metadata("token_endpoint"))
        ));
    }

    #[test]
    fn a_server_without_s256_is_refused_rather_than_downgraded() {
        // The only alternative is `plain`, where the verifier travels in the
        // clear on the first leg and protects nothing. Falling back to it
        // would leave a public client with no protection at all while still
        // looking like it had PKCE.
        let plain = json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "code_challenge_methods_supported": ["plain"],
        });
        assert!(matches!(
            ServerMetadata::parse(&plain),
            Err(OAuthError::Unsupported(_))
        ));
    }

    #[test]
    fn robinhoods_own_metadata_parses() {
        // The shape observed on the live discovery document.
        // Copied from the live response, field for field.
        let document = json!({
            "authorization_endpoint": "https://robinhood.com/oauth",
            "code_challenge_methods_supported": ["S256"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "issuer": "https://agent.robinhood.com/mcp/trading",
            "registration_endpoint": "https://agent.robinhood.com/oauth/trading/register",
            "response_types_supported": ["code"],
            "scopes_supported": ["internal"],
            "token_endpoint": "https://api.robinhood.com/oauth2/token/",
            "token_endpoint_auth_methods_supported": ["none"],
        });
        let metadata = ServerMetadata::parse(&document).expect("parses");
        assert_eq!(metadata.token_endpoint, "https://api.robinhood.com/oauth2/token/");
        assert_eq!(
            metadata.registration_endpoint.as_deref(),
            Some("https://agent.robinhood.com/oauth/trading/register")
        );
        assert_eq!(metadata.scopes, vec!["internal".to_owned()]);
    }

    #[test]
    fn a_challenge_is_the_hash_of_the_verifier_not_the_verifier() {
        let pkce = Pkce::generate();
        assert_ne!(pkce.verifier, pkce.challenge);
        assert_eq!(pkce.verifier.len(), 43, "32 bytes, base64url, unpadded");

        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn two_flows_do_not_share_a_verifier() {
        assert_ne!(Pkce::generate().verifier, Pkce::generate().verifier);
    }

    #[test]
    fn an_authorization_url_carries_the_challenge_and_never_the_verifier() {
        let pkce = Pkce::generate();
        let url = authorization_url(
            "https://robinhood.com/oauth",
            "client-1",
            "http://127.0.0.1:5173/callback",
            "state-1",
            &pkce.challenge,
            &["internal".to_owned()],
            "https://agent.robinhood.com/mcp/trading",
        );
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
        assert!(
            !url.contains(&pkce.verifier),
            "the verifier is the secret; only its hash may travel"
        );
        // The redirect and resource must survive percent-encoding intact.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5173%2Fcallback"));
        assert!(url.contains("resource=https%3A%2F%2Fagent.robinhood.com%2Fmcp%2Ftrading"));
    }

    #[test]
    fn an_endpoint_that_already_has_a_query_keeps_it() {
        let url = authorization_url(
            "https://example.com/oauth?tenant=eu",
            "c",
            "http://127.0.0.1:1/callback",
            "s",
            "ch",
            &[],
            "https://example.com/api",
        );
        assert!(url.contains("tenant=eu"), "{url}");
        assert!(url.contains("?tenant=eu&response_type=code"), "{url}");
    }

    #[test]
    fn a_redirect_yields_its_code_and_state() {
        let callback = Callback::parse("GET /callback?code=abc&state=xyz HTTP/1.1");
        assert_eq!(
            callback,
            Some(Callback::Code {
                code: "abc".to_owned(),
                state: "xyz".to_owned()
            })
        );
    }

    #[test]
    fn a_refusal_is_read_as_a_refusal_not_a_missing_code() {
        let callback = Callback::parse(
            "GET /callback?error=access_denied&error_description=User%20said%20no HTTP/1.1",
        );
        assert_eq!(
            callback,
            Some(Callback::Error {
                error: "access_denied".to_owned(),
                description: Some("User said no".to_owned())
            })
        );
    }

    #[test]
    fn browser_noise_is_ignored_rather_than_treated_as_the_answer() {
        // Browsers fetch a favicon off the redirect page and open speculative
        // connections. Taking the first thing that arrives as the redirect
        // would abandon the flow every time.
        assert_eq!(Callback::parse("GET /favicon.ico HTTP/1.1"), None);
        assert_eq!(Callback::parse("GET /callback HTTP/1.1"), None);
        assert_eq!(Callback::parse("garbage"), None);
    }

    #[test]
    fn a_token_response_becomes_an_absolute_expiry() {
        // Absolute, because a duration means nothing next to a keychain entry
        // read back a day later.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let tokens = Tokens::parse(
            &json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_in": 3600,
                "token_type": "Bearer",
            }),
            now,
        )
        .expect("parses");

        assert_eq!(tokens.expires_at, Some(now + Duration::from_secs(3600)));
        assert!(!tokens.is_expired(now));
        assert!(tokens.is_expired(now + Duration::from_secs(3600)));
    }

    #[test]
    fn a_token_is_refreshed_before_it_actually_expires() {
        // Expiring mid-request looks like a permissions failure, which is a
        // much harder thing to diagnose than an early refresh.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let tokens = Tokens::parse(&json!({ "access_token": "at", "expires_in": 100 }), now)
            .expect("parses");
        assert!(
            tokens.is_expired(now + Duration::from_secs(50)),
            "a token with under a minute left is treated as spent"
        );
    }

    #[test]
    fn a_token_with_no_stated_expiry_is_usable() {
        let tokens = Tokens::parse(&json!({ "access_token": "at" }), SystemTime::now())
            .expect("parses");
        assert_eq!(tokens.expires_at, None);
        assert!(!tokens.is_expired(SystemTime::now()));
    }

    #[test]
    fn an_error_response_is_an_error_not_a_missing_field() {
        let err = Tokens::parse(
            &json!({ "error": "invalid_grant", "error_description": "expired" }),
            SystemTime::now(),
        )
        .expect_err("the server refused");
        assert!(
            matches!(err, OAuthError::Denied { ref error, .. } if error == "invalid_grant"),
            "{err}"
        );
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn tokens_survive_a_round_trip_through_storage() {
        // They live in the OS keychain as a string, so this is the actual
        // path, not a formality.
        let tokens = Tokens {
            access_token: "at".to_owned(),
            refresh_token: Some("rt".to_owned()),
            expires_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        };
        let text = serde_json::to_string(&tokens).expect("serialises");
        let read: Tokens = serde_json::from_str(&text).expect("parses");
        assert_eq!(read.access_token, tokens.access_token);
        assert_eq!(read.refresh_token, tokens.refresh_token);
        assert_eq!(read.expires_at, tokens.expires_at);
    }
}
