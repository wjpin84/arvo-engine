//! Reading the run's positions back out as Arvo trades.
//!
//! Nautilus keeps every position in its cache. This lifts them across the
//! containment boundary as [`arvo_research::Trade`] values, so the rest of
//! Arvo can count win rates, holding periods and fees without ever naming a
//! Nautilus type.
//!
//! Nothing here computes anything. Every number is Nautilus's own — its
//! realised P&L, its commissions, its timestamps — because a second
//! calculation of the same quantity is a second thing that can be wrong, and
//! the one that disagrees with the equity curve would be this one.

use arvo_research::{Direction, ExitReason, Trade};
use nautilus_common::cache::Cache;
use nautilus_core::UnixNanos;
use nautilus_model::{
    enums::PositionSide,
    orders::Order,
    position::Position,
};

/// Every position the run opened, oldest first.
///
/// # Where the closed ones live
///
/// Under a netting OMS there is only ever *one* live position per instrument.
/// When a cycle closes, Nautilus snapshots it and resets the live record to
/// open the next one under the same id. So `positions()` returns the current
/// cycle and nothing else, and a run that traded twelve times reports one.
///
/// Found by reconciling the ledger against the equity curve, which is the
/// only reason it was found at all: the count looked plausible, every trade
/// in it was real, and the eleven missing ones left no trace anywhere in the
/// output.
///
/// Sorted rather than taken in cache order: the ledger is read as a sequence
/// by anything that looks at streaks or holding periods, and neither cache
/// iteration order nor snapshot order is part of Nautilus's contract.
pub fn from_cache(cache: &Cache) -> Vec<Trade> {
    let mut positions = cache.position_snapshots(None, None);
    positions.extend(
        cache
            .positions(None, None, None, None, None)
            .into_iter()
            .map(|position| position.cloned()),
    );

    let mut trades: Vec<(UnixNanos, Trade)> = positions
        .iter()
        // A cycle that closed leaves the live record reset and empty behind
        // it. Skipping anything that never held size is what stops that husk
        // being counted as one more trade at zero.
        .filter(|position| position.peak_qty.as_f64() > 0.0)
        .map(|position| (position.ts_opened, to_trade(cache, position)))
        .collect();
    trades.sort_by_key(|(opened, _)| *opened);
    trades.into_iter().map(|(_, trade)| trade).collect()
}

fn to_trade(cache: &Cache, position: &Position) -> Trade {
    // Peak rather than current: a closed position's current quantity is zero,
    // and the size that mattered is the size that was actually held.
    let quantity = position.peak_qty.as_f64();

    Trade {
        // Nautilus's own id for the position's instrument. Once a run can hold
        // several, a ledger that does not say which one a trade was in cannot
        // be marked to market at all.
        instrument: position.instrument_id.to_string(),
        opened: instant(position.ts_opened),
        closed: position.ts_closed.map(instant),
        direction: match position.side {
            PositionSide::Short => Direction::Short,
            // Flat is only reachable for a position that never held anything,
            // which cannot produce a fill. Long is the honest reading of a
            // record that exists at all.
            PositionSide::Long | PositionSide::Flat => Direction::Long,
        },
        quantity,
        entry: position.avg_px_open,
        exit: position.avg_px_close,
        // Nautilus already subtracts commission from realised P&L, so this is
        // what actually landed in the account. Recomputing it from prices and
        // quantity would produce a second number that disagrees.
        pnl: position.realized_pnl.map_or(0.0, |money| money.as_f64()),
        commission: position
            .commissions
            .get(&position.settlement_currency)
            .map_or(0.0, nautilus_model::types::Money::as_f64),
        exit_reason: exit_reason(cache, position),
    }
}

/// How the position ended, read from the tag the strategy stamped on the
/// order that closed it.
///
/// The tag, not the order type. Arvo's stops are enforced by the strategy —
/// it watches the bar low and sends a plain market order when the level is
/// breached — so the venue never sees a `StopMarket` and every exit looks
/// identical from the outside. Classifying by order type would have reported
/// "0 stop exits" on every run ever, which reads as a fact about the strategy
/// rather than a hole in the instrumentation.
fn exit_reason(cache: &Cache, position: &Position) -> ExitReason {
    let Some(closing) = position.closing_order_id else {
        return ExitReason::StillOpen;
    };
    let tagged = |tag: &str| {
        cache
            .order(&closing)
            .and_then(|order| order.tags().map(|tags| tags.iter().any(|t| t == tag)))
            .unwrap_or(false)
    };
    // Signal is the fallback, including for an order no longer in the cache:
    // the position did close, and calling that a stop on no evidence would
    // inflate the count this field exists to make honest.
    if tagged(crate::strategy::EXIT_HALT) {
        ExitReason::Halted
    } else if tagged(crate::strategy::EXIT_STOP) {
        ExitReason::Stop
    } else {
        ExitReason::Signal
    }
}

/// A Nautilus timestamp as a UTC instant.
///
/// Saturating rather than fallible: every timestamp here was produced by the
/// engine from data Arvo supplied, so an unrepresentable one is not reachable
/// from user input, and an `Option` on every field would be noise.
fn instant(at: UnixNanos) -> chrono::NaiveDateTime {
    i64::try_from(at.as_u64()).map_or(chrono::NaiveDateTime::MAX, |nanos| {
        chrono::DateTime::from_timestamp_nanos(nanos).naive_utc()
    })
}
