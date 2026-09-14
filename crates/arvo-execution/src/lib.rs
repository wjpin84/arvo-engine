//! Sending orders somewhere, and finding out what a backtest got wrong.
//!
//! # Why this crate exists apart from the runtime
//!
//! Because a trading session must be runnable without a window. `arvo-runtime`
//! is the Tauri host; anything living there drags a GUI toolkit into a process
//! whose job is to sit on a socket for six and a half hours. This crate depends
//! on the research vocabulary and nothing else, so a session can run headless,
//! under test, and in CI.
//!
//! # One gate, many proposers
//!
//! [`Session`] is the loop: a proposal arrives from somewhere, [`RiskGate`]
//! sizes it or refuses it, an [`Executor`] sends what survives, and the fill
//! comes back to the gate. The technical engine and an alert pipeline are two
//! proposers into one account, and the only thing keeping them from taking two
//! full-sized positions in what is economically one bet is that neither can
//! reach an executor directly. That is enforced by shape: [`Session::propose`]
//! is the only public path to [`Executor::submit`].
//!
//! # What paper trading is actually for
//!
//! Not rehearsal. The measurement.
//!
//! A backtest asserts a cost model — `slippage_bps`, `commission_bps` — and
//! then scores itself against its own assertion. Nothing in it can discover
//! that the assumption was wrong, because both sides of every check use the
//! same assumed number. `arvo_research::reconcile` catches an engine
//! contradicting itself; it cannot catch an engine being consistently wrong
//! about the world.
//!
//! [`paper::PaperExecutor`] is the first thing here that can. It fills against
//! prices that actually arrived, records what each fill really cost against the
//! price the decision was made at, and [`Divergence`] compares that to what the
//! backtest assumed. A paper executor that filled at the requested price would
//! measure nothing at all — it would be a backtest with extra steps, agreeing
//! with itself.
//!
//! # A fill is not a bar
//!
//! Prices driving a paper session come from a live tick stream, and nothing
//! here may write one into the data library. An experiment pins its dataset as
//! a content hash of fetched bars; a tick that became a bar would make every
//! stored verdict describe a dataset no fetch ever returned. Execution reads
//! ticks. It never publishes them.

pub mod paper;
pub mod poll;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use arvo_research::risk::{Decision, Proposal, Rejection, RiskGate};

/// Which way an order goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// A broker's handle on one order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

impl std::fmt::Display for OrderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the gate approved, on its way to a venue.
///
/// Market orders only, deliberately. A limit price is a second risk decision —
/// it trades certainty of fill for certainty of price — and the gate has no
/// opinion about it. Adding limits means deciding what an unfilled order does
/// to a position the strategy believes it has, which is a real design and not
/// a parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub instrument: String,
    pub side: Side,
    /// Sized by the gate, never by the proposer.
    pub quantity: f64,
    /// The price the decision was made at.
    ///
    /// **Not a limit.** It is recorded so the difference between it and the
    /// fill can be measured, which is the entire point of running on paper.
    pub decision_price: f64,
    /// When the signal that caused this was generated — not when the order was
    /// sent. Signal-to-fill is the latency that costs money.
    pub decision_at: NaiveDateTime,
    /// Which path asked, so a fill can be attributed afterwards.
    pub proposer: String,
}

/// One completed execution, with everything needed to say what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub order: OrderId,
    pub instrument: String,
    pub side: Side,
    pub proposer: String,
    pub quantity: f64,
    pub decision_price: f64,
    pub fill_price: f64,
    pub decision_at: NaiveDateTime,
    pub filled_at: NaiveDateTime,
}

impl Execution {
    /// How much worse than the decision price this filled, in basis points.
    ///
    /// Signed so that **positive is always adverse**, whichever way the order
    /// went: paying above the decision price on a buy and receiving below it on
    /// a sell are the same problem, and an unsigned figure would let a bad buy
    /// and a good sell cancel out into an encouraging zero.
    #[must_use]
    pub fn slippage_bps(&self) -> f64 {
        if self.decision_price <= 0.0 {
            return 0.0;
        }
        let adverse = match self.side {
            Side::Buy => self.fill_price - self.decision_price,
            Side::Sell => self.decision_price - self.fill_price,
        };
        adverse / self.decision_price * 10_000.0
    }

    /// Signal to fill, in milliseconds.
    #[must_use]
    pub fn latency_ms(&self) -> i64 {
        (self.filled_at - self.decision_at).num_milliseconds()
    }
}

/// What a paper session learned that a backtest could not.
///
/// # Why this is the deliverable
///
/// A backtest scores itself against its own cost assumption, so it can never
/// find out the assumption was wrong. This is the first number in the platform
/// produced by comparing an assumption to something that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Divergence {
    pub fills: usize,
    /// Approved orders that never filled.
    ///
    /// Counted separately and never averaged into the slippage: an order that
    /// did not fill is not a fill that cost nothing. A backtest assumes every
    /// order fills, so this figure has no counterpart to be compared against —
    /// it is pure news.
    pub unfilled: usize,
    /// Mean adverse slippage across fills, in basis points.
    pub mean_slippage_bps: f64,
    /// The single worst fill, which is what a thin book does to size.
    pub worst_slippage_bps: f64,
    pub mean_latency_ms: f64,
    pub worst_latency_ms: i64,
    /// What the backtest assumed, for comparison. `None` when nobody said.
    pub assumed_slippage_bps: Option<f64>,
}

impl Divergence {
    /// Measured slippage minus assumed, in basis points.
    ///
    /// Positive means the backtest was optimistic — every stored result on this
    /// strategy overstates its return by roughly this much per fill, doubled
    /// for a round trip.
    #[must_use]
    pub fn optimism_bps(&self) -> Option<f64> {
        self.assumed_slippage_bps
            .map(|assumed| self.mean_slippage_bps - assumed)
    }

    /// Summarises a run's executions.
    #[must_use]
    pub fn of(executions: &[Execution], unfilled: usize, assumed_slippage_bps: Option<f64>) -> Self {
        let fills = executions.len();
        if fills == 0 {
            return Self {
                fills: 0,
                unfilled,
                mean_slippage_bps: 0.0,
                worst_slippage_bps: 0.0,
                mean_latency_ms: 0.0,
                worst_latency_ms: 0,
                assumed_slippage_bps,
            };
        }

        #[expect(clippy::cast_precision_loss, reason = "fill counts are small")]
        let count = fills as f64;
        let slippage: Vec<f64> = executions.iter().map(Execution::slippage_bps).collect();
        let latency: Vec<i64> = executions.iter().map(Execution::latency_ms).collect();

        Self {
            fills,
            unfilled,
            mean_slippage_bps: slippage.iter().sum::<f64>() / count,
            worst_slippage_bps: slippage.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            #[expect(clippy::cast_precision_loss, reason = "milliseconds, not eons")]
            mean_latency_ms: latency.iter().sum::<i64>() as f64 / count,
            worst_latency_ms: latency.iter().copied().max().unwrap_or_default(),
            assumed_slippage_bps,
        }
    }
}

/// One position the venue says the account holds.
///
/// The venue's word, not the gate's. Reconciliation exists because the two can
/// differ, so a type that could only be built from the gate's own book would
/// be unable to express the disagreement it is here to find.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    /// The venue's own name for it — a bare ticker, with no venue suffix.
    ///
    /// Bare because the executor cannot honestly say more. The gate keys
    /// positions by Arvo instrument id (`MSFT.RH`), which encodes *which
    /// library the bars came from* — and a broker reporting `MSFT` has no idea
    /// where this platform fetched its prices. Joining the two is the caller's
    /// fact; see [`Session::reconcile`].
    pub symbol: String,
    /// Signed: negative is short. A venue reports a short as a negative
    /// quantity and flattening it means buying, so dropping the sign here
    /// would have reconciliation exit a short by selling more of it.
    pub quantity: f64,
    /// What the account paid, averaged. The gate books entries at fill price
    /// and this is the venue's equivalent of one.
    pub entry: f64,
}

/// What the venue holds and has working, right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VenueState {
    pub positions: Vec<Holding>,
    /// Orders resting at the venue, whoever placed them.
    pub resting: Vec<OrderId>,
}

impl VenueState {
    /// Whether the account is flat and quiet, which is the only state a fresh
    /// session can assume without asking.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.positions.is_empty() && self.resting.is_empty()
    }
}

/// What a session found at the venue that it did not put there.
#[derive(Debug, Default)]
pub struct Reconciliation {
    /// Positions the gate has been told about. It now sizes against them.
    pub adopted: Vec<Holding>,
    /// Orders cancelled because nothing in this session was watching them.
    pub cancelled: Vec<OrderId>,
    /// Orders the venue would not take back. Still working, still unwatched.
    pub stranded: Vec<(OrderId, ExecutionError)>,
}

impl Reconciliation {
    /// Whether anything was found. `false` is the ordinary start.
    #[must_use]
    pub fn found_anything(&self) -> bool {
        !self.adopted.is_empty() || !self.cancelled.is_empty() || !self.stranded.is_empty()
    }
}

/// What a kill switch managed to do.
///
/// Not a `Result`: the halt is armed either way, and some exits succeeding
/// while others fail is the ordinary case rather than an error condition. A
/// caller that collapses this to success or failure is discarding the half it
/// most needs.
#[derive(Debug, Default)]
pub struct Flatten {
    /// Exits the venue acknowledged.
    pub submitted: Vec<OrderId>,
    /// Positions the venue would not take an exit for. **Still held**, by an
    /// account that has stopped trading.
    pub failed: Vec<(String, ExecutionError)>,
}

impl Flatten {
    /// Whether every held position got an exit out of the door.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("{venue} rejected the order: {reason}")]
    Rejected { venue: String, reason: String },
    #[error("talking to {venue}: {detail}")]
    Transport { venue: String, detail: String },
    #[error("{venue} does not trade {instrument}")]
    UnknownInstrument { venue: String, instrument: String },
}

/// Somewhere orders can be sent.
///
/// Implemented by [`paper::PaperExecutor`] now, and by a broker later. The
/// trait is deliberately thin: everything a venue does that is *not*
/// venue-specific — sizing, limits, refusals — already happened at the gate.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    /// Names the venue, for the record and for error messages.
    fn venue(&self) -> &str;

    /// Sends one order.
    ///
    /// Returns when the venue has *acknowledged* it, not when it filled. Those
    /// are different instants and conflating them is how a system reports a
    /// position it does not hold.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue refuses or cannot be reached.
    async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError>;

    /// Executions that have completed since the last call, and how many
    /// acknowledged orders are still outstanding.
    ///
    /// Polled rather than pushed. A broker API is polled anyway, and a channel
    /// here would mean every implementation owning a task whose lifetime nobody
    /// asked about — see `arvo_plugin_host` for what happens when a component's
    /// lifecycle is nobody's job.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue cannot be reached.
    async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError>;

    /// What the venue holds and has working, asked of the venue.
    ///
    /// No default. A default would report a flat, quiet account, which is a
    /// silent wrong answer for precisely the case this exists to catch — so a
    /// new venue has to say, and the compiler makes it. The same argument
    /// `arvo_data::source::Source::basis` makes about a basis.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue cannot be reached.
    async fn at_venue(&self) -> Result<VenueState, ExecutionError>;

    /// Takes one order back.
    ///
    /// Separate from the cancelling [`Self::drain`] already does for its own
    /// stale orders, because this one is for orders the session never placed
    /// and therefore has no record of.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue refuses or cannot be reached.
    /// An order that has already filled or been cancelled is the venue's to
    /// refuse, and refusing is the right answer.
    async fn cancel(&self, order: &OrderId) -> Result<(), ExecutionError>;

    /// What the account can spend on a new position right now, as the venue
    /// counts it — already net of orders it has working.
    ///
    /// `None` when this venue cannot say, which leaves the gate's cash ceiling
    /// off: entries are sized on the opening balance and the venue refuses
    /// what cannot be paid for. Defaulted for that reason — unlike
    /// [`Self::at_venue`], not knowing degrades to the old behaviour rather
    /// than to a silently wrong answer. See ADR-0015.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue cannot be reached.
    async fn buying_power(&self) -> Result<Option<f64>, ExecutionError> {
        Ok(None)
    }
}

/// A live or paper trading session: one gate, one venue, many proposers.
///
/// # The invariant
///
/// Nothing reaches [`Executor::submit`] except through [`Self::propose`]. That
/// is the whole design: two proposers into one account is safe exactly as long
/// as one authority sizes both, and it stops being safe the moment a path can
/// reach the venue on its own.
pub struct Session<E: Executor> {
    gate: RiskGate,
    executor: E,
    executions: Vec<Execution>,
    unfilled: usize,
    /// Refusals, kept rather than logged. A pipeline refused for staleness
    /// nine times in ten has a latency problem, and one refused on the daily
    /// limit had a bad day; those want different responses.
    refusals: Vec<(String, Rejection)>,
    assumed_slippage_bps: Option<f64>,
}

impl<E: Executor> Session<E> {
    #[must_use]
    pub fn new(gate: RiskGate, executor: E) -> Self {
        Self {
            gate,
            executor,
            executions: Vec::new(),
            unfilled: 0,
            refusals: Vec::new(),
            assumed_slippage_bps: None,
        }
    }

    /// Records what the backtest assumed, so [`Self::divergence`] can compare.
    #[must_use]
    pub const fn against_assumed_slippage_bps(mut self, bps: f64) -> Self {
        self.assumed_slippage_bps = Some(bps);
        self
    }

    #[must_use]
    pub const fn gate(&self) -> &RiskGate {
        &self.gate
    }

    #[must_use]
    pub fn refusals(&self) -> &[(String, Rejection)] {
        &self.refusals
    }

    /// The venue, for a caller that has to drive it.
    ///
    /// A paper session needs this to feed prices in; a live one uses it for
    /// nothing but the venue name. Read-only on purpose — handing out `&mut`
    /// would be handing out a way to submit an order behind the gate's back,
    /// which is the one thing this type exists to prevent.
    #[must_use]
    pub const fn executor(&self) -> &E {
        &self.executor
    }

    /// Puts one proposal to the gate and, if it survives, to the venue.
    ///
    /// `now` is passed rather than read from a clock, so a session replays
    /// identically under test and in a backtest. See
    /// `arvo_research::risk::RiskGate::propose`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] only if the venue itself failed. A refusal by
    /// the gate is an ordinary outcome, not an error: it is the system working.
    pub async fn propose(
        &mut self,
        proposal: &Proposal,
        now: NaiveDateTime,
        correlations: Option<&dyn arvo_research::risk::Correlations>,
    ) -> Result<Option<OrderId>, ExecutionError> {
        // Asked per proposal, not cached: every fill and every working order
        // changes it, and a stale figure is the refused order this exists to
        // prevent.
        let spendable = self.executor.buying_power().await?;
        match self.gate.propose_within(proposal, now, correlations, spendable) {
            Decision::Reject(rejection) => {
                self.refusals.push((proposal.proposer.clone(), rejection));
                Ok(None)
            }
            Decision::Accept { quantity } => {
                let order = Order {
                    instrument: proposal.instrument.clone(),
                    side: Side::Buy,
                    quantity,
                    decision_price: proposal.reference_price,
                    // The *signal's* instant, carried through untouched. Timing
                    // the latency from when the order was sent would measure
                    // the last hop and hide everything before it, which is
                    // where an LLM or an alert queue actually spends its time.
                    decision_at: proposal.signalled_at,
                    proposer: proposal.proposer.clone(),
                };
                self.executor.submit(&order).await.map(Some)
            }
        }
    }

    /// Collects completed executions and tells the gate what the account holds.
    ///
    /// Called on a timer or after each proposal. The gate is updated from
    /// *fills*, never from acceptances — an approved order that never filled is
    /// not a position, and a gate that assumed otherwise would refuse trades on
    /// exposure the account does not have.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue cannot be reached.
    pub async fn settle(&mut self) -> Result<usize, ExecutionError> {
        let (executions, outstanding) = self.executor.drain().await?;
        self.unfilled = outstanding;

        let settled = executions.len();
        for execution in executions {
            match execution.side {
                Side::Buy => {
                    self.gate.opened(
                        &execution.instrument,
                        execution.quantity,
                        execution.fill_price,
                        execution.filled_at.date(),
                    );
                }
                Side::Sell => {
                    // Realised against the entry the gate is holding, so the
                    // daily loss limit and the drawdown halt see the same
                    // number the ledger will.
                    let pnl = self
                        .gate
                        .positions()
                        .get(&execution.instrument)
                        .map_or(0.0, |held| {
                            (execution.fill_price - held.entry) * execution.quantity
                        });
                    self.gate
                        .closed(&execution.instrument, pnl, execution.filled_at.date());
                }
            }
            self.executions.push(execution);
        }
        Ok(settled)
    }

    /// Marks the account to market, which is what the drawdown halt watches.
    ///
    /// Driven by the caller because only it knows what the positions are worth
    /// now: the gate holds entry prices, not live ones, and the executor holds
    /// prices but not the account. A halt that saw only realised losses would
    /// let an account fall to nothing while holding.
    pub fn mark(&mut self, equity: f64) {
        self.gate.mark(equity);
    }

    /// Flattens a held position.
    ///
    /// # Why this does not go through the gate
    ///
    /// Every check in [`RiskGate`] asks whether to *take* risk. None of them
    /// should be able to stop you shedding it, and several of them would: a
    /// halted account cannot propose, and a daily loss limit reached mid-session
    /// would refuse the exit that caps the loss — converting a bad day into an
    /// uncapped one. An exit path that can be refused is not an exit path.
    ///
    /// So closing is a direct instruction, and the only thing the gate does
    /// with it is book the result once it fills.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue refuses or cannot be reached.
    /// Closing something not held is not an error — it is already flat, which
    /// is the state the caller asked for.
    pub async fn close(
        &mut self,
        instrument: &str,
        price: f64,
        at: NaiveDateTime,
    ) -> Result<Option<OrderId>, ExecutionError> {
        let Some(held) = self.gate.positions().get(instrument).copied() else {
            return Ok(None);
        };

        let order = Order {
            instrument: instrument.to_owned(),
            side: Side::Sell,
            quantity: held.quantity,
            decision_price: price,
            decision_at: at,
            proposer: "exit".to_owned(),
        };
        self.executor.submit(&order).await.map(Some)
    }

    /// Squares this session against what the venue already has.
    ///
    /// Call once, before the first proposal. `venue` is the library suffix
    /// this session's instruments are filed under — the broker reports bare
    /// tickers and the gate keys by Arvo instrument id, and only the caller
    /// knows which library supplied the bars. A gate starts flat and quiet
    /// because a fresh `RiskGate` has no book — and an account is not
    /// obliged to agree. A crash mid-session, a restart after a deploy, or an
    /// order placed by hand all leave the venue holding something this process
    /// has never heard of, and a gate that starts up believing it is flat
    /// sizes the next trade against capital that is already committed.
    ///
    /// # What it does, and why it is not just "adopt and carry on"
    ///
    /// **Positions are adopted.** The gate is told, so sizing, the position
    /// cap and the correlation cap all see them, and so an exit is sized
    /// against what is actually held.
    ///
    /// **Resting orders are cancelled.** An order placed by a session that no
    /// longer exists has no owner: nothing will cancel it when it goes stale
    /// and nothing will book it when it fills. It cannot be adopted either —
    /// [`poll::Sent`] needs the price and instant the *signal* fired, and the
    /// venue does not know them. So it is taken back, and a strategy that
    /// still wants the position may propose it again with a fresh decision
    /// behind it.
    ///
    /// **And then the account is halted.** This is the part worth arguing
    /// about. Adopting silently would restore the positions and lose
    /// everything the gate knows *around* them: the day-trade count, today's
    /// realised loss, the equity peak the drawdown halt measures from. An
    /// account that hit its daily loss limit, crashed, and restarted would be
    /// free to trade again — a limit that a restart lifts is not a limit. So
    /// the session comes up stopped, naming what it found, and a person
    /// releases it with [`Self::rearm`] once they have looked.
    ///
    /// A clear venue halts nothing. The ordinary start is unaffected.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionError`] if the venue cannot be asked. A cancel the
    /// venue refuses is not an error — it is reported in
    /// [`Reconciliation::stranded`], because an order that would not die is
    /// exactly what the operator needs to be told about.
    pub async fn reconcile(
        &mut self,
        now: NaiveDateTime,
        venue: &str,
    ) -> Result<Reconciliation, ExecutionError> {
        let state = self.executor.at_venue().await?;
        let mut found = Reconciliation::default();

        for holding in state.positions {
            // Booked through the same door a fill uses, so there is one way
            // the gate learns it holds something.
            //
            // ponytail: one venue for the whole session. A session trades one
            // library's instruments, so the suffix is the same for all of
            // them; the day a session mixes two libraries this has to become a
            // lookup from symbol to the id it is filed under — which
            // `arvo_data::source::existing_venues` already computes, for the
            // caller to hand in.
            let instrument = format!("{}.{venue}", holding.symbol);
            self.gate
                .opened(&instrument, holding.quantity, holding.entry, now.date());
            found.adopted.push(holding);
        }

        for order in state.resting {
            match self.executor.cancel(&order).await {
                Ok(()) => found.cancelled.push(order),
                Err(err) => found.stranded.push((order, err)),
            }
        }

        if found.found_anything() {
            self.gate.kill(&format!(
                "this session started against an account that already held {} position{} and {} resting order{}{}; nothing was proposed until you released it",
                found.adopted.len(),
                if found.adopted.len() == 1 { "" } else { "s" },
                found.cancelled.len() + found.stranded.len(),
                if found.cancelled.len() + found.stranded.len() == 1 { "" } else { "s" },
                if found.stranded.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", {} of which the venue would not cancel and are still working",
                        found.stranded.len()
                    )
                },
            ));
        }
        Ok(found)
    }

    /// Stops the account and flattens everything it holds. The kill switch.
    ///
    /// # The order matters
    ///
    /// The halt is armed **before** the first exit is sent, so a proposer that
    /// fires while the account is being flattened is refused rather than
    /// opening into the exit. Flattening first would leave exactly that window
    /// open, and the window is widest precisely when this is being used.
    ///
    /// Arming does not block the exits: [`Self::close`] does not go through the
    /// gate, for the reason recorded there — an exit path that can be refused
    /// is not an exit path.
    ///
    /// # Why this returns rather than failing
    ///
    /// A venue that refuses one exit must not stop the other seven. Every
    /// position is attempted, and what could not be exited is *named* — those
    /// are still held, by an account that has stopped trading, which is the one
    /// state a person pressing this needs to be told about. The halt is armed
    /// either way.
    ///
    /// `prices` is the reference price per instrument, used only to measure the
    /// exit's slippage afterwards. A missing one does not stop the exit: these
    /// are market orders, and refusing to flatten for want of a quote would be
    /// the failure this method exists to prevent.
    pub async fn kill(
        &mut self,
        reason: &str,
        prices: &std::collections::BTreeMap<String, f64>,
        at: NaiveDateTime,
    ) -> Flatten {
        self.gate.kill(reason);

        let held: Vec<String> = self.gate.positions().keys().cloned().collect();
        let mut flatten = Flatten::default();
        for instrument in held {
            // ponytail: an unquoted exit is recorded with no reference price,
            // so `Execution::slippage_bps` reports zero for it rather than a
            // number measured against the wrong thing. Feed the map from the
            // same quotes that drive `mark` and this does not arise.
            let price = prices.get(&instrument).copied().unwrap_or_default();
            match self.close(&instrument, price, at).await {
                Ok(Some(order)) => flatten.submitted.push(order),
                Ok(None) => {}
                Err(err) => flatten.failed.push((instrument, err)),
            }
        }
        flatten
    }

    /// Lifts a kill switch. Returns whether it lifted.
    ///
    /// `false` means this session is halted for a reason nobody chose — a
    /// drawdown breach — and that one does not lift. See
    /// `arvo_research::risk::Halt::manual`.
    pub fn rearm(&mut self) -> bool {
        self.gate.release()
    }

    /// What this session measured that a backtest could not.
    #[must_use]
    pub fn divergence(&self) -> Divergence {
        Divergence::of(&self.executions, self.unfilled, self.assumed_slippage_bps)
    }

    #[must_use]
    pub fn executions(&self) -> &[Execution] {
        &self.executions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};

    fn at(minute: u32, second: u32, milli: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 9)
            .expect("valid")
            .and_time(
                NaiveTime::from_hms_milli_opt(14, minute, second, milli).expect("valid"),
            )
    }

    fn execution(side: Side, decision: f64, fill: f64, latency_ms: u32) -> Execution {
        Execution {
            order: OrderId("o-1".to_owned()),
            instrument: "MSFT.RH".to_owned(),
            side,
            proposer: "technical".to_owned(),
            quantity: 10.0,
            decision_price: decision,
            fill_price: fill,
            decision_at: at(30, 0, 0),
            filled_at: at(30, 0, latency_ms),
        }
    }

    /// A venue that takes nothing, to exercise the half of a kill switch that
    /// matters most: what happens when the exits do not go out.
    struct Refusing;

    #[async_trait::async_trait]
    impl Executor for Refusing {
        fn venue(&self) -> &str {
            "refusing"
        }

        async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
            Err(ExecutionError::Transport {
                venue: "refusing".to_owned(),
                detail: format!("no route for {}", order.instrument),
            })
        }

        async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
            Ok((Vec::new(), 0))
        }

        async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
            Ok(VenueState::default())
        }

        async fn cancel(&self, _: &OrderId) -> Result<(), ExecutionError> {
            Err(ExecutionError::Transport {
                venue: "refusing".to_owned(),
                detail: "takes nothing back either".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn one_exit_the_venue_refuses_does_not_strand_the_others() {
        // The failure mode a `?` would have shipped: the first unreachable
        // instrument aborts the flatten and everything after it stays held, by
        // an account nobody is watching any more because the button was pressed.
        use arvo_research::{risk::RiskGate, RiskModel};

        let day = NaiveDate::from_ymd_opt(2026, 9, 9).expect("valid");
        let mut gate = RiskGate::new(RiskModel::default(), 10_000.0, day);
        gate.opened("MSFT.RH", 10.0, 100.0, day);
        gate.opened("AAPL.RH", 20.0, 50.0, day);
        gate.opened("NVDA.RH", 5.0, 200.0, day);
        let mut session = Session::new(gate, Refusing);

        let flatten = session
            .kill("operator pulled it", &Default::default(), at(0, 0, 0))
            .await;

        assert!(!flatten.complete());
        assert_eq!(flatten.failed.len(), 3, "every position was attempted");
        let stranded: Vec<&str> = flatten
            .failed
            .iter()
            .map(|(instrument, _)| instrument.as_str())
            .collect();
        assert_eq!(stranded, ["AAPL.RH", "MSFT.RH", "NVDA.RH"]);

        // Armed regardless. The exits failing is the reason to stop trading,
        // not a reason to carry on.
        assert!(session.gate().halted().is_some());
    }

    /// A venue with a stated amount to spend, remembering what it was sent.
    struct Funded {
        cash: Option<f64>,
        sent: std::sync::Mutex<Vec<f64>>,
    }

    #[async_trait::async_trait]
    impl Executor for Funded {
        fn venue(&self) -> &str {
            "funded"
        }

        async fn submit(&self, order: &Order) -> Result<OrderId, ExecutionError> {
            self.sent.lock().expect("unpoisoned").push(order.quantity);
            Ok(OrderId("o".to_owned()))
        }

        async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
            Ok((Vec::new(), 0))
        }

        async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
            Ok(VenueState::default())
        }

        async fn cancel(&self, _: &OrderId) -> Result<(), ExecutionError> {
            Ok(())
        }

        async fn buying_power(&self) -> Result<Option<f64>, ExecutionError> {
            Ok(self.cash)
        }
    }

    async fn sent_for(cash: Option<f64>) -> Vec<f64> {
        use arvo_research::{
            risk::{Proposal, RiskGate},
            CostModel, RiskModel,
        };
        let gate = RiskGate::new(RiskModel::default(), 100_000.0, day())
            .with_costs(CostModel::proportional(1.0, 1.0));
        let mut session = Session::new(
            gate,
            Funded {
                cash,
                sent: std::sync::Mutex::default(),
            },
        );
        let signalled = at(30, 0, 0);
        let proposal = Proposal {
            instrument: "MSFT.AIEX".to_owned(),
            proposer: "test".to_owned(),
            signalled_at: signalled,
            reference_price: 100.0,
            stop_distance: None,
            desired_quantity: Some(5_000.0),
            opens_short: false,
        };
        session
            .propose(&proposal, signalled, None)
            .await
            .expect("the venue answered");
        let sent = session.executor().sent.lock().expect("unpoisoned").clone();
        sent
    }

    #[tokio::test]
    async fn a_live_entry_is_capped_at_what_the_broker_says_it_can_spend() {
        // ADR-0015's follow-up. The gate sizes off the opening balance, so an
        // account down to $20,000 kept proposing $100,000 of stock and the
        // broker refused it. Asked per proposal, the venue's figure caps it.
        let capped = sent_for(Some(20_000.0)).await;
        assert_eq!(capped, vec![199.0], "$20,000 all in at $100.02 a share");

        let unknown = sent_for(None).await;
        assert_eq!(unknown, vec![999.0], "a venue that cannot say leaves the ceiling off, costs still in");
    }

    /// A venue that already holds things, to exercise the one case a paper
    /// executor structurally cannot have.
    struct Stocked {
        state: VenueState,
        /// Orders it will refuse to take back.
        immovable: Vec<&'static str>,
    }

    #[async_trait::async_trait]
    impl Executor for Stocked {
        fn venue(&self) -> &str {
            "stocked"
        }

        async fn submit(&self, _: &Order) -> Result<OrderId, ExecutionError> {
            Ok(OrderId("new".to_owned()))
        }

        async fn drain(&self) -> Result<(Vec<Execution>, usize), ExecutionError> {
            Ok((Vec::new(), 0))
        }

        async fn at_venue(&self) -> Result<VenueState, ExecutionError> {
            Ok(self.state.clone())
        }

        async fn cancel(&self, order: &OrderId) -> Result<(), ExecutionError> {
            if self.immovable.contains(&order.0.as_str()) {
                return Err(ExecutionError::Rejected {
                    venue: "stocked".to_owned(),
                    reason: "already filling".to_owned(),
                });
            }
            Ok(())
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 9).expect("valid")
    }

    fn stocked(state: VenueState, immovable: Vec<&'static str>) -> Session<Stocked> {
        use arvo_research::{risk::RiskGate, RiskModel};
        Session::new(
            RiskGate::new(RiskModel::default(), 10_000.0, day()),
            Stocked { state, immovable },
        )
    }

    fn holding(symbol: &str, quantity: f64) -> Holding {
        Holding {
            symbol: symbol.to_owned(),
            quantity,
            entry: 100.0,
        }
    }

    #[tokio::test]
    async fn a_clear_venue_is_the_ordinary_start_and_changes_nothing() {
        // The common path. A reconciliation that halted every session would be
        // a reconciliation nobody ran.
        let mut session = stocked(VenueState::default(), Vec::new());
        let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        assert!(!found.found_anything());
        assert!(session.gate().halted().is_none());
        assert!(session.gate().positions().is_empty());
    }

    #[tokio::test]
    async fn a_position_the_gate_never_saw_is_adopted_and_then_sized_against() {
        // The hazard in one test. Before this the gate started flat, so it
        // would size the next trade against capital already committed.
        let state = VenueState {
            positions: vec![holding("MSFT", 20.0)],
            resting: Vec::new(),
        };
        let mut session = stocked(state, Vec::new());
        let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        assert_eq!(found.adopted, vec![holding("MSFT", 20.0)]);
        let held = session
            .gate()
            .positions()
            .get("MSFT.RH")
            .expect("the gate was told");
        assert!((held.quantity - 20.0).abs() < 1e-9);
        assert!((held.entry - 100.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_short_keeps_its_sign_so_flattening_it_does_not_double_it() {
        // A venue reports a short as a negative quantity, and flattening one
        // means buying. Dropping the sign would have the exit sell more.
        let state = VenueState {
            positions: vec![holding("MSFT", -20.0)],
            resting: Vec::new(),
        };
        let mut session = stocked(state, Vec::new());
        session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        let held = session.gate().positions()["MSFT.RH"];
        assert!(held.quantity < 0.0, "{held:?}");
    }

    #[tokio::test]
    async fn an_order_nobody_is_watching_is_taken_back() {
        // It cannot be adopted: `poll::Sent` needs the price and instant the
        // signal fired, and the venue does not know them. So it is cancelled,
        // and a strategy that still wants the position proposes it again with
        // a fresh decision behind it.
        let state = VenueState {
            positions: Vec::new(),
            resting: vec![OrderId("ord-1".to_owned()), OrderId("ord-2".to_owned())],
        };
        let mut session = stocked(state, Vec::new());
        let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        assert_eq!(found.cancelled.len(), 2);
        assert!(found.stranded.is_empty());
    }

    #[tokio::test]
    async fn an_order_the_venue_will_not_take_back_is_named_rather_than_forgotten() {
        // Still working, still unwatched, and the one thing the operator most
        // needs to be told. Dropping it silently would report a clean start.
        let state = VenueState {
            positions: Vec::new(),
            resting: vec![OrderId("ord-1".to_owned()), OrderId("stuck".to_owned())],
        };
        let mut session = stocked(state, vec!["stuck"]);
        let found = session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        assert_eq!(found.cancelled, vec![OrderId("ord-1".to_owned())]);
        assert_eq!(found.stranded.len(), 1);
        assert_eq!(found.stranded[0].0, OrderId("stuck".to_owned()));
        assert!(
            session
                .gate()
                .halted()
                .is_some_and(|why| why.contains("would not cancel")),
            "{:?}",
            session.gate().halted()
        );
    }

    #[tokio::test]
    async fn finding_anything_stops_the_account_until_a_person_looks() {
        // Adopting silently would restore the positions and lose everything
        // the gate knows around them — the day-trade count, today's realised
        // loss, the equity peak the drawdown halt measures from. An account
        // that hit its daily loss limit, crashed and restarted would be free
        // to trade again, and a limit a restart lifts is not a limit.
        let state = VenueState {
            positions: vec![holding("MSFT", 20.0)],
            resting: Vec::new(),
        };
        let mut session = stocked(state, Vec::new());
        session.reconcile(at(0, 0, 0), "RH").await.expect("readable");

        let why = session.gate().halted().expect("halted").to_owned();
        assert!(why.contains("1 position"), "{why}");

        let proposal = arvo_research::risk::Proposal {
            instrument: "AAPL.RH".to_owned(),
            proposer: "technical".to_owned(),
            signalled_at: at(0, 0, 0),
            reference_price: 100.0,
            stop_distance: Some(2.0),
            desired_quantity: None,
            opens_short: false,
        };
        assert!(
            session
                .propose(&proposal, at(0, 0, 0), None)
                .await
                .expect("venue")
                .is_none(),
            "nothing may be proposed into an account nobody has looked at"
        );

        // And a person can release it, because this halt is one somebody chose.
        assert!(session.rearm());
        assert!(session.gate().halted().is_none());
    }

    #[test]
    fn adverse_slippage_is_positive_whichever_way_the_order_went() {
        // Unsigned, a bad buy and a lucky sell cancel into an encouraging zero.
        let bought_high = execution(Side::Buy, 100.0, 100.10, 0);
        let sold_low = execution(Side::Sell, 100.0, 99.90, 0);
        assert!((bought_high.slippage_bps() - 10.0).abs() < 1e-9);
        assert!((sold_low.slippage_bps() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_fill_better_than_the_decision_price_is_negative_slippage() {
        // It happens, and reporting it as zero would bias the measurement in
        // exactly the direction this exists to detect.
        let lucky = execution(Side::Buy, 100.0, 99.95, 0);
        assert!((lucky.slippage_bps() + 5.0).abs() < 1e-9);
    }

    #[test]
    fn latency_is_measured_from_the_signal_not_the_send() {
        // Timing from the send would measure the last hop and hide the queue,
        // which is where an alert pipeline actually spends its time.
        assert_eq!(execution(Side::Buy, 100.0, 100.0, 250).latency_ms(), 250);
    }

    #[test]
    fn divergence_says_how_optimistic_the_backtest_was() {
        // The number this whole crate exists to produce.
        let executions = vec![
            execution(Side::Buy, 100.0, 100.04, 120),
            execution(Side::Buy, 100.0, 100.06, 380),
        ];
        let divergence = Divergence::of(&executions, 0, Some(1.0));

        assert_eq!(divergence.fills, 2);
        assert!((divergence.mean_slippage_bps - 5.0).abs() < 1e-9);
        assert!((divergence.worst_slippage_bps - 6.0).abs() < 1e-9);
        assert!(
            (divergence.optimism_bps().expect("assumed was given") - 4.0).abs() < 1e-9,
            "assumed 1bp, measured 5bp: every stored result is 4bp per fill optimistic"
        );
        assert_eq!(divergence.worst_latency_ms, 380);
    }

    #[test]
    fn an_unfilled_order_is_counted_and_never_averaged_in_as_a_free_fill() {
        // A backtest assumes every order fills, so this has no counterpart to
        // compare against. Averaging it in as zero slippage would report a
        // failure to trade as a perfect trade.
        let divergence = Divergence::of(&[execution(Side::Buy, 100.0, 100.05, 0)], 3, Some(1.0));
        assert_eq!(divergence.fills, 1);
        assert_eq!(divergence.unfilled, 3);
        assert!((divergence.mean_slippage_bps - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_session_with_no_fills_reports_nothing_rather_than_dividing_by_zero() {
        let divergence = Divergence::of(&[], 0, None);
        assert_eq!(divergence.fills, 0);
        assert!(divergence.optimism_bps().is_none());
    }
}
