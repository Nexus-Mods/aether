//! The fuzz bodies on stable Rust, in every CI run: each seed, each crash
//! kept under `fuzz/regressions/`, and random bytes. A panic is the failure.

use proptest::prelude::*;

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
        aether_fuzz::run(target, bytes);
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
            aether_fuzz::run(target, &bytes);
        }
    }
}

/// Seeds cut short or with a byte flipped: the shapes a fuzzer's first
/// mutations reach.
#[test]
fn mutated_seeds_run_through_their_bodies() {
    for (target, bytes) in aether_fuzz::seeds() {
        for cut in 0..=bytes.len() {
            aether_fuzz::run(target, &bytes[..cut]);
        }
        for i in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                b[i] ^= flip;
                aether_fuzz::run(target, &b);
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
            aether_fuzz::run(target, &b);
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
                aether_fuzz::$body(&data);
            }
        }
    )*};
}

random_bytes!(
    reply_decode,
    request_decode,
    shim_cfg,
    ring_response,
    dir_records
);
