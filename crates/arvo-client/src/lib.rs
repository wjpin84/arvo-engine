//! What a front end needs to talk to the engine, and nothing else.
//!
//! The window, the CLI, the MCP server and a script reach the engine over
//! its gRPC API (ADR-0018). This crate is that API's client side: the types
//! generated from `engine.proto`, the wire format the workbench's views
//! travel in, and the error every command answers with.
//!
//! It also holds [`discovery`]: where the engine is and which tokens it serves
//! behind. That is the contract's other half — a front end cannot call an
//! engine it cannot find — and the engine writes the same file this reads.
//!
//! It depends on `arvo-views` and the gRPC stack, and on no engine crate.
//! That is the point: when the window depends on this crate and `arvo-views`
//! alone, the window and the engine can live in different repositories
//! (#127). Anything that would make this crate need `arvo-research`,
//! `arvo-data` or a venue belongs on the other side of the wire.

/// Generated from the contract in `contract/protos`: the models where their
/// subject lives, the services apart from them.
///
/// The module tree mirrors the package names, because that is how a type in
/// one package refers to a type in another. The aliases below are what the
/// rest of this workspace uses.
///
/// The lint allows are for generated code: a oneof over findings has one
/// variant far larger than the others, and that is what the shape is.
#[allow(clippy::large_enum_variant, clippy::doc_markdown, clippy::derive_partial_eq_without_eq)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/arvo.rs"));

    pub use arvo::common::v1 as common;
    pub use arvo::market::v1 as market;
    pub use arvo::platform::v1 as platform;
    pub use arvo::portfolio::v1 as portfolio;
    pub use arvo::research::v1 as research;
    pub use arvo::services::v1 as services;
    pub use arvo::session::v1 as session;
    pub use arvo::views::v1 as views;
}

pub mod discovery;
pub mod wire;

mod error;
pub use error::CommandError;
