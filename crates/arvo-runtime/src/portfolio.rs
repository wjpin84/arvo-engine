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
use arvo_portfolio::{
    csv::{CsvHoldings, ImportReport},
    history::{latest_change, SnapshotStore},
    PriceSource, ValuedPortfolio,
};
// The view shapes live in `arvo-views` so the window cannot drift from
// them. See that crate for what two hand-mirrored copies cost.
pub use arvo_views::{ChangeView, HoldingView, ImportView, PortfolioLibraryView, PortfolioView, ValuePoint};

use crate::commands::CommandError;

/// Where holdings files live: `<app data>/portfolios`, one CSV per portfolio.
pub const PORTFOLIO_SUBDIR: &str = "portfolios";

/// Where daily valuations are kept.
pub const SNAPSHOT_SUBDIR: &str = "snapshots";

pub struct PortfolioService {
    holdings: CsvHoldings,
    bars: CsvBars,
    directory: PathBuf,
    snapshots: SnapshotStore,
}

impl PortfolioService {
    #[must_use]
    pub fn new(portfolio_dir: PathBuf, data_dir: PathBuf, snapshot_dir: PathBuf) -> Self {
        Self {
            holdings: CsvHoldings::new(portfolio_dir.clone()),
            bars: CsvBars::new(data_dir),
            directory: portfolio_dir,
            snapshots: SnapshotStore::new(snapshot_dir),
        }
    }
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
    let mut views = Vec::with_capacity(portfolios.len());

    for imported in &portfolios {
        let valued = imported.portfolio.value(&closes);

        // Record before reading, so today's look is part of today's line.
        // A failed write must not fail the view: the valuation on screen is
        // real whether or not it could be filed.
        if let Err(err) = service.snapshots.record(&valued, as_of) {
            tracing::error!(error = %err, portfolio = %valued.name, "could not record a snapshot");
        }

        let history = service.snapshots.history(&valued.name).unwrap_or_default();
        for problem in &history.problems {
            tracing::warn!(problem, "could not read a stored snapshot");
        }

        views.push(view_of(&valued, &imported.report, &history.snapshots));
    }

    Ok(PortfolioLibraryView {
        directory,
        portfolios: views,
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
        let Ok(Some((_, last))) = bars.coverage(&instrument, arvo_data::BarInterval::DAILY) else {
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

fn view_of(
    valued: &ValuedPortfolio,
    report: &ImportReport,
    snapshots: &[arvo_portfolio::history::Snapshot],
) -> PortfolioView {
    PortfolioView {
        name: valued.name.clone(),
        as_of: valued.as_of.to_string(),
        total_value: valued.total_value,
        total_cost: valued.total_cost,
        unrealized: valued.unrealized,
        unrealized_pct: valued.unrealized_pct,
        without_cost_basis: valued.without_cost_basis,
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
        value_history: snapshots
            .iter()
            .map(|snapshot| ValuePoint {
                time: snapshot
                    .taken_on
                    .and_time(chrono::NaiveTime::MIN)
                    .and_utc()
                    .timestamp(),
                value: snapshot.portfolio.total_value,
            })
            .collect(),
        change: latest_change(snapshots).map(|change| ChangeView {
            from: change.from.to_string(),
            to: change.to.to_string(),
            absolute: change.absolute,
            percent: change.percent,
        }),
        import: ImportView {
            columns: report.columns.clone(),
            ignored: report.ignored.clone(),
            rows_imported: report.rows_imported,
            rows_skipped: report.rows_skipped.clone(),
            cost_basis_derived: report.cost_basis_derived,
        },
    }
}
