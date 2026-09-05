//! How much time one bar covers.
//!
//! Until now every bar was a day, and that assumption was spread across three
//! crates: a `daily_bars` method, a close-of-day timestamp at the engine
//! boundary, and a hardcoded 252 for annualising. The last of those is the
//! dangerous one — it is silently wrong for any other resolution, and Sharpe
//! computed with it would be off by a factor of √(bars per day) with nothing
//! in the output to show it.
//!
//! An interval is also part of the reproducibility record. The same rule at
//! five minutes and at one day is not the same experiment, and a stored
//! finding that did not say which one it was would not be reproducible.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Trading days in a year. The conventional figure for US equities.
const TRADING_DAYS: f64 = 252.0;

/// Minutes in a regular US equity session, 09:30 to 16:00.
///
/// An assumption, and one that only holds for US regular hours: a futures or
/// crypto session is longer, and an extended-hours request covers more. It is
/// named here rather than buried in a constant so the day it is wrong, it is
/// findable.
const SESSION_MINUTES: f64 = 390.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
}

/// A bar's resolution: a count of some unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarInterval {
    pub step: u32,
    pub unit: IntervalUnit,
}

impl BarInterval {
    /// The daily bar, which is what everything was before intervals existed.
    pub const DAILY: Self = Self {
        step: 1,
        unit: IntervalUnit::Day,
    };

    #[must_use]
    pub const fn new(step: u32, unit: IntervalUnit) -> Self {
        Self { step, unit }
    }

    /// How long one bar covers.
    ///
    /// Used to timestamp a bar at the instant it *closes* rather than opens,
    /// which is what stops a strategy acting on a price it could not yet have
    /// seen.
    #[must_use]
    pub fn duration(&self) -> chrono::Duration {
        let step = i64::from(self.step.max(1));
        match self.unit {
            IntervalUnit::Second => chrono::Duration::seconds(step),
            IntervalUnit::Minute => chrono::Duration::minutes(step),
            IntervalUnit::Hour => chrono::Duration::hours(step),
            IntervalUnit::Day => chrono::Duration::days(step),
            IntervalUnit::Week => chrono::Duration::weeks(step),
        }
    }

    /// Roughly how many of these bars occur in a year.
    ///
    /// The number every annualised statistic is scaled by, so getting it
    /// wrong quietly rescales Sharpe and volatility rather than failing.
    ///
    /// Intraday counts assume a regular US equity session — see
    /// [`SESSION_MINUTES`]. A market that trades around the clock would need a
    /// different figure, and the honest fix then is to make session length a
    /// property of the dataset rather than a constant here.
    #[must_use]
    pub fn periods_per_year(&self) -> f64 {
        let step = f64::from(self.step.max(1));
        match self.unit {
            IntervalUnit::Second => TRADING_DAYS * SESSION_MINUTES * 60.0 / step,
            IntervalUnit::Minute => TRADING_DAYS * SESSION_MINUTES / step,
            IntervalUnit::Hour => TRADING_DAYS * (SESSION_MINUTES / 60.0) / step,
            IntervalUnit::Day => TRADING_DAYS / step,
            IntervalUnit::Week => 52.0 / step,
        }
    }

    /// Whether this resolution is finer than a day.
    #[must_use]
    pub const fn is_intraday(&self) -> bool {
        matches!(
            self.unit,
            IntervalUnit::Second | IntervalUnit::Minute | IntervalUnit::Hour
        )
    }
}

impl Default for BarInterval {
    fn default() -> Self {
        Self::DAILY
    }
}

impl fmt::Display for BarInterval {
    /// `5minute`, `1day` — the same spelling [`FromStr`] accepts, so a value
    /// can round-trip through a config file or a stored record.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = match self.unit {
            IntervalUnit::Second => "second",
            IntervalUnit::Minute => "minute",
            IntervalUnit::Hour => "hour",
            IntervalUnit::Day => "day",
            IntervalUnit::Week => "week",
        };
        write!(f, "{}{unit}", self.step)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0:?} is not an interval like `5minute` or `1day`")]
pub struct ParseIntervalError(String);

impl FromStr for BarInterval {
    type Err = ParseIntervalError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // Whitespace anywhere, not just at the ends: "5 minutes" and
        // "5minutes" mean the same thing and both get typed by people.
        let trimmed: String = text
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| ParseIntervalError(text.to_owned()))?;
        let (count, unit) = trimmed.split_at(split);

        // A bare unit means one of them: `day` is `1day`.
        let step: u32 = if count.is_empty() {
            1
        } else {
            count
                .parse()
                .map_err(|_| ParseIntervalError(text.to_owned()))?
        };
        if step == 0 {
            return Err(ParseIntervalError(text.to_owned()));
        }

        let unit = match unit.trim_end_matches('s') {
            "second" | "sec" => IntervalUnit::Second,
            "minute" | "min" => IntervalUnit::Minute,
            "hour" | "hr" => IntervalUnit::Hour,
            "day" => IntervalUnit::Day,
            "week" | "wk" => IntervalUnit::Week,
            _ => return Err(ParseIntervalError(text.to_owned())),
        };
        Ok(Self { step, unit })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daily_bar_still_annualises_at_252() {
        // The number every existing result was computed with. If this ever
        // changes, every stored finding silently becomes incomparable.
        assert!((BarInterval::DAILY.periods_per_year() - 252.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intraday_resolutions_have_far_more_periods_than_a_day() {
        // The bug this type exists to prevent: a five-minute Sharpe computed
        // with 252 would be understated by about nine times.
        let five_minute = BarInterval::new(5, IntervalUnit::Minute);
        let daily = BarInterval::DAILY;
        let ratio = five_minute.periods_per_year() / daily.periods_per_year();
        assert!(
            (ratio - 78.0).abs() < 1e-9,
            "78 five-minute bars a session: {ratio}"
        );
    }

    #[test]
    fn an_hourly_bar_is_six_and_a_half_a_day() {
        let hourly = BarInterval::new(1, IntervalUnit::Hour);
        assert!((hourly.periods_per_year() - 252.0 * 6.5).abs() < 1e-9);
    }

    #[test]
    fn duration_is_what_a_bar_actually_covers() {
        assert_eq!(
            BarInterval::new(5, IntervalUnit::Minute).duration(),
            chrono::Duration::minutes(5)
        );
        assert_eq!(BarInterval::DAILY.duration(), chrono::Duration::days(1));
    }

    #[test]
    fn an_interval_round_trips_through_its_own_spelling() {
        for text in ["15second", "1minute", "5minute", "1hour", "1day", "1week"] {
            let parsed: BarInterval = text.parse().expect(text);
            assert_eq!(parsed.to_string(), text, "round trip for {text}");
        }
    }

    #[test]
    fn common_spellings_are_accepted() {
        assert_eq!(
            "day".parse::<BarInterval>().expect("bare unit means one"),
            BarInterval::DAILY
        );
        assert_eq!(
            "5 MINUTES".parse::<BarInterval>().expect("case and plural"),
            BarInterval::new(5, IntervalUnit::Minute)
        );
    }

    #[test]
    fn nonsense_is_refused_rather_than_defaulted() {
        for text in ["", "5", "fortnight", "0day", "-1day", "5minutes ago"] {
            assert!(
                text.parse::<BarInterval>().is_err(),
                "{text:?} should not parse"
            );
        }
    }

    #[test]
    fn only_sub_daily_resolutions_are_intraday() {
        assert!(BarInterval::new(5, IntervalUnit::Minute).is_intraday());
        assert!(BarInterval::new(1, IntervalUnit::Hour).is_intraday());
        assert!(!BarInterval::DAILY.is_intraday());
        assert!(!BarInterval::new(1, IntervalUnit::Week).is_intraday());
    }
}
