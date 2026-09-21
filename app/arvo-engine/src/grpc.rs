//! The research tier over gRPC, behind the engine's token.

use std::future::Future;

use serde_json::Value;
use tokio::net::TcpListener;
use tonic::{Request, Response, Status};

use crate::research::Research;
use crate::session::Sessions;

pub use arvo_client::proto as proto;

use proto::services::research_server::{self, ResearchServer};
use proto::services::accounts_server::{self, AccountsServer};
use proto::services::market_server::{self, MarketServer};
use proto::services::platform_server::{self, PlatformServer};
use proto::services::portfolio_server::{self, PortfolioServer};
use proto::services::research_files_server::{self, ResearchFilesServer};
use proto::services::scripts_server::{self, ScriptsServer};
use proto::services::sessions_server::{self, SessionsServer};
use proto::common::{Empty, ExportedPath};
use proto::market::{
    CompareRequest, DataLibraryView, FetchRequest, FetchView, Instrument, InstrumentSearch, Instruments,
    MatchesView, QuoteTick, SourceComparisonView, SourcesView, WatchlistView,
};
use proto::platform::{
    AccountKeys, AccountsView, ExtensionStrategiesView, JobId, JobsView, PluginsView, RunId, ScriptJobsView,
    ScriptOutputView, ScriptPath, ScriptSchedule, SignIn, SignalsView,
    StrategyContributionsView, VendorId, VendorProfile,
};
use proto::portfolio::{PortfolioLibraryView, PortfolioName};
use proto::research::{
    Advice, AttachRequest, Attachment, AttachmentRef, Attachments, BarView, BarsRequest, BarsView, BookRequest,
    ComparisonView, Finding, RegimePointView, RegimeView,
    FindingId, FindingIds, FindingSummary, Findings, HistoryView, PanelView, Point, ProblemsView, RecordView,
    ReplayView, ReportFigure, ReportRequest, RiskModel, Rules, Ruleset, RulesetForm, RulesetPath, Rulesets,
    RunRequest, SharedExperiment, Strategies, StudyRequest, StudyView, TradeExport, WalkForwardView,
};
use proto::session::{HaltRequest, PromotionView, SessionId, SessionList, SessionStatus, StartRequest};
use arvo_api::EventView;

/// The keys of a summary that have their own fields; everything else is
/// `detail_json`.
const TYPED: &[&str] = &[
    "id", "kind", "subject", "verdict", "recorded_at", "read_this_first", "reasons", "advice", "attachments",
    "strategy", "code_commit", "ruleset_hash",
];

/// The largest message either side accepts: a figure or a trades table
/// attached to a finding, with room. tonic's default is 4 MB.
pub use arvo_client::wire::MAX_MESSAGE_BYTES;

struct Service {
    research: Research,
    /// What `Subscribe` hands out: every event any part of the engine raises.
    events: tokio::sync::broadcast::Sender<EventView>,
    /// The workbench's runs and memory over the same folder: what the window
    /// renders, served here so the window need not run them itself.
    workbench: std::sync::Arc<arvo_service::research::ResearchService>,
}

struct Control {
    sessions: std::sync::Arc<Sessions>,
    /// Fires the server's shutdown future. Taken once: a second `Shutdown`
    /// while the first is in flight is answered and ignored.
    stop: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

/// The two tokens the engine serves behind: research for every front end,
/// control for the ones allowed to reach an executor (ADR-0018 point 4).
/// Everything the three tiers are served over: one bundle, because they are
/// built together in `main` and handed over together.
pub struct Engine {
    pub research: Research,
    pub sessions: std::sync::Arc<Sessions>,
    pub events: tokio::sync::broadcast::Sender<EventView>,
    pub jobs: arvo_schedule::Jobs,
    pub plugins: arvo_service::plugins::Plugins,
    pub stream: arvo_service::stream::Stream,
    pub ticks: tokio::sync::broadcast::Sender<arvo_api::QuoteTick>,
    /// Sent when a front end asks this engine to stop. `serve` waits on the
    /// other half alongside whatever `shutdown` it was given.
    pub stop: tokio::sync::oneshot::Sender<()>,
}

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
    engine: Engine,
    tokens: &Tokens,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let Engine { research, sessions, events, jobs, plugins, stream, ticks, stop } = engine;
    let stop = std::sync::Arc::new(std::sync::Mutex::new(Some(stop)));
    // A person's scripts run here rather than in a window, so a cadence they
    // set is honoured after they close it (ADR-0018). Built before the
    // services because constructing it registers every saved schedule.
    let scripts = arvo_service::scripts::Scripts::new(
        research.root().to_path_buf(),
        arvo_service::project::app_data_root().map_or_else(|_| research.root().to_path_buf(), std::convert::Into::into),
        jobs.clone(),
    );
    let workbench = std::sync::Arc::new(arvo_service::research::ResearchService::new(
        research.root().join(arvo_service::research::DATA_SUBDIR),
        research.root().join(arvo_service::research::EVIDENCE_SUBDIR),
    ));
    let portfolios = std::sync::Arc::new(arvo_service::portfolio::PortfolioService::new(
        research.root().join(arvo_service::portfolio::PORTFOLIO_SUBDIR),
        research.root().join(arvo_service::research::DATA_SUBDIR),
        research.root().join(arvo_service::portfolio::SNAPSHOT_SUBDIR),
    ));
    let research_tier = tonic::service::interceptor::InterceptedService::new(
        ResearchServer::new(Service { research, events: events.clone(), workbench: workbench.clone() })
            .max_decoding_message_size(MAX_MESSAGE_BYTES),
        bearer(&tokens.research, "engine.json"),
    );
    // One interceptor, applied to each service a person drives. The token is a
    // property of the service, not its name: what a service is *about* is its
    // domain, and who may call it is this line.
    let control = || bearer(&tokens.control, "control.json");
    let stream = std::sync::Arc::new(stream);
    let sessions_tier =
        tonic::service::interceptor::InterceptedService::new(SessionsServer::new(Control { sessions, stop }), control());
    let market_tier = tonic::service::interceptor::InterceptedService::new(
        MarketServer::new(Market {
            workbench: workbench.clone(),
            portfolios: portfolios.clone(),
            events: events.clone(),
            stream,
            ticks,
        })
        .max_decoding_message_size(MAX_MESSAGE_BYTES),
        control(),
    );
    let accounts_tier =
        tonic::service::interceptor::InterceptedService::new(AccountsServer::new(Accounts { events }), control());
    let portfolio_tier = tonic::service::interceptor::InterceptedService::new(
        PortfolioServer::new(Portfolio { portfolios }).max_decoding_message_size(MAX_MESSAGE_BYTES),
        control(),
    );
    let platform_tier = tonic::service::interceptor::InterceptedService::new(
        PlatformServer::new(Platform { jobs, plugins }).max_decoding_message_size(MAX_MESSAGE_BYTES),
        control(),
    );
    let files_tier = tonic::service::interceptor::InterceptedService::new(
        ResearchFilesServer::new(ResearchFiles { workbench }).max_decoding_message_size(MAX_MESSAGE_BYTES),
        control(),
    );
    let scripts_tier = tonic::service::interceptor::InterceptedService::new(
        ScriptsServer::new(Scripts { scripts }).max_decoding_message_size(MAX_MESSAGE_BYTES),
        control(),
    );
    tonic::transport::Server::builder()
        .add_service(research_tier)
        .add_service(sessions_tier)
        .add_service(market_tier)
        .add_service(accounts_tier)
        .add_service(portfolio_tier)
        .add_service(platform_tier)
        .add_service(files_tier)
        .add_service(scripts_tier)
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
        frozen: status.frozen,
        reconciled: status.reconciled,
        verdict: status.verdict,
        verdict_reason: status.verdict_reason,
        warnings: status.warnings,
    }
}

/// The data library, the sources that fill it, and prices.
struct Market {
    workbench: std::sync::Arc<arvo_service::research::ResearchService>,
    /// The watchlist starts from what you hold.
    portfolios: std::sync::Arc<arvo_service::portfolio::PortfolioService>,
    events: tokio::sync::broadcast::Sender<EventView>,
    stream: std::sync::Arc<arvo_service::stream::Stream>,
    ticks: tokio::sync::broadcast::Sender<arvo_api::QuoteTick>,
}

impl Market {
    /// Broadcasts whatever a data call wants to announce.
    fn report(&self) -> impl Fn(EventView) + Send + Sync + use<'_> {
        move |event| {
            let _ = self.events.send(event);
        }
    }
}

#[tonic::async_trait]
impl market_server::Market for Market {
    type StreamQuotesStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<QuoteTick, Status>> + Send>>;

    async fn view_library(&self, _: Request<Empty>) -> Result<Response<DataLibraryView>, Status> {
        let view = arvo_service::research::data::library(&self.workbench).map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn list_sources(&self, _: Request<Empty>) -> Result<Response<SourcesView>, Status> {
        let sources = arvo_service::research::data::list_sources().await;
        Ok(Response::new(SourcesView { sources }))
    }

    async fn search_instruments(&self, request: Request<InstrumentSearch>) -> Result<Response<MatchesView>, Status> {
        let InstrumentSearch { query, source } = request.into_inner();
        let view = arvo_service::research::data::fetch::search(&self.workbench, &query, source.as_deref(), &self.report())
            .await
            .map_err(refused)?;
        Ok(Response::new(MatchesView { matches: view }))
    }

    async fn fetch_bars(&self, request: Request<FetchRequest>) -> Result<Response<FetchView>, Status> {
        let FetchRequest { instrument, interval, days, source } = request.into_inner();
        let view = arvo_service::research::data::fetch::bars(
            &self.workbench,
            &instrument,
            &interval,
            days,
            source.as_deref(),
            &self.report(),
        )
        .await
        .map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn compare_sources(&self, request: Request<CompareRequest>) -> Result<Response<SourceComparisonView>, Status> {
        let CompareRequest { instrument, interval, first, second, days } = request.into_inner();
        let view = arvo_service::research::data::fetch::compare_two(
            &instrument,
            &interval,
            first.as_deref(),
            second.as_deref(),
            days,
            &self.report(),
        )
        .await
        .map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn watchlist(&self, _: Request<Empty>) -> Result<Response<WatchlistView>, Status> {
        use arvo_service::research::data::watchlist;

        let held = self.portfolios.held();
        let library = self.workbench.bars.instruments().unwrap_or_default();
        let (chosen, priceable) = watchlist::symbols_for(&held, library);
        // Before the quote call rather than after: if the broker session is
        // dead the snapshot below fails, and the stream is the only thing that
        // can still price these rows.
        self.stream.watch(priceable.iter().map(|id| watchlist::symbol_only(id)).collect());
        let rows = watchlist::priced(&held, chosen, &priceable, &self.report()).await;
        Ok(Response::new(WatchlistView { rows }))
    }

    async fn stream_quotes(&self, _: Request<Empty>) -> Result<Response<Self::StreamQuotesStream>, Status> {
        use tokio_stream::StreamExt as _;

        // A listener that falls behind drops ticks rather than stalling the
        // socket: the next print is worth more than the one it missed.
        let stream = tokio_stream::wrappers::BroadcastStream::new(self.ticks.subscribe())
            .filter_map(|tick| tick.ok())
            .map(Ok);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// A person's relationship with each vendor. The engine holds every
/// credential (ADR-0028).
struct Accounts {
    events: tokio::sync::broadcast::Sender<EventView>,
}

#[tonic::async_trait]
impl accounts_server::Accounts for Accounts {
    async fn list_accounts(&self, _: Request<Empty>) -> Result<Response<AccountsView>, Status> {
        Ok(Response::new(AccountsView { accounts: arvo_service::accounts::list().await }))
    }

    async fn begin_sign_in(&self, request: Request<VendorId>) -> Result<Response<SignIn>, Status> {
        let vendor = required(&request.get_ref().vendor, "vendor")?.to_owned();
        let pending = arvo_service::accounts::begin_sign_in(&vendor).await.map_err(refused)?;
        let url = pending.url.clone();
        // The person takes as long as they take. Finishing happens here rather
        // than in the caller's request, and the outcome is an event
        // (ADR-0028 point 4), so the CLI and the window learn it the same way.
        let events = self.events.clone();
        tokio::spawn(async move {
            let event = match arvo_service::accounts::finish_sign_in(pending).await {
                Ok(()) => arvo_service::events::feed_connected(&vendor),
                Err(err) => {
                    eprintln!("arvo-engine: the {vendor} sign-in did not complete: {err}");
                    arvo_service::events::feed_disconnected(&vendor, &err.to_string(), false)
                }
            };
            let _ = events.send(event);
        });
        Ok(Response::new(SignIn { url }))
    }

    async fn disconnect_account(&self, request: Request<VendorProfile>) -> Result<Response<Empty>, Status> {
        let VendorProfile { vendor, profile } = request.into_inner();
        arvo_service::accounts::disconnect(&vendor, profile.as_deref()).map_err(refused)?;
        let why = if vendor == "alpaca" { "you removed the keys" } else { "you signed out" };
        let _ = self.events.send(arvo_service::events::feed_disconnected(&vendor, why, true));
        Ok(Response::new(Empty {}))
    }

    async fn store_account_keys(&self, request: Request<AccountKeys>) -> Result<Response<Empty>, Status> {
        let AccountKeys { vendor, profile, key_id, secret } = request.into_inner();
        arvo_service::accounts::store_keys(&vendor, profile.as_deref(), &key_id, &secret).map_err(refused)?;
        let _ = self.events.send(arvo_service::events::feed_connected(&vendor));
        Ok(Response::new(Empty {}))
    }
}

/// What is held, valued. Read-only: nothing here places an order.
struct Portfolio {
    portfolios: std::sync::Arc<arvo_service::portfolio::PortfolioService>,
}

#[tonic::async_trait]
impl portfolio_server::Portfolio for Portfolio {
    async fn list_portfolios(&self, _: Request<Empty>) -> Result<Response<PortfolioLibraryView>, Status> {
        let view = arvo_service::portfolio::list(&self.portfolios).map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn sync_accounts(&self, _: Request<Empty>) -> Result<Response<PortfolioLibraryView>, Status> {
        let view = arvo_service::portfolio::sync_accounts(&self.portfolios).await.map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn sync_portfolio(&self, request: Request<PortfolioName>) -> Result<Response<PortfolioLibraryView>, Status> {
        let name = required(&request.get_ref().name, "name")?;
        let view = arvo_service::portfolio::sync_one(&self.portfolios, name).await.map_err(refused)?;
        Ok(Response::new(view))
    }
}

/// The engine's own jobs, and the plugins it hosts (ADR-0029).
struct Platform {
    jobs: arvo_schedule::Jobs,
    plugins: arvo_service::plugins::Plugins,
}

#[tonic::async_trait]
impl platform_server::Platform for Platform {
    async fn list_jobs(&self, _: Request<Empty>) -> Result<Response<JobsView>, Status> {
        Ok(Response::new(JobsView { jobs: self.jobs.snapshot() }))
    }

    async fn run_job(&self, request: Request<JobId>) -> Result<Response<Empty>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        if !self.jobs.snapshot().iter().any(|job| job.id == id) {
            return Err(Status::not_found(format!("the engine has no job {id:?}")));
        }
        self.jobs.run_now(id.to_owned());
        Ok(Response::new(Empty {}))
    }

    async fn list_plugins(&self, _: Request<Empty>) -> Result<Response<PluginsView>, Status> {
        Ok(Response::new(PluginsView { plugins: self.plugins.snapshot().await }))
    }

    async fn refresh_plugins(&self, _: Request<Empty>) -> Result<Response<PluginsView>, Status> {
        Ok(Response::new(PluginsView { plugins: self.plugins.refresh().await }))
    }

    async fn list_signals(&self, _: Request<Empty>) -> Result<Response<SignalsView>, Status> {
        Ok(Response::new(SignalsView { signals: self.plugins.signals().await }))
    }

    async fn reconcile_providers(&self, _: Request<Empty>) -> Result<Response<Empty>, Status> {
        self.plugins.reconcile().await;
        Ok(Response::new(Empty {}))
    }

    async fn extension_strategies(&self, _: Request<Empty>) -> Result<Response<ExtensionStrategiesView>, Status> {
        let by_extension = arvo_service::extensions::contributed_views()
            .into_iter()
            .map(|(extension, contributions)| (extension, StrategyContributionsView { contributions }))
            .collect();
        Ok(Response::new(ExtensionStrategiesView { by_extension }))
    }
}

/// A person's own scripts: running one, and the cadences they set.
///
/// Here rather than in a window because a schedule that only fires while
/// someone is watching is not a schedule (ADR-0018).
struct Scripts {
    scripts: std::sync::Arc<arvo_service::scripts::Scripts>,
}

#[tonic::async_trait]
impl scripts_server::Scripts for Scripts {
    type WatchOutputStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<ScriptOutputView, Status>> + Send>>;

    async fn run_script(&self, request: Request<ScriptPath>) -> Result<Response<RunId>, Status> {
        let path = required(&request.get_ref().path, "path")?.to_owned();
        let run = self.scripts.start(&path).map_err(refused)?;
        Ok(Response::new(RunId { run }))
    }

    async fn stop_script(&self, request: Request<RunId>) -> Result<Response<Empty>, Status> {
        self.scripts.stop(request.get_ref().run);
        Ok(Response::new(Empty {}))
    }

    async fn watch_output(&self, _: Request<Empty>) -> Result<Response<Self::WatchOutputStream>, Status> {
        use tokio_stream::StreamExt as _;

        // A reader that falls behind drops lines rather than stalling the
        // script: a run is not for the watchers' benefit.
        let stream = tokio_stream::wrappers::BroadcastStream::new(self.scripts.watch())
            .filter_map(|line| line.ok())
            .map(Ok::<_, Status>);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_script_jobs(&self, _: Request<Empty>) -> Result<Response<ScriptJobsView>, Status> {
        Ok(Response::new(ScriptJobsView { jobs: self.scripts.saved() }))
    }

    async fn save_script_job(&self, request: Request<ScriptSchedule>) -> Result<Response<ScriptJobsView>, Status> {
        let ScriptSchedule { script, every_secs, cron, enabled } = request.into_inner();
        let jobs = self.scripts.save(script, every_secs, cron, enabled).map_err(refused)?;
        Ok(Response::new(ScriptJobsView { jobs }))
    }

    async fn remove_script_job(&self, request: Request<ScriptPath>) -> Result<Response<ScriptJobsView>, Status> {
        let jobs = self.scripts.remove(&request.get_ref().path).map_err(refused)?;
        Ok(Response::new(ScriptJobsView { jobs }))
    }
}

/// The research files a person asks for: an experiment someone sent, and
/// what a finding turns into on disk.
struct ResearchFiles {
    workbench: std::sync::Arc<arvo_service::research::ResearchService>,
}

#[tonic::async_trait]
impl research_files_server::ResearchFiles for ResearchFiles {
    async fn run_shared_experiment(&self, request: Request<SharedExperiment>) -> Result<Response<StudyView>, Status> {
        let SharedExperiment { text, instrument } = request.into_inner();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_shared(workbench, &text, &instrument)
        })
        .await?;
        Ok(Response::new(view))
    }

    async fn export_trades(&self, request: Request<TradeExport>) -> Result<Response<ExportedPath>, Status> {
        let TradeExport { name, rows } = request.into_inner();
        let path = arvo_service::research::history::export_trades(&self.workbench, &name, &rows).map_err(refused)?;
        Ok(Response::new(ExportedPath { path }))
    }

    async fn export_experiment(&self, request: Request<FindingId>) -> Result<Response<ExportedPath>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        let path = arvo_service::research::history::export_experiment(&self.workbench, id).map_err(refused)?;
        Ok(Response::new(ExportedPath { path }))
    }

    async fn compose_report(&self, request: Request<ReportFigure>) -> Result<Response<ExportedPath>, Status> {
        let ReportFigure { finding_id, figure } = request.into_inner();
        let id = required(&finding_id, "finding_id")?;
        let path = arvo_service::research::history::compose_report(&self.workbench, id, figure.as_deref())
            .map_err(refused)?;
        Ok(Response::new(ExportedPath { path }))
    }

    async fn attachment_path(&self, request: Request<AttachmentRef>) -> Result<Response<ExportedPath>, Status> {
        let AttachmentRef { finding_id, hash } = request.into_inner();
        let path = arvo_service::research::history::attachment_path(
            &self.workbench,
            required(&finding_id, "finding_id")?,
            required(&hash, "hash")?,
        )
        .map_err(refused)?;
        Ok(Response::new(ExportedPath { path }))
    }
}


#[tonic::async_trait]
impl sessions_server::Sessions for Control {
    async fn shutdown(&self, _: Request<Empty>) -> Result<Response<Empty>, Status> {
        // Answered first, then acted on: the caller needs the reply before the
        // transport goes away.
        if let Ok(mut stop) = self.stop.lock() {
            if let Some(stop) = stop.take() {
                let _ = stop.send(());
            }
        }
        Ok(Response::new(Empty {}))
    }

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

    async fn reconcile_session(&self, request: Request<SessionId>) -> Result<Response<SessionStatus>, Status> {
        let id = request.into_inner().id;
        let sessions = self.sessions.clone();
        // Waits for the session loop to take the command, up to a pause step.
        tokio::task::spawn_blocking(move || sessions.reconcile(&id))
            .await
            .map_err(|err| Status::internal(err.to_string()))?
            .map(|status| Response::new(session_status(status)))
            .map_err(Status::failed_precondition)
    }

    async fn resume_session(&self, request: Request<SessionId>) -> Result<Response<SessionStatus>, Status> {
        let id = request.into_inner().id;
        let sessions = self.sessions.clone();
        tokio::task::spawn_blocking(move || sessions.resume(&id))
            .await
            .map_err(|err| Status::internal(err.to_string()))?
            .map(|status| Response::new(session_status(status)))
            .map_err(Status::failed_precondition)
    }

    async fn halt_session(&self, request: Request<HaltRequest>) -> Result<Response<SessionStatus>, Status> {
        let request = request.into_inner();
        let sessions = self.sessions.clone();
        // Waits for the exits to be sent, up to a pause step.
        tokio::task::spawn_blocking(move || sessions.halt(&request.id, &request.reason))
            .await
            .map_err(|err| Status::internal(err.to_string()))?
            .map(|status| Response::new(session_status(status)))
            .map_err(Status::failed_precondition)
    }

    async fn check_promotion(&self, request: Request<StartRequest>) -> Result<Response<PromotionView>, Status> {
        let request = request.into_inner();
        let finding = required(&request.finding, "finding")?;
        let executor = required(&request.executor, "executor")?;
        let gate = self.sessions.promotion(finding, executor).map_err(Status::invalid_argument)?;
        Ok(Response::new(PromotionView {
            allowed: gate.allowed,
            reasons: gate.reasons,
            verdict: gate.verdict.unwrap_or_default(),
            paper_days: gate.paper_days,
            paper_verdict: gate.paper_verdict,
        }))
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

const DEFAULT_BARS: u32 = 60;
const MAX_BARS: u32 = 2000;

/// The library's bars a [`BarsRequest`] asks for: the instrument as given,
/// the interval it parsed to, and at most `last` bars from the end of the
/// window (#195).
fn bars_in(data: &std::path::Path, asked: &BarsRequest) -> Result<(String, arvo_data::BarInterval, Vec<arvo_data::Bar>), Status> {
    use arvo_data::BarProvider as _;
    let instrument = required(&asked.instrument, "instrument")?.to_owned();
    let interval: arvo_data::BarInterval = asked
        .interval
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("1day")
        .parse()
        .map_err(|err| Status::invalid_argument(format!("interval: {err}")))?;
    let day = |field: &Option<String>, name: &str, or: chrono::NaiveDate| -> Result<chrono::NaiveDate, Status> {
        match field.as_deref().filter(|text| !text.trim().is_empty()) {
            Some(text) => text
                .trim()
                .parse()
                .map_err(|err| Status::invalid_argument(format!("{name} is YYYY-MM-DD: {err}"))),
            None => Ok(or),
        }
    };
    let from = day(&asked.from, "from", chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default())?;
    let to = day(&asked.to, "to", chrono::Utc::now().date_naive())?;
    let mut bars = arvo_data::CsvBars::new(data)
        .bars(&instrument, interval, from, to)
        .map_err(|err| Status::not_found(format!("{instrument} at {interval}: {err}")))?;
    if bars.is_empty() {
        return Err(Status::not_found(format!("{instrument} has no {interval} bars between {from} and {to}")));
    }
    let keep = asked.last.unwrap_or(DEFAULT_BARS).min(MAX_BARS) as usize;
    let skip = bars.len().saturating_sub(keep);
    bars.drain(..skip);
    Ok((instrument, interval, bars))
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
            strategy: text(summary.get("strategy")),
            code_commit: text(summary.get("code_commit")),
            ruleset_hash: text(summary.get("ruleset_hash")),
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
    type SubscribeStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<EventView, Status>> + Send>>;

    async fn subscribe(&self, _: Request<Empty>) -> Result<Response<Self::SubscribeStream>, Status> {
        use tokio_stream::StreamExt as _;
        // A receiver that lagged gets the events after the gap, not an error:
        // what it missed is in the session record.
        let stream = tokio_stream::wrappers::BroadcastStream::new(self.events.subscribe())
            .filter_map(|item| item.ok())
            .map(Ok);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_strategies(&self, _: Request<Empty>) -> Result<Response<Strategies>, Status> {
        let strategies = arvo_service::research::list_strategies(self.research.root())
            .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(Strategies { strategies }))
    }

    async fn list_rulesets(&self, _: Request<Empty>) -> Result<Response<Rulesets>, Status> {
        let rulesets = arvo_service::rulesets::list(self.research.root());
        Ok(Response::new(Rulesets { rulesets }))
    }

    async fn read_ruleset(&self, request: Request<RulesetPath>) -> Result<Response<RulesetForm>, Status> {
        let path = required(&request.get_ref().path, "path")?;
        arvo_service::rulesets::read_form(self.research.root(), path)
            .map(Response::new)
            .map_err(Status::invalid_argument)
    }

    async fn write_ruleset(&self, request: Request<RulesetForm>) -> Result<Response<Ruleset>, Status> {
        arvo_service::rulesets::write_form(self.research.root(), request.into_inner())
            .map(Response::new)
            .map_err(Status::invalid_argument)
    }

    async fn list_rules(&self, _: Request<Empty>) -> Result<Response<Rules>, Status> {
        Ok(Response::new(Rules { rules: arvo_service::rulesets::list_rules() }))
    }

    async fn get_risk_model(&self, _: Request<Empty>) -> Result<Response<RiskModel>, Status> {
        Ok(Response::new(arvo_service::risk::view(self.research.root())))
    }

    async fn view_study(&self, request: Request<StudyRequest>) -> Result<Response<StudyView>, Status> {
        let StudyRequest { instrument, strategy } = request.into_inner();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_study(workbench, &instrument, strategy.as_deref())
        })
        .await?;
        Ok(Response::new(view))
    }

    async fn view_walk_forward(&self, request: Request<StudyRequest>) -> Result<Response<WalkForwardView>, Status> {
        let StudyRequest { instrument, strategy } = request.into_inner();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::study::run_walk_forward(workbench, &instrument, strategy.as_deref())
        })
        .await?;
        Ok(Response::new(view))
    }

    async fn view_panel(&self, _: Request<Empty>) -> Result<Response<PanelView>, Status> {
        let view = blocking(&self.workbench, arvo_service::research::study::run_panel).await?;
        Ok(Response::new(view))
    }

    async fn view_book(&self, request: Request<BookRequest>) -> Result<Response<StudyView>, Status> {
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
        Ok(Response::new(view))
    }

    async fn view_history(&self, _: Request<Empty>) -> Result<Response<HistoryView>, Status> {
        let view = arvo_service::research::history::list_history(&self.workbench).map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn view_record(&self, request: Request<FindingId>) -> Result<Response<RecordView>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        let view = arvo_service::research::history::open_record(&self.workbench, id).map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn view_replay(&self, request: Request<FindingId>) -> Result<Response<ReplayView>, Status> {
        let id = required(&request.get_ref().id, "id")?.to_owned();
        let view = blocking(&self.workbench, move |workbench| {
            arvo_service::research::history::replay_record(workbench, &id)
        })
        .await?;
        Ok(Response::new(view))
    }

    async fn view_comparison(&self, request: Request<FindingIds>) -> Result<Response<ComparisonView>, Status> {
        let view = arvo_service::research::history::compare_records(&self.workbench, &request.get_ref().ids)
            .map_err(refused)?;
        Ok(Response::new(view))
    }

    async fn read_bars(&self, request: Request<BarsRequest>) -> Result<Response<BarsView>, Status> {
        let (instrument, interval, bars) = bars_in(self.research.data(), request.get_ref())?;
        Ok(Response::new(BarsView {
            instrument,
            interval: interval.to_string(),
            bars: bars
                .into_iter()
                .map(|bar| BarView {
                    at: bar.at.to_string(),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                })
                .collect(),
        }))
    }

    async fn view_regime(&self, request: Request<BarsRequest>) -> Result<Response<RegimeView>, Status> {
        use arvo_research::regime::{label, LOOKBACK};
        // Labelled over the window plus the lookback before it, so the first
        // bar asked for has a label rather than the first twenty saying
        // nothing; then only the window is answered.
        let asked = request.get_ref();
        let wider = BarsRequest { last: asked.last.map(|last| last.saturating_add(LOOKBACK as u32)), ..asked.clone() };
        let (instrument, interval, bars) = bars_in(self.research.data(), &wider)?;
        let keep = asked.last.unwrap_or(DEFAULT_BARS).min(MAX_BARS) as usize;
        let curve: Vec<arvo_research::EquityPoint> =
            bars.iter().map(|bar| arvo_research::EquityPoint { at: bar.at, equity: bar.close }).collect();
        let labels = label(&curve, LOOKBACK);
        let skip = bars.len().saturating_sub(keep);
        let points: Vec<RegimePointView> = bars
            .iter()
            .zip(labels)
            .skip(skip)
            .map(|(bar, regime)| RegimePointView { at: bar.at.to_string(), regime: regime.map(|regime| regime.label().to_owned()) })
            .collect();
        let mut shares = std::collections::HashMap::new();
        for point in &points {
            if let Some(regime) = &point.regime {
                *shares.entry(regime.clone()).or_insert(0) += 1;
            }
        }
        Ok(Response::new(RegimeView {
            instrument,
            interval: interval.to_string(),
            lookback: LOOKBACK as u32,
            current: points.last().and_then(|point| point.regime.clone()),
            points,
            shares,
        }))
    }

    async fn view_problems(&self, _: Request<Empty>) -> Result<Response<ProblemsView>, Status> {
        let view = arvo_service::research::history::list_research_problems(&self.workbench, self.research.root())
            .map_err(refused)?;
        Ok(Response::new(ProblemsView { problems: view }))
    }

    async fn list_attachments(&self, request: Request<FindingId>) -> Result<Response<Attachments>, Status> {
        let id = required(&request.get_ref().id, "id")?;
        let kept = arvo_service::research::history::list_attachments(&self.workbench, id).map_err(refused)?;
        Ok(Response::new(Attachments { attachments: kept }))
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
                            strategy: text(row.get("strategy")),
                            code_commit: text(row.get("code_commit")),
                            ruleset_hash: text(row.get("ruleset_hash")),
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
                // A reported run may say what its rule saw (#190); the rule
                // and the value together, or nothing.
                journal: match (&trade.rule, trade.signal) {
                    (Some(rule), Some(signal)) => Some(arvo_research::Journal {
                        rule: rule.clone(),
                        signal,
                        regime: trade.regime.clone(),
                        asked: trade.asked.unwrap_or(trade.quantity),
                    }),
                    _ => None,
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
    use super::proto::services::research_client::ResearchClient;
    use super::*;
    use arvo_api::{EventKindView, SeverityView};

    const TOKEN: &str = "test-token";
    const CONTROL: &str = "control-token";

    /// A served engine over an empty app data directory, and a way to stop it.
    async fn engine() -> (tempfile::TempDir, String, tokio::sync::oneshot::Sender<()>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = format!("http://{}", listener.local_addr().expect("address"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let research = Research::new(dir.path());
        let events = tokio::sync::broadcast::channel(256).0;
        let sessions = std::sync::Arc::new(Sessions::new(dir.path(), events.clone()));
        let tokens = Tokens { research: TOKEN.to_owned(), control: CONTROL.to_owned() };
        // The path, not the handle: `dir` is returned to the test so the
        // directory outlives the server.
        let root = dir.path().to_path_buf();
        tokio::spawn(async move {
            let jobs = arvo_schedule::Jobs::new(std::sync::Arc::new(|future| {
                let handle = tokio::spawn(future);
                Box::new(move || handle.abort())
            }));
            // No plugins.toml in the temporary directory, so this is a registry
            // over nothing — which is what these tests want.
            let plugins = arvo_service::plugins::Plugins::start(&root, &jobs, |_| {}).await;
            let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
            let ticks = tokio::sync::broadcast::channel(16).0;
            let (stream, streaming) = arvo_service::stream::start(ticks.clone(), |_| {});
            // Driven, so the handle is live; with no symbols it connects and
            // waits, and the test never asks for a watchlist.
            tokio::spawn(streaming);
            let engine = Engine { research, sessions, events, jobs, plugins, stream, ticks, stop: stop_tx };
            serve(listener, engine, &tokens, async {
                let _ = stopped.await;
            })
            .await
            .expect("serves");
        });
        (dir, address, stop)
    }

    /// Stopping the engine is a control action, and it exists because a
    /// killed process runs no destructors: the window asks rather than kills,
    /// so the providers the engine supervises stop with it (ADR-0029).
    #[tokio::test]
    async fn only_the_control_token_can_stop_the_engine() {
        let (_dir, address, _stop) = engine().await;
        let mut control = super::proto::services::sessions_client::SessionsClient::connect(address).await.expect("connects");
        let refused = control.shutdown(with_token(Empty {}, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        control.shutdown(with_token(Empty {}, CONTROL)).await.expect("asked");
    }

    /// The boundary ADR-0016 draws: the research token opens nothing that
    /// reaches an executor.
    #[tokio::test]
    async fn the_research_token_cannot_reach_a_session() {
        let (_dir, address, _stop) = engine().await;
        let mut control = proto::services::sessions_client::SessionsClient::connect(address).await.expect("connects");
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
                .map(|fixed| proto::research::Param { name: fixed.name.clone(), values: vec![fixed.value] })
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
        assert!(history.entries.is_empty() && history.unreadable.is_empty(), "{history:?}");

        let problems = client.view_problems(with_token(Empty {}, TOKEN)).await.expect("ok").into_inner();
        assert!(problems.problems.is_empty(), "{problems:?}");

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
    async fn the_research_token_cannot_fetch_and_the_control_token_reads_the_library() {
        let (_dir, address, _stop) = engine().await;
        let address2 = address.clone();
        let mut data = super::proto::services::market_client::MarketClient::connect(address).await.expect("connects");

        // ADR-0016: fetching changes the library and stales findings, so it is
        // a person's decision. An agent holds engine.json and nothing else.
        let refused = data.view_library(with_token(Empty {}, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        let refused = data
            .fetch_bars(with_token(
                FetchRequest { instrument: "AAPL.YF".to_owned(), interval: "1d".to_owned(), days: None, source: None },
                TOKEN,
            ))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);

        let library = data.view_library(with_token(Empty {}, CONTROL)).await.expect("ok").into_inner();
        assert!(library.instruments.is_empty(), "{library:?}");

        let sources = data.list_sources(with_token(Empty {}, CONTROL)).await.expect("ok").into_inner();
        assert!(sources.sources.iter().any(|source| source.venue == "YF"), "{sources:?}");

        // A credential is the engine's alone (ADR-0028), so it sits behind the
        // control token like every other service a person drives.
        let mut accounts =
            super::proto::services::accounts_client::AccountsClient::connect(address2).await.expect("connects");
        let refused = accounts.list_accounts(with_token(Empty {}, TOKEN)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        let listed = accounts.list_accounts(with_token(Empty {}, CONTROL)).await.expect("ok").into_inner();
        assert!(listed.accounts.iter().any(|account| account.id == "alpaca"), "{listed:?}");

        // Refused before any browser could open, so this stays offline.
        let refused = accounts
            .begin_sign_in(with_token(VendorId { vendor: "yahoo".to_owned() }, CONTROL))
            .await
            .unwrap_err();
        assert!(refused.message().contains("no sign-in"), "{refused:?}");

        // An empty query asks no vendor anything, so this stays offline.
        let found = data
            .search_instruments(with_token(InstrumentSearch { query: "  ".to_owned(), source: None }, CONTROL))
            .await
            .expect("ok")
            .into_inner();
        assert!(found.matches.is_empty(), "{found:?}");
    }

    #[tokio::test]
    async fn a_session_that_fails_is_announced_to_a_subscriber() {
        use tokio_stream::StreamExt as _;
        let (_dir, address, _stop) = engine().await;
        let mut client = ResearchClient::connect(address.clone()).await.expect("connects");
        let mut events = client.subscribe(with_token(Empty {}, TOKEN)).await.expect("subscribed").into_inner();

        let mut control = super::proto::services::sessions_client::SessionsClient::connect(address).await.expect("connects");
        // A finding that does not exist: the session thread fails before it
        // builds an executor, so nothing here reaches a venue.
        control
            .start_session(with_token(StartRequest { finding: "nope".to_owned(), executor: "alpaca-paper".to_owned() }, CONTROL))
            .await
            .expect("starts");

        let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
            .await
            .expect("an event within ten seconds")
            .expect("the stream is open")
            .expect("ok");
        assert_eq!(
            first.kind,
            Some(EventKindView::session("nope@alpaca-paper".to_owned(), "failed".to_owned())),
            "{first:?}"
        );
        assert_eq!(first.severity(), SeverityView::Warning);
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
        let ledger: Vec<proto::research::LedgerTrade> = (0..40u32)
            .map(|n| proto::research::LedgerTrade {
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
        // Each service is its own file in the contract now, which is the
        // boundary this asserts made visible.
        let proto = include_str!("../../../contract/protos/arvo/services/v1/research.proto");
        let market = include_str!("../../../contract/protos/arvo/services/v1/market.proto");
        // Each service is its own file, so where a call lives is where it is
        // written down. Fetching changes the library and stales findings; a
        // shared experiment is a file somebody sent. Both are a person's
        // decision, and both are declared away from `Research`.
        let block = |text: &'static str, name: &str| -> &'static str {
            text.split(&format!("service {name} {{"))
                .nth(1)
                .and_then(|rest| rest.split("
}").next())
                .unwrap_or_else(|| panic!("{name} is declared"))
        };
        assert!(block(market, "Market").contains("rpc FetchBars"), "fetching is not the research tier's");
        assert!(
            block(proto, "ResearchFiles").contains("rpc RunSharedExperiment"),
            "importing is not the research tier's"
        );
        let research = block(proto, "Research");
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
        // caller. Subscribe only listens. ReadBars and ViewRegime read the
        // library the studies read, and nothing else: an agent that can run
        // a study on those bars can look at them (#195).
        assert_eq!(
            calls,
            [
                "ListStrategies", "ListInstruments", "ListFindings", "OpenFinding", "RunStudy", "RunWalkForward",
                "RecordFinding", "AttachFile", "ListRulesets", "ReadRuleset", "WriteRuleset", "ListRules", "GetRiskModel",
                "ViewStudy", "ViewWalkForward", "ViewPanel", "ViewBook", "ViewHistory", "ViewRecord", "ViewReplay",
                "ViewComparison", "ReadBars", "ViewRegime", "ViewProblems", "ListAttachments", "Subscribe",
            ]
        );
        for forbidden in ["Fetch", "Order", "Trade", "Share", "Import", "Key", "Sign", "Session", "Halt"] {
            assert!(!calls.iter().any(|call| call.contains(forbidden)), "{forbidden}");
        }
    }
}
