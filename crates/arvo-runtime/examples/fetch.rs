//! Fetches history into the data library, without launching the window.
//!
//!     cargo run -p arvo-runtime --example fetch -- \
//!         <data-dir> [--interval 1day] [--years N] SYMBOL...
//!
//! The same path the Data sidebar uses: the same broker call, the same
//! dropping of the server's synthesised gap-fill bars, the same quality
//! inspection, and the same comparison against whatever copy is already on
//! disk. A fetch here is the fetch the window would have done.
//!
//! Exists because the thing the platform is currently short of is breadth of
//! real data, and filling that from a terminal is a great deal faster than
//! clicking five times per instrument. It reads the broker session already in
//! the keychain and never asks for one — signing in stays in the window,
//! where the browser is.
//!
//! Read-only, and only historicals. Nothing here can place, cancel or modify
//! anything.

use arvo_data::BarInterval;
use arvo_runtime_lib::source;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let root = if args.is_empty() {
        return Err(
            "usage: fetch <data-dir> [--source robinhood] [--interval 1day] [--years N]              SYMBOL..."
                .into(),
        );
    } else {
        args.remove(0)
    };

    let source = source::by_id(
        &take_value(&mut args, "--source").unwrap_or_else(|| "robinhood".to_owned()),
    )?;
    let interval = take_value(&mut args, "--interval")
        .unwrap_or_else(|| "1day".to_owned())
        .parse::<BarInterval>()
        .map_err(|err| format!("--interval: {err}"))?;
    let years: i64 = take_value(&mut args, "--years")
        .unwrap_or_else(|| "5".to_owned())
        .parse()
        .map_err(|_| "--years wants a number")?;

    if args.is_empty() {
        return Err("name at least one symbol".into());
    }

    // Checked before the first call rather than discovered on it: "no session"
    // is a different instruction from "that symbol is unknown", and meeting the
    // first one five symbols in wastes the four that worked. A source needing
    // no credential answers `true` here, which is the honest answer to "can
    // this fetch right now".
    if !source.connected().await? {
        return Err(format!(
            "no {} session stored — sign in from the window first",
            source.id()
        )
        .into());
    }

    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(365 * years);
    println!("{interval} bars, {from} to {to}\n");

    let (mut fetched, mut failed) = (0_usize, 0_usize);
    for symbol in &args {
        // Bare tickers in, `SYMBOL.VENUE` out: the venue is the source's to
        // decide, and typing it here is how two spellings of one instrument end
        // up in the library.
        match source::ingest(root.as_ref(), source.as_ref(), symbol, interval, from, to).await {
            Ok(report) => {
                fetched += 1;
                println!("{}: {} bars", report.instrument, report.bars);
                if report.interpolated > 0 {
                    println!("  {} invented bars dropped", report.interpolated);
                }
                match report.dividends {
                    Some(paid) => println!("  {paid} dividends"),
                    None => println!("  no dividend series from this source"),
                }
                for finding in &report.quality.findings {
                    println!("  [{:?}] {}", finding.severity, finding.detail);
                }
                if let Some(revision) = &report.revision {
                    println!("  against the copy already held: {revision:?}");
                }
            }
            Err(err) => {
                failed += 1;
                // Named and carried on. One unknown symbol is not a reason to
                // abandon the four after it.
                println!("{symbol}: {err}");
            }
        }
    }

    println!("\n{fetched} fetched, {failed} failed");
    Ok(())
}

/// Takes `--flag value` out of the arguments, if it is there.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == flag)?;
    let value = args.get(at + 1).cloned();
    args.drain(at..=(at + 1).min(args.len() - 1));
    value
}
