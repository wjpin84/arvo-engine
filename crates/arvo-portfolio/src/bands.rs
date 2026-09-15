//! How much of a portfolio one holding may be, and what to do when it is more.
//!
//! Two rules, both mechanical:
//!
//! * **A band** you declare for a holding: a target weight and a ceiling.
//!   Above the ceiling, the warning says how much to trim to get back to the
//!   target. A rebalancing rule decided once is only worth having if it is
//!   applied without re-deciding it every time the price moves.
//! * **A default ceiling** for every holding you have not declared a band for.
//!   Declaring a band is how a deliberate concentration — a core index fund at
//!   forty percent — stops being flagged.
//!
//! Weights are of the whole portfolio, cash included, which is how a broker
//! shows them.
//!
//! # The file
//!
//! `<portfolio>.bands` beside the holdings file, so `robinhood-1234.bands` for
//! `robinhood-1234.csv`. Not `.csv`, because every `.csv` in that folder is
//! read as holdings.
//!
//! ```text
//! instrument,target,max
//! XRP-USD,0.10,0.15
//! VTI,0.42,
//! ```
//!
//! Matched by ticker, so `XRP-USD` covers `XRP-USD.RH`. A blank `max` declares
//! the holding intended and exempts it from the default ceiling.

use std::collections::BTreeMap;

use crate::ValuedPortfolio;

/// Above this share of the portfolio, an undeclared holding is flagged.
///
/// A quarter: past it, one name's bad year is the portfolio's bad year.
pub const DEFAULT_MAX_WEIGHT: f64 = 0.25;

/// One declared band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub target: f64,
    pub max: Option<f64>,
}

/// Reads a bands file into bands by ticker, or says which line is wrong.
///
/// A line that does not parse fails the file rather than being skipped: a
/// rebalancing rule silently missing is a rule believed to be in force.
///
/// # Errors
///
/// A message naming the line, when a weight is not a fraction between 0 and 1
/// or `max` is below `target`.
pub fn parse(text: &str) -> Result<BTreeMap<String, Band>, String> {
    let mut bands = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if index == 0 || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let fraction = |field: Option<&&str>| -> Result<Option<f64>, String> {
            match field.filter(|text| !text.is_empty()) {
                None => Ok(None),
                Some(text) => match text.parse::<f64>() {
                    Ok(value) if (0.0..=1.0).contains(&value) => Ok(Some(value)),
                    _ => Err(format!(
                        "line {}: {text:?} is not a weight between 0 and 1",
                        index + 1
                    )),
                },
            }
        };
        let ticker = fields
            .first()
            .filter(|ticker| !ticker.is_empty())
            .ok_or_else(|| format!("line {}: no instrument", index + 1))?;
        let target = fraction(fields.get(1))?
            .ok_or_else(|| format!("line {}: {ticker} has no target", index + 1))?;
        let max = fraction(fields.get(2))?;
        if max.is_some_and(|max| max < target) {
            return Err(format!("line {}: {ticker}'s max is below its target", index + 1));
        }
        bands.insert(ticker_of(ticker).to_owned(), Band { target, max });
    }
    Ok(bands)
}

/// Every holding over its ceiling, largest breach first, as a sentence.
#[must_use]
pub fn warnings(valued: &ValuedPortfolio, bands: &BTreeMap<String, Band>) -> Vec<String> {
    let mut over: Vec<(f64, String)> = valued
        .holdings
        .iter()
        .filter(|holding| !holding.instrument.eq_ignore_ascii_case(crate::CASH))
        .filter_map(|holding| {
            let ticker = ticker_of(&holding.instrument);
            let percent = |weight: f64| format!("{:.1}%", weight * 100.0);
            match bands.get(ticker) {
                Some(Band { target, max: Some(max) }) if holding.weight > *max => {
                    let trim = (holding.weight - target) * valued.total_value;
                    Some((
                        holding.weight - max,
                        format!(
                            "{ticker} is {} of the portfolio, above its {} band. Trim about \
                             ${trim:.2} to return it to {}.",
                            percent(holding.weight),
                            percent(*max),
                            percent(*target),
                        ),
                    ))
                }
                Some(_) => None,
                None if holding.weight > DEFAULT_MAX_WEIGHT => Some((
                    holding.weight - DEFAULT_MAX_WEIGHT,
                    format!(
                        "{ticker} is {} of the portfolio, more than the {} any one undeclared \
                         holding is allowed. Trim it, or declare a band for it if the \
                         concentration is intended.",
                        percent(holding.weight),
                        percent(DEFAULT_MAX_WEIGHT),
                    ),
                )),
                None => None,
            }
        })
        .collect();
    over.sort_by(|a, b| b.0.total_cmp(&a.0));
    over.into_iter().map(|(_, warning)| warning).collect()
}

/// `XRP-USD.RH` → `XRP-USD`. A venue says where a price came from.
fn ticker_of(instrument: &str) -> &str {
    instrument
        .rsplit_once('.')
        .map_or(instrument, |(ticker, _)| ticker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Holding, Portfolio};

    fn valued(rows: &[(&str, f64)]) -> ValuedPortfolio {
        Portfolio {
            name: "p".to_owned(),
            as_of: chrono::NaiveDate::from_ymd_opt(2026, 9, 15).expect("valid"),
            holdings: rows
                .iter()
                // Cash is valued at its quantity, anything else at one unit.
                .map(|(instrument, value)| Holding {
                    instrument: (*instrument).to_owned(),
                    quantity: Some(if *instrument == crate::CASH { *value } else { 1.0 }),
                    cost_basis: Some(*value),
                    price: Some(*value),
                    value: None,
                })
                .collect(),
        }
        .value(&BTreeMap::new())
    }

    #[test]
    fn a_holding_over_its_band_is_told_how_much_to_trim_back_to_target() {
        // $200 of $1,000 is 20%, over a 15% band; back to 10% is $100.
        let portfolio = valued(&[("XRP-USD.RH", 200.0), ("A.RH", 200.0), ("B.RH", 200.0), ("C.RH", 200.0), ("CASH", 200.0)]);
        let bands = parse("instrument,target,max\nXRP-USD,0.10,0.15\n").expect("valid");
        assert_eq!(
            warnings(&portfolio, &bands),
            ["XRP-USD is 20.0% of the portfolio, above its 15.0% band. Trim about $100.00 to return it to 10.0%."]
        );
    }

    #[test]
    fn an_undeclared_concentration_is_flagged_and_declaring_it_silences_it() {
        let portfolio = valued(&[("VTI.RH", 420.0), ("A.RH", 200.0), ("B.RH", 200.0), ("C.RH", 180.0)]);
        let flagged = warnings(&portfolio, &BTreeMap::new());
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert!(flagged[0].starts_with("VTI is 42.0%"), "{flagged:?}");

        let declared = parse("instrument,target,max\nVTI,0.42,\n").expect("valid");
        assert!(warnings(&portfolio, &declared).is_empty());
    }

    #[test]
    fn cash_is_never_a_concentration() {
        assert!(warnings(&valued(&[("CASH", 900.0), ("A.RH", 100.0)]), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_bad_line_fails_the_file_rather_than_dropping_a_rule() {
        assert!(parse("instrument,target,max\nXRP-USD,10,15\n").unwrap_err().contains("line 2"));
        assert!(parse("instrument,target,max\nXRP-USD,0.2,0.1\n").unwrap_err().contains("below"));
        assert!(parse("instrument,target,max\nXRP-USD,,0.15\n").unwrap_err().contains("no target"));
    }
}
