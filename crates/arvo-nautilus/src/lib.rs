//! The single boundary between Arvo and NautilusTrader.
//!
//! # The containment rule
//!
//! **No crate other than this one may name a Nautilus type.** Nautilus's Rust
//! crates are published at `0.x` (0.63.0 at time of writing), so every release
//! may break; its own README treats Python as the primary API and Rust as
//! internal infrastructure. That is survivable only while the blast radius is
//! one crate, and the rule is what keeps it there.
//!
//! The rule is enforced by the direction of the dependency rather than by
//! anyone remembering it: [`arvo_research`] defines [`SimulationProvider`] and
//! this crate implements it, so the research domain cannot name a Nautilus
//! type even by accident. Nothing in this crate's public API mentions one
//! either — the only exported item is [`NautilusSimulation`], whose whole
//! surface is Arvo types.
//!
//! # What crosses the boundary
//!
//! Experiments down, results up — never trading primitives. Orders, fills,
//! positions and accounts exist only inside this crate and below. That is what
//! lets containment coexist with *not* duplicating Nautilus's domain model:
//! Arvo never manipulates a trading primitive, so it never needs to model one.
//!
//! # Panics are converted here, not propagated
//!
//! Nautilus's ergonomic constructors (`Price::new`, `Quantity::new`,
//! `BarSpecification::new`, ...) panic on invalid input; only the `_checked`
//! variants return a result. Everything below uses the checked forms, because
//! a bad number from a config file or a UI field is ordinary input, not a bug
//! worth aborting the process over.
//!
//! # Licensing
//!
//! NautilusTrader is LGPL-3.0-only and is linked into this binary. See the
//! repository `NOTICE`. A crate boundary is not a licensing boundary — the
//! obligation attaches to the distributed binary — but keeping the dependency
//! to this one crate keeps the fact obvious rather than diffuse.

mod fee;
mod fill;
mod ledger;
mod strategy;

use std::str::FromStr;
use std::sync::Once;

use arvo_data::BarProvider;
use arvo_research::{
    EquityPoint, Experiment, SimulationError, SimulationProvider, SimulationResult, StrategySpec,
};
use chrono::NaiveTime;
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::logging::logging_set_bypass;
use nautilus_execution::models::{fee::FeeModelHandle, fill::FillModelHandle};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarSpecification, BarType, Data},
    enums::{AccountType, AggregationSource, BarAggregation, BookType, OmsType, PriceType},
    identifiers::{InstrumentId, Symbol},
    instruments::{Equity, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::strategy::{StrategyConfig, StrategyCore};
use rust_decimal::Decimal;

/// The Nautilus version this crate is pinned to, recorded on every result.
///
/// A result is only comparable to another produced by the same engine, so the
/// version is part of the evidence rather than a build detail.
const ENGINE: &str = "nautilus 0.63.0";

/// The strategies wired up so far. See [`strategy`] for why these two.
const SMA_CROSS: &str = "sma_cross";
const BUY_AND_HOLD: &str = arvo_research::evaluation::BUY_AND_HOLD;

/// US equity conventions. Daily bars from the free exports are quoted in cents
/// and traded in whole shares; nothing yet needs another instrument class, and
/// guessing at one would mean guessing wrong.
const PRICE_PRECISION: u8 = 2;
const SIZE_PRECISION: u8 = 0;

/// Runs Arvo experiments on Nautilus's backtest engine.
///
/// Owns its data source so an experiment carries only a *reference* to its
/// dataset: reproducibility needs the identity of the input, and which
/// provider resolves that identity is a wiring decision, not a research one.
#[derive(Debug)]
pub struct NautilusSimulation<P> {
    bars: P,
}

impl<P: BarProvider> NautilusSimulation<P> {
    pub const fn new(bars: P) -> Self {
        Self { bars }
    }
}

impl<P: BarProvider> SimulationProvider for NautilusSimulation<P> {
    fn engine(&self) -> &str {
        ENGINE
    }

    fn run(&self, experiment: &Experiment) -> Result<SimulationResult, SimulationError> {
        let plan = Plan::from_spec(&experiment.strategy)?;
        experiment
            .risk
            .check()
            .map_err(|reason| SimulationError::Rejected(format!("risk model: {reason}")))?;
        experiment
            .costs
            .check()
            .map_err(|reason| SimulationError::Rejected(format!("cost model: {reason}")))?;

        let instrument_id = InstrumentId::from_str(&experiment.instrument).map_err(|err| {
            SimulationError::Rejected(format!("instrument {:?}: {err}", experiment.instrument))
        })?;

        let bars = self
            .bars
            .bars(
                &experiment.instrument,
                experiment.interval,
                experiment.window.from,
                experiment.window.to,
            )
            .map_err(|err| SimulationError::Engine(Box::new(err)))?;

        if bars.is_empty() {
            return Err(SimulationError::NoData {
                instrument: experiment.instrument.clone(),
                from: experiment.window.from,
                to: experiment.window.to,
            });
        }

        // A strategy that cannot even warm up has not been tested, and a run
        // that produces no signal is not evidence that there was none.
        if bars.len() <= plan.min_bars() {
            return Err(SimulationError::Rejected(format!(
                "{} bars is not enough for {}, which needs more than {}",
                bars.len(),
                experiment.strategy.name,
                plan.min_bars()
            )));
        }

        run_backtest(experiment, &plan, instrument_id, &bars)
    }
}

/// A strategy request, parsed out of the untyped spec and validated before
/// anything expensive starts.
enum Plan {
    SmaCross {
        fast_period: usize,
        slow_period: usize,
        trade_size: f64,
    },
    BuyAndHold {
        trade_size: f64,
    },
}

impl Plan {
    fn from_spec(spec: &StrategySpec) -> Result<Self, SimulationError> {
        let param = |name: &str| -> Result<f64, SimulationError> {
            spec.params.get(name).copied().ok_or_else(|| {
                SimulationError::Rejected(format!("{} requires a {name:?} parameter", spec.name))
            })
        };
        let period = |name: &str| -> Result<usize, SimulationError> {
            let value = param(name)?;
            if !value.is_finite() || value < 1.0 || value.fract() != 0.0 || value > 10_000.0 {
                return Err(SimulationError::Rejected(format!(
                    "{name} must be a whole number of bars between 1 and 10000, got {value}"
                )));
            }
            Ok(value as usize)
        };
        let trade_size = || -> Result<f64, SimulationError> {
            let value = param("trade_size")?;
            // `is_finite` first: every comparison against NaN is false, so a
            // bare `<= 0.0` would wave NaN straight through into sizing.
            if !value.is_finite() || value <= 0.0 {
                return Err(SimulationError::Rejected(format!(
                    "trade_size must be positive, got {value}"
                )));
            }
            Ok(value)
        };

        match spec.name.as_str() {
            SMA_CROSS => {
                let fast_period = period("fast")?;
                let slow_period = period("slow")?;
                if fast_period >= slow_period {
                    return Err(SimulationError::Rejected(format!(
                        "fast period {fast_period} must be shorter than slow period {slow_period}"
                    )));
                }
                Ok(Self::SmaCross {
                    fast_period,
                    slow_period,
                    trade_size: trade_size()?,
                })
            }
            BUY_AND_HOLD => Ok(Self::BuyAndHold {
                trade_size: trade_size()?,
            }),
            _ => Err(SimulationError::UnknownStrategy(spec.name.clone())),
        }
    }

    /// Bars needed before the strategy can act at all.
    const fn min_bars(&self) -> usize {
        match self {
            Self::SmaCross { slow_period, .. } => *slow_period,
            // One to buy on, and at least one more for the position to have
            // done anything.
            Self::BuyAndHold { .. } => 1,
        }
    }

    const fn trade_size(&self) -> f64 {
        match self {
            Self::SmaCross { trade_size, .. } | Self::BuyAndHold { trade_size } => *trade_size,
        }
    }
}

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

fn run_backtest(
    experiment: &Experiment,
    plan: &Plan,
    instrument_id: InstrumentId,
    bars: &[arvo_data::Bar],
) -> Result<SimulationResult, SimulationError> {
    let rejected = |context: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("{context}: {err}"))
    };

    silence_nautilus_logging();

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())
        .map_err(|err| rejected("creating the engine", &err))?;

    let currency = Currency::USD();
    let starting_balance = Money::new_checked(experiment.starting_cash, currency)
        .map_err(|err| rejected("starting cash", &err))?;

    engine
        .add_venue(
            SimulatedVenueConfig::builder()
                .venue(instrument_id.venue)
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
        engine.change_fill_model(instrument_id.venue, FillModelHandle::new(model));
    }

    let instrument = equity(instrument_id, currency, experiment.costs.commission_bps)
        .map_err(|err| rejected("building the instrument", &err))?;
    engine
        .add_instrument(&instrument)
        .map_err(|err| rejected("adding the instrument", &err))?;

    let (step, aggregation) = aggregation_of(experiment.interval)?;
    let spec = BarSpecification::new_checked(step, aggregation, PriceType::Last)
        .map_err(|err| rejected("bar specification", &err))?;
    // `External` says these bars arrived already aggregated rather than being
    // built by the engine from ticks, which is what a daily export is.
    let bar_type = BarType::new(instrument_id, spec, AggregationSource::External);

    let data = bars
        .iter()
        .map(|bar| to_nautilus_bar(bar_type, bar, experiment.interval).map(Data::Bar))
        .collect::<Result<Vec<_>, _>>()?;

    engine
        .add_data(data, None, true, true)
        .map_err(|err| rejected("adding bar data", &err))?;

    let core = StrategyCore::new(StrategyConfig {
        strategy_id: None,
        order_id_tag: Some("001".to_owned()),
        oms_type: Some(OmsType::Netting),
        // Deliberately NOT `manage_stop`. It looks like the right thing — flatten
        // open positions when the run ends so nothing is left unrealised — but
        // Nautilus already marks open positions to market in its returns series,
        // so it changes no number, and its market-exit loop never completes in a
        // backtest with no data left to fill against. The trader then never
        // reaches STOPPED and disposal fails on every single run.
        ..StrategyConfig::default()
    });
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
    let risk = strategy::Risk {
        stop_atr_multiple: experiment.risk.stop_atr_multiple,
        atr_period: experiment.risk.atr_period,
        risk_amount: experiment
            .risk
            .risk_per_trade
            .map(|fraction| fraction * experiment.starting_cash),
        max_position_value: experiment
            .risk
            .max_position_fraction
            .map(|fraction| fraction * experiment.starting_cash),
    };

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
            risk,
        )),
        Plan::BuyAndHold { .. } => {
            engine.add_strategy(strategy::BuyAndHold::new(core, bar_type, trade_size))
        }
    }
    .map_err(|err| rejected("adding the strategy", &err))?;

    // The window is already expressed by the data: bars were filtered to it on
    // the way in, so bounding the run again would only add a way to disagree
    // with itself.
    engine
        .run(None, None, Some(experiment.id.to_string()), false)
        .map_err(|err| SimulationError::Engine(Box::new(BacktestFailed(err.to_string()))))?;

    let result = engine.get_result();
    // Before `dispose`: the positions live in the kernel's cache, and
    // disposal is what tears it down.
    let ledger = ledger::from_cache(&engine.kernel_mut().cache.borrow());
    let equity_curve = compound(
        experiment.starting_cash,
        experiment.window.from.and_time(NaiveTime::MIN),
        result.returns_series.iter(),
    );
    engine.dispose();

    Ok(SimulationResult {
        experiment: experiment.id.clone(),
        engine: ENGINE.to_owned(),
        trades: u32::try_from(ledger.len()).unwrap_or(u32::MAX),
        equity_curve,
        ledger,
    })
}

/// Builds the traded instrument, with the experiment's commission applied as
/// the venue fee.
///
/// The cost model has to reach the engine or pinning it in the experiment is
/// theatre — this is the half that does reach it.
fn equity(
    instrument_id: InstrumentId,
    currency: Currency,
    commission_bps: f64,
) -> anyhow::Result<InstrumentAny> {
    let fee = Decimal::try_from(commission_bps / 10_000.0)?;
    let tick = Price::new_checked(0.01, PRICE_PRECISION)?;

    // Optional fields are left unset rather than passed as `None`: the builder
    // applies the same defaults checked construction would.
    let equity = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .currency(currency)
        .price_precision(PRICE_PRECISION)
        .price_increment(tick)
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;

    Ok(InstrumentAny::Equity(equity))
}

fn to_nautilus_bar(
    bar_type: BarType,
    bar: &arvo_data::Bar,
    interval: arvo_data::BarInterval,
) -> Result<Bar, SimulationError> {
    let rejected = |what: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("bar {}: {what}: {err}", bar.at))
    };

    let price = |name: &str, value: f64| {
        Price::new_checked(value, PRICE_PRECISION).map_err(|err| rejected(name, &err))
    };

    // A bar is only knowable once its period has closed. Timestamping it at
    // the *end* of that period is what stops a strategy acting on a close it
    // could not have seen yet — the look-ahead bias this platform exists to
    // catch. That was close-of-day while everything was daily; it is
    // close-of-bar now, and the daily case is unchanged by it.
    let ts = close_of_bar(bar.at, interval).ok_or_else(|| {
        SimulationError::Rejected(format!(
            "bar timestamp {} is outside the representable range",
            bar.at
        ))
    })?;

    Bar::new_checked(
        bar_type,
        price("open", bar.open)?,
        price("high", bar.high)?,
        price("low", bar.low)?,
        price("close", bar.close)?,
        Quantity::new_checked(bar.volume, SIZE_PRECISION)
            .map_err(|err| rejected("volume", &err))?,
        ts,
        ts,
    )
    .map_err(|err| rejected("failed Nautilus's OHLC checks", &err))
}

/// The instant a bar's period ends, as UNIX nanoseconds.
fn close_of_bar(at: chrono::NaiveDateTime, interval: arvo_data::BarInterval) -> Option<UnixNanos> {
    let end = at.checked_add_signed(interval.duration())?.and_utc();
    let nanos = end.timestamp_nanos_opt()?;
    u64::try_from(nanos).ok().map(UnixNanos::from)
}

/// Maps an Arvo interval onto Nautilus's own aggregation vocabulary.
///
/// # Errors
///
/// Returns [`SimulationError::Unsupported`] for a resolution Nautilus has no
/// aggregation for, rather than silently substituting a neighbouring one — a
/// backtest quietly run at the wrong resolution is worse than one refused.
fn aggregation_of(
    interval: arvo_data::BarInterval,
) -> Result<(usize, BarAggregation), SimulationError> {
    use arvo_data::IntervalUnit;
    let step = usize::try_from(interval.step).map_err(|_| {
        SimulationError::Unsupported(format!("interval step {} is too large", interval.step))
    })?;
    let aggregation = match interval.unit {
        IntervalUnit::Second => BarAggregation::Second,
        IntervalUnit::Minute => BarAggregation::Minute,
        IntervalUnit::Hour => BarAggregation::Hour,
        IntervalUnit::Day => BarAggregation::Day,
        IntervalUnit::Week => BarAggregation::Week,
    };
    Ok((step, aggregation))
}

/// Turns Nautilus's dated period returns into a dated equity curve.
///
/// Nautilus reports returns; evaluation wants equity, and everything else
/// (drawdown, Sharpe, hit rate) derives from equity. The curve opens at the
/// starting balance so a run with no trades is a flat single point rather than
/// an empty vector that reads like a failure.
///
/// The dates come along. Nautilus keys its returns by timestamp and this used
/// to drop them on the floor, which made the curve impossible to draw, to
/// align against another run, or to ask *when* a drawdown happened. Keeping
/// them costs a conversion.
fn compound<'a>(
    starting_cash: f64,
    opened: chrono::NaiveDateTime,
    returns: impl Iterator<Item = (&'a UnixNanos, &'a f64)>,
) -> Vec<EquityPoint> {
    let mut equity = starting_cash;
    let mut curve = vec![EquityPoint { at: opened, equity }];
    for (at, value) in returns {
        equity *= 1.0 + value;
        curve.push(EquityPoint {
            at: instant_of(*at).unwrap_or(opened),
            equity,
        });
    }
    curve
}

/// The UTC instant a Nautilus timestamp names.
///
/// The whole instant, not just its date: at an intraday resolution many
/// points share a day, and collapsing them would flatten the curve into one
/// value per day with no warning.
fn instant_of(at: UnixNanos) -> Option<chrono::NaiveDateTime> {
    let nanos = i64::try_from(at.as_u64()).ok()?;
    Some(chrono::DateTime::from_timestamp_nanos(nanos).naive_utc())
}

/// Wraps a Nautilus engine failure so it can cross the boundary as a plain
/// `std::error::Error` without exporting a Nautilus type.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BacktestFailed(String);

#[cfg(test)]
mod tests {
    use super::*;
    use arvo_data::InMemoryBars;
    use arvo_research::{
        CostModel, DatasetRef, DateRange, ExperimentId, HypothesisId, StrategySpec,
    };
    use chrono::NaiveDate;
    use std::collections::BTreeMap;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
    }

    /// A deterministic price path that crosses in both directions, so the
    /// strategy has something to react to.
    fn sawtooth(days: usize) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            // Slow drift up, with a cycle superimposed to force crossings.
            let phase = (index % 40) as f64;
            let cycle = if phase < 20.0 { phase } else { 40.0 - phase };
            let close = 100.0 + index as f64 * 0.05 + cycle * 0.5;
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    fn experiment(params: BTreeMap<String, f64>, bars: &[arvo_data::Bar]) -> Experiment {
        Experiment {
            id: ExperimentId::from("e-1"),
            hypothesis: HypothesisId::from("h-1"),
            instrument: "AAPL.NASDAQ".to_owned(),
            window: DateRange::new(
                bars.first().expect("fixture is not empty").at.date(),
                bars.last().expect("fixture is not empty").at.date(),
            )
            .expect("fixture window is ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: "fixture".to_owned(),
                version: "1".to_owned(),
            },
            strategy: StrategySpec {
                name: SMA_CROSS.to_owned(),
                params,
            },
            costs: CostModel::proportional(1.0, 0.0),
            risk: arvo_research::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 42,
        }
    }

    fn params(fast: f64, slow: f64) -> BTreeMap<String, f64> {
        BTreeMap::from([
            ("fast".to_owned(), fast),
            ("slow".to_owned(), slow),
            ("trade_size".to_owned(), 100.0),
        ])
    }

    fn provider(bars: Vec<arvo_data::Bar>) -> NautilusSimulation<InMemoryBars> {
        NautilusSimulation::new(InMemoryBars::new().with_instrument("AAPL.NASDAQ", bars))
    }

    #[test]
    fn an_experiment_runs_end_to_end_through_nautilus() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let simulation = provider(bars);

        let result = simulation
            .run(&experiment)
            .expect("the backtest should run");

        assert_eq!(result.experiment, experiment.id);
        assert_eq!(result.engine, ENGINE);
        assert!(
            !result.equity_curve.is_empty(),
            "a completed run always has at least its opening balance"
        );
        assert!(
            (result.equity_curve[0].equity - experiment.starting_cash).abs() < f64::EPSILON,
            "the curve opens at the starting balance"
        );
        assert!(result.trades > 0, "a crossing path should trade");
    }

    #[test]
    fn the_same_experiment_twice_gives_the_same_answer() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);

        let first = provider(bars.clone())
            .run(&experiment)
            .expect("first run should succeed");
        let second = provider(bars)
            .run(&experiment)
            .expect("second run should succeed");

        assert_eq!(
            first.equity_curve, second.equity_curve,
            "reproducibility is the whole point; two identical experiments must agree"
        );
        assert_eq!(first.trades, second.trades);
    }

    #[test]
    fn an_unknown_strategy_is_named_back() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.strategy.name = "buy_the_dip".to_owned();

        let err = provider(bars)
            .run(&experiment)
            .expect_err("nothing implements that");
        assert!(
            matches!(err, SimulationError::UnknownStrategy(ref name) if name == "buy_the_dip"),
            "{err}"
        );
    }

    #[test]
    fn slippage_makes_the_same_strategy_worse() {
        // The reason the feature exists: a backtest run without it is
        // optimistic, and the optimism has to show up as a number.
        let bars = sawtooth(200);
        let clean = experiment(params(5.0, 10.0), &bars);
        let mut slipped = clean.clone();
        slipped.costs.slippage_bps = 25.0;

        let without = provider(bars.clone()).run(&clean).expect("runs");
        let with = provider(bars).run(&slipped).expect("runs");

        assert_eq!(
            without.trades, with.trades,
            "slippage should cost money, not change which signals fired"
        );
        let final_equity = |result: &SimulationResult| {
            result.equity_curve.last().expect("non-empty").equity
        };
        assert!(
            final_equity(&with) < final_equity(&without),
            "slipped {} should end below unslipped {}",
            final_equity(&with),
            final_equity(&without)
        );
    }

    #[test]
    fn the_ledger_describes_the_same_run_the_curve_does() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let result = provider(bars).run(&experiment).expect("runs");

        assert_eq!(
            result.trades as usize,
            result.ledger.len(),
            "the count is derived from the ledger, so they cannot disagree"
        );
        assert!(!result.ledger.is_empty(), "the fixture is built to trade");

        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);
        assert_eq!(stats.closed + stats.still_open, result.trades);

        // Every closed trade must be a real round trip with both prices and
        // an ordered pair of timestamps. A ledger that reports a fill at zero
        // or an exit before its entry is worse than no ledger.
        for trade in result.ledger.iter().filter(|t| t.closed.is_some()) {
            assert!(trade.entry > 0.0 && trade.quantity > 0.0, "{trade:?}");
            assert!(trade.exit.expect("closed") > 0.0, "{trade:?}");
            assert!(trade.closed.expect("closed") >= trade.opened, "{trade:?}");
        }

        // The ledger is the only place commission is visible at all, and it
        // has to be non-zero at 1 bps or the cost model is not reaching fills.
        assert!(
            stats.total_commission > 0.0,
            "1 bps of commission should have been charged somewhere"
        );

        // Realised P&L must account for the curve. Not exactly — a position
        // still open at the end is marked to market by the curve and not yet
        // realised by the ledger — so the check is that they agree in sign
        // and magnitude once the open one is set aside.
        if stats.still_open == 0 {
            let realised: f64 = result.ledger.iter().map(|trade| trade.pnl).sum();
            let curve = result.equity_curve.last().expect("non-empty").equity
                - experiment.starting_cash;
            assert!(
                (realised - curve).abs() < 1.0,
                "ledger realised {realised} vs curve {curve}"
            );
        }
    }

    #[test]
    fn a_stop_exit_is_distinguishable_from_a_signal_exit() {
        // The strategy enforces its own stops with plain market orders, so
        // nothing about the order *type* says why it was sent. Without the
        // tag this reports zero stops on every run, which reads as a fact
        // about the strategy rather than a hole in the instrumentation.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: Some(1.0),
            atr_period: 14,
            ..arvo_research::RiskModel::default()
        };

        let result = provider(bars).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);

        assert_eq!(
            stats.signal_exits + stats.stop_exits,
            stats.closed,
            "every closed trade left for exactly one reason"
        );
        assert!(
            stats.stop_exits > 0,
            "a one-ATR stop on an oscillating fixture must be hit at least once;              got {} signal exits and no stops",
            stats.signal_exits
        );
    }

    #[test]
    fn venue_fees_cost_money_that_a_rate_alone_would_not_charge() {
        // The point of the extra shapes: a flat per-fill charge is invisible
        // to a bps model, and on a small position it is the dominant cost.
        let bars = sawtooth(200);
        let free = experiment(params(10.0, 30.0), &bars);
        let mut charged = free.clone();
        charged.costs.per_fill = 5.0;

        let without = provider(bars.clone()).run(&free).expect("runs");
        let with = provider(bars).run(&charged).expect("runs");

        let commission = |result: &SimulationResult| {
            arvo_research::TradeStats::from_ledger(&result.ledger).total_commission
        };
        assert_eq!(
            without.trades, with.trades,
            "a fee should cost money, not change which signals fired"
        );
        // Two fills a round trip at $5 each.
        let expected = 10.0 * f64::from(with.trades);
        let extra = commission(&with) - commission(&without);
        assert!(
            (extra - expected).abs() < 1.0,
            "expected about {expected} of extra fees, got {extra}"
        );
    }

    #[test]
    fn a_sell_side_fee_is_not_charged_on_the_buy() {
        // SEC- and FINRA-shaped charges fall on sales. Charging them both
        // ways would double a round trip's regulatory cost, and nothing in
        // the output would show it.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        // A round number against a ~$100 fixture: 100 bps of sale proceeds.
        experiment.costs.sell_notional_bps = 100.0;

        let result = provider(bars).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);

        // Sell proceeds only, so roughly 1% of one side of each round trip
        // rather than of both. Compared against the two-sided figure, which
        // is what a side-blind implementation would produce.
        let sold: f64 = result
            .ledger
            .iter()
            .filter_map(|trade| Some(trade.exit? * trade.quantity))
            .sum();
        let one_sided = sold * 0.01;
        assert!(
            stats.total_commission < one_sided * 1.5,
            "commission {} looks two-sided against {one_sided} of sale proceeds",
            stats.total_commission
        );
        assert!(
            stats.total_commission > one_sided * 0.5,
            "commission {} is too small to include the sell-side charge",
            stats.total_commission
        );
    }

    #[test]
    fn a_negative_fee_is_refused_rather_than_paying_us_to_trade() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.costs.per_fill = -1.0;

        let err = provider(bars).run(&experiment).expect_err("a fee is not a rebate");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn nonsense_slippage_is_refused_rather_than_run() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.costs.slippage_bps = -2.0;

        let err = provider(bars)
            .run(&experiment)
            .expect_err("negative slippage would pay us to trade");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn a_window_with_no_data_is_distinguished_from_a_bad_one() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.window =
            DateRange::new(date(2030, 1, 1), date(2030, 2, 1)).expect("window is ordered");

        let err = provider(bars).run(&experiment).expect_err("no bars there");
        assert!(matches!(err, SimulationError::NoData { .. }), "{err}");
    }

    #[test]
    fn too_little_history_to_fill_the_slow_average_is_rejected() {
        let bars = sawtooth(10);
        let experiment = experiment(params(5.0, 30.0), &bars);

        let err = provider(bars)
            .run(&experiment)
            .expect_err("30-period average cannot fill from 10 bars");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn nonsensical_periods_are_rejected_before_the_engine_starts() {
        let bars = sawtooth(200);

        for (fast, slow, why) in [
            (30.0, 10.0, "fast must be shorter than slow"),
            (0.0, 10.0, "a zero period is not a period"),
            (10.5, 30.0, "a period is a whole number of bars"),
        ] {
            let experiment = experiment(params(fast, slow), &bars);
            let err = provider(bars.clone()).run(&experiment).expect_err(why);
            assert!(matches!(err, SimulationError::Rejected(_)), "{why}: {err}");
        }
    }

    #[test]
    fn a_daily_bar_is_timestamped_at_the_close_of_its_day() {
        let ts = close_of_bar(
            date(2024, 1, 2).and_time(NaiveTime::MIN),
            arvo_data::BarInterval::DAILY,
        )
        .expect("date is representable");
        let expected = date(2024, 1, 3)
            .and_time(NaiveTime::MIN)
            .and_utc()
            .timestamp_nanos_opt()
            .expect("date is representable");
        assert_eq!(
            ts.as_u64(),
            u64::try_from(expected).expect("timestamp is positive"),
            "a close must not be visible before its day has ended"
        );
    }

    #[test]
    fn an_intraday_bar_closes_at_the_end_of_its_own_period() {
        // The look-ahead guard has to scale with the bar, not stay at a day:
        // a five-minute close is knowable five minutes after it opens, and
        // holding it back a whole day would hide a day of information.
        let opens = date(2024, 1, 2).and_hms_opt(14, 30, 0).expect("valid time");
        let ts = close_of_bar(
            opens,
            arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute),
        )
        .expect("representable");

        let expected = opens
            .and_utc()
            .timestamp_nanos_opt()
            .expect("representable")
            + 5 * 60 * 1_000_000_000;
        assert_eq!(
            ts.as_u64(),
            u64::try_from(expected).expect("positive"),
            "a five-minute bar closes five minutes after it opens"
        );
    }

    #[test]
    fn an_empty_return_series_still_yields_the_opening_balance() {
        let empty: std::collections::BTreeMap<UnixNanos, f64> = std::collections::BTreeMap::new();
        let curve = compound(
            100_000.0,
            date(2024, 1, 1).and_time(NaiveTime::MIN),
            empty.iter(),
        );
        assert_eq!(curve.len(), 1, "the opening balance is always a point");
        assert!((curve[0].equity - 100_000.0).abs() < f64::EPSILON);
        assert_eq!(curve[0].at.date(), date(2024, 1, 1));
    }

    /// A steadily rising market, so buy-and-hold must show a gain. If the
    /// benchmark comes back flat here, the position is being left open and
    /// unrealised and every comparison drawn against it is worthless.
    fn rising(days: usize) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            let close = 100.0 + index as f64 * 0.25;
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    #[test]
    fn the_benchmark_holds_the_market_rather_than_sitting_in_cash() {
        let bars = rising(120);
        let mut experiment = experiment(params(5.0, 20.0), &bars);
        experiment.strategy = arvo_research::evaluation::benchmark_for(&experiment).strategy;

        let result = provider(bars)
            .run(&experiment)
            .expect("benchmark should run");
        let metrics = arvo_research::Metrics::from_curve(
            &result.equity_curve,
            result.trades,
            arvo_research::evaluation::TRADING_DAYS_PER_YEAR,
        )
        .expect("a held position moves the curve");

        assert!(
            metrics.total_return > 0.0,
            "buy-and-hold in a rising market must gain; got {:?}",
            metrics
        );
    }

    #[test]
    fn the_research_loop_runs_end_to_end_and_produces_evidence() {
        let bars = sawtooth(400);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let criteria = arvo_research::EvaluationCriteria::default();

        let evidence =
            arvo_research::evaluate_against_benchmark(&provider(bars), &experiment, &criteria)
                .expect("both runs should complete");

        assert_eq!(evidence.hypothesis, experiment.hypothesis);
        assert_eq!(
            evidence.experiment, experiment,
            "the record pins the whole run"
        );
        assert_eq!(evidence.engine, ENGINE);
        assert_ne!(
            evidence.benchmark, experiment.id,
            "the benchmark is a separate run"
        );
        assert!(
            !evidence.evaluation.reasons.is_empty(),
            "a verdict without a reason is not evidence"
        );
        // The control is not expected to beat the market; what matters is that
        // the loop reached a stated verdict rather than an accident.
        assert!(
            matches!(
                evidence.evaluation.verdict,
                arvo_research::Verdict::NotSupported | arvo_research::Verdict::Inconclusive
            ),
            "a moving-average crossover should not beat buy-and-hold here: {:?}",
            evidence.evaluation
        );
    }

    #[test]
    fn a_family_selects_in_sample_and_is_judged_out_of_sample() {
        use arvo_research::{ExperimentFamily, ParameterGrid};

        let bars = sawtooth(600);
        let template = experiment(params(10.0, 30.0), &bars);
        let family = ExperimentFamily::new(
            template.clone(),
            ParameterGrid::new()
                .axis("fast", vec![5.0, 10.0, 15.0])
                .axis("slow", vec![30.0, 50.0]),
        );

        let found = arvo_research::run_family(
            &provider(bars),
            &family,
            &arvo_research::EvaluationCriteria::default(),
        )
        .expect("the family should run");

        assert_eq!(
            found.selection.trials, 6,
            "every grid point should have run"
        );
        assert!(
            found.in_sample.to < found.out_of_sample.from,
            "the winner must be judged on days it was not chosen on"
        );
        assert_eq!(
            found.selected.window, found.out_of_sample,
            "the reported run is the out-of-sample one"
        );
        assert!(
            found.out_of_sample_evidence.experiment.window == found.out_of_sample,
            "and its evidence agrees"
        );
        assert!(
            !matches!(found.verdict, arvo_research::Verdict::Supported),
            "a moving-average grid should not beat buy-and-hold out of sample: {:?}",
            found.reasons
        );
    }

    #[test]
    fn returns_compound_rather_than_summing() {
        let day = |d: u32| {
            close_of_bar(
                date(2024, 1, d).and_time(NaiveTime::MIN),
                arvo_data::BarInterval::DAILY,
            )
            .expect("representable")
        };
        let returns: std::collections::BTreeMap<UnixNanos, f64> =
            [(day(1), 0.1), (day(2), 0.1)].into_iter().collect();

        let curve = compound(
            100.0,
            date(2024, 1, 1).and_time(NaiveTime::MIN),
            returns.iter(),
        );
        assert!((curve[2].equity - 121.0).abs() < 1e-9, "{curve:?}");
        assert_eq!(
            curve[2].at.date(),
            date(2024, 1, 3),
            "a bar timestamped at its close lands on the following calendar day"
        );
    }
}
