//! How a strategy protects itself: the limits pinned into every experiment.

use serde::{Deserialize, Serialize};

/// How a strategy protects itself.
///
/// Pinned into the experiment beside the cost model, and for the same reason:
/// it changes the result more than most strategy parameters do. For most
/// systematic strategies the stop is not a detail bolted on afterwards — it
/// *is* part of the rule, and "momentum breakout with a 2× ATR stop" and
/// "momentum breakout" are two different strategies with different return
/// distributions.
///
/// Running without one is allowed and is the honest default for a control,
/// but it should be a stated choice rather than an omission: an unstopped
/// strategy has a fatter left tail than the stopped version of itself, so a
/// backtest that quietly leaves the stop out flatters the idea.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskModel {
    /// Stop distance as a multiple of ATR. `None` runs with no stop at all.
    ///
    /// A multiple rather than a fixed price: a stop has to scale with how
    /// much the instrument actually moves, or it is arbitrarily tight on a
    /// volatile name and arbitrarily loose on a quiet one.
    pub stop_atr_multiple: Option<f64>,
    /// Bars of history the ATR is averaged over.
    pub atr_period: usize,
    /// Fraction of starting capital risked on one trade, sizing the position
    /// so that a stop-out costs about that much.
    ///
    /// `None` trades a fixed quantity instead. Requires a stop: risking 1%
    /// means nothing without a distance to the exit, and there is no
    /// defensible default to invent for it.
    pub risk_per_trade: Option<f64>,
    /// The largest fraction of starting capital one position may occupy.
    ///
    /// Not a refinement — without it, risk sizing is unbounded. Position size
    /// is capital-at-risk divided by the stop distance, and a *tight* stop
    /// therefore buys a *bigger* position. At a daily resolution ATR is wide
    /// enough that this never binds; on five-minute bars a one-dollar stop on
    /// a five-hundred-dollar share asks for a position several times the
    /// account, every order is rejected, and the backtest reports zero trades
    /// with no error anywhere. Found exactly that way.
    ///
    /// Defaults to one whole account: you cannot spend money you do not have,
    /// which is arithmetic rather than a strategy choice. Above 1.0 is
    /// leverage and has to be asked for.
    pub max_position_fraction: Option<f64>,
    /// How far the account may fall below its own peak before the rule stops
    /// trading, as a fraction. `None` runs without a halt.
    ///
    /// A per-*trade* stop and this are different instruments. A stop bounds
    /// one loss; this bounds the sum of them, which is the number that
    /// actually ends accounts — twenty consecutive stop-outs each costing one
    /// percent is a well-behaved rule and a twenty percent hole.
    ///
    /// The halt is **permanent for the run**. Nothing else is coherent: a rule
    /// that stops trading cannot recover the equity that would let it resume,
    /// so "halt until recovered" would either never resume or would have to
    /// keep trading to find out — which is not a halt.
    ///
    /// It changes what a result *means*, not just its size. A halted run
    /// reports the return it had when it stopped, over a window it did not
    /// finish, and the ledger's last trade says so.
    #[serde(default)]
    pub max_drawdown: Option<f64>,
    /// The most positions the account may hold at once, across every
    /// instrument in the run.
    ///
    /// Only reachable once a run could hold more than one instrument. Before
    /// books there was one position or none and the question could not arise;
    /// now a rule pointed at eight instruments can be in all eight, and the
    /// only thing that stops it is running out of cash — which is a fact about
    /// the account, not a risk decision.
    ///
    /// Counted against what the *engine* holds rather than what any one
    /// strategy believes it holds. The members of a book are separate strategy
    /// instances that share an account and know nothing of each other, so a
    /// cap kept per strategy would be N caps of one.
    ///
    /// `None` is no limit, which is every run recorded before this existed.
    #[serde(default)]
    pub max_concurrent_positions: Option<usize>,
    /// How much of starting capital may be lost in realised losses in one day
    /// before trading stops until tomorrow, as a fraction.
    ///
    /// A different instrument from [`Self::max_drawdown`], and the difference
    /// is the point. The drawdown halt is a conclusion — the idea is wrong,
    /// stop funding it, permanently. This is a rule about how bad a single
    /// session may get, and it lifts overnight. A system with only the halt
    /// either sets it tight enough to end the account's life on one bad
    /// morning, or loose enough that a bad morning runs unimpeded.
    ///
    /// Realised only. An open position moving against you is what the drawdown
    /// halt watches; counting it here too would stop trading on a mark that may
    /// reverse before it is ever booked.
    ///
    /// `None` is no limit, which is every run recorded before this existed.
    #[serde(default)]
    pub max_daily_loss: Option<f64>,
    /// How many positions may be held among instruments that move together.
    ///
    /// [`Self::max_concurrent_positions`] counts tickers; this counts *bets*.
    /// Five positions each mildly correlated with the index and almost
    /// perfectly correlated with each other is one bet wearing five names, and
    /// a cap on the count alone says nothing about it.
    ///
    /// `None` is no cap. Setting one requires something that can supply
    /// correlations at decision time — see [`crate::Correlations`], and
    /// note that a cap with no source refuses rather than passes.
    #[serde(default)]
    pub correlation_cap: Option<crate::CorrelationCap>,
    /// How many positions may be held in one sector (#29).
    ///
    /// `None` is no cap, which is every run recorded before this existed. A
    /// name the cap has no sector for is refused rather than assumed to be in
    /// a sector of its own — see [`crate::Rejection::SectorUnknown`].
    #[serde(default)]
    pub sector_cap: Option<crate::SectorCap>,
    /// Which day-trading constraint the account is subject to.
    ///
    /// A backtest that ignores this is backtesting a system nobody can open: a
    /// day-trading rule on a small margin account gets three round trips per
    /// five business days in reality and unlimited ones in a simulation that
    /// does not model the rule.
    ///
    /// `default` is unconstrained, which is what every finding recorded before
    /// this existed actually ran under.
    #[serde(default)]
    pub day_trading: crate::DayTradingRule,
}

impl Default for RiskModel {
    /// No stop and fixed sizing — what a backtest does when nobody has said
    /// otherwise. Named rather than implied, so the absence is visible in the
    /// record.
    fn default() -> Self {
        Self {
            stop_atr_multiple: None,
            atr_period: 14,
            risk_per_trade: None,
            max_position_fraction: Some(1.0),
            // No halt by default. A drawdown limit is a real choice about how
            // much of an idea you are willing to fund before concluding it is
            // wrong, and inventing one would silently change every result.
            max_drawdown: None,
            // No cap either, for the same reason: a limit on concurrent
            // positions is a decision about how much of the account one idea
            // may occupy, and choosing one here would change every book that
            // has ever run without anyone asking for it.
            max_concurrent_positions: None,
            // Both of these change what a run does, so neither gets a default
            // that nobody asked for. An invented daily limit would silently
            // truncate sessions in every stored finding.
            max_daily_loss: None,
            correlation_cap: None,
            sector_cap: None,
            // Unconstrained, and named rather than implied: modelling the
            // pattern-day-trader rule changes how many trades a run can make,
            // so switching it on silently would change every stored result.
            day_trading: crate::DayTradingRule::Unconstrained,
        }
    }
}

impl RiskModel {
    /// Rejects combinations that cannot mean anything.
    ///
    /// # Errors
    ///
    /// Returns a reason if the model is self-contradictory — most importantly
    /// sizing by risk with no stop to measure the risk against, which would
    /// otherwise silently fall back to some invented quantity.
    pub fn check(&self) -> Result<(), String> {
        if self.atr_period == 0 {
            return Err("atr_period must be at least 1 bar".to_owned());
        }
        if let Some(limit) = self.max_drawdown {
            // Zero halts before the first trade; one or more can never be
            // reached. Both describe a run nobody meant to ask for.
            if !limit.is_finite() || limit <= 0.0 || limit >= 1.0 {
                return Err(format!(
                    "max_drawdown must be a fraction between 0 and 1, got {limit}"
                ));
            }
        }
        if let Some(multiple) = self.stop_atr_multiple {
            if !multiple.is_finite() || multiple <= 0.0 {
                return Err(format!(
                    "stop_atr_multiple must be positive, got {multiple}"
                ));
            }
        }
        if let Some(cap) = self.max_position_fraction {
            if !cap.is_finite() || cap <= 0.0 {
                return Err(format!("max_position_fraction must be positive, got {cap}"));
            }
        }
        if let Some(risk) = self.risk_per_trade {
            if !risk.is_finite() || risk <= 0.0 || risk >= 1.0 {
                return Err(format!(
                    "risk_per_trade is a fraction of capital between 0 and 1, got {risk}"
                ));
            }
            if self.stop_atr_multiple.is_none() {
                return Err(
                    "risk_per_trade needs a stop: position size is capital-at-risk divided by \
                     the distance to the exit, and without a stop there is no distance"
                        .to_owned(),
                );
            }
        }
        if let Some(limit) = self.max_daily_loss {
            // Zero stops before the first trade; one or more can never be
            // reached. Both describe a session nobody meant to ask for.
            if !limit.is_finite() || limit <= 0.0 || limit >= 1.0 {
                return Err(format!(
                    "max_daily_loss must be a fraction between 0 and 1, got {limit}"
                ));
            }
        }
        if let Some(cap) = self.correlation_cap {
            if !cap.above.is_finite() || !(0.0..=1.0).contains(&cap.above) {
                return Err(format!(
                    "correlation_cap.above must be a correlation between 0 and 1, got {}",
                    cap.above
                ));
            }
            if cap.max_positions == 0 {
                return Err(
                    "correlation_cap.max_positions of 0 refuses every correlated trade; remove the cap instead of setting it to zero"
                        .to_owned(),
                );
            }
        }
        if let Some(cap) = &self.sector_cap {
            if cap.max_positions == 0 {
                return Err(
                    "sector_cap.max_positions of 0 refuses every trade; remove the cap instead of setting it to zero"
                        .to_owned(),
                );
            }
            if cap.sectors.is_empty() {
                return Err(
                    "sector_cap names no sectors, so it would refuse every trade as unclassified"
                        .to_owned(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_by_risk_without_a_stop_is_refused() {
        // The important one. Position size is capital-at-risk divided by the
        // distance to the exit; with no stop there is no distance, and the
        // only alternatives are to invent a quantity or to silently ignore
        // the risk setting. Both are worse than refusing.
        let model = RiskModel {
            stop_atr_multiple: None,
            risk_per_trade: Some(0.01),
            ..RiskModel::default()
        };
        let err = model.check().expect_err("should refuse");
        assert!(err.contains("needs a stop"), "{err}");
    }

    #[test]
    fn a_stop_and_a_risk_fraction_together_are_valid() {
        let model = RiskModel {
            stop_atr_multiple: Some(2.0),
            risk_per_trade: Some(0.01),
            ..RiskModel::default()
        };
        assert!(model.check().is_ok());
    }

    #[test]
    fn a_position_cap_bounds_what_risk_sizing_can_ask_for() {
        // The intraday failure this exists to prevent: position size is
        // capital-at-risk over stop distance, so a one-dollar stop on a
        // five-hundred-dollar share asks for several accounts' worth. Every
        // order is then rejected and the backtest reports no trades at all,
        // with nothing anywhere saying why.
        let uncapped = RiskModel {
            stop_atr_multiple: Some(2.0),
            risk_per_trade: Some(0.01),
            max_position_fraction: None,
            ..RiskModel::default()
        };
        assert!(
            uncapped.check().is_ok(),
            "uncapped is legal, just unbounded"
        );
        assert_eq!(
            RiskModel::default().max_position_fraction,
            Some(1.0),
            "and the default is one whole account"
        );
    }

    #[test]
    fn nonsensical_risk_settings_are_refused() {
        for (model, why) in [
            (
                RiskModel {
                    stop_atr_multiple: Some(-1.0),
                    ..RiskModel::default()
                },
                "a negative stop distance",
            ),
            (
                RiskModel {
                    atr_period: 0,
                    ..RiskModel::default()
                },
                "an ATR over zero bars",
            ),
            (
                RiskModel {
                    stop_atr_multiple: Some(2.0),
                    risk_per_trade: Some(1.5),
                    ..RiskModel::default()
                },
                "risking more than all the capital",
            ),
        ] {
            assert!(model.check().is_err(), "{why} should be refused");
        }
    }

    #[test]
    fn the_default_risk_model_is_no_stop_and_says_so() {
        let model = RiskModel::default();
        assert_eq!(model.stop_atr_multiple, None);
        assert_eq!(model.risk_per_trade, None);
        assert_eq!(
            model.max_position_fraction,
            Some(1.0),
            "spending more than the account holds is not a strategy choice"
        );
        assert!(model.check().is_ok(), "absence is valid, just visible");
    }
}
