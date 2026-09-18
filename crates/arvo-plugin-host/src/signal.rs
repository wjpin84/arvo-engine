//! Signals a plugin process publishes (#163).
//!
//! The second caller of the signal namespace (#160): Arvo's own indicators
//! are one, a process Arvo spawns and owns is the other. It is not a new
//! trait — a regime classifier is a signal source that yields labels rather
//! than numbers, and it reaches the namespace through the same `Signals`
//! everything else does.
//!
//! # The invariant crosses the process boundary
//!
//! A predicate over an absent value is false, and that has to hold when the
//! value is absent because a process is down rather than because an
//! indicator is warming up. So:
//!
//! - A provider that answers with a point and no value publishes an absent
//!   signal, which satisfies nothing.
//! - A provider that cannot be reached publishes **nothing**, and its names
//!   leave the namespace. A name nobody published reads exactly like a value
//!   that is not there, so a rule over a signal from a dead process does not
//!   fire — and does not quietly keep firing on the last value it saw.
//!
//! The second is the one worth being careful about. Holding a last-known
//! value across a provider's death is the most natural thing to write and
//! the most dangerous: a rule gated on "the market is trending" would go on
//! believing it for as long as the classifier stayed down.

use std::collections::BTreeMap;

use arvo_data::{Signal, SignalName, Signals};

use crate::source::{with_token, Token};

/// Generated from `protos/arvo/signal/v1/signal.proto`.
pub mod v1 {
    tonic::include_proto!("arvo.signal.v1");
}

use v1::signals_client::SignalsClient;

/// The service name a manifest lists to say it publishes signals. A manifest
/// naming it is one the registry may call `Describe` on.
pub const SERVICE: &str = "arvo.signal.v1.Signals";

/// What a provider said it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: SignalName,
    pub description: String,
    /// Whether each value is computed from data available at that instant.
    /// A study may gate on a causal signal and never on a descriptive one.
    pub causal: bool,
    /// Why it is not causal, in the provider's own words. Empty when it is.
    pub because: String,
    /// What computes it, by name and version.
    pub engine: String,
}

/// A supervised process, as a publisher of signals.
#[derive(Debug, Clone)]
pub struct GrpcSignals {
    address: String,
    token: Option<Token>,
    declarations: Vec<Declaration>,
}

impl GrpcSignals {
    /// Asks a plugin what it publishes.
    ///
    /// # Errors
    ///
    /// The address not answering, or a declaration whose name is not a
    /// signal name — which is refused here rather than carried as a string
    /// that no rule could ever match.
    pub async fn discover(address: &str, token: Option<Token>) -> Result<Self, String> {
        let mut client = SignalsClient::connect(address.to_owned())
            .await
            .map_err(|err| err.to_string())?;
        let declared = client
            .describe(with_token(v1::Empty {}, token.as_ref()))
            .await
            .map_err(|status| status.to_string())?
            .into_inner();

        let mut declarations = Vec::with_capacity(declared.signals.len());
        for signal in declared.signals {
            let name = SignalName::new(signal.name).map_err(|err| err.to_string())?;
            declarations.push(Declaration {
                name,
                description: signal.description,
                causal: signal.causal,
                because: signal.because,
                engine: signal.engine,
            });
        }
        Ok(Self {
            address: address.to_owned(),
            token,
            declarations,
        })
    }

    #[must_use]
    pub fn declarations(&self) -> &[Declaration] {
        &self.declarations
    }

    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// What this provider says right now.
    ///
    /// An error is not an empty answer with a different colour: a caller that
    /// cannot reach a provider publishes nothing for it, which is what
    /// [`publish_into`] does.
    ///
    /// # Errors
    ///
    /// The address not answering, or a value whose name is not a signal name.
    pub async fn latest(&self) -> Result<Vec<Signal>, String> {
        let mut client = SignalsClient::connect(self.address.clone())
            .await
            .map_err(|err| err.to_string())?;
        let values = client
            .latest(with_token(v1::Empty {}, self.token.as_ref()))
            .await
            .map_err(|status| status.to_string())?
            .into_inner();

        let mut signals = Vec::with_capacity(values.values.len());
        for value in values.values {
            let Ok(name) = SignalName::new(value.name) else {
                // A name that is not a name cannot be read by any rule, so
                // carrying it would be carrying something unusable. Dropped
                // rather than failing the whole poll: one malformed entry
                // should not take a provider's other signals with it.
                continue;
            };
            let Ok(at) = value.at.parse() else {
                continue;
            };
            // `Signal::new` takes the `Option` as it arrived — absence is
            // preserved across the boundary rather than defaulted here.
            signals.push(Signal::new(name, at, value.value));
        }
        Ok(signals)
    }
}

/// Builds the namespace from every provider, fresh.
///
/// Fresh is the point. The namespace is rebuilt from what answered *this*
/// time, so a provider that has gone away takes its names with it and every
/// rule over them reads absent. Nothing is carried forward from the last
/// poll, because a value that is no longer being published is not a value.
///
/// A provider that fails is logged and contributes nothing. It is not an
/// error to the caller: one classifier being down is a signal being absent,
/// which is a state every reader already handles.
pub async fn publish_into(providers: &[GrpcSignals]) -> Signals {
    let mut namespace = Signals::new();
    for provider in providers {
        match provider.latest().await {
            Ok(signals) => {
                for signal in signals {
                    namespace.publish(signal);
                }
            }
            Err(why) => {
                tracing::warn!(address = %provider.address, why, "a signal provider said nothing");
            }
        }
    }
    namespace
}

/// Every declaration across every provider, by name.
///
/// Used to say what *could* be published, which is not the same question as
/// what is. A name declared and not currently published is a signal that
/// exists and has nothing to say.
#[must_use]
pub fn declared(providers: &[GrpcSignals]) -> BTreeMap<SignalName, Declaration> {
    providers
        .iter()
        .flat_map(|provider| provider.declarations.iter().cloned())
        .map(|declaration| (declaration.name.clone(), declaration))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio_stream::wrappers::TcpListenerStream;

    /// A provider that says whatever the test tells it to.
    #[derive(Default, Clone)]
    struct Fake(Arc<Mutex<Vec<v1::Value>>>);

    #[tonic::async_trait]
    impl v1::signals_server::Signals for Fake {
        async fn describe(
            &self,
            _request: tonic::Request<v1::Empty>,
        ) -> Result<tonic::Response<v1::Declarations>, tonic::Status> {
            Ok(tonic::Response::new(v1::Declarations {
                signals: vec![v1::Declaration {
                    name: "regime.trend".to_owned(),
                    description: "What kind of market this is".to_owned(),
                    causal: true,
                    because: String::new(),
                    engine: "price-behavior-v2.1-frozen".to_owned(),
                }],
            }))
        }

        async fn latest(
            &self,
            _request: tonic::Request<v1::Empty>,
        ) -> Result<tonic::Response<v1::Values>, tonic::Status> {
            Ok(tonic::Response::new(v1::Values {
                values: self.0.lock().expect("not poisoned").clone(),
            }))
        }
    }

    /// Starts one, answering until the returned handle is aborted.
    async fn provider(saying: Vec<v1::Value>) -> (String, Fake, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let address = format!("http://{}", listener.local_addr().expect("addressed"));
        let fake = Fake(Arc::new(Mutex::new(saying)));
        let serving = fake.clone();
        let handle = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(v1::signals_server::SignalsServer::new(serving))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        assert!(until(&address, true).await, "the provider never came up");
        (address, fake, handle)
    }

    /// Waits until the address is answering, or until it is not.
    ///
    /// Not a fixed sleep. A sleep long enough on an idle machine is not long
    /// enough on a machine running the rest of the suite beside it, and a
    /// test that passes alone and fails in the run is worse than one that
    /// fails always.
    async fn until(address: &str, answering: bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if SignalsClient::connect(address.to_owned()).await.is_ok() == answering {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    fn value(name: &str, at: &str, value: Option<f64>) -> v1::Value {
        v1::Value {
            name: name.to_owned(),
            at: at.to_owned(),
            value,
        }
    }

    #[tokio::test]
    async fn a_declaration_says_whether_a_study_could_ever_gate_on_it() {
        let (address, _fake, handle) = provider(vec![]).await;
        let provider = GrpcSignals::discover(&address, None).await.expect("discovers");

        let declared = provider.declarations();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].name.as_str(), "regime.trend");
        assert!(declared[0].causal, "the provider says it is computed from what the bar knew");
        assert_eq!(declared[0].engine, "price-behavior-v2.1-frozen");
        handle.abort();
    }

    #[tokio::test]
    async fn an_absent_value_survives_the_process_boundary() {
        let (address, fake, handle) = provider(vec![
            value("regime.trend", "2026-09-18T14:30:00", None),
            value("regime.strength", "2026-09-18T14:30:00", Some(0.8)),
        ])
        .await;
        let provider = GrpcSignals::discover(&address, None).await.expect("discovers");

        let namespace = publish_into(std::slice::from_ref(&provider)).await;
        // The point arrived, and it is still absent: not zero, and not
        // dropped into a namespace that would then have no opinion at all.
        assert_eq!(namespace.value("regime.trend"), None);
        assert!(!namespace.satisfies("regime.trend", |v| v <= 0.0), "absent satisfies nothing");
        assert_eq!(namespace.value("regime.strength"), Some(0.8));

        // A value that stops being published is absent from the next poll,
        // rather than carried forward.
        *fake.0.lock().expect("not poisoned") = vec![value("regime.trend", "2026-09-18T14:45:00", Some(1.0))];
        let namespace = publish_into(std::slice::from_ref(&provider)).await;
        assert_eq!(namespace.value("regime.trend"), Some(1.0));
        assert_eq!(namespace.value("regime.strength"), None, "it was not published this time");
        handle.abort();
    }

    #[tokio::test]
    async fn a_provider_that_is_gone_publishes_nothing_rather_than_what_it_last_said() {
        let (address, _fake, handle) = provider(vec![value("regime.trend", "2026-09-18T14:30:00", Some(1.0))]).await;
        let provider = GrpcSignals::discover(&address, None).await.expect("discovers");
        assert!(
            publish_into(std::slice::from_ref(&provider)).await.satisfies("regime.trend", |v| v > 0.0),
            "while it is up, it answers"
        );

        // The dangerous case: a rule gated on "the market is trending" must
        // not go on believing it for as long as the classifier stays down.
        handle.abort();
        assert!(until(&address, false).await, "the provider never went away");

        let namespace = publish_into(std::slice::from_ref(&provider)).await;
        assert_eq!(namespace.value("regime.trend"), None, "a dead provider says nothing");
        assert!(!namespace.satisfies("regime.trend", |v| v > 0.0));
        assert!(namespace.is_empty(), "and nothing is carried forward from the last poll");
    }
}
