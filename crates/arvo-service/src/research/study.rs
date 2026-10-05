//! The runs themselves: a study, a book, a panel, a walk-forward.
//!
//! Every one of these states its assumptions in the result rather than
//! implying them — the split, the number of configurations tried, the bar a
//! no-skill search would clear, the costs assumed.

use super::*;

/// Persists a finding and hands back its view.
///
/// A failed write does not fail the run: the result is real and already on
/// screen, and refusing to show it because a file could not be written would
/// throw away the expensive half over the cheap half. It is logged loudly
/// instead, since a store that silently stops recording is worse than one that
/// never started.
pub fn remember<V: Recorded>(
    service: &ResearchService,
    plan: Option<&str>,
    (mut view, record): (V, Record),
) -> Result<V, CommandError> {
    let stored = StoredRecord::new(record, chrono::Utc::now()).with_provenance(super::provenance_for(plan));
    view.identify(keep(service, &stored));
    Ok(view)
}

/// Saves `stored` and answers with its id, for the view that shows it.
///
/// The view carries the id it was stored under, so a tab showing a fresh run
/// can ask for a report of it (#159) without going back to History to find
/// out what it was called.
fn keep(service: &ResearchService, stored: &StoredRecord) -> String {
    match service.memory.save(stored) {
        Ok(path) => tracing::info!(id = %stored.id, path = %path.display(), "recorded a finding"),
        Err(err) => tracing::error!(error = %err, id = %stored.id, "could not record a finding"),
    }
    stored.id.clone()
}

/// Who asked for a run, when it was not a person at the window: an agent or a
/// script, and where in its code the call was made.
#[derive(Debug, Clone, Copy)]
pub struct Asker<'a> {
    pub author: &'a str,
    pub origin: Option<&'a str>,
}

/// A view that came from a stored finding and can say which one.
pub trait Recorded {
    fn identify(&mut self, id: String);
}

macro_rules! recorded {
    ($($view:ty),+) => {
        $(impl Recorded for $view {
            fn identify(&mut self, id: String) {
                self.id = id;
            }
        })+
    };
}

recorded!(StudyView, WalkForwardView, PanelView);

/// The panel's window and combined dataset identity, over whatever
/// instruments currently have data.
///
/// The window is the *overlap* of what the members cover, not the union:
/// instruments judged over different periods are not a cross-section, and a
/// mean across them would compare different markets.
///
/// The identity is every member's hash combined, so editing any one file — or
/// adding or removing an instrument — marks the whole panel result stale.
/// The window a study of `plan` on `instrument` runs over, and the identity of
/// the data it runs on.
///
/// For a rule on the instrument's bars, its coverage and its fingerprint. For
/// a rule on its option chain (#86), where the bars *and* the chain exist —
/// history before the first expiration's opening window has nothing to trade,
/// and spent in-sample it would make every configuration look alike — and a
/// version covering both, tagged `chain:` so a staleness check knows to hash
/// the chain as well.
///
/// # Errors
///
/// A message naming what is missing, or what could not be read.
pub fn study_data(
    bars: &CsvBars,
    instrument: &str,
    plan: &StrategyPlan,
) -> Result<(DateRange, String), String> {
    let interval = plan.interval();
    let missing = || {
        format!(
            "{instrument} holds no {interval} bars; {} is defined at that resolution",
            plan.label
        )
    };
    let (mut from, mut to) = bars
        .coverage(instrument, interval)
        .map_err(|err| format!("reading {instrument}: {err}"))?
        .ok_or_else(missing)?;
    let fingerprint = bars
        .fingerprint(instrument, interval)
        .map_err(|err| format!("hashing {instrument}: {err}"))?
        .ok_or_else(missing)?;
    if !plan.trades_options() {
        return Ok((DateRange::new(from, to).map_err(|err| err.to_string())?, fingerprint));
    }

    let symbol = instrument.split('.').next().unwrap_or_default();
    let expirations: Vec<chrono::NaiveDate> = bars
        .option_contracts(symbol, interval)
        .map_err(|err| format!("listing {symbol} contracts: {err}"))?
        .iter()
        .filter_map(|name| arvo_data::option::OptionContract::parse(name))
        .map(|contract| contract.expiration)
        .collect();
    let (Some(first), Some(last)) = (expirations.iter().min(), expirations.iter().max()) else {
        return Err(format!(
            "{} trades {symbol}'s option chain and the library holds none; fetch it first",
            plan.label
        ));
    };
    // Far enough before the first expiration for the longest target to open —
    // a month-out rule's opening window; a same-day rule's is its own session.
    let lead = if plan.intraday { 0 } else { 60 };
    from = from.max(*first - chrono::Duration::days(lead));
    to = to.min(*last);
    let chain = bars
        .option_chain_fingerprint(symbol, interval)
        .map_err(|err| format!("hashing {symbol}'s chain: {err}"))?
        .ok_or_else(|| format!("{symbol}'s chain holds no bars"))?;
    Ok((
        DateRange::new(from, to).map_err(|err| format!("{instrument} and its chain do not overlap: {err}"))?,
        chain_dataset_version(&fingerprint, &chain),
    ))
}

/// The version of a study on bars and a chain together.
pub fn chain_dataset_version(fingerprint: &str, chain: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(fingerprint.as_bytes());
    hasher.update(chain.as_bytes());
    format!("{CHAIN_VERSION}{}", hasher.finalize().to_hex())
}

/// Marks a dataset version that covers an option chain as well as bars.
pub const CHAIN_VERSION: &str = "chain:";

/// A sector cap over these members, refusing if any of them has no label.
///
/// Refused here rather than left to the gate, which would refuse every entry
/// in an unlabelled name and produce a book that looks like a rule finding no
/// signals in it. Only the members' labels are kept, so the record holds what
/// the run used and nothing it did not.
///
/// ponytail: labels are today's classification, applied to the whole window —
/// a company that changed sector is judged as what it is now. Point-in-time
/// labels need a vendor that has them, as with index membership (#9).
pub fn sector_cap(
    max_positions: usize,
    instruments: &[String],
    mut labels: std::collections::BTreeMap<String, String>,
) -> Result<arvo_research::SectorCap, String> {
    let tickers: std::collections::BTreeSet<&str> = instruments
        .iter()
        .map(|id| arvo_data::source::symbol_of(id))
        .collect();
    let missing: Vec<&str> = tickers
        .iter()
        .copied()
        .filter(|ticker| !labels.contains_key(*ticker))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "Robinhood has no sector for {}, so a sector cap would refuse every entry there \
             (funds and delisted names have none). Remove those members from the book, or \
             run it without the cap",
            missing.join(", "),
        ));
    }
    labels.retain(|ticker, _| tickers.contains(ticker.as_str()));
    Ok(arvo_research::SectorCap {
        max_positions,
        sectors: labels,
    })
}

/// A book's dataset identity: every member's name and hash, in order.
///
/// One function because two places must agree on it exactly — the run that
/// records it and the staleness check that recomputes it. When they were two
/// copies, only one of them existed, and every book read as stale.
pub fn book_dataset_version(instruments: &[String], fingerprints: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (instrument, fingerprint) in instruments.iter().zip(fingerprints) {
        hasher.update(instrument.as_bytes());
        hasher.update(fingerprint.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub fn panel_dataset_version(
    bars: &CsvBars,
) -> Option<(String, Vec<String>, chrono::NaiveDate, chrono::NaiveDate)> {
    panel_dataset_version_over(bars, &bars.instruments().ok()?, arvo_data::BarInterval::DAILY)
}

/// The same over a named list at an interval: a universe (#227). Members
/// with no usable series are left out; the caller says which.
pub fn panel_dataset_version_over(
    bars: &CsvBars,
    members: &[String],
    interval: arvo_data::BarInterval,
) -> Option<(String, Vec<String>, chrono::NaiveDate, chrono::NaiveDate)> {
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut hasher = blake3::Hasher::new();
    let mut instruments = Vec::new();

    for id in members.iter().cloned() {
        let Ok(Some((first, last))) = bars.coverage(&id, interval) else {
            continue;
        };
        if let Ok(Some(fingerprint)) = bars.fingerprint(&id, interval) {
            hasher.update(fingerprint.as_bytes());
        }
        from = from.max(first);
        to = to.min(last);
        instruments.push(id);
    }

    if instruments.is_empty() {
        return None;
    }
    Some((
        hasher.finalize().to_hex().to_string(),
        instruments,
        from,
        to,
    ))
}

/// What can be run, so the UI offers the engine's actual list rather than a
/// copy of it that drifts.
///
/// # Errors
///
/// Never. Fallible only to match the shape every other command has.
#[allow(clippy::unnecessary_wraps, reason = "uniform command signature")]
pub fn list_strategies(root: &std::path::Path) -> Result<Vec<Strategy>, CommandError> {
    // Arvo's own, the extensions' (#162) and the project's rulesets, from
    // one place, so the picker cannot drift from what can be run. Re-read
    // on every ask: a ruleset edited a moment ago is what the person means.
    // From `root`, the engine's own project: an engine started over another
    // directory must not offer the rulesets of whichever folder the app
    // data remembers, which it did.
    crate::rulesets::refresh_at(root);
    Ok(super::offered()
        .into_iter()
        .map(|plan| Strategy {
            name: plan.name().to_owned(),
            label: plan.label.to_owned(),
            premise: plan.premise.to_owned(),
            interval: plan.interval().to_string(),
            backtests: arvo_api::count(plan.backtests()),
            // The shipped rule behind this plan, when there is one. A ruleset
            // is a grid over a shipped rule and inherits the answer; a plan
            // this build does not know ranks nothing.
            ranks_a_set: super::StrategyPlan::find(plan.name()).is_some_and(|found| found.ranks_a_set()),
        })
        .collect())
}

/// The panel the workbench runs: the same grid, one configuration chosen
/// across every instrument that has data.
///
/// The window is the *intersection* of what the instruments cover. Running
/// each over its own span would mean the panel's instruments were judged on
/// different market conditions, and a mean across those is not a
/// cross-sectional result.
#[must_use]
pub fn panel_for(
    instruments: Vec<String>,
    window: DateRange,
    dataset_version: &str,
) -> arvo_research::PanelStudy {
    let plan = StrategyPlan::find(STRATEGY).expect("the default strategy is in PLANS");
    panel_for_plan(instruments, window, dataset_version, plan)
}

/// A panel of `plan` over `instruments` (#227).
pub fn panel_for_plan(
    instruments: Vec<String>,
    window: DateRange,
    dataset_version: &str,
    plan: &StrategyPlan,
) -> arvo_research::PanelStudy {
    // The panel treats its instruments as one dataset, so the basis is read
    // across all of them rather than from the subject — which here is the word
    // "panel" and has no venue.
    let adjustment = crate::source::adjustment_across(instruments.iter().map(String::as_str));
    let mut template = template_for("panel", plan, window, dataset_version, adjustment);
    // And the costs are read across them too, for the same reason: the subject
    // is the word "panel", which `Instrument::of` describes as a share, so a
    // panel of coins would otherwise be charged a share's basis point.
    if !plan.trades_options() {
        template.costs = spot_costs_across(instruments.iter().map(String::as_str));
    }
    arvo_research::PanelStudy::new(template, instruments, plan.grid())
}

/// What one side of a trade in `subject` costs, at market.
///
/// A coin's fees are a different order of magnitude from a share's
/// (`CRYPTO_COMMISSION_BPS`), and which applies is a property of the
/// instrument rather than of the rule — so it is read from the instrument, the
/// same place its tick, lot and hours come from.
fn spot_costs(subject: &str) -> CostModel {
    if matches!(
        arvo_data::Instrument::of(subject).kind,
        arvo_data::instrument::Kind::Crypto { .. }
    ) {
        CostModel::proportional(CRYPTO_COMMISSION_BPS, CRYPTO_SLIPPAGE_BPS)
    } else {
        CostModel::proportional(COMMISSION_BPS, SLIPPAGE_BPS)
    }
}

/// The dearer of the two, if any of `instruments` is a coin.
///
/// A panel carries one cost model and every member's run derives from it, so a
/// panel mixing asset classes has no single honest answer. It takes the dearer
/// one: charging a share a coin's fees understates that member, and understating
/// a result is the failure this platform can live with. The reverse — charging a
/// coin a share's basis point — is the one it cannot.
fn spot_costs_across<'a>(instruments: impl IntoIterator<Item = &'a str>) -> CostModel {
    let any_coin = instruments.into_iter().any(|instrument| {
        matches!(
            arvo_data::Instrument::of(instrument).kind,
            arvo_data::instrument::Kind::Crypto { .. }
        )
    });
    if any_coin {
        CostModel::proportional(CRYPTO_COMMISSION_BPS, CRYPTO_SLIPPAGE_BPS)
    } else {
        CostModel::proportional(COMMISSION_BPS, SLIPPAGE_BPS)
    }
}

pub fn template_for(
    subject: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
    adjustment: arvo_data::source::Adjustment,
) -> Experiment {
    // An option rule pays an option's spread, not equity basis points, and
    // sizes by collateral: the stop, risk fraction and halt below mean nothing
    // to it, and the engine refuses them rather than record limits it ignores.
    let (costs, risk, claim) = if plan.trades_options() {
        (
            CostModel {
                option_spread: Some(arvo_research::OptionSpread::MEASURED),
                ..CostModel::proportional(COMMISSION_BPS, 0.0)
            },
            arvo_research::RiskModel::default(),
            format!("{} beats holding {subject}", plan.label),
        )
    } else {
        (
            spot_costs(subject),
            // The project's file, or the shipped model (`crate::risk`).
            crate::risk::current(),
            format!("trend-following predicts returns in {subject}"),
        )
    };
    Experiment {
        id: ExperimentId(format!("study-{subject}")),
        hypothesis: HypothesisId(claim),
        instrument: subject.to_owned(),
        alongside: Vec::new(),
        underlying: None,
        window,
        // From the strategy, not fixed. An opening range on daily bars is not
        // a slower opening range, it is a different rule — and the engine
        // refuses the combination rather than producing a curve for it.
        interval: plan.interval(),
        dataset: DatasetRef {
            id: subject.to_owned(),
            // A content hash of the bars, so a stored result knows exactly
            // which data produced it. This used to be the fixed string
            // "local-csv", which meant editing a CSV left every earlier result
            // still claiming to be reproducible against it.
            version: dataset_version.to_owned(),
            // Which decides whether the dividend gap is a correction to
            // subtract or a description of what the margin is made of. Taken
            // from the source that wrote the bars, because it is not visible
            // in them.
            adjustment,
        },
        strategy: StrategySpec {
            rule: plan.definition().cloned(),
            // The engine's rule, not the picker's name: a contributed
            // document (#162) searches one of Arvo's rules under its own
            // name, and a finding has to record what ran so it can be
            // replayed without the extension that suggested it.
            name: plan.rule().to_owned(),
            params: plan
                .fixed
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
        },
        costs,
        risk,
        starting_cash: STARTING_CASH,
        seed: 1,
    }
}

/// The study the workbench runs: a moving-average grid over the instrument's
/// whole history.
///
/// Public so the `study` example can run exactly what the view runs. A second
/// definition of "the study" that drifted from this one would make headless
/// verification worthless.
///
/// The grid is fixed at nine configurations. That number is itself part of the
/// claim — [`arvo_research::run_family`] deflates the result by it — so it is
/// written here in the open rather than tuned per run.
/// How long each selection window looks back, and how often it re-selects.
///
/// Three years to choose on, two to be judged on. The step is two rather than
/// one for a measured reason: every fold's out-of-sample period is an
/// independent backtest that starts cold, so a rule with a 120-bar slow
/// average cannot trade in the first 120 bars of it. At a one-year step that
/// is half the fold, and this grid's slowest configuration then produced no
/// trades at all in nine folds out of sixteen. Two years halves the waste.
///
/// These are a claim, not a setting: a walk-forward run at a different cadence
/// is a different experiment, and the pair is part of what the record pins.
/// The fewest instruments a ranking rule is allowed to rank.
///
/// Four, holding two or three, so the choice is a choice. Ranking two and
/// holding the better one is a coin toss dressed as a selection, and the
/// evaluation machinery would score it as diligently as anything else.
pub const MIN_RANKED: usize = 4;

const IN_SAMPLE_DAYS: i64 = 365 * 3;
const STEP_DAYS: i64 = 365 * 2;

/// Builds the rolling procedure for one instrument.
///
/// Same template and grid as [`study_for`], deliberately: the point of a
/// walk-forward is to be comparable with the single split it replaces, and a
/// different grid would make the two incomparable while looking like a
/// stronger result.
#[must_use]
pub fn walk_forward_for(
    instrument: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> arvo_research::WalkForward {
    arvo_research::WalkForward {
        hypothesis: HypothesisId(format!("trend-following predicts returns in {instrument}")),
        template: template_for(
            instrument,
            plan,
            window,
            dataset_version,
            crate::source::adjustment_across([instrument]),
        ),
        grid: plan.grid(),
        in_sample_days: IN_SAMPLE_DAYS,
        step_days: STEP_DAYS,
        // Anchored: every selection sees all history. The alternative assumes
        // old data stops applying, which is a claim about the market nobody
        // here has evidence for.
        anchored: true,
    }
}

pub fn study_for(
    instrument: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> ExperimentFamily {
    ExperimentFamily::new(
        template_for(
            instrument,
            plan,
            window,
            dataset_version,
            crate::source::adjustment_across([instrument]),
        ),
        plan.grid(),
    )
}

/// Runs a parameter study on one instrument, records it and reports what
/// survives. Blocking: a family is `trials + 2` backtests of CPU-bound work,
/// and every caller puts it on a blocking thread.
///
/// # Errors
///
/// An unknown rule, a ranking rule (which needs a book), an instrument
/// without bars at the rule's resolution, or a failed run.
pub fn run_study(service: &ResearchService, instrument: &str, strategy: Option<&str>) -> Result<StudyView, CommandError> {
    service.load_risk()?;
    let name = strategy.unwrap_or(STRATEGY);
    let plan = StrategyPlan::find(name)
        .ok_or_else(|| CommandError::Failed(format!("no rule or ruleset called {name:?}")))?;
    if plan.ranks_a_set() {
        // It would run. It would produce a curve, a verdict and no
        // information: a ranking with a field of one holds that one whatever
        // it did, so the result describes the instrument and not the rule.
        return Err(CommandError::Failed(format!(
            "{} ranks instruments against each other and needs more than one; run it as a book",
            plan.label
        )));
    }
    let (window, fingerprint) = study_data(&service.bars, instrument, plan).map_err(CommandError::Failed)?;
    let family = study_for(instrument, plan, window, &fingerprint);
    let found = arvo_research::run_family(
        service.simulation.as_ref(),
        &family,
        &arvo_research::EvaluationCriteria::default(),
    )
    .map_err(|err| CommandError::Failed(err.to_string()))?;
    let view = study_view(&found, &service.bars, service.simulation.engine());
    remember(service, Some(plan.name()), (view, Record::Study(Box::new(found))))
}

/// Runs a rolling re-selection over an instrument's whole history and records
/// it. Blocking, and slower than a study by roughly the number of folds.
///
/// # Errors
///
/// An unknown rule, no bars at its resolution, or a span too short to roll.
pub fn run_walk_forward(
    service: &ResearchService,
    instrument: &str,
    strategy: Option<&str>,
) -> Result<WalkForwardView, CommandError> {
    service.load_risk()?;
    let name = strategy.unwrap_or(STRATEGY);
    let plan = StrategyPlan::find(name)
        .ok_or_else(|| CommandError::Failed(format!("no rule or ruleset called {name:?}")))?;
    let (window, fingerprint) = study_data(&service.bars, instrument, plan).map_err(CommandError::Failed)?;
    let procedure = walk_forward_for(instrument, plan, window, &fingerprint);
    let found = arvo_research::run_walk_forward(
        service.simulation.as_ref(),
        &procedure,
        &arvo_research::EvaluationCriteria::default(),
    )
    .map_err(|err| CommandError::Failed(err.to_string()))?;
    let view = walk_forward_view(&found, &service.bars, service.simulation.engine());
    remember(service, Some(plan.name()), (view, Record::WalkForward(Box::new(found))))
}

/// Runs one configuration across every instrument that has data and records
/// it. Blocking.
///
/// # Errors
///
/// No instruments with usable data, or a failed run.
pub fn run_panel(service: &ResearchService) -> Result<PanelView, CommandError> {
    service.load_risk()?;
    let (dataset, instruments, from, to) = panel_dataset_version(&service.bars)
        .ok_or_else(|| CommandError::Failed("no instruments with usable data".to_owned()))?;
    let window = DateRange::new(from, to).map_err(|err| CommandError::Failed(err.to_string()))?;
    let study = panel_for(instruments, window, &dataset);
    let found = arvo_research::run_panel(
        service.simulation.as_ref(),
        &study,
        &arvo_research::EvaluationCriteria::default(),
    )
    .map_err(|err| CommandError::Failed(err.to_string()))?;
    let view = panel_view(&found, service.simulation.engine());
    remember(service, Some(STRATEGY), (view, Record::Panel(Box::new(found))))
}

/// The sector cap a book asked for, labelled from Robinhood before the run:
/// a cap that cannot be labelled is a run that cannot happen, and finding
/// out before any bars are read wastes nothing.
///
/// # Errors
///
/// Robinhood could not classify every member.
/// A panel over a universe (#227): the members that have a series at its
/// interval, under `strategy` or the default, with the universe and its
/// reason on the finding.
///
/// # Errors
///
/// No such strategy, a strategy defined at another interval, or no member
/// with a series yet.
pub fn run_panel_over(
    service: &ResearchService,
    universe: &crate::universes::Universe,
    strategy: Option<&str>,
) -> Result<PanelView, CommandError> {
    run_panel_over_as(service, universe, strategy, None)
}

/// [`run_panel_over`], saying who asked.
///
/// With an asker the panel is that author's finding, held to everything the
/// author has run, exactly as a study is (`StoredRecord::by_agent`). A panel
/// is the widest search Arvo runs, and it was the one an agent could repeat
/// for nothing: it was saved as a person's, so no count rose and no bar moved
/// (arvo-engine#15). The view is built after the author's bar is applied, so
/// it shows the verdict that was stored and not the one the panel had on its
/// own.
///
/// # Errors
///
/// As [`run_panel_over`], and when the author's earlier findings cannot be
/// read: a bar drawn from half a history would be a lenient one.
pub fn run_panel_over_as(
    service: &ResearchService,
    universe: &crate::universes::Universe,
    strategy: Option<&str>,
    asker: Option<Asker<'_>>,
) -> Result<PanelView, CommandError> {
    service.load_risk()?;
    let plan = match strategy {
        Some(name) => StrategyPlan::find(name)
            .ok_or_else(|| CommandError::Failed(format!("no strategy {name:?}; list_strategies says what there is")))?,
        None => StrategyPlan::find(STRATEGY).expect("the default strategy is in PLANS"),
    };
    if plan.interval() != universe.interval {
        return Err(CommandError::Failed(format!(
            "{} is defined at {}, and universe {} is {}",
            plan.name(),
            plan.interval(),
            universe.name,
            universe.interval
        )));
    }
    let (dataset, instruments, from, to) =
        panel_dataset_version_over(&service.bars, &universe.instruments, universe.interval).ok_or_else(|| {
            CommandError::Failed(format!(
                "no member of {} has a series at {} yet; the universes job fetches them, or run `arvo-engine universes refresh`",
                universe.name, universe.interval
            ))
        })?;
    let missing: Vec<String> = universe.instruments.iter().filter(|id| !instruments.contains(id)).cloned().collect();
    let window = DateRange::new(from, to).map_err(|err| CommandError::Failed(err.to_string()))?;
    let study = panel_for_plan(instruments, window, &dataset, plan);
    let mut found = arvo_research::run_panel(
        service.simulation.as_ref(),
        &study,
        &arvo_research::EvaluationCriteria::default(),
    )
    .map_err(|err| CommandError::Failed(err.to_string()))?;
    found.universe = Some(arvo_research::UniverseRef {
        name: universe.name.clone(),
        reason: universe.reason.clone(),
        size: universe.instruments.len(),
        missing,
    });
    let record = Record::Panel(Box::new(found));
    let now = chrono::Utc::now();
    let stored = match asker {
        Some(Asker { author, origin }) => {
            let history = service.memory.load().map_err(|err| CommandError::Failed(err.to_string()))?.records;
            StoredRecord::by_agent(record, author, &history, now).with_origin(origin.map(ToOwned::to_owned))
        }
        None => StoredRecord::new(record, now),
    }
    .with_provenance(super::provenance_for(Some(plan.name())));
    let Record::Panel(found) = &stored.record else { unreachable!("a panel was just put there") };
    let mut view = panel_view(found, service.simulation.engine());
    view.identify(keep(service, &stored));
    Ok(view)
}

pub async fn book_sector_cap(
    max_per_sector: Option<usize>,
    instruments: &[String],
) -> Result<Option<arvo_research::SectorCap>, CommandError> {
    let Some(max_positions) = max_per_sector else { return Ok(None) };
    let tickers: Vec<&str> = instruments.iter().map(|id| arvo_data::source::symbol_of(id)).collect();
    let labels = crate::source::robinhood::Robinhood.sectors(&tickers).await.map_err(|err| {
        CommandError::Failed(format!(
            "a sector cap needs each member's sector from Robinhood, and asking failed: {err}. Sign in to \
             Robinhood, or run the book without the cap"
        ))
    })?;
    sector_cap(max_positions, instruments, labels).map(Some).map_err(CommandError::Failed)
}

/// Studies one rule across several instruments sharing one account, and
/// records it. Blocking. `sector_cap` comes from [`book_sector_cap`], which
/// is the part that talks to a venue.
///
/// # Errors
///
/// Fewer than two instruments, a ranking rule on too few, a member without
/// bars at the rule's resolution, no period in common, or a failed run.
pub fn run_book(
    service: &ResearchService,
    instruments: Vec<String>,
    strategy: Option<&str>,
    max_concurrent_positions: Option<usize>,
    sector_cap: Option<arvo_research::SectorCap>,
) -> Result<StudyView, CommandError> {
    service.load_risk()?;
    if instruments.len() < 2 {
        return Err(CommandError::Failed("a book needs at least two instruments; one is a study".to_owned()));
    }
    let name = strategy.unwrap_or(STRATEGY);
    let plan = StrategyPlan::find(name)
        .ok_or_else(|| CommandError::Failed(format!("no rule or ruleset called {name:?}")))?;
    // A ranking rule holding the top few of two is holding one of them, which
    // is a coin toss the rest of the machinery would dutifully evaluate.
    if plan.ranks_a_set() && instruments.len() < MIN_RANKED {
        return Err(CommandError::Failed(format!(
            "{} ranks instruments against each other; {MIN_RANKED} is the fewest a ranking says anything \
             about, and this has {}",
            plan.label,
            instruments.len()
        )));
    }
    let interval = plan.interval();
    // The window every member can be held over, and one hash covering all of
    // them: a dataset version naming only the head would call a book stale
    // when the head changed and fresh when any other member did.
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut fingerprints = Vec::with_capacity(instruments.len());
    for instrument in &instruments {
        let missing = || {
            CommandError::Failed(format!(
                "{instrument} holds no {interval} bars; {} is defined at that resolution",
                plan.label
            ))
        };
        let coverage = service
            .bars
            .coverage(instrument, interval)
            .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
            .ok_or_else(missing)?;
        let fingerprint = service
            .bars
            .fingerprint(instrument, interval)
            .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
            .ok_or_else(missing)?;
        fingerprints.push(fingerprint);
        from = from.max(coverage.0);
        to = to.min(coverage.1);
    }
    let window = DateRange::new(from, to).map_err(|_| {
        CommandError::Failed(format!(
            "these {} instruments have no period in common: the latest start is {from} and the earliest end is {to}",
            instruments.len(),
        ))
    })?;
    let dataset_version = book_dataset_version(&instruments, &fingerprints);

    let head = instruments[0].clone();
    let mut family = study_for(&head, plan, window, &dataset_version);
    // The head stays the experiment's identity; the rest are what it is held
    // alongside. `ExperimentFamily` varies parameters, not instruments, so
    // setting this on the template sets it for every trial.
    family.template.alongside = instruments[1..].to_vec();
    family.template.risk.max_concurrent_positions = max_concurrent_positions;
    family.template.risk.sector_cap = sector_cap;
    family.template.risk.check().map_err(CommandError::Failed)?;
    family.template.id = ExperimentId(format!("book-{}", instruments.join("+")));
    family.template.hypothesis = HypothesisId(format!(
        "{} predicts returns across {} instruments sharing one account",
        plan.label,
        instruments.len(),
    ));
    let found = arvo_research::run_family(
        service.simulation.as_ref(),
        &family,
        &arvo_research::EvaluationCriteria::default(),
    )
    .map_err(|err| CommandError::Failed(err.to_string()))?;
    let view = study_view(&found, &service.bars, service.simulation.engine());
    remember(service, Some(plan.name()), (view, Record::Study(Box::new(found))))
}

/// Runs a shared experiment file against this machine's data for
/// `instrument`, and records what it found. Blocking.
///
/// The file brings the question and how much searching produced it; the data,
/// and therefore the answer, are this machine's. Its search is counted into
/// the bar (`ExperimentFamily::prior_trials`), so a survivor of someone else's
/// fifty-configuration grid is held to the best of fifty-plus, not of the
/// handful run here.
///
/// Nothing is stored until it has run: an imported file is a question, and the
/// finding that lands in research memory is the one produced here.
///
/// # Errors
///
/// The import's own reason — a newer format, a strategy this build lacks — a
/// ranking rule, an option-chain experiment, or an instrument with no bars at
/// the file's resolution.
pub fn run_shared(service: &ResearchService, text: &str, instrument: &str) -> Result<StudyView, CommandError> {
    let shared = arvo_research::share::import(text, arvo_nautilus::STRATEGIES)
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    if arvo_nautilus::CROSS_SECTIONAL_STRATEGIES.contains(&shared.strategy.name.as_str()) {
        return Err(CommandError::Failed(format!(
            "{} ranks instruments against each other and cannot be run on one",
            shared.strategy.name
        )));
    }
    // A shared file carries a strategy and a resolution, not a chain: a
    // finding on one would be versioned by the bars alone and never go stale
    // when the chain it traded changed.
    if shared.strategy.name == arvo_nautilus::PUT_SPREAD {
        return Err(CommandError::Failed(
            "an option-chain experiment cannot be run from a shared file yet; run it as a study".to_owned(),
        ));
    }
    let interval = shared.interval;
    let missing = || {
        CommandError::Failed(format!(
            "{instrument} holds no {interval} bars, which is the resolution this experiment is defined at"
        ))
    };
    let coverage = service
        .bars
        .coverage(instrument, interval)
        .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
        .ok_or_else(missing)?;
    let fingerprint = service
        .bars
        .fingerprint(instrument, interval)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(missing)?;
    let window = DateRange::new(coverage.0, coverage.1).map_err(|err| CommandError::Failed(err.to_string()))?;
    let family = shared.family(
        instrument,
        window,
        DatasetRef {
            id: instrument.to_owned(),
            version: fingerprint,
            adjustment: crate::source::adjustment_across([instrument]),
        },
    );
    let found = arvo_research::run_family(service.simulation.as_ref(), &family, &shared.criteria)
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    let view = study_view(&found, &service.bars, service.simulation.engine());
    remember(service, None, (view, Record::Study(Box::new(found))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn window() -> DateRange {
        DateRange::new(
            NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid"),
            NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        )
        .expect("ordered")
    }

    #[test]
    fn a_sector_cap_names_the_members_it_cannot_label_and_keeps_only_the_members() {
        let labels = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(ticker, sector)| ((*ticker).to_owned(), (*sector).to_owned()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let members = ["KO.RH".to_owned(), "NEE.YF".to_owned(), "SIVB.AIEX".to_owned()];

        let refused = sector_cap(1, &members, labels(&[("KO", "Staples"), ("NEE", "Utilities")]))
            .expect_err("SIVB has no sector");
        assert!(refused.contains("no sector for SIVB"), "{refused}");

        let cap = sector_cap(
            1,
            &members[..2],
            labels(&[("KO", "Staples"), ("NEE", "Utilities"), ("MSFT", "Technology")]),
        )
        .expect("both labelled");
        assert_eq!(cap.sectors.len(), 2, "the record holds what the run used");
    }

    #[test]
    fn every_offered_strategy_can_be_planned_by_the_engine() {
        // `PLANS` is what the sidebar menu shows. A plan the engine rejects —
        // a missing parameter, a period pair the rule refuses — is a failure
        // the user only discovers after choosing it and waiting.
        for plan in PLANS {
            let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
            for combination in family.grid.combinations() {
                let mut spec = family.template.strategy.clone();
                spec.params.extend(combination);
                assert!(
                    arvo_nautilus::check_plan(&spec, family.template.interval).is_ok(),
                    "{} cannot be planned: {:?}",
                    plan.name,
                    spec.params
                );
            }
        }
    }

    /// #161: every family Arvo ships says the same thing as a document.
    ///
    /// The second implementation, and the test that keeps it honest. A
    /// document form that could not express the catalog already in the
    /// product would be an adapter for one outside user's catalog, which is
    /// what ADR-0005 exists to stop.
    #[test]
    fn every_offered_strategy_is_the_same_search_written_as_a_document() {
        for plan in PLANS {
            let document = plan.document();
            assert_eq!(document.name, plan.name);
            assert_eq!(document.interval, plan.interval());

            let arvo_research::StrategyKind::Grid(grid) = &document.kind else {
                panic!("{} is a grid", plan.name)
            };
            assert_eq!(
                grid.grid().combinations(),
                plan.grid().combinations(),
                "{} searches something else as a document",
                plan.name
            );
            assert_eq!(grid.configurations() + 2, plan.backtests(), "{}", plan.name);
            assert!(document.reads().is_empty(), "a grid reads no signals");

            // It is a document, so it survives being written down.
            let json = serde_json::to_string(&document).expect("serialises");
            let read: arvo_research::StrategyDocument = serde_json::from_str(&json).expect("reads back");
            assert_eq!(read.version(), document.version(), "{} is addressed by what it says", plan.name);
        }
    }

    /// #162: what an extension may contribute to the picker, and what it
    /// is told when it may not.
    #[test]
    fn a_contributed_document_is_offered_only_when_it_describes_a_run() {
        use arvo_research::{Grid, Rules, StrategyDocument, StrategyKind};
        use std::collections::BTreeMap;

        let document = |kind| StrategyDocument {
            name: "fast-cross".to_owned(),
            label: "Faster crossover".to_owned(),
            premise: "Their grid, Arvo's rule.".to_owned(),
            interval: arvo_data::BarInterval::DAILY,
            kind,
        };
        let grid = |rule: &str, axes: BTreeMap<String, Vec<f64>>| {
            StrategyKind::Grid(Grid { rule: rule.to_owned(), fixed: BTreeMap::new(), axes })
        };
        let axes = BTreeMap::from([
            ("fast".to_owned(), vec![3.0, 5.0]),
            ("slow".to_owned(), vec![20.0, 40.0]),
        ]);

        // A rule Arvo implements, searched over someone else's grid.
        let offered = document(grid("sma_cross", axes.clone()));
        assert_eq!(super::offerable(&offered).map(StrategyPlan::name), Ok("sma_cross"));

        // A rule it does not, named as helpfully as possible.
        let unknown = super::offerable(&document(grid("their_own_rule", axes.clone())))
            .expect_err("Arvo cannot run a rule it does not have");
        assert!(unknown.contains("no rule called"), "{unknown}");
        assert!(unknown.contains("sma_cross"), "and says what it does have: {unknown}");

        // A search with nothing in it.
        let empty = BTreeMap::from([("fast".to_owned(), vec![])]);
        assert!(super::offerable(&document(grid("sma_cross", empty))).is_err());

        // Parameters the engine will not take: found now rather than after a
        // person has chosen it and waited.
        let wrong = BTreeMap::from([("nonsense".to_owned(), vec![1.0])]);
        assert!(super::offerable(&document(grid("sma_cross", wrong))).is_err());

        // A resolution the rule is not defined at. An opening range on daily
        // bars is a different rule, not a slower one.
        let mut misresolved = document(grid("opening_range", axes.clone()));
        misresolved.interval = arvo_data::BarInterval::DAILY;
        let refused = super::offerable(&misresolved).expect_err("the engine refuses the combination");
        assert!(refused.contains("is defined at"), "{refused}");

        // A rules document: readable (#161), with no runner here.
        let rules = super::offerable(&document(StrategyKind::Rules(Rules { entry: arvo_research::RuleSet::default(), exit: arvo_research::RuleSet::default() })))
            .expect_err("no runner for one yet");
        assert!(rules.contains("#125"), "and it says where that question lives: {rules}");
    }

    /// #162: a contributed strategy runs its own search, under Arvo's rule.
    #[test]
    fn a_contributed_strategy_joins_the_picker_and_runs_the_rule_it_names() {
        use arvo_research::{Grid, StrategyDocument, StrategyKind};
        use std::collections::BTreeMap;

        let document = StrategyDocument {
            name: "fast-cross".to_owned(),
            label: "Faster crossover".to_owned(),
            premise: "Their grid, Arvo's rule.".to_owned(),
            interval: arvo_data::BarInterval::DAILY,
            kind: StrategyKind::Grid(Grid {
                rule: "sma_cross".to_owned(),
                fixed: BTreeMap::from([("trade_size".to_owned(), 10.0)]),
                axes: BTreeMap::from([
                    ("fast".to_owned(), vec![3.0, 5.0]),
                    ("slow".to_owned(), vec![20.0, 40.0]),
                ]),
            }),
        };
        super::set_contributed(&[("their-catalog.fast-cross".to_owned(), document)]);

        let plan = StrategyPlan::find("their-catalog.fast-cross").expect("it is in the catalog");
        assert_eq!(plan.name(), "their-catalog.fast-cross", "the picker shows their name");
        assert_eq!(plan.rule(), "sma_cross", "and the engine is handed its own rule");
        assert_eq!(plan.grid().combinations().len(), 4, "their grid, not Arvo's");

        // The finding records what ran, so it replays without the extension
        // that suggested it.
        let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
        assert_eq!(family.template.strategy.name, "sma_cross");
        assert_eq!(family.template.strategy.params.get("trade_size"), Some(&10.0));

        // `offered` rather than `list_strategies`, which re-reads this
        // machine's extensions and project and would replace the fixture.
        assert!(
            super::offered().iter().any(|plan| plan.name() == "their-catalog.fast-cross"),
            "and the picker offers it"
        );

        // Removing the extension takes it back out.
        super::set_contributed(&[]);
        assert!(StrategyPlan::find("their-catalog.fast-cross").is_none());
        assert!(!super::offered().iter().any(|plan| plan.name().contains("fast-cross")));
    }

    #[test]
    fn a_session_anchored_strategy_asks_for_intraday_bars() {
        // The reason the interval lives on the plan: an opening range on daily
        // bars is a different rule, and the engine refuses it. Getting this
        // wrong means the study fails at the last moment instead of asking
        // for the right data.
        for plan in PLANS {
            let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
            assert_eq!(
                family.template.interval.is_intraday(),
                plan.intraday,
                "{} asked for the wrong resolution",
                plan.name
            );
        }
    }

    #[test]
    fn an_option_study_pays_an_option_spread_and_sets_no_limits_the_rule_ignores() {
        let plan = StrategyPlan::find("put_spread").expect("offered");
        let family = study_for("SPY.AIEX", plan, window(), "chain:x");
        let template = &family.template;
        assert!(template.costs.option_spread.is_some());
        assert_eq!(template.costs.slippage_bps, 0.0, "a spread is stated once");
        assert!(template.risk.stop_atr_multiple.is_none() && template.risk.risk_per_trade.is_none());
        assert!(!template.interval.is_intraday());
        assert_eq!(family.grid.size(), 6);
    }

    #[test]
    fn an_option_study_runs_where_the_bars_and_the_chain_both_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bars = CsvBars::new(dir.path());
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).expect("valid");
        let bar = |at: NaiveDate| arvo_data::Bar {
            at: at.and_time(chrono::NaiveTime::MIN),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        };
        let plan = StrategyPlan::find("put_spread").expect("offered");
        bars.write("SPY.AIEX", arvo_data::BarInterval::DAILY, &[bar(day(2023, 1, 3)), bar(day(2025, 6, 2))])
            .expect("write");
        assert!(
            study_data(&bars, "SPY.AIEX", plan).expect_err("no chain").contains("fetch it first")
        );

        bars.write("SPY240315P00500000.AOPT", arvo_data::BarInterval::DAILY, &[bar(day(2024, 3, 1))])
            .expect("write");
        bars.write("SPY241220P00500000.AOPT", arvo_data::BarInterval::DAILY, &[bar(day(2024, 12, 2))])
            .expect("write");
        let (range, version) = study_data(&bars, "SPY.AIEX", plan).expect("both exist");
        assert_eq!(range.from, day(2024, 3, 15) - chrono::Duration::days(60));
        assert_eq!(range.to, day(2024, 12, 20));
        assert!(version.starts_with(CHAIN_VERSION));

        // A stock rule on the same instrument keeps its own window and version.
        let control = StrategyPlan::find(STRATEGY).expect("offered");
        let (range, version) = study_data(&bars, "SPY.AIEX", control).expect("bars exist");
        assert_eq!(range.from, day(2023, 1, 3));
        assert!(!version.starts_with(CHAIN_VERSION));

        // Revising a contract changes the study's version.
        bars.write("SPY241220P00500000.AOPT", arvo_data::BarInterval::DAILY, &[arvo_data::Bar { close: 1.0, high: 2.0, ..bar(day(2024, 12, 2)) }])
            .expect("write");
        let (_, revised) = study_data(&bars, "SPY.AIEX", plan).expect("both exist");
        assert_ne!(revised, version);
    }

    #[test]
    fn the_study_grid_is_nine_configurations_and_says_so() {
        let plan = StrategyPlan::find(STRATEGY).expect("the default is offered");
        let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
        assert_eq!(
            family.grid.size(),
            9,
            "the trial count is deflated against, so it must be what it claims"
        );
        assert_eq!(
            plan.backtests(),
            11,
            "the spinner promises this many, so it has to be what runs"
        );
        assert_eq!(family.template.strategy.name, STRATEGY);
        assert_eq!(
            family.template.dataset.version, "test-fingerprint",
            "the dataset identity must reach the record, or nothing can be found stale"
        );
        assert!(
            family.template.costs.slippage_bps > 0.0,
            "a study that assumes free fills is the optimistic one, and the engine honours \
             slippage now"
        );
    }

    /// A coin is charged a coin's fees, and a share is not charged a coin's.
    ///
    /// Alpaca's entry tier takes 0.25% from a taker and Arvo sends market
    /// orders. Charging the equity basis point understated a round trip by
    /// fifty times, in the direction that makes a rule look tradeable.
    #[test]
    fn a_coin_costs_what_a_coin_costs_and_a_share_is_unchanged() {
        let share = spot_costs("MSFT.RH");
        assert!((share.commission_bps - COMMISSION_BPS).abs() < f64::EPSILON);
        assert!((share.slippage_bps - SLIPPAGE_BPS).abs() < f64::EPSILON);

        let coin = spot_costs("BTC-USD.ACRYPTO");
        assert!((coin.commission_bps - CRYPTO_COMMISSION_BPS).abs() < f64::EPSILON);
        assert!((coin.slippage_bps - CRYPTO_SLIPPAGE_BPS).abs() < f64::EPSILON);
        assert!(
            coin.commission_bps > share.commission_bps * 20.0,
            "a coin is not a share with wider spreads; it is a different order of magnitude"
        );

        // And the conservative tier lifts it further rather than capping it.
        let strict = coin.at(arvo_research::risk::CostTier::Conservative);
        assert!(strict.commission_bps > coin.commission_bps);

        // A panel reads its members, because its own subject is the word
        // "panel" and `Instrument::of` calls that a share.
        assert!(
            (spot_costs_across(["MSFT.RH", "AAPL.YF"]).commission_bps - COMMISSION_BPS).abs()
                < f64::EPSILON
        );
        let coins = spot_costs_across(["BTC-USD.ACRYPTO", "ETH-USD.ACRYPTO"]);
        assert!((coins.commission_bps - CRYPTO_COMMISSION_BPS).abs() < f64::EPSILON);
        // Mixed: the dearer one, so nothing is charged less than it costs.
        let mixed = spot_costs_across(["MSFT.RH", "BTC-USD.ACRYPTO"]);
        assert!((mixed.commission_bps - CRYPTO_COMMISSION_BPS).abs() < f64::EPSILON);
    }
}
