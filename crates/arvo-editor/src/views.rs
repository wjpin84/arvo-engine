//! The shapes the backend sends.
//!
//! There is nothing here any more, and that is the point. These were
//! twenty-six structs written out by hand to mirror `arvo-runtime`'s, with
//! nothing checking that the two agreed — and a mismatch is invisible to the
//! compiler, appearing at runtime as a deserialisation error if it appears at
//! all.
//!
//! It cost two bugs in one afternoon: `CurvePoint.time` was `i64` on one side
//! and `String` on this one, which broke every study the app could run; and
//! `WalkForwardView.recommendations` was never declared here, so serde ignored
//! the field and it could never have been shown. One definition removes the
//! class.
//!
//! `SessionView` is the exception that stayed: it is written *by* this side as
//! well as read, and its Rust twin is the on-disk format rather than a view.

pub(crate) use arvo_views::*;

use serde::Deserialize;

/// The workspace as it was left.
///
/// `Serialize` too: this goes back out on every layout change.
#[derive(Clone, Default, Deserialize, serde::Serialize)]
pub(crate) struct SessionView {
    /// dockview's own serialisation, as text. See `arvo_runtime::session`
    /// for why it is not a structured value.
    pub(crate) layout: Option<String>,
    pub(crate) active_view: Option<String>,
    pub(crate) output_visible: bool,
    pub(crate) theme: Option<String>,
    pub(crate) strategy: Option<String>,
    /// Saved arrangements, in the order they were made. `default` so a
    /// session written before workspaces existed still opens.
    #[serde(default)]
    pub(crate) workspaces: Vec<WorkspaceView>,
}
