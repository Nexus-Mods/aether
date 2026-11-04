#![deny(unsafe_code)]

//! `aether-hooks`: installs NT detours that answer every operation on a path under a
//! managed root from the director, over its ring. There is no shim-local answer: without the director's
//! client attached nothing is under a root.

#[macro_use]
mod detour_table;
mod bootstrap;
/// Lock-free record of the hook currently executing, in a shared file.
pub mod breadcrumb;
mod child;
pub mod director;
mod exception_log;
mod handle_tags;
mod hook;
mod hookstats;
mod lazy_section;
pub mod loadlog;
mod ntbuf;
mod ntdef;
#[allow(unsafe_code)]
mod peb;
mod read_cache;
/// The registry hooks' client for the director's registry overlay.
pub mod regclient;
mod regkeys;
mod regnotify;
mod regquery;
mod regwrite;
mod sync;
mod synth_file;
mod synth_section;
mod tramp;
mod whereami;

pub use bootstrap::{bootstrap_failed_content, bootstrap_from_config_path, BootstrapError};
pub use child::{finish_ready_handshake, signal_bootstrap_failed};
// The encoders (and `StaticImport`) live in `aether_protocol::shimcfg` — pure
// byte assembly with no Windows dependency — so a native Linux Director can
// build a shim config too. Re-exported here so every existing caller
// (`aether-inject`'s tests, `aether-hooks/tests/diagnostics/exit_stall_repro.rs`)
// keeps compiling unchanged against `aether_hooks::`.
pub use aether_protocol::shimcfg::{encode_config, encode_config_full, StaticImport};
#[doc(hidden)]
pub use hook::as_shim_io_for_tests;

/// Fill a data section over a director file on demand when it is larger
/// than `bytes` (256 MiB in the shipped shim; 0 restores that), so a test
/// reaches the demand-paged path with a small file. Not in the shipped shim.
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub fn set_eager_section_max(bytes: u64) {
    lazy_section::set_eager_max(bytes);
}
/// Call `observe` with the process handle and id of each child the process
/// hook creates, before the child is injected: what a test reads of the
/// child (its image, say) while it exists. `false` when one is already set.
/// Not in the shipped shim.
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub fn on_child_created(observe: fn(windows_sys::Win32::Foundation::HANDLE, u32)) -> bool {
    hook::CHILD_CREATED.set(observe).is_ok()
}
/// Run one `extern "system"` entry point's body with its panic contained.
///
/// Exported for `aether-shim`, which owns the injected DLL's other
/// `extern "system"` entry point (`DllMain`). An
/// unwind out of any `extern` frame is an immediate `abort()` of the *game*
/// process, so "every entry point of the shim contains its panic" is a property
/// of the DLL rather than of this crate — and it is checked as one, across both
/// crates' sources, by
/// `no_extern_hook_bypasses_the_panic_containment_macro`.
pub use hook::contain_panic;
pub use hook::{
    install, registry_detours_installed, skipped_detours, tracked_handle_count, HookGuard,
    InstallError,
};
/// The under-root open classifier's counters. Exported so a gate's own tests
/// can assert that a bypass class it closed reads **zero** — see
/// [`hookstats::outcome_count`]. A class nobody asserts on is a class that can
/// quietly start (or stop) counting again.
pub use hookstats::{
    child_inject_refused_count, child_inject_refused_total, delete_on_close_refused_count,
    hook_panic_count, hook_panics_total, link_refused_count, outcome_count, reg_notify_count,
    reg_overlay_disabled_by, reg_read_fallback_count, reg_unresolved_count,
    reg_write_refused_count, section_failure_count, unrouted_director_opens, OpenOutcome,
    RegNotify,
};

/// The canonical path the registry hooks recorded for a key handle (synthetic or
/// pass-through), if they track it. For tests and diagnostics.
pub fn registry_handle_path(handle: isize) -> Option<String> {
    regkeys::path_of(handle)
}

/// Live key handles the registry hooks track: (synthetic, pass-through).
pub fn registry_handle_counts() -> (usize, usize) {
    regkeys::counts()
}

/// Key handles with a kept enumeration snapshot (registry query hooks). For tests and
/// diagnostics.
pub fn registry_enum_states() -> usize {
    regquery::states()
}

/// Handles the registry hooks remember as not keys they serve. For tests and diagnostics.
pub fn registry_not_ours_count() -> usize {
    regkeys::not_ours_count()
}

/// Registry change notifications waiting on the overlay. For tests and diagnostics.
pub fn registry_notify_pending() -> usize {
    regnotify::pending()
}

/// Whether a handle value is one of the registry hooks' synthetic key handles.
pub fn is_synthetic_key_handle(handle: isize) -> bool {
    regkeys::is_synthetic(handle)
}

/// The read cache's counters, `None` when it is off. For tests that must know which path a
/// small read took (cached through an immutable handle, or over the ring).
pub fn read_cache_stats_for_tests() -> Option<aether_ipc::CacheStats> {
    read_cache::stats()
}
