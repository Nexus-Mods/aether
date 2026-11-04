//! The gap hunt's suspected bugs in the file calls (B3, B4, B5, B6, B8) and
//! the shim tests that go with them (S2, S4, S5, S9, S12), each against a
//! director answer `docs/wire.md` allows:
//!
//! - MKDIR answered `ST_OK` on an existing directory (3.4.9): a create still
//!   reports the collision, an open-if reports the open (B3, S5).
//! - READDIR failing, overflowing or answered "not found" (3.4.2), and a
//!   large listing resumed over many small buffers (B4, S2).
//! - A director that issues `fh` 0 (3.4.3): a new directory's handle closes
//!   nothing at the director (B5, S9).
//! - Reading and truncating through a handle opened with `OPEN_WRITE` (3.4.3,
//!   B6, S12).
//! - A write of several chunks, one of which fails, and `ST_NO_SPACE`
//!   (3.4.5, B8, S4).
#![allow(unsafe_code)] // Win32 and ntdll calls

use crate::fakedirector::{self, Fake, Fault, Match, Options, ReadStyle};
use crate::ntapi::{abs_name, nt_open_dir_abs, ObjectAttributes, UnicodeString};
use aether_protocol::{Opcode, OpenFlags, Reply, Request, Status};
use core::ffi::c_void;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_DISK_FULL, ERROR_GEN_FAILURE,
    ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FindClose, FindFirstFileW, FindNextFileW, GetFileSizeEx,
    ReadFile, SetEndOfFile, SetFilePointerEx, WriteFile, CREATE_ALWAYS, FILE_BEGIN, FILE_CURRENT,
    FILE_SHARE_READ, OPEN_EXISTING, WIN32_FIND_DATAW,
};

#[repr(C)]
struct IoStatusBlock {
    status: isize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        iosb: *mut IoStatusBlock,
        alloc: *const i64,
        attrs: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_len: u32,
    ) -> i32;
    fn NtQueryDirectoryFile(
        handle: HANDLE,
        event: HANDLE,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut IoStatusBlock,
        info: *mut c_void,
        length: u32,
        class: u32,
        single: u8,
        name: *const UnicodeString,
        restart: u8,
    ) -> i32;
    fn NtWriteFile(
        handle: HANDLE,
        event: HANDLE,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut IoStatusBlock,
        buffer: *const c_void,
        length: u32,
        offset: *const i64,
        key: *const u32,
    ) -> i32;
}

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_OPEN_IF: u32 = 3;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_OPENED: usize = 1;
const FILE_DIRECTORY_INFORMATION: u32 = 1;
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;

/// The ring's overflow status (wire.md 2.5, step 3).
const OVERFLOW: i32 = i32::MIN;

/// What one WRITE carries at the default `payload_cap` (4096 - 24).
const CHUNK: usize = 4072;

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

fn on(op: Opcode) -> Match {
    Match {
        op: Some(op),
        ..Match::default()
    }
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error value.
    unsafe { GetLastError() }
}

/// `CreateDirectoryW(p)`: `Ok` or the Win32 error.
fn create_directory(p: &Path) -> Result<(), u32> {
    let w = wide(p);
    // SAFETY: a NUL-terminated name, no security attributes.
    if unsafe { CreateDirectoryW(w.as_ptr(), core::ptr::null()) } != 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

fn create_file(p: &Path, access: u32, disposition: u32) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ,
            core::ptr::null(),
            disposition,
            0,
            core::ptr::null_mut(),
        )
    }
}

/// `ReadFile` of up to `n` bytes: the bytes, or the Win32 error.
fn read(h: HANDLE, n: usize) -> Result<Vec<u8>, u32> {
    let mut buf = vec![0u8; n];
    let mut got = 0u32;
    // SAFETY: `buf` holds `n` bytes; synchronous, no OVERLAPPED.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            n as u32,
            &mut got,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    buf.truncate(got as usize);
    Ok(buf)
}

/// `WriteFile` at the file pointer: `(ok, bytes written, Win32 error)`.
fn write(h: HANDLE, data: &[u8]) -> (bool, u32, u32) {
    let mut put = u32::MAX;
    // SAFETY: `data` is readable for its length; synchronous, no OVERLAPPED.
    let ok = unsafe {
        WriteFile(
            h,
            data.as_ptr(),
            data.len() as u32,
            &mut put,
            core::ptr::null_mut(),
        )
    } != 0;
    (ok, put, if ok { 0 } else { last_error() })
}

/// `NtWriteFile` at the file pointer: `(status, IoStatus.Information)`.
fn nt_write(h: HANDLE, data: &[u8]) -> (i32, usize) {
    let mut iosb = IoStatusBlock {
        status: 0,
        information: usize::MAX,
    };
    // SAFETY: `data` is readable for its length; synchronous handle, no
    // event or APC, the current position.
    let st = unsafe {
        NtWriteFile(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            data.as_ptr().cast(),
            data.len() as u32,
            core::ptr::null(),
            core::ptr::null(),
        )
    };
    (st, iosb.information)
}

fn seek(h: HANDLE, to: i64, how: u32) -> i64 {
    let mut at = -1i64;
    // SAFETY: a handle this test opened.
    assert_ne!(unsafe { SetFilePointerEx(h, to, &mut at, how) }, 0);
    at
}

fn size_of(h: HANDLE) -> i64 {
    let mut size = -1i64;
    // SAFETY: a handle this test opened.
    assert_ne!(unsafe { GetFileSizeEx(h, &mut size) }, 0);
    size
}

fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

/// `FindFirstFileW(dir\*)` then `FindNextFileW` to the end: the names, or
/// the first call's Win32 error.
fn find_all(dir: &Path) -> Result<Vec<String>, u32> {
    let w = wide(&dir.join("*"));
    let mut data: WIN32_FIND_DATAW = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated pattern and a find-data buffer.
    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let name = |d: &WIN32_FIND_DATAW| {
        let n = d.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
        String::from_utf16_lossy(&d.cFileName[..n])
    };
    let mut out = vec![name(&data)];
    // SAFETY: the find handle from above, closed once below.
    while unsafe { FindNextFileW(h, &mut data) } != 0 {
        out.push(name(&data));
    }
    // SAFETY: as above.
    unsafe { FindClose(h) };
    Ok(out)
}

/// One `NtQueryDirectoryFile` of `FileDirectoryInformation` into a buffer
/// of `len` bytes: the status and the names it holds.
fn query_dir(h: HANDLE, len: usize, restart: bool) -> (i32, Vec<String>) {
    let mut buf = vec![0u8; len];
    let mut iosb = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: a directory handle this test opened, a buffer of `len`
    // bytes, no wildcard.
    let st = unsafe {
        NtQueryDirectoryFile(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            buf.as_mut_ptr().cast(),
            len as u32,
            FILE_DIRECTORY_INFORMATION,
            0,
            core::ptr::null(),
            restart as u8,
        )
    };
    let mut names = Vec::new();
    if st >= 0 && iosb.information > 0 {
        // FILE_DIRECTORY_INFORMATION: NextEntryOffset@0, FileNameLength@60,
        // FileName@64.
        let mut off = 0usize;
        loop {
            let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let n = u32::from_le_bytes(buf[off + 60..off + 64].try_into().unwrap()) as usize;
            let units: Vec<u16> = buf[off + 64..off + 64 + n]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            names.push(String::from_utf16_lossy(&units));
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    (st, names)
}

/// The names in a recorded READDIR reply, in the director's order.
fn listed(r: &aether_refdirector::harness::Recorded) -> Vec<String> {
    match r.reply() {
        Some(Reply::ReadDir(list)) => list.iter().map(|e| e.name.to_string()).collect(),
        other => panic!("not a READDIR listing: {other:?}"),
    }
}

// ---- B3, S5: MKDIR answered OK on an existing directory -------------------

/// A writable `save/` holding the directory `save/sub`, whose MKDIR the
/// director answers `ST_OK` (Cairn's writable providers, wire.md 3.4.9).
fn existing_dir_ok() -> Fake {
    Fake::new()
        .with_dir("save/sub")
        .writable_under("save/")
        .mkdir_existing_ok()
}

#[test]
fn create_directory_of_an_existing_directory_fails_even_when_mkdir_answers_ok() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, existing_dir_ok(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let made = create_directory(&root.join("save").join("sub"));

    assert_eq!(made, Err(ERROR_ALREADY_EXISTS));
    let ops = served.recorder.ops_since(mark);
    assert!(ops.contains(&Some(Opcode::GetAttr)), "{ops:?}");
    assert!(!ops.contains(&Some(Opcode::Mkdir)), "{ops:?}");
}

#[test]
fn open_if_of_an_existing_directory_reports_opened() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, existing_dir_ok(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let name = abs_name(&root.join("save").join("sub").to_string_lossy());
    let mut h: HANDLE = core::ptr::null_mut();
    let mut iosb = IoStatusBlock {
        status: 0,
        information: usize::MAX,
    };
    // SAFETY: an OBJECT_ATTRIBUTES naming an absolute path, an IO status
    // block, no EA.
    let st = unsafe {
        NtCreateFile(
            &mut h,
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            &name.oa,
            &mut iosb,
            core::ptr::null(),
            0,
            7,
            FILE_OPEN_IF,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            core::ptr::null(),
            0,
        )
    };

    assert_eq!(st, 0, "NtCreateFile: {st:#x}");
    assert_eq!(iosb.information, FILE_OPENED);
    close(h);
    let ops = served.recorder.ops_since(mark);
    assert!(!ops.contains(&Some(Opcode::Mkdir)), "{ops:?}");
}

#[test]
fn create_directory_of_a_new_name_still_costs_getattr_then_mkdir() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, existing_dir_ok(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    assert_eq!(create_directory(&root.join("save").join("New")), Ok(()));

    assert_eq!(
        served.recorder.ops_since(mark),
        [Some(Opcode::GetAttr), Some(Opcode::Mkdir)]
    );
    assert!(served.fake.contents("save/new").is_none(), "a directory");
    // The directory is there, as created.
    assert_eq!(
        create_directory(&root.join("save").join("new")),
        Err(ERROR_ALREADY_EXISTS)
    );
}

/// The GETATTR before MKDIR names the path folded, as every request but one
/// that creates a name does (wire.md 3.2); MKDIR keeps the caller's spelling.
#[test]
fn the_getattr_before_mkdir_sends_the_folded_path() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, existing_dir_ok(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    for (name, made) in [("Sub", false), ("NewDir", true)] {
        let mark = served.recorder.mark();
        let got = create_directory(&root.join("Save").join(name));
        assert_eq!(got.is_ok(), made, "{name}: {got:?}");
        let sent: Vec<(Opcode, String)> = served
            .recorder
            .since(mark)
            .iter()
            .filter_map(|r| match r.request()? {
                Request::GetAttr { path, .. } => Some((Opcode::GetAttr, path.to_string())),
                Request::Mkdir { path, .. } => Some((Opcode::Mkdir, path.to_string())),
                _ => None,
            })
            .collect();
        let folded = format!("save/{}", name.to_lowercase());
        let mut want = vec![(Opcode::GetAttr, folded)];
        if made {
            want.push((Opcode::Mkdir, format!("Save/{name}")));
        }
        assert_eq!(sent, want, "{name}");
    }
}

// ---- B4, S2: READDIR failing, overflowing, vanished; large listings -------

/// `save/` with three files, and an empty `empty/`.
fn three_files() -> Fake {
    ["a.txt", "b.txt", "c.txt"]
        .iter()
        .fold(Fake::new().with_dir("save").with_dir("empty"), |f, n| {
            f.with(&format!("save/{n}"), b"x".to_vec(), ReadStyle::Whole)
        })
}

#[test]
fn a_readdir_that_fails_fails_the_enumeration() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, three_files(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let fault = served
        .faults
        .arm(on(Opcode::ReadDir), Fault::Status(Status::IoError));

    let found = find_all(&root.join("save"));

    assert_eq!(served.faults.applied(fault), 1);
    assert_eq!(found, Err(ERROR_GEN_FAILURE));
}

#[test]
fn an_overflowing_listing_fails_the_enumeration() {
    isolate!();
    let root = root();
    // About 50 bytes an entry: 200 pass the 4 KiB cap.
    let fake = (0..200).fold(Fake::new().with_dir("save"), |f, i| {
        f.with(
            &format!("save/a-name-of-some-length-{i:04}.txt"),
            b"x".to_vec(),
            ReadStyle::Whole,
        )
    });
    let served = fakedirector::install_with(&root, fake, Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let found = find_all(&root.join("save"));

    let seen = served.recorder.since(mark);
    let readdir: Vec<_> = seen
        .iter()
        .filter(|r| r.op() == Some(Opcode::ReadDir))
        .collect();
    assert_eq!(readdir.len(), 1, "{seen:#?}");
    assert_eq!(readdir[0].status, OVERFLOW, "the overflow reached the shim");
    assert_eq!(found, Err(ERROR_GEN_FAILURE));
}

#[test]
fn a_readdir_of_a_vanished_directory_is_empty() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, three_files(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let fault = served
        .faults
        .arm(on(Opcode::ReadDir), Fault::Status(Status::NotFound));

    let found = find_all(&root.join("save"));
    assert_eq!(served.faults.applied(fault), 1);
    served.faults.clear();
    let empty = find_all(&root.join("empty"));

    assert_eq!(found, empty, "as for an empty directory");
    // Today's answer for both: the first query is STATUS_NO_MORE_FILES
    // (ERROR_NO_MORE_FILES), where NTFS answers STATUS_NO_SUCH_FILE
    // (ERROR_FILE_NOT_FOUND) for a search that finds nothing.
    assert_eq!(found, Err(ERROR_NO_MORE_FILES));
}

#[test]
fn a_failed_listing_is_asked_again() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, three_files(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let dir = root.join("save");
    let once = Match {
        times: Some(1),
        ..on(Opcode::ReadDir)
    };
    let fault = served
        .faults
        .arm(once.clone(), Fault::Status(Status::IoError));

    // On one handle: the failed query keeps nothing, so the next one, with
    // no restart, asks the director again.
    let (st, h) = nt_open_dir_abs(&dir.to_string_lossy(), FILE_LIST_DIRECTORY);
    assert_eq!(st, 0, "open {st:#x}");
    let h: HANDLE = h;
    let mark = served.recorder.mark();
    let (first, _) = query_dir(h, 4096, false);
    let (second, names) = query_dir(h, 4096, false);
    let (third, _) = query_dir(h, 4096, false);
    close(h);
    assert_eq!(served.faults.applied(fault), 1);
    assert_eq!(first, STATUS_UNSUCCESSFUL);
    assert_eq!(
        (second, names),
        (0, vec!["a.txt".into(), "b.txt".into(), "c.txt".into()])
    );
    assert_eq!(third, STATUS_NO_MORE_FILES);
    assert_eq!(served.recorder.count_since(mark, Opcode::ReadDir), 2);

    // And through Win32: a failed FindFirstFileW, then one that lists all.
    let again = served.faults.arm(once, Fault::Status(Status::IoError));
    assert_eq!(find_all(&dir), Err(ERROR_GEN_FAILURE));
    assert_eq!(served.faults.applied(again), 1);
    assert_eq!(
        find_all(&dir).map(|mut n| {
            n.sort();
            n
        }),
        Ok(vec!["a.txt".into(), "b.txt".into(), "c.txt".into()])
    );
}

#[test]
fn a_listing_of_3000_entries_is_resumed_over_many_buffers_complete_and_in_order() {
    isolate!();
    let root = root();
    let fake = (0..3000).fold(Fake::new().with_dir("big"), |f, i| {
        // Not in name order, so the director's order is its own.
        f.with(
            &format!("big/entry-{:04}.dat", (i * 7919) % 3000),
            Vec::new(),
            ReadStyle::Whole,
        )
    });
    let served = fakedirector::install_with(
        &root,
        fake,
        Options {
            payload_cap: 1024 * 1024,
            ..Options::default()
        },
    );
    let _hooks = aether_hooks::install().expect("install");
    let (st, h) = nt_open_dir_abs(&root.join("big").to_string_lossy(), FILE_LIST_DIRECTORY);
    assert_eq!(st, 0, "open {st:#x}");
    let h: HANDLE = h;
    let mark = served.recorder.mark();

    let mut names = Vec::new();
    let mut calls = 0;
    loop {
        let (st, got) = query_dir(h, 4096, calls == 0);
        calls += 1;
        if st == STATUS_NO_MORE_FILES {
            break;
        }
        assert_eq!(st, 0, "query {calls}: {st:#x}");
        assert!(!got.is_empty());
        names.extend(got);
    }
    close(h);

    let seen = served.recorder.since(mark);
    let readdir: Vec<_> = seen
        .iter()
        .filter(|r| r.op() == Some(Opcode::ReadDir))
        .collect();
    assert_eq!(readdir.len(), 1, "one READDIR for the whole walk");
    let sent = listed(readdir[0]);
    assert_eq!(sent.len(), 3000);
    assert!(calls > 50, "{calls} buffers");
    assert_eq!(names, sent, "every name once, in the director's order");
}

// ---- B5, S9: a director that issues fh 0 ---------------------------------

#[test]
fn closing_a_new_directory_sends_no_close() {
    isolate!();
    let root = root();
    let fake = Fake::new()
        .with_dir("save")
        .with("save/a.txt", b"hello".to_vec(), ReadStyle::Whole)
        .writable_under("save/")
        .with_fh_base(0);
    let served = fakedirector::install_with(&root, fake, Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let file = create_file(
        &root.join("save").join("a.txt"),
        GENERIC_READ,
        OPEN_EXISTING,
    );
    assert_ne!(file, INVALID_HANDLE_VALUE, "CreateFileW");
    let opened = served.recorder.since(mark);
    match opened.last().and_then(|r| r.reply()) {
        Some(Reply::Open(o)) => assert_eq!(o.fh, 0, "the director issued fh 0"),
        other => panic!("not an OPEN reply: {other:?}"),
    }
    let mark = served.recorder.mark();

    assert_eq!(create_directory(&root.join("save").join("new")), Ok(()));
    let ops = served.recorder.ops_since(mark);
    assert!(!ops.contains(&Some(Opcode::Close)), "{ops:?}");

    assert_eq!(read(file, 100), Ok(b"hello".to_vec()));
    close(file);
    assert_eq!(served.fake.tally.closes("save/a.txt"), 1);
}

// ---- B6, S12: reading through a write handle ------------------------------

#[test]
fn a_read_write_handle_reads_back_what_it_wrote() {
    isolate!();
    let root = root();
    let fake = Fake::new().with_dir("save").writable_under("save/");
    let served = fakedirector::install_with(&root, fake, Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let h = create_file(
        &root.join("save").join("game.sav"),
        GENERIC_READ | GENERIC_WRITE,
        CREATE_ALWAYS,
    );
    assert_ne!(h, INVALID_HANDLE_VALUE, "CreateFileW");
    assert_eq!(write(h, b"saved bytes"), (true, 11, 0));
    assert_eq!(seek(h, 0, FILE_BEGIN), 0);
    assert_eq!(read(h, 100), Ok(b"saved bytes".to_vec()));
    assert_eq!(seek(h, 5, FILE_BEGIN), 5);
    // SAFETY: a handle this test opened.
    assert_ne!(unsafe { SetEndOfFile(h) }, 0, "SetEndOfFile");
    assert_eq!(size_of(h), 5);
    assert_eq!(seek(h, 0, FILE_BEGIN), 0);
    assert_eq!(read(h, 100), Ok(b"saved".to_vec()));
    close(h);

    let seen = served.recorder.since(mark);
    let open = seen
        .iter()
        .find_map(|r| match r.request() {
            Some(Request::Open { flags, .. }) => Some(flags),
            _ => None,
        })
        .expect("an OPEN");
    assert_eq!(
        open,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNC,
        "no OPEN_READ: the write open's flags"
    );
    assert!(seen.iter().any(|r| r.op() == Some(Opcode::Read)));
    assert!(seen.iter().any(|r| r.op() == Some(Opcode::SetAttr)));
    assert_eq!(
        served.fake.contents("save/game.sav"),
        Some(b"saved".to_vec())
    );
}

// ---- B8, S4: writes of several chunks -------------------------------------

/// A writable `save/` and an open `save/out.bin`, made empty
/// (`CREATE_ALWAYS`, read and write).
fn writable_out(opts: Options) -> (&'static fakedirector::Served, HANDLE) {
    let root = root();
    let fake = Fake::new().with_dir("save").writable_under("save/");
    let served = fakedirector::install_with(&root, fake, opts);
    std::mem::forget(aether_hooks::install().expect("install"));
    let h = create_file(
        &root.join("save").join("out.bin"),
        GENERIC_READ | GENERIC_WRITE,
        CREATE_ALWAYS,
    );
    assert_ne!(h, INVALID_HANDLE_VALUE, "CreateFileW");
    (served, h)
}

/// The offsets of the WRITEs recorded since `mark`.
fn write_offsets(
    served: &fakedirector::Served,
    mark: aether_refdirector::harness::Mark,
) -> Vec<u64> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request() {
            Some(Request::Write { offset, .. }) => Some(offset),
            _ => None,
        })
        .collect()
}

#[test]
fn a_write_of_three_chunks_and_a_byte_sends_four_writes_at_the_right_offsets() {
    isolate!();
    let (served, h) = writable_out(Options::default());
    let data = fakedirector::pattern(3 * CHUNK + 1);
    let mark = served.recorder.mark();

    assert_eq!(write(h, &data), (true, data.len() as u32, 0));

    assert_eq!(write_offsets(served, mark), [0, 4072, 8144, 12216]);
    assert_eq!(seek(h, 0, FILE_CURRENT), data.len() as i64);
    assert_eq!(size_of(h), data.len() as i64);
    close(h);
    assert_eq!(served.fake.contents("save/out.bin"), Some(data));
}

#[test]
fn a_later_chunk_failing_reports_what_landed() {
    isolate!();
    let (served, h) = writable_out(Options::default());
    let data = fakedirector::pattern(3 * CHUNK + 1);
    let second = served.faults.arm(
        Match {
            nth: Some(2),
            ..on(Opcode::Write)
        },
        Fault::Status(Status::IoError),
    );
    let mark = served.recorder.mark();

    let (st, information) = nt_write(h, &data);

    assert_eq!(served.faults.applied(second), 1);
    assert_eq!(
        write_offsets(served, mark),
        [0, 4072],
        "no chunk after the failure"
    );
    assert_eq!(st, STATUS_UNSUCCESSFUL);
    assert_eq!(information, CHUNK, "IoStatus.Information: what landed");
    assert_eq!(seek(h, 0, FILE_CURRENT), CHUNK as i64);
    assert_eq!(size_of(h), CHUNK as i64);

    // WriteFile, again failing at its second chunk: the error, and the
    // pointer past what landed.
    let third = served.faults.arm(
        Match {
            nth: Some(2),
            ..on(Opcode::Write)
        },
        Fault::Status(Status::IoError),
    );
    let (ok, put, err) = write(h, &data);
    // KernelBase reports 0 written for an error status whatever
    // IoStatus.Information holds: the count is NtWriteFile's (above).
    assert_eq!(put, 0, "lpNumberOfBytesWritten");
    assert_eq!(served.faults.applied(third), 1);
    assert!(!ok);
    assert_eq!(err, ERROR_GEN_FAILURE);
    assert_eq!(seek(h, 0, FILE_CURRENT), 2 * CHUNK as i64);
    assert_eq!(size_of(h), 2 * CHUNK as i64);
    close(h);
    assert_eq!(
        served.fake.contents("save/out.bin"),
        Some([&data[..CHUNK], &data[..CHUNK]].concat())
    );
}

#[test]
fn no_space_is_disk_full() {
    isolate!();
    let (served, h) = writable_out(Options::default());
    let full = served
        .faults
        .arm(on(Opcode::Write), Fault::Status(Status::NoSpace));

    let (ok, _, err) = write(h, b"does not fit");

    assert_eq!(served.faults.applied(full), 1);
    assert!(!ok);
    assert_eq!(err, ERROR_DISK_FULL);
    assert_eq!(seek(h, 0, FILE_CURRENT), 0);
    assert_eq!(size_of(h), 0);
    close(h);
}

/// A WRITE reply claiming 100 bytes more than the request carried.
fn written_past_the_chunk(r: &mut aether_refdirector::harness::OwnedReplyBytes) {
    let len = u32::from_le_bytes(r.request[16..20].try_into().unwrap());
    r.payload[..4].copy_from_slice(&(len + 100).to_le_bytes());
}

#[test]
fn a_written_count_past_the_chunk_is_not_believed() {
    isolate!();
    let (served, h) = writable_out(Options::default());
    let lie = served
        .faults
        .arm(on(Opcode::Write), Fault::Rewrite(written_past_the_chunk));

    let (st, information) = nt_write(h, b"ten bytes!");

    assert_eq!(served.faults.applied(lie), 1);
    assert_eq!(st, STATUS_UNSUCCESSFUL);
    assert_eq!(information, 0);
    assert_eq!(
        seek(h, 0, FILE_CURRENT),
        0,
        "the pointer is not past the data"
    );
    assert!(size_of(h) <= 10, "the size is not past the data");
    close(h);
}
