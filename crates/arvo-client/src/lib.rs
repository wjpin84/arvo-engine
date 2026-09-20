//! What a front end needs to talk to the engine, and nothing else.
//!
//! The window, the CLI, the MCP server and a script reach the engine over
//! its gRPC API (ADR-0018). This crate is that API's client side: the stubs
//! generated from the contract's services, the wire format the workbench's
//! views travel in, and the error every command answers with.
//!
//! It also holds [`discovery`]: where the engine is and which tokens it serves
//! behind. That is the contract's other half — a front end cannot call an
//! engine it cannot find — and the engine writes the same file this reads.
//!
//! It depends on `arvo-api` and the gRPC stack, and on no engine crate.
//! That is the point: when the window depends on this crate and `arvo-api`
//! alone, the window and the engine can live in different repositories
//! (#127). Anything that would make this crate need `arvo-research`,
//! `arvo-data` or a venue belongs on the other side of the wire.

/// The contract, as this side of the wire uses it.
///
/// The services are generated here; every type they carry is generated once,
/// in `arvo-api`, and re-exported below so a call site names one path whether
/// it wants a stub or a shape.
pub mod proto {
    /// The stubs, client and server, from `arvo/services/v1`.
    ///
    /// The lint allows are for generated code.
    #[allow(clippy::large_enum_variant, clippy::doc_markdown, clippy::derive_partial_eq_without_eq)]
    pub mod services {
        include!(concat!(env!("OUT_DIR"), "/arvo.services.v1.rs"));
    }

    pub use arvo_api::{common, market, platform, portfolio, research, session};
}

pub mod discovery;
pub mod wire;

mod error;
pub use error::CommandError;
