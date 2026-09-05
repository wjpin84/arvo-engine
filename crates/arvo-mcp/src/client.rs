//! The HTTP shell around [`crate::protocol`].
//!
//! Thin on purpose. Everything subtle about MCP lives in the framing, which is
//! pure and tested next door; what remains here is one POST, a couple of
//! headers, and an id counter.
//!
//! # Where OAuth attaches
//!
//! The token is handed in and this crate never negotiates one. That is the
//! seam: an authorization layer above obtains and refreshes a token, stores it
//! in the OS keychain via `arvo-core::secrets`, and hands the current value
//! here. Baking the flow in would tie a protocol client to one authorization
//! scheme and make both harder to test than either is alone.
//!
//! [`ClientError::Unauthorized`] is called out separately for exactly that
//! reason: it is the signal for the layer above to refresh and retry, and it
//! would be lost inside a generic "the request failed".

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::protocol::{self, ProtocolError, Tool, SESSION_HEADER};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not reach {endpoint}")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    /// The token is missing, expired or rejected.
    ///
    /// Distinguished from every other failure because it is the one the
    /// caller can actually do something about: refresh and try again.
    #[error("not authorized (HTTP {status}) — the token is missing, expired or rejected")]
    Unauthorized { status: u16 },
    #[error("server returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// A connection to one MCP endpoint.
pub struct McpClient {
    endpoint: String,
    token: String,
    http: reqwest::Client,
    next_id: AtomicU64,
    /// Issued by the server on initialize and echoed on every later request.
    /// Absent for servers that do not use sessions.
    session: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for McpClient {
    /// Never prints the token. A bearer token in a log or a panic message is
    /// a credential leak, and `derive(Debug)` on a struct holding one is the
    /// easiest way to cause it by accident.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl McpClient {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
            http: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            session: std::sync::Mutex::new(None),
        }
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Performs the handshake and returns the protocol version the server
    /// chose.
    ///
    /// Sends the `initialized` notification afterwards, which the spec
    /// requires before any other request; a server is within its rights to
    /// reject everything until it arrives.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] if the endpoint is unreachable, the token is
    /// rejected, or the handshake response is malformed.
    pub async fn connect(&self) -> Result<String, ClientError> {
        let result = self
            .send(protocol::initialize(
                self.id(),
                "arvo",
                env!("CARGO_PKG_VERSION"),
            ))
            .await?;
        let version = protocol::negotiated_version(&result)?;
        self.send_notification(protocol::initialized()).await?;
        Ok(version)
    }

    /// Everything the server offers.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] on transport, authorization or protocol failure.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, ClientError> {
        let result = self.send(protocol::tools_list(self.id())).await?;
        Ok(protocol::tools(&result)?)
    }

    /// Calls one tool and returns the text it produced.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] on transport or authorization failure, and
    /// [`ClientError::Protocol`] when the tool itself reports an error.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, ClientError> {
        let result = self
            .send(protocol::tools_call(self.id(), name, arguments))
            .await?;
        Ok(protocol::tool_text(&result)?)
    }

    /// Calls a tool and parses its text as JSON.
    ///
    /// Every server used here answers with a JSON document inside a text
    /// block, so this saves each caller the same two lines.
    ///
    /// # Errors
    ///
    /// As [`Self::call_tool`], plus [`ProtocolError::Malformed`] if the text
    /// is not JSON.
    pub async fn call_tool_json(&self, name: &str, arguments: Value) -> Result<Value, ClientError> {
        let text = self.call_tool(name, arguments).await?;
        Ok(serde_json::from_str(&text).map_err(ProtocolError::Malformed)?)
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, ClientError> {
        let mut request = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            // Both, because the server picks the framing and either is legal.
            .header("Accept", "application/json, text/event-stream")
            .json(body);

        if let Some(session) = self.session.lock().ok().and_then(|s| s.clone()) {
            request = request.header(SESSION_HEADER, session);
        }

        request
            .send()
            .await
            .map_err(|source| ClientError::Transport {
                endpoint: self.endpoint.clone(),
                source,
            })
    }

    async fn send(&self, body: Value) -> Result<Value, ClientError> {
        let response = self.post(&body).await?;
        let status = response.status();

        // Captured before the body is consumed.
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
        {
            if let Ok(mut held) = self.session.lock() {
                *held = Some(session);
            }
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        let text = response
            .text()
            .await
            .map_err(|source| ClientError::Transport {
                endpoint: self.endpoint.clone(),
                source,
            })?;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ClientError::Unauthorized {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                // Truncated: an HTML error page is not worth carrying whole
                // through an error chain, and the first part says enough.
                body: text.chars().take(400).collect(),
            });
        }

        Ok(protocol::parse(&text, content_type.as_deref())?)
    }

    /// A notification expects no reply, so an empty body is success rather
    /// than a malformed response.
    async fn send_notification(&self, body: Value) -> Result<(), ClientError> {
        let response = self.post(&body).await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ClientError::Unauthorized {
                status: status.as_u16(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_token() {
        // A bearer token in a log line or a panic message is a credential
        // leak, and a derived Debug is the easiest way to cause one.
        let client = McpClient::new("https://example.test/mcp", "super-secret-token");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains("example.test"), "the endpoint is useful");
    }

    #[test]
    fn request_ids_do_not_repeat() {
        // A repeated id makes two in-flight replies indistinguishable.
        let client = McpClient::new("https://example.test/mcp", "t");
        let ids: Vec<u64> = (0..5).map(|_| client.id()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "{ids:?}");
    }
}
