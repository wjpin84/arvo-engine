//! Which sources this build offers.
//!
//! The trait, the ingest pipeline and the cross-source comparison live in
//! [`arvo_data::source`], beside the library they fill. What stays here is the
//! registry — the one thing that has to know every vendor, and therefore the
//! one thing that belongs in the composition root rather than in any crate a
//! vendor depends on.
//!
//! Re-exported so callers name one path. `arvo_runtime_lib::source::Source` and
//! `arvo_data::source::Source` are the same trait.
//!
//! Plugin-served sources (ADR-0022) join the list through [`plug`], which the
//! composition root calls whenever the plugin registry has probed. From then
//! on [`all`] lists them beside the compiled-in ones and nothing downstream
//! can tell which is which.

use std::sync::RwLock;

pub use arvo_data::source::*;
use arvo_plugin_host::source::GrpcSource;

pub use arvo_alpaca as alpaca;
pub use arvo_robinhood as robinhood;
pub use arvo_yfinance as yahoo;

/// Every source the app can fetch from, in the order a menu should list them:
/// the compiled-in ones, then whatever plugins serve.
///
/// A function rather than a registry struct for the compiled-in part: they
/// are known at compile time, and a registry with a map and a registration
/// call would be ceremony around a `vec!`. The plugin part is the registry
/// this comment once said would arrive with `arvo_plugin_host`, and did.
#[must_use]
pub fn all() -> Vec<Box<dyn Source>> {
    let plugged = PLUGGED.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    merged(compiled_in(), plugged.iter().cloned().map(|source| Box::new(source) as Box<dyn Source>).collect())
}

/// The compiled-in sources with the plugged ones merged in.
///
/// A plugged source with an id this build also carries **replaces** it, in
/// place: a source that has moved out to a plugin is served by the plugin
/// when the plugin is up (ADR-0022), and by the build when it is not, and a
/// person sees one Yahoo either way rather than two. An id this build has
/// never heard of is appended.
fn merged(mut sources: Vec<Box<dyn Source>>, plugged: Vec<Box<dyn Source>>) -> Vec<Box<dyn Source>> {
    for plugin in plugged {
        match sources.iter().position(|own| own.id() == plugin.id()) {
            Some(at) => sources[at] = plugin,
            None => sources.push(plugin),
        }
    }
    sources
}

/// The sources plugins serve, as last probed.
///
/// ponytail: process-wide state, because `all()` is called from free
/// functions six layers from anything holding the registry, and threading it
/// through every one of them to avoid a static would be the ceremony the
/// comment above warns about. Becomes state on the app when a second
/// process-wide list appears or a test needs two registries at once.
static PLUGGED: RwLock<Vec<GrpcSource>> = RwLock::new(Vec::new());

/// Replaces the plugin-served sources [`all`] lists. Called by the
/// composition root after every probe of the plugin registry.
pub fn plug(sources: Vec<GrpcSource>) {
    *PLUGGED.write().unwrap_or_else(std::sync::PoisonError::into_inner) = sources;
}

/// The sources this build carries itself.
fn compiled_in() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(robinhood::Robinhood),
        Box::new(yahoo::Yahoo::new()),
        // Both Alpaca feeds, because which one a key is entitled to is not
        // knowable without asking, and the two are different datasets rather
        // than one source configured two ways.
        Box::new(alpaca::Alpaca::iex()),
        Box::new(alpaca::Alpaca::sip()),
        // Total return: each a separate dataset under its own venue, for the
        // same reason — ADR-0013.
        Box::new(yahoo::Yahoo::total_return()),
        Box::new(alpaca::Alpaca::iex_total_return()),
        Box::new(alpaca::Alpaca::sip_total_return()),
    ]
}

/// One source by the id it reports.
///
/// # Errors
///
/// Returns [`SourceError::Unsupported`] naming what is available, rather than
/// `None`: a caller that got `None` would have to invent that message itself,
/// and the call sites would drift.
pub fn by_id(id: &str) -> Result<Box<dyn Source>, SourceError> {
    all()
        .into_iter()
        .find(|source| source.id() == id)
        .ok_or_else(|| {
            let known: Vec<&str> = all().iter().map(|source| source.id()).collect();
            SourceError::Unsupported(format!(
                "{id:?} is not a source this knows; try one of {}",
                known.join(", ")
            ))
        })
}

/// The adjustment basis the stored bars for a set of instruments are on.
///
/// Read from the venue in each instrument id against the registry above, which
/// is the only place that knows it: the basis is a property of the source that
/// wrote the file, and it is not visible in the bars. A total-return series is
/// a different dataset under a different venue, so the venue is enough — see
/// [ADR-0013].
///
/// Two ways this answers `Split` without being told so:
///
/// - **An unknown venue.** A library can hold bars filed under a venue no
///   shipped source claims — an import, or a source since removed. `Split` is
///   what every shipped source is on, and the wrong guess in this direction
///   subtracts a correction that was already made rather than leaving one out.
/// - **A mixed set.** A panel treats its instruments as one dataset, so it gets
///   one basis. `Split` again, for the same reason and in the same direction:
///   the dividend gap is then read as a correction and subtracted, understating
///   the margin on the total-return members rather than overstating it on the
///   split-adjusted ones. An understated edge is the safe error for a figure
///   whose purpose is to stop a result being believed too readily.
///
/// [ADR-0013]: https://github.com/wjpin84/arvo-desktop/blob/master/https://github.com/wjpin84/arvo-adrs/blob/main/0013-dividends-arrive-as-reinvestment.md
#[must_use]
pub fn adjustment_across<'a>(instruments: impl IntoIterator<Item = &'a str>) -> Adjustment {
    let sources = all();
    agreed(instruments.into_iter().map(|instrument| {
        let venue = instrument.split_once('.').map(|(_, venue)| venue)?;
        sources
            .iter()
            .find(|source| source.venue() == venue)
            .map(|source| source.basis().adjustment)
    }))
}

/// The one basis a set of bases is on, or `Split` when they are not on one.
///
/// Split out from [`adjustment_across`] because it is the half that can be
/// wrong, and the mixed and unknown cases need testing without a registry.
fn agreed(bases: impl IntoIterator<Item = Option<Adjustment>>) -> Adjustment {
    let mut bases = bases.into_iter();
    match bases.next().flatten() {
        Some(first) if bases.all(|other| other == Some(first)) => first,
        _ => Adjustment::Split,
    }
}

#[cfg(test)]
mod tests {
    /// A plugin serving an id the build carries takes its place; one the
    /// build has never heard of joins the end. Nobody sees two Yahoos.
    #[test]
    fn a_plugged_source_replaces_its_compiled_in_twin_and_a_new_one_is_appended() {
        use super::{Basis, Feed, Adjustment, Source};

        use super::{Fetched, SourceError};
        use arvo_data::BarInterval;

        struct Own;
        struct Plug(&'static str, &'static str);
        #[async_trait::async_trait]
        impl Source for Own {
            fn id(&self) -> &'static str {
                "yahoo"
            }
            fn label(&self) -> &'static str {
                "Yahoo, compiled in"
            }
            fn venue(&self) -> &'static str {
                "YF"
            }
            fn basis(&self) -> Basis {
                Basis { feed: Feed::Consolidated, adjustment: Adjustment::Split }
            }
            async fn bars(&self, _: &str, _: BarInterval, _: chrono::NaiveDate, _: chrono::NaiveDate) -> Result<Fetched, SourceError> {
                Ok(Fetched::default())
            }
        }
        #[async_trait::async_trait]
        impl Source for Plug {
            fn id(&self) -> &'static str {
                self.0
            }
            fn label(&self) -> &'static str {
                self.1
            }
            fn venue(&self) -> &'static str {
                "YF"
            }
            fn basis(&self) -> Basis {
                Basis { feed: Feed::Consolidated, adjustment: Adjustment::Split }
            }
            async fn bars(&self, _: &str, _: BarInterval, _: chrono::NaiveDate, _: chrono::NaiveDate) -> Result<Fetched, SourceError> {
                Ok(Fetched::default())
            }
        }

        let merged = super::merged(
            vec![Box::new(Own)],
            vec![Box::new(Plug("yahoo", "Yahoo, from the plugin")), Box::new(Plug("polygon", "Polygon"))],
        );
        let labels: Vec<&str> = merged.iter().map(|source| source.label()).collect();
        assert_eq!(labels, ["Yahoo, from the plugin", "Polygon"]);
    }

    use super::*;

    #[test]
    fn a_set_on_one_basis_reports_it() {
        let tr = Some(Adjustment::TotalReturn);
        assert_eq!(agreed([tr, tr, tr]), Adjustment::TotalReturn);
        assert_eq!(
            agreed([Some(Adjustment::Split), Some(Adjustment::Split)]),
            Adjustment::Split
        );
    }

    #[test]
    fn a_mixed_or_unknown_set_falls_back_to_the_basis_that_corrects() {
        // Both fall the same way and for the same reason: subtracting a
        // correction that was already made understates the margin, and leaving
        // one out overstates it. Only one of those errors invites belief.
        assert_eq!(
            agreed([Some(Adjustment::TotalReturn), Some(Adjustment::Split)]),
            Adjustment::Split
        );
        assert_eq!(
            agreed([Some(Adjustment::TotalReturn), None]),
            Adjustment::Split
        );
        assert_eq!(agreed([None]), Adjustment::Split);
        assert_eq!(agreed([]), Adjustment::Split);
    }

    #[test]
    fn an_instrument_with_no_venue_is_not_assumed_to_be_on_any_basis() {
        // A ledger from before instruments were named carries a bare symbol.
        assert_eq!(adjustment_across(["MSFT"]), Adjustment::Split);
        assert_eq!(adjustment_across(["MSFT.RH"]), Adjustment::Split);
    }

    #[test]
    fn the_two_shipped_sources_are_comparable_with_each_other() {
        // The property that keeps `compare_sources` meaningful. If either
        // source ever changes basis — asking Yahoo for `adjclose`, or Alpaca
        // for the free IEX feed — this fails, which is the point: the change
        // would otherwise make every cross-check report a rescaling forever
        // and nobody would know why.
        let broker = robinhood::Robinhood.basis();
        let second = yahoo::Yahoo::new().basis();
        assert!(
            broker.comparable_with(second),
            "{broker:?} against {second:?}"
        );
        assert_eq!(broker.mismatch_with(second), None);
    }

    #[test]
    fn a_total_return_dataset_is_recognised_by_its_venue() {
        // What reaches `DatasetRef::adjustment`, and therefore whether the
        // dividend gap is subtracted or read as composition.
        for source in all() {
            let instrument = format!("AAPL.{}", source.venue());
            assert_eq!(
                adjustment_across([instrument.as_str()]),
                source.basis().adjustment,
                "{}",
                source.id()
            );
        }
        assert_eq!(adjustment_across(["AAPL.YFTR"]), Adjustment::TotalReturn);
        assert_eq!(adjustment_across(["AAPL.ASIPTR"]), Adjustment::TotalReturn);
    }

    #[test]
    fn a_default_window_is_as_deep_as_every_named_source_allows() {
        // #10: it was thirty days of intraday for every source.
        let five_minute = arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);
        let (broker, yahoo, alpaca) =
            (robinhood::Robinhood, yahoo::Yahoo::new(), alpaca::Alpaca::sip());

        assert_eq!(default_days(&[&alpaca], five_minute), 730, "years, where it pages");
        assert_eq!(default_days(&[&broker], five_minute), 84, "under the broker's cap");
        assert_eq!(default_days(&[&yahoo], five_minute), 59, "inside Yahoo's retention");
        assert_eq!(
            default_days(&[&broker, &yahoo], five_minute),
            59,
            "a cross-check fits both, or compares nothing"
        );
        assert_eq!(default_days(&[&broker], arvo_data::BarInterval::DAILY), 3_650);
    }

    #[test]
    fn every_source_says_how_it_is_authorised() {
        // `list_sources` turns this into what a window offers, so a source
        // that answered wrongly would show a sign-in button for a vendor with
        // no sign-in, or no button at all for one that needs a key.
        use std::collections::BTreeMap;
        let declared: BTreeMap<&str, Credential> =
            all().iter().map(|s| (s.id(), s.credential())).collect();

        assert_eq!(declared["robinhood"], Credential::SignIn, "OAuth, a browser");
        assert_eq!(declared["alpaca-iex"], Credential::Keys, "a pair, typed once");
        assert_eq!(declared["alpaca-sip"], Credential::Keys, "the same pair");
        assert_eq!(declared["alpaca-iex-tr"], Credential::Keys, "the same pair");
        assert_eq!(declared["alpaca-sip-tr"], Credential::Keys, "the same pair");
        assert_eq!(declared["yahoo-tr"], Credential::None);
        assert_eq!(
            declared["yahoo"],
            Credential::None,
            "public data — offering a sign-in would offer something that cannot be done"
        );
    }

    #[test]
    fn an_unknown_source_names_the_ones_that_exist() {
        let Err(SourceError::Unsupported(message)) = by_id("bloomberg") else {
            panic!("an unknown id is unsupported");
        };
        assert!(message.contains("robinhood"), "{message}");
        assert!(message.contains("yahoo"), "{message}");
    }

    #[test]
    fn every_source_has_its_own_venue_and_id() {
        // Two sources sharing a venue would file two datasets under one name.
        let sources = all();
        let ids: std::collections::BTreeSet<_> = sources.iter().map(|s| s.id()).collect();
        let venues: std::collections::BTreeSet<_> = sources.iter().map(|s| s.venue()).collect();
        assert_eq!(ids.len(), sources.len(), "ids collide");
        assert_eq!(venues.len(), sources.len(), "venues collide");
    }

    #[test]
    fn the_free_feed_is_not_comparable_with_the_broker() {
        // And says why, rather than reporting Aligned and leaving it there.
        let why = alpaca::Alpaca::iex()
            .basis()
            .mismatch_with(robinhood::Robinhood.basis())
            .expect("a single venue against the tape");
        assert!(why.contains("IEX"), "{why}");
    }

    #[test]
    fn the_paid_feed_is_comparable_with_the_broker() {
        assert_eq!(
            alpaca::Alpaca::sip()
                .basis()
                .mismatch_with(robinhood::Robinhood.basis()),
            None
        );
    }
}
