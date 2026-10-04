//! Which underlyings the engine records option chains for (#230).
//!
//! No vendor serves an option quote after the fact (#83), so a chain that was
//! not recorded on the day is gone. That makes this list the whole of what
//! the option rules can ever be studied on, and it is chosen the way a
//! universe is (#227): for liquidity, stated out loud, never for returns.
//!
//! The file is `option-quotes/underlyings.json`:
//!
//! ```json
//! {
//!   "reason": "The most liquid US option markets: tight spreads and daily or weekly expiries.",
//!   "underlyings": [
//!     { "symbol": "SPY", "why": "the deepest option book there is; daily expiries" },
//!     { "symbol": "QQQ", "why": "the same for the Nasdaq 100; daily expiries" }
//!   ]
//! }
//! ```
//!
//! Absent, the engine records SPY alone, which is what it did before this
//! existed and is the honest default: one underlying whose chain is certain
//! to be worth having.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The default when the project names no list: the one underlying the engine
/// has always recorded.
pub const DEFAULT: &str = "SPY";

/// The file, under the option-quotes folder.
pub const FILE: &str = "underlyings.json";

/// The underlyings to record, and why each is there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Underlyings {
    /// How the list was chosen, as a whole. Required, for the reason a
    /// universe's is: a list chosen by looking at returns is the search the
    /// platform exists to deflate.
    pub reason: String,
    pub underlyings: Vec<Underlying>,
}

/// One underlying, and what it is doing on the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Underlying {
    pub symbol: String,
    /// Why this one: the liquidity, the expiries. Not its returns.
    #[serde(default)]
    pub why: String,
}

/// The symbols to record under `root`, in the order the file gives them.
///
/// A file that cannot be read or cannot be used leaves the default in place
/// and says why, because recording nothing is worse than recording SPY: the
/// day's chains cannot be fetched again tomorrow.
#[must_use]
pub fn wanted(root: &Path) -> (Vec<String>, Option<String>) {
    let path = root.join(crate::jobs::OPTION_QUOTES_SUBDIR).join(FILE);
    if !path.is_file() {
        return (vec![DEFAULT.to_owned()], None);
    }
    match read(&path) {
        Ok(listed) => (listed, None),
        Err(why) => (vec![DEFAULT.to_owned()], Some(format!("{why}; recording {DEFAULT} alone"))),
    }
}

/// Reads and checks the list.
///
/// # Errors
///
/// Unreadable, unparseable, no reason, no symbols, a symbol twice, or a
/// symbol that is not a plain ticker.
pub fn read(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let listed: Underlyings = serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if listed.reason.trim().is_empty() {
        return Err(format!("{}: says no reason; a list chosen without one is a search waiting to happen", path.display()));
    }
    if listed.underlyings.is_empty() {
        return Err(format!("{}: lists no underlyings", path.display()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for under in &listed.underlyings {
        let symbol = under.symbol.trim();
        if symbol.is_empty() || !symbol.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
            return Err(format!("{}: {:?} is not a ticker", path.display(), under.symbol));
        }
        if !seen.insert(symbol.to_owned()) {
            return Err(format!("{}: {symbol} is listed twice", path.display()));
        }
    }
    Ok(listed.underlyings.iter().map(|under| under.symbol.trim().to_owned()).collect())
}

/// How much a day of an underlying's chains costs on disk, in bytes (#230).
///
/// One of the three numbers that decide whether the library needs a
/// different store (#200); the others come from the universes.
#[must_use]
pub fn bytes_today(root: &Path, symbol: &str, day: chrono::NaiveDate) -> u64 {
    // A chain is kept as `option-quotes/<SYMBOL>.<feed>/<day>.csv`, so the
    // underlying is the folder and the day is the file inside it. One
    // underlying may have more than one feed's folder.
    let dir = root.join(crate::jobs::OPTION_QUOTES_SUBDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let prefix = format!("{symbol}.");
    let file = format!("{}.csv", day.format("%Y-%m-%d"));
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name == symbol || name.starts_with(&prefix)
        })
        .filter_map(|entry| std::fs::metadata(entry.path().join(&file)).ok().map(|meta| meta.len()))
        .sum()
}

/// The chain folders under `root`, one per underlying and feed, sorted.
fn chain_folders(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join(crate::jobs::OPTION_QUOTES_SUBDIR)) else { return Vec::new() };
    let mut folders: Vec<_> = entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
    folders.sort();
    folders
}

/// The days a chain folder holds, oldest first, each in whichever form it is
/// in. A day in both forms is one a compaction was interrupted on, after the
/// Parquet file was proven: the Parquet file is the day.
fn days_in(folder: &Path) -> Vec<(chrono::NaiveDate, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut days = std::collections::BTreeMap::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        let compacted = match path.extension().and_then(|ext| ext.to_str()) {
            Some("parquet") => true,
            Some("csv") => false,
            _ => continue,
        };
        let Some(day) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok())
        else {
            continue;
        };
        if compacted || !days.contains_key(&day) {
            days.insert(day, path);
        }
    }
    days.into_iter().collect()
}

/// What one pass of compaction did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub days: usize,
    /// Snapshots left out because they repeated the one before them.
    pub repeats: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// A day that could not be compacted, and why. Its CSV is still there.
    pub failed: Vec<String>,
}

impl Compaction {
    #[must_use]
    pub fn describe(&self) -> String {
        let megabytes = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        let mut said = if self.days == 0 {
            "no finished day left as CSV".to_owned()
        } else {
            format!(
                "{} day(s) compacted, {:.1} MB to {:.1} MB, {} repeated snapshot(s) left out",
                self.days,
                megabytes(self.bytes_before),
                megabytes(self.bytes_after),
                self.repeats
            )
        };
        for failure in &self.failed {
            said.push_str("; ");
            said.push_str(failure);
        }
        said
    }
}

/// Rewrites every finished day's CSV as Parquet (ADR-0039).
///
/// A day is finished when it is before `today`: the recorder appends to
/// today's file, and a Parquet file cannot be appended to. A day that fails is
/// named and left as it was, and the rest are still done.
#[must_use]
pub fn compact_finished(root: &Path, today: chrono::NaiveDate) -> Compaction {
    let mut done = Compaction::default();
    for folder in chain_folders(root) {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        let mut finished: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
            .filter(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok())
                    .is_some_and(|day| day < today)
            })
            .collect();
        finished.sort();
        for csv in finished {
            match arvo_data::quotes::compact(&csv) {
                Ok(compacted) => {
                    done.days += 1;
                    done.repeats += compacted.repeats;
                    done.bytes_before += compacted.bytes_before;
                    done.bytes_after += compacted.bytes_after;
                }
                Err(err) => done.failed.push(format!("{}: {err}", csv.display())),
            }
        }
    }
    done
}

/// The premium bands a spread is read in, in dollars: the ones
/// [`arvo_research::OptionSpread`]'s own calibration was written down in.
const BANDS: [(&str, f64, f64); 5] = [
    ("under $0.10", 0.0, 0.10),
    ("$0.10 to $1", 0.10, 1.0),
    ("$1 to $3", 1.0, 3.0),
    ("$3 to $10", 3.0, 10.0),
    ("$10 and over", 10.0, f64::INFINITY),
];

/// The half-spreads recorded in one band of premium.
#[derive(Debug, Clone, PartialEq)]
pub struct SpreadBand {
    pub label: &'static str,
    /// Distinct quotes: a contract's quote that had not changed by the next
    /// snapshot is counted once.
    pub quotes: usize,
    /// The median half-spread, in dollars per share.
    pub p50: f64,
    pub p90: f64,
    /// What the cost model charges at the band's two ends. The second is
    /// absent for the band with no upper end.
    pub model: (f64, Option<f64>),
}

/// What a recording says an option costs to cross, on one feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Spreads {
    /// The folder's name: the underlying and the feed, e.g. `SPY.indicative`.
    pub chain: String,
    pub days: usize,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    pub quotes: usize,
    pub bands: Vec<SpreadBand>,
}

impl Spreads {
    /// The table, as the cost model's own documentation lays it out.
    #[must_use]
    pub fn table(&self) -> String {
        let span = match (self.from, self.to) {
            (Some(from), Some(to)) => format!("{from} to {to}"),
            _ => "nothing recorded".to_owned(),
        };
        let mut text = format!(
            "{}  {} day(s), {span}, {} distinct quotes\n  half-spread in dollars per share; the model is OptionSpread::MEASURED at the band's ends\n\n  {:<14} {:>10} {:>8} {:>8}   {}\n",
            self.chain, self.days, self.quotes, "premium", "quotes", "p50", "p90", "model"
        );
        for band in &self.bands {
            let model = match band.model {
                (low, Some(high)) if (high - low).abs() < 1e-9 => format!("{low:.3}"),
                (low, Some(high)) => format!("{low:.3} to {high:.3}"),
                (low, None) => format!("{low:.3} and up"),
            };
            if band.quotes == 0 {
                text.push_str(&format!("  {:<14} {:>10} {:>8} {:>8}   {model}\n", band.label, 0, "-", "-"));
            } else {
                text.push_str(&format!(
                    "  {:<14} {:>10} {:>8.3} {:>8.3}   {model}\n",
                    band.label, band.quotes, band.p50, band.p90
                ));
            }
        }
        text
    }
}

/// The value `fraction` of the way up a sorted list, by nearest rank.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Reads every recorded day of `symbol` and says what its spreads were, one
/// answer per feed, since a spread measured on a feed is a spread on that
/// feed.
///
/// This is the reader the recording never had. The cost model's constant was
/// typed in from one afternoon, and its documentation asks for it to be
/// measured again "as the recorder accumulates".
///
/// A quote with no offer, or with its bid above its ask, is left out: neither
/// has a spread to measure. A zero bid is kept, because it is what a far
/// out-of-the-money contract really quotes.
///
/// # Errors
///
/// Nothing recorded for the symbol, or a day that cannot be read.
pub fn spreads(root: &Path, symbol: &str) -> Result<Vec<Spreads>, String> {
    let prefix = format!("{symbol}.");
    let folders: Vec<_> = chain_folders(root)
        .into_iter()
        .filter(|folder| {
            folder.file_name().and_then(|name| name.to_str()).is_some_and(|name| name == symbol || name.starts_with(&prefix))
        })
        .collect();
    if folders.is_empty() {
        return Err(format!(
            "nothing recorded for {symbol} under {}",
            root.join(crate::jobs::OPTION_QUOTES_SUBDIR).display()
        ));
    }
    let model = arvo_research::OptionSpread::MEASURED;
    let mut answers = Vec::new();
    for folder in folders {
        let days = days_in(&folder);
        let mut seen = std::collections::HashSet::new();
        let mut half_spreads: Vec<Vec<f64>> = vec![Vec::new(); BANDS.len()];
        for (_, path) in &days {
            for quote in arvo_data::quotes::read(path).map_err(|err| err.to_string())? {
                if quote.ask <= 0.0 || quote.ask < quote.bid {
                    continue;
                }
                // The same quote in the next snapshot is the same quote. The
                // symbol is hashed so the set holds two integers per quote
                // rather than a string.
                let name = {
                    use std::hash::{Hash as _, Hasher as _};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    quote.symbol.hash(&mut hasher);
                    hasher.finish()
                };
                if !seen.insert((name, quote.quote_at)) {
                    continue;
                }
                let premium = quote.mid();
                if let Some(band) = BANDS.iter().position(|(_, low, high)| premium >= *low && premium < *high) {
                    half_spreads[band].push(quote.half_spread());
                }
            }
        }
        let bands = BANDS
            .iter()
            .zip(half_spreads.iter_mut())
            .map(|((label, low, high), values)| {
                values.sort_by(f64::total_cmp);
                SpreadBand {
                    label,
                    quotes: values.len(),
                    p50: percentile(values, 0.5),
                    p90: percentile(values, 0.9),
                    model: (model.half_spread(*low), high.is_finite().then(|| model.half_spread(*high))),
                }
            })
            .collect();
        answers.push(Spreads {
            chain: folder.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned(),
            days: days.len(),
            from: days.first().map(|(day, _)| *day),
            to: days.last().map(|(day, _)| *day),
            quotes: seen.len(),
            bands,
        });
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let folder = dir.path().join(crate::jobs::OPTION_QUOTES_SUBDIR);
        std::fs::create_dir_all(&folder).expect("folder");
        std::fs::write(folder.join(FILE), text).expect("write");
        dir
    }

    #[test]
    fn a_list_is_read_in_order_and_a_bad_one_leaves_spy_recording() {
        let good = r#"{
          "reason": "The most liquid US option markets: tight spreads and daily or weekly expiries.",
          "underlyings": [
            { "symbol": "SPY", "why": "the deepest option book there is" },
            { "symbol": "QQQ", "why": "the same for the Nasdaq 100" }
          ]
        }"#;
        let dir = project(good);
        assert_eq!(wanted(dir.path()), (vec!["SPY".to_owned(), "QQQ".to_owned()], None));

        // A day's chains cannot be fetched tomorrow, so a bad file falls back
        // to recording SPY rather than recording nothing.
        for (text, expected) in [
            (r#"{ "reason": " ", "underlyings": [{ "symbol": "SPY" }] }"#, "no reason"),
            (r#"{ "reason": "x", "underlyings": [] }"#, "lists no underlyings"),
            (r#"{ "reason": "x", "underlyings": [{ "symbol": "SPY" }, { "symbol": "SPY" }] }"#, "listed twice"),
            (r#"{ "reason": "x", "underlyings": [{ "symbol": "not a ticker" }] }"#, "is not a ticker"),
            ("{ not json", "key must be a string"),
        ] {
            let dir = project(text);
            let (symbols, why) = wanted(dir.path());
            assert_eq!(symbols, vec![DEFAULT.to_owned()], "{text}");
            let why = why.expect("the reason is said");
            assert!(why.contains(expected), "{expected} in {why}");
            assert!(why.contains("recording SPY alone"), "{why}");
        }

        // A day's chains are measured where they are kept: a folder per
        // underlying and feed, a file per day inside it (#200's second input).
        let sized = project(good);
        let chains = sized.path().join(crate::jobs::OPTION_QUOTES_SUBDIR).join("SPY.indicative");
        std::fs::create_dir_all(&chains).expect("chains");
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 25).expect("a date");
        std::fs::write(chains.join("2026-09-25.csv"), "x".repeat(1024)).expect("write");
        std::fs::write(chains.join("2026-09-24.csv"), "x".repeat(4096)).expect("write");
        assert_eq!(bytes_today(sized.path(), "SPY", day), 1024, "the day asked for, not the folder");
        assert_eq!(bytes_today(sized.path(), "QQQ", day), 0, "nothing recorded for it yet");

        // No file at all is the default, and not a complaint.
        let bare = tempfile::tempdir().expect("tempdir");
        assert_eq!(wanted(bare.path()), (vec![DEFAULT.to_owned()], None));
    }

    /// One snapshot of two contracts: a ten-cent-wide call at about $2.30 and
    /// a penny-wide put at about a cent.
    fn snapshot(recorded_at: &str, quote_at: &str) -> String {
        format!(
            "{recorded_at},SPY261005C00760000,2026-10-05,C,760,{quote_at},2.25,2.35,50,3,769.64,769.78\n\
             {recorded_at},SPY261005P00700000,2026-10-05,P,700,{quote_at},0,0.01,0,900,769.64,769.78\n"
        )
    }

    #[test]
    fn a_finished_day_is_compacted_and_the_recording_is_read_in_both_forms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chains = dir.path().join(crate::jobs::OPTION_QUOTES_SUBDIR).join("SPY.indicative");
        std::fs::create_dir_all(&chains).expect("chains");
        let header = arvo_data::quotes::HEADER;
        // Friday: two snapshots that differ. Saturday: Friday's close, twice,
        // which is what the recorder wrote before it knew what a weekend was.
        std::fs::write(
            chains.join("2026-10-02.csv"),
            format!(
                "{header}\n{}{}",
                snapshot("2026-10-02T14:38:26Z", "2026-10-02T14:38:20.000Z"),
                snapshot("2026-10-02T14:53:26Z", "2026-10-02T14:53:20.000Z")
            ),
        )
        .expect("write");
        std::fs::write(
            chains.join("2026-10-03.csv"),
            format!(
                "{header}\n{}{}",
                snapshot("2026-10-03T13:34:00Z", "2026-10-02T19:59:59.000Z"),
                snapshot("2026-10-03T13:49:00Z", "2026-10-02T19:59:59.000Z")
            ),
        )
        .expect("write");
        // Today is still being written, so it stays as it is.
        std::fs::write(
            chains.join("2026-10-05.csv"),
            format!("{header}\n{}", snapshot("2026-10-05T14:38:26Z", "2026-10-05T14:38:20.000Z")),
        )
        .expect("write");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).expect("a date");
        let done = compact_finished(dir.path(), today);
        assert_eq!((done.days, done.repeats), (2, 1), "{}", done.describe());
        assert!(done.failed.is_empty(), "{:?}", done.failed);
        assert!(chains.join("2026-10-02.parquet").exists() && !chains.join("2026-10-02.csv").exists());
        assert!(chains.join("2026-10-03.parquet").exists() && !chains.join("2026-10-03.csv").exists());
        assert!(chains.join("2026-10-05.csv").exists(), "today is the recorder's");
        assert_eq!(compact_finished(dir.path(), today).days, 0, "and a second pass finds nothing to do");

        // The reader takes Parquet days and the CSV day alike, and counts a
        // quote once however many snapshots it sat in: Friday's two, the
        // close once, and today's.
        let read = spreads(dir.path(), "SPY").expect("a recording");
        assert_eq!(read.len(), 1, "one feed");
        let spy = &read[0];
        assert_eq!(spy.chain, "SPY.indicative");
        assert_eq!((spy.days, spy.quotes), (3, 8));
        let band = |label: &str| spy.bands.iter().find(|band| band.label == label).expect("a band");
        assert_eq!(band("under $0.10").quotes, 4);
        assert!((band("under $0.10").p50 - 0.005).abs() < 1e-9, "half of a penny-wide market");
        assert_eq!(band("$1 to $3").quotes, 4);
        assert!((band("$1 to $3").p90 - 0.05).abs() < 1e-9, "half of a ten-cent-wide market");
        assert_eq!(band("$10 and over").quotes, 0);
        assert!(spy.table().contains("SPY.indicative  3 day(s), 2026-10-02 to 2026-10-05, 8 distinct quotes"));

        assert!(spreads(dir.path(), "QQQ").expect_err("nothing recorded").contains("nothing recorded for QQQ"));
    }
}
