//! How much of an intraday result was earned while the market was closed.
//!
//! A rule run on five-minute bars reads as an intraday rule, and its verdict is
//! read as a statement about intraday timing. But nothing stops it holding a
//! position overnight, and the return between one session's close and the
//! next one's open is a different exposure — news, earnings, the documented
//! overnight drift — that no intraday signal chose. A rule whose every dollar
//! came from the gaps it happened to hold through has shown nothing about its
//! entries, and the equity curve reports the two identically.
//!
//! So the curve is split at session boundaries. Like the dividend gap, this
//! sits beside the result rather than being folded into it: it changes how a
//! verdict is read, not whether it passed.
//!
//! # The approximation
//!
//! The overnight step is the equity change from the last point of one session
//! to the first point of the next. Equity is marked at bar closes, so that step
//! also carries the first bar of the new session — five minutes on a
//! five-minute run. Separating them needs the opening print, which the curve
//! does not hold; the error is small next to the gap it sits beside.

use serde::{Deserialize, Serialize};

use crate::EquityPoint;

/// A curve's return, split at session boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OvernightSplit {
    /// Compounded return across every session boundary.
    pub overnight: f64,
    /// Compounded return within sessions. With [`Self::overnight`], compounds
    /// to the curve's total.
    pub session: f64,
    /// Session boundaries across which equity moved — nights a position was
    /// held through.
    pub nights_held: usize,
}

impl OvernightSplit {
    /// Whether the result was made between sessions and lost, or not made,
    /// within them.
    #[must_use]
    pub fn earned_only_overnight(&self) -> bool {
        let total = (1.0 + self.overnight) * (1.0 + self.session) - 1.0;
        total > 0.0 && self.session <= 0.0
    }
}

/// Splits an intraday curve at its session boundaries.
///
/// A boundary is a change of UTC date, the same rule
/// `arvo_nautilus`'s session indicator uses, which holds for US regular hours.
/// `None` for a curve too short to have a step, or one with no boundary in it.
#[must_use]
pub fn split(curve: &[EquityPoint]) -> Option<OvernightSplit> {
    let (mut overnight, mut session, mut nights_held, mut boundaries) = (1.0, 1.0, 0, 0);
    for pair in curve.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from.equity <= 0.0 {
            continue;
        }
        let ratio = to.equity / from.equity;
        if from.at.date() == to.at.date() {
            session *= ratio;
        } else {
            boundaries += 1;
            overnight *= ratio;
            if (ratio - 1.0).abs() > f64::EPSILON {
                nights_held += 1;
            }
        }
    }
    (boundaries > 0).then_some(OvernightSplit {
        overnight: overnight - 1.0,
        session: session - 1.0,
        nights_held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(day: u32, hour: u32, equity: f64) -> EquityPoint {
        EquityPoint {
            at: chrono::NaiveDate::from_ymd_opt(2024, 7, day)
                .expect("valid")
                .and_hms_opt(hour, 0, 0)
                .expect("valid"),
            equity,
        }
    }

    #[test]
    fn the_two_parts_compound_to_the_whole() {
        let curve = [
            point(1, 14, 100.0),
            point(1, 19, 98.0),
            point(2, 14, 106.0),
            point(2, 19, 104.0),
        ];
        let split = split(&curve).expect("one night");
        let total = (1.0 + split.overnight) * (1.0 + split.session) - 1.0;
        assert!((total - 0.04).abs() < 1e-12, "{total}");
        assert!(split.session < 0.0 && split.overnight > 0.0);
        assert_eq!(split.nights_held, 1);
    }

    #[test]
    fn a_profit_made_only_in_the_gaps_is_named() {
        // Lost two in each session, gained eight overnight.
        let curve = [
            point(1, 14, 100.0),
            point(1, 19, 98.0),
            point(2, 14, 106.0),
            point(2, 19, 104.0),
        ];
        assert!(split(&curve).expect("one night").earned_only_overnight());
    }

    #[test]
    fn a_rule_flat_every_night_holds_nothing_overnight() {
        let curve = [
            point(1, 14, 100.0),
            point(1, 19, 103.0),
            point(2, 14, 103.0),
            point(2, 19, 105.0),
        ];
        let split = split(&curve).expect("one boundary");
        assert_eq!(split.nights_held, 0);
        assert!(split.overnight.abs() < 1e-12);
        assert!(!split.earned_only_overnight());
    }

    #[test]
    fn a_single_session_has_nothing_to_split() {
        assert_eq!(split(&[point(1, 14, 100.0), point(1, 19, 101.0)]), None);
        assert_eq!(split(&[]), None);
    }
}
