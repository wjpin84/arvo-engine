//! A library held in memory, for fixtures.

use std::collections::BTreeMap;

use chrono::NaiveDate;

use crate::{option, Bar, BarInterval, BarProvider, DataError};

/// Bars held in memory.
///
/// The deterministic source: a fixture that cannot fail to read, cannot
/// change between runs, and needs no disk. Used for exercising the research
/// loop end to end and for regression fixtures pinned into evidence.
#[derive(Debug, Default, Clone)]
pub struct InMemoryBars {
    /// Keyed by instrument *and* resolution: the same instrument at five
    /// minutes and at one day is two different series, and returning one when
    /// the other was asked for would be a silent resolution mismatch.
    bars: BTreeMap<(String, String), Vec<Bar>>,
    instruments: std::collections::BTreeSet<String>,
}

impl InMemoryBars {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an instrument's daily history.
    #[must_use]
    pub fn with_instrument(self, instrument: &str, bars: Vec<Bar>) -> Self {
        self.with_interval(instrument, BarInterval::DAILY, bars)
    }

    /// Adds an instrument's history at one resolution, sorted on the way in so
    /// callers need not care about the order they supply.
    #[must_use]
    pub fn with_interval(
        mut self,
        instrument: &str,
        interval: BarInterval,
        mut bars: Vec<Bar>,
    ) -> Self {
        bars.sort_by_key(|bar| bar.at);
        self.instruments.insert(instrument.to_owned());
        self.bars
            .insert((instrument.to_owned(), interval.to_string()), bars);
        self
    }
}

impl BarProvider for InMemoryBars {
    fn source(&self) -> &str {
        "in-memory"
    }

    fn bars(
        &self,
        instrument: &str,
        interval: BarInterval,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        if !self.instruments.contains(instrument) {
            return Err(DataError::UnknownInstrument(instrument.to_owned()));
        }
        // Known instrument, nothing at this resolution: empty rather than
        // unknown, matching the distinction the trait already draws.
        let Some(bars) = self
            .bars
            .get(&(instrument.to_owned(), interval.to_string()))
        else {
            return Ok(Vec::new());
        };

        Ok(bars
            .iter()
            .filter(|bar| bar.at.date() >= from && bar.at.date() <= to)
            .copied()
            .collect())
    }

    fn option_contracts(
        &self,
        underlying: &str,
        interval: BarInterval,
    ) -> Result<Vec<String>, DataError> {
        let spelled = interval.to_string();
        Ok(self
            .bars
            .keys()
            .filter(|(name, at)| {
                *at == spelled
                    && option::OptionContract::parse(name)
                        .is_some_and(|contract| contract.underlying == underlying)
            })
            .map(|(name, _)| name.clone())
            .collect())
    }
}
