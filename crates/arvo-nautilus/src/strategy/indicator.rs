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

/// Exponentially weighted moving average over `period` values.
///
/// Seeded with the simple average of the first `period` values rather than
/// with the first value alone. Seeding on one value makes the early output
/// depend almost entirely on whichever bar happened to be first, and a rule
/// that enters on a cross would take that as signal — the same failure the
/// `None`-until-warm rule above exists to stop, arriving by a different route.
#[derive(Debug)]
pub(crate) struct Ema {
    /// `2 / (period + 1)`, the conventional weighting.
    alpha: f64,
    /// Decides when the first value appears, and what it is.
    seed: Sma,
    value: Option<f64>,
}

impl Ema {
    pub(crate) fn new(period: usize) -> Self {
        Self {
            alpha: 2.0 / (period as f64 + 1.0),
            seed: Sma::new(period),
            value: None,
        }
    }

    /// Feeds a value in, returning the average once `period` values are held.
    pub(crate) fn update(&mut self, value: f64) -> Option<f64> {
        match self.value {
            // Warm: the ordinary recurrence.
            Some(previous) => {
                let next = self.alpha * value + (1.0 - self.alpha) * previous;
                self.value = Some(next);
                Some(next)
            }
            // Warming: the seed decides when, and what, the first value is.
            None => {
                let seeded = self.seed.update(value)?;
                self.value = Some(seeded);
                Some(seeded)
            }
        }
    }

}

/// Wilder's relative strength index over `period` bars, as a percentage.
///
/// Needs `period + 1` values, not `period`: it is computed from *changes*, and
/// n values hold n-1 changes. Reporting after `period` values would be an
/// index over one change too few, which is the kind of off-by-one that never
/// looks wrong on a chart.
#[derive(Debug)]
pub(crate) struct Rsi {
    period: usize,
    previous: Option<f64>,
    /// Sums while warming; Wilder averages once warm.
    gains: f64,
    losses: f64,
    seen: usize,
    warm: bool,
}

impl Rsi {
    pub(crate) fn new(period: usize) -> Self {
        Self { period, previous: None, gains: 0.0, losses: 0.0, seen: 0, warm: false }
    }

    /// Feeds a value in, returning the index once `period + 1` are held.
    pub(crate) fn update(&mut self, value: f64) -> Option<f64> {
        // The first value has no change to measure, so there is nothing to
        // report and nothing to accumulate.
        let previous = self.previous.replace(value)?;
        let change = value - previous;
        let (gain, loss) = if change >= 0.0 { (change, 0.0) } else { (0.0, -change) };
        let period = self.period as f64;

        if self.warm {
            // Wilder's smoothing: an EMA with alpha = 1 / period.
            self.gains = (self.gains * (period - 1.0) + gain) / period;
            self.losses = (self.losses * (period - 1.0) + loss) / period;
        } else {
            self.gains += gain;
            self.losses += loss;
            self.seen += 1;
            if self.seen < self.period {
                return None;
            }
            self.gains /= period;
            self.losses /= period;
            self.warm = true;
        }

        // No losses at all is 100 by definition, and dividing would be a NaN
        // that every comparison against it reads as false.
        if self.losses <= 0.0 {
            return Some(if self.gains > 0.0 { 100.0 } else { 50.0 });
        }
        let strength = self.gains / self.losses;
        Some(100.0 - 100.0 / (1.0 + strength))
    }
}

/// Which of MACD's three series a rule is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacdLine {
    /// The fast EMA less the slow one.
    Macd,
    /// The EMA of the MACD line.
    Signal,
    /// MACD less signal — what a histogram draws.
    Histogram,
}

/// Moving average convergence/divergence.
///
/// One declaration yields one series, chosen by [`MacdLine`], because a rule
/// reads named numbers. Declare it twice under two names to compare the MACD
/// line against its signal; the second copy costs two EMAs and keeps the rule
/// language's shape — every indicator is a name and a number — intact.
#[derive(Debug)]
pub(crate) struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
    line: MacdLine,
}

impl Macd {
    pub(crate) fn new(fast: usize, slow: usize, signal: usize, line: MacdLine) -> Self {
        Self { fast: Ema::new(fast), slow: Ema::new(slow), signal: Ema::new(signal), line }
    }

    /// Feeds a value in. `None` until every average it needs is warm — for the
    /// signal and the histogram that is the slow period *plus* the signal
    /// period, because the signal is an average of a series that does not
    /// exist yet.
    pub(crate) fn update(&mut self, value: f64) -> Option<f64> {
        // Both are fed every bar, warm or not, so neither lags the other.
        let fast = self.fast.update(value);
        let slow = self.slow.update(value);
        let macd = fast? - slow?;
        match self.line {
            MacdLine::Macd => Some(macd),
            MacdLine::Signal => self.signal.update(macd),
            MacdLine::Histogram => Some(macd - self.signal.update(macd)?),
        }
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
/// hours, which run 13:30–20:00 UTC in summer and 14:30–21:00 in winter and
/// never cross midnight, and it is the
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
    fn an_ema_is_none_until_its_window_fills_and_is_seeded_on_the_average() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.update(10.0), None, "one value is not a three-bar average");
        assert_eq!(ema.update(20.0), None);
        // Seeded on the simple average of the first three, not on the first
        // value: seeding on one makes the early output depend on whichever bar
        // was first, and a cross rule would read that as signal.
        assert_eq!(ema.update(30.0), Some(20.0));
        // Then the recurrence, alpha = 2 / (3 + 1) = 0.5.
        assert_eq!(ema.update(40.0), Some(30.0));
        assert_eq!(ema.update(40.0), Some(35.0));
    }

    #[test]
    fn an_rsi_needs_one_more_bar_than_its_period_and_pins_its_ends() {
        // n values hold n-1 changes, so a 3-period index needs 4 bars. An index
        // reported after 3 would be over one change too few — the kind of
        // off-by-one that never looks wrong on a chart.
        let mut rsi = Rsi::new(3);
        assert_eq!(rsi.update(100.0), None, "no change yet");
        assert_eq!(rsi.update(101.0), None, "one change");
        assert_eq!(rsi.update(102.0), None, "two changes");
        let first = rsi.update(103.0).expect("three changes is enough");
        // Every change up: no losses at all, which is 100 by definition rather
        // than a division by zero.
        assert!((first - 100.0).abs() < 1e-9, "{first}");

        // Every change down is the mirror.
        let mut falling = Rsi::new(3);
        for value in [100.0, 99.0, 98.0, 97.0] {
            falling.update(value);
        }
        let low = falling.update(96.0).expect("warm");
        assert!(low < 1e-9, "all losses is 0, got {low}");

        // A flat series has neither gains nor losses; 50 is the honest answer
        // and a NaN would be read as false by every comparison against it.
        let mut flat = Rsi::new(3);
        let mut last = None;
        for _ in 0..6 {
            last = flat.update(50.0);
        }
        assert_eq!(last, Some(50.0));
    }

    #[test]
    fn a_macd_line_waits_for_its_slow_average_and_the_signal_waits_for_the_line() {
        // The line is available once the slow EMA is: 3 bars here.
        let mut line = Macd::new(2, 3, 2, MacdLine::Macd);
        assert_eq!(line.update(1.0), None);
        assert_eq!(line.update(2.0), None);
        assert!(line.update(3.0).is_some(), "the slow average is warm at 3");

        // The signal averages the line, which does not exist until then, so the
        // two warmups add: 3 + 2 - 1 = 4.
        let mut signal = Macd::new(2, 3, 2, MacdLine::Signal);
        assert_eq!(signal.update(1.0), None);
        assert_eq!(signal.update(2.0), None);
        assert_eq!(signal.update(3.0), None, "the line exists but the signal has one value");
        assert!(signal.update(4.0).is_some(), "the signal is warm at 4");

        // On a rising series the fast average leads, so the line is positive.
        let mut rising = Macd::new(2, 4, 2, MacdLine::Macd);
        let mut last = None;
        for step in 1..=10 {
            last = rising.update(f64::from(step));
        }
        assert!(last.expect("warm") > 0.0, "fast leads slow while rising: {last:?}");

        // The histogram is the line less its signal, which the two agree on.
        let mut hist = Macd::new(2, 4, 2, MacdLine::Histogram);
        let mut line_only = Macd::new(2, 4, 2, MacdLine::Macd);
        let mut sig_only = Macd::new(2, 4, 2, MacdLine::Signal);
        let mut seen = 0;
        for step in 1..=12 {
            let value = f64::from(step);
            if let (Some(h), Some(l), Some(g)) = (hist.update(value), line_only.update(value), sig_only.update(value)) {
                assert!((h - (l - g)).abs() < 1e-9, "histogram is line - signal, got {h} vs {}", l - g);
                seen += 1;
            }
        }
        assert!(seen > 0, "the three were never all warm at once");
    }

    /// #160: an indicator that is not ready yet says so in the one way a
    /// rule cannot mistake for a number.
    ///
    /// Every one of them is fed one observation short of its window and
    /// published as a [`arvo_data::Signal`], which is how a provider (#163)
    /// and a stored series (#164) will publish theirs. The assertion is the
    /// invariant itself: a predicate over what came out is false. It would
    /// fail the moment any of these started returning `Some(0.0)` while
    /// warming up — the quiet failure the signal type exists to stop, since
    /// `0.0` is a value every threshold below zero happily matches.
    #[test]
    fn no_indicator_reports_a_zero_where_it_means_it_does_not_know() {
        use arvo_data::{Signal, SignalName};

        let at = "2024-01-02T14:30:00".parse().expect("a timestamp");
        let mut sma = Sma::new(3);
        let mut momentum = Momentum::new(3);
        let mut atr = Atr::new(3);
        let mut donchian = Donchian::new(3);
        let mut vwap = SessionVwap::default();

        // Two bars, where each of these needs at least three — and five for
        // the session deviation, which is the rules module's own guard.
        let mut warming = Vec::new();
        for (high, low, close) in [(11.0, 9.0, 10.0), (12.0, 10.0, 11.0)] {
            donchian.push(high, low);
            vwap.push(high, low, close, 100.0);
            warming.push(("arvo.sma.3", sma.update(close)));
            warming.push(("arvo.momentum.3", momentum.update(close)));
            warming.push(("arvo.atr.3", atr.update(high, low, close)));
            warming.push(("arvo.donchian.3.high", donchian.channel().map(|(high, _)| high)));
            warming.push(("arvo.vwap.deviation", vwap.deviation(5)));
        }

        for (name, value) in warming {
            let signal = Signal::new(SignalName::new(name).expect("a name"), at, value);
            assert_eq!(signal.value, None, "{name} answered before its window was full");
            assert!(!signal.at_most(0.0), "{name} would satisfy a rule meaning 'at or below zero'");
            assert!(!signal.satisfies(|_| true), "{name} satisfies nothing while it is absent");
        }

        // And once it is full, the same signal answers.
        let ready = Signal::new(SignalName::new("arvo.sma.3").expect("a name"), at, sma.update(12.0));
        assert!(ready.at_least(10.0), "a full window is a value like any other");
    }

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
