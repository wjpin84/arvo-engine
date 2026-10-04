//! When the US regular session is open.
//!
//! Three things assume a bar is from regular hours and none of them can see
//! when it is not: [`BarInterval::periods_per_year`](crate::BarInterval)
//! counts a 390-minute day, an opening range is built from a session's first
//! bars, and a session VWAP averages whatever the session held. A pre-market
//! bar at 04:00 breaks all three at once, silently — the opening range becomes
//! the thinnest prints of the day.
//!
//! So this is the one statement of what regular hours are, used to keep
//! extended-hours bars out at the source and to flag any already on disk.
//!
//! # Daylight saving, without a timezone database
//!
//! The session is 09:30–16:00 New York time, which is 13:30 UTC in summer and
//! 14:30 in winter. A check written as a fixed UTC window is right for half the
//! year. The US rule is short enough to state here rather than pull in the
//! tz database for one question, and clocks change at 02:00 on a Sunday, so
//! the date alone decides every instant a session can contain.
//!
//! # A clock asks this too, not only a bar
//!
//! A bar on a Saturday does not exist, so for bars the time of day was the
//! whole question. But the option-quote recorder and a live session ask it of
//! *now*, and now is a Saturday twice a week: the recorder kept Friday's close
//! every fifteen minutes all weekend, and a session called a silent feed dark
//! (arvo-engine#28). So a weekend is outside the session, whatever the time.
//!
//! ponytail: no exchange calendar, so an early close (13:00 the day after
//! Thanksgiving) still counts 13:00–16:00 as regular, and a weekday holiday
//! counts as open. No bars exist to misclassify on a holiday; a clock that
//! asks on one is told the market is open, and the recorder's own check for an
//! unchanged chain is what keeps it quiet. Add the calendar when a third
//! caller needs the day rather than the hour.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

/// The IANA zone the session above is stated in, for a reader that draws
/// bars in exchange time. Bars themselves are UTC.
pub const ZONE: &str = "America/New_York";

/// Whether a bar opening at `open` (UTC) falls inside the US regular session:
/// 09:30 to 16:00 New York time, Monday to Friday.
#[must_use]
pub fn in_regular_session(open: NaiveDateTime) -> bool {
    let offset = if daylight_saving(open.date()) { 4 } else { 5 };
    let local = open - Duration::hours(offset);
    if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let (start, end) = (
        NaiveTime::from_hms_opt(9, 30, 0).expect("valid"),
        NaiveTime::from_hms_opt(16, 0, 0).expect("valid"),
    );
    local.time() >= start && local.time() < end
}

/// The regular close on `date`, 16:00 New York, as a UTC instant.
#[must_use]
pub fn regular_close(date: NaiveDate) -> NaiveDateTime {
    let offset = if daylight_saving(date) { 4 } else { 5 };
    date.and_hms_opt(16, 0, 0).expect("valid") + Duration::hours(offset)
}

/// Whether US Eastern time is on daylight saving on `date`.
fn daylight_saving(date: NaiveDate) -> bool {
    let year = date.year();
    let sunday = |month, n| NaiveDate::from_weekday_of_month_opt(year, month, Weekday::Sun, n);
    let (start, end) = if year >= 2007 {
        // Energy Policy Act of 2005: second Sunday of March to the first of
        // November.
        (sunday(3, 2), sunday(11, 1))
    } else {
        // Before it: first Sunday of April to the last of October.
        (sunday(4, 1), sunday(10, 5).or_else(|| sunday(10, 4)))
    };
    match (start, end) {
        (Some(start), Some(end)) => date >= start && date < end,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .expect("valid")
            .and_hms_opt(hour, minute, 0)
            .expect("valid")
    }

    #[test]
    fn the_open_is_an_hour_later_in_utc_in_winter() {
        // The mistake a fixed UTC window makes half the year.
        assert!(in_regular_session(utc(2024, 7, 1, 13, 30)), "09:30 EDT");
        assert!(!in_regular_session(utc(2024, 1, 2, 13, 30)), "08:30 EST");
        assert!(in_regular_session(utc(2024, 1, 2, 14, 30)), "09:30 EST");
    }

    #[test]
    fn the_last_bar_opens_before_four_and_the_close_itself_is_after_hours() {
        assert!(in_regular_session(utc(2024, 7, 1, 19, 55)), "15:55 EDT");
        assert!(!in_regular_session(utc(2024, 7, 1, 20, 0)), "16:00 EDT");
        assert!(in_regular_session(utc(2024, 1, 2, 20, 55)), "15:55 EST");
    }

    #[test]
    fn pre_market_and_after_hours_are_outside() {
        assert!(!in_regular_session(utc(2024, 7, 1, 8, 0)), "04:00 EDT");
        assert!(!in_regular_session(utc(2024, 7, 1, 23, 55)), "19:55 EDT");
    }

    #[test]
    fn a_weekend_is_outside_the_session_at_any_hour() {
        // 2026-10-02 is a Friday. The recorder ran at this hour on the two
        // days after it and wrote Friday's close 26 times each (arvo-engine#28).
        assert!(in_regular_session(utc(2026, 10, 2, 14, 38)), "Friday 10:38 EDT");
        assert!(!in_regular_session(utc(2026, 10, 3, 14, 38)), "Saturday 10:38 EDT");
        assert!(!in_regular_session(utc(2026, 10, 4, 14, 38)), "Sunday 10:38 EDT");
        assert!(in_regular_session(utc(2026, 10, 5, 14, 38)), "Monday 10:38 EDT");
    }

    #[test]
    fn the_clocks_change_on_the_dates_the_law_says() {
        // 2024: 10 March and 3 November.
        assert!(!daylight_saving(
            NaiveDate::from_ymd_opt(2024, 3, 9).expect("valid")
        ));
        assert!(daylight_saving(
            NaiveDate::from_ymd_opt(2024, 3, 11).expect("valid")
        ));
        assert!(daylight_saving(
            NaiveDate::from_ymd_opt(2024, 11, 1).expect("valid")
        ));
        assert!(!daylight_saving(
            NaiveDate::from_ymd_opt(2024, 11, 4).expect("valid")
        ));
        // 2006, the old rule: 2 April and 29 October.
        assert!(!daylight_saving(
            NaiveDate::from_ymd_opt(2006, 3, 31).expect("valid")
        ));
        assert!(daylight_saving(
            NaiveDate::from_ymd_opt(2006, 4, 3).expect("valid")
        ));
        assert!(!daylight_saving(
            NaiveDate::from_ymd_opt(2006, 10, 30).expect("valid")
        ));
    }
}
