//! The Arvo engine (ADR-0018).
//!
//!     arvo-engine [<dir>]
//!     arvo-engine session list
//!     arvo-engine session start <finding> <alpaca-paper|alpaca-live|robinhood-1234>
//!     arvo-engine session stop <id>
//!
//! The `session` form is a client of the running engine, over its control
//! token (`control.json`); the rest starts one.
//!
//! Serves the research tier on a free loopback port and writes the address
//! and a fresh token to `engine.json` in the app data directory, where the
//! window and the Python client look for it. Data — the library, the
//! evidence store — is read from the open project folder. With `<dir>`, both
//! are that directory: a test or a script running over a folder of its own.
//! A second engine for the same directory finds the first answering and
//! leaves it running. Ctrl-C stops it and removes the file.

use std::path::PathBuf;

use arvo_engine::{discovery, grpc, research, session};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("arvo-engine: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("session") {
        return session_command(&args[1..]).await;
    }
    let (root, data) = match args.first() {
        Some(dir) => (PathBuf::from(&dir), PathBuf::from(dir)),
        None => (
            arvo_service::project::app_data_root().map_err(|err| format!("no app data directory: {err}"))?,
            research::default_root()?,
        ),
    };

    if let Some(found) = discovery::running(&root) {
        eprintln!(
            "arvo-engine: already running at {} (pid {}); leaving it",
            found.address, found.pid
        );
        return Ok(());
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("binding a loopback port: {err}"))?;
    let address = listener
        .local_addr()
        .map_err(|err| format!("reading the bound address: {err}"))?;
    let tokens = grpc::Tokens { research: discovery::new_token(), control: discovery::new_token() };
    let pid = std::process::id();
    discovery::write(&root, &discovery::Discovery { address, token: tokens.research.clone(), pid })
        .map_err(|err| format!("writing {}: {err}", discovery::FILE))?;
    discovery::write_control(&root, &tokens.control)
        .map_err(|err| format!("writing {}: {err}", discovery::CONTROL_FILE))?;
    eprintln!("arvo-engine: serving research and sessions on {address} for {}", data.display());

    // One hub for everything the engine raises, shared by the sessions and the
    // data tier and handed out by Subscribe (#150).
    let events = tokio::sync::broadcast::channel(256).0;
    let sessions = std::sync::Arc::new(session::Sessions::new(&data, events.clone()));
    // The engine's own jobs, on its own runtime: this is `#[tokio::main]`, so
    // `tokio::spawn` is what puts a loop on a reactor here.
    let jobs = arvo_service::scheduler::Jobs::new(std::sync::Arc::new(|future| {
        let handle = tokio::spawn(future);
        Box::new(move || handle.abort())
    }));
    {
        let research = std::sync::Arc::new(arvo_service::research::ResearchService::new(
            data.join(arvo_service::research::DATA_SUBDIR),
            data.join(arvo_service::research::EVIDENCE_SUBDIR),
        ));
        let raising = events.clone();
        arvo_service::jobs::register(&jobs, &data, research, move |event| {
            let _ = raising.send(event);
        });
    }
    // The plugins, and what they serve, in the process that outlives the
    // window (ADR-0029). `root` is the app data directory, where plugins.toml
    // and the extensions folder live.
    let plugins = {
        let raising = events.clone();
        arvo_service::plugins::Plugins::start(&root, &jobs, move |event| {
            let _ = raising.send(event);
        })
        .await
    };
    let stopping = plugins.clone();
    // Two ways to stop: a signal, or a front end asking over the control tier.
    // The second exists because a killed process runs no destructors, so the
    // providers this supervises would outlive it (ADR-0029).
    let (stop, asked) = tokio::sync::oneshot::channel();
    let engine = grpc::Engine { research: research::Research::new(&data), sessions, events, jobs, plugins, stop };
    let served = grpc::serve(listener, engine, &tokens, async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = asked => {}
        }
    })
    .await;
    // Nothing this started lingers (ADR-0023 point 5), now that this is what
    // started it.
    stopping.stop_all().await;
    discovery::remove_if_ours(&root, pid);
    served.map_err(|err| format!("serving: {err}"))
}

/// `session list|start|stop`, against the engine `engine.json` names.
async fn session_command(args: &[String]) -> Result<(), String> {
    use arvo_engine::grpc::proto::{sessions_client::SessionsClient, Empty, SessionId, SessionStatus, StartRequest};

    let root = arvo_service::project::app_data_root().map_err(|err| format!("no app data directory: {err}"))?;
    let found = discovery::running(&root).ok_or("no engine is running; open Arvo or start arvo-engine")?;
    let token = discovery::read_control(&root).ok_or("no control.json beside engine.json")?;
    let mut client = SessionsClient::connect(format!("http://{}", found.address))
        .await
        .map_err(|err| format!("connecting to the engine: {err}"))?;
    fn bearer<T>(message: T, token: &str) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().expect("hex is ascii"));
        request
    }
    let show = |status: &SessionStatus| {
        println!(
            "{}  {}  {} {}  signals {}  submitted {}  refused {}  fills {}{}{}{}",
            status.id,
            status.state,
            status.instrument,
            status.strategy,
            status.signals,
            status.submitted,
            status.refused,
            status.fills,
            status.last_bar.as_ref().map_or(String::new(), |at| format!("  last bar {at}")),
            status.halted.as_ref().map_or(String::new(), |why| format!("  HALTED: {why}")),
            status.last_error.as_ref().map_or(String::new(), |err| format!("  error: {err}")),
        );
    };
    match args {
        [verb] if verb == "list" => {
            let listed = client.list_sessions(bearer(Empty {}, &token)).await.map_err(|err| err.message().to_owned())?;
            let sessions = listed.into_inner().sessions;
            if sessions.is_empty() {
                println!("no sessions");
            }
            sessions.iter().for_each(show);
        }
        [verb, finding, executor] if verb == "start" => {
            let started = client
                .start_session(bearer(StartRequest { finding: finding.clone(), executor: executor.clone() }, &token))
                .await
                .map_err(|err| err.message().to_owned())?;
            show(&started.into_inner());
        }
        [verb, id] if verb == "stop" => {
            let stopped = client
                .stop_session(bearer(SessionId { id: id.clone() }, &token))
                .await
                .map_err(|err| err.message().to_owned())?;
            show(&stopped.into_inner());
        }
        _ => return Err("usage: arvo-engine session list | start <finding> <executor> | stop <id>".to_owned()),
    }
    Ok(())
}
