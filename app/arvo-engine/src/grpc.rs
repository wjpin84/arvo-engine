//! The research tier over gRPC, behind the engine's token.

use std::future::Future;

use serde_json::Value;
use tokio::net::TcpListener;
use tonic::{Request, Response, Status};

use crate::research::Research;
use crate::session::Sessions;

pub use arvo_client::proto as proto;

use proto::research_server::{self, ResearchServer};
use proto::sessions_server::{self, SessionsServer};
use proto::{
    Advice, AttachRequest, Attachment, Attachments, Empty, Finding, FindingId, FindingSummary, Findings,
    BookRequest, FindingIds, Instrument, Instruments, Point, ReportRequest, RiskModel, Rules, RulesetForm, RulesetPath,
    Rulesets, Ruleset, RunRequest, StudyRequest, View, SessionId, SessionList, SessionStatus, StartRequest, Strategies,
};
use arvo_client::wire;

/// The keys of a summary that have their own fields; everything else is
/// `detail_json`.
const TYPED: &[&str] = &[
    "id", "kind", "subject", "verdict", "recorded_at", "read_this_first", "reasons", "advice", "attachments",
];

/// The largest message either side accepts: a figure or a trades table
/// attached to a finding, with room. tonic's default is 4 MB.
pub use arvo_client::wire::MAX_MESSAGE_BYTES;

struct Service {
    research: Research,
    /// The workbench's runs and memory over the same folder: what the window
    /// renders, served here so the window need not run them itself.
    workbench: std::sync::Arc<arvo_service::research::ResearchService>,
}

struct Control {
    sessions: std::sync::Arc<Sessions>,
}

/// The two tokens the engine serves behind: research for every front end,
/// control for the ones allowed to reach an executor (ADR-0018 point 4).
pub struct Tokens {
    pub research: String,
    pub control: String,
}

/// Serves the research tier and the control tier on `listener` until
/// `shutdown` resolves.
///
/// Every call must carry `authorization: Bearer <token>`. The engine's own
/// listener is loopback-only; the token is what stops another local program
/// that has not read the user's `engine.json` — or, for a session, its
/// `control.json`.
///
/// # Errors
///
/// When the transport fails.
pub async fn serve(
    listener: TcpListener,
    research: Research,
    sessions: std::sync::Arc<Sessions>,
    tokens: &Tokens,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let workbench = std::sync::Arc::new(arvo_service::research::ResearchService::new(
        research.root().join(arvo_service::research::DATA_SUBDIR),
        research.root().join(arvo_service::research::EVIDENCE_SUBDIR),
    ));
    let research_tier = tonic::service::interceptor::InterceptedService::new(
        ResearchServer::new(Service { research, workbench }).max_decoding_message_size(MAX_MESSAGE_BYTES),
        bearer(&tokens.research, "engine.json"),
    );
    let control_tier = tonic::service::interceptor::InterceptedService::new(
        SessionsServer::new(Control { sessions }),
        bearer(&tokens.control, "control.json"),
    );
    tonic::transport::Server::builder()
        .add_service(research_tier)
        .add_service(control_tier)
        .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), shutdown)
        .await
}

/// An interceptor admitting only `Bearer <token>`.
fn bearer(token: &str, file: &'static str) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    let expected = format!("Bearer {token}");
    move |request: Request<()>| match request.metadata().get("authorization").and_then(|value| value.to_str().ok()) {
        Some(given) if given == expected => Ok(request),
        _ => Err(Status::unauthenticated(format!(
            "missing or wrong token; read it from {file} in the Arvo app data directory"
        ))),
    }
}

fn session_status(status: crate::session::Status) -> SessionStatus {
    SessionStatus {
        id: status.id,
        finding: status.finding,
        executor: status.executor,
        instrument: status.instrument,
        strategy: status.strategy,
        started_at: status.started_at,
        state: status.state,
        signals: status.signals,
        submitted: status.submitted,
        refused: status.refused,
        fills: status.fills,
        halted: status.halted,
        last_error: status.last_error,
        last_bar: status.last_bar,
    }
}

#[tonic::async_trait]
impl sessions_server::Sessions for Control {
    async fn start_session(&self, request: Request<StartRequest>) -> Result<Response<SessionStatus>, Status> {
        let request = request.into_inner();
        let finding = required(&request.finding, "finding")?;
        let executor = required(&request.executor, "executor")?;
        self.sessions
            .start(finding, executor)
            .map(|status| Response::new(session_status(status)))
            .map_err(Status::failed_precondition)
    }

    async fn stop_session(&self, request: Request<SessionId>) -> Result<Response<SessionStatus>, Status> {
        let id = request.into_inner().id;
        let sessions = self.sessions.clone();
        // Joins the session thread, which can take a poll step.
        tokio::task::spawn_blocking(move || sessions.stop(&id))
            .await
            .map_err(|err| Status::internal(err.to_string()))?
            .map(|status| Response::new(session_status(status)))
            .map_err(Status::not_found)
    }

    async fn list_sessions(&self, _: Request<Empty>) -> Result<Response<SessionList>, Status> {
        Ok(Response::new(SessionList {
            sessions: self.sessions.list().into_iter().map(session_status).collect(),
        }))
    }
}

/// A JSON value as text: a string as itself, null as empty, anything else as
/// its JSON.
/// The workbench's refusal, as the caller's mistake: a rule that does not
/// exist, an instrument without bars, a finding that is not there.
fn refused(err: arvo_service::CommandError) -> Status {
    Status::invalid_argument(err.to_string())
}

/// A view, serialised for the wire.
fn viewed<T: serde::Serialize>(kind: &str, value: &T) -> Result<Response<View>, Status> {
    wire::view(kind, value).map(Response::new).map_err(Status::internal)
}

/// Runs `work` on a blocking thread with the workbench: studies are backtests,
/// and the async runtime is not where they belong.
async fn blocking<T: Send + 'static>(
    workbench: &std::sync::Arc<arvo_service::research::ResearchService>,
    work: impl FnOnce(&arvo_service::research::ResearchService) -> Result<T, arvo_service::CommandError> + Send + 'static,
) -> Result<T, Status> {
    let workbench = workbench.clone();
    tokio::task::spawn_blocking(move || work(&workbench))
        .await
        .map_err(|err| Status::internal(format!("the run did not finish: {err}")))?
        .map_err(refused)
}

fn size(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

fn required<'a>(field: &'a str, name: &str) -> Result<&'a str, Status> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        Err(Status::invalid_argument(format!("{name} is required")))
    } else {
        Ok(trimmed)
    }
}

fn finding(summary: &Value) -> Finding {
    let author = summary.pointer("/author/id").and_then(Value::as_str).unwrap_or_default();
    let detail: serde_json::Map<String, Value> = summary
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !TYPED.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    Finding {
        summary: Some(FindingSummary {
            id: text(summary.get("id")),
            kind: text(summary.get("kind")),
            subject: text(summary.get("subject")),
            verdict: text(summary.get("verdict")),
            recorded_at: text(summary.get("recorded_at")),
            author: author.to_owned(),
        }),
        read_this_first: text(summary.get("read_this_first")),
        reasons: summary
            .get("reasons")
            .and_then(Value::as_array)
            .map(|reasons| reasons.iter().map(|reason| text(Some(reason))).collect())
            .unwrap_or_default(),
        advice: summary
            .get("advice")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| Advice {
                        severity: text(item.get("severity")),
                        finding: text(item.get("finding")),
                        action: text(item.get("action")),
                        evidence: text(item.get("evidence")),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        attachments: attachments(summary.get("attachments")),
        detail_json: Value::Object(detail).to_string(),
    }
}

fn attachments(listed: Option<&Value>) -> Vec<Attachment> {
    listed
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| Attachment {
                    name: text(item.get("name")),
                    media_type: text(item.get("media_type")),
                    hash: text(item.get("hash")),
                    bytes: item.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    added_at: text(item.get("added_at")),
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Service {
    async fn run(&self, request: RunRequest, rolling: bool) -> Result<Response<Finding>, Status> {
        let instrument = required(&request.instrument, "instrument")?.to_owned();
        let strategy = required(&request.strategy, "strategy")?.to_owned();
        let author = required(&request.author, "author")?.to_owned();
        let origin = Some(request.origin.trim().to_owned()).filter(|origin| !origin.is_empty());
        let research = self.research.clone();
        // A study is seconds to a minute of engine work; off the async threads.
        let outcome = tokio::task::spawn_blocking(move || {
            let call = if rolling { "run_walk_forward" } else { "run_study" };
            let outcome = research.run(&instrument, &strategy, rolling, &author, origin.as_deref());
            let arguments = serde_json::json!({ "instrument": instrument, "strategy": strategy });
            research.audit("grpc", &author, call, &arguments, &outcome);
            outcome
        })
        .await
        .map_err(|err| Status::internal(format!("the run did not finish: {err}")))?;
        outcome
            .map(|summary| Response::new(finding(&summary)))
            .map_err(Status::failed_precondition)
    }
}

#[tonic::async_trait]
impl research_server::Research for Service {
    async fn list_strategies(&self, _: Request<Empty>) -> Result<Response<Strategies>, Status> {
        let plans = arvo_service::research::list_strategies().map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(Strategies {
            strategies: plans
                .into_iter()
                .map(|plan| {
                    let ranks_a_set = arvo_service::research::StrategyPlan::find(&plan.name)
                        .is_some_and(|found| found.ranks_a_set());
                    wire::strategy(plan, ranks_a_set)
                })
                .collect(),
        }))
    }

    async fn list_rulesets(&self, _: Request<Empty>) -> Result<Response<Rulesets>, Status> {
        let rulesets = arvo_service::rulesets::list(self.research.root()).into_iter().map(wire::ruleset).collect();
        Ok(Response::new(Rulesets { rulesets }))
    }

    async fn read_ruleset(&self, request: Request<RulesetPath>) -> Result<Response<RulesetForm>, Status> {
        let path = required(&request.get_ref().path, "path")?;
        arvo_service::rulesets::read_form(self.research.root(), path)
            .map(|form| Response::new(wire::ruleset_form(form)))
            .map_err(Status::invalid_argument)
    }

    async fn write_ruleset(&self, request: Request<RulesetForm>) -> Result<Response<Ruleset>, Status> {
        let form = wire::ruleset_form_view(request.into_inner());
        arvo_service::rulesets::write_form(self.research.root(), form)
            .map(|written| Response::new(wire::ruleset(written)))
            .map_err(Status::invalid_argument)
    }

    async fn list_rules(&self, _: Request<Empty>) -> Result<Response<Rules>, Status> {
        Ok(Response::new(Rules { rules: arvo_service::rulesets::list_rules().into_iter().map(wire::rule).collect() }))
    }

    async fn get_risk_model(&self, _: Request<Empty>) -> Result<Response<RiskModel>, Status> {
        Ok(Response::new(wire::risk_model(arvo_service::risk::view(self.research.root()))))
    }

    async fn view_study(&self, request: Request<StudyRequest>) -> Result<Response<View>, Status> {
        let StudyRequest { instrument, strategy } = request.into_inner();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_study(workbench, &instrument, strategy.as_deref())
        })
        .await?;
        viewed("StudyView", &view)
    }

    async fn view_walk_forward(&self, request: Request<StudyRequest>) -> Result<Response<View>, Status> {
        let StudyRequest { instrument, strategy } = request.into_inner();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_walk_forward(workbench, &instrument, strategy.as_deref())
        })
        .await?;
        viewed("WalkForwardView", &view)
    }

    async fn view_panel(&self, _: Request<Empty>) -> Result<Response<View>, Status> {
        let view = blocking(&self.workbench, arvo_service::research::study::run_panel).await?;
        viewed("PanelView", &view)
    }

    async fn view_book(&self, request: Request<BookRequest>) -> Result<Response<View>, Status> {
        let BookRequest { instruments, strategy, max_concurrent_positions, max_per_sector } = request.into_inner();
        let sector_cap = arvo_service::research::study::book_sector_cap(max_per_sector.map(size), &instruments)
            .await
            .map_err(refused)?;
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_book(
                workbench,
                instruments,
                strategy.as_deref(),
                max_concurrent_positions.map(size),
                sector_cap,
            )
        })
        .await?;
        viewed("StudyView", &view)
    }

    async fn view_history(&self, _: Request<Empty>) -> Result<Response<View>, Status> {
        let view = arvo_service::research::history::list_history(&self.workbench).map_err(refused)?;
        viewed("HistoryView", &view)
    }

    async fn view_record(&self, request: Request<FindingId>) -> Result<Response<View>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        let view = arvo_service::research::history::open_record(&self.workbench, id).map_err(refused)?;
        viewed("RecordView", &view)
    }

    async fn view_replay(&self, request: Request<FindingId>) -> Result<Response<View>, Status> {
        let id = required(&request.get_ref().id, "id")?.to_owned();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::history::replay_record(workbench, &id)
        })
        .await?;
        viewed("ReplayView", &view)
    }

    async fn view_comparison(&self, request: Request<FindingIds>) -> Result<Response<View>, Status> {
        let view = arvo_service::research::history::compare_records(&self.workbench, &request.get_ref().ids)
            .map_err(refused)?;
        viewed("ComparisonView", &view)
    }

    async fn view_problems(&self, _: Request<Empty>) -> Result<Response<View>, Status> {
        let view = arvo_service::research::history::list_research_problems(&self.workbench, self.research.root())
            .map_err(refused)?;
        viewed("Vec<ResearchProblemView>", &view)
    }

    async fn list_attachments(&self, request: Request<FindingId>) -> Result<Response<Attachments>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        let kept = arvo_service::research::history::list_attachments(&self.workbench, id).map_err(refused)?;
        Ok(Response::new(Attachments { attachments: kept.into_iter().map(wire::attachment).collect() }))
    }

    async fn list_instruments(&self, _: Request<Empty>) -> Result<Response<Instruments>, Status> {
        let listed = self.research.list_instruments().map_err(Status::internal)?;
        Ok(Response::new(Instruments {
            instruments: listed
                .get("instruments")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| Instrument {
                            id: text(row.get("instrument")),
                            interval: text(row.get("interval")),
                            from: text(row.get("from")),
                            to: text(row.get("to")),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }))
    }

    async fn list_findings(&self, _: Request<Empty>) -> Result<Response<Findings>, Status> {
        let listed = self.research.list_findings().map_err(Status::internal)?;
        Ok(Response::new(Findings {
            findings: listed
                .get("findings")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| FindingSummary {
                            id: text(row.get("id")),
                            kind: text(row.get("kind")),
                            subject: text(row.get("subject")),
                            verdict: text(row.get("verdict")),
                            recorded_at: text(row.get("recorded_at")),
                            author: text(row.get("agent")),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            unreadable: listed
                .get("unreadable")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or_default(),
        }))
    }

    async fn open_finding(&self, request: Request<FindingId>) -> Result<Response<Finding>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        self.research
            .open_finding(id)
            .map(|summary| Response::new(finding(&summary)))
            .map_err(Status::not_found)
    }

    async fn run_study(&self, request: Request<RunRequest>) -> Result<Response<Finding>, Status> {
        self.run(request.into_inner(), false).await
    }

    async fn run_walk_forward(
        &self,
        request: Request<RunRequest>,
    ) -> Result<Response<Finding>, Status> {
        self.run(request.into_inner(), true).await
    }

    async fn attach_file(&self, request: Request<AttachRequest>) -> Result<Response<Attachments>, Status> {
        let request = request.into_inner();
        let id = required(&request.finding_id, "finding_id")?.to_owned();
        let name = required(&request.name, "name")?.to_owned();
        if request.data.is_empty() {
            return Err(Status::invalid_argument("data is empty"));
        }
        let media_type = if request.media_type.trim().is_empty() { "application/octet-stream".to_owned() } else { request.media_type.clone() };
        let research = self.research.clone();
        let kept = tokio::task::spawn_blocking(move || research.attach(&id, &name, &media_type, &request.data))
            .await
            .map_err(|err| Status::internal(format!("the attach did not finish: {err}")))?
            .map_err(Status::not_found)?;
        Ok(Response::new(Attachments { attachments: attachments(Some(&kept)) }))
    }

    async fn record_finding(&self, request: Request<ReportRequest>) -> Result<Response<Finding>, Status> {
        let request = request.into_inner();
        let author = required(&request.author, "author")?.to_owned();
        let origin = Some(request.origin.trim().to_owned()).filter(|origin| !origin.is_empty());
        let (reported, claim) = reported_from(&request).map_err(Status::invalid_argument)?;
        let research = self.research.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let arguments = serde_json::json!({
                "instrument": reported.experiment.instrument,
                "engine": reported.engine,
                "points": reported.strategy_curve.len(),
                "trades": reported.strategy_ledger.len(),
            });
            let outcome = research.record(reported, claim, &author, origin.as_deref());
            research.audit("grpc", &author, "record_finding", &arguments, &outcome);
            outcome
        })
        .await
        .map_err(|err| Status::internal(format!("the record did not finish: {err}")))?;
        outcome
            .map(|summary| Response::new(finding(&summary)))
            .map_err(Status::failed_precondition)
    }
}

/// The wire's report as the contract, or what is wrong with it. Everything
/// ADR-0026 requires is checked here, so a script hears about a missing
/// field by name rather than getting an `Inconclusive` it cannot explain.
fn reported_from(request: &ReportRequest) -> Result<(arvo_research::Reported, String), String> {
    use arvo_research::{trade::Direction, trade::ExitReason, CostModel, DatasetRef, DateRange, Experiment, ExperimentId, HypothesisId, RiskModel, StrategySpec, Trade};
    use chrono::{NaiveDate, NaiveDateTime};

    let text = |value: &str, name: &str| -> Result<String, String> {
        let value = value.trim();
        if value.is_empty() { Err(format!("{name} is required")) } else { Ok(value.to_owned()) }
    };
    let date = |value: &str, name: &str| -> Result<NaiveDate, String> {
        NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").map_err(|_| format!("{name} {value:?} is not a date like 2026-01-31"))
    };
    let instant = |value: &str, name: &str| -> Result<NaiveDateTime, String> {
        NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| date(value, name).map(|day| day.and_hms_opt(0, 0, 0).expect("midnight")))
            .map_err(|_| format!("{name} {value:?} is not a time like 2026-01-31T09:30:00"))
    };
    let points = |points: &[Point], name: &str| -> Result<Vec<arvo_research::EquityPoint>, String> {
        points
            .iter()
            .map(|point| Ok(arvo_research::EquityPoint { at: instant(&point.at, name)?, equity: point.equity }))
            .collect()
    };

    let instrument = text(&request.instrument, "instrument")?;
    let engine = text(&request.engine, "engine")?;
    let hypothesis = text(&request.hypothesis_id, "hypothesis_id")?;
    let strategy = text(&request.strategy, "strategy")?;
    let window = DateRange::new(date(&request.from, "from")?, date(&request.to, "to")?).map_err(|err| err.to_string())?;
    let interval: arvo_data::BarInterval =
        request.interval.parse().map_err(|_| format!("interval {:?} is not one like 5minute or 1day", request.interval))?;
    let adjustment = match request.adjustment.trim() {
        "" | "split" => arvo_data::source::Adjustment::Split,
        "total_return" => arvo_data::source::Adjustment::TotalReturn,
        other => return Err(format!("adjustment {other:?} is neither split nor total_return")),
    };
    let dataset = DatasetRef {
        id: text(&request.dataset_id, "dataset_id")?,
        version: text(&request.dataset_version, "dataset_version")?,
        adjustment,
    };
    let strategy_curve = points(&request.strategy_curve, "strategy_curve")?;
    if strategy_curve.is_empty() {
        return Err("strategy_curve is required".to_owned());
    }
    let benchmark_curve = if request.benchmark_curve.is_empty() { None } else { Some(points(&request.benchmark_curve, "benchmark_curve")?) };
    let ledger = request
        .ledger
        .iter()
        .map(|trade| {
            Ok(Trade {
                instrument: trade.instrument.clone(),
                opened: instant(&trade.opened, "opened")?,
                closed: if trade.closed.trim().is_empty() { None } else { Some(instant(&trade.closed, "closed")?) },
                direction: match trade.direction.trim().to_ascii_lowercase().as_str() {
                    "" | "long" => Direction::Long,
                    "short" => Direction::Short,
                    other => return Err(format!("direction {other:?} is neither long nor short")),
                },
                quantity: trade.quantity,
                entry: trade.entry,
                exit: trade.exit,
                pnl: trade.pnl,
                commission: trade.commission,
                exit_reason: match trade.exit_reason.trim().to_ascii_lowercase().as_str() {
                    "" | "signal" => ExitReason::Signal,
                    "stop" => ExitReason::Stop,
                    "halted" => ExitReason::Halted,
                    "expired" => ExitReason::Expired,
                    "still_open" | "open" => ExitReason::StillOpen,
                    other => return Err(format!("exit_reason {other:?} is not one this knows (signal, stop, halted, expired, still_open)")),
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let experiment = Experiment {
        id: ExperimentId::from(format!("reported-{engine}-{instrument}-{}-{}", window.from, window.to).as_str()),
        hypothesis: HypothesisId::from(hypothesis.as_str()),
        instrument,
        alongside: Vec::new(),
        underlying: None,
        window,
        interval,
        dataset,
        strategy: StrategySpec { name: strategy, params: request.params.iter().map(|(k, v)| (k.clone(), *v)).collect() },
        costs: {
            // The three the wire carries; the rest are what a run assumes
            // when it says nothing, exactly as a study's would.
            let mut costs = CostModel::proportional(0.0, 0.0);
            costs.commission_bps = request.commission_bps;
            costs.slippage_bps = request.slippage_bps;
            costs.per_fill = request.per_fill;
            costs
        },
        risk: RiskModel { stop_atr_multiple: request.stop_atr_multiple, ..RiskModel::default() },
        starting_cash: if request.starting_cash > 0.0 { request.starting_cash } else { strategy_curve[0].equity },
        seed: 0,
    };
    Ok((
        arvo_research::Reported {
            hypothesis: experiment.hypothesis.clone(),
            experiment,
            engine,
            strategy_curve,
            strategy_ledger: ledger,
            benchmark_curve,
            trials: request.trials.map(|trials| trials as usize),
        },
        request.claim.trim().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::proto::research_client::ResearchClient;
    use super::*;

    const TOKEN: &str = "test-token";
    const CONTROL: &str = "control-token";

    /// A served engine over an empty app data directory, and a way to stop it.
    async fn engine() -> (tempfile::TempDir, String, tokio::sync::oneshot::Sender<()>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = format!("http://{}", listener.local_addr().expect("address"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let research = Research::new(dir.path());
        let sessions = std::sync::Arc::new(Sessions::new(dir.path()));
        let tokens = Tokens { research: TOKEN.to_owned(), control: CONTROL.to_owned() };
        tokio::spawn(async move {
            serve(listener, research, sessions, &tokens, async {
                let _ = stopped.await;
            })
            .await
            .expect("serves");
        });
        (dir, address, stop)
    }

    /// The boundary ADR-0016 draws: the research token opens nothing that
    /// reaches an executor.
    #[tokio::test]
    async fn the_research_token_cannot_reach_a_session() {
        let (_dir, address, _stop) = engine().await;
        let mut control = proto::sessions_client::SessionsClient::connect(address).await.expect("connects");
        let refused = control.list_sessions(with_token(Empty {}, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        assert!(refused.message().contains("control.json"), "{}", refused.message());

        let listed = control.list_sessions(with_token(Empty {}, CONTROL)).await.expect("ok").into_inner();
        assert!(listed.sessions.is_empty());
        let refused = control
            .start_session(with_token(StartRequest { finding: "f".to_owned(), executor: "etrade".to_owned() }, CONTROL))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    }

    fn with_token<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().expect("ascii"));
        request
    }

    #[tokio::test]
    async fn a_call_without_the_token_is_refused_and_with_it_is_answered() {
        let (_dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");

        let refused = client.list_strategies(Request::new(Empty {})).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        let wrong = client.list_strategies(with_token(Empty {}, "guess")).await.unwrap_err();
        assert_eq!(wrong.code(), tonic::Code::Unauthenticated);

        let strategies = client
            .list_strategies(with_token(Empty {}, TOKEN))
            .await
            .expect("answered")
            .into_inner()
            .strategies;
        assert!(strategies.iter().any(|plan| plan.name == "sma_cross"), "{strategies:?}");
    }

    #[tokio::test]
    async fn a_ruleset_written_over_the_wire_is_listed_read_back_and_runs_under_the_shipped_risk() {
        let (_dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");

        let rules = client.list_rules(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner().rules;
        let cross = rules.iter().find(|rule| rule.name == "sma_cross").expect("shipped");
        let form = RulesetForm {
            name: "my_cross".to_owned(),
            rule: cross.name.clone(),
            label: "Mine".to_owned(),
            premise: String::new(),
            params: cross
                .fixed
                .iter()
                .map(|fixed| proto::Param { name: fixed.name.clone(), values: vec![fixed.value] })
                .chain(cross.axes.iter().cloned())
                .collect(),
        };
        let written = client.write_ruleset(with_token(form, TOKEN)).await.expect("written").into_inner();
        assert_eq!(written.problem, None, "{written:?}");

        let listed = client.list_rulesets(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner().rulesets;
        assert_eq!(listed.iter().map(|ruleset| ruleset.name.as_str()).collect::<Vec<_>>(), ["my_cross"]);

        let read = client
            .read_ruleset(with_token(RulesetPath { path: written.path.clone() }, TOKEN))
            .await
            .expect("read")
            .into_inner();
        assert_eq!(read.rule, "sma_cross");
        let outside = client.read_ruleset(with_token(RulesetPath { path: "../x.json".to_owned() }, TOKEN)).await;
        assert_eq!(outside.unwrap_err().code(), tonic::Code::InvalidArgument);

        let risk = client.get_risk_model(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        assert!(!risk.exists && risk.error.is_none(), "{risk:?}");
        assert!(risk.model_json.starts_with('{'), "{}", risk.model_json);
    }

    #[tokio::test]
    async fn the_workbench_views_come_over_the_wire_and_a_study_without_bars_is_refused() {
        let (_dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");

        let history = client.view_history(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        assert_eq!(history.kind, "HistoryView");
        let decoded: arvo_service::research::HistoryView = wire::decode(history).expect("decodes");
        assert!(decoded.entries.is_empty() && decoded.unreadable.is_empty());

        let problems = client.view_problems(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        let decoded: Vec<arvo_service::research::ResearchProblemView> = wire::decode(problems).expect("decodes");
        assert!(decoded.is_empty());

        let no_bars = client
            .view_study(with_token(StudyRequest { instrument: "NOPE.YF".to_owned(), strategy: None }, TOKEN))
            .await
            .unwrap_err();
        assert_eq!(no_bars.code(), tonic::Code::InvalidArgument, "{no_bars:?}");
        let no_rule = client
            .view_walk_forward(with_token(
                StudyRequest { instrument: "NOPE.YF".to_owned(), strategy: Some("nothing".to_owned()) },
                TOKEN,
            ))
            .await
            .unwrap_err();
        assert!(no_rule.message().contains("nothing"), "{no_rule:?}");
        let one = client
            .view_book(with_token(
                BookRequest { instruments: vec!["A.YF".to_owned()], strategy: None, max_concurrent_positions: None, max_per_sector: None },
                TOKEN,
            ))
            .await
            .unwrap_err();
        assert!(one.message().contains("two instruments"), "{one:?}");
        let gone = client.view_record(with_token(FindingId { id: "nope".to_owned() }, TOKEN)).await.unwrap_err();
        assert_eq!(gone.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn an_empty_memory_lists_nothing_and_a_missing_finding_is_not_found() {
        let (_dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");
        let findings = client.list_findings(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        assert!(findings.findings.is_empty());
        let instruments =
            client.list_instruments(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        assert!(instruments.instruments.is_empty());

        let missing = client
            .open_finding(with_token(FindingId { id: "nope".to_owned() }, TOKEN))
            .await
            .unwrap_err();
        assert_eq!(missing.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_run_needs_an_author_and_says_why_it_cannot_run() {
        let (dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");
        let anonymous = RunRequest {
            instrument: "AAPL.RH".to_owned(),
            strategy: "sma_cross".to_owned(),
            author: String::new(),
            origin: String::new(),
        };
        let refused = client.run_study(with_token(anonymous, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().contains("author"), "{}", refused.message());

        let no_data = RunRequest {
            instrument: "AAPL.RH".to_owned(),
            strategy: "sma_cross".to_owned(),
            author: "script:test".to_owned(),
            origin: "C:/proj/scan.py:1".to_owned(),
        };
        let failed = client.run_study(with_token(no_data, TOKEN)).await.unwrap_err();
        assert_eq!(failed.code(), tonic::Code::FailedPrecondition);

        let audit = std::fs::read_to_string(dir.path().join(crate::research::AUDIT_FILE)).expect("audited");
        let line: Value = serde_json::from_str(audit.lines().last().expect("a line")).expect("json");
        assert_eq!(line["via"], "grpc");
        assert_eq!(line["agent"], "script:test");
        assert_eq!(line["ok"], serde_json::json!(false));
    }

    /// ADR-0026 over the wire: evidence in, Arvo's verdict out, saved as
    /// the author's finding; what is missing is named; no benchmark is
    /// Inconclusive rather than a guess.
    #[tokio::test]
    async fn a_script_records_evidence_and_arvo_judges_it() {
        let (dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address).await.expect("connects");
        let curve = |step: f64| -> Vec<Point> {
            (0..300u32)
                .map(|n| Point {
                    at: (chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("date") + chrono::Duration::days(i64::from(n)))
                        .format("%Y-%m-%d")
                        .to_string(),
                    equity: 100_000.0 * (1.0 + step).powi(n as i32),
                })
                .collect()
        };
        let ledger: Vec<proto::LedgerTrade> = (0..40u32)
            .map(|n| proto::LedgerTrade {
                opened: format!("2024-{:02}-{:02}", 1 + n / 28, 1 + n % 28),
                closed: format!("2024-{:02}-{:02}T16:00:00", 1 + n / 28, 1 + n % 28),
                entry: 100.0,
                exit: Some(101.0),
                pnl: 1.0,
                ..Default::default()
            })
            .collect();
        let request = |benchmark: bool| ReportRequest {
            author: "script:their-engine".to_owned(),
            origin: "C:/proj/their.py:7".to_owned(),
            hypothesis_id: "h-momentum".to_owned(),
            claim: "momentum persists".to_owned(),
            instrument: "SPY.THEIRS".to_owned(),
            from: "2024-01-01".to_owned(),
            to: "2024-10-26".to_owned(),
            interval: "1day".to_owned(),
            dataset_id: "theirs:SPY".to_owned(),
            dataset_version: "sha256-of-inputs".to_owned(),
            strategy: "rsi2-pullback".to_owned(),
            engine: "their-engine 0.2".to_owned(),
            strategy_curve: curve(0.002),
            ledger: ledger.clone(),
            benchmark_curve: if benchmark { curve(0.0005) } else { Vec::new() },
            trials: Some(12),
            ..Default::default()
        };

        let found = client.record_finding(with_token(request(true), TOKEN)).await.expect("recorded").into_inner();
        let summary = found.summary.expect("a summary");
        assert_eq!(summary.kind, "reported");
        assert_eq!(summary.author, "script:their-engine");
        assert!(["Supported", "NotSupported", "Inconclusive"].contains(&summary.verdict.as_str()), "{}", summary.verdict);
        let detail: Value = serde_json::from_str(&found.detail_json).expect("json");
        assert_eq!(detail["reported"]["engine"], "their-engine 0.2");
        assert_eq!(detail["reported"]["trades"], 40);
        assert_eq!(detail["reported"]["trials"], 12);
        let listed = client.list_findings(with_token(Empty {}, TOKEN)).await.expect("lists").into_inner();
        assert_eq!(listed.findings.len(), 1);
        assert_eq!(listed.findings[0].id, summary.id);

        let alone = client.record_finding(with_token(request(false), TOKEN)).await.expect("recorded").into_inner();
        assert_eq!(alone.summary.expect("a summary").verdict, "Inconclusive");
        assert!(alone.reasons.iter().any(|reason| reason.contains("benchmark")), "{:?}", alone.reasons);

        let mut nameless = request(true);
        nameless.instrument = String::new();
        let refused = client.record_finding(with_token(nameless, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().contains("instrument"), "{}", refused.message());

        let audit = std::fs::read_to_string(dir.path().join(crate::research::AUDIT_FILE)).expect("audited");
        assert!(audit.contains("record_finding"));

        // A file kept with it: listed on the finding, stored once however
        // often the same bytes arrive under the same name.
        let attach = |name: &str, data: &[u8]| AttachRequest {
            finding_id: summary.id.clone(),
            name: name.to_owned(),
            media_type: "text/csv".to_owned(),
            data: data.to_vec(),
        };
        let kept = client.attach_file(with_token(attach("trades.csv", b"a,b\n1,2\n"), TOKEN)).await.expect("kept").into_inner();
        assert_eq!(kept.attachments.len(), 1);
        assert_eq!(kept.attachments[0].bytes, 8);
        let again = client.attach_file(with_token(attach("trades.csv", b"a,b\n1,2\n"), TOKEN)).await.expect("kept").into_inner();
        assert_eq!(again.attachments.len(), 1, "same name, same bytes: one entry");
        let more = client.attach_file(with_token(attach("report.md", b"# ok"), TOKEN)).await.expect("kept").into_inner();
        assert_eq!(more.attachments.len(), 2);
        let opened = client.open_finding(with_token(FindingId { id: summary.id.clone() }, TOKEN)).await.expect("opens").into_inner();
        assert_eq!(opened.attachments.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["trades.csv", "report.md"]);
        let empty = client.attach_file(with_token(attach("empty.txt", b""), TOKEN)).await.unwrap_err();
        assert_eq!(empty.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn nothing_in_the_research_service_can_fetch_share_or_trade() {
        // The boundary is what is offered. A call that named a source, a key
        // or an order would be a way past it no argument check could close.
        let proto = include_str!("../../../protos/arvo/engine/v1/engine.proto");
        // The research service's block alone: the control tier is a second
        // service behind a second token, and its calls are the point of it.
        let research = proto
            .split("service Research {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the research service is declared");
        let calls: Vec<&str> = research
            .lines()
            .filter_map(|line| line.trim().strip_prefix("rpc "))
            .filter_map(|rest| rest.split('(').next())
            .collect();
        // RecordFinding takes evidence in; it fetches nothing, shares nothing
        // and trades nothing, and the type has no field for a verdict.
        // AttachFile keeps bytes with a finding the caller already owns.
        // The ruleset calls read and write files under the project's own
        // rulesets folder, validated by `offerable` before they are written,
        // and GetRiskModel reads the risk file: what a study will run under,
        // never a way to change it. The View calls are the window's renderings
        // of the same runs and the same memory RunStudy and ListFindings
        // reach; ViewBook takes instruments and caps, and its sector labels
        // come from a session this machine already holds, never from the
        // caller.
        assert_eq!(
            calls,
            [
                "ListStrategies", "ListInstruments", "ListFindings", "OpenFinding", "RunStudy", "RunWalkForward",
                "RecordFinding", "AttachFile", "ListRulesets", "ReadRuleset", "WriteRuleset", "ListRules", "GetRiskModel",
                "ViewStudy", "ViewWalkForward", "ViewPanel", "ViewBook", "ViewHistory", "ViewRecord", "ViewReplay",
                "ViewComparison", "ViewProblems", "ListAttachments",
            ]
        );
        for forbidden in ["Fetch", "Order", "Trade", "Share", "Import", "Key", "Sign", "Session", "Halt"] {
            assert!(!calls.iter().any(|call| call.contains(forbidden)), "{forbidden}");
        }
    }
}
