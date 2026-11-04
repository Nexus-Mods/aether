//! Answers a director may give that the reference gives only with a knob:
//! a writable root served as an overlay over a read-only lower
//! (`Reference::with_lower`, `docs/wire.md` 3.4.9 "Names only in a
//! read-only lower"), and the smaller alternatives wire.md 3.4.2, 3.4.3,
//! 3.4.7 and 3.4.8 allow. Each shows what the program sees.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{pattern, Fake, Fault, Match, ReadStyle, Served};
use crate::fileops::{close, find_all, find_name, last_error, names, open_read, serve, wide};
use crate::ntapi;
use aether_protocol::{Opcode, Reply, Request, Status};
use aether_refdirector::harness::{Mark, OpenEdit, OwnedReplyBytes};
use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, GetFileAttributesW, MoveFileExW, INVALID_FILE_ATTRIBUTES,
};

const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001_u32 as i32;
const KIB: usize = 1024;

/// `Data/` writable, holding `Keep.esp` of its own and, only in the lower,
/// the directory `Low` (with `x.esp` in it) and the file `f.esp`.
fn overlay() -> Fake {
    Fake::new()
        .with_dir("Data")
        .with("Data/Keep.esp", pattern(KIB), ReadStyle::Whole)
        .with_lower_dir("Data/Low")
        .with_lower("Data/Low/x.esp", pattern(KIB))
        .with_lower("Data/f.esp", pattern(KIB))
        .writable_under("data/")
}

/// `(status, path)` of every DELETE since `mark`.
fn deletes(served: &Served, mark: Mark) -> Vec<(i32, String)> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request()? {
            Request::Delete { path, .. } => Some((r.status, path.to_string())),
            _ => None,
        })
        .collect()
}

/// `(status, from, to)` of every RENAME since `mark`.
fn renames(served: &Served, mark: Mark) -> Vec<(i32, String, String)> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.request()? {
            Request::Rename { from, to, .. } => Some((r.status, from.to_string(), to.to_string())),
            _ => None,
        })
        .collect()
}

fn nt_open_dir(p: &std::path::Path, access: u32) -> HANDLE {
    let (st, h) = ntapi::nt_open_dir_abs(&p.display().to_string(), access);
    assert!(st >= 0, "open {}: {st:#x}", p.display());
    h
}

#[test]
fn deleting_a_lower_only_directory_with_children_hides_it() {
    isolate!();
    let (root, served) = serve(overlay());
    let data = root.join("Data");
    let mark = served.recorder.mark();
    let h = nt_open_dir(&data.join("Low"), ntapi::DELETE);
    let st = ntapi::nt_set_disposition_delete(h);
    ntapi::close(h);
    assert!(st >= 0, "FileDispositionInformation: {st:#x}");
    assert_eq!(
        deletes(served, mark),
        [(Status::Ok.code(), "data/low".to_string())]
    );
    let mut listed = names(&data);
    listed.sort();
    assert_eq!(listed, ["Keep.esp", "f.esp"], "Low is hidden");
}

#[test]
fn renaming_a_lower_only_directory_fails_as_io_error_maps() {
    isolate!();
    let (root, served) = serve(overlay());
    let data = root.join("Data");
    let from = wide(&data.join("Low"));
    let to = wide(&data.join("Moved"));
    let mark = served.recorder.mark();
    // SAFETY: NUL-terminated names.
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    assert_eq!((ok, last_error()), (0, ERROR_GEN_FAILURE));
    assert_eq!(
        renames(served, mark),
        [(
            Status::IoError.code(),
            "data/low".to_string(),
            "Data/Moved".to_string()
        )]
    );
    assert!(names(&data).contains(&"Low".to_string()), "Low stays");
}

#[test]
fn mkdir_over_a_lower_only_file_makes_a_directory() {
    isolate!();
    let (root, served) = serve(overlay());
    let path = wide(&root.join("Data").join("f.esp"));
    let mark = served.recorder.mark();
    // SAFETY: a NUL-terminated name, no security attributes.
    let made = unsafe { CreateDirectoryW(path.as_ptr(), core::ptr::null()) };
    assert_ne!(made, 0, "CreateDirectoryW: {}", last_error());
    assert!(
        served.recorder.count_since(mark, Opcode::Mkdir) == 1,
        "{:?}",
        served.recorder.ops_since(mark)
    );
    // SAFETY: a NUL-terminated name.
    let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
    assert_ne!(attrs, INVALID_FILE_ATTRIBUTES, "{}", last_error());
    assert_ne!(attrs & ntapi::FILE_ATTRIBUTE_DIRECTORY, 0, "{attrs:#x}");
}

/// S17 (`docs/wire.md` 3.4.3): bits 1 to 7 of an OPEN reply's flags byte
/// are reserved. A handle answered with them set works, and only bit 0
/// says immutable: a mutable file stays uncached, an immutable one cached.
#[test]
fn an_open_reply_with_reserved_flag_bits_set_is_read_as_its_bit_0() {
    isolate!();
    let (root, served) = serve(
        Fake::new()
            .with_dir("data")
            .with_dir("saves")
            .with("data/c.esp", pattern(64 * KIB), ReadStyle::Whole)
            .with("saves/s.ess", pattern(64 * KIB), ReadStyle::Whole)
            .writable_under("saves/"),
    );
    for prefix in ["data/", "saves/"] {
        served.overrides.open(
            prefix,
            OpenEdit {
                reserved_flags: Some(0xFE),
                ..OpenEdit::default()
            },
        );
    }
    let want = pattern(64 * KIB);
    let mut buf = vec![0u8; KIB];
    let read = |h: HANDLE, buf: &mut Vec<u8>| {
        let mark = served.recorder.mark();
        let (st, n) = ntapi::nt_read_at(h, 0, buf);
        assert!(st >= 0 && n == KIB, "NtReadFile: {st:#x} {n}");
        assert_eq!(buf[..], want[..KIB]);
        served.recorder.count_since(mark, Opcode::Read)
    };

    for (path, immutable, costs) in [
        (root.join("saves").join("s.ess"), false, [1, 1, 1]),
        (root.join("data").join("c.esp"), true, [1, 0, 0]),
    ] {
        let mark = served.recorder.mark();
        let h = open_read(&path);
        let seen = served.recorder.since(mark);
        let open = seen
            .iter()
            .find(|r| r.op() == Some(Opcode::Open))
            .expect("an OPEN");
        assert_eq!(
            open.reply[16 + 1] & 0xFE,
            0xFE,
            "the reserved bits were sent"
        );
        assert!(
            matches!(open.reply(), Some(Reply::Open(o)) if o.immutable == immutable),
            "{}",
            path.display()
        );
        let got: Vec<usize> = (0..3).map(|_| read(h, &mut buf)).collect();
        assert_eq!(got, costs, "READs per read of {}", path.display());
        close(h);
    }
}

/// Every directory entry of a READDIR reply given `size` 4096.
fn directory_sizes_4096(r: &mut OwnedReplyBytes) {
    let p = &mut r.payload;
    let count = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let mut at = 4;
    for _ in 0..count {
        let len = u32::from_le_bytes(p[at..at + 4].try_into().unwrap()) as usize;
        at += 4 + len;
        let is_dir = p[at] != 0;
        if is_dir {
            p[at + 1..at + 9].copy_from_slice(&4096u64.to_le_bytes());
        }
        at += 1 + 8 + 8;
    }
}

/// S17, a pin of `docs/wire.md` 3.4.2: a directory entry's `size` is copied
/// into both `EndOfFile` and `AllocationSize`, 0 or not.
#[test]
fn a_directory_size_that_is_not_zero_is_shown_as_sent() {
    isolate!();
    let (root, served) = serve(Fake::new().with_dir("Data").with_dir("Data/Sub").with(
        "Data/a.esp",
        pattern(10),
        ReadStyle::Whole,
    ));
    let id = served.faults.arm(
        Match {
            op: Some(Opcode::ReadDir),
            ..Match::default()
        },
        Fault::Rewrite(directory_sizes_4096),
    );
    let data = root.join("Data");
    let h = nt_open_dir(&data, ntapi::FILE_LIST_DIRECTORY);
    let mut records = ntapi::nt_enum_records(h);
    ntapi::close(h);
    records.sort_by(|a, b| a.name.cmp(&b.name));
    let sizes: Vec<(String, i64, i64, bool)> = records
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.end_of_file,
                r.allocation_size,
                r.file_attributes & ntapi::FILE_ATTRIBUTE_DIRECTORY != 0,
            )
        })
        .collect();
    assert_eq!(
        sizes,
        [
            ("Sub".to_string(), 4096, 4096, true),
            ("a.esp".to_string(), 10, 10, false)
        ]
    );
    let listed = find_all(&data).expect("list Data");
    let sub = listed.iter().find(|d| find_name(d) == "Sub").expect("Sub");
    assert_eq!((sub.nFileSizeHigh, sub.nFileSizeLow), (0, 4096));
    assert!(served.faults.applied(id) >= 2);
}

/// S17 (`docs/wire.md` 3.4.7, 3.4.8): a director may answer RENAME or
/// DELETE of the root `ST_IO_ERROR` rather than `ST_BAD_REQUEST`. Either
/// is a failure the shim reports as `STATUS_UNSUCCESSFUL`.
#[test]
fn rename_or_delete_of_the_root_answered_io_error_fails_as_the_table_says() {
    isolate!();
    let (root, served) = serve(Fake::new().with_dir("Data").writable_under(""));
    let mut ids = Vec::new();
    for op in [Opcode::Rename, Opcode::Delete] {
        ids.push(served.faults.arm(
            Match {
                op: Some(op),
                ..Match::default()
            },
            Fault::Status(Status::IoError),
        ));
    }
    let h = nt_open_dir(&root, ntapi::DELETE);

    let mark = served.recorder.mark();
    let target = root.join("Inside").display().to_string();
    let st = ntapi::nt_rename(h, &target, ntapi::FILE_RENAME_INFORMATION);
    assert_eq!(st, STATUS_UNSUCCESSFUL, "rename: {st:#x}");
    let sent = renames(served, mark);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, Status::IoError.code());
    assert_eq!(sent[0].2, "Inside");

    let mark = served.recorder.mark();
    let st = ntapi::nt_set_disposition_delete(h);
    assert_eq!(st, STATUS_UNSUCCESSFUL, "delete: {st:#x}");
    let sent = deletes(served, mark);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, Status::IoError.code());
    ntapi::close(h);

    for id in ids {
        assert_eq!(served.faults.applied(id), 1);
    }
    assert!(root.is_dir(), "the root is still there");
    assert_eq!(names(&root), ["Data"]);
}
