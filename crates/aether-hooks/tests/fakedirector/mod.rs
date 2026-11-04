//! The shim tests' director: `aether_refdirector::Reference` served over a
//! real named section, the one the shim's own `FuseClient` attaches to
//! through `try_init_from_env` exactly as in production. Only the far side
//! of the ring is in memory.
//!
//! What the director answers lives in `crates/aether-refdirector`. This
//! module creates the section, installs the handshake environment, runs the
//! serve thread with the arena laid out as a director lays it out, and makes
//! the extra clients a few tests need.

// Compiled into several test binaries, each using a different subset.
#![allow(dead_code, unused_imports)]

use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

use aether_ipc::{DataArena, RingServer, SpinNotifier};
use aether_win::SharedMapping;

// The tests know the reference director by its old name here.
pub(crate) use aether_refdirector::{key, pattern, ReadStyle, Reference as Fake, BULK_THRESHOLD};

/// Deliberately small: 4 KiB caps an inline READ response at 4088 bytes, so
/// any fixture bigger than that is guaranteed to span several ring round
/// trips. A test that only ever moved one payload's worth of data would pass
/// against an implementation that reads once and calls it done.
pub(crate) const PAYLOAD_CAP: u32 = 4096;
pub(crate) const SLOTS: u32 = 8;

/// An arena of `SLOTS` x 256 KiB. The bank size the client computes
/// (`arena_len / slot_count`, clamped to at least 256 KiB) then agrees exactly
/// with the one the server hands out, so a bulk read is not silently truncated
/// to a smaller bank and resumed, which would still pass a byte-exactness
/// test while hiding whether the bank sizing was right.
pub(crate) const ARENA_LEN: usize = SLOTS as usize * 256 * 1024;

/// Control ring length, and therefore the arena's offset within the section:
/// the layout a director uses (`arena_offset = ring_bytes`).
fn ring_bytes() -> usize {
    let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
    aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride
}

/// Publish `fake` on a real named section and point the shim's `FuseClient` at
/// it, once per test process. Returns the fake so tests can read its counters.
///
/// The section, the server thread and the fake are all leaked deliberately:
/// they must outlive every test in the binary, and the process is the only
/// thing that can decide when that is.
///
/// `virtual_dir` becomes `VFS_VIRTUAL_DIR`: root 0 of the shim's client. A
/// test that declares more roots sets `VFS_VIRTUAL_ROOTS` before this.
///
/// `arena_len` of 0 forces every READ inline; [`ARENA_LEN`] gives the section
/// a real bulk arena laid out the way a director lays one out, so a
/// request at or above [`BULK_THRESHOLD`] takes the same transport it takes
/// live. Below the threshold reads stay inline either way, so one ring can
/// cover both.
pub(crate) fn install(
    virtual_dir: &std::path::Path,
    fake: Fake,
    arena_len: usize,
) -> &'static Fake {
    static FAKE: OnceLock<&'static Fake> = OnceLock::new();
    FAKE.get_or_init(|| {
        let fake: &'static Fake = Box::leak(Box::new(fake));
        let name = section_name();
        let arena_offset = ring_bytes();
        // Whole section, ring first then arena: `VFS_RING_BYTES` names the
        // whole thing because the shim maps all of it and a bulk response's
        // offset is section-absolute.
        let map_bytes = ((arena_offset + arena_len + 0xFFFF) & !0xFFFF).max(256 * 1024);
        let mapping: &'static SharedMapping = Box::leak(Box::new(
            SharedMapping::create(&name, map_bytes).expect("section"),
        ));
        aether_ipc::ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP).expect("ring init");
        // The registry generation is in the ring header before any client
        // attaches, and each write's is there before its reply leaves.
        fake.publish_generation(move |g| {
            aether_ipc::ring::publish_reg_generation(mapping.seg(), g)
        });

        let mut served = fake.share();
        std::thread::Builder::new()
            .name("fake-director".into())
            .spawn(move || {
                let server = RingServer::new(mapping.seg(), SpinNotifier).expect("ring open");
                // One arena for the life of the thread, banked per slot: a
                // director worker's shape. `banks == slot_count` is what makes
                // the client's `arena_len / slot_count` bank size agree with
                // the server's.
                let arena = (arena_len > 0).then(|| {
                    DataArena::new(mapping.seg(), arena_offset, arena_len, SLOTS as usize)
                });
                // Never set: runs until the ring goes away with the process.
                // A spin, which is fine for a fixture whose whole life is a
                // handful of copy-ups.
                static STOP: AtomicBool = AtomicBool::new(false);
                let _ = aether_rpc::serve_with_arena(&server, arena.as_ref(), &mut served, &STOP);
            })
            .expect("server thread");

        std::env::set_var(aether_env::RING_SECTION, &name);
        std::env::set_var(aether_env::RING_BYTES, map_bytes.to_string());
        std::env::set_var(aether_env::RING_PAYLOAD_CAP, PAYLOAD_CAP.to_string());
        std::env::set_var(aether_env::ARENA_OFFSET, arena_offset.to_string());
        std::env::set_var(aether_env::ARENA_LEN, arena_len.to_string());
        std::env::set_var(aether_env::VIRTUAL_DIR, virtual_dir);
        aether_hooks::director::try_init_from_env().expect("fuse client");
        fake
    })
}

/// The section [`install`] serves.
fn section_name() -> String {
    format!("Local\\aether-shim-cowseed-{}", std::process::id())
}

/// Another client on the ring [`install`] serves, as a second injected process of the session
/// would have: its own mapping of the section, its own `FuseClient`.
pub(crate) fn second_client(virtual_dir: &std::path::Path) -> aether_hooks::director::FuseClient {
    let bytes: usize = std::env::var(aether_env::RING_BYTES)
        .unwrap()
        .parse()
        .unwrap();
    aether_hooks::director::FuseClient::connect(
        &section_name(),
        &[(
            aether_redirect::RootId::DEFAULT,
            virtual_dir.to_string_lossy().into_owned(),
        )],
        PAYLOAD_CAP,
        bytes,
        0,
    )
    .expect("second client")
}

/// A client on a ring nobody serves (a director that died), giving up on each request after
/// `deadline`.
pub(crate) fn unserved_client(
    virtual_dir: &std::path::Path,
    deadline: std::time::Duration,
) -> aether_hooks::director::FuseClient {
    let name = format!("Local\\aether-shim-dead-{}", std::process::id());
    let bytes = 256 * 1024;
    let mapping: &'static SharedMapping = Box::leak(Box::new(
        SharedMapping::create(&name, bytes).expect("section"),
    ));
    aether_ipc::ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP).expect("ring init");
    aether_hooks::director::FuseClient::connect(
        &name,
        &[(
            aether_redirect::RootId::DEFAULT,
            virtual_dir.to_string_lossy().into_owned(),
        )],
        PAYLOAD_CAP,
        bytes,
        0,
    )
    .expect("client on an unserved ring")
    .with_deadline(deadline)
}
