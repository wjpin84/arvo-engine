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
}
