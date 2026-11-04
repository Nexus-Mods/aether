//! `aether-conform`: tests a director against the contract by playing
//! scenarios against it and comparing its answers with the reference's
//! (`aether_refdirector::Reference`).
//!
//! A [`scenario::Scenario`] is a tree to mount ([`tree::Tree`], which lives
//! in `aether-refdirector` so that `Reference::from_tree` can build the
//! reference from it without that crate depending on this one) and a list of
//! steps, read from JSON in `contract/protocol.json`'s names
//! ([`scenario::load`], [`scenario::load_dir`]). A [`compare::Session`]
//! decides, step by step, whether a director's answer is the one expected,
//! allowing only the differences `docs/wire.md` allows.

#![forbid(unsafe_code)]

pub mod compare;
pub mod scenario;

pub use aether_refdirector::tree;
pub use compare::{Mismatch, Session};
pub use scenario::{
    load, load_dir, Expect, LoadError, OwnedDirEntry, OwnedReply, OwnedRequest, Scenario, Step,
};
