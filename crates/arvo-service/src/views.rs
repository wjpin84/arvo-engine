//! The project, described to DuckDB (ADR-0039).
//!
//! A project is a folder of files, and the engine reads each of them by name.
//! A person has questions the engine was never asked: every refusal by reason,
//! what an agent ran last week, how wide SPY's options were on a Tuesday. The
//! engine links no database to answer them. It writes one file,
//! `.arvo/views.sql`, that names each store as a view, and DuckDB reads the
//! files where they lie:
//!
//! ```text
//! duckdb -init .arvo/views.sql
//! ```
//!
//! run from the project folder, since the paths in it are relative.
//!
//! # Why the file is generated
//!
//! DuckDB refuses a view over a pattern that matches nothing, and a store can
//! be in either of two forms while it moves from CSV to Parquet. So the file
//! is written from what is on disk: a view exists when its store does, and
//! reads whichever forms are there. A query written against `option_quotes`
//! keeps working on the day the quotes are compacted.
//!
//! # Reading only
//!
//! The engine is the only writer of a project's stores. Nothing here can
//! write, and nothing in the research tier runs SQL: this is for a person, a
//! script, or an agent with a shell, not a tool on the list (ADR-0016).

use std::path::{Path, PathBuf};

/// Where the views are written, under the project.
pub const FILE: &str = ".arvo/views.sql";

const HEADER: &str = "\
-- This project's stores, as DuckDB views (ADR-0039).
--
-- Written by arvo-engine from what is on disk, and rewritten when a store
-- appears or changes form. Edits here are overwritten.
--
-- From the project folder:   duckdb -init .arvo/views.sql
--
-- Read, do not write: the engine is the only writer of these files.
";

/// Whether `folder` directly holds a file with this extension.
fn holds(folder: &Path, extension: &str) -> bool {
    std::fs::read_dir(folder).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let path = entry.path();
            path.is_file() && path.extension().is_some_and(|ext| ext == extension)
        })
    })
}

/// Whether any folder directly under `parent` holds a file with this extension.
fn any_child_holds(parent: &Path, extension: &str) -> bool {
    std::fs::read_dir(parent)
        .is_ok_and(|entries| entries.flatten().any(|entry| entry.path().is_dir() && holds(&entry.path(), extension)))
}

/// The file's name without its folder or extension, in DuckDB's regex. The
/// class takes either slash, because DuckDB reports a Windows path as Windows
/// wrote it.
fn stem(extension: &str) -> String {
    format!(r"regexp_extract(filename, '([^/\\]+)\.{extension}$', 1)")
}

/// One resolution of the bar library, as a branch of the `bars` view.
fn bars_branch(pattern: &str, interval: &str) -> String {
    format!(
        "SELECT {} AS instrument, '{interval}' AS \"interval\", CAST(\"date\" AS TIMESTAMP) AS time,\n       \
         open, high, low, close, volume\n  \
         FROM read_csv('{pattern}', header = true, filename = true,\n       \
         columns = {{'date': 'VARCHAR', 'open': 'DOUBLE', 'high': 'DOUBLE', 'low': 'DOUBLE', 'close': 'DOUBLE', 'volume': 'DOUBLE'}})",
        stem("csv")
    )
}

/// The views for the stores `root` holds, as one SQL script.
#[must_use]
pub fn sql(root: &Path) -> String {
    let mut views: Vec<String> = Vec::new();
    let data = root.join(crate::research::DATA_SUBDIR);

    // Bars: the root of the library is daily, and each finer resolution is a
    // folder named for it.
    let mut bars = Vec::new();
    if holds(&data, "csv") {
        bars.push(bars_branch("data/*.csv", "1day"));
    }
    let mut finer: Vec<String> = std::fs::read_dir(&data)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir() && holds(&entry.path(), "csv"))
                .filter_map(|entry| entry.file_name().into_string().ok())
                // Every other folder in the library is a store of its own.
                .filter(|name| !matches!(name.as_str(), "dividends" | "options" | "signals"))
                .collect()
        })
        .unwrap_or_default();
    finer.sort();
    for interval in finer {
        bars.push(bars_branch(&format!("data/{interval}/*.csv"), &interval));
    }
    if !bars.is_empty() {
        views.push(format!(
            "-- Every bar in the library. `time` is the bar's open, in UTC.\nCREATE OR REPLACE VIEW bars AS\n{};",
            bars.join("\nUNION ALL\n")
        ));
    }

    if holds(&data.join("dividends"), "csv") {
        views.push(format!(
            "-- Cash dividends, by ex-date.\nCREATE OR REPLACE VIEW dividends AS\n\
             SELECT {} AS instrument, CAST(ex_date AS DATE) AS ex_date, amount\n  \
             FROM read_csv('data/dividends/*.csv', header = true, filename = true,\n       \
             columns = {{'ex_date': 'VARCHAR', 'amount': 'DOUBLE'}});",
            stem("csv")
        ));
    }

    // Option quotes: a finished day is Parquet, today is a CSV, and a
    // recording made before compaction existed may be all CSV.
    let quotes = root.join(crate::jobs::OPTION_QUOTES_SUBDIR);
    let chain = r"regexp_extract(filename, '([^/\\]+)[/\\][^/\\]+$', 1)";
    let columns = "recorded_at, symbol, expiration, \"right\" AS put_call, strike, quote_at,\n       \
                   bid, ask, bid_size, ask_size, underlying_bid, underlying_ask";
    let mut forms = Vec::new();
    if any_child_holds(&quotes, "parquet") {
        forms.push(format!(
            "SELECT {chain} AS chain, {columns}\n  FROM read_parquet('option-quotes/*/*.parquet', filename = true)"
        ));
    }
    if any_child_holds(&quotes, "csv") {
        // `ignore_errors`: a crash can tear the last line of today's file.
        forms.push(format!(
            "SELECT {chain} AS chain, {columns}\n  \
             FROM read_csv('option-quotes/*/*.csv', header = true, filename = true, ignore_errors = true,\n       \
             columns = {{'recorded_at': 'TIMESTAMPTZ', 'symbol': 'VARCHAR', 'expiration': 'DATE', 'right': 'VARCHAR',\n                  \
             'strike': 'DOUBLE', 'quote_at': 'TIMESTAMPTZ', 'bid': 'DOUBLE', 'ask': 'DOUBLE', 'bid_size': 'DOUBLE',\n                  \
             'ask_size': 'DOUBLE', 'underlying_bid': 'DOUBLE', 'underlying_ask': 'DOUBLE'}})"
        ));
    }
    if !forms.is_empty() {
        views.push(format!(
            "-- Recorded option chains. `chain` is the underlying and the feed, e.g. SPY.indicative.\n\
             -- A quote that had not changed by the next snapshot appears in both.\n\
             CREATE OR REPLACE VIEW option_quotes AS\n{};",
            forms.join("\nUNION ALL\n")
        ));
    }

    // Findings: one JSON file each. The index beside them is a cache, and its
    // name does not begin with a digit.
    if holds(&root.join(crate::research::EVIDENCE_SUBDIR), "json") {
        views.push(
            "-- Every finding's record. `record` is the whole of it, for anything not lifted into a column.\n\
             CREATE OR REPLACE VIEW findings AS\n\
             SELECT id, CAST(recorded_at AS TIMESTAMPTZ) AS recorded_at,\n       \
             json_extract_string(record, '$.kind') AS kind,\n       \
             json_extract_string(record, '$.verdict') AS verdict,\n       \
             json_extract_string(record, '$.hypothesis') AS hypothesis,\n       \
             coalesce(json_extract_string(author, '$.id'), json_extract_string(author, '$.by'), 'person') AS author,\n       \
             CAST(json_extract(record, '$.selection.trials') AS INTEGER) AS trials,\n       \
             json_extract_string(record, '$.dataset.id') AS dataset,\n       \
             json_extract_string(record, '$.dataset.version') AS dataset_version,\n       \
             json_extract_string(provenance, '$.code_commit') AS code_commit,\n       \
             json_extract_string(provenance, '$.ruleset.name') AS ruleset,\n       \
             CAST(json_extract(record, '$.reasons') AS VARCHAR[]) AS reasons,\n       \
             json_extract_string(artifact, '$.hash') AS artifact,\n       \
             record\n  \
             FROM read_json('evidence/[0-9]*.json', maximum_object_size = 268435456,\n       \
             columns = {'id': 'VARCHAR', 'recorded_at': 'VARCHAR', 'author': 'JSON', 'provenance': 'JSON', 'artifact': 'JSON', 'record': 'JSON'});"
                .to_owned(),
        );
    }

    // Curves: a finding's series are kept apart from its record, one Parquet
    // file per artifact, named by a hash of what it holds (ADR-0037). A
    // finding written before that carries its curves in `record` and has no
    // row here until the store is rewritten.
    let artifacts = root.join(crate::research::EVIDENCE_SUBDIR).join(arvo_research::memory::artifact::ARTIFACTS_SUBDIR);
    if holds(&artifacts, "parquet") {
        views.push(format!(
            "-- Every equity curve, by the artifact it is kept in: join to findings on `artifact`.\n\
             -- `series` is where in the record the curve belongs.\n\
             CREATE OR REPLACE VIEW curves AS\n\
             SELECT {} AS artifact, series, \"at\" AS time, value AS equity\n  \
             FROM read_parquet('evidence/artifacts/*.parquet', filename = true);",
            stem("parquet")
        ));
    }

    if holds(&root.join("sessions"), "jsonl") {
        views.push(format!(
            "-- Every line of every session's record, in the order it was written.\n\
             CREATE OR REPLACE VIEW sessions AS\n\
             SELECT {} AS session, CAST(\"at\" AS TIMESTAMPTZ) AS time, event, detail\n  \
             FROM read_json('sessions/*.jsonl', format = 'newline_delimited', filename = true,\n       \
             columns = {{'at': 'VARCHAR', 'event': 'VARCHAR', 'detail': 'JSON'}});",
            stem("jsonl")
        ));
    }

    if root.join("agent-audit.jsonl").is_file() {
        views.push(
            "-- What an agent or a script asked the engine to do.\n\
             CREATE OR REPLACE VIEW audit AS\n\
             SELECT CAST(\"at\" AS TIMESTAMPTZ) AS time, agent, via, tool, arguments, ok, finding, error\n  \
             FROM read_json('agent-audit.jsonl', format = 'newline_delimited',\n       \
             columns = {'at': 'VARCHAR', 'agent': 'VARCHAR', 'via': 'VARCHAR', 'tool': 'VARCHAR', 'arguments': 'JSON',\n                  \
             'ok': 'BOOLEAN', 'finding': 'VARCHAR', 'error': 'VARCHAR'});"
                .to_owned(),
        );
    }

    if views.is_empty() {
        return format!("{HEADER}\n-- Nothing is stored here yet, so there is nothing to name.\n");
    }
    format!("{HEADER}\n{}\n", views.join("\n\n"))
}

/// Writes the views for `root`, leaving the file alone when it already says
/// the same thing.
///
/// # Errors
///
/// The folder or the file could not be written.
pub fn write(root: &Path) -> std::io::Result<PathBuf> {
    let path = root.join(FILE);
    let text = sql(root);
    if std::fs::read_to_string(&path).is_ok_and(|held| held == text) {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("folder");
        std::fs::write(path, "x").expect("write");
    }

    #[test]
    fn a_view_exists_when_its_store_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = sql(dir.path());
        assert!(!empty.contains("CREATE"), "a view over nothing is an error in DuckDB: {empty}");

        put(dir.path(), "data/AAPL.YF.csv");
        put(dir.path(), "data/5minute/AAPL.AIEX.csv");
        put(dir.path(), "data/dividends/AAPL.YF.csv");
        put(dir.path(), "evidence/20260914T170439317-TQQQ.RH.json");
        put(dir.path(), "sessions/20260921T134031540-AAPL_AIEX_alpaca-paper.jsonl");
        let text = sql(dir.path());
        for view in ["bars", "dividends", "findings", "sessions"] {
            assert!(text.contains(&format!("CREATE OR REPLACE VIEW {view} AS")), "{view} in {text}");
        }
        assert!(text.contains("'data/*.csv'") && text.contains("'1day'"));
        assert!(text.contains("'data/5minute/*.csv'") && text.contains("'5minute'"));
        assert!(!text.contains("'dividends' AS \"interval\""), "dividends are a store of their own, not a resolution");
        assert!(!text.contains("VIEW option_quotes"), "nothing recorded yet");
        assert!(!text.contains("VIEW audit"), "no agent has asked anything yet");
        assert!(!text.contains("VIEW curves"), "a store written before artifacts has no curves beside it");

        // A finding's curves are kept beside its record, and joined on the
        // hash the record names.
        put(dir.path(), "evidence/artifacts/0f3a.parquet");
        let text = sql(dir.path());
        assert!(text.contains("CREATE OR REPLACE VIEW curves AS"));
        assert!(text.contains("json_extract_string(artifact, '$.hash') AS artifact"));
    }

    #[test]
    fn a_store_that_moves_from_csv_to_parquet_keeps_its_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let csv = "read_csv('option-quotes/*/*.csv'";
        let parquet = "read_parquet('option-quotes/*/*.parquet'";

        // Before compaction: every day is a CSV.
        put(dir.path(), "option-quotes/SPY.indicative/2026-10-02.csv");
        put(dir.path(), "option-quotes/SPY.indicative/2026-10-05.csv");
        let before = sql(dir.path());
        assert!(before.contains("VIEW option_quotes") && before.contains(csv) && !before.contains(parquet));

        // A finished day is compacted, and today is still being written.
        std::fs::rename(
            dir.path().join("option-quotes/SPY.indicative/2026-10-02.csv"),
            dir.path().join("option-quotes/SPY.indicative/2026-10-02.parquet"),
        )
        .expect("rename");
        let during = sql(dir.path());
        assert!(during.contains(csv) && during.contains(parquet) && during.contains("UNION ALL"));

        // The same columns under the same names in both forms, so a query
        // written against the view does not care which it is reading.
        let select = "recorded_at, symbol, expiration, \"right\" AS put_call, strike, quote_at";
        assert_eq!(during.matches(select).count(), 2);

        // Written once, and left alone while it says the same thing.
        let path = write(dir.path()).expect("written");
        let stamp = std::fs::metadata(&path).expect("meta").modified().expect("mtime");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), during);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(dir.path()).expect("unchanged");
        assert_eq!(std::fs::metadata(&path).expect("meta").modified().expect("mtime"), stamp);
    }
}
