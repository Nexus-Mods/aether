//! What each hooked call costs, counted, against a record (`costs.txt`):
//! the requests it sends the director (round trips), the bytes they move
//! over the ring, and the allocations the shim makes on the calling thread.
//!
//! # The record
//!
//! `costs.txt` holds one line per case, sorted by case name:
//!
//! ```text
//! <case> round_trips=<n> ring_bytes=<n> allocs<=<n>
//! ```
//!
//! [`check`] compares a case's measured counts with its line:
//!
//! - `round_trips` and `ring_bytes` must equal the record. They depend on
//!   what the shim sends and what the reference answers, not on the
//!   machine: every path on the ring is relative to its root, and every
//!   name, size and time the tests serve is fixed.
//! - `allocs` must be at most the record. The count is the global
//!   allocator's on the test's own thread (`aether_testkit::alloc`), so it
//!   leaves out the director's serve threads and any thread of the shim's
//!   own. It can move with the machine (a longer path can take one more
//!   reallocation), so only an increase fails. A decrease passes and prints
//!   the command that rewrites the record, so the record follows a saving.
//!
//! A count that differs fails with the recorded and the measured value. A
//! deliberate change of a cost rewrites the record in the same commit:
//!
//! ```text
//! AETHER_COSTS_UPDATE=1 cargo test -p aether-hooks --test harness costs::
//! ```
//!
//! Each case runs in its own process (`isolate!`). Under
//! `AETHER_COSTS_UPDATE=1` the child does not compare: it writes its
//! measured line to its standard output, and the parent test process,
//! which reads that output, merges the line into `costs.txt` (one file
//! write at a time, under a lock). The rewrite is refused where `CI` is
//! set: the record is a developer's to change. Without the switch,
//! [`every_case_has_one_line_in_the_record`] keeps the record and the cases
//! in step.
//!
//! # What a case measures
//!
//! A case sets up the director and the hooks, makes the call once untimed
//! (so one-time state is filled: lazily built tables, the first use of a
//! cache's storage), and measures the second call. A case whose point is
//! the first call ends in `_cold` and measures that one. Everything the
//! measured call needs (names, buffers) is built before it, so the test's
//! own allocations are not counted.
//!
//! A case is a Win32 call where the NT calls it makes are fixed: one call,
//! or an open, one call and a close. Where the host decides how many NT
//! calls a Win32 call makes, the case is the NT calls themselves:
//!
//! - `GetFinalPathNameByHandleW` asks `NtQueryInformationFile` and
//!   `NtQueryObject` as often, and with what buffers, as the host's
//!   kernelbase chooses (about three calls on Windows 11). Its cases are
//!   one `NtQueryInformationFile(FileNameInformation)`, the call the shim
//!   answers from the spelling cache.
//! - `FindFirstFileW` and `FindNextFileW` ask `NtQueryDirectoryFile` as
//!   often as their buffer needs. The shim lists a directory once per handle
//!   (one READDIR), so the round trips would not move with the host, but each
//!   query allocates. `list_100_entries` is the open, the queries with a
//!   fixed 4 KiB buffer to `STATUS_NO_MORE_FILES`, and the close;
//!   `list_next_entry` is one query after the first.
//!
//! The process start (`CreateProcessW`) is hooked at
//! `CreateProcessInternalW`, and every request it sends is the shim's own.
//!
//! # Notes on rows
//!
//! - `create_directory`: GETATTR, then MKDIR (the shim asks first, so an
//!   existing directory fails `ERROR_ALREADY_EXISTS` whatever MKDIR says).
//! - `final_path_name_not_stored`: a director that answers STORED_NAMES
//!   `ST_NOT_FOUND`. The shim keeps nothing from a failure, so every query
//!   asks again: one round trip per NT call.
//! - `section_lazy_page_fill`: the fill runs on the shim's fill thread, so
//!   its allocations are not counted; the faulting thread's are (it hands
//!   the job over and waits).
//! - `create_process`: GETATTR and STAGE_EXEC for the image. The child
//!   starts in the root's backing directory, which the shim makes and the
//!   host checks on disk, never through the director. The STAGE_EXEC reply
//!   is the staged copy's real path, whose length is the machine's (its
//!   temporary folder); that length is left out of the case's
//!   `ring_bytes`.
//! - `reg_*`: the overlay key's name carries the process id, zero-padded to
//!   ten digits so that its length is the same in every process.
#![allow(unsafe_code)] // Win32 and ntdll calls

use crate::child_start::{self, Given, IMAGE};
use crate::common::run_isolated_output;
use crate::fakedirector::{pattern, Fake, Fault, Match, Options, ReadStyle, Served};
use crate::fileops::{self, close, create, open_read, wide};
use crate::reg;
use aether_protocol::{Opcode, Status};
use core::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{
    ERROR_PROCESS_ABORTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, DeleteFileW, GetFileAttributesExW, GetFileExInfoStandard, MoveFileExW,
    ReadFile, WriteFile, FILE_FLAG_BACKUP_SEMANTICS, OPEN_EXISTING, WIN32_FILE_ATTRIBUTE_DATA,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_DWORD, REG_OPTION_NON_VOLATILE,
};

// ---- the record ----------------------------------------------------------

/// Every case, sorted: one `#[test]` each, one line each in `costs.txt`.
const CASES: &[&str] = &[
    "attributes_in_root",
    "attributes_outside_root",
    "close",
    "create_directory",
    "create_process",
    "delete",
    "final_path_name_cold",
    "final_path_name_not_stored",
    "final_path_name_warm",
    "list_100_entries",
    "list_next_entry",
    "move",
    "open_missing",
    "open_outside_root",
    "open_read_root",
    "open_write_writable",
    "query_key_not_a_key",
    "query_size_class",
    "read_bulk_1_mib",
    "read_small_cached",
    "read_small_cached_cold",
    "read_small_mutable",
    "read_stream_5_mib",
    "reg_query_overlay_cold",
    "reg_query_overlay_warm",
    "section_eager_1_mib",
    "section_lazy_page_fill",
    "write_4_kib",
    "write_three_chunks_and_a_byte",
];

/// The record as this binary was built with it.
const RECORD: &str = include_str!("costs.txt");

/// The record's file, which `AETHER_COSTS_UPDATE=1` rewrites.
const RECORD_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/harness/costs.txt");

/// What the record starts with: how to read and rewrite it.
const HEADER: &str = "\
# What each hooked call costs: requests to the director, bytes over the ring,
# allocations on the calling thread. Checked by tests/harness/costs.rs, which
# says what each case measures. round_trips and ring_bytes must match exactly;
# allocs may not go up. To rewrite it after a deliberate change, in the same
# commit:
#   AETHER_COSTS_UPDATE=1 cargo test -p aether-hooks --test harness costs::
";

/// The marker a child's line to its parent starts with.
const ROW: &str = "aether-costs-row: ";
const LOWER: &str = "aether-costs-lower: ";
const SENT: &str = "aether-costs-sent: ";

/// One case's counts, and (measured, not recorded) the opcodes it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Measured {
    round_trips: u64,
    ring_bytes: u64,
    allocs: u64,
    sent: Vec<Option<Opcode>>,
}

impl Measured {
    fn line(&self, case: &str) -> String {
        format!(
            "{case} round_trips={} ring_bytes={} allocs<={}",
            self.round_trips, self.ring_bytes, self.allocs
        )
    }
}

/// A record line, or `None` for a comment, a blank or a malformed line.
fn parse(line: &str) -> Option<(&str, Measured)> {
    let mut words = line.split_whitespace();
    let case = words.next().filter(|w| !w.starts_with('#'))?;
    let mut field = |name: &str| -> Option<u64> { words.next()?.strip_prefix(name)?.parse().ok() };
    let m = Measured {
        round_trips: field("round_trips=")?,
        ring_bytes: field("ring_bytes=")?,
        allocs: field("allocs<=")?,
        sent: Vec::new(),
    };
    Some((case, m))
}

fn recorded(case: &str) -> Option<Measured> {
    RECORD
        .lines()
        .filter_map(parse)
        .find(|(c, _)| *c == case)
        .map(|(_, m)| m)
}

fn updating() -> bool {
    aether_env::opt_in("AETHER_COSTS_UPDATE")
}

/// A line for the parent, past libtest's capture.
fn to_parent(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Compare `m` with the record for `case`, or under `AETHER_COSTS_UPDATE=1`
/// hand it to the parent to record. In the child process.
fn check(case: &str, m: Measured) {
    assert!(CASES.contains(&case), "{case} is not in CASES");
    if updating() {
        to_parent(&format!("{ROW}{}", m.line(case)));
        to_parent(&format!("{SENT}{case}: {:?}", m.sent));
        return;
    }
    let Some(r) = recorded(case) else {
        panic!(
            "{case} has no line in costs.txt; measured `{}`. Record it with \
             AETHER_COSTS_UPDATE=1 cargo test -p aether-hooks --test harness costs::",
            m.line(case)
        );
    };
    let mut wrong = Vec::new();
    if m.round_trips != r.round_trips {
        wrong.push(format!(
            "round_trips was {}, now {}",
            r.round_trips, m.round_trips
        ));
    }
    if m.ring_bytes != r.ring_bytes {
        wrong.push(format!(
            "ring_bytes was {}, now {}",
            r.ring_bytes, m.ring_bytes
        ));
    }
    if m.allocs > r.allocs {
        wrong.push(format!("allocs was at most {}, now {}", r.allocs, m.allocs));
    }
    assert!(
        wrong.is_empty(),
        "{case}: {} (sent {:?}). If the change is meant, rewrite the record: \
         AETHER_COSTS_UPDATE=1 cargo test -p aether-hooks --test harness costs::",
        wrong.join("; "),
        m.sent
    );
    if m.allocs < r.allocs {
        to_parent(&format!(
            "{LOWER}{case}: allocs went down from {} to {}; lower the record: \
             AETHER_COSTS_UPDATE=1 cargo test -p aether-hooks --test harness costs::",
            r.allocs, m.allocs
        ));
    }
}

/// One write of `costs.txt` at a time in the parent.
static MERGE: Mutex<()> = Mutex::new(());

/// In the parent, after the child passed: fold its row into `costs.txt`, or
/// show its note that a count went down.
fn from_child(out: &str) {
    // libtest prints the test's name on the same line, before the marker.
    fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
        line.find(marker).map(|i| &line[i + marker.len()..])
    }
    for line in out.lines() {
        if let Some(note) = after(line, LOWER) {
            // Past the parent's own capture, so a passing run shows it; on
            // GitHub Actions as a notice on the run.
            let mark = if std::env::var_os("GITHUB_ACTIONS").is_some() {
                "::notice title=costs.txt::"
            } else {
                "costs: "
            };
            let _ = writeln!(std::io::stderr(), "{mark}{note}");
        } else if let Some(row) = after(line, ROW) {
            merge(row);
        } else if let Some(sent) = after(line, SENT) {
            let _ = writeln!(std::io::stderr(), "costs: {sent}");
        }
    }
}

fn merge(row: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "AETHER_COSTS_UPDATE=1 under CI: the record is rewritten by hand"
    );
    let (case, _) = parse(row).expect("a well-formed row");
    let _one = MERGE.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::fs::read_to_string(RECORD_PATH).unwrap_or_default();
    let mut rows: Vec<String> = now
        .lines()
        .filter(|l| parse(l).is_some_and(|(c, _)| c != case))
        .map(str::to_string)
        .collect();
    rows.push(row.to_string());
    rows.sort();
    let mut text = HEADER.to_string();
    for r in rows {
        text.push_str(&r);
        text.push('\n');
    }
    std::fs::write(RECORD_PATH, text).expect("write costs.txt");
}

/// `isolate!` for a cost case: the parent also takes the child's row or
/// note ([`from_child`]).
macro_rules! cost_case {
    () => {{
        fn f() {}
        if let Some(out) = run_isolated_output(::std::any::type_name_of_val(&f), None) {
            from_child(&out);
            return;
        }
    }};
}

/// Counts for `f`, made on this thread: the requests the recorder saw and
/// their bytes, and the allocations.
fn measure<R>(served: &Served, f: impl FnOnce() -> R) -> (R, Measured) {
    let mark = served.recorder.mark();
    let (r, allocs) = aether_testkit::alloc::allocations(f);
    let m = Measured {
        round_trips: served.recorder.total_since(mark) as u64,
        ring_bytes: served.recorder.ring_bytes_since(mark),
        allocs: allocs as u64,
        sent: served.recorder.ops_since(mark),
    };
    (r, m)
}

#[test]
fn every_case_has_one_line_in_the_record() {
    let mut sorted = CASES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, CASES, "CASES is sorted, each once");
    if updating() {
        return;
    }
    assert!(
        RECORD.starts_with(HEADER),
        "costs.txt starts with its header"
    );
    let lines: Vec<&str> = RECORD
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    for l in &lines {
        assert!(parse(l).is_some(), "a malformed line in costs.txt: {l:?}");
    }
    let cases: Vec<&str> = lines
        .iter()
        .filter_map(|l| parse(l))
        .map(|(c, _)| c)
        .collect();
    assert_eq!(cases, CASES, "costs.txt has one line per case, sorted");
}

// ---- the calls -------------------------------------------------------------

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// A read the read cache serves (under its 64 KiB block) or, on a mutable
/// file, one inline READ (the 4 KiB ring holds 4088 bytes).
const SMALL: usize = KIB;
/// What one WRITE carries at the default `payload_cap` (4096 - 24).
const CHUNK: usize = 4072;
/// The listing buffer: the 4 KiB `FindFirstFileW` asks with, so a listing
/// takes several queries.
const LIST_BUF: usize = 4096;
/// Entries in `data/list`.
const LISTED: usize = 100;

const FILE_NAME_INFORMATION: u32 = 9;
const FILE_STANDARD_INFORMATION: u32 = 5;
const KEY_BASIC_INFORMATION: u32 = 0;
const STATUS_OBJECT_TYPE_MISMATCH: i32 = 0xC000_0024u32 as i32;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationFile(
        h: HANDLE,
        iosb: *mut [usize; 2],
        info: *mut c_void,
        len: u32,
        class: u32,
    ) -> i32;
    fn NtQueryKey(h: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

/// The tree every file case serves: a read-only `data/` (immutable), a
/// writable `saves/` (mutable).
fn fake() -> Fake {
    let mut fake = Fake::new()
        .with_dir("data")
        .with("data/a.esp", pattern(MIB), ReadStyle::Whole)
        .with("data/b.esp", pattern(MIB), ReadStyle::Whole)
        .with("data/big.bin", pattern(5 * MIB), ReadStyle::Whole)
        .with_dir("data/list")
        .with_dir("saves")
        .with("saves/s.ess", pattern(MIB), ReadStyle::Whole)
        .writable_under("saves/");
    for i in 0..LISTED {
        fake = fake.with(
            &format!("data/list/f{i:03}.esp"),
            pattern(64),
            ReadStyle::Whole,
        );
    }
    for name in ["m0", "m1", "d0", "d1"] {
        fake = fake.with(&format!("saves/{name}.ess"), pattern(64), ReadStyle::Whole);
    }
    fake
}

fn serve() -> (PathBuf, &'static Served) {
    fileops::serve(fake())
}

fn open_write(p: &Path) -> HANDLE {
    create(p, GENERIC_WRITE, OPEN_EXISTING, 0)
        .unwrap_or_else(|e| panic!("open {} to write: {e}", p.display()))
}

/// `CreateFileW(GENERIC_READ)` of a name built beforehand: the handle or
/// `INVALID_HANDLE_VALUE`.
fn open_wide(w: &[u16], access: u32) -> HANDLE {
    // SAFETY: a NUL-terminated name; no security attributes or template.
    unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            w.as_ptr(),
            access,
            fileops::SHARE_ALL,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    }
}

fn read_into(h: HANDLE, buf: &mut [u8]) -> usize {
    let mut got = 0u32;
    // SAFETY: `buf` holds its length; synchronous, no OVERLAPPED.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut got,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "ReadFile: {}", fileops::last_error());
    got as usize
}

/// One `ReadFile` of `buf.len()` bytes at `offset` (one `NtReadFile`): the
/// count it read.
fn read_at(h: HANDLE, offset: u64, buf: &mut [u8]) -> usize {
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: plain data, all-zero is valid.
    let mut ov: OVERLAPPED = unsafe { core::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    let mut got = 0u32;
    // SAFETY: `buf` holds its length; a synchronous handle, so the
    // OVERLAPPED only carries the offset.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut got,
            &mut ov,
        )
    };
    assert_ne!(ok, 0, "ReadFile: {}", fileops::last_error());
    got as usize
}

fn write_all(h: HANDLE, data: &[u8]) {
    let mut put = 0u32;
    // SAFETY: `data` holds its length; synchronous, no OVERLAPPED.
    let ok = unsafe {
        WriteFile(
            h,
            data.as_ptr(),
            data.len() as u32,
            &mut put,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "WriteFile: {}", fileops::last_error());
    assert_eq!(put as usize, data.len(), "WriteFile wrote short");
}

fn attributes(w: &[u16]) -> bool {
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated name and a buffer of the standard class's type.
    unsafe {
        GetFileAttributesExW(
            w.as_ptr(),
            GetFileExInfoStandard,
            (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        ) != 0
    }
}

/// `NtQueryInformationFile(class)` into `buf`: the status.
fn query_info(h: HANDLE, buf: &mut [u8], class: u32) -> i32 {
    let mut iosb = [0usize; 2];
    // SAFETY: `buf` holds its length.
    unsafe {
        NtQueryInformationFile(
            h,
            &mut iosb,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            class,
        )
    }
}

type NtQueryDirectoryFileFn = unsafe extern "system" fn(
    HANDLE,
    HANDLE,
    *const c_void,
    *const c_void,
    *mut [usize; 2],
    *mut c_void,
    u32,
    u32,
    u8,
    *const c_void,
    u8,
) -> i32;

/// `ntdll!NtQueryDirectoryFile`, looked up once before anything is counted
/// (another module declares it with its own types).
fn nt_query_directory_file() -> NtQueryDirectoryFileFn {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: NUL-terminated names; ntdll is always loaded.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        let p = GetProcAddress(ntdll, c"NtQueryDirectoryFile".as_ptr().cast())
            .expect("NtQueryDirectoryFile");
        core::mem::transmute::<unsafe extern "system" fn() -> isize, NtQueryDirectoryFileFn>(p)
    }
}

/// A directory opened to list it: `FILE_LIST_DIRECTORY`, backup semantics.
fn open_dir(w: &[u16]) -> HANDLE {
    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            fileops::SHARE_ALL,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(
        h,
        INVALID_HANDLE_VALUE,
        "open the directory: {}",
        fileops::last_error()
    );
    h
}

/// One `NtQueryDirectoryFile(FileDirectoryInformation)` into `buf`: the
/// status and how many records it wrote.
fn list_once(q: NtQueryDirectoryFileFn, h: HANDLE, buf: &mut [u8], single: bool) -> (i32, usize) {
    const FILE_DIRECTORY_INFORMATION: u32 = 1;
    let mut iosb = [0usize; 2];
    // SAFETY: a directory handle, a buffer of its length, no wildcard.
    let st = unsafe {
        q(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            FILE_DIRECTORY_INFORMATION,
            u8::from(single),
            core::ptr::null(),
            0,
        )
    };
    if st != 0 || iosb[1] == 0 {
        return (st, 0);
    }
    // Records chain by `NextEntryOffset`, the first field; 0 ends the chain.
    let (mut n, mut at) = (1, 0usize);
    loop {
        let next = u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        if next == 0 {
            break (st, n);
        }
        at += next;
        n += 1;
    }
}

/// What `FindFirstFileW`, `FindNextFileW` and `FindClose` do to a directory,
/// at the NT level with a buffer of fixed size: open it, query until
/// `STATUS_NO_MORE_FILES`, close it. How many records came back.
fn list_all(q: NtQueryDirectoryFileFn, w: &[u16], buf: &mut [u8]) -> usize {
    const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;
    let h = open_dir(w);
    let mut total = 0;
    loop {
        let (st, n) = list_once(q, h, buf, false);
        if st == STATUS_NO_MORE_FILES {
            break;
        }
        assert_eq!(st, 0, "NtQueryDirectoryFile: {st:#x}");
        total += n;
    }
    close(h);
    total
}

// ---- opens, attributes, close --------------------------------------------

#[test]
fn open_read_root() {
    cost_case!();
    let (root, served) = serve();
    let w = wide(&root.join("data").join("a.esp"));
    close(open_wide(&w, GENERIC_READ));
    let (h, m) = measure(served, || open_wide(&w, GENERIC_READ));
    assert_ne!(h, INVALID_HANDLE_VALUE);
    close(h);
    check("open_read_root", m);
}

#[test]
fn open_missing() {
    cost_case!();
    let (root, served) = serve();
    let w = wide(&root.join("data").join("missing.esp"));
    assert_eq!(open_wide(&w, GENERIC_READ), INVALID_HANDLE_VALUE);
    let (h, m) = measure(served, || open_wide(&w, GENERIC_READ));
    assert_eq!(h, INVALID_HANDLE_VALUE);
    check("open_missing", m);
}

#[test]
fn open_write_writable() {
    cost_case!();
    let (root, served) = serve();
    let w = wide(&root.join("saves").join("s.ess"));
    close(open_wide(&w, GENERIC_WRITE));
    let (h, m) = measure(served, || open_wide(&w, GENERIC_WRITE));
    assert_ne!(h, INVALID_HANDLE_VALUE);
    close(h);
    check("open_write_writable", m);
}

#[test]
fn close_a_handle() {
    cost_case!();
    let (root, served) = serve();
    let p = root.join("data").join("a.esp");
    close(open_read(&p));
    let h = open_read(&p);
    let ((), m) = measure(served, || close(h));
    check("close", m);
}

#[test]
fn attributes_in_root() {
    cost_case!();
    let (root, served) = serve();
    let w = wide(&root.join("data").join("a.esp"));
    assert!(attributes(&w));
    let (ok, m) = measure(served, || attributes(&w));
    assert!(ok);
    check("attributes_in_root", m);
}

#[test]
fn attributes_outside_root() {
    cost_case!();
    let (_root, served) = serve();
    let outside = aether_testkit::tempdir().expect("temp folder");
    let file = outside.path().join("real.txt");
    std::fs::write(&file, b"on disk").unwrap();
    let w = wide(&file);
    assert!(attributes(&w));
    let (ok, m) = measure(served, || attributes(&w));
    assert!(ok);
    check("attributes_outside_root", m);
}

/// `CreateFileW` and `CloseHandle` of a real file outside every root.
#[test]
fn open_outside_root() {
    cost_case!();
    let (_root, served) = serve();
    let outside = aether_testkit::tempdir().expect("temp folder");
    let file = outside.path().join("real.txt");
    std::fs::write(&file, b"on disk").unwrap();
    let w = wide(&file);
    close(open_wide(&w, GENERIC_READ));
    let tracked = aether_hooks::tracked_handle_count();
    let ((), m) = measure(served, || {
        let h = open_wide(&w, GENERIC_READ);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(
            aether_hooks::tracked_handle_count(),
            tracked,
            "the open recorded the handle"
        );
        close(h);
    });
    check("open_outside_root", m);
}

#[test]
fn query_size_class() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("data").join("a.esp"));
    let mut buf = [0u8; 64];
    assert_eq!(query_info(h, &mut buf, FILE_STANDARD_INFORMATION), 0);
    let (st, m) = measure(served, || {
        query_info(h, &mut buf, FILE_STANDARD_INFORMATION)
    });
    assert_eq!(st, 0);
    // EndOfFile, after AllocationSize.
    assert_eq!(
        i64::from_le_bytes(buf[8..16].try_into().unwrap()),
        MIB as i64
    );
    close(h);
    check("query_size_class", m);
}

// ---- reads -----------------------------------------------------------------

#[test]
fn read_small_cached_cold() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("data").join("a.esp"));
    let mut buf = vec![0u8; SMALL];
    let (n, m) = measure(served, || read_into(h, &mut buf));
    assert_eq!(n, SMALL);
    assert!(buf == pattern(SMALL));
    close(h);
    check("read_small_cached_cold", m);
}

#[test]
fn read_small_cached() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("data").join("a.esp"));
    let mut buf = vec![0u8; SMALL];
    assert_eq!(read_into(h, &mut buf), SMALL);
    let (n, m) = measure(served, || read_into(h, &mut buf));
    assert_eq!(n, SMALL);
    assert!(buf == pattern(2 * SMALL)[SMALL..]);
    close(h);
    check("read_small_cached", m);
}

#[test]
fn read_small_mutable() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("saves").join("s.ess"));
    let mut buf = vec![0u8; SMALL];
    assert_eq!(read_into(h, &mut buf), SMALL);
    let (n, m) = measure(served, || read_into(h, &mut buf));
    assert_eq!(n, SMALL);
    assert!(buf == pattern(2 * SMALL)[SMALL..]);
    close(h);
    check("read_small_mutable", m);
}

#[test]
fn read_bulk_1_mib() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("data").join("a.esp"));
    let mut buf = vec![0u8; MIB];
    assert_eq!(read_at(h, 0, &mut buf), MIB);
    buf.fill(0);
    let (n, m) = measure(served, || read_at(h, 0, &mut buf));
    assert_eq!(n, MIB);
    assert!(buf == pattern(MIB));
    close(h);
    check("read_bulk_1_mib", m);
}

/// S7: one `NtReadFile` of 5 MiB, the stream pipeline's size.
#[test]
fn read_stream_5_mib() {
    cost_case!();
    let (root, served) = serve();
    let h = open_read(&root.join("data").join("big.bin"));
    let mut buf = vec![0u8; 5 * MIB];
    assert_eq!(read_at(h, 0, &mut buf), 5 * MIB);
    buf.fill(0);
    let (n, m) = measure(served, || read_at(h, 0, &mut buf));
    assert_eq!(n, 5 * MIB);
    assert!(buf == pattern(5 * MIB));
    close(h);
    check("read_stream_5_mib", m);
}

// ---- writes ----------------------------------------------------------------

#[test]
fn write_4_kib() {
    cost_case!();
    let (root, served) = serve();
    let h = open_write(&root.join("saves").join("s.ess"));
    let data = pattern(4 * KIB);
    write_all(h, &data);
    let ((), m) = measure(served, || write_all(h, &data));
    close(h);
    check("write_4_kib", m);
}

/// S4: a write the shim splits into three full chunks and one byte.
#[test]
fn write_three_chunks_and_a_byte() {
    cost_case!();
    let (root, served) = serve();
    let h = open_write(&root.join("saves").join("s.ess"));
    let data = pattern(3 * CHUNK + 1);
    write_all(h, &data);
    let ((), m) = measure(served, || write_all(h, &data));
    close(h);
    check("write_three_chunks_and_a_byte", m);
}

// ---- names -----------------------------------------------------------------

/// Listing 100 names: OPEN, READDIR, CLOSE, whatever the buffer.
#[test]
fn list_100_entries() {
    cost_case!();
    let (root, served) = serve();
    let q = nt_query_directory_file();
    let w = wide(&root.join("data").join("list"));
    let mut buf = vec![0u8; LIST_BUF];
    assert_eq!(list_all(q, &w, &mut buf), LISTED);
    let (n, m) = measure(served, || list_all(q, &w, &mut buf));
    assert_eq!(n, LISTED);
    check("list_100_entries", m);
}

/// A query after the first on a listed handle: served from the listing the
/// handle keeps.
#[test]
fn list_next_entry() {
    cost_case!();
    let (root, served) = serve();
    let q = nt_query_directory_file();
    let h = open_dir(&wide(&root.join("data").join("list")));
    let mut buf = vec![0u8; LIST_BUF];
    assert_eq!(list_once(q, h, &mut buf, true), (0, 1));
    let (r, m) = measure(served, || list_once(q, h, &mut buf, true));
    assert_eq!(r, (0, 1));
    close(h);
    check("list_next_entry", m);
}

#[test]
fn move_a_file() {
    cost_case!();
    let (root, served) = serve();
    let at = |n: &str| wide(&root.join("saves").join(n));
    let (m0, m0b, m1, m1b) = (at("m0.ess"), at("m0b.ess"), at("m1.ess"), at("m1b.ess"));
    // SAFETY: NUL-terminated names.
    let mv = |a: &[u16], b: &[u16]| unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) };
    assert_ne!(mv(&m0, &m0b), 0, "MoveFileExW: {}", fileops::last_error());
    let (ok, m) = measure(served, || mv(&m1, &m1b));
    assert_ne!(ok, 0, "MoveFileExW: {}", fileops::last_error());
    check("move", m);
}

#[test]
fn delete_a_file() {
    cost_case!();
    let (root, served) = serve();
    let (d0, d1) = (
        wide(&root.join("saves").join("d0.ess")),
        wide(&root.join("saves").join("d1.ess")),
    );
    // SAFETY: NUL-terminated names.
    let del = |p: &[u16]| unsafe { DeleteFileW(p.as_ptr()) };
    assert_ne!(del(&d0), 0, "DeleteFileW: {}", fileops::last_error());
    let (ok, m) = measure(served, || del(&d1));
    assert_ne!(ok, 0, "DeleteFileW: {}", fileops::last_error());
    check("delete", m);
}

/// Task 3A's B3: GETATTR, then MKDIR.
#[test]
fn create_directory() {
    cost_case!();
    let (root, served) = serve();
    let (n0, n1) = (
        wide(&root.join("saves").join("new0")),
        wide(&root.join("saves").join("new1")),
    );
    // SAFETY: NUL-terminated names; no security attributes.
    let mkdir = |p: &[u16]| unsafe { CreateDirectoryW(p.as_ptr(), core::ptr::null()) };
    assert_ne!(mkdir(&n0), 0, "CreateDirectoryW: {}", fileops::last_error());
    let (ok, m) = measure(served, || mkdir(&n1));
    assert_ne!(ok, 0, "CreateDirectoryW: {}", fileops::last_error());
    check("create_directory", m);
}

/// The spelling `NtQueryInformationFile(FileNameInformation)` gives: `\`
/// and the stored spelling under the root, at its end.
fn file_name(buf: &[u8]) -> String {
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    let units: Vec<u16> = buf[4..4 + len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&units)
}

/// The spelling cache's tree: `Mods/Sub/F.esp`, stored in that case.
fn names_fake() -> Fake {
    Fake::new()
        .with_dir("Mods")
        .with_dir("Mods/Sub")
        .with("Mods/Sub/F.esp", pattern(64), ReadStyle::Whole)
        .writable_under("mods/")
}

/// The first name query on a file: one STORED_NAMES.
#[test]
fn final_path_name_cold() {
    cost_case!();
    let (root, served) = fileops::serve(names_fake());
    let h = open_read(&root.join("mods").join("SUB").join("f.ESP"));
    let mut buf = vec![0u8; 2048];
    let (st, m) = measure(served, || query_info(h, &mut buf, FILE_NAME_INFORMATION));
    assert_eq!(st, 0);
    assert!(
        file_name(&buf).ends_with(r"\Mods\Sub\F.esp"),
        "{}",
        file_name(&buf)
    );
    close(h);
    check("final_path_name_cold", m);
}

/// A repeated name query: answered from the spelling cache.
#[test]
fn final_path_name_warm() {
    cost_case!();
    let (root, served) = fileops::serve(names_fake());
    let h = open_read(&root.join("mods").join("SUB").join("f.ESP"));
    let mut buf = vec![0u8; 2048];
    assert_eq!(query_info(h, &mut buf, FILE_NAME_INFORMATION), 0);
    let (st, m) = measure(served, || query_info(h, &mut buf, FILE_NAME_INFORMATION));
    assert_eq!(st, 0);
    assert!(
        file_name(&buf).ends_with(r"\Mods\Sub\F.esp"),
        "{}",
        file_name(&buf)
    );
    close(h);
    check("final_path_name_warm", m);
}

/// A director that answers every STORED_NAMES `ST_NOT_FOUND`: the caller's
/// spelling, asked again each time.
#[test]
fn final_path_name_not_stored() {
    cost_case!();
    let (root, served) = fileops::serve(names_fake());
    let id = served.faults.arm(
        Match {
            op: Some(Opcode::StoredNames),
            ..Match::default()
        },
        Fault::Status(Status::NotFound),
    );
    let h = open_read(&root.join("mods").join("SUB").join("f.ESP"));
    let mut buf = vec![0u8; 2048];
    assert_eq!(query_info(h, &mut buf, FILE_NAME_INFORMATION), 0);
    let (st, m) = measure(served, || query_info(h, &mut buf, FILE_NAME_INFORMATION));
    assert_eq!(st, 0);
    assert!(
        file_name(&buf).ends_with(r"\SUB\f.ESP"),
        "{}",
        file_name(&buf)
    );
    assert!(served.faults.applied(id) >= 2, "the fault fired");
    close(h);
    check("final_path_name_not_stored", m);
}

// ---- sections --------------------------------------------------------------

fn mapping(file: HANDLE) -> HANDLE {
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READONLY};
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
    assert!(
        !s.is_null(),
        "CreateFileMappingW: {}",
        fileops::last_error()
    );
    s
}

fn view(section: HANDLE) -> *const u8 {
    use windows_sys::Win32::System::Memory::{MapViewOfFile, FILE_MAP_READ};
    // SAFETY: a section this test created.
    let v = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0) };
    assert!(
        !v.Value.is_null(),
        "MapViewOfFile: {}",
        fileops::last_error()
    );
    v.Value as *const u8
}

/// S6: a 1 MiB section, filled whole at creation.
#[test]
fn section_eager_1_mib() {
    cost_case!();
    let (root, served) = serve();
    let a = open_read(&root.join("data").join("a.esp"));
    close(mapping(a));
    let b = open_read(&root.join("data").join("b.esp"));
    let (s, m) = measure(served, || mapping(b));
    close(s);
    close(a);
    close(b);
    check("section_eager_1_mib", m);
}

/// S6: one 256 KiB page fill of a demand-paged section (sections over
/// 4 MiB, here, through `set_eager_section_max`), outside the 2 MiB warmed
/// at creation.
#[test]
fn section_lazy_page_fill() {
    cost_case!();
    aether_hooks::set_eager_section_max(4 * MIB as u64);
    let (root, served) = serve();
    let want = pattern(5 * MIB);
    let file = open_read(&root.join("data").join("big.bin"));
    let section = mapping(file);
    let v = view(section);
    // SAFETY: the view maps the whole 5 MiB file.
    let at = |off: usize| unsafe { core::ptr::read_volatile(v.add(off)) };
    assert_eq!(at(3 * MIB), want[3 * MIB]);
    let (b, m) = measure(served, || at(4 * MIB));
    assert_eq!(b, want[4 * MIB]);
    use windows_sys::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
    // SAFETY: the view mapped above, once.
    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v as *mut _ }) };
    close(section);
    close(file);
    check("section_lazy_page_fill", m);
}

// ---- a child under a root --------------------------------------------------

/// S1: `CreateProcessW` of an image under a root, one STAGE_EXEC. The child
/// the hook starts never reports ready (the injected shim is this test), so
/// the call ends refused after `READY_TIMEOUT_SECS`.
#[test]
fn create_process() {
    cost_case!();
    let Some(probe) = child_start::probe() else {
        return;
    };
    let (s, _) = child_start::setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    );
    let image = s.root.join(IMAGE);
    let started = |r: Result<u32, u32>| matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED));
    assert!(started(child_start::create(&image)));
    let (r, mut m) = measure(s.served, || child_start::create(&image));
    assert!(started(r), "{r:?}");
    child_start::created();
    let staged = s.staged.to_string_lossy().len() as u64;
    m.ring_bytes -= staged;
    check("create_process", m);
}

// ---- the registry ----------------------------------------------------------

/// `HKCU\Software\<name>-<pid>`, the pid in ten digits: an overlay-only key
/// whose name has the same length in every process.
fn overlay_key(name: &str) -> String {
    format!(r"Software\{name}-{:010}", std::process::id())
}

fn reg_key_with_value(sub: &str) -> HKEY {
    let w: Vec<u16> = sub.encode_utf16().chain(Some(0)).collect();
    let mut k: HKEY = core::ptr::null_mut();
    let mut disp = 0u32;
    // SAFETY: a NUL-terminated name and out-parameters.
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w.as_ptr(),
            0,
            core::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            core::ptr::null(),
            &mut k,
            &mut disp,
        )
    };
    assert_eq!(st, 0, "RegCreateKeyExW {sub}");
    let name: Vec<u16> = "fov".encode_utf16().chain(Some(0)).collect();
    // SAFETY: a key just created, a NUL-terminated name, 4 bytes of data.
    let st = unsafe {
        RegSetValueExW(
            k,
            name.as_ptr(),
            0,
            REG_DWORD,
            90u32.to_le_bytes().as_ptr(),
            4,
        )
    };
    assert_eq!(st, 0, "RegSetValueExW");
    // SAFETY: the key above, closed once.
    unsafe { RegCloseKey(k) };
    let mut k: HKEY = core::ptr::null_mut();
    // SAFETY: as above.
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w.as_ptr(), 0, KEY_READ, &mut k) };
    assert_eq!(st, 0, "RegOpenKeyExW {sub}");
    k
}

/// `RegQueryValueExW(k, "fov")` as a DWORD.
fn query_fov(k: HKEY, name: &[u16]) -> u32 {
    let mut data = [0u8; 4];
    let mut len = 4u32;
    let mut ty = 0u32;
    // SAFETY: an open key, a NUL-terminated name, a 4-byte buffer.
    let st = unsafe {
        RegQueryValueExW(
            k,
            name.as_ptr(),
            core::ptr::null(),
            &mut ty,
            data.as_mut_ptr(),
            &mut len,
        )
    };
    assert_eq!((st, ty, len), (0, REG_DWORD, 4), "RegQueryValueExW");
    u32::from_le_bytes(data)
}

fn reg_hooks() -> &'static Served {
    reg::install_hooks_with(
        "costs",
        Options {
            arena_len: 0,
            ..Options::default()
        },
    )
}

#[test]
fn reg_query_overlay_cold() {
    cost_case!();
    let served = reg_hooks();
    let k = reg_key_with_value(&overlay_key("AetherCostsCold"));
    let name: Vec<u16> = "fov".encode_utf16().chain(Some(0)).collect();
    let (v, m) = measure(served, || query_fov(k, &name));
    assert_eq!(v, 90);
    // SAFETY: the key opened above, closed once.
    unsafe { RegCloseKey(k) };
    check("reg_query_overlay_cold", m);
}

#[test]
fn reg_query_overlay_warm() {
    cost_case!();
    let served = reg_hooks();
    let k = reg_key_with_value(&overlay_key("AetherCostsWarm"));
    let name: Vec<u16> = "fov".encode_utf16().chain(Some(0)).collect();
    assert_eq!(query_fov(k, &name), 90);
    let (v, m) = measure(served, || query_fov(k, &name));
    assert_eq!(v, 90);
    // SAFETY: the key opened above, closed once.
    unsafe { RegCloseKey(k) };
    check("reg_query_overlay_warm", m);
}

/// `NtQueryKey` on a handle that is not a key (a real file outside every
/// root): the host's answer, and nothing asked.
#[test]
fn query_key_not_a_key() {
    cost_case!();
    let served = reg_hooks();
    let outside = aether_testkit::tempdir().expect("temp folder");
    let file = outside.path().join("real.txt");
    std::fs::write(&file, b"on disk").unwrap();
    let h = open_read(&file);
    let mut buf = [0u8; 256];
    let mut ret = 0u32;
    // SAFETY: a file handle and a buffer of its length.
    let q = |buf: &mut [u8; 256], ret: &mut u32| unsafe {
        NtQueryKey(h, KEY_BASIC_INFORMATION, buf.as_mut_ptr().cast(), 256, ret)
    };
    assert_eq!(q(&mut buf, &mut ret), STATUS_OBJECT_TYPE_MISMATCH);
    let (st, m) = measure(served, || q(&mut buf, &mut ret));
    assert_eq!(st, STATUS_OBJECT_TYPE_MISMATCH);
    close(h);
    check("query_key_not_a_key", m);
}
