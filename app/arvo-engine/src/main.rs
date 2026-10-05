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

use arvo_engine::{discovery, grpc, research, venues};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("arvo-engine: {err}");
        std::process::exit(1);
    }
}

const USAGE: &str = "usage:
  arvo-engine [<data-dir>]                       serve; the app data directory when none is given
  arvo-engine [<data-dir>] shutdown              stop that engine: its sessions end with `stopped`, its plugins stop, and it exits; positions are not touched
  arvo-engine [<data-dir>] session list          the verbs reach the engine serving <data-dir>,
  arvo-engine [<data-dir>] session start <finding> <executor>   or the app data one when none is given
  arvo-engine [<data-dir>] session stop|reconcile|resume <id>
  arvo-engine [<data-dir>] session halt <id>|--all [reason...] the kill switch: arm the gate, flatten, stay halted
  arvo-engine [<data-dir>] session explain <id> <time>        the chain behind every position held then
  arvo-engine [<data-dir>] review [YYYY-MM-DD]   the review after the close: written under reviews/, and printed
  arvo-engine [<data-dir>] rank [--rule R] [--instrument I]   the leaderboard: every comparable finding, in the one order
  arvo-engine [<data-dir>] universes [refresh]   the project's universes and their coverage; `refresh` fetches what is missing or behind
  arvo-engine [<data-dir>] pine <file> [--interval 1day] [--keep]   read a Pine v5 strategy as a rule; --keep writes it under rules/
  arvo-engine [<data-dir>] option-quotes spreads <symbol>   what the recorded chains say an option costs to cross, by premium
  arvo-engine [<data-dir>] option-quotes compact   rewrite every finished day of recorded quotes as Parquet
  arvo-engine [<data-dir>] views [--print]       the project's stores as DuckDB views, written to .arvo/views.sql; --print shows them and writes nothing
  arvo-engine [<data-dir>] evidence rewrite      rewrite every finding compact, with its curves in an artifact; stop the engine first";

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("session") {
        return session_command(None, &args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("shutdown") {
        return shutdown_command(None, &args[1..]).await;
    }
    // `arvo-engine <data-dir> session …`: the verbs against the engine that
    // serves that directory, which wrote its engine.json there (#223).
    if let [dir, verb, rest @ ..] = args.as_slice() {
        if verb == "session" && std::path::Path::new(dir).is_dir() {
            return session_command(Some(PathBuf::from(dir)), rest).await;
        }
        if verb == "shutdown" && std::path::Path::new(dir).is_dir() {
            return shutdown_command(Some(PathBuf::from(dir)), rest).await;
        }
        if verb == "review" && std::path::Path::new(dir).is_dir() {
            return review_command(PathBuf::from(dir), rest);
        }
        if verb == "rank" && std::path::Path::new(dir).is_dir() {
            return rank_command(PathBuf::from(dir), rest);
        }
        if verb == "universes" && std::path::Path::new(dir).is_dir() {
            return universes_command(PathBuf::from(dir), rest).await;
        }
        if verb == "pine" && std::path::Path::new(dir).is_dir() {
            return pine_command(PathBuf::from(dir), rest);
        }
        if verb == "option-quotes" && std::path::Path::new(dir).is_dir() {
            return option_quotes_command(&PathBuf::from(dir), rest);
        }
        if verb == "views" && std::path::Path::new(dir).is_dir() {
            return views_command(&PathBuf::from(dir), rest);
        }
        if verb == "evidence" && std::path::Path::new(dir).is_dir() {
            return evidence_command(&PathBuf::from(dir), rest);
        }
    }
    if args.first().map(String::as_str) == Some("evidence") {
        return evidence_command(&research::default_root()?, &args[1..]);
    }
    if args.first().map(String::as_str) == Some("views") {
        return views_command(&research::default_root()?, &args[1..]);
    }
    if args.first().map(String::as_str) == Some("option-quotes") {
        return option_quotes_command(&research::default_root()?, &args[1..]);
    }
    if args.first().map(String::as_str) == Some("pine") {
        return pine_command(research::default_root()?, &args[1..]);
    }
    if args.first().map(String::as_str) == Some("universes") {
        return universes_command(research::default_root()?, &args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("review") {
        return review_command(research::default_root()?, &args[1..]);
    }
    if args.first().map(String::as_str) == Some("rank") {
        return rank_command(research::default_root()?, &args[1..]);
    }
    // A positional argument is a data directory that exists. Anything else
    // is a mistake to say so about, not a directory to create and serve: an
    // `arvo-engine help` once started a second engine under `./help`.
    let (root, data) = match args.first().map(String::as_str) {
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(dir) if std::path::Path::new(dir).is_dir() => (PathBuf::from(dir), PathBuf::from(dir)),
        Some(other) => return Err(format!("{other:?} is not a directory or a verb\n{USAGE}")),
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

    // Asked before this engine writes its own engine.json, which the same
    // question would then find.
    let elsewhere = serving(&data);

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
    let sessions = std::sync::Arc::new(arvo_trading::Sessions::new(&data, events.clone(), std::sync::Arc::new(venues::Brokers)));
    // The sessions the last engine left without a last word (#13): killed, or
    // dead, it took their threads with it, and this one used to answer "no
    // sessions" while a venue still held what they had opened. Not while
    // another engine serves this data, whose live sessions have no last word
    // yet either.
    match elsewhere {
        Some(other) => eprintln!(
            "arvo-engine: another engine (pid {}) is serving {}; not looking for sessions it may be running",
            other.pid,
            data.display()
        ),
        None => {
            for dropped in sessions.note_dropped() {
                eprintln!("arvo-engine: {} was dropped: {}", dropped.id, dropped.last_error.unwrap_or_default());
            }
        }
    }
    arvo_service::jobs::prepare(&data);

    // The engine's own jobs, on its own runtime: this is `#[tokio::main]`, so
    // `tokio::spawn` is what puts a loop on a reactor here.
    let jobs = arvo_schedule::Jobs::new(std::sync::Arc::new(|future| {
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
    // The live price stream: one socket for the whole machine, held open
    // whether or not a window is watching.
    let ticks = tokio::sync::broadcast::channel(1024).0;
    let (stream, streaming) = {
        let raising = events.clone();
        arvo_service::stream::start(ticks.clone(), move |event| {
            let _ = raising.send(event);
        })
    };
    tokio::spawn(streaming);

    let hosted = sessions.clone();
    let engine =
        grpc::Engine { research: research::Research::new(&data), sessions, events, jobs, plugins, stream, ticks, stop };
    let served = grpc::serve(listener, engine, &tokens, async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = asked => {}
        }
    })
    .await;
    // The way out, in the order things depend on each other. Sessions first:
    // each loop finishes its poll and writes `stopped`, so a record that ends
    // here reads as an engine that was stopped and not one that died (#13).
    // Positions are left as they are. On a blocking thread, since it waits on
    // other threads.
    let ended = tokio::task::spawn_blocking(move || hosted.stop_all(SESSIONS_GRACE)).await.unwrap_or_default();
    for id in &ended.stopped {
        eprintln!("arvo-engine: stopped {id}; its positions are as they were");
    }
    for id in &ended.unfinished {
        eprintln!(
            "arvo-engine: {id} did not stop within {} seconds; its record will not end with `stopped`",
            SESSIONS_GRACE.as_secs()
        );
    }
    // Nothing this started lingers (ADR-0023 point 5), now that this is what
    // started it.
    stopping.stop_all().await;
    // Last, because it is what `shutdown` waits for: once the file is gone the
    // engine has nothing left to do but return.
    discovery::remove_if_ours(&root, pid);
    served.map_err(|err| format!("serving: {err}"))
}

/// How long the sessions get to finish their poll when the engine stops. A
/// loop wakes every second to look, so this is room for a call in flight to a
/// venue and no more.
const SESSIONS_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// `shutdown`: asks the engine serving `dir` to stop, and waits until it has.
///
/// The engine stops its sessions so each record ends with `stopped`, stops the
/// plugins it started, removes its `engine.json` and exits. Killing the
/// process does none of that: no destructor runs, the plugins are orphaned,
/// and a session's record just ends. Positions are not touched; `session halt`
/// is the kill switch.
///
/// No engine running is not an error: the thing asked for is already true,
/// and `arvo-engine shutdown` followed by a build should not fail on a
/// machine where the engine was not up.
async fn shutdown_command(dir: Option<PathBuf>, args: &[String]) -> Result<(), String> {
    use arvo_engine::grpc::proto::common::Empty;
    use arvo_engine::grpc::proto::services::sessions_client::SessionsClient;

    if !args.is_empty() {
        return Err(USAGE.to_owned());
    }
    let data = match &dir {
        Some(dir) => dir.clone(),
        None => research::default_root()?,
    };
    let root = match dir {
        Some(dir) => dir,
        None => arvo_service::project::app_data_root().map_err(|err| format!("no app data directory: {err}"))?,
    };
    let Some(found) = discovery::running(&root) else {
        println!("no engine is running for {}", root.display());
        return Ok(());
    };
    let token = discovery::read_control(&root).ok_or("no control.json beside engine.json")?;
    let mut client = SessionsClient::connect(format!("http://{}", found.address))
        .await
        .map_err(|err| format!("connecting to the engine: {err}"))?;
    let bearer = |message| arvo_client::request(&token, message).map_err(|err| err.to_string());

    // Which sessions are up, so the answer can say what became of each.
    let live: Vec<String> = client
        .list_sessions(bearer(Empty {})?)
        .await
        .map_err(|err| err.message().to_owned())?
        .into_inner()
        .sessions
        .into_iter()
        .filter(|status| matches!(status.state.as_str(), "starting" | "running" | "frozen" | "halted"))
        .map(|status| status.id)
        .collect();
    client.shutdown(bearer(Empty {})?).await.map_err(|err| err.message().to_owned())?;

    // Gone when its engine.json no longer names it: removing that file is the
    // last thing the engine does.
    let patience = SESSIONS_GRACE + grpc::STREAMS_GRACE + std::time::Duration::from_secs(20);
    let deadline = std::time::Instant::now() + patience;
    while discovery::read(&root).is_some_and(|still| still.pid == found.pid) {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "asked the engine (pid {}) to stop and it has not after {} seconds; it can be ended with the operating system, which orphans its plugins",
                found.pid,
                patience.as_secs()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // The file goes a moment before the process does, and on Windows the
    // binary stays locked until the process has.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    println!("the engine (pid {}) has stopped", found.pid);
    for id in &live {
        let path = arvo_trading::record_path(&data, id);
        let ended_with_stopped = std::fs::read_to_string(&path).ok().is_some_and(|record| {
            record
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .is_some_and(|line| line.get("event").and_then(serde_json::Value::as_str) == Some("stopped"))
        });
        if ended_with_stopped {
            println!("  {id}: stopped, positions as they were");
        } else {
            println!("  {id}: its record does not end with `stopped`; see {}", path.display());
        }
    }
    Ok(())
}

/// `pine <file> [--interval I] [--keep]`: reads a Pine v5 strategy as a rule
/// (#228) and prints it, with everything the reader set aside. `--keep`
/// writes it under `rules/`, which is the same decision `write_rule` makes.
/// Needs no engine.
fn pine_command(root: PathBuf, args: &[String]) -> Result<(), String> {
    let (mut file, mut interval, mut keep) = (None, arvo_data::BarInterval::DAILY, false);
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--keep" => keep = true,
            "--interval" => {
                let named = rest.next().ok_or_else(|| USAGE.to_owned())?;
                interval = named.parse().map_err(|err| format!("{named:?}: {err}"))?;
            }
            _ if file.is_none() => file = Some(PathBuf::from(argument)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let file = file.ok_or_else(|| USAGE.to_owned())?;
    let text = std::fs::read_to_string(&file).map_err(|err| format!("reading {}: {err}", file.display()))?;
    let translated = arvo_research::pine::translate(&text, interval).map_err(|err| err.to_string())?;
    let translated = arvo_research::pine::attributed(translated, &text);
    for line in &translated.ignored {
        eprintln!("arvo-engine: set aside  {line}");
    }
    eprintln!(
        "arvo-engine: {} — enter when {}{}",
        translated.rule.name,
        translated.rule.entry.describe(),
        translated.rule.exit.as_ref().map_or_else(String::new, |exit| format!("; leave when {}", exit.describe()))
    );
    if keep {
        let written = arvo_service::rules::write(&root, &translated.rule)?;
        eprintln!("arvo-engine: written to {}", written.path);
    } else {
        println!("{}", serde_json::to_string_pretty(&translated.rule).map_err(|err| err.to_string())?);
    }
    Ok(())
}

/// `universes [refresh]`: the project's universes with their members'
/// coverage; `refresh` fetches what is missing or behind (#227). Needs no
/// engine, but a venue's source may need its credentials.
async fn universes_command(root: PathBuf, args: &[String]) -> Result<(), String> {
    let refresh = match args {
        [] => false,
        [word] if word == "refresh" => true,
        _ => return Err(USAGE.to_owned()),
    };
    let service = arvo_service::research::ResearchService::new(
        root.join(arvo_service::research::DATA_SUBDIR),
        root.join(arvo_service::research::EVIDENCE_SUBDIR),
    );
    let listed = arvo_service::universes::read_all(&root);
    if listed.is_empty() {
        println!("no universes under {}", root.join(arvo_service::universes::SUBDIR).display());
    }
    for (path, universe) in listed {
        match universe {
            Err(why) => println!("{path}: {why}"),
            Ok(universe) => {
                let covered = universe
                    .instruments
                    .iter()
                    .filter(|id| matches!(arvo_data::BarProvider::coverage(&service.bars, id, universe.interval), Ok(Some(_))))
                    .count();
                println!(
                    "{}  {} member(s), {} with a series at {}
  {}",
                    universe.name,
                    universe.instruments.len(),
                    covered,
                    universe.interval,
                    universe.reason
                );
                if refresh {
                    let report: arvo_service::research::data::Report<'_> = &|_event| {};
                    let done = arvo_service::universes::refresh(&service, &universe, report).await?;
                    println!("  {}", done.describe());
                    for (id, why) in &done.failed {
                        println!("  {id}: {why}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// `rank [--rule R] [--instrument I]`: the leaderboard over the findings under
/// `root`, in the one order (#226). Needs no engine.
fn rank_command(root: PathBuf, args: &[String]) -> Result<(), String> {
    let (mut rule, mut instrument) = (None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--rule" => rule = rest.next().cloned(),
            "--instrument" => instrument = rest.next().cloned(),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let service = arvo_service::research::ResearchService::new(
        root.join(arvo_service::research::DATA_SUBDIR),
        root.join(arvo_service::research::EVIDENCE_SUBDIR),
    );
    arvo_service::rulesets::refresh_at(&root);
    let ranking = arvo_service::research::rank::rank(&service, rule.as_deref(), instrument.as_deref()).map_err(|err| err.to_string())?;
    println!("{:>3}  {:<34} {:<12} {:<24} {:<13} {:<13} {:>10} {:>8} {:>7} {:>6}", "#", "finding", "instrument", "rule", "verdict", "conservative", "expectancy", "return", "dd", "trades");
    for (n, row) in ranking.rows.iter().enumerate() {
        println!(
            "{:>3}  {:<34} {:<12} {:<24} {:<13} {:<13} {:>10} {:>7.1}% {:>6.1}% {:>6}{}",
            n + 1,
            row.id,
            row.subject,
            row.rule,
            row.verdict,
            if row.conservative_verdict.is_empty() { "not measured" } else { &row.conservative_verdict },
            format!("{:+.2} {}", row.expectancy, if row.expectancy_costs == "conservative" { "c" } else { "s" }),
            row.total_return * 100.0,
            row.max_drawdown * 100.0,
            row.trades,
            if row.stale == Some(true) { "  stale" } else { "" },
        );
    }
    for note in &ranking.notes {
        eprintln!("arvo-engine: {note}");
    }
    Ok(())
}

/// `option-quotes spreads <symbol>` and `option-quotes compact`: the recorded
/// chains under `root`, read and tidied (ADR-0039). Needs no engine.
///
/// `spreads` prints what the recording says an option costs to cross, beside
/// what the cost model charges. It changes nothing: moving the model's
/// constant re-costs every option study, which is a decision and not a
/// side effect of reading a table.
fn option_quotes_command(root: &std::path::Path, args: &[String]) -> Result<(), String> {
    match args {
        [verb, symbol] if verb == "spreads" => {
            for chain in arvo_service::option_quotes::spreads(root, symbol)? {
                println!("{}", chain.table());
            }
            Ok(())
        }
        [verb] if verb == "compact" => {
            let today = chrono::Utc::now().date_naive();
            let done = arvo_service::option_quotes::compact_finished(root, today);
            println!("{}", done.describe());
            if done.failed.is_empty() {
                Ok(())
            } else {
                Err(format!("{} day(s) could not be compacted and are still CSV", done.failed.len()))
            }
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// The engine serving the data under `root`, if one is. It wrote its
/// engine.json there, or in the app data directory when `root` is the project
/// it remembers.
fn serving(root: &std::path::Path) -> Option<discovery::Discovery> {
    let same = |a: &std::path::Path, b: &std::path::Path| {
        std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(a, b)| a == b)
    };
    let remembered = arvo_service::project::remembered().is_some_and(|project| same(&project, root));
    discovery::running(root).or_else(|| {
        remembered
            .then(|| arvo_service::project::app_data_root().ok())
            .flatten()
            .and_then(|dir| discovery::running(&dir))
    })
}

/// `evidence rewrite`: every finding under `root` in the form this build
/// writes, compact and with its curves in an artifact (ADR-0037, ADR-0039).
///
/// For a store written before either existed. It refuses while an engine is
/// serving the directory: that engine may be an older build, which could not
/// read a finding once it is rewritten, and a session reads its finding when
/// it starts.
fn evidence_command(root: &std::path::Path, args: &[String]) -> Result<(), String> {
    match args {
        [verb] if verb == "rewrite" => {
            if let Some(found) = serving(root) {
                return Err(format!(
                    "an engine is running (pid {}); stop it first, since a build older than this one cannot read a rewritten finding",
                    found.pid
                ));
            }
            let store = arvo_research::EvidenceStore::new(root.join(arvo_service::research::EVIDENCE_SUBDIR));
            let done = store.rewrite().map_err(|err| err.to_string())?;
            println!("{}", done.describe());
            if done.problems.is_empty() {
                Ok(())
            } else {
                Err(format!("{} finding(s) were left as they were", done.problems.len()))
            }
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// `views [--print]`: the stores under `root` as DuckDB views (ADR-0039).
/// Needs no engine, and the engine links no DuckDB: this writes the file a
/// person opens with it.
fn views_command(root: &std::path::Path, args: &[String]) -> Result<(), String> {
    match args {
        [flag] if flag == "--print" => {
            print!("{}", arvo_service::views::sql(root));
            Ok(())
        }
        [] => {
            let path = arvo_service::views::write(root).map_err(|err| format!("writing the views: {err}"))?;
            println!(
                "{}\n\nfrom {}:\n  duckdb -init {}",
                path.display(),
                root.display(),
                arvo_service::views::FILE
            );
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// `review [YYYY-MM-DD]`: the day's review from the session records under
/// `root`, written under `reviews/` and printed. Needs no engine (#217).
/// A day already reviewed is printed as it was written, not rewritten, and
/// a day that is not over is printed and not written.
fn review_command(root: PathBuf, args: &[String]) -> Result<(), String> {
    let day = match args {
        [] => chrono::Utc::now().date_naive(),
        [day] => day.parse().map_err(|err| format!("{day:?} is not a date (YYYY-MM-DD): {err}"))?,
        _ => return Err(USAGE.to_owned()),
    };
    let opened = arvo_service::review::open(&root, day, chrono::Utc::now())?;
    if opened.written_now {
        eprintln!("arvo-engine: written to {}", opened.path.display());
    }
    print!("{}", opened.markdown);
    Ok(())
}

/// `session list|start|stop|reconcile|resume`, against the engine whose
/// `engine.json` is in `dir` — the app data directory when none is given,
/// with the project it remembers as the data directory; `session explain
/// <id> <time>` reads the record on disk and needs no engine.
async fn session_command(dir: Option<PathBuf>, args: &[String]) -> Result<(), String> {
    use arvo_engine::grpc::proto::common::Empty;
    use arvo_engine::grpc::proto::services::sessions_client::SessionsClient;
    use arvo_engine::grpc::proto::session::{HaltRequest, SessionId, SessionStatus, StartRequest};

    if let [verb, id, when] = args {
        if verb == "explain" {
            let when = arvo_engine::explain::parse_when(when)?;
            let data = match &dir {
                Some(dir) => dir.clone(),
                None => research::default_root()?,
            };
            let path = arvo_trading::record_path(&data, id);
            let record = std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
            print!("{}", arvo_engine::explain::explain(&record, when));
            return Ok(());
        }
    }
    let root = match dir {
        Some(dir) => dir,
        None => arvo_service::project::app_data_root().map_err(|err| format!("no app data directory: {err}"))?,
    };
    let found = discovery::running(&root)
        .ok_or_else(|| format!("no engine is running for {}; open Arvo or start arvo-engine", root.display()))?;
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
            "{}  {}  {} {}  signals {}  submitted {}  refused {}  fills {}  {}{}{}{}{}{}{}",
            status.id,
            status.state,
            status.instrument,
            status.strategy,
            status.signals,
            status.submitted,
            status.refused,
            status.fills,
            match status.verdict_reason.as_ref() {
                Some(why) => format!("{} ({why})", status.verdict),
                None => status.verdict.clone(),
            },
            status.last_bar.as_ref().map_or(String::new(), |at| format!("  last bar {at}")),
            if status.warnings.is_empty() { String::new() } else { format!("  NEAR: {}", status.warnings.join("; ")) },
            status.divergence.as_ref().map_or(String::new(), |measured| {
                format!(
                    "  slippage {:.1} bps (worst {:.1}{}), latency {:.0} ms{}",
                    measured.mean_slippage_bps,
                    measured.worst_slippage_bps,
                    measured.assumed_slippage_bps.map_or(String::new(), |assumed| format!("; assumed {assumed:.1}")),
                    measured.mean_latency_ms,
                    if measured.unfilled > 0 { format!(", {} unfilled", measured.unfilled) } else { String::new() },
                )
            }),
            status.halted.as_ref().map_or(String::new(), |why| format!("  HALTED: {why}")),
            status.frozen.as_ref().map_or(String::new(), |why| {
                format!("  FROZEN: {why}{}", if status.reconciled { " (reconciled; resume when ready)" } else { " (reconcile first)" })
            }),
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
        [verb, id] if verb == "reconcile" => {
            let reconciled = client
                .reconcile_session(bearer(SessionId { id: id.clone() }, &token))
                .await
                .map_err(|err| err.message().to_owned())?;
            show(&reconciled.into_inner());
        }
        [verb, target, reason @ ..] if verb == "halt" => {
            let reason = reason.join(" ");
            let ids: Vec<String> = if target == "--all" {
                let listed = client.list_sessions(bearer(Empty {}, &token)).await.map_err(|err| err.message().to_owned())?;
                listed
                    .into_inner()
                    .sessions
                    .into_iter()
                    .filter(|status| matches!(status.state.as_str(), "starting" | "running" | "frozen"))
                    .map(|status| status.id)
                    .collect()
            } else {
                vec![target.clone()]
            };
            if ids.is_empty() {
                println!("nothing to halt");
            }
            for id in ids {
                let halted = client
                    .halt_session(bearer(HaltRequest { id, reason: reason.clone() }, &token))
                    .await
                    .map_err(|err| err.message().to_owned())?;
                show(&halted.into_inner());
            }
        }
        [verb, id] if verb == "resume" => {
            let resumed = client
                .resume_session(bearer(SessionId { id: id.clone() }, &token))
                .await
                .map_err(|err| err.message().to_owned())?;
            show(&resumed.into_inner());
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(())
}
