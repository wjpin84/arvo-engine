//! What a run pins as its data (ADR-0036).
//!
//! A finding records a version of the data it ran on, and is stale when the
//! library no longer hashes to it. The version used to be a hash of each
//! instrument's whole series. That was sound while a fetch was something a
//! person did. Once the universes were kept fetched by a job, every member
//! gained a bar every trading day, its hash changed, and every finding on it
//! was stale by the next morning, though not one bar any of those runs read
//! had changed. A mark that arrives daily says nothing.
//!
//! So a run pins the bars it read: the span from the first bar it could read
//! to the last day of its window. The version says so in its own text,
//!
//! ```text
//! window:2016-10-05..2026-10-02:4f1c…
//! ```
//!
//! so the check that recomputes it needs nothing but the version, the
//! instruments and the resolution, all of which a finding's summary carries.
//! A bar added after the span leaves the version as it was. A bar inside it
//! that is revised, removed or added does not.
//!
//! # One function per shape, used by the run and by the check
//!
//! The run records a version and the staleness check recomputes it. When
//! those were two pieces of code they drifted: a book was checked against its
//! head instrument alone and read as stale for ever, and a panel over a
//! universe was checked against every instrument in the library, with the
//! same result. Here each shape has one function, and both sides call it.
//!
//! # A version from before this
//!
//! Has no tag. It keeps its meaning, the whole-series hash, and is checked
//! the old way. The two kinds are never compared with each other, and nothing
//! on disk is rewritten.

use arvo_data::{BarInterval, BarProvider, CsvBars};
use chrono::NaiveDate;

/// Marks a version that covers a span of bars and says which.
pub const WINDOW: &str = "window:";

/// Marks a version that covers an option chain as well as bars.
pub const CHAIN: &str = "chain:";

fn tagged(from: NaiveDate, to: NaiveDate, hash: &str) -> String {
    format!("{WINDOW}{from}..{to}:{hash}")
}

/// The span a tagged version covers, whether or not it also covers a chain.
/// `None` for a version from before spans were pinned.
#[must_use]
pub fn span_of(version: &str) -> Option<(NaiveDate, NaiveDate)> {
    let rest = version.strip_prefix(CHAIN).unwrap_or(version).strip_prefix(WINDOW)?;
    let (span, _hash) = rest.split_once(':')?;
    let (from, to) = span.split_once("..")?;
    Some((from.parse().ok()?, to.parse().ok()?))
}

/// One instrument's bars from `from` to `to`.
///
/// `None` when it holds nothing there, which a check reads as the data being
/// gone.
#[must_use]
pub fn of_series(
    bars: &dyn BarProvider,
    instrument: &str,
    interval: BarInterval,
    from: NaiveDate,
    to: NaiveDate,
) -> Option<String> {
    let hash = bars.fingerprint_within(instrument, interval, from, to).ok().flatten()?;
    Some(tagged(from, to, &hash))
}

/// Several instruments over one span: a book's members, or a panel's.
///
/// Each member's name goes in beside its bars, so the same series under
/// another name, or the members in another order, is another dataset. `None`
/// when any member holds nothing in the span: a panel short a member is not
/// the panel that ran.
#[must_use]
pub fn of_members(
    bars: &dyn BarProvider,
    members: &[String],
    interval: BarInterval,
    from: NaiveDate,
    to: NaiveDate,
) -> Option<String> {
    if members.is_empty() {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    for member in members {
        let hash = bars.fingerprint_within(member, interval, from, to).ok().flatten()?;
        hasher.update(&(member.len() as u64).to_le_bytes());
        hasher.update(member.as_bytes());
        hasher.update(hash.as_bytes());
    }
    Some(tagged(from, to, &hasher.finalize().to_hex()))
}

/// An instrument's bars over a span, and the whole of its option chain.
///
/// The chain is hashed whole, as it was: it is fetched by hand and rarely, so
/// it has not had the daily churn the bars had, and narrowing it is for the
/// day it does.
#[must_use]
pub fn of_chain(
    bars: &CsvBars,
    instrument: &str,
    interval: BarInterval,
    from: NaiveDate,
    to: NaiveDate,
) -> Option<String> {
    let series = bars.fingerprint_within(instrument, interval, from, to).ok().flatten()?;
    let symbol = instrument.split('.').next().unwrap_or_default();
    let chain = bars.option_chain_fingerprint(symbol, interval).ok().flatten()?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(series.as_bytes());
    hasher.update(chain.as_bytes());
    Some(format!("{CHAIN}{}", tagged(from, to, &hasher.finalize().to_hex())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arvo_data::Bar;

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, n).expect("valid")
    }

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .zip(1..)
            .map(|(close, n)| Bar {
                at: day(n).and_hms_opt(0, 0, 0).expect("valid"),
                open: *close,
                high: *close,
                low: *close,
                close: *close,
                volume: 1_000.0,
            })
            .collect()
    }

    fn library(series: &[(&str, &[f64])]) -> (tempfile::TempDir, CsvBars) {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = CsvBars::new(dir.path());
        for (name, closes) in series {
            library.write(name, BarInterval::DAILY, &bars(closes)).expect("written");
        }
        (dir, library)
    }

    #[test]
    fn a_series_that_grows_is_the_same_series_over_the_span_that_was_read() {
        let (_dir, held) = library(&[("AAA.YF", &[10.0, 11.0, 12.0])]);
        let pinned = of_series(&held, "AAA.YF", BarInterval::DAILY, day(1), day(3)).expect("a version");
        assert_eq!(span_of(&pinned), Some((day(1), day(3))), "the version says what it covers");

        // The next trading day arrives. Nothing the run read has changed.
        held.write("AAA.YF", BarInterval::DAILY, &bars(&[10.0, 11.0, 12.0, 13.0])).expect("written");
        assert_eq!(of_series(&held, "AAA.YF", BarInterval::DAILY, day(1), day(3)), Some(pinned.clone()));
        assert_ne!(
            held.fingerprint("AAA.YF", BarInterval::DAILY).expect("reads"),
            held.fingerprint_within("AAA.YF", BarInterval::DAILY, day(1), day(3)).expect("reads"),
            "which is exactly where the whole-series hash went wrong"
        );

        // A bar inside the span is revised, and that is a different series.
        held.write("AAA.YF", BarInterval::DAILY, &bars(&[10.0, 11.5, 12.0, 13.0])).expect("written");
        assert_ne!(of_series(&held, "AAA.YF", BarInterval::DAILY, day(1), day(3)), Some(pinned));

        // Nothing held in the span is the data being gone.
        assert_eq!(of_series(&held, "AAA.YF", BarInterval::DAILY, day(10), day(12)), None);
        assert_eq!(of_series(&held, "NOPE.YF", BarInterval::DAILY, day(1), day(3)), None);
    }

    #[test]
    fn members_are_pinned_by_name_and_order_and_all_have_to_be_there() {
        let (_dir, held) = library(&[("AAA.YF", &[10.0, 11.0, 12.0]), ("BBB.YF", &[20.0, 21.0, 22.0])]);
        let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
        let both = of_members(&held, &names(&["AAA.YF", "BBB.YF"]), BarInterval::DAILY, day(1), day(3)).expect("a version");

        assert_ne!(
            of_members(&held, &names(&["BBB.YF", "AAA.YF"]), BarInterval::DAILY, day(1), day(3)),
            Some(both.clone()),
            "another order is another dataset"
        );
        assert_ne!(of_members(&held, &names(&["AAA.YF"]), BarInterval::DAILY, day(1), day(3)), Some(both.clone()));
        assert_eq!(
            of_members(&held, &names(&["AAA.YF", "GONE.YF"]), BarInterval::DAILY, day(1), day(3)),
            None,
            "a panel short a member is not the panel that ran"
        );
        assert_eq!(of_members(&held, &[], BarInterval::DAILY, day(1), day(3)), None);

        // One member gains a day, and the pinned span is as it was.
        held.write("BBB.YF", BarInterval::DAILY, &bars(&[20.0, 21.0, 22.0, 23.0])).expect("written");
        assert_eq!(of_members(&held, &names(&["AAA.YF", "BBB.YF"]), BarInterval::DAILY, day(1), day(3)), Some(both));
    }

    #[test]
    fn a_version_from_before_spans_has_none() {
        assert_eq!(span_of("2be437d2c8ec"), None);
        assert_eq!(span_of("chain:2be437d2c8ec"), None, "an older chain version is still an older version");
        assert_eq!(span_of("window:2026-03-01..2026-03-03:abc"), Some((day(1), day(3))));
        assert_eq!(span_of("chain:window:2026-03-01..2026-03-03:abc"), Some((day(1), day(3))));
        assert_eq!(span_of("window:not-a-date..2026-03-03:abc"), None);
    }
}
