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

mod bugs_files;
mod recording;
