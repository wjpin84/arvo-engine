//! Sending orders to Alpaca, behind the gate.
//!
//! # Paper first, and not as a rehearsal
//!
//! `https://paper-api.alpaca.markets` is the same API as the live one against
//! simulated money. That makes it the only place the parts of trading that are
//! not arithmetic get exercised at all: an order rejected for buying power, a
//! fill that arrives in two pieces, an order placed at 16:05 that sits until
//! the next open, a symbol the venue will not trade. None of those appear in a
//! backtest, and none appear in `arvo_execution::paper::PaperExecutor` either,
//! because it fills whatever it is handed against whatever price arrives next.
//!
//! So the two do not overlap and neither retires the other:
//!
//! | | answers |
//! |---|---|
//! | this, on paper | does the *integration* work — auth, lifecycle, rejections |
//! | `PaperExecutor` | was the *cost assumption* right, against real prices |
//!
//! Pointing `Divergence` at Alpaca paper fills would measure Alpaca's fill
//! simulator, not the market. It is the wrong instrument for that question and
//! the only instrument for this one.
//!
//! # What keeps this behind the gate
//!
//! Shape, not configuration. This is an [`Executor`], and the only public path
//! to [`Executor::submit`] is `arvo_execution::Session::propose` — so nothing
//! reaches `POST /v2/orders` without having been sized by the risk gate and
//! checked against the halt, the day-trade budget, the loss limit and the
//! staleness window.
//!
//! # Stale orders
//!
//! Alpaca queues a market order placed outside regular hours rather than
//! refusing it, exactly as Robinhood does, and for a day-trading system a
//! signal from 16:05 filling at 09:30 tomorrow is the worst available outcome.
//! The guard is the same one and for the same reason: no market calendar, but
//! anything unfilled [`DEFAULT_MAX_ORDER_AGE_SECS`] after the *signal* is
//! cancelled and reported unfilled. That covers after-hours, holidays, halts
//! and thin books with one mechanism.

use std::collections::BTreeMap;

use arvo_execution::poll::{reconcile, Outstanding, State};
use arvo_execution::{Execution, ExecutionError, Executor, Order, OrderId, Side};
use serde_json::{json, Value};

use crate::auth;

/// The paper endpoint: the real trading API, simulated money.
const PAPER: &str = "https://paper-api.alpaca.markets";
/// The live endpoint. Real money.
const LIVE: &str = "https://api.alpaca.markets";

/// How long an unfilled order may rest before it is cancelled.
///
/// Two minutes, matching the broker executor. A market order in a liquid name
/// fills in under a second, so anything still resting is queued for a session
/// that has not started, halted, or in a book that cannot absorb the size —
/// none of which improve with waiting, while the price the decision was made
/// at recedes the whole time.
pub const DEFAULT_MAX_ORDER_AGE_SECS: i64 = 120;

/// Places orders through Alpaca's trading API.
///
/// Built by [`AlpacaExecutor::paper`] or [`AlpacaExecutor::live`]. Which one is
/// part of what this *is* rather than a setting on it — the same argument that
/// makes [`crate::Alpaca`] two constructors instead of one with a feed field,
/// and here it is the difference between simulated and real money.
pub struct AlpacaExecutor {
    api: &'static str,
    venue: &'static str,
    max_order_age: chrono::Duration,
    outstanding: Outstanding,
}

impl std::fmt::Debug for AlpacaExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlpacaExecutor")
            .field("venue", &self.venue)
            .field("max_order_age", &self.max_order_age)
            .finish_non_exhaustive()
    }
}

impl AlpacaExecutor {
    /// Simulated money against the real API. Start here.
    #[must_use]
    pub fn paper() -> Self {
        Self::at(PAPER, "ALPACA-PAPER")
    }

    /// Real money.
    ///
    /// Named rather than defaulted, and a separate constructor rather than a
    /// boolean, because `AlpacaExecutor::live()` at a call site says what it is
    /// and `AlpacaExecutor::new(false)` does not.
    #[must_use]
    pub fn live() -> Self {
        Self::at(LIVE, "ALPACA")
    }

    fn at(api: &'static str, venue: &'static str) -> Self {
        Self {
            api,
            venue,
            max_order_age: chrono::Duration::seconds(DEFAULT_MAX_ORDER_AGE_SECS),
            outstanding: Outstanding::new(),
        }
    }

    /// Overrides how long an order may rest. See [`DEFAULT_MAX_ORDER_AGE_SECS`].
    #[must_use]
    pub const fn with_max_order_age(mut self, age: chrono::Duration) -> Self {
        self.max_order_age = age;
        self
    }

    /// Whether this is pointed at simulated money.
    ///
    /// Exposed so a caller can say so on a screen. A session that cannot tell
    /// the operator which account it is trading is one keystroke from a
    /// surprise.
    #[must_use]
    pub fn is_paper(&self) -> bool {
        self.api == PAPER
    }

    /// How many orders are acknowledged and not yet resolved.
    #[must_use]
    pub fn resting(&self) -> usize {
        self.outstanding.len()
    }
}

#[async_trait::async_trait]
impl Executor for AlpacaExecutor {
    fn venue(&self) -> &str {
        self.venue
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
        let body = order_body(order);
        let response = auth::post(&format!("{}/v2/orders", self.api), &body)
            .await
            .map_err(|err| rejected(self.venue, &err))?;

        let id = placed_id(self.venue, &response)?;
        self.outstanding.watch(&id, order);
        Ok(OrderId(id))
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        let watching = self.outstanding.snapshot();
        if watching.is_empty() {
            return Ok((Vec::new(), 0));
        }

        // `status=all` because a filled or cancelled order is no longer open,
        // and asking only for open ones would leave every resolved order
        // outstanding forever — reported as unfilled on every later drain.
        let response = auth::get(&format!(
            "{}/v2/orders?status=all&limit=500&direction=desc",
            self.api
        ))
        .await
        .map_err(|err| transport(self.venue, &err))?;

        let mut outcome = reconcile(
            &watching,
            &order_states(self.venue, &response)?,
            chrono::Utc::now().naive_utc(),
            self.max_order_age,
        );

        for id in &outcome.stale {
            // A cancel that fails leaves the order outstanding on purpose: it
            // may have filled between the read and the request, and forgetting
            // it here would lose a fill the account really took.
            if auth::delete(&format!("{}/v2/orders/{id}", self.api))
                .await
                .is_ok()
            {
                outcome.resolved.push(id.clone());
            }
        }

        self.outstanding.forget(&outcome.resolved);
        Ok((outcome.executions, self.resting()))
    }
}

/// The body for one order.
///
/// Market, day, market-hours only — because [`Order`] is market-only: a limit
/// price is a second risk decision and the gate has no opinion about it.
///
/// `client_order_id` is derived from the order rather than generated. Alpaca
/// refuses a duplicate, which is exactly what should happen to a retry of a
/// request whose reply was lost: the request having succeeded and the reply
/// having failed are indistinguishable from here, and a fresh id per attempt is
/// how one signal becomes two positions.
fn order_body(order: &Order) -> Value {
    json!({
        "symbol": arvo_data::source::symbol_of(&order.instrument),
        // A string, and not rounded: `risk::decide` floors to whole shares and
        // refuses anything below one, so this is the size that was approved.
        "qty": order.quantity.to_string(),
        "side": match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        "type": "market",
        // Day, not good-till-cancelled. An order that outlives the session that
        // sized it is an order nothing is watching: the gate's day-trade count,
        // loss limit and halt are all per-session, and a GTC order would fill
        // tomorrow against none of them.
        "time_in_force": "day",
        "client_order_id": client_order_id(order),
    })
}

/// A stable idempotency key for one order.
///
/// UUIDv5 over the fields that make this order *this* order. Two proposals for
/// the same instrument, side, size and signal instant, from the same proposer,
/// are the same order — and if they are not, they were indistinguishable
/// anyway.
fn client_order_id(order: &Order) -> String {
    let seed = format!(
        "{}|{:?}|{}|{}|{}",
        order.instrument,
        order.side,
        order.quantity,
        order.decision_at.and_utc().timestamp_millis(),
        order.proposer,
    );
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, seed.as_bytes()).to_string()
}

/// The id the venue gave the order it accepted.
fn placed_id(venue: &str, response: &Value) -> Result<String, ExecutionError> {
    response
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionError::Rejected {
            venue: venue.to_owned(),
            reason: format!("the order was acknowledged without an id: {response}"),
        })
}

/// Every order the venue reported, by id.
///
/// Confirmed against Alpaca's published order schema rather than guessed: a
/// bare array, `id`, `status`, `filled_avg_price`, `filled_at`.
///
/// # Why this fails loudly
///
/// An unreadable response that returned an empty map would report *no fills*,
/// which is indistinguishable from a quiet market — so a field rename at the
/// vendor would silently stop the platform booking its own trades while every
/// order it placed went on filling.
fn order_states(venue: &str, response: &Value) -> Result<BTreeMap<String, State>, ExecutionError> {
    // Alpaca answers with a bare array rather than an envelope.
    let orders = response
        .as_array()
        .ok_or_else(|| ExecutionError::Transport {
            venue: venue.to_owned(),
            detail: format!("expected an array of orders, got {response}"),
        })?;

    let mut states = BTreeMap::new();
    for order in orders {
        let (Some(id), Some(status)) = (
            order.get("id").and_then(Value::as_str),
            order.get("status").and_then(Value::as_str),
        ) else {
            continue;
        };
        states.insert(id.to_owned(), one_state(order, status));
    }
    Ok(states)
}

fn one_state(order: &Value, status: &str) -> State {
    match status {
        "filled" => {
            // A fill whose price or instant cannot be read is *not* a fill at
            // zero — it is an order still to be resolved, so the next poll gets
            // another chance and the stale timeout catches it if there is never
            // a readable one.
            let price = order
                .get("filled_avg_price")
                .and_then(Value::as_str)
                .and_then(|price| price.parse::<f64>().ok());
            let at = order
                .get("filled_at")
                .and_then(Value::as_str)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.naive_utc());
            match (price, at) {
                (Some(price), Some(at)) if price > 0.0 => State::Filled { price, at },
                _ => State::Open,
            }
        }
        // Alpaca's documented terminal statuses, and only those. An order in
        // any other state can still change, and the cost of the two mistakes
        // is not symmetric: stop watching one that later fills and the gate
        // believes the account is flat while it holds a position nobody sized.
        // Keep watching one that never fills and it rests until the stale
        // timeout cancels it, which is a request the venue answers.
        //
        // `done_for_day` and `suspended` read like endings and are not on that
        // list. A day order that is done for the day goes on to `expired` or
        // `canceled`, and this sees that — or the timeout gets there first.
        "canceled" | "expired" | "replaced" | "rejected" => State::Gone,
        // `partially_filled` lands here with everything else. Half the size
        // the gate approved is not the position it approved, and an
        // unrecognised status is a reason to keep looking rather than to
        // guess — Alpaca has sixteen of them and adds to the list.
        _ => State::Open,
    }
}

fn rejected(venue: &str, err: &arvo_data::source::SourceError) -> ExecutionError {
    ExecutionError::Rejected {
        venue: venue.to_owned(),
        reason: err.to_string(),
    }
}

fn transport(venue: &str, err: &arvo_data::source::SourceError) -> ExecutionError {
    ExecutionError::Transport {
        venue: venue.to_owned(),
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

    fn at(minute: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 9)
            .expect("valid")
            .and_time(NaiveTime::from_hms_opt(14, minute, second).expect("valid"))
    }

    fn order() -> Order {
        Order {
            instrument: "MSFT.ASIP".to_owned(),
            side: Side::Buy,
            quantity: 20.0,
            decision_price: 100.0,
            decision_at: at(30, 0),
            proposer: "technical".to_owned(),
        }
    }

    #[test]
    fn paper_and_live_are_different_venues() {
        // Two datasets, two venues — the same argument the source makes about
        // IEX and SIP, and here the difference is whether the money is real.
        assert!(AlpacaExecutor::paper().is_paper());
        assert!(!AlpacaExecutor::live().is_paper());
        assert_ne!(
            AlpacaExecutor::paper().venue(),
            AlpacaExecutor::live().venue()
        );
    }

    #[test]
    fn an_order_is_sent_as_a_market_day_order_in_the_bare_symbol() {
        // `MSFT.ASIP` is an Arvo instrument id. Sending it as the symbol would
        // ask the venue to trade something that does not exist.
        let body = order_body(&order());
        assert_eq!(body["symbol"], "MSFT");
        assert_eq!(body["side"], "buy");
        assert_eq!(body["type"], "market");
        assert_eq!(body["qty"], "20");
        assert_eq!(
            body["time_in_force"], "day",
            "a GTC order would outlive the session that sized it"
        );
    }

    #[test]
    fn the_same_order_retried_carries_the_same_client_order_id() {
        // Alpaca refuses a duplicate, which is what should happen to a retry of
        // a request whose reply was lost.
        assert_eq!(client_order_id(&order()), client_order_id(&order()));
    }

    #[test]
    fn two_different_orders_do_not_share_one() {
        // The other half: a key so stable that a genuinely new order reused it
        // would have the venue refuse a trade that was meant to happen.
        let base = client_order_id(&order());
        for changed in [
            Order { quantity: 21.0, ..order() },
            Order { side: Side::Sell, ..order() },
            Order { decision_at: at(30, 1), ..order() },
            Order { instrument: "AAPL.ASIP".to_owned(), ..order() },
            Order { proposer: "alert".to_owned(), ..order() },
        ] {
            assert_ne!(client_order_id(&changed), base, "{changed:?}");
        }
    }

    #[test]
    fn an_acknowledgement_without_an_id_is_a_rejection_rather_than_a_lost_order() {
        // Returning `Ok` here would leave an order at the venue that nothing is
        // watching, which is worse than a failed submit.
        assert!(placed_id("ALPACA-PAPER", &json!({ "status": "new" })).is_err());
        assert_eq!(
            placed_id("ALPACA-PAPER", &json!({ "id": "ord-1" })).expect("an id"),
            "ord-1"
        );
    }

    #[test]
    fn a_response_nobody_can_read_is_an_error_not_an_empty_book() {
        // An empty map reads as "no fills", which is what a quiet market looks
        // like — so a field rename would stop the platform booking its own
        // trades and say nothing.
        let err = order_states("ALPACA-PAPER", &json!({ "orders": [] }))
            .expect_err("an envelope is not the array Alpaca sends");
        assert!(err.to_string().contains("array of orders"), "{err}");
    }

    #[test]
    fn a_filled_order_reports_the_price_and_instant_it_actually_filled_at() {
        let states = order_states(
            "ALPACA-PAPER",
            &json!([{
                "id": "ord-1",
                "status": "filled",
                "filled_avg_price": "100.08",
                "filled_at": "2026-09-09T14:30:00.250Z",
            }]),
        )
        .expect("readable");

        let State::Filled { price, at: filled } = &states["ord-1"] else {
            panic!("filled: {states:?}");
        };
        assert!((price - 100.08).abs() < 1e-9);
        // Read as UTC. Booking a fill an hour out would report an hour of
        // latency on every trade.
        assert_eq!(*filled, at(30, 0) + chrono::Duration::milliseconds(250));
    }

    #[test]
    fn a_partial_fill_is_not_a_fill() {
        // Half the size the gate approved is not the position it approved.
        let states = order_states(
            "ALPACA-PAPER",
            &json!([{
                "id": "ord-1",
                "status": "partially_filled",
                "filled_avg_price": "100.08",
                "filled_at": "2026-09-09T14:30:00Z",
            }]),
        )
        .expect("readable");
        assert_eq!(states["ord-1"], State::Open);
    }

    #[test]
    fn a_fill_whose_price_cannot_be_read_stays_open_rather_than_booking_at_zero() {
        let states = order_states(
            "ALPACA-PAPER",
            &json!([{ "id": "ord-1", "status": "filled" }]),
        )
        .expect("readable");
        assert_eq!(states["ord-1"], State::Open);
    }

    #[test]
    fn every_way_an_order_ends_without_filling_resolves_it() {
        // Alpaca's documented terminal set, less `filled`. Left open, each
        // would rest outstanding forever and be reported as an unfilled order
        // on every drain for the rest of the session.
        for ending in ["canceled", "expired", "rejected", "replaced"] {
            let states =
                order_states("ALPACA-PAPER", &json!([{ "id": "ord-1", "status": ending }]))
                    .expect("readable");
            assert_eq!(states["ord-1"], State::Gone, "{ending}");
        }
    }

    #[test]
    fn a_status_this_does_not_recognise_is_watched_rather_than_guessed() {
        // Alpaca has sixteen of them and adds to the list. An unknown one is a
        // reason to keep looking, and the stale timeout is the backstop.
        for working in [
            "new",
            "accepted",
            "pending_new",
            "calculated",
            "held",
            // The two that read like endings and are not on Alpaca's terminal
            // list. Treating them as over stops the watch on an order that can
            // still fill, and a fill nobody booked is a position the gate does
            // not know it holds.
            "done_for_day",
            "suspended",
            "invented",
        ] {
            let states =
                order_states("ALPACA-PAPER", &json!([{ "id": "ord-1", "status": working }]))
                    .expect("readable");
            assert_eq!(states["ord-1"], State::Open, "{working}");
        }
    }
}
