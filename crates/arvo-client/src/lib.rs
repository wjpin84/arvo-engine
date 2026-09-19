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

/// Generated from `protos/arvo/engine/v1/engine.proto`: the engine's local
/// API, served by `arvo-engine`.
pub mod proto {
    tonic::include_proto!("arvo.engine.v1");
}

pub mod discovery;
pub mod wire;

mod error;
pub use error::CommandError;
