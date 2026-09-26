//! What Arvo does, without a window.
//!
//! The research tier, the project folder, the venue sources, the rulesets and
//! the risk model — everything the window's commands, the headless engine and
//! the MCP server share (#146). The window (`arvo-runtime`) wraps these in
//! Tauri commands; the engine serves them over gRPC. Neither reimplements
//! them, so a script's study is the study a person runs (ADR-0018).
//!
//! Nothing here names a Tauri type. That is the boundary, and the build
//! enforces it: this crate has no `tauri` dependency.

pub mod accounts;
pub mod events;
pub mod extensions;
pub mod plugins;
pub mod portfolio;
pub mod jobs;
pub mod project;
pub mod research;
pub mod review;
pub mod risk;
pub mod rules;
pub mod rulesets;
pub mod scripts;
pub mod source;
pub mod stream;
pub mod universes;

pub use arvo_client::CommandError;

