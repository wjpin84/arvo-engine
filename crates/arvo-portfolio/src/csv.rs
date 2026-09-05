//! Reading a holdings export, whoever produced it.
//!
//! Brokers all export the same handful of facts under different column names:
//! Fidelity writes `Current Value`, someone else writes `Market Value`, a
//! 401(k) platform writes `Ending Market Value`. Rather than one hardcoded
//! schema per broker — each of which has to be guessed at, and silently
//! breaks when the broker changes a header — this maps columns **by name**
//! against a list of known aliases and then **reports what it did**.
//!
//! That report is the important part. An importer that guesses wrong produces
//! a portfolio that looks entirely plausible and is wrong, and no number in it
//! shows the mistake. This one states which column it used for each role, so a
//! bad mapping is visible immediately rather than after a decision is made on
//! it.
//!
//! # What it needs
//!
//! An instrument (a symbol, or failing that a description), a quantity, and
//! either a price or a value. Everything else is optional:
//!
//! * **Cost basis** may be total or per-share. Total wins; per-share is
//!   multiplied by quantity and the report says so. Absent entirely is fine
//!   and normal for a 401(k) — see [`crate::Holding::cost_basis`].
//! * **Account** splits one file into several portfolios, which is how
//!   brokers that export every account at once are handled.
//!
//! # What it refuses
//!
//! A file whose header carries no recognisable instrument or quantity column.
//! Refusing is deliberate: the alternative is importing something whose shape
//! was misunderstood.
//!
//! Rows that do not parse are skipped and listed, rather than failing the
//! file — broker exports routinely end with disclaimer paragraphs, and losing
//! a whole portfolio to a legal footer would be absurd. A row that looks like
//! data and fails is still reported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::{Holding, Portfolio};

#[derive(Debug, thiserror::Error)]
pub enum CsvError {
    #[error("reading {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: no header row with a recognisable instrument and quantity column")]
    Unrecognised { path: PathBuf },
}

/// How a file was read, so a wrong mapping is visible rather than silent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Role → the column heading actually used for it.
    pub columns: Vec<(String, String)>,
    /// Headings that were present and not used for anything.
    pub ignored: Vec<String>,
    pub rows_imported: usize,
    /// Rows that looked like data and could not be read, with the reason.
    /// Disclaimer text at the foot of a file lands here and is harmless.
    pub rows_skipped: Vec<String>,
    /// True when cost basis came from a per-share column multiplied by
    /// quantity rather than from a reported total.
    pub cost_basis_derived: bool,
}

/// A portfolio and the story of how it was read.
#[derive(Debug, Clone)]
pub struct Imported {
    pub portfolio: Portfolio,
    pub report: ImportReport,
}

/// Column aliases, lowercased. Order matters: earlier is preferred.
mod roles {
    pub const INSTRUMENT: &[&str] = &["instrument", "symbol", "ticker", "security id"];
    pub const DESCRIPTION: &[&str] = &[
        "description",
        "security description",
        "investment",
        "fund name",
        "name",
        "security name",
    ];
    pub const QUANTITY: &[&str] = &[
        "quantity",
        "shares",
        "qty",
        "share quantity",
        "number of shares",
        "units",
    ];
    pub const PRICE: &[&str] = &[
        "price",
        "last price",
        "current price",
        "share price",
        "closing price",
        "nav",
        "unit price",
    ];
    pub const VALUE: &[&str] = &[
        "current value",
        "market value",
        "value",
        "ending market value",
        "total value",
        "balance",
    ];
    pub const COST_TOTAL: &[&str] = &[
        "cost basis total",
        "total cost basis",
        "cost basis",
        "total cost",
    ];
    pub const COST_PER_SHARE: &[&str] = &[
        "average cost basis",
        "avg cost basis",
        "average cost",
        "cost per share",
    ];
    pub const ACCOUNT: &[&str] = &[
        "account number",
        "account name",
        "account",
        "plan",
        "plan name",
    ];
}

/// A directory of holdings files.
#[derive(Debug, Clone)]
pub struct CsvHoldings {
    root: PathBuf,
}

impl CsvHoldings {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every portfolio in the directory.
    ///
    /// `as_of` is supplied rather than read from the file: an export is a
    /// snapshot and rarely says when it was taken. The caller knows better.
    ///
    /// A missing directory is an empty library, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`CsvError`] if the directory cannot be listed, or if a file
    /// cannot be read or has no recognisable header.
    pub fn portfolios(&self, as_of: NaiveDate) -> Result<Vec<Imported>, CsvError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CsvError::Io {
                    path: self.root.clone(),
                    source,
                })
            }
        };

        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
            })
            .collect();
        paths.sort();

        let mut imported = Vec::new();
        for path in &paths {
            imported.extend(read_file(path, as_of)?);
        }
        Ok(imported)
    }
}

/// Splits a CSV line, respecting quotes.
///
/// Not optional, and not premature. Fund names carry commas —
/// `"VANGUARD TARGET RETIREMENT 2050 FUND, INVESTOR SHARES"` is one field,
/// and splitting it naively shifts every later column left by one. The
/// quantity column then holds text, every row is skipped, and the portfolio
/// disappears with no error at all. Verified before this was written: a bare
/// `split(',')` lost the whole file in silence.
///
/// Handles the doubled-quote escape (`""`) because that is how a quote inside
/// a quoted field is written.
/// Rows that are summaries, not holdings.
///
/// Matched exactly rather than by prefix: plenty of real funds are called
/// "Total Stock Market Index", and skipping those would quietly delete a
/// position. "Account Total" is never a fund.
const SUMMARY_ROWS: &[&str] = &[
    "total",
    "totals",
    "account total",
    "grand total",
    "subtotal",
    "sub total",
    "portfolio total",
    "plan total",
];

/// Whether a file is comma- or tab-separated.
///
/// Copying a holdings table out of a web page gives tabs, and that is a
/// perfectly reasonable way to get data out of a platform whose only export
/// is a PDF. Guessing wrong finds no columns and refuses the file.
fn delimiter_of(text: &str) -> char {
    let head: Vec<&str> = text.lines().take(5).collect();
    let count = |sep: char| -> usize { head.iter().map(|line| line.matches(sep).count()).sum() };
    if count('\t') > count(',') {
        '\t'
    } else {
        ','
    }
}

fn split_on(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => {
                fields.push(current.trim().to_owned());
                current = String::new();
            }
            _ => current.push(c),
        }
    }
    fields.push(current.trim().to_owned());
    fields
}

#[cfg(test)]
fn split(line: &str) -> Vec<String> {
    split_on(line, ',')
}

/// Normalises a heading for matching: lowercase, and underscores treated as
/// spaces so `cost_basis` and `Cost Basis` are the same column.
fn normalise(column: &str) -> String {
    column
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Finds a column whose heading matches one of `aliases`, ignoring any
/// column already claimed by another role.
///
/// `exclude` is not housekeeping, it is correctness. A fuzzy search for
/// "cost basis" happily matches "Average Cost Basis", and using a per-share
/// figure as a total understates cost by the share count while every number
/// downstream still looks entirely reasonable. Claiming the more specific
/// role first and excluding it here is what stops that.
fn find_excluding(header: &[String], aliases: &[&str], exclude: &[usize]) -> Option<usize> {
    let normalised: Vec<String> = header.iter().map(|column| normalise(column)).collect();
    let free = |index: &usize| !exclude.contains(index);

    // Exact first, across all aliases, so a precise heading is never lost to
    // a loose match on a different column.
    for alias in aliases {
        if let Some(index) = normalised
            .iter()
            .position(|column| column == alias)
            .filter(free)
        {
            return Some(index);
        }
    }
    for alias in aliases {
        if let Some(index) = normalised
            .iter()
            .enumerate()
            .filter(|(index, _)| free(index))
            .find(|(_, column)| column.contains(alias))
            .map(|(index, _)| index)
        {
            return Some(index);
        }
    }
    None
}

fn find(header: &[String], aliases: &[&str]) -> Option<usize> {
    find_excluding(header, aliases, &[])
}

struct Mapping {
    instrument: Option<usize>,
    description: Option<usize>,
    quantity: Option<usize>,
    price: Option<usize>,
    value: Option<usize>,
    cost_total: Option<usize>,
    cost_per_share: Option<usize>,
    account: Option<usize>,
}

fn map_header(header: &[String]) -> Option<Mapping> {
    let instrument = find(header, roles::INSTRUMENT);
    let description = find(header, roles::DESCRIPTION);
    let quantity = find(header, roles::QUANTITY);
    let price = find(header, roles::PRICE);
    let value = find(header, roles::VALUE);

    // Something to name the holding by.
    if instrument.is_none() && description.is_none() {
        return None;
    }
    // And something to size it in money. A reported value is enough on its
    // own: a collective investment trust in a 401(k) reports a balance and no
    // unit count at all, and demanding units would refuse the whole account.
    if value.is_none() && !(quantity.is_some() && price.is_some()) {
        return None;
    }

    // Per-share is resolved first and then excluded from the total search:
    // it is the more specific heading, and letting the looser pattern claim
    // it is the silent-wrongness case described on `find_excluding`.
    let cost_per_share = find(header, roles::COST_PER_SHARE);
    let claimed: Vec<usize> = cost_per_share.into_iter().collect();

    Some(Mapping {
        instrument,
        description,
        quantity,
        price,
        value,
        cost_total: find_excluding(header, roles::COST_TOTAL, &claimed),
        cost_per_share,
        account: find(header, roles::ACCOUNT),
    })
}

/// Parses a money-ish field: `$1,234.56`, `(12.00)` for negative, `--` and
/// `n/a` for absent. Brokers write all of these.
fn number(raw: &str) -> Option<f64> {
    let cleaned = raw.trim();
    if cleaned.is_empty()
        || cleaned.eq_ignore_ascii_case("n/a")
        || cleaned.eq_ignore_ascii_case("na")
        || cleaned == "--"
        || cleaned == "-"
    {
        return None;
    }

    let negative = cleaned.starts_with('(') && cleaned.ends_with(')');
    let stripped: String = cleaned
        .trim_matches(|c| c == '(' || c == ')')
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();

    let value: f64 = stripped.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some(if negative { -value.abs() } else { value })
}

fn read_file(path: &Path, as_of: NaiveDate) -> Result<Vec<Imported>, CsvError> {
    let text = std::fs::read_to_string(path).map_err(|source| CsvError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    // The header is not always the first line: exports often open with a
    // title or a blank. Take the first line that maps.
    let delimiter = delimiter_of(&text);
    let lines: Vec<&str> = text.lines().collect();
    let (header_at, header, mapping) = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let columns = split_on(line, delimiter);
            map_header(&columns).map(|mapping| (index, columns, mapping))
        })
        .ok_or_else(|| CsvError::Unrecognised {
            path: path.to_path_buf(),
        })?;

    let mut report = ImportReport::default();
    let mut used = Vec::new();
    let mut note = |role: &str, index: Option<usize>, report: &mut ImportReport| {
        if let Some(index) = index {
            if let Some(name) = header.get(index) {
                report.columns.push((role.to_owned(), name.clone()));
                used.push(index);
            }
        }
    };
    note("instrument", mapping.instrument, &mut report);
    note("description", mapping.description, &mut report);
    note("quantity", mapping.quantity, &mut report);
    note("price", mapping.price, &mut report);
    note("value", mapping.value, &mut report);
    note("cost basis (total)", mapping.cost_total, &mut report);
    if mapping.cost_total.is_none() {
        note(
            "cost basis (per share)",
            mapping.cost_per_share,
            &mut report,
        );
        report.cost_basis_derived = mapping.cost_per_share.is_some();
    }
    note("account", mapping.account, &mut report);

    report.ignored = header
        .iter()
        .enumerate()
        .filter(|(index, name)| !used.contains(index) && !name.is_empty())
        .map(|(_, name)| name.clone())
        .collect();

    // Grouped by account when the file names one, so a single export
    // containing several accounts becomes several portfolios.
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("portfolio")
        .to_owned();
    let mut grouped: BTreeMap<String, Vec<Holding>> = BTreeMap::new();

    for (offset, line) in lines.iter().enumerate().skip(header_at + 1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields = split_on(line, delimiter);

        match holding_from(&fields, &mapping) {
            Some(holding) => {
                let account = mapping
                    .account
                    .and_then(|index| fields.get(index))
                    .filter(|value| !value.is_empty())
                    .map_or_else(|| stem.clone(), |value| format!("{stem} · {value}"));
                grouped.entry(account).or_default().push(holding);
                report.rows_imported += 1;
            }
            None => report
                .rows_skipped
                .push(format!("line {}: {line}", offset + 1)),
        }
    }

    if grouped.is_empty() {
        // The header was understood but nothing in the file survived parsing.
        // Returning an empty list here would make the file disappear without
        // a word; an empty portfolio carrying the report at least says that
        // it was read and that every row was rejected, and why.
        return Ok(vec![Imported {
            portfolio: Portfolio {
                name: stem,
                as_of,
                holdings: Vec::new(),
            },
            report,
        }]);
    }

    Ok(grouped
        .into_iter()
        .map(|(name, holdings)| Imported {
            portfolio: Portfolio {
                name,
                as_of,
                holdings,
            },
            report: report.clone(),
        })
        .collect())
}

fn holding_from(fields: &[String], mapping: &Mapping) -> Option<Holding> {
    let text = |index: Option<usize>| {
        index
            .and_then(|index| fields.get(index))
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    let instrument = text(mapping.instrument)
        .or_else(|| text(mapping.description))?
        .to_owned();

    // A summary line is not a holding, and importing one double-counts the
    // whole account.
    if SUMMARY_ROWS.contains(&normalise(&instrument).as_str()) {
        return None;
    }

    let quantity = mapping
        .quantity
        .and_then(|index| number(fields.get(index)?))
        .filter(|quantity| *quantity >= 0.0);
    let price = mapping.price.and_then(|index| number(fields.get(index)?));
    let value = mapping.value.and_then(|index| number(fields.get(index)?));

    // Money, one way or another, or this is not a position we can report.
    //
    // Cash is the exception and needs stating: it carries a quantity of
    // dollars and no price, because its price is one. The valuation layer has
    // always known that; this check did not, so a cash line was silently
    // dropped from a file that parsed perfectly otherwise.
    let is_cash = instrument.eq_ignore_ascii_case(crate::CASH);
    if !is_cash && value.is_none() && !(quantity.is_some() && price.is_some()) {
        return None;
    }

    let cost_basis = mapping
        .cost_total
        .and_then(|index| number(fields.get(index)?))
        .or_else(|| {
            // Per share only means anything when there are shares.
            mapping
                .cost_per_share
                .and_then(|index| number(fields.get(index)?))
                .zip(quantity)
                .map(|(per_share, quantity)| per_share * quantity)
        });

    Some(Holding {
        instrument,
        quantity,
        cost_basis,
        price,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_of() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 4).expect("valid")
    }

    fn load(body: &str) -> Vec<Imported> {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("export.csv"), body).expect("fixture writes");
        CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect("should read")
    }

    #[test]
    fn the_native_format_still_reads() {
        let imported = load(
            "instrument,quantity,cost_basis,price\n\
             AAPL.NASDAQ,25,4210.50,191.24\n",
        );
        let holding = &imported[0].portfolio.holdings[0];
        assert_eq!(holding.instrument, "AAPL.NASDAQ");
        assert_eq!(holding.cost_basis, Some(4210.50));
        assert_eq!(holding.price, Some(191.24));
        assert_eq!(holding.quantity, Some(25.0));
    }

    #[test]
    fn a_brokerage_style_export_maps_by_column_name() {
        let imported = load(
            "Account Number,Symbol,Description,Quantity,Last Price,Current Value,Cost Basis Total\n\
             X123,AAPL,APPLE INC,25,$191.24,\"$4,781.00\",$4210.50\n",
        );
        let report = &imported[0].report;

        assert!(
            report
                .columns
                .contains(&("instrument".to_owned(), "Symbol".to_owned())),
            "{:?}",
            report.columns
        );
        assert!(report
            .columns
            .contains(&("price".to_owned(), "Last Price".to_owned())));
        assert!(!report.cost_basis_derived, "a total was reported directly");
        assert_eq!(imported[0].portfolio.holdings[0].price, Some(191.24));
    }

    #[test]
    fn a_per_share_cost_is_multiplied_up_and_the_report_says_so() {
        // The mistake that would otherwise be invisible: average cost read as
        // if it were the total understates cost by the share count, and every
        // number downstream still looks reasonable.
        let imported = load(
            "Symbol,Quantity,Last Price,Average Cost Basis\n\
             AAPL,10,200.00,150.00\n",
        );
        assert_eq!(
            imported[0].portfolio.holdings[0].cost_basis,
            Some(1500.0),
            "10 shares at 150 average is 1500 total, not 150"
        );
        assert!(imported[0].report.cost_basis_derived);
    }

    #[test]
    fn a_total_cost_column_beats_a_per_share_one_when_both_exist() {
        let imported = load(
            "Symbol,Quantity,Last Price,Cost Basis Total,Average Cost Basis\n\
             AAPL,10,200.00,1234.00,150.00\n",
        );
        assert_eq!(imported[0].portfolio.holdings[0].cost_basis, Some(1234.0));
        assert!(!imported[0].report.cost_basis_derived);
    }

    #[test]
    fn a_retirement_export_with_no_cost_basis_imports_without_inventing_one() {
        let imported = load(
            "Investment,Shares,Share Price,Ending Market Value\n\
             FIDELITY 500 INDEX,120.5,180.22,21716.51\n",
        );
        let holding = &imported[0].portfolio.holdings[0];
        assert_eq!(holding.instrument, "FIDELITY 500 INDEX");
        assert_eq!(holding.cost_basis, None, "absent is absent, not zero");
        assert_eq!(holding.price, Some(180.22));
    }

    #[test]
    fn a_value_column_with_no_price_is_divided_out() {
        let imported = load("Investment,Units,Balance\nTARGET 2050,100,25000\n");
        // Value with no price: the value is kept as reported and the unit
        // price is derived at valuation time, where the units live.
        assert_eq!(imported[0].portfolio.holdings[0].value, Some(25_000.0));
        assert_eq!(imported[0].portfolio.holdings[0].quantity, Some(100.0));
    }

    #[test]
    fn several_accounts_in_one_file_become_several_portfolios() {
        let imported = load(
            "Account Number,Symbol,Quantity,Last Price\n\
             X1,AAPL,10,100\n\
             X2,MSFT,5,200\n\
             X1,TSLA,2,300\n",
        );
        assert_eq!(imported.len(), 2, "two accounts, two portfolios");
        assert_eq!(imported[0].portfolio.holdings.len(), 2, "X1 holds two");
        assert!(imported[0].portfolio.name.contains("X1"));
    }

    #[test]
    fn a_disclaimer_footer_does_not_cost_the_whole_file() {
        let imported = load(
            "Symbol,Quantity,Last Price\n\
             AAPL,10,100\n\
             \n\
             \"Brokerage services provided by Example LLC. Past performance is no guarantee.\"\n",
        );
        assert_eq!(imported[0].portfolio.holdings.len(), 1);
        assert_eq!(
            imported[0].report.rows_skipped.len(),
            1,
            "and the skipped line is still reported, not hidden"
        );
    }

    #[test]
    fn a_quoted_field_may_contain_commas() {
        // Fund names do this constantly. A naive split shifts every later
        // column left by one, the quantity becomes text, every row is
        // skipped, and the whole portfolio silently disappears.
        let imported = load(
            "Investment,Shares,Share Price,Ending Market Value
             \"VANGUARD TARGET RETIREMENT 2050 FUND, INVESTOR SHARES\",100,25.00,2500.00
",
        );
        let holding = &imported[0].portfolio.holdings[0];
        assert_eq!(
            holding.instrument,
            "VANGUARD TARGET RETIREMENT 2050 FUND, INVESTOR SHARES"
        );
        assert_eq!(holding.quantity, Some(100.0));
        assert_eq!(holding.price, Some(25.0));
    }

    #[test]
    fn a_doubled_quote_is_an_escaped_quote() {
        let fields = split("a,\"say \"\"hi\"\" now\",c");
        assert_eq!(fields, vec!["a", "say \"hi\" now", "c"]);
    }

    #[test]
    fn a_file_where_every_row_fails_still_reports_itself() {
        // Otherwise the file vanishes from the list with no explanation, which
        // is indistinguishable from never having added it.
        let imported = load(
            "Symbol,Quantity,Last Price
AAPL,not-a-number,oops
",
        );
        assert_eq!(imported.len(), 1);
        assert!(imported[0].portfolio.holdings.is_empty());
        assert_eq!(imported[0].report.rows_imported, 0);
        assert_eq!(imported[0].report.rows_skipped.len(), 1);
    }

    #[test]
    fn a_cash_line_survives_the_import() {
        // Regression: the "must have money" check rejected cash, because cash
        // has a quantity of dollars and no price. It parsed, then vanished,
        // and the only visible symptom was a total that was quietly short.
        let imported = load(
            "instrument,quantity,cost_basis,price
             AAPL,10,1000,150
             CASH,11.85,11.85,
",
        );
        assert_eq!(imported[0].portfolio.holdings.len(), 2);
        assert!(
            imported[0].report.rows_skipped.is_empty(),
            "{:?}",
            imported[0].report.rows_skipped
        );

        let valued = imported[0]
            .portfolio
            .value(&std::collections::BTreeMap::new());
        assert!((valued.cash - 11.85).abs() < 1e-9);
        assert!((valued.total_value - 1511.85).abs() < 1e-9);
    }

    #[test]
    fn a_tab_separated_table_pasted_from_a_web_page_reads() {
        // A 401(k) platform whose only export is a PDF still lets you copy
        // the holdings table, and what you get is tab separated.
        let imported = load(
            "Name\tAsset Class\t% Invested\tBalance\tCost Basis\n\
             JPMCB SRPB 2050 CFX1\tBlended Fund Investments*\t100.00%\t$224,630.21\t$154,350.78\n\
             Account Total\t\t100%\t$224,630.21\t\n",
        );

        assert_eq!(
            imported[0].portfolio.holdings.len(),
            1,
            "the Account Total row must not become a second position"
        );
        let holding = &imported[0].portfolio.holdings[0];
        assert_eq!(holding.instrument, "JPMCB SRPB 2050 CFX1");
        assert_eq!(holding.quantity, None, "no unit count is reported");
        assert_eq!(holding.value, Some(224_630.21));
        assert_eq!(holding.cost_basis, Some(154_350.78));
    }

    #[test]
    fn a_total_row_is_skipped_but_a_fund_named_total_is_not() {
        let imported = load(
            "Symbol,Description,Quantity,Last Price\n\
             VTI,VANGUARD TOTAL STOCK MARKET,10,100\n\
             Account Total,,,\n",
        );
        assert_eq!(
            imported[0].portfolio.holdings.len(),
            1,
            "a real fund with 'total' in its name must survive"
        );
        assert_eq!(imported[0].portfolio.holdings[0].instrument, "VTI");
    }

    #[test]
    fn a_header_it_cannot_understand_is_refused_rather_than_guessed_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.csv"), "alpha,beta,gamma\n1,2,3\n").expect("writes");
        let err = CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect_err("nothing recognisable");
        assert!(matches!(err, CsvError::Unrecognised { .. }), "{err}");
    }

    #[test]
    fn parentheses_mean_negative_and_placeholders_mean_absent() {
        assert_eq!(number("$1,234.56"), Some(1234.56));
        assert_eq!(number("(12.00)"), Some(-12.0));
        assert_eq!(number("n/a"), None);
        assert_eq!(number("--"), None);
        assert_eq!(number(""), None);
    }

    #[test]
    fn unused_columns_are_listed_so_a_missed_mapping_is_visible() {
        let imported = load(
            "Symbol,Quantity,Last Price,Today's Gain/Loss Dollar\n\
             AAPL,10,100,5.00\n",
        );
        assert!(
            imported[0]
                .report
                .ignored
                .iter()
                .any(|name| name.contains("Gain/Loss")),
            "{:?}",
            imported[0].report.ignored
        );
    }
}
