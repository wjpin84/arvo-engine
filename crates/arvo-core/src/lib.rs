//! Platform primitives shared across Arvo.
//!
//! Deliberately generic: nothing here knows about finance, AI, or any
//! particular plugin. Crates that sit above the platform depend on this one,
//! so anything added here is paid for by all of them.
//!
//! Two things a reader might expect here and will not find:
//!
//! * **The plugin registry and its execution tiers** live in
//!   `arvo-plugin-host`, which pulls in `tonic` and `wasmtime`. Keeping them
//!   out means depending on `arvo-core` does not drag a gRPC stack and a WASM
//!   engine along with it.
//! * **The scheduler** lives in `arvo-runtime`, because its only
//!   implementation is bound to `tauri::async_runtime` (deliberately — see the
//!   comment there). Hosting it here would make every dependent crate depend
//!   on Tauri.

pub mod config;
pub mod engine;
pub mod events;
pub mod secrets;
