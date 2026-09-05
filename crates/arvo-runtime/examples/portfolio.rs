//! Values your holdings from the command line, without launching the window.
//!
//!     cargo run -p arvo-runtime --example portfolio -- <portfolio-dir> [data-dir] [snapshot-dir]
//!
//! With a snapshot directory it also *records* the valuation, exactly as the
//! app does — which makes this a way to take a snapshot without opening the
//! window, and the only way to check that path outside the GUI.
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
    let snapshot_dir = args.next();

    // Last closes are best effort: an instrument with no bars simply is not
    // in the map, and the valuation reports it as unpriced rather than
    // guessing at a number to sit next to real money.
    let mut closes = BTreeMap::new();
    if let Some(data_dir) = &data_dir {
        let bars = CsvBars::new(data_dir);
        for instrument in bars.instruments()? {
            if let Some((_, last)) = bars.coverage(&instrument, arvo_data::BarInterval::DAILY)? {
                if let Some(bar) = bars.daily_bars(&instrument, last, last)?.last() {
                    closes.insert(instrument, bar.close);
                }
            }
        }
    }

    let today = chrono::Utc::now().date_naive();
    for imported in CsvHoldings::new(&portfolio_dir).portfolios(today)? {
        let valued = imported.portfolio.value(&closes);
        println!("\n=== {} ===", valued.name);
        println!(
            "  value {:.2}   cost {}   unrealised {}   cash {:.2}",
            valued.total_value,
            valued
                .total_cost
                .map_or_else(|| "not reported".to_owned(), |cost| format!("{cost:.2}")),
            valued
                .unrealized
                .map_or_else(|| "n/a".to_owned(), |gain| format!("{gain:+.2}")),
            valued.cash
        );
        // How the file was read, printed every time: a mis-mapped column is
        // the failure this importer is most likely to have, and it is
        // invisible in the numbers themselves.
        println!("  read as:");
        for (role, column) in &imported.report.columns {
            println!("    {role:<22} <- {column}");
        }
        if imported.report.cost_basis_derived {
            println!("    (cost basis multiplied up from a per-share column)");
        }
        if !imported.report.ignored.is_empty() {
            println!("    ignored: {}", imported.report.ignored.join(", "));
        }
        for skipped in &imported.report.rows_skipped {
            println!("    skipped {skipped}");
        }
        for holding in &valued.holdings {
            println!(
                "    {:<40} qty {:>10}  px {:>9}  value {:>11.2}  {:>5.1}%  [{}]",
                holding.instrument,
                holding
                    .quantity
                    .map_or_else(|| "-".to_owned(), |q| format!("{q:.2}")),
                holding
                    .price
                    .map_or_else(|| "-".to_owned(), |p| format!("{p:.2}")),
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

        if let Some(snapshot_dir) = &snapshot_dir {
            let store = arvo_portfolio::history::SnapshotStore::new(snapshot_dir);
            store.record(&valued, today)?;
            let history = store.history(&valued.name)?;
            println!("  history: {} snapshot(s)", history.snapshots.len());
            for snapshot in &history.snapshots {
                println!(
                    "    {}  {:.2}",
                    snapshot.taken_on, snapshot.portfolio.total_value
                );
            }
            match arvo_portfolio::history::latest_change(&history.snapshots) {
                Some(change) => println!(
                    "    change {:+.2} ({} -> {})",
                    change.absolute, change.from, change.to
                ),
                None => println!("    change: needs a second day to compare"),
            }
        }
    }

    Ok(())
}
