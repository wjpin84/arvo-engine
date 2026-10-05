//! What a session's state is, and how a change to it is announced.
//!
//! [`Status`] is the whole of what a front end sees. The rest is how the handle
//! and the thread talk: one slot holding at most one pending command, because a
//! session asked to stop twice is still stopping once.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arvo_api::EventView;
use arvo_execution::Divergence;
use serde::Serialize;
use tokio::sync::broadcast;

/// How often a session asks the source for new bars: once a minute, whatever
/// the resolution. A daily rule mostly hears "nothing new"; a 5-minute one
/// hears a bar every fifth ask. One knob, and it is the vendor's rate limit
/// that sets it.
pub(crate) const POLL: Duration = Duration::from_secs(60);

/// What a session is doing, as the window and the CLI see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub id: String,
    pub finding: String,
    pub executor: String,
    pub instrument: String,
    pub strategy: String,
    pub started_at: String,
    /// `starting`, `running`, `frozen`, `halted`, `stopped`, `failed` or
    /// `dropped`: the engine hosting it ended without stopping it (#13).
    /// Frozen is a book the venue disagrees with (#187) or a feed that has
    /// gone dark (#185); `frozen` says which.
    pub state: String,
    pub signals: u32,
    pub submitted: u32,
    pub refused: u32,
    pub fills: u32,
    /// Why the gate halted, when it did.
    pub halted: Option<String>,
    /// What failed and has not succeeded since. A fetch, a settle, an audit
    /// or a reconcile: the one that failed clears it by succeeding, so a blip
    /// at one in the morning stops being reported at nine as though it were
    /// current. Another call's success does not clear it, or a settle that
    /// keeps failing would be hidden by the next bar that arrived.
    pub last_error: Option<String>,
    /// Which call [`Self::last_error`] came from, in the record's words.
    pub error_from: Option<&'static str>,
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
pub(crate) enum Command {
    /// Make the gate's book the venue's.
    Reconcile,
    /// Take entries again. Only after a reconcile.
    Resume,
    /// The kill switch (#127): arm the gate, then flatten. Carries the reason.
    Halt(String),
}

/// Why a session is not taking entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freeze {
    /// The book disagrees with the venue (#187). Lifts on a resume, after a
    /// reconcile.
    Discrepancy { reconciled: bool },
    /// The feed has gone dark (#185). Lifts on its own when it is back, or
    /// on a resume.
    Stale,
}

/// One slot: the session loop takes what is there at the top of each turn,
/// and the caller waits for the slot to empty.
pub(crate) type Mailbox = Arc<Mutex<Option<Command>>>;

/// One line of a session's record.
#[derive(Debug, Serialize)]
pub(crate) struct Event<'a> {
    pub(crate) at: String,
    pub(crate) event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<serde_json::Value>,
}

pub(crate) struct Running {
    pub(crate) status: Arc<Mutex<Status>>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) mailbox: Mailbox,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

/// Tells whoever is listening what state a session is in now. A send with
/// no receiver is fine: the engine runs with the window closed.
pub(crate) fn announce(events: &broadcast::Sender<EventView>, status: &Status) {
    let why = status
        .last_error
        .as_deref()
        .or(status.halted.as_deref())
        .or(status.frozen.as_deref());
    let _ = events.send(arvo_service::events::session(
        &status.id,
        &status.state,
        why,
    ));
}
