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
    GetFileInformationByHandleEx, GetFileSizeEx, ReadFile, SetFilePointerEx, WriteFile, DELETE,
    FILE_BASIC_INFO, FILE_BEGIN, FILE_CURRENT, FILE_END, FILE_FLAG_DELETE_ON_CLOSE,
    FILE_STANDARD_INFO, OPEN_EXISTING, WIN32_FILE_ATTRIBUTE_DATA,
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

/// S9 (`docs/wire.md` 3.4.3: any u64 is a valid `fh`): a director whose
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

/// S13 (`docs/wire.md` 3.2, "Timestamps"): one non-zero `mtime` is one
/// `LastWriteTime` in every query that reports it.
#[test]
#[ignore = "fails: handle and path queries report SYNTH_FILETIME whatever mtime the director \
            sends; awaiting a ruling (task-6-report.md, S13)"]
fn a_nonzero_mtime_agrees_across_basic_info_directory_records_and_full_attributes() {
    isolate!();
    const MTIME: i64 = 1_700_000_000;
    // Seconds since 1970 as 100 ns ticks since 1601.
    let want = MTIME * 10_000_000 + 116_444_736_000_000_000;
    let (root, _served) = serve(fake().with_mtime("data/c.esp", MTIME));
    let path = root.join("data").join("c.esp");

    let listed = find_all(&root.join("data"))
        .expect("list data")
        .into_iter()
        .find(|d| find_name(d) == "c.esp")
        .expect("c.esp listed");
    let t = listed.ftLastWriteTime;
    let record = (t.dwHighDateTime as i64) << 32 | t.dwLowDateTime as i64;

    let h = open_read(&path);
    let by_handle = basic(h).LastWriteTime;
    close(h);

    let (st, full) = ntapi::nt_query_full_attributes_abs(&path.display().to_string());
    assert!(st >= 0, "NtQueryFullAttributesFile: {st:#x}");

    assert_eq!(
        (record, by_handle, full.last_write_time),
        (want, want, want),
        "(directory record, FileBasicInformation, NtQueryFullAttributesFile)"
    );
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
