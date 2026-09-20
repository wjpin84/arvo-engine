//! Live sessions: a finding's rule, running against a venue.
//!
//! # What a session is
//!
//! One finding, one executor, one thread. The finding's experiment is rebuilt
//! as a shadow engine (`arvo_nautilus::Shadow`) warmed on the library up to
//! today; from then on the venue's own source is asked for bars at the
//! experiment's resolution, each completed bar is pushed through the shadow,
//! and what the rule sends comes back as signals. An entry goes to the risk
//! gate and, if it survives, to the venue; an exit goes straight to the venue
//! (ADR-0009: nothing may stop you shedding risk). The same policy, the same
//! rule, the same bars a backtest would have seen — that is the whole point.
//!
//! # Paper is a broker's paper account
//!
//! `alpaca-paper` is Alpaca's paper endpoint: real API, real fills and
//! latency, simulated money. That is what makes its divergence a measurement
//! rather than an assumption. `alpaca-live` and `robinhood` are real money and
//! say so by name.
//!
//! # A bar is accepted only once it is over
//!
//! A vendor serves today's daily bar while today is still trading. Pushing it
//! would decide on a close that has not happened. A bar is pushed only when
//! `at + interval` is in the past, which for a daily rule means the signal
//! fires overnight and fills at the open — the fill the backtest assumed
//! (ADR-0010).
//!
//! # A disagreement with the venue is an incident, not a warning
//!
//! Every poll the gate's book is audited against the venue's (#187). A
//! position the venue reports and the gate does not, or the other way round,
//! means something traded that this rule did not decide, and the rule can no
//! longer size against a book it trusts. The session *freezes*: entries are
//! refused, exits still go out (ADR-0009), bars keep flowing so the rule
//! stays current, and the record names the disagreement. A person reconciles
//! — the gate is made to agree with the venue, since the venue holds the
//! money — and then resumes, and both are events in the record. Nothing
//! resumes on its own: a freeze that lifted itself would be a warning.
//!
//! # The record is a chain
//!
//! Each event names what caused it (#188): a signal its bar, an order its
//! signal, a fill its order and the position it left. `explain` walks it
//! backwards from a position to the bar it was decided on.
//!
//! # Nothing here writes the library
//!
//! Bars fetched for a session are pushed and forgotten. The library is fetched
//! files with a content hash (ADR-0008), and a session's bars have no place in
//! it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arvo_data::{BarProvider as _, CsvBars};
use arvo_execution::{Executor, Session};
use arvo_nautilus::{NautilusSimulation, Shadow, Side, Signal};
use arvo_research::{DateRange, EvidenceStore, Experiment, Record, RiskGate};
use arvo_risk::Proposal;
use arvo_api::EventView;
use serde::Serialize;
use tokio::sync::broadcast;

/// Where a session's record goes, under the data root: one JSON line per
/// event, which is what a later view of "what did the system do" reads.
pub const SUBDIR: &str = "sessions";

/// How a session names its venue: `alpaca-paper`, `alpaca-live`, or
/// `robinhood-<last four of the account>`, since a Robinhood login holds
/// more than one account and a session trades exactly one.
pub const EXECUTORS: &[&str] = &["alpaca-paper", "alpaca-live", "robinhood-<last4>"];

fn executor_is_known(executor: &str) -> bool {
    matches!(executor, "alpaca-paper" | "alpaca-live")
        || executor.strip_prefix("robinhood-").is_some_and(|last| last.len() == 4 && last.chars().all(|c| c.is_ascii_digit()))
}

/// How often a session asks the source for new bars: once a minute, whatever
/// the resolution. A daily rule mostly hears "nothing new"; a 5-minute one
/// hears a bar every fifth ask. One knob, and it is the vendor's rate limit
/// that sets it.
const POLL: Duration = Duration::from_secs(60);

/// What a session is doing, as the window and the CLI see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub id: String,
    pub finding: String,
    pub executor: String,
    pub instrument: String,
    pub strategy: String,
    pub started_at: String,
    /// `starting`, `running`, `frozen`, `halted`, `stopped` or `failed`.
    pub state: String,
    pub signals: u32,
    pub submitted: u32,
    pub refused: u32,
    pub fills: u32,
    /// Why the gate halted, when it did.
    pub halted: Option<String>,
    pub last_error: Option<String>,
    /// When the last bar was pushed, if any.
    pub last_bar: Option<String>,
    /// What the gate and the venue disagreed about, while frozen.
    pub frozen: Option<String>,
    /// Whether the disagreement has been reconciled, so a resume is allowed.
    pub reconciled: bool,
}

/// What a person can ask of a frozen session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Make the gate's book the venue's.
    Reconcile,
    /// Take entries again. Only after a reconcile.
    Resume,
}

/// One slot: the session loop takes what is there at the top of each turn,
/// and the caller waits for the slot to empty.
type Mailbox = Arc<Mutex<Option<Command>>>;

/// One line of a session's record.
#[derive(Debug, Serialize)]
struct Event<'a> {
    at: String,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<serde_json::Value>,
}

struct Running {
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    mailbox: Mailbox,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Every session this engine is hosting.
pub struct Sessions {
    data: PathBuf,
    running: Mutex<BTreeMap<String, Running>>,
    /// Where state changes go, for whoever is listening (#150). Sent, never
    /// awaited: a session does not wait for the window.
    events: broadcast::Sender<EventView>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions").field("data", &self.data).finish_non_exhaustive()
    }
}

impl Sessions {
    #[must_use]
    pub fn new(data: &Path, events: broadcast::Sender<EventView>) -> Self {
        Self { data: data.to_path_buf(), running: Mutex::new(BTreeMap::new()), events }
    }

    /// Starts a session for `finding` against `executor`.
    ///
    /// Returns as soon as the thread is up; the finding is opened and the
    /// shadow warmed on that thread, so a bad finding shows as `failed` in
    /// [`Self::list`] rather than as an error here — a session is something
    /// you watch, not something you await.
    ///
    /// # Errors
    ///
    /// An executor not in [`EXECUTORS`], or a finding already running.
    pub fn start(&self, finding: &str, executor: &str) -> Result<Status, String> {
        if !executor_is_known(executor) {
            return Err(format!("no executor {executor:?}; one of {}", EXECUTORS.join(", ")));
        }
        let mut running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = format!("{finding}@{executor}");
        if let Some(existing) = running.get(&id) {
            let status = existing.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(status.state.as_str(), "starting" | "running") {
                return Err(format!("{id} is already running"));
            }
        }
        let status = Arc::new(Mutex::new(Status {
            id: id.clone(),
            finding: finding.to_owned(),
            executor: executor.to_owned(),
            instrument: String::new(),
            strategy: String::new(),
            started_at: chrono::Utc::now().to_rfc3339(),
            state: "starting".to_owned(),
            signals: 0,
            submitted: 0,
            refused: 0,
            fills: 0,
            halted: None,
            last_error: None,
            last_bar: None,
            frozen: None,
            reconciled: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mailbox: Mailbox = Arc::default();
        let snapshot = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let thread = {
            let data = self.data.clone();
            let finding = finding.to_owned();
            let executor = executor.to_owned();
            let status = status.clone();
            let stop = stop.clone();
            let mailbox = mailbox.clone();
            let events = self.events.clone();
            // Its own thread: the shadow's message bus is thread-local
            // (ADR-0001), and a session is a loop that sleeps.
            std::thread::Builder::new()
                .name(id.clone())
                .spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(&data, &finding, &executor, &status, &stop, &mailbox, &events)
                    }));
                    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match outcome {
                        Ok(Ok(())) => {
                            if status.state != "halted" {
                                status.state = "stopped".to_owned();
                            }
                        }
                        Ok(Err(reason)) => {
                            status.state = "failed".to_owned();
                            status.last_error = Some(reason);
                        }
                        Err(_) => {
                            status.state = "failed".to_owned();
                            status.last_error = Some("the session thread panicked".to_owned());
                        }
                    }
                    announce(&events, &status);
                })
                .map_err(|err| format!("starting the session thread: {err}"))?
        };
        running.insert(id, Running { status, stop, mailbox, thread: Some(thread) });
        Ok(snapshot)
    }

    /// Asks a session to stop after its current poll. Positions are left as
    /// they are: stopping is not flattening, and a stop that sold everything
    /// would be a kill switch nobody asked for.
    ///
    /// # Errors
    ///
    /// No session by that id.
    pub fn stop(&self, id: &str) -> Result<Status, String> {
        let mut running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = running.get_mut(id).ok_or_else(|| format!("no session {id}"))?;
        found.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = found.thread.take() {
            let _ = thread.join();
        }
        let status = found.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        Ok(status)
    }

    /// Makes a frozen session's gate agree with the venue. The session stays
    /// frozen: reconciling is looking, resuming is deciding.
    ///
    /// # Errors
    ///
    /// No session by that id, or one that is not frozen.
    pub fn reconcile(&self, id: &str) -> Result<Status, String> {
        self.command(id, Command::Reconcile)
    }

    /// Lets a frozen session take entries again.
    ///
    /// # Errors
    ///
    /// No session by that id, one that is not frozen, or one not yet
    /// reconciled: resuming against a book the venue disagrees with is the
    /// state the freeze exists to prevent.
    pub fn resume(&self, id: &str) -> Result<Status, String> {
        self.command(id, Command::Resume)
    }

    fn command(&self, id: &str, command: Command) -> Result<Status, String> {
        let (status, mailbox) = {
            let running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let found = running.get(id).ok_or_else(|| format!("no session {id}"))?;
            (found.status.clone(), found.mailbox.clone())
        };
        {
            let status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if status.state != "frozen" {
                return Err(format!("{id} is {}, not frozen", status.state));
            }
            if command == Command::Resume && !status.reconciled {
                return Err(format!("{id} has not been reconciled; reconcile first"));
            }
        }
        *mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(command);
        // The loop takes the command within a pause step. Waiting for that
        // means the status handed back already shows what the command did.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let now = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        Ok(now)
    }

    #[must_use]
    pub fn list(&self) -> Vec<Status> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|running| running.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone())
            .collect()
    }
}

/// The experiment a finding ran, for a session to run again from today.
fn experiment_of(store: &EvidenceStore, finding: &str) -> Result<Experiment, String> {
    let stored = store.open(finding).map_err(|err| err.to_string())?;
    match stored.record {
        Record::Study(study) => Ok(study.selected),
        Record::WalkForward(walk) => Ok(walk.template),
        Record::Reported(reported) => Ok(reported.reported.experiment),
        // ponytail: a panel selects one parameter set over many instruments;
        // add when someone wants to trade a panel rather than one of its members.
        Record::Panel(_) => Err("a panel finding names no single experiment to run".to_owned()),
    }
}

fn run(
    data: &Path,
    finding: &str,
    executor: &str,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    mailbox: &Mailbox,
    events: &broadcast::Sender<EventView>,
) -> Result<(), String> {
    let store = EvidenceStore::new(&data.join("evidence"));
    let mut experiment = experiment_of(&store, finding)?;
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
    experiment.window = DateRange::new(experiment.window.from, today).map_err(|err| err.to_string())?;
    {
        let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        status.instrument = instrument.clone();
        status.strategy = experiment.strategy.name.clone();
    }

    let source = arvo_service::source::all()
        .into_iter()
        .find(|source| source.venue() == venue)
        .ok_or_else(|| format!("no source serves venue {venue}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;

    let record = Recorder::open(data, &format!("{finding}@{executor}"))?;
    let library = CsvBars::new(&data.join("data"));
    let last_in_library = library
        .bars(&instrument, experiment.interval, experiment.window.from, today)
        .map_err(|err| err.to_string())?
        .last()
        .map(|bar| bar.at);
    let mut shadow = NautilusSimulation::new(library).shadow(&experiment).map_err(|err| err.to_string())?;
    record.write("started", Some(serde_json::json!({ "experiment": experiment.id.to_string(), "warm_until": last_in_library })));

    match executor {
        "alpaca-paper" => runtime.block_on(drive(
            arvo_alpaca::AlpacaExecutor::paper(),
            &experiment,
            &mut shadow,
            source.as_ref(),
            symbol,
            venue,
            last_in_library,
            &record,
            status,
            stop,
            mailbox,
            events,
        )),
        "alpaca-live" => runtime.block_on(drive(
            arvo_alpaca::AlpacaExecutor::live(),
            &experiment,
            &mut shadow,
            source.as_ref(),
            symbol,
            venue,
            last_in_library,
            &record,
            status,
            stop,
            mailbox,
            events,
        )),
        robinhood if robinhood.starts_with("robinhood-") => {
            let last4 = &robinhood["robinhood-".len()..];
            let account = runtime
                .block_on(arvo_robinhood::Robinhood.holdings())
                .map_err(|err| err.to_string())?
                .into_iter()
                .map(|held| held.account_number)
                .find(|number| number.ends_with(last4))
                .ok_or_else(|| format!("no Robinhood account ends in {last4}"))?;
            runtime.block_on(drive(
                arvo_robinhood::RobinhoodExecutor::new(account),
                &experiment,
                &mut shadow,
                source.as_ref(),
                symbol,
                venue,
                last_in_library,
                &record,
                status,
                stop,
                mailbox,
                events,
            ))
        }
        other => Err(format!("no executor {other:?}")),
    }
}

/// The loop: reconcile, then poll for bars until asked to stop or halted,
/// auditing the book against the venue each time round.
#[expect(clippy::too_many_arguments, reason = "one call site; a struct would only rename the arguments")]
async fn drive<E: Executor>(
    executor: E,
    experiment: &Experiment,
    shadow: &mut Shadow,
    source: &dyn arvo_data::source::Source,
    symbol: &str,
    venue: &str,
    mut last_pushed: Option<chrono::NaiveDateTime>,
    record: &Recorder,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    mailbox: &Mailbox,
    events: &broadcast::Sender<EventView>,
) -> Result<(), String> {
    let now = || chrono::Utc::now().naive_utc();
    let instrument = experiment.instrument.clone();
    let proposer = format!("shadow:{}", experiment.strategy.name);
    let mut session = Session::new(
        RiskGate::new(experiment.risk.clone(), experiment.starting_cash, now().date()),
        executor,
    )
    .against_assumed_slippage_bps(experiment.costs.slippage_bps);

    // What the venue already holds is adopted and halts the session: a
    // position this rule did not open is one it cannot reason about.
    let found = session.reconcile(now(), venue).await.map_err(|err| err.to_string())?;
    if found.found_anything() {
        record.write(
            "reconciled",
            Some(serde_json::json!({
                "adopted": found.adopted.iter().map(|h| (&h.symbol, h.quantity)).collect::<Vec<_>>(),
                "cancelled": found.cancelled.len(),
                "stranded": found.stranded.len(),
            })),
        );
    }
    if let Some(why) = session.gate().halted() {
        halt(status, record, events, why);
        return Ok(());
    }
    {
        let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        status.state = "running".to_owned();
        announce(events, &status);
    }

    // `Some` while frozen; the flag says whether a reconcile has happened.
    let mut frozen: Option<bool> = None;
    while !stop.load(Ordering::SeqCst) {
        // Taken, then the lock is dropped: the caller polls that lock while
        // the command runs, and a reconcile waits on the venue.
        let command = mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(command) = command {
            match command {
                Command::Reconcile => match session.adopt(now(), venue).await {
                    Ok(corrected) => {
                        let positions: BTreeMap<&String, f64> =
                            session.gate().positions().iter().map(|(instrument, held)| (instrument, held.quantity)).collect();
                        record.write("reconciled", Some(serde_json::json!({ "corrected": corrected, "positions": positions })));
                        frozen = Some(true);
                        status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).reconciled = true;
                    }
                    Err(err) => {
                        status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(err.to_string());
                        record.write("reconcile_failed", Some(serde_json::json!(err.to_string())));
                    }
                },
                Command::Resume => {
                    if frozen == Some(true) {
                        frozen = None;
                        record.write("resumed", None);
                        let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        status.state = "running".to_owned();
                        status.frozen = None;
                        status.reconciled = false;
                        announce(events, &status);
                    } else {
                        record.write("resume_refused", Some(serde_json::json!("not reconciled")));
                    }
                }
            }
        }
        let today = now().date();
        // From the last bar the rule saw, so a library that stopped a fortnight
        // ago is caught up bar by bar rather than skipped to today; three days
        // back otherwise, so a Monday still sees Friday's bar.
        let from = last_pushed.map_or(today - chrono::Duration::days(3), |last| last.date());
        let fetched = match source.bars(symbol, experiment.interval, from, today).await {
            Ok(fetched) => fetched.bars,
            Err(err) => {
                status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(err.to_string());
                record.write("fetch_failed", Some(serde_json::json!(err.to_string())));
                pause(stop, mailbox).await;
                continue;
            }
        };
        let fresh: Vec<_> = fetched
            .into_iter()
            .filter(|bar| last_pushed.map_or(true, |last| bar.at > last))
            .filter(|bar| bar.at + experiment.interval.duration() <= now())
            .collect();

        for bar in fresh {
            let signals = shadow.push(&[(instrument.clone(), bar.clone())]).map_err(|err| err.to_string())?;
            last_pushed = Some(bar.at);
            record.write("bar", Some(serde_json::json!({ "at": bar.at, "close": bar.close, "signals": signals.len() })));
            {
                let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                status.last_bar = Some(bar.at.to_string());
                status.signals += u32::try_from(signals.len()).unwrap_or(u32::MAX);
            }
            for (n, signal) in signals.iter().enumerate() {
                // The bar's instant and the signal's place in it: unique in
                // the record, and readable back to the bar without a lookup.
                let id = format!("{}#{n}", bar.at);
                act(&mut session, &id, signal, &proposer, now(), frozen.is_some(), record, status).await?;
            }
            if let Some(why) = session.gate().halted() {
                halt(status, record, events, why);
                return Ok(());
            }
        }

        match session.settle().await {
            Ok(filled) if filled > 0 => {
                let executions = session.executions();
                for execution in &executions[executions.len() - filled..] {
                    let mut detail = serde_json::to_value(execution).unwrap_or_default();
                    let position = session.gate().positions().get(&execution.instrument).map_or(0.0, |held| held.quantity);
                    detail["position"] = serde_json::json!(position);
                    record.write("filled", Some(detail));
                }
                status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fills +=
                    u32::try_from(filled).unwrap_or(u32::MAX);
            }
            Ok(_) => {}
            Err(err) => {
                status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(err.to_string());
                record.write("settle_failed", Some(serde_json::json!(err.to_string())));
            }
        }
        if frozen.is_none() {
            match session.audit(venue).await {
                Ok(found) if !found.is_empty() => {
                    frozen = Some(false);
                    let why = found
                        .iter()
                        .map(|d| format!("{}: gate {} venue {}", d.instrument, d.expected, d.at_venue))
                        .collect::<Vec<_>>()
                        .join("; ");
                    record.write("frozen", Some(serde_json::json!({ "discrepancies": found })));
                    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    status.state = "frozen".to_owned();
                    status.frozen = Some(why);
                    status.reconciled = false;
                    announce(events, &status);
                }
                Ok(_) => {}
                Err(err) => {
                    status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last_error = Some(err.to_string());
                    record.write("audit_failed", Some(serde_json::json!(err.to_string())));
                }
            }
        }
        pause(stop, mailbox).await;
    }
    record.write("stopped", None);
    Ok(())
}

/// Sleeps one poll, waking early when asked to stop or handed a command, so
/// neither is a minute away.
async fn pause(stop: &AtomicBool, mailbox: &Mailbox) {
    let step = Duration::from_secs(1);
    let mut slept = Duration::ZERO;
    while slept < POLL
        && !stop.load(Ordering::SeqCst)
        && mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_none()
    {
        tokio::time::sleep(step).await;
        slept += step;
    }
}

/// One signal, to the gate or to the venue. While `frozen`, an entry is
/// refused before it reaches the gate; an exit is never refused.
#[expect(clippy::too_many_arguments, reason = "one call site; a struct would only rename the arguments")]
async fn act<E: Executor>(
    session: &mut Session<E>,
    id: &str,
    signal: &Signal,
    proposer: &str,
    now: chrono::NaiveDateTime,
    frozen: bool,
    record: &Recorder,
    status: &Mutex<Status>,
) -> Result<(), String> {
    record.write("signal", Some(serde_json::json!({
        "id": id,
        "bar": signal.signalled_at,
        "side": format!("{:?}", signal.side),
        "quantity": signal.quantity,
        "price": signal.reference_price,
        "at": signal.signalled_at,
        "exit": signal.exit,
    })));
    if let Some(why) = &signal.exit {
        // Exits do not ask the gate (ADR-0009).
        let sent = session
            .close(&signal.instrument, signal.reference_price, signal.signalled_at)
            .await
            .map_err(|err| err.to_string())?;
        record.write("exit", Some(serde_json::json!({ "signal": id, "why": why, "order": sent.as_ref().map(ToString::to_string) })));
        if sent.is_some() {
            status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).submitted += 1;
        }
        return Ok(());
    }
    if signal.side == Side::Sell {
        // ponytail: every hosted rule is long-only; a sell that is not an exit
        // is a short, and the gate's short path is for options.
        record.write("ignored", Some(serde_json::json!({ "signal": id, "why": "a sell to open is not hosted" })));
        return Ok(());
    }
    if frozen {
        record.write("refused", Some(serde_json::json!({ "signal": id, "why": "frozen: the book disagrees with the venue; reconcile and resume" })));
        status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).refused += 1;
        return Ok(());
    }
    let proposal = Proposal {
        instrument: signal.instrument.clone(),
        proposer: proposer.to_owned(),
        signalled_at: signal.signalled_at,
        reference_price: signal.reference_price,
        stop_distance: signal.stop_distance,
        desired_quantity: Some(signal.quantity),
        opens_short: false,
    };
    match session.propose(&proposal, now, None).await.map_err(|err| err.to_string())? {
        Some(order) => {
            record.write("submitted", Some(serde_json::json!({ "signal": id, "order": order.to_string() })));
            status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).submitted += 1;
        }
        None => {
            let why = session.refusals().last().map(|(_, rejection)| format!("{rejection:?}"));
            record.write("refused", Some(serde_json::json!({ "signal": id, "why": why })));
            status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).refused += 1;
        }
    }
    Ok(())
}

fn halt(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, why: &str) {
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "halted".to_owned();
    status.halted = Some(why.to_owned());
    record.write("halted", Some(serde_json::json!(why)));
    announce(events, &status);
}

/// Tells whoever is listening what state a session is in now. A send with
/// no receiver is fine: the engine runs with the window closed.
fn announce(events: &broadcast::Sender<EventView>, status: &Status) {
    let why = status.last_error.as_deref().or(status.halted.as_deref()).or(status.frozen.as_deref());
    let _ = events.send(arvo_service::events::session(&status.id, &status.state, why));
}

/// Appends a session's events to its file.
struct Recorder {
    path: PathBuf,
}

/// `<data>/sessions/<id>.jsonl`, with anything but a letter, digit, `-` or
/// `_` in the id made `_`.
#[must_use]
pub fn record_path(data: &Path, id: &str) -> PathBuf {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    data.join(SUBDIR).join(format!("{safe}.jsonl"))
}

impl Recorder {
    fn open(data: &Path, id: &str) -> Result<Self, String> {
        let path = record_path(data, id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        Ok(Self { path })
    }

    fn write(&self, event: &str, detail: Option<serde_json::Value>) {
        use std::io::Write as _;
        let line = Event { at: chrono::Utc::now().to_rfc3339(), event, detail };
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) else {
            return;
        };
        if let Ok(text) = serde_json::to_string(&line) {
            let _ = writeln!(file, "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_executor_is_refused_before_a_thread_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
        let refused = sessions.start("f-1", "etrade").expect_err("not an executor");
        assert!(refused.contains("alpaca-paper"), "{refused}");
        assert!(sessions.list().is_empty());
        assert!(executor_is_known("robinhood-8591"));
        assert!(!executor_is_known("robinhood"), "which account?");
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
    fn only_a_frozen_session_takes_a_reconcile_or_a_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
        let started = sessions.start("nope", "alpaca-paper").expect("starts");
        sessions.stop(&started.id).expect("joins");
        let refused = sessions.reconcile(&started.id).expect_err("not frozen");
        assert!(refused.contains("failed, not frozen"), "{refused}");
        assert!(sessions.resume(&started.id).is_err());
        assert!(sessions.resume("nobody").expect_err("unknown").contains("no session"));
    }
}
