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

use crate::promotion::{executor_is_known, is_paper, promotion, Promotion, EXECUTORS, SUBDIR};
use crate::record::Recorder;
use crate::run::run;
use crate::venues::Venues;
use crate::status::{announce, Command, Mailbox, Running, Status};

/// What [`Sessions::stop_all`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stopped {
    /// Sessions whose loop was up and has ended: each record's last line is
    /// `stopped`.
    pub stopped: Vec<String>,
    /// Sessions that had not ended when the wait ran out.
    pub unfinished: Vec<String>,
}

/// `<data>/sessions/hosted.json`: the sessions to bring back when an engine
/// starts (#13). Written from what is running whenever that changes, and left
/// as it is when the engine goes down, so the next engine knows what the last
/// one had up.
pub const HOSTED: &str = "hosted.json";

/// One session the engine is to keep up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hosted {
    pub id: String,
    pub finding: String,
    pub executor: String,
}

/// Writes which sessions are wanted up, from what is running now.
///
/// Only a session that is starting or running. A frozen or halted one is
/// waiting for a person, and a restart that quietly resumed it would undo
/// what the person decided; a failed or stopped one has ended. Nothing is
/// written once the engine has begun to stop: its sessions ending then does
/// not mean they are not wanted.
fn remember(data: &Path, running: &Mutex<BTreeMap<String, Running>>, stopping: &AtomicBool) {
    if stopping.load(Ordering::SeqCst) {
        return;
    }
    let wanted: Vec<Hosted> = running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter_map(|found| {
            let status = found.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            matches!(status.state.as_str(), "starting" | "running").then(|| Hosted {
                id: status.id.clone(),
                finding: status.finding.clone(),
                executor: status.executor.clone(),
            })
        })
        .collect();
    let Ok(text) = serde_json::to_string_pretty(&wanted) else { return };
    let path = data.join(SUBDIR).join(HOSTED);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

/// Every session this engine is hosting.
pub struct Sessions {
    data: PathBuf,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
    /// Set once [`Self::stop_all`] has begun, so the sessions ending on the
    /// way out are not written off as unwanted.
    stopping: Arc<AtomicBool>,
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

/// Whether a session in `state` takes the kill switch: one whose loop is
/// still up — live, frozen, or halted with the loop kept running to settle
/// exits and take exactly this (#11). A stopped or failed session has no loop
/// to flatten anything.
pub(crate) fn takes_the_kill_switch(state: &str) -> bool {
    matches!(state, "starting" | "running" | "frozen" | "halted")
}

/// A session's status before its thread has said anything.
fn starting(id: &str, finding: &str, executor: &str) -> Status {
    Status {
        id: id.to_owned(),
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
    }
}

impl Sessions {
    #[must_use]
    pub fn new(data: &Path, events: broadcast::Sender<EventView>, venues: Arc<dyn Venues>) -> Self {
        Self {
            data: data.to_path_buf(),
            running: Arc::default(),
            stopping: Arc::default(),
            events,
            venues,
        }
    }

    /// Writes which sessions are wanted up; see [`HOSTED`]. Called wherever
    /// that changes, and every few seconds by the engine for the changes a
    /// session makes to itself.
    pub fn remember(&self) {
        remember(&self.data, &self.running, &self.stopping);
    }

    /// Brings back the sessions the last engine had up (#13), as [`HOSTED`]
    /// lists them, and says what became of each: started, or refused and why.
    /// A start is the same start a person makes, promotion gate included, and
    /// it adopts what the venue holds.
    ///
    /// For an engine's start, after [`Self::note_dropped`], and only when no
    /// other engine is serving the same data.
    pub fn restore(&self) -> Vec<(Hosted, Result<Status, String>)> {
        let Ok(text) = std::fs::read_to_string(self.data.join(SUBDIR).join(HOSTED)) else {
            return Vec::new();
        };
        let wanted: Vec<Hosted> = serde_json::from_str(&text).unwrap_or_default();
        wanted.into_iter().map(|hosted| {
            let outcome = self.start(&hosted.finding, &hosted.executor);
            (hosted, outcome)
        }).collect()
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
        let status = Arc::new(Mutex::new(starting(&id, finding, executor)));
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
            let session = id.clone();
            let hosted = Arc::clone(&self.running);
            let stopping = Arc::clone(&self.stopping);
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
                        // A loop that ended this way wrote no `stopped`,
                        // and a record that just stops reads as an engine
                        // that died (#13). Its last line says which.
                        Ok(Err(reason)) => {
                            crate::record::failed(&data, &session, &reason);
                            status.state = "failed".to_owned();
                            status.last_error = Some(reason);
                            status.error_from = Some("failed");
                        }
                        Err(_) => {
                            crate::record::failed(&data, &session, "the session thread panicked");
                            status.state = "failed".to_owned();
                            status.last_error = Some("the session thread panicked".to_owned());
                            status.error_from = Some("panicked");
                        }
                    }
                    announce(&events, &status);
                    drop(status);
                    // Ended on its own: not wanted back. Skipped while the
                    // engine stops, which also keeps this from waiting on a
                    // lock that `stop_all` holds while it waits on this.
                    remember(&data, &hosted, &stopping);
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
        drop(running);
        self.remember();
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
        // The thread is joined with the registry unlocked: on its way out it
        // writes what is still wanted, which needs the registry.
        let (thread, status) = {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let found = running
                .get_mut(id)
                .ok_or_else(|| format!("no session {id}"))?;
            found.stop.store(true, Ordering::SeqCst);
            (found.thread.take(), Arc::clone(&found.status))
        };
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        self.remember();
        let status = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(status)
    }

    /// Asks every session whose loop is still up to stop, and waits up to
    /// `within` for them to do it. For the engine's way out.
    ///
    /// An engine that simply ended took its sessions' threads with it, and a
    /// record that stops mid-day with no `stopped` line reads the same as a
    /// crash (#13). Asked this way each loop finishes its poll and writes the
    /// line. Positions are left as they are, as with [`Self::stop`]: stopping
    /// is not flattening.
    ///
    /// All are asked before any is waited for, so the wait is one poll and not
    /// one poll each. A session that does not end in time is named and left:
    /// the engine is going down either way, and that record will end without
    /// its line, which is the truth about it.
    pub fn stop_all(&self, within: Duration) -> Stopped {
        // What is up now is what the next engine brings back (#13): written
        // once more, then left alone while the loops end.
        self.remember();
        self.stopping.store(true, Ordering::SeqCst);
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only a loop that is still up has anything to stop. One that failed
        // or was stopped already has said so in its own record.
        let live: Vec<String> = running
            .iter()
            .filter(|(_, found)| found.thread.as_ref().is_some_and(|thread| !thread.is_finished()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &live {
            if let Some(found) = running.get(id) {
                found.stop.store(true, Ordering::SeqCst);
            }
        }
        let deadline = std::time::Instant::now() + within;
        let mut done = Stopped::default();
        for id in live {
            let Some(found) = running.get_mut(&id) else { continue };
            let Some(thread) = found.thread.take() else { continue };
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if thread.is_finished() {
                let _ = thread.join();
                done.stopped.push(id);
            } else {
                found.thread = Some(thread);
                done.unfinished.push(id);
            }
        }
        done
    }

    /// Says so about every session an engine left without a last word (#13).
    ///
    /// An engine that is killed, or dies, takes its sessions' threads with
    /// it. Each record stops wherever it was, and the next engine knew nothing
    /// of them: it answered "no sessions" while a venue still held what they
    /// had opened. A record that ends on anything but `stopped`, `failed` or
    /// `dropped` is such a session. It gets a `dropped` line and an event, and
    /// a place in [`Self::list`] until it is started again or this engine
    /// ends.
    ///
    /// The line is stamped with the record's last moment, not this one: that
    /// is when the session ended, and the promotion gate counts a paper
    /// session's days by its record. Nothing is restarted. Starting a session
    /// is a decision, and a start adopts what the venue holds.
    ///
    /// For an engine's start, and only when no other engine is serving the
    /// same data: a session that is running has no last word yet either.
    pub fn note_dropped(&self) -> Vec<Status> {
        let Ok(dir) = std::fs::read_dir(self.data.join(SUBDIR)) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
            .collect();
        paths.sort();
        let mut dropped = Vec::new();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let lines: Vec<serde_json::Value> =
                text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
            let Some(last) = lines.last() else { continue };
            let event = last["event"].as_str().unwrap_or_default().to_owned();
            if matches!(event.as_str(), "stopped" | "failed" | "dropped") {
                continue;
            }
            let now = chrono::Utc::now().to_rfc3339();
            let last_at = last["at"].as_str().unwrap_or(&now).to_owned();
            let started = lines.iter().rev().find(|line| line["event"] == "started");
            let said = |key: &str| started.and_then(|line| line["detail"][key].as_str()).unwrap_or_default().to_owned();
            // A record from before a start named its session has only its
            // file's name to go by, which has lost the id's punctuation.
            let id = Some(said("session")).filter(|id| !id.is_empty()).unwrap_or_else(|| {
                path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default()
            });
            let mut status = starting(&id, &said("finding"), &said("executor"));
            status.started_at = started.and_then(|line| line["at"].as_str()).unwrap_or_default().to_owned();
            status.state = "dropped".to_owned();
            status.last_error = Some(format!(
                "the engine hosting it ended without stopping it; its record's last line is `{event}` at {last_at}. \
                 Whatever it held is still at the venue: start it again and it adopts what the venue holds"
            ));
            status.error_from = Some("dropped");
            // An engine killed mid-write leaves half a line; the next one
            // starts on its own.
            if !text.ends_with('\n') {
                use std::io::Write as _;
                if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) {
                    let _ = writeln!(file);
                }
            }
            Recorder { path }.write_at(last_at, "dropped", Some(serde_json::json!({ "after": event, "noticed_at": now })));
            announce(&self.events, &status);
            self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
                id,
                Running {
                    status: Arc::new(Mutex::new(status.clone())),
                    stop: Arc::default(),
                    mailbox: Mailbox::default(),
                    thread: None,
                },
            );
            dropped.push(status);
        }
        dropped
    }

    /// A session whose thread is `thread`, for a test that needs a loop it
    /// controls rather than one that opens a finding.
    #[cfg(test)]
    pub(crate) fn adopt(&self, id: &str, stop: Arc<AtomicBool>, thread: std::thread::JoinHandle<()>) {
        let mut status = crate::tests::fresh_status();
        status.id = id.to_owned();
        if let Some((finding, executor)) = id.split_once('@') {
            status.finding = finding.to_owned();
            status.executor = executor.to_owned();
        }
        let status = Arc::new(Mutex::new(status));
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned(), Running { status, stop, mailbox: Mailbox::default(), thread: Some(thread) });
    }

    /// Sets an adopted session's state, as its loop would.
    #[cfg(test)]
    pub(crate) fn set_state(&self, id: &str, state: &str) {
        if let Some(found) = self.running.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id) {
            found.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).state = state.to_owned();
        }
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
    /// A session already halted takes it too (#11): arming is a no-op on a
    /// halted gate, and flattening is what a halted account still needs —
    /// a position adopted at start, or held through a drawdown halt, is
    /// otherwise closable only at the venue by hand.
    ///
    /// # Errors
    ///
    /// No session by that id, or one that has stopped or failed.
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
                    if !takes_the_kill_switch(&status.state) {
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
        // A halt is a person's decision, and a halted session is not
        // brought back by a restart.
        self.remember();
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
