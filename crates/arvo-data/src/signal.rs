//! Named values that may be absent, and the one rule that makes them safe
//! to write rules over (#160).
//!
//! A signal is a name, an instant, and a value that may not be there. The
//! invariant is the whole point:
//!
//! > **A predicate over an absent value is false.** Never zero, never true,
//! > never the last known value.
//!
//! The failure this exists to stop is quiet. An indicator with too little
//! history is `None` in the place that computes it and `0.0` by the time a
//! rule reads it, and `0.0` is a value a threshold matches — so a rule
//! meaning "only when the deviation is below −2" fires on every bar of the
//! warm-up, in whatever direction the first few bars happened to go. There
//! is no error, no missing data warning, and a backtest that looks like it
//! found something.
//!
//! [`Signals`] is the namespace, and it is open. A fixed list of indicators
//! as the definition would be choosing one user's instrument set for
//! everyone: Arvo's own indicators publish here, and so does a provider
//! (#163) and a stored series (#164). **A name nobody published reads
//! exactly like a value that is not there** — both are absent, and both
//! satisfy nothing. A rule that reads an indicator nobody computed does not
//! fire, and does not raise.
//!
//! What is deliberately not here: any indicator, any staleness rule (a
//! signal carries when it was computed; deciding how old is too old is the
//! caller's, and #164's), and any rule model over several signals — that is
//! a strategy document (#161), which is built on this.

use std::collections::BTreeMap;
use std::fmt;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// A signal's name: an open, dotted namespace, e.g. `arvo.sma.20`,
/// `regime.trend`, `their-engine.price-behavior`.
///
/// Validated rather than a bare `String` because names arrive over a wire
/// from a provider (#163) and are used as file names by a stored series
/// (#164). The rules are the small ones that keep a name a name: something
/// there, no separators that mean something to a path, no whitespace to be
/// trimmed differently by two readers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SignalName(String);

/// Why a name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a signal name: {1}")]
pub struct BadSignalName(String, &'static str);

impl SignalName {
    /// # Errors
    ///
    /// Empty, or holding anything but letters, digits, `.`, `_` and `-`, or
    /// with an empty dotted segment.
    pub fn new(name: impl Into<String>) -> Result<Self, BadSignalName> {
        let name = name.into();
        let refuse = |why| Err(BadSignalName(name.clone(), why));
        if name.is_empty() {
            return refuse("it is empty");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return refuse("only letters, digits, '.', '_' and '-' are allowed");
        }
        if name.split('.').any(str::is_empty) {
            return refuse("a dotted segment is empty");
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SignalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SignalName {
    type Error = BadSignalName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<SignalName> for String {
    fn from(name: SignalName) -> Self {
        name.0
    }
}

/// One named value, as of one instant, which may not be there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub name: SignalName,
    /// The bar or instant this describes — not when it was computed.
    pub at: NaiveDateTime,
    /// `None` is "not known here", and is the only way to say it. A
    /// warming-up indicator, a provider that has not seen enough of the
    /// session, a classifier that declined: all of them are this.
    pub value: Option<f64>,
}

impl Signal {
    /// A value an indicator produced, absent or not.
    ///
    /// Takes the `Option` an indicator already returns, so the seam between
    /// computing and publishing cannot introduce a default. A non-finite
    /// value is absent: a NaN reaches a rule as "every comparison false",
    /// which is the right answer by accident, and an infinity sorts above
    /// everything, which is the wrong answer on purpose.
    #[must_use]
    pub fn new(name: SignalName, at: NaiveDateTime, value: Option<f64>) -> Self {
        Self {
            name,
            at,
            value: value.filter(|v| v.is_finite()),
        }
    }

    /// The invariant: an absent value satisfies nothing.
    ///
    /// Every question about a signal goes through here rather than through
    /// its `value`, so that "is it below the threshold" cannot be answered
    /// by a number nobody computed.
    #[must_use]
    pub fn satisfies(&self, predicate: impl FnOnce(f64) -> bool) -> bool {
        self.value.is_some_and(predicate)
    }

    /// Known, and at least `floor`. False when absent.
    #[must_use]
    pub fn at_least(&self, floor: f64) -> bool {
        self.satisfies(|value| value >= floor)
    }

    /// Known, and at most `ceiling`. False when absent.
    #[must_use]
    pub fn at_most(&self, ceiling: f64) -> bool {
        self.satisfies(|value| value <= ceiling)
    }
}

/// The namespace: what is known right now, by name.
///
/// Open on purpose. Anything may publish into it, and a reader asking for a
/// name that was never published gets the same answer as one asking for a
/// value that is not there.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    known: BTreeMap<SignalName, Signal>,
}

impl Signals {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes a signal, replacing whatever was last known under that name.
    ///
    /// A signal whose value is absent is *published as absent* rather than
    /// skipped: "the classifier ran and declined to say" and "nothing has
    /// run" are the same to a rule, and keeping the entry is what lets a
    /// reader tell them apart for a person.
    pub fn publish(&mut self, signal: Signal) {
        self.known.insert(signal.name.clone(), signal);
    }

    /// What is held under `name`, published or not.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Signal> {
        self.known.get(name)
    }

    /// The value under `name`. `None` for a name nobody published and for a
    /// name published without a value — a rule cannot tell the two apart,
    /// and must not.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(|signal| signal.value)
    }

    /// The invariant, across the namespace: an unpublished name satisfies
    /// nothing, exactly as an absent value does.
    #[must_use]
    pub fn satisfies(&self, name: &str, predicate: impl FnOnce(f64) -> bool) -> bool {
        self.get(name).is_some_and(|signal| signal.satisfies(predicate))
    }

    /// Every name published, in order.
    pub fn names(&self) -> impl Iterator<Item = &SignalName> {
        self.known.keys()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.known.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

// `BTreeMap<SignalName, _>` looked up by `&str`: the name is a newtype over
// `String`, and borrowing it as one is what keeps every reader from having
// to construct a validated name just to ask a question.
impl std::borrow::Borrow<str> for SignalName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> NaiveDateTime {
        "2024-01-02T00:00:00".parse().expect("a timestamp")
    }

    fn named(name: &str) -> SignalName {
        SignalName::new(name).expect("a name")
    }

    #[test]
    fn a_predicate_over_an_absent_value_is_false_whichever_way_it_points() {
        let absent = Signal::new(named("arvo.sma.20"), at(), None);

        // The trap this type exists to close: every one of these is true of
        // `0.0`, and `0.0` is what an absent value decays into the moment it
        // is unwrapped with a default.
        assert!(!absent.at_most(0.0), "not below a threshold");
        assert!(!absent.at_least(0.0), "not above one either");
        assert!(!absent.satisfies(|v| v == 0.0), "not equal to zero");
        assert!(!absent.satisfies(|_| true), "and not true for a predicate that ignores it");
        assert_eq!(absent.value, None, "the only way to say it is still None");
    }

    #[test]
    fn a_value_that_is_not_a_number_is_not_a_value() {
        for not_a_value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let signal = Signal::new(named("arvo.momentum.60"), at(), Some(not_a_value));
            assert_eq!(signal.value, None, "{not_a_value} is absent, not a number a rule can match");
            assert!(!signal.at_least(f64::MIN), "and it satisfies nothing");
        }
    }

    #[test]
    fn a_name_nobody_published_reads_like_a_value_that_is_not_there() {
        let mut signals = Signals::new();
        signals.publish(Signal::new(named("arvo.rsi.2"), at(), None));

        // The trigger for this ticket: a rule that reads an indicator nobody
        // computed. It does not fire, and it does not raise.
        assert!(!signals.satisfies("arvo.rsi.2", |v| v < 10.0), "published, no value");
        assert!(!signals.satisfies("nobody.computed.this", |v| v < 10.0), "never published");
        assert_eq!(signals.value("nobody.computed.this"), signals.value("arvo.rsi.2"));

        signals.publish(Signal::new(named("arvo.rsi.2"), at(), Some(5.0)));
        assert!(signals.satisfies("arvo.rsi.2", |v| v < 10.0), "and a real value answers");
        assert_eq!(signals.len(), 1, "republishing replaces rather than accumulates");
    }

    #[test]
    fn the_namespace_is_open_but_a_name_is_still_a_name() {
        for good in ["arvo.sma.20", "regime.trend", "their-engine.price-behavior_v2", "x"] {
            assert!(SignalName::new(good).is_ok(), "{good} is a name");
        }
        // Nothing that could be read as a path, a blank, or a segment that
        // is not there: these arrive from a provider over a wire (#163) and
        // become file names in the library (#164).
        for bad in ["", "arvo..sma", ".leading", "trailing.", "with space", "a/b", "../escape"] {
            assert!(SignalName::new(bad).is_err(), "{bad:?} is not");
        }
    }
}
