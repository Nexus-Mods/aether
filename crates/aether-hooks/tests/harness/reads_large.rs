//! Large reads through the shim (gap items S3, S7, B2): one `NtReadFile` of
//! a megabyte and more, a director that answers a bulk READ inline, an arena
//! too small for the shim's chunk, and reads of 4 MiB and over, which take
//! the deep pipeline, with other threads at work beside them.
//!
//! The contract these tests hold the shim to:
//!
//! - wire.md 3.4.4: a director answers a READ that sets `FLAG_READ_BULK` in
//!   bulk when it has an arena, and `n < len` is end of file. A director
//!   that answers such a READ inline breaks that, and the shim takes its
//!   short answer as the end of the file (pinned here, not changed;
//!   conformance C1 checks directors for it).
//! - wire.md 2.6: each bank must hold the shim's bulk chunk, so
//!   `arena_len >= 256 KiB x slot_count`. A ring whose arena is smaller is
//!   used as if it had none: the shim sends inline READs that any director
//!   following 3.4.4 answers whole.
#![allow(unsafe_code)] // Win32 calls and reads through mapped views

use crate::fakedirector::{self, pattern, Fake, Fault, Match, Options, ReadStyle, RingKind};
use crate::ntapi;
use aether_protocol::{Opcode, Request, RequestFlags};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_GEN_FAILURE, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesExW, GetFileExInfoStandard, FILE_SHARE_READ, OPEN_EXISTING,
    WIN32_FILE_ATTRIBUTE_DATA,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READONLY,
};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// The payload cap of the tests that need the shim's 1 MiB bulk chunk: an
/// inline reply then holds at most `MIB - 8` bytes.
const CAP_1_MIB: u32 = MIB as u32;
/// wire.md 2.6's smallest bank: the shim's smallest bulk chunk.
const MIN_BANK: usize = 256 * KIB;
/// The largest READ wire.md 3.4.4 lets a director answer inline when the
/// request does not set the bulk flag: one below `BULK_THRESHOLD`.
const INLINE_MAX: u32 = 65_535;
/// How long a test waits on the recorder before calling it a failure. Only
/// a bound: each wait ends on the event it waits for.
const WAIT: Duration = Duration::from_secs(10);

const FILE: &str = "data/big.bin";

/// A virtual root on disk (`<tmp>/root`), kept for the life of the test
/// process.
fn root() -> PathBuf {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    root
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error value.
    unsafe { GetLastError() }
}

/// Serve `fake` on a ring `opts` describes and install the hooks.
fn serve(fake: Fake, opts: Options) -> (PathBuf, &'static fakedirector::Served) {
    let root = root();
    let served = fakedirector::install_with(&root, fake, opts);
    std::mem::forget(aether_hooks::install().expect("install"));
    (root, served)
}

/// The ring the 1 MiB tests use: a 1 MiB payload cap and banks of 1 MiB, so
/// the shim's bulk chunk is 1 MiB and an inline reply holds `MIB - 8`.
fn one_mib_ring() -> Options {
    Options {
        payload_cap: CAP_1_MIB,
        arena_len: fakedirector::SLOTS as usize * MIB,
        ..Options::default()
    }
}

fn open(p: &Path) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(
        h,
        INVALID_HANDLE_VALUE,
        "open {}: {}",
        p.display(),
        last_error()
    );
    h
}

fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

/// One `NtReadFile` of `len` bytes from the start of `p`: its status, and
/// the bytes its `Information` names.
fn read_once(p: &Path, len: usize) -> (i32, Vec<u8>) {
    let h = open(p);
    let mut buf = vec![0u8; len];
    let (st, n) = ntapi::nt_read_at(h, 0, &mut buf);
    close(h);
    buf.truncate(n.min(len));
    (st, buf)
}

/// `CreateFileMappingW` of the whole file: the section, or the Win32 error.
fn mapping(file: HANDLE) -> Result<HANDLE, u32> {
    // SAFETY: a file handle this test opened; unnamed, whole file.
    let s = unsafe {
        CreateFileMappingW(
            file,
            core::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            core::ptr::null(),
        )
    };
    if s.is_null() {
        Err(last_error())
    } else {
        Ok(s)
    }
}

/// The whole of `p` as a section maps it: `Err` with the Win32 error when
/// the section cannot be made.
fn mapped(p: &Path, len: usize) -> Result<Vec<u8>, u32> {
    let file = open(p);
    let section = match mapping(file) {
        Ok(s) => s,
        Err(e) => {
            close(file);
            return Err(e);
        }
    };
    // SAFETY: a section this test created.
    let v = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0) };
    assert!(!v.Value.is_null(), "MapViewOfFile: {}", last_error());
    // SAFETY: the view is mapped for the section's `len` bytes and stays
    // mapped while this copies them.
    let got = unsafe { std::slice::from_raw_parts(v.Value as *const u8, len) }.to_vec();
    // SAFETY: the view this mapped, unmapped once.
    assert_ne!(
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v.Value }) },
        0,
        "UnmapViewOfFile: {}",
        last_error()
    );
    close(section);
    close(file);
    Ok(got)
}

fn reads_of(file: &str) -> Match {
    Match {
        op: Some(Opcode::Read),
        path_prefix: Some(file.to_string()),
        ..Match::default()
    }
}

/// The READs recorded since `mark`: request flags, `len`, slot, and the
/// count the reply gave.
fn reads_since(
    served: &fakedirector::Served,
    mark: aether_refdirector::harness::Mark,
) -> Vec<(u32, u32, u32)> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request() {
            Some(Request::Read { len, .. }) => Some((r.flags, len, r.slot)),
            _ => None,
        })
        .collect()
}

fn is_bulk(flags: u32) -> bool {
    flags & RequestFlags::READ_BULK.bits() != 0
}

// ---- one call ------------------------------------------------------------

#[test]
fn one_read_of_1_mib_returns_1_mib_in_one_call() {
    isolate!();
    let want = pattern(2 * MIB);
    let (root, served) = serve(
        Fake::new().with(FILE, want.clone(), ReadStyle::Whole),
        one_mib_ring(),
    );
    let mark = served.recorder.mark();
    let (st, got) = read_once(&root.join(FILE), MIB);
    assert!(st >= 0, "NtReadFile: {st:#x}");
    assert_eq!(got.len(), MIB, "one NtReadFile of 1 MiB came back short");
    assert!(got == want[..MIB], "the bytes differ from the file's");
    let reads = reads_since(served, mark);
    assert!(
        !reads.is_empty() && reads.iter().all(|&(f, _, _)| is_bulk(f)),
        "a 1 MiB read with an arena goes in bulk: {reads:?}"
    );
}

/// B2, the inline half, pinned. wire.md 3.4.4: a director with an arena
/// answers a READ that sets `FLAG_READ_BULK` in bulk, and `n < len` is end
/// of file. This director answers the shim's 1 MiB bulk chunk inline, which
/// holds at most `payload_cap - 8` bytes, so the shim reads the file as
/// ending there. The director is outside the contract (conformance C1
/// catches it); the shim does not guess past a short answer.
#[test]
fn an_inline_only_director_truncates_a_bulk_read_as_the_contract_says() {
    isolate!();
    let want = pattern(2 * MIB);
    let (root, served) = serve(
        Fake::new()
            .inline_reads(CAP_1_MIB)
            .with(FILE, want.clone(), ReadStyle::Whole),
        one_mib_ring(),
    );
    let mark = served.recorder.mark();
    let (st, got) = read_once(&root.join(FILE), MIB);
    assert!(st >= 0, "NtReadFile: {st:#x}");
    assert_eq!(got.len(), 1_048_568, "the short inline answer is the end");
    assert!(got == want[..got.len()], "the bytes differ from the file's");
    let reads = reads_since(served, mark);
    assert_eq!(reads.len(), 1, "one bulk chunk, answered short: {reads:?}");
    assert!(is_bulk(reads[0].0), "the shim asked in bulk");
}

// ---- an arena below wire.md 2.6's rule -----------------------------------

/// B2, the bank half: an arena of `SLOTS x 128 KiB` has banks smaller than
/// the shim's 256 KiB chunk (wire.md 2.6), so a bulk READ would come back
/// short and end the file. The shim uses the ring as if it had no arena.
fn an_arena_below_the_rule_is_not_used_at(payload_cap: u32) {
    let size = 2 * MIB;
    let want = pattern(size);
    let (root, served) = serve(
        Fake::new().with(FILE, want.clone(), ReadStyle::Whole),
        Options {
            payload_cap,
            arena_len: fakedirector::SLOTS as usize * 128 * KIB,
            ..Options::default()
        },
    );
    assert!(
        fakedirector::SLOTS as usize * 128 * KIB < MIN_BANK * fakedirector::SLOTS as usize,
        "the arena is below the rule"
    );
    let path = root.join(FILE);
    let mark = served.recorder.mark();
    let (st, got) = read_once(&path, size);
    assert!(st >= 0, "NtReadFile: {st:#x}");
    assert_eq!(got.len(), size, "one NtReadFile of 2 MiB came back short");
    assert!(got == want, "one call: the bytes differ from the file's");
    assert!(
        std::fs::read(&path).expect("std::fs::read") == want,
        "std::fs::read: the bytes differ from the file's"
    );
    let reads = reads_since(served, mark);
    assert!(!reads.is_empty(), "nothing was read through the ring");
    for &(flags, len, _) in &reads {
        assert!(!is_bulk(flags), "a READ asked in bulk: {reads:?}");
        // Unflagged and under BULK_THRESHOLD: wire.md 3.4.4 has every
        // director answer it inline, whole.
        assert!(
            len <= INLINE_MAX,
            "a READ of {len} bytes may be answered in bulk"
        );
    }
}

#[test]
fn an_arena_below_the_rule_is_not_used() {
    isolate!();
    an_arena_below_the_rule_is_not_used_at(fakedirector::PAYLOAD_CAP);
}

/// The same at a 1 MiB payload cap, where an inline READ of `payload_cap -
/// 8` would be at least `BULK_THRESHOLD`: a director with an arena answers
/// that in bulk from its small bank, short. So the shim keeps its inline
/// READs below the threshold.
#[test]
fn an_arena_below_the_rule_is_not_used_at_a_1_mib_payload_cap() {
    isolate!();
    an_arena_below_the_rule_is_not_used_at(CAP_1_MIB);
}

/// wire.md 3.4.4, the section paragraph: a fill answered short of the size
/// OPEN gave fails the section. This director answers the 1 MiB bulk chunk
/// inline (`MIB - 8` bytes), so the eager fill of a 2 MiB file is short and
/// `CreateFileMappingW` fails; no section of zero-filled bytes is made.
#[test]
fn a_section_over_an_inline_only_director_fails_as_the_contract_says() {
    isolate!();
    let (root, served) = serve(
        Fake::new()
            .inline_reads(CAP_1_MIB)
            .with(FILE, pattern(2 * MIB), ReadStyle::Whole),
        one_mib_ring(),
    );
    let mark = served.recorder.mark();
    assert_eq!(mapped(&root.join(FILE), 2 * MIB), Err(ERROR_GEN_FAILURE));
    let reads = reads_since(served, mark);
    assert!(
        !reads.is_empty() && reads.iter().all(|&(f, _, _)| is_bulk(f)),
        "the fill asked in bulk: {reads:?}"
    );
}

// ---- 4 MiB and over: the deep pipeline -----------------------------------

/// Hold the first `depth` READs of [`FILE`] at the director while one
/// `NtReadFile` of 5 MiB runs on another thread, and wait until `depth` are
/// in flight at once: the shim's pipeline for the read. Then let them go;
/// the bytes must be the file's and no more than `depth` were ever in
/// flight.
fn a_5_mib_read_pipelines(opts: Options, depth: usize) {
    let size = 5 * MIB;
    let want = pattern(size);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), opts);
    let path = root.join(FILE);
    let held = served.faults.arm(
        Match {
            times: Some(depth as u32),
            ..reads_of(FILE)
        },
        Fault::Delay(WAIT),
    );
    served.recorder.reset_max_in_flight();
    let mark = served.recorder.mark();
    let reader = std::thread::spawn(move || read_once(&path, size));
    assert!(
        served.recorder.wait_in_flight(depth, WAIT),
        "{depth} READs were never in flight at once (at most {})",
        served.recorder.max_in_flight()
    );
    assert!(
        served.faults.wait_applied(held, depth as u32, WAIT),
        "the delay did not hold {depth} READs"
    );
    served.faults.release();
    let (st, got) = reader.join().expect("reader");
    assert!(st >= 0, "NtReadFile: {st:#x}");
    assert_eq!(got.len(), size, "one NtReadFile of 5 MiB came back short");
    assert!(got == want, "the bytes differ from the file's");
    assert_eq!(
        served.recorder.max_in_flight(),
        depth,
        "the pipeline's depth"
    );
    // The held READs were in flight together, so each had a slot of its
    // own, and so a bank of its own.
    let reads = reads_since(served, mark);
    let mut slots: Vec<u32> = reads[..depth].iter().map(|&(_, _, s)| s).collect();
    slots.sort_unstable();
    slots.dedup();
    assert_eq!(slots.len(), depth, "held READs shared a slot: {reads:?}");
    assert!(reads.iter().all(|&(f, _, _)| is_bulk(f)), "{reads:?}");
}

/// S7: a read of at least 4 MiB has `PIPELINE_DEPTH_STREAM` (8) READs in
/// flight, when the data gate allows that many from one call. The gate
/// gives one call at most half its permits and keeps a quarter for others
/// (`aether_ipc::DataGate`), so 8 needs a limit of 15 or more: 24 workers on
/// 24 slots (limit 18, 9 per call).
#[test]
fn a_5_mib_read_uses_the_deep_pipeline_and_is_byte_exact() {
    isolate!();
    a_5_mib_read_pipelines(
        Options {
            slots: 24,
            worker_hint: 24,
            arena_len: 24 * MIN_BANK,
            ..Options::default()
        },
        8,
    );
}

/// Pinned: at 16 workers (on 32 slots) the gate, not
/// `PIPELINE_DEPTH_STREAM`, sets the depth: limit 12, so 6 per call.
#[test]
fn a_5_mib_read_at_16_workers_is_6_deep() {
    isolate!();
    a_5_mib_read_pipelines(
        Options {
            slots: 32,
            worker_hint: 16,
            arena_len: 32 * MIN_BANK,
            ..Options::default()
        },
        6,
    );
}

/// S7: two threads stream 5 MiB each, one through `NtReadFile` and one
/// through `NtCreateSection`, against a director whose every READ takes
/// 20 ms, while a third asks for a file's attributes in a loop. The data
/// gate keeps a director worker free of reads (wire.md 2.8), so every
/// GETATTR is answered within the client's 300 ms deadline.
#[test]
fn a_stream_read_does_not_starve_another_thread_s_getattr() {
    isolate!();
    const A: &str = "data/a.bin";
    const B: &str = "data/b.bin";
    const SMALL: &str = "data/small.txt";
    let deadline = Duration::from_millis(300);
    let size = 5 * MIB;
    let want = pattern(size);
    let (root, served) = serve(
        Fake::new()
            .with(A, want.clone(), ReadStyle::Whole)
            .with(B, want.clone(), ReadStyle::Whole)
            .with(SMALL, b"small".to_vec(), ReadStyle::Whole),
        Options {
            ring: RingKind::SectionWithWake,
            deadline: Some(deadline),
            ..Options::default()
        },
    );
    let slow = served.faults.arm(
        Match {
            op: Some(Opcode::Read),
            ..Match::default()
        },
        Fault::Delay(Duration::from_millis(20)),
    );
    let reads_done = std::sync::Arc::new(AtomicUsize::new(0));
    let (a, b) = (root.join(A), root.join(B));
    let streamed = {
        let done = reads_done.clone();
        std::thread::spawn(move || {
            let r = read_once(&a, size);
            done.fetch_add(1, Ordering::SeqCst);
            r
        })
    };
    let sectioned = {
        let done = reads_done.clone();
        std::thread::spawn(move || {
            let r = mapped(&b, size);
            done.fetch_add(1, Ordering::SeqCst);
            r
        })
    };

    let small = wide(&root.join(SMALL));
    let mark = served.recorder.mark();
    let mut calls = 0u32;
    let mut slowest = Duration::ZERO;
    while reads_done.load(Ordering::SeqCst) < 2 {
        let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { core::mem::zeroed() };
        let t = Instant::now();
        // SAFETY: a NUL-terminated name and a buffer of the standard class.
        let ok = unsafe {
            GetFileAttributesExW(
                small.as_ptr(),
                GetFileExInfoStandard,
                &mut data as *mut _ as *mut _,
            )
        };
        let took = t.elapsed();
        assert_ne!(ok, 0, "GetFileAttributesExW failed: {}", last_error());
        assert_eq!(data.nFileSizeLow, 5, "the served file's size");
        slowest = slowest.max(took);
        calls += 1;
    }
    let (st, got) = streamed.join().expect("NtReadFile thread");
    assert!(st >= 0, "NtReadFile: {st:#x}");
    assert!(got == want, "NtReadFile: the bytes differ from the file's");
    assert!(
        sectioned
            .join()
            .expect("section thread")
            .expect("CreateFileMappingW")
            == want,
        "the section differs from the file"
    );
    assert!(served.faults.applied(slow) > 0, "no READ was slowed");
    assert!(calls > 0, "no GETATTR ran beside the reads");
    assert!(
        served.recorder.count_since(mark, Opcode::GetAttr) >= calls as usize,
        "a GETATTR did not cross the ring"
    );
    assert!(
        slowest < deadline,
        "a GETATTR took {slowest:?} beside two stream reads"
    );
}
