//! The handle: what a caller does to a session from outside its thread.
//!
//! One finding and one executor make one session, keyed `finding@executor`.
//! Everything here either starts a thread or leaves a message for one; none of
//! it touches a venue, which is what makes these refusals testable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arvo_api::EventView;
use arvo_research::live::Expectation;
use arvo_research::{EvidenceStore, Experiment, Record};
use tokio::sync::broadcast;

use crate::promotion::{executor_is_known, is_paper, promotion, Promotion, EXECUTORS};
use crate::run::run;
use crate::venues::Venues;
use crate::status::{announce, Command, Mailbox, Running, Status};

/// Every session this engine is hosting.
pub struct Sessions {
    data: PathBuf,
    running: Mutex<BTreeMap<String, Running>>,
    /// Where state changes go, for whoever is listening (#150). Sent, never
    /// awaited: a session does not wait for the window.
    events: broadcast::Sender<EventView>,
    /// The venues a session may run against. Supplied rather than chosen here,
    /// so the loop can be driven against a fake (see [`crate::venues`]).
    venues: Arc<dyn Venues>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("data", &self.data)
            .finish_non_exhaustive()
    }
}

impl Sessions {
    #[must_use]
    pub fn new(data: &Path, events: broadcast::Sender<EventView>, venues: Arc<dyn Venues>) -> Self {
        Self {
            data: data.to_path_buf(),
            running: Mutex::new(BTreeMap::new()),
            events,
            venues,
        }
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
            return Err(format!(
                "no executor {executor:?}; one of {}",
                EXECUTORS.join(", ")
            ));
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
            return Err(format!(
                "no executor {executor:?}; one of {}",
                EXECUTORS.join(", ")
            ));
        }
        if !is_paper(executor) {
            let gate = promotion(&self.data, finding);
            if !gate.allowed {
                return Err(format!("promotion gate: {}", gate.reasons.join("; ")));
            }
        }
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = format!("{finding}@{executor}");
        if let Some(existing) = running.get(&id) {
            let status = existing
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            error_from: None,
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
        let snapshot = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let thread = {
            let data = self.data.clone();
            let finding = finding.to_owned();
            let executor = executor.to_owned();
            let status = status.clone();
            let stop = stop.clone();
            let mailbox = mailbox.clone();
            let events = self.events.clone();
            let venues = Arc::clone(&self.venues);
            // Its own thread: the shadow's message bus is thread-local
            // (ADR-0001), and a session is a loop that sleeps.
            std::thread::Builder::new()
                .name(id.clone())
                .spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(
                            &data, &finding, &executor, venues.as_ref(), &status, &stop,
                            &mailbox, &events,
                        )
                    }));
                    let mut status = status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match outcome {
                        Ok(Ok(())) => {
                            if status.state != "halted" {
                                status.state = "stopped".to_owned();
                            }
                        }
                        Ok(Err(reason)) => {
                            status.state = "failed".to_owned();
                            status.last_error = Some(reason);
                            status.error_from = Some("failed");
                        }
                        Err(_) => {
                            status.state = "failed".to_owned();
                            status.last_error = Some("the session thread panicked".to_owned());
                            status.error_from = Some("panicked");
                        }
                    }
                    announce(&events, &status);
                })
                .map_err(|err| format!("starting the session thread: {err}"))?
        };
        running.insert(
            id,
            Running {
                status,
                stop,
                mailbox,
                thread: Some(thread),
            },
        );
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
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = running
            .get_mut(id)
            .ok_or_else(|| format!("no session {id}"))?;
        found.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = found.thread.take() {
            let _ = thread.join();
        }
        let status = found
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
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
        let reason = if reason.trim().is_empty() {
            "a person pressed the kill switch".to_owned()
        } else {
            reason.to_owned()
        };
        self.command(id, Command::Halt(reason))
    }

    fn command(&self, id: &str, command: Command) -> Result<Status, String> {
        let (status, mailbox) = {
            let running = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let found = running.get(id).ok_or_else(|| format!("no session {id}"))?;
            (found.status.clone(), found.mailbox.clone())
        };
        {
            let status = status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        *mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(command);
        // The loop takes the command within a pause step. Waiting for that
        // means the status handed back already shows what the command did.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let now = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(now)
    }

    #[must_use]
    pub fn list(&self) -> Vec<Status> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|running| {
                running
                    .status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .collect()
    }
}

/// The experiment a finding ran, for a session to run again from today.
/// The experiment a finding names, and what its out-of-sample trades lead a
/// session to expect (#221). Only a study has an out-of-sample ledger to
/// draw the expectation from; a session on anything else is Inconclusive
/// for as long as it runs, and its record says so at the start.
pub(crate) fn experiment_of(
    store: &EvidenceStore,
    finding: &str,
) -> Result<(Experiment, Option<Expectation>), String> {
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
