//! Translating a Pine v5 strategy into a rule (#228).
//!
//! Not to host TradingView's strategies, but to run on them the one thing
//! TradingView's tester cannot: deflation against the author's search, a
//! walk-forward, a re-run under conservative costs, and a paper gate.
//!
//! # What is translated
//!
//! The subset [`crate::rule`] can say, and nothing else. Every other
//! construct is **named** in the refusal rather than skipped, because a
//! silently dropped `request.security` or a dropped short leaves a rule that
//! runs, produces a curve, and is not the strategy anybody wrote.
//!
//! Drawing is the one exception. `plot` and its neighbours have no effect on
//! what a rule trades and appear in nearly every published script; refusing
//! them would refuse nearly every script, and dropping them silently would
//! be the dishonesty above. They are ignored and *listed* as ignored.
//!
//! # What this is not
//!
//! Not a Pine interpreter. It reads a script the way a person reads one
//! looking for the trade, and when it cannot be sure, it refuses. A script
//! that imports is a script whose every line this understood.

use std::collections::BTreeMap;

use crate::rule::{Condition, Indicator, Input, Operand, RuleDefinition, Source, Value};

/// A script read: the rule it describes, and what was ignored on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub rule: RuleDefinition,
    /// The author the script names, as written. Empty when it names none.
    pub author: String,
    /// Lines that do not affect what the rule trades, listed so the person
    /// can see this read them and decided they did not matter.
    pub ignored: Vec<String>,
}

/// Why a script cannot become a rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PineError {
    #[error("this reads Pine v5; the script says {found:?}")]
    Version { found: String },
    #[error("not a strategy: a rule needs strategy() and its entries; an indicator only draws")]
    NotAStrategy,
    #[error("the script never enters: nothing calls strategy.entry")]
    NoEntry,
    /// Everything the reader could not honour, at once: a person rewriting
    /// the script wants the whole list, not the first line of it.
    #[error("{} construct(s) this cannot translate: {}", .constructs.len(), .constructs.join("; "))]
    Unsupported { constructs: Vec<String> },
}

/// Reads a Pine v5 strategy as a rule (#228).
///
/// `interval` is the resolution the rule will run at: Pine takes it from the
/// chart, so it is not in the script and the caller must say.
///
/// # Errors
///
/// [`PineError`], naming every construct that could not be translated.
pub fn translate(text: &str, interval: arvo_data::BarInterval) -> Result<Translated, PineError> {
    let mut reader = Reader::new(interval);
    reader.read(text)?;
    reader.finish()
}

/// What a name in the script stands for, once it has been read. A parameter's
/// default and an indicator's definition live in the reader's own maps, which
/// are what the rule is built from; this only says which kind a name is.
#[derive(Debug, Clone, Copy)]
enum Binding {
    /// An `input.*`: a number a ruleset's grid may vary.
    Param,
    /// A `ta.*` series bound to a name.
    Indicator,
    /// A plain number, folded where it is used.
    Number(f64),
}

/// Constructs that are refused on sight, with what to say about each. Listed
/// rather than inferred so the message names the Pine word a person would
/// search for, not a symptom.
const REFUSED: &[(&str, &str)] = &[
    ("request.security", "request.security: another symbol or timeframe; a rule here runs on one instrument at one interval"),
    ("ta.vwap", "ta.vwap: this build's indicators are SMA, EMA, ATR, RSI, MACD, MAX and MIN"),
    // MACD exists as an indicator, but Pine's call hands back three series at
    // once and a rule here reads one named number per indicator. Declaring it
    // three times in a rule file says the same thing; a tuple does not
    // translate, and guessing which of the three was meant would be worse.
    ("ta.macd", "ta.macd: returns three series from one call; declare MACD in a rule file, once per series you read"),
    ("ta.stoch", "ta.stoch: this build's indicators are SMA, EMA, ATR, RSI, MACD, MAX and MIN"),
    ("ta.bb", "ta.bb: this build's indicators are SMA, EMA, ATR, RSI, MACD, MAX and MIN"),
    ("varip", "varip: state that survives a bar; a rule here is a function of the bars it has seen"),
    ("strategy.order", "strategy.order: only strategy.entry, strategy.close and strategy.exit are read"),
    ("strategy.short", "strategy.short: every rule here is long only"),
    ("strategy.cancel", "strategy.cancel: resting orders are not modelled"),
    ("pyramiding", "pyramiding: a rule here holds one position at a time"),
    ("alert", "alert: an alert is not a trade"),
    ("array.", "arrays: the rule language has no collections"),
    ("matrix.", "matrices: the rule language has no collections"),
    (":=", ":= reassignment: a rule here is a function of the bars it has seen, not a running variable"),
    ("[1]", "a historical reference like [1]: crossings are the way to say \"since the last bar\""),
    ("request.", "request.*: outside data"),
];

/// Pine keywords, matched at the start of a line: a script's title may
/// contain the word "for" and that is not a loop.
const REFUSED_KEYWORDS: &[(&str, &str)] = &[
    ("var ", "var: state that survives a bar; a rule here is a function of the bars it has seen"),
    ("for ", "a loop: a condition is an expression over this bar's values"),
    ("while ", "a loop: a condition is an expression over this bar's values"),
    ("switch", "switch: a condition is an expression over this bar's values"),
];

/// Lines that draw. They change nothing about what is traded.
const DRAWING: &[&str] =
    &["plot", "plotshape", "plotchar", "plotarrow", "plotcandle", "plotbar", "bgcolor", "barcolor", "fill", "hline", "line.", "label.", "table.", "box."];

struct Reader {
    interval: arvo_data::BarInterval,
    title: String,
    author: String,
    bindings: BTreeMap<String, Binding>,
    params: BTreeMap<String, f64>,
    indicators: BTreeMap<String, Indicator>,
    entry: Option<Condition>,
    exit: Option<Condition>,
    unsupported: Vec<String>,
    ignored: Vec<String>,
    is_strategy: bool,
    entered: bool,
}

impl Reader {
    fn new(interval: arvo_data::BarInterval) -> Self {
        Self {
            interval,
            title: String::new(),
            author: String::new(),
            bindings: BTreeMap::new(),
            params: BTreeMap::new(),
            indicators: BTreeMap::new(),
            entry: None,
            exit: None,
            unsupported: Vec::new(),
            ignored: Vec::new(),
            is_strategy: false,
            entered: false,
        }
    }

    fn read(&mut self, text: &str) -> Result<(), PineError> {
        let lines: Vec<&str> = text.lines().collect();
        if let Some(version) = lines.iter().find_map(|line| line.trim().strip_prefix("//@version=")) {
            if version.trim() != "5" {
                return Err(PineError::Version { found: version.trim().to_owned() });
            }
        }
        // The author is a convention, not syntax: published scripts carry it
        // in a leading comment. Taken as written, never guessed.
        if let Some(author) = lines.iter().find_map(|line| {
            let line = line.trim();
            line.strip_prefix("// @author").or_else(|| line.strip_prefix("//@author")).or_else(|| line.strip_prefix("// author:"))
        }) {
            self.author = author.trim_start_matches([':', '=', ' ']).trim().to_owned();
        }

        let mut at = 0;
        while at < lines.len() {
            let line = lines[at];
            let body = strip_comment(line);
            let trimmed = body.trim();
            if trimmed.is_empty() {
                at += 1;
                continue;
            }
            // An `if` owns the lines indented under it.
            if let Some(condition) = trimmed.strip_prefix("if ") {
                let indent = indent_of(line);
                let mut block = Vec::new();
                at += 1;
                while at < lines.len() {
                    let next = lines[at];
                    if strip_comment(next).trim().is_empty() {
                        at += 1;
                        continue;
                    }
                    if indent_of(next) <= indent {
                        break;
                    }
                    block.push(strip_comment(next).trim().to_owned());
                    at += 1;
                }
                self.conditional(condition.trim(), &block);
                continue;
            }
            self.statement(trimmed);
            at += 1;
        }
        Ok(())
    }

    /// One statement outside any `if`.
    fn statement(&mut self, line: &str) {
        if self.refuse(line) {
            return;
        }
        if DRAWING.iter().any(|word| line.starts_with(word)) {
            self.ignored.push(format!("{line}  (drawing; it changes nothing the rule trades)"));
            return;
        }
        if let Some(rest) = line.strip_prefix("strategy(").or_else(|| line.strip_prefix("strategy (")) {
            self.is_strategy = true;
            self.title = first_string(rest).unwrap_or_default();
            // `overlay`, `initial_capital`, `default_qty_*` and the rest are
            // the chart's and the account's, which Arvo's risk model owns.
            self.ignored.push(format!("{line}  (the strategy's own account settings; Arvo's risk model sizes every trade)"));
            return;
        }
        if line.starts_with("indicator(") || line.starts_with("study(") {
            self.ignored.push(format!("{line}  (an indicator declaration)"));
            return;
        }
        // A binding: `name = <expression>`.
        if let Some((name, value)) = split_assignment(line) {
            self.bind(name, value);
            return;
        }
        if line.starts_with("strategy.entry") || line.starts_with("strategy.close") || line.starts_with("strategy.exit") {
            // Outside an `if` it would fire on every bar, which is not a rule
            // anyone means to run.
            self.unsupported.push(format!("{line}: an entry or exit outside an `if` fires on every bar"));
            return;
        }
        self.unsupported.push(format!("{line}: this reader does not know this line"));
    }

    /// `if <condition>` and the block under it.
    fn conditional(&mut self, condition: &str, block: &[String]) {
        if self.refuse(condition) {
            return;
        }
        let parsed = match self.condition(condition) {
            Ok(parsed) => Some(parsed),
            Err(why) => {
                // Read the block anyway: a person rewriting the script wants
                // every line this could not take, not the first one.
                self.unsupported.push(why);
                None
            }
        };
        for line in block {
            if self.refuse(line) {
                continue;
            }
            if DRAWING.iter().any(|word| line.starts_with(word)) {
                self.ignored.push(format!("{line}  (drawing; it changes nothing the rule trades)"));
                continue;
            }
            if line.starts_with("strategy.entry") {
                if line.contains("strategy.short") {
                    self.unsupported.push(format!("{line}: every rule here is long only"));
                    continue;
                }
                if let Some(parsed) = parsed.clone() {
                    self.entered = true;
                    keep(&mut self.entry, parsed);
                }
            } else if line.starts_with("strategy.close") || line.starts_with("strategy.exit") {
                if let Some(parsed) = parsed.clone() {
                    keep(&mut self.exit, parsed);
                }
            } else if let Some((name, value)) = split_assignment(line) {
                // A binding inside a branch is conditional state, which the
                // rule language has no way to say.
                let _ = (name, value);
                self.unsupported.push(format!("{line}: a value assigned inside an `if` is state the rule language cannot hold"));
            } else {
                self.unsupported.push(format!("{line}: this reader does not know this line"));
            }
        }
    }

    /// `name = <expression>`: an input, a `ta.*` call, or a number.
    fn bind(&mut self, name: &str, value: &str) {
        if let Some(default) = input_default(value) {
            self.params.insert(name.to_owned(), default);
            self.bindings.insert(name.to_owned(), Binding::Param);
            return;
        }
        match self.indicator(value) {
            Ok(Some(indicator)) => {
                self.indicators.insert(name.to_owned(), indicator);
                self.bindings.insert(name.to_owned(), Binding::Indicator);
            }
            Ok(None) => match value.trim().parse::<f64>() {
                Ok(number) => {
                    self.bindings.insert(name.to_owned(), Binding::Number(number));
                }
                Err(_) => self.unsupported.push(format!("{name} = {value}: this reader does not know this expression")),
            },
            Err(why) => self.unsupported.push(why),
        }
    }

    /// A `ta.*` call this build can evaluate, or `None` when the expression
    /// is not a call at all.
    fn indicator(&mut self, value: &str) -> Result<Option<Indicator>, String> {
        let value = value.trim();
        let Some((call, args)) = split_call(value) else { return Ok(None) };
        let args = self.arguments(&args);
        let period = |args: &[String], at: usize| -> Result<Value, String> {
            let Some(raw) = args.get(at) else { return Err(format!("{value}: no period")) };
            self.number_or_param(raw).ok_or_else(|| format!("{value}: {raw} is not a number or an input"))
        };
        let input = |args: &[String], at: usize| -> Result<Input, String> {
            match args.get(at).map(String::as_str) {
                None | Some("close") => Ok(Input::Close),
                Some("open") => Ok(Input::Open),
                Some("high") => Ok(Input::High),
                Some("low") => Ok(Input::Low),
                Some("volume") => Ok(Input::Volume),
                Some("hl2" | "hlc3" | "ohlc4") => Err(format!("{value}: a compound source is not one of the bar's fields")),
                Some(other) => Err(format!("{value}: {other} is not one of the bar's fields")),
            }
        };
        match call.as_str() {
            "ta.sma" => Ok(Some(Indicator::Sma { input: input(&args, 0)?, period: period(&args, 1)? })),
            "ta.ema" => Ok(Some(Indicator::Ema { input: input(&args, 0)?, period: period(&args, 1)? })),
            "ta.atr" => Ok(Some(Indicator::Atr { period: period(&args, 0)? })),
            "ta.rsi" => Ok(Some(Indicator::Rsi { input: input(&args, 0)?, period: period(&args, 1)? })),
            "ta.highest" => Ok(Some(Indicator::Max { input: input(&args, 0)?, period: period(&args, 1)? })),
            "ta.lowest" => Ok(Some(Indicator::Min { input: input(&args, 0)?, period: period(&args, 1)? })),
            _ => Ok(None),
        }
    }

    /// A number, or the name of an input.
    fn number_or_param(&self, raw: &str) -> Option<Value> {
        let raw = raw.trim();
        if let Ok(number) = raw.parse::<f64>() {
            return Some(Value::Literal(number));
        }
        match self.bindings.get(raw) {
            Some(Binding::Param) => Some(Value::Param(raw.to_owned())),
            Some(Binding::Number(number)) => Some(Value::Literal(*number)),
            _ => None,
        }
    }

    /// A condition: `ta.crossover(a, b)`, a comparison, or those combined.
    fn condition(&mut self, text: &str) -> Result<Condition, String> {
        let text = text.trim().trim_end_matches(&[' ', '\t'][..]);
        // `or` binds loosest, then `and`, then `not`, then a comparison.
        if let Some((left, right)) = split_operator(text, " or ") {
            return Ok(Condition::Or(vec![self.condition(&left)?, self.condition(&right)?]));
        }
        if let Some((left, right)) = split_operator(text, " and ") {
            return Ok(Condition::And(vec![self.condition(&left)?, self.condition(&right)?]));
        }
        if let Some(rest) = text.strip_prefix("not ") {
            return Ok(Condition::Not(Box::new(self.condition(rest)?)));
        }
        if let Some(inner) = unwrap_parens(text) {
            return self.condition(&inner);
        }
        if let Some((call, args)) = split_call(text) {
            let args = self.arguments(&args);
            let pair = |args: &[String]| -> Result<Vec<Operand>, String> {
                match args {
                    [a, b] => Ok(vec![self.operand(a)?, self.operand(b)?]),
                    _ => Err(format!("{text}: expected two arguments")),
                }
            };
            return match call.as_str() {
                "ta.crossover" => Ok(Condition::CrossAbove(pair(&args)?)),
                "ta.crossunder" => Ok(Condition::CrossBelow(pair(&args)?)),
                "ta.cross" => Err(format!("{text}: ta.cross is either direction; say ta.crossover or ta.crossunder")),
                other => Err(format!("{text}: {other} is not a condition this reader knows")),
            };
        }
        for (symbol, greater) in [(">=", true), ("<=", false), (">", true), ("<", false)] {
            if let Some((left, right)) = split_operator(text, symbol) {
                if symbol.len() == 2 {
                    // The rule language's comparisons are strict; an
                    // inclusive one would quietly change the entry's edge.
                    return Err(format!("{text}: {symbol} is inclusive and the rule language compares strictly; write > or <"));
                }
                let operands = vec![self.operand(&left)?, self.operand(&right)?];
                return Ok(if greater { Condition::Gt(operands) } else { Condition::Lt(operands) });
            }
        }
        Err(format!("{text}: this reader does not know this condition"))
    }

    /// One side of a comparison: an indicator's name, a bar field, an input,
    /// or a number.
    fn operand(&self, text: &str) -> Result<Operand, String> {
        let text = text.trim();
        if let Ok(number) = text.parse::<f64>() {
            return Ok(Operand::Number(number));
        }
        if matches!(text, "open" | "high" | "low" | "close" | "volume") {
            return Ok(Operand::Var { var: text.to_owned() });
        }
        match self.bindings.get(text) {
            Some(Binding::Indicator | Binding::Param) => Ok(Operand::Var { var: text.to_owned() }),
            Some(Binding::Number(number)) => Ok(Operand::Number(*number)),
            None => Err(format!("{text}: nothing in the script gives this a value")),
        }
    }

    /// A call's arguments, with `name=value` reduced to its value: Pine names
    /// arguments and the rule language positions them.
    fn arguments(&self, args: &str) -> Vec<String> {
        split_arguments(args)
            .into_iter()
            .map(|arg| match arg.split_once('=') {
                Some((name, value)) if !name.trim().is_empty() && !value.trim().is_empty() => match name.trim() {
                    "source" | "length" | "src" => value.trim().to_owned(),
                    _ => arg.trim().to_owned(),
                },
                _ => arg.trim().to_owned(),
            })
            .collect()
    }

    /// Whether the line holds something refused on sight; records it if so.
    fn refuse(&mut self, line: &str) -> bool {
        let mut found = false;
        let mut say = |why: &str| {
            let said = why.to_owned();
            if !self.unsupported.contains(&said) {
                self.unsupported.push(said);
            }
        };
        for (needle, why) in REFUSED {
            if line.contains(needle) {
                say(why);
                found = true;
            }
        }
        for (keyword, why) in REFUSED_KEYWORDS {
            if line.trim_start().starts_with(keyword) {
                say(why);
                found = true;
            }
        }
        found
    }

    fn finish(self) -> Result<Translated, PineError> {
        if !self.unsupported.is_empty() {
            return Err(PineError::Unsupported { constructs: self.unsupported });
        }
        if !self.is_strategy {
            return Err(PineError::NotAStrategy);
        }
        if !self.entered || self.entry.is_none() {
            return Err(PineError::NoEntry);
        }
        Ok(Translated {
            rule: RuleDefinition {
                name: slug(&self.title),
                label: self.title.clone(),
                premise: String::new(),
                interval: self.interval,
                params: self.params,
                indicators: self.indicators,
                entry: self.entry.unwrap_or(Condition::And(Vec::new())),
                exit: self.exit,
                source: None,
            },
            author: self.author.clone(),
            ignored: self.ignored,
        })
    }
}

/// Attaches where the script came from, so a finding on it can be traced
/// back to the text it was translated from (#228).
#[must_use]
pub fn attributed(mut translated: Translated, text: &str) -> Translated {
    let author = translated.author.clone();
    translated.rule.source = Some(Source {
        kind: "pine".to_owned(),
        title: translated.rule.label.clone(),
        author,
        hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
    });
    translated
}

/// Two conditions for the same thing: kept as "either fires", which is what
/// two `if` blocks that both enter mean.
fn keep(held: &mut Option<Condition>, next: Condition) {
    *held = Some(match held.take() {
        None => next,
        Some(Condition::Or(mut some)) => {
            some.push(next);
            Condition::Or(some)
        }
        Some(first) => Condition::Or(vec![first, next]),
    });
}

/// A name for the rule, from the script's title.
fn slug(title: &str) -> String {
    let name: String = title
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = name.trim_matches('_').to_owned();
    // Collapse runs, so "Two  SMA (cross)" is not "two__sma__cross_".
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for c in name.chars() {
        if c == '_' {
            if !last_underscore {
                out.push(c);
            }
            last_underscore = true;
        } else {
            out.push(c);
            last_underscore = false;
        }
    }
    if out.is_empty() { "imported".to_owned() } else { out }
}

fn strip_comment(line: &str) -> &str {
    // A `//` inside a string is not a comment; the scripts this reads put
    // strings only in titles, so the first quote wins.
    match (line.find("//"), line.find('"')) {
        (Some(at), Some(quote)) if quote < at => line,
        (Some(at), _) => &line[..at],
        (None, _) => line,
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `name = value`, when the line is an assignment and not a comparison.
fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let at = line.find('=')?;
    if line[at..].starts_with("==") || at == 0 {
        return None;
    }
    let before = line[..at].trim_end();
    if before.ends_with(['>', '<', '!', '=']) {
        return None;
    }
    let name = before.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((name, line[at + 1..].trim()))
}

/// `f(a, b)` as `("f", "a, b")`.
fn split_call(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let open = text.find('(')?;
    if !text.ends_with(')') {
        return None;
    }
    let name = text[..open].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return None;
    }
    Some((name.to_owned(), text[open + 1..text.len() - 1].to_owned()))
}

/// Splits on commas that are not inside brackets or a string.
fn split_arguments(args: &str) -> Vec<String> {
    let (mut out, mut depth, mut quoted, mut current) = (Vec::new(), 0i32, false, String::new());
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '(' | '[' if !quoted => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' if !quoted => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 && !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out.into_iter().map(|arg| arg.trim().to_owned()).collect()
}

/// Splits on `needle` at bracket depth zero, so `f(a > b) > c` splits on the
/// second `>`.
fn split_operator(text: &str, needle: &str) -> Option<(String, String)> {
    let (mut depth, mut quoted) = (0i32, false);
    let bytes: Vec<char> = text.chars().collect();
    for at in 0..bytes.len() {
        match bytes[at] {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth -= 1,
            _ => {}
        }
        if depth != 0 || quoted {
            continue;
        }
        let rest: String = bytes[at..].iter().collect();
        if rest.starts_with(needle) {
            let left: String = bytes[..at].iter().collect();
            let right: String = bytes[at + needle.chars().count()..].iter().collect();
            if left.trim().is_empty() || right.trim().is_empty() {
                continue;
            }
            return Some((left.trim().to_owned(), right.trim().to_owned()));
        }
    }
    None
}

/// `(x)` as `x`, when the parentheses wrap the whole expression.
fn unwrap_parens(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with('(') || !text.ends_with(')') {
        return None;
    }
    let mut depth = 0i32;
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && at != text.len() - 1 {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(text[1..text.len() - 1].to_owned())
}

/// An `input.*` call's default, which is its first argument.
fn input_default(value: &str) -> Option<f64> {
    let (call, args) = split_call(value.trim())?;
    if call != "input" && !call.starts_with("input.") {
        return None;
    }
    split_arguments(&args).first()?.trim().parse().ok()
}

/// The first `"..."` in a call's arguments.
fn first_string(rest: &str) -> Option<String> {
    let open = rest.find('"')?;
    let close = rest[open + 1..].find('"')? + open + 1;
    Some(rest[open + 1..close].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAILY: arvo_data::BarInterval = arvo_data::BarInterval::DAILY;

    /// The classic, as TradingView's own documentation writes it.
    const TWO_SMA: &str = r#"//@version=5
// @author nobody
strategy("Two SMA cross", overlay=true)
fast = input.int(10, "Fast")
slow = input.int(30, "Slow")
fastMa = ta.sma(close, fast)
slowMa = ta.sma(close, slow)
plot(fastMa, color=color.blue)
plot(slowMa, color=color.orange)
if ta.crossover(fastMa, slowMa)
    strategy.entry("long", strategy.long)
if ta.crossunder(fastMa, slowMa)
    strategy.close("long")
"#;

    #[test]
    fn the_classic_two_sma_cross_imports_to_the_control_written_as_data() {
        let translated = translate(TWO_SMA, DAILY).expect("the classic imports");
        let rule = &translated.rule;
        assert_eq!(rule.name, "two_sma_cross");
        assert_eq!(rule.label, "Two SMA cross");
        assert_eq!(rule.interval, DAILY);
        assert_eq!(rule.params, BTreeMap::from([("fast".to_owned(), 10.0), ("slow".to_owned(), 30.0)]));
        assert_eq!(
            rule.indicators,
            BTreeMap::from([
                ("fastMa".to_owned(), Indicator::Sma { input: Input::Close, period: Value::Param("fast".to_owned()) }),
                ("slowMa".to_owned(), Indicator::Sma { input: Input::Close, period: Value::Param("slow".to_owned()) }),
            ])
        );
        assert_eq!(rule.entry.describe(), "fastMa crossed above slowMa");
        assert_eq!(rule.exit.as_ref().expect("an exit").describe(), "fastMa crossed below slowMa");
        // It runs: the defaults resolve and the periods are whole bars.
        let resolved = rule.resolve(&BTreeMap::new()).expect("the imported rule runs");
        assert_eq!(resolved.min_bars(), 30);
        // The drawing was read and set aside, not silently dropped.
        assert_eq!(translated.ignored.len(), 3, "two plots and the strategy declaration: {:?}", translated.ignored);
        assert!(translated.ignored.iter().any(|line| line.starts_with("plot(fastMa")));

        let attributed = attributed(translated, TWO_SMA);
        let source = attributed.rule.source.as_ref().expect("provenance");
        assert_eq!((source.kind.as_str(), source.title.as_str(), source.author.as_str()), ("pine", "Two SMA cross", "nobody"));
        assert_eq!(source.hash, blake3::hash(TWO_SMA.as_bytes()).to_hex().to_string());
    }

#[test]
    fn an_rsi_mean_reversion_script_imports_and_macd_says_why_it_cannot() {
        // The second-commonest shape on TradingView after a moving-average
        // cross, and one this could not read until EMA and RSI existed as
        // indicators. Worth a test of its own: adding an indicator to the rule
        // language does nothing for an import until the translator knows it,
        // and the refusal list said so for three releases.
        let script = r#"//@version=5
strategy("RSI dip", overlay=false)
length = input.int(14, "RSI length")
trend = ta.ema(close, 200)
osc = ta.rsi(close, length)
if osc < 30 and close > trend
    strategy.entry("long", strategy.long)
if osc > 70
    strategy.close("long")
"#;
        let translated = translate(script, DAILY).expect("EMA and RSI translate now");
        assert_eq!(
            translated.rule.indicators,
            BTreeMap::from([
                ("trend".to_owned(), Indicator::Ema { input: Input::Close, period: Value::Literal(200.0) }),
                ("osc".to_owned(), Indicator::Rsi { input: Input::Close, period: Value::Param("length".to_owned()) }),
            ])
        );
        // It runs, and warms on the longest of the two — the EMA at 200.
        let resolved = translated.rule.resolve(&BTreeMap::new()).expect("the imported rule runs");
        assert_eq!(resolved.min_bars(), 200);

        // MACD is an indicator here but not an import: Pine hands back three
        // series from one call and a rule reads one named number per
        // indicator. The refusal has to say that rather than claim MACD is
        // missing, which is what it used to say.
        let macd = "//@version=5\nstrategy(\"M\")\n[m, sig, h] = ta.macd(close, 12, 26, 9)\nif m > sig\n    strategy.entry(\"long\", strategy.long)\n";
        let Err(PineError::Unsupported { constructs }) = translate(macd, DAILY) else {
            panic!("a tuple-returning call is refused");
        };
        let said = constructs.join(" | ");
        assert!(said.contains("three series"), "the reason is the tuple, not a missing indicator: {said}");
        assert!(!said.contains("indicators are"), "it no longer claims MACD is absent: {said}");
    }

    #[test]
    fn every_construct_it_cannot_translate_is_named_at_once() {
        let script = r#"//@version=5
strategy("Everything", overlay=true)
spx = request.security("SPX", "D", close)
k = ta.stoch(close, high, low, 14)
var count = 0
count := count + 1
if k > 70
    strategy.entry("short", strategy.short)
alert("fired")
"#;
        let Err(PineError::Unsupported { constructs }) = translate(script, DAILY) else {
            panic!("a script full of what this cannot say is refused");
        };
        let said = constructs.join(" | ");
        for word in ["request.security", "ta.stoch", "strategy.short", "alert", ":="] {
            assert!(said.contains(word), "{word} is named: {said}");
        }
    }

    #[test]
    fn a_script_that_is_not_a_strategy_or_never_enters_says_which() {
        let indicator = "//@version=5\nindicator(\"Just a line\")\nplot(ta.sma(close, 20))\n";
        assert_eq!(translate(indicator, DAILY).expect_err("an indicator only draws"), PineError::NotAStrategy);

        let quiet = "//@version=5\nstrategy(\"Quiet\")\nfast = ta.sma(close, 10)\nplot(fast)\n";
        assert_eq!(translate(quiet, DAILY).expect_err("it never enters"), PineError::NoEntry);

        let old = "//@version=4\nstrategy(\"Old\")\n";
        assert_eq!(translate(old, DAILY).expect_err("v4"), PineError::Version { found: "4".to_owned() });
    }

    #[test]
    fn comparisons_and_boolean_operators_read_the_way_pine_writes_them() {
        let script = r#"//@version=5
strategy("Bands")
len = input.int(20, "Length")
top = ta.highest(high, len)
floorPrice = ta.lowest(low, len)
if close > top and not (close < floorPrice)
    strategy.entry("long", strategy.long)
if close < floorPrice or close > 500
    strategy.close("long")
"#;
        let translated = translate(script, DAILY).expect("reads");
        assert_eq!(translated.rule.entry.describe(), "close > top and not (close < floorPrice)");
        assert_eq!(translated.rule.exit.as_ref().expect("exit").describe(), "close < floorPrice or close > 500");
        assert_eq!(
            translated.rule.indicators["top"],
            Indicator::Max { input: Input::High, period: Value::Param("len".to_owned()) }
        );
        translated.rule.resolve(&BTreeMap::new()).expect("it runs");
    }

    #[test]
    fn an_inclusive_comparison_is_refused_rather_than_quietly_made_strict() {
        let script = "//@version=5\nstrategy(\"Edge\")\nfast = ta.sma(close, 10)\nif close >= fast\n    strategy.entry(\"long\", strategy.long)\n";
        let Err(PineError::Unsupported { constructs }) = translate(script, DAILY) else {
            panic!(">= changes the entry's edge and is refused");
        };
        assert!(constructs[0].contains(">="), "{constructs:?}");
    }
}
