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
//!
//! The `aether-conform` binary plays scenarios against a director under
//! test: [`adapter`] is the protocol the director's adapter speaks, [`attach`]
//! joins the ring the adapter names as the shim would, [`run`] plays and
//! compares, and [`report`] writes TAP or JSON. [`adapters`] holds the
//! built-in reference adapter. [`malformed`] sends requests that are not
//! valid requests through the raw ring. [`generate`] plays generated cases,
//! which [`model`] makes from proptest values by reading them against an
//! in-process reference, and shrinks the first that fails into a scenario
//! file. [`clients`] sends requests from a slot the caller picks and runs
//! a scenario's other clients as processes of their own; [`concurrent`]
//! plays rounds of generated cases from several clients at once.
//! `docs/conformance.md` is the user's guide.
//!
//! Unsafe is confined to [`job`] (Windows only): the Job Object an adapter
//! runs in and the director's wake event.

#![deny(unsafe_code)]

pub mod adapter;
pub mod adapters;
pub mod attach;
pub mod clients;
pub mod compare;
pub mod concurrent;
pub mod generate;
#[cfg(windows)]
pub mod job;
pub mod malformed;
pub mod model;
pub mod report;
pub mod run;
pub mod scenario;

pub use aether_refdirector::tree;
pub use compare::{Mismatch, Session};
pub use scenario::{
    load, load_dir, Expect, LoadError, OwnedDirEntry, OwnedReply, OwnedRequest, Scenario, Step,
};
