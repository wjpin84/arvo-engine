//! Every transition a session can make, and the three places each one lands.
//!
//! A state change is never only a field: it is the status a front end reads, the
//! record a later review replays, and the event a subscriber is told. These all
//! write to all three, which is why they sit together — a transition that
//! updated two of the three is the bug this shape exists to prevent.

use std::sync::Mutex;

use arvo_api::{EventKindView, EventView, SeverityView};
use arvo_research::live::Live;
use tokio::sync::broadcast;

use crate::record::Recorder;
use crate::status::{announce, Status};

pub(crate) fn freeze(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    event: &str,
    detail: serde_json::Value,
    why: String,
    reconciled: bool,
) {
    record.write(event, Some(detail));
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "frozen".to_owned();
    status.frozen = Some(why);
    status.reconciled = reconciled;
    announce(events, &status);
}

pub(crate) fn thaw(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    why: Option<&str>,
) {
    record.write("resumed", why.map(|why| serde_json::json!(why)));
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "running".to_owned();
    status.frozen = None;
    status.reconciled = false;
    announce(events, &status);
}

/// A poll, a settle, an audit or a reconcile that failed: kept on the status,
/// written to the record, and raised as an alert (#196) — the session is
/// still running, but on stale ground, and nobody watching the window
/// would otherwise know until the next thing broke.
pub(crate) fn trouble(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    event: &str,
    err: &impl std::fmt::Display,
) {
    record.write(event, Some(serde_json::json!(err.to_string())));
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.last_error = Some(err.to_string());
    status.error_from = Some(event_name(event));
    announce(events, &status);
}

/// The record's event names are `&'static str` literals at every call site;
/// this narrows one to the set [`recovered`] compares against, so a typo is a
/// compile error rather than an error that never clears.
pub(crate) fn event_name(event: &str) -> &'static str {
    match event {
        "reconcile_failed" => "reconcile_failed",
        "fetch_failed" => "fetch_failed",
        "settle_failed" => "settle_failed",
        "audit_failed" => "audit_failed",
        other => {
            debug_assert!(false, "{other} is not one of the four calls that can fail");
            "failed"
        }
    }
}

/// The call named by `event` succeeded: if that is what the status is holding
/// an error from, it is over and the row stops saying so.
pub(crate) fn recovered(
    status: &Mutex<Status>,
    events: &broadcast::Sender<EventView>,
    event: &'static str,
) {
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if status.error_from == Some(event) {
        status.last_error = None;
        status.error_from = None;
        announce(events, &status);
    }
}

pub(crate) fn halt(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    why: &str,
) {
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "halted".to_owned();
    status.halted = Some(why.to_owned());
    record.write("halted", Some(serde_json::json!(why)));
    announce(events, &status);
}

/// The verdict changed: on the record, on the status, and — when the rule
/// has left what its finding described — raised as an alert, since nothing
/// else will change. The state does not: a Diverging session keeps trading
/// until a person or the agent decides otherwise (#221).
pub(crate) fn judged(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    verdict: &Live,
) {
    record.write(
        "verdict",
        Some(serde_json::to_value(verdict).unwrap_or_default()),
    );
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.verdict = verdict.name().to_owned();
    status.verdict_reason = verdict.reason();
    if let Live::Diverging(reason) = verdict {
        let _ = events.send(EventView::new(
            EventKindView::session(status.id.clone(), status.state.clone()),
            "Session diverging from its finding".to_owned(),
            format!("{}: {reason}", status.id),
            SeverityView::Warning,
        ));
    }
}

/// What changed about the limits a session is near.
pub(crate) struct Warned {
    pub(crate) entered: Vec<String>,
    pub(crate) cleared: Vec<String>,
    /// Everything the session is near now, in the gate's words.
    pub(crate) now: Vec<String>,
}

/// The warning band moved (#191): on the record, on the status, and raised
/// as an alert when a limit was entered, so someone can look before the
/// gate stops the account. Clearing is recorded and not raised.
pub(crate) fn warned(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    changed: Warned,
) {
    record.write(
        "warning",
        Some(serde_json::json!({ "entered": changed.entered, "cleared": changed.cleared, "near": changed.now })),
    );
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.warnings = changed.now;
    if !changed.entered.is_empty() {
        let _ = events.send(EventView::new(
            EventKindView::session(status.id.clone(), status.state.clone()),
            "Session near a limit".to_owned(),
            format!("{}: {}", status.id, status.warnings.join("; ")),
            SeverityView::Warning,
        ));
    }
}
