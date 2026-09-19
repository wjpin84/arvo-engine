//! Stored findings: listing them, reopening one, re-running it, comparing
//! several, and writing a ledger out as CSV.
//!
//! The claim `Experiment` makes about itself — that its fields are everything
//! needed to reproduce a run — is the one the whole evidence store rests on.
//! [`replay_record`] is what checks it.

use super::*;

/// What the data behind a finding hashes to *now*, or `None` if it is gone.
///
/// From the summary, so staleness costs no parsing either. A panel's identity
/// is every member's hash combined and is recomputed the same way it was
/// produced — over the instruments present today, so one added or removed also
/// reads as stale, which is correct: the panel would not run the same twice.
pub fn live_version(service: &ResearchService, summary: &arvo_research::Summary) -> Option<String> {
    match (&summary.instrument, summary.interval) {
        // A study of an option rule: the bars and the chain, as it was made.
        (Some(instrument), Some(interval))
            if summary.dataset_version.starts_with(super::study::CHAIN_VERSION) =>
        {
            let fingerprint = service.bars.fingerprint(instrument, interval).ok().flatten()?;
            let symbol = instrument.split('.').next().unwrap_or_default();
            let chain = service
                .bars
                .option_chain_fingerprint(symbol, interval)
                .ok()
                .flatten()?;
            Some(super::study::chain_dataset_version(&fingerprint, &chain))
        }
        (Some(instrument), Some(interval)) if summary.alongside.is_empty() => {
            service.bars.fingerprint(instrument, interval).ok().flatten()
        }
        // A book: recomputed the way `run_book` produced it. A member whose
        // data is gone makes the whole book's data gone.
        (Some(instrument), Some(interval)) => {
            let members: Vec<String> = std::iter::once(instrument.clone())
                .chain(summary.alongside.iter().cloned())
                .collect();
            let fingerprints = members
                .iter()
                .map(|member| service.bars.fingerprint(member, interval).ok().flatten())
                .collect::<Option<Vec<String>>>()?;
            Some(super::study::book_dataset_version(&members, &fingerprints))
        }
        _ => panel_dataset_version(&service.bars).map(|(version, _, _, _)| version),
    }
}

/// A stored finding as the window shows it, whatever kind it is.
pub fn record_view(service: &ResearchService, stored: arvo_research::StoredRecord) -> RecordView {
    let engine = service.simulation.engine();
    let (id, recorded_at, author) = (
        stored.id.clone(),
        stored.recorded_at.to_rfc3339(),
        stored.author.agent().unwrap_or_default().to_owned(),
    );
    let attachments: Vec<arvo_views::AttachmentView> = stored.attachments.iter().map(attachment_view).collect();
    match stored.record {
        // The view carries the id it was read from, so a tab opened out of
        // History can ask for a report of the record it is showing (#159).
        Record::Study(evidence) => {
            let mut view = study_view(&evidence, &service.bars, engine);
            view.id = id;
            RecordView::Study(Box::new(view))
        }
        Record::Panel(evidence) => {
            let mut view = panel_view(&evidence, engine);
            view.id = id;
            RecordView::Panel(Box::new(view))
        }
        Record::WalkForward(evidence) => {
            let mut view = walk_forward_view(&evidence, &service.bars, engine);
            view.id = id;
            RecordView::WalkForward(Box::new(view))
        }
        Record::Reported(evidence) => {
            use arvo_research::Judgement;
            let experiment = &evidence.reported.experiment;
            let evaluated = match &evidence.judgement {
                Judgement::Evaluated(evaluation) => Some(evaluation.as_ref()),
                Judgement::Inconclusive { .. } => None,
            };
            RecordView::Reported(Box::new(arvo_views::ReportedView {
                id,
                hypothesis: evidence.reported.hypothesis.to_string(),
                claim: evidence.claim.clone(),
                instrument: experiment.instrument.clone(),
                engine: evidence.reported.engine.clone(),
                from: experiment.window.from.to_string(),
                to: experiment.window.to.to_string(),
                interval: experiment.interval.to_string(),
                dataset: format!("{}@{}", experiment.dataset.id, experiment.dataset.version),
                strategy: experiment.strategy.name.clone(),
                verdict: format!("{:?}", evidence.verdict),
                reasons: evidence.reasons.clone(),
                trades: arvo_research::TradeStats::from_ledger(&evidence.reported.strategy_ledger).closed,
                trials: evidence.reported.trials,
                total_return: evaluated.map(|e| e.strategy.total_return),
                excess_return: evaluated.map(|e| e.excess_return),
                sharpe: evaluated.and_then(|e| e.strategy.sharpe),
                max_drawdown: evaluated.map(|e| e.strategy.max_drawdown),
                recorded_at,
                author,
                attachments,
            }))
        }
    }
}

pub fn attachment_view(kept: &arvo_research::memory::Attachment) -> arvo_views::AttachmentView {
    arvo_views::AttachmentView {
        name: kept.name.clone(),
        media_type: kept.media_type.clone(),
        hash: kept.hash.clone(),
        bytes: kept.bytes,
        added_at: kept.added_at.to_rfc3339(),
    }
}

/// `C:\proj\scans\a.py:12` as (`scans/a.py`, 12) when the file is under
/// `root`, else nothing. The file is resolved the way the project resolves
/// its own, so a script invoked by a differently-cased path still matches.
#[must_use]
pub fn origin_in(origin: &str, root: &std::path::Path) -> Option<(String, u32)> {
    let (file, line) = origin.rsplit_once(':')?;
    let line: u32 = line.trim().parse().ok()?;
    let file = std::path::PathBuf::from(file.trim());
    let file = dunce::canonicalize(&file).unwrap_or(file);
    let relative = file.strip_prefix(root).ok()?;
    let relative = relative
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!relative.is_empty()).then_some((relative, line))
}

/// What a finding says that should sit beside the code that produced it:
/// the verdict, then every warning and recommendation, each with the
/// severity the Problems panel understands.
pub fn caveats(view: &RecordView) -> Vec<(String, String)> {
    let (verdict, reasons, recommendations): (&str, &[String], &[RecommendationView]) = match view {
        RecordView::Study(study) => (&study.verdict, &study.reasons, &study.recommendations),
        RecordView::WalkForward(walk) => (&walk.verdict, &[], &walk.recommendations),
        RecordView::Panel(panel) => (&panel.verdict, &[], &panel.recommendations),
        // No recommendations: those read a study's selection, and a reported
        // finding has none. The verdict and its reasons are the caveats.
        RecordView::Reported(reported) => (&reported.verdict, &reported.reasons, &[]),
    };
    let mut out = vec![("info".to_owned(), format!("Verdict: {verdict}"))];
    out.extend(reasons.iter().map(|reason| ("info".to_owned(), reason.clone())));
    out.extend(recommendations.iter().map(|item| {
        let severity = match item.severity.to_lowercase() {
            s if s.starts_with("block") => "error",
            s if s.starts_with("warn") => "warning",
            _ => "info",
        };
        (severity.to_owned(), format!("{}: {}", item.finding, item.action))
    }));
    out
}

pub fn replay_view(outcome: &arvo_research::Replay) -> ReplayView {
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
pub fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// What a comparison is worth, and what is wrong with it.
pub struct Judged {
    pub best: Option<f64>,
    pub bar: Option<f64>,
    pub survived: bool,
    pub notes: Vec<String>,
}

/// Applies the same multiple-testing bar to the comparison that each study
/// applies to its own grid.
///
/// Separated from the command so it can be tested: this is the claim the whole
/// screen exists to make, and a comparison table that silently sorted by
/// return would be the most persuasive way this application could mislead
/// someone.
pub fn judge_comparison(rows: &[ComparisonRowView], mut notes: Vec<String>) -> Judged {
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
pub fn comparable(record: &Record) -> Option<(&arvo_research::Evaluation, String)> {
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
        Record::Reported(evidence) => match &evidence.judgement {
            arvo_research::Judgement::Evaluated(evaluation) => {
                Some((evaluation.as_ref(), format!("{} (reported by {})", evidence.reported.experiment.strategy.name, evidence.reported.engine)))
            }
            arvo_research::Judgement::Inconclusive { .. } => None,
        },
        Record::Panel(_) => None,
    }
}

/// Where exports are written.
pub const EXPORTS_SUBDIR: &str = "exports";

/// The PNG behind what the window captured.
///
/// A canvas hands back a data URL — `data:image/png;base64,...` — and raw
/// base64 is just as good an answer; the payload is what is wanted either
/// way.
pub fn figure_bytes(encoded: &str) -> Result<Vec<u8>, CommandError> {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD
        .decode(encoded.rsplit(',').next().unwrap_or_default())
        .map_err(|err| CommandError::Failed(format!("the figure is not base64: {err}")))
}

/// Where `name` will be written in the exports directory, which is created
/// if it is not there.
///
/// The name is slugged: it comes from an instrument id, and a path separator
/// in it would write somewhere nobody asked for.
pub fn export_path(service: &ResearchService, name: &str) -> Result<std::path::PathBuf, CommandError> {
    let directory = service.data_dir.parent().map_or_else(
        || service.data_dir.join(EXPORTS_SUBDIR),
        |root| root.join(EXPORTS_SUBDIR),
    );
    std::fs::create_dir_all(&directory)
        .map_err(|err| CommandError::Failed(format!("creating {}: {err}", directory.display())))?;
    Ok(directory.join(export_name(name)?))
}

/// `name` with everything that is not a letter, a digit or a dot replaced.
pub fn export_name(name: &str) -> Result<String, CommandError> {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' })
        .collect();
    // Dots survive for the extension, so a name that is only dots could still
    // name the parent directory. It never gets that far.
    if slug.trim_matches('.').is_empty() {
        return Err(CommandError::Failed(format!("{name:?} is not a usable file name")));
    }
    Ok(slug)
}

/// A ledger as CSV.
///
/// Quoting is real, not assumed away. The rows hold timestamps and numbers
/// today, and the moment a strategy name or a note reaches a cell an unquoted
/// writer silently shifts every column after it — the same failure that ate a
/// fund name in the portfolio importer, found only because a file that should
/// have held forty holdings held none.
pub fn trades_csv(rows: &[TradeRowExport]) -> String {
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

#[cfg(test)]
mod staleness_tests {
    use super::*;

    fn write(dir: &std::path::Path, instrument: &str, close: f64) {
        std::fs::write(
            dir.join(format!("{instrument}.csv")),
            format!(
                "date,open,high,low,close,volume\n2024-01-02,{close},{close},{close},{close},100\n\
                 2024-01-03,{close},{close},{close},{close},100\n"
            ),
        )
        .expect("fixture writes");
    }

    fn book(service: &ResearchService) -> arvo_research::Summary {
        let members = vec!["AAPL.SIM".to_owned(), "MSFT.SIM".to_owned()];
        let fingerprints: Vec<String> = members
            .iter()
            .map(|member| {
                service
                    .bars
                    .fingerprint(member, arvo_data::BarInterval::DAILY)
                    .expect("reads")
                    .expect("has bars")
            })
            .collect();
        arvo_research::Summary {
            attachments: 0,
            id: "book".to_owned(),
            recorded_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid"),
            kind: "study".to_owned(),
            subject: "AAPL.SIM".to_owned(),
            verdict: arvo_research::Verdict::NotSupported,
            hypothesis: arvo_research::HypothesisId::from("h"),
            dataset_version: super::super::study::book_dataset_version(&members, &fingerprints),
            instrument: Some("AAPL.SIM".to_owned()),
            interval: Some(arvo_data::BarInterval::DAILY),
            alongside: vec!["MSFT.SIM".to_owned()],
            agent: None,
            origin: None,
            trials: None,
        }
    }

    #[test]
    fn an_origin_under_the_project_becomes_a_relative_site() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("real");
        std::fs::create_dir(root.join("scans")).unwrap();
        std::fs::write(root.join("scans").join("a.py"), "").unwrap();
        let origin = format!("{}:12", root.join("scans").join("a.py").display());
        assert_eq!(origin_in(&origin, &root), Some(("scans/a.py".to_owned(), 12)));
        assert_eq!(origin_in("C:/elsewhere/b.py:3", &root), None);
        assert_eq!(origin_in("nonsense", &root), None);
    }

    #[test]
    fn a_book_is_current_until_one_of_its_members_changes() {
        // It used to be checked against its head instrument's hash alone, so
        // every book read as stale from the moment it was recorded, and no
        // book could ever be replayed.
        let dir = tempfile::tempdir().expect("tempdir");
        let (data, evidence) = (dir.path().join("data"), dir.path().join("evidence"));
        std::fs::create_dir_all(&data).expect("dir");
        write(&data, "AAPL.SIM", 100.0);
        write(&data, "MSFT.SIM", 200.0);
        let service = ResearchService::new(data.clone(), evidence);

        let summary = book(&service);
        assert_eq!(
            live_version(&service, &summary).as_deref(),
            Some(summary.dataset_version.as_str()),
            "an untouched book is current"
        );

        write(&data, "MSFT.SIM", 201.0);
        assert_ne!(
            live_version(&service, &summary).as_deref(),
            Some(summary.dataset_version.as_str()),
            "a member that is not the head changed, and the book is stale"
        );

        std::fs::remove_file(data.join("MSFT.SIM.csv")).expect("removes");
        assert_eq!(live_version(&service, &summary), None, "and with a member gone, so is its data");
    }

    #[test]
    fn a_captured_figure_is_read_from_a_data_url_or_from_plain_base64() {
        // "png" — the bytes do not matter here, only that both spellings of
        // what the window sends arrive as the same file.
        let from_url = figure_bytes("data:image/png;base64,cG5n").expect("a data url");
        assert_eq!(from_url, b"png");
        assert_eq!(figure_bytes("cG5n").expect("plain base64"), from_url);
        assert!(figure_bytes("not base64!").is_err(), "and what is not base64 is refused");
    }

    #[test]
    fn an_export_name_cannot_name_a_directory() {
        assert_eq!(export_name("study-SPY.RH.report.md").expect("a name"), "study-SPY.RH.report.md");
        assert_eq!(export_name("../../evil.md").expect("a name"), "..-..-evil.md");
        assert!(export_name("..").is_err(), "a name that is only dots is refused");
    }
}

/// Everything in research memory, newest first, from the store's summaries:
/// a stored finding is around 400 KB and this renders a column of names.
/// Findings that could not be read come back too, rather than as a log line.
///
/// # Errors
///
/// The directory cannot be listed.
pub fn list_history(service: &ResearchService) -> Result<HistoryView, CommandError> {
    let (summaries, unreadable) = service.memory.summaries().map_err(|err| CommandError::Failed(err.to_string()))?;
    Ok(HistoryView {
        entries: summaries
            .iter()
            .map(|summary| {
                let live = live_version(service, summary);
                HistoryEntryView {
                    id: summary.id.clone(),
                    kind: summary.kind.clone(),
                    subject: summary.subject.clone(),
                    verdict: verdict_label(summary.verdict).to_owned(),
                    recorded_at: summary.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
                    stale: live.map(|live| live != summary.dataset_version),
                    agent: summary.agent.clone(),
                    origin: summary.origin.clone(),
                    trials: summary.trials,
                    attachments: summary.attachments,
                }
            })
            .collect(),
        unreadable: unreadable.into_iter().map(|item| UnreadableView { id: item.id, reason: item.reason }).collect(),
    })
}

/// The files kept with a finding (#157).
///
/// # Errors
///
/// No such finding.
pub fn list_attachments(service: &ResearchService, id: &str) -> Result<Vec<arvo_views::AttachmentView>, CommandError> {
    let stored = service.memory.open(id).map_err(|err| CommandError::Failed(err.to_string()))?;
    Ok(stored.attachments.iter().map(attachment_view).collect())
}

/// The caveats of the latest finding at every script line under `root` that
/// asked for a run, for the Problems panel. Nothing for findings whose origin
/// is elsewhere or unknown.
///
/// # Errors
///
/// The findings store cannot be read.
pub fn list_research_problems(
    service: &ResearchService,
    root: &std::path::Path,
) -> Result<Vec<arvo_views::ResearchProblemView>, CommandError> {
    let (summaries, _) = service.memory.summaries().map_err(|err| CommandError::Failed(err.to_string()))?;
    // The latest finding per call site: a line re-run replaces what it said.
    let mut latest: std::collections::HashMap<(String, u32), &arvo_research::Summary> = std::collections::HashMap::new();
    for summary in &summaries {
        let Some(site) = summary.origin.as_deref().and_then(|origin| origin_in(origin, root)) else { continue };
        let slot = latest.entry(site).or_insert(summary);
        if summary.recorded_at > slot.recorded_at {
            *slot = summary;
        }
    }
    if latest.is_empty() {
        return Ok(Vec::new());
    }
    let loaded = service.memory.load().map_err(|err| CommandError::Failed(err.to_string()))?;
    let mut out = Vec::new();
    for ((path, line), summary) in latest {
        let Some(stored) = loaded.records.iter().find(|stored| stored.id == summary.id) else { continue };
        let view = record_view(service, stored.clone());
        out.extend(caveats(&view).into_iter().map(|(severity, message)| arvo_views::ResearchProblemView {
            path: path.clone(),
            line,
            severity,
            message,
            finding: summary.id.clone(),
        }));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

/// Reopens one stored finding.
///
/// # Errors
///
/// The store cannot be read, or no such finding.
pub fn open_record(service: &ResearchService, id: &str) -> Result<RecordView, CommandError> {
    let loaded = service.memory.load().map_err(|err| CommandError::Failed(err.to_string()))?;
    let stored = loaded
        .records
        .into_iter()
        .find(|stored| stored.id == id)
        .ok_or_else(|| CommandError::Failed(format!("no stored finding {id:?}")))?;
    Ok(record_view(service, stored))
}

/// Runs a stored finding again and reports whether it still comes out the
/// same. Blocking: one engine run. A finding whose numbers cannot be
/// regenerated is not evidence; it is a screenshot of a number.
///
/// # Errors
///
/// No such finding.
pub fn replay_record(service: &ResearchService, id: &str) -> Result<ReplayView, CommandError> {
    let stored = service.memory.open(id).map_err(|err| CommandError::Failed(err.to_string()))?;
    // Hashed here rather than inside the replay: what the data is now is a
    // question about this machine, and the research crate has no filesystem.
    let live = live_version(service, &stored.summary());
    let outcome = arvo_research::replay(service.simulation.as_ref(), &stored.record, live.as_deref());
    Ok(replay_view(&outcome))
}

/// Reads several stored findings against each other, and counts the choice
/// among them as the search it is: the best of six no-skill searches still
/// looks better than the average of them.
///
/// # Errors
///
/// A named finding cannot be read.
pub fn compare_records(service: &ResearchService, ids: &[String]) -> Result<ComparisonView, CommandError> {
    let mut rows = Vec::with_capacity(ids.len());
    let mut curves = Vec::with_capacity(ids.len());
    let mut notes = Vec::new();
    for id in ids {
        let stored = service.memory.open(id).map_err(|err| CommandError::Failed(err.to_string()))?;
        let summary = stored.summary();
        let Some((evaluation, strategy_name)) = comparable(&stored.record) else {
            // A panel is one configuration across many instruments; a study
            // is one instrument. The same table would invite reading one
            // number against the other, and they are not the same number.
            notes.push(format!("{} is a panel and is not comparable row-for-row with a single study", summary.subject));
            continue;
        };
        let live = live_version(service, &summary);
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
