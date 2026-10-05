//! Universes (#227): named lists of instruments, chosen for a reason other
//! than their returns, kept fetched by the engine.
//!
//! A universe is a file, `universes/<name>.json`:
//!
//! ```json
//! {
//!   "name": "etf30",
//!   "reason": "The thirty most-traded US ETFs by 2026 volume: liquidity, not returns.",
//!   "interval": { "step": 1, "unit": "day" },
//!   "since": "2016-01-01",
//!   "instruments": ["SPY.YF", "QQQ.YF", "IWM.YF"]
//! }
//! ```
//!
//! The reason is required and is shown wherever the universe is: a list
//! chosen by looking at returns is the search the platform exists to deflate,
//! and the file says out loud how it was chosen. A panel over a universe
//! records the universe and its reason on the finding, and its size is named
//! there as the search that picking one member of it would be. Membership is
//! today's; survivorship is [#9](https://github.com/wjpin84/arvo-desktop/issues/9).

use std::path::Path;

use arvo_data::{BarInterval, BarProvider as _};
use serde::{Deserialize, Serialize};

use crate::research::ResearchService;

pub const SUBDIR: &str = "universes";

/// A named list of instruments and why they were chosen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Universe {
    pub name: String,
    /// How the list was chosen: an index's members, a liquidity floor, a
    /// sector. Required, and shown wherever the universe is.
    pub reason: String,
    /// `SYMBOL.VENUE`, each on a venue a source serves.
    pub instruments: Vec<String>,
    pub interval: BarInterval,
    /// The earliest date worth having. Absent is the source's default reach.
    #[serde(default)]
    pub since: Option<chrono::NaiveDate>,
}

/// Every universe file under `root`, in path order: what it says, or why it
/// cannot be used.
#[must_use]
pub fn read_all(root: &Path) -> Vec<(String, Result<Universe, String>)> {
    let dir = root.join(SUBDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let relative = format!("{SUBDIR}/{}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()));
            (relative, read_one(&path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Reads one universe file and checks it can be used.
///
/// # Errors
///
/// The file cannot be read or parsed; the reason is empty; the list is
/// empty or repeats a member; a member names no venue, or a venue no source
/// serves.
pub fn read_one(path: &Path) -> Result<Universe, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let universe: Universe = serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    check(&universe).map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(universe)
}

/// One universe by name.
///
/// # Errors
///
/// No such file, or a file that cannot be used.
pub fn find(root: &Path, name: &str) -> Result<Universe, String> {
    let path = root.join(SUBDIR).join(format!("{name}.json"));
    if !path.is_file() {
        let known: Vec<String> = read_all(root).into_iter().filter_map(|(_, u)| u.ok().map(|u| u.name)).collect();
        return Err(format!(
            "no universe called {name:?}; {}",
            if known.is_empty() { "the project has no universes/ folder yet".to_owned() } else { format!("the project has {}", known.join(", ")) }
        ));
    }
    read_one(&path)
}

fn check(universe: &Universe) -> Result<(), String> {
    if universe.reason.trim().is_empty() {
        return Err("says no reason; a universe chosen without one is a search waiting to happen".to_owned());
    }
    if universe.instruments.is_empty() {
        return Err("lists no instruments".to_owned());
    }
    let venues: std::collections::BTreeSet<String> = crate::source::all().iter().map(|source| source.venue().to_owned()).collect();
    let mut seen = std::collections::BTreeSet::new();
    for id in &universe.instruments {
        let Some((symbol, venue)) = id.split_once('.') else {
            return Err(format!("{id:?} names no venue; write SYMBOL.VENUE"));
        };
        if symbol.is_empty() || !venues.contains(venue) {
            return Err(format!("{id:?}: no source serves venue {venue:?}; this build serves {}", venues.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        if !seen.insert(id.as_str()) {
            return Err(format!("{id:?} is listed twice"));
        }
    }
    Ok(())
}

/// What a refresh did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refreshed {
    /// Members fetched because the library had nothing, or nothing recent.
    pub fetched: Vec<String>,
    /// Members the library already had up to the last completed bar.
    pub current: Vec<String>,
    /// Members whose fetch failed, with the reason.
    pub failed: Vec<(String, String)>,
    /// Members already fetched for the bar that is due, which the source did
    /// not have: a market holiday has no bar. Not asked again until a later
    /// bar is due.
    pub absent: Vec<String>,
}

/// What has been fetched and did not bring the bar that was due, by member,
/// interval and the day wanted. The job runs hourly, and without this a
/// holiday would refetch every member's whole history each hour until the
/// next session.
// ponytail: in memory, so a restart tries once more; a market calendar would
// replace it.
static TRIED: std::sync::Mutex<std::collections::BTreeSet<(String, String, chrono::NaiveDate)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

impl Refreshed {
    #[must_use]
    pub fn describe(&self) -> String {
        let absent = if self.absent.is_empty() {
            String::new()
        } else {
            format!(", {} the source has nothing newer for", self.absent.len())
        };
        format!("{} fetched, {} current, {} failed{absent}", self.fetched.len(), self.current.len(), self.failed.len())
    }
}

/// Whether the library's series for `id` at `interval` ends before the last
/// bar that could exist by `now` (UTC): a daily series is current through the
/// last weekday before today; an intraday one through today's session once it
/// has closed.
fn stale(service: &ResearchService, id: &str, interval: BarInterval, now: chrono::NaiveDateTime) -> bool {
    let Ok(Some((_, last))) = service.bars.coverage(id, interval) else { return true };
    last < stale_wants(interval, now)
}

/// Fetches every member whose series is missing or behind, through the
/// source that serves its venue. A member that fails is named and the rest
/// are still fetched.
///
/// # Errors
///
/// Never for one member's failure; only when nothing about the universe can
/// be acted on.
pub async fn refresh(service: &ResearchService, universe: &Universe, report: crate::research::data::Report<'_>) -> Result<Refreshed, String> {
    let sources = crate::source::all();
    let now = chrono::Utc::now().naive_utc();
    let today = now.date();
    let mut done = Refreshed::default();
    for id in &universe.instruments {
        let (symbol, venue) = id.split_once('.').ok_or_else(|| format!("{id:?} names no venue"))?;
        if !stale(service, id, universe.interval, now) {
            done.current.push(id.clone());
            continue;
        }
        let tried = (id.clone(), universe.interval.to_string(), stale_wants(universe.interval, now));
        if TRIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&tried) {
            done.absent.push(id.clone());
            continue;
        }
        let Some(source) = sources.iter().find(|source| source.venue() == venue) else {
            done.failed.push((id.clone(), format!("no source serves venue {venue:?}")));
            continue;
        };
        // The whole reach every time: the library's write replaces a series
        // with what was fetched, so a short refetch would truncate years of
        // history to a week. Back to `since` when the file says, never past
        // the source's own reach.
        let reach = arvo_data::source::default_days(&[source.as_ref()], universe.interval);
        let days = universe
            .since
            .map_or(reach, |since| u32::try_from((today - since).num_days().max(1)).unwrap_or(reach).min(reach));
        let from = today - chrono::Duration::days(i64::from(days));
        match arvo_data::source::ingest(&service.data_dir, source.as_ref(), symbol, universe.interval, from, today).await {
            Ok(_) => {
                if stale(service, id, universe.interval, now) {
                    TRIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(tried);
                }
                done.fetched.push(id.clone());
            }
            Err(err) => {
                // A dead session is announced once, by the same path a
                // person's fetch would announce it.
                let _ = crate::research::data::failed(report, &err);
                done.failed.push((id.clone(), err.to_string()));
            }
        }
    }
    Ok(done)
}

/// The day of the most recent bar that can exist by `now` (UTC).
///
/// For a daily series, the last weekday before today. An intraday series is
/// due as soon as today's regular session has closed: the day's review draws
/// these bars, and waiting for the date to roll over in UTC left the review
/// of a session without its bars until late in the evening.
fn stale_wants(interval: BarInterval, now: chrono::NaiveDateTime) -> chrono::NaiveDate {
    let today = now.date();
    let intraday = matches!(interval.unit, arvo_data::IntervalUnit::Minute | arvo_data::IntervalUnit::Hour);
    let weekend = matches!(chrono::Datelike::weekday(&today), chrono::Weekday::Sat | chrono::Weekday::Sun);
    // Five minutes past the close, for the last bar to be published.
    if intraday && !weekend && now >= arvo_data::session::regular_close(today) + chrono::Duration::minutes(5) {
        return today;
    }
    let now = today;
    let mut wanted = now.pred_opt().unwrap_or(now);
    while matches!(chrono::Datelike::weekday(&wanted), chrono::Weekday::Sat | chrono::Weekday::Sun) {
        wanted = wanted.pred_opt().unwrap_or(wanted);
    }
    wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETFS: &str = r#"{
      "name": "etf3",
      "reason": "three ETFs everyone trades; liquidity, not returns",
      "interval": { "step": 1, "unit": "day" },
      "since": "2016-01-01",
      "instruments": ["SPY.YF", "QQQ.YF", "IWM.YF"]
    }"#;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(SUBDIR)).expect("universes/");
        std::fs::write(dir.path().join(SUBDIR).join("etf3.json"), ETFS).expect("write");
        dir
    }

    #[test]
    fn a_universe_file_is_read_and_a_bad_one_says_why() {
        let dir = project();
        let write = |name: &str, text: &str| std::fs::write(dir.path().join(SUBDIR).join(format!("{name}.json")), text).expect("write");
        write("noreason", r#"{ "name": "noreason", "reason": " ", "interval": {"step":1,"unit":"day"}, "instruments": ["SPY.YF"] }"#);
        write("novenue", r#"{ "name": "novenue", "reason": "x", "interval": {"step":1,"unit":"day"}, "instruments": ["SPY"] }"#);
        write("unknown", r#"{ "name": "unknown", "reason": "x", "interval": {"step":1,"unit":"day"}, "instruments": ["SPY.NOPE"] }"#);
        write("twice", r#"{ "name": "twice", "reason": "x", "interval": {"step":1,"unit":"day"}, "instruments": ["SPY.YF", "SPY.YF"] }"#);

        let read: std::collections::BTreeMap<String, Result<Universe, String>> = read_all(dir.path()).into_iter().collect();
        let etf3 = read["universes/etf3.json"].as_ref().expect("reads");
        assert_eq!(etf3.instruments.len(), 3);
        assert_eq!(etf3.since, chrono::NaiveDate::from_ymd_opt(2016, 1, 1));
        assert!(read["universes/noreason.json"].as_ref().expect_err("refused").contains("no reason"));
        assert!(read["universes/novenue.json"].as_ref().expect_err("refused").contains("names no venue"));
        assert!(read["universes/unknown.json"].as_ref().expect_err("refused").contains("no source serves venue"));
        assert!(read["universes/twice.json"].as_ref().expect_err("refused").contains("listed twice"));

        assert_eq!(find(dir.path(), "etf3").expect("found").name, "etf3");
        let missing = find(dir.path(), "sp100").expect_err("no such file");
        assert!(missing.contains("no universe called \"sp100\"") && missing.contains("etf3"), "{missing}");
    }

    #[test]
    fn a_series_is_current_through_the_last_weekday_before_today() {
        let dir = project();
        let service = ResearchService::new(dir.path().join(crate::research::DATA_SUBDIR), dir.path().join(crate::research::EVIDENCE_SUBDIR));
        let date = |day: u32| chrono::NaiveDate::from_ymd_opt(2026, 9, day).expect("date");
        let at = |day: u32, hour: u32, minute: u32| date(day).and_hms_opt(hour, minute, 0).expect("time");
        let (thursday, friday, saturday, monday) = (24, 25, 26, 28);
        assert!(stale(&service, "SPY.YF", BarInterval::DAILY, at(monday, 12, 0)), "nothing in the library is stale");
        // The rule wants Friday's bar on Saturday and Monday, and Thursday's on Friday.
        let daily = |day, hour| super::stale_wants(BarInterval::DAILY, at(day, hour, 0));
        assert_eq!(daily(saturday, 12), date(friday));
        assert_eq!(daily(monday, 12), date(friday));
        assert_eq!(daily(friday, 12), date(thursday));
        assert_eq!(daily(friday, 23), date(thursday), "a daily series still waits for the date to roll");
    }

    /// The review after the close draws the day's five-minute bars, so they
    /// are due when the session closes and not when the date rolls in UTC.
    #[test]
    fn an_intraday_series_is_due_once_todays_session_has_closed() {
        let five = BarInterval { step: 5, unit: arvo_data::IntervalUnit::Minute };
        let date = |day: u32| chrono::NaiveDate::from_ymd_opt(2026, 10, day).expect("date");
        let wants = |day: u32, hour: u32, minute: u32| {
            super::stale_wants(five, date(day).and_hms_opt(hour, minute, 0).expect("time"))
        };
        // Monday 2026-10-05. The close is 16:00 New York, 20:00 UTC.
        assert_eq!(wants(5, 15, 0), date(2), "mid-session: Friday is the last finished day");
        assert_eq!(wants(5, 20, 4), date(2), "a moment for the last bar to be published");
        assert_eq!(wants(5, 20, 5), date(5), "due from five past the close");
        assert_eq!(wants(6, 3, 0), date(5), "and still the day wanted after midnight UTC");
        assert_eq!(wants(10, 21, 0), date(9), "a Saturday has no session to close");
    }
}
