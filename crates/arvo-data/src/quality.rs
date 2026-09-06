//! What is wrong with a price series.
//!
//! Parsing already refuses bars that cannot be prices — `high` below `low`, a
//! negative close, a malformed date. Those are *impossible* bars. This is
//! about bars that are individually possible and collectively wrong: a day
//! that never happened, a price repeated because a feed stalled, a split the
//! vendor forgot to adjust for. A backtest cannot tell any of them from real
//! movement, and will produce a confident verdict either way.
//!
//! # The bar for adding a check
//!
//! **A check that fires on legitimate data is worse than no check**, because
//! it teaches people to skip the warnings — and then the real one goes past
//! unread too. So every check here either describes something that cannot
//! legitimately happen (two bars at the same instant), or carries a threshold
//! chosen to sit well outside ordinary market behaviour, with the assumption
//! written down beside it.
//!
//! Nothing here refuses to run a backtest. These are observations attached to
//! a result, because whether a 43% single-day fall is a data fault or March
//! 2020 is a judgement about the world that this module has no way to make.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{Bar, BarInterval};

/// Consecutive identical bars before the series is called stalled.
///
/// Three, because two can happen on a quiet instrument and three in a row with
/// *every* field identical — open, high, low and close — is a feed repeating
/// itself rather than a market standing still.
const STALL: usize = 3;

/// How far a single bar's true range may exceed the median before it is
/// called out.
///
/// Ten times. Real markets do produce days like that — a crash, an earnings
/// gap — so this is deliberately not an error. It is the shape a bad print
/// also has, and the two are worth telling apart by eye.
const OUTLIER: f64 = 10.0;

/// Calendar days between daily bars before a gap is called out.
///
/// Five covers a weekend plus a public holiday either side of it. A longer
/// hole is a trading week that is missing from the file, which a backtest will
/// read as a single large move.
const DAILY_GAP_DAYS: i64 = 5;

/// How close a move must be to a whole-number split ratio to be suspected.
///
/// One percent. A two-for-one split is exactly −50%; a market that happens to
/// fall 49.7% in a day is not going to be mistaken for one, and a fall of
/// exactly 50.0% almost certainly is one.
const SPLIT_TOLERANCE: f64 = 0.01;

/// How much a bar has to move at all before a split is even considered.
const SPLIT_FLOOR: f64 = 0.30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Cannot legitimately be true of a price series.
    Fault,
    /// Possible, and unusual enough to be worth a look.
    Suspect,
}

/// One thing worth knowing about a series.
///
/// `kind` is a `&'static str` and this is `Serialize` only: it is a domain
/// value, not a wire shape. The crossing to the window happens in
/// `arvo-runtime`, the same way every other domain type reaches it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// A short name for the kind of problem, stable enough to filter on.
    pub kind: &'static str,
    /// Where in the series, when there is a single place.
    pub at: Option<chrono::NaiveDateTime>,
    pub detail: String,
}

/// What an inspection found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub bars: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    #[must_use]
    pub fn faults(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Fault)
            .count()
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Looks over a series and reports what is wrong with it.
///
/// The bars are assumed sorted, which every [`crate::BarProvider`] guarantees.
#[must_use]
pub fn inspect(bars: &[Bar], interval: BarInterval) -> Report {
    let mut findings = Vec::new();
    duplicates(bars, &mut findings);
    stalls(bars, &mut findings);
    silent_movement(bars, &mut findings);
    gaps(bars, interval, &mut findings);
    splits(bars, &mut findings);
    outliers(bars, &mut findings);

    // Worst first, then chronological. A reader scanning the top of the list
    // should meet the things that cannot be true before the things that are
    // merely unusual.
    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.at.cmp(&b.at)));
    Report {
        bars: bars.len(),
        findings,
    }
}

/// Two bars for the same instant.
///
/// Not a judgement call: a series cannot have two prices for one moment. It
/// happens when two exports are concatenated, or a fetch is appended to a file
/// it overlaps.
fn duplicates(bars: &[Bar], out: &mut Vec<Finding>) {
    let mut counts: BTreeMap<chrono::NaiveDateTime, usize> = BTreeMap::new();
    for bar in bars {
        *counts.entry(bar.at).or_default() += 1;
    }
    for (at, count) in counts.into_iter().filter(|(_, count)| *count > 1) {
        out.push(Finding {
            severity: Severity::Fault,
            kind: "duplicate",
            at: Some(at),
            detail: format!("{count} bars share this timestamp"),
        });
    }
}

/// A run of bars with every price identical.
///
/// A feed that stalled and repeated its last value. A strategy reads the flat
/// stretch as a market that stopped moving, which is a different thing from a
/// market nobody was watching.
fn stalls(bars: &[Bar], out: &mut Vec<Finding>) {
    let same = |a: &Bar, b: &Bar| {
        a.open == b.open && a.high == b.high && a.low == b.low && a.close == b.close
    };

    let mut run_start = 0;
    for index in 1..=bars.len() {
        let continues = index < bars.len() && same(&bars[index], &bars[run_start]);
        if continues {
            continue;
        }
        let length = index - run_start;
        if length >= STALL {
            out.push(Finding {
                severity: Severity::Suspect,
                kind: "stalled",
                at: Some(bars[run_start].at),
                detail: format!("{length} consecutive bars with identical prices"),
            });
        }
        run_start = index;
    }
}

/// A bar that moved without anyone trading.
///
/// Zero volume and a non-zero range is a contradiction: the range *is* trading.
/// Some feeds report zero volume for a synthesised or interpolated bar, which
/// is exactly the sort of invented price a breakout rule cannot tell from a
/// real one.
fn silent_movement(bars: &[Bar], out: &mut Vec<Finding>) {
    let count = bars
        .iter()
        .filter(|bar| bar.volume <= 0.0 && bar.high > bar.low)
        .count();
    if count > 0 {
        out.push(Finding {
            severity: Severity::Suspect,
            kind: "no-volume",
            at: bars
                .iter()
                .find(|bar| bar.volume <= 0.0 && bar.high > bar.low)
                .map(|bar| bar.at),
            detail: format!("{count} bars have a price range but no volume"),
        });
    }
}

/// Holes in the series.
///
/// Daily bars are checked against the calendar, with enough slack for a
/// weekend and a holiday. Intraday bars are checked only *within* a session —
/// the overnight hole between one day's close and the next day's open is not a
/// gap, and treating it as one would report every night.
fn gaps(bars: &[Bar], interval: BarInterval, out: &mut Vec<Finding>) {
    let step = interval.duration();
    for pair in bars.windows(2) {
        let (previous, next) = (&pair[0], &pair[1]);
        let apart = next.at - previous.at;

        let hole = if interval.is_intraday() {
            // Same day only. A session boundary is not a gap.
            previous.at.date() == next.at.date() && apart > step
        } else {
            apart > chrono::Duration::days(DAILY_GAP_DAYS)
        };

        if hole {
            out.push(Finding {
                severity: Severity::Suspect,
                kind: "gap",
                at: Some(previous.at),
                detail: format!(
                    "{} to {} with nothing in between",
                    previous.at.format("%Y-%m-%d %H:%M"),
                    next.at.format("%Y-%m-%d %H:%M"),
                ),
            });
        }
    }
}

/// A move that looks like an unadjusted corporate action.
///
/// A two-for-one split halves the price overnight with nothing else changing.
/// The test is proximity to a whole-number ratio: a fall of exactly 50.0% is
/// almost certainly a split, and one of 49.7% almost certainly is not. Raw
/// prices make a split look like a crash, which a breakout rule will trade.
fn splits(bars: &[Bar], out: &mut Vec<Finding>) {
    // Ratios worth naming. Anything rarer is caught by the outlier check.
    const RATIOS: &[(f64, &str)] = &[
        (2.0, "2:1"),
        (3.0, "3:1"),
        (4.0, "4:1"),
        (5.0, "5:1"),
        (10.0, "10:1"),
    ];

    for pair in bars.windows(2) {
        let (previous, next) = (&pair[0], &pair[1]);
        if previous.close <= 0.0 || next.close <= 0.0 {
            continue;
        }
        let change = (next.close - previous.close) / previous.close;
        if change.abs() < SPLIT_FLOOR {
            continue;
        }

        let ratio = previous.close / next.close;
        for (factor, name) in RATIOS {
            // Forward and reverse: a 1-for-10 consolidation is the same shape
            // upside down, and is just as invisible to a strategy.
            let matches = (ratio - factor).abs() / factor < SPLIT_TOLERANCE
                || (ratio.recip() - factor).abs() / factor < SPLIT_TOLERANCE;
            if matches {
                out.push(Finding {
                    severity: Severity::Suspect,
                    kind: "split",
                    at: Some(next.at),
                    detail: format!(
                        "price moved {:+.1}% — close to a {name} split, which the data may not \
                         be adjusted for",
                        change * 100.0
                    ),
                });
                break;
            }
        }
    }
}

/// A bar far larger than the rest of the series.
///
/// Against the *median* rather than the mean, because the mean is moved by
/// exactly the bars this is looking for.
///
/// # Relative range, and why the first version was wrong
///
/// The comparison is `(high - low) / close`, not `high - low`. Measured in
/// absolute terms, a series that drifts from 100 to 1,000 has late bars whose
/// ranges are ten times the median of the whole — and the first version of
/// this check duly reported **159 outliers in 5,000 bars** of an ordinary
/// upward-drifting fixture. That is precisely the crying-wolf failure this
/// module's own preamble warns about, found by running it on real data rather
/// than on a test that only ever used one price level.
///
/// Real markets do produce genuine outliers; so do bad prints, and the two
/// look identical from here. That is why it is a suspicion rather than a
/// fault.
fn outliers(bars: &[Bar], out: &mut Vec<Finding>) {
    if bars.len() < 20 {
        // Too short for a median to describe anything.
        return;
    }
    let relative = |bar: &Bar| {
        if bar.close > 0.0 {
            (bar.high - bar.low) / bar.close
        } else {
            0.0
        }
    };

    let mut ranges: Vec<f64> = bars.iter().map(relative).collect();
    ranges.sort_by(f64::total_cmp);
    let median = ranges[ranges.len() / 2];
    if median <= 0.0 {
        return;
    }

    for bar in bars {
        let range = relative(bar);
        if range > median * OUTLIER {
            out.push(Finding {
                severity: Severity::Suspect,
                kind: "outlier",
                at: Some(bar.at),
                detail: format!(
                    "range is {:.1}% of price, {:.0}x the usual {:.1}%",
                    range * 100.0,
                    range / median,
                    median * 100.0
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn bar(day: u32, close: f64) -> Bar {
        Bar {
            at: at(day),
            open: close,
            high: close + 0.5,
            low: close - 0.5,
            close,
            volume: 1_000.0,
        }
    }

    fn kinds(report: &Report) -> Vec<&str> {
        report.findings.iter().map(|f| f.kind).collect()
    }

    #[test]
    fn an_ordinary_series_produces_nothing() {
        // The most important test here. A check that fires on legitimate data
        // teaches people to skip the warnings, and then the real one goes past
        // unread too.
        let bars: Vec<Bar> = (1..=25)
            .map(|day| bar(day, 100.0 + f64::from(day) * 0.3))
            .collect();
        let report = inspect(&bars, BarInterval::DAILY);
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    #[test]
    fn two_bars_for_one_instant_is_a_fault_not_a_suspicion() {
        // A series cannot have two prices for one moment. It happens when two
        // exports are concatenated or a fetch overlaps the file it appends to.
        let bars = vec![bar(1, 100.0), bar(1, 101.0), bar(2, 102.0)];
        let report = inspect(&bars, BarInterval::DAILY);
        assert_eq!(report.faults(), 1);
        assert!(kinds(&report).contains(&"duplicate"));
    }

    #[test]
    fn a_stalled_feed_is_caught_and_two_quiet_bars_are_not() {
        let mut quiet = vec![bar(1, 100.0), bar(2, 100.0), bar(3, 101.0)];
        quiet[1] = Bar { at: at(2), ..quiet[0] };
        assert!(
            inspect(&quiet, BarInterval::DAILY).is_clean(),
            "two identical bars is a quiet market"
        );

        let stalled = vec![
            Bar { at: at(1), ..bar(1, 100.0) },
            Bar { at: at(2), ..bar(1, 100.0) },
            Bar { at: at(3), ..bar(1, 100.0) },
        ];
        assert!(kinds(&inspect(&stalled, BarInterval::DAILY)).contains(&"stalled"));
    }

    #[test]
    fn a_bar_that_moved_without_trading_is_a_contradiction() {
        // The range *is* trading. Feeds report this for synthesised bars —
        // exactly the invented price a breakout rule cannot tell from a real
        // one.
        let bars = vec![
            bar(1, 100.0),
            Bar { volume: 0.0, ..bar(2, 101.0) },
            bar(3, 102.0),
        ];
        assert!(kinds(&inspect(&bars, BarInterval::DAILY)).contains(&"no-volume"));
    }

    #[test]
    fn a_weekend_is_not_a_gap_and_a_missing_week_is() {
        // 5 January 2024 was a Friday; the 8th is the Monday.
        let weekend = vec![bar(5, 100.0), bar(8, 101.0)];
        assert!(inspect(&weekend, BarInterval::DAILY).is_clean());

        let missing = vec![bar(5, 100.0), bar(20, 101.0)];
        assert!(kinds(&inspect(&missing, BarInterval::DAILY)).contains(&"gap"));
    }

    #[test]
    fn an_overnight_hole_is_not_an_intraday_gap() {
        // Every night would otherwise be reported, which is the shape of a
        // check nobody reads.
        let interval = BarInterval::new(5, crate::IntervalUnit::Minute);
        let session = |day: u32, hour: u32, minute: u32| Bar {
            at: chrono::NaiveDate::from_ymd_opt(2024, 1, day)
                .expect("valid")
                .and_hms_opt(hour, minute, 0)
                .expect("valid"),
            ..bar(day, 100.0)
        };
        let overnight = vec![session(2, 19, 55), session(3, 13, 30)];
        assert!(inspect(&overnight, interval).is_clean());

        let mid_session = vec![session(2, 13, 30), session(2, 15, 0)];
        assert!(kinds(&inspect(&mid_session, interval)).contains(&"gap"));
    }

    #[test]
    fn an_unadjusted_split_is_spotted_and_a_crash_is_not() {
        // Raw prices make a split look like a crash, which a breakout rule
        // will trade. The test is proximity to a whole ratio: exactly −50% is
        // almost certainly a split, −43% almost certainly is not.
        let split = vec![bar(1, 200.0), bar(2, 100.0)];
        assert!(kinds(&inspect(&split, BarInterval::DAILY)).contains(&"split"));

        let crash = vec![bar(1, 200.0), bar(2, 114.0)];
        assert!(
            !kinds(&inspect(&crash, BarInterval::DAILY)).contains(&"split"),
            "a 43% fall is a market, not a ratio"
        );
    }

    #[test]
    fn a_reverse_split_is_the_same_shape_upside_down() {
        let bars = vec![bar(1, 10.0), bar(2, 100.0)];
        assert!(kinds(&inspect(&bars, BarInterval::DAILY)).contains(&"split"));
    }

    #[test]
    fn an_ordinary_move_below_the_floor_is_never_called_a_split() {
        // Two consecutive closes in a 3:1 *ratio* is a split; a 2% drift that
        // happens to divide neatly is not, and the floor is what keeps the
        // check from firing on ordinary days.
        let bars: Vec<Bar> = (1..=10).map(|day| bar(day, 100.0 + f64::from(day))).collect();
        assert!(!kinds(&inspect(&bars, BarInterval::DAILY)).contains(&"split"));
    }

    #[test]
    fn a_bad_print_stands_out_against_the_median_not_the_mean() {
        // The mean is moved by exactly the bar this is looking for.
        let mut bars: Vec<Bar> = (1..=25).map(|day| bar(day, 100.0)).collect();
        bars[12] = Bar {
            high: 160.0,
            low: 100.0,
            close: 100.0,
            ..bars[12]
        };
        assert!(kinds(&inspect(&bars, BarInterval::DAILY)).contains(&"outlier"));
    }

    #[test]
    fn a_series_that_grows_tenfold_is_not_all_outliers() {
        // The bug the first version shipped: measured in absolute terms, a
        // series drifting from 100 to 1,000 has late bars whose ranges are ten
        // times the median of the whole, and it reported 159 outliers across
        // 5,000 bars of an ordinary fixture. Range is compared as a fraction
        // of price for exactly this reason.
        let bars: Vec<Bar> = (1..=200)
            .map(|index| {
                let close = 100.0 * (1.0 + f64::from(index) * 0.05);
                Bar {
                    at: at(1) + chrono::Duration::days(i64::from(index)),
                    open: close,
                    // A constant one percent of price, at every level.
                    high: close * 1.005,
                    low: close * 0.995,
                    close,
                    volume: 1_000.0,
                }
            })
            .collect();
        let report = inspect(&bars, BarInterval::DAILY);
        assert!(
            !kinds(&report).contains(&"outlier"),
            "a constant relative range is not an outlier at any price level: {:?}",
            report.findings
        );
    }

    #[test]
    fn a_series_too_short_for_a_median_makes_no_claim_about_outliers() {
        let bars = vec![bar(1, 100.0), Bar { high: 500.0, ..bar(2, 100.0) }];
        assert!(!kinds(&inspect(&bars, BarInterval::DAILY)).contains(&"outlier"));
    }

    #[test]
    fn faults_come_before_suspicions() {
        let mut bars: Vec<Bar> = (1..=25).map(|day| bar(day, 100.0)).collect();
        bars.push(bar(25, 100.0));
        let report = inspect(&bars, BarInterval::DAILY);
        assert_eq!(report.findings[0].severity, Severity::Fault);
    }
}
