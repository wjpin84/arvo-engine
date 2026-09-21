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
//! # Streamed when the source can, polled when it cannot
//!
//! An intraday rule asks the source for a live feed (`Source::stream`, #185)
//! and takes each bar as it closes; the poll still runs once a minute
//! underneath, catching up what the library lacked at start and anything a
//! feed dropped. A feed that goes dark freezes the session the way a book
//! disagreement does — entries wait, exits go — and the freeze lifts on its
//! own when the feed is back, because nothing about the book is in doubt.
//! A daily rule polls, as it always did; a minute either way is nothing to
//! a bar that closes overnight.
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

use arvo_data::source::FeedEvent;
use arvo_data::{BarProvider as _, CsvBars};
use arvo_execution::{Divergence, Execution, Executor, Session};
use arvo_nautilus::{NautilusSimulation, Shadow, Side, Signal};
use arvo_research::live::{judge, Expectation, Live, Observed};
use arvo_research::{DateRange, EvidenceStore, Experiment, Record, RiskGate, Verdict};
use arvo_risk::{Proposal, Warning};
use arvo_api::{EventKindView, EventView, SeverityView};
use serde::Serialize;
use tokio::sync::broadcast;

/// Where a session's record goes, under the data root: one JSON line per
/// event, which is what a later view of "what did the system do" reads.
pub const SUBDIR: &str = "sessions";

/// How a session names its venue: `alpaca-paper`, `alpaca-live`, or
/// `robinhood-<last four of the account>`, since a Robinhood login holds
/// more than one account and a session trades exactly one.
pub const EXECUTORS: &[&str] = &["alpaca-paper", "alpaca-live", "robinhood-<last4>"];

/// How long a finding must have run on paper before real money (#194).
///
/// Calendar days between the paper session's start and its last event.
/// Five is one trading week: long enough for a daily rule to have seen a
/// few bars and for the feed, the fills and the reconciliation to have
/// been exercised, and short enough that it is done rather than skipped.
/// The session verdict (#221) is what says whether those days looked like
/// the finding; this only says they happened.
pub const PAPER_MINIMUM_DAYS: i64 = 5;

/// The one executor that is not real money.
fn is_paper(executor: &str) -> bool {
    executor == "alpaca-paper"
}

/// The promotion gate's answer for one finding (#194): whether it may go to
/// real money, every reason it may not, and what the gate looked at.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Promotion {
    pub allowed: bool,
    pub reasons: Vec<String>,
    /// The finding's own verdict, when it can be opened.
    pub verdict: Option<String>,
    /// Days on paper, by the paper record; `None` without one.
    pub paper_days: Option<i64>,
    /// The paper session's last verdict against the finding, when it has one.
    pub paper_verdict: Option<String>,
}

/// Why a finding may not go to real money: every reason, so the person
/// fixes them all at once rather than one per attempt. Empty when it may.
///
/// The promotion gate (#194): a live executor accepts only a finding whose
/// verdict is Supported and which has run on paper for
/// [`PAPER_MINIMUM_DAYS`] without diverging from itself. The check is here,
/// on the start, for people and agents alike; nothing else creates a
/// session.
fn promotion(data: &Path, finding: &str) -> Promotion {
    let mut reasons = Vec::new();
    let answer = |reasons: Vec<String>, verdict: Option<String>, paper_days: Option<i64>, paper_verdict: Option<String>| Promotion {
        allowed: reasons.is_empty(),
        reasons,
        verdict,
        paper_days,
        paper_verdict,
    };
    let store = EvidenceStore::new(data.join("evidence"));
    let found = match store.open(finding) {
        Ok(stored) => {
            let verdict = stored.record.verdict();
            if verdict != Verdict::Supported {
                reasons.push(format!("the finding's verdict is {verdict:?}, not Supported"));
            }
            Some(format!("{verdict:?}"))
        }
        Err(err) => {
            reasons.push(format!("the finding cannot be opened: {err}"));
            None
        }
    };

    let paper = record_path(data, &format!("{finding}@alpaca-paper"));
    let Ok(text) = std::fs::read_to_string(&paper) else {
        reasons.push(format!("no paper session on this finding; run it on alpaca-paper for {PAPER_MINIMUM_DAYS} days first"));
        return answer(reasons, found, None, None);
    };
    let mut started: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut verdict: Option<(String, Option<String>)> = None;
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let at = event["at"].as_str().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()).map(|at| at.with_timezone(&chrono::Utc));
        match event["event"].as_str() {
            Some("started") if started.is_none() => started = at,
            Some("verdict") => {
                verdict = event["detail"]["verdict"].as_str().map(|name| {
                    (name.to_owned(), event["detail"]["reason"].as_str().map(str::to_owned))
                });
            }
            _ => {}
        }
        last = at.or(last);
    }
    let paper_days = match (started, last) {
        (Some(started), Some(last)) => {
            let days = (last - started).num_days();
            if days < PAPER_MINIMUM_DAYS {
                reasons.push(format!("the paper session ran {days} day(s); {PAPER_MINIMUM_DAYS} are needed"));
            }
            Some(days)
        }
        _ => {
            reasons.push("the paper session's record has no start".to_owned());
            None
        }
    };
    if let Some(("diverging", why)) = verdict.as_ref().map(|(name, why)| (name.as_str(), why)) {
        reasons.push(format!(
            "the paper session was diverging from the finding when last judged{}",
            why.as_ref().map_or(String::new(), |why| format!(" ({why})"))
        ));
    }
    answer(reasons, found, paper_days, verdict.map(|(name, _)| name))
}

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
    /// Frozen is a book the venue disagrees with (#187) or a feed that has
    /// gone dark (#185); `frozen` says which.
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
    /// Whether the rule is still the rule its finding described (#221):
    /// `holding`, `diverging` or `inconclusive`. A judgement, not a limit;
    /// it changes nothing at the gate.
    pub verdict: String,
    /// What was seen against what was expected, while diverging.
    pub verdict_reason: Option<String>,
    /// The gate's limits this session is near (#191), in the gate's words.
    /// Empty when it is near none, and while halted.
    pub warnings: Vec<String>,
    /// What the fills cost against the decision prices (#18), once anything
    /// has filled, with the slippage the finding assumed beside it.
    pub divergence: Option<(Divergence, Option<f64>)>,
}

/// What a person can ask of a running session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// Make the gate's book the venue's.
    Reconcile,
    /// Take entries again. Only after a reconcile.
    Resume,
    /// The kill switch (#127): arm the gate, then flatten. Carries the reason.
    Halt(String),
}

/// Why a session is not taking entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freeze {
    /// The book disagrees with the venue (#187). Lifts on a resume, after a
    /// reconcile.
    Discrepancy { reconciled: bool },
    /// The feed has gone dark (#185). Lifts on its own when it is back, or
    /// on a resume.
    Stale,
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
    /// What the promotion gate would say to [`Self::start`], without
    /// starting (#199). Paper is always allowed; the answer still says what
    /// the gate saw, so a window can show the road ahead.
    ///
    /// # Errors
    ///
    /// An executor not in [`EXECUTORS`].
    pub fn promotion(&self, finding: &str, executor: &str) -> Result<Promotion, String> {
        if !executor_is_known(executor) {
            return Err(format!("no executor {executor:?}; one of {}", EXECUTORS.join(", ")));
        }
        let mut gate = promotion(&self.data, finding);
        if is_paper(executor) {
            gate.allowed = true;
            gate.reasons.clear();
        }
        Ok(gate)
    }

    /// # Errors
    ///
    /// An executor not in [`EXECUTORS`], or a finding already running.
    pub fn start(&self, finding: &str, executor: &str) -> Result<Status, String> {
        if !executor_is_known(executor) {
            return Err(format!("no executor {executor:?}; one of {}", EXECUTORS.join(", ")));
        }
        if !is_paper(executor) {
            let gate = promotion(&self.data, finding);
            if !gate.allowed {
                return Err(format!("promotion gate: {}", gate.reasons.join("; ")));
            }
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
            verdict: "inconclusive".to_owned(),
            verdict_reason: None,
            warnings: Vec::new(),
            divergence: None,
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

    /// The kill switch: arms the gate and flattens what the session holds.
    /// The session stays up, halted, so the exits' fills are still booked
    /// and the record says what the venue would not exit.
    ///
    /// # Errors
    ///
    /// No session by that id, or one that is not running or frozen.
    pub fn halt(&self, id: &str, reason: &str) -> Result<Status, String> {
        let reason = if reason.trim().is_empty() { "a person pressed the kill switch".to_owned() } else { reason.to_owned() };
        self.command(id, Command::Halt(reason))
    }

    fn command(&self, id: &str, command: Command) -> Result<Status, String> {
        let (status, mailbox) = {
            let running = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let found = running.get(id).ok_or_else(|| format!("no session {id}"))?;
            (found.status.clone(), found.mailbox.clone())
        };
        {
            let status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match command {
                Command::Halt(_) => {
                    if !matches!(status.state.as_str(), "starting" | "running" | "frozen") {
                        return Err(format!("{id} is {}; nothing to halt", status.state));
                    }
                }
                Command::Reconcile | Command::Resume => {
                    if status.state != "frozen" {
                        return Err(format!("{id} is {}, not frozen", status.state));
                    }
                    if command == Command::Resume && !status.reconciled {
                        return Err(format!("{id} has not been reconciled; reconcile first"));
                    }
                }
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
/// The experiment a finding names, and what its out-of-sample trades lead a
/// session to expect (#221). Only a study has an out-of-sample ledger to
/// draw the expectation from; a session on anything else is Inconclusive
/// for as long as it runs, and its record says so at the start.
fn experiment_of(store: &EvidenceStore, finding: &str) -> Result<(Experiment, Option<Expectation>), String> {
    let stored = store.open(finding).map_err(|err| err.to_string())?;
    match stored.record {
        Record::Study(study) => {
            let evaluation = &study.out_of_sample_evidence.evaluation;
            let expected = Expectation::of(
                &evaluation.strategy_ledger,
                evaluation.strategy.max_drawdown,
                evaluation.strategy_curve.len(),
                study.selected.costs.slippage_bps,
            );
            Ok((study.selected, expected))
        }
        Record::WalkForward(walk) => Ok((walk.template, None)),
        Record::Reported(reported) => Ok((reported.reported.experiment, None)),
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
    record.write(
        "expectation",
        Some(expected.as_ref().map_or_else(
            || serde_json::json!({ "none": "the finding is not a study, or has no closed out-of-sample trade; the verdict stays inconclusive" }),
            |expected| serde_json::to_value(expected).unwrap_or_default(),
        )),
    );

    match executor {
        "alpaca-paper" => runtime.block_on(drive(
            arvo_alpaca::AlpacaExecutor::paper(),
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
        )),
        "alpaca-live" => runtime.block_on(drive(
            arvo_alpaca::AlpacaExecutor::live(),
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
                expected.clone(),
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
    expected: Option<Expectation>,
    record: &Recorder,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    mailbox: &Mailbox,
    events: &broadcast::Sender<EventView>,
) -> Result<(), String> {
    let now = || chrono::Utc::now().naive_utc();
    let mut watch = Watch::new(expected, experiment.starting_cash);
    let instrument = experiment.instrument.clone();
    let proposer = format!("shadow:{}", experiment.strategy.name);
    // The source that serves the bars says what the instrument is — its
    // lot, tick, hours — and the gate sizes against that (#186).
    let described = source.instrument(symbol);
    record.write("instrument", Some(serde_json::to_value(&described).unwrap_or_default()));
    let mut gate = RiskGate::new(experiment.risk.clone(), experiment.starting_cash, now().date());
    gate.learn(described);
    let mut session = Session::new(gate, executor).against_assumed_slippage_bps(experiment.costs.slippage_bps);

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

    // The feed, when the source has one for this interval; the poll runs
    // underneath either way.
    let mut feed = source.stream(symbol, experiment.interval);
    record.write("feed", Some(serde_json::json!({ "streaming": feed.is_some(), "source": source.id() })));
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
        let command = mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(command) = command {
            match command {
                Command::Reconcile => match session.adopt(now(), venue).await {
                    Ok(corrected) => {
                        let positions: BTreeMap<&String, f64> =
                            session.gate().positions().iter().map(|(instrument, held)| (instrument, held.quantity)).collect();
                        record.write("reconciled", Some(serde_json::json!({ "corrected": corrected, "positions": positions })));
                        if let Some(Freeze::Discrepancy { reconciled }) = &mut frozen {
                            *reconciled = true;
                        }
                        status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).reconciled = true;
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
                    let prices: BTreeMap<String, f64> =
                        session.gate().positions().keys().filter_map(|held| last_close.map(|close| (held.clone(), close))).collect();
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
                    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    status.state = "halted".to_owned();
                    status.halted = Some(if flatten.complete() {
                        reason
                    } else {
                        format!("{reason}; {} position(s) the venue would not exit are still held", flatten.failed.len())
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
            *mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
                        freeze(status, record, events, "frozen", serde_json::json!({ "stale": why }), format!("stale feed: {why}"), true);
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
                Ok(fetched) => fresh.extend(fetched.bars),
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
            let signals = shadow.push(&[(instrument.clone(), bar)]).map_err(|err| err.to_string())?;
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
                let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                status.last_bar = Some(bar.at.to_string());
                status.signals += u32::try_from(signals.len()).unwrap_or(u32::MAX);
            }
            for (n, signal) in signals.iter().enumerate() {
                // The bar's instant and the signal's place in it: unique in
                // the record, and readable back to the bar without a lookup.
                let id = format!("{}#{n}", bar.at);
                act(&mut session, &mut watch, &id, signal, &proposer, now(), frozen.is_some(), record, status).await?;
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
                        let position = session.gate().positions().get(&execution.instrument).map_or(0.0, |held| held.quantity);
                        detail["position"] = serde_json::json!(position);
                        record.write("filled", Some(detail));
                    }
                    status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fills +=
                        u32::try_from(filled).unwrap_or(u32::MAX);
                }
                Ok(_) => {}
                Err(err) => {
                    trouble(status, record, events, "settle_failed", &err);
                }
            }
            let divergence = session.divergence();
            watch.settle(session.executions(), &divergence, last_close);
            if divergence.fills > 0 {
                status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).divergence =
                    Some((divergence, Some(experiment.costs.slippage_bps)));
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
                            .map(|d| format!("{}: gate {} venue {}", d.instrument, d.expected, d.at_venue))
                            .collect::<Vec<_>>()
                            .join("; ");
                        freeze(status, record, events, "frozen", serde_json::json!({ "discrepancies": found }), why, false);
                    }
                    Ok(_) => {}
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
    watch: &mut Watch,
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
        "rule": signal.rule,
        "signal": signal.signal,
        "regime": signal.regime,
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
    match session.propose(&proposal, now, None).await.map_err(|err| err.to_string())? {
        Some(order) => {
            watch.entered(order.to_string(), signal.regime.clone());
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

fn freeze(
    status: &Mutex<Status>,
    record: &Recorder,
    events: &broadcast::Sender<EventView>,
    event: &str,
    detail: serde_json::Value,
    why: String,
    reconciled: bool,
) {
    record.write(event, Some(detail));
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "frozen".to_owned();
    status.frozen = Some(why);
    status.reconciled = reconciled;
    announce(events, &status);
}

fn thaw(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, why: Option<&str>) {
    record.write("resumed", why.map(|why| serde_json::json!(why)));
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "running".to_owned();
    status.frozen = None;
    status.reconciled = false;
    announce(events, &status);
}

/// A poll, a settle, an audit or a reconcile that failed: kept on the status,
/// written to the record, and raised as an alert (#196) — the session is
/// still running, but on stale ground, and nobody watching the window
/// would otherwise know until the next thing broke.
fn trouble(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, event: &str, err: &impl std::fmt::Display) {
    record.write(event, Some(serde_json::json!(err.to_string())));
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    status.last_error = Some(err.to_string());
    announce(events, &status);
}

fn halt(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, why: &str) {
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    status.state = "halted".to_owned();
    status.halted = Some(why.to_owned());
    record.write("halted", Some(serde_json::json!(why)));
    announce(events, &status);
}

/// The verdict changed: on the record, on the status, and — when the rule
/// has left what its finding described — raised as an alert, since nothing
/// else will change. The state does not: a Diverging session keeps trading
/// until a person or the agent decides otherwise (#221).
fn judged(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, verdict: &Live) {
    record.write("verdict", Some(serde_json::to_value(verdict).unwrap_or_default()));
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
struct Warned {
    entered: Vec<String>,
    cleared: Vec<String>,
    /// Everything the session is near now, in the gate's words.
    now: Vec<String>,
}

/// The warning band moved (#191): on the record, on the status, and raised
/// as an alert when a limit was entered, so someone can look before the
/// gate stops the account. Clearing is recorded and not raised.
fn warned(status: &Mutex<Status>, record: &Recorder, events: &broadcast::Sender<EventView>, changed: Warned) {
    record.write(
        "warning",
        Some(serde_json::json!({ "entered": changed.entered, "cleared": changed.cleared, "near": changed.now })),
    );
    let mut status = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// The session's own ledger, kept for its verdict (#221).
///
/// Built from the fills as they settle: a buy opens or adds to the one
/// position a session holds, a sell realises against its average entry,
/// and the position going flat closes a round trip. The regime on each
/// entry is the one its signal carried, remembered by order id when the
/// order was sent. A sell of something this session did not open (an
/// adopted position) is not a round trip of the rule's and is not counted.
struct Watch {
    expected: Option<Expectation>,
    seen: Observed,
    /// Order id to the regime its signal was decided in.
    regimes: BTreeMap<String, Option<String>>,
    /// How many of the session's executions are already folded in.
    folded: usize,
    quantity: f64,
    /// Average entry of what is held.
    entry: f64,
    realised: f64,
    /// Realised so far in the open round trip.
    round_trip: f64,
    peak: f64,
    starting_cash: f64,
    /// What the account is worth at the last mark.
    equity: f64,
    verdict: Live,
    /// The limits the session was near at the last look, by name (#191).
    near: Vec<String>,
}

impl Watch {
    fn new(expected: Option<Expectation>, starting_cash: f64) -> Self {
        Self {
            expected,
            seen: Observed::default(),
            regimes: BTreeMap::new(),
            folded: 0,
            quantity: 0.0,
            entry: 0.0,
            realised: 0.0,
            round_trip: 0.0,
            peak: starting_cash,
            starting_cash,
            equity: starting_cash,
            verdict: Live::Inconclusive,
            near: Vec::new(),
        }
    }

    fn equity(&self) -> f64 {
        self.equity
    }

    /// The change in which limits the session is near, if any. Compared
    /// by limit, not by figure: the figure moves on every bar, and a record
    /// that logged each move would bury the one line that matters.
    fn warned(&mut self, warnings: &[Warning]) -> Option<Warned> {
        let names: Vec<String> = warnings.iter().map(|warning| warning.limit.clone()).collect();
        if names == self.near {
            return None;
        }
        let entered = names.iter().filter(|name| !self.near.contains(name)).cloned().collect();
        let cleared = self.near.iter().filter(|name| !names.contains(name)).cloned().collect();
        self.near = names;
        Some(Warned { entered, cleared, now: warnings.iter().map(ToString::to_string).collect() })
    }

    fn entered(&mut self, order: String, regime: Option<String>) {
        self.regimes.insert(order, regime);
    }

    fn bar(&mut self, close: f64) {
        self.seen.bars += 1;
        self.mark(close);
    }

    fn settle(&mut self, executions: &[Execution], divergence: &Divergence, close: Option<f64>) {
        for execution in &executions[self.folded.min(executions.len())..] {
            match execution.side {
                arvo_execution::Side::Buy => {
                    if self.quantity <= 0.0 {
                        self.seen.entries.push(self.regimes.remove(&execution.order.to_string()).flatten());
                        self.round_trip = 0.0;
                    }
                    let total = self.quantity + execution.quantity;
                    self.entry = (self.entry * self.quantity + execution.fill_price * execution.quantity) / total;
                    self.quantity = total;
                }
                arvo_execution::Side::Sell => {
                    if self.quantity <= 0.0 {
                        continue;
                    }
                    let sold = execution.quantity.min(self.quantity);
                    let pnl = (execution.fill_price - self.entry) * sold;
                    self.realised += pnl;
                    self.round_trip += pnl;
                    self.quantity -= sold;
                    if self.quantity <= 1e-9 {
                        self.quantity = 0.0;
                        self.seen.pnls.push(self.round_trip);
                        self.round_trip = 0.0;
                    }
                }
            }
        }
        self.folded = executions.len();
        self.seen.fills = divergence.fills;
        self.seen.mean_slippage_bps = divergence.mean_slippage_bps;
        if let Some(close) = close {
            self.mark(close);
        }
    }

    /// Marks what is held at the last close, so the drawdown sees an open
    /// loss and not only realised ones.
    fn mark(&mut self, close: f64) {
        let equity = self.starting_cash + self.realised + self.quantity * (close - self.entry);
        self.equity = equity;
        self.peak = self.peak.max(equity);
        if self.starting_cash > 0.0 {
            self.seen.drawdown = self.seen.drawdown.max((self.peak - equity) / self.starting_cash);
        }
    }

    /// The verdict, when it changed.
    fn judge(&mut self) -> Option<&Live> {
        let now = self.expected.as_ref().map_or(Live::Inconclusive, |expected| judge(expected, &self.seen));
        if now == self.verdict {
            return None;
        }
        self.verdict = now;
        Some(&self.verdict)
    }
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

    /// A paper record spanning `days`, ending on the given verdict.
    fn paper_record(data: &Path, finding: &str, days: i64, verdict: Option<&str>) {
        let path = record_path(data, &format!("{finding}@alpaca-paper"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let start = chrono::Utc::now() - chrono::Duration::days(days);
        let mut lines = vec![
            format!(r#"{{"at":"{}","event":"started","detail":null}}"#, start.to_rfc3339()),
            format!(r#"{{"at":"{}","event":"bar","detail":null}}"#, (start + chrono::Duration::days(1)).to_rfc3339()),
        ];
        if let Some(verdict) = verdict {
            lines.push(format!(
                r#"{{"at":"{}","event":"verdict","detail":{{"verdict":"{verdict}","reason":"drawdown"}}}}"#,
                chrono::Utc::now().to_rfc3339()
            ));
        }
        lines.push(format!(r#"{{"at":"{}","event":"stopped","detail":null}}"#, chrono::Utc::now().to_rfc3339()));
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn real_money_is_refused_without_a_paper_session_and_the_refusal_names_the_gate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = Sessions::new(dir.path(), broadcast::channel(16).0);
        let refused = sessions.start("f-1", "alpaca-live").expect_err("no paper session");
        assert!(refused.starts_with("promotion gate:"), "{refused}");
        assert!(refused.contains("no paper session"), "{refused}");
        assert!(refused.contains("cannot be opened"), "every reason, not the first: {refused}");
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
        let short = sessions.start("f-1", "robinhood-1234").expect_err("too short");
        assert!(short.contains(&format!("ran {} day(s); {PAPER_MINIMUM_DAYS} are needed", PAPER_MINIMUM_DAYS - 1)), "{short}");
        assert!(!short.contains("diverging"), "{short}");

        paper_record(dir.path(), "f-1", PAPER_MINIMUM_DAYS + 2, Some("diverging"));
        let diverging = sessions.start("f-1", "robinhood-1234").expect_err("diverging");
        assert!(diverging.contains("was diverging from the finding when last judged (drawdown)"), "{diverging}");
        assert!(!diverging.contains("day(s)"), "long enough: {diverging}");

        // Long enough and holding: only the finding itself stands in the way
        // here, since this store has none.
        paper_record(dir.path(), "f-1", PAPER_MINIMUM_DAYS, Some("holding"));
        let only_the_finding = sessions.start("f-1", "robinhood-1234").expect_err("no finding");
        assert!(only_the_finding.contains("cannot be opened"), "{only_the_finding}");
        assert!(!only_the_finding.contains("paper"), "the paper record passed: {only_the_finding}");

        // Asked rather than tried: the same answer, with what the gate saw.
        let asked = sessions.promotion("f-1", "robinhood-1234").unwrap();
        assert!(!asked.allowed);
        assert_eq!(asked.reasons.len(), 1, "{:?}", asked.reasons);
        assert_eq!(asked.paper_days, Some(PAPER_MINIMUM_DAYS));
        assert_eq!(asked.paper_verdict.as_deref(), Some("holding"));
        assert_eq!(asked.verdict, None, "no finding to open");
        let paper = sessions.promotion("f-1", "alpaca-paper").unwrap();
        assert!(paper.allowed && paper.reasons.is_empty(), "paper needs no promotion");
        assert_eq!(paper.paper_days, Some(PAPER_MINIMUM_DAYS), "but the road ahead is still shown");
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
        let at = chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap().and_hms_opt(14, 0, 0).unwrap();
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
        assert!(watch.judge().is_none(), "inconclusive is where it starts, so nothing changed");

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
        assert!(matches!(watch.judge(), Some(Live::Diverging(Reason::Drawdown { .. }))));

        fills.push(fill("x", Side::Sell, 10.0, 90.0));
        fills.push(fill("b", Side::Buy, 5.0, 50.0));
        fills.push(fill("y", Side::Sell, 5.0, 52.0));
        watch.settle(&fills, &Divergence::of(&fills, 0, Some(5.0)), Some(52.0));
        assert_eq!(watch.seen.pnls, vec![-100.0, 10.0]);
        assert_eq!(watch.seen.entries, vec![Some("ranging".to_owned()), Some("trending up".to_owned())]);
        assert_eq!(watch.seen.fills, 4);
        assert!(watch.judge().is_none(), "still diverging on the drawdown; no change to announce");
    }

    #[test]
    fn the_watch_reports_a_warning_once_per_limit_entered_or_cleared() {
        let mut watch = Watch::new(None, 10_000.0);
        let near = |limit: &str, used: f64| Warning { limit: limit.to_owned(), used, allowed: 0.10 };
        assert!(watch.warned(&[]).is_none(), "near nothing, as before");
        let first = watch.warned(&[near("drawdown", 0.081)]).expect("entered");
        assert_eq!(first.entered, vec!["drawdown".to_owned()]);
        assert!(first.cleared.is_empty());
        assert!(first.now[0].starts_with("drawdown 8.1%"), "{:?}", first.now);
        // The figure moved; the limit did not. Nothing to record.
        assert!(watch.warned(&[near("drawdown", 0.085)]).is_none());
        let second = watch.warned(&[near("positions", 4.0)]).expect("one in, one out");
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
        assert!(sessions.resume("nobody").expect_err("unknown").contains("no session"));
    }
}
