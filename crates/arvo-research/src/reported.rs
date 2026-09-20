//! Evidence any engine can produce, judged the way Arvo judges its own
//! (ADR-0026).
//!
//! A [`Reported`] finding is what a run produces before anyone concludes
//! anything from it: the experiment as data, the strategy's equity curve and
//! trade ledger, a benchmark curve when there is one, and the size of the
//! search it came from. [`judge`] computes the verdict from that with the
//! same criteria a Study uses. There is no field for a verdict, a metric or
//! an excess return: a submitter provides evidence, never a conclusion.
//!
//! # The test that makes this a platform
//!
//! ADR-0005: a trait is earned by two implementations. Arvo's own runners
//! are the second one, so each produces this contract from what it stored:
//! [`FamilyEvidence::reported`], [`WalkForwardEvidence::reported`], and every
//! member of a panel through [`InstrumentOutcome::reported`]. Their tests
//! check that judging the contract gives back the numbers the runner
//! recorded. Whatever a runner knows beyond the contract — a family's
//! selection, a walk-forward's fold stability, a panel's breadth — stays with
//! the runner and is exactly what its own verdict adds.

use serde::{Deserialize, Serialize};

use crate::evaluation::{Evaluation, EvaluationCriteria, Metrics, Verdict};
use crate::family::Selection;
use crate::{
    EquityPoint, Experiment, FamilyEvidence, HypothesisId, InstrumentOutcome, PanelEvidence, Trade,
    TradeStats, WalkForwardEvidence,
};

/// The evidence a verdict is computed from, as any engine can hand it over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    pub hypothesis: HypothesisId,
    /// Instrument, window, interval, dataset, the strategy as a name and
    /// its parameters, costs, risk, starting cash. The strategy name is the
    /// engine's own; Arvo cannot run it and does not pretend to.
    pub experiment: Experiment,
    /// What computed this, by name and version.
    pub engine: String,
    /// Account equity, one point per bar at the experiment's interval.
    pub strategy_curve: Vec<EquityPoint>,
    /// Every round trip, and any position still open at the end.
    pub strategy_ledger: Vec<Trade>,
    /// Buy-and-hold over the same window and capital. Absent, the finding is
    /// `Inconclusive`: a return with nothing to beat is not a result.
    pub benchmark_curve: Option<Vec<EquityPoint>>,
    /// How many trials the author's whole search has run (ADR-0014). Absent,
    /// the finding is held to the largest search the author ever declared.
    pub trials: Option<usize>,
}

/// What Arvo made of reported evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Judgement {
    /// Both curves were there and long enough: the full evaluation, verdict
    /// and reasons included, exactly as a Study carries.
    Evaluated(Box<Evaluation>),
    /// The evidence cannot answer the question either way, and why.
    Inconclusive { reasons: Vec<String> },
}

impl Judgement {
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self {
            Self::Evaluated(evaluation) => evaluation.verdict,
            Self::Inconclusive { .. } => Verdict::Inconclusive,
        }
    }

    #[must_use]
    pub fn reasons(&self) -> &[String] {
        match self {
            Self::Evaluated(evaluation) => &evaluation.reasons,
            Self::Inconclusive { reasons } => reasons,
        }
    }
}

/// Computes the verdict from reported evidence, with the criteria a Study
/// uses and the same order of objections: too little evidence first, then
/// the result. The trade count is the ledger's completed round trips, the
/// way a run's own count is derived, so the two cannot disagree.
#[must_use]
pub fn judge(reported: &Reported, criteria: &EvaluationCriteria) -> Judgement {
    let periods = reported.experiment.interval.periods_per_year();
    let trades = TradeStats::from_ledger(&reported.strategy_ledger).closed;
    let Some(strategy) = Metrics::from_curve(&reported.strategy_curve, trades, periods) else {
        return Judgement::Inconclusive {
            reasons: vec![format!(
                "the strategy's curve has {} equity points, too few to evaluate",
                reported.strategy_curve.len()
            )],
        };
    };
    let Some(benchmark_curve) = &reported.benchmark_curve else {
        return Judgement::Inconclusive {
            reasons: vec!["no benchmark curve: a return with nothing to beat is not a result".to_owned()],
        };
    };
    // Buy-and-hold is one trade; its count plays no part in the verdict.
    let Some(benchmark) = Metrics::from_curve(benchmark_curve, 1, periods) else {
        return Judgement::Inconclusive {
            reasons: vec![format!(
                "the benchmark's curve has {} equity points, too few to evaluate",
                benchmark_curve.len()
            )],
        };
    };
    let evaluation = Evaluation::new(strategy, benchmark, reported.strategy_curve.clone(), benchmark_curve.clone(), criteria)
        .with_trades(reported.strategy_ledger.clone());
    Judgement::Evaluated(Box::new(evaluation))
}

/// A reported finding as the ledger holds it: the evidence, the criteria it
/// was judged by, and what Arvo made of it. The fourth kind of record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportedEvidence {
    pub reported: Reported,
    /// The claim in prose, as the submitter stated it.
    pub claim: String,
    pub criteria: EvaluationCriteria,
    pub judgement: Judgement,
    /// The search this finding declares, in the shape every other record's
    /// search takes, so the author's bar counts it (ADR-0014). No scored
    /// trials: an outside engine reports how many it ran, not what each
    /// scored, and the bar is raised by the count alone.
    pub selection: Selection,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// Judges reported evidence and keeps the judgement with it. The verdict
/// and reasons are copied out where every other record keeps them, so a
/// later refusal can mark this one the same way.
#[must_use]
pub fn record(reported: Reported, claim: String, criteria: &EvaluationCriteria) -> ReportedEvidence {
    let judgement = judge(&reported, criteria);
    let best_sharpe = match &judgement {
        Judgement::Evaluated(evaluation) => evaluation.strategy.sharpe.unwrap_or(0.0),
        Judgement::Inconclusive { .. } => 0.0,
    };
    let selection = Selection {
        trials: reported.trials.unwrap_or(0),
        best_sharpe,
        expected_best_under_null: None,
        survived_deflation: true,
        prior_trials: 0,
        scored: Vec::new(),
    };
    ReportedEvidence {
        verdict: judgement.verdict(),
        reasons: judgement.reasons().to_vec(),
        reported,
        claim,
        criteria: *criteria,
        judgement,
        selection,
    }
}

impl FamilyEvidence {
    /// The out-of-sample evidence this study's verdict came from, as the
    /// contract. What the study knows beyond it is its selection.
    #[must_use]
    pub fn reported(&self) -> Reported {
        let evidence = &self.out_of_sample_evidence;
        Reported {
            hypothesis: evidence.hypothesis.clone(),
            experiment: evidence.experiment.clone(),
            engine: evidence.engine.clone(),
            strategy_curve: evidence.evaluation.strategy_curve.clone(),
            strategy_ledger: evidence.evaluation.strategy_ledger.clone(),
            benchmark_curve: Some(evidence.evaluation.benchmark_curve.clone()),
            trials: Some(self.selection.trials + self.selection.prior_trials),
        }
    }
}

impl WalkForwardEvidence {
    /// The stitched out-of-sample record, as the contract: the combined curve,
    /// every fold's ledger in order, the benchmark over the same span. What
    /// the walk-forward knows beyond it is whether its folds selected above
    /// chance and how stable their choices were.
    #[must_use]
    pub fn reported(&self) -> Reported {
        Reported {
            hypothesis: self.hypothesis.clone(),
            experiment: self.template.clone(),
            engine: self
                .folds
                .first()
                .map(|fold| fold.out_of_sample_evidence.engine.clone())
                .unwrap_or_default(),
            strategy_curve: self.combined_curve.clone(),
            strategy_ledger: self
                .folds
                .iter()
                .flat_map(|fold| fold.out_of_sample_evidence.evaluation.strategy_ledger.iter().cloned())
                .collect(),
            benchmark_curve: Some(self.benchmark_curve.clone()),
            trials: Some(self.folds.iter().map(|fold| fold.selection.trials).sum()),
        }
    }
}

impl InstrumentOutcome {
    /// This member's out-of-sample run as the contract, when the panel kept
    /// it. `None` for a panel recorded before members' curves were kept,
    /// which is a true statement about that record rather than a failure.
    #[must_use]
    pub fn reported(&self, hypothesis: &HypothesisId, trials: usize) -> Option<Reported> {
        let kept = self.kept.as_ref()?;
        Some(Reported {
            hypothesis: hypothesis.clone(),
            experiment: kept.experiment.clone(),
            engine: kept.engine.clone(),
            strategy_curve: kept.strategy_curve.clone(),
            strategy_ledger: kept.ledger.clone(),
            benchmark_curve: Some(kept.benchmark_curve.clone()),
            trials: Some(trials),
        })
    }
}

impl PanelEvidence {
    /// Every member as the contract, one per instrument. What the panel knows
    /// beyond them is the pooling: breadth, and how many beat their
    /// benchmark. `None` for a panel recorded before members' curves were
    /// kept.
    #[must_use]
    pub fn reported(&self) -> Option<Vec<Reported>> {
        let trials = self.selection.trials + self.selection.prior_trials;
        self.per_instrument.iter().map(|member| member.reported(&self.hypothesis, trials)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Record;
    use crate::{Direction, Evidence, ExitReason, ExperimentId, Selection};
    use chrono::NaiveDate;

    fn day(n: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 1, 1).expect("a date").and_hms_opt(0, 0, 0).expect("a time")
            + chrono::Duration::days(i64::from(n))
    }

    /// A curve that compounds `step` a day for `days` days from 100,000.
    fn curve(step: f64, days: u32) -> Vec<EquityPoint> {
        (0..=days)
            .map(|n| EquityPoint { at: day(n), equity: 100_000.0 * (1.0 + step).powi(n as i32) })
            .collect()
    }

    fn round_trip(n: u32, pnl: f64) -> Trade {
        Trade {
            instrument: String::new(),
            opened: day(n),
            closed: Some(day(n + 1)),
            direction: Direction::Long,
            quantity: 1.0,
            entry: 100.0,
            exit: Some(100.0 + pnl),
            pnl,
            commission: 0.0,
            exit_reason: ExitReason::Signal,
            journal: None,
        }
    }

    fn experiment() -> Experiment {
        let Record::Study(family) = crate::memory::tests::study("AAPL.NASDAQ", "hash-a") else { unreachable!() };
        family.selected.clone()
    }

    /// A study built the way the runner builds one: metrics from the curves,
    /// the verdict from the criteria, the ledger's round trips as the count.
    fn study(strategy_step: f64, benchmark_step: f64, trades: u32) -> FamilyEvidence {
        let criteria = EvaluationCriteria::default();
        let experiment = experiment();
        let periods = experiment.interval.periods_per_year();
        let strategy_curve = curve(strategy_step, 300);
        let benchmark_curve = curve(benchmark_step, 300);
        let ledger: Vec<Trade> = (0..trades).map(|n| round_trip(n, 10.0)).collect();
        let closed = TradeStats::from_ledger(&ledger).closed;
        let evaluation = Evaluation::new(
            Metrics::from_curve(&strategy_curve, closed, periods).expect("long enough"),
            Metrics::from_curve(&benchmark_curve, 1, periods).expect("long enough"),
            strategy_curve,
            benchmark_curve,
            &criteria,
        )
        .with_trades(ledger);
        let (verdict, reasons) = (evaluation.verdict, evaluation.reasons.clone());
        FamilyEvidence {
            hypothesis: experiment.hypothesis.clone(),
            in_sample: experiment.window,
            out_of_sample: experiment.window,
            selection: Selection {
                trials: 9,
                best_sharpe: 1.0,
                expected_best_under_null: Some(0.5),
                survived_deflation: true,
                prior_trials: 3,
                scored: Vec::new(),
            },
            out_of_sample_evidence: Evidence {
                hypothesis: experiment.hypothesis.clone(),
                experiment: experiment.clone(),
                benchmark: ExperimentId::from("b"),
                engine: "test engine 1".to_owned(),
                criteria,
                evaluation,
            },
            selected: experiment,
            failures: Vec::new(),
            verdict,
            reasons,
        }
    }

    /// The ADR-0005 test for a Study: what it stored, handed back as the
    /// contract and judged again, is the same verdict for the same reasons,
    /// across every verdict the criteria can produce.
    #[test]
    fn a_study_judged_from_its_own_contract_gets_its_own_verdict_back() {
        let cases = [
            (0.002, 0.0005, 40, "beats the benchmark on enough trades"),
            (0.0005, 0.002, 40, "loses to the benchmark"),
            (0.002, 0.0005, 3, "too few trades to read"),
        ];
        for (strategy, benchmark, trades, what) in cases {
            let stored = study(strategy, benchmark, trades);
            let again = judge(&stored.reported(), &stored.out_of_sample_evidence.criteria);
            assert_eq!(again.verdict(), stored.verdict, "{what}");
            assert_eq!(again.reasons(), stored.reasons.as_slice(), "{what}");
            let Judgement::Evaluated(evaluation) = &again else { panic!("{what}: both curves were there") };
            assert_eq!(evaluation.strategy, stored.out_of_sample_evidence.evaluation.strategy, "{what}");
            assert_eq!(evaluation.excess_return, stored.out_of_sample_evidence.evaluation.excess_return, "{what}");
            assert_eq!(stored.reported().trials, Some(12), "the whole search, prior trials included");
        }
    }

    /// The record kind: judged once, stored with its judgement, and it comes
    /// back from JSON as the same thing, including the search the bar counts.
    #[test]
    fn a_reported_finding_is_recorded_with_its_judgement_and_survives_a_round_trip() {
        let stored = study(0.002, 0.0005, 40);
        let mut reported = stored.reported();
        reported.trials = Some(7);
        let evidence = record(reported, "momentum persists".to_owned(), &EvaluationCriteria::default());
        assert_eq!(evidence.verdict, stored.verdict, "Arvo's verdict, not the submitter's");
        assert_eq!(evidence.selection.trials, 7, "the declared search, where the bar reads it");
        assert!(evidence.selection.scored.is_empty());
        let text = serde_json::to_string(&evidence).expect("encodes");
        let back: ReportedEvidence = serde_json::from_str(&text).expect("decodes");
        // Field by field rather than whole: a curve's equity is written to
        // the stored format's precision, so the points are not bit-identical
        // after a round trip and were never meant to be.
        assert_eq!(back.verdict, evidence.verdict);
        assert_eq!(back.reasons, evidence.reasons);
        assert_eq!(back.selection, evidence.selection);
        assert_eq!(back.claim, evidence.claim);
        assert_eq!(back.reported.engine, evidence.reported.engine);
        assert_eq!(back.reported.experiment.dataset, evidence.reported.experiment.dataset);
        assert_eq!(back.reported.strategy_curve.len(), evidence.reported.strategy_curve.len());
        assert_eq!(back.reported.strategy_ledger.len(), evidence.reported.strategy_ledger.len());
        assert_eq!(back.judgement.verdict(), evidence.judgement.verdict());
    }

    /// What a submitter cannot leave out, and what happens when they do.
    #[test]
    fn without_a_benchmark_or_enough_curve_the_answer_is_inconclusive_and_says_why() {
        let mut reported = study(0.002, 0.0005, 40).reported();
        reported.benchmark_curve = None;
        let judged = judge(&reported, &EvaluationCriteria::default());
        assert_eq!(judged.verdict(), Verdict::Inconclusive);
        assert!(judged.reasons()[0].contains("no benchmark"), "{:?}", judged.reasons());

        let mut reported = study(0.002, 0.0005, 40).reported();
        reported.strategy_curve.truncate(1);
        let judged = judge(&reported, &EvaluationCriteria::default());
        assert_eq!(judged.verdict(), Verdict::Inconclusive);
        assert!(judged.reasons()[0].contains("too few"), "{:?}", judged.reasons());
    }

    /// The ADR-0005 test for a walk-forward: the stitched record, judged as
    /// the contract, reproduces the numbers the runner recorded. The verdict
    /// itself may differ, because a walk-forward also asks whether its folds
    /// selected above chance, which is what it knows beyond the contract.
    #[test]
    fn a_walk_forward_judged_from_its_contract_reproduces_its_numbers() {
        let fold_a = study(0.002, 0.0005, 20);
        let fold_b = study(0.0015, 0.0005, 20);
        let periods = fold_a.selected.interval.periods_per_year();
        let combined_curve = curve(0.0018, 600);
        let benchmark_curve = curve(0.0005, 600);
        let ledger_len = 40;
        let combined = Metrics::from_curve(&combined_curve, ledger_len, periods).expect("long enough");
        let benchmark = Metrics::from_curve(&benchmark_curve, 1, periods).expect("long enough");
        let walk = WalkForwardEvidence {
            hypothesis: fold_a.hypothesis.clone(),
            template: fold_a.selected.clone(),
            in_sample_days: 300,
            step_days: 300,
            anchored: false,
            grid: None,
            criteria: Some(EvaluationCriteria::default()),
            excess_return: combined.total_return - benchmark.total_return,
            combined,
            benchmark,
            combined_curve,
            benchmark_curve,
            combined_trades: TradeStats::default(),
            stability: Vec::new(),
            folds_surviving_deflation: 2,
            folds_without_trades: 0,
            folds: vec![fold_a, fold_b],
            verdict: Verdict::Supported,
            reasons: Vec::new(),
        };

        let reported = walk.reported();
        assert_eq!(reported.strategy_ledger.len(), ledger_len as usize, "every fold's ledger, in order");
        assert_eq!(reported.trials, Some(18), "the folds' searches, summed");
        assert_eq!(reported.engine, "test engine 1");
        let Judgement::Evaluated(again) = judge(&reported, &EvaluationCriteria::default()) else { panic!("both curves") };
        assert_eq!(again.strategy.total_return, walk.combined.total_return);
        assert_eq!(again.strategy.max_drawdown, walk.combined.max_drawdown);
        assert_eq!(again.benchmark.total_return, walk.benchmark.total_return);
        assert_eq!(again.excess_return, walk.excess_return);
    }

    /// The ADR-0005 test for a panel: a member that kept its run is the
    /// contract, and judged again gives the numbers the panel pooled. A member
    /// from before curves were kept says so with `None`, not with a wrong
    /// answer.
    #[test]
    fn a_panel_member_that_kept_its_run_is_the_contract_and_one_that_did_not_says_so() {
        let stored = study(0.002, 0.0005, 30);
        let evidence = &stored.out_of_sample_evidence;
        let kept = InstrumentOutcome {
            instrument: "AAPL.NASDAQ".to_owned(),
            strategy: evidence.evaluation.strategy.clone(),
            benchmark: evidence.evaluation.benchmark.clone(),
            excess_return: evidence.evaluation.excess_return,
            kept: Some(crate::panel::KeptEvidence {
                experiment: evidence.experiment.clone(),
                engine: evidence.engine.clone(),
                strategy_curve: evidence.evaluation.strategy_curve.clone(),
                benchmark_curve: evidence.evaluation.benchmark_curve.clone(),
                ledger: evidence.evaluation.strategy_ledger.clone(),
            }),
        };
        let reported = kept.reported(&stored.hypothesis, 12).expect("kept");
        let Judgement::Evaluated(again) = judge(&reported, &EvaluationCriteria::default()) else { panic!("both curves") };
        assert_eq!(again.strategy, kept.strategy);
        assert_eq!(again.excess_return, kept.excess_return);

        let before = InstrumentOutcome { kept: None, ..kept };
        assert!(before.reported(&stored.hypothesis, 12).is_none(), "recorded before its curves were kept");
    }
}
