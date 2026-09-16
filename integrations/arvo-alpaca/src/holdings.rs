//! What an Alpaca account holds, for importing as a portfolio.
//!
//! Paper and live are different accounts with different positions, so this
//! reads the one an [`Env`] names, with that environment's keys against that
//! environment's endpoint. Alpaca prices every position and reports cash, so
//! the imported file needs nothing fetched afterwards.

use arvo_data::source::SourceError;
use serde_json::Value;

use crate::auth::{self, Env};

/// One position: a quantity of a symbol, priced by Alpaca, with what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// The bare ticker, e.g. `AAPL`.
    pub symbol: String,
    pub quantity: f64,
    /// Total paid for what is held now, when Alpaca says.
    pub cost_basis: Option<f64>,
    /// Alpaca's current price for it.
    pub price: Option<f64>,
}

/// One Alpaca account: its number, its positions and its cash.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountHoldings {
    pub account_number: String,
    pub positions: Vec<Position>,
    pub cash: f64,
}

fn num(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.parse().ok(),
        other => other.as_f64(),
    }
}

/// The positions and cash of the account `env` names.
///
/// # Errors
///
/// [`SourceError::NoSession`] when that environment has no keys, or a
/// transport error from Alpaca.
pub async fn holdings(env: Env) -> Result<AccountHoldings, SourceError> {
    let host = env.trading_host();
    let positions = auth::get_env(env, &format!("{host}/v2/positions")).await?;
    let account = auth::get_env(env, &format!("{host}/v2/account")).await?;
    Ok(parse(&positions, &account))
}

/// Reads the two Alpaca replies into an account. Split out so it is tested
/// without a network.
fn parse(positions: &Value, account: &Value) -> AccountHoldings {
    let held = positions
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|position| {
            let symbol = position.get("symbol")?.as_str()?.to_owned();
            let quantity = num(position.get("qty"))?;
            Some(Position {
                symbol,
                quantity,
                cost_basis: num(position.get("cost_basis")),
                price: num(position.get("current_price")),
            })
        })
        .collect();
    AccountHoldings {
        account_number: account.get("account_number").and_then(Value::as_str).unwrap_or_default().to_owned(),
        positions: held,
        cash: num(account.get("cash")).unwrap_or(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_reply_becomes_positions_and_cash_and_a_bad_row_is_skipped() {
        let positions = json!([
            { "symbol": "AAPL", "qty": "10", "cost_basis": "1500.00", "current_price": "190.5" },
            { "symbol": "MSFT", "qty": "3.5", "cost_basis": "900", "current_price": "410" },
            { "qty": "1" }
        ]);
        let account = json!({ "account_number": "PA3ABCDE1234", "cash": "2500.75" });
        let held = parse(&positions, &account);
        assert_eq!(held.account_number, "PA3ABCDE1234");
        assert_eq!(held.cash, 2500.75);
        assert_eq!(held.positions.len(), 2, "the row with no symbol is dropped");
        assert_eq!(held.positions[0], Position { symbol: "AAPL".into(), quantity: 10.0, cost_basis: Some(1500.0), price: Some(190.5) });
    }
}
