//! Turning numbers into the strings a person reads.
//!
//! Small, and gathered rather than inlined, because these are the decisions
//! that make a report legible or not — thousands separators, how many decimal
//! places a ratio earns, how much of a hash is enough to recognise — and they
//! have to agree across every view. A percentage rendered two ways in one
//! window looks like two different measurements.

/// Grouped to thousands. A portfolio total is read as a quantity of money,
/// and `128450.75` is materially harder to read at a glance than
/// `128,450.75` — which matters more here than anywhere else in the app.
pub(crate) fn money(value: f64) -> String {
    let negative = value < 0.0;
    let whole = value.abs().trunc();
    let cents = ((value.abs() - whole) * 100.0).round() as u64;
    let digits: Vec<char> = format!("{whole:.0}").chars().collect();

    let mut grouped = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*digit);
    }

    format!("{}${grouped}.{cents:02}", if negative { "-" } else { "" })
}

pub(crate) fn percent(value: f64) -> String {
    format!("{:+.2}%", value * 100.0)
}

/// First twelve characters of a content hash. Enough to compare two by eye
/// and to spot that they differ; the full value lives in the record.
pub(crate) fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// A probability, as a percentage.
///
/// An absent one is a dash rather than 0%: a curve that never moved has no
/// confidence to report, and a zero would read as "measured, and it is
/// certainly not real".
pub(crate) fn probability(value: Option<f64>) -> String {
    value.map_or_else(|| "\u{2014}".to_owned(), |v| format!("{:.0}%", v * 100.0))
}

pub(crate) fn ratio(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"))
}

/// A coloured dot carrying the verdict, so a history list scans at a glance
/// without reading every line.
pub(crate) fn verdict_dot(verdict: &str) -> &'static str {
    match verdict {
        "Supported" => "research-dot supported",
        "Not supported" => "research-dot refuted",
        _ => "research-dot inconclusive",
    }
}

pub(crate) fn verdict_class(verdict: &str) -> &'static str {
    match verdict {
        "Supported" => "research-verdict supported",
        "Not supported" => "research-verdict refuted",
        _ => "research-verdict inconclusive",
    }
}
