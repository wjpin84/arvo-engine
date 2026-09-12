//! Turning "what I sent" and "what the venue says" into fills.
//!
//! # Why this is not in the integrations
//!
//! Every broker answers a different shape, and every broker has its own word
//! for an order that is over — `cancelled`, `canceled`, `expired`, `voided`,
//! `done_for_day`. Reading that shape is the vendor's job and belongs in the
//! vendor's crate.
//!
//! What is *not* the vendor's job is what to do about it, and that is this
//! module. Which outstanding orders became fills, which ended without one,
//! which have rested long enough to be pulled back: the answers are the same
//! at every venue, and the rules are subtle enough that two copies would drift.
//! Four of them are easy to get wrong in ways that cost money rather than
//! failing loudly:
//!
//! - **A partial fill is not a fill.** Half the size the gate approved is not
//!   the position it approved, and booking it as complete tells the gate the
//!   account holds more than it does.
//! - **An order the venue has not listed yet is not gone.** Brokers are
//!   eventually consistent, and treating an absent order as dead drops a real
//!   one on the floor moments after placing it.
//! - **Staleness is measured from the signal, not the send.** What goes stale
//!   is the price the gate sized against, and that started ageing the moment
//!   the signal fired. Counting from the send hides every queue in front of it.
//! - **Nothing is forgotten until the venue agrees.** An order asked to cancel
//!   may have filled in the moment between the read and the request.
//!
//! # Why polling
//!
//! [`Executor::drain`](crate::Executor::drain) is polled rather than pushed,
//! for the reason recorded there: a broker API is polled anyway, and a channel
//! would mean every implementation owning a task whose lifetime nobody asked
//! about.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::NaiveDateTime;

use crate::{Execution, Order, OrderId, Side};

/// What was sent, kept until the venue says what became of it.
///
/// Held rather than re-read from the order record, because half of it is not
/// the venue's to know: `decision_price` and `decision_at` describe the moment
/// the *signal* fired, and the measurement this platform runs on is the gap
/// between that moment and the fill. An executor that reconstructed them from
/// the broker's reply would be timing the last hop and reporting it as the
/// latency.
#[derive(Debug, Clone)]
pub struct Sent {
    instrument: String,
    side: Side,
    quantity: f64,
    decision_price: f64,
    decision_at: NaiveDateTime,
    proposer: String,
}

impl From<&Order> for Sent {
    fn from(order: &Order) -> Self {
        Self {
            instrument: order.instrument.clone(),
            side: order.side,
            quantity: order.quantity,
            decision_price: order.decision_price,
            decision_at: order.decision_at,
            proposer: order.proposer.clone(),
        }
    }
}

/// What became of one order, in the vocabulary every venue's own words map on
/// to.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Still working — including partially filled, which is not a fill.
    Open,
    Filled { price: f64, at: NaiveDateTime },
    /// Cancelled, rejected, expired, voided: resolved without a fill.
    Gone,
}

/// What one poll of a venue concluded.
#[derive(Debug, Default)]
pub struct Outcome {
    pub executions: Vec<Execution>,
    /// Orders to stop watching, because the venue has finished with them.
    pub resolved: Vec<String>,
    /// Orders that have rested too long and should be taken back. **Not**
    /// resolved — a cancel is a request, and the answer decides.
    pub stale: Vec<String>,
}

/// Decides what each outstanding order became, given what the venue reported.
///
/// `now` is passed rather than read from a clock, so the staleness rule is
/// testable at all.
#[must_use]
pub fn reconcile(
    watching: &BTreeMap<String, Sent>,
    reported: &BTreeMap<String, State>,
    now: NaiveDateTime,
    max_order_age: chrono::Duration,
) -> Outcome {
    let mut outcome = Outcome::default();

    for (id, sent) in watching {
        match reported.get(id) {
            Some(State::Filled { price, at }) => {
                outcome.executions.push(Execution {
                    order: OrderId(id.clone()),
                    instrument: sent.instrument.clone(),
                    side: sent.side,
                    proposer: sent.proposer.clone(),
                    quantity: sent.quantity,
                    decision_price: sent.decision_price,
                    fill_price: *price,
                    decision_at: sent.decision_at,
                    filled_at: *at,
                });
                outcome.resolved.push(id.clone());
            }
            // Gone without a fill. Dropped rather than resent: the gate sized
            // this against a price that is now minutes old, and a retry would
            // be a new decision wearing an old one's timestamp.
            Some(State::Gone) => outcome.resolved.push(id.clone()),
            Some(State::Open) if now - sent.decision_at > max_order_age => {
                outcome.stale.push(id.clone());
            }
            // Still working, still young. Left to work.
            Some(State::Open) => {}
            // Acknowledged but not yet listed. Left alone.
            None => {}
        }
    }
    outcome
}

/// The orders a venue has acknowledged and not yet resolved.
///
/// Shared because the locking is the same everywhere and getting it wrong is
/// the same everywhere: a poisoned mutex must not panic an executor mid-session
/// — losing the record of an outstanding order is worse than any reason the
/// lock was poisoned — so every access degrades rather than unwraps.
#[derive(Debug, Default)]
pub struct Outstanding(Mutex<BTreeMap<String, Sent>>);

impl Outstanding {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an order the venue has acknowledged.
    pub fn watch(&self, id: &str, order: &Order) {
        if let Ok(mut held) = self.0.lock() {
            held.insert(id.to_owned(), Sent::from(order));
        }
    }

    /// A copy, so the async work that follows holds no lock.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, Sent> {
        self.0.lock().map(|held| held.clone()).unwrap_or_default()
    }

    pub fn forget(&self, ids: &[String]) {
        if let Ok(mut held) = self.0.lock() {
            for id in ids {
                held.remove(id);
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().map_or(0, |held| held.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};

    fn at(minute: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 9)
            .expect("valid")
            .and_time(NaiveTime::from_hms_opt(14, minute, second).expect("valid"))
    }

    fn order() -> Order {
        Order {
            instrument: "MSFT.RH".to_owned(),
            side: Side::Buy,
            quantity: 20.0,
            decision_price: 100.0,
            decision_at: at(30, 0),
            proposer: "technical".to_owned(),
        }
    }

    fn watching() -> BTreeMap<String, Sent> {
        [("ord-1".to_owned(), Sent::from(&order()))]
            .into_iter()
            .collect()
    }

    fn max_age() -> chrono::Duration {
        chrono::Duration::seconds(120)
    }

    #[test]
    fn a_fill_becomes_an_execution_that_remembers_the_decision_it_came_from() {
        // The point of holding `Sent`: the venue knows the fill and nothing
        // else. Signal-to-fill is the latency that costs money, and it cannot
        // be reconstructed from the order record.
        let reported = [(
            "ord-1".to_owned(),
            State::Filled {
                price: 100.08,
                at: at(30, 1),
            },
        )]
        .into_iter()
        .collect();

        let outcome = reconcile(&watching(), &reported, at(30, 2), max_age());
        assert_eq!(outcome.resolved, ["ord-1"]);
        assert!(outcome.stale.is_empty());

        let [execution] = &outcome.executions[..] else {
            panic!("one fill: {:?}", outcome.executions);
        };
        assert!((execution.decision_price - 100.0).abs() < 1e-9);
        assert!((execution.fill_price - 100.08).abs() < 1e-9);
        assert_eq!(execution.latency_ms(), 1_000);
        assert!((execution.slippage_bps() - 8.0).abs() < 1e-6);
        assert_eq!(execution.proposer, "technical");
    }

    #[test]
    fn an_order_that_ended_without_filling_is_stopped_being_watched() {
        // Left outstanding, it would be reported as an unfilled order on every
        // drain for the rest of the session — and `Divergence` counts those.
        let reported = [("ord-1".to_owned(), State::Gone)].into_iter().collect();
        let outcome = reconcile(&watching(), &reported, at(30, 2), max_age());

        assert_eq!(outcome.resolved, ["ord-1"]);
        assert!(outcome.executions.is_empty(), "gone is not filled");
        assert!(outcome.stale.is_empty(), "nothing left to cancel");
    }

    #[test]
    fn an_order_still_working_is_left_alone_until_it_has_rested_too_long() {
        let reported: BTreeMap<String, State> =
            [("ord-1".to_owned(), State::Open)].into_iter().collect();

        let young = reconcile(&watching(), &reported, at(30, 30), max_age());
        assert!(young.stale.is_empty(), "half a minute is not stale");
        assert!(young.resolved.is_empty(), "and it is still ours to watch");

        // Two minutes and a second after the *signal*, not after the send.
        let old = reconcile(&watching(), &reported, at(32, 1), max_age());
        assert_eq!(old.stale, ["ord-1"]);
        assert!(
            old.resolved.is_empty(),
            "not resolved until the cancel is acknowledged — it may have \
             filled in the meantime, and forgetting it would lose that fill"
        );
    }

    #[test]
    fn an_order_the_venue_has_not_listed_yet_is_neither_filled_nor_abandoned() {
        // Brokers are eventually consistent. Treating an absent order as gone
        // would drop a real order on the floor moments after placing it.
        let outcome = reconcile(&watching(), &BTreeMap::new(), at(30, 2), max_age());
        assert!(outcome.executions.is_empty());
        assert!(outcome.resolved.is_empty());
        assert!(outcome.stale.is_empty());
    }

    #[test]
    fn an_order_this_session_did_not_place_is_not_reconciled() {
        // A person trading by hand in the same account is not a fill of
        // anything proposed here.
        let reported = [("someone-elses".to_owned(), State::Gone)]
            .into_iter()
            .collect();
        let outcome = reconcile(&BTreeMap::new(), &reported, at(30, 2), max_age());
        assert!(outcome.executions.is_empty());
        assert!(outcome.resolved.is_empty());
    }

    #[test]
    fn watching_and_forgetting_are_what_the_count_of_unfilled_orders_reads() {
        let outstanding = Outstanding::new();
        assert!(outstanding.is_empty());

        outstanding.watch("ord-1", &order());
        outstanding.watch("ord-2", &order());
        assert_eq!(outstanding.len(), 2);
        assert_eq!(outstanding.snapshot().len(), 2);

        outstanding.forget(&["ord-1".to_owned()]);
        assert_eq!(outstanding.len(), 1);
        // Forgetting one nobody is watching is not an error.
        outstanding.forget(&["ord-1".to_owned(), "never-sent".to_owned()]);
        assert_eq!(outstanding.len(), 1);
    }
}
