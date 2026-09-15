//! Option runs: what a contract settles against, and which contracts a chain
//! rule may choose from.

use std::str::FromStr;

use arvo_data::BarProvider;
use arvo_research::{Experiment, SimulationError, SimulationResult};
use nautilus_model::identifiers::InstrumentId;

use crate::backtest::run_backtest;
use crate::plan::Plan;
use crate::NautilusSimulation;

/// What an option run settles against at expiry (#84).
pub(crate) struct Settlement {
    /// The underlying's symbol, which Nautilus looks up on the option's venue.
    pub(crate) symbol: String,
    /// The underlying's own bars, when a rule reads them to decide (#86).
    /// Empty for a run that only settles against it.
    pub(crate) drive: Vec<arvo_data::Bar>,
    /// Each contract expiring inside the window, with the underlying's close on
    /// that contract's expiration date.
    pub(crate) closes: Vec<(arvo_data::option::OptionContract, f64)>,
}

impl<P: BarProvider> NautilusSimulation<P> {
    /// The underlying an option run names, and its close on every expiration
    /// the window reaches.
    ///
    /// Refused rather than run without, because a contract held to expiry
    /// with nothing to settle against stays open and is marked at its last
    /// trade for as long as the window runs — found exactly that way, holding
    /// one for four months after it ceased to exist.
    pub(crate) fn settlement(
        &self,
        experiment: &Experiment,
        contracts: &[arvo_data::option::OptionContract],
    ) -> Result<Settlement, SimulationError> {
        let name = experiment.underlying.as_deref().ok_or_else(|| {
            SimulationError::Rejected(
                "an option run needs `underlying`, the stock series it settles against \
                 (e.g. SPY.AIEX): without it a contract held to expiry is never settled"
                    .to_owned(),
            )
        })?;
        self.settlement_against(name, experiment, contracts)
    }

    /// As [`Self::settlement`], against a named underlying.
    fn settlement_against(
        &self,
        name: &str,
        experiment: &Experiment,
        contracts: &[arvo_data::option::OptionContract],
    ) -> Result<Settlement, SimulationError> {
        let rejected = |why: String| SimulationError::Rejected(why);
        let symbol = name.split('.').next().unwrap_or_default();
        if let Some(contract) = contracts.iter().find(|c| c.underlying != symbol) {
            return Err(rejected(format!(
                "{} is an option on {}, and the underlying named is {name}",
                contract.symbol(),
                contract.underlying
            )));
        }
        let bars = self
            .bars
            .bars(name, experiment.interval, experiment.window.from, experiment.window.to)
            .map_err(|err| rejected(format!("reading underlying {name}: {err}")))?;

        let mut closes = Vec::new();
        for contract in contracts
            .iter()
            .filter(|c| c.expiration <= experiment.window.to)
        {
            // The last bar on the expiration date closes at 16:00 at any
            // resolution: the daily bar, or the 15:55 five-minute bar.
            let close = bars
                .iter()
                .rev()
                .find(|bar| bar.at.date() == contract.expiration)
                .map(|bar| bar.close)
                .ok_or_else(|| {
                    rejected(format!(
                        "{name} has no bar on {}, the day {} settles",
                        contract.expiration,
                        contract.symbol()
                    ))
                })?;
            closes.push((contract.clone(), close));
        }
        Ok(Settlement {
            symbol: symbol.to_owned(),
            drive: Vec::new(),
            closes,
        })
    }
}

/// How far below the underlying's lowest close a put spread run loads strikes.
///
/// Wide on purpose. A 20-delta put a month out sits a few percent below spot; a
/// short strike the rule wants that lies below what was loaded is caught and
/// skipped (`put_spread::Skip::EdgeOfChain`) rather than silently replaced.
const PUT_SPREAD_REACH: f64 = 0.30;

impl<P: BarProvider> NautilusSimulation<P> {
    /// A put spread run: the underlying drives, the chain's puts trade (#86).
    ///
    /// # Which contracts are loaded
    ///
    /// Puts expiring from the window's start to the rule's target days past its
    /// end, struck between [`PUT_SPREAD_REACH`] below the underlying's lowest
    /// price and its highest, over the days each could have been opened on.
    /// That range reads prices after an entry date, which would be look-ahead
    /// if it chose anything; it only decides what is *available*, as wide as the
    /// rule could want, and the rule refuses a strike at its edge.
    pub(crate) fn run_put_spread(
        &self,
        experiment: &Experiment,
        plan: &Plan,
    ) -> Result<SimulationResult, SimulationError> {
        // What the rule can choose from: how far past a day its expirations may
        // lie, whether it buys calls too, and so which side of the money needs
        // strikes.
        let (days_out, calls) = match *plan {
            Plan::PutSpread { rule, .. } => (rule.dte + 7, false),
            Plan::ZeroDteBreakout { .. } => (0, true),
            _ => return Err(SimulationError::Rejected("not a chain plan".to_owned())),
        };
        let rejected = |why: String| SimulationError::Rejected(why);
        let name = experiment.instrument.as_str();
        if arvo_data::option::OptionContract::parse(name).is_some()
            || !experiment.alongside.is_empty()
        {
            return Err(rejected(
                "put_spread runs on one underlying (e.g. SPY.AIEX) and chooses its own contracts"
                    .to_owned(),
            ));
        }
        if experiment
            .underlying
            .as_deref()
            .is_some_and(|underlying| underlying != name)
        {
            return Err(rejected(format!(
                "put_spread settles against its own instrument {name}, not {:?}",
                experiment.underlying
            )));
        }
        // The rule sizes by collateral and exits by its own levels. A stop, a
        // risk fraction or a drawdown halt would be recorded and not applied.
        let risk = experiment.risk;
        if risk.stop_atr_multiple.is_some()
            || risk.risk_per_trade.is_some()
            || risk.max_drawdown.is_some()
        {
            return Err(rejected(
                "put_spread does not apply a stop, a risk fraction or a drawdown halt; leave \
                 them unset rather than record limits the run ignores"
                    .to_owned(),
            ));
        }
        if experiment.costs.option_spread.is_none() || experiment.costs.slippage_bps != 0.0 {
            return Err(rejected(
                "cost model: an option run needs option_spread and no slippage_bps".to_owned(),
            ));
        }

        let (from, to) = (experiment.window.from, experiment.window.to);
        let underlying = self
            .bars
            .bars(name, experiment.interval, from, to)
            .map_err(|err| rejected(format!("reading {name}: {err}")))?;
        if underlying.is_empty() {
            return Err(SimulationError::NoData {
                instrument: name.to_owned(),
                from,
                to,
            });
        }
        let symbol = name.split('.').next().unwrap_or_default();

        let reach = chrono::Duration::days(days_out);
        let mut book = Vec::new();
        let mut contracts = Vec::new();
        let listed = self
            .bars
            .option_contracts(symbol, experiment.interval)
            .map_err(|err| rejected(format!("listing {symbol} contracts: {err}")))?;
        for contract_name in listed {
            let Some(contract) = arvo_data::option::OptionContract::parse(&contract_name) else {
                continue;
            };
            let is_call = contract.right == arvo_data::option::Right::Call;
            if (is_call && !calls) || contract.expiration < from || contract.expiration > to + reach
            {
                continue;
            }
            let openable = underlying
                .iter()
                .filter(|bar| {
                    bar.at.date() >= contract.expiration - reach
                        && bar.at.date() <= contract.expiration
                })
                .map(|bar| (bar.low, bar.high))
                .reduce(|(low, high), (l, h)| (low.min(l), high.max(h)));
            let Some((low, high)) = openable else {
                continue;
            };
            let (floor, ceiling) = if calls {
                (low * (1.0 - PUT_SPREAD_REACH), high * (1.0 + PUT_SPREAD_REACH))
            } else {
                (low * (1.0 - PUT_SPREAD_REACH), high)
            };
            if contract.strike < floor || contract.strike > ceiling {
                continue;
            }
            let bars = self
                .bars
                .bars(&contract_name, experiment.interval, from, to)
                .map_err(|err| rejected(format!("reading {contract_name}: {err}")))?;
            if bars.is_empty() {
                continue;
            }
            let id = InstrumentId::from_str(&contract_name)
                .map_err(|err| rejected(format!("instrument {contract_name:?}: {err}")))?;
            contracts.push(contract);
            book.push((id, contract_name, bars));
        }
        let Some(venue) = book.first().map(|(id, _, _)| id.venue) else {
            return Err(rejected(format!(
                "no {symbol} contracts in the library for {from}..{to} at {}; fetch the chain first",
                experiment.interval
            )));
        };
        if let Some((_, other, _)) = book.iter().find(|(id, _, _)| id.venue != venue) {
            return Err(rejected(format!(
                "{other} is not on {venue}: a shared account cannot span venues"
            )));
        }
        // Sorted, so instruments are added and ids issued in the same order on
        // every run.
        book.sort_by(|a, b| a.1.cmp(&b.1));

        let mut settlement = self.settlement_against(name, experiment, &contracts)?;
        settlement.drive = underlying;
        run_backtest(experiment, plan, &book, Some(&settlement))
    }
}
