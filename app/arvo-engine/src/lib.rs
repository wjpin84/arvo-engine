//! Arvo's engine and its local API (ADR-0018, stage 2).
//!
//! The research tier first: what any front end other than the window may do,
//! over gRPC on loopback, behind a token the engine writes to `engine.json`.
//! The MCP server calls the same [`research`] code, so an agent's run, a
//! script's run and a person's run are one run.

pub mod discovery;
pub mod grpc;
pub mod research;
pub mod session;
