//! Values your holdings from the command line, without launching the window.
//!
//!     cargo run -p arvo-runtime --example portfolio -- <portfolio-dir> [data-dir]
//!
//! Runs the same reader and the same valuation the Portfolio view uses, for
//! the same reason the `study` example exists: the GUI and the domain fail in
//! completely different ways, and only one of them can be checked here.

use std::collections::BTreeMap;

use arvo_data::{BarProvider, CsvBars};
use arvo_portfolio::csv::CsvHoldings;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let portfolio_dir = args
        .next()
        .ok_or("usage: portfolio <portfolio-dir> [data-dir]")?;
    let data_dir = args.next();

    // Last closes are best effort: an instrument with no bars simply is not
    // in the map, and the valuation reports it as unpriced rather than
    // guessing at a number to sit next to real money.
    let mut closes = BTreeMap::new();
    if let Some(data_dir) = &data_dir {
        let bars = CsvBars::new(data_dir);
        for instrument in bars.instruments()? {
            if let Some((_, last)) = bars.coverage(&instrument)? {
                if let Some(bar) = bars.daily_bars(&instrument, last, last)?.last() {
                    closes.insert(instrument, bar.close);
                }
            }
        }
    }

    let today = chrono::Utc::now().date_naive();
    for portfolio in CsvHoldings::new(&portfolio_dir).portfolios(today)? {
        let valued = portfolio.value(&closes);
        println!("\n=== {} ===", valued.name);
        println!(
            "  value {:.2}   cost {:.2}   unrealised {:+.2}   cash {:.2}",
            valued.total_value, valued.total_cost, valued.unrealized, valued.cash
        );
        for holding in &valued.holdings {
            println!(
                "    {:<14} qty {:>10.2}  px {:>9.2}  value {:>11.2}  {:>5.1}%  [{}]",
                holding.instrument,
                holding.quantity,
                holding.price,
                holding.market_value,
                holding.weight * 100.0,
                match holding.priced_by {
                    arvo_portfolio::PriceSource::Statement => "statement",
                    arvo_portfolio::PriceSource::LastClose => "last close",
                    arvo_portfolio::PriceSource::Face => "face",
                }
            );
        }
        if !valued.unpriced.is_empty() {
            println!(
                "    excluded, no price available: {}",
                valued.unpriced.join(", ")
            );
        }
    }

    Ok(())
}
