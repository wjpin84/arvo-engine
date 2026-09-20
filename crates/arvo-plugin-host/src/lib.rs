//! Loads and probes Arvo plugins.
//!
//! One tier: a plugin is a process that speaks gRPC at an address, and
//! [`registry`] probes it. A second, in-process WASM tier existed until it was
//! removed — see the note on [`registry::PluginRegistry`] for why a sandbox
//! that grants no imports cannot host the thing plugins are now for.

pub mod registry;
pub mod signal;
pub mod source;
pub mod supervisor;

/// Generated from `extension/proto/arvo/plugin/v1/plugin.proto`.
pub mod plugin {
    tonic::include_proto!("arvo.plugin.v1");
}
