//! How much of an accepted proposal to send.

use arvo_data::instrument::Kind;
use arvo_data::Instrument;

use super::decide::{AccountState, Decision, Proposal, Rejection};
use crate::{CostModel, RiskModel};

/// How much of it to buy.
///
/// # Why this sizes off the opening balance and not current equity
///
/// Because the backtest does. `arvo-nautilus` resolves `risk_per_trade` and
/// `max_position_fraction` into currency amounts **once, before the run**, as
/// fractions of `starting_cash`. A live gate that sized off a moving equity
/// figure would compound where the backtest did not, and the two systems would
/// then differ in the one number that decides how much money is at stake —
/// which is exactly the divergence this whole design exists to prevent.
///
/// It also means an account that has lost money keeps proposing the same size.
/// That is the backtest's behaviour and it is deliberate here; the control that
/// is *supposed* to respond to losses is the drawdown halt, not a quiet
/// shrinking of every position.
///
/// The cap is not a refinement: position size is capital-at-risk over stop
/// distance, so a *tight* stop buys a *bigger* position, and without a ceiling a
/// five-minute ATR asks for several times the account. Whether the account can
/// actually pay is settled at the venue, in a backtest by the engine rejecting
/// the order and live by the broker doing the same.
///
/// # What it can pay, and what it costs to
///
/// That last sentence was the design, and it failed silently: the engine
/// refused the order, the strategy was never told, and a rule that had lost
/// money once took almost none of its later signals. So both ceilings are now
/// in *all-in* terms — price plus the experiment's slippage and commission —
/// and when the caller knows the account's cash, the entry is also capped at
/// what that cash can buy. Sizing still comes off the opening balance; only
/// affordability reads the live figure.
///
/// # The headroom the whole-share floor used to provide
///
/// The fill lands at the next bar's price, not the reference, and slippage
/// rounds up to a whole tick. Flooring to whole shares absorbed both almost
/// always — a position of a thousand shares gives up to one share, a tenth of a
/// percent, and that was the margin. It was never stated as a margin; it was a
/// side effect of the lot being one.
///
/// A coin's lot is a hundred-millionth, so flooring gives back nothing and the
/// entry is sized to the last cent the account holds. Any upward tick before the
/// fill then makes it unaffordable, and the venue refuses it. On a five-minute
/// crypto run that was 618 refusals out of 620 signals (arvo-desktop #251): the
/// gate approved every one and the venue took two.
///
/// So the margin is explicit now: [`HEADROOM_BPS`] of the cash, less whatever a
/// lot is worth, because flooring to whole lots already leaves up to one lot
/// unspent. For any instrument whose lot is worth more than that — every share,
/// every option — the shortfall is zero and the arithmetic is exactly what it
/// always was. It is a coin, whose lot is worth a millionth of a cent, that gets
/// a margin it did not have.
/// The most contracts one proposal may sell, however much cash there is. A
/// bound on the collateral search, not a trading limit anyone should meet.
///
/// Counted in whole lots as a `u32`, which is a ceiling in *units* as small as
/// the lot is: ten thousand lots of a satoshi is a ten-thousandth of a coin.
/// Nothing on a spot pair reaches it — this bound lives inside the short-option
/// branch, and `Kind::Crypto` cannot match `Kind::Option(_)` — so the ordinary
/// path below sizes a coin in `f64` all the way through (#244).
///
/// ponytail: if anything other than an option ever needs collateral sizing,
/// this and `sellable` want counting in units rather than in whole lots.
const MAX_CONTRACTS: u32 = 10_000;

/// How much of the spendable cash an entry leaves unspent, in basis points.
///
/// Ten. Enough to absorb the gap between the reference price this sizes against
/// and the next bar's price it fills at, which is what the whole-share floor
/// used to absorb by accident. Deliberately smaller than the slack an equity
/// position of a thousand shares already had, so that an instrument whose lot
/// provides more than this keeps deciding for itself and every result computed
/// before this is unchanged.
const HEADROOM_BPS: f64 = 10.0;

pub(super) fn size(
    model: &RiskModel,
    account: &AccountState<'_>,
    proposal: &Proposal,
    instrument: &Instrument,
    costs: Option<&CostModel>,
) -> Decision {
    let starting_cash = account.starting_cash;
    if proposal.reference_price <= 0.0 {
        return Decision::Reject(Rejection::TooSmall { affordable: 0.0 });
    }

    let by_risk = match (model.risk_per_trade, proposal.stop_distance) {
        (Some(_), None | Some(0.0)) => return Decision::Reject(Rejection::NoStop),
        (Some(fraction), Some(distance)) => Some(starting_cash * fraction / distance),
        (None, _) => None,
    };

    // An option is traded in shares of what it delivers, a contract at a time:
    // units of 100, each priced per share as quoted. Holding the unit at one
    // share keeps every dollar figure downstream — curve, stops, P&L — the
    // product of a price and a quantity, with no multiplier to forget.
    // What the instrument's source said, or what its name says (#186). An
    // asked-for quantity that is not whole lots is the proposer's mistake.
    let lot = instrument.lot;
    let option = matches!(instrument.kind, Kind::Option(_));
    if let Some(asked) = proposal.desired_quantity {
        if !instrument.is_whole_lots(asked) {
            return Decision::Reject(Rejection::NotWholeLot { asked, lot });
        }
    }

    // What one unit costs to buy, all in. An option pays half its spread
    // rather than equity basis points.
    let (per_unit, per_fill) = costs.map_or((proposal.reference_price, 0.0), |costs| {
        let crossed = match costs.option_spread {
            Some(spread) if option => {
                proposal.reference_price + spread.half_spread(proposal.reference_price)
            }
            _ => proposal.reference_price * (1.0 + costs.slippage_bps / 10_000.0),
        };
        (
            crossed * (1.0 + costs.commission_bps / 10_000.0),
            costs.per_fill,
        )
    });
    let ceiling = model.max_position_fraction.unwrap_or(1.0);

    // Cash already promised against the options the account is short (#84). A
    // cash account cannot spend it twice, and the venue's free balance does not
    // know it is promised: selling a put *adds* its premium to that balance.
    let held: Vec<(&str, f64)> = account
        .positions
        .iter()
        .map(|(name, position)| (name.as_str(), position.quantity))
        .collect();
    let reserve = match crate::collateral::reserved(held.iter().copied()) {
        Ok(reserve) => reserve,
        Err(uncovered) => {
            return Decision::Reject(Rejection::Uncovered {
                underlying: uncovered.underlying,
                expiration: uncovered.expiration,
            })
        }
    };
    let spendable = account.spendable.map(|cash| cash - reserve);

    if proposal.opens_short {
        if !option {
            return Decision::Reject(Rejection::CannotShort {
                instrument: proposal.instrument.clone(),
            });
        }
        // Sized by the cash its worst case at expiry needs, not by its premium:
        // selling a $2 put asks for $200 and can cost $64,000.
        let available = spendable
            .unwrap_or(starting_cash)
            .min(starting_cash * ceiling)
            - per_fill;
        let wanted = by_risk
            .or(proposal.desired_quantity)
            .map_or(MAX_CONTRACTS, |units| (units / lot).floor().clamp(0.0, f64::from(MAX_CONTRACTS)) as u32);
        return match crate::collateral::sellable(&held, &proposal.instrument, available, wanted) {
            Err(uncovered) => Decision::Reject(Rejection::Uncovered {
                underlying: uncovered.underlying,
                expiration: uncovered.expiration,
            }),
            Ok(0) => Decision::Reject(Rejection::TooSmall {
                affordable: available.max(0.0),
            }),
            Ok(contracts) => Decision::Accept {
                quantity: f64::from(contracts) * lot,
            },
        };
    }

    let by_cap = ((starting_cash * ceiling - per_fill) / per_unit).max(0.0);
    // The cash ceiling keeps a little back. See the type note: the whole-share
    // floor used to do this by accident and a satoshi lot does not.
    let by_cash = spendable.map(|cash| {
        // Only the shortfall. Flooring to whole lots below already leaves up to
        // one lot unspent, so reserving a lot here as well would take two — and
        // the equity arithmetic has to come out where it always did.
        let reserved = (cash * HEADROOM_BPS / 10_000.0 - lot * per_unit).max(0.0);
        ((cash - reserved - per_fill) / per_unit).max(0.0)
    });
    let by_cap = by_cash.map_or(by_cap, |by_cash| by_cap.min(by_cash));

    // Risk sizing wins where it applies; otherwise what was asked for; and the
    // cap bounds either. A proposer's desired size is a request, never a
    // permission.
    let wanted = by_risk.or(proposal.desired_quantity).unwrap_or(by_cap);
    let quantity = (wanted.min(by_cap) / lot).floor() * lot;
    if quantity < lot {
        return Decision::Reject(Rejection::TooSmall { affordable: by_cap });
    }
    Decision::Accept { quantity }
}
