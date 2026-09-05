//! An MCP client, so Arvo can call a remote tool server itself.
//!
//! Arvo already reaches external capabilities two ways — a gRPC subprocess
//! tier and a WASM tier, both for code that runs on this machine. This is the
//! third shape: a *remote* service, spoken over HTTP, that a vendor operates
//! and authenticates. A brokerage that publishes an MCP endpoint is the
//! motivating case, and there is no local process to spawn for it.
//!
//! # Scope
//!
//! Read-shaped protocol plumbing: connect, list what is offered, call a tool.
//! Deliberately not here:
//!
//! * **OAuth.** The token is supplied by the caller. Discovery, dynamic client
//!   registration, PKCE, a loopback listener for the redirect, and refresh are
//!   a substantial piece of work with their own failure modes, and welding
//!   them to the protocol would make both untestable. See the module docs on
//!   [`client`] for the seam.
//! * **Any policy about which tools may be called.** This crate will happily
//!   call whatever it is asked to. Deciding that an order-placing tool is off
//!   limits is a capability decision, and it belongs above this, next to the
//!   secrets and permission systems — not buried in a transport.
//!
//! # Shape
//!
//! [`protocol`] is pure: values in, values out, no I/O, tested exhaustively
//! without a server. [`client`] is the thin HTTP shell around it. The fiddly
//! parts of MCP are all in the framing, so keeping the framing testable
//! without a network is most of the battle.

pub mod client;
pub mod protocol;

pub use client::{ClientError, McpClient};
pub use protocol::{Tool, PROTOCOL_VERSION};
