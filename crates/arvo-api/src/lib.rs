//! The contract's types: what the engine is made of, and what a front end renders.
//!
//! # Why this is a crate and not two mirrored files
//!
//! It used to be two. `arvo-runtime` defined these with `Serialize`, the
//! editor redefined all twenty-six of them by hand with `Deserialize`, and
//! nothing checked that the two agreed — a mismatch is invisible to the
//! compiler and appears at runtime as a deserialisation error, if it appears
//! at all.
//!
//! It cost two bugs in one afternoon. `CurvePoint.time` became `i64` on one
//! side and stayed `String` on the other, which broke every study the app
//! could run; and the editor never declared `WalkForwardView.recommendations`
//! at all, so serde quietly ignored the field and it could never have been
//! shown. The second was found only by writing a throwaway script to diff the
//! two files, which is not a thing anyone will remember to do again.
//!
//! One definition removes the class. That is the whole argument.
//!
//! # What belongs here
//!
//! Data, and nothing else. No behaviour, no dependency on the research
//! domain, no knowledge of Tauri or the DOM. A method here would be a place
//! for the two sides to start disagreeing about what a number means, and a
//! dependency here is one the WebAssembly build has to carry.
//!
//! Conversions from domain types live in `arvo-runtime`, as free functions
//! rather than `From` impls — with the types here and the domain there,
//! neither is local to that crate and the orphan rule forbids it. That is the
//! rule doing its job.

/// Generated from the contract in `contract/protos`: every message package,
/// each domain's models and the views a front end renders of them.
///
/// Generated rather than written here: a front end in any language reads the
/// same files, which is what makes the contract a contract rather than a Rust
/// crate two repositories happen to share (#127).
///
/// The module tree mirrors the package names, because that is how a type in
/// one package refers to a type in another. The services are not here;
/// `arvo-client` generates those and points them back at these types.
///
/// The lint allows are for generated code: a oneof over findings has one
/// variant far larger than the others, and that is what the shape is.
#[allow(clippy::large_enum_variant, clippy::doc_markdown, clippy::derive_partial_eq_without_eq)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/arvo.rs"));
}

/// One module per domain, for code that wants to say which it means.
pub use generated::arvo::{
    common::v1 as common, market::v1 as market, platform::v1 as platform,
    portfolio::v1 as portfolio, research::v1 as research, session::v1 as session,
};

/// Every shape at the root as well, because a window names a view on nearly
/// every line and `arvo_api::research::StudyView` would be noise. The names
/// do not collide: proto has no overloading and neither does this.
pub use generated::arvo::{
    common::v1::*, market::v1::*, platform::v1::*, portfolio::v1::*, research::v1::*,
    session::v1::*,
};

/// What the generated shapes cannot say for themselves.
///
/// proto3 makes every message field absent-able, so a nested shape arrives as
/// an `Option` even where it is always sent. These accessors say "the sender
/// always fills this", in one place, instead of at every reader.
mod ergonomics {
    use super::{
        platform::{event_kind_view, plugin_status_view},
        research::record_view,
        EventKindView, EventView, FeedEvent, FindingsEvent, PluginEvent, SessionEvent, SeverityView,
        StreamEvent,
        BookView, ImportView, MetricsView, PanelView, PluginStatusView, PluginStatusViewReachable,
        PluginStatusViewUnreachable, PluginView, PortfolioView, RecordView, ReportedView, StudyView,
        TradesView, WalkForwardView,
    };

    /// A count on the wire. Rust counts in `usize`; the contract is explicit
    /// about width, because a reader in another language has to be.
    #[must_use]
    pub fn count(n: usize) -> u32 {
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// A count read back, for code that indexes with it.
    #[must_use]
    pub fn size(n: u32) -> usize {
        usize::try_from(n).unwrap_or(usize::MAX)
    }

    macro_rules! always {
        ($owner:ty, $field:ident, $ty:ty) => {
            impl $owner {
                /// The sender always fills this.
                ///
                /// # Panics
                ///
                /// Only if it did not, which would mean the two sides disagree
                /// about the contract.
                #[must_use]
                pub fn $field(&self) -> &$ty {
                    self.$field.as_ref().expect(concat!(stringify!($owner), " always carries ", stringify!($field)))
                }
            }
        };
    }
    always!(StudyView, strategy, MetricsView);
    always!(StudyView, benchmark, MetricsView);
    always!(StudyView, trades_detail, TradesView);
    always!(WalkForwardView, strategy, MetricsView);
    always!(WalkForwardView, benchmark, MetricsView);
    always!(WalkForwardView, trades_detail, TradesView);
    always!(BookView, metrics, MetricsView);
    always!(PortfolioView, import, ImportView);

    impl PluginView {
        /// Whether the last probe reached it.
        #[must_use]
        pub fn reachable(&self) -> bool {
            matches!(self.status.as_ref().and_then(|s| s.of.as_ref()), Some(plugin_status_view::Of::Reachable(_)))
        }

        /// What it said it is, if the last probe reached it.
        #[must_use]
        pub fn reached(&self) -> Option<&PluginStatusViewReachable> {
            match self.status.as_ref().and_then(|status| status.of.as_ref()) {
                Some(plugin_status_view::Of::Reachable(status)) => Some(status),
                _ => None,
            }
        }

        /// Why the last probe did not reach it, if it did not.
        ///
        /// A plugin whose status is missing counts as unreached: the window
        /// says so rather than drawing it as healthy.
        #[must_use]
        pub fn unreachable(&self) -> Option<&str> {
            match self.status.as_ref().and_then(|status| status.of.as_ref()) {
                Some(plugin_status_view::Of::Unreachable(status)) => Some(&status.reason),
                Some(plugin_status_view::Of::Reachable(_)) => None,
                None => Some("the engine sent no status"),
            }
        }
    }

    impl PluginStatusView {
        /// It answered, and said what it is.
        #[must_use]
        pub fn reachable(name: String, version: String, capabilities: Vec<String>) -> Self {
            Self {
                of: Some(plugin_status_view::Of::Reachable(PluginStatusViewReachable { name, version, capabilities })),
            }
        }

        /// It did not answer, and why.
        #[must_use]
        pub fn unreachable(reason: String) -> Self {
            Self { of: Some(plugin_status_view::Of::Unreachable(PluginStatusViewUnreachable { reason })) }
        }
    }

    impl EventKindView {
        /// A plugin became reachable, or stopped being.
        #[must_use]
        pub fn plugin(id: String, reachable: bool) -> Self {
            Self { of: Some(event_kind_view::Of::Plugin(PluginEvent { id, reachable })) }
        }

        /// A broker connection came up or went away.
        #[must_use]
        pub fn feed(id: String, connected: bool) -> Self {
            Self { of: Some(event_kind_view::Of::Feed(FeedEvent { id, connected })) }
        }

        /// The live price stream stopped or came back.
        #[must_use]
        pub fn stream(live: bool) -> Self {
            Self { of: Some(event_kind_view::Of::Stream(StreamEvent { live })) }
        }

        /// Stored findings went stale.
        #[must_use]
        pub fn findings(count: u32) -> Self {
            Self { of: Some(event_kind_view::Of::Findings(FindingsEvent { count })) }
        }

        /// A trading session changed state.
        #[must_use]
        pub fn session(id: String, state: String) -> Self {
            Self { of: Some(event_kind_view::Of::Session(SessionEvent { id, state })) }
        }
    }

    impl EventView {
        /// An event, with the kind and the text a renderer shows.
        #[must_use]
        pub fn new(kind: EventKindView, title: String, detail: String, severity: SeverityView) -> Self {
            Self { kind: Some(kind), title, detail, severity: severity as i32 }
        }

        /// What happened, structurally, if the sender said.
        #[must_use]
        pub fn of(&self) -> Option<&event_kind_view::Of> {
            self.kind.as_ref()?.of.as_ref()
        }
    }

    impl RecordView {
        /// A study, as a reopened record.
        #[must_use]
        pub fn study(view: StudyView) -> Self {
            Self { of: Some(record_view::Of::Study(view)) }
        }

        /// A walk-forward, as a reopened record.
        #[must_use]
        pub fn walk_forward(view: WalkForwardView) -> Self {
            Self { of: Some(record_view::Of::Walkforward(view)) }
        }

        /// A panel, as a reopened record.
        #[must_use]
        pub fn panel(view: PanelView) -> Self {
            Self { of: Some(record_view::Of::Panel(view)) }
        }

        /// Evidence Arvo did not compute, as a reopened record.
        #[must_use]
        pub fn reported(view: ReportedView) -> Self {
            Self { of: Some(record_view::Of::Reported(view)) }
        }
    }
}

pub use ergonomics::{count, size};
