//! Implied volatility and greeks (#16).
//!
//! Nothing that is not naked directional can be modelled without them: a put
//! spread is sold by delta, a 0DTE position is sized by gamma, and a strike
//! chosen by "10% below spot" means a different risk at 12% volatility than at
//! 40%. No vendor this platform can reach serves them historically, so they are
//! computed here from prices.
//!
//! # The model, and where it is wrong
//!
//! Black-Scholes-Merton with a continuous dividend yield. Three ways that is
//! not SPY, each stated so nobody mistakes the number for the market's:
//!
//! - **American exercise.** SPY options can be exercised early; this prices
//!   European ones. The gap is negligible for out-of-the-money contracts and
//!   real for deep in-the-money puts, whose implied volatility this overstates.
//! - **Discrete dividends.** SPY pays quarterly; a continuous yield smears each
//!   payment across the year. Imply the yield per expiration
//!   ([`implied_dividend_yield`]) and the payment lands in the right contracts.
//!   Early exercise ahead of an ex-date is still unpriced: in-the-money calls
//!   expiring just after one imply too high a volatility.
//! - **The rate is an input.** A wrong one barely moves a month's option, and
//!   the implied yield absorbs most of it. Checked against recorded SPY quotes:
//!   with the yield implied, calls and puts 32 days out agreed within 0.3 vol
//!   points at every strike within 2% of spot.
//!
//! And one about the inputs: an option bar's close and the underlying's close
//! are the last trades in the same period, not the same instant. On a thin
//! contract the option's last trade can be minutes older than the underlying's,
//! and the implied volatility then prices a spot that no longer existed.
//!
//! # Time
//!
//! Calendar time, to the second, to the regular close on the expiration date
//! ([`OptionContract::expires_at`]), over a 365-day year. A 0DTE contract at
//! noon has half a trading day left, and a model that counted whole days would
//! call it expired.
//!
//! ponytail: calendar time charges a weekend as two days of variance that the
//! market barely moves through. Switch to trading time if a short-dated
//! position's theta over a weekend matters.

use arvo_data::option::{OptionContract, Right};
use chrono::NaiveDateTime;

use crate::psr::normal_cdf;

const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// The volatilities an implied volatility is searched between. 0.01% to 500%:
/// wide enough for a 0DTE contract on a stressed afternoon, narrow enough that
/// a price only reachable beyond it is a bad price rather than a volatility.
const VOL_BOUNDS: (f64, f64) = (0.0001, 5.0);

/// What a contract is priced against, besides itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Market {
    /// The underlying's price.
    pub spot: f64,
    /// Continuously compounded risk-free rate, as a fraction.
    pub rate: f64,
    /// Continuous dividend yield, as a fraction.
    pub dividend_yield: f64,
}

/// A contract's model price and sensitivities, per share.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Greeks {
    pub price: f64,
    /// Change in price per $1 in the underlying.
    pub delta: f64,
    /// Change in delta per $1 in the underlying.
    pub gamma: f64,
    /// Change in price per volatility *point* (0.01).
    pub vega: f64,
    /// Change in price per calendar day, holding everything else. Negative for
    /// a long option: time is what the holder pays.
    pub theta: f64,
}

/// Years from `at` to the contract's close, never negative.
#[must_use]
pub fn years_to_expiry(contract: &OptionContract, at: NaiveDateTime) -> f64 {
    let seconds = (contract.expires_at() - at).num_seconds();
    (seconds.max(0) as f64) / SECONDS_PER_YEAR
}

/// Price and greeks at volatility `vol` with `years` to expiry.
///
/// At or past expiry, or at zero volatility, the price is the discounted
/// forward intrinsic value and delta is the step it takes at the strike —
/// the limit the formula approaches, without dividing by zero to get there.
#[must_use]
pub fn greeks(contract: &OptionContract, market: Market, years: f64, vol: f64) -> Greeks {
    let Market {
        spot,
        rate,
        dividend_yield,
    } = market;
    let strike = contract.strike;
    let carry = (-dividend_yield * years).exp();
    let discount = (-rate * years).exp();
    let sign = match contract.right {
        Right::Call => 1.0,
        Right::Put => -1.0,
    };

    if years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
        let forward_intrinsic = (sign * (spot * carry - strike * discount)).max(0.0);
        let in_the_money = sign * (spot * carry - strike * discount) > 0.0;
        return Greeks {
            price: forward_intrinsic,
            delta: if in_the_money { sign * carry } else { 0.0 },
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
        };
    }

    let root = vol * years.sqrt();
    let d1 = ((spot / strike).ln() + (rate - dividend_yield + vol * vol / 2.0) * years) / root;
    let d2 = d1 - root;
    let density = (-d1 * d1 / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();

    let price =
        sign * (spot * carry * normal_cdf(sign * d1) - strike * discount * normal_cdf(sign * d2));
    let delta = sign * carry * normal_cdf(sign * d1);
    let gamma = carry * density / (spot * root);
    let vega = spot * carry * density * years.sqrt();
    let theta = -spot * carry * density * vol / (2.0 * years.sqrt())
        - sign * rate * strike * discount * normal_cdf(sign * d2)
        + sign * dividend_yield * spot * carry * normal_cdf(sign * d1);

    Greeks {
        price,
        delta,
        gamma,
        vega: vega / 100.0,
        theta: theta / 365.0,
    }
}

/// The volatility at which the model prices the contract at `premium`.
///
/// `None` when no volatility does: a premium below the no-arbitrage floor (a
/// stale trade against a spot that has moved, or a carry assumption that is
/// wrong), or one only reachable past [`VOL_BOUNDS`]. Refusing is the point —
/// clamping those to a bound would put a confident number on a bad price.
#[must_use]
pub fn implied_volatility(
    contract: &OptionContract,
    premium: f64,
    market: Market,
    years: f64,
) -> Option<f64> {
    if !premium.is_finite() || premium <= 0.0 || years <= 0.0 {
        return None;
    }
    let at = |vol| greeks(contract, market, years, vol).price;
    let (mut low, mut high) = VOL_BOUNDS;
    if premium < at(low) || premium > at(high) {
        return None;
    }
    // Bisection: price is monotone in volatility, so this cannot fail to
    // converge the way Newton's method does on a far out-of-the-money contract
    // whose vega is nearly zero.
    for _ in 0..100 {
        let mid = (low + high) / 2.0;
        if at(mid) < premium {
            low = mid;
        } else {
            high = mid;
        }
        if high - low < 1e-7 {
            break;
        }
    }
    Some((low + high) / 2.0)
}

/// The dividend yield one expiration's own prices imply, at a given rate.
///
/// Put-call parity, `call - put = spot·e^(-qT) - strike·e^(-rT)`, solved for
/// `q` at each `(strike, call, put)` and the median taken.
///
/// # Per expiration, because dividends are not continuous
///
/// SPY pays quarterly. A contract expiring after an ex-date carries that whole
/// payment; one expiring before it carries none — and a constant yield prices
/// both wrong, the short-dated one worst. Recorded quotes on 2026-09-14 showed
/// it plainly: the expiration four days out implied a *15%* yield (a quarterly
/// payment inside four days), the one a month out about 2.5%. Implied per
/// expiration, the payment is in the price, and calls and puts agree.
///
/// # Why the rate stays an input
///
/// Parity is a line in the strike and could give the rate as its slope, but
/// over the few percent of strikes near the money a few cents of quote noise
/// moves that slope enough to imply a negative rate — it did, on the same
/// quotes. The rate barely moves a month's option; the yield absorbs what error
/// it has.
///
/// Near-the-money strikes, mids from one moment. The median rather than a mean,
/// so one stale quote cannot drag it. `None` with fewer than three strikes or
/// if no strike gives a positive discounted spot.
#[must_use]
pub fn implied_dividend_yield(
    pairs: &[(f64, f64, f64)],
    spot: f64,
    years: f64,
    rate: f64,
) -> Option<f64> {
    if pairs.len() < 3 || years <= 0.0 || spot <= 0.0 {
        return None;
    }
    let mut yields: Vec<f64> = pairs
        .iter()
        .filter_map(|(strike, call, put)| {
            let carried = (call - put + strike * (-rate * years).exp()) / spot;
            (carried > 0.0).then(|| -carried.ln() / years)
        })
        .collect();
    if yields.len() < 3 {
        return None;
    }
    yields.sort_by(f64::total_cmp);
    let middle = yields.len() / 2;
    Some(if yields.len().is_multiple_of(2) {
        (yields[middle - 1] + yields[middle]) / 2.0
    } else {
        yields[middle]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn contract(right: Right, strike: f64) -> OptionContract {
        OptionContract {
            underlying: "X".to_owned(),
            expiration: NaiveDate::from_ymd_opt(2026, 12, 18).expect("valid"),
            right,
            strike,
        }
    }

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() < tolerance
    }

    #[test]
    fn matches_the_textbook() {
        // Hull, Options, Futures and Other Derivatives, example 15.6:
        // S=42, K=40, r=10%, sigma=20%, T=0.5 -> call 4.76, put 0.81.
        let market = Market {
            spot: 42.0,
            rate: 0.10,
            dividend_yield: 0.0,
        };
        let call = greeks(&contract(Right::Call, 40.0), market, 0.5, 0.2);
        let put = greeks(&contract(Right::Put, 40.0), market, 0.5, 0.2);
        assert!(close(call.price, 4.76, 0.005), "{}", call.price);
        assert!(close(put.price, 0.81, 0.005), "{}", put.price);
        assert!(close(call.delta, 0.779, 0.001), "{}", call.delta);
    }

    #[test]
    fn calls_and_puts_satisfy_parity_with_a_dividend_yield() {
        let market = Market {
            spot: 650.0,
            rate: 0.04,
            dividend_yield: 0.012,
        };
        for (strike, years, vol) in [(600.0, 0.1, 0.18), (650.0, 0.01, 0.12), (700.0, 1.0, 0.3)] {
            let call = greeks(&contract(Right::Call, strike), market, years, vol);
            let put = greeks(&contract(Right::Put, strike), market, years, vol);
            let forward = 650.0 * (-0.012 * years).exp() - strike * (-0.04 * years).exp();
            assert!(
                close(call.price - put.price, forward, 1e-9),
                "{strike} {years}"
            );
            assert!(close(call.delta - put.delta, (-0.012 * years).exp(), 1e-12));
        }
    }

    #[test]
    fn the_greeks_are_the_slopes_of_the_price() {
        let option = contract(Right::Put, 640.0);
        let market = Market {
            spot: 650.0,
            rate: 0.04,
            dividend_yield: 0.012,
        };
        let (years, vol, h) = (30.0 / 365.0, 0.16, 0.01);
        let at = |spot: f64, years: f64, vol: f64| {
            greeks(&option, Market { spot, ..market }, years, vol)
        };
        let g = at(650.0, years, vol);

        let delta = (at(650.0 + h, years, vol).price - at(650.0 - h, years, vol).price) / (2.0 * h);
        let gamma = (at(650.0 + h, years, vol).delta - at(650.0 - h, years, vol).delta) / (2.0 * h);
        let vega = (at(650.0, years, vol + 0.0001).price - at(650.0, years, vol - 0.0001).price)
            / 0.0002
            / 100.0;
        // Per year over a sliver, then per day: a whole-day step would measure
        // the curvature over that day too.
        let e = 1e-6;
        let theta =
            (at(650.0, years - e, vol).price - at(650.0, years + e, vol).price) / (2.0 * e) / 365.0;

        assert!(close(g.delta, delta, 1e-6), "{} vs {delta}", g.delta);
        assert!(close(g.gamma, gamma, 1e-6), "{} vs {gamma}", g.gamma);
        assert!(close(g.vega, vega, 1e-6), "{} vs {vega}", g.vega);
        assert!(close(g.theta, theta, 1e-6), "{} vs {theta}", g.theta);
        assert!(g.theta < 0.0, "a long option pays for time");
    }

    #[test]
    fn implied_volatility_recovers_the_volatility_that_priced_it() {
        let market = Market {
            spot: 650.0,
            rate: 0.04,
            dividend_yield: 0.012,
        };
        // Including two hours to a 0DTE close, a far put and a deep call.
        for (right, strike, years, vol) in [
            (Right::Put, 600.0, 45.0 / 365.0, 0.22),
            (Right::Call, 650.0, 2.0 / (24.0 * 365.0), 0.11),
            (Right::Put, 645.0, 2.0 / (24.0 * 365.0), 0.35),
            (Right::Call, 500.0, 0.5, 0.25),
        ] {
            let option = contract(right, strike);
            let premium = greeks(&option, market, years, vol).price;
            let implied = implied_volatility(&option, premium, market, years)
                .expect("priced by a volatility");
            assert!(
                close(implied, vol, 1e-4),
                "{right:?} {strike}: {implied} vs {vol}"
            );
        }
    }

    #[test]
    fn a_price_no_volatility_reaches_has_no_implied_volatility() {
        let market = Market {
            spot: 650.0,
            rate: 0.04,
            dividend_yield: 0.012,
        };
        let deep = contract(Right::Call, 600.0);
        // Worth at least ~50 by arbitrage; a trade at 40 is stale or wrong.
        assert_eq!(implied_volatility(&deep, 40.0, market, 0.05), None);
        assert_eq!(implied_volatility(&deep, 0.0, market, 0.05), None);
        assert_eq!(
            implied_volatility(&deep, 60.0, market, 0.0),
            None,
            "expired"
        );
        assert_eq!(
            implied_volatility(&deep, 651.0, market, 0.05),
            None,
            "worth more than the stock"
        );
    }

    #[test]
    fn a_chain_implies_the_yield_it_was_priced_with_and_shrugs_off_a_stale_quote() {
        let market = Market {
            spot: 762.0,
            rate: 0.04,
            dividend_yield: 0.019,
        };
        let years = 32.0 / 365.0;
        // A skew, so no strike shares another's volatility.
        let mut pairs: Vec<(f64, f64, f64)> = (740..=785)
            .step_by(5)
            .map(|strike| {
                let strike = f64::from(strike);
                let vol = 0.14 + (762.0 - strike) * 0.002;
                let call = greeks(&contract(Right::Call, strike), market, years, vol).price;
                let put = greeks(&contract(Right::Put, strike), market, years, vol).price;
                (strike, call, put)
            })
            .collect();
        pairs[3].1 += 2.0;
        let implied = implied_dividend_yield(&pairs, 762.0, years, 0.04).expect("implied");
        assert!(close(implied, 0.019, 1e-9), "{implied}");
        assert_eq!(
            implied_dividend_yield(&pairs[..2], 762.0, years, 0.04),
            None
        );
    }

    #[test]
    fn a_dividend_inside_a_short_expiration_shows_as_a_large_yield() {
        // A $1.90 quarterly payment four days out, priced as the forward it
        // makes: the yield that reproduces that forward is ~23%, not ~1%.
        let (spot, years, rate): (f64, f64, f64) = (762.0, 4.0 / 365.0, 0.04);
        let forward_carry = (spot - 1.90) / spot;
        let pairs: Vec<(f64, f64, f64)> = [755.0, 760.0, 765.0]
            .iter()
            .map(|&strike| {
                (
                    strike,
                    spot * forward_carry - strike * (-rate * years).exp() + 1.0,
                    1.0,
                )
            })
            .collect();
        let implied = implied_dividend_yield(&pairs, spot, years, rate).expect("implied");
        assert!(
            close(implied, -forward_carry.ln() / years, 1e-9),
            "{implied}"
        );
        assert!(implied > 0.2);
    }

    #[test]
    fn at_expiry_the_price_is_intrinsic() {
        let market = Market {
            spot: 657.54,
            rate: 0.04,
            dividend_yield: 0.012,
        };
        let call = greeks(&contract(Right::Call, 655.0), market, 0.0, 0.2);
        assert!(close(call.price, 2.54, 1e-9));
        assert!(close(call.delta, 1.0, 1e-12));
        assert!(
            greeks(&contract(Right::Put, 655.0), market, 0.0, 0.2)
                .price
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn time_runs_to_the_close_on_the_expiration_date() {
        let option = OptionContract::parse("SPY260914C00760000").expect("valid");
        // 14:00 UTC is 10:00 EDT: six hours to the 16:00 close.
        let at = NaiveDate::from_ymd_opt(2026, 9, 14)
            .expect("valid")
            .and_hms_opt(14, 0, 0)
            .expect("valid");
        assert!(close(
            years_to_expiry(&option, at),
            6.0 / (24.0 * 365.0),
            1e-12
        ));
        let after = at + chrono::Duration::hours(7);
        assert_eq!(years_to_expiry(&option, after), 0.0);
    }
}
