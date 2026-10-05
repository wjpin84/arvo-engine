//! The review after the close (#217): what the day's sessions did, read
//! back off their records, against what the findings said they would do.
//!
//! The session record already holds everything: every signal and what the
//! gate did with it, every fill against its decision price, the journal on
//! every trade, the verdict and the warning band as they moved, and what a
//! person did (halts, resumes, positions adopted). This is the reader. It
//! writes one Markdown file and one JSON file per day under `reviews/`, so a
//! person opens it in the editor and an agent asks for it through a tool.
//!
//! A day is a UTC date. The US regular session sits inside one (13:30 to
//! 20:00 UTC), which is what makes the simple thing correct here; a market
//! whose session crosses midnight UTC needs the per-market calendar the
//! all-markets issue asks for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where the reviews go, under the project.
pub const DIR: &str = "reviews";
/// Where the session records are, under the project.
const SESSIONS: &str = "sessions";

/// One fill of the day, against the price the decision was made at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub at: String,
    pub instrument: String,
    pub side: String,
    pub quantity: f64,
    pub decision_price: f64,
    pub fill_price: f64,
    /// Positive is adverse, whichever way the order went.
    pub slippage_bps: f64,
    pub latency_ms: i64,
}

/// One round trip closed on the day, with what the rule saw when it opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundTrip {
    pub instrument: String,
    pub opened: String,
    pub closed: String,
    pub quantity: f64,
    pub entry: f64,
    pub exit: f64,
    pub pnl: f64,
    /// The condition that fired, in the rule's words; empty when unknown.
    pub rule: String,
    pub regime: String,
    /// Why it was closed: `signal`, `stop`, `target`, `halt`, or empty.
    pub exit_reason: String,
    /// The stop the gate was watching for this trip: the entry less the
    /// signal's stop distance. `None` when the signal carried no stop or
    /// the record predates the distance being kept.
    #[serde(default)]
    pub stop: Option<f64>,
}

/// An order from the moment it was sent to the moment it ended: what a
/// chart draws as a line at the order's price from placement to fill, or
/// to the end of the day when it never filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub placed_at: String,
    /// `buy` or `sell`.
    pub side: String,
    /// The price the rule decided at: the signal's reference price, which
    /// is what the order was sent against.
    pub price: f64,
    /// When the venue answered, `None` while it never did.
    pub resolved_at: Option<String>,
    /// `filled`, else `unfilled`: ended at the venue without a fill, or
    /// still open when the record ends.
    pub outcome: String,
}

/// Losses with the same condition, regime and exit, added up: how a rule
/// is failing, rather than a list of red rows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LossGroup {
    pub count: u32,
    pub total: f64,
}

/// One signal the gate refused, with when and why (#229). The counts in
/// [`SessionReview::refused`] say how often; this says where on the day, so
/// a chart can put it on the bar it happened at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub at: String,
    /// The gate's reason, grouped the way the counts group it.
    pub reason: String,
    /// The reason in full, as the record has it.
    pub detail: String,
}

/// A stretch of the day the session was not taking entries (#229): frozen on
/// a discrepancy or a dark feed, or halted. `until` is absent when it had not
/// ended by the last event of the day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub from: String,
    #[serde(default)]
    pub until: Option<String>,
    /// "frozen" or "halted".
    pub kind: String,
    pub why: String,
}

/// One session's day.
/// When a signal fired and what it was: the rule's name for an entry, the
/// exit's reason for an exit. Kept so a chart can put the day's signals on
/// the time axis, which a count cannot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub at: String,
    pub what: String,
    /// The prices the rule decided against, on an entry. `default`: a
    /// review written before this field still reads.
    #[serde(default)]
    pub levels: Vec<Level>,
}

/// A price a rule decided against, by its name in the rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub name: String,
    pub price: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionReview {
    /// The record's file stem: the finding and the executor.
    pub id: String,
    pub instrument: String,
    pub bars: u32,
    pub signals: u32,
    /// Each of them, in order. `default` because a review written before
    /// this field still reads.
    #[serde(default)]
    pub signals_at: Vec<Signal>,
    /// Every order placed or resolved on the day, from sending to its end.
    /// `default` because a review written before this field still reads.
    #[serde(default)]
    pub orders: Vec<Order>,
    /// Entries and exits that went to the venue.
    pub submitted: u32,
    /// Orders that ended at the venue without a fill (#231).
    #[serde(default)]
    pub unfilled: u32,
    /// Refusals by the gate's reason.
    pub refused: BTreeMap<String, u32>,
    /// Exits by reason.
    pub exits: BTreeMap<String, u32>,
    pub fills: Vec<Fill>,
    pub mean_slippage_bps: Option<f64>,
    /// What the finding's cost model assumed, from the record's expectation.
    pub assumed_slippage_bps: Option<f64>,
    /// Mean profit per closed trade the finding led the session to expect.
    pub expected_per_trade: Option<f64>,
    pub round_trips: Vec<RoundTrip>,
    /// Realised on the day, over the round trips that closed on it.
    pub realised: f64,
    /// Losing round trips grouped by `rule | regime | exit`.
    pub losses: BTreeMap<String, LossGroup>,
    pub freezes: u32,
    pub feed_gaps: u32,
    /// Each refusal with its time (#229), for a chart. `default` because a
    /// review written before this had none.
    #[serde(default)]
    pub refusals: Vec<Refusal>,
    /// The stretches the session was not taking entries (#229).
    #[serde(default)]
    pub spans: Vec<Span>,
    /// Halts, in the record's words: the gate's, or a person's.
    pub halts: Vec<String>,
    /// How the session ended, when nobody stopped it (#13): it failed, or the
    /// engine hosting it died and the next one found the record unfinished.
    #[serde(default)]
    pub endings: Vec<String>,
    /// The verdict each time it changed, with its reason.
    pub verdicts: Vec<String>,
    /// The limits entered, each time the band moved.
    pub warnings: Vec<String>,
    /// What a person did: positions adopted, a halt by hand, a resume, a
    /// stop. Intervening after losses is the most common way a Supported
    /// rule underperforms its backtest, so these sit beside the P&L.
    pub interventions: Vec<String>,
}

/// The day, across sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub day: NaiveDate,
    pub written_at: DateTime<Utc>,
    pub sessions: Vec<SessionReview>,
}

impl Review {
    /// Realised across every session, on the day.
    #[must_use]
    pub fn realised(&self) -> f64 {
        self.sessions.iter().map(|session| session.realised).sum()
    }
}

struct Line {
    at: DateTime<Utc>,
    event: String,
    detail: Value,
}

/// What a signal said when it fired, kept by id for the orders and the
/// round trips that came of it.
#[derive(Default, Clone)]
struct Journaled {
    rule: String,
    regime: String,
    /// The reference price the rule decided at.
    price: Option<f64>,
    /// How far below the entry the stop sat, when the signal carried one.
    stop_distance: Option<f64>,
}

fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let at = DateTime::parse_from_rfc3339(value.get("at")?.as_str()?).ok()?.with_timezone(&Utc);
            Some(Line { at, event: value.get("event")?.as_str()?.to_owned(), detail: value.get("detail").cloned().unwrap_or(Value::Null) })
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

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

/// Reads every record under `root/sessions` and reviews `day`. A session
/// with nothing on that day is left out; a project with no records reviews
/// an empty day rather than failing, since a quiet day is a day.
#[must_use]
pub fn review(root: &Path, day: NaiveDate) -> Review {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join(SESSIONS)) {
        let mut paths: Vec<PathBuf> =
            entries.filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect();
        paths.sort();
        for path in paths {
            let Ok(record) = std::fs::read_to_string(&path) else { continue };
            let id = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            if let Some(reviewed) = review_record(&id, &record, day) {
                sessions.push(reviewed);
            }
        }
    }
    Review { day, written_at: Utc::now(), sessions }
}

/// One record's day. `None` when nothing happened on it.
#[must_use]
pub fn review_record(id: &str, record: &str, day: NaiveDate) -> Option<SessionReview> {
    let lines = parse(record);
    if !lines.iter().any(|line| line.at.date_naive() == day) {
        return None;
    }
    let mut out = SessionReview { id: id.to_owned(), ..SessionReview::default() };

    // What the whole record says about each signal and order, whatever day
    // it was: a round trip closed today may have opened last week.
    let mut journal: BTreeMap<String, Journaled> = BTreeMap::new(); // signal id → what the signal said
    let mut submitted: BTreeMap<String, String> = BTreeMap::new(); // order → signal id
    let mut exit_orders: BTreeMap<String, String> = BTreeMap::new(); // order → why
    let mut orders: BTreeMap<String, Order> = BTreeMap::new(); // order → its life
    // An order is sent against the signal's reference price, which is the
    // price the line is drawn at.
    let placed = |orders: &mut BTreeMap<String, Order>, journal: &BTreeMap<String, Journaled>, line: &Line, side: &str| {
        let order = text(&line.detail, "order");
        if order.is_empty() {
            return;
        }
        let price = journal.get(&text(&line.detail, "signal")).and_then(|signal| signal.price).unwrap_or_default();
        orders.insert(
            order.clone(),
            Order { id: order, placed_at: line.at.to_rfc3339(), side: side.to_owned(), price, resolved_at: None, outcome: "unfilled".to_owned() },
        );
    };
    for line in &lines {
        match line.event.as_str() {
            "instrument" => out.instrument = text(&line.detail, "id"),
            "expectation" => {
                out.expected_per_trade = number(&line.detail, "expectancy");
                out.assumed_slippage_bps = number(&line.detail, "slippage_bps");
            }
            "signal" => {
                journal.insert(
                    text(&line.detail, "id"),
                    Journaled {
                        rule: text(&line.detail, "rule"),
                        regime: text(&line.detail, "regime"),
                        price: number(&line.detail, "price"),
                        stop_distance: number(&line.detail, "stop_distance"),
                    },
                );
            }
            "submitted" => {
                submitted.insert(text(&line.detail, "order"), text(&line.detail, "signal"));
                placed(&mut orders, &journal, line, "buy");
            }
            "exit" => {
                let order = text(&line.detail, "order");
                if !order.is_empty() {
                    exit_orders.insert(order, text(&line.detail, "why"));
                    placed(&mut orders, &journal, line, "sell");
                }
            }
            "filled" => {
                if let Some(order) = orders.get_mut(&text(&line.detail, "order")) {
                    order.resolved_at = Some(line.at.to_rfc3339());
                    order.outcome = "filled".to_owned();
                }
            }
            _ => {}
        }
    }
    // The day's orders: placed today, or resolved today after an earlier day.
    let on_day = |at: &str| at.starts_with(&day.to_string());
    out.orders = orders.into_values().filter(|order| on_day(&order.placed_at) || order.resolved_at.as_deref().is_some_and(on_day)).collect();

    // The book, fill by fill, so a sell today realises against the buys
    // that built the position, whenever they were.
    let mut held: f64 = 0.0;
    let mut entry: f64 = 0.0;
    let mut opened_at = String::new();
    let mut opened_by: Option<String> = None; // the order that opened the trip
    for line in &lines {
        let today = line.at.date_naive() == day;
        match line.event.as_str() {
            "bar" if today => out.bars += 1,
            "signal" if today => {
                out.signals += 1;
                // An entry names its rule; an exit names its reason.
                let what = match line.detail.get("exit") {
                    Some(Value::String(why)) => format!("exit: {why}"),
                    _ => text(&line.detail, "rule"),
                };
                let levels = line
                    .detail
                    .get("levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| Some(Level { name: level.get("name")?.as_str()?.to_owned(), price: level.get("price")?.as_f64()? }))
                            .collect()
                    })
                    .unwrap_or_default();
                out.signals_at.push(Signal { at: line.at.to_rfc3339(), what, levels });
            }
            "submitted" if today => out.submitted += 1,
            "refused" if today => {
                let detail = text(&line.detail, "why");
                let reason = reason_of(&detail);
                *out.refused.entry(reason.clone()).or_default() += 1;
                out.refusals.push(Refusal { at: line.at.to_rfc3339(), reason, detail });
            }
            "exit" if today => {
                *out.exits.entry(text(&line.detail, "why")).or_default() += 1;
                // An exit that carried an order went to the venue (#231).
                if line.detail.get("order").is_some_and(|order| !order.is_null()) {
                    out.submitted += 1;
                }
            }
            "unfilled" if today => {
                out.unfilled += line.detail.get("count").and_then(serde_json::Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
            }
            "filled" => {
                let side = text(&line.detail, "side");
                let quantity = number(&line.detail, "quantity").unwrap_or_default();
                let decision = number(&line.detail, "decision_price").unwrap_or_default();
                let price = number(&line.detail, "fill_price").unwrap_or_default();
                let order = text(&line.detail, "order");
                if today {
                    let adverse = if side == "sell" { decision - price } else { price - decision };
                    let latency = parse_naive(&text(&line.detail, "filled_at")).zip(parse_naive(&text(&line.detail, "decision_at")))
                        .map_or(0, |(filled, decided)| (filled - decided).num_milliseconds());
                    out.fills.push(Fill {
                        at: line.at.to_rfc3339(),
                        instrument: text(&line.detail, "instrument"),
                        side: side.clone(),
                        quantity,
                        decision_price: decision,
                        fill_price: price,
                        slippage_bps: if decision > 0.0 { adverse / decision * 10_000.0 } else { 0.0 },
                        latency_ms: latency,
                    });
                }
                if side == "sell" {
                    if held <= 0.0 {
                        continue;
                    }
                    let sold = quantity.min(held);
                    let pnl = (price - entry) * sold;
                    held -= sold;
                    if today {
                        let Journaled { rule, regime, stop_distance, .. } = opened_by
                            .as_ref()
                            .and_then(|order| submitted.get(order))
                            .and_then(|signal| journal.get(signal))
                            .cloned()
                            .unwrap_or_default();
                        // The gate watched the entry less the distance; a
                        // trip built from several buys takes the average entry.
                        let stop = stop_distance.map(|distance| entry - distance);
                        let exit_reason = exit_orders.get(&order).cloned().unwrap_or_default();
                        if pnl < 0.0 {
                            let key = format!(
                                "{} | {} | {}",
                                if rule.is_empty() { "?" } else { &rule },
                                if regime.is_empty() { "?" } else { &regime },
                                if exit_reason.is_empty() { "?" } else { &exit_reason }
                            );
                            let group = out.losses.entry(key).or_default();
                            group.count += 1;
                            group.total += pnl;
                        }
                        out.realised += pnl;
                        out.round_trips.push(RoundTrip {
                            instrument: text(&line.detail, "instrument"),
                            opened: opened_at.clone(),
                            closed: line.at.to_rfc3339(),
                            quantity: sold,
                            entry,
                            exit: price,
                            pnl,
                            rule,
                            regime,
                            exit_reason,
                            stop,
                        });
                    }
                    if held <= 1e-9 {
                        held = 0.0;
                        opened_by = None;
                    }
                } else {
                    if held <= 0.0 {
                        opened_at = line.at.to_rfc3339();
                        opened_by = Some(order);
                    }
                    let total = held + quantity;
                    entry = if total > 0.0 { (entry * held + price * quantity) / total } else { 0.0 };
                    held = total;
                }
            }
            "frozen" if today => {
                out.freezes += 1;
                // The record says what it froze on; the shape of the detail
                // differs by cause, so whichever field is there is the why.
                let why = if line.detail.is_string() {
                    line.detail.as_str().unwrap_or_default().to_owned()
                } else {
                    let stale = text(&line.detail, "stale");
                    if stale.is_empty() { "the book disagrees with the venue".to_owned() } else { format!("stale feed: {stale}") }
                };
                out.spans.push(Span { from: line.at.to_rfc3339(), until: None, kind: "frozen".to_owned(), why });
            }
            // A resume ends the last span that has not ended. Openly by time
            // rather than by matching a cause: a session is frozen once at a
            // time, and the record is in order.
            "resumed" if today => {
                if let Some(span) = out.spans.iter_mut().rev().find(|span| span.until.is_none()) {
                    span.until = Some(line.at.to_rfc3339());
                }
            }
            // A feed gap is noticed only when the next message lands, so the
            // silence it names is already over: the session freezes and thaws
            // in the same instant. It happened, and the counts say so, but it
            // was never a stretch the session spent refusing entries — and
            // drawing it as one on a chart says something untrue about the
            // day. Such a span is dropped when the day is finished.
            "feed_down" if today => out.feed_gaps += 1,
            "halted" if today => {
                let why = if line.detail.is_string() {
                    line.detail.as_str().unwrap_or_default().to_owned()
                } else {
                    text(&line.detail, "reason")
                };
                // A halt does not lift on its own, so the span runs to the
                // end of the day unless a person released it.
                out.spans.push(Span { from: line.at.to_rfc3339(), until: None, kind: "halted".to_owned(), why });
                if line.detail.is_string() {
                    out.halts.push(format!("the gate: {}", line.detail.as_str().unwrap_or_default()));
                } else {
                    let reason = text(&line.detail, "reason");
                    out.halts.push(format!("by hand: {reason}"));
                    out.interventions.push(format!("{} halted by hand: {reason}", clock(line.at)));
                }
            }
            "verdict" if today => {
                let name = text(&line.detail, "verdict");
                let reason = text(&line.detail, "reason");
                out.verdicts.push(if reason.is_empty() { name } else { format!("{name} ({reason})") });
            }
            "warning" if today => {
                if let Some(entered) = line.detail.get("entered").and_then(Value::as_array) {
                    for limit in entered.iter().filter_map(Value::as_str) {
                        out.warnings.push(limit.to_owned());
                    }
                }
            }
            // The book is rebuilt from the venue at every start: whatever the
            // record thought was still open was closed outside this session,
            // or is adopted by the reconcile that follows (#9).
            "started" => {
                held = 0.0;
                entry = 0.0;
                opened_by = None;
            }
            "reconciled" => {
                if let Some(adopted) = line.detail.get("adopted").and_then(Value::as_array) {
                    held = 0.0;
                    entry = 0.0;
                    // (symbol, quantity, entry); a record from before #9 carries
                    // (symbol, quantity) and its adopted lot is priced at zero.
                    for lot in adopted {
                        let quantity = lot.get(1).and_then(Value::as_f64).unwrap_or(0.0);
                        let price = lot.get(2).and_then(Value::as_f64).unwrap_or(0.0);
                        let total = held + quantity;
                        entry = if total > 0.0 { (entry * held + price * quantity) / total } else { 0.0 };
                        held = total;
                    }
                    if held > 0.0 {
                        opened_at = line.at.to_rfc3339();
                        opened_by = None;
                    }
                }
                if today {
                    let adopted = line.detail.get("adopted").and_then(Value::as_array).map_or(0, Vec::len);
                    let corrected = line.detail.get("corrected").and_then(Value::as_array).map_or(0, Vec::len);
                    if adopted > 0 {
                        out.interventions.push(format!("{} adopted {adopted} position(s) the venue already held", clock(line.at)));
                    }
                    if corrected > 0 {
                        out.interventions.push(format!("{} reconciled {corrected} position(s) the rule did not open", clock(line.at)));
                    }
                }
            }
            "resumed" if today && line.detail.is_null() => out.interventions.push(format!("{} resumed by hand", clock(line.at))),
            "stopped" if today => out.interventions.push(format!("{} stopped", clock(line.at))),
            "failed" if today => out.endings.push(format!(
                "failed at {}: {}",
                clock(line.at),
                line.detail.as_str().unwrap_or("no reason recorded")
            )),
            // Stamped with the record's last moment, so it falls on the day
            // the session ended and not the day somebody noticed.
            "dropped" if today => out.endings.push(format!(
                "was dropped after {}: the engine ended without stopping it, and what it held was left at the venue",
                clock(line.at)
            )),
            _ => {}
        }
    }
    if !out.fills.is_empty() {
        out.mean_slippage_bps = Some(out.fills.iter().map(|fill| fill.slippage_bps).sum::<f64>() / out.fills.len() as f64);
    }
    // See the note at "resumed": a span that closed in the instant it opened
    // was a silence that had already ended, not a stretch of the day.
    out.spans.retain(|span| {
        let stamp = |text: &str| chrono::DateTime::parse_from_rfc3339(text).ok();
        let (Some(from), Some(until)) = (stamp(&span.from), span.until.as_deref().and_then(stamp)) else {
            return true;
        };
        (until - from).num_seconds() >= 1
    });
    Some(out)
}

/// `Stale { age_ms: 307244, limit_ms: 500 }` is a `Stale` refusal.
fn reason_of(why: &str) -> String {
    why.split(['{', ':', '(']).next().unwrap_or(why).trim().to_owned()
}

fn parse_naive(text: &str) -> Option<chrono::NaiveDateTime> {
    text.parse().ok()
}

fn clock(at: DateTime<Utc>) -> String {
    at.format("%H:%M").to_string()
}

/// The review as a person reads it.
#[must_use]
pub fn markdown(review: &Review) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Review · {}\n", review.day);
    if review.sessions.is_empty() {
        let _ = writeln!(out, "No session had anything to say on this day.");
        return out;
    }
    let _ = writeln!(
        out,
        "{} session(s); realised {:+.2} across them. Written {}.\n",
        review.sessions.len(),
        review.realised(),
        review.written_at.format("%Y-%m-%d %H:%M UTC")
    );
    for session in &review.sessions {
        let _ = writeln!(out, "## {}\n", session.id);
        let _ = writeln!(
            out,
            "{} · {} bar(s), {} signal(s), {} submitted, {} refused, {} fill(s), realised {:+.2}{}",
            if session.instrument.is_empty() { "?" } else { &session.instrument },
            session.bars,
            session.signals,
            session.submitted,
            session.refused.values().sum::<u32>(),
            session.fills.len(),
            session.realised,
            session.expected_per_trade.map_or(String::new(), |expected| format!(" (the finding expected {expected:+.2} per trade)"))
        );
        if let Some(mean) = session.mean_slippage_bps {
            let worst = session.fills.iter().map(|fill| fill.slippage_bps).fold(f64::MIN, f64::max);
            let latency = session.fills.iter().map(|fill| fill.latency_ms).sum::<i64>() / session.fills.len().max(1) as i64;
            let _ = writeln!(
                out,
                "\nFills cost {mean:.1} bps on average, {worst:.1} at worst{}; signal to fill {latency} ms.",
                session.assumed_slippage_bps.map_or(String::new(), |assumed| format!(" (the finding assumed {assumed:.1})"))
            );
        }
        if !session.refused.is_empty() {
            let _ = writeln!(out, "\nRefused: {}.", session.refused.iter().map(|(why, n)| format!("{why} ×{n}")).collect::<Vec<_>>().join(", "));
        }
        if !session.exits.is_empty() {
            let _ = writeln!(out, "Exits: {}.", session.exits.iter().map(|(why, n)| format!("{why} ×{n}")).collect::<Vec<_>>().join(", "));
        }
        if session.unfilled > 0 {
            let _ = writeln!(out, "Unfilled: {} order(s) ended at the venue without a fill.", session.unfilled);
        }
        if !session.round_trips.is_empty() {
            let _ = writeln!(out, "\n| opened | closed | qty | entry | exit | pnl | rule | regime | exit |\n|---|---|---|---|---|---|---|---|---|");
            for trip in &session.round_trips {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {:.2} | {:.2} | {:+.2} | {} | {} | {} |",
                    &trip.opened[..16.min(trip.opened.len())],
                    &trip.closed[..16.min(trip.closed.len())],
                    trip.quantity,
                    trip.entry,
                    trip.exit,
                    trip.pnl,
                    trip.rule,
                    trip.regime,
                    trip.exit_reason
                );
            }
        }
        if !session.losses.is_empty() {
            let _ = writeln!(out, "\nLosses, grouped (rule | regime | exit):");
            for (key, group) in &session.losses {
                let _ = writeln!(out, "- {key}: {} trade(s), {:+.2}", group.count, group.total);
            }
        }
        let mut notes = Vec::new();
        if session.freezes > 0 {
            notes.push(format!("froze {} time(s)", session.freezes));
        }
        if session.feed_gaps > 0 {
            notes.push(format!("the feed went dark {} time(s)", session.feed_gaps));
        }
        for halt in &session.halts {
            notes.push(format!("halted, {halt}"));
        }
        notes.extend(session.endings.iter().cloned());
        for verdict in &session.verdicts {
            notes.push(format!("verdict became {verdict}"));
        }
        for warning in &session.warnings {
            notes.push(format!("came within the band on {warning}"));
        }
        if !notes.is_empty() {
            let _ = writeln!(out, "\nThe day: {}.", notes.join("; "));
        }
        if !session.interventions.is_empty() {
            let _ = writeln!(out, "\nA person: {}.", session.interventions.join("; "));
        }
        let _ = writeln!(out);
    }
    out
}

/// Writes `reviews/<day>.md` and `reviews/<day>.json` under `root`, and
/// answers the Markdown's path.
///
/// # Errors
///
/// The directory could not be made or a file could not be written.
pub fn write(root: &Path, review: &Review) -> Result<PathBuf, String> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let md = dir.join(format!("{}.md", review.day));
    std::fs::write(&md, markdown(review)).map_err(|err| format!("{}: {err}", md.display()))?;
    let json = dir.join(format!("{}.json", review.day));
    let text = serde_json::to_string_pretty(review).map_err(|err| err.to_string())?;
    std::fs::write(&json, text).map_err(|err| format!("{}: {err}", json.display()))?;
    Ok(md)
}

/// Whether `day` is over for everything that traded in it, as at `now`.
///
/// An equity day ends at the regular close, and the review waits a quarter of an
/// hour past it for the last fills to settle. A day that held a session on an
/// instrument trading around the clock does not end there — it ends when the day
/// does, at midnight UTC — so such a day is only finished once the date has
/// rolled over.
///
/// A day with no sessions at all is not a day anyone needs a review of, which is
/// what replaced asking the equity calendar whether it was a trading day: a
/// Saturday with a crypto session is a trading day, and a Tuesday with nothing
/// running is not.
#[must_use]
pub fn is_finished(review: &Review, now: DateTime<Utc>) -> bool {
    if review.sessions.is_empty() {
        return false;
    }
    let continuous = review.sessions.iter().any(|session| {
        arvo_data::Instrument::of(&session.instrument).hours == arvo_data::Hours::Continuous
    });
    if continuous {
        return now.naive_utc().date() > review.day;
    }
    now.naive_utc() >= arvo_data::session::regular_close(review.day) + chrono::Duration::minutes(15)
}

/// The review already written for `day`, if there is one.
#[must_use]
pub fn read(root: &Path, day: NaiveDate) -> Option<(PathBuf, String)> {
    let md = root.join(DIR).join(format!("{day}.md"));
    std::fs::read_to_string(&md).ok().map(|text| (md, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When a day is over, for an equity day and for one that never closed.
    #[test]
    fn a_continuous_day_is_finished_at_midnight_and_an_equity_day_at_the_close() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 26).expect("valid");
        let reviewed = |instrument: &str| Review {
            day,
            written_at: Utc::now(),
            sessions: vec![SessionReview {
                id: "s".to_owned(),
                instrument: instrument.to_owned(),
                ..SessionReview::default()
            }],
        };
        let at = |hour: u32, minute: u32| {
            day.and_hms_opt(hour, minute, 0).expect("valid").and_utc()
        };

        // 16:00 New York is 20:00 UTC in September; the review waits a quarter
        // of an hour past it.
        let share = reviewed("MSFT.RH");
        assert!(!is_finished(&share, at(20, 10)), "ten minutes after the close is too early");
        assert!(is_finished(&share, at(20, 16)), "a quarter of an hour after it is not");

        // A coin's day is not over when the equity market shuts.
        let coin = reviewed("BTC-USD.ACRYPTO");
        assert!(!is_finished(&coin, at(20, 16)), "the coin is still trading");
        assert!(!is_finished(&coin, at(23, 59)), "and still trading a minute before midnight");
        let tomorrow = (day + chrono::Duration::days(1)).and_hms_opt(0, 5, 0).expect("valid").and_utc();
        assert!(is_finished(&coin, tomorrow), "finished once the day itself is");

        // A day with both waits for the later of them.
        let both = Review {
            sessions: vec![
                share.sessions[0].clone(),
                coin.sessions[0].clone(),
            ],
            ..reviewed("MSFT.RH")
        };
        assert!(!is_finished(&both, at(20, 16)));
        assert!(is_finished(&both, tomorrow));

        // A day nothing ran on is not a day to review — which is what replaced
        // asking the equity calendar whether it was a trading day. A Saturday
        // with a crypto session is a trading day; a Tuesday with nothing is not.
        let empty = Review { day, written_at: Utc::now(), sessions: Vec::new() };
        assert!(!is_finished(&empty, tomorrow));
    }

    /// A buy before a stop, flattened by hand and never sold on the record,
    /// is not the entry of the trip the next session opens (#9): the book is
    /// rebuilt at every start, and a reconcile that adopts a lot prices it.
    #[test]
    fn a_restart_closes_what_the_record_still_held() {
        let fill = |at: &str, order: &str, side: &str, quantity: f64, price: f64| {
            line(at, "filled", serde_json::json!({ "order": order, "instrument": "AAPL.AIEX", "side": side, "quantity": quantity, "decision_price": price, "fill_price": price, "decision_at": at, "filled_at": at }))
        };
        let day = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let record = [
            line("2026-09-21T13:55:00Z", "started", Value::Null),
            line("2026-09-21T13:55:00Z", "instrument", serde_json::json!({ "id": "AAPL.AIEX" })),
            fill("2026-09-21T13:56:11Z", "o1", "buy", 297.0, 335.89),
            line("2026-09-21T14:08:00Z", "stopped", Value::Null),
            line("2026-09-29T14:21:00Z", "started", Value::Null),
            line("2026-09-29T14:21:00Z", "reconciled", serde_json::json!({ "adopted": [], "cancelled": 0, "stranded": 0 })),
            fill("2026-09-30T13:50:11Z", "o2", "buy", 299.0, 333.43),
            fill("2026-09-30T14:05:20Z", "o3", "sell", 299.0, 338.17),
        ]
        .join("\n");
        let reviewed = review_record("f@alpaca-paper", &record, day).expect("the trip closed today");
        assert_eq!(reviewed.round_trips.len(), 1);
        let trip = &reviewed.round_trips[0];
        assert!((trip.entry - 333.43).abs() < 1e-9, "priced against its own buy, not the 09-21 lot: {}", trip.entry);
        assert!((trip.pnl - 1417.26).abs() < 1e-6, "{}", trip.pnl);
        assert!(trip.opened.starts_with("2026-09-30T13:50"), "{}", trip.opened);
        assert!((reviewed.realised - 1417.26).abs() < 1e-6);

        // A reconcile that adopts a lot carries its entry, so a position that
        // legitimately survived a restart is priced at what the venue says.
        let adopted = [
            line("2026-09-29T14:21:00Z", "started", Value::Null),
            line("2026-09-29T14:21:00Z", "instrument", serde_json::json!({ "id": "AAPL.AIEX" })),
            line("2026-09-29T14:21:00Z", "reconciled", serde_json::json!({ "adopted": [["AAPL", 100.0, 330.0]], "cancelled": 0, "stranded": 0 })),
            fill("2026-09-30T14:05:20Z", "o4", "sell", 100.0, 338.0),
        ]
        .join("\n");
        let reviewed = review_record("f@alpaca-paper", &adopted, day).expect("the adopted lot closed today");
        assert_eq!(reviewed.round_trips.len(), 1);
        assert!((reviewed.round_trips[0].pnl - 800.0).abs() < 1e-9, "{}", reviewed.round_trips[0].pnl);
        assert!(reviewed.round_trips[0].rule.is_empty(), "no order of ours opened it");
    }

    fn line(at: &str, event: &str, detail: Value) -> String {
        serde_json::json!({ "at": at, "event": event, "detail": detail }).to_string()
    }

    /// A session nobody stopped says how it ended (#13), under the day it
    /// ended on, and not as something a person did.
    #[test]
    fn a_day_that_ended_in_a_failure_or_a_dead_engine_says_so() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let record = [
            line("2026-09-22T13:40:00Z", "started", Value::Null),
            line("2026-09-22T14:02:00Z", "failed", serde_json::json!("the venue said no")),
            line("2026-09-22T14:30:00Z", "started", Value::Null),
            line("2026-09-22T15:05:00Z", "bar", Value::Null),
            // Noticed two days later, stamped with the record's last moment.
            line("2026-09-22T15:05:00Z", "dropped", serde_json::json!({ "after": "bar", "noticed_at": "2026-09-24T09:00:00Z" })),
        ]
        .join("\n");
        let reviewed = review_record("f@alpaca-paper", &record, day).expect("the day has lines");
        assert_eq!(reviewed.endings.len(), 2, "{:?}", reviewed.endings);
        assert!(reviewed.endings[0].starts_with("failed at ") && reviewed.endings[0].ends_with(": the venue said no"), "{}", reviewed.endings[0]);
        assert!(reviewed.endings[1].starts_with("was dropped after "), "{}", reviewed.endings[1]);
        assert!(reviewed.interventions.is_empty(), "nobody did this: {:?}", reviewed.interventions);
        assert!(review_record("f@alpaca-paper", &record, NaiveDate::from_ymd_opt(2026, 9, 24).unwrap()).is_none(), "not the day it was noticed");

        let said = markdown(&Review { day, written_at: Utc::now(), sessions: vec![reviewed] });
        assert!(said.contains("the engine ended without stopping it"), "{said}");
    }

    /// Two round trips closed on the day: one opened the day before and
    /// stopped out at a loss, one opened and closed on a signal for a gain;
    /// a refusal, a warning, a verdict, a halt by hand and a stop.
    fn record() -> String {
        [
            line("2026-09-20T13:31:00Z", "started", Value::Null),
            line("2026-09-20T13:31:00Z", "instrument", serde_json::json!({ "id": "AAPL.AIEX" })),
            line("2026-09-20T13:31:00Z", "expectation", serde_json::json!({ "expectancy": 12.5, "slippage_bps": 1.0 })),
            line("2026-09-20T14:00:00Z", "signal", serde_json::json!({ "id": "s1", "rule": "close above the opening range", "regime": "ranging" })),
            line("2026-09-20T14:00:00Z", "submitted", serde_json::json!({ "signal": "s1", "order": "o1" })),
            line("2026-09-20T14:00:02Z", "filled", serde_json::json!({ "order": "o1", "instrument": "AAPL.AIEX", "side": "buy", "quantity": 10.0, "decision_price": 100.0, "fill_price": 100.1, "decision_at": "2026-09-20T14:00:00", "filled_at": "2026-09-20T14:00:02" })),
            line("2026-09-21T13:35:00Z", "bar", serde_json::json!({ "at": "2026-09-21T13:30:00" })),
            line("2026-09-21T13:36:00Z", "signal", serde_json::json!({ "id": "x1", "exit": "stop" })),
            line("2026-09-21T13:36:00Z", "exit", serde_json::json!({ "signal": "x1", "why": "stop", "order": "o2" })),
            line("2026-09-21T13:37:00Z", "unfilled", serde_json::json!({ "count": 1, "why": "ended at the venue without a fill" })),
            line("2026-09-21T13:36:03Z", "filled", serde_json::json!({ "order": "o2", "instrument": "AAPL.AIEX", "side": "sell", "quantity": 10.0, "decision_price": 98.0, "fill_price": 97.9, "decision_at": "2026-09-21T13:36:00", "filled_at": "2026-09-21T13:36:03" })),
            line("2026-09-21T14:00:00Z", "signal", serde_json::json!({ "id": "s2", "rule": "close above the opening range", "regime": "trending up" })),
            line("2026-09-21T14:00:00Z", "refused", serde_json::json!({ "signal": "s2", "why": "Stale { age_ms: 900, limit_ms: 500 }" })),
            line("2026-09-21T14:05:00Z", "signal", serde_json::json!({ "id": "s3", "rule": "close above the opening range", "regime": "trending up", "price": 101.5, "levels": [{ "name": "range high", "price": 101.2 }, { "name": "target", "price": 102.4 }] })),
            line("2026-09-21T14:05:00Z", "submitted", serde_json::json!({ "signal": "s3", "order": "o3" })),
            line("2026-09-21T14:05:01Z", "filled", serde_json::json!({ "order": "o3", "instrument": "AAPL.AIEX", "side": "buy", "quantity": 5.0, "decision_price": 100.0, "fill_price": 100.0, "decision_at": "2026-09-21T14:05:00", "filled_at": "2026-09-21T14:05:01" })),
            line("2026-09-21T14:30:00Z", "frozen", serde_json::json!({ "stale": "nothing heard for 900s" })),
            line("2026-09-21T14:40:00Z", "resumed", serde_json::json!("the feed is back")),
            // A gap noticed only once it had ended: frozen and thawed in the
            // same instant, which is not a stretch of the day.
            line("2026-09-21T16:00:00Z", "feed_down", serde_json::json!("nothing heard for 7200s")),
            line("2026-09-21T16:00:00Z", "frozen", serde_json::json!({ "stale": "nothing heard for 7200s" })),
            line("2026-09-21T16:00:00Z", "resumed", serde_json::json!("the feed is back")),
            line("2026-09-21T15:00:00Z", "warning", serde_json::json!({ "entered": ["drawdown"], "cleared": [], "near": ["drawdown 8.1% of a 10.0% limit"] })),
            line("2026-09-21T16:00:00Z", "signal", serde_json::json!({ "id": "x2", "exit": "signal" })),
            line("2026-09-21T16:00:00Z", "exit", serde_json::json!({ "signal": "x2", "why": "signal", "order": "o4" })),
            line("2026-09-21T16:00:01Z", "filled", serde_json::json!({ "order": "o4", "instrument": "AAPL.AIEX", "side": "sell", "quantity": 5.0, "decision_price": 103.0, "fill_price": 103.0, "decision_at": "2026-09-21T16:00:00", "filled_at": "2026-09-21T16:00:01" })),
            line("2026-09-21T16:30:00Z", "verdict", serde_json::json!({ "verdict": "diverging", "reason": "drawdown" })),
            line("2026-09-21T17:00:00Z", "halted", serde_json::json!({ "reason": "enough for today", "flattened": [], "failed": [] })),
            line("2026-09-21T17:01:00Z", "stopped", Value::Null),
        ]
        .join("\n")
    }

    #[test]
    fn a_refusal_keeps_its_time_and_a_freeze_becomes_a_span_that_closes() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let reviewed = review_record("f@alpaca-paper", &record(), day).expect("something happened");

        // The counts say how often; these say where on the day (#229).
        assert_eq!(reviewed.refusals.len(), 1);
        let refusal = &reviewed.refusals[0];
        assert!(refusal.at.starts_with("2026-09-21T14:00:00"), "{}", refusal.at);
        assert_eq!(refusal.reason, "Stale");
        assert!(refusal.detail.contains("age_ms"), "the reason in full: {}", refusal.detail);

        // A freeze that was resumed is a closed span; a halt is not lifted by
        // anything the record holds, so its span stays open.
        let frozen = reviewed.spans.iter().find(|span| span.kind == "frozen").expect("the freeze");
        assert!(frozen.from.starts_with("2026-09-21T14:30:00"), "{}", frozen.from);
        assert!(frozen.until.as_deref().is_some_and(|until| until.starts_with("2026-09-21T14:40:00")), "{:?}", frozen.until);
        assert!(frozen.why.contains("nothing heard"), "{}", frozen.why);
        // The gap that had already ended is counted but is not a stretch: two
        // freezes happened, and only one of them was a span of the day.
        assert_eq!(reviewed.freezes, 2, "both froze the session");
        assert_eq!(reviewed.spans.iter().filter(|span| span.kind == "frozen").count(), 1, "{:?}", reviewed.spans);

        let halted = reviewed.spans.iter().find(|span| span.kind == "halted").expect("the halt");
        assert_eq!(halted.until, None, "a halt stays until someone releases it");
        assert_eq!(halted.why, "enough for today");
    }

    #[test]
    fn a_day_is_read_back_off_the_record() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let reviewed = review_record("f@alpaca-paper", &record(), day).expect("something happened");
        assert_eq!(reviewed.instrument, "AAPL.AIEX");
        // The entry and both exits carried orders to the venue (#231).
        assert_eq!((reviewed.bars, reviewed.signals, reviewed.submitted), (1, 4, 3));
        // Each signal keeps its time and what it was, so a chart can place it.
        assert_eq!(reviewed.signals_at.len(), 4);
        assert_eq!(reviewed.signals_at[0].what, "exit: stop");
        assert!(reviewed.signals_at[0].at.starts_with("2026-09-21T13:36:00"), "{}", reviewed.signals_at[0].at);
        assert_eq!(reviewed.signals_at[1].what, "close above the opening range");
        // The prices the rule decided against ride on its signal.
        let s3 = &reviewed.signals_at[2];
        assert_eq!(s3.levels.iter().map(|level| (level.name.as_str(), level.price)).collect::<Vec<_>>(), [("range high", 101.2), ("target", 102.4)]);
        // Every order of the day, from sending to its end: the entry o3 was
        // sent as a buy, the exits o2 and o4 as sells.
        let order = |id: &str| reviewed.orders.iter().find(|order| order.id == id).unwrap_or_else(|| panic!("order {id}: {:?}", reviewed.orders));
        assert_eq!(order("o3").side, "buy");
        assert!(order("o3").placed_at.starts_with("2026-09-21T14:05:00"), "{}", order("o3").placed_at);
        assert_eq!((order("o2").side.as_str(), order("o4").side.as_str()), ("sell", "sell"));
        assert_eq!(reviewed.unfilled, 1);
        assert_eq!(reviewed.refused.get("Stale"), Some(&1));
        assert_eq!(reviewed.exits.get("stop"), Some(&1));
        assert_eq!(reviewed.fills.len(), 3, "the day's fills only");
        assert_eq!(reviewed.round_trips.len(), 2);
        // Yesterday's buy at 100.1 sold today at 97.9: a loss, on a stop, in
        // the regime the entry signal carried.
        let lost = &reviewed.round_trips[0];
        assert!((lost.pnl + 22.0).abs() < 1e-9, "{}", lost.pnl);
        assert_eq!((lost.rule.as_str(), lost.regime.as_str(), lost.exit_reason.as_str()), ("close above the opening range", "ranging", "stop"));
        let won = &reviewed.round_trips[1];
        assert!((won.pnl - 15.0).abs() < 1e-9);
        assert_eq!(won.regime, "trending up");
        assert!((reviewed.realised + 7.0).abs() < 1e-9);
        let group = reviewed.losses.get("close above the opening range | ranging | stop").expect("grouped");
        assert_eq!(group.count, 1);
        assert_eq!(reviewed.expected_per_trade, Some(12.5));
        assert_eq!(reviewed.assumed_slippage_bps, Some(1.0));
        assert!(reviewed.mean_slippage_bps.unwrap() > 0.0, "the sell at 97.9 against 98 is adverse");
        assert_eq!(reviewed.warnings, vec!["drawdown".to_owned()]);
        assert_eq!(reviewed.verdicts, vec!["diverging (drawdown)".to_owned()]);
        assert_eq!(reviewed.halts, vec!["by hand: enough for today".to_owned()]);
        assert_eq!(reviewed.interventions.len(), 2, "{:?}", reviewed.interventions);
        assert!(reviewed.interventions[0].contains("halted by hand"));

        let yesterday = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let before = review_record("f@alpaca-paper", &record(), yesterday).expect("the buy");
        assert!(before.round_trips.is_empty(), "nothing closed that day");
        assert_eq!(before.fills.len(), 1);
        assert!(review_record("f@alpaca-paper", &record(), NaiveDate::from_ymd_opt(2026, 9, 22).unwrap()).is_none());
    }

    #[test]
    fn the_review_is_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        std::fs::create_dir_all(dir.path().join(SESSIONS)).unwrap();
        std::fs::write(dir.path().join(SESSIONS).join("f@alpaca-paper.jsonl"), record()).unwrap();
        let reviewed = review(dir.path(), day);
        assert_eq!(reviewed.sessions.len(), 1);
        let text = markdown(&reviewed);
        assert!(text.contains("# Review · 2026-09-21"), "{text}");
        assert!(text.contains("Losses, grouped"), "{text}");
        assert!(text.contains("A person: 17:00 halted by hand: enough for today; 17:01 stopped."), "{text}");
        let path = write(dir.path(), &reviewed).unwrap();
        assert!(path.ends_with("reviews/2026-09-21.md") || path.ends_with("reviews\\2026-09-21.md"));
        let (_, again) = read(dir.path(), day).expect("written");
        assert_eq!(again, text);
        let quiet = review(dir.path(), NaiveDate::from_ymd_opt(2026, 9, 25).unwrap());
        assert!(markdown(&quiet).contains("No session had anything to say"));
    }
}
