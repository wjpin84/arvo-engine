//! Portfolio commands: what the workbench can ask about what you hold.
//!
//! Read-only throughout. Nothing here places, cancels or modifies an order.
//! Holdings come from files: one you export yourself, or one
//! [`sync_robinhood`] writes from Robinhood's read tools. Either way the file
//! is what gets valued, so the domain, the valuation and this view do not
//! know which it was.

use std::collections::{BTreeMap, BTreeSet};
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

use crate::source;
use crate::CommandError;

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

    /// Every *priceable* instrument held, across every portfolio.
    ///
    /// Identifiers only — no quantities, no values. A caller wants to know
    /// what you hold so it can look those up; what they are worth is this
    /// module's answer and comes from disk, never from the network.
    ///
    /// # Cash is not a ticker
    ///
    /// `CASH` is excluded, and that is a bug fix rather than tidiness. The
    /// domain is explicit that cash is worth its face value and never needs a
    /// price looked up — but `CASH` is also a real listed symbol, so asking
    /// an equity feed about it returns a genuine quote for a company nobody
    /// here holds. The watchlist showed a cash line at $83.11, up 0.41%,
    /// which is precisely the unverifiable number beside real money that this
    /// module exists to refuse.
    ///
    /// An unreadable holdings file yields nothing rather than an error: this
    /// feeds a convenience, and refusing to show prices because one CSV is
    /// malformed would be the wrong trade.
    #[must_use]
    pub fn held(&self) -> BTreeSet<String> {
        self.holdings
            .portfolios(chrono::Utc::now().date_naive())
            .unwrap_or_default()
            .iter()
            .flat_map(|imported| imported.portfolio.holdings.iter())
            .map(|holding| holding.instrument.clone())
            .filter(|instrument| !is_cash(instrument))
            .collect()
    }
}

/// Whether an instrument id names cash rather than something with a price.
///
/// Case-insensitively, matching how the importer and the valuation both test
/// it — a holdings file written `cash` is the same line as one written
/// `CASH`, and a third spelling of this check is a third chance to disagree.
fn is_cash(instrument: &str) -> bool {
    instrument.eq_ignore_ascii_case(arvo_portfolio::CASH)
}

/// Every portfolio, valued.
///
/// Prices come from the statement where it carried them and from the data
/// library's last close otherwise. Nothing is fetched from a network: the
/// only prices this platform has are the ones already on disk, and pretending
/// otherwise would put an unverifiable number next to real money.
///
/// # Errors
///
/// The portfolio folder cannot be read.
pub fn list(service: &PortfolioService) -> Result<PortfolioLibraryView, CommandError> {
    library(service)
}

/// Writes every Robinhood account's holdings to `robinhood-<last four>.csv`
/// beside the other holdings files, then lists them all (#27).
///
/// A file rather than a live view, for the reason fetched bars are files
/// (ADR-0008): what was valued stays on disk to be looked at, and the snapshot
/// history treats a synced portfolio exactly like an exported one.
///
/// Prices are Robinhood's own at the moment of the sync, filed as statement
/// prices. A position it did not quote is written with no price and shows as
/// unpriced rather than at some other source's close.
///
/// # Errors
///
/// [`CommandError`] if Robinhood is not signed in or cannot be read, or a file
/// cannot be written. Nothing is written unless every account was read.
pub async fn sync_accounts(service: &PortfolioService) -> Result<PortfolioLibraryView, CommandError> {
    // Every vendor that can report holdings; each skips itself when it has
    // no credential, so this never fails for a vendor you have not connected.
    for vendor in ["robinhood", "alpaca"] {
        sync_vendor(vendor, &service.directory).await?;
    }
    library(service)
}

/// Refreshes one synced portfolio from its vendor. A file the person
/// exported themselves has no vendor to ask, and says so.
///
/// # Errors
///
/// No portfolio of that name, not a synced one, or the vendor unreadable.
pub async fn sync_one(service: &PortfolioService, name: &str) -> Result<PortfolioLibraryView, CommandError> {
    let as_of = chrono::Utc::now().date_naive();
    let imported = service
        .holdings
        .portfolios(as_of)
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    let found = imported
        .iter()
        .find(|imported| imported.portfolio.name == name)
        .ok_or_else(|| CommandError::Failed(format!("no portfolio called {name:?}")))?;
    let vendor = source_of(&found.path).ok_or_else(|| {
        CommandError::Failed(format!(
            "{name} is a file you exported yourself; replace {} to refresh it",
            found.path.display()
        ))
    })?;
    if !sync_vendor(&vendor, &service.directory).await? {
        return Err(CommandError::Failed(format!(
            "{vendor} is not connected; add its account under Accounts, then refresh"
        )));
    }
    library(service)
}

/// Reads every account a vendor holds and rewrites their files, whole or not
/// at all. `false` when the vendor has no credential to read with, so a
/// "sync everything" pass skips it rather than failing.
async fn sync_vendor(vendor: &str, directory: &std::path::Path) -> Result<bool, CommandError> {
    std::fs::create_dir_all(directory)
        .map_err(|err| CommandError::Failed(format!("creating the portfolio folder: {err}")))?;
    match vendor {
        "robinhood" => {
            if !source::robinhood::is_connected().unwrap_or(false) {
                return Ok(false);
            }
            let accounts = arvo_robinhood::Robinhood
                .holdings()
                .await
                .map_err(|err| CommandError::Failed(format!("could not read Robinhood holdings: {err}")))?;
            for account in &accounts {
                write_synced(directory, account)
                    .map_err(|err| CommandError::Failed(format!("writing a synced portfolio: {err}")))?;
            }
            Ok(true)
        }
        "alpaca" => {
            let mut any = false;
            for env in [arvo_alpaca::Env::Paper, arvo_alpaca::Env::Live] {
                if !arvo_alpaca::has(env).unwrap_or(false) {
                    continue;
                }
                let held = arvo_alpaca::holdings(env)
                    .await
                    .map_err(|err| CommandError::Failed(format!("could not read Alpaca {} holdings: {err}", env.id())))?;
                write_alpaca(directory, env, &held)
                    .map_err(|err| CommandError::Failed(format!("writing a synced portfolio: {err}")))?;
                any = true;
            }
            Ok(any)
        }
        other => Err(CommandError::Failed(format!("{other} cannot be synced from here yet"))),
    }
}

/// The vendor a synced file came from, from its name: a sync writes
/// `<vendor>-<account>.csv`, and nothing else in the folder starts with a
/// vendor's id. `None` for a file the person exported.
#[must_use]
pub fn source_of(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    // The vendor, not the source: `alpaca-paper-1234` came from Alpaca, and a
    // refresh asks Alpaca. Every source's vendor, longest first so `alpaca`
    // is preferred where a name could be read two ways.
    let mut vendors: Vec<&'static str> = source::all().iter().map(|source| source.vendor()).collect();
    vendors.sort_unstable();
    vendors.dedup();
    vendors
        .into_iter()
        .filter(|vendor| stem.len() > vendor.len() && stem.starts_with(vendor) && stem[vendor.len()..].starts_with('-'))
        .max_by_key(|vendor| vendor.len())
        .map(ToOwned::to_owned)
}

/// Whether a holdings file is a paper account: `alpaca-paper-1234.csv`, as
/// [`write_alpaca`] files it, or anything a person names with a `paper`
/// segment themselves.
#[must_use]
pub fn is_paper(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.split('-').any(|part| part.eq_ignore_ascii_case("paper")))
}

/// When a file was last written, for the sidebar's note; `None` when the
/// file system cannot say.
fn refreshed_at(path: &std::path::Path) -> Option<String> {
    let millis = crate::project::modified(path);
    if millis == 0 {
        return None;
    }
    let when = chrono::DateTime::from_timestamp_millis(i64::try_from(millis).ok()?)?;
    Some(when.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
}

/// The holdings file for one account, written whole or not at all.
///
/// Through a temporary file and a rename, because a half-written file is an
/// unreadable file, and one unreadable file fails the whole portfolio list.
fn write_synced(directory: &std::path::Path, account: &arvo_robinhood::HeldAccount) -> std::io::Result<()> {
    let last_four = &account.account_number[account.account_number.len().saturating_sub(4)..];
    let path = directory.join(format!("robinhood-{last_four}.csv"));
    let partial = path.with_extension("csv.partial");
    std::fs::write(&partial, holdings_csv(account))?;
    std::fs::rename(&partial, &path)
}

/// One Alpaca account as a holdings file, filed under `alpaca-<env>-<last
/// four>.csv` so paper and live are separate portfolios and a refresh knows
/// which vendor wrote it.
fn write_alpaca(
    directory: &std::path::Path,
    env: arvo_alpaca::Env,
    held: &arvo_alpaca::AccountHoldings,
) -> std::io::Result<()> {
    let last_four = &held.account_number[held.account_number.len().saturating_sub(4)..];
    let path = directory.join(format!("alpaca-{}-{last_four}.csv", env.id()));
    let partial = path.with_extension("csv.partial");
    std::fs::write(&partial, alpaca_csv(env, held))?;
    std::fs::rename(&partial, &path)
}

/// An Alpaca account as the holdings CSV the importer reads. The same shape
/// as [`holdings_csv`], filed under the environment's venue.
fn alpaca_csv(env: arvo_alpaca::Env, held: &arvo_alpaca::AccountHoldings) -> String {
    let optional = |value: Option<f64>| value.map_or_else(String::new, |value| value.to_string());
    let mut out = String::from("instrument,quantity,cost_basis,price
");
    for position in &held.positions {
        out.push_str(&format!(
            "{}.{},{},{},{}
",
            position.symbol,
            env.venue(),
            position.quantity,
            optional(position.cost_basis),
            optional(position.price),
        ));
    }
    out.push_str(&format!("{},{},{},
", arvo_portfolio::CASH, held.cash, held.cash));
    out
}

/// One account as a holdings file the importer reads without guessing.
///
/// Cash carries itself as its cost, so an account with cash still has a total
/// cost basis; without it, one cash row would withhold the whole total.
fn holdings_csv(account: &arvo_robinhood::HeldAccount) -> String {
    let optional = |value: Option<f64>| value.map_or_else(String::new, |value| value.to_string());
    let mut out = String::from("instrument,quantity,cost_basis,price\n");
    for held in &account.holdings {
        out.push_str(&format!(
            "{}.{},{},{},{}\n",
            held.symbol,
            arvo_robinhood::VENUE,
            held.quantity,
            optional(held.cost_basis),
            optional(held.price),
        ));
    }
    out.push_str(&format!(
        "{},{},{},\n",
        arvo_portfolio::CASH,
        account.cash,
        account.cash
    ));
    out
}

fn library(service: &PortfolioService) -> Result<PortfolioLibraryView, CommandError> {
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

        let warnings = band_warnings(&service.directory, &valued);
        let mut view = view_of(&valued, &imported.report, &history.snapshots, warnings);
        view.source = source_of(&imported.path);
        view.paper = is_paper(&imported.path);
        view.refreshed_at = refreshed_at(&imported.path);
        views.push(view);
    }

    Ok(PortfolioLibraryView {
        directory,
        portfolios: views,
    })
}

/// Concentration and rebalancing warnings, against `<name>.bands` when there
/// is one and the default ceiling otherwise.
///
/// A bands file that cannot be read becomes the first warning rather than an
/// error: the rest of the portfolio is still worth showing, and silently
/// falling back to no bands would hide that a rule is not being applied.
fn band_warnings(directory: &std::path::Path, valued: &ValuedPortfolio) -> Vec<String> {
    let file = format!("{}.bands", valued.name);
    let bands = match std::fs::read_to_string(directory.join(&file)) {
        Ok(text) => arvo_portfolio::bands::parse(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(err.to_string()),
    };
    match bands {
        Ok(bands) => arvo_portfolio::bands::warnings(valued, &bands),
        Err(reason) => {
            let mut out = vec![format!("{file} was not applied: {reason}")];
            out.extend(arvo_portfolio::bands::warnings(valued, &BTreeMap::new()));
            out
        }
    }
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
    warnings: Vec<String>,
) -> PortfolioView {
    PortfolioView {
        warnings,
        name: valued.name.clone(),
        as_of: valued.as_of.to_string(),
        total_value: valued.total_value,
        total_cost: valued.total_cost,
        unrealized: valued.unrealized,
        unrealized_pct: valued.unrealized_pct,
        without_cost_basis: arvo_views::count(valued.without_cost_basis),
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
        import: Some(ImportView {
            columns: report
                .columns
                .iter()
                .map(|(name, value)| arvo_views::NamedText { name: name.clone(), value: value.clone() })
                .collect(),
            ignored: report.ignored.clone(),
            rows_imported: arvo_views::count(report.rows_imported),
            rows_skipped: report.rows_skipped.clone(),
            cost_basis_derived: report.cost_basis_derived,
        }),
        source: None,
        paper: false,
        refreshed_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{alpaca_csv, holdings_csv, is_cash, is_paper, source_of};

    #[test]
    fn an_alpaca_file_names_its_vendor_and_its_venue() {
        assert_eq!(source_of(std::path::Path::new("alpaca-paper-1234.csv")).as_deref(), Some("alpaca"));
        assert_eq!(source_of(std::path::Path::new("alpaca-live-9876.csv")).as_deref(), Some("alpaca"));
        let held = arvo_alpaca::AccountHoldings {
            account_number: "PA3ABCDE1234".to_owned(),
            positions: vec![arvo_alpaca::Position { symbol: "AAPL".to_owned(), quantity: 10.0, cost_basis: Some(1500.0), price: Some(190.5) }],
            cash: 500.0,
        };
        let csv = alpaca_csv(arvo_alpaca::Env::Paper, &held);
        assert!(csv.contains("AAPL.ALPACA-PAPER,10,1500,190.5"), "{csv}");
        assert!(csv.contains("CASH,500,500,"), "{csv}");
    }

    #[test]
    fn a_synced_file_names_its_vendor_and_an_export_does_not() {
        assert_eq!(source_of(std::path::Path::new("robinhood-8591.csv")).as_deref(), Some("robinhood"));
        assert_eq!(source_of(std::path::Path::new("my-401k.csv")), None);
        assert_eq!(source_of(std::path::Path::new("robinhood.csv")), None, "no account, not a sync");
        assert!(is_paper(std::path::Path::new("alpaca-paper-1234.csv")));
        assert!(!is_paper(std::path::Path::new("alpaca-live-9876.csv")));
        assert!(!is_paper(std::path::Path::new("robinhood-8591.csv")));
        assert!(!is_paper(std::path::Path::new("paperwork.csv")), "a segment, not a substring");
    }

    #[test]
    fn a_broken_bands_file_is_named_and_the_default_ceiling_still_applies() {
        let dir = tempfile::tempdir().expect("temp dir");
        let valued = arvo_portfolio::Portfolio {
            name: "robinhood-1234".to_owned(),
            as_of: chrono::NaiveDate::from_ymd_opt(2026, 9, 15).expect("valid"),
            holdings: vec![arvo_portfolio::Holding {
                instrument: "VTI.RH".to_owned(),
                quantity: Some(1.0),
                cost_basis: None,
                price: Some(100.0),
                value: None,
            }],
        }
        .value(&std::collections::BTreeMap::new());

        assert!(super::band_warnings(dir.path(), &valued)[0].starts_with("VTI is 100.0%"));

        std::fs::write(dir.path().join("robinhood-1234.bands"), "instrument,target,max\nVTI,42,\n")
            .expect("written");
        let warnings = super::band_warnings(dir.path(), &valued);
        assert!(warnings[0].starts_with("robinhood-1234.bands was not applied"), "{warnings:?}");
        assert!(warnings[1].starts_with("VTI is 100.0%"), "{warnings:?}");
    }

    #[test]
    fn a_synced_account_reads_back_through_the_importer_at_the_brokers_prices() {
        // Through the real importer and valuation, because the file is the
        // contract: a column it misreads would produce a plausible, wrong
        // portfolio.
        let account = arvo_robinhood::HeldAccount {
            account_number: "000001234".to_owned(),
            holdings: vec![
                arvo_robinhood::Held {
                    symbol: "VTI".to_owned(),
                    quantity: 1.5,
                    cost_basis: Some(500.0),
                    price: Some(400.0),
                },
                arvo_robinhood::Held {
                    symbol: "XRP-USD".to_owned(),
                    quantity: 100.0,
                    cost_basis: Some(120.0),
                    price: Some(1.4),
                },
                arvo_robinhood::Held {
                    symbol: "ODD".to_owned(),
                    quantity: 2.0,
                    cost_basis: Some(10.0),
                    price: None,
                },
            ],
            cash: 10.1,
        };
        let dir = tempfile::tempdir().expect("temp dir");
        super::write_synced(dir.path(), &account).expect("written");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 15).expect("valid");
        let imported = arvo_portfolio::csv::CsvHoldings::new(dir.path())
            .portfolios(today)
            .expect("reads");
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].portfolio.name, "robinhood-1234");
        assert!(imported[0].report.rows_skipped.is_empty(), "{:?}", imported[0].report);

        let valued = imported[0].portfolio.value(&std::collections::BTreeMap::new());
        assert!((valued.total_value - (600.0 + 140.0 + 10.1)).abs() < 1e-9, "{valued:?}");
        assert!((valued.cash - 10.1).abs() < 1e-9);
        assert_eq!(valued.unpriced, ["ODD.RH"], "no quote, no invented price");
        assert!(
            (valued.total_cost.expect("every priced row has a cost") - (500.0 + 120.0 + 10.1)).abs()
                < 1e-9
        );
        assert!(holdings_csv(&account).contains("XRP-USD.RH,100,120,1.4"));
    }

    /// The watchlist showed a cash line priced at $83.11, up 0.41% — `CASH`
    /// is a real listed symbol, so an equity feed answers for it happily.
    /// Cash is worth its face value and is never looked up.
    #[test]
    fn cash_is_never_something_to_price() {
        assert!(is_cash("CASH"));
        assert!(is_cash("cash"), "the importer matches case-insensitively; so does this");
        assert!(!is_cash("MSFT"));
        // Not a prefix match: a real ticker that starts with the letters is
        // a real holding, and dropping it would hide a position.
        assert!(!is_cash("CASHX"));
    }
}
