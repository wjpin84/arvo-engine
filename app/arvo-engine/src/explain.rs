//! Why did Arvo hold what it held, when it held it (#188).
//!
//! A session's record is one JSON line per event, appended as things
//! happen. Since #188 each event names what caused it: a signal names the
//! bar it was decided on, an order names the signal that asked for it, a
//! fill names the order and the position it left behind. That makes the
//! record a chain rather than a log, and this is the one reader that walks
//! it — `arvo-engine session explain <id> <time>`.
//!
//! The reader works from the file alone. The engine need not be running and
//! the session need not exist any more; the record outlives both, which is
//! what makes it a record.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// One line of the record, as written by `session::Recorder`.
struct Line {
    at: DateTime<Utc>,
    event: String,
    detail: Value,
}

fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let at = DateTime::parse_from_rfc3339(value.get("at")?.as_str()?).ok()?.with_timezone(&Utc);
            Some(Line {
                at,
                event: value.get("event")?.as_str()?.to_owned(),
                detail: value.get("detail").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

fn number(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or_default()
}

/// How one position came to be: the fills that built it, or the
/// reconciliation that adopted it.
enum Origin<'a> {
    Fills(Vec<&'a Line>),
    Adopted(&'a Line),
}

/// The book at `when`, per instrument, and how each position got there.
///
/// Read from the fills' own `position` field where a fill carries one and
/// from the fill's side and quantity where it does not (records written
/// before #188). A reconciliation that adopted a position replaces whatever
/// the fills said: the venue's word, as the session took it.
fn book_at<'a>(lines: &'a [Line], when: DateTime<Utc>) -> BTreeMap<String, (f64, Origin<'a>)> {
    let mut book: BTreeMap<String, (f64, Origin<'a>)> = BTreeMap::new();
    for line in lines.iter().filter(|line| line.at <= when) {
        match line.event.as_str() {
            "filled" => {
                let instrument = text(&line.detail, "instrument");
                let entry = book.entry(instrument).or_insert((0.0, Origin::Fills(Vec::new())));
                let quantity = number(&line.detail, "quantity");
                entry.0 = match line.detail.get("position").and_then(Value::as_f64) {
                    Some(position) => position,
                    None if text(&line.detail, "side") == "sell" => entry.0 - quantity,
                    None => entry.0 + quantity,
                };
                match &mut entry.1 {
                    Origin::Fills(fills) if entry.0.abs() > 1e-9 => fills.push(line),
                    // Flat again: the next position starts a new story.
                    _ => entry.1 = Origin::Fills(if entry.0.abs() > 1e-9 { vec![line] } else { Vec::new() }),
                }
            }
            "reconciled" => {
                // At start: `adopted` is [[symbol, quantity], ...] with no
                // venue suffix; mid-session: `positions` is {instrument: qty}.
                if let Some(positions) = line.detail.get("positions").and_then(Value::as_object) {
                    for (instrument, quantity) in positions {
                        book.insert(instrument.clone(), (quantity.as_f64().unwrap_or_default(), Origin::Adopted(line)));
                    }
                    book.retain(|instrument, _| positions.contains_key(instrument));
                } else if let Some(adopted) = line.detail.get("adopted").and_then(Value::as_array) {
                    for pair in adopted {
                        if let (Some(symbol), Some(quantity)) = (pair.get(0).and_then(Value::as_str), pair.get(1).and_then(Value::as_f64)) {
                            book.insert(symbol.to_owned(), (quantity, Origin::Adopted(line)));
                        }
                    }
                } else if let Some(corrected) = line.detail.get("corrected").and_then(Value::as_array) {
                    // A mid-session reconcile written before the book was
                    // recorded with it (#187 alone): what the venue had is
                    // what the gate was made to hold.
                    for each in corrected {
                        book.insert(text(each, "instrument"), (number(each, "at_venue"), Origin::Adopted(line)));
                    }
                }
            }
            _ => {}
        }
    }
    book.retain(|_, (quantity, _)| quantity.abs() > 1e-9);
    book
}

/// The chain behind one fill: fill ← order ← signal ← bar.
fn chain(lines: &[Line], fill: &Line, out: &mut String) {
    use std::fmt::Write as _;
    let order = text(&fill.detail, "order");
    let sent = lines.iter().find(|line| {
        matches!(line.event.as_str(), "submitted" | "exit") && text(&line.detail, "order") == order
    });
    let signal_id = sent.map(|line| text(&line.detail, "signal")).unwrap_or_default();
    let signal = lines.iter().find(|line| line.event == "signal" && text(&line.detail, "id") == signal_id);
    let bar = signal.and_then(|signal| {
        let at = text(&signal.detail, "bar");
        lines.iter().find(|line| line.event == "bar" && text(&line.detail, "at") == at)
    });

    match bar {
        Some(bar) => {
            let _ = writeln!(out, "    bar       {} close {}  (seen {})", text(&bar.detail, "at"), number(&bar.detail, "close"), bar.at.to_rfc3339());
        }
        None => {
            let _ = writeln!(out, "    bar       not in this record");
        }
    }
    match signal {
        Some(signal) => {
            let exit = match signal.detail.get("exit") {
                Some(Value::String(why)) => format!("  exit: {why}"),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "    signal    {}  {} {} @ {}{exit}",
                signal_id,
                text(&signal.detail, "side").to_lowercase(),
                number(&signal.detail, "quantity"),
                number(&signal.detail, "price"),
            );
        }
        None => {
            let _ = writeln!(out, "    signal    not in this record");
        }
    }
    match sent {
        Some(sent) if sent.event == "exit" => {
            let _ = writeln!(out, "    exit      order {order} sent without asking the gate (ADR-0009), {}", text(&sent.detail, "why"));
        }
        Some(sent) => {
            let _ = writeln!(out, "    gate      accepted; order {order} acknowledged {}", sent.at.to_rfc3339());
        }
        None => {
            let _ = writeln!(out, "    order     {order} — not sent by this session as recorded");
        }
    }
    let _ = writeln!(
        out,
        "    fill      {} {} @ {} at {}  (decided @ {}, {} {})",
        text(&fill.detail, "side"),
        number(&fill.detail, "quantity"),
        number(&fill.detail, "fill_price"),
        text(&fill.detail, "filled_at"),
        number(&fill.detail, "decision_price"),
        text(&fill.detail, "proposer"),
        fill.detail.get("position").and_then(Value::as_f64).map_or(String::new(), |position| format!("→ position {position}")),
    );
}

/// The chain behind every position the session held at `when`.
#[must_use]
pub fn explain(record: &str, when: DateTime<Utc>) -> String {
    use std::fmt::Write as _;
    let lines = parse(record);
    let mut out = String::new();
    if lines.is_empty() {
        return "no record\n".to_owned();
    }
    let book = book_at(&lines, when);
    if book.is_empty() {
        let last = lines.iter().rev().find(|line| line.at <= when && matches!(line.event.as_str(), "filled" | "reconciled"));
        let _ = writeln!(
            out,
            "flat at {}{}",
            when.to_rfc3339(),
            last.map_or(String::new(), |line| format!("; last change to the book was {} at {}", line.event, line.at.to_rfc3339())),
        );
        return out;
    }
    for (instrument, (quantity, origin)) in &book {
        let _ = writeln!(out, "{instrument}: {quantity} at {}", when.to_rfc3339());
        match origin {
            Origin::Adopted(line) => {
                let _ = writeln!(
                    out,
                    "  adopted from the venue by a reconcile at {}: not this rule's decision, and there is no bar or signal behind it",
                    line.at.to_rfc3339()
                );
            }
            Origin::Fills(fills) => {
                for (n, fill) in fills.iter().enumerate() {
                    let _ = writeln!(out, "  fill {} of {}", n + 1, fills.len());
                    chain(&lines, fill, &mut out);
                }
            }
        }
    }
    out
}

/// `<time>` as the CLI takes it: RFC 3339 with an offset, or a naive
/// `YYYY-MM-DDTHH:MM:SS` / `YYYY-MM-DD HH:MM:SS` read as UTC.
///
/// # Errors
///
/// Anything else.
pub fn parse_when(text: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Ok(naive.and_utc());
        }
    }
    Err(format!("{text:?} is not a time; use 2026-09-18T14:37:22Z or 2026-09-18 14:37:22 (UTC)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> String {
        [
            r#"{"at":"2026-09-18T14:36:00+00:00","event":"started","detail":{"experiment":"x"}}"#,
            r#"{"at":"2026-09-18T14:36:01+00:00","event":"bar","detail":{"at":"2026-09-17T00:00:00","close":497.75,"signals":1}}"#,
            r#"{"at":"2026-09-18T14:36:01+00:00","event":"signal","detail":{"id":"2026-09-17T00:00:00#0","bar":"2026-09-17T00:00:00","side":"Buy","quantity":137.0,"price":497.75,"at":"2026-09-17T00:00:00","exit":null}}"#,
            r#"{"at":"2026-09-18T14:36:02+00:00","event":"submitted","detail":{"signal":"2026-09-17T00:00:00#0","order":"ord-1"}}"#,
            r#"{"at":"2026-09-18T14:37:22+00:00","event":"filled","detail":{"order":"ord-1","instrument":"MSFT.RH","side":"buy","proposer":"shadow:sma_cross","quantity":137.0,"decision_price":497.75,"fill_price":497.9,"decision_at":"2026-09-17T00:00:00","filled_at":"2026-09-18T14:37:22","position":137.0}}"#,
            r#"{"at":"2026-09-18T15:00:00+00:00","event":"frozen","detail":{"discrepancies":[{"instrument":"BTCUSD.RH","expected":0.0,"at_venue":0.001}]}}"#,
            r#"{"at":"2026-09-18T15:01:00+00:00","event":"reconciled","detail":{"corrected":[],"positions":{"MSFT.RH":137.0,"BTCUSD.RH":0.001}}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn a_position_is_explained_back_to_the_bar_it_was_decided_on() {
        let out = explain(&record(), parse_when("2026-09-18T14:37:22Z").unwrap());
        assert!(out.starts_with("MSFT.RH: 137 at "), "{out}");
        assert!(out.contains("bar       2026-09-17T00:00:00 close 497.75"), "{out}");
        assert!(out.contains("signal    2026-09-17T00:00:00#0  buy 137 @ 497.75"), "{out}");
        assert!(out.contains("gate      accepted; order ord-1 acknowledged 2026-09-18T14:36:02+00:00"), "{out}");
        assert!(out.contains("fill      buy 137 @ 497.9 at 2026-09-18T14:37:22") && out.contains("→ position 137"), "{out}");
        assert!(!out.contains("BTCUSD"), "adopted later, not held yet");
    }

    #[test]
    fn before_the_fill_the_session_was_flat() {
        let out = explain(&record(), parse_when("2026-09-18 14:37:00").unwrap());
        assert!(out.starts_with("flat at 2026-09-18T14:37:00+00:00"), "{out}");
    }

    #[test]
    fn an_adopted_position_says_so_rather_than_inventing_a_signal() {
        let out = explain(&record(), parse_when("2026-09-18T15:02:00Z").unwrap());
        assert!(out.contains("BTCUSD.RH: 0.001 at"), "{out}");
        assert!(out.contains("adopted from the venue by a reconcile at 2026-09-18T15:01:00+00:00"), "{out}");
        // The reconcile named MSFT too, at the quantity the fills built; it is
        // reported as the venue's word from then on.
        assert!(out.contains("MSFT.RH: 137 at"), "{out}");
    }

    #[test]
    fn a_time_that_is_not_one_is_refused_with_the_shapes_that_work() {
        let refused = parse_when("yesterday").unwrap_err();
        assert!(refused.contains("2026-09-18T14:37:22Z"), "{refused}");
        assert_eq!(parse_when("2026-09-18T14:37:22Z").unwrap(), parse_when("2026-09-18 14:37:22").unwrap());
    }
}
