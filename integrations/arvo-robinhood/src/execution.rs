//! Sending orders to Robinhood, behind the gate.
//!
//! # Why this is here and not in `arvo-execution`
//!
//! Nothing about this is generic MCP. An order needs an account number, the
//! account has to be marked agentic, the idempotency key is called `ref_id`,
//! and fills are found by polling `get_equity_orders`. Those are Robinhood's
//! facts, and they belong beside Robinhood's other facts — the same argument
//! that puts every [`arvo_data::source::Source`] implementation in its own
//! integration rather than in the crate that defines the trait.
//!
//! `arvo-execution` keeps the vocabulary. This keeps the vendor.
//!
//! # The gate is not optional, and that is structural
//!
//! This type is an [`Executor`], and the only public path to
//! [`Executor::submit`] is `arvo_execution::Session::propose`. So an order
//! placed here has been sized by the risk gate, checked against the day-trade
//! budget, the daily loss limit, the correlation cap and the staleness window,
//! and refused if the account is halted. A second path to this struct would
//! undo all of that, which is why it holds no method that sends an order
//! except the trait's.
//!
//! # Stale orders, and why there is no market calendar here
//!
//! A market order placed outside regular hours is **not rejected** — it queues
//! for the next regular open. For a day-trading system that is the worst
//! available outcome: a signal from 16:05 fills at 09:30 tomorrow, against a
//! book that has moved overnight, and the gate books it as though the decision
//! were fresh.
//!
//! The obvious guard is a market calendar. It is the wrong one: it needs a
//! timezone database for the ET conversion, a holiday list, an early-close
//! list, and it still says nothing about a halted symbol or a book too thin to
//! fill. All of those have the same symptom — the order sits.
//!
//! So the guard is on the symptom. Anything still unfilled after
//! [`DEFAULT_MAX_ORDER_AGE_SECS`] is cancelled and reported as unfilled, which
//! covers after-hours, holidays, halts and thin books with one mechanism and no
//! calendar to maintain. Session-length modelling is [#11]; this does not need
//! to wait for it.
//!
//! [#11]: https://github.com/wjpin84/arvo-desktop/issues/11

use std::collections::BTreeMap;

use arvo_execution::poll::{reconcile, Outstanding, State};
use arvo_execution::{Execution, ExecutionError, Executor, Order, OrderId, Side};
use serde_json::{json, Value};

use crate::auth::connect;
use crate::source::VENUE;

/// The MCP tool that places an equity order.
const PLACE: &str = "place_equity_order";
/// The MCP tool that reports what happened to them.
const ORDERS: &str = "get_equity_orders";
/// The MCP tool that takes one back.
const CANCEL: &str = "cancel_equity_order";

/// How long an unfilled order may rest before it is cancelled.
///
/// Two minutes. A market order in a liquid name fills in under a second, so
/// anything still resting after this is not slow — it is queued for a session
/// that has not started, in a symbol that is halted, or in a book that cannot
/// absorb the size. None of those improve with waiting, and all of them get
/// worse: the price the decision was made at is receding the whole time.
pub const DEFAULT_MAX_ORDER_AGE_SECS: i64 = 120;

/// Places orders through Robinhood's MCP server.
///
/// # Errors it does not try to prevent
///
/// The account must be marked `agentic_allowed`; a non-agentic one is rejected
/// by the server. That is not checked here, because checking it means a call
/// whose answer can change between the check and the order — and the rejection
/// is clear, immediate and arrives with the order that caused it.
pub struct RobinhoodExecutor {
    /// Supplied by whoever built this, never discovered.
    ///
    /// An executor that picked an account off `get_accounts` would trade in
    /// whichever one the API happened to list first. There is no sensible
    /// default for "which of someone's accounts should this send real orders
    /// to", so there is no default.
    account: String,
    max_order_age: chrono::Duration,
    /// Acknowledged orders whose outcome is not yet known, by order id.
    outstanding: Outstanding,
}

impl std::fmt::Debug for RobinhoodExecutor {
    /// Never prints the account number. It is not a credential, but it is the
    /// one field here that identifies a real person's brokerage account, and a
    /// panic message is not where it should turn up.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RobinhoodExecutor")
            .field("account", &"<redacted>")
            .field("max_order_age", &self.max_order_age)
            .finish_non_exhaustive()
    }
}

impl RobinhoodExecutor {
    #[must_use]
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
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

    /// How many orders are acknowledged and not yet resolved.
    #[must_use]
    pub fn resting(&self) -> usize {
        self.outstanding.len()
    }
}

#[async_trait::async_trait]
impl Executor for RobinhoodExecutor {
    fn venue(&self) -> &str {
        VENUE
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
        let client = connect().await.map_err(|err| ExecutionError::Transport {
            venue: VENUE.to_owned(),
            detail: err.to_string(),
        })?;

        let response = client
            .call_tool_json(PLACE, order_args(&self.account, order))
            .await
            .map_err(|err| ExecutionError::Rejected {
                venue: VENUE.to_owned(),
                reason: err.to_string(),
            })?;

        let id = placed_id(&response)?;

        self.outstanding.watch(&id, order);
        Ok(OrderId(id))
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        let watching = self.outstanding.snapshot();
        if watching.is_empty() {
            return Ok((Vec::new(), 0));
        }

        let client = connect().await.map_err(|err| ExecutionError::Transport {
            venue: VENUE.to_owned(),
            detail: err.to_string(),
        })?;

        let response = client
            .call_tool_json(
                ORDERS,
                json!({
                    "account_number": self.account,
                    // Only what this platform placed. A person selling
                    // something by hand in the same account is not a fill of
                    // anything proposed here, and booking it as one would tell
                    // the gate it holds a position nobody sized.
                    "placed_agent": "agentic",
                }),
            )
            .await
            .map_err(|err| ExecutionError::Transport {
                venue: VENUE.to_owned(),
                detail: err.to_string(),
            })?;

        let mut outcome = reconcile(
            &watching,
            &order_states(&response)?,
            chrono::Utc::now().naive_utc(),
            self.max_order_age,
        );

        for id in &outcome.stale {
            // A cancel that fails leaves the order outstanding on purpose: it
            // may have filled in the moment between reading and cancelling,
            // and forgetting it here would lose a fill the account really
            // took. The next poll sees it again.
            if client
                .call_tool_json(
                    CANCEL,
                    json!({ "account_number": self.account, "order_id": id }),
                )
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

/// The arguments for one order.
///
/// Market orders only, because [`Order`] is market-only: a limit price is a
/// second risk decision and the gate has no opinion about it.
///
/// `ref_id` is derived from the order rather than generated, so a retry of a
/// transport failure carries the *same* key and the gateway deduplicates it. A
/// fresh UUID per attempt is how one signal becomes two positions when a
/// response is lost on the way back — the request having succeeded and the
/// reply having failed are indistinguishable from here.
fn order_args(account: &str, order: &Order) -> Value {
    json!({
        "account_number": account,
        "symbol": arvo_data::source::symbol_of(&order.instrument),
        "side": match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        "type": "market",
        // A string, as the tool expects, and never rounded: the gate sized
        // this and a quantity the executor adjusted would not be the one that
        // was approved. Safe to format directly because `risk::decide` floors
        // to whole shares and refuses anything below one — if that ever changes
        // this needs a six-decimal truncation, rounding *down*, since only that
        // direction stays inside what was approved.
        "quantity": order.quantity.to_string(),
        "time_in_force": "gfd",
        "ref_id": ref_id(order),
    })
}

/// A stable idempotency key for one order.
///
/// UUIDv5 over the fields that make this order *this* order. Two proposals for
/// the same instrument, side, size and signal instant, from the same proposer,
/// are the same order — and if they are not, they were indistinguishable
/// anyway.
fn ref_id(order: &Order) -> String {
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
fn placed_id(response: &Value) -> Result<String, ExecutionError> {
    response
        .pointer("/data/id")
        .or_else(|| response.pointer("/id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionError::Rejected {
            venue: VENUE.to_owned(),
            reason: format!("the order was acknowledged without an id: {response}"),
        })
}

/// Every order the venue reported, by id.
///
/// # Why this fails loudly
///
/// An unreadable response that returned an empty map would report *no fills*,
/// which is indistinguishable from a quiet market — so a field rename at the
/// vendor would silently stop the platform booking its own trades while every
/// order it placed went on filling. Naming what was looked for turns that into
/// one obvious error on the first poll.
fn order_states(response: &Value) -> Result<BTreeMap<String, State>, ExecutionError> {
    let orders = response
        .pointer("/data/orders")
        .or_else(|| response.pointer("/data/results"))
        .or_else(|| response.pointer("/orders"))
        .and_then(Value::as_array)
        .ok_or_else(|| ExecutionError::Transport {
            venue: VENUE.to_owned(),
            detail: format!("no data.orders, data.results or orders array in {response}"),
        })?;

    let mut states = BTreeMap::new();
    for order in orders {
        let Some(id) = order.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(state) = order.get("state").and_then(Value::as_str) else {
            continue;
        };
        states.insert(id.to_owned(), one_state(order, state));
    }
    Ok(states)
}

fn one_state(order: &Value, state: &str) -> State {
    match state {
        "filled" => {
            // Prices arrive as strings. A fill whose price cannot be read is
            // *not* a fill at zero — it is an order still to be resolved, and
            // leaving it open means the next poll gets another chance rather
            // than the ledger getting a free share.
            let price = order
                .get("average_price")
                .and_then(Value::as_str)
                .and_then(|price| price.parse::<f64>().ok());
            let at = order
                .get("last_transaction_at")
                .or_else(|| order.get("updated_at"))
                .and_then(Value::as_str)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.naive_utc());
            match (price, at) {
                (Some(price), Some(at)) if price > 0.0 => State::Filled { price, at },
                _ => State::Open,
            }
        }
        "cancelled" | "canceled" | "rejected" | "failed" | "voided" => State::Gone,
        _ => State::Open,
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
            instrument: "MSFT.RH".to_owned(),
            side: Side::Buy,
            quantity: 20.0,
            decision_price: 100.0,
            decision_at: at(30, 0),
            proposer: "technical".to_owned(),
        }
    }

    #[test]
    fn an_order_is_sent_as_a_market_order_in_the_bare_symbol() {
        // `MSFT.RH` is an Arvo instrument id. Sending it as the symbol would
        // ask the venue to trade something that does not exist.
        let args = order_args("ACC-1", &order());
        assert_eq!(args["symbol"], "MSFT");
        assert_eq!(args["side"], "buy");
        assert_eq!(args["type"], "market");
        assert_eq!(args["quantity"], "20");
        assert_eq!(args["account_number"], "ACC-1");
    }

    #[test]
    fn the_same_order_retried_carries_the_same_idempotency_key() {
        // The one that turns a lost response into two positions. A request that
        // succeeded with a reply that failed is indistinguishable from a
        // request that failed, so the key has to come from the order.
        assert_eq!(ref_id(&order()), ref_id(&order()));
        assert_eq!(
            order_args("ACC-1", &order())["ref_id"],
            order_args("ACC-1", &order())["ref_id"]
        );
    }

    #[test]
    fn two_different_orders_do_not_share_one() {
        // And the other half: a key so stable that a second, genuinely new
        // order reuses it would have the gateway swallow a trade that was
        // meant to happen.
        let base = ref_id(&order());
        for changed in [
            Order { quantity: 21.0, ..order() },
            Order { side: Side::Sell, ..order() },
            Order { decision_at: at(30, 1), ..order() },
            Order { instrument: "AAPL.RH".to_owned(), ..order() },
            Order { proposer: "alert".to_owned(), ..order() },
        ] {
            assert_ne!(ref_id(&changed), base, "{changed:?}");
        }
    }

    #[test]
    fn an_acknowledgement_without_an_id_is_a_rejection_rather_than_a_lost_order() {
        // Returning `Ok` here would leave an order at the venue that nothing
        // is watching, which is the one outcome worse than a failed submit.
        assert!(placed_id(&json!({ "data": {} })).is_err());
        assert_eq!(
            placed_id(&json!({ "data": { "id": "ord-1" } })).expect("an id"),
            "ord-1"
        );
    }

    #[test]
    fn a_response_nobody_can_read_is_an_error_not_an_empty_book() {
        // An empty map reads as "no fills", which is exactly what a quiet
        // market looks like — so a field rename at the vendor would stop the
        // platform booking its own trades and say nothing.
        let err = order_states(&json!({ "data": { "unexpected": [] } }))
            .expect_err("unreadable is an error");
        assert!(err.to_string().contains("data.orders"), "{err}");
    }

    #[test]
    fn a_filled_order_reports_the_price_and_instant_it_actually_filled_at() {
        let states = order_states(&json!({
            "data": { "orders": [{
                "id": "ord-1",
                "state": "filled",
                "average_price": "100.0800",
                "last_transaction_at": "2026-09-09T14:30:00.250Z",
            }] }
        }))
        .expect("readable");

        let State::Filled { price, at: filled } = &states["ord-1"] else {
            panic!("filled: {states:?}");
        };
        assert!((price - 100.08).abs() < 1e-9);
        // Read as UTC, not as a local instant. Booking a fill an hour out
        // would report an hour of latency on every trade.
        assert_eq!(*filled, at(30, 0) + chrono::Duration::milliseconds(250));
    }

    #[test]
    fn a_fill_whose_price_cannot_be_read_stays_open_rather_than_booking_at_zero() {
        // A free share is not a conservative error. Leaving it open means the
        // next poll gets another chance, and the stale timeout catches it if
        // there never is one.
        let states = order_states(&json!({
            "data": { "orders": [{ "id": "ord-1", "state": "filled" }] }
        }))
        .expect("readable");
        assert_eq!(states["ord-1"], State::Open);
    }

    #[test]
    fn a_partial_fill_is_not_a_fill() {
        // Half the size the gate approved is not the position it approved, and
        // booking it as complete tells the gate the account holds more than it
        // does.
        let states = order_states(&json!({
            "data": { "orders": [{
                "id": "ord-1",
                "state": "partially_filled",
                "average_price": "100.08",
                "last_transaction_at": "2026-09-09T14:30:00Z",
            }] }
        }))
        .expect("readable");
        assert_eq!(states["ord-1"], State::Open);
    }

    #[test]
    fn every_way_an_order_ends_without_filling_resolves_it() {
        // Left open, each of these would rest in `outstanding` forever and be
        // reported as an unfilled order on every drain for the rest of the
        // session.
        for ending in ["cancelled", "canceled", "rejected", "failed", "voided"] {
            let states = order_states(&json!({
                "data": { "orders": [{ "id": "ord-1", "state": ending }] }
            }))
            .expect("readable");
            assert_eq!(states["ord-1"], State::Gone, "{ending}");
        }
    }

    #[test]
    fn the_account_number_never_appears_in_debug_output() {
        let printed = format!("{:?}", RobinhoodExecutor::new("ACC-SECRET"));
        assert!(!printed.contains("ACC-SECRET"), "{printed}");
    }
}
