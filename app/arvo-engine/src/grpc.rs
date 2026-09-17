//! The research tier over gRPC, behind the engine's token.

use std::future::Future;

use serde_json::Value;
use tokio::net::TcpListener;
use tonic::{Request, Response, Status};

use crate::research::Research;

pub mod proto {
    tonic::include_proto!("arvo.engine.v1");
}

use proto::research_server::{self, ResearchServer};
use proto::{
    Advice, Empty, Finding, FindingId, FindingSummary, Findings, Instrument, Instruments, Point,
    ReportRequest, RunRequest, Strategies, Strategy,
};

/// The keys of a summary that have their own fields; everything else is
/// `detail_json`.
const TYPED: &[&str] = &[
    "id", "kind", "subject", "verdict", "recorded_at", "read_this_first", "reasons", "advice",
];

struct Service {
    research: Research,
}

/// Serves the research tier on `listener` until `shutdown` resolves.
///
/// Every call must carry `authorization: Bearer <token>`. The engine's own
/// listener is loopback-only; the token is what stops another local program
/// that has not read the user's `engine.json`.
///
/// # Errors
///
/// When the transport fails.
pub async fn serve(
    listener: TcpListener,
    research: Research,
    token: &str,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let expected = format!("Bearer {token}");
    let service = ResearchServer::with_interceptor(Service { research }, move |request: Request<()>| {
        match request.metadata().get("authorization").and_then(|value| value.to_str().ok()) {
            Some(given) if given == expected => Ok(request),
            _ => Err(Status::unauthenticated(
                "missing or wrong token; read it from engine.json in the Arvo app data directory",
            )),
        }
    });
    tonic::transport::Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), shutdown)
        .await
}

/// A JSON value as text: a string as itself, null as empty, anything else as
/// its JSON.
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
        detail_json: Value::Object(detail).to_string(),
    }
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
        let plans = arvo_runtime_lib::research::list_strategies()
            .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(Strategies {
            strategies: plans
                .into_iter()
                .map(|plan| Strategy {
                    ranks_a_set: arvo_runtime_lib::research::StrategyPlan::find(&plan.name)
                        .is_some_and(|found| found.ranks_a_set()),
                    name: plan.name,
                    label: plan.label,
                    interval: plan.interval,
                    premise: plan.premise,
                })
                .collect(),
        }))
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

    /// A served engine over an empty app data directory, and a way to stop it.
    async fn engine() -> (tempfile::TempDir, String, tokio::sync::oneshot::Sender<()>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = format!("http://{}", listener.local_addr().expect("address"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let research = Research::new(dir.path());
        tokio::spawn(async move {
            serve(listener, research, TOKEN, async {
                let _ = stopped.await;
            })
            .await
            .expect("serves");
        });
        (dir, address, stop)
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
    }

    #[test]
    fn nothing_in_the_research_service_can_fetch_share_or_trade() {
        // The boundary is what is offered. A call that named a source, a key
        // or an order would be a way past it no argument check could close.
        let proto = include_str!("../../../protos/arvo/engine/v1/engine.proto");
        let calls: Vec<&str> = proto
            .lines()
            .filter_map(|line| line.trim().strip_prefix("rpc "))
            .filter_map(|rest| rest.split('(').next())
            .collect();
        // RecordFinding takes evidence in; it fetches nothing, shares nothing
        // and trades nothing, and the type has no field for a verdict.
        assert_eq!(
            calls,
            ["ListStrategies", "ListInstruments", "ListFindings", "OpenFinding", "RunStudy", "RunWalkForward", "RecordFinding"]
        );
        for forbidden in ["Fetch", "Order", "Trade", "Share", "Import", "Key", "Sign", "Session", "Halt"] {
            assert!(!calls.iter().any(|call| call.contains(forbidden)), "{forbidden}");
        }
    }
}
