//! Implied volatility across strikes from recorded option quotes (#16): the
//! check that the pricing model and its carry describe the market.
//!
//! ```text
//! cargo run -p arvo-alpaca --example quote_smile -- <quotes.csv> <recorded_at> <expiration> [rate]
//! ```
//!
//! The dividend yield is implied from the expiration's own prices. Calls and
//! puts at one strike are then the same bet on volatility, so the two columns
//! should agree; where they part, a quote is stale or early exercise is priced
//! in (in-the-money puts above spot).

use std::collections::BTreeMap;

use arvo_data::option::{OptionContract, Right};
use arvo_research::greeks::{implied_dividend_yield, implied_volatility, years_to_expiry, Market};

struct Quote {
    contract: OptionContract,
    mid: f64,
    years: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: <quotes.csv> <recorded_at> <expiration> [rate]".into());
    }
    let (recorded_at, expiration) = (args[1].as_str(), args[2].as_str());
    let rate: f64 = args.get(3).map_or(Ok(0.04), |a| a.parse())?;

    let mut quotes = Vec::new();
    let mut spot = 0.0;
    for line in std::fs::read_to_string(&args[0])?.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 12 || f[0] != recorded_at || f[2] != expiration {
            continue;
        }
        let Some(contract) = OptionContract::parse(f[1]) else {
            continue;
        };
        let (bid, ask): (f64, f64) = (f[6].parse()?, f[7].parse()?);
        if bid <= 0.0 || ask < bid {
            continue;
        }
        spot = (f[10].parse::<f64>()? + f[11].parse::<f64>()?) / 2.0;
        // The quote's own time, not the snapshot's: that is when this price
        // was the price.
        let at = chrono::DateTime::parse_from_rfc3339(f[5])?.naive_utc();
        let years = years_to_expiry(&contract, at);
        quotes.push(Quote {
            contract,
            mid: (bid + ask) / 2.0,
            years,
        });
    }

    let by_strike = |quotes: &[Quote]| {
        let mut pairs: BTreeMap<i64, (Option<f64>, Option<f64>)> = BTreeMap::new();
        for q in quotes {
            let entry = pairs
                .entry((q.contract.strike * 100.0).round() as i64)
                .or_default();
            match q.contract.right {
                Right::Call => entry.0 = Some(q.mid),
                Right::Put => entry.1 = Some(q.mid),
            }
        }
        pairs
    };
    let years = quotes.first().map_or(0.0, |q| q.years);
    let near_money: Vec<(f64, f64, f64)> = by_strike(&quotes)
        .iter()
        .filter_map(|(k, (c, p))| Some((*k as f64 / 100.0, (*c)?, (*p)?)))
        .filter(|(k, _, _)| (k / spot - 1.0).abs() <= 0.02)
        .collect();
    let Some(dividend_yield) = implied_dividend_yield(&near_money, spot, years, rate) else {
        return Err("too few strikes with both a call and a put near the money".into());
    };
    println!(
        "spot {spot:.2}, rate {:.2}% (input), yield {:.2}% (implied from {} strikes), expiring {expiration}",
        rate * 100.0,
        dividend_yield * 100.0,
        near_money.len()
    );

    let market = Market {
        spot,
        rate,
        dividend_yield,
    };
    let mut smile: BTreeMap<i64, (Option<f64>, Option<f64>)> = BTreeMap::new();
    for q in &quotes {
        let iv = implied_volatility(&q.contract, q.mid, market, q.years);
        let entry = smile
            .entry((q.contract.strike * 100.0).round() as i64)
            .or_default();
        match q.contract.right {
            Right::Call => entry.0 = iv,
            Right::Put => entry.1 = iv,
        }
    }

    println!(
        "{:>8} {:>8} {:>8} {:>7}",
        "strike", "call iv", "put iv", "gap"
    );
    let show = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{:.1}%", v * 100.0));
    for (strike, (call, put)) in &smile {
        let strike = *strike as f64 / 100.0;
        if (strike / spot - 1.0).abs() > 0.05 {
            continue;
        }
        let gap = match (call, put) {
            (Some(c), Some(p)) => format!("{:+.1}", (c - p) * 100.0),
            _ => String::new(),
        };
        println!("{strike:>8} {:>8} {:>8} {gap:>7}", show(*call), show(*put));
    }
    Ok(())
}
