//! A strategy expressed as data (#161).
//!
//! A strategy that is a document can be diffed in git, generated in batches,
//! content-addressed, and shipped inside an extension by someone who is not
//! us. The outside user's catalog is 272 execution copies a matrix builder
//! generated from rule registries; Arvo's own families with their parameter
//! grids are the same thing already — a search over data — and are the
//! second implementation ([ADR-0005](../../../https://github.com/wjpin84/arvo-adrs/blob/main/0005-providers-are-earned.md)).
//!
//! # What a document says, and what it does not
//!
//! It names a **kind** and carries that kind's parameters. Two kinds exist:
//! a [`Grid`] over a rule Arvo implements, and [`Rules`] — thresholds over
//! named signals (#160). A fitted model would be a third. Making the
//! threshold form the only form is the overfit this milestone exists to
//! avoid, so the kind is an enum with room in it rather than a shape every
//! strategy has to be bent into.
//!
//! This is a strategy Arvo can **read**: list it, hash it, diff it, expand
//! its grid, and ask its rules what they say about a set of signals. What
//! happens when a strategy needs code Arvo does not have is #125's question,
//! not this one's.
//!
//! # The invariant it inherits
//!
//! A rule over a signal that is absent — not published, or published with no
//! value — is **false**. It comes from [`arvo_data::Signals`] and is not
//! re-implemented here; a rule that reads an indicator nobody computed does
//! not fire.

use std::collections::BTreeMap;

use arvo_data::{BarInterval, SignalName, Signals};
use serde::{Deserialize, Serialize};

use crate::ParameterGrid;

/// A strategy as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyDocument {
    /// The stable name a finding records: `sma_cross`, `their.rsi2-pullback`.
    pub name: String,
    /// What to call it in a menu.
    pub label: String,
    /// One line on what it trades. A name is not a description, and the
    /// difference between two rules is the whole point of having both.
    pub premise: String,
    /// The resolution it is defined at.
    pub interval: BarInterval,
    pub kind: StrategyKind,
}

/// What sort of strategy a document describes.
///
/// Open on purpose. A fitted model, a ranking rule with its own scoring, a
/// schedule — each is a kind, and none of them is a threshold rule with the
/// corners knocked off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategyKind {
    /// A rule Arvo implements, searched over a grid of parameters. Every
    /// strategy Arvo ships is one of these.
    Grid(Grid),
    /// Thresholds over named signals (#160), combined with all-of or
    /// any-of. A rule model of one line, which is what makes a catalog of
    /// them generable.
    Rules(Rules),
}

/// A rule Arvo implements, and the parameters a search varies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    /// The rule the engine knows by this name. Not restated as flags here:
    /// whether it ranks a set or trades an option chain is the engine's
    /// fact, and a second list of those is a second thing to forget to
    /// update.
    pub rule: String,
    /// Parameters every trial shares.
    #[serde(default)]
    pub fixed: BTreeMap<String, f64>,
    /// What the search varies, and over what values.
    #[serde(default)]
    pub axes: BTreeMap<String, Vec<f64>>,
}

impl Grid {
    /// The search this describes, in the form the family runner takes.
    #[must_use]
    pub fn grid(&self) -> ParameterGrid {
        self.axes
            .iter()
            .fold(ParameterGrid::new(), |grid, (name, values)| grid.axis(name, values.clone()))
    }

    /// How many configurations the grid holds. Zero when an axis has no
    /// values, which is a document that tests nothing and should be refused
    /// rather than run.
    #[must_use]
    pub fn configurations(&self) -> usize {
        if self.axes.values().any(Vec::is_empty) {
            return 0;
        }
        self.axes.values().map(Vec::len).product()
    }
}

/// Thresholds over named signals: when to be in, and when to come out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    pub entry: RuleSet,
    /// Empty is legitimate: a strategy that leaves on its stop alone says
    /// nothing here, and says nothing rather than saying "always".
    #[serde(default)]
    pub exit: RuleSet,
}

/// How the rules in a set combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    /// Every rule must hold.
    #[default]
    All,
    /// Any one of them is enough.
    Any,
}

/// A set of rules and how they combine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub combine: Combine,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// Whether this set holds, given what is known right now.
    ///
    /// **An empty set never holds**, whichever way it combines. `All` over
    /// nothing is vacuously true in logic and would be "enter on every bar"
    /// here, which is not what a document with no rules in it meant. A set
    /// that says nothing does not fire, and that reading is safe in both
    /// directions: an empty entry never enters, an empty exit never leaves
    /// on a signal.
    #[must_use]
    pub fn holds(&self, signals: &Signals) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        match self.combine {
            Combine::All => self.rules.iter().all(|rule| rule.holds(signals)),
            Combine::Any => self.rules.iter().any(|rule| rule.holds(signals)),
        }
    }

    /// Every signal this set reads, so a caller can say which are missing
    /// before running anything.
    pub fn reads(&self) -> impl Iterator<Item = &SignalName> {
        self.rules.iter().map(|rule| &rule.signal)
    }
}

/// One threshold over one named signal.
///
/// A minimum, a maximum, and whether each is exclusive. Both bounds absent
/// is a rule that says "this signal is known", which is a real thing to ask
/// and is false when it is not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub signal: SignalName,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// `> min` rather than `>= min`.
    #[serde(default)]
    pub min_exclusive: bool,
    /// `< max` rather than `<= max`.
    #[serde(default)]
    pub max_exclusive: bool,
}

impl Rule {
    /// Whether the rule holds, given what is known right now.
    ///
    /// Asked through [`Signals::satisfies`], so the answer for a signal
    /// nobody published and for one published without a value is the same
    /// answer a missing value gets anywhere else in Arvo: false. Never zero,
    /// never the last known value (#160).
    #[must_use]
    pub fn holds(&self, signals: &Signals) -> bool {
        signals.satisfies(self.signal.as_str(), |value| {
            let above = self.min.is_none_or(|min| if self.min_exclusive { value > min } else { value >= min });
            let below = self.max.is_none_or(|max| if self.max_exclusive { value < max } else { value <= max });
            above && below
        })
    }
}

impl StrategyDocument {
    /// A content address for the document: the same strategy hashes the
    /// same, and any change to what it says is a different strategy.
    ///
    /// Over the document's JSON, which is what is shipped and diffed —
    /// deterministic here because every map in it is ordered and every list
    /// keeps the order it was written in. Including the label and the
    /// premise: a catalog entry whose description changed is a different
    /// entry to a person reading it, and pinning the prose costs nothing.
    #[must_use]
    pub fn version(&self) -> String {
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        blake3::hash(&canonical).to_hex().to_string()
    }

    /// Every signal the document reads, or none for a kind that reads none.
    pub fn reads(&self) -> Vec<&SignalName> {
        match &self.kind {
            StrategyKind::Grid(_) => Vec::new(),
            StrategyKind::Rules(rules) => rules.entry.reads().chain(rules.exit.reads()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> SignalName {
        SignalName::new(name).expect("a name")
    }

    fn rule(signal: &str, min: Option<f64>, max: Option<f64>) -> Rule {
        Rule {
            signal: named(signal),
            min,
            max,
            min_exclusive: false,
            max_exclusive: false,
        }
    }

    fn signals(of: &[(&str, Option<f64>)]) -> Signals {
        let mut signals = Signals::new();
        for (name, value) in of {
            let at = "2024-01-02T00:00:00".parse().expect("a timestamp");
            signals.publish(arvo_data::Signal::new(named(name), at, *value));
        }
        signals
    }

    #[test]
    fn a_rule_over_a_signal_nobody_computed_never_holds() {
        let below_zero = rule("arvo.vwap.deviation", None, Some(-2.0));

        // The whole reason the rule model is worth copying: this is true of
        // 0.0, and 0.0 is what an absent value decays into when it is
        // defaulted somewhere between being computed and being read.
        assert!(!below_zero.holds(&signals(&[])), "never published");
        assert!(!below_zero.holds(&signals(&[("arvo.vwap.deviation", None)])), "published, no value");
        assert!(below_zero.holds(&signals(&[("arvo.vwap.deviation", Some(-2.5))])), "and a real value answers");
    }

    #[test]
    fn a_bound_is_inclusive_unless_the_document_says_otherwise() {
        let at_least_two = rule("x", Some(2.0), None);
        assert!(at_least_two.holds(&signals(&[("x", Some(2.0))])));

        let above_two = Rule { min_exclusive: true, ..rule("x", Some(2.0), None) };
        assert!(!above_two.holds(&signals(&[("x", Some(2.0))])));
        assert!(above_two.holds(&signals(&[("x", Some(2.1))])));

        // Both bounds, as a band.
        let between = rule("x", Some(1.0), Some(3.0));
        assert!(between.holds(&signals(&[("x", Some(2.0))])));
        assert!(!between.holds(&signals(&[("x", Some(3.5))])));

        // Neither bound: a rule that asks only whether the signal is known.
        let known = rule("x", None, None);
        assert!(known.holds(&signals(&[("x", Some(0.0))])), "zero is a value");
        assert!(!known.holds(&signals(&[("x", None)])), "absent is not");
    }

    #[test]
    fn a_set_that_says_nothing_does_not_fire() {
        let known = signals(&[("a", Some(1.0)), ("b", Some(1.0))]);

        // `All` over nothing is vacuously true in logic, and would be "enter
        // on every bar" here.
        assert!(!RuleSet::default().holds(&known), "an empty set, all-of");
        assert!(!RuleSet { combine: Combine::Any, rules: vec![] }.holds(&known), "and any-of");

        let both = RuleSet { combine: Combine::All, rules: vec![rule("a", Some(1.0), None), rule("b", Some(1.0), None)] };
        assert!(both.holds(&known));
        let either = RuleSet { combine: Combine::Any, rules: vec![rule("a", Some(9.0), None), rule("b", Some(1.0), None)] };
        assert!(either.holds(&known), "one of them is enough");

        // And one missing input takes the whole all-of set with it.
        assert!(!both.holds(&signals(&[("a", Some(1.0))])));
        assert!(!either.holds(&signals(&[("a", Some(0.0)), ("b", None)])), "any-of, with nothing to satisfy it");
    }

    #[test]
    fn a_grid_document_is_the_search_it_describes() {
        let document = StrategyDocument {
            name: "sma_cross".to_owned(),
            label: "Moving-average crossover".to_owned(),
            premise: "The control.".to_owned(),
            interval: BarInterval::DAILY,
            kind: StrategyKind::Grid(Grid {
                rule: "sma_cross".to_owned(),
                fixed: BTreeMap::from([("trade_size".to_owned(), 100.0)]),
                axes: BTreeMap::from([
                    ("fast".to_owned(), vec![5.0, 10.0]),
                    ("slow".to_owned(), vec![30.0, 60.0, 120.0]),
                ]),
            }),
        };
        let StrategyKind::Grid(grid) = &document.kind else { panic!("a grid") };
        assert_eq!(grid.configurations(), 6);
        assert_eq!(grid.grid().combinations().len(), 6);
        assert!(document.reads().is_empty(), "a grid reads no signals");

        // An axis with no values is a document that tests nothing.
        let mut empty = grid.clone();
        empty.axes.insert("slow".to_owned(), vec![]);
        assert_eq!(empty.configurations(), 0);
    }

    #[test]
    fn a_document_is_addressed_by_everything_it_says() {
        let document = StrategyDocument {
            name: "their.rsi2-pullback".to_owned(),
            label: "RSI(2) pullback".to_owned(),
            premise: "Buy weakness in an uptrend.".to_owned(),
            interval: BarInterval::DAILY,
            kind: StrategyKind::Rules(Rules {
                entry: RuleSet { combine: Combine::All, rules: vec![rule("arvo.rsi.2", None, Some(10.0))] },
                exit: RuleSet::default(),
            }),
        };
        let version = document.version();
        assert_eq!(version, document.clone().version(), "the same document, the same address");

        let mut retitled = document.clone();
        retitled.label = "RSI(2) pullback, revised".to_owned();
        assert_ne!(version, retitled.version(), "prose is part of what was shipped");

        let mut relaxed = document.clone();
        let StrategyKind::Rules(rules) = &mut relaxed.kind else { panic!("rules") };
        rules.entry.rules[0].max = Some(15.0);
        assert_ne!(version, relaxed.version(), "and so is every threshold");

        // It survives the trip it exists to make: written, diffed, read back.
        let json = serde_json::to_string_pretty(&document).expect("serialises");
        assert!(json.contains("\"kind\": \"rules\""), "the kind is named in the file");
        let read: StrategyDocument = serde_json::from_str(&json).expect("reads back");
        assert_eq!(read.version(), version);
        assert_eq!(read.reads(), vec![&named("arvo.rsi.2")]);
    }
}
