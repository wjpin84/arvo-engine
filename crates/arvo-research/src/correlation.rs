//! Pairwise correlation from bars already seen.
//!
//! # Why this exists
//!
//! [`crate::RiskModel::correlation_cap`] caps positions among instruments that
//! move together — five names each mildly correlated with the index and almost
//! perfectly correlated with *each other* is one bet wearing five names. The cap
//! shipped with nothing able to evaluate it, and a cap with no source refuses
//! every entry, so configuring one made a backtest report zero trades.
//!
//! # Why it is fed rather than computed up front
//!
//! Because computing correlations over the whole backtest window and then using
//! them to gate trades inside that window is look-ahead. It would refuse a trade
//! in March on the strength of how two instruments moved in November.
//!
//! So this is a *rolling* estimate: bars are pushed as they arrive, and
//! [`Correlations::between`] answers from what has been pushed and nothing else.
//! In a backtest that is the bars the engine has already delivered; live it is
//! the bars that have already printed. Same type, same answer shape, both sides
//! — the same argument as [`crate::risk::decide`].
//!
//! # Returns, not prices
//!
//! Correlating price *levels* measures whether two instruments both drifted
//! upward, which almost everything does — SPY and a random rising line correlate
//! near 1.0 on levels and near 0.0 on returns. The second number is the one that
//! says whether two positions are the same bet.
//!
//! # Paired on time, not on position
//!
//! Two instruments do not always have a bar for the same instant: one halts, one
//! lists late, one is thinly traded. Pairing the *n*-th return of each would
//! silently compare Tuesday against Thursday and drift further apart with every
//! missing bar. Only instants present in both series are compared, which is the
//! same rule `arvo_data::agreement` uses when it compares two vendors.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::NaiveDateTime;

use crate::risk::Correlations;

/// How many paired returns are needed before a correlation is worth stating.
///
/// Thirty. Below that the estimate is dominated by whichever few days happen to
/// be in it, and a cap acting on it would be refusing trades on noise. It is the
/// same reasoning as the trade-count bar in evaluation: a statistic over a
/// handful of observations describes the handful.
pub const MIN_PAIRED_RETURNS: usize = 30;

/// How many returns the estimate looks back over.
///
/// Ninety, which is a trading quarter on daily bars. Long enough that one
/// turbulent fortnight does not decide the answer, short enough that a
/// correlation that genuinely broke down is not defended by a year of history.
///
/// A window rather than all history, because correlation is not stable: two
/// instruments that moved together through one regime routinely stop, and an
/// all-history estimate would keep insisting they are one bet long after they
/// stopped being one.
pub const DEFAULT_WINDOW: usize = 90;

/// Pairwise correlation of returns over a trailing window.
///
/// Interior mutability because [`Correlations`] takes `&self` and because a
/// book's members are separate strategy instances sharing one of these. Wrap it
/// in an `Arc` and hand every instance a handle.
#[derive(Debug)]
pub struct RollingCorrelations {
    window: usize,
    min_paired: usize,
    /// Closes by instrument and instant. `BTreeMap` on the inner key so the
    /// series stays in time order however bars arrive, which matters because
    /// returns are differences between *adjacent* observations.
    closes: Mutex<BTreeMap<String, BTreeMap<NaiveDateTime, f64>>>,
}

impl Default for RollingCorrelations {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW, MIN_PAIRED_RETURNS)
    }
}

impl RollingCorrelations {
    #[must_use]
    pub fn new(window: usize, min_paired: usize) -> Self {
        Self {
            window: window.max(2),
            min_paired: min_paired.max(2),
            closes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records one bar's close.
    ///
    /// Call this for every bar of every instrument the run touches, before any
    /// risk decision on that bar. A bar that is not pushed is a bar this cannot
    /// see, which is the whole point — but it also means an instrument nobody
    /// feeds correlates with nothing and reads as *unknown*, not as
    /// uncorrelated.
    pub fn observe(&self, instrument: &str, at: NaiveDateTime, close: f64) {
        if !close.is_finite() || close <= 0.0 {
            // A non-positive close makes a return meaningless rather than
            // merely wrong. Dropping it keeps one bad bar from poisoning every
            // pair the instrument is in.
            return;
        }
        let mut closes = self
            .closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let series = closes.entry(instrument.to_owned()).or_default();
        series.insert(at, close);

        // One more than the window, because n returns need n+1 closes.
        while series.len() > self.window + 1 {
            let oldest = *series.keys().next().expect("non-empty");
            series.remove(&oldest);
        }
    }

    /// How many instruments have been fed anything.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Paired returns for two instruments, oldest first.
    ///
    /// A return is only produced where *both* instruments have the bar before it
    /// as well, so a gap in either series breaks the pair on both sides rather
    /// than quietly spanning it.
    fn paired(&self, first: &str, second: &str) -> Option<(Vec<f64>, Vec<f64>)> {
        let closes = self
            .closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let left = closes.get(first)?;
        let right = closes.get(second)?;

        let shared: Vec<NaiveDateTime> = left
            .keys()
            .filter(|at| right.contains_key(*at))
            .copied()
            .collect();

        let mut a = Vec::with_capacity(shared.len());
        let mut b = Vec::with_capacity(shared.len());
        for pair in shared.windows(2) {
            let (previous, current) = (pair[0], pair[1]);
            let (lp, lc) = (left[&previous], left[&current]);
            let (rp, rc) = (right[&previous], right[&current]);
            a.push((lc - lp) / lp);
            b.push((rc - rp) / rp);
        }
        Some((a, b))
    }
}

impl Correlations for RollingCorrelations {
    /// Pearson correlation of the two return series.
    ///
    /// `None` — meaning *unknown*, which the gate refuses on rather than
    /// treating as uncorrelated — when either instrument is unseen, when fewer
    /// than [`MIN_PAIRED_RETURNS`] returns overlap, or when either series never
    /// moved. A constant series has zero variance and no correlation with
    /// anything; reporting 0.0 there would claim two instruments are independent
    /// on the evidence that one of them did nothing.
    fn between(&self, first: &str, second: &str) -> Option<f64> {
        if first == second {
            // Not a special case so much as the honest answer: an instrument is
            // perfectly correlated with itself, and the cap treating it as one
            // bet with itself is correct.
            return self.closes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(first)
                .then_some(1.0);
        }

        let (a, b) = self.paired(first, second)?;
        if a.len() < self.min_paired {
            return None;
        }

        #[expect(clippy::cast_precision_loss, reason = "windows are dozens of bars")]
        let n = a.len() as f64;
        let mean_a = a.iter().sum::<f64>() / n;
        let mean_b = b.iter().sum::<f64>() / n;

        let mut covariance = 0.0;
        let mut var_a = 0.0;
        let mut var_b = 0.0;
        for (x, y) in a.iter().zip(&b) {
            let (dx, dy) = (x - mean_a, y - mean_b);
            covariance += dx * dy;
            var_a += dx * dx;
            var_b += dy * dy;
        }

        let denominator = (var_a * var_b).sqrt();
        if denominator <= 0.0 || !denominator.is_finite() {
            return None;
        }
        // Clamped: floating-point error can put a perfect correlation a hair
        // outside the range, and a caller comparing against a threshold of 1.0
        // should not be at the mercy of the last bit.
        Some((covariance / denominator).clamp(-1.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};

    fn day(n: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 1, 1)
            .expect("valid")
            .and_time(NaiveTime::MIN)
            + chrono::Duration::days(i64::from(n))
    }

    /// Feeds `closes` as consecutive daily bars.
    fn feed(tracker: &RollingCorrelations, instrument: &str, closes: &[f64]) {
        for (index, close) in closes.iter().enumerate() {
            tracker.observe(
                instrument,
                day(u32::try_from(index).expect("small")),
                *close,
            );
        }
    }

    /// A series that rises by `step` each bar, then falls, alternating — so it
    /// has real variance rather than a constant drift.
    fn wiggle(len: usize, base: f64, step: f64) -> Vec<f64> {
        (0..len)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "short fixtures")]
                let i = i as f64;
                base + step * (i % 7.0 - 3.0)
            })
            .collect()
    }

    #[test]
    fn two_instruments_moving_together_correlate_near_one() {
        let tracker = RollingCorrelations::default();
        let series = wiggle(60, 100.0, 2.0);
        feed(&tracker, "QQQ.RH", &series);
        // Same shape at a different level and scale: same bet, different size.
        let levered: Vec<f64> = series.iter().map(|c| c * 3.0).collect();
        feed(&tracker, "TQQQ.RH", &levered);

        let rho = tracker
            .between("QQQ.RH", "TQQQ.RH")
            .expect("enough paired returns");
        assert!(rho > 0.99, "same shape should be near 1.0, got {rho}");
    }

    #[test]
    fn two_instruments_moving_oppositely_correlate_near_minus_one() {
        // The cap keys on absolute correlation: a perfectly inverse pair is
        // also one bet, held both ways.
        let tracker = RollingCorrelations::default();
        let series = wiggle(60, 100.0, 2.0);
        feed(&tracker, "SPY.RH", &series);
        let inverse: Vec<f64> = series.iter().map(|c| 300.0 - c).collect();
        feed(&tracker, "SH.RH", &inverse);

        let rho = tracker.between("SPY.RH", "SH.RH").expect("paired");
        assert!(rho < -0.9, "inverse shape should be near -1.0, got {rho}");
    }

    #[test]
    fn correlation_is_of_returns_not_of_price_levels() {
        // The distinction the whole file rests on. Both series rise
        // monotonically, so their *levels* correlate at 1.0 — but their
        // bar-to-bar returns are unrelated, and that is what says whether two
        // positions are the same bet.
        let tracker = RollingCorrelations::default();
        let mut rising = 100.0;
        let mut jagged = 100.0;
        for n in 0..60 {
            let i = f64::from(n);
            rising += 1.0;
            // Rises too, but by a step that swings independently.
            jagged += if n % 2 == 0 { 0.2 } else { 3.0 + (i % 3.0) };
            tracker.observe("STEADY.RH", day(n), rising);
            tracker.observe("JAGGED.RH", day(n), jagged);
        }

        let rho = tracker.between("STEADY.RH", "JAGGED.RH").expect("paired");
        assert!(
            rho.abs() < 0.9,
            "levels both rise, but returns should not read as one bet: {rho}"
        );
    }

    #[test]
    fn an_instrument_nobody_has_fed_is_unknown_not_uncorrelated() {
        // The gate refuses on None. Returning 0.0 here would let an unseen
        // instrument through a correlation cap on the evidence that nothing is
        // known about it.
        let tracker = RollingCorrelations::default();
        feed(&tracker, "QQQ.RH", &wiggle(60, 100.0, 2.0));
        assert_eq!(tracker.between("QQQ.RH", "NEVER.RH"), None);
        assert_eq!(tracker.between("NEVER.RH", "ALSONEVER.RH"), None);
    }

    #[test]
    fn too_few_paired_returns_is_unknown() {
        // Early in a run nothing is known yet, so a configured cap refuses —
        // the same shape as refusing to enter before the ATR has warmed up.
        let tracker = RollingCorrelations::default();
        feed(&tracker, "A.RH", &wiggle(10, 100.0, 2.0));
        feed(&tracker, "B.RH", &wiggle(10, 50.0, 1.0));
        assert_eq!(tracker.between("A.RH", "B.RH"), None, "10 bars is not 30");
    }

    #[test]
    fn only_instants_present_in_both_series_are_compared() {
        // Pairing the n-th return of each would compare Tuesday against
        // Thursday and drift further apart with every missing bar.
        let tracker = RollingCorrelations::default();
        let series = wiggle(60, 100.0, 2.0);
        feed(&tracker, "DENSE.RH", &series);
        // Same instrument, every other bar missing.
        for (index, close) in series.iter().enumerate().filter(|(i, _)| i % 2 == 0) {
            tracker.observe(
                "SPARSE.RH",
                day(u32::try_from(index).expect("small")),
                *close,
            );
        }

        // 30 shared instants gives 29 paired returns — one short of the bar,
        // which is itself the point: the sparse series has less evidence and
        // this says so rather than padding it.
        assert_eq!(tracker.between("DENSE.RH", "SPARSE.RH"), None);
    }

    #[test]
    fn a_series_that_never_moved_correlates_with_nothing() {
        // Zero variance. Reporting 0.0 would claim two instruments are
        // independent on the evidence that one of them did nothing.
        let tracker = RollingCorrelations::default();
        feed(&tracker, "MOVES.RH", &wiggle(60, 100.0, 2.0));
        feed(&tracker, "FLAT.RH", &[50.0; 60]);
        assert_eq!(tracker.between("MOVES.RH", "FLAT.RH"), None);
    }

    #[test]
    fn the_answer_only_uses_bars_already_pushed() {
        // The no-look-ahead property, asserted rather than assumed. Correlating
        // over the whole window up front would refuse a trade in March on the
        // strength of how two instruments moved in November.
        let tracker = RollingCorrelations::default();
        let a = wiggle(60, 100.0, 2.0);
        let b: Vec<f64> = a.iter().map(|c| c * 2.0).collect();

        // Halfway through, there is not yet enough to answer.
        feed(&tracker, "A.RH", &a[..20]);
        feed(&tracker, "B.RH", &b[..20]);
        assert_eq!(tracker.between("A.RH", "B.RH"), None, "20 bars is not 30");

        // The rest arrives and the answer appears. Nothing about the tail was
        // available before it was pushed.
        for (index, (x, y)) in a.iter().zip(&b).enumerate().skip(20) {
            let at = day(u32::try_from(index).expect("small"));
            tracker.observe("A.RH", at, *x);
            tracker.observe("B.RH", at, *y);
        }
        assert!(tracker.between("A.RH", "B.RH").expect("now known") > 0.99);
    }

    #[test]
    fn the_window_forgets_correlation_that_broke_down() {
        // Correlation is not stable. An all-history estimate would keep
        // insisting two instruments are one bet long after they stopped.
        let tracker = RollingCorrelations::new(40, 30);
        let together = wiggle(50, 100.0, 2.0);
        let apart = wiggle(50, 100.0, 2.0);

        // Phase one: identical.
        for (index, close) in together.iter().enumerate() {
            let at = day(u32::try_from(index).expect("small"));
            tracker.observe("A.RH", at, *close);
            tracker.observe("B.RH", at, *close);
        }
        assert!(tracker.between("A.RH", "B.RH").expect("paired") > 0.99);

        // Phase two: B goes its own way for longer than the window.
        for (offset, close) in apart.iter().enumerate() {
            let index = 50 + offset;
            let at = day(u32::try_from(index).expect("small"));
            tracker.observe("A.RH", at, together[offset]);
            let drift = f64::from(u32::try_from(offset).expect("short fixture")) * 1.7 % 11.0;
            tracker.observe("B.RH", at, close + drift);
        }

        let rho = tracker.between("A.RH", "B.RH").expect("paired");
        assert!(
            rho < 0.99,
            "the old regime should have rolled out of the window, got {rho}"
        );
    }

    #[test]
    fn an_instrument_is_one_bet_with_itself() {
        let tracker = RollingCorrelations::default();
        feed(&tracker, "QQQ.RH", &wiggle(60, 100.0, 2.0));
        assert_eq!(tracker.between("QQQ.RH", "QQQ.RH"), Some(1.0));
        assert_eq!(
            tracker.between("NEVER.RH", "NEVER.RH"),
            None,
            "but only if it exists"
        );
    }

    #[test]
    fn a_nonsense_close_is_dropped_rather_than_poisoning_every_pair() {
        let tracker = RollingCorrelations::default();
        tracker.observe("A.RH", day(0), f64::NAN);
        tracker.observe("A.RH", day(1), 0.0);
        tracker.observe("A.RH", day(2), -5.0);
        assert_eq!(tracker.tracked(), 0, "nothing usable was recorded");
    }
}
