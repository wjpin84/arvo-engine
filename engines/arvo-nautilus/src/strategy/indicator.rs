//! The running calculations strategies are built out of.
//!
//! Hand-rolled rather than pulling in `nautilus-indicators`: each is a ring
//! buffer and a running total, and one fewer pinned `0.x` crate is worth more
//! than the lines saved.
//!
//! Every one of them returns `None` until its window is actually full. That is
//! the single most important property here — an average over three of a ten-bar
//! window is not a ten-bar average, and treating it as one manufactures signal
//! at the start of every backtest, in the direction of whatever the first few
//! bars happened to do.

use std::collections::VecDeque;

use chrono::NaiveDate;
use nautilus_core::UnixNanos;

/// Simple moving average over the last `period` values.
#[derive(Debug)]
pub(crate) struct Sma {
    pub(crate) period: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl Sma {
    pub(crate) fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
        }
    }

    /// Feeds a value in, returning the average once `period` values are held.
    pub(crate) fn update(&mut self, value: f64) -> Option<f64> {
        self.window.push_back(value);
        self.sum += value;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.sum / self.period as f64)
    }
}

/// Return over a fixed lookback: what this instrument did, as one number.
///
/// The measure a cross-sectional rule ranks on. Deliberately the plainest one
/// there is — price now against price `period` bars ago — because the
/// question that rule asks is whether *ranking* adds anything, and a clever
/// measure would confound the answer with itself.
///
/// A fraction rather than a ratio, so instruments at different prices are
/// comparable, which is the whole point of ranking them.
pub(crate) struct Momentum {
    pub(crate) period: usize,
    window: VecDeque<f64>,
}

impl Momentum {
    pub(crate) fn new(period: usize) -> Self {
        Self {
            period,
            // One more than the period: the return spans `period` intervals
            // and therefore needs `period + 1` observations.
            window: VecDeque::with_capacity(period + 1),
            }
    }

    /// Feeds a close in, returning the return over the lookback once there is
    /// one to compute.
    pub(crate) fn update(&mut self, close: f64) -> Option<f64> {
        self.window.push_back(close);
        if self.window.len() > self.period + 1 {
            self.window.pop_front();
        }
        if self.window.len() <= self.period {
            return None;
        }
        let oldest = *self.window.front()?;
        // A series that reached zero has no meaningful return over it, and a
        // division here would produce an infinity that sorts above everything.
        (oldest > 0.0).then(|| close / oldest - 1.0)
    }
}

/// Average true range: how far this instrument actually moves in a bar.
///
/// True range is the widest of the bar's own span and the two gaps to the
/// previous close, so an overnight gap counts as movement rather than being
/// invisible. That matters for a stop: a gap is exactly the move a stop exists
/// to survive, and a range that ignored it would size stops off the calm part
/// of the distribution.
#[derive(Debug)]
pub(crate) struct Atr {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
    previous_close: Option<f64>,
}

impl Atr {
    pub(crate) fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
            previous_close: None,
        }
    }

    /// Feeds a bar in, returning the average once `period` ranges are held.
    pub(crate) fn update(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let range = match self.previous_close {
            None => high - low,
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
        };
        self.previous_close = Some(close);

        self.window.push_back(range);
        self.sum += range;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.sum / self.period as f64)
    }
}

/// Highest high and lowest low over a trailing window — a Donchian channel.
///
/// The window deliberately excludes the bar being judged. A breakout rule
/// comparing today's close against a range that *includes* today's high can
/// never fire on a new high, because the high is already in the range; the
/// same rule written the obvious way silently becomes a different, weaker
/// rule. So [`Self::channel`] is read before the bar is pushed in.
///
/// ponytail: rescans the window on each read, O(period) per bar. A monotonic
/// deque makes it O(1) amortised; worth doing if periods ever reach the
/// thousands, which for a breakout lookback they do not.
#[derive(Debug)]
pub(crate) struct Donchian {
    period: usize,
    window: VecDeque<(f64, f64)>,
}

impl Donchian {
    pub(crate) fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
        }
    }

    /// The channel over the bars seen *so far*, excluding whatever is about to
    /// be pushed. `None` until the window is full.
    pub(crate) fn channel(&self) -> Option<(f64, f64)> {
        if self.window.len() < self.period {
            return None;
        }
        let highest = self
            .window
            .iter()
            .map(|(high, _)| *high)
            .fold(f64::NEG_INFINITY, f64::max);
        let lowest = self
            .window
            .iter()
            .map(|(_, low)| *low)
            .fold(f64::INFINITY, f64::min);
        Some((highest, lowest))
    }

    pub(crate) fn push(&mut self, high: f64, low: f64) {
        self.window.push_back((high, low));
        if self.window.len() > self.period {
            self.window.pop_front();
        }
    }
}

/// Volume-weighted average price since the session opened, with its spread.
///
/// Anchored to the session rather than to a rolling window, which is what
/// makes it VWAP rather than a moving average: the number traders watch is
/// "the average price paid *today*", and it resets when today does.
///
/// The spread is a genuine volume-weighted standard deviation, accumulated
/// from running sums of `w`, `w·p` and `w·p²`. A band drawn at a fixed
/// percentage instead would be far too wide in a quiet session and far too
/// tight in a violent one, which is the whole distinction the strategy trades.
#[derive(Debug, Default)]
pub(crate) struct SessionVwap {
    weight: f64,
    weighted_price: f64,
    weighted_square: f64,
    bars: usize,
}

impl SessionVwap {
    /// Starts a new session. Everything accumulated so far is discarded.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Adds one bar, using its typical price as the price traded.
    pub(crate) fn push(&mut self, high: f64, low: f64, close: f64, volume: f64) {
        // A bar with no reported volume still happened. Weighting it at zero
        // would drop it from the average entirely, and some feeds report zero
        // volume on thin bars rather than omitting them.
        let weight = if volume > 0.0 { volume } else { 1.0 };
        let price = (high + low + close) / 3.0;
        self.weight += weight;
        self.weighted_price += weight * price;
        self.weighted_square += weight * price * price;
        self.bars += 1;
    }

    /// The session's volume-weighted average price.
    pub(crate) fn value(&self) -> Option<f64> {
        (self.weight > 0.0).then(|| self.weighted_price / self.weight)
    }

    /// Volume-weighted standard deviation of price around the VWAP.
    ///
    /// Needs `min_bars` before it will answer. A deviation computed from two
    /// bars is near zero, so every price looks like a huge number of standard
    /// deviations from the mean, and a reversion rule reading it would fire on
    /// the second bar of every session.
    pub(crate) fn deviation(&self, min_bars: usize) -> Option<f64> {
        if self.bars < min_bars || self.weight <= 0.0 {
            return None;
        }
        let mean = self.weighted_price / self.weight;
        let variance = (self.weighted_square / self.weight) - mean * mean;
        // Floating-point cancellation can push a genuinely-zero variance a
        // hair below zero. A negative variance is not a small number, it is a
        // NaN waiting to be compared against.
        (variance > 0.0).then(|| variance.sqrt())
    }
}

/// Which trading day a bar belongs to.
///
/// A session boundary is a change of UTC date. That holds for US regular
/// hours, which run 13:30–20:00 UTC and never cross midnight, and it is the
/// only market this crate's instrument conventions describe. A market whose
/// session spans UTC midnight would need the exchange calendar instead, and
/// would silently see two sessions where there is one — so the assumption is
/// named here rather than left implied.
#[derive(Debug, Default)]
pub(crate) struct Session {
    day: Option<NaiveDate>,
}

impl Session {
    /// Records the bar and says whether it opened a new session.
    ///
    /// The very first bar counts as opening one: a strategy that waits for a
    /// *change* would sit out its first day.
    pub(crate) fn advance(&mut self, at: UnixNanos) -> bool {
        let Some(day) = date_of(at) else {
            return false;
        };
        let changed = self.day != Some(day);
        self.day = Some(day);
        changed
    }
}

fn date_of(at: UnixNanos) -> Option<NaiveDate> {
    let nanos = i64::try_from(at.as_u64()).ok()?;
    Some(chrono::DateTime::from_timestamp_nanos(nanos).date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn momentum_is_withheld_until_the_lookback_is_covered() {
        // A return over five bars needs six observations, and reporting one
        // early would rank an instrument on a shorter window than its peers.
        let mut momentum = Momentum::new(5);
        for close in [100.0, 101.0, 102.0, 103.0, 104.0] {
            assert_eq!(momentum.update(close), None);
        }
        assert!(momentum.update(110.0).is_some());
    }

    #[test]
    fn momentum_is_a_fraction_so_instruments_at_different_prices_compare() {
        // The whole point of ranking. A ten-dollar stock that doubled and a
        // thousand-dollar stock that doubled rank equally.
        let mut cheap = Momentum::new(2);
        let mut dear = Momentum::new(2);
        for (a, b) in [(10.0, 1000.0), (10.0, 1000.0), (20.0, 2000.0)] {
            let (x, y) = (cheap.update(a), dear.update(b));
            if let (Some(x), Some(y)) = (x, y) {
                assert!((x - y).abs() < 1e-12, "{x} vs {y}");
                assert!((x - 1.0).abs() < 1e-12, "both doubled");
            }
        }
    }

    #[test]
    fn a_series_through_zero_reports_nothing_rather_than_an_infinity() {
        // An infinity sorts above every real instrument, so a rule ranking on
        // it would hold whichever series was most broken.
        let mut momentum = Momentum::new(1);
        assert_eq!(momentum.update(0.0), None);
        assert_eq!(momentum.update(50.0), None, "no return over a zero base");
    }

    #[test]
    fn an_average_is_withheld_until_its_window_is_full() {
        let mut sma = Sma::new(3);
        assert_eq!(sma.update(1.0), None);
        assert_eq!(sma.update(2.0), None);
        assert_eq!(sma.update(3.0), Some(2.0));
    }

    #[test]
    fn true_range_counts_an_overnight_gap() {
        let mut atr = Atr::new(2);
        // First bar has no previous close, so its range is just high-low.
        assert_eq!(atr.update(10.0, 9.0, 10.0), None);
        // Second bar opens far below and never touches the old close: its own
        // span is 1.0, but the real move from 10.0 down to 6.0 is 4.0. A range
        // that ignored the gap would size every stop off the calm part of the
        // distribution.
        let value = atr.update(7.0, 6.0, 6.5).expect("two ranges held");
        assert!((value - (1.0 + 4.0) / 2.0).abs() < 1e-12, "{value}");
    }

    #[test]
    fn an_average_true_range_waits_for_its_window() {
        let mut atr = Atr::new(3);
        assert_eq!(atr.update(10.0, 9.0, 9.5), None);
        assert_eq!(atr.update(10.0, 9.0, 9.5), None);
        assert!(atr.update(10.0, 9.0, 9.5).is_some(), "three ranges is three");
    }

    #[test]
    fn a_channel_excludes_the_bar_about_to_be_judged() {
        // The bug this ordering prevents: a new high compared against a
        // channel that already contains it can never be a breakout, so the
        // rule quietly stops being a breakout rule.
        let mut donchian = Donchian::new(2);
        donchian.push(10.0, 9.0);
        donchian.push(11.0, 10.0);

        let (highest, lowest) = donchian.channel().expect("two bars held");
        assert!((highest - 11.0).abs() < f64::EPSILON);
        assert!((lowest - 9.0).abs() < f64::EPSILON);

        // A bar making a new high of 12 is above the channel it is judged
        // against, which is what lets the breakout fire.
        assert!(12.0 > highest);
    }

    #[test]
    fn a_channel_is_withheld_until_its_window_is_full() {
        let mut donchian = Donchian::new(3);
        donchian.push(10.0, 9.0);
        assert_eq!(donchian.channel(), None);
        donchian.push(10.0, 9.0);
        assert_eq!(donchian.channel(), None);
        donchian.push(10.0, 9.0);
        assert!(donchian.channel().is_some());
    }

    #[test]
    fn vwap_weights_by_volume_rather_than_counting_bars() {
        let mut vwap = SessionVwap::default();
        // One bar at 10 on tiny volume, one at 20 on large volume. A plain
        // mean says 15; the volume-weighted answer is much nearer 20, which
        // is the price people actually paid.
        vwap.push(10.0, 10.0, 10.0, 1.0);
        vwap.push(20.0, 20.0, 20.0, 9.0);
        let value = vwap.value().expect("two bars");
        assert!((value - 19.0).abs() < 1e-12, "{value}");
    }

    #[test]
    fn a_session_reset_forgets_the_previous_day() {
        let mut vwap = SessionVwap::default();
        vwap.push(10.0, 10.0, 10.0, 100.0);
        vwap.reset();
        assert_eq!(vwap.value(), None, "yesterday is not part of today's VWAP");
    }

    #[test]
    fn deviation_is_withheld_until_there_are_enough_bars_to_mean_anything() {
        let mut vwap = SessionVwap::default();
        vwap.push(10.0, 10.0, 10.0, 1.0);
        vwap.push(11.0, 11.0, 11.0, 1.0);
        assert_eq!(
            vwap.deviation(5),
            None,
            "two bars have a near-zero spread, so everything looks extreme"
        );
        for price in [12.0, 9.0, 10.5] {
            vwap.push(price, price, price, 1.0);
        }
        assert!(vwap.deviation(5).is_some());
    }

    #[test]
    fn a_flat_session_has_no_deviation_rather_than_a_negative_one() {
        // Every bar at the same price: the variance is exactly zero, and
        // floating-point cancellation can land it a hair below. `sqrt` of
        // that is NaN, and NaN compares false against everything — a reversion
        // rule would simply stop firing, silently.
        let mut vwap = SessionVwap::default();
        for _ in 0..10 {
            vwap.push(10.0, 10.0, 10.0, 100.0);
        }
        assert_eq!(vwap.deviation(5), None);
    }

    #[test]
    fn a_session_boundary_is_a_change_of_day() {
        let nanos = |day: u32, hour: u32| {
            let at = NaiveDate::from_ymd_opt(2026, 8, day)
                .expect("valid")
                .and_hms_opt(hour, 0, 0)
                .expect("valid")
                .and_utc();
            UnixNanos::from(u64::try_from(at.timestamp_nanos_opt().expect("in range")).expect("positive"))
        };

        let mut session = Session::default();
        assert!(session.advance(nanos(24, 14)), "the first bar opens one");
        assert!(!session.advance(nanos(24, 19)), "same day, same session");
        assert!(session.advance(nanos(25, 14)), "next day, next session");
    }
}
