//! Stored findings: listing them, reopening one, re-running it, comparing
//! several, and writing a ledger out as CSV.
//!
//! The claim `Experiment` makes about itself — that its fields are everything
//! needed to reproduce a run — is the one the whole evidence store rests on.
//! [`replay_record`] is what checks it.

use super::*;

/// Everything in research memory, newest first.
///
/// Reads the store's *summaries* rather than its findings. A stored finding is
/// around 400 KB — curves, ledgers, per-fold evidence, the search surface —
/// and this renders a column of names; parsing every byte of every one to do
/// that cost 1.7 MB for five findings and would cost seconds for five hundred.
///
/// Findings that could not be read come back too. They used to be a log line,
/// which is how four of them were lost to a field rename without anyone being
/// told.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the directory cannot be listed.
#[tauri::command]
pub async fn list_history(
    service: tauri::State<'_, ResearchService>,
) -> Result<HistoryView, CommandError> {
    let (summaries, unreadable) = service
        .memory
        .summaries()
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    Ok(HistoryView {
        entries: summaries
            .iter()
            .map(|summary| {
                let live = live_version(&service, summary);
                HistoryEntryView {
                    id: summary.id.clone(),
                    kind: summary.kind.clone(),
                    subject: summary.subject.clone(),
                    verdict: verdict_label(summary.verdict).to_owned(),
                    recorded_at: summary.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
                    stale: live.map(|live| live != summary.dataset_version),
                }
            })
            .collect(),
        unreadable: unreadable
            .into_iter()
            .map(|item| UnreadableView {
                id: item.id,
                reason: item.reason,
            })
            .collect(),
    })
}

/// What the data behind a finding hashes to *now*, or `None` if it is gone.
///
/// From the summary, so staleness costs no parsing either. A panel's identity
/// is every member's hash combined and is recomputed the same way it was
/// produced — over the instruments present today, so one added or removed also
/// reads as stale, which is correct: the panel would not run the same twice.
pub(crate) fn live_version(service: &ResearchService, summary: &arvo_research::Summary) -> Option<String> {
    match (&summary.instrument, summary.interval) {
        (Some(instrument), Some(interval)) => {
            service.bars.fingerprint(instrument, interval).ok().flatten()
        }
        _ => panel_dataset_version(&service.bars).map(|(version, _, _, _)| version),
    }
}

/// Reopens one stored finding.
#[tauri::command]
pub async fn open_record(
    id: String,
    service: tauri::State<'_, ResearchService>,
) -> Result<RecordView, CommandError> {
    let loaded = service
        .memory
        .load()
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    let stored = loaded
        .records
        .into_iter()
        .find(|stored| stored.id == id)
        .ok_or_else(|| CommandError::Failed(format!("no stored finding {id:?}")))?;

    let engine = service.simulation.engine();
    Ok(match stored.record {
        Record::Study(evidence) => {
            RecordView::Study(Box::new(study_view(&evidence, &service.bars, engine)))
        }
        Record::Panel(evidence) => RecordView::Panel(Box::new(panel_view(&evidence, engine))),
        Record::WalkForward(evidence) => {
            RecordView::WalkForward(Box::new(walk_forward_view(
                &evidence,
                &service.bars,
                engine,
            )))
        }
    })
}

/// Runs a stored finding again and reports whether it still comes out the same.
///
/// The claim `Experiment` makes about itself — that its fields are everything
/// needed to reproduce a run — is the one the whole evidence store rests on,
/// and nothing checked it until this existed. A finding whose numbers cannot
/// be regenerated is not evidence; it is a screenshot of a number.
///
/// Costs one engine run: a study records the winning configuration with its
/// window already set to the period it was judged on, so this repeats the run
/// that was written down rather than the search that found it.
#[tauri::command]
pub async fn replay_record(
    id: String,
    service: tauri::State<'_, ResearchService>,
) -> Result<ReplayView, CommandError> {
    let stored = service
        .memory
        .open(&id)
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    // Hashed here rather than inside the replay: what the data is now is a
    // question about this machine, and the research crate has no filesystem.
    let live = live_version(&service, &stored.summary());
    let simulation = service.simulation.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let outcome = arvo_research::replay(simulation.as_ref(), &stored.record, live.as_deref());
        Ok(replay_view(&outcome))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("replay did not finish: {err}")))?
}

pub(crate) fn replay_view(outcome: &arvo_research::Replay) -> ReplayView {
    use arvo_research::Replay;
    match outcome {
        Replay::Reproduced { points, trades } => ReplayView {
            outcome: "reproduced".to_owned(),
            holds: true,
            detail: format!(
                "Ran again and produced the same {points} equity points and the same {trades} \
                 trades."
            ),
            divergence: None,
        },
        Replay::DataChanged { recorded, current } => ReplayView {
            outcome: "data-changed".to_owned(),
            holds: false,
            detail: format!(
                "The data behind this finding is not the data that produced it \
                 ({} now, {} then), so re-running would measure something else.",
                short_hash(current),
                short_hash(recorded),
            ),
            divergence: None,
        },
        Replay::EngineChanged { recorded, current } => ReplayView {
            outcome: "engine-changed".to_owned(),
            holds: false,
            detail: format!(
                "Recorded on {recorded}; this build runs {current}. A different simulator \
                 disagreeing is not evidence about the old one, so the run was not attempted."
            ),
            divergence: None,
        },
        Replay::Diverged(divergence) => ReplayView {
            outcome: "diverged".to_owned(),
            holds: false,
            detail: format!(
                "Same data, same engine, different answer: {}. Something that decides the result \
                 is not recorded in the experiment.",
                divergence.what,
            ),
            divergence: Some(DivergenceView {
                what: divergence.what.clone(),
                at: divergence.at.map(|at| at as u32),
                when: divergence.when.map(|when| when.to_string()),
                recorded: divergence.recorded,
                replayed: divergence.replayed,
                relative: divergence.relative,
            }),
        },
        Replay::NotReplayable { why } => ReplayView {
            outcome: "not-replayable".to_owned(),
            holds: false,
            detail: format!("Not checked: {why}."),
            divergence: None,
        },
        Replay::Failed { error } => ReplayView {
            outcome: "failed".to_owned(),
            holds: false,
            detail: format!("It would not run again: {error}."),
            divergence: None,
        },
    }
}

/// First eight characters of a content hash, or the whole thing if shorter.
pub(crate) fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Reads several stored findings against each other.
///
/// # The comparison is itself a search, and this is where that gets counted
///
/// Each finding already deflates the grid *inside* it: a study that tried nine
/// configurations knows it tried nine. What none of them can know is that they
/// are one of six findings a person is about to pick a winner from. Choosing
/// the best of six is a search of size six, and the best of six no-skill
/// searches still looks better than the average of them.
///
/// So the same bar is applied here, across the findings' out-of-sample
/// Sharpes. It is a weaker claim than the per-study one and is stated as such:
/// these are out-of-sample results rather than in-sample scores, and the
/// approximation assumes independent draws, which six strategies on the same
/// instrument over the same window are emphatically not. Both of those make it
/// conservative in the same direction — it is easier to pass than it should
/// be, which is the safe way for a check like this to be wrong.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if a named finding cannot be read.
#[tauri::command]
pub async fn compare_records(
    ids: Vec<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<ComparisonView, CommandError> {
    let mut rows = Vec::with_capacity(ids.len());
    let mut curves = Vec::with_capacity(ids.len());
    let mut notes = Vec::new();

    for id in &ids {
        let stored = service
            .memory
            .open(id)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let summary = stored.summary();

        let Some((evaluation, strategy_name)) = comparable(&stored.record) else {
            // A panel is one configuration across many instruments; a study is
            // one instrument. Putting them in the same table would invite
            // reading one number against the other, and they are not the same
            // number.
            notes.push(format!(
                "{} is a panel and is not comparable row-for-row with a single study",
                summary.subject
            ));
            continue;
        };

        let live = live_version(&service, &summary);
        rows.push(ComparisonRowView {
            id: id.clone(),
            subject: summary.subject.clone(),
            kind: summary.kind.clone(),
            strategy_name,
            verdict: verdict_label(summary.verdict).to_owned(),
            recorded_at: summary.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
            total_return: evaluation.strategy.total_return,
            excess_return: evaluation.excess_return,
            sharpe: evaluation.strategy.sharpe,
            max_drawdown: evaluation.strategy.max_drawdown,
            trades: evaluation.strategy.trades,
            win_rate: evaluation.strategy_trades.win_rate,
            profit_factor: evaluation.strategy_trades.profit_factor,
            stale: live.map(|live| live != summary.dataset_version),
        });
        curves.push(NamedCurveView {
            name: format!("{} · {}", summary.subject, summary.kind),
            points: curve_points(&evaluation.strategy_curve),
        });
    }

    let judged = judge_comparison(&rows, notes);
    Ok(ComparisonView {
        rows,
        curves,
        best_sharpe: judged.best,
        expected_best_under_null: judged.bar,
        survived_deflation: judged.survived,
        notes: judged.notes,
    })
}

/// What a comparison is worth, and what is wrong with it.
pub(crate) struct Judged {
    best: Option<f64>,
    bar: Option<f64>,
    survived: bool,
    notes: Vec<String>,
}

/// Applies the same multiple-testing bar to the comparison that each study
/// applies to its own grid.
///
/// Separated from the command so it can be tested: this is the claim the whole
/// screen exists to make, and a comparison table that silently sorted by
/// return would be the most persuasive way this application could mislead
/// someone.
pub(crate) fn judge_comparison(rows: &[ComparisonRowView], mut notes: Vec<String>) -> Judged {
    let sharpes: Vec<f64> = rows.iter().filter_map(|row| row.sharpe).collect();
    let best = sharpes.iter().copied().fold(None::<f64>, |best, value| {
        Some(best.map_or(value, |held: f64| held.max(value)))
    });
    let bar = arvo_research::family::expected_best_under_null(&sharpes);
    let survived = best.is_some_and(|best| bar.is_none_or(|bar| best > bar));

    if let (Some(best), Some(bar)) = (best, bar) {
        if best <= bar {
            notes.push(format!(
                "the best of these is a Sharpe of {best:.2}, and the best of {} results with no \
                 skill at all would be expected to reach {bar:.2} — picking the winner of this \
                 comparison is picking noise",
                sharpes.len(),
            ));
        }
    }
    if rows.iter().any(|row| row.stale == Some(true)) {
        notes.push(
            "at least one of these was produced from data that has since changed, so they were \
             not all measured on the same thing"
                .to_owned(),
        );
    }
    // Different instruments are the quiet way a comparison stops being one.
    let subjects: std::collections::HashSet<&str> =
        rows.iter().map(|row| row.subject.as_str()).collect();
    if subjects.len() > 1 {
        notes.push(
            "these are different instruments, so the differences between them are as much about \
             the instruments as about the rules"
                .to_owned(),
        );
    }

    Judged {
        best,
        bar,
        survived,
        notes,
    }
}

/// The evaluation and strategy name of a finding that can sit in a comparison
/// row, or `None` for one that cannot.
pub(crate) fn comparable(record: &Record) -> Option<(&arvo_research::Evaluation, String)> {
    match record {
        Record::Study(evidence) => Some((
            &evidence.out_of_sample_evidence.evaluation,
            evidence.selected.strategy.name.clone(),
        )),
        Record::WalkForward(evidence) => evidence.folds.first().map(|fold| {
            (
                &fold.out_of_sample_evidence.evaluation,
                format!("{} (rolling)", evidence.template.strategy.name),
            )
        }),
        Record::Panel(_) => None,
    }
}

/// Where exports are written.
pub const EXPORTS_SUBDIR: &str = "exports";

/// Writes a ledger to CSV and reveals it in the file manager.
///
/// A fixed directory beside the evidence rather than a save dialog: a dialog
/// needs another Tauri plugin and another capability, and the thing anyone
/// actually wants is the file, in a place they can find twice. Revealing it
/// with the opener already in the app is the whole of the "where did it go"
/// problem.
///
/// Quoting is real, not assumed away. A ledger holds timestamps and numbers
/// today, and the moment a strategy name or a note reaches a cell, an
/// unquoted writer silently shifts every column after it — the same failure
/// that ate a fund name in the portfolio importer.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the directory cannot be created, the
/// file cannot be written, or the file manager cannot be opened.
#[tauri::command]
pub fn export_trades(
    name: String,
    rows: Vec<TradeRowExport>,
    app: tauri::AppHandle,
    service: tauri::State<'_, ResearchService>,
) -> Result<String, CommandError> {
    use tauri_plugin_opener::OpenerExt as _;

    let directory = service.data_dir.parent().map_or_else(
        || service.data_dir.join(EXPORTS_SUBDIR),
        |root| root.join(EXPORTS_SUBDIR),
    );
    std::fs::create_dir_all(&directory)
        .map_err(|err| CommandError::Failed(format!("creating {}: {err}", directory.display())))?;

    // Slugged, because the name comes from an instrument id and a path
    // separator in it would write somewhere nobody asked for.
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = directory.join(format!("{slug}-trades.csv"));

    let out = trades_csv(&rows);

    std::fs::write(&path, out)
        .map_err(|err| CommandError::Failed(format!("writing {}: {err}", path.display())))?;

    // Reveal rather than open: a CSV opened in whatever owns the extension is
    // a spreadsheet nobody asked to launch.
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|err| CommandError::Failed(format!("showing {}: {err}", path.display())))?;
    Ok(path.display().to_string())
}

/// A ledger as CSV.
///
/// Quoting is real, not assumed away. The rows hold timestamps and numbers
/// today, and the moment a strategy name or a note reaches a cell an unquoted
/// writer silently shifts every column after it — the same failure that ate a
/// fund name in the portfolio importer, found only because a file that should
/// have held forty holdings held none.
pub(crate) fn trades_csv(rows: &[TradeRowExport]) -> String {
    let cell = |text: &str| {
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    };
    // An absent number is an empty cell, not a zero: a still-open position has
    // no exit price, and writing 0 there would read as a trade closed at zero.
    let number = |value: Option<f64>| value.map(|v| format!("{v}")).unwrap_or_default();

    let mut out = String::with_capacity(rows.len() * 96 + 128);
    // The instrument first, and always — including for a single study, where
    // every cell holds the same name. A column that appears only sometimes is
    // one a script reading these files has to detect, and a header that
    // changes shape between exports is the same silent column shift the
    // quoting above exists to prevent.
    out.push_str(
        "instrument,opened,closed,direction,quantity,entry,exit,pnl,commission,held_days,\
         exit_reason\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            cell(&row.instrument),
            cell(&row.opened),
            cell(&row.closed),
            cell(&row.direction),
            row.quantity,
            row.entry,
            number(row.exit),
            row.pnl,
            row.commission,
            number(row.held_days),
            cell(&row.exit_reason),
        ));
    }
    out
}

/// A row as it comes back from the view.
///
/// Deserialized rather than re-derived from the stored finding: the table
/// exports what is on screen, including whatever sort the reader applied. An
/// export that silently differed from the table above it would be worse than
/// none.
#[derive(serde::Deserialize)]
pub struct TradeRowExport {
    /// Which instrument the round trip was in.
    ///
    /// `default` because an ordinary study's rows do not carry one: the
    /// subject line already says it, and a column repeating the same name on
    /// every line is noise. On a book it is the difference between a usable
    /// export and a list of trades from three instruments with no way to tell
    /// them apart.
    #[serde(default)]
    pub instrument: String,
    pub opened: String,
    pub closed: String,
    pub direction: String,
    pub quantity: f64,
    pub entry: f64,
    pub exit: Option<f64>,
    pub pnl: f64,
    pub commission: f64,
    pub held_days: Option<f64>,
    pub exit_reason: String,
}

#[cfg(test)]
mod comparison_tests {
    use super::*;

    fn row(subject: &str, sharpe: f64) -> ComparisonRowView {
        ComparisonRowView {
            id: format!("{subject}-{sharpe}"),
            subject: subject.to_owned(),
            kind: "study".to_owned(),
            strategy_name: "sma_cross".to_owned(),
            verdict: "Not supported".to_owned(),
            recorded_at: "2026-01-01 00:00".to_owned(),
            total_return: 0.1,
            excess_return: -0.02,
            sharpe: Some(sharpe),
            max_drawdown: 0.1,
            trades: 40,
            win_rate: Some(0.4),
            profit_factor: Some(1.2),
            stale: Some(false),
        }
    }

    #[test]
    fn picking_the_best_of_several_is_a_search_of_that_size() {
        // The claim the whole screen exists to make. Each finding already
        // deflates the grid inside it; none of them knows it is one of six a
        // person is about to pick a winner from.
        //
        // The fixture used to be an evenly spaced ladder, 0.30 to 0.55 in
        // steps of 0.05, and it passed only because the null bar was
        // miscalibrated. A uniform ladder is not noise-shaped: its maximum
        // sits 1.34 standard deviations above its mean, where six normal
        // draws average 1.27. Corrected, that ladder *should* squeak past the
        // bar, and it does.
        //
        // So the fixture is now what it always claimed to be — a winner that
        // is not meaningfully ahead of the field it was picked from.
        let sharpes = [0.30, 0.40, 0.45, 0.50, 0.52, 0.55];
        let rows: Vec<_> = sharpes
            .iter()
            .map(|sharpe| row("MSFT.NASDAQ", *sharpe))
            .collect();
        let judged = judge_comparison(&rows, Vec::new());

        assert_eq!(judged.best, Some(0.55));
        let bar = judged.bar.expect("six results is enough to say");
        assert!(
            bar > 0.55,
            "a winner this close to its field is what a no-skill search of six \
             produces: bar {bar}"
        );
        assert!(!judged.survived);
        assert!(
            judged.notes.iter().any(|note| note.contains("picking noise")),
            "{:?}",
            judged.notes
        );
    }

    #[test]
    fn a_winner_that_is_genuinely_ahead_of_the_field_is_not_called_noise() {
        // The other half, and the half a bar set too high could never show.
        // Five results clustered near 0.3 and one at 1.2 is not somebody
        // getting lucky six times; refusing it would make the screen a thing
        // that only ever says no, which is as useless as one that only ever
        // says yes and considerably harder to notice.
        let sharpes = [0.28, 0.30, 0.31, 0.29, 0.32, 1.20];
        let rows: Vec<_> = sharpes
            .iter()
            .map(|sharpe| row("MSFT.NASDAQ", *sharpe))
            .collect();
        let judged = judge_comparison(&rows, Vec::new());

        let bar = judged.bar.expect("six results is enough to say");
        assert!(
            judged.survived,
            "best {:?} against a bar of {bar}",
            judged.best
        );
        assert!(
            !judged.notes.iter().any(|note| note.contains("picking noise")),
            "{:?}",
            judged.notes
        );
    }

    #[test]
    fn a_clear_winner_survives_the_same_bar() {
        // The check has to be passable, or it says nothing.
        let mut rows: Vec<_> = (0..6).map(|_| row("MSFT.NASDAQ", 0.10)).collect();
        rows.push(row("MSFT.NASDAQ", 2.5));
        let judged = judge_comparison(&rows, Vec::new());

        assert!(judged.survived, "bar {:?}", judged.bar);
        assert!(!judged.notes.iter().any(|note| note.contains("picking noise")));
    }

    #[test]
    fn two_findings_are_still_a_choice_between_two() {
        // `expected_best_under_null` needs at least two draws, which is
        // exactly the smallest comparison anyone would make.
        let rows = vec![row("MSFT.NASDAQ", 0.4), row("MSFT.NASDAQ", 0.42)];
        assert!(judge_comparison(&rows, Vec::new()).bar.is_some());
    }

    #[test]
    fn comparing_different_instruments_is_said_out_loud() {
        // The quiet way a comparison stops being one: the differences are then
        // as much about the instruments as about the rules.
        let rows = vec![row("MSFT.NASDAQ", 0.4), row("AAPL.NASDAQ", 0.9)];
        let judged = judge_comparison(&rows, Vec::new());
        assert!(
            judged.notes.iter().any(|note| note.contains("different instruments")),
            "{:?}",
            judged.notes
        );
    }

    #[test]
    fn a_stale_finding_is_flagged_rather_than_quietly_included() {
        let mut rows = vec![row("MSFT.NASDAQ", 0.4), row("MSFT.NASDAQ", 0.9)];
        rows[1].stale = Some(true);
        let judged = judge_comparison(&rows, Vec::new());
        assert!(
            judged.notes.iter().any(|note| note.contains("since changed")),
            "{:?}",
            judged.notes
        );
    }

    #[test]
    fn findings_with_no_sharpe_do_not_become_a_bar_of_their_own() {
        // A curve that never moved has no Sharpe. Treating that as a zero
        // would drag the no-skill bar down and make everything else look good.
        let mut rows = vec![row("MSFT.NASDAQ", 1.2)];
        rows.push(ComparisonRowView {
            sharpe: None,
            ..row("MSFT.NASDAQ", 0.0)
        });
        let judged = judge_comparison(&rows, Vec::new());
        assert_eq!(judged.best, Some(1.2));
        assert_eq!(judged.bar, None, "one measurable result is not a comparison");
    }
}

#[cfg(test)]
mod csv_tests {
    use super::*;

    fn row(reason: &str) -> TradeRowExport {
        TradeRowExport {
            instrument: "AAPL.NASDAQ".to_owned(),
            opened: "2024-01-02 00:00".to_owned(),
            closed: "2024-01-05 00:00".to_owned(),
            direction: "long".to_owned(),
            quantity: 100.0,
            entry: 10.5,
            exit: Some(11.25),
            pnl: 74.0,
            commission: 1.0,
            held_days: Some(3.0),
            exit_reason: reason.to_owned(),
        }
    }

    #[test]
    fn a_header_and_one_line_per_row() {
        let text = trades_csv(&[row("signal"), row("stop")]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("instrument,opened,closed,direction"));
        assert_eq!(lines[0].split(',').count(), 11);
        assert_eq!(lines[1].split(',').count(), 11);
    }

    #[test]
    fn a_comma_in_a_cell_does_not_shift_every_column_after_it() {
        // The failure this quoting exists for, and one this codebase has
        // already met: a naive comma split ate a quoted fund name in the
        // portfolio importer and silently produced an empty file.
        let text = trades_csv(&[row("stopped, then re-entered")]);
        let line = text.lines().nth(1).expect("one row");
        assert!(
            line.contains("\"stopped, then re-entered\""),
            "the cell must be quoted: {line}"
        );
    }

    #[test]
    fn a_quote_in_a_cell_is_doubled_rather_than_ending_the_field() {
        let mut awkward = row("signal");
        awkward.direction = "he said \"long\"".to_owned();
        let line = trades_csv(&[awkward]).lines().nth(1).expect("one row").to_owned();
        assert!(line.contains("\"he said \"\"long\"\"\""), "{line}");
    }

    #[test]
    fn an_open_position_writes_an_empty_cell_not_a_zero() {
        // A zero exit price reads as a trade closed at nothing, which is a
        // real-looking number for something that did not happen.
        let mut open = row("open");
        open.closed = String::new();
        open.exit = None;
        open.held_days = None;

        let line = trades_csv(&[open]).lines().nth(1).expect("one row").to_owned();
        let cells: Vec<&str> = line.split(',').collect();
        assert_eq!(cells[2], "", "no close time");
        assert_eq!(cells[6], "", "no exit price");
        assert_eq!(cells[9], "", "no holding period");
    }

    #[test]
    fn a_book_export_says_which_instrument_each_row_was_in() {
        // Without it the file is a list of round trips from several
        // instruments with no way to tell them apart, which is worse than
        // useless: it looks complete.
        let mut second = row("signal");
        second.instrument = "MSFT.NASDAQ".to_owned();
        let text = trades_csv(&[row("signal"), second]);
        let lines: Vec<&str> = text.lines().collect();

        assert!(lines[1].starts_with("AAPL.NASDAQ,"), "{}", lines[1]);
        assert!(lines[2].starts_with("MSFT.NASDAQ,"), "{}", lines[2]);
    }

    #[test]
    fn an_empty_ledger_is_a_header_and_nothing_else() {
        // Not an empty file: a spreadsheet opening a zero-byte CSV shows an
        // error, and the honest thing to say is "these are the columns, there
        // were no trades".
        let text = trades_csv(&[]);
        assert_eq!(text.lines().count(), 1);
    }
}
