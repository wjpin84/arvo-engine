//! Loads and probes Arvo plugins.
//!
//! One registry, two execution tiers — gRPC subprocess ([`registry`]) and
//! in-process WASM component ([`wasm`]) — so callers never need to know which
//! tier an entry is on.

pub mod registry;
pub mod wasm;

/// Generated from `protos/arvo/plugin/v1/plugin.proto`.
pub mod plugin {
    tonic::include_proto!("arvo.plugin.v1");
}
