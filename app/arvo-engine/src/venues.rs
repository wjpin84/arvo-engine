//! The real venues: the brokers this build can reach.
//!
//! The one implementation of [`arvo_trading::Venues`] that sends orders to
//! somewhere that can fill them. It lives here rather than in `arvo-trading`
//! because the broker clients and the source registry do, and because keeping
//! it out of that crate is what lets a test put a fake in its place — see
//! `arvo_trading::venues` for the two live bugs that bought this arrangement.

use arvo_execution::Executor;
use arvo_trading::Venues;

/// Alpaca's paper endpoint, Alpaca live, and a Robinhood account by its last
/// four digits.
pub struct Brokers;

/// The names [`Brokers`] serves. `robinhood-<last4>` is a pattern, not a name:
/// the account is resolved from the digits when the session starts.
pub const NAMES: &[&str] = &["alpaca-paper", "alpaca-live", "robinhood-<last4>"];

#[async_trait::async_trait]
impl Venues for Brokers {
    fn serves(&self, executor: &str) -> bool {
        matches!(executor, "alpaca-paper" | "alpaca-live")
            // The digits are required: "robinhood" alone names no account, and
            // guessing one is not a thing to do with real money.
            || executor.strip_prefix("robinhood-").is_some_and(|last4| {
                last4.len() >= 4 && last4.chars().all(|c| c.is_ascii_digit())
            })
    }

    fn names(&self) -> Vec<String> {
        NAMES.iter().map(|name| (*name).to_owned()).collect()
    }

    fn source(&self, venue: &str) -> Result<Box<dyn arvo_data::source::Source>, String> {
        arvo_service::source::all()
            .into_iter()
            .find(|source| source.venue() == venue)
            .ok_or_else(|| format!("no source serves venue {venue}"))
    }

    async fn executor(&self, executor: &str) -> Result<Box<dyn Executor>, String> {
        match executor {
            "alpaca-paper" => Ok(Box::new(arvo_alpaca::AlpacaExecutor::paper())),
            "alpaca-live" => Ok(Box::new(arvo_alpaca::AlpacaExecutor::live())),
            robinhood if robinhood.starts_with("robinhood-") => {
                let last4 = &robinhood["robinhood-".len()..];
                let account = arvo_robinhood::Robinhood
                    .holdings()
                    .await
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .map(|held| held.account_number)
                    .find(|number| number.ends_with(last4))
                    .ok_or_else(|| format!("no Robinhood account ends in {last4}"))?;
                Ok(Box::new(arvo_robinhood::RobinhoodExecutor::new(account)))
            }
            other => Err(format!("no executor {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_robinhood_name_without_account_digits_is_not_a_venue() {
        let brokers = Brokers;
        assert!(brokers.serves("alpaca-paper"));
        assert!(brokers.serves("alpaca-live"));
        assert!(brokers.serves("robinhood-8591"));
        // Each of these would otherwise reach whichever account came back
        // first, which is not a thing to do with real money.
        assert!(!brokers.serves("robinhood"), "which account?");
        assert!(!brokers.serves("robinhood-"), "which account?");
        assert!(!brokers.serves("robinhood-abcd"), "not digits");
        assert!(!brokers.serves("robinhood-85"), "too few digits to identify one");
        assert!(!brokers.serves("alpaca"), "paper or live?");
    }
}
