//! The Source trait over gRPC, in both directions (ADR-0022).
//!
//! `arvo.source.v1.Source` mirrors [`arvo_data::source::Source`] method for
//! method. [`GrpcSource`] is the host's half: the Rust trait implemented over
//! a client, so a plugin-served source goes into the same `Vec<Box<dyn Source>>`
//! as a compiled-in one and nothing downstream knows the difference.
//! [`Served`] is the plugin's half: the generated server trait implemented over
//! that same Rust trait, so a plugin binary is a `main` that builds its sources
//! and calls [`serve`]. The two halves live in one file so the encoding is
//! written once and read from both sides.
//!
//! # Errors cross as status codes
//!
//! One gRPC code per [`SourceError`] variant. The host rebuilds the variant
//! from the code plus what it already knows: which source it asked (the
//! `vendor` every variant carries) and what it asked for (the instrument and
//! interval an `Empty` names). So [`SourceError::needs_sign_in`], the one
//! question every caller asks of an error, survives the boundary.
//!
//! # `&'static str` across a wire
//!
//! The trait names a source with `&'static str` because a compiled-in source
//! is a literal. A described source is a `String` that arrived at runtime, and
//! it is interned rather than leaked per call: a name is leaked once, however
//! many times the plugin is re-described.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use arvo_data::source::{Adjustment, Basis, Credential, Feed, Fetched, Match, Quote, Source, SourceError};
use arvo_data::{Bar, BarInterval, Dividend, IntervalUnit};
use chrono::{NaiveDate, NaiveDateTime};
use tonic::transport::{Channel, Server};
use tonic::{Code, Request, Response, Status};

use crate::plugin::plugin_server::{Plugin, PluginServer};
use crate::plugin::{Capability, GetManifestRequest, Manifest};

/// Generated from `protos/arvo/source/v1/source.proto`.
pub mod v1 {
    tonic::include_proto!("arvo.source.v1");
}

use v1::source_client::SourceClient;
use v1::source_server::{Source as SourceService, SourceServer};

/// The service name a manifest lists to say it serves sources (ADR-0022
/// point 2). A manifest naming it is one the registry may call `Describe` on.
pub const SERVICE: &str = "arvo.source.v1.Source";

/// The wire spelling of a bar's opening time. No zone: [`Bar::at`] is naive.
const AT: &str = "%Y-%m-%dT%H:%M:%S";
/// The wire spelling of a date.
const DATE: &str = "%Y-%m-%d";

/// The intervals a description states its window limits for.
///
/// [`Source::max_days`] is a function of the interval and is synchronous, so
/// it cannot be asked over the wire per call. The plugin answers it up front
/// for the intervals anyone fetches at; an interval not listed has no limit
/// the host will apply, and the source still refuses over its own limit,
/// naming it, exactly as a compiled-in source without a `max_days` does.
const LIMITED_INTERVALS: [BarInterval; 7] = [
    BarInterval::new(1, IntervalUnit::Minute),
    BarInterval::new(5, IntervalUnit::Minute),
    BarInterval::new(15, IntervalUnit::Minute),
    BarInterval::new(30, IntervalUnit::Minute),
    BarInterval::new(1, IntervalUnit::Hour),
    BarInterval::DAILY,
    BarInterval::new(1, IntervalUnit::Week),
];

/// A string that arrived at runtime, made `'static` once.
fn intern(text: &str) -> &'static str {
    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = INTERNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = set.get(text) {
        return found;
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

// ---- encoding, shared by both halves --------------------------------------------

/// A source as the wire describes it.
fn describe(source: &dyn Source) -> v1::Description {
    let basis = source.basis();
    v1::Description {
        id: source.id().to_owned(),
        label: source.label().to_owned(),
        venue: source.venue().to_owned(),
        credential: match source.credential() {
            Credential::None => v1::Credential::None,
            Credential::SignIn => v1::Credential::SignIn,
            Credential::Keys => v1::Credential::Keys,
        } as i32,
        vendor: source.vendor().to_owned(),
        vendor_label: source.vendor_label().to_owned(),
        provides: source.provides().iter().map(|p| (*p).to_owned()).collect(),
        basis: Some(v1::Basis {
            adjustment: match basis.adjustment {
                Adjustment::Split => v1::Adjustment::Split,
                Adjustment::TotalReturn => v1::Adjustment::TotalReturn,
            } as i32,
            single_venue: match basis.feed {
                Feed::Consolidated => None,
                Feed::SingleVenue(venue) => Some(venue.to_owned()),
            },
        }),
        limits: LIMITED_INTERVALS
            .iter()
            .filter_map(|interval| {
                source
                    .max_days(*interval)
                    .map(|days| v1::WindowLimit { interval: interval.to_string(), days })
            })
            .collect(),
    }
}

fn bar_to_wire(bar: &Bar) -> v1::Bar {
    v1::Bar {
        at: bar.at.format(AT).to_string(),
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
        volume: bar.volume,
    }
}

fn bar_from_wire(bar: v1::Bar, vendor: &'static str) -> Result<Bar, SourceError> {
    let at = NaiveDateTime::parse_from_str(&bar.at, AT).map_err(|_| SourceError::Malformed {
        vendor,
        detail: format!("a bar's time {:?} is not {AT}", bar.at),
    })?;
    Ok(Bar {
        at,
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
        volume: bar.volume,
    })
}

fn date_from_wire(text: &str, what: &str) -> Result<NaiveDate, Status> {
    NaiveDate::parse_from_str(text, DATE)
        .map_err(|_| Status::invalid_argument(format!("{what} {text:?} is not a date like 2026-01-31")))
}

/// The status a plugin answers with for each failure, and the message the
/// host will read back. The message is the one field the host cannot rebuild
/// on its own, so each variant sends the part of itself the host lacks.
fn status_from(err: SourceError) -> Status {
    match err {
        SourceError::NoSession { .. } => Status::new(Code::Unauthenticated, err.to_string()),
        SourceError::Unsupported(what) => Status::new(Code::InvalidArgument, what),
        // `what` alone: the host puts its own vendor back in front of it.
        SourceError::Unoffered { what, .. } => Status::new(Code::Unimplemented, what),
        SourceError::Empty { .. } => Status::new(Code::NotFound, err.to_string()),
        SourceError::Malformed { detail, .. } => Status::new(Code::DataLoss, detail),
        SourceError::Transport { detail, .. } => Status::new(Code::Unavailable, detail),
        SourceError::Credential { detail, .. } => Status::new(Code::FailedPrecondition, detail),
        SourceError::Write(_) => Status::new(Code::Internal, err.to_string()),
    }
}

/// The failure a status means, from the host's side. `asked` is the
/// instrument and interval of a bars request, which an `Empty` names and the
/// wire does not carry back.
fn error_from(status: &Status, vendor: &'static str, id: &'static str, asked: Option<(&str, BarInterval)>) -> SourceError {
    let detail = status.message().to_owned();
    match status.code() {
        Code::Unauthenticated => SourceError::NoSession { vendor },
        Code::InvalidArgument => SourceError::Unsupported(detail),
        Code::Unimplemented => SourceError::Unoffered { vendor: id, what: intern(&detail) },
        Code::NotFound => match asked {
            Some((instrument, interval)) => SourceError::Empty {
                vendor: id,
                instrument: instrument.to_owned(),
                interval: interval.to_string(),
            },
            None => SourceError::Unsupported(detail),
        },
        Code::DataLoss => SourceError::Malformed { vendor: id, detail },
        Code::FailedPrecondition => SourceError::Credential { vendor: id, detail },
        // Unavailable, and every code this never sends: the call did not complete.
        _ => SourceError::Transport { vendor: id, detail },
    }
}

// ---- the host's half -------------------------------------------------------------

/// A source a plugin serves, as the rest of Arvo sees it: a `dyn Source`.
///
/// Every declared property was read once at discovery; every fetch is a call.
/// The channel is lazy, so holding one for a plugin that has gone away costs
/// nothing until something asks it for bars, which then fails as
/// [`SourceError::Transport`] like any other unreachable vendor.
///
/// `Clone`, because the registry keeps one and every `source::all()` hands
/// out a fresh box; a channel clone shares the connection.
#[derive(Clone)]
pub struct GrpcSource {
    client: SourceClient<Channel>,
    id: &'static str,
    label: &'static str,
    venue: &'static str,
    vendor: &'static str,
    vendor_label: &'static str,
    provides: &'static [&'static str],
    credential: Credential,
    basis: Basis,
    limits: Vec<(BarInterval, u32)>,
}

impl std::fmt::Debug for GrpcSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcSource").field("id", &self.id).field("venue", &self.venue).finish_non_exhaustive()
    }
}

impl GrpcSource {
    /// Asks the plugin at `address` what it serves. One [`GrpcSource`] per
    /// description, in the order the plugin listed them.
    ///
    /// # Errors
    ///
    /// The address is not a URI, the plugin cannot be reached, or a
    /// description is one the trait would refuse (no basis, no id).
    pub async fn discover(address: &str) -> Result<Vec<Self>, String> {
        let channel = Channel::from_shared(address.to_owned())
            .map_err(|err| format!("{address:?} is not a plugin address: {err}"))?
            .connect_lazy();
        let mut client = SourceClient::new(channel.clone());
        let described = client
            .describe(v1::Empty {})
            .await
            .map_err(|status| format!("{address}: {}", status.message()))?
            .into_inner();
        described
            .sources
            .into_iter()
            .map(|description| Self::from_description(channel.clone(), description))
            .collect()
    }

    fn from_description(channel: Channel, description: v1::Description) -> Result<Self, String> {
        if description.id.is_empty() {
            return Err("a source with no id".to_owned());
        }
        // No default on the host either: the trait gives `basis` none for a
        // reason, and a plugin that forgot it gets refused rather than filed
        // as consolidated and split-adjusted.
        let basis = description
            .basis
            .clone()
            .ok_or_else(|| format!("source {:?} declares no basis", description.id))?;
        let feed = match basis.single_venue.as_deref() {
            None | Some("") => Feed::Consolidated,
            Some(venue) => Feed::SingleVenue(intern(venue)),
        };
        let adjustment = match basis.adjustment() {
            v1::Adjustment::Split => Adjustment::Split,
            v1::Adjustment::TotalReturn => Adjustment::TotalReturn,
        };
        let credential = match description.credential() {
            v1::Credential::None => Credential::None,
            v1::Credential::SignIn => Credential::SignIn,
            v1::Credential::Keys => Credential::Keys,
        };
        let id = intern(&description.id);
        let label = intern(&description.label);
        let vendor = if description.vendor.is_empty() {
            intern(description.id.split('-').next().unwrap_or(&description.id))
        } else {
            intern(&description.vendor)
        };
        let vendor_label = if description.vendor_label.is_empty() { label } else { intern(&description.vendor_label) };
        let provides: Vec<&'static str> = description.provides.iter().map(|p| intern(p)).collect();
        let limits = description
            .limits
            .iter()
            .filter_map(|limit| limit.interval.parse::<BarInterval>().ok().map(|interval| (interval, limit.days)))
            .collect();
        Ok(Self {
            client: SourceClient::new(channel),
            id,
            label,
            venue: intern(&description.venue),
            vendor,
            vendor_label,
            // Leaked once per discovery; a handful of pointers, and discovery
            // happens when a plugin appears, not per call.
            provides: Box::leak(provides.into_boxed_slice()),
            credential,
            basis: Basis { feed, adjustment },
            limits,
        })
    }

    fn error(&self, status: &Status, asked: Option<(&str, BarInterval)>) -> SourceError {
        error_from(status, self.vendor, self.id, asked)
    }
}

#[async_trait::async_trait]
impl Source for GrpcSource {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn venue(&self) -> &'static str {
        self.venue
    }

    fn credential(&self) -> Credential {
        self.credential
    }

    fn vendor(&self) -> &'static str {
        self.vendor
    }

    fn vendor_label(&self) -> &'static str {
        self.vendor_label
    }

    fn provides(&self) -> &'static [&'static str] {
        self.provides
    }

    fn basis(&self) -> Basis {
        self.basis
    }

    fn max_days(&self, interval: BarInterval) -> Option<u32> {
        self.limits.iter().find(|(limited, _)| *limited == interval).map(|(_, days)| *days)
    }

    async fn connected(&self) -> Result<bool, SourceError> {
        let mut client = self.client.clone();
        client
            .connected(v1::SourceId { id: self.id.to_owned() })
            .await
            .map(|reply| reply.into_inner().connected)
            .map_err(|status| self.error(&status, None))
    }

    async fn bars(
        &self,
        symbol: &str,
        interval: BarInterval,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Fetched, SourceError> {
        let mut client = self.client.clone();
        let reply = client
            .bars(v1::BarsRequest {
                id: self.id.to_owned(),
                symbol: symbol.to_owned(),
                interval: interval.to_string(),
                from: from.format(DATE).to_string(),
                to: to.format(DATE).to_string(),
            })
            .await
            .map_err(|status| self.error(&status, Some((symbol, interval))))?
            .into_inner();
        let bars = reply.bars.into_iter().map(|bar| bar_from_wire(bar, self.id)).collect::<Result<Vec<_>, _>>()?;
        Ok(Fetched { bars, interpolated: reply.interpolated as usize })
    }

    async fn dividends(&self, symbol: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Dividend>, SourceError> {
        let mut client = self.client.clone();
        let reply = client
            .dividends(v1::DividendsRequest {
                id: self.id.to_owned(),
                symbol: symbol.to_owned(),
                from: from.format(DATE).to_string(),
                to: to.format(DATE).to_string(),
            })
            .await
            .map_err(|status| self.error(&status, None))?
            .into_inner();
        reply
            .dividends
            .into_iter()
            .map(|dividend| {
                let ex_date = NaiveDate::parse_from_str(&dividend.ex_date, DATE).map_err(|_| SourceError::Malformed {
                    vendor: self.id,
                    detail: format!("a dividend's ex-date {:?} is not {DATE}", dividend.ex_date),
                })?;
                Ok(Dividend { ex_date, amount: dividend.amount })
            })
            .collect()
    }

    async fn search(&self, root: &Path, query: &str, limit: usize) -> Result<Vec<Match>, SourceError> {
        let mut client = self.client.clone();
        let reply = client
            .search(v1::SearchRequest {
                id: self.id.to_owned(),
                library_root: root.to_string_lossy().into_owned(),
                query: query.to_owned(),
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
            })
            .await
            .map_err(|status| self.error(&status, None))?
            .into_inner();
        Ok(reply
            .matches
            .into_iter()
            .map(|found| Match {
                instrument: found.instrument,
                symbol: found.symbol,
                name: found.name,
                price: found.price,
                change: found.change,
            })
            .collect())
    }

    async fn quotes(&self, instruments: &[String]) -> Result<Vec<Quote>, SourceError> {
        let mut client = self.client.clone();
        let reply = client
            .quotes(v1::QuotesRequest { id: self.id.to_owned(), instruments: instruments.to_vec() })
            .await
            .map_err(|status| self.error(&status, None))?
            .into_inner();
        Ok(reply
            .quotes
            .into_iter()
            .map(|quote| Quote { instrument: quote.instrument, price: quote.price, change: quote.change })
            .collect())
    }
}

// ---- the plugin's half -------------------------------------------------------------

struct Inner {
    id: String,
    name: String,
    version: String,
    sources: Vec<Box<dyn Source>>,
}

/// What a plugin binary serves: its manifest, and the sources behind
/// `arvo.source.v1.Source`. Cheap to clone, because tonic wants one owner per
/// service and this serves two.
#[derive(Clone)]
pub struct Served(Arc<Inner>);

impl Served {
    /// `id` and `name` are the manifest's; `version` is the plugin's own crate
    /// version, which is why it is a parameter rather than this crate's.
    #[must_use]
    pub fn new(id: &str, name: &str, version: &str, sources: Vec<Box<dyn Source>>) -> Self {
        Self(Arc::new(Inner { id: id.to_owned(), name: name.to_owned(), version: version.to_owned(), sources }))
    }

    fn find(&self, id: &str) -> Result<&dyn Source, Status> {
        self.0
            .sources
            .iter()
            .find(|source| source.id() == id)
            .map(AsRef::as_ref)
            .ok_or_else(|| Status::not_found(format!("this plugin serves no source called {id:?}")))
    }
}

#[tonic::async_trait]
impl Plugin for Served {
    async fn get_manifest(&self, _request: Request<GetManifestRequest>) -> Result<Response<Manifest>, Status> {
        Ok(Response::new(Manifest {
            id: self.0.id.clone(),
            name: self.0.name.clone(),
            version: self.0.version.clone(),
            api_version: "v1".to_owned(),
            capabilities: vec![Capability {
                name: SERVICE.to_owned(),
                description: format!(
                    "{} source{}: {}",
                    self.0.sources.len(),
                    if self.0.sources.len() == 1 { "" } else { "s" },
                    self.0.sources.iter().map(|s| s.id()).collect::<Vec<_>>().join(", ")
                ),
            }],
        }))
    }
}

#[tonic::async_trait]
impl SourceService for Served {
    async fn describe(&self, _request: Request<v1::Empty>) -> Result<Response<v1::Descriptions>, Status> {
        Ok(Response::new(v1::Descriptions {
            sources: self.0.sources.iter().map(|source| describe(source.as_ref())).collect(),
        }))
    }

    async fn connected(&self, request: Request<v1::SourceId>) -> Result<Response<v1::ConnectedReply>, Status> {
        let source = self.find(&request.into_inner().id)?;
        let connected = source.connected().await.map_err(status_from)?;
        Ok(Response::new(v1::ConnectedReply { connected }))
    }

    async fn bars(&self, request: Request<v1::BarsRequest>) -> Result<Response<v1::BarsReply>, Status> {
        let request = request.into_inner();
        let source = self.find(&request.id)?;
        let interval: BarInterval = request
            .interval
            .parse()
            .map_err(|_| Status::invalid_argument(format!("{:?} is not an interval like 5minute or 1day", request.interval)))?;
        let from = date_from_wire(&request.from, "from")?;
        let to = date_from_wire(&request.to, "to")?;
        let fetched = source.bars(&request.symbol, interval, from, to).await.map_err(status_from)?;
        Ok(Response::new(v1::BarsReply {
            bars: fetched.bars.iter().map(bar_to_wire).collect(),
            interpolated: u32::try_from(fetched.interpolated).unwrap_or(u32::MAX),
        }))
    }

    async fn dividends(&self, request: Request<v1::DividendsRequest>) -> Result<Response<v1::DividendsReply>, Status> {
        let request = request.into_inner();
        let source = self.find(&request.id)?;
        let from = date_from_wire(&request.from, "from")?;
        let to = date_from_wire(&request.to, "to")?;
        let dividends = source.dividends(&request.symbol, from, to).await.map_err(status_from)?;
        Ok(Response::new(v1::DividendsReply {
            dividends: dividends
                .iter()
                .map(|dividend| v1::Dividend { ex_date: dividend.ex_date.format(DATE).to_string(), amount: dividend.amount })
                .collect(),
        }))
    }

    async fn search(&self, request: Request<v1::SearchRequest>) -> Result<Response<v1::Matches>, Status> {
        let request = request.into_inner();
        let source = self.find(&request.id)?;
        let matches = source
            .search(Path::new(&request.library_root), &request.query, request.limit as usize)
            .await
            .map_err(status_from)?;
        Ok(Response::new(v1::Matches {
            matches: matches
                .into_iter()
                .map(|found| v1::Match {
                    instrument: found.instrument,
                    symbol: found.symbol,
                    name: found.name,
                    price: found.price,
                    change: found.change,
                })
                .collect(),
        }))
    }

    async fn quotes(&self, request: Request<v1::QuotesRequest>) -> Result<Response<v1::QuotesReply>, Status> {
        let request = request.into_inner();
        let source = self.find(&request.id)?;
        let quotes = source.quotes(&request.instruments).await.map_err(status_from)?;
        Ok(Response::new(v1::QuotesReply {
            quotes: quotes
                .into_iter()
                .map(|quote| v1::Quote { instrument: quote.instrument, price: quote.price, change: quote.change })
                .collect(),
        }))
    }
}

/// Serves the manifest and the sources at `addr` until the process ends.
///
/// # Errors
///
/// The address cannot be bound, or the server fails while running.
pub async fn serve(addr: SocketAddr, plugin: Served) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(PluginServer::new(plugin.clone()))
        .add_service(SourceServer::new(plugin))
        .serve(addr)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A source with one bar and every declaration set to something a default
    /// would not produce, so the round trip has something to lose.
    struct Fake;

    #[async_trait::async_trait]
    impl Source for Fake {
        fn id(&self) -> &'static str {
            "fake-tr"
        }
        fn label(&self) -> &'static str {
            "Fake (total return)"
        }
        fn venue(&self) -> &'static str {
            "FAKETR"
        }
        fn credential(&self) -> Credential {
            Credential::Keys
        }
        fn vendor_label(&self) -> &'static str {
            "Fake Vendor"
        }
        fn provides(&self) -> &'static [&'static str] {
            &["bars", "quotes"]
        }
        fn basis(&self) -> Basis {
            Basis { feed: Feed::SingleVenue("IEX"), adjustment: Adjustment::TotalReturn }
        }
        fn max_days(&self, interval: BarInterval) -> Option<u32> {
            interval.is_intraday().then_some(60)
        }
        async fn bars(&self, symbol: &str, _: BarInterval, _: NaiveDate, _: NaiveDate) -> Result<Fetched, SourceError> {
            if symbol == "NONE" {
                return Err(SourceError::Empty { vendor: "fake-tr", instrument: symbol.to_owned(), interval: "1day".to_owned() });
            }
            Ok(Fetched {
                bars: vec![Bar {
                    at: NaiveDate::from_ymd_opt(2026, 1, 30).expect("a date").and_hms_opt(9, 30, 0).expect("a time"),
                    open: 1.0,
                    high: 2.0,
                    low: 0.5,
                    close: 1.5,
                    volume: 1000.0,
                }],
                interpolated: 3,
            })
        }
        async fn quotes(&self, instruments: &[String]) -> Result<Vec<Quote>, SourceError> {
            Ok(instruments.iter().map(|i| Quote { instrument: i.clone(), price: 42.0, change: None }).collect())
        }
    }

    fn lazy() -> Channel {
        Channel::from_static("http://127.0.0.1:1").connect_lazy()
    }

    // `connect_lazy` still wants a reactor to hand the channel to.
    #[tokio::test]
    async fn a_description_keeps_every_declaration_through_the_wire() {
        let source = GrpcSource::from_description(lazy(), describe(&Fake)).expect("a described source");
        assert_eq!(source.id(), "fake-tr");
        assert_eq!(source.label(), "Fake (total return)");
        assert_eq!(source.venue(), "FAKETR");
        assert_eq!(source.vendor(), "fake", "derived from the id, as the trait's default does");
        assert_eq!(source.vendor_label(), "Fake Vendor");
        assert_eq!(source.provides(), &["bars", "quotes"]);
        assert_eq!(source.credential(), Credential::Keys);
        assert_eq!(source.basis(), Fake.basis(), "the declaration this whole module exists to carry");
        assert_eq!(source.max_days(BarInterval::new(5, IntervalUnit::Minute)), Some(60));
        assert_eq!(source.max_days(BarInterval::DAILY), None);
    }

    // `connect_lazy` still wants a reactor to hand the channel to.
    #[tokio::test]
    async fn a_source_without_a_basis_is_refused_not_defaulted() {
        let mut description = describe(&Fake);
        description.basis = None;
        assert!(GrpcSource::from_description(lazy(), description).is_err());
    }

    #[test]
    fn every_error_comes_back_as_the_variant_it_left_as() {
        let asked = Some(("SPY", BarInterval::DAILY));
        type Holds = fn(&SourceError) -> bool;
        let cases: Vec<(SourceError, Holds)> = vec![
            (SourceError::NoSession { vendor: "v" }, |e| e.needs_sign_in()),
            (SourceError::Unsupported("weekly".into()), |e| matches!(e, SourceError::Unsupported(w) if w == "weekly")),
            (SourceError::Unoffered { vendor: "v", what: "dividends" }, |e| {
                matches!(e, SourceError::Unoffered { what: "dividends", .. })
            }),
            (SourceError::Empty { vendor: "v", instrument: "SPY".into(), interval: "1day".into() }, |e| {
                matches!(e, SourceError::Empty { instrument, interval, .. } if instrument == "SPY" && interval == "1day")
            }),
            (SourceError::Malformed { vendor: "v", detail: "d".into() }, |e| matches!(e, SourceError::Malformed { detail, .. } if detail == "d")),
            (SourceError::Transport { vendor: "v", detail: "t".into() }, |e| matches!(e, SourceError::Transport { detail, .. } if detail == "t")),
            (SourceError::Credential { vendor: "v", detail: "c".into() }, |e| matches!(e, SourceError::Credential { detail, .. } if detail == "c")),
        ];
        for (sent, holds) in cases {
            let said = sent.to_string();
            let back = error_from(&status_from(sent), "vendor", "id", asked);
            assert!(holds(&back), "{said:?} came back as {back:?}");
        }
        // A status this never sends is a call that did not complete.
        assert!(matches!(
            error_from(&Status::deadline_exceeded("slow"), "vendor", "id", None),
            SourceError::Transport { .. }
        ));
    }

    async fn wait_until_serving(address: &str) {
        for _ in 0..50 {
            if SourceClient::connect(address.to_owned()).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the plugin never came up at {address}");
    }

    #[tokio::test]
    async fn a_served_source_is_a_source_on_the_other_side() {
        let addr: SocketAddr = "127.0.0.1:50071".parse().expect("an address");
        let plugin = Served::new("fake", "Fake Plugin", "9.9.9", vec![Box::new(Fake)]);
        tokio::spawn(serve(addr, plugin));
        wait_until_serving("http://127.0.0.1:50071").await;

        let found = GrpcSource::discover("http://127.0.0.1:50071").await.expect("discovered");
        assert_eq!(found.len(), 1);
        let source = &found[0];
        assert_eq!(source.id(), "fake-tr");
        assert_eq!(source.basis(), Fake.basis());
        assert!(source.connected().await.expect("asked"), "the trait's default, crossed");

        let day = NaiveDate::from_ymd_opt(2026, 1, 30).expect("a date");
        let fetched = source.bars("SPY", BarInterval::DAILY, day, day).await.expect("bars");
        assert_eq!(fetched.interpolated, 3);
        assert_eq!(fetched.bars.len(), 1);
        assert_eq!(fetched.bars[0].at.format(AT).to_string(), "2026-01-30T09:30:00", "the opening time, to the second");
        assert_eq!(fetched.bars[0].close, 1.5);

        let empty = source.bars("NONE", BarInterval::DAILY, day, day).await.expect_err("nothing there");
        assert!(matches!(empty, SourceError::Empty { instrument, .. } if instrument == "NONE"));

        let unoffered = source.dividends("SPY", day, day).await.expect_err("the trait's default");
        assert!(matches!(unoffered, SourceError::Unoffered { what: "dividends", .. }), "{unoffered:?}");

        let quotes = source.quotes(&["SPY.X".to_owned()]).await.expect("quotes");
        assert_eq!(quotes[0].price, 42.0);

        // The manifest names the service, which is what the registry reads.
        let mut plugin = crate::plugin::plugin_client::PluginClient::connect("http://127.0.0.1:50071".to_owned())
            .await
            .expect("connect");
        let manifest = plugin.get_manifest(GetManifestRequest {}).await.expect("manifest").into_inner();
        assert_eq!(manifest.version, "9.9.9", "the plugin's version, not this crate's");
        assert!(manifest.capabilities.iter().any(|c| c.name == SERVICE));
    }
}
