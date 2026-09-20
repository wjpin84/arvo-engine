use super::*;
use chrono::{NaiveDate, NaiveTime};

fn at(minute: u32, second: u32, milli: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 9, 9)
        .expect("valid")
        .and_time(
            NaiveTime::from_hms_milli_opt(14, minute, second, milli).expect("valid"),
        )
}

fn execution(side: Side, decision: f64, fill: f64, latency_ms: u32) -> Execution {
    Execution {
        order: OrderId("o-1".to_owned()),
        instrument: "MSFT.RH".to_owned(),
        side,
        proposer: "technical".to_owned(),
        quantity: 10.0,
        decision_price: decision,
        fill_price: fill,
        decision_at: at(30, 0, 0),
        filled_at: at(30, 0, latency_ms),
    }
}

/// A venue that takes nothing, to exercise the half of a kill switch that
/// matters most: what happens when the exits do not go out.
struct Refusing;

#[async_trait::async_trait]
impl Executor for Refusing {
    fn venue(&self) -> &str {
        "refusing"
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
        Err(ExecutionError::Transport {
            venue: "refusing".to_owned(),
            detail: format!("no route for {}", order.instrument),
        })
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        Ok((Vec::new(), 0))
    }

    async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
        Ok(VenueState::default())
    }

    async fn cancel(&self, _: &OrderId) -> Result<(), ExecutionError> {
        Err(ExecutionError::Transport {
            venue: "refusing".to_owned(),
            detail: "takes nothing back either".to_owned(),
        })
    }
}

#[tokio::test]
async fn one_exit_the_venue_refuses_does_not_strand_the_others() {
    // The failure mode a `?` would have shipped: the first unreachable
    // instrument aborts the flatten and everything after it stays held, by
    // an account nobody is watching any more because the button was pressed.
    use arvo_risk::{RiskGate, RiskModel};

    let day = NaiveDate::from_ymd_opt(2026, 9, 9).expect("valid");
    let mut gate = RiskGate::new(RiskModel::default(), 10_000.0, day);
    gate.opened("MSFT.RH", 10.0, 100.0, day);
    gate.opened("AAPL.RH", 20.0, 50.0, day);
    gate.opened("NVDA.RH", 5.0, 200.0, day);
    let mut session = Session::new(gate, Refusing);

    let flatten = session
        .kill("operator pulled it", &Default::default(), at(0, 0, 0))
        .await;

    assert!(!flatten.complete());
    assert_eq!(flatten.failed.len(), 3, "every position was attempted");
    let stranded: Vec<&str> = flatten
        .failed
        .iter()
        .map(|(instrument, _)| instrument.as_str())
        .collect();
    assert_eq!(stranded, ["AAPL.RH", "MSFT.RH", "NVDA.RH"]);

    // Armed regardless. The exits failing is the reason to stop trading,
    // not a reason to carry on.
    assert!(session.gate().halted().is_some());
}

/// A venue with a stated amount to spend, remembering what it was sent.
struct Funded {
    cash: Option<f64>,
    sent: std::sync::Mutex<Vec<f64>>,
}

#[async_trait::async_trait]
impl Executor for Funded {
    fn venue(&self) -> &str {
        "funded"
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
        self.sent.lock().expect("unpoisoned").push(order.quantity);
        Ok(OrderId("o".to_owned()))
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        Ok((Vec::new(), 0))
    }

    async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
        Ok(VenueState::default())
    }

    async fn cancel(&self, _: &OrderId) -> Result<(), ExecutionError> {
        Ok(())
    }

    async fn buying_power(&self) -> Result<Option<f64>, ExecutionError> {
        Ok(self.cash)
    }
}

async fn sent_for(cash: Option<f64>) -> Vec<f64> {
    use arvo_risk::{CostModel, Proposal, RiskGate, RiskModel};
    let gate = RiskGate::new(RiskModel::default(), 100_000.0, day())
        .with_costs(CostModel::proportional(1.0, 1.0));
    let mut session = Session::new(
        gate,
        Funded {
            cash,
            sent: std::sync::Mutex::default(),
        },
    );
    let signalled = at(30, 0, 0);
    let proposal = Proposal {
        instrument: "MSFT.AIEX".to_owned(),
        proposer: "test".to_owned(),
        signalled_at: signalled,
        reference_price: 100.0,
        stop_distance: None,
        desired_quantity: Some(5_000.0),
        opens_short: false,
    };
    session
        .propose(&proposal, signalled, None)
        .await
        .expect("the venue answered");
    let sent = session.executor().sent.lock().expect("unpoisoned").clone();
    sent
}

#[tokio::test]
async fn a_live_entry_is_capped_at_what_the_broker_says_it_can_spend() {
    // ADR-0015's follow-up. The gate sizes off the opening balance, so an
    // account down to $20,000 kept proposing $100,000 of stock and the
    // broker refused it. Asked per proposal, the venue's figure caps it.
    let capped = sent_for(Some(20_000.0)).await;
    assert_eq!(capped, vec![199.0], "$20,000 all in at $100.02 a share");

    let unknown = sent_for(None).await;
    assert_eq!(unknown, vec![999.0], "a venue that cannot say leaves the ceiling off, costs still in");
}

/// A venue that already holds things, to exercise the one case a paper
/// executor structurally cannot have.
struct Stocked {
    state: VenueState,
    /// Orders it will refuse to take back.
    immovable: Vec<&'static str>,
    /// Orders it reports as still working.
    outstanding: usize,
}

#[async_trait::async_trait]
impl Executor for Stocked {
    fn venue(&self) -> &str {
        "stocked"
    }

    async fn submit(&self, _: &Order) -> Result<OrderId, ExecutionError> {
        Ok(OrderId("new".to_owned()))
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
        Ok((Vec::new(), self.outstanding))
    }

    async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
        Ok(self.state.clone())
    }

    async fn cancel(&self, order: &OrderId) -> Result<(), ExecutionError> {
        if self.immovable.contains(&order.0.as_str()) {
            return Err(ExecutionError::Rejected {
                venue: "stocked".to_owned(),
                reason: "already filling".to_owned(),
            });
        }
        Ok(())
    }
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 9).expect("valid")
}

fn stocked(state: VenueState, immovable: Vec<&'static str>) -> Session<Stocked> {
    use arvo_risk::{RiskGate, RiskModel};
    Session::new(
        RiskGate::new(RiskModel::default(), 10_000.0, day()),
        Stocked { state, immovable, outstanding: 0 },
    )
}

fn holding(symbol: &str, quantity: f64) -> Holding {
    Holding {
        symbol: symbol.to_owned(),
        quantity,
        entry: 100.0,
    }
}

#[tokio::test]
async fn a_clear_venue_is_the_ordinary_start_and_changes_nothing() {
    // The common path. A reconciliation that halted every session would be
    // a reconciliation nobody ran.
    let mut session = stocked(VenueState::default(), Vec::new());
    let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    assert!(!found.found_anything());
    assert!(session.gate().halted().is_none());
    assert!(session.gate().positions().is_empty());
}

#[tokio::test]
async fn a_position_the_gate_never_saw_is_adopted_and_then_sized_against() {
    // The hazard in one test. Before this the gate started flat, so it
    // would size the next trade against capital already committed.
    let state = VenueState {
        positions: vec![holding("MSFT", 20.0)],
        resting: Vec::new(),
    };
    let mut session = stocked(state, Vec::new());
    let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    assert_eq!(found.adopted, vec![holding("MSFT", 20.0)]);
    let held = session
        .gate()
        .positions()
        .get("MSFT.RH")
        .expect("the gate was told");
    assert!((held.quantity - 20.0).abs() < 1e-9);
    assert!((held.entry - 100.0).abs() < 1e-9);
}

#[tokio::test]
async fn a_short_keeps_its_sign_so_flattening_it_does_not_double_it() {
    // A venue reports a short as a negative quantity, and flattening one
    // means buying. Dropping the sign would have the exit sell more.
    let state = VenueState {
        positions: vec![holding("MSFT", -20.0)],
        resting: Vec::new(),
    };
    let mut session = stocked(state, Vec::new());
    session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    let held = session.gate().positions()["MSFT.RH"];
    assert!(held.quantity < 0.0, "{held:?}");
}

#[tokio::test]
async fn an_order_nobody_is_watching_is_taken_back() {
    // It cannot be adopted: `poll::Sent` needs the price and instant the
    // signal fired, and the venue does not know them. So it is cancelled,
    // and a strategy that still wants the position proposes it again with
    // a fresh decision behind it.
    let state = VenueState {
        positions: Vec::new(),
        resting: vec![OrderId("ord-1".to_owned()), OrderId("ord-2".to_owned())],
    };
    let mut session = stocked(state, Vec::new());
    let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    assert_eq!(found.cancelled.len(), 2);
    assert!(found.stranded.is_empty());
}

#[tokio::test]
async fn an_order_the_venue_will_not_take_back_is_named_rather_than_forgotten() {
    // Still working, still unwatched, and the one thing the operator most
    // needs to be told. Dropping it silently would report a clean start.
    let state = VenueState {
        positions: Vec::new(),
        resting: vec![OrderId("ord-1".to_owned()), OrderId("stuck".to_owned())],
    };
    let mut session = stocked(state, vec!["stuck"]);
    let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    assert_eq!(found.cancelled, vec![OrderId("ord-1".to_owned())]);
    assert_eq!(found.stranded.len(), 1);
    assert_eq!(found.stranded[0].0, OrderId("stuck".to_owned()));
    assert!(
        session
            .gate()
            .halted()
            .is_some_and(|why| why.contains("would not cancel")),
        "{:?}",
        session.gate().halted()
    );
}

#[tokio::test]
async fn finding_anything_stops_the_account_until_a_person_looks() {
    // Adopting silently would restore the positions and lose everything
    // the gate knows around them — the day-trade count, today's realised
    // loss, the equity peak the drawdown halt measures from. An account
    // that hit its daily loss limit, crashed and restarted would be free
    // to trade again, and a limit a restart lifts is not a limit.
    let state = VenueState {
        positions: vec![holding("MSFT", 20.0)],
        resting: Vec::new(),
    };
    let mut session = stocked(state, Vec::new());
    session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

    let why = session.gate().halted().expect("halted").to_owned();
    assert!(why.contains("1 position"), "{why}");

    let proposal = arvo_risk::Proposal {
        instrument: "AAPL.RH".to_owned(),
        proposer: "technical".to_owned(),
        signalled_at: at(0, 0, 0),
        reference_price: 100.0,
        stop_distance: Some(2.0),
        desired_quantity: None,
        opens_short: false,
    };
    assert!(
        session
            .propose(&proposal, at(0, 0, 0), None)
            .await
            .expect("venue")
            .is_none(),
        "nothing may be proposed into an account nobody has looked at"
    );

    // And a person can release it, because this halt is one somebody chose.
    assert!(session.rearm());
    assert!(session.gate().halted().is_none());
}

#[test]
fn adverse_slippage_is_positive_whichever_way_the_order_went() {
    // Unsigned, a bad buy and a lucky sell cancel into an encouraging zero.
    let bought_high = execution(Side::Buy, 100.0, 100.10, 0);
    let sold_low = execution(Side::Sell, 100.0, 99.90, 0);
    assert!((bought_high.slippage_bps() - 10.0).abs() < 1e-9);
    assert!((sold_low.slippage_bps() - 10.0).abs() < 1e-9);
}

#[test]
fn a_fill_better_than_the_decision_price_is_negative_slippage() {
    // It happens, and reporting it as zero would bias the measurement in
    // exactly the direction this exists to detect.
    let lucky = execution(Side::Buy, 100.0, 99.95, 0);
    assert!((lucky.slippage_bps() + 5.0).abs() < 1e-9);
}

#[test]
fn latency_is_measured_from_the_signal_not_the_send() {
    // Timing from the send would measure the last hop and hide the queue,
    // which is where an alert pipeline actually spends its time.
    assert_eq!(execution(Side::Buy, 100.0, 100.0, 250).latency_ms(), 250);
}

#[test]
fn divergence_says_how_optimistic_the_backtest_was() {
    // The number this whole crate exists to produce.
    let executions = vec![
        execution(Side::Buy, 100.0, 100.04, 120),
        execution(Side::Buy, 100.0, 100.06, 380),
    ];
    let divergence = Divergence::of(&executions, 0, Some(1.0));

    assert_eq!(divergence.fills, 2);
    assert!((divergence.mean_slippage_bps - 5.0).abs() < 1e-9);
    assert!((divergence.worst_slippage_bps - 6.0).abs() < 1e-9);
    assert!(
        (divergence.optimism_bps().expect("assumed was given") - 4.0).abs() < 1e-9,
        "assumed 1bp, measured 5bp: every stored result is 4bp per fill optimistic"
    );
    assert_eq!(divergence.worst_latency_ms, 380);
}

#[test]
fn an_unfilled_order_is_counted_and_never_averaged_in_as_a_free_fill() {
    // A backtest assumes every order fills, so this has no counterpart to
    // compare against. Averaging it in as zero slippage would report a
    // failure to trade as a perfect trade.
    let divergence = Divergence::of(&[execution(Side::Buy, 100.0, 100.05, 0)], 3, Some(1.0));
    assert_eq!(divergence.fills, 1);
    assert_eq!(divergence.unfilled, 3);
    assert!((divergence.mean_slippage_bps - 5.0).abs() < 1e-9);
}

#[test]
fn a_session_with_no_fills_reports_nothing_rather_than_dividing_by_zero() {
    let divergence = Divergence::of(&[], 0, None);
    assert_eq!(divergence.fills, 0);
    assert!(divergence.optimism_bps().is_none());
}

#[tokio::test]
async fn an_audit_names_what_the_gate_and_the_venue_disagree_about() {
    // The gate holds MSFT 20 and AAPL 5; the venue holds MSFT 10 and TSLA 3.
    // Every kind of disagreement at once: shrunk, gone, and never heard of.
    let state = VenueState {
        positions: vec![holding("MSFT", 10.0), holding("TSLA", 3.0)],
        resting: Vec::new(),
    };
    let mut session = stocked(state, Vec::new());
    session.gate.opened("MSFT.RH", 20.0, 100.0, day());
    session.gate.opened("AAPL.RH", 5.0, 100.0, day());

    let found = session.audit("RH").await.expect("readable");
    let mut named: Vec<(&str, f64, f64)> = found.iter().map(|d| (d.instrument.as_str(), d.expected, d.at_venue)).collect();
    named.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(named, vec![("AAPL.RH", 5.0, 0.0), ("MSFT.RH", 20.0, 10.0), ("TSLA.RH", 0.0, 3.0)]);
    assert!(session.gate().halted().is_none(), "an audit reports; it does not decide");
}

#[tokio::test]
async fn an_audit_agreeing_with_the_venue_says_nothing() {
    let state = VenueState { positions: vec![holding("MSFT", 20.0)], resting: Vec::new() };
    let mut session = stocked(state, Vec::new());
    session.gate.opened("MSFT.RH", 20.0, 100.0, day());
    assert!(session.audit("RH").await.expect("readable").is_empty());
}

#[tokio::test]
async fn an_audit_stays_quiet_while_its_own_order_is_still_working() {
    // The venue is ahead of the gate by exactly the order in flight. Raising
    // an incident over that would freeze every session on every entry.
    let state = VenueState { positions: vec![holding("MSFT", 20.0)], resting: Vec::new() };
    let mut session = stocked(state, Vec::new());
    session.executor.outstanding = 1;
    session.settle().await.expect("drains");
    assert!(session.audit("RH").await.expect("readable").is_empty());

    session.executor.outstanding = 0;
    session.settle().await.expect("drains");
    assert_eq!(session.audit("RH").await.expect("readable").len(), 1, "and speaks once the order has settled");
}

#[tokio::test]
async fn adopting_makes_the_gate_agree_with_the_venue_and_the_audit_go_quiet() {
    let state = VenueState {
        positions: vec![holding("MSFT", 10.0), holding("TSLA", 3.0)],
        resting: Vec::new(),
    };
    let mut session = stocked(state, Vec::new());
    session.gate.opened("MSFT.RH", 20.0, 90.0, day());
    session.gate.opened("AAPL.RH", 5.0, 100.0, day());

    let corrected = session.adopt(at(0, 0, 0), "RH").await.expect("readable");
    assert_eq!(corrected.len(), 3);
    let book = session.gate().positions();
    assert!((book["MSFT.RH"].quantity - 10.0).abs() < 1e-9);
    assert!((book["MSFT.RH"].entry - 100.0).abs() < 1e-9, "the venue's entry, not the gate's");
    assert!((book["TSLA.RH"].quantity - 3.0).abs() < 1e-9);
    assert!(!book.contains_key("AAPL.RH"));
    assert!(session.audit("RH").await.expect("readable").is_empty());
}
