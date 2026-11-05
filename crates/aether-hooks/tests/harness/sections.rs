//! Sections over director files (wire.md 3.4.4), with the reference behind
//! the ring:
//! a data section filled whole at `CreateFileMappingW`, one filled on demand
//! (above `EAGER_MAX`, here lowered to [`EAGER_MAX`] through
//! `aether_hooks::set_eager_section_max` so no test needs a 256 MiB file),
//! and an image section of a served PE.
//!
//! What they hold the shim to:
//!
//! - A demand-paged section keeps reading through its file's director
//!   handle after the program closed the file. CLOSE goes when the last of
//!   file handle, section and views goes, once (wire.md 3.4.10).
//! - A fill that fails, or comes back short of what the file holds there
//!   (wire.md 3.4.4: `n < len` is the end of the file, so short before the
//!   size the OPEN gave is a failure), is never committed as zeros. Eager:
//!   the section fails. Demand-paged: the READ is tried again, then the
//!   fault becomes `STATUS_IN_PAGE_ERROR`, as NT raises for a mapped file
//!   whose paging read fails. The zeros past the end of a file stay.
#![allow(unsafe_code)] // Win32 calls and reads through mapped views

use crate::fakedirector::{self, pattern, Fake, Fault, Match, Options, ReadStyle};
use aether_protocol::{Opcode, Request, RequestFlags, Status};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_GEN_FAILURE, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, OPEN_EXISTING};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READONLY, SEC_IMAGE,
};
use windows_sys::Win32::System::Threading::ExitProcess;

const MIB: usize = 1024 * 1024;
/// The demand-paging threshold these tests set: sections larger than this
/// are filled on fault.
const EAGER_MAX: u64 = 4 * MIB as u64;
/// A demand-paged file: past the 2 MiB the shim warms at creation.
const LAZY: usize = 8 * MIB;
/// Where the demand-paged tests touch: a 256 KiB chunk outside the warm
/// window.
const TOUCH: usize = 6 * MIB;
/// The shim's fill unit.
const CHUNK: usize = 256 * 1024;
const PAGE: usize = 4096;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const STATUS_IN_PAGE_ERROR: u32 = 0xC000_0006;

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

fn reads(op: Opcode) -> Match {
    Match {
        op: Some(op),
        path_prefix: Some(FILE.to_string()),
        ..Match::default()
    }
}

/// Serve `fake` with the default geometry and install the hooks; with
/// `lazy`, sections over [`EAGER_MAX`] are demand-paged.
fn serve(fake: Fake, lazy: bool) -> (PathBuf, &'static fakedirector::Served) {
    if lazy {
        aether_hooks::set_eager_section_max(EAGER_MAX);
    }
    let root = root();
    let served = fakedirector::install_with(&root, fake, Options::default());
    std::mem::forget(aether_hooks::install().expect("install"));
    (root, served)
}

fn open(p: &Path, access: u32) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
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

/// `CreateFileMappingW` of the whole file: the section, or the Win32 error.
fn mapping(file: HANDLE, protect: u32) -> Result<HANDLE, u32> {
    // SAFETY: a file handle this test opened; unnamed, whole file.
    let s =
        unsafe { CreateFileMappingW(file, core::ptr::null(), protect, 0, 0, core::ptr::null()) };
    if s.is_null() {
        Err(last_error())
    } else {
        Ok(s)
    }
}

/// `MapViewOfFile` of the whole section, for reading.
fn view(section: HANDLE) -> *const u8 {
    // SAFETY: a section this test created.
    let v = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0) };
    assert!(!v.Value.is_null(), "MapViewOfFile: {}", last_error());
    v.Value as *const u8
}

fn unmap(v: *const u8) {
    // SAFETY: a view this test mapped, unmapped once.
    let ok = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v as *mut _ }) };
    assert_ne!(ok, 0, "UnmapViewOfFile: {}", last_error());
}

fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

/// `len` bytes of the view at `at`, read as a program reads them: a
/// demand-paged page faults on the first of them.
fn bytes(v: *const u8, at: usize, len: usize) -> Vec<u8> {
    // SAFETY: the view is mapped for at least `at + len` bytes, and stays
    // mapped while this reads.
    (0..len)
        .map(|i| unsafe { core::ptr::read_volatile(v.add(at + i)) })
        .collect()
}

/// The offsets of the READs recorded since `mark`.
fn read_offsets(
    served: &fakedirector::Served,
    mark: aether_refdirector::harness::Mark,
) -> Vec<u64> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request() {
            Some(Request::Read { offset, .. }) => Some(offset),
            _ => None,
        })
        .collect()
}

// ---- eager: filled whole at creation -------------------------------------

#[test]
fn a_1_mib_section_over_a_director_file_maps_its_bytes() {
    isolate!();
    let want = pattern(MIB);
    let (root, served) = serve(
        Fake::new().with(FILE, want.clone(), ReadStyle::Whole),
        false,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let mark = served.recorder.mark();
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let filled = served.recorder.since(mark);
    assert!(!filled.is_empty(), "the section was filled without a READ");
    for r in &filled {
        assert_eq!(r.op(), Some(Opcode::Read), "the fill sent {:?}", r.op());
        assert_ne!(
            r.flags & RequestFlags::READ_BULK.bits(),
            0,
            "a fill READ was not bulk"
        );
        assert_eq!(r.status, 0, "a fill READ failed");
    }
    let after = served.recorder.mark();
    let v = view(section);
    assert!(bytes(v, 0, MIB) == want, "the view differs from the file");
    assert_eq!(
        served.recorder.total_since(after),
        0,
        "mapping or reading the view crossed the ring"
    );
    unmap(v);
    close(section);
    close(file);
}

#[test]
fn a_failed_eager_fill_fails_the_section() {
    isolate!();
    let want = pattern(4 * MIB);
    let (root, served) = serve(
        Fake::new().with(FILE, want.clone(), ReadStyle::Whole),
        false,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let id = served.faults.arm(
        Match {
            nth: Some(2),
            ..reads(Opcode::Read)
        },
        Fault::Status(Status::IoError),
    );
    assert_eq!(mapping(file, PAGE_READONLY), Err(ERROR_GEN_FAILURE));
    assert_eq!(served.faults.applied(id), 1, "the fault never fired");
    assert_eq!(
        aether_hooks::section_failure_count("eager-fill"),
        1,
        "a failed eager fill is counted"
    );

    served.faults.clear();
    let section = mapping(file, PAGE_READONLY).expect("the section without the fault");
    let v = view(section);
    assert!(
        bytes(v, 0, want.len()) == want,
        "the view differs from the file"
    );
    unmap(v);
    close(section);
    close(file);
}

#[test]
fn a_short_eager_fill_fails_the_section() {
    isolate!();
    // OPEN says 4 MiB; the director has 3.
    let (root, _served) = serve(
        Fake::new().with(
            FILE,
            pattern(4 * MIB),
            ReadStyle::ShorterThanClaimed(3 * MIB as u64),
        ),
        false,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    assert_eq!(mapping(file, PAGE_READONLY), Err(ERROR_GEN_FAILURE));
    close(file);
}

// ---- image sections ------------------------------------------------------

#[test]
fn an_image_section_of_a_served_dll_loads() {
    isolate!();
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    let pe = std::fs::read(Path::new(&system).join("System32").join("version.dll"))
        .expect("a system DLL to serve");
    // The shim writes the image it hands the kernel beside its config: keep
    // that in this test's own folder.
    let cfg = aether_testkit::tempdir().expect("temp folder");
    std::env::set_var(aether_env::SHIM_CONFIG, cfg.path().join("shim.cfg"));
    let (root, _served) = serve(
        Fake::new().with("bin/version.dll", pe.clone(), ReadStyle::Whole),
        false,
    );
    let file = open(
        &root.join("bin/version.dll"),
        GENERIC_READ | GENERIC_EXECUTE,
    );
    let section = mapping(file, PAGE_READONLY | SEC_IMAGE).expect("an image section");
    let v = view(section);
    let lfanew = u32::from_le_bytes(pe[0x3C..0x40].try_into().unwrap()) as usize;
    // SizeOfHeaders: the optional header's field at offset 60.
    let at = lfanew + 24 + 60;
    let headers = u32::from_le_bytes(pe[at..at + 4].try_into().unwrap()) as usize;
    // The mapped headers are the file's, except ImageBase (a PE32+
    // optional header's 8 bytes at 24), which the kernel sets to where it
    // put the image.
    let base = lfanew + 24 + 24;
    let mut got = bytes(v, 0, headers);
    assert_eq!(got[..2], *b"MZ");
    got[base..base + 8].copy_from_slice(&pe[base..base + 8]);
    assert!(
        got == pe[..headers],
        "the image's headers differ from the file's"
    );
    unmap(v);
    close(section);
    close(file);
}

// ---- demand-paged: filled on fault ---------------------------------------

#[test]
fn a_lazy_section_fills_on_first_touch_only() {
    isolate!();
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    // Filled through a second view: the path this host takes.
    assert_eq!(aether_hooks::lazy_region_count("shared"), 1);
    assert_eq!(aether_hooks::lazy_region_count("private-reserve-failed"), 0);

    let mark = served.recorder.mark();
    assert!(bytes(v, TOUCH, PAGE) == want[TOUCH..TOUCH + PAGE]);
    assert_eq!(
        read_offsets(served, mark),
        [TOUCH as u64],
        "the first touch should fetch its chunk, once"
    );
    let again = served.recorder.mark();
    assert!(bytes(v, TOUCH, CHUNK) == want[TOUCH..TOUCH + CHUNK]);
    assert_eq!(
        served.recorder.total_since(again),
        0,
        "a filled chunk was fetched again"
    );
    unmap(v);
    close(section);
    close(file);
}

#[test]
fn a_lazy_section_still_reads_after_its_file_handle_is_closed() {
    isolate!();
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let a = view(section);
    let b = view(section);

    let mark = served.recorder.mark();
    close(file);
    assert!(
        bytes(a, TOUCH, PAGE) == want[TOUCH..TOUCH + PAGE],
        "the page read after the file's close is not the file's"
    );
    unmap(a);
    assert!(bytes(b, TOUCH + CHUNK, PAGE) == want[TOUCH + CHUNK..TOUCH + CHUNK + PAGE]);
    unmap(b);
    assert_eq!(
        served.recorder.count_since(mark, Opcode::Close),
        0,
        "CLOSE went while the section was open"
    );
    close(section);
    assert_eq!(
        served.recorder.count_since(mark, Opcode::Close),
        1,
        "the section's last release should close the director handle, once"
    );
    assert_eq!(served.fake.tally.closes(FILE), 1);
}

/// One of the three things that keep a demand-paged section's director
/// handle.
#[derive(Debug, Clone, Copy)]
enum Holder {
    File,
    Section,
    View,
}

#[test]
fn a_lazy_section_s_director_handle_closes_with_the_last_holder_in_every_order() {
    isolate!();
    use Holder::*;
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let orders = [
        [File, Section, View],
        [File, View, Section],
        [Section, File, View],
        [Section, View, File],
        [View, File, Section],
        [View, Section, File],
    ];
    for order in orders {
        let file = open(&root.join(FILE), GENERIC_READ);
        let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
        let v = view(section);
        let mark = served.recorder.mark();
        let mut mapped = true;
        for (i, h) in order.iter().enumerate() {
            match h {
                File => close(file),
                Section => close(section),
                View => {
                    unmap(v);
                    mapped = false;
                }
            }
            if mapped {
                // A chunk not touched before: it is fetched now.
                let at = TOUCH - (i + 1) * CHUNK;
                assert!(
                    bytes(v, at, PAGE) == want[at..at + PAGE],
                    "{order:?}: the page after closing {h:?} is not the file's"
                );
            }
            let closes = served.recorder.count_since(mark, Opcode::Close);
            let last = i == order.len() - 1;
            assert_eq!(
                closes,
                usize::from(last),
                "{order:?}: CLOSEs after closing {h:?}"
            );
        }
    }
}

#[test]
fn a_failed_lazy_fill_is_asked_again_before_the_fault_returns() {
    isolate!();
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    let id = served.faults.arm(
        Match {
            times: Some(1),
            ..reads(Opcode::Read)
        },
        Fault::Status(Status::IoError),
    );
    let mark = served.recorder.mark();
    assert!(
        bytes(v, TOUCH, CHUNK) == want[TOUCH..TOUCH + CHUNK],
        "a chunk whose first READ failed holds something other than the file"
    );
    assert_eq!(served.faults.applied(id), 1, "the fault never fired");
    assert_eq!(
        read_offsets(served, mark),
        [TOUCH as u64, TOUCH as u64],
        "the failed READ should be asked again, once"
    );
    unmap(v);
    close(section);
    close(file);
}

/// What [`in_page_witness`] checks the raised exception against.
static WITNESS: std::sync::OnceLock<(
    usize,
    &'static fakedirector::Served,
    aether_refdirector::harness::Mark,
)> = std::sync::OnceLock::new();

/// Exit code of a witnessed in-page error whose parameters or READ count
/// are not what the shim should raise.
const WRONG_IN_PAGE_ERROR: u32 = 0xE000_0001;

/// The last vectored handler: sees the exception the shim's handler passed
/// on. An in-page error with the faulting address, the failed read's status
/// and four READs of the chunk (the try and three more) ends the process
/// with the in-page error's code, anything else with [`WRONG_IN_PAGE_ERROR`].
unsafe extern "system" fn in_page_witness(info: *mut EXCEPTION_POINTERS) -> i32 {
    let rec = &*(*info).ExceptionRecord;
    if rec.ExceptionCode as u32 != STATUS_IN_PAGE_ERROR {
        return 0; // EXCEPTION_CONTINUE_SEARCH
    }
    let &(addr, served, mark) = WITNESS.get().expect("armed");
    let ok = rec.NumberParameters == 3
        && rec.ExceptionInformation[0] == 0 // a read
        && rec.ExceptionInformation[1] == addr
        && rec.ExceptionInformation[2] == 0xC000_0001 // STATUS_UNSUCCESSFUL
        && read_offsets(served, mark) == [TOUCH as u64; 4]
        // Counted for the stats report, ungated.
        && aether_hooks::section_failure_count("in-page-error") == 1;
    ExitProcess(if ok {
        STATUS_IN_PAGE_ERROR
    } else {
        WRONG_IN_PAGE_ERROR
    });
}

#[test]
fn a_lazy_fill_that_keeps_failing_raises_an_in_page_error() {
    isolate_expecting_exit!(STATUS_IN_PAGE_ERROR);
    let (root, served) = serve(
        Fake::new().with(FILE, pattern(LAZY), ReadStyle::Whole),
        true,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    served
        .faults
        .arm(reads(Opcode::Read), Fault::Status(Status::IoError));
    let _ = WITNESS.set((v as usize + TOUCH, served, served.recorder.mark()));
    // SAFETY: a handler that only reads the record and ends the process.
    assert!(!unsafe { AddVectoredExceptionHandler(0, Some(in_page_witness)) }.is_null());
    // Dies here, of the in-page error.
    let got = bytes(v, TOUCH, 1);
    panic!("a page whose READ always fails was read as {got:?}");
}

#[test]
fn a_lazy_fill_short_of_the_file_s_size_raises_an_in_page_error() {
    isolate_expecting_exit!(STATUS_IN_PAGE_ERROR);
    // OPEN says 8 MiB; the director has 5.
    let (root, _served) = serve(
        Fake::new().with(
            FILE,
            pattern(LAZY),
            ReadStyle::ShorterThanClaimed(5 * MIB as u64),
        ),
        true,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    let got = bytes(v, TOUCH, 1);
    panic!("a page past what the director holds was read as {got:?}");
}

#[test]
fn a_short_fill_at_the_file_s_end_is_zero_filled_as_nt_does() {
    isolate!();
    // One demand-paged and one eager file, each ending 100 bytes into a page.
    let lazy = pattern(LAZY + 100);
    let eager = pattern(MIB + 100);
    let (root, _served) = serve(
        Fake::new().with(FILE, lazy.clone(), ReadStyle::Whole).with(
            "data/small.bin",
            eager.clone(),
            ReadStyle::Whole,
        ),
        true,
    );
    for (name, want) in [(FILE, &lazy), ("data/small.bin", &eager)] {
        let file = open(&root.join(name), GENERIC_READ);
        let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
        let v = view(section);
        let tail = want.len() / PAGE * PAGE;
        assert!(
            bytes(v, tail, want.len() - tail) == want[tail..],
            "{name}: the last bytes are not the file's"
        );
        assert!(
            bytes(v, want.len(), tail + PAGE - want.len())
                .iter()
                .all(|&b| b == 0),
            "{name}: the page past the end of the file is not zeros"
        );
        unmap(v);
        close(section);
        close(file);
    }
}

// ---- image sections: reads that fail or come back short ------------------

/// `System32\version.dll`'s bytes, to serve, with the shim told to write the
/// image it hands the kernel beside a config in this test's own folder.
fn system_dll() -> Vec<u8> {
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    let pe = std::fs::read(Path::new(&system).join("System32").join("version.dll"))
        .expect("a system DLL to serve");
    let cfg = aether_testkit::tempdir().expect("temp folder");
    std::env::set_var(aether_env::SHIM_CONFIG, cfg.path().join("shim.cfg"));
    std::mem::forget(cfg);
    pe
}

const DLL: &str = "bin/version.dll";

#[test]
fn a_short_image_read_fails_the_section() {
    isolate!();
    let pe = system_dll();
    // OPEN gives the DLL's size; the director holds half of it.
    let half = (pe.len() / 2) as u64;
    let (root, served) = serve(
        Fake::new().with(DLL, pe, ReadStyle::ShorterThanClaimed(half)),
        false,
    );
    let file = open(&root.join(DLL), GENERIC_READ | GENERIC_EXECUTE);
    let mark = served.recorder.mark();
    assert_eq!(
        mapping(file, PAGE_READONLY | SEC_IMAGE),
        Err(ERROR_GEN_FAILURE),
        "a short image read must fail the section as a failed eager fill does"
    );
    // The try and up to three more.
    let tries = read_offsets(served, mark)
        .iter()
        .filter(|&&o| o == 0)
        .count();
    assert_eq!(
        tries,
        4,
        "READs from offset 0: {:?}",
        read_offsets(served, mark)
    );
    assert_eq!(
        aether_hooks::section_failure_count("image-read"),
        1,
        "a failed image read is counted"
    );
    close(file);
}

#[test]
fn an_image_read_that_fails_once_is_asked_again() {
    isolate!();
    let pe = system_dll();
    let (root, served) = serve(Fake::new().with(DLL, pe.clone(), ReadStyle::Whole), false);
    let file = open(&root.join(DLL), GENERIC_READ | GENERIC_EXECUTE);
    let id = served.faults.arm(
        Match {
            op: Some(Opcode::Read),
            path_prefix: Some(DLL.to_string()),
            times: Some(1),
            ..Match::default()
        },
        Fault::Status(Status::IoError),
    );
    let section = mapping(file, PAGE_READONLY | SEC_IMAGE).expect("an image section");
    assert_eq!(served.faults.applied(id), 1, "the fault never fired");
    let v = view(section);
    assert_eq!(bytes(v, 0, 2), b"MZ");
    unmap(v);
    close(section);
    close(file);
}

// ---- demand-paged: two threads on one chunk ------------------------------

#[test]
fn a_second_thread_never_reads_a_chunk_before_its_fill_is_done() {
    isolate!();
    use std::time::Duration;
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section) as usize;
    let mark = served.recorder.mark();
    // The first thread's fill READ is held until released.
    let id = served.faults.arm(
        Match {
            times: Some(1),
            ..reads(Opcode::Read)
        },
        Fault::Delay(Duration::from_secs(10)),
    );
    let first = std::thread::spawn(move || bytes(v as *const u8, TOUCH, PAGE));
    assert!(
        served.faults.wait_applied(id, 1, Duration::from_secs(10)),
        "the first touch's READ never reached the director"
    );
    // A second thread reads another page of the same chunk while the fill
    // is held: it must wait for the fill, never see what is there before.
    let (tx, rx) = std::sync::mpsc::channel();
    let (touching_tx, touching) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        let _ = touching_tx.send(());
        let _ = tx.send(bytes(v as *const u8, TOUCH + 2 * PAGE, PAGE));
    });
    // The fill stays held until the second thread is about to touch, and
    // for a while after, so its touch meets the fill in progress.
    touching
        .recv_timeout(Duration::from_secs(10))
        .expect("the second thread never started");
    let early = rx.recv_timeout(Duration::from_millis(500));
    served.faults.release();
    let got = match early {
        Ok(b) => b,
        Err(_) => rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the second thread's read"),
    };
    assert!(
        got == want[TOUCH + 2 * PAGE..TOUCH + 3 * PAGE],
        "the second thread read the chunk mid-fill (all zeros: {})",
        got.iter().all(|&b| b == 0)
    );
    assert!(first.join().unwrap() == want[TOUCH..TOUCH + PAGE]);
    second.join().unwrap();
    assert_eq!(
        read_offsets(served, mark),
        [TOUCH as u64],
        "one fill serves both threads"
    );
    unmap(v as *const u8);
    close(section);
    close(file);
}

// ---- demand-paged: fills of different chunks ----------------------------

/// Where the other-chunk touches land: outside the warm window and outside
/// [`TOUCH`]'s chunk.
const OTHER_TOUCH: usize = 4 * MIB;

/// While one chunk's fill READ is held, a fault on another chunk of the
/// same section, and one on a chunk of another section, are filled: only a
/// thread on the held chunk waits for it.
#[test]
fn a_held_fill_does_not_hold_fills_of_other_chunks() {
    isolate!();
    use std::time::Duration;
    const OTHER: &str = "data/other.bin";
    let want = pattern(LAZY);
    let other_want: Vec<u8> = pattern(LAZY).iter().map(|b| b ^ 0x5A).collect();
    let (root, served) = serve(
        Fake::new().with(FILE, want.clone(), ReadStyle::Whole).with(
            OTHER,
            other_want.clone(),
            ReadStyle::Whole,
        ),
        true,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section) as usize;
    let other_file = open(&root.join(OTHER), GENERIC_READ);
    let other_section = mapping(other_file, PAGE_READONLY).expect("CreateFileMappingW");
    let ov = view(other_section) as usize;
    // The first thread's fill READ is held until released (or MAX_DELAY).
    let id = served.faults.arm(
        Match {
            times: Some(1),
            ..reads(Opcode::Read)
        },
        Fault::Delay(Duration::from_secs(10)),
    );
    let first = std::thread::spawn(move || bytes(v as *const u8, TOUCH, PAGE));
    assert!(
        served.faults.wait_applied(id, 1, Duration::from_secs(10)),
        "the first touch's READ never reached the director"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let same_tx = tx.clone();
    let same = std::thread::spawn(move || {
        let _ = same_tx.send(("same", bytes(v as *const u8, OTHER_TOUCH, PAGE)));
    });
    let other = std::thread::spawn(move || {
        let _ = tx.send(("other", bytes(ov as *const u8, OTHER_TOUCH, PAGE)));
    });
    // Both while the first fill is still held.
    let mut early = Vec::new();
    for _ in 0..2 {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(got) => early.push(got),
            Err(_) => break,
        }
    }
    let held = !first.is_finished();
    served.faults.release();
    let first_got = first.join().unwrap();
    same.join().unwrap();
    other.join().unwrap();
    assert!(
        held,
        "the first fill was not held while the others were filled"
    );
    assert_eq!(
        early.len(),
        2,
        "fills that finished while another chunk's fill was held: {:?}",
        early.iter().map(|(w, _)| *w).collect::<Vec<_>>()
    );
    for (which, got) in early {
        let src = if which == "same" { &want } else { &other_want };
        assert!(
            got == src[OTHER_TOUCH..OTHER_TOUCH + PAGE],
            "the {which} chunk's bytes are not the file's"
        );
    }
    assert!(first_got == want[TOUCH..TOUCH + PAGE]);
    unmap(ov as *const u8);
    close(other_section);
    close(other_file);
    unmap(v as *const u8);
    close(section);
    close(file);
}

// ---- unmapping: real views, and the process an unmap names ---------------

/// A view of a real file (outside every root) unmaps through the hooks: its
/// range is free afterwards.
#[test]
fn a_real_file_s_view_still_unmaps() {
    isolate!();
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_FREE};
    let (_root, _served) = serve(Fake::new(), false);
    let dir = aether_testkit::tempdir().expect("temp folder");
    let real = dir.path().join("real.bin");
    std::fs::write(&real, pattern(64 * 1024)).unwrap();
    let file = open(&real, GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("a real section");
    let v = view(section);
    assert!(bytes(v, 0, 16) == pattern(16));
    unmap(v);
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: a query of an address this test owned.
    let n = unsafe {
        VirtualQuery(
            v.cast(),
            &mut mbi,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    assert_ne!(n, 0);
    assert_eq!(mbi.State, MEM_FREE, "the real view is still mapped");
    close(section);
    close(file);
}

/// An unmap that names this process by a real handle (not the
/// `NtCurrentProcess` pseudo-handle) retires a synthetic view as the
/// pseudo-handle does.
#[test]
fn an_unmap_naming_this_process_by_a_real_handle_retires_a_synthetic_view() {
    isolate!();
    use windows_sys::Win32::System::Memory::UnmapViewOfFile2;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_VM_OPERATION,
    };
    let want = pattern(MIB);
    let (root, _served) = serve(Fake::new().with(FILE, want, ReadStyle::Whole), false);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    // SAFETY: a handle to this process, closed below.
    let me = unsafe { OpenProcess(PROCESS_VM_OPERATION, 0, GetCurrentProcessId()) };
    assert!(!me.is_null(), "OpenProcess: {}", last_error());
    // SAFETY: a view this test mapped, unmapped once.
    let ok = unsafe { UnmapViewOfFile2(me, MEMORY_MAPPED_VIEW_ADDRESS { Value: v as _ }, 0) };
    assert_ne!(ok, 0, "UnmapViewOfFile2: {}", last_error());
    // Retired: a second unmap finds no view there.
    // SAFETY: as above; expected to fail.
    let again = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v as _ }) };
    assert_eq!(again, 0, "the synthetic view was not retired");
    close(me);
    close(section);
    close(file);
}

/// A demand-paged section whose own pagefile section cannot be had (the
/// test switch; in the field, the commit or pool limit, or 32-bit address
/// space) is still made, from private memory, and counted as such.
#[test]
fn a_lazy_section_whose_section_reserve_fails_is_private_and_still_reads() {
    isolate!();
    aether_hooks::fail_shared_lazy_reserve(true);
    let want = pattern(LAZY);
    let (root, served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("the section, from private memory");
    let v = view(section);
    assert_eq!(aether_hooks::lazy_region_count("private-reserve-failed"), 1);
    assert_eq!(aether_hooks::lazy_region_count("shared"), 0);
    let mark = served.recorder.mark();
    assert!(bytes(v, TOUCH, CHUNK) == want[TOUCH..TOUCH + CHUNK]);
    assert_eq!(read_offsets(served, mark), [TOUCH as u64]);
    unmap(v);
    close(section);
    close(file);

    // The switch off, the next section takes the shared path again: a
    // failure is not kept for the process.
    aether_hooks::fail_shared_lazy_reserve(false);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section);
    assert_eq!(aether_hooks::lazy_region_count("shared"), 1);
    assert!(bytes(v, TOUCH, PAGE) == want[TOUCH..TOUCH + PAGE]);
    unmap(v);
    close(section);
    close(file);
}

// ---- demand-paged: many threads on a chunk whose fill keeps failing ------

/// The semaphore [`in_page_parker`] releases once per in-page error, as an
/// address (`HANDLE` is not `Sync`).
static RAISED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// A last vectored handler that counts each in-page error the shim passed on
/// and parks the thread that took it for good (it cannot resume: its page
/// has nothing). The test thread reads the count and ends the process.
unsafe extern "system" fn in_page_parker(info: *mut EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::System::Threading::{ReleaseSemaphore, Sleep};
    let rec = &*(*info).ExceptionRecord;
    if rec.ExceptionCode as u32 != STATUS_IN_PAGE_ERROR {
        return 0; // EXCEPTION_CONTINUE_SEARCH
    }
    ReleaseSemaphore(*RAISED.get().expect("armed") as _, 1, core::ptr::null_mut());
    loop {
        Sleep(u32::MAX);
    }
}

/// Exit code of the many-waiters test when the raises or the READs are not
/// what the shim should give.
const WRONG_WAITERS: u32 = 0xE000_0002;

/// Every thread waiting on a chunk whose fill failed its bounded retries
/// raises at once, without READs of its own, and so does a later touch: the
/// chunk is failed for the life of its section.
#[test]
fn threads_on_a_chunk_whose_fill_keeps_failing_all_raise_after_one_bounded_try() {
    isolate_expecting_exit!(STATUS_IN_PAGE_ERROR);
    use std::time::Duration;
    use windows_sys::Win32::System::Threading::{CreateSemaphoreW, WaitForSingleObject};
    const WAITERS: usize = 4;
    // OPEN says 8 MiB; the director has 5, so every READ of the chunk at 6
    // MiB comes back short: a failed fill.
    let (root, served) = serve(
        Fake::new().with(
            FILE,
            pattern(LAZY),
            ReadStyle::ShorterThanClaimed(5 * MIB as u64),
        ),
        true,
    );
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section) as usize;
    // The first READ is held so every waiter is queued on the chunk before
    // any try fails.
    let hold = served.faults.arm(
        Match {
            times: Some(1),
            ..reads(Opcode::Read)
        },
        Fault::Delay(Duration::from_secs(10)),
    );
    served
        .faults
        .arm(reads(Opcode::Read), Fault::Status(Status::IoError));
    // SAFETY: an unnamed semaphore, leaked with the process.
    let sem = unsafe { CreateSemaphoreW(core::ptr::null(), 0, 64, core::ptr::null()) };
    assert!(!sem.is_null());
    let _ = RAISED.set(sem as usize);
    // SAFETY: a handler that only counts and parks.
    assert!(!unsafe { AddVectoredExceptionHandler(0, Some(in_page_parker)) }.is_null());
    let mark = served.recorder.mark();

    let touch = move |at: usize| {
        std::thread::spawn(move || {
            let got = bytes(v as *const u8, at, 1);
            panic!("a page whose READs all fail was read as {got:?}");
        })
    };
    let wait_raised = |n: usize| {
        for _ in 0..n {
            // SAFETY: the semaphore above.
            if unsafe { WaitForSingleObject(sem, 30_000) } != 0 {
                // SAFETY: plain FFI.
                unsafe { ExitProcess(WRONG_WAITERS) };
            }
        }
    };
    let _first = touch(TOUCH);
    assert!(served.faults.wait_applied(hold, 1, Duration::from_secs(10)));
    let _waiters: Vec<_> = (1..WAITERS).map(|i| touch(TOUCH + i * PAGE)).collect();
    // The waiters reach the chunk while its first READ is held. Nothing can
    // say they are blocked on it, so they are given a moment, as a bound on
    // the test, not a condition of its result: a waiter that comes after
    // the chunk failed raises the same way.
    std::thread::sleep(Duration::from_millis(200));
    served.faults.release();
    wait_raised(WAITERS);
    // A later touch of the failed chunk.
    let _later = touch(TOUCH + WAITERS * PAGE);
    wait_raised(1);

    let reads = read_offsets(served, mark);
    let raised = aether_hooks::section_failure_count("in-page-error");
    // SAFETY: plain FFI.
    unsafe {
        ExitProcess(
            if reads == [TOUCH as u64; 4] && raised == (WAITERS + 1) as u64 {
                STATUS_IN_PAGE_ERROR
            } else {
                eprintln!("READs {reads:?}, in-page errors {raised}");
                WRONG_WAITERS
            },
        )
    };
}

// ---- demand-paged: a fault while the process exits -----------------------

/// Set in [`lazy_exit_child`]'s process: the folder its handshake files are
/// in. Absent in an ordinary run, so the child test is a no-op then.
const EXIT_CHILD_DIR: &str = "AETHER_LAZY_EXIT_CHILD_DIR";
/// The child's exit codes: its exit-time reads saw the file's bytes, saw
/// other bytes, or never ran (the process ended by its `ExitProcess`).
const EXIT_FILLED: u32 = 0x0E0F_0001;
const EXIT_WRONG: u32 = 0x0E0F_0002;
const EXIT_NOT_READ: u32 = 0x0E0F_0003;
/// Bytes compared at each exit-time read.
const EXIT_READ: usize = 16;

/// The child's view, once it is about to exit: its process-detach TLS
/// callback reads through it. 0 in every other process.
static EXIT_VIEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// What those reads should see: the file's bytes at the chunk a killed
/// thread was filling, and at a chunk nobody touched.
static EXIT_WANT: std::sync::OnceLock<[[u8; EXIT_READ]; 2]> = std::sync::OnceLock::new();
/// The two places read at exit.
const EXIT_AT: [usize; 2] = [TOUCH + PAGE, OTHER_TOUCH];

/// This binary's TLS callback. The loader calls it with
/// `DLL_PROCESS_DETACH` from `LdrShutdownProcess`: after `ExitProcess` has
/// ended every other thread (the fill workers among them), with the loader
/// lock held, where a DLL's detach or its CRT's `atexit` handlers run.
#[used]
#[link_section = ".CRT$XLY"]
static ON_TLS: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *mut core::ffi::c_void) =
    on_tls;

unsafe extern "system" fn on_tls(
    _: *mut core::ffi::c_void,
    reason: u32,
    _: *mut core::ffi::c_void,
) {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    const DLL_PROCESS_DETACH: u32 = 0;
    let v = EXIT_VIEW.load(Ordering::SeqCst);
    let Some(want) = EXIT_WANT.get() else {
        return;
    };
    if reason != DLL_PROCESS_DETACH || v == 0 {
        return;
    }
    // No allocation: a killed thread may have held the heap's lock.
    let ok = EXIT_AT.iter().zip(want).all(|(&at, want)| {
        (0..EXIT_READ).all(|i| core::ptr::read_volatile((v + at + i) as *const u8) == want[i])
    });
    TerminateProcess(
        GetCurrentProcess(),
        if ok { EXIT_FILLED } else { EXIT_WRONG },
    );
}

/// The handshake between the test and its child: a file appearing in `dir`.
fn wait_for_file(dir: &Path, name: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !dir.join(name).exists() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

/// The process the test watches exit: it maps a demand-paged section of the
/// test's director, has a thread start a fill of [`TOUCH`]'s chunk (whose
/// READ the test holds), and exits; at exit its TLS callback reads that
/// chunk and an untouched one.
#[test]
#[ignore = "the child of a_fault_while_the_process_exits_is_filled; inert on its own"]
fn lazy_exit_child() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
    };
    let Some(dir) = std::env::var_os(EXIT_CHILD_DIR).map(PathBuf::from) else {
        return;
    };
    // SAFETY: sets this process's error mode; no pointers.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
    aether_hooks::director::try_init_from_env().expect("attach to the test's director");
    aether_hooks::set_eager_section_max(EAGER_MAX);
    std::mem::forget(aether_hooks::install().expect("install"));
    let root = PathBuf::from(std::env::var_os(aether_env::VIRTUAL_DIR).expect("root"));
    let want = pattern(LAZY);
    let _ = EXIT_WANT.set(EXIT_AT.map(|at| want[at..at + EXIT_READ].try_into().unwrap()));
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY).expect("CreateFileMappingW");
    let v = view(section) as usize;
    std::fs::write(dir.join("mapped"), b"").unwrap();
    assert!(wait_for_file(&dir, "armed"), "the test never held the READ");
    // Ended by the exit mid-fill: the chunk stays marked as filling.
    std::thread::spawn(move || bytes(v as *const u8, TOUCH, 1));
    assert!(
        wait_for_file(&dir, "go"),
        "the test never saw the held READ"
    );
    EXIT_VIEW.store(v, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: plain FFI.
    unsafe { ExitProcess(EXIT_NOT_READ) };
}

/// A fault in a demand-paged section during process exit is filled, and the
/// process ends: once `ExitProcess` has ended the fill workers, and the
/// thread that was filling a chunk, no fill waits on them. At exit the
/// child reads the chunk the killed thread was filling and one nobody
/// touched, from its TLS callback's `DLL_PROCESS_DETACH`, and sees the
/// file's bytes (READ, wire.md 3.4.4).
#[test]
fn a_fault_while_the_process_exits_is_filled() {
    isolate!();
    use std::time::{Duration, Instant};
    let root = root();
    let dir = aether_testkit::tempdir().expect("handshake folder");
    let served = fakedirector::install_with(
        &root,
        Fake::new().with(FILE, pattern(LAZY), ReadStyle::Whole),
        fakedirector::Options::default(),
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sections::lazy_exit_child",
            "--include-ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(EXIT_CHILD_DIR, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the child");
    let end = |child: &mut std::process::Child, why: &str| -> ! {
        let _ = child.kill();
        let _ = child.wait();
        panic!("{why}");
    };
    if !wait_for_file(dir.path(), "mapped") {
        end(&mut child, "the child never mapped the section");
    }
    // The next READ, the thread's fill of TOUCH's chunk, is held.
    let hold = served.faults.arm(
        Match {
            times: Some(1),
            ..reads(Opcode::Read)
        },
        Fault::Delay(Duration::from_secs(10)),
    );
    std::fs::write(dir.path().join("armed"), b"").unwrap();
    if !served.faults.wait_applied(hold, 1, Duration::from_secs(10)) {
        end(&mut child, "the child's fill READ never came");
    }
    std::fs::write(dir.path().join("go"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                end(&mut child, "the child never exited: a fault at exit hung")
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    served.faults.release();
    assert_eq!(
        status.code().map(|c| c as u32),
        Some(EXIT_FILLED),
        "the child's exit-time reads ({status})"
    );
}

// ---- demand-paged: under the loader lock ---------------------------------

/// Exit code of the loader-lock test when its section never came back.
const HUNG_UNDER_LOADER_LOCK: u32 = 0x0E0F_0004;

/// The first demand-paged section is created, and a chunk of it faulted in,
/// by a thread holding the loader lock, as a `DllMain` would: both are
/// filled (READ, wire.md 3.4.4) and return. The fill workers start with
/// that section, and a thread started under the loader lock cannot run
/// until it is released, so no fill there may wait on one.
#[test]
fn a_lazy_section_created_under_the_loader_lock_fills() {
    isolate!();
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    type Lock = unsafe extern "system" fn(u32, *mut u32, *mut usize) -> i32;
    type Unlock = unsafe extern "system" fn(u32, usize) -> i32;
    type Export = unsafe extern "system" fn() -> isize;
    let want = pattern(LAZY);
    let (root, _served) = serve(Fake::new().with(FILE, want.clone(), ReadStyle::Whole), true);
    // SAFETY: ntdll exports cast to their signatures.
    let (lock, unlock) = unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        let f = |n: &core::ffi::CStr| GetProcAddress(ntdll, n.as_ptr().cast()).expect("export");
        (
            core::mem::transmute::<Export, Lock>(f(c"LdrLockLoaderLock")),
            core::mem::transmute::<Export, Unlock>(f(c"LdrUnlockLoaderLock")),
        )
    };
    // Started before the lock is taken: a hang ends the process, without
    // `ExitProcess`, which would wait for the lock.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(20));
        // SAFETY: plain FFI.
        unsafe { TerminateProcess(GetCurrentProcess(), HUNG_UNDER_LOADER_LOCK) };
    });
    let mut cookie = 0usize;
    // SAFETY: takes the loader lock, released below on this thread.
    assert_eq!(unsafe { lock(0, core::ptr::null_mut(), &mut cookie) }, 0);
    let file = open(&root.join(FILE), GENERIC_READ);
    let section = mapping(file, PAGE_READONLY);
    let read = section.map(|s| {
        let v = view(s);
        (bytes(v, 0, PAGE), bytes(v, TOUCH, PAGE), v)
    });
    // SAFETY: the cookie the lock gave.
    assert_eq!(unsafe { unlock(0, cookie) }, 0);
    let (warm, touched, v) = read.expect("CreateFileMappingW");
    assert!(warm == want[..PAGE], "the warm window's bytes");
    assert!(
        touched == want[TOUCH..TOUCH + PAGE],
        "the faulted chunk's bytes"
    );
    unmap(v);
    close(section.unwrap());
    close(file);
}
