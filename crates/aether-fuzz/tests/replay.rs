//! The fuzz bodies on stable Rust, in every CI run: each seed, each crash
//! kept under `fuzz/regressions/`, and random bytes. A panic is the failure.

use proptest::prelude::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Counts what the calling thread allocates while it asks (tests run on
/// several threads), so a body's memory is checked, not only its result.
struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every method forwards to `System` unchanged; the counters are
// thread-local cells without destructors, so counting allocates nothing.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            BYTES.with(|b| b.set(b.get().saturating_add(l.size())));
            CALLS.with(|c| c.set(c.get() + 1));
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if ARMED.with(Cell::get) {
            BYTES.with(|b| b.set(b.get().saturating_add(n)));
            CALLS.with(|c| c.set(c.get() + 1));
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Counting = Counting;

/// What one input may allocate: every body's memory is its input's size
/// times a small factor (a listing, the decoded node, the strings made of
/// them) plus what the body itself builds at a fixed size (a ring, a buffer
/// of at most 64 KiB, a copy of at most 8 KiB). A claim in the input (a count,
/// a length) that bought memory would be far over it.
fn budget(len: usize) -> (usize, usize) {
    (64 * len + (256 << 10), 32 * len + 512)
}

/// Run `target`'s body on `data`, and check what it allocated.
fn bounded(target: &str, data: &[u8]) {
    BYTES.with(|b| b.set(0));
    CALLS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let ran = std::panic::catch_unwind(|| aether_fuzz::run(target, data));
    ARMED.with(|a| a.set(false));
    if let Err(e) = ran {
        std::panic::resume_unwind(e);
    }
    let (bytes, calls) = (BYTES.with(Cell::get), CALLS.with(Cell::get));
    let (max_bytes, max_calls) = budget(data.len());
    assert!(
        bytes <= max_bytes && calls <= max_calls,
        "{target} on {} bytes allocated {bytes} bytes in {calls} calls",
        data.len()
    );
}

#[test]
fn every_seed_runs_through_its_body() {
    let seeds = aether_fuzz::seeds();
    for target in aether_fuzz::TARGETS {
        assert!(
            seeds.iter().any(|(t, _)| *t == target),
            "{target} has no seed"
        );
    }
    for (target, bytes) in &seeds {
        bounded(target, bytes);
    }
}

#[test]
fn every_kept_crash_runs_through_its_body() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/regressions");
    for target in aether_fuzz::TARGETS {
        let Ok(files) = std::fs::read_dir(root.join(target)) else {
            continue;
        };
        for f in files {
            let path = f.expect("read a regressions folder").path();
            let bytes = std::fs::read(&path).expect("read a regression");
            bounded(target, &bytes);
        }
    }
}

/// Seeds cut short or with a byte flipped: the shapes a fuzzer's first
/// mutations reach.
#[test]
fn mutated_seeds_run_through_their_bodies() {
    for (target, bytes) in aether_fuzz::seeds() {
        for cut in 0..=bytes.len() {
            bounded(target, &bytes[..cut]);
        }
        for i in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                b[i] ^= flip;
                bounded(target, &b);
            }
        }
    }
}

/// Seeds with several bytes changed, spliced or grown, from a fixed
/// generator: a few rounds of what a fuzzer does, so the structured inputs
/// (which random bytes seldom reach) are mutated deeper than one byte.
/// `AETHER_FUZZ_ROUNDS` runs more of them.
#[test]
fn mutated_seeds_deeply() {
    let rounds: usize = std::env::var("AETHER_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for (target, seed) in aether_fuzz::seeds() {
        for _ in 0..rounds {
            let mut b = seed.clone();
            for _ in 0..1 + next() % 4 {
                let i = next() as usize % (b.len() + 1);
                match next() % 5 {
                    0 if i < b.len() => b[i] = next() as u8,
                    1 if i < b.len() => {
                        b.remove(i);
                    }
                    2 => b.insert(i, next() as u8),
                    3 if i + 4 <= b.len() => {
                        b[i..i + 4].copy_from_slice(&[0xff, 0xff, 0xff, next() as u8 | 0x80])
                    }
                    _ if i < b.len() => b.truncate(i),
                    _ => {}
                }
            }
            bounded(target, &b);
        }
    }
}

fn any_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..160)
}

macro_rules! random_bytes {
    ($($body:ident),*) => {$(
        proptest! {
            #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]
            #[test]
            fn $body(data in any_bytes()) {
                bounded(stringify!($body), &data);
            }
        }
    )*};
}

random_bytes!(
    reply_decode,
    request_decode,
    shim_cfg,
    ring_response,
    dir_records,
    shim_env
);

/// Inputs built to make a body allocate on a claim: counts and lengths of
/// 0xffffffff in front of a little data, and long runs of separators.
#[test]
fn claims_in_the_input_buy_no_memory() {
    for target in aether_fuzz::TARGETS {
        for fill in [0x00u8, 0x01, 0x3b, 0x3d, 0xff] {
            for len in [0usize, 1, 5, 9, 13, 64, 4096] {
                bounded(target, &vec![fill; len]);
            }
        }
    }
}

/// Every handshake variable the shim parses is named in `contract/env.json`
/// and seeds the target, so a renamed or dropped one fails here.
#[test]
fn every_parsed_handshake_variable_is_in_the_contract_and_seeds_the_target() {
    let contract = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/env.json"),
    )
    .expect("read contract/env.json");
    let names = aether_fuzz::parsed_handshake_names();
    assert!(!names.is_empty());
    for name in &names {
        assert!(contract.contains(&format!("\"{name}\"")), "{name}");
    }
    let seeds = aether_fuzz::seeds()
        .into_iter()
        .filter(|(t, _)| *t == "shim_env")
        .count();
    assert!(
        seeds > names.len(),
        "{seeds} seeds for {} names",
        names.len()
    );
}
