//! The shim against a director that records every request and can be told
//! what to answer (`fakedirector::install_with`): what crosses the ring for
//! each hooked call, what the caches save, and what the shim makes of a
//! reply it cannot use.
//!
//! Each `#[test]` runs in a fresh process (see `common`), so each installs
//! its own ring geometry, faults and hooks. Allocations on a test's own
//! thread are counted (`aether_testkit::alloc`). Add a scenario as a module
//! below.
#![cfg(windows)]

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../fakedirector/mod.rs"]
mod fakedirector;

#[global_allocator]
static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;

// ntdll bindings, all raw calls.
#[allow(unsafe_code)]
#[path = "../ntapi/mod.rs"]
mod ntapi;

// The registry tests' director and hooks, NT key calls and scratch keys; this binary uses
// some of them.
#[allow(unsafe_code, dead_code)]
#[path = "../common/reg.rs"]
mod reg;

mod bugs_files;
mod caches_handles;
mod caches_names;
mod caches_read;
mod caches_registry;
mod child_start;
mod fault_table;
mod fileops;
mod geometry;
mod idle;
mod overlay_answers;
mod reads_large;
mod recording;
mod sections;
