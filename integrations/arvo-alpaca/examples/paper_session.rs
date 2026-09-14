//! One paper round trip through the gate, against Alpaca's paper account.
//!
//!     cargo run -p arvo-alpaca --example paper_session -- SYMBOL [--hold | --clear]
//!
//! Exercises the parts of trading that are not arithmetic, on simulated money,
//! in market hours — which is the only time an order fills rather than queues:
//!
//! 1. **Reconcile** against what the paper account already holds. Anything
//!    found is adopted, resting orders are cancelled, and the session halts.
//! 2. **Propose** one share through the risk gate, priced at Alpaca's latest
//!    IEX one-minute close.
//! 3. **Settle** until it resolves: filled, or cancelled as stale.
//! 4. **Kill**: arm the halt, flatten, settle the exit, confirm the venue is
//!    clear and the halt lifts.
//!
//! `--hold` stops after step 3 and leaves the share held. Run again to watch a
//! restart adopt the position and halt; add `--clear` to flatten what the
//! restart found and release. Without `--clear`, anything already in the
//! account is reported and left alone — it may be someone else's test.
//!
//! **Paper only, by construction.** This names `AlpacaExecutor::paper()` and
//! nothing else; there is no flag that reaches the live endpoint. Uses the keys
//! the workbench stored in the OS keychain.

use std::collections::BTreeMap;
use std::time::Duration;

use arvo_alpaca::{Alpaca, AlpacaExecutor};
use arvo_data::source::Source;
use arvo_execution::{Executor, Session};
use arvo_research::risk::{Proposal, RiskGate};
use arvo_research::RiskModel;

/// The library suffix positions are filed under: the free feed's venue.
const VENUE: &str = arvo_alpaca::IEX_VENUE;
const STARTING_CASH: f64 = 100_000.0;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let hold = args.iter().any(|arg| arg == "--hold");
    let clear = args.iter().any(|arg| arg == "--clear");
    args.retain(|arg| arg != "--hold" && arg != "--clear");
    let [symbol] = args.as_slice() else {
        return Err("usage: paper_session SYMBOL [--hold | --clear]".into());
    };
    let symbol = symbol.to_uppercase();
    let instrument = format!("{symbol}.{VENUE}");

    let executor = AlpacaExecutor::paper();
    assert!(
        executor.is_paper(),
        "this example must never reach the live endpoint"
    );
    let now = || chrono::Utc::now().naive_utc();
    let mut session = Session::new(
        RiskGate::new(RiskModel::default(), STARTING_CASH, now().date()),
        executor,
    );
    println!(
        "venue: {} (paper), buying power {:?}",
        session.executor().venue(),
        session.executor().buying_power().await?,
    );

    // 1. Reconcile.
    let found = session.reconcile(now(), VENUE).await?;
    println!(
        "reconcile: adopted {:?}, cancelled {}, stranded {}",
        found
            .adopted
            .iter()
            .map(|h| (&h.symbol, h.quantity))
            .collect::<Vec<_>>(),
        found.cancelled.len(),
        found.stranded.len(),
    );
    if let Some(why) = session.gate().halted() {
        println!("  halted, as designed: {why}");
        if !clear {
            println!("  left alone; run with --clear to flatten what was found and release");
        } else {
            println!("  flattening what was found, then releasing");
            let flatten = session
                .kill("paper test: clearing a restart", &BTreeMap::new(), now())
                .await;
            report_flatten(&flatten);
            settle(&mut session).await?;
            show_venue(&session).await?;
            println!("  release lifted the halt: {}", session.rearm());
        }
        return Ok(());
    }

    // 2. Propose one share at the latest IEX price.
    let price = latest_price(&symbol).await?;
    println!("reference price {symbol}: {price:.2} (latest IEX 1-minute close)");
    let proposal = Proposal {
        instrument: instrument.clone(),
        proposer: "paper-session-example".to_owned(),
        signalled_at: now(),
        reference_price: price,
        stop_distance: None,
        desired_quantity: Some(1.0),
    };
    match session.propose(&proposal, now(), None).await? {
        Some(order) => println!("submitted: {order}"),
        None => {
            println!("refused by the gate: {:?}", session.refusals());
            return Ok(());
        }
    }

    // 3. Settle.
    settle(&mut session).await?;
    show_venue(&session).await?;
    if hold {
        println!("--hold: leaving the position open. Run again to test a restart, then with --clear to flatten it.");
        return Ok(());
    }

    // 4. Kill: halt first, then flatten.
    let prices = BTreeMap::from([(instrument, latest_price(&symbol).await?)]);
    let flatten = session
        .kill("paper test: kill switch", &prices, now())
        .await;
    report_flatten(&flatten);
    let refused = session.propose(&proposal, now(), None).await?;
    println!(
        "a proposal while halted was {}",
        if refused.is_none() {
            "refused"
        } else {
            "ACCEPTED — this is a bug"
        }
    );
    settle(&mut session).await?;
    show_venue(&session).await?;
    println!("release lifted the halt: {}", session.rearm());

    let divergence = session.divergence();
    println!(
        "divergence: {} fills, {} unfilled, mean slippage {:.2} bps (worst {:.2}), mean latency {:.0} ms (worst {})",
        divergence.fills,
        divergence.unfilled,
        divergence.mean_slippage_bps,
        divergence.worst_slippage_bps,
        divergence.mean_latency_ms,
        divergence.worst_latency_ms,
    );
    println!(
        "  (Alpaca paper fills measure Alpaca's simulator, not the market — see the executor docs)"
    );
    Ok(())
}

/// Alpaca's latest one-minute close on the free IEX feed.
async fn latest_price(symbol: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let today = chrono::Utc::now().date_naive();
    let fetched = Alpaca::iex()
        .bars(
            symbol,
            arvo_data::BarInterval::new(1, arvo_data::IntervalUnit::Minute),
            today,
            today + chrono::Duration::days(1),
        )
        .await?;
    let last = fetched
        .bars
        .last()
        .ok_or("no one-minute bars today — is the market open?")?;
    println!("  (last bar opened {} UTC)", last.at);
    Ok(last.close)
}

/// Drains until nothing is outstanding, for up to three minutes — long enough
/// for the executor's two-minute stale-order cancel to fire.
async fn settle<E: Executor>(session: &mut Session<E>) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..90 {
        let settled = session.settle().await?;
        for execution in session.executions().iter().rev().take(settled) {
            println!(
                "  filled {:?} {} x{} at {:.4}: slippage {:.2} bps, latency {} ms",
                execution.side,
                execution.instrument,
                execution.quantity,
                execution.fill_price,
                execution.slippage_bps(),
                execution.latency_ms(),
            );
        }
        if session.divergence().unfilled == 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!("  still outstanding after three minutes");
    Ok(())
}

async fn show_venue<E: Executor>(session: &Session<E>) -> Result<(), Box<dyn std::error::Error>> {
    let state = session.executor().at_venue().await?;
    println!(
        "venue now holds {:?} with {} resting order(s)",
        state
            .positions
            .iter()
            .map(|h| (&h.symbol, h.quantity))
            .collect::<Vec<_>>(),
        state.resting.len(),
    );
    Ok(())
}

fn report_flatten(flatten: &arvo_execution::Flatten) {
    println!(
        "kill: {} exit(s) submitted, {} failed{}",
        flatten.submitted.len(),
        flatten.failed.len(),
        if flatten.complete() {
            ""
        } else {
            " — STILL HELD"
        },
    );
}
