//! Sections over director files (S6), with the reference behind the ring:
//! a data section filled whole at `CreateFileMappingW`, one filled on demand
//! (above `EAGER_MAX`, here lowered to [`EAGER_MAX`] through
//! `aether_hooks::set_eager_section_max` so no test needs a 256 MiB file),
//! and an image section of a served PE.
//!
//! The gap hunt's suspected bugs:
//!
//! - B1: a demand-paged section keeps reading through its file's director
//!   handle after the program closed the file. CLOSE goes when the last of
//!   file handle, section and views goes, once (wire.md 3.4.10).
//! - B9: a fill that fails, or comes back short of what the file holds there
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
fn a_failed_lazy_fill_is_retried_at_the_next_touch() {
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
