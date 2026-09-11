//! The runs themselves: a study, a book, a panel, a walk-forward.
//!
//! Every one of these states its assumptions in the result rather than
//! implying them — the split, the number of configurations tried, the bar a
//! no-skill search would clear, the costs assumed.

use super::*;

/// Runs a parameter study on one instrument and reports what survives.
///
/// Offloaded to a blocking thread: a family is `trials + 2` backtests of
/// CPU-bound work, and running it on the async runtime would freeze the
/// window for the duration.
#[tauri::command]
pub async fn run_study(
    instrument: String,
    strategy: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<StudyView, CommandError> {
    let name = strategy.unwrap_or_else(|| STRATEGY.to_owned());
    let plan = StrategyPlan::find(&name)
        .ok_or_else(|| CommandError::Failed(format!("no strategy called {name:?}")))?;
    if plan.ranks_a_set() {
        // It would run. It would produce a curve, a verdict and no
        // information: a ranking with a field of one holds that one whatever
        // it did, so the result describes the instrument and not the rule.
        return Err(CommandError::Failed(format!(
            "{} ranks instruments against each other and needs more than one; run it as a book",
            plan.label
        )));
    }
    let interval = plan.interval();

    let simulation = service.simulation.clone();
    // The resolution the *strategy* needs, not whatever the library happens to
    // hold. Saying which resolution is missing is the difference between a
    // usable message and "holds no bars" on an instrument the sidebar just
    // listed.
    let missing = || {
        CommandError::Failed(format!(
            "{instrument} holds no {interval} bars; {} is defined at that resolution",
            plan.label
        ))
    };
    let coverage = service
        .bars
        .coverage(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
        .ok_or_else(missing)?;

    let engine = simulation.engine().to_owned();
    let fingerprint = service
        .bars
        .fingerprint(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(missing)?;

    // A copy for the blocking closure: rendering the report reads the bars
    // back to draw them, and the `State` cannot cross that boundary.
    let library = service.bars.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let window = DateRange::new(coverage.0, coverage.1)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let family = study_for(&instrument, plan, window, &fingerprint);

        let found = arvo_research::run_family(
            simulation.as_ref(),
            &family,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = study_view(&found, &library, &engine);
        Ok((view, Record::Study(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the study did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Studies one rule across several instruments **sharing one account**.
///
/// The difference from a panel is the whole point. A panel runs each member on
/// its own with the full balance behind it and combines the answers; this runs
/// them together, so a position one member takes is capital another cannot.
/// Two rules that look identical when simulated separately — one that wants
/// three positions at once and one that wants them in turn — are told apart
/// here and nowhere else.
///
/// The window is the intersection of every member's coverage, not the union: a
/// book cannot hold an instrument over a period it has no prices for, and
/// running the members over different spans would make the shared balance a
/// fiction.
///
/// `max_concurrent_positions` is the most the account may hold at once across
/// every member. `None` is uncapped, which is what every run before books did
/// and what the workbench sends today — deliberately not defaulted to a
/// number, because a cap is a decision about how much of the account one idea
/// may occupy and inventing one here would change every result without being
/// asked.
///
/// Uncapped is not unlimited in practice: the account still runs out of cash.
/// But that is a fact about the balance rather than a risk decision, and the
/// two are worth not confusing for each other.
///
/// # Errors
///
/// Returns [`CommandError`] if fewer than two instruments were named, if any
/// of them holds no bars at the strategy's resolution, or if their coverage
/// does not overlap.
#[tauri::command]
pub async fn run_book(
    instruments: Vec<String>,
    strategy: Option<String>,
    max_concurrent_positions: Option<usize>,
    service: tauri::State<'_, ResearchService>,
) -> Result<StudyView, CommandError> {
    if instruments.len() < 2 {
        return Err(CommandError::Failed(
            "a book needs at least two instruments; one is a study".to_owned(),
        ));
    }

    let name = strategy.unwrap_or_else(|| STRATEGY.to_owned());
    let plan = StrategyPlan::find(&name)
        .ok_or_else(|| CommandError::Failed(format!("no strategy called {name:?}")))?;

    // A ranking rule holding the top few of two is holding one of them, which
    // is a coin toss the rest of the machinery would dutifully evaluate.
    if plan.ranks_a_set() && instruments.len() < MIN_RANKED {
        return Err(CommandError::Failed(format!(
            "{} ranks instruments against each other; {MIN_RANKED} is the fewest a ranking \
             says anything about, and this has {}",
            plan.label,
            instruments.len()
        )));
    }
    let interval = plan.interval();

    // The window every member can be held over, and one hash covering all of
    // them. Both have to span the whole book: a dataset version naming only the
    // head instrument would call a book stale when the head changed and fresh
    // when any other member did.
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut hasher = blake3::Hasher::new();

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

        hasher.update(instrument.as_bytes());
        hasher.update(fingerprint.as_bytes());
        from = from.max(coverage.0);
        to = to.min(coverage.1);
    }

    let window = DateRange::new(from, to).map_err(|_| {
        CommandError::Failed(format!(
            "these {} instruments have no period in common: the latest start is {from} \
             and the earliest end is {to}",
            instruments.len(),
        ))
    })?;
    let dataset_version = hasher.finalize().to_hex().to_string();

    let simulation = service.simulation.clone();
    let engine = simulation.engine().to_owned();
    let library = service.bars.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let head = instruments[0].clone();
        let mut family = study_for(&head, plan, window, &dataset_version);
        // The head stays the experiment's identity; the rest are what it is
        // held alongside. `ExperimentFamily` varies parameters, not
        // instruments, so setting this on the template sets it for every trial.
        family.template.alongside = instruments[1..].to_vec();
        family.template.risk.max_concurrent_positions = max_concurrent_positions;
        family.template.id = ExperimentId(format!("book-{}", instruments.join("+")));
        family.template.hypothesis = HypothesisId(format!(
            "{} predicts returns across {} instruments sharing one account",
            plan.label,
            instruments.len(),
        ));

        let found = arvo_research::run_family(
            simulation.as_ref(),
            &family,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = study_view(&found, &library, &engine);
        Ok((view, Record::Study(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the book did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Persists a finding and hands back its view.
///
/// A failed write does not fail the run: the result is real and already on
/// screen, and refusing to show it because a file could not be written would
/// throw away the expensive half over the cheap half. It is logged loudly
/// instead, since a store that silently stops recording is worse than one that
/// never started.
pub(crate) fn remember<V>(
    service: &tauri::State<'_, ResearchService>,
    (view, record): (V, Record),
) -> Result<V, CommandError> {
    let stored = StoredRecord::new(record, chrono::Utc::now());
    match service.memory.save(&stored) {
        Ok(path) => tracing::info!(id = %stored.id, path = %path.display(), "recorded a finding"),
        Err(err) => tracing::error!(error = %err, id = %stored.id, "could not record a finding"),
    }
    Ok(view)
}

/// The panel's window and combined dataset identity, over whatever
/// instruments currently have data.
///
/// The window is the *overlap* of what the members cover, not the union:
/// instruments judged over different periods are not a cross-section, and a
/// mean across them would compare different markets.
///
/// The identity is every member's hash combined, so editing any one file — or
/// adding or removing an instrument — marks the whole panel result stale.
pub(crate) fn panel_dataset_version(
    bars: &CsvBars,
) -> Option<(String, Vec<String>, chrono::NaiveDate, chrono::NaiveDate)> {
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut hasher = blake3::Hasher::new();
    let mut instruments = Vec::new();

    for id in bars.instruments().ok()? {
        let Ok(Some((first, last))) = bars.coverage(&id, arvo_data::BarInterval::DAILY) else {
            continue;
        };
        if let Ok(Some(fingerprint)) = bars.fingerprint(&id, arvo_data::BarInterval::DAILY) {
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
#[tauri::command]
#[allow(clippy::unnecessary_wraps, reason = "uniform command signature")]
pub fn list_strategies() -> Result<Vec<StrategyView>, CommandError> {
    Ok(PLANS
        .iter()
        .map(|plan| StrategyView {
            name: plan.name.to_owned(),
            label: plan.label.to_owned(),
            premise: plan.premise.to_owned(),
            interval: plan.interval().to_string(),
            backtests: plan.backtests(),
        })
        .collect())
}

/// Runs a rolling re-selection over an instrument's whole history.
///
/// Slower than a study by roughly the number of folds — every fold is a full
/// grid search plus an out-of-sample run — which is why the caller is told the
/// backtest count before it starts.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the instrument has no bars at the
/// strategy's resolution, or the span is too short to roll.
#[tauri::command]
pub async fn run_walk_forward(
    instrument: String,
    strategy: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<WalkForwardView, CommandError> {
    let name = strategy.unwrap_or_else(|| STRATEGY.to_owned());
    let plan = StrategyPlan::find(&name)
        .ok_or_else(|| CommandError::Failed(format!("no strategy called {name:?}")))?;
    let interval = plan.interval();

    let simulation = service.simulation.clone();
    let missing = || {
        CommandError::Failed(format!(
            "{instrument} holds no {interval} bars; {} is defined at that resolution",
            plan.label
        ))
    };
    let coverage = service
        .bars
        .coverage(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
        .ok_or_else(missing)?;
    let fingerprint = service
        .bars
        .fingerprint(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(missing)?;
    let engine = simulation.engine().to_owned();

    let library = service.bars.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let window = DateRange::new(coverage.0, coverage.1)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let procedure = walk_forward_for(&instrument, plan, window, &fingerprint);

        let found = arvo_research::run_walk_forward(
            simulation.as_ref(),
            &procedure,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = walk_forward_view(&found, &library, &engine);
        Ok((view, Record::WalkForward(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the walk-forward did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Runs one configuration across every instrument that has data.
///
/// This is the study that can actually reach a verdict: a single instrument
/// produces a dozen or two round trips against a thirty-trade bar, and no
/// amount of history fixes that. Pooling across instruments does.
#[tauri::command]
pub async fn run_panel(
    service: tauri::State<'_, ResearchService>,
) -> Result<PanelView, CommandError> {
    let (dataset, instruments, from, to) = panel_dataset_version(&service.bars)
        .ok_or_else(|| CommandError::Failed("no instruments with usable data".to_owned()))?;

    let simulation = service.simulation.clone();
    let engine = simulation.engine().to_owned();

    tauri::async_runtime::spawn_blocking(move || {
        let window =
            DateRange::new(from, to).map_err(|err| CommandError::Failed(err.to_string()))?;
        let study = panel_for(instruments, window, &dataset);

        let found = arvo_research::run_panel(
            simulation.as_ref(),
            &study,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = panel_view(&found, &engine);
        Ok((view, Record::Panel(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the panel did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
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
    let template = template_for("panel", plan, window, dataset_version);
    arvo_research::PanelStudy::new(template, instruments, plan.grid())
}

pub(crate) fn template_for(
    subject: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> Experiment {
    Experiment {
        id: ExperimentId(format!("study-{subject}")),
        hypothesis: HypothesisId(format!("trend-following predicts returns in {subject}")),
        instrument: subject.to_owned(),
        alongside: Vec::new(),
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
        },
        strategy: StrategySpec {
            name: plan.name.to_owned(),
            params: plan
                .fixed
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
        },
        costs: CostModel::proportional(COMMISSION_BPS, SLIPPAGE_BPS),
        risk: arvo_research::RiskModel {
            stop_atr_multiple: Some(STOP_ATR_MULTIPLE),
            atr_period: ATR_PERIOD,
            risk_per_trade: Some(RISK_PER_TRADE),
            ..arvo_research::RiskModel::default()
        },
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
const MIN_RANKED: usize = 4;

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
        template: template_for(instrument, plan, window, dataset_version),
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
        template_for(instrument, plan, window, dataset_version),
        plan.grid(),
    )
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
}
