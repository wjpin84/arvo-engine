//! The jobs that belong to whatever process holds the data.
//!
//! They are here rather than in the window because that is where their work is:
//! one hashes the bar library to find findings that went stale, one records an
//! option chain from a venue, one keeps the universes fetched, one writes the
//! day's review, and one sweeps a universe nobody has swept. All of them want to
//! keep running when the window is closed (ADR-0018), and the chain needs a
//! credential the engine holds (ADR-0028).
//!
//! The window keeps its own jobs — the script runs a person scheduled, whose
//! output goes to its terminal panel — and the Jobs view is the two lists
//! together.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arvo_api::EventView;

use crate::research::ResearchService;
use arvo_schedule::Jobs;

/// Under the data root: one file of option quotes per day.
pub const OPTION_QUOTES_SUBDIR: &str = "option-quotes";

/// Makes the project's own files exist before anything reads them: the
/// `.gitignore` that keeps personal data out of git, and the risk model as a
/// file the person can open.
///
/// Only for a project, never the app data directory, and only warnings if it
/// cannot: a folder Arvo cannot write to is a problem to report, not a reason
/// to refuse to start.
pub fn prepare(root: &Path) {
    // The library first, and for the app data directory too: an empty folder
    // that exists is a clearer instruction than a path in an error message.
    let data = root.join(crate::research::DATA_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&data) {
        eprintln!("arvo-engine: could not create {}; studies will find no instruments: {err}", data.display());
    }
    if crate::project::remembered().is_none() {
        return;
    }
    if let Err(err) = crate::project::ensure_gitignore(root) {
        eprintln!("arvo-engine: could not write {}/.gitignore: {err}", root.display());
    }
    if let Err(err) = crate::risk::ensure(root) {
        eprintln!("arvo-engine: could not write the project risk model in {}: {err}", root.display());
    }
    // The stores as DuckDB views (ADR-0039), from what is on disk now.
    if let Err(err) = crate::views::write(root) {
        eprintln!("arvo-engine: could not write {}/{}: {err}", root.display(), crate::views::FILE);
    }
}

/// Registers the staleness check and the option-chain recorder on `jobs`.
///
/// `raise` is how a job tells someone: the engine broadcasts, so a stale
/// finding reaches every front end that is listening.
pub fn register(jobs: &Jobs, root: &Path, research: Arc<ResearchService>, raise: impl Fn(EventView) + Send + Sync + 'static) {
    // Tell someone when a stored finding goes stale while they are not
    // looking. On a blocking thread: it hashes bar files. The first check runs
    // at startup, which is when "while you were away" is true.
    let reported = root.join(crate::research::staleness::FILE);
    let raise = Arc::new(raise);
    let announce = raise.clone();
    let checking = research.clone();
    jobs.every("staleness", "Check findings for staleness", crate::research::staleness::EVERY, move || {
        let reported = reported.clone();
        let research = checking.clone();
        let raise = raise.clone();
        async move {
            let raised = tokio::task::spawn_blocking(move || {
                crate::research::staleness::check(&research, &reported).map(|event| {
                    let said = event.title.clone();
                    raise(event);
                    said
                })
            })
            .await
            .map_err(|err| format!("the check did not finish: {err}"))?;
            Ok(raised.unwrap_or_else(|| "nothing newly stale".to_owned()))
        }
    });

    // Universes kept fetched (#227): every hour, each member whose series is
    // missing or behind the last completed bar, through the source that
    // serves its venue. Hourly so the day's intraday bars are in the library
    // within the hour after the close, which is when its review is read; a
    // run that finds everything current fetches nothing.
    let fetching = research.clone();
    let announce_fetch = announce.clone();
    let universes_root = root.to_path_buf();
    jobs.every("universes", "Keep the universes fetched", std::time::Duration::from_secs(60 * 60), move || {
        let research = fetching.clone();
        let raise = announce_fetch.clone();
        let root = universes_root.clone();
        async move {
            let mut said = Vec::new();
            for (path, universe) in crate::universes::read_all(&root) {
                match universe {
                    Ok(universe) => {
                        let report: crate::research::data::Report<'_> = &|event| raise(event);
                        let done = crate::universes::refresh(&research, &universe, report).await?;
                        said.push(format!("{}: {}", universe.name, done.describe()));
                    }
                    Err(why) => said.push(format!("{path}: {why}")),
                }
            }
            Ok(if said.is_empty() { "no universes".to_owned() } else { said.join("; ") })
        }
    });

    // A universe nobody has swept (ADR-0034). The universes sat unrun for days
    // with every member's bars already on disk, because nothing scheduled a
    // search — the platform scheduled fetching and reviewing and left the
    // question to whoever remembered to ask it.
    //
    // # Why it sweeps only what has never been swept
    //
    // Every run charges its author's search history (ADR-0014), and the store is
    // already charging 156 configurations across 22 findings. A nightly sweep of
    // nine panels would add three thousand a year and raise the deflated bar
    // until nothing could clear it — a scheduled job that made the gates
    // unpassable would be worse than no job at all.
    //
    // So this fills gaps and never re-measures. A pair of universe and rule that
    // has a recorded panel is left alone however old it is; a new universe, or a
    // new rule, or a rule whose resolution newly matches, gets one sweep.
    // Re-measuring the same pair against newer bars stays a deliberate act,
    // because deciding it is worth another draw on the search budget is a
    // judgement and not a schedule.
    //
    // One panel per tick, because a hundred-instrument panel is a couple of
    // minutes of every core and a backlog of nine should not take the machine
    // for half an hour at once.
    let sweeping = research.clone();
    let announce_sweep = announce.clone();
    let sweep_root = root.to_path_buf();
    jobs.every("research", "Sweep a universe nobody has swept", std::time::Duration::from_secs(60 * 60), move || {
        let research = sweeping.clone();
        let raise = announce_sweep.clone();
        let root = sweep_root.clone();
        async move {
            let queue = tokio::task::spawn_blocking({
                let research = research.clone();
                let root = root.clone();
                move || crate::research::sweep::unswept(&research, &root)
            })
            .await
            .map_err(|err| format!("the sweep did not decide what to run: {err}"))?;
            if queue.is_empty() {
                return Ok("nothing unswept".to_owned());
            }

            // The first pair that runs is the tick's one panel. A pair the
            // engine refuses is passed over rather than waited on: a refusal
            // records nothing, so the pair stays unswept, and stopping at it
            // stopped every pair behind it for as long as it was refused.
            // ponytail: a refusal is assumed cheap, a check before any
            // backtest. If one ever costs minutes, cap the refusals per tick.
            let mut refused = Vec::new();
            for (universe, rule) in queue {
                let said = format!("{rule} over {}", universe.name);
                let ran = tokio::task::spawn_blocking({
                    let research = research.clone();
                    move || crate::research::study::run_panel_over(&research, &universe, Some(&rule))
                })
                .await
                .map_err(|err| format!("{said} did not finish: {err}"))?;
                match ran {
                    Ok(view) => {
                        raise(EventView::new(
                            arvo_api::EventKindView::findings(0),
                            format!("Swept {said}"),
                            format!("{}: {}", view.verdict, view.reasons.join("; ")),
                            arvo_api::SeverityView::Info,
                        ));
                        let passed = if refused.is_empty() { String::new() } else { format!("; passed over {}", refused.join("; ")) };
                        return Ok(format!("{said}: {}{passed}", view.verdict));
                    }
                    Err(err) => refused.push(format!("{said} ({err})")),
                }
            }
            Err(format!("nothing unswept could be run: {}", refused.join("; ")))
        }
    });

    // The review after the close (#217): once a day, a quarter of an hour
    // after the regular close, when the day's sessions have settled their
    // last fills. Checked every ten minutes so a restart in the evening
    // still writes it; never rewritten, so a person's reading of it stays
    // what they read.
    let reviewing = root.to_path_buf();
    jobs.every("review", "Write the review after the close", std::time::Duration::from_secs(10 * 60), move || {
        let root = reviewing.clone();
        let raise = announce.clone();
        async move {
            let now = chrono::Utc::now();
            // Today and yesterday, because a day is not always over when the
            // equity market closes. A day that ran a session on a continuous
            // instrument is finished at midnight UTC, so it becomes reviewable
            // only after the date rolls over — reviewing just `today` would
            // never write it at all.
            let mut said = Vec::new();
            for day in [now.naive_utc().date() - chrono::Duration::days(1), now.naive_utc().date()] {
                if crate::review::read(&root, day).is_some() {
                    continue;
                }
                let reviewed = crate::review::review(&root, day);
                if !crate::review::is_finished(&reviewed, now) {
                    continue;
                }
                let path = crate::review::write(&root, &reviewed)?;
                raise(EventView::new(
                    arvo_api::EventKindView::findings(0),
                    "The day's review is written".to_owned(),
                    format!("{}: {} session(s), realised {:+.2}", path.display(), reviewed.sessions.len(), reviewed.realised()),
                    arvo_api::SeverityView::Info,
                ));
                said.push(format!("written to {}", path.display()));
            }
            Ok(if said.is_empty() { "nothing finished to review".to_owned() } else { said.join("; ") })
        }
    });

    // SPY option quotes, kept because no vendor serves them afterwards (#83).
    // Only in the regular session, and only with Alpaca keys: without them
    // there is nothing to record and nothing to say.
    let quotes: PathBuf = root.join(OPTION_QUOTES_SUBDIR);
    let listing = root.to_path_buf();
    jobs.every(
        "option-quotes",
        "Record option quotes",
        std::time::Duration::from_secs(15 * 60),
        move || {
            let dir = quotes.clone();
            let root = listing.clone();
            async move {
                let now = chrono::Utc::now();
                if !arvo_data::session::in_regular_session(now.naive_utc()) {
                    return Ok("outside the regular session: a weekend, or before the open or after the close".to_owned());
                }
                // Every underlying the project asked for (#230). One that
                // fails is named and the rest are still recorded: a chain
                // missed today cannot be fetched tomorrow (#83).
                let (symbols, complaint) = crate::option_quotes::wanted(&root);
                let mut said: Vec<String> = complaint.into_iter().collect();
                let (mut recorded, mut asleep, mut unchanged) = (0usize, 0usize, 0usize);
                for symbol in &symbols {
                    match crate::source::alpaca::options::record_chain(symbol, &dir, now).await {
                        // The chain is the last one over again: a holiday or
                        // an early close, which the session check cannot see
                        // (arvo-engine#28). Nothing was written.
                        Ok(chain) if chain.unchanged => unchanged += 1,
                        Ok(chain) => {
                            recorded += 1;
                            said.push(format!("{symbol} {}", chain.contracts));
                        }
                        Err(arvo_data::source::SourceError::NoSession { .. }) => asleep += 1,
                        Err(err) => said.push(format!("{symbol}: {err}")),
                    }
                }
                if asleep == symbols.len() {
                    return Ok("no Alpaca session".to_owned());
                }
                if unchanged > 0 && recorded == 0 && said.is_empty() {
                    return Ok("nothing recorded: no chain has changed since the last snapshot, so the market is closed".to_owned());
                }
                if unchanged > 0 {
                    said.push(format!("{unchanged} unchanged and not written"));
                }
                // The day's cost on disk, which is one of the three numbers
                // that decide whether the library needs a different store.
                let bytes: u64 = symbols
                    .iter()
                    .map(|symbol| crate::option_quotes::bytes_today(&root, symbol, now.date_naive()))
                    .sum();
                Ok(format!(
                    "{recorded} of {} recorded, {:.1} MB today: {}",
                    symbols.len(),
                    bytes as f64 / (1024.0 * 1024.0),
                    said.join("; ")
                ))
            }
        },
    );

    // A finished day of quotes becomes Parquet (ADR-0039): the same rows in a
    // seventeenth of the bytes, and readable. Hourly, because nothing is
    // waiting on it: a day is finished from the moment the date turns, and
    // the first pass after that does the work. On a blocking thread: it reads
    // and rewrites whole files.
    let compacting = root.to_path_buf();
    jobs.every(
        "option-quotes-compact",
        "Compact finished days of option quotes",
        std::time::Duration::from_secs(60 * 60),
        move || {
            let root = compacting.clone();
            async move {
                let today = chrono::Utc::now().date_naive();
                tokio::task::spawn_blocking(move || {
                    let done = crate::option_quotes::compact_finished(&root, today);
                    // A day changed form, so the views say so. A failure here
                    // costs a stale views file, not the compaction.
                    if done.days > 0 && crate::project::remembered().is_some() {
                        if let Err(err) = crate::views::write(&root) {
                            tracing::warn!(error = %err, "could not rewrite the project's DuckDB views");
                        }
                    }
                    done.describe()
                })
                    .await
                    .map_err(|err| format!("the compaction did not finish: {err}"))
            }
        },
    );
}
