//! Holdings read from a file you export yourself.
//!
//! # The format
//!
//! One CSV per portfolio, named after it, with the header:
//!
//! ```text
//! instrument,quantity,cost_basis,price
//! AAPL.NASDAQ,25,4210.50,191.24
//! MSFT.NASDAQ,10,3105.00,
//! CASH,1832.44,1832.44,
//! ```
//!
//! * `instrument` — the same identifier the data library uses, so a holding
//!   and its price history are one instrument rather than two things that
//!   look alike. `CASH` is reserved and worth its face value.
//! * `cost_basis` — **total** paid, not per share. That is what a brokerage
//!   statement reports, and dividing down is safer than multiplying up.
//! * `price` — optional. Blank falls back to the last close in the data
//!   library, and a holding with neither is reported as unpriced rather than
//!   quietly valued at zero.
//!
//! # Why an export rather than a broker connection
//!
//! This is deliberately the boring path, and it is not a placeholder. It
//! needs no credentials, breaks no terms of service, and cannot get an
//! account locked. Robinhood in particular retired its public API, so every
//! library that talks to it drives the private mobile API with full account
//! credentials — the opposite of the scoped, capability-gated access this
//! platform is supposed to use.
//!
//! A sanctioned read-only connector slots in beside this reader when there is
//! one. The domain, the valuation and the view above them do not change.
//!
//! **This is not any particular broker's export format.** It is Arvo's. A
//! mapping from a real statement is a separate, small job best done with an
//! actual file in hand rather than guessed at.

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
    #[error("{path} line {line}: {reason}")]
    Malformed {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// A directory of holdings files, one portfolio each.
#[derive(Debug, Clone)]
pub struct CsvHoldings {
    root: PathBuf,
}

impl CsvHoldings {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every portfolio in the directory, by file name.
    ///
    /// `as_of` is supplied rather than read from the file: a holdings export
    /// is a snapshot and the file does not reliably say when it was taken.
    /// The caller knows better — the file's modification time, or today.
    ///
    /// A missing directory is an empty library, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`CsvError`] if the directory cannot be listed, or if any file
    /// in it cannot be read or parsed. One bad file fails the load loudly
    /// rather than silently returning a short list — a portfolio that is
    /// quietly missing a position is worse than one that refuses to load.
    pub fn portfolios(&self, as_of: NaiveDate) -> Result<Vec<Portfolio>, CsvError> {
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

        paths
            .iter()
            .map(|path| read_portfolio(path, as_of))
            .collect()
    }
}

fn read_portfolio(path: &Path, as_of: NaiveDate) -> Result<Portfolio, CsvError> {
    let text = std::fs::read_to_string(path).map_err(|source| CsvError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("portfolio")
        .to_owned();

    let mut holdings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || (index == 0 && line.to_ascii_lowercase().starts_with("instrument")) {
            continue;
        }
        holdings.push(parse_row(path, index + 1, line)?);
    }

    Ok(Portfolio {
        name,
        as_of,
        holdings,
    })
}

/// ponytail: split on commas, no quoting. A holdings export is symbols and
/// numbers; swap in the `csv` crate the day a broker ships a quoted field.
fn parse_row(path: &Path, line_no: usize, line: &str) -> Result<Holding, CsvError> {
    let malformed = |reason: String| CsvError::Malformed {
        path: path.to_path_buf(),
        line: line_no,
        reason,
    };

    let mut fields = line.split(',').map(str::trim);
    let instrument = fields
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed("missing instrument".to_owned()))?
        .to_owned();

    let mut number = |name: &str, required: bool| -> Result<Option<f64>, CsvError> {
        let raw = fields.next().unwrap_or("");
        if raw.is_empty() {
            return if required {
                Err(malformed(format!("missing {name}")))
            } else {
                Ok(None)
            };
        }
        // Statements like to write 1,234.56 and $1,234.56. The comma is
        // already gone by the time we get here (it was the delimiter), so a
        // thousands separator would have silently split the field — hence
        // rejecting anything that does not parse rather than salvaging it.
        let cleaned = raw.trim_start_matches('$');
        let value: f64 = cleaned
            .parse()
            .map_err(|err| malformed(format!("{name} {raw:?}: {err}")))?;
        if !value.is_finite() {
            return Err(malformed(format!("{name} {raw:?} is not finite")));
        }
        Ok(Some(value))
    };

    let quantity = number("quantity", true)?.unwrap_or_default();
    let cost_basis = number("cost_basis", true)?.unwrap_or_default();
    let price = number("price", false)?;

    if quantity < 0.0 {
        return Err(malformed(format!(
            "quantity {quantity} is negative; short positions are not modelled"
        )));
    }
    if price.is_some_and(|price| price < 0.0) {
        return Err(malformed("price is negative".to_owned()));
    }

    Ok(Holding {
        instrument,
        quantity,
        cost_basis,
        price,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_of() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 4).expect("valid")
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("fixture should write");
    }

    #[test]
    fn a_holdings_file_reads_into_a_named_portfolio() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "brokerage.csv",
            "instrument,quantity,cost_basis,price\n\
             AAPL.NASDAQ,25,4210.50,191.24\n\
             MSFT.NASDAQ,10,3105.00,\n\
             CASH,1832.44,1832.44,\n",
        );

        let portfolios = CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect("should read");

        assert_eq!(portfolios.len(), 1);
        assert_eq!(portfolios[0].name, "brokerage", "named after the file");
        assert_eq!(portfolios[0].holdings.len(), 3);
        assert_eq!(portfolios[0].holdings[0].price, Some(191.24));
        assert_eq!(
            portfolios[0].holdings[1].price, None,
            "a blank price falls back to the data library later"
        );
    }

    #[test]
    fn a_dollar_sign_is_tolerated_because_statements_write_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "p.csv",
            "instrument,quantity,cost_basis,price\nA.X,2,$100.00,$60.00\n",
        );

        let portfolios = CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect("should read");
        assert_eq!(portfolios[0].holdings[0].price, Some(60.0));
    }

    #[test]
    fn a_bad_row_fails_the_load_and_names_the_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "p.csv",
            "instrument,quantity,cost_basis,price\n\
             A.X,2,100.00,60.00\n\
             B.X,notanumber,50,\n",
        );

        let err = CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect_err("should refuse");
        match err {
            CsvError::Malformed { line, .. } => assert_eq!(line, 3),
            other => panic!("expected a malformed row, got {other}"),
        }
    }

    #[test]
    fn a_short_position_is_refused_rather_than_mis_valued() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "p.csv",
            "instrument,quantity,cost_basis,price\nA.X,-5,100,20\n",
        );

        let err = CsvHoldings::new(dir.path())
            .portfolios(as_of())
            .expect_err("should refuse");
        assert!(
            matches!(err, CsvError::Malformed { ref reason, .. } if reason.contains("negative")),
            "{err}"
        );
    }

    #[test]
    fn an_absent_directory_is_an_empty_library_not_a_failure() {
        assert!(CsvHoldings::new("/no/such/place")
            .portfolios(as_of())
            .expect("absence is not failure")
            .is_empty());
    }
}
