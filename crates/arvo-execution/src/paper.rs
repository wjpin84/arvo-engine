//! Filling orders against prices that actually arrived.
//!
//! # The one rule
//!
//! **It never fills at the price you asked for.** An order rests until a price
//! for its instrument is observed, and fills at that price.
//!
//! That is the entire value of the thing. A paper executor that filled at the
//! decision price would be a backtest with extra steps: it would confirm the
//! cost assumption by construction, agree with every stored finding, and be
//! incapable of discovering the one class of error nobody else here can catch —
//! that the assumption about the world was wrong. `arvo_research::reconcile`
//! catches an engine contradicting itself. Nothing catches an engine being
//! consistently, quietly optimistic. This does.
//!
//! # What it does not simulate
//!
//! Partial fills, queue position, book depth, and the fact that a large order
//! moves the price it is filling against. All of those make real slippage worse
//! than this measures, so the number it produces is a **floor**: the market is
//! at least this expensive, and the backtest is at least this optimistic.
//!
//! Stating that is the point of the section. A paper figure quoted as *the*
//! cost, when it silently ignores impact, is a precise understatement — and a
//! precise understatement is worse than no number, because it invites belief.
//!
//! ponytail: no partial fills, no book. Add depth simulation when order size is
//! a meaningful fraction of displayed volume; until then size is small enough
//! that queue position dominates and neither is modelled honestly by guessing.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::NaiveDateTime;

use crate::{Execution, ExecutionError, Executor, Order, OrderId};

/// An order waiting for a price.
#[derive(Debug, Clone)]
struct Resting {
    id: OrderId,
    order: Order,
}

/// Fills against observed prices, and remembers what each one really cost.
///
/// Interior mutability because [`Executor`] takes `&self`: a broker client is
/// shared and does not need exclusive access to send an order, and forcing
/// `&mut` here would push a lock into every caller instead of keeping it in the
/// one place that needs it.
pub struct PaperExecutor {
    venue: String,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    resting: Vec<Resting>,
    completed: Vec<Execution>,
    /// Last observed price per instrument, so an order arriving between ticks
    /// is not stranded until the next one.
    ///
    /// Deliberately *not* used to fill on arrival — see [`PaperExecutor::on_price`].
    last: HashMap<String, (f64, NaiveDateTime)>,
}

impl PaperExecutor {
    #[must_use]
    pub fn new(venue: impl Into<String>) -> Self {
        Self {
            venue: venue.into(),
            state: Mutex::new(State::default()),
        }
    }

    /// Feeds one observed price, filling anything resting in that instrument.
    ///
    /// # Why an order does not fill against the last price it already knew
    ///
    /// Because that price is in the past. Filling a freshly submitted order
    /// against a tick that arrived before the decision was made would produce
    /// negative latency and flattering slippage — it is look-ahead, in the one
    /// place the platform has been most careful to avoid it. An order rests
    /// until the *next* price arrives, which is what happens in a market.
    ///
    /// The cached last price exists only so a caller can ask what an instrument
    /// was worth; nothing fills against it.
    pub fn on_price(&self, instrument: &str, price: f64, at: NaiveDateTime) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            // A panic in a fill loop must not silently stop a session from
            // trading; the state it protects is plain data and is not left
            // half-updated by a panic between two field writes.
            poisoned.into_inner()
        });
        state.last.insert(instrument.to_owned(), (price, at));

        let (fillable, waiting): (Vec<Resting>, Vec<Resting>) = state
            .resting
            .drain(..)
            .partition(|resting| resting.order.instrument == instrument);
        state.resting = waiting;

        for resting in fillable {
            state.completed.push(Execution {
                order: resting.id,
                instrument: resting.order.instrument,
                side: resting.order.side,
                proposer: resting.order.proposer,
                quantity: resting.order.quantity,
                decision_price: resting.order.decision_price,
                fill_price: price,
                decision_at: resting.order.decision_at,
                filled_at: at,
            });
        }
    }

    /// Prices this has seen, for a caller that wants to mark a position.
    #[must_use]
    pub fn last_price(&self, instrument: &str) -> Option<f64> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last
            .get(instrument)
            .map(|(price, _)| *price)
    }

    /// Orders still waiting for a price.
    #[must_use]
    pub fn resting(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resting
            .len()
    }
}

#[async_trait::async_trait]
impl Executor for PaperExecutor {
    fn venue(&self) -> &str {
        &self.venue
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
        if order.quantity <= 0.0 {
            return Err(ExecutionError::Rejected {
                venue: self.venue.clone(),
                reason: format!("quantity {} is not tradeable", order.quantity),
            });
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_id += 1;
        let id = OrderId(format!("paper-{}", state.next_id));
        state.resting.push(Resting {
            id: id.clone(),
            order: order.clone(),
        });
        Ok(id)
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let completed = std::mem::take(&mut state.completed);
        Ok((completed, state.resting.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Divergence, Session, Side};
    use arvo_research::risk::{Proposal, RiskGate};
    use arvo_research::RiskModel;
    use chrono::{NaiveDate, NaiveTime};

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 9).expect("valid")
    }

    fn at(second: u32, milli: u32) -> NaiveDateTime {
        day().and_time(NaiveTime::from_hms_milli_opt(14, 30, second, milli).expect("valid"))
    }

    fn order(price: f64) -> Order {
        Order {
            instrument: "MSFT.RH".to_owned(),
            side: Side::Buy,
            quantity: 10.0,
            decision_price: price,
            decision_at: at(0, 0),
            proposer: "technical".to_owned(),
        }
    }

    #[tokio::test]
    async fn an_order_fills_at_the_next_observed_price_not_the_one_it_asked_for() {
        // The rule the whole crate rests on. Filling at the decision price
        // would confirm the cost assumption by construction and measure
        // nothing at all.
        let paper = PaperExecutor::new("paper");
        paper.submit(&order(100.0)).await.expect("accepted");
        paper.on_price("MSFT.RH", 100.07, at(0, 200));

        let (executions, resting) = paper.drain().await.expect("drain");
        assert_eq!(resting, 0);
        assert_eq!(executions.len(), 1);
        assert!((executions[0].fill_price - 100.07).abs() < 1e-9);
        assert!((executions[0].slippage_bps() - 7.0).abs() < 1e-9);
        assert_eq!(executions[0].latency_ms(), 200);
    }

    #[tokio::test]
    async fn an_order_does_not_fill_against_a_price_that_arrived_before_it() {
        // Look-ahead, in the one place this platform is most careful about it:
        // it would produce negative latency and flattering slippage.
        let paper = PaperExecutor::new("paper");
        paper.on_price("MSFT.RH", 99.0, at(0, 0));
        paper.submit(&order(100.0)).await.expect("accepted");

        let (executions, resting) = paper.drain().await.expect("drain");
        assert!(executions.is_empty(), "nothing may fill on a stale tick");
        assert_eq!(resting, 1, "it waits for the next price");
    }

    #[tokio::test]
    async fn an_order_in_another_instrument_is_left_resting() {
        let paper = PaperExecutor::new("paper");
        paper.submit(&order(100.0)).await.expect("accepted");
        paper.on_price("AAPL.RH", 200.0, at(0, 100));

        let (executions, resting) = paper.drain().await.expect("drain");
        assert!(executions.is_empty());
        assert_eq!(resting, 1);
    }

    #[tokio::test]
    async fn draining_twice_does_not_report_the_same_fill_twice() {
        let paper = PaperExecutor::new("paper");
        paper.submit(&order(100.0)).await.expect("accepted");
        paper.on_price("MSFT.RH", 100.05, at(0, 100));

        assert_eq!(paper.drain().await.expect("first").0.len(), 1);
        assert_eq!(paper.drain().await.expect("second").0.len(), 0);
    }

    #[tokio::test]
    async fn a_zero_quantity_order_is_refused_rather_than_resting_forever() {
        let paper = PaperExecutor::new("paper");
        let mut empty = order(100.0);
        empty.quantity = 0.0;
        assert!(matches!(
            paper.submit(&empty).await,
            Err(ExecutionError::Rejected { .. })
        ));
    }

    fn proposal(instrument: &str, price: f64, signalled: NaiveDateTime) -> Proposal {
        Proposal {
            instrument: instrument.to_owned(),
            proposer: "technical".to_owned(),
            signalled_at: signalled,
            reference_price: price,
            stop_distance: Some(2.0),
            desired_quantity: None,
        }
    }

    fn session() -> Session<PaperExecutor> {
        Session::new(
            RiskGate::new(RiskModel::default(), 2_000.0, day()),
            PaperExecutor::new("paper"),
        )
        .against_assumed_slippage_bps(1.0)
    }

    #[tokio::test]
    async fn the_loop_runs_proposal_to_fill_to_position() {
        let mut session = session();
        let proposal = proposal("MSFT.RH", 100.0, at(0, 0));

        let id = session
            .propose(&proposal, at(0, 50), None)
            .await
            .expect("venue is fine")
            .expect("the gate approved it");
        assert!(id.0.starts_with("paper-"));

        // Nothing is held until something fills.
        assert!(session.gate().positions().is_empty());

        session.executor().on_price("MSFT.RH", 100.08, at(0, 300));
        assert_eq!(session.settle().await.expect("settle"), 1);

        let held = session
            .gate()
            .positions()
            .get("MSFT.RH")
            .expect("the fill became a position");
        assert!((held.quantity - 20.0).abs() < 1e-9);
        assert!((held.entry - 100.08).abs() < 1e-9, "booked at the fill price");
    }

    #[tokio::test]
    async fn a_refused_proposal_never_reaches_the_venue() {
        // The invariant: nothing gets to an executor except through the gate.
        let mut session = session();
        let stale = proposal("MSFT.RH", 100.0, at(0, 0));

        let sent = session
            .propose(&stale, at(1, 0), None)
            .await
            .expect("venue is fine");
        assert!(sent.is_none(), "a one-second-old signal is stale");
        assert_eq!(session.refusals().len(), 1);
        assert_eq!(session.executor().resting(), 0);
    }

    #[tokio::test]
    async fn two_proposers_into_one_account_cannot_both_get_filled() {
        // The hazard the gate exists for, end to end through a venue.
        let mut session = session();
        let first = proposal("MSFT.RH", 100.0, at(0, 0));
        session
            .propose(&first, at(0, 10), None)
            .await
            .expect("venue")
            .expect("approved");
        session.executor().on_price("MSFT.RH", 100.0, at(0, 100));
        session.settle().await.expect("settle");

        let mut second = proposal("MSFT.RH", 100.0, at(1, 0));
        second.proposer = "alert".to_owned();
        let sent = session
            .propose(&second, at(1, 10), None)
            .await
            .expect("venue");

        assert!(sent.is_none(), "the account already holds this");
        assert_eq!(session.executor().resting(), 0);
    }

    #[tokio::test]
    async fn a_session_measures_how_optimistic_the_backtest_was() {
        // Gap #2's deliverable: the first number here produced by comparing an
        // assumption to something that happened.
        let mut session = session();

        for (n, (instrument, fill)) in [("MSFT.RH", 100.04), ("AAPL.RH", 100.06)]
            .into_iter()
            .enumerate()
        {
            #[expect(clippy::cast_possible_truncation, reason = "two iterations")]
            let second = n as u32;
            let proposal = proposal(instrument, 100.0, at(second, 0));
            session
                .propose(&proposal, at(second, 10), None)
                .await
                .expect("venue")
                .expect("approved");
            session
                .executor()
                .on_price(instrument, fill, at(second, 200));
            session.settle().await.expect("settle");
        }

        let divergence: Divergence = session.divergence();
        assert_eq!(divergence.fills, 2);
        assert!((divergence.mean_slippage_bps - 5.0).abs() < 1e-9);
        assert!(
            (divergence.optimism_bps().expect("assumed 1bp") - 4.0).abs() < 1e-9,
            "the cost model assumed 1bp and the market charged 5"
        );
    }

    #[tokio::test]
    async fn a_halted_account_can_still_flatten() {
        // The property that matters most in this file. Every gate check asks
        // whether to take risk; none may stop you shedding it. A daily loss
        // limit that refused the exit would convert a bad day into an uncapped
        // one.
        let mut session = Session::new(
            RiskGate::new(
                RiskModel {
                    max_drawdown: Some(0.05),
                    ..RiskModel::default()
                },
                2_000.0,
                day(),
            ),
            PaperExecutor::new("paper"),
        );

        let entry = proposal("MSFT.RH", 100.0, at(0, 0));
        session
            .propose(&entry, at(0, 10), None)
            .await
            .expect("venue")
            .expect("approved");
        session.executor().on_price("MSFT.RH", 100.0, at(0, 100));
        session.settle().await.expect("settle");

        // The account falls far enough to halt.
        session.mark(1_800.0);
        assert!(session.gate().halted().is_some(), "the halt is in force");

        // A new entry is refused...
        let another = proposal("AAPL.RH", 50.0, at(1, 0));
        assert!(session
            .propose(&another, at(1, 10), None)
            .await
            .expect("venue")
            .is_none());

        // ...and the exit is not.
        let exit = session
            .close("MSFT.RH", 90.0, at(1, 0))
            .await
            .expect("venue");
        assert!(exit.is_some(), "a halted account must be able to flatten");
    }

    #[tokio::test]
    async fn a_round_trip_books_its_profit_and_loss_against_the_fill_price() {
        let mut session = session();
        let entry = proposal("MSFT.RH", 100.0, at(0, 0));
        session
            .propose(&entry, at(0, 10), None)
            .await
            .expect("venue")
            .expect("approved");
        session.executor().on_price("MSFT.RH", 100.0, at(0, 100));
        session.settle().await.expect("settle");
        let opening_equity = session.gate().equity();

        session.close("MSFT.RH", 105.0, at(1, 0)).await.expect("venue");
        session.executor().on_price("MSFT.RH", 105.0, at(1, 100));
        session.settle().await.expect("settle");

        assert!(
            session.gate().positions().is_empty(),
            "the position is gone"
        );
        // 20 shares bought at 100, sold at 105.
        assert!(
            (session.gate().equity() - opening_equity - 100.0).abs() < 1e-9,
            "equity moved by the round trip's realised P&L"
        );
    }

    #[tokio::test]
    async fn closing_something_not_held_is_not_an_error() {
        // Already flat is the state the caller asked for.
        let mut session = session();
        assert!(session
            .close("MSFT.RH", 100.0, at(0, 0))
            .await
            .expect("venue")
            .is_none());
    }

    #[tokio::test]
    async fn an_approved_order_that_never_fills_is_reported_as_unfilled() {
        // A backtest assumes every order fills, so this has no counterpart.
        let mut session = session();
        let proposal = proposal("MSFT.RH", 100.0, at(0, 0));
        session
            .propose(&proposal, at(0, 10), None)
            .await
            .expect("venue")
            .expect("approved");

        assert_eq!(session.settle().await.expect("settle"), 0);
        let divergence = session.divergence();
        assert_eq!(divergence.fills, 0);
        assert_eq!(divergence.unfilled, 1);
    }
}
