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
    Advice, Empty, Finding, FindingId, FindingSummary, Findings, Instrument, Instruments,
    RunRequest, Strategies, Strategy,
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
        let research = self.research.clone();
        // A study is seconds to a minute of engine work; off the async threads.
        let outcome = tokio::task::spawn_blocking(move || {
            let call = if rolling { "run_walk_forward" } else { "run_study" };
            let outcome = research.run(&instrument, &strategy, rolling, &author);
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
        };
        let refused = client.run_study(with_token(anonymous, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().contains("author"), "{}", refused.message());

        let no_data = RunRequest {
            instrument: "AAPL.RH".to_owned(),
            strategy: "sma_cross".to_owned(),
            author: "script:test".to_owned(),
        };
        let failed = client.run_study(with_token(no_data, TOKEN)).await.unwrap_err();
        assert_eq!(failed.code(), tonic::Code::FailedPrecondition);

        let audit = std::fs::read_to_string(dir.path().join(crate::research::AUDIT_FILE)).expect("audited");
        let line: Value = serde_json::from_str(audit.lines().last().expect("a line")).expect("json");
        assert_eq!(line["via"], "grpc");
        assert_eq!(line["agent"], "script:test");
        assert_eq!(line["ok"], serde_json::json!(false));
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
        assert_eq!(
            calls,
            ["ListStrategies", "ListInstruments", "ListFindings", "OpenFinding", "RunStudy", "RunWalkForward"]
        );
        for forbidden in ["Fetch", "Order", "Trade", "Share", "Import", "Key", "Sign", "Session", "Halt"] {
            assert!(!calls.iter().any(|call| call.contains(forbidden)), "{forbidden}");
        }
    }
}
