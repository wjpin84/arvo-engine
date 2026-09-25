//! Strategy requests, parsed out of the untyped spec and checked before a
//! backtest starts.

use arvo_research::{SimulationError, StrategySpec};

use crate::strategy;
use crate::{
    BUY_AND_HOLD, CROSS_SECTIONAL, MOMENTUM_BREAKOUT, OPENING_RANGE, PUT_SPREAD, SELL_AND_HOLD,
    SESSION_ANCHORED, SMA_CROSS, VOLATILITY_BREAKOUT, VWAP_REVERSION, ZERO_DTE_BREAKOUT,
    ZERO_DTE_PUT_SPREAD,
};

/// A strategy request, parsed out of the untyped spec and validated before
/// anything expensive starts.
pub(crate) enum Plan {
    /// A rule written as data (#225), resolved against the spec's params.
    Data {
        rule: Box<arvo_research::rule::Resolved>,
        trade_size: f64,
    },
    SmaCross {
        fast_period: usize,
        slow_period: usize,
        trade_size: f64,
    },
    OpeningRange {
        range_bars: usize,
        target_range_multiple: f64,
        trade_size: f64,
    },
    VolatilityBreakout {
        entry_atr_multiple: f64,
        atr_period: usize,
        trade_size: f64,
    },
    VwapReversion {
        entry_deviations: f64,
        trade_size: f64,
    },
    MomentumBreakout {
        entry_period: usize,
        exit_period: usize,
        trade_size: f64,
    },
    CrossSectionalMomentum {
        lookback: usize,
        hold_top: usize,
        trade_size: f64,
    },
    BuyAndHold {
        trade_size: f64,
    },
    SellAndHold {
        trade_size: f64,
    },
    PutSpread {
        rule: strategy::PutSpreadRule,
        trade_size: f64,
    },
    ZeroDteBreakout {
        rule: strategy::BreakoutRule,
        trade_size: f64,
    },
}

impl Plan {
    /// Parses and validates a strategy request.
    ///
    /// The interval is needed as well as the spec because two of these rules
    /// are only defined intraday, and a resolution mismatch is not something
    /// the result would show: the run completes, the curve looks ordinary, and
    /// the numbers describe a rule nobody meant to test.
    pub(crate) fn from_spec(
        spec: &StrategySpec,
        interval: arvo_data::BarInterval,
    ) -> Result<Self, SimulationError> {
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

        let multiple = |name: &str| -> Result<f64, SimulationError> {
            let value = param(name)?;
            if !value.is_finite() || value <= 0.0 || value > 100.0 {
                return Err(SimulationError::Rejected(format!(
                    "{name} must be a positive multiple no greater than 100, got {value}"
                )));
            }
            Ok(value)
        };

        // A rule written as data carries its definition; the name is its own.
        if let Some(definition) = &spec.rule {
            let resolved = definition
                .resolve(&spec.params)
                .map_err(|err| SimulationError::Rejected(err.to_string()))?;
            if resolved.interval != interval {
                return Err(SimulationError::Rejected(format!(
                    "{} is defined at {}, and the run says {interval}",
                    resolved.name, resolved.interval
                )));
            }
            return Ok(Self::Data { rule: Box::new(resolved), trade_size: trade_size()? });
        }

        if SESSION_ANCHORED.contains(&spec.name.as_str()) && !interval.is_intraday() {
            return Err(SimulationError::Rejected(format!(
                "{} is defined against a trading session and cannot run on {interval} bars; at \
                 that resolution a session is a single bar, so the rule would still produce a \
                 curve while measuring something nobody asked for",
                spec.name
            )));
        }

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
            OPENING_RANGE => Ok(Self::OpeningRange {
                range_bars: period("range_bars")?,
                target_range_multiple: multiple("target_range_multiple")?,
                trade_size: trade_size()?,
            }),
            VOLATILITY_BREAKOUT => Ok(Self::VolatilityBreakout {
                entry_atr_multiple: multiple("entry_atr_multiple")?,
                atr_period: period("atr_period")?,
                trade_size: trade_size()?,
            }),
            VWAP_REVERSION => Ok(Self::VwapReversion {
                entry_deviations: multiple("entry_deviations")?,
                trade_size: trade_size()?,
            }),
            MOMENTUM_BREAKOUT => {
                let entry_period = period("entry_period")?;
                let exit_period = period("exit_period")?;
                if exit_period > entry_period {
                    return Err(SimulationError::Rejected(format!(
                        "exit period {exit_period} must not exceed entry period {entry_period}; a \
                         rule that needs more evidence to leave than to enter gives most of a \
                         trend back before it admits the trend ended"
                    )));
                }
                Ok(Self::MomentumBreakout {
                    entry_period,
                    exit_period,
                    trade_size: trade_size()?,
                })
            }
            CROSS_SECTIONAL => {
                let lookback = period("lookback")?;
                let hold_top = period("hold_top")?;
                Ok(Self::CrossSectionalMomentum {
                    lookback,
                    hold_top,
                    trade_size: trade_size()?,
                })
            }
            BUY_AND_HOLD => Ok(Self::BuyAndHold {
                trade_size: trade_size()?,
            }),
            SELL_AND_HOLD => Ok(Self::SellAndHold {
                trade_size: trade_size()?,
            }),
            ZERO_DTE_BREAKOUT => {
                if !interval.is_intraday() {
                    return Err(SimulationError::Rejected(format!(
                        "{ZERO_DTE_BREAKOUT} reads a session's opening range and cannot run on \
                         {interval} bars"
                    )));
                }
                let delta = param("delta")?;
                if !delta.is_finite() || delta <= 0.0 || delta >= 1.0 {
                    return Err(SimulationError::Rejected(format!(
                        "delta must be a fraction between 0 and 1, got {delta}"
                    )));
                }
                let target_multiple = param("target_multiple")?;
                if !target_multiple.is_finite() || target_multiple <= 1.0 || target_multiple > 20.0 {
                    return Err(SimulationError::Rejected(format!(
                        "target_multiple is what the option must reach as a multiple of its price, \
                         above 1 and no more than 20, got {target_multiple}"
                    )));
                }
                let stop_fraction = param("stop_fraction")?;
                if !stop_fraction.is_finite() || stop_fraction <= 0.0 || stop_fraction >= 1.0 {
                    return Err(SimulationError::Rejected(format!(
                        "stop_fraction is the share of the price lost before selling, between 0 \
                         and 1, got {stop_fraction}"
                    )));
                }
                let rate = |name: &str| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    if !value.is_finite() || !(0.0..0.5).contains(&value) {
                        return Err(SimulationError::Rejected(format!(
                            "{name} is a yearly fraction (0.04 is 4%), got {value}"
                        )));
                    }
                    Ok(value)
                };
                Ok(Self::ZeroDteBreakout {
                    rule: strategy::BreakoutRule {
                        range_bars: period("range_bars")?,
                        delta,
                        target_multiple,
                        stop_fraction,
                        rate: rate("rate")?,
                        dividend_yield: rate("dividend_yield")?,
                    },
                    trade_size: trade_size()?,
                })
            }
            PUT_SPREAD | ZERO_DTE_PUT_SPREAD => {
                let same_day = spec.name == ZERO_DTE_PUT_SPREAD;
                // A month-out spread is chosen on a day's closes; a same-day one
                // at a time of day, which daily bars do not have.
                if same_day != interval.is_intraday() {
                    return Err(SimulationError::Rejected(format!(
                        "{} decides on {} bars and cannot run on {interval} bars",
                        spec.name,
                        if same_day { "intraday" } else { "daily" }
                    )));
                }
                let fraction = |name: &str, upper_inclusive: bool| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    let fits = value.is_finite()
                        && value > 0.0
                        && (value < 1.0 || (upper_inclusive && value <= 1.0));
                    if !fits {
                        return Err(SimulationError::Rejected(format!(
                            "{name} must be a fraction between 0 and 1, got {value}"
                        )));
                    }
                    Ok(value)
                };
                let rate = |name: &str| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    if !value.is_finite() || !(0.0..0.5).contains(&value) {
                        return Err(SimulationError::Rejected(format!(
                            "{name} is a yearly fraction (0.04 is 4%), got {value}"
                        )));
                    }
                    Ok(value)
                };
                let (dte, exit_dte, stop_multiple, entry_minutes) = if same_day {
                    let stop = param("stop_multiple")?;
                    if !stop.is_finite() || stop <= 1.0 || stop > 20.0 {
                        return Err(SimulationError::Rejected(format!(
                            "stop_multiple is what buying the spread back may cost as a multiple \
                             of its credit, above 1 and no more than 20, got {stop}"
                        )));
                    }
                    let entry = param("entry_minutes")?;
                    // Leaves the last hour: an entry in it is a different trade.
                    if !entry.is_finite() || entry.fract() != 0.0 || !(0.0..=330.0).contains(&entry) {
                        return Err(SimulationError::Rejected(format!(
                            "entry_minutes is whole minutes after the open, 0 to 330, got {entry}"
                        )));
                    }
                    (0, None, Some(stop), Some(entry as i64))
                } else {
                    let dte = period("dte")?;
                    let exit_dte = param("exit_dte")?;
                    if !exit_dte.is_finite()
                        || exit_dte < 0.0
                        || exit_dte.fract() != 0.0
                        || exit_dte >= dte as f64
                    {
                        return Err(SimulationError::Rejected(format!(
                            "exit_dte must be a whole number of days below dte ({dte}), got {exit_dte}"
                        )));
                    }
                    (dte as i64, Some(exit_dte as i64), None, None)
                };
                let width = param("width")?;
                if !width.is_finite() || width <= 0.0 {
                    return Err(SimulationError::Rejected(format!(
                        "width is a strike distance in dollars and must be positive, got {width}"
                    )));
                }
                Ok(Self::PutSpread {
                    rule: strategy::PutSpreadRule {
                        dte,
                        short_delta: fraction("short_delta", false)?,
                        width,
                        take_profit: fraction("take_profit", true)?,
                        exit_dte,
                        stop_multiple,
                        entry_minutes,
                        rate: rate("rate")?,
                        dividend_yield: rate("dividend_yield")?,
                    },
                    trade_size: trade_size()?,
                })
            }
            _ => Err(SimulationError::UnknownStrategy(spec.name.clone())),
        }
    }

    /// Bars needed before the strategy can act at all.
    pub(crate) fn min_bars(&self) -> usize {
        match self {
            Self::Data { rule, .. } => rule.min_bars(),
            Self::SmaCross { slow_period, .. } => *slow_period,
            // The range itself. A window that only covers the range has not
            // given the rule a single bar to break out on.
            Self::OpeningRange { range_bars, .. } => *range_bars,
            Self::VolatilityBreakout { atr_period, .. } => *atr_period,
            // Enough of a session for a volume-weighted deviation to mean
            // something, which is the gate this rule's entry waits on.
            Self::VwapReversion { .. } => 5,
            Self::MomentumBreakout { entry_period, .. } => *entry_period,
            // The lookback the ranking is computed over. Until every
            // instrument has one, the field is partial and the ranking is a
            // statement about whichever happened to warm up first.
            Self::CrossSectionalMomentum { lookback, .. } => *lookback,
            // One to buy on, and at least one more for the position to have
            // done anything.
            Self::BuyAndHold { .. } | Self::SellAndHold { .. } | Self::PutSpread { .. } => 1,
            // The range has to be formed before a break means anything.
            Self::ZeroDteBreakout { rule, .. } => rule.range_bars,
        }
    }

    pub(crate) const fn trade_size(&self) -> f64 {
        match self {
            Self::Data { trade_size, .. }
            | Self::SmaCross { trade_size, .. }
            | Self::OpeningRange { trade_size, .. }
            | Self::VolatilityBreakout { trade_size, .. }
            | Self::VwapReversion { trade_size, .. }
            | Self::MomentumBreakout { trade_size, .. }
            | Self::CrossSectionalMomentum { trade_size, .. }
            | Self::BuyAndHold { trade_size }
            | Self::SellAndHold { trade_size }
            | Self::PutSpread { trade_size, .. }
            | Self::ZeroDteBreakout { trade_size, .. } => *trade_size,
        }
    }
}
