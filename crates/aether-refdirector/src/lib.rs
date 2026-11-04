//! `aether-refdirector`: the reference and null directors.
//!
//! [`Reference`] answers every opcode from a provider graph and a registry
//! overlay held in memory. It is the server the shim's tests run against
//! (`crates/aether-hooks/tests/fakedirector`) and the oracle a conformance
//! tool compares a director with, so what it answers is fixed: the replies
//! the shim tests' fake director gave before it became this crate. [`Null`]
//! answers every opcode OK with the smallest valid reply, for measuring the
//! ring alone.
//!
//! Both implement [`aether_rpc::Director`]; serve them with
//! [`aether_rpc::serve_with_arena`] (bulk READ needs the arena) or
//! [`aether_rpc::serve`]. Neither depends on an OS.
//!
//! [`tree`] is the tree a conformance scenario mounts and
//! [`Reference::from_tree`] the reference serving it; with the `scenario`
//! feature a [`tree::Tree`] deserialises from a scenario file.

#![forbid(unsafe_code)]

mod null;
mod reference;
pub mod tree;

pub use null::Null;
pub use reference::{key, pattern, ReadStyle, Reference, Tally, BULK_THRESHOLD, MAX_FILE_LEN};
