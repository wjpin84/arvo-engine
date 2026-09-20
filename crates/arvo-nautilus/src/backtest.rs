//! One backtest: the venue, the instruments and their bars, the strategies,
//! and the result read back out of the engine.

use std::sync::Once;

use arvo_research::{Experiment, SimulationError, SimulationResult};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::logging::logging_set_bypass;
use nautilus_core::UnixNanos;
use nautilus_execution::models::{fee::FeeModelHandle, fill::FillModelHandle};
use nautilus_model::{
    data::{BarSpecification, BarType, Data, IndexPriceUpdate},
    enums::{AccountType, AggregationSource, BookType, OmsType, PriceType},
    identifiers::{InstrumentId, Symbol, Venue},
    instruments::{IndexInstrument, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::strategy::{StrategyConfig, StrategyCore};

use crate::chain::Settlement;
use crate::convert::{
    aggregation_of, equity, option, to_nautilus_bar, PRICE_PRECISION, SIZE_PRECISION,
};
use crate::plan::Plan;
use crate::{fee, fill, ledger, strategy, ENGINE};

/// Stops Nautilus writing its own log to stdout.
///
/// Nautilus installs a process-wide logger and emits a line per order and per
/// bar. Arvo already owns observability (`arvo_runtime::init_tracing`), and a
/// release build has no console at all — `windows_subsystem = "windows"` — so
/// that output goes nowhere while still costing the run.
///
/// `BacktestEngineConfig::bypass_logging` looks like the knob for this and is
/// not: nothing in the Rust kernel reads that field, it is honoured on the
/// Python side. `logging_set_bypass` is the switch that works.
///
/// It is a global, so this is deliberately process-wide and set once. Engine
/// failures are unaffected — they come back as `Err` from `run`, not as a log
/// line somebody has to notice.
fn silence_nautilus_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `NAUTILUS_LOG` is Nautilus's own escape hatch. If it is set, someone
        // is deliberately debugging an engine run, so leave their logging
        // alone — a silence with no way out is how integration faults stay
        // hidden.
        if std::env::var_os("NAUTILUS_LOG").is_none() {
            logging_set_bypass();
        }
    });
}

pub(crate) fn run_backtest(
    experiment: &Experiment,
    plan: &Plan,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    settlement: Option<&Settlement>,
) -> Result<SimulationResult, SimulationError> {
    let built = build(experiment, plan, book, settlement)?;
    finish_marked(built.engine, experiment, book, built.clock)
}

/// An engine with everything added and nothing run yet.
///
/// Shared by a backtest, which runs it to the end, and a shadow
/// (`crate::shadow`), which runs it to now and then a bar at a time — so the
/// two cannot come to differ in what a rule is given.
pub(crate) struct Built<'a> {
    pub(crate) engine: BacktestEngine,
    /// Each book member's bar type, in book order: what a later bar for that
    /// member must be stamped with.
    pub(crate) bar_types: Vec<BarType>,
    /// The underlying's name and bars, when a chain run marks its curve by
    /// them rather than by every contract.
    clock: Option<(String, &'a [arvo_data::Bar])>,
}

pub(crate) fn build<'a>(
    experiment: &Experiment,
    plan: &Plan,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    settlement: Option<&'a Settlement>,
) -> Result<Built<'a>, SimulationError> {
    silence_nautilus_logging();

    // Checked by the caller, which will not build an empty book.
    let venue = book
        .first()
        .ok_or_else(|| SimulationError::Rejected("no instruments to run".to_owned()))?
        .0
        .venue;

    let currency = Currency::USD();
    let mut engine = engine_with_venue(experiment, venue, currency)?;

    let (step, aggregation) = aggregation_of(experiment.interval)?;
    let spec = BarSpecification::new_checked(step, aggregation, PriceType::Last)
        .map_err(|err| rejected("bar specification", &err))?;

    let bar_types = add_book(&mut engine, experiment, book, spec, currency)?;

    // The underlying, on the option's venue, because that is where Nautilus's
    // own settlement looks for it — and registered as an *index*, so that an
    // in-the-money contract is settled in cash at its intrinsic value and an
    // out-of-the-money one at nothing.
    //
    // # Cash, not shares
    //
    // SPY options deliver shares, and Nautilus will deliver them if asked. In a
    // cash account that is wrong more often than right: sizing pays for the
    // premium, not the strike, so exercising one in-the-money call a $10k
    // account holds bought $64,000 of SPY it could never have owned, and every
    // figure afterwards carried six times the account in stock. A broker sells
    // such a contract at the close instead. Settling at intrinsic value against
    // the close is that sale, and it is also what delivery nets to wherever the
    // shares would be sold again — a spread's two legs, or a 0DTE position.
    // What it does not model is a strategy that means to keep the shares.
    //
    // The one price settlement reads is the underlying's index price, so each
    // expiration gets exactly one: that day's close, a nanosecond before the
    // contract's expiry fires. Daily bars are stamped at the end of their day,
    // after 16:00 — without this, a daily run would settle on the day before.
    let driver = match settlement {
        Some(settlement) => {
            add_settlement(&mut engine, experiment, settlement, venue, spec, currency)?
        }
        None => None,
    };

    let trade_size = Quantity::new_checked(plan.trade_size(), SIZE_PRECISION)
        .map_err(|err| rejected("trade size", &err))?;

    // Risk is expressed as a fraction of capital in the record and as an
    // amount of money here: the strategy needs a distance-to-loss in the same
    // units as the price it is stopping against.
    //
    // Measured against *starting* capital rather than current equity, so
    // sizing is fixed-fractional rather than compounding. That is a real and
    // common choice, but it is a choice — a compounding version risks more
    // after a win and less after a loss, and would produce a different curve.
    //
    // Every member of a book gets the same limits, expressed against the whole
    // account rather than a share of it. That is deliberate: a per-member cap
    // of `1/N` would pre-allocate capital and there would be nothing left to
    // contend for. The contention is the measurement.
    // The model travels whole rather than as resolved currency amounts. The
    // fractions are divided out at decision time by `arvo_research::decide`,
    // which is the same function a live gate calls — so the engine and a live
    // session cannot drift apart on how a limit is applied.
    let risk = strategy::Risk {
        model: experiment.risk.clone(),
        costs: experiment.costs,
        starting_cash: experiment.starting_cash,
    };

    // One estimate for the whole run, shared by every strategy instance. A book's
    // members know nothing of each other, so a per-instrument tracker would only
    // ever see one series and could not correlate anything with anything.
    //
    // Fed from bars as the engine delivers them, never computed over the window
    // up front — that would refuse a trade in March on the strength of how two
    // instruments moved in November.
    let correlations = std::sync::Arc::new(arvo_research::RollingCorrelations::default());

    // A ranking rule is one decision-maker over the whole set, not one per
    // instrument: it has to see every score before it can say which is best.
    // So it is added once, subscribed to all of them, and the per-instrument
    // loop below is skipped entirely.
    if let (Plan::ZeroDteBreakout { rule, .. }, Some(driver)) = (plan, driver) {
        let core = shared_core();
        engine
            .add_strategy(strategy::ZeroDteBreakout::new(
                core,
                driver,
                bar_types.clone(),
                *rule,
                risk,
                trade_size,
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        let clock = settlement.map(|settlement| {
            (format!("{}.{venue}", settlement.symbol), settlement.drive.as_slice())
        });
        return Ok(Built { engine, bar_types, clock });
    }

    if let (Plan::PutSpread { rule, .. }, Some(driver)) = (plan, driver) {
        let core = shared_core();
        engine
            .add_strategy(strategy::PutSpread::new(
                core,
                driver,
                bar_types.clone(),
                *rule,
                risk,
                trade_size,
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        let clock = settlement.map(|settlement| {
            (format!("{}.{venue}", settlement.symbol), settlement.drive.as_slice())
        });
        return Ok(Built { engine, bar_types, clock });
    }

    if let Plan::CrossSectionalMomentum {
        lookback, hold_top, ..
    } = *plan
    {
        let core = shared_core();
        engine
            .add_strategy(strategy::CrossSectionalMomentum::new(
                core,
                bar_types.clone(),
                trade_size,
                lookback,
                hold_top,
                risk.clone(),
                correlations.clone(),
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        return Ok(Built { engine, bar_types, clock: None });
    }

    // One strategy instance per instrument, all settling against the one
    // account added above. This is what makes capital contention real: when two
    // members want in at the same time, the second is filled out of whatever
    // the first left, and on a cash account it may not be filled at all. A
    // simulation that runs each instrument separately with the whole balance
    // behind it cannot express that, however its results are combined
    // afterwards.
    //
    // The rules themselves are untouched. Each still sees exactly one
    // instrument; sharing is the account's job, not theirs.
    add_each(&mut engine, experiment, plan, &bar_types, trade_size, risk, &correlations)?;

    Ok(Built { engine, bar_types, clock: None })
}

fn rejected(context: &str, err: &dyn std::fmt::Display) -> SimulationError {
    SimulationError::Rejected(format!("{context}: {err}"))
}

/// The engine, with the run's one venue and the fill model its costs call for.
fn engine_with_venue(
    experiment: &Experiment,
    venue: Venue,
    currency: Currency,
) -> Result<BacktestEngine, SimulationError> {
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())
        .map_err(|err| rejected("creating the engine", &err))?;

    let starting_balance = Money::new_checked(experiment.starting_cash, currency)
        .map_err(|err| rejected("starting cash", &err))?;

    engine
        .add_venue(
            SimulatedVenueConfig::builder()
                .venue(venue)
                .oms_type(OmsType::Netting)
                .account_type(AccountType::Cash)
                .book_type(BookType::L1_MBP)
                .starting_balances(vec![starting_balance])
                .bar_execution(true)
                // Only when there is something to charge that a rate cannot
                // say. With nothing extra, Nautilus's own model computes the
                // same number, and leaving it in place keeps every result
                // produced before venue fees existed reproducible.
                .fee_model(if experiment.costs.has_venue_fees() {
                    FeeModelHandle::new(fee::VenueFees::new(&experiment.costs))
                } else {
                    FeeModelHandle::default()
                })
                .build()
                .map_err(|err| rejected("venue config", &err))?,
        )
        .map_err(|err| rejected("adding the venue", &err))?;

    // Only when there is slippage to apply. A zero-slippage run keeps
    // Nautilus's own matching untouched, so every result produced before this
    // existed is still reproducible byte for byte.
    if experiment.costs.slippage_bps != 0.0 {
        let model = fill::BpsSlippage::new(experiment.costs.slippage_bps)
            .map_err(SimulationError::Rejected)?;
        engine.change_fill_model(venue, FillModelHandle::new(model));
    }
    // `run` has refused an option run without a spread, or with basis points
    // beside it, so this is the only fill model an option run can have.
    if let Some(spread) = experiment.costs.option_spread {
        engine.change_fill_model(
            venue,
            FillModelHandle::new(fill::PremiumSpread::new(spread)),
        );
    }

    Ok(engine)
}

/// Every instrument in the book with its bars, returning each one's bar type
/// in book order.
fn add_book(
    engine: &mut BacktestEngine,
    experiment: &Experiment,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    spec: BarSpecification,
    currency: Currency,
) -> Result<Vec<BarType>, SimulationError> {
    let mut bar_types = Vec::with_capacity(book.len());
    for (instrument_id, _, bars) in book {
        let instrument = match arvo_data::option::OptionContract::parse(&instrument_id.to_string())
        {
            Some(contract) => option(
                *instrument_id,
                &contract,
                currency,
                experiment.costs.commission_bps,
            ),
            None => equity(*instrument_id, currency, experiment.costs.commission_bps),
        }
        .map_err(|err| rejected("building the instrument", &err))?;
        engine
            .add_instrument(&instrument)
            .map_err(|err| rejected("adding the instrument", &err))?;

        // `External` says these bars arrived already aggregated rather than
        // being built by the engine from ticks, which is what a daily export is.
        let bar_type = BarType::new(*instrument_id, spec, AggregationSource::External);

        let data = bars
            .iter()
            .map(|bar| to_nautilus_bar(bar_type, bar, experiment.interval).map(Data::Bar))
            .collect::<Result<Vec<_>, _>>()?;

        // Once per instrument rather than one merged batch: Nautilus sorts what
        // it is given, and handing it each series separately keeps the merge its
        // problem rather than a second place interleaving could go wrong.
        engine
            .add_data(data, None, true, true)
            .map_err(|err| rejected("adding bar data", &err))?;

        bar_types.push(bar_type);
    }

    Ok(bar_types)
}

/// The underlying an option run settles against: an index instrument, its
/// close on each expiration, and its own bars when a rule reads them. Returns
/// the bar type of those bars, which then drive the rule.
fn add_settlement(
    engine: &mut BacktestEngine,
    experiment: &Experiment,
    settlement: &Settlement,
    venue: Venue,
    spec: BarSpecification,
    currency: Currency,
) -> Result<Option<BarType>, SimulationError> {
    let mut driver: Option<BarType> = None;
    let underlying_id = InstrumentId::from(format!("{}.{venue}", settlement.symbol).as_str());
    let index = IndexInstrument::builder()
        .instrument_id(underlying_id)
        .raw_symbol(Symbol::from(settlement.symbol.as_str()))
        .currency(currency)
        .price_precision(PRICE_PRECISION)
        .size_precision(SIZE_PRECISION)
        .price_increment(
            Price::new_checked(0.01, PRICE_PRECISION)
                .map_err(|err| rejected("underlying tick", &err))?,
        )
        .size_increment(Quantity::from(1))
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()
        .map_err(|err| rejected("building the underlying", &err))?;
    engine
        .add_instrument(&InstrumentAny::IndexInstrument(index))
        .map_err(|err| rejected("adding the underlying", &err))?;
    let prints = settlement
        .closes
        .iter()
        .map(|(contract, close)| {
            let at = contract
                .expires_at()
                .and_utc()
                .timestamp_nanos_opt()
                .and_then(|ns| u64::try_from(ns - 1).ok())
                .ok_or_else(|| {
                    SimulationError::Rejected(format!(
                        "{} expiry is not representable",
                        contract.symbol()
                    ))
                })?;
            // ponytail: rounded to the cent, so a 657.535 close settles at
            // 657.54 — up to half a cent a share, $0.50 a contract, either
            // way. Round against each contract's holder if that matters.
            let price = Price::new_checked(*close, PRICE_PRECISION)
                .map_err(|err| rejected("underlying close", &err))?;
            Ok(Data::IndexPrice(IndexPriceUpdate::new(
                underlying_id,
                price,
                UnixNanos::from(at),
                UnixNanos::from(at),
            )))
        })
        .collect::<Result<Vec<_>, SimulationError>>()?;
    if !prints.is_empty() {
        engine
            .add_data(prints, None, true, true)
            .map_err(|err| rejected("adding settlement prints", &err))?;
    }
    if !settlement.drive.is_empty() {
        let bar_type = BarType::new(underlying_id, spec, AggregationSource::External);
        let data = settlement
            .drive
            .iter()
            .map(|bar| to_nautilus_bar(bar_type, bar, experiment.interval).map(Data::Bar))
            .collect::<Result<Vec<_>, _>>()?;
        engine
            .add_data(data, None, true, true)
            .map_err(|err| rejected("adding underlying bars", &err))?;
        driver = Some(bar_type);
    }
    Ok(driver)
}

/// The strategy config a rule added once across the whole set runs under.
fn shared_core() -> StrategyCore {
    StrategyCore::new(StrategyConfig {
        strategy_id: None,
        order_id_tag: Some("001".to_owned()),
        oms_type: Some(OmsType::Netting),
        ..StrategyConfig::default()
    })
}

/// One strategy instance per instrument, on the shared account.
fn add_each(
    engine: &mut BacktestEngine,
    experiment: &Experiment,
    plan: &Plan,
    bar_types: &[BarType],
    trade_size: Quantity,
    risk: strategy::Risk,
    correlations: &std::sync::Arc<arvo_research::RollingCorrelations>,
) -> Result<(), SimulationError> {
    for (index, bar_type) in bar_types.iter().copied().enumerate() {
        let core = StrategyCore::new(StrategyConfig {
            strategy_id: None,
            // Distinct per member, because Nautilus builds client order ids
            // from it. Two strategies sharing a tag collide on their first
            // simultaneous order — which is exactly the case a book exists to
            // simulate, so it must not be an id collision instead.
            order_id_tag: Some(format!("{:03}", index + 1)),
            oms_type: Some(OmsType::Netting),
            // Deliberately NOT `manage_stop`. It looks like the right thing —
            // flatten open positions when the run ends so nothing is left
            // unrealised — but Nautilus already marks open positions to market
            // in its returns series, so it changes no number, and its
            // market-exit loop never completes in a backtest with no data left
            // to fill against. The trader then never reaches STOPPED and
            // disposal fails on every single run.
            ..StrategyConfig::default()
        });

        match *plan {
            Plan::SmaCross {
                fast_period,
                slow_period,
                ..
            } => engine.add_strategy(strategy::SmaCross::new(
                core,
                bar_type,
                trade_size,
                fast_period,
                slow_period,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::OpeningRange {
                range_bars,
                target_range_multiple,
                ..
            } => engine.add_strategy(strategy::OpeningRange::new(
                core,
                bar_type,
                trade_size,
                range_bars,
                target_range_multiple,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::VolatilityBreakout {
                entry_atr_multiple,
                atr_period,
                ..
            } => engine.add_strategy(strategy::VolatilityBreakout::new(
                core,
                bar_type,
                trade_size,
                entry_atr_multiple,
                atr_period,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::VwapReversion {
                entry_deviations, ..
            } => engine.add_strategy(strategy::VwapReversion::new(
                core,
                bar_type,
                trade_size,
                entry_deviations,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::MomentumBreakout {
                entry_period,
                exit_period,
                ..
            } => engine.add_strategy(strategy::MomentumBreakout::new(
                core,
                bar_type,
                trade_size,
                entry_period,
                exit_period,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::BuyAndHold { .. } => {
                engine.add_strategy(strategy::BuyAndHold::new(
                    core,
                    bar_type,
                    trade_size,
                    experiment.starting_cash,
                    experiment.costs,
                    correlations.clone(),
                ))
            }
            Plan::SellAndHold { .. } => engine.add_strategy(strategy::SellAndHold::new(
                core,
                bar_type,
                trade_size,
                risk.clone(),
                correlations.clone(),
            )),
            Plan::CrossSectionalMomentum { .. }
            | Plan::PutSpread { .. }
            | Plan::ZeroDteBreakout { .. } => {
                // Added once for the whole set above, and returned before
                // reaching here. The compiler cannot see that, so this says
                // it rather than pretending the case is possible.
                return Err(SimulationError::Rejected(
                    "a ranking or chain rule is added once across every instrument, not once per instrument"
                        .to_owned(),
                ));
            }
        }
        .map_err(|err| rejected("adding the strategy", &err))?;
    }

    Ok(())
}

/// Runs the engine to the end and reads the result back, with the curve's
/// clock taken from `driver` and only the instruments the ledger holds marked
/// (#87).
///
/// A chain run holds tens of thousands of contracts it never trades. Marking
/// the curve against every one of them at every bar is instants times
/// instruments — minutes per run at five-minute bars, for a curve that only the
/// traded few can move. The underlying's bars give every instant a session
/// has; the traded contracts give every mark.
fn finish_marked(
    mut engine: BacktestEngine,
    experiment: &Experiment,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    driver: Option<(String, &[arvo_data::Bar])>,
) -> Result<SimulationResult, SimulationError> {
    // The window is already expressed by the data: bars were filtered to it on
    // the way in, so bounding the run again would only add a way to disagree
    // with itself.
    engine
        .run(None, None, Some(experiment.id.to_string()), false)
        .map_err(|err| SimulationError::Engine(Box::new(BacktestFailed(err.to_string()))))?;

    // Before `dispose`: the positions live in the kernel's cache, and
    // disposal is what tears it down.
    let ledger = ledger::from_cache(&engine.kernel_mut().cache.borrow());
    let refused = ledger::refused(&engine.kernel_mut().cache.borrow());
    engine.dispose();

    // From the ledger and the prices, not from `engine.get_result()`. Nautilus
    // reports a `returns_series`, and it is not an equity curve: it is the
    // day-over-day change in the account's *cash balance*, which on a cash
    // account excludes the market value of anything held. Buying reads as a
    // catastrophic loss and selling as an enormous gain, both the size of the
    // position's notional. See `arvo_research::trade::equity_curve`.
    let series: Vec<(String, Vec<arvo_data::Bar>)> = match driver {
        None => book
            .iter()
            .map(|(_, name, bars)| (name.clone(), bars.clone()))
            .collect(),
        Some((name, bars)) => {
            let traded: std::collections::BTreeSet<&str> =
                ledger.iter().map(|trade| trade.instrument.as_str()).collect();
            std::iter::once((name, bars.to_vec()))
                .chain(
                    book.iter()
                        .filter(|(_, name, _)| traded.contains(name.as_str()))
                        .map(|(_, name, bars)| (name.clone(), bars.clone())),
                )
                .collect()
        }
    };
    let equity_curve = arvo_research::trade::equity_curve(
        experiment.starting_cash,
        &series,
        experiment.interval,
        &ledger,
    );

    Ok(SimulationResult {
        experiment: experiment.id.clone(),
        engine: ENGINE.to_owned(),
        // Positions, not legs: a spread is one trade (see `trade::positions`).
        trades: u32::try_from(arvo_research::trade::positions(&ledger).len()).unwrap_or(u32::MAX),
        equity_curve,
        ledger,
        refused,
    })
}

/// Wraps a Nautilus engine failure so it can cross the boundary as a plain
/// `std::error::Error` without exporting a Nautilus type.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BacktestFailed(String);
