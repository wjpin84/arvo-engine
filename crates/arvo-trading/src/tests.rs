//! The session loop's own tests.
//!
//! Together rather than beside each module because they drive the handle
//! from outside — a promotion refused, an error cleared, a watch built from
//! fills — which is the level the two live bugs of 2026-09-24 escaped at.

use std::path::Path;
use std::sync::Mutex;

use arvo_execution::{Executor, Order, OrderId, Session, Divergence, Execution};
use arvo_research::live::{Expectation, Live};
use arvo_risk::Warning;
use arvo_risk::{RiskGate, RiskModel};
use arvo_nautilus::{Side, Signal};
use tokio::sync::broadcast;

use crate::bar::{act, caught_up, held_for};
use crate::promotion::PAPER_MINIMUM_DAYS;
use crate::record::Recorder;
use crate::sessions::Sessions;
use crate::state::{recovered, trouble};
use crate::status::Status;
use crate::watch::Watch;

use super::*;

#[test]
fn a_bar_that_closed_before_the_session_started_is_caught_up_not_traded() {
    let five = chrono::Duration::minutes(5);
    let started = "2026-09-22T14:11:30".parse().expect("time");
    let at = |text: &str| text.parse().expect("time");
    // Yesterday's entry bar, and this morning's bars before the start.
    assert!(caught_up(at("2026-09-21T13:45:00"), five, started));
    assert!(caught_up(at("2026-09-22T14:05:00"), five, started));
    // The bar open at the start closes after it: live.
    assert!(!caught_up(at("2026-09-22T14:10:00"), five, started));
    assert!(!caught_up(at("2026-09-22T14:15:00"), five, started));
}

#[test]
fn an_error_is_cleared_by_the_call_that_raised_it_and_by_no_other() {
    let events = broadcast::channel(16).0;
    let status = Mutex::new(Status {
        id: "s".to_owned(),
        finding: "f".to_owned(),
        executor: "alpaca-paper".to_owned(),
        instrument: String::new(),
        strategy: String::new(),
        state: "running".to_owned(),
        signals: 0,
        submitted: 0,
        refused: 0,
        fills: 0,
        halted: None,
        last_error: None,
        error_from: None,
        last_bar: None,
        frozen: None,
        reconciled: false,
        started_at: String::new(),
        verdict: "inconclusive".to_owned(),
        verdict_reason: None,
        warnings: Vec::new(),
        divergence: None,
    });
    let read = || {
        let status = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (status.last_error.clone(), status.error_from)
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let record = Recorder::open(dir.path(), "f@alpaca-paper").expect("record");
    trouble(
        &status,
        &record,
        &events,
        "settle_failed",
        &"the venue timed out",
    );
    assert_eq!(
        read(),
        (
            Some("the venue timed out".to_owned()),
            Some("settle_failed")
        )
    );

    // A bar arriving does not mean the settle is working again.
    recovered(&status, &events, "fetch_failed");
    assert_eq!(
        read().0,
        Some("the venue timed out".to_owned()),
        "another call's success hides nothing"
    );

    // Its own success does.
    recovered(&status, &events, "settle_failed");
    assert_eq!(read(), (None, None));
}

#[test]
fn an_unknown_executor_is_refused_before_a_thread_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    let refused = sessions
        .start("f-1", "etrade")
        .expect_err("not an executor");
    assert!(refused.contains("alpaca-paper"), "{refused}");
    assert!(sessions.list().is_empty());
    // The naming rules themselves are `Brokers`' own test; here the point is
    // that the handle refuses before a thread exists.
    assert!(sessions.start("f-1", "robinhood").is_err(), "which account?");
}

/// A paper record spanning `days`, ending on the given verdict.
fn paper_record(data: &Path, finding: &str, days: i64, verdict: Option<&str>) {
    let path = record_path(data, &format!("{finding}@alpaca-paper"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let start = chrono::Utc::now() - chrono::Duration::days(days);
    let mut lines = vec![
        format!(
            r#"{{"at":"{}","event":"started","detail":null}}"#,
            start.to_rfc3339()
        ),
        format!(
            r#"{{"at":"{}","event":"bar","detail":null}}"#,
            (start + chrono::Duration::days(1)).to_rfc3339()
        ),
    ];
    if let Some(verdict) = verdict {
        lines.push(format!(
                r#"{{"at":"{}","event":"verdict","detail":{{"verdict":"{verdict}","reason":"drawdown"}}}}"#,
                chrono::Utc::now().to_rfc3339()
            ));
    }
    lines.push(format!(
        r#"{{"at":"{}","event":"stopped","detail":null}}"#,
        chrono::Utc::now().to_rfc3339()
    ));
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn real_money_is_refused_without_a_paper_session_and_the_refusal_names_the_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    let refused = sessions
        .start("f-1", "alpaca-live")
        .expect_err("no paper session");
    assert!(refused.starts_with("promotion gate:"), "{refused}");
    assert!(refused.contains("no paper session"), "{refused}");
    assert!(
        refused.contains("cannot be opened"),
        "every reason, not the first: {refused}"
    );
    assert!(sessions.list().is_empty(), "refused before a thread starts");
    // Paper needs no promotion: this one fails in its thread on the
    // missing finding, which is the next test's business.
    assert!(sessions.start("f-1", "alpaca-paper").is_ok());
}

#[test]
fn a_paper_session_too_short_or_diverging_does_not_promote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    paper_record(dir.path(), "f-1", PAPER_MINIMUM_DAYS - 1, Some("holding"));
    let short = sessions
        .start("f-1", "robinhood-1234")
        .expect_err("too short");
    assert!(
        short.contains(&format!(
            "ran {} day(s); {PAPER_MINIMUM_DAYS} are needed",
            PAPER_MINIMUM_DAYS - 1
        )),
        "{short}"
    );
    assert!(!short.contains("diverging"), "{short}");

    paper_record(dir.path(), "f-1", PAPER_MINIMUM_DAYS + 2, Some("diverging"));
    let diverging = sessions
        .start("f-1", "robinhood-1234")
        .expect_err("diverging");
    assert!(
        diverging.contains("was diverging from the finding when last judged (drawdown)"),
        "{diverging}"
    );
    assert!(!diverging.contains("day(s)"), "long enough: {diverging}");

    // Long enough and holding: only the finding itself stands in the way
    // here, since this store has none.
    paper_record(dir.path(), "f-1", PAPER_MINIMUM_DAYS, Some("holding"));
    let only_the_finding = sessions
        .start("f-1", "robinhood-1234")
        .expect_err("no finding");
    assert!(
        only_the_finding.contains("cannot be opened"),
        "{only_the_finding}"
    );
    assert!(
        !only_the_finding.contains("paper"),
        "the paper record passed: {only_the_finding}"
    );

    // Asked rather than tried: the same answer, with what the gate saw.
    let asked = sessions.promotion("f-1", "robinhood-1234").unwrap();
    assert!(!asked.allowed);
    assert_eq!(asked.reasons.len(), 1, "{:?}", asked.reasons);
    assert_eq!(asked.paper_days, Some(PAPER_MINIMUM_DAYS));
    assert_eq!(asked.paper_verdict.as_deref(), Some("holding"));
    assert_eq!(asked.verdict, None, "no finding to open");
    let paper = sessions.promotion("f-1", "alpaca-paper").unwrap();
    assert!(
        paper.allowed && paper.reasons.is_empty(),
        "paper needs no promotion"
    );
    assert_eq!(
        paper.paper_days,
        Some(PAPER_MINIMUM_DAYS),
        "but the road ahead is still shown"
    );
    assert!(sessions.promotion("f-1", "etrade").is_err());
}

#[test]
fn a_missing_finding_fails_the_session_rather_than_the_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    let started = sessions.start("nope", "alpaca-paper").expect("starts");
    assert_eq!(started.state, "starting");
    let stopped = sessions.stop(&started.id).expect("joins");
    assert_eq!(stopped.state, "failed");
    assert!(stopped.last_error.is_some());
    assert_eq!(sessions.list().len(), 1);
}

#[test]
fn only_a_live_session_takes_the_kill_switch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    let started = sessions.start("nope", "alpaca-paper").expect("starts");
    sessions.stop(&started.id).expect("joins");
    let refused = sessions.halt(&started.id, "").expect_err("not running");
    assert!(refused.contains("nothing to halt"), "{refused}");
}

#[test]
fn the_watch_builds_round_trips_from_fills_and_marks_the_open_loss() {
    use arvo_execution::{OrderId, Side};
    use arvo_research::live::Reason;
    let at = chrono::NaiveDate::from_ymd_opt(2026, 9, 21)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap();
    let fill = |order: &str, side: Side, quantity: f64, price: f64| Execution {
        order: OrderId(order.to_owned()),
        instrument: "AAPL.AIEX".to_owned(),
        side,
        proposer: "shadow:test".to_owned(),
        quantity,
        decision_price: price,
        fill_price: price,
        decision_at: at,
        filled_at: at,
    };
    let expected = Expectation {
        trades: 40,
        expectancy: 10.0,
        deviation: 4.0,
        max_drawdown: 0.05,
        entries_per_bar: 0.05,
        regimes: ["ranging".to_owned()].into_iter().collect(),
        slippage_bps: 5.0,
    };
    let mut watch = Watch::new(Some(expected), 10_000.0);
    assert!(
        watch.judge().is_none(),
        "inconclusive is where it starts, so nothing changed"
    );

    watch.entered("a".to_owned(), Some("ranging".to_owned()));
    watch.entered("b".to_owned(), Some("trending up".to_owned()));
    let mut fills = vec![fill("a", Side::Buy, 10.0, 100.0)];
    watch.settle(&fills, &Divergence::of(&fills, 0, Some(5.0)), Some(100.0));
    assert_eq!(watch.seen.entries, vec![Some("ranging".to_owned())]);
    assert!(watch.seen.pnls.is_empty(), "still open");

    // The open position falls 8% of the account before it is sold: the
    // drawdown sees it while it is open, and that alone is a verdict.
    watch.bar(20.0);
    assert!(watch.seen.drawdown > 0.079, "{}", watch.seen.drawdown);
    assert!(matches!(
        watch.judge(),
        Some(Live::Diverging(Reason::Drawdown { .. }))
    ));

    fills.push(fill("x", Side::Sell, 10.0, 90.0));
    fills.push(fill("b", Side::Buy, 5.0, 50.0));
    fills.push(fill("y", Side::Sell, 5.0, 52.0));
    watch.settle(&fills, &Divergence::of(&fills, 0, Some(5.0)), Some(52.0));
    assert_eq!(watch.seen.pnls, vec![-100.0, 10.0]);
    assert_eq!(
        watch.seen.entries,
        vec![Some("ranging".to_owned()), Some("trending up".to_owned())]
    );
    assert_eq!(watch.seen.fills, 4);
    assert!(
        watch.judge().is_none(),
        "still diverging on the drawdown; no change to announce"
    );
}

#[test]
fn the_watch_reports_a_warning_once_per_limit_entered_or_cleared() {
    let mut watch = Watch::new(None, 10_000.0);
    let near = |limit: &str, used: f64| Warning {
        limit: limit.to_owned(),
        used,
        allowed: 0.10,
    };
    assert!(watch.warned(&[]).is_none(), "near nothing, as before");
    let first = watch.warned(&[near("drawdown", 0.081)]).expect("entered");
    assert_eq!(first.entered, vec!["drawdown".to_owned()]);
    assert!(first.cleared.is_empty());
    assert!(first.now[0].starts_with("drawdown 8.1%"), "{:?}", first.now);
    // The figure moved; the limit did not. Nothing to record.
    assert!(watch.warned(&[near("drawdown", 0.085)]).is_none());
    let second = watch
        .warned(&[near("positions", 4.0)])
        .expect("one in, one out");
    assert_eq!(second.entered, vec!["positions".to_owned()]);
    assert_eq!(second.cleared, vec!["drawdown".to_owned()]);
    let last = watch.warned(&[]).expect("cleared");
    assert!(last.entered.is_empty());
    assert_eq!(last.cleared, vec!["positions".to_owned()]);
    assert!(last.now.is_empty());
}

#[test]
fn only_a_frozen_session_takes_a_reconcile_or_a_resume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0, std::sync::Arc::new(TestVenues));
    let started = sessions.start("nope", "alpaca-paper").expect("starts");
    sessions.stop(&started.id).expect("joins");
    let refused = sessions.reconcile(&started.id).expect_err("not frozen");
    assert!(refused.contains("failed, not frozen"), "{refused}");
    assert!(sessions.resume(&started.id).is_err());
    assert!(sessions
        .resume("nobody")
        .expect_err("unknown")
        .contains("no session"));
}

/// A venue that records what it was sent and refuses anything stale.
///
/// The two-minute limit is the real one: a broker rejects an order whose
/// decision is older than that, which is how #231 showed itself — the engine
/// cancelled its own sells because every exit arrived already expired.
#[derive(Clone)]
struct FakeVenue {
    /// Shared with the test, which reads it after `act` returns.
    sent: std::sync::Arc<std::sync::Mutex<Vec<Order>>>,
    stale_after: chrono::Duration,
}

impl FakeVenue {
    fn new() -> Self {
        Self {
            sent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            stale_after: chrono::Duration::minutes(2),
        }
    }

    fn orders(&self) -> Vec<Order> {
        self.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

#[async_trait::async_trait]
impl Executor for FakeVenue {
    fn venue(&self) -> &str {
        "fake"
    }

    async fn submit(&self, order: &Order) -> Result<OrderId, arvo_execution::ExecutionError> {
        let age = chrono::Utc::now().naive_utc() - order.decision_at;
        if age > self.stale_after {
            return Err(arvo_execution::ExecutionError::Rejected {
                venue: "fake".to_owned(),
                reason: format!(
                    "decided {}s ago, past the {}s limit",
                    age.num_seconds(),
                    self.stale_after.num_seconds()
                ),
            });
        }
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(order.clone());
        Ok(OrderId("fake-1".to_owned()))
    }

    async fn drain(&self) -> Result<(Vec<Execution>, usize), arvo_execution::ExecutionError> {
        Ok((Vec::new(), 0))
    }

    async fn at_venue(&self) -> Result<arvo_execution::VenueState, arvo_execution::ExecutionError> {
        Ok(arvo_execution::VenueState::default())
    }

    async fn cancel(&self, _order: &OrderId) -> Result<(), arvo_execution::ExecutionError> {
        Ok(())
    }
}

/// The names the tests treat as real venues. Mirrors nothing: `Brokers` has its
/// own test for its own naming rules (`arvo_engine::venues`); these tests only
/// need *a* set to refuse an unknown name against.
struct TestVenues;

#[async_trait::async_trait]
impl crate::Venues for TestVenues {
    fn serves(&self, executor: &str) -> bool {
        matches!(executor, "alpaca-paper" | "alpaca-live" | "robinhood-8591")
    }

    fn names(&self) -> Vec<String> {
        ["alpaca-paper", "alpaca-live", "robinhood-<last4>"].iter().map(|n| (*n).to_owned()).collect()
    }

    fn source(&self, venue: &str) -> Result<Box<dyn arvo_data::source::Source>, String> {
        Err(format!("no source for {venue} in tests"))
    }

    async fn executor(&self, executor: &str) -> Result<Box<dyn Executor>, String> {
        Err(format!("no executor {executor} in tests"))
    }
}

/// A `Status` as a session starts, since it has no `Default`.
fn fresh_status() -> Status {
    Status {
        id: "f-1@fake".to_owned(),
        finding: "f-1".to_owned(),
        executor: "fake".to_owned(),
        instrument: String::new(),
        strategy: String::new(),
        started_at: chrono::Utc::now().to_rfc3339(),
        state: "running".to_owned(),
        signals: 0,
        submitted: 0,
        refused: 0,
        fills: 0,
        halted: None,
        last_error: None,
        error_from: None,
        last_bar: None,
        frozen: None,
        reconciled: false,
        divergence: None,
        verdict: String::new(),
        verdict_reason: None,
        warnings: Vec::new(),
    }
}

/// A session holding `quantity` of `instrument`, so an exit has something to sell.
fn holding(instrument: &str, quantity: f64, price: f64) -> RiskGate {
    let day = chrono::Utc::now().date_naive();
    let mut gate = RiskGate::new(RiskModel::default(), 100_000.0, day);
    gate.opened(instrument, quantity, price, day);
    gate
}

#[tokio::test]
async fn an_exit_is_stamped_when_it_is_sent_so_the_venue_does_not_call_it_stale() {
    // #231: exits carried the *bar's* time. A five-minute bar is stamped at its
    // open, so by the time the order reached the venue it was already past the
    // two-minute staleness limit and was refused — and the next poll cancelled
    // it, while the rule believed it was flat. The account stayed long.
    //
    // This is the test that did not exist: `act` is driven against a venue that
    // enforces the real limit, with a signal whose bar is old.
    let venue = FakeVenue::new();
    let mut session = Session::new(holding("AAPL.NASDAQ", 10.0, 100.0), venue.clone());
    let mut watch = Watch::new(None, 100_000.0);
    let dir = tempfile::tempdir().expect("tempdir");
    let record = Recorder::open(dir.path(), "exit@fake").expect("record");
    let status = Mutex::new(fresh_status());

    let now = chrono::Utc::now().naive_utc();
    let bar_at = now - chrono::Duration::minutes(5);
    let signal = Signal {
        instrument: "AAPL.NASDAQ".to_owned(),
        side: Side::Sell,
        quantity: 10.0,
        reference_price: 100.0,
        stop_distance: None,
        signalled_at: bar_at,
        exit: Some("the rule says out".to_owned()),
        rule: Some("test".to_owned()),
        signal: None,
        regime: None,
    };

    act(&mut session, &mut watch, "s#0", &signal, "test", now, None, &record, &status)
        .await
        .expect("the exit is sent");

    let orders = venue.orders();
    assert_eq!(orders.len(), 1, "the exit reached the venue and was not called stale");
    assert_eq!(
        orders[0].decision_at, now,
        "stamped when sent, not when the bar opened — passing {bar_at} here is #231"
    );
}

#[test]
fn a_bar_the_session_caught_up_on_is_held_and_so_is_a_frozen_one() {
    // #224: `caught_up` was correct and tested; nothing asked it. The decision
    // now has a name, so the policy is testable rather than buried in the poll
    // loop where it could only be found by watching a live session — as it was,
    // when a mid-day start replayed yesterday's bars and filled 297 AAPL.
    let interval = chrono::Duration::minutes(5);
    let started = chrono::NaiveDate::from_ymd_opt(2026, 9, 24)
        .expect("a date")
        .and_hms_opt(14, 11, 0)
        .expect("a time");

    let yesterday = chrono::NaiveDate::from_ymd_opt(2026, 9, 23)
        .expect("a date")
        .and_hms_opt(13, 45, 0)
        .expect("a time");
    assert_eq!(
        held_for(false, yesterday, interval, started),
        Some("catch-up: the bar closed before this session started; the rule is warmed on it, not traded"),
        "yesterday's bar must not be traded on today's market"
    );

    // A bar that closed after the session started is this session's to trade.
    let live = started + chrono::Duration::minutes(1);
    assert_eq!(held_for(false, live, interval, started), None, "a live bar is tradeable");

    // Frozen outranks everything: the book disagrees with the venue.
    let frozen = held_for(true, live, interval, started).expect("frozen holds");
    assert!(frozen.starts_with("frozen:"), "{frozen}");
}

#[tokio::test]
async fn a_held_entry_never_reaches_the_venue() {
    // The other half of #224: whatever `held_for` decides, `act` must honour it
    // for an entry. Driven against a venue that would happily accept the order,
    // so a failure here means a real submission.
    let venue = FakeVenue::new();
    let mut session = Session::new(
        RiskGate::new(RiskModel::default(), 100_000.0, chrono::Utc::now().date_naive()),
        venue.clone(),
    );
    let mut watch = Watch::new(None, 100_000.0);
    let dir = tempfile::tempdir().expect("tempdir");
    let record = Recorder::open(dir.path(), "entry@fake").expect("record");
    let status = Mutex::new(fresh_status());

    let now = chrono::Utc::now().naive_utc();
    let signal = Signal {
        instrument: "AAPL.NASDAQ".to_owned(),
        side: Side::Buy,
        quantity: 297.0,
        reference_price: 100.0,
        stop_distance: Some(2.0),
        signalled_at: now,
        exit: None,
        rule: Some("test".to_owned()),
        signal: None,
        regime: None,
    };

    act(
        &mut session,
        &mut watch,
        "s#0",
        &signal,
        "test",
        now,
        Some("catch-up: the bar closed before this session started"),
        &record,
        &status,
    )
    .await
    .expect("a held entry is not an error");

    assert!(venue.orders().is_empty(), "a held entry must not be sent");
    assert_eq!(
        status.lock().expect("status").refused,
        1,
        "and the refusal is counted, not silent"
    );
}

/// A coin's fraction survives the live path, not just the backtest.
///
/// #248. #245 proved a fraction reaches a Nautilus order inside a backtest.
/// This is the other half: the same size through the session loop, the risk
/// gate and out to a venue. A rounding anywhere along it would turn 0.0125 of a
/// coin into nothing, and the session would look like it was refusing entries
/// for a reason nobody could find.
#[tokio::test]
async fn a_coins_fraction_reaches_the_venue_through_the_session() {
    let venue = FakeVenue::new();
    let mut session = Session::new(
        RiskGate::new(
            RiskModel {
                risk_per_trade: None,
                max_position_fraction: Some(1.0),
                ..RiskModel::default()
            },
            100_000.0,
            chrono::Utc::now().date_naive(),
        ),
        venue.clone(),
    );
    let mut watch = Watch::new(None, 100_000.0);
    let dir = tempfile::tempdir().expect("tempdir");
    let record = Recorder::open(dir.path(), "coin@fake").expect("record");
    let status = Mutex::new(fresh_status());

    let now = chrono::Utc::now().naive_utc();
    let asked = 0.012_5;
    let signal = Signal {
        instrument: "BTC-USD.ACRYPTO".to_owned(),
        side: Side::Buy,
        quantity: asked,
        reference_price: 84_000.0,
        stop_distance: Some(2_000.0),
        signalled_at: now,
        exit: None,
        rule: Some("test".to_owned()),
        signal: None,
        regime: None,
    };

    act(
        &mut session,
        &mut watch,
        "s#0",
        &signal,
        "test",
        now,
        // Nothing holds it: not frozen, and the bar is this session's.
        None,
        &record,
        &status,
    )
    .await
    .expect("a coin is a tradeable instrument");

    let orders = venue.orders();
    assert_eq!(orders.len(), 1, "the entry reached the venue: {orders:?}");
    assert!(
        (orders[0].quantity - asked).abs() < 1e-9,
        "the fraction survived the gate and the loop: asked {asked}, sent {}",
        orders[0].quantity
    );
    assert_eq!(
        status.lock().expect("status").refused,
        0,
        "and nothing refused it"
    );
}
