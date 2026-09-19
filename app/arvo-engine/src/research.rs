//! The research tier: what any front end other than the window may do.
//!
//! Read research memory, and run research. Nothing else:
//!
//! - **No fetching.** Changing the data library makes existing findings stale
//!   and reaches a vendor with a credential; both are a person's decision.
//! - **No sharing, no broker, no orders.** Nothing here names a source, a key
//!   or an executor, so there is nothing to misuse — the boundary is what is
//!   offered, not a check an argument could talk its way past (ADR-0016).
//!
//! # Why every run is its author's, and deflated
//!
//! A caller running study after study until one comes out `Supported` is an
//! unbounded search nothing else counts (#25). So a run is saved through
//! `StoredRecord::by_agent`, held to the bar for everything this author has
//! tried, and the result says so. An agent over MCP and a script over gRPC are
//! both authors in that sense.
//!
//! # The audit trail (#33)
//!
//! Every call is appended to `agent-audit.jsonl` beside the evidence: when,
//! who, through what, which call, the arguments, whether it worked, and the
//! finding it produced.

use std::io::Write;
use std::path::{Path, PathBuf};

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{EvaluationCriteria, EvidenceStore, Record, StoredRecord};
use arvo_service::research::{study_data, study_for, walk_forward_for, StrategyPlan};
use serde_json::{json, Value};

/// Where calls are recorded, in the app data directory.
pub const AUDIT_FILE: &str = "agent-audit.jsonl";

/// Where the window keeps its data: the open project folder, else its app
/// data directory — the same choice `arvo_service::project::data_root`
/// makes, so a script and the window read one library.
///
/// # Errors
///
/// When no project is open and `APPDATA` is not set.
pub fn default_root() -> Result<PathBuf, String> {
    match arvo_service::project::remembered() {
        Some(folder) => Ok(folder),
        None => arvo_service::project::app_data_root().map_err(|err| format!("no data directory given and {err}")),
    }
}

/// Research over one app data directory. Cheap to clone: it holds paths.
#[derive(Debug, Clone)]
pub struct Research {
    data: PathBuf,
    evidence: PathBuf,
    audit: PathBuf,
}

impl Research {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            data: root.join("data"),
            evidence: root.join("evidence"),
            audit: root.join(AUDIT_FILE),
        }
    }

    fn store(&self) -> EvidenceStore {
        EvidenceStore::new(&self.evidence)
    }

    /// The project folder this research runs over.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.data.parent().unwrap_or(&self.data)
    }

    /// Every instrument with data, by resolution and date range.
    ///
    /// # Errors
    ///
    /// When the library cannot be listed.
    pub fn list_instruments(&self) -> Result<Value, String> {
        let bars = CsvBars::new(&self.data);
        let mut out = Vec::new();
        for interval in [
            arvo_data::BarInterval::DAILY,
            arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute),
        ] {
            let names: Vec<String> = if interval.is_intraday() {
                std::fs::read_dir(self.data.join(interval.to_string()))
                    .map(|entries| {
                        entries
                            .flatten()
                            .filter_map(|entry| {
                                entry.path().file_stem().map(|s| s.to_string_lossy().into_owned())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                bars.instruments().map_err(|err| err.to_string())?
            };
            for name in names {
                if let Ok(Some((from, to))) = bars.coverage(&name, interval) {
                    out.push(json!({
                        "instrument": name,
                        "interval": interval.to_string(),
                        "from": from.to_string(),
                        "to": to.to_string(),
                    }));
                }
            }
        }
        Ok(json!({ "instruments": out }))
    }

    /// Every finding in research memory.
    ///
    /// # Errors
    ///
    /// When the evidence store cannot be read at all.
    pub fn list_findings(&self) -> Result<Value, String> {
        let (summaries, unreadable) = self.store().summaries().map_err(|err| err.to_string())?;
        Ok(json!({
            "findings": summaries.iter().map(|s| json!({
                "id": s.id,
                "kind": s.kind,
                "subject": s.subject,
                "verdict": s.verdict,
                "recorded_at": s.recorded_at.to_rfc3339(),
                "agent": s.agent,
            })).collect::<Vec<_>>(),
            "unreadable": unreadable.len(),
        }))
    }

    /// One finding, summarised.
    ///
    /// # Errors
    ///
    /// When there is no such finding or it cannot be read.
    pub fn open_finding(&self, id: &str) -> Result<Value, String> {
        let stored = self.store().open(id).map_err(|err| err.to_string())?;
        Ok(summarize(&stored))
    }

    /// Runs a study, or a walk-forward when `rolling`, saved as `author`'s.
    ///
    /// # Errors
    ///
    /// An unknown strategy, a ranking strategy on one instrument, an
    /// instrument without data, a failed run or a failed save.
    pub fn run(
        &self,
        instrument: &str,
        strategy: &str,
        rolling: bool,
        author: &str,
        origin: Option<&str>,
    ) -> Result<Value, String> {
        // The project's risk model, the same one the window's studies run
        // under; a bad file refuses the run rather than defaulting. And its
        // rulesets, so a strategy can be one an agent wrote a moment ago.
        let root = self.data.parent().unwrap_or(&self.data);
        arvo_service::risk::load(root)?;
        arvo_service::rulesets::refresh_at(root);
        let plan = StrategyPlan::find(strategy)
            .ok_or_else(|| format!("no strategy {strategy:?}; list_strategies says what there is"))?;
        if plan.ranks_a_set() {
            return Err(format!(
                "{strategy} ranks instruments against each other and cannot be run on one"
            ));
        }
        let bars = CsvBars::new(&self.data);
        let (window, fingerprint) = study_data(&bars, instrument, plan)?;
        let simulation = NautilusSimulation::new(CsvBars::new(&self.data));
        let criteria = EvaluationCriteria::default();

        let record = if rolling {
            let procedure = walk_forward_for(instrument, plan, window, &fingerprint);
            Record::WalkForward(Box::new(
                arvo_research::run_walk_forward(&simulation, &procedure, &criteria)
                    .map_err(|err| err.to_string())?,
            ))
        } else {
            let family = study_for(instrument, plan, window, &fingerprint);
            Record::Study(Box::new(
                arvo_research::run_family(&simulation, &family, &criteria)
                    .map_err(|err| err.to_string())?,
            ))
        };

        let store = self.store();
        let history = store.load().map_err(|err| err.to_string())?.records;
        let stored = StoredRecord::by_agent(record, author, &history, chrono::Utc::now())
            .with_origin(origin.map(ToOwned::to_owned));
        store.save(&stored).map_err(|err| err.to_string())?;
        Ok(summarize(&stored))
    }

    /// Records evidence an engine Arvo did not run computed, judged here by
    /// the criteria a study uses, under `author`, held to the author's whole
    /// search (ADR-0026).
    ///
    /// # Errors
    ///
    /// The store cannot be read or written.
    pub fn record(
        &self,
        reported: arvo_research::Reported,
        claim: String,
        author: &str,
        origin: Option<&str>,
    ) -> Result<Value, String> {
        let evidence = arvo_research::reported::record(reported, claim, &EvaluationCriteria::default());
        let record = Record::Reported(Box::new(evidence));
        let store = self.store();
        let history = store.load().map_err(|err| err.to_string())?.records;
        let stored = StoredRecord::by_agent(record, author, &history, chrono::Utc::now())
            .with_origin(origin.map(ToOwned::to_owned));
        store.save(&stored).map_err(|err| err.to_string())?;
        Ok(summarize(&stored))
    }

    /// Keeps a file with a finding (#157): bytes stored once by hash, the
    /// record listing it. Returns every attachment the finding now has.
    ///
    /// # Errors
    ///
    /// No such finding, or the store cannot be written.
    pub fn attach(&self, id: &str, name: &str, media_type: &str, data: &[u8]) -> Result<Value, String> {
        let kept = self.store().attach(id, name, media_type, data).map_err(|err| err.to_string())?;
        Ok(json!(kept))
    }

    /// Appends one line to the audit trail. Best effort, loudly: a trail that
    /// cannot be written is said on stderr and never fails the call — the
    /// finding, which is the durable record, is already saved.
    pub fn audit(
        &self,
        via: &str,
        author: &str,
        call: &str,
        arguments: &Value,
        outcome: &Result<Value, String>,
    ) {
        let line = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "agent": author,
            "via": via,
            "tool": call,
            "arguments": arguments,
            "ok": outcome.is_ok(),
            "finding": outcome.as_ref().ok().and_then(|value| value.get("id")).cloned(),
            "error": outcome.as_ref().err(),
        });
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(err) = written {
            eprintln!("could not append to {}: {err}", self.audit.display());
        }
    }
}

/// What a reader needs to read a finding, and nothing it could mistake for
/// more: the verdict before the numbers, the advice beside them, and who ran it
/// against what bar.
#[must_use]
pub fn summarize(stored: &StoredRecord) -> Value {
    let record = &stored.record;
    let mut out = json!({
        "id": stored.id,
        "kind": record.kind(),
        "subject": record.subject(),
        "verdict": record.verdict(),
        "recorded_at": stored.recorded_at.to_rfc3339(),
        "author": stored.author,
        "attachments": stored.attachments,
        "read_this_first": record.verdict().read_this_first(),
    });
    match record {
        Record::Study(found) => {
            let evaluation = &found.out_of_sample_evidence.evaluation;
            out["reasons"] = json!(found.reasons);
            out["advice"] = advice(arvo_research::recommend(found));
            out["search"] = json!({
                "trials": found.selection.trials,
                "prior_trials": found.selection.prior_trials,
                "best_in_sample_sharpe": found.selection.best_sharpe,
                "expected_best_under_null": found.selection.expected_best_under_null,
                "survived_deflation": found.selection.survived_deflation,
                "chosen": found.selected.strategy.params,
            });
            out["out_of_sample"] = json!({
                "from": found.out_of_sample.from.to_string(),
                "to": found.out_of_sample.to.to_string(),
                "trades": evaluation.strategy.trades,
                "total_return": evaluation.strategy.total_return,
                "excess_return": evaluation.excess_return,
                "sharpe": evaluation.strategy.sharpe,
                "max_drawdown": evaluation.strategy.max_drawdown,
                "refused_orders": evaluation.refused_orders,
            });
        }
        Record::WalkForward(found) => {
            out["reasons"] = json!(found.reasons);
            out["advice"] = advice(arvo_research::recommend_walk_forward(found));
            out["combined"] = json!({
                "folds": found.folds.len(),
                "folds_surviving_deflation": found.folds_surviving_deflation,
                "trades": found.combined_trades.closed,
                "total_return": found.combined.total_return,
                "excess_return": found.excess_return,
                "sharpe": found.combined.sharpe,
                "max_drawdown": found.combined.max_drawdown,
            });
        }
        Record::Panel(found) => {
            out["reasons"] = json!(found.reasons);
            out["pooled"] = json!({
                "instruments": found.pooled.instruments,
                "trades": found.pooled.total_trades,
                "mean_excess_return": found.pooled.mean_excess_return,
            });
        }
        Record::Reported(found) => {
            let evaluated = match &found.judgement {
                arvo_research::Judgement::Evaluated(evaluation) => Some(evaluation.as_ref()),
                arvo_research::Judgement::Inconclusive { .. } => None,
            };
            let experiment = &found.reported.experiment;
            out["reasons"] = json!(found.reasons);
            // No advice: the recommendations read a study's selection, and
            // a reported finding has none. What it has is the verdict.
            out["advice"] = json!([]);
            out["reported"] = json!({
                "engine": found.reported.engine,
                "claim": found.claim,
                "instrument": experiment.instrument,
                "from": experiment.window.from.to_string(),
                "to": experiment.window.to.to_string(),
                "dataset": format!("{}@{}", experiment.dataset.id, experiment.dataset.version),
                "trials": found.reported.trials,
                "trades": arvo_research::TradeStats::from_ledger(&found.reported.strategy_ledger).closed,
                "total_return": evaluated.map(|e| e.strategy.total_return),
                "excess_return": evaluated.map(|e| e.excess_return),
                "sharpe": evaluated.and_then(|e| e.strategy.sharpe),
                "max_drawdown": evaluated.map(|e| e.strategy.max_drawdown),
            });
        }
    }
    out
}

fn advice(items: Vec<arvo_research::Recommendation>) -> Value {
    json!(items
        .iter()
        .map(|item| json!({
            "severity": item.severity.label(),
            "finding": item.finding,
            "action": item.action,
            "evidence": item.evidence,
        }))
        .collect::<Vec<_>>())
}
