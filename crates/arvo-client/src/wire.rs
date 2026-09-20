//! What both ends of the wire agree on beyond the messages themselves.
//!
//! This module once held a pair of functions for every shape the contract
//! did not yet define, converting a hand-written Rust type to its message and
//! back. Every one of those shapes is a message now, so the pairs are gone
//! and the engine, the window and any other front end read the same type.
//! What is left is the one number a client and a server have to agree on and
//! a proto cannot say.

/// The most a message may carry, both ways. A study view holds curves,
/// ledgers and a search surface; tonic's default of four megabytes is not a
/// fit for a finding.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;
