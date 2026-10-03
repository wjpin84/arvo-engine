//! What one completed bar does.
//!
//! The bar goes through the shadow engine and whatever the rule says comes back
//! as a signal. An entry goes to the risk gate; an exit goes straight out,
//! because nothing may stop you shedding risk (ADR-0009).

use std::sync::Mutex;

use arvo_execution::{Executor, Session};
use arvo_nautilus::{Side, Signal};
use arvo_risk::Proposal;

use crate::record::Recorder;
use crate::status::Status;
use crate::watch::Watch;

/// Whether a bar closed before the session started (#224). The shadow is
/// warmed on such a bar so the rule stands where it would have, and its
/// signals are on the record, but nothing is traded on it: a session that
/// started at 14:11 must not send yesterday's 13:45 entry to today's market,
/// which it did on 2026-09-22 at 108 bps of slippage.
pub(crate) fn caught_up(
    bar_at: chrono::NaiveDateTime,
    interval: chrono::Duration,
    started: chrono::NaiveDateTime,
) -> bool {
    bar_at + interval <= started
}

/// Why an entry on this bar must not be sent, if it must not.
///
/// The two reasons an entry is refused before it reaches the gate, decided in
/// one place because #224 was not a wrong answer from [`caught_up`] — that was
/// correct and tested — but nothing *asking* it. A named function can be tested;
/// an `if` buried in the poll loop could only be found in a live session, and
/// was.
#[must_use]
pub(crate) fn held_for(
    frozen: bool,
    bar_at: chrono::NaiveDateTime,
    interval: chrono::Duration,
    started: chrono::NaiveDateTime,
) -> Option<&'static str> {
    if frozen {
        Some("frozen: the book disagrees with the venue; reconcile and resume")
    } else if caught_up(bar_at, interval, started) {
        Some("catch-up: the bar closed before this session started; the rule is warmed on it, not traded")
    } else {
        None
    }
}

/// One signal, to the gate or to the venue. With `held` set, an entry is
/// refused before it reaches the gate, for that reason: the session is
/// frozen, or the bar is one it caught up on (#224). An exit is never
/// refused.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site; a struct would only rename the arguments"
)]
pub(crate) async fn act<E: Executor>(
    session: &mut Session<E>,
    watch: &mut Watch,
    id: &str,
    signal: &Signal,
    proposer: &str,
    now: chrono::NaiveDateTime,
    held: Option<&str>,
    record: &Recorder,
    status: &Mutex<Status>,
) -> Result<(), String> {
    record.write(
        "signal",
        Some(serde_json::json!({
            "id": id,
            "bar": signal.signalled_at,
            "side": format!("{:?}", signal.side),
            "quantity": signal.quantity,
            "price": signal.reference_price,
            // Kept so the review can draw the stop the gate watched (#43).
            "stop_distance": signal.stop_distance,
            "at": signal.signalled_at,
            "exit": signal.exit,
            "rule": signal.rule,
            "signal": signal.signal,
            "regime": signal.regime,
        })),
    );
    if let Some(why) = &signal.exit {
        // Exits do not ask the gate (ADR-0009).
        // Stamped now, as an entry is: the stale rule measures from the
        // decision, and a bar is stamped at its open, so an exit carrying its
        // bar's time was five minutes old when sent and the next poll
        // cancelled it (#231). The bar's time is on the `signal` event above.
        let sent = session
            .close(&signal.instrument, signal.reference_price, now)
            .await
            .map_err(|err| err.to_string())?;
        record.write("exit", Some(serde_json::json!({ "signal": id, "why": why, "order": sent.as_ref().map(ToString::to_string) })));
        if sent.is_some() {
            status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .submitted += 1;
        }
        return Ok(());
    }
    if signal.side == Side::Sell {
        // ponytail: every hosted rule is long-only; a sell that is not an exit
        // is a short, and the gate's short path is for options.
        record.write(
            "ignored",
            Some(serde_json::json!({ "signal": id, "why": "a sell to open is not hosted" })),
        );
        return Ok(());
    }
    if let Some(why) = held {
        record.write(
            "refused",
            Some(serde_json::json!({ "signal": id, "why": why })),
        );
        status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refused += 1;
        return Ok(());
    }
    let proposal = Proposal {
        instrument: signal.instrument.clone(),
        proposer: proposer.to_owned(),
        // The instant the shadow produced it — now — not the bar's own time.
        // The gate's staleness limit measures the hop from signal to gate;
        // a bar is stamped at its open, so a 5-minute bar would read as five
        // minutes old and a daily one as a night old, and both were refused.
        // The bar's time is on the record's `signal` event beside this.
        signalled_at: now,
        reference_price: signal.reference_price,
        stop_distance: signal.stop_distance,
        desired_quantity: Some(signal.quantity),
        opens_short: false,
    };
    match session
        .propose(&proposal, now, None)
        .await
        .map_err(|err| err.to_string())?
    {
        Some(order) => {
            watch.entered(order.to_string(), signal.regime.clone());
            record.write(
                "submitted",
                Some(serde_json::json!({ "signal": id, "order": order.to_string() })),
            );
            status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .submitted += 1;
        }
        None => {
            let why = session
                .refusals()
                .last()
                .map(|(_, rejection)| format!("{rejection:?}"));
            record.write(
                "refused",
                Some(serde_json::json!({ "signal": id, "why": why })),
            );
            status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .refused += 1;
        }
    }
    Ok(())
}
