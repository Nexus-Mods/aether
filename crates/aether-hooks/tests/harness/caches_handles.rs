//! Per-handle state (`synth_file`: size, position, delete-on-close, the
//! read-cache file) request by request: what a director handle answers
//! without asking, what it never carries over to a handle that reuses its
//! `fh`, and the queries that cross every time.
//!
//! The tree: `data/` read-only (the reference answers its files
//! immutable), `saves/` and `mods/` writable.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{pattern, Fake, ReadStyle, Served};
use crate::fileops::{close, create, find_all, find_name, open_read, open_with, ops, serve, wide};
use crate::ntapi;
use aether_protocol::{Opcode, Reply, Request};
use aether_refdirector::harness::Mark;
use std::path::Path;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    FileBasicInfo, FileStandardInfo, GetFileAttributesExW, GetFileExInfoStandard,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileSizeEx, GetFileTime, ReadFile,
    SetFilePointerEx, WriteFile, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_BASIC_INFO, FILE_BEGIN,
    FILE_CURRENT, FILE_END, FILE_FLAG_DELETE_ON_CLOSE, FILE_STANDARD_INFO, OPEN_EXISTING,
    WIN32_FILE_ATTRIBUTE_DATA,
};

const KIB: usize = 1024;
/// A read the read cache serves (below its 64 KiB threshold).
const SMALL: usize = KIB;

fn fake() -> Fake {
    Fake::new()
        .with_dir("data")
        .with_dir("saves")
        .with_dir("mods")
        .with("data/c.esp", pattern(64 * KIB), ReadStyle::Whole)
        .with("data/d.esp", other(64 * KIB), ReadStyle::Whole)
        .with("saves/s.ess", pattern(4 * KIB), ReadStyle::Whole)
        .with("mods/a.bin", pattern(KIB), ReadStyle::Whole)
        .with("mods/b.bin", pattern(KIB), ReadStyle::Whole)
        .writable_under("saves/")
        .writable_under("mods/")
}

/// Other bytes of the same length as `pattern(len)`, differing at every
/// position.
fn other(len: usize) -> Vec<u8> {
    pattern(len).into_iter().map(|b| !b).collect()
}

/// The `fh` of every OPEN reply since `mark`.
fn opened(served: &Served, mark: Mark) -> Vec<u64> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.reply() {
            Some(Reply::Open(o)) => Some(o.fh),
            _ => None,
        })
        .collect()
}

/// `(opcode, fh)` of every request since `mark` that names a handle.
fn on_handles(served: &Served, mark: Mark) -> Vec<(Opcode, u64)> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request()? {
            Request::Read { fh, .. } => Some((Opcode::Read, fh)),
            Request::Write { fh, .. } => Some((Opcode::Write, fh)),
            Request::SetAttr { fh, .. } => Some((Opcode::SetAttr, fh)),
            Request::Close { fh } => Some((Opcode::Close, fh)),
            _ => None,
        })
        .collect()
}

fn read_next(h: HANDLE, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut got = 0u32;
    // SAFETY: `buf` holds `len` bytes; synchronous, no OVERLAPPED.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            len as u32,
            &mut got,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "ReadFile: {}", crate::fileops::last_error());
    buf.truncate(got as usize);
    buf
}

fn write_next(h: HANDLE, data: &[u8]) {
    let mut put = 0u32;
    // SAFETY: `data` is valid for its length; synchronous, no OVERLAPPED.
    let ok = unsafe {
        WriteFile(
            h,
            data.as_ptr(),
            data.len() as u32,
            &mut put,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "WriteFile: {}", crate::fileops::last_error());
    assert_eq!(put as usize, data.len());
}

fn seek(h: HANDLE, to: i64, how: u32) -> i64 {
    let mut at = 0i64;
    // SAFETY: a handle this test opened.
    assert_ne!(
        unsafe { SetFilePointerEx(h, to, &mut at, how) },
        0,
        "SetFilePointerEx"
    );
    at
}

fn size_of(h: HANDLE) -> i64 {
    let mut n = 0i64;
    // SAFETY: a handle this test opened.
    assert_ne!(unsafe { GetFileSizeEx(h, &mut n) }, 0, "GetFileSizeEx");
    n
}

fn standard(h: HANDLE) -> FILE_STANDARD_INFO {
    // SAFETY: plain data, all-zero is valid.
    let mut info: FILE_STANDARD_INFO = unsafe { core::mem::zeroed() };
    // SAFETY: a buffer of the class's size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h,
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            core::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    assert_ne!(ok, 0, "FileStandardInfo: {}", crate::fileops::last_error());
    info
}

fn basic(h: HANDLE) -> FILE_BASIC_INFO {
    // SAFETY: plain data, all-zero is valid.
    let mut info: FILE_BASIC_INFO = unsafe { core::mem::zeroed() };
    // SAFETY: a buffer of the class's size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h,
            FileBasicInfo,
            (&mut info as *mut FILE_BASIC_INFO).cast(),
            core::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    assert_ne!(ok, 0, "FileBasicInfo: {}", crate::fileops::last_error());
    info
}

fn attributes_ex(p: &Path) -> WIN32_FILE_ATTRIBUTE_DATA {
    let w = wide(p);
    // SAFETY: plain data, all-zero is valid.
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated name and a buffer of the level's type.
    let ok = unsafe {
        GetFileAttributesExW(
            w.as_ptr(),
            GetFileExInfoStandard,
            (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        )
    };
    assert_ne!(
        ok,
        0,
        "GetFileAttributesExW: {}",
        crate::fileops::last_error()
    );
    data
}

/// S9 (`docs/wire.md` 3.4.3: `fh` is any u64 but 0; the shim gives 0 no
/// meaning, so 0 is tried too): a director whose
/// handles count up from `base`. Open, read, write and close each carry
/// the handle as issued, and a second handle is the next one.
fn handles_from(base: u64) {
    let (root, served) = serve(fake().with_fh_base(base));
    let path = root.join("saves").join("s.ess");
    let mark = served.recorder.mark();
    let h = open_with(&path, GENERIC_READ | GENERIC_WRITE);
    assert_eq!(read_next(h, 100), pattern(100));
    write_next(h, b"hello");
    close(h);
    assert_eq!(opened(served, mark), [base]);
    assert_eq!(
        on_handles(served, mark),
        [
            (Opcode::Read, base),
            (Opcode::Write, base),
            (Opcode::Close, base)
        ]
    );
    let bytes = served.fake.contents("saves/s.ess").unwrap();
    assert_eq!(&bytes[100..105], b"hello");

    let mark = served.recorder.mark();
    let h = open_read(&path);
    assert_eq!(read_next(h, 105)[100..], *b"hello");
    close(h);
    assert_eq!(opened(served, mark), [base + 1]);
    assert_eq!(
        on_handles(served, mark),
        [(Opcode::Read, base + 1), (Opcode::Close, base + 1)]
    );
}

#[test]
fn handles_above_u32_max_work() {
    isolate!();
    handles_from((1 << 32) + 1);
}

#[test]
fn handles_with_the_top_bit_set_work() {
    isolate!();
    handles_from((1 << 63) + 1);
}

/// S9: a director that issues a closed `fh` again (`Reference::reissue_fh`,
/// wire.md 3.4.3 "unique among open handles"). The new handle carries
/// nothing over: not the first handle's delete-on-close, not its file's
/// cached blocks.
#[test]
fn an_fh_reissued_after_close_inherits_nothing() {
    isolate!();
    let (root, served) = serve(fake().reissue_fh());

    // A delete-on-close handle, closed: CLOSE, then DELETE of its path.
    let mark = served.recorder.mark();
    let a = create(
        &root.join("mods").join("a.bin"),
        GENERIC_READ | DELETE,
        OPEN_EXISTING,
        FILE_FLAG_DELETE_ON_CLOSE,
    )
    .expect("open a.bin delete-on-close");
    close(a);
    let first = opened(served, mark);
    assert_eq!(first.len(), 1);
    assert!(
        served.fake.contents("mods/a.bin").is_none(),
        "a.bin deleted"
    );

    // Another file, given the same fh: closing it deletes nothing.
    let mark = served.recorder.mark();
    let b = open_read(&root.join("mods").join("b.bin"));
    assert_eq!(opened(served, mark), first, "the reference reissued the fh");
    let mark = served.recorder.mark();
    close(b);
    assert_eq!(ops(served, mark), [Some(Opcode::Close)]);
    assert!(served.fake.contents("mods/b.bin").is_some(), "b.bin kept");

    // A read-cached handle, closed; another file given its fh reads its own
    // bytes, fetched for it.
    let mark = served.recorder.mark();
    let c = open_read(&root.join("data").join("c.esp"));
    let mut buf = vec![0u8; SMALL];
    assert!(ntapi::nt_read_at(c, 0, &mut buf).0 >= 0);
    assert_eq!(buf, pattern(SMALL));
    close(c);
    let fh = opened(served, mark);
    let mark = served.recorder.mark();
    let d = open_read(&root.join("data").join("d.esp"));
    assert_eq!(opened(served, mark), fh, "the reference reissued the fh");
    let mark = served.recorder.mark();
    assert!(ntapi::nt_read_at(d, 0, &mut buf).0 >= 0);
    assert_eq!(buf, other(SMALL), "d.esp's bytes, not c.esp's");
    assert_eq!(
        on_handles(served, mark),
        [(Opcode::Read, fh[0])],
        "the read crossed for d.esp"
    );
    close(d);
}

/// A `FILETIME` as one number.
fn ticks(t: windows_sys::Win32::Foundation::FILETIME) -> i64 {
    ((t.dwHighDateTime as i64) << 32) | t.dwLowDateTime as i64
}

/// The last-write time each handle query reports for `h`:
/// `FileBasicInformation`, `GetFileTime`, `GetFileInformationByHandle`.
fn handle_times(h: HANDLE) -> [i64; 3] {
    let basic = basic(h).LastWriteTime;
    let mut c = windows_sys::Win32::Foundation::FILETIME::default();
    let (mut a, mut w) = (c, c);
    // SAFETY: a handle this test opened and three FILETIMEs.
    assert_ne!(
        unsafe { GetFileTime(h, &mut c, &mut a, &mut w) },
        0,
        "GetFileTime"
    );
    // SAFETY: plain data, all-zero is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: as above.
    assert_ne!(
        unsafe { GetFileInformationByHandle(h, &mut info) },
        0,
        "GetFileInformationByHandle"
    );
    [basic, ticks(w), ticks(info.ftLastWriteTime)]
}

/// The last-write time each query by path reports for `p`:
/// `NtQueryFullAttributesFile`, `GetFileAttributesExW`, and
/// `NtQueryInformationByName` of `FileNetworkOpenInformation` (34) and
/// `FileStatInformation` (68). One GETATTR each.
fn path_times(p: &Path) -> [i64; 4] {
    let s = p.display().to_string();
    let (st, full) = ntapi::nt_query_full_attributes_abs(&s);
    assert!(st >= 0, "NtQueryFullAttributesFile: {st:#x}");
    let by_name = |class: u32, at: usize| {
        let (st, buf) = ntapi::nt_query_by_name_abs(&s, class).expect("NtQueryInformationByName");
        assert!(st >= 0, "NtQueryInformationByName({class}): {st:#x}");
        i64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
    };
    [
        full.last_write_time,
        ticks(attributes_ex(p).ftLastWriteTime),
        by_name(34, 16),
        by_name(68, 24),
    ]
}

/// The last-write time of `name`'s record in `FindFirstFileW(dir\*)`.
fn listed_time(dir: &Path, name: &str) -> i64 {
    let listed = find_all(dir).expect("list");
    let d = listed
        .iter()
        .find(|d| find_name(d) == name)
        .unwrap_or_else(|| panic!("{name} listed"));
    ticks(d.ftLastWriteTime)
}

/// What every query reports for a file the director sends no time for
/// (`docs/wire.md` 3.2): 2024-01-01T00:00:00Z.
const PLACEHOLDER: i64 = 133_485_408_000_000_000;

/// S13 (`docs/wire.md` 3.2, "Timestamps"): one non-zero `mtime` is one
/// `LastWriteTime` in a listing, in every query by path and in every query
/// by handle. A handle asks GETATTR once, on its first time query, and
/// again after a write through it.
#[test]
fn a_nonzero_mtime_agrees_across_basic_info_directory_records_and_full_attributes() {
    isolate!();
    const MTIME: i64 = 1_700_000_000;
    // Seconds since 1970 as 100 ns ticks since 1601.
    let want = MTIME * 10_000_000 + 116_444_736_000_000_000;
    let (root, served) = serve(
        fake()
            .with_mtime("data/c.esp", MTIME)
            .with_mtime("saves/s.ess", MTIME),
    );
    let data = root.join("data");
    let path = data.join("c.esp");

    assert_eq!(listed_time(&data, "c.esp"), want, "the listing");
    let mark = served.recorder.mark();
    assert_eq!(path_times(&path), [want; 4], "by path");
    assert_eq!(
        ops(served, mark),
        [Some(Opcode::GetAttr); 4],
        "one GETATTR each, as before"
    );

    let h = open_read(&path);
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [want; 3], "by handle");
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)], "the first asks");
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [want; 3]);
    assert_eq!(ops(served, mark), [], "the handle keeps it");
    close(h);

    // A write through the handle: its next time query asks again, once.
    let h = open_with(
        &root.join("saves").join("s.ess"),
        GENERIC_READ | GENERIC_WRITE,
    );
    assert_eq!(handle_times(h), [want; 3]);
    write_next(h, b"x");
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [want; 3]);
    assert_eq!(
        ops(served, mark),
        [Some(Opcode::GetAttr)],
        "after the write"
    );
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [want; 3]);
    assert_eq!(ops(served, mark), []);
    close(h);
}

/// A director that sends no time: every query reports the placeholder. A
/// listing and queries by path carrying no time settle nothing, so the
/// first handle time query in the process asks once; its answer (no time)
/// settles it, and no later handle time query asks, on that handle or
/// another, after a write too.
#[test]
fn a_director_that_sends_no_time_is_asked_once_per_process() {
    isolate!();
    let (root, served) = serve(fake());
    let data = root.join("data");
    let path = data.join("c.esp");
    assert_eq!(listed_time(&data, "c.esp"), PLACEHOLDER);
    assert_eq!(path_times(&path), [PLACEHOLDER; 4]);

    let h = open_read(&path);
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [PLACEHOLDER; 3]);
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)], "the first asks");
    close(h);

    let h = open_with(
        &root.join("saves").join("s.ess"),
        GENERIC_READ | GENERIC_WRITE,
    );
    write_next(h, b"x");
    let mark = served.recorder.mark();
    for _ in 0..2 {
        assert_eq!(handle_times(h), [PLACEHOLDER; 3]);
    }
    assert_eq!(ops(served, mark), [], "settled: no time is sent");
    close(h);
}

/// A handle's time query before anything else in the process asks, and the
/// time it gets turns per-handle fetching on: it agrees with the listing
/// and the queries by path that come after it, and another handle asks too.
#[test]
fn a_first_handle_time_query_that_finds_a_time_turns_fetching_on() {
    isolate!();
    const MTIME: i64 = 1_700_000_000;
    let want = MTIME * 10_000_000 + 116_444_736_000_000_000;
    let (root, served) = serve(
        fake()
            .with_mtime("data/c.esp", MTIME)
            .with_mtime("data/d.esp", MTIME + 1),
    );
    let data = root.join("data");
    let path = data.join("c.esp");
    let h = open_read(&path);
    let mark = served.recorder.mark();
    assert_eq!(handle_times(h), [want; 3]);
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)]);
    assert_eq!(path_times(&path), [want; 4]);
    assert_eq!(listed_time(&data, "c.esp"), want);
    close(h);

    let d = open_read(&data.join("d.esp"));
    let mark = served.recorder.mark();
    assert_eq!(handle_times(d), [want + 10_000_000; 3]);
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)]);
    close(d);
}

/// A time seen after a handle's query settled "no time" turns fetching on
/// again: a director whose first file had none but whose others do.
#[test]
fn a_time_seen_after_none_was_settled_turns_fetching_on_again() {
    isolate!();
    const MTIME: i64 = 1_700_000_000;
    let want = MTIME * 10_000_000 + 116_444_736_000_000_000;
    let (root, served) = serve(fake().with_mtime("data/d.esp", MTIME));
    let data = root.join("data");
    let c = open_read(&data.join("c.esp"));
    assert_eq!(handle_times(c), [PLACEHOLDER; 3], "c.esp has no time");
    close(c);
    let d = open_read(&data.join("d.esp"));
    let mark = served.recorder.mark();
    assert_eq!(handle_times(d), [PLACEHOLDER; 3], "settled: not asked");
    assert_eq!(ops(served, mark), []);
    assert_eq!(listed_time(&data, "d.esp"), want);
    let mark = served.recorder.mark();
    assert_eq!(
        handle_times(d),
        [want; 3],
        "the listing's time turned it on"
    );
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)]);
    close(d);
}

/// The `mtime` a director reports after the test's write: [`later_times`]
/// puts it in every GETATTR and READDIR reply.
const LATER_MTIME: i64 = 1_800_000_000;

/// Every GETATTR and READDIR reply given [`LATER_MTIME`], as a director
/// whose file a write just changed answers.
fn later_times(r: &mut aether_refdirector::harness::OwnedReplyBytes) {
    let t = LATER_MTIME.to_le_bytes();
    match r.op() {
        Some(Opcode::GetAttr) if r.payload.len() >= 18 && r.payload[0] != 0 => {
            r.payload[10..18].copy_from_slice(&t)
        }
        Some(Opcode::ReadDir) if r.payload.len() >= 4 => {
            let p = &mut r.payload;
            let count = u32::from_le_bytes(p[0..4].try_into().unwrap());
            let mut at = 4;
            for _ in 0..count {
                let len = u32::from_le_bytes(p[at..at + 4].try_into().unwrap()) as usize;
                at += 4 + len + 1 + 8;
                p[at..at + 8].copy_from_slice(&t);
                at += 8;
            }
        }
        _ => {}
    }
}

/// Times are kept per file, as NTFS keeps them: a write through one handle
/// makes every handle on the same file in this process ask again, so the
/// other handle agrees with the listing and the queries by path after it.
#[test]
fn a_write_through_one_handle_refreshes_the_time_another_handle_kept() {
    isolate!();
    const MTIME: i64 = 1_700_000_000;
    let ft = |s: i64| s * 10_000_000 + 116_444_736_000_000_000;
    let (root, served) = serve(fake().with_mtime("saves/s.ess", MTIME));
    let saves = root.join("saves");
    let path = saves.join("s.ess");
    // The other handle is opened in another case: it is the same file.
    let other = open_read(&saves.join("S.ESS"));
    assert_eq!(handle_times(other), [ft(MTIME); 3]);

    let w = open_with(&path, GENERIC_READ | GENERIC_WRITE);
    let id = served.faults.arm(
        crate::fakedirector::Match::default(),
        crate::fakedirector::Fault::Rewrite(later_times),
    );
    write_next(w, b"x");
    close(w);

    let mark = served.recorder.mark();
    assert_eq!(
        handle_times(other),
        [ft(LATER_MTIME); 3],
        "the kept time was dropped"
    );
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr)]);
    assert_eq!(listed_time(&saves, "s.ess"), ft(LATER_MTIME));
    assert_eq!(path_times(&path), [ft(LATER_MTIME); 4]);
    assert!(served.faults.applied(id) >= 1);
    close(other);
}

#[test]
fn size_and_position_queries_on_a_director_handle_cost_zero_round_trips() {
    isolate!();
    let (root, served) = serve(fake());
    let h = open_read(&root.join("data").join("c.esp"));
    let len = 64 * KIB as i64;
    let mark = served.recorder.mark();
    assert_eq!(size_of(h), len);
    assert_eq!(seek(h, 0, FILE_END), len);
    assert_eq!(seek(h, 10, FILE_BEGIN), 10);
    assert_eq!(seek(h, 5, FILE_CURRENT), 15);
    assert_eq!(seek(h, 0, FILE_CURRENT), 15);
    let s = standard(h);
    assert_eq!((s.EndOfFile, s.Directory), (len, false));
    assert_eq!(
        served.recorder.total_since(mark),
        0,
        "{:#?}",
        served.recorder.since(mark)
    );
    close(h);
}

#[test]
fn a_write_past_the_end_raises_the_handle_s_size_without_asking() {
    isolate!();
    let (root, served) = serve(fake());
    let h = open_with(
        &root.join("saves").join("s.ess"),
        GENERIC_READ | GENERIC_WRITE,
    );
    let end = 4 * KIB as i64;
    assert_eq!(seek(h, end + 100, FILE_BEGIN), end + 100);
    let mark = served.recorder.mark();
    write_next(h, b"0123456789");
    assert_eq!(ops(served, mark), [Some(Opcode::Write)]);
    let mark = served.recorder.mark();
    assert_eq!(size_of(h), end + 110);
    assert_eq!(standard(h).EndOfFile, end + 110);
    assert_eq!(seek(h, 0, FILE_CURRENT), end + 110);
    assert_eq!(served.recorder.total_since(mark), 0);
    close(h);
    assert_eq!(
        served.fake.contents("saves/s.ess").unwrap().len() as i64,
        end + 110
    );
}

/// There is no attribute cache: each query by path is one GETATTR. A cache
/// added later shows up here, and in Task 7's record, as fewer.
#[test]
fn getattr_by_path_crosses_every_time() {
    isolate!();
    let (root, served) = serve(fake());
    let path = root.join("data").join("c.esp");
    let mark = served.recorder.mark();
    for _ in 0..3 {
        let a = attributes_ex(&path);
        assert_eq!(a.nFileSizeLow as usize, 64 * KIB);
    }
    assert_eq!(ops(served, mark), [Some(Opcode::GetAttr); 3]);
}

/// The requests an open of a missing file sends: the baseline Task 7
/// gates. There is no negative cache, so a second open sends them again.
#[test]
fn a_failed_open_s_requests_are_what_they_are() {
    isolate!();
    let (root, served) = serve(fake());
    let path = root.join("data").join("missing.esp");
    for n in 1..=2 {
        let mark = served.recorder.mark();
        assert_eq!(
            create(&path, GENERIC_READ, OPEN_EXISTING, 0),
            Err(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND),
            "open {n}"
        );
        assert_eq!(ops(served, mark), [Some(Opcode::Open)], "open {n}");
        let statuses: Vec<i32> = served
            .recorder
            .since(mark)
            .iter()
            .map(|r| r.status)
            .collect();
        assert_eq!(
            statuses,
            [aether_protocol::Status::NotFound.code()],
            "open {n}"
        );
    }
}
