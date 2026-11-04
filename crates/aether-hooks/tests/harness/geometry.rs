//! Every ring geometry and transport a director may choose (wire.md 2.7):
//! the 1 MiB payload cap a production director uses, banks of 1 and 4 MiB, rings
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

use crate::fakedirector::{
    self, pattern, Fake, Fault, Match, Options, ReadStyle, RingKind, Served,
};
use aether_protocol::Opcode;
use aether_refdirector::harness::MAX_DELAY;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How long a test waits for an event it expects (a held request reaching
/// the director, a stat answered, the battery ending) before it calls the
/// run hung. Only a bound: each wait ends on its event.
const WAIT: Duration = Duration::from_secs(30);

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
fn run(opts: Options) -> &'static Served {
    let root = root();
    let n = entries(opts.payload_cap);
    let (slots, hint) = (opts.slots, opts.worker_hint);
    let served = fakedirector::install_with(&root, fake(n), opts);
    std::mem::forget(aether_hooks::install().expect("install"));
    battery(served, &root, n);
    concurrent(served, &root, slots, hint);
    served
}

/// The director's thread count, as the fake serves it: the ring's
/// `worker_hint` (4 when 0), at most one per slot.
fn workers(hint: u32, slots: u32) -> u32 {
    (if hint == 0 { 4 } else { hint }).min(slots).max(1)
}

/// The data requests the shim keeps in flight, from wire.md 2.8's formula
/// (written out here, not taken from the shim's `data_limit`).
fn limit(hint: u32, slots: u32) -> u32 {
    let w = workers(hint, slots);
    let reserve = (w / 4).max(1);
    w.saturating_sub(reserve).max(1)
}

/// Threads read the 2 MiB file while the director holds every READ, so the
/// data requests in flight reach what the shim allows and stay there:
///
/// - `limit` requests reach the director at once (wire.md 2.8), on `limit`
///   different slots, so the arena banks and slot stride past slot 0 are
///   used, and the bytes every thread reads are exact;
/// - when the director has a thread beyond those (the reserve), a getattr
///   and a listing are answered while every data request is held. A shim
///   that ignored the hint or the slot cap would have put a READ on that
///   thread and these would wait for ever.
fn concurrent(served: &Served, root: &Path, slots: u32, hint: u32) {
    let limit = limit(hint, slots);
    let free_worker = workers(hint, slots) > limit;
    let want = std::sync::Arc::new(pattern(2 * MIB));
    let mark = served.recorder.mark();
    let held = served.faults.arm(
        Match {
            op: Some(Opcode::Read),
            path_prefix: Some("data/big.bin".into()),
            ..Match::default()
        },
        Fault::Delay(MAX_DELAY),
    );
    let readers: Vec<_> = (0..limit + 2)
        .map(|_| {
            let (path, want) = (root.join("data").join("big.bin"), want.clone());
            std::thread::spawn(move || std::fs::read(path).is_ok_and(|got| got == *want))
        })
        .collect();
    let release = || {
        served.faults.clear();
        served.faults.release();
    };
    if !served.faults.wait_applied(held, limit, WAIT) {
        release();
        panic!("{limit} READs never reached the director");
    }
    if free_worker {
        let (tx, rx) = mpsc::channel();
        let data = root.join("data");
        std::thread::spawn(move || {
            let small = std::fs::metadata(data.join("small.txt")).map(|m| m.len());
            let listed = std::fs::read_dir(data.join("list")).map(|d| d.count());
            let _ = tx.send((small, listed));
        });
        // Under the holds' own limit, so a getattr that waited behind them
        // fails here rather than after they ran out.
        match rx.recv_timeout(MAX_DELAY / 2) {
            Ok((small, listed)) => {
                assert_eq!(small.ok(), Some(1000), "getattr while reads are held");
                assert!(listed.is_ok_and(|n| n > 0), "listing while reads are held");
            }
            Err(_) => {
                release();
                panic!("a getattr waited behind {limit} held READs: the director's free thread was taken");
            }
        }
    }
    // Every READ that reached the director is still held: more than `limit`
    // means the shim sent more data requests than it may.
    let holding = served.faults.applied(held);
    release();
    assert_eq!(holding, limit, "READs held at once");
    for r in readers {
        assert!(r.join().unwrap(), "a concurrent read returned wrong bytes");
    }

    let seen = served.recorder.since(mark);
    let reads: Vec<_> = seen
        .iter()
        .filter(|r| r.op() == Some(Opcode::Read))
        .collect();
    let mut used: Vec<u32> = reads.iter().map(|r| r.slot).collect();
    used.sort_unstable();
    used.dedup();
    assert!(
        used.len() >= limit.min(slots) as usize,
        "READs used slots {used:?}, expected at least {limit} of {slots}"
    );
    assert!(used.iter().all(|&s| s < slots), "{used:?} of {slots} slots");
    assert!(
        reads.iter().any(|r| r.bulk_len > 0),
        "no READ crossed in the arena"
    );
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

/// The serve threads wait on the event for ever (`wake_wait_ms` of
/// `INFINITE`) and never poll, so a request is served only if the shim
/// signals `VFS_SERVER_EV`: one it does not signal hangs. The battery runs on
/// a thread this one waits on, so that hang fails the test (after `WAIT`)
/// rather than CI. Every wait that ended was a signal, none a timeout.
#[test]
fn the_server_event_wakes_the_director() {
    isolate!();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let served = run(Options {
            ring: RingKind::SectionWithWake,
            wake_wait_ms: u32::MAX,
            ..Options::default()
        });
        let _ = tx.send(served);
    });
    let served = rx
        .recv_timeout(WAIT)
        .expect("the battery hung: the shim does not signal the director's event");
    assert!(std::env::var_os(aether_env::SERVER_EV).is_some());
    assert!(served.wakes.signalled() > 0, "no wait ended on a signal");
    assert_eq!(served.wakes.timeouts(), 0, "a wait timed out");
}

/// A payload cap that is not a multiple of 8: `slot_stride` is
/// `align8(32 + payload_cap)` (wire.md 2.4), and every slot after 0 sits at
/// a stride past it.
#[test]
fn payload_cap_4099() {
    isolate!();
    run(ring(8, 4099, 256 * KIB));
}

#[test]
fn payload_cap_5003_on_three_slots() {
    isolate!();
    run(ring(3, 5003, 256 * KIB));
}

/// The Wine and Proton transport: the ring is a file (`VFS_RING_PATH`), and
/// a `VFS_RING_SECTION` left in the environment does not win over it. A
/// section of that name exists, an initialised ring nobody serves, so a shim
/// that tried it first would attach to it and get no answer: the battery
/// passing means it went to the file. No wake event is named for a file ring.
#[test]
fn a_file_ring_through_the_hooks() {
    isolate!();
    let stale = format!("Local\\aether-shim-stale-{}", std::process::id());
    let decoy: &'static aether_win::SharedMapping = Box::leak(Box::new(
        aether_win::SharedMapping::create(&stale, MIB).expect("decoy section"),
    ));
    aether_ipc::ring::init(decoy.seg(), 8, 4096).expect("decoy ring");
    std::env::set_var(aether_env::RING_SECTION, &stale);
    assert!(std::env::var_os(aether_env::SERVER_EV).is_none());
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
