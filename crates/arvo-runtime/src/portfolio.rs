//! Portfolio commands: what the workbench can ask about what you hold.
//!
//! Read-only throughout. Nothing here places, cancels or modifies an order,
//! and there is no broker credential anywhere in the path — holdings come
//! from a file you export yourself. When a sanctioned read-only connector
//! exists it slots in beside the reader without changing the domain, the
//! valuation or this view.

use std::collections::BTreeMap;
use std::path::PathBuf;

use arvo_data::{BarProvider, CsvBars};
use arvo_portfolio::{csv::CsvHoldings, PriceSource, ValuedPortfolio};
use serde::Serialize;

use crate::commands::CommandError;

/// Where holdings files live: `<app data>/portfolios`, one CSV per portfolio.
pub const PORTFOLIO_SUBDIR: &str = "portfolios";

pub struct PortfolioService {
    holdings: CsvHoldings,
    bars: CsvBars,
    directory: PathBuf,
}

impl PortfolioService {
    #[must_use]
    pub fn new(portfolio_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            holdings: CsvHoldings::new(portfolio_dir.clone()),
            bars: CsvBars::new(data_dir),
            directory: portfolio_dir,
        }
    }
}

#[derive(Serialize)]
pub struct HoldingView {
    pub instrument: String,
    pub quantity: f64,
    pub price: f64,
    pub market_value: f64,
    pub cost_basis: f64,
    pub unrealized: f64,
    pub unrealized_pct: Option<f64>,
    pub weight: f64,
    /// "statement", "last_close" or "face" — so a price nobody verified is
    /// visibly different from one that came off a statement.
    pub priced_by: String,
}

#[derive(Serialize)]
pub struct PortfolioView {
    pub name: String,
    pub as_of: String,
    pub total_value: f64,
    pub total_cost: f64,
    pub unrealized: f64,
    pub unrealized_pct: Option<f64>,
    pub cash: f64,
    pub holdings: Vec<HoldingView>,
    pub unpriced: Vec<String>,
}

#[derive(Serialize)]
pub struct PortfolioLibraryView {
    /// Shown so somebody with no holdings file knows where to put one.
    pub directory: String,
    pub portfolios: Vec<PortfolioView>,
}

/// Every portfolio, valued.
///
/// Prices come from the statement where it carried them and from the data
/// library's last close otherwise. Nothing is fetched from a network: the
/// only prices this platform has are the ones already on disk, and pretending
/// otherwise would put an unverifiable number next to real money.
#[tauri::command]
pub async fn list_portfolios(
    service: tauri::State<'_, PortfolioService>,
) -> Result<PortfolioLibraryView, CommandError> {
    let directory = service.directory.display().to_string();

    // Today, not a date read from the file: a holdings export is a snapshot
    // and the file does not reliably say when it was taken. Claiming a date
    // it did not state would be inventing provenance.
    let as_of = chrono::Utc::now().date_naive();
    let portfolios = service
        .holdings
        .portfolios(as_of)
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    let closes = last_closes(&service.bars);
    Ok(PortfolioLibraryView {
        directory,
        portfolios: portfolios
            .iter()
            .map(|portfolio| view_of(&portfolio.value(&closes)))
            .collect(),
    })
}

/// The most recent close the data library holds for each instrument.
///
/// Best effort by design: an instrument with no bars simply is not in the map,
/// and the valuation reports it as unpriced rather than guessing.
fn last_closes(bars: &CsvBars) -> BTreeMap<String, f64> {
    let mut closes = BTreeMap::new();
    let Ok(instruments) = bars.instruments() else {
        return closes;
    };
    for instrument in instruments {
        let Ok(Some((_, last))) = bars.coverage(&instrument) else {
            continue;
        };
        if let Ok(bars_in_window) = bars.daily_bars(&instrument, last, last) {
            if let Some(bar) = bars_in_window.last() {
                closes.insert(instrument, bar.close);
            }
        }
    }
    closes
}

fn view_of(valued: &ValuedPortfolio) -> PortfolioView {
    PortfolioView {
        name: valued.name.clone(),
        as_of: valued.as_of.to_string(),
        total_value: valued.total_value,
        total_cost: valued.total_cost,
        unrealized: valued.unrealized,
        unrealized_pct: valued.unrealized_pct,
        cash: valued.cash,
        holdings: valued
            .holdings
            .iter()
            .map(|holding| HoldingView {
                instrument: holding.instrument.clone(),
                quantity: holding.quantity,
                price: holding.price,
                market_value: holding.market_value,
                cost_basis: holding.cost_basis,
                unrealized: holding.unrealized,
                unrealized_pct: holding.unrealized_pct,
                weight: holding.weight,
                priced_by: match holding.priced_by {
                    PriceSource::Statement => "statement",
                    PriceSource::LastClose => "last close",
                    PriceSource::Face => "face value",
                }
                .to_owned(),
            })
            .collect(),
        unpriced: valued.unpriced.clone(),
    }
}
