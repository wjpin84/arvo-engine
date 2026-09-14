//! Fetches the option contracts that expired (or will) in a window, near the
//! money, with their bars, into the library (#15). Read-only market data.
//!
//! ```text
//! cargo run -p arvo-alpaca --example fetch_chain -- \
//!     <underlying> <expiring-from> <expiring-to> <interval> <band> <horizon-days> [library]
//!
//! # 0DTE: five-minute bars on each contract's expiration day, strikes within 3%
//! ... fetch_chain -- SPY 2025-09-08 2025-09-12 5minute 0.03 0
//! # Premium selling: daily bars over the 45 days before expiry, within 10%
//! ... fetch_chain -- SPY 2025-08-01 2025-09-30 day 0.10 45
//! ```
//!
//! The underlying's daily bars are fetched first (Alpaca IEX, into the library)
//! because the band is taken around where it actually traded.

use std::collections::BTreeMap;

use arvo_alpaca::{chain, Alpaca};
use arvo_data::option::near_the_money;
use arvo_data::{BarInterval, BarProvider, CsvBars};
use chrono::NaiveDate;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        return Err("usage: <underlying> <expiring-from> <expiring-to> <interval> <band> <horizon-days> [library]".into());
    }
    let underlying = args[0].as_str();
    let from: NaiveDate = args[1].parse()?;
    let to: NaiveDate = args[2].parse()?;
    let interval: BarInterval = args[3].parse()?;
    let band: f64 = args[4].parse()?;
    let horizon: i64 = args[5].parse()?;
    let root = args.get(6).map_or_else(
        || {
            std::path::PathBuf::from(std::env::var("APPDATA").expect("APPDATA, or pass a library"))
                .join("com.arvo.desktop")
                .join("data")
        },
        std::path::PathBuf::from,
    );
    let library = CsvBars::new(&root);

    let earliest = from - chrono::Duration::days(horizon);
    let spot = arvo_data::source::ingest(
        &root,
        &Alpaca::iex(),
        underlying,
        BarInterval::DAILY,
        earliest,
        to,
    )
    .await?;
    let underlying_bars = library.bars(&spot.instrument, BarInterval::DAILY, earliest, to)?;
    println!("{}: {} daily bars", spot.instrument, underlying_bars.len());

    let listed = chain::contracts(underlying, from, to).await?;
    let wanted = near_the_money(&listed, &underlying_bars, band, horizon);
    println!(
        "{} contracts listed, {} near the money",
        listed.len(),
        wanted.len()
    );

    // One window per expiration, so each batch of bars is one request series.
    let mut by_expiry: BTreeMap<NaiveDate, Vec<_>> = BTreeMap::new();
    for w in wanted {
        by_expiry.entry(w.contract.expiration).or_default().push(w);
    }

    let (mut written, mut bars, mut silent) = (0, 0, 0);
    for (expiration, group) in by_expiry {
        let symbols: Vec<String> = group.iter().map(|w| w.contract.symbol()).collect();
        let fetched = chain::bars(&symbols, interval, group[0].from, group[0].to).await?;
        for symbol in &symbols {
            match fetched.get(symbol) {
                Some(series) => {
                    library.write(&format!("{symbol}.{}", chain::VENUE), interval, series)?;
                    written += 1;
                    bars += series.len();
                }
                None => silent += 1,
            }
        }
        println!(
            "{expiration}: {} contracts, {} traded",
            symbols.len(),
            symbols.iter().filter(|s| fetched.contains_key(*s)).count()
        );
    }
    println!("wrote {written} contracts, {bars} bars; {silent} never traded in their window and have no file");
    Ok(())
}
