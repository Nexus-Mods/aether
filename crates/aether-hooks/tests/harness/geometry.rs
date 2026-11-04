//! Every ring geometry and transport a director may choose (gap item S10):
//! the 1 MiB payload cap Cairn's director uses, banks of 1 and 4 MiB, rings
//! of one and two slots, a worker hint below, above and equal to what the
//! ring can serve, the director's wake event, and the file ring that Wine
//! and Proton use.
//!
//! One battery runs against each: an open, a small read, a 2 MiB read, a
//! getattr, a listing of 200 names, a write in three chunks, a rename, a
//! delete and the closes. Every result is checked against what the
//! reference holds, so a geometry the shim cannot use shows as a wrong
//! answer, not only as a hang.
//!
//! The geometry sets what the shim does with it: `payload_cap` the largest
//! inline reply, `arena_len / slots` the bulk bank (an arena under wire.md
//! 2.6's 256 KiB per slot is not used, see `reads_large`), `worker_hint` how
//! many data requests it keeps in flight.

use crate::fakedirector::{self, pattern, Fake, Options, ReadStyle, RingKind, Served};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// Names in the listing at a payload cap that holds them: 200. A listing is
/// one reply that must fit `payload_cap` (wire.md 3.4.2), and 200 entries of
/// these names take 6 804 bytes, so a ring of the default 4096-byte cap
/// lists [`SMALL_ENTRIES`], which take 3 404.
const ENTRIES: usize = 200;
const SMALL_ENTRIES: usize = 100;

/// How many entries the listing has at `payload_cap`.
fn entries(payload_cap: u32) -> usize {
    if payload_cap >= 8192 {
        ENTRIES
    } else {
        SMALL_ENTRIES
    }
}

/// A virtual root on disk (`<tmp>/root`), kept for the life of the test
/// process.
fn root() -> PathBuf {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    root
}

fn name(i: usize) -> String {
    format!("entry{i:03}.dat")
}

/// What the battery works on: read-only `data/` with a small file, a 2 MiB
/// file and `entries` more, and a writable `save/`.
fn fake(entries: usize) -> Fake {
    let mut fake = Fake::new()
        .with_dir("data")
        .with_dir("data/list")
        .with_dir("save")
        .writable_under("save/")
        .with("data/small.txt", pattern(1000), ReadStyle::Whole)
        .with("data/big.bin", pattern(2 * MIB), ReadStyle::Whole);
    for i in 0..entries {
        fake = fake.with(
            &format!("data/list/{}", name(i)),
            vec![i as u8; 3],
            ReadStyle::Whole,
        );
    }
    fake
}

/// Serve the battery's reference on `opts`, install the hooks, run the
/// battery.
fn run(opts: Options) {
    let root = root();
    let n = entries(opts.payload_cap);
    let served = fakedirector::install_with(&root, fake(n), opts);
    std::mem::forget(aether_hooks::install().expect("install"));
    battery(served, &root, n);
}

/// The nine steps, each checked against the reference's contents.
fn battery(served: &Served, root: &Path, entries: usize) {
    let data = root.join("data");
    let save = root.join("save");

    // Open and a small read.
    let mut f = std::fs::File::open(data.join("small.txt")).expect("open small");
    let mut small = vec![0u8; 1000];
    f.read_exact(&mut small).expect("small read");
    assert_eq!(small, pattern(1000), "the small read");
    drop(f);

    // A 2 MiB read.
    let big = std::fs::read(data.join("big.bin")).expect("2 MiB read");
    assert!(
        big == pattern(2 * MIB),
        "the 2 MiB read ({} bytes)",
        big.len()
    );

    // Getattr.
    let meta = std::fs::metadata(data.join("big.bin")).expect("getattr");
    assert_eq!(
        (meta.len(), meta.is_file()),
        (2 * MIB as u64, true),
        "getattr"
    );
    assert!(std::fs::metadata(data.join("list"))
        .expect("getattr dir")
        .is_dir());

    // A listing of 200 entries (100 at a 4096-byte cap, see `ENTRIES`).
    let mut names: Vec<String> = std::fs::read_dir(data.join("list"))
        .expect("readdir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let want: Vec<String> = (0..entries).map(name).collect();
    assert_eq!(names, want, "the listing of {entries} entries");

    // A write in three chunks; the reference holds all of them.
    let chunks: [Vec<u8>; 3] = [vec![1; 5000], vec![2; 70_000], vec![3; 123]];
    let mut f = std::fs::File::create(save.join("out.bin")).expect("create");
    for c in &chunks {
        f.write_all(c).expect("write chunk");
    }
    drop(f);
    let whole: Vec<u8> = chunks.concat();
    assert_eq!(
        served.fake.contents("save/out.bin"),
        Some(whole.clone()),
        "the written bytes"
    );

    // Rename.
    std::fs::rename(save.join("out.bin"), save.join("moved.bin")).expect("rename");
    assert_eq!(served.fake.contents("save/out.bin"), None, "the old name");
    assert_eq!(
        served.fake.contents("save/moved.bin"),
        Some(whole),
        "the new name"
    );
    assert_eq!(
        std::fs::metadata(save.join("moved.bin")).unwrap().len(),
        5000 + 70_000 + 123
    );

    // Delete.
    std::fs::remove_file(save.join("moved.bin")).expect("delete");
    assert_eq!(
        served.fake.contents("save/moved.bin"),
        None,
        "the deleted file"
    );
    assert!(std::fs::metadata(save.join("moved.bin")).is_err());

    // Every open the battery made was closed.
    let files = ["data/small.txt", "data/big.bin", "save/out.bin"];
    let opens: u64 = files.iter().map(|f| served.fake.tally.opens(f)).sum();
    let closes: u64 = files.iter().map(|f| served.fake.tally.closes(f)).sum();
    assert_eq!(opens, closes, "opens and closes");
}

/// `slots` slots with banks of `bank` bytes, payload cap `cap`.
fn ring(slots: u32, cap: u32, bank: usize) -> Options {
    Options {
        slots,
        payload_cap: cap,
        arena_len: slots as usize * bank,
        ..Options::default()
    }
}

#[test]
fn production_geometry_1_mib_cap_32_slots_1_mib_banks() {
    isolate!();
    run(Options {
        worker_hint: 16,
        ..ring(32, MIB as u32, MIB)
    });
}

#[test]
fn banks_of_4_mib() {
    isolate!();
    run(ring(8, MIB as u32, 4 * MIB));
}

#[test]
fn one_slot() {
    isolate!();
    run(ring(1, 4096, 256 * KIB));
}

#[test]
fn two_slots() {
    isolate!();
    run(ring(2, 4096, 256 * KIB));
}

#[test]
fn worker_hint_1() {
    isolate!();
    run(Options {
        worker_hint: 1,
        ..Options::default()
    });
}

#[test]
fn worker_hint_16() {
    isolate!();
    run(Options {
        worker_hint: 16,
        ..ring(32, 4096, 256 * KIB)
    });
}

#[test]
fn worker_hint_above_slot_count() {
    isolate!();
    run(Options {
        worker_hint: 16,
        ..ring(4, 4096, 256 * KIB)
    });
}

/// The serve threads wait on the event only (`Wake::Event`: no polling), so
/// a request is served at once only if the shim signals `VFS_SERVER_EV`. A
/// shim that does not leaves each request waiting for the threads' 1 s
/// safety bound, which the bound below (the battery makes far more than 5
/// requests) would catch.
#[test]
fn the_server_event_wakes_the_director() {
    isolate!();
    let started = Instant::now();
    run(Options {
        ring: RingKind::SectionWithWake,
        ..Options::default()
    });
    assert!(
        std::env::var_os(aether_env::SERVER_EV).is_some(),
        "the event was published"
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "the battery took {took:?}");
}

/// The Wine and Proton transport: the ring is a file (`VFS_RING_PATH`), and
/// a stale `VFS_RING_SECTION` left in the environment does not win over it:
/// no section of that name exists, so a shim that tried it first would not
/// attach.
#[test]
fn a_file_ring_through_the_hooks() {
    isolate!();
    let stale = format!("Local\\aether-shim-stale-{}", std::process::id());
    std::env::set_var(aether_env::RING_SECTION, &stale);
    run(Options {
        ring: RingKind::File,
        ..Options::default()
    });
    assert_eq!(
        std::env::var(aether_env::RING_SECTION).as_deref(),
        Ok(stale.as_str()),
        "the stale section was still set"
    );
    assert!(std::env::var_os(aether_env::RING_PATH).is_some());
}
