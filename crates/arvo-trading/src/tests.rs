//! The session loop's own tests.
//!
//! Together rather than beside each module because they drive the handle
//! from outside — a promotion refused, an error cleared, a watch built from
//! fills — which is the level the two live bugs of 2026-09-24 escaped at.

use std::path::Path;
use std::sync::Mutex;

use arvo_execution::{Divergence, Execution};
use arvo_research::live::{Expectation, Live};
use arvo_risk::Warning;
use tokio::sync::broadcast;

use crate::bar::caught_up;
use crate::promotion::{executor_is_known, PAPER_MINIMUM_DAYS};
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
    let refused = sessions
        .start("f-1", "etrade")
        .expect_err("not an executor");
    assert!(refused.contains("alpaca-paper"), "{refused}");
    assert!(sessions.list().is_empty());
    assert!(executor_is_known("robinhood-8591"));
    assert!(!executor_is_known("robinhood"), "which account?");
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
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
    let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
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
