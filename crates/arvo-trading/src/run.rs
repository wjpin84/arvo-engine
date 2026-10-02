//! The thread: wiring a finding to a venue, then polling until told to stop.
//!
//! [`run`] is the composition — it turns the two names in a session's id into a
//! real source and a real executor, warms a shadow engine on the library, and
//! hands both to [`drive`]. [`drive`] is generic over its executor and takes
//! its source behind a trait object, so the loop itself does not know which
//! venue it is talking to.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use arvo_api::EventView;
use arvo_data::source::FeedEvent;
use arvo_data::{BarProvider as _, CsvBars};
use arvo_execution::{Executor, Session};
use arvo_nautilus::{NautilusSimulation, Shadow};
use arvo_research::live::Expectation;
use arvo_research::{DateRange, EvidenceStore, Experiment, RiskGate};
use tokio::sync::broadcast;

use crate::bar::{act, held_for};
use crate::record::Recorder;
use crate::sessions::experiment_of;
use crate::state::{freeze, halt, judged, recovered, thaw, trouble, warned};
use crate::status::{announce, Command, Freeze, Mailbox, Status, POLL};
use crate::venues::Venues;
use crate::watch::Watch;

#[expect(
    clippy::too_many_arguments,
    reason = "one call site; a struct would only rename the arguments"
)]
pub(crate) fn run(
    data: &Path,
    finding: &str,
    executor: &str,
    venues: &dyn Venues,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    mailbox: &Mailbox,
    events: &broadcast::Sender<EventView>,
) -> Result<(), String> {
    let store = EvidenceStore::new(data.join("evidence"));
    let (mut experiment, expected) = experiment_of(&store, finding)?;
    if experiment.instruments().len() != 1 {
        return Err("a session runs one instrument; a book is not hosted yet".to_owned());
    }
    let instrument = experiment.instrument.clone();
    let (symbol, venue) = instrument
        .split_once('.')
        .ok_or_else(|| format!("{instrument} names no venue"))?;
    let today = chrono::Utc::now().date_naive();
    // Warmed on everything the library has from the finding's own start, so
    // the rule stands today where it would have stood had the backtest kept
    // running.
    experiment.window =
        DateRange::new(experiment.window.from, today).map_err(|err| err.to_string())?;
    {
        let mut status = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.instrument = instrument.clone();
        status.strategy = experiment.strategy.name.clone();
    }

    let source = venues.source(venue)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;

    let record = Recorder::open(data, &format!("{finding}@{executor}"))?;
    let library = CsvBars::new(data.join("data"));
    let last_in_library = library
        .bars(
            &instrument,
            experiment.interval,
            experiment.window.from,
            today,
        )
        .map_err(|err| err.to_string())?
        .last()
        .map(|bar| bar.at);
    let mut shadow = NautilusSimulation::new(library)
        .shadow(&experiment)
        .map_err(|err| err.to_string())?;
    record.write("started", Some(serde_json::json!({ "experiment": experiment.id.to_string(), "warm_until": last_in_library })));
    record.write(
        "expectation",
        Some(expected.as_ref().map_or_else(
            || serde_json::json!({ "none": "the finding is not a study, or has no closed out-of-sample trade; the verdict stays inconclusive" }),
            |expected| serde_json::to_value(expected).unwrap_or_default(),
        )),
    );

    // Which venue this is, decided by whoever built the `Venues` — the engine
    // for a broker, a test for a fake. The loop below is the same either way,
    // which is the point: it is now reachable without a brokerage account.
    let executor = runtime.block_on(venues.executor(executor))?;
    runtime.block_on(drive(
        executor,
        &experiment,
        &mut shadow,
        source.as_ref(),
        symbol,
        venue,
        last_in_library,
        expected.clone(),
        &record,
        status,
        stop,
        mailbox,
        events,
    ))
}

/// The loop: reconcile, then poll for bars until asked to stop or halted,
/// auditing the book against the venue each time round.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site; a struct would only rename the arguments"
)]
pub(crate) async fn drive<E: Executor>(
    executor: E,
    experiment: &Experiment,
    shadow: &mut Shadow,
    source: &dyn arvo_data::source::Source,
    symbol: &str,
    venue: &str,
    mut last_pushed: Option<chrono::NaiveDateTime>,
    expected: Option<Expectation>,
    record: &Recorder,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    mailbox: &Mailbox,
    events: &broadcast::Sender<EventView>,
) -> Result<(), String> {
    let now = || chrono::Utc::now().naive_utc();
    let started = now();
    let mut watch = Watch::new(expected, experiment.starting_cash);
    let mut seen_gone = 0usize;
    let instrument = experiment.instrument.clone();
    let proposer = format!("shadow:{}", experiment.strategy.name);
    // The source that serves the bars says what the instrument is — its
    // lot, tick, hours — and the gate sizes against that (#186).
    let described = source.instrument(symbol);
    record.write(
        "instrument",
        Some(serde_json::to_value(&described).unwrap_or_default()),
    );
    let mut gate = RiskGate::new(
        experiment.risk.clone(),
        experiment.starting_cash,
        now().date(),
    );
    gate.learn(described);
    let mut session =
        Session::new(gate, executor).against_assumed_slippage_bps(experiment.costs.slippage_bps);

    // What the venue already holds is adopted and halts the session: a
    // position this rule did not open is one it cannot reason about.
    //
    // Written at every start, flat included, with each holding's entry: the
    // review rebuilds the book from this record, and a position closed by
    // hand between two sessions leaves no sell behind it. An empty `adopted`
    // is the one statement that closes those lots (#9).
    let found = session
        .reconcile(now(), venue)
        .await
        .map_err(|err| err.to_string())?;
    record.write(
        "reconciled",
        Some(serde_json::json!({
            "adopted": found.adopted.iter().map(|h| (&h.symbol, h.quantity, h.entry)).collect::<Vec<_>>(),
            "cancelled": found.cancelled.len(),
            "stranded": found.stranded.len(),
        })),
    );
    if let Some(why) = session.gate().halted() {
        halt(status, record, events, why);
        return Ok(());
    }
    {
        let mut status = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.state = "running".to_owned();
        announce(events, &status);
    }

    // The feed, when the source has one for this interval; the poll runs
    // underneath either way.
    let mut feed = source.stream(symbol, experiment.interval);
    record.write(
        "feed",
        Some(serde_json::json!({ "streaming": feed.is_some(), "source": source.id() })),
    );
    let mut frozen: Option<Freeze> = None;
    let mut last_poll: Option<std::time::Instant> = None;
    // The last close seen, as the reference price for a kill switch's exits.
    let mut last_close: Option<f64> = None;
    // Whether the kill switch fired: the loop then keeps settling the exits
    // rather than ending on the gate's halt like a drawdown does.
    let mut killed = false;
    while !stop.load(Ordering::SeqCst) {
        // Read, then the lock is dropped: the caller polls that lock while
        // the command runs, and a reconcile waits on the venue.
        let command = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(command) = command {
            match command {
                Command::Reconcile => match session.adopt(now(), venue).await {
                    Ok(corrected) => {
                        let positions: BTreeMap<&String, f64> = session
                            .gate()
                            .positions()
                            .iter()
                            .map(|(instrument, held)| (instrument, held.quantity))
                            .collect();
                        record.write("reconciled", Some(serde_json::json!({ "corrected": corrected, "positions": positions })));
                        if let Some(Freeze::Discrepancy { reconciled }) = &mut frozen {
                            *reconciled = true;
                        }
                        status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .reconciled = true;
                        recovered(status, events, "reconcile_failed");
                    }
                    Err(err) => {
                        trouble(status, record, events, "reconcile_failed", &err);
                    }
                },
                Command::Resume => match frozen {
                    Some(Freeze::Discrepancy { reconciled: true } | Freeze::Stale) => {
                        frozen = None;
                        thaw(status, record, events, None);
                    }
                    _ => record.write("resume_refused", Some(serde_json::json!("not reconciled"))),
                },
                Command::Halt(reason) => {
                    // Armed first, then flattened; see `Session::kill`. The
                    // reference prices are the last closes, so the exits'
                    // slippage is measured against something.
                    let prices: BTreeMap<String, f64> = session
                        .gate()
                        .positions()
                        .keys()
                        .filter_map(|held| last_close.map(|close| (held.clone(), close)))
                        .collect();
                    let flatten = session.kill(&reason, &prices, now()).await;
                    killed = true;
                    frozen = None;
                    record.write(
                        "halted",
                        Some(serde_json::json!({
                            "reason": reason,
                            "flattened": flatten.submitted.iter().map(ToString::to_string).collect::<Vec<_>>(),
                            "failed": flatten.failed.iter().map(|(instrument, err)| format!("{instrument}: {err}")).collect::<Vec<_>>(),
                        })),
                    );
                    let mut status = status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    status.state = "halted".to_owned();
                    status.halted = Some(if flatten.complete() {
                        reason
                    } else {
                        format!(
                            "{reason}; {} position(s) the venue would not exit are still held",
                            flatten.failed.len()
                        )
                    });
                    status.frozen = None;
                    status.reconciled = false;
                    announce(events, &status);
                }
            }
            // Cleared only now: the caller waits on this slot, and the status
            // it reads back must already show what the command did. Taking it
            // first let a resume sent straight after a reconcile find the
            // reconcile not yet done.
            *mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }

        let mut fresh: Vec<arvo_data::Bar> = Vec::new();
        // One event from the feed, or a second of nothing. The wait is what
        // paces a streaming session; a polling one sleeps in `pause` below.
        if let Some(live) = &mut feed {
            match tokio::time::timeout(Duration::from_secs(1), live.next()).await {
                Ok(Some(FeedEvent::Bar(bar))) => fresh.push(bar),
                Ok(Some(FeedEvent::Up)) => {
                    record.write("feed_up", None);
                    if matches!(frozen, Some(Freeze::Stale)) {
                        frozen = None;
                        thaw(status, record, events, Some("the feed is back"));
                    }
                }
                Ok(Some(FeedEvent::Down(why))) => {
                    record.write("feed_down", Some(serde_json::json!(why)));
                    if frozen.is_none() {
                        frozen = Some(Freeze::Stale);
                        // Nothing to reconcile: the book is not in doubt, so a
                        // person may resume at once rather than wait for the feed.
                        freeze(
                            status,
                            record,
                            events,
                            "frozen",
                            serde_json::json!({ "stale": why }),
                            format!("stale feed: {why}"),
                            true,
                        );
                    }
                }
                Ok(None) => {
                    record.write("feed_ended", None);
                    feed = None;
                }
                Err(_) => {}
            }
        }

        let due = last_poll.is_none_or(|last| last.elapsed() >= POLL);
        if due {
            last_poll = Some(std::time::Instant::now());
            let today = now().date();
            // From the last bar the rule saw, so a library that stopped a fortnight
            // ago is caught up bar by bar rather than skipped to today; three days
            // back otherwise, so a Monday still sees Friday's bar.
            let from = last_pushed.map_or(today - chrono::Duration::days(3), |last| last.date());
            match source.bars(symbol, experiment.interval, from, today).await {
                Ok(fetched) => {
                    fresh.extend(fetched.bars);
                    recovered(status, events, "fetch_failed");
                }
                Err(err) => {
                    trouble(status, record, events, "fetch_failed", &err);
                }
            }
        }
        fresh.sort_by_key(|bar| bar.at);
        fresh.dedup_by_key(|bar| bar.at);
        fresh.retain(|bar| last_pushed.is_none_or(|last| bar.at > last));
        fresh.retain(|bar| bar.at + experiment.interval.duration() <= now());
        let pushed = !fresh.is_empty();

        for bar in fresh {
            let signals = shadow
                .push(&[(instrument.clone(), bar)])
                .map_err(|err| err.to_string())?;
            last_pushed = Some(bar.at);
            last_close = Some(bar.close);
            watch.bar(bar.close);
            // Marked to market before the signals are acted on, so an open
            // loss reaches the drawdown halt and the warning band on the bar
            // that made it, not on the next fill.
            session.mark(watch.equity());
            if let Some(changed) = watch.warned(&session.gate().warnings(now().date())) {
                warned(status, record, events, changed);
            }
            record.write("bar", Some(serde_json::json!({ "at": bar.at, "close": bar.close, "signals": signals.len() })));
            {
                let mut status = status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                status.last_bar = Some(bar.at.to_string());
                status.signals += u32::try_from(signals.len()).unwrap_or(u32::MAX);
            }
            for (n, signal) in signals.iter().enumerate() {
                // The bar's instant and the signal's place in it: unique in
                // the record, and readable back to the bar without a lookup.
                let id = format!("{}#{n}", bar.at);
                let held = held_for(
                    frozen.is_some(),
                    bar.at,
                    experiment.interval.duration(),
                    started,
                );
                act(
                    &mut session,
                    &mut watch,
                    &id,
                    signal,
                    &proposer,
                    now(),
                    held,
                    record,
                    status,
                )
                .await?;
            }
            if killed {
                // Already halted by hand; the loop stays up to book the exits.
            } else if let Some(why) = session.gate().halted() {
                halt(status, record, events, why);
                return Ok(());
            }
        }

        // The venue is asked on the poll's cadence, or right after a bar that
        // may have sent something; a streaming session's one-second turns do
        // not each cost two broker calls.
        if due || pushed {
            match session.settle().await {
                Ok(filled) if filled > 0 => {
                    let executions = session.executions();
                    for execution in &executions[executions.len() - filled..] {
                        let mut detail = serde_json::to_value(execution).unwrap_or_default();
                        let position = session
                            .gate()
                            .positions()
                            .get(&execution.instrument)
                            .map_or(0.0, |held| held.quantity);
                        detail["position"] = serde_json::json!(position);
                        record.write("filled", Some(detail));
                    }
                    status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .fills += u32::try_from(filled).unwrap_or(u32::MAX);
                    recovered(status, events, "settle_failed");
                }
                Ok(_) => recovered(status, events, "settle_failed"),
                Err(err) => {
                    trouble(status, record, events, "settle_failed", &err);
                }
            }
            let gone = session.gone();
            if gone > seen_gone {
                // An order that ended without a fill is on the record (#231),
                // so a sell that never happened is not a silence.
                record.write(
                    "unfilled",
                    Some(serde_json::json!({
                        "count": gone - seen_gone,
                        "why": "ended at the venue without a fill: cancelled, rejected or expired",
                    })),
                );
                seen_gone = gone;
            }
            let divergence = session.divergence();
            watch.settle(session.executions(), &divergence, last_close);
            if divergence.fills > 0 {
                status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .divergence = Some((divergence, Some(experiment.costs.slippage_bps)));
            }
            session.mark(watch.equity());
            if let Some(changed) = watch.warned(&session.gate().warnings(now().date())) {
                warned(status, record, events, changed);
            }
            if let Some(verdict) = watch.judge() {
                judged(status, record, events, verdict);
            }
            if frozen.is_none() {
                match session.audit(venue).await {
                    Ok(found) if !found.is_empty() => {
                        frozen = Some(Freeze::Discrepancy { reconciled: false });
                        let why = found
                            .iter()
                            .map(|d| {
                                format!(
                                    "{}: gate {} venue {}",
                                    d.instrument, d.expected, d.at_venue
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        freeze(
                            status,
                            record,
                            events,
                            "frozen",
                            serde_json::json!({ "discrepancies": found }),
                            why,
                            false,
                        );
                    }
                    Ok(_) => recovered(status, events, "audit_failed"),
                    Err(err) => {
                        trouble(status, record, events, "audit_failed", &err);
                    }
                }
            }
        }
        if feed.is_none() {
            pause(stop, mailbox).await;
        }
    }
    record.write("stopped", None);
    Ok(())
}

/// Sleeps one poll, waking early when asked to stop or handed a command, so
/// neither is a minute away.
pub(crate) async fn pause(stop: &AtomicBool, mailbox: &Mailbox) {
    let step = Duration::from_secs(1);
    let mut slept = Duration::ZERO;
    while slept < POLL
        && !stop.load(Ordering::SeqCst)
        && mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    {
        tokio::time::sleep(step).await;
        slept += step;
    }
}
