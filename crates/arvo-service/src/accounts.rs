//! Accounts: a person's relationship with each vendor, as one list.
//!
//! An account is a vendor with a credential kind (none, a browser sign-in, or
//! a key pair), a state, and what it provides. Sources that share a credential
//! store share the account: Alpaca's four venues are one Alpaca account.
//!
//! # Why this is here rather than in the window
//!
//! The engine owns a venue session end to end (ADR-0028). It binds the
//! loopback listener, so it holds the PKCE verifier and is the only process
//! that can exchange the code; it therefore holds the refresh token, and
//! because providers rotate those, it must be the only process that refreshes.
//! The window's part is to open a URL.
//!
//! The flows themselves stay vendor code. Robinhood signs in through a browser
//! and OAuth; Alpaca is a key pair typed once.

use arvo_api::{AccountView, SubAccountView};

use crate::source::{self, Credential};
use crate::CommandError;

/// A sign-in that has been started: the URL to send someone to, and what is
/// needed to finish once they come back.
pub struct Pending {
    /// Where to send the person. The caller opens it; see ADR-0028 for why
    /// that is the one part the engine does not do.
    pub url: String,
    inner: arvo_oauth::Pending,
}

/// Written by hand, and deliberately thin: a `Pending` holds a PKCE verifier
/// and a registered client id, and neither belongs in a log line.
impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").field("url", &self.url).finish_non_exhaustive()
    }
}

fn kind(credential: Credential) -> &'static str {
    match credential {
        Credential::None => "none",
        Credential::SignIn => "sign_in",
        Credential::Keys => "keys",
    }
}

/// Every vendor, whether its credential is held, and what it provides.
pub async fn list() -> Vec<AccountView> {
    let mut out: Vec<AccountView> = Vec::new();
    for source in source::all() {
        let connected = source.connected().await.unwrap_or(false);
        match out.iter_mut().find(|account| account.id == source.vendor()) {
            Some(account) => {
                account.sources.push(source.label().to_owned());
                // Any source that can fetch means the credential is there.
                account.connected |= connected;
            }
            None => out.push(AccountView {
                id: source.vendor().to_owned(),
                label: source.vendor_label().to_owned(),
                credential: kind(source.credential()).to_owned(),
                connected,
                provides: source.provides().iter().map(|s| (*s).to_owned()).collect(),
                sources: vec![source.label().to_owned()],
                subaccounts: Vec::new(),
            }),
        }
    }
    // Alpaca is one login with two key pairs — paper and live — so its row
    // carries two sub-accounts, and its own `connected` is whether either
    // exists, which is all a data call needs.
    if let Some(alpaca) = out.iter_mut().find(|account| account.id == "alpaca") {
        let paper = source::alpaca::has(source::alpaca::Env::Paper).unwrap_or(false);
        let live = source::alpaca::has(source::alpaca::Env::Live).unwrap_or(false);
        alpaca.connected = paper || live;
        alpaca.subaccounts = vec![
            SubAccountView { id: "paper".to_owned(), label: "Paper".to_owned(), connected: paper, live: false },
            SubAccountView { id: "live".to_owned(), label: "Live".to_owned(), connected: live, live: true },
        ];
    }
    out
}

/// Starts a browser sign-in and answers with the URL to open.
///
/// Returns as soon as the URL exists. Waiting for the person would be a call
/// that outlives any sensible timeout, so the outcome arrives as an event
/// instead (ADR-0028 point 4).
///
/// # Errors
///
/// A vendor with no sign-in, or discovery and registration failed.
pub async fn begin_sign_in(vendor: &str) -> Result<Pending, CommandError> {
    if vendor != source::robinhood::SOURCE_ID {
        return Err(CommandError::Failed(format!("{vendor} has no sign-in; it uses keys or needs nothing")));
    }
    tracing::info!(vendor, "starting the sign-in");
    let inner = source::robinhood::begin_sign_in()
        .await
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    Ok(Pending { url: inner.url.clone(), inner })
}

/// Waits for the redirect, exchanges the code and stores the credential.
///
/// Bounded: a sign-in nobody completes expires rather than holding a port and
/// a verifier for as long as the engine runs.
///
/// # Errors
///
/// The person never came back, or the exchange was refused.
pub async fn finish_sign_in(pending: Pending) -> Result<(), CommandError> {
    source::robinhood::complete_sign_in(pending.inner)
        .await
        .map_err(|err| CommandError::Failed(err.to_string()))
}

/// Forgets a vendor's credential.
///
/// # Errors
///
/// A vendor that holds none, an Alpaca account that was not named, or the
/// keychain refused.
pub fn disconnect(vendor: &str, profile: Option<&str>) -> Result<(), CommandError> {
    match vendor {
        source::robinhood::SOURCE_ID => {
            source::robinhood::disconnect().map_err(|err| CommandError::Failed(err.to_string()))
        }
        "alpaca" => source::alpaca::forget(alpaca_env(profile)?).map_err(|err| CommandError::Failed(err.to_string())),
        other => Err(CommandError::Failed(format!("{other} holds no credential to remove"))),
    }
}

/// Stores a key pair for a vendor that uses one.
///
/// # Errors
///
/// A vendor that does not use keys, an account that was not named, half a
/// pair, or the keychain refused.
pub fn store_keys(vendor: &str, profile: Option<&str>, key_id: &str, secret: &str) -> Result<(), CommandError> {
    if vendor != "alpaca" {
        return Err(CommandError::Failed(format!("{vendor} does not use a key pair")));
    }
    let env = alpaca_env(profile)?;
    let keys = checked_keys(key_id, secret).map_err(CommandError::Failed)?;
    tracing::info!(vendor, account = env.id(), "storing the key pair");
    source::alpaca::store(env, &keys).map_err(|err| CommandError::Failed(err.to_string()))
}

fn alpaca_env(profile: Option<&str>) -> Result<source::alpaca::Env, CommandError> {
    let profile = profile.ok_or_else(|| CommandError::Failed("say which Alpaca account: paper or live".to_owned()))?;
    source::alpaca::Env::from_id(profile)
        .ok_or_else(|| CommandError::Failed(format!("{profile:?} is not an Alpaca account; use paper or live")))
}

fn checked_keys(key_id: &str, secret: &str) -> Result<source::alpaca::Keys, String> {
    let key_id = key_id.trim().to_owned();
    let secret = secret.trim().to_owned();
    if key_id.is_empty() || secret.is_empty() {
        return Err("both the key id and the secret are needed".to_owned());
    }
    Ok(source::alpaca::Keys { key_id, secret })
}

#[cfg(test)]
mod tests {
    use super::checked_keys;

    #[test]
    fn a_pasted_key_keeps_its_characters_and_loses_its_whitespace() {
        let keys = checked_keys("  PKTEST123  ", " secret ").expect("a real pair");
        assert_eq!(keys.key_id, "PKTEST123");
        assert_eq!(keys.secret, "secret");
    }

    #[test]
    fn half_a_credential_is_refused_rather_than_stored() {
        for (key_id, secret) in [("", "secret"), ("PKTEST123", ""), ("   ", " "), ("", "")] {
            let refused = checked_keys(key_id, secret).expect_err(&format!("{key_id:?}/{secret:?} is not a pair"));
            assert!(refused.contains("both"), "{refused}");
        }
    }

    #[test]
    fn a_vendor_without_a_sign_in_is_refused_before_any_browser_opens() {
        let refused = futures_lite_block_on(super::begin_sign_in("yahoo")).expect_err("yahoo has no sign-in");
        assert!(refused.to_string().contains("no sign-in"), "{refused}");
    }

    /// The smallest executor that will drive one future to completion: this
    /// crate has no async runtime of its own and needs none for a refusal
    /// that happens before the first await point.
    fn futures_lite_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut future = Box::pin(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }
}
