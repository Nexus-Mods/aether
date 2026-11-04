//! What the shim makes of a reply it cannot use (`docs/wire.md` 3.7), as a
//! table, and one test per row that puts every fault of the table to the
//! hooked call through the harness's `Faults`.
//!
//! The table is [`expected`]: for each hooked operation ([`HookedOp`]) and
//! each kind of bad reply ([`FaultClass`]), what the program gets
//! ([`Expect`]). It is the source of truth. wire.md 3.7 is written from it
//! and checked against it, cell by cell, by
//! [`the_wire_document_s_fault_table_is_the_test_s`]; on a difference that
//! test prints the table as it should read.
//!
//! Each row's test (`row_*`, one process each) arms one fault at a time on
//! the operation's opcode, for one request, and makes the call on a thread
//! of its own under a watchdog. It asserts:
//!
//! - the call returned within the watchdog (the client's deadline is
//!   300 ms);
//! - the fault fired (`Faults::applied` is 1);
//! - the result is the table's;
//! - the next call of the same operation, unfaulted, succeeds, so the shim
//!   keeps serving;
//! - no hook panicked over the row (`hook_panics_total`).
//!
//! A row collects every cell that differs and fails once with all of them,
//! so one run shows the whole row.
#![allow(unsafe_code)] // Win32 and ntdll calls

use crate::fakedirector::{self, pattern, Fake, Fault, Match, Options, ReadStyle, Served};
use crate::ntapi::{
    abs_name, nt_delete_file, nt_open_abs, nt_open_dir_abs, nt_query_full_attributes_abs,
    nt_read_at, nt_rename, nt_set_disposition_delete, nt_set_info_raw, DELETE, FILE_LIST_DIRECTORY,
    FILE_RENAME_INFORMATION,
};
use aether_protocol::{Opcode, Status};
use aether_refdirector::harness::{BulkLie, OwnedReplyBytes};
use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ---- the table -----------------------------------------------------------

/// A hooked operation, as a program reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookedOp {
    /// The attach HEARTBEAT: whether the shim's load goes on.
    Attach,
    /// `NtOpenFile` of a file for reading (OPEN).
    OpenRead,
    /// `NtCreateFile` of an existing file for writing, `FILE_OPEN` (OPEN).
    OpenWrite,
    /// `NtQueryFullAttributesFile` (GETATTR).
    GetAttr,
    /// `NtReadFile` of 100 bytes of a file that may change (one inline READ).
    ReadInline,
    /// `NtReadFile` of 128 KiB (one bulk READ).
    ReadBulk,
    /// `NtCreateSection` over a 1 MiB file: filled at creation (bulk READs).
    SectionFill,
    /// `NtWriteFile` of one chunk (WRITE).
    WriteFirst,
    /// `NtWriteFile` of three chunks, the fault on the second (WRITE).
    WriteLater,
    /// `NtSetInformationFile(FileEndOfFileInformation)` (SETATTR).
    SetSize,
    /// The first `NtQueryDirectoryFile` on a directory handle (READDIR).
    ReadDir,
    /// `NtSetInformationFile(FileRenameInformation)` (RENAME).
    Rename,
    /// `NtDeleteFile` (DELETE).
    DeleteByName,
    /// `NtSetInformationFile(FileDispositionInformation)` (DELETE).
    DeleteOnHandle,
    /// `NtCreateFile` of a new directory, `FILE_CREATE` (MKDIR).
    Mkdir,
    /// `GetFinalPathNameByHandleW` (STORED_NAMES).
    StoredNames,
    /// `CreateProcessW` of an image under a root (STAGE_EXEC).
    StageExec,
    /// `NtQueryValueKey` (REG_KEY).
    RegRead,
    /// `NtSetValueKey` (REG_SET_VALUE).
    RegWrite,
}

impl HookedOp {
    pub(crate) const ALL: [HookedOp; 19] = [
        HookedOp::Attach,
        HookedOp::OpenRead,
        HookedOp::OpenWrite,
        HookedOp::GetAttr,
        HookedOp::ReadInline,
        HookedOp::ReadBulk,
        HookedOp::SectionFill,
        HookedOp::WriteFirst,
        HookedOp::WriteLater,
        HookedOp::SetSize,
        HookedOp::ReadDir,
        HookedOp::Rename,
        HookedOp::DeleteByName,
        HookedOp::DeleteOnHandle,
        HookedOp::Mkdir,
        HookedOp::StoredNames,
        HookedOp::StageExec,
        HookedOp::RegRead,
        HookedOp::RegWrite,
    ];

    /// The row's name in wire.md 3.7.
    pub(crate) fn label(self) -> &'static str {
        match self {
            HookedOp::Attach => "attach (HEARTBEAT)",
            HookedOp::OpenRead => "open to read (OPEN)",
            HookedOp::OpenWrite => "open to write (OPEN)",
            HookedOp::GetAttr => "attributes by name (GETATTR)",
            HookedOp::ReadInline => "read, inline (READ)",
            HookedOp::ReadBulk => "read, bulk (READ)",
            HookedOp::SectionFill => "section filled at creation (READ)",
            HookedOp::WriteFirst => "write, first chunk (WRITE)",
            HookedOp::WriteLater => "write, a later chunk (WRITE)",
            HookedOp::SetSize => "set the size (SETATTR)",
            HookedOp::ReadDir => "list a directory (READDIR)",
            HookedOp::Rename => "rename (RENAME)",
            HookedOp::DeleteByName => "delete by name (DELETE)",
            HookedOp::DeleteOnHandle => "delete through a handle (DELETE)",
            HookedOp::Mkdir => "create a directory (MKDIR)",
            HookedOp::StoredNames => "final name (STORED_NAMES)",
            HookedOp::StageExec => "start a child (STAGE_EXEC)",
            HookedOp::RegRead => "registry read (REG_KEY)",
            HookedOp::RegWrite => "registry write (REG_SET_VALUE)",
        }
    }

    /// The opcode whose reply is faulted.
    pub(crate) fn opcode(self) -> Opcode {
        match self {
            HookedOp::Attach => Opcode::Heartbeat,
            HookedOp::OpenRead | HookedOp::OpenWrite => Opcode::Open,
            HookedOp::GetAttr => Opcode::GetAttr,
            HookedOp::ReadInline | HookedOp::ReadBulk | HookedOp::SectionFill => Opcode::Read,
            HookedOp::WriteFirst | HookedOp::WriteLater => Opcode::Write,
            HookedOp::SetSize => Opcode::SetAttr,
            HookedOp::ReadDir => Opcode::ReadDir,
            HookedOp::Rename => Opcode::Rename,
            HookedOp::DeleteByName | HookedOp::DeleteOnHandle => Opcode::Delete,
            HookedOp::Mkdir => Opcode::Mkdir,
            HookedOp::StoredNames => Opcode::StoredNames,
            HookedOp::StageExec => Opcode::StageExec,
            HookedOp::RegRead => Opcode::RegKey,
            HookedOp::RegWrite => Opcode::RegSetValue,
        }
    }

    /// How many of the operation's requests one call's fault answers:
    /// one, but every STORED_NAMES of the call for a final name, which a
    /// program's one query may ask twice (once for the length).
    pub(crate) fn times(self) -> Option<u32> {
        (self != HookedOp::StoredNames).then_some(1)
    }

    pub(crate) fn reads(self) -> bool {
        self.opcode() == Opcode::Read
    }

    /// Whether the faulted request's OK reply carries nothing.
    pub(crate) fn empty_reply(self) -> bool {
        matches!(
            self.opcode(),
            Opcode::Heartbeat | Opcode::SetAttr | Opcode::Rename | Opcode::Delete | Opcode::Mkdir
        )
    }
}

/// A kind of reply the shim cannot use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultClass {
    /// A named status, no payload (the director is not asked).
    Status(Status),
    /// A negative status no table names (-99).
    UnknownNegative,
    /// A positive status (1).
    Positive,
    /// A reply past `payload_cap`, which the ring sends as its overflow
    /// status, `i32::MIN`.
    Overflow,
    /// The director's OK payload less its last byte.
    Truncated,
    /// The director's OK payload with four bytes 0xFF after it.
    Trailing,
    /// OK with the three bytes 0xFF 0xFF 0xFF for a payload: shorter than
    /// every fixed reply, and not UTF-8.
    Garbage,
    /// An OK bulk READ reply naming data outside the slot's bank, or more
    /// than was asked for.
    Bulk(BulkLie),
    /// No reply within the client's deadline.
    NoReply,
    /// The call after one that got no reply in time, made once the late
    /// reply has landed: the late reply must not answer it.
    Late,
}

impl FaultClass {
    pub(crate) const ALL: [FaultClass; 25] = [
        FaultClass::Status(Status::NotFound),
        FaultClass::Status(Status::Exists),
        FaultClass::Status(Status::ReadOnly),
        FaultClass::Status(Status::IsDir),
        FaultClass::Status(Status::NoSpace),
        FaultClass::Status(Status::BadRequest),
        FaultClass::Status(Status::CrossRoot),
        FaultClass::Status(Status::NotADirectory),
        FaultClass::Status(Status::IoError),
        FaultClass::Status(Status::BadFh),
        FaultClass::Status(Status::NotSupported),
        FaultClass::Status(Status::ReplyTooLarge),
        FaultClass::UnknownNegative,
        FaultClass::Positive,
        FaultClass::Overflow,
        FaultClass::Truncated,
        FaultClass::Trailing,
        FaultClass::Garbage,
        FaultClass::Bulk(BulkLie::PastArena),
        FaultClass::Bulk(BulkLie::PastBank),
        FaultClass::Bulk(BulkLie::Wrapping),
        FaultClass::Bulk(BulkLie::LenOverRequest),
        FaultClass::NoReply,
        FaultClass::Late,
        // Not a fault: what [`expected`] gives for it is the row's success, the
        // answer to a good reply. Here so every status of `Status::ALL` is.
        // A row skips it.
        FaultClass::Status(Status::Ok),
    ];

    /// The harness fault that makes this reply.
    fn fault(self) -> Fault {
        match self {
            FaultClass::Status(s) => Fault::Status(s),
            FaultClass::UnknownNegative => Fault::RawStatus(-99),
            FaultClass::Positive => Fault::RawStatus(1),
            FaultClass::Overflow => Fault::Oversize,
            FaultClass::Truncated => Fault::Rewrite(drop_last_byte),
            FaultClass::Trailing => Fault::Trailing(vec![0xFF; 4]),
            FaultClass::Garbage => Fault::Garbage(vec![0xFF; 3]),
            FaultClass::Bulk(lie) => Fault::BulkOutside(lie),
            FaultClass::NoReply | FaultClass::Late => Fault::Delay(HELD),
        }
    }
}

/// The OK payload less its last byte (an empty one stays empty).
fn drop_last_byte(r: &mut OwnedReplyBytes) {
    if r.status == Status::Ok.code() {
        r.payload.pop();
    }
}

/// What the program gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// This NTSTATUS (`STATUS_SUCCESS` when the call goes on as if the reply
    /// were good).
    Nt(i32),
    /// This Win32 error, from a call the shim answers at the Win32 level.
    Win32(u32),
    /// The call succeeds without the director's answer: a registry read
    /// serves the real key alone, a final name is the program's own
    /// spelling.
    Fallback,
    /// The shim's load fails.
    LoadFails,
    /// The fault cannot reach this operation (a bulk READ reply on an
    /// operation that reads nothing, a late reply to the one attach).
    NotApplicable,
}

pub(crate) const STATUS_SUCCESS: i32 = 0;
pub(crate) const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;
pub(crate) const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
pub(crate) const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
pub(crate) const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
pub(crate) const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
pub(crate) const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
pub(crate) const STATUS_DISK_FULL: i32 = 0xC000_007Fu32 as i32;
pub(crate) const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
pub(crate) const STATUS_NOT_SAME_DEVICE: i32 = 0xC000_00D4u32 as i32;
pub(crate) const STATUS_CANNOT_DELETE: i32 = 0xC000_0121u32 as i32;
pub(crate) const STATUS_KEY_DELETED: i32 = 0xC000_017Cu32 as i32;
pub(crate) const STATUS_REGISTRY_IO_FAILED: i32 = 0xC000_016Bu32 as i32;
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;

/// The NTSTATUS names a cell may hold, written without `STATUS_`.
const NT_NAMES: &[(i32, &str)] = &[
    (STATUS_NO_MORE_FILES, "NO_MORE_FILES"),
    (STATUS_UNSUCCESSFUL, "UNSUCCESSFUL"),
    (STATUS_INVALID_PARAMETER, "INVALID_PARAMETER"),
    (STATUS_ACCESS_DENIED, "ACCESS_DENIED"),
    (STATUS_OBJECT_NAME_NOT_FOUND, "OBJECT_NAME_NOT_FOUND"),
    (STATUS_OBJECT_NAME_COLLISION, "OBJECT_NAME_COLLISION"),
    (STATUS_DISK_FULL, "DISK_FULL"),
    (STATUS_FILE_IS_A_DIRECTORY, "FILE_IS_A_DIRECTORY"),
    (STATUS_NOT_SAME_DEVICE, "NOT_SAME_DEVICE"),
    (STATUS_CANNOT_DELETE, "CANNOT_DELETE"),
    (STATUS_KEY_DELETED, "KEY_DELETED"),
    (STATUS_REGISTRY_IO_FAILED, "REGISTRY_IO_FAILED"),
];

/// What the program gets when `fault` answers `op`'s request.
pub(crate) fn expected(op: HookedOp, fault: FaultClass) -> Expect {
    use FaultClass as F;
    use HookedOp as O;
    let ok = Expect::Nt(STATUS_SUCCESS);
    let fail = Expect::Nt(STATUS_UNSUCCESSFUL);
    let nt = Expect::Nt;
    match fault {
        F::Status(Status::Ok) => return ok,
        F::Bulk(_) if !op.reads() => return Expect::NotApplicable,
        F::Late if op == O::Attach => return Expect::NotApplicable,
        // A late reply answers nothing: the next call is served.
        F::Late => return ok,
        // An OK reply that carries nothing still decodes with bytes after it,
        // and one with nothing to cut is the director's own.
        F::Truncated | F::Trailing | F::Garbage if op.empty_reply() => return ok,
        _ => {}
    }
    match op {
        O::Attach => Expect::LoadFails,
        O::OpenRead => match fault {
            F::Status(Status::NotFound) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
            F::Status(Status::Exists) => nt(STATUS_OBJECT_NAME_COLLISION),
            F::Trailing => ok,
            _ => fail,
        },
        O::GetAttr => match fault {
            F::Status(Status::NotFound) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
            F::Trailing => ok,
            _ => fail,
        },
        O::OpenWrite => match fault {
            F::Status(Status::NotFound) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
            F::Status(Status::Exists) => nt(STATUS_OBJECT_NAME_COLLISION),
            F::Status(Status::ReadOnly) => nt(STATUS_ACCESS_DENIED),
            F::Status(Status::IsDir) => nt(STATUS_FILE_IS_A_DIRECTORY),
            F::Trailing => ok,
            _ => fail,
        },
        O::ReadInline | O::ReadBulk | O::SectionFill => match fault {
            F::Trailing => ok,
            _ => fail,
        },
        O::WriteFirst | O::WriteLater => match fault {
            F::Status(Status::NoSpace) => nt(STATUS_DISK_FULL),
            // The shim reads only the first word of a WRITE reply.
            F::Trailing | F::Truncated => ok,
            _ => fail,
        },
        O::SetSize | O::DeleteOnHandle => fail,
        O::ReadDir => match fault {
            // The directory is gone: an empty listing.
            F::Status(Status::NotFound) => nt(STATUS_NO_MORE_FILES),
            F::Trailing => ok,
            _ => fail,
        },
        O::Rename => match fault {
            F::Status(Status::CrossRoot) => nt(STATUS_NOT_SAME_DEVICE),
            _ => fail,
        },
        O::DeleteByName => match fault {
            F::Status(Status::NotFound) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
            F::Status(Status::ReadOnly) => nt(STATUS_ACCESS_DENIED),
            F::Status(Status::IsDir) => nt(STATUS_FILE_IS_A_DIRECTORY),
            _ => fail,
        },
        O::Mkdir => match fault {
            F::Status(Status::NotFound) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
            F::Status(Status::ReadOnly) => nt(STATUS_ACCESS_DENIED),
            // The director made the directory before a reply it sent too
            // late, or too large; the shim's question after the failed MKDIR
            // finds it there.
            F::NoReply | F::Overflow => nt(STATUS_OBJECT_NAME_COLLISION),
            _ => fail,
        },
        O::StoredNames => Expect::Fallback,
        O::RegRead => match fault {
            // The key's overlay is larger than one reply: it holds what the real
            // key does not, so the read fails rather than serve the real key.
            F::Status(Status::ReplyTooLarge) | F::Overflow => nt(STATUS_REGISTRY_IO_FAILED),
            _ => Expect::Fallback,
        },
        O::StageExec => Expect::Win32(ERROR_FILE_NOT_FOUND),
        O::RegWrite => match fault {
            F::Status(Status::BadRequest) => nt(STATUS_INVALID_PARAMETER),
            F::Status(Status::NotFound) => nt(STATUS_KEY_DELETED),
            F::Status(Status::Exists) => nt(STATUS_CANNOT_DELETE),
            _ => fail,
        },
    }
}

/// wire.md 3.7's columns: each header and the fault classes it stands for.
/// Every class in a column must give the same answer on every row.
pub(crate) const COLUMNS: &[(&str, &[FaultClass])] = &[
    ("`ST_NOT_FOUND`", &[FaultClass::Status(Status::NotFound)]),
    ("`ST_EXISTS`", &[FaultClass::Status(Status::Exists)]),
    ("`ST_READ_ONLY`", &[FaultClass::Status(Status::ReadOnly)]),
    ("`ST_IS_DIR`", &[FaultClass::Status(Status::IsDir)]),
    ("`ST_NO_SPACE`", &[FaultClass::Status(Status::NoSpace)]),
    (
        "`ST_BAD_REQUEST`",
        &[FaultClass::Status(Status::BadRequest)],
    ),
    ("`ST_CROSS_ROOT`", &[FaultClass::Status(Status::CrossRoot)]),
    (
        "`ST_REPLY_TOO_LARGE`",
        &[FaultClass::Status(Status::ReplyTooLarge)],
    ),
    (
        "any other status",
        &[
            FaultClass::Status(Status::NotADirectory),
            FaultClass::Status(Status::IoError),
            FaultClass::Status(Status::BadFh),
            FaultClass::Status(Status::NotSupported),
            FaultClass::UnknownNegative,
            FaultClass::Positive,
        ],
    ),
    ("overflow", &[FaultClass::Overflow]),
    ("truncated", &[FaultClass::Truncated]),
    ("trailing bytes", &[FaultClass::Trailing]),
    ("garbage", &[FaultClass::Garbage]),
    (
        "bulk reply out of place",
        &[
            FaultClass::Bulk(BulkLie::PastArena),
            FaultClass::Bulk(BulkLie::PastBank),
            FaultClass::Bulk(BulkLie::Wrapping),
            FaultClass::Bulk(BulkLie::LenOverRequest),
        ],
    ),
    ("no reply in time", &[FaultClass::NoReply]),
    ("the call after a late reply", &[FaultClass::Late]),
];

/// A cell as wire.md 3.7 writes it.
fn cell(e: Expect) -> String {
    match e {
        Expect::Nt(STATUS_SUCCESS) => "success".into(),
        Expect::Nt(s) => NT_NAMES
            .iter()
            .find(|(v, _)| *v == s)
            .map(|(_, n)| (*n).to_string())
            .unwrap_or_else(|| format!("{s:#x}")),
        Expect::Win32(ERROR_FILE_NOT_FOUND) => "refused".into(),
        Expect::Win32(e) => format!("Win32 error {e}"),
        Expect::Fallback => "fallback".into(),
        Expect::LoadFails => "load fails".into(),
        Expect::NotApplicable => "n/a".into(),
    }
}

/// The table as wire.md 3.7 should hold it, from [`expected`].
fn rendered() -> Vec<Vec<String>> {
    let mut rows = vec![std::iter::once("Operation".to_string())
        .chain(COLUMNS.iter().map(|(h, _)| (*h).to_string()))
        .collect::<Vec<_>>()];
    for op in HookedOp::ALL {
        let mut row = vec![op.label().to_string()];
        for (h, classes) in COLUMNS {
            let first = expected(op, classes[0]);
            for c in classes.iter() {
                assert_eq!(
                    expected(op, *c),
                    first,
                    "column {h} mixes answers on row {:?}: {c:?}",
                    op
                );
            }
            row.push(cell(first));
        }
        rows.push(row);
    }
    rows
}

fn markdown(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        out.push_str("| ");
        out.push_str(&r.join(" | "));
        out.push_str(" |\n");
        if i == 0 {
            out.push('|');
            out.push_str(&"---|".repeat(r.len()));
            out.push('\n');
        }
    }
    out
}

/// The first table after the `### 3.7` heading of `doc`, its rows split into
/// trimmed cells (the separator row left out).
fn table_in(doc: &str) -> Vec<Vec<String>> {
    let after = doc
        .split_once("\n### 3.7 ")
        .expect("wire.md has a section 3.7")
        .1;
    let section = after.split("\n### ").next().unwrap_or(after);
    section
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .filter(|l| !l.starts_with("|---") && !l.starts_with("| ---"))
        .map(|l| {
            l.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn every_fault_class_is_in_one_column() {
    for c in FaultClass::ALL {
        if c == FaultClass::Status(Status::Ok) {
            continue;
        }
        let n = COLUMNS.iter().filter(|(_, cs)| cs.contains(&c)).count();
        assert_eq!(n, 1, "{c:?} is in {n} columns");
    }
    // Every named status but OK is a fault class.
    for s in Status::ALL {
        assert!(FaultClass::ALL.contains(&FaultClass::Status(*s)), "{s:?}");
    }
}

#[test]
fn the_wire_document_s_fault_table_is_the_test_s() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/wire.md");
    let doc = std::fs::read_to_string(&path).expect("docs/wire.md");
    let want = rendered();
    let got = table_in(&doc);
    let mut diffs = Vec::new();
    for (i, w) in want.iter().enumerate() {
        match got.get(i) {
            Some(g) if g == w => {}
            Some(g) => {
                for (j, wc) in w.iter().enumerate() {
                    let gc = g.get(j).map(String::as_str).unwrap_or("<missing>");
                    if gc != wc {
                        diffs.push(format!(
                            "row {:?}, column {:?}: wire.md has {gc:?}, the table {wc:?}",
                            w[0], want[0][j]
                        ));
                    }
                }
                if g.len() != w.len() {
                    diffs.push(format!(
                        "row {:?}: {} cells, want {}",
                        w[0],
                        g.len(),
                        w.len()
                    ));
                }
            }
            None => diffs.push(format!("row {:?} is missing", w[0])),
        }
    }
    if got.len() > want.len() {
        diffs.push(format!("wire.md has {} rows more", got.len() - want.len()));
    }
    assert!(
        diffs.is_empty(),
        "wire.md 3.7 differs from the table:\n{}\n\nThe table as it should read:\n\n{}",
        diffs.join("\n"),
        markdown(&want)
    );
}

// ---- running a row -------------------------------------------------------

/// The client's deadline in these tests.
pub(crate) const DEADLINE: Duration = Duration::from_millis(300);

/// How long a delayed reply is held: well past the deadline, released as
/// soon as the call has given up.
pub(crate) const HELD: Duration = Duration::from_secs(5);

/// How long a call may take before the row calls it hung.
const WATCHDOG: Duration = Duration::from_secs(8);

/// A tap behind the harness chain, for a setup below.
pub(crate) type Tapped = Box<dyn aether_rpc::Tap + Send>;

/// What one call saw: the table's kind of answer, or a description of an
/// answer that is wrong (success with the wrong bytes, say).
pub(crate) type Seen = Result<Expect, String>;

/// Everything a row's calls need, shared with the threads they run on.
pub(crate) struct Ctx {
    /// The virtual root on disk.
    pub(crate) root: PathBuf,
    /// A registry key handle opened through the hooks (`HKCU\<key>`).
    pub(crate) key: usize,
    /// The real image under the root, for `StageExec`.
    pub(crate) image: PathBuf,
}

/// How a row's call is made: on fresh names `i`.
type Call = fn(&'static Ctx, HookedOp, usize) -> Seen;

/// Runs `op`'s call `i` on its own thread, under the watchdog.
fn watched(ctx: &'static Ctx, op: HookedOp, i: usize, call: Call) -> Seen {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(call(ctx, op, i));
    });
    match rx.recv_timeout(WATCHDOG) {
        Ok(seen) => seen,
        Err(_) => panic!("{op:?} call {i} hung past {WATCHDOG:?}"),
    }
}

/// Waits until no request is with the director.
fn drained(served: &Served) {
    let end = Instant::now() + Duration::from_secs(10);
    while served.recorder.in_flight() > 0 {
        assert!(Instant::now() < end, "a request stayed in flight");
        // SAFETY: plain FFI.
        unsafe { windows_sys::Win32::System::Threading::Sleep(1) };
    }
}

/// Every fault class of the table against `op`, as described at the top.
/// `recover_each`: make the unfaulted call after every class (else once, at
/// the end).
fn run_row(served: &Served, ctx: &'static Ctx, op: HookedOp, call: Call, recover_each: bool) {
    let panics = aether_hooks::hook_panics_total();
    let mut next = 0usize;
    let mut fresh = || {
        next += 1;
        next
    };
    let mut wrong = Vec::new();
    let success = expected(op, FaultClass::Status(Status::Ok));
    // The unfaulted call works before any fault.
    let first = watched(ctx, op, fresh(), call);
    assert_eq!(first, Ok(success), "{op:?} without a fault");
    for class in FaultClass::ALL {
        if class == FaultClass::Status(Status::Ok) {
            continue;
        }
        let want = expected(op, class);
        if want == Expect::NotApplicable {
            continue;
        }
        let id = served.faults.arm(
            Match {
                op: Some(op.opcode()),
                nth: (op == HookedOp::WriteLater).then_some(2),
                times: op.times(),
                ..Match::default()
            },
            class.fault(),
        );
        let mut got = watched(ctx, op, fresh(), call);
        if matches!(class, FaultClass::NoReply | FaultClass::Late) {
            served.faults.release();
            drained(served);
        }
        served.faults.clear();
        if class == FaultClass::Late {
            got = watched(ctx, op, fresh(), call);
        }
        let applied = served.faults.applied(id);
        if applied == 0 || (applied > 1 && op.times().is_some()) {
            wrong.push(format!("{class:?}: the fault applied {applied} times"));
        }
        if got != Ok(want) {
            wrong.push(format!("{class:?}: got {got:?}, the table says {want:?}"));
        }
        if recover_each {
            let after = watched(ctx, op, fresh(), call);
            if after != Ok(success) {
                wrong.push(format!("{class:?}: the next call got {after:?}"));
            }
        }
    }
    let after = watched(ctx, op, fresh(), call);
    if after != Ok(success) {
        wrong.push(format!("after the row: {after:?}"));
    }
    if aether_hooks::hook_panics_total() != panics {
        wrong.push("a hook panicked".into());
    }
    assert!(
        wrong.is_empty(),
        "{op:?} ({}):\n{}",
        op.label(),
        wrong.join("\n")
    );
}

// ---- the file rows -------------------------------------------------------

/// The size of `save/data.bin`, read inline and in bulk.
const DATA_LEN: usize = 256 * 1024;
/// The size of `save/big.bin`, mapped.
const BIG_LEN: usize = 1024 * 1024;
/// What one WRITE carries at the default `payload_cap` (4096 - 24).
pub(crate) const CHUNK: usize = 4072;
/// How many fresh names a row may use.
const NAMES: usize = 128;

/// The file rows' director: a read-only `ro/` and `Names/`, a writable
/// `save/`.
fn file_director() -> Fake {
    file_director_named(NAMES)
}

/// [`file_director`] with `names` names under `Names/`, one for each final
/// name a test asks for (an answered one is remembered).
pub(crate) fn file_director_named(names: usize) -> Fake {
    let mut fake = Fake::new()
        .with_dir("ro")
        .with("ro/a.txt", b"hello".to_vec(), ReadStyle::Whole)
        .with("save/data.bin", pattern(DATA_LEN), ReadStyle::Whole)
        .with("save/big.bin", pattern(BIG_LEN), ReadStyle::Whole)
        .with("save/ow.bin", b"open me".to_vec(), ReadStyle::Whole)
        .writable_under("save/");
    for i in 0..names {
        fake = fake.with(
            &format!("Names/N{i}/File.txt"),
            b"x".to_vec(),
            ReadStyle::Whole,
        );
    }
    fake
}

fn file_row(op: HookedOp) {
    let (served, ctx) = file_setup(file_director(), None);
    run_row(served, ctx, op, file_call, true);
}

/// `fake` served at the client deadline [`DEADLINE`] and the hooks
/// installed: what [`file_call`] needs. `tap` sees each reply as the
/// director gave it (`Options::tap`).
pub(crate) fn file_setup(fake: Fake, tap: Option<Tapped>) -> (&'static Served, &'static Ctx) {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    let served = fakedirector::install_with(
        &root,
        fake,
        Options {
            deadline: Some(DEADLINE),
            tap,
            ..Options::default()
        },
    );
    std::mem::forget(aether_hooks::install().expect("install"));
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
        root,
        key: 0,
        image: PathBuf::new(),
    }));
    (served, ctx)
}

fn p(ctx: &Ctx, rel: &str) -> String {
    ctx.root.join(rel).to_string_lossy().into_owned()
}

#[repr(C)]
struct IoStatusBlock {
    status: isize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        handle: *mut *mut c_void,
        access: u32,
        oa: *const crate::ntapi::ObjectAttributes,
        iosb: *mut IoStatusBlock,
        alloc: *const i64,
        attrs: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_len: u32,
    ) -> i32;
    fn NtWriteFile(
        handle: *mut c_void,
        event: *mut c_void,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut IoStatusBlock,
        buffer: *const c_void,
        length: u32,
        offset: *const i64,
        key: *const u32,
    ) -> i32;
    fn NtReadFile(
        handle: *mut c_void,
        event: *mut c_void,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut IoStatusBlock,
        buffer: *mut c_void,
        length: u32,
        offset: *const i64,
        key: *const u32,
    ) -> i32;
    fn NtCreateSection(
        section: *mut *mut c_void,
        access: u32,
        oa: *const c_void,
        max_size: *const i64,
        protection: u32,
        attributes: u32,
        file: *mut c_void,
    ) -> i32;
    fn NtQueryDirectoryFile(
        handle: *mut c_void,
        event: *mut c_void,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut IoStatusBlock,
        info: *mut c_void,
        length: u32,
        class: u32,
        single: u8,
        name: *const crate::ntapi::UnicodeString,
        restart: u8,
    ) -> i32;
    fn NtQueryValueKey(
        key: isize,
        name: *const c_void,
        class: u32,
        info: *mut c_void,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn NtSetValueKey(
        key: isize,
        name: *const c_void,
        title: u32,
        ty: u32,
        data: *const c_void,
        len: u32,
    ) -> i32;
}

const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_SHARE_ALL: u32 = 7;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OVERWRITE_IF: u32 = 5;
const FILE_DIRECTORY_FILE: u32 = 0x01;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_END_OF_FILE_INFORMATION: u32 = 20;
const FILE_POSITION_INFORMATION: u32 = 14;
const FILE_DIRECTORY_INFORMATION: u32 = 1;

/// `NtCreateFile` of an absolute path: its status and handle.
fn nt_create(path: &str, access: u32, disposition: u32, options: u32) -> (i32, *mut c_void) {
    let n = abs_name(path);
    let mut h: *mut c_void = core::ptr::null_mut();
    let mut iosb = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: an OBJECT_ATTRIBUTES naming `path`, out-parameters of this frame.
    let st = unsafe {
        NtCreateFile(
            &mut h,
            access | SYNCHRONIZE,
            &n.oa,
            &mut iosb,
            core::ptr::null(),
            0,
            FILE_SHARE_ALL,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            core::ptr::null(),
            0,
        )
    };
    (st, h)
}

/// A new file in `save/` open for writing (and deleting): the handle.
pub(crate) fn new_file(ctx: &Ctx, rel: &str) -> Result<*mut c_void, String> {
    let (st, h) = nt_create(
        &p(ctx, rel),
        FILE_READ_DATA | FILE_WRITE_DATA | DELETE,
        FILE_OVERWRITE_IF,
        FILE_NON_DIRECTORY_FILE,
    );
    if st != 0 {
        return Err(format!("creating {rel}: {st:#x}"));
    }
    Ok(h)
}

pub(crate) fn close(h: *mut c_void) {
    crate::ntapi::close(h);
}

/// `NtWriteFile` of `data` at offset 0: the status and `Information`.
pub(crate) fn nt_write(h: *mut c_void, data: &[u8]) -> (i32, usize) {
    let mut iosb = IoStatusBlock {
        status: 0,
        information: usize::MAX,
    };
    let at = 0i64;
    // SAFETY: a synchronous handle this test opened, `data` readable.
    let st = unsafe {
        NtWriteFile(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            data.as_ptr().cast(),
            data.len() as u32,
            &at,
            core::ptr::null(),
        )
    };
    (st, iosb.information)
}

/// `NtSetInformationFile(FileEndOfFileInformation)` with the size `at`
/// bytes into `buf`.
fn set_size_in(h: *mut c_void, buf: &mut [u8], at: usize, size: i64) -> i32 {
    buf[at..at + 8].copy_from_slice(&size.to_le_bytes());
    nt_set_info_raw(h, &mut buf[at..at + 8], FILE_END_OF_FILE_INFORMATION)
}

/// [`set_size_in`] from an 8-aligned buffer.
fn set_size(h: *mut c_void, size: i64) -> i32 {
    let mut buf = [0u64; 1];
    // SAFETY: the u64's 8 bytes, viewed as bytes.
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), 8) };
    set_size_in(h, bytes, 0, size)
}

/// Found by the set-size row: a size read from a buffer that is not
/// 8-aligned panicked inside the hook. It is read where it lies.
#[test]
fn a_size_from_a_buffer_off_alignment_is_set() {
    isolate!();
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    let served = fakedirector::install_with(&root, file_director(), Options::default());
    std::mem::forget(aether_hooks::install().expect("install"));
    let ctx = Ctx {
        root,
        key: 0,
        image: PathBuf::new(),
    };
    let panics = aether_hooks::hook_panics_total();
    let h = new_file(&ctx, "save/odd.bin").unwrap();
    let mut buf = [0u64; 2];
    // SAFETY: the two u64s' 16 bytes, viewed as bytes.
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), 16) };
    assert_eq!(set_size_in(h, bytes, 1, 10), STATUS_SUCCESS);
    close(h);
    assert_eq!(
        aether_hooks::hook_panics_total(),
        panics,
        "no hook panicked"
    );
    assert_eq!(
        served.fake.contents("save/odd.bin").map(|c| c.len()),
        Some(10)
    );
}

/// [`a_size_from_a_buffer_off_alignment_is_set`] for the file position.
#[test]
fn a_position_from_a_buffer_off_alignment_is_set() {
    isolate!();
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    fakedirector::install_with(&root, file_director(), Options::default());
    std::mem::forget(aether_hooks::install().expect("install"));
    let ctx = Ctx {
        root,
        key: 0,
        image: PathBuf::new(),
    };
    let panics = aether_hooks::hook_panics_total();
    let (st, h) = nt_open_abs(&p(&ctx, "save/data.bin"), FILE_READ_DATA);
    assert_eq!(st, STATUS_SUCCESS);
    let mut buf = [0u64; 2];
    // SAFETY: the two u64s' 16 bytes, viewed as bytes.
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), 16) };
    bytes[1..9].copy_from_slice(&1000i64.to_le_bytes());
    let st = nt_set_info_raw(h, &mut bytes[1..9], FILE_POSITION_INFORMATION);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(
        aether_hooks::hook_panics_total(),
        panics,
        "no hook panicked"
    );
    // A read at the file pointer starts there.
    let mut got = [0u8; 16];
    let (st, n) = read_at_pointer(h, &mut got);
    close(h);
    assert_eq!((st, n), (STATUS_SUCCESS, 16));
    assert_eq!(got, pattern(DATA_LEN)[1000..1016]);
}

/// `NtReadFile` at the file pointer: the status and `Information`.
fn read_at_pointer(h: *mut c_void, buf: &mut [u8]) -> (i32, usize) {
    let mut iosb = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: a synchronous handle this test opened, `buf` writable.
    let st = unsafe {
        NtReadFile(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            &mut iosb,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            core::ptr::null(),
            core::ptr::null(),
        )
    };
    (st, iosb.information)
}

/// wire.md 2.1: the arena starts at `VFS_ARENA_OFFSET`, which need not be
/// the end of the last slot. With the arena 64 KiB and 8 bytes past it,
/// every bulk READ is served, and a reply naming another slot's bank, or
/// past the arena, still fails.
#[test]
fn an_arena_away_from_the_ring_serves_every_bulk_read() {
    isolate!();
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    let served = fakedirector::install_with(
        &root,
        file_director(),
        Options {
            arena_gap: 64 * 1024 + 8,
            deadline: Some(DEADLINE),
            ..Options::default()
        },
    );
    std::mem::forget(aether_hooks::install().expect("install"));
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
        root,
        key: 0,
        image: PathBuf::new(),
    }));
    let ok = Ok(Expect::Nt(STATUS_SUCCESS));
    let mark = served.recorder.mark();
    assert_eq!(file_call(ctx, HookedOp::ReadBulk, 0), ok, "a bulk read");
    assert_eq!(file_call(ctx, HookedOp::SectionFill, 0), ok, "a section");
    assert!(
        served
            .recorder
            .since(mark)
            .iter()
            .any(|r| r.op() == Some(Opcode::Read) && r.bulk_len > 0),
        "bulk replies were served"
    );
    for lie in [BulkLie::PastBank, BulkLie::PastArena] {
        let id = served.faults.arm(
            Match {
                op: Some(Opcode::Read),
                times: Some(1),
                ..Match::default()
            },
            Fault::BulkOutside(lie),
        );
        assert_eq!(
            file_call(ctx, HookedOp::ReadBulk, 0),
            Ok(Expect::Nt(STATUS_UNSUCCESSFUL)),
            "{lie:?}"
        );
        assert_eq!(served.faults.applied(id), 1, "{lie:?} fired");
    }
    assert_eq!(file_call(ctx, HookedOp::ReadBulk, 0), ok, "and then served");
}

/// An NT status as what a call saw.
pub(crate) fn status(st: i32) -> Seen {
    Ok(Expect::Nt(st))
}

pub(crate) fn file_call(ctx: &'static Ctx, op: HookedOp, i: usize) -> Seen {
    match op {
        HookedOp::OpenRead => {
            let (st, h) = nt_open_abs(&p(ctx, "ro/a.txt"), FILE_READ_DATA);
            if st == 0 {
                close(h);
            }
            status(st)
        }
        HookedOp::OpenWrite => {
            let (st, h) = nt_create(
                &p(ctx, "save/ow.bin"),
                FILE_WRITE_DATA,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE,
            );
            if st == 0 {
                close(h);
            }
            status(st)
        }
        HookedOp::GetAttr => {
            let (st, info) = nt_query_full_attributes_abs(&p(ctx, "ro/a.txt"));
            if st == 0 && info.end_of_file != 5 {
                return Err(format!("success with size {}", info.end_of_file));
            }
            status(st)
        }
        HookedOp::ReadInline | HookedOp::ReadBulk => {
            let (st, h) = nt_open_abs(&p(ctx, "save/data.bin"), FILE_READ_DATA);
            if st != 0 {
                return Err(format!("open: {st:#x}"));
            }
            let len = if op == HookedOp::ReadInline {
                100
            } else {
                128 * 1024
            };
            let mut buf = vec![0u8; len];
            let (st, n) = nt_read_at(h, 0, &mut buf);
            close(h);
            if st == 0 && (n != len || buf != pattern(DATA_LEN)[..len]) {
                return Err(format!("success with {n} bytes, not the file's"));
            }
            status(st)
        }
        HookedOp::SectionFill => section_fill(ctx),
        HookedOp::WriteFirst | HookedOp::WriteLater => {
            let h = new_file(ctx, &format!("save/w{i}.bin"))?;
            let len = if op == HookedOp::WriteFirst {
                100
            } else {
                2 * CHUNK + 10
            };
            let data = vec![0x5Au8; len];
            let (st, n) = nt_write(h, &data);
            close(h);
            let landed = if st == 0 {
                len
            } else if op == HookedOp::WriteLater {
                CHUNK
            } else {
                0
            };
            if n != landed {
                return Err(format!("{st:#x} with Information {n}, not {landed}"));
            }
            status(st)
        }
        HookedOp::SetSize => {
            let h = new_file(ctx, &format!("save/s{i}.bin"))?;
            let st = set_size(h, 10);
            close(h);
            status(st)
        }
        HookedOp::ReadDir => {
            let (st, h) = nt_open_dir_abs(&p(ctx, "ro"), FILE_LIST_DIRECTORY);
            if st != 0 {
                return Err(format!("open: {st:#x}"));
            }
            let mut buf = vec![0u8; 4096];
            let mut iosb = IoStatusBlock {
                status: 0,
                information: 0,
            };
            // SAFETY: a directory handle this test opened, a 4 KiB buffer.
            let st = unsafe {
                NtQueryDirectoryFile(
                    h,
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                    &mut iosb,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    FILE_DIRECTORY_INFORMATION,
                    0,
                    core::ptr::null(),
                    1,
                )
            };
            close(h);
            if st == 0 {
                // FILE_DIRECTORY_INFORMATION: FileNameLength@60, FileName@64.
                let n = u32::from_le_bytes(buf[60..64].try_into().unwrap()) as usize;
                let units: Vec<u16> = buf[64..64 + n]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let name = String::from_utf16_lossy(&units);
                if name != "a.txt" {
                    return Err(format!("success listing {name:?}"));
                }
            }
            status(st)
        }
        HookedOp::Rename => {
            let h = new_file(ctx, &format!("save/r{i}.txt"))?;
            let st = nt_rename(
                h,
                &p(ctx, &format!("save/r{i}-moved.txt")),
                FILE_RENAME_INFORMATION,
            );
            close(h);
            status(st)
        }
        HookedOp::DeleteByName => {
            close(new_file(ctx, &format!("save/d{i}.txt"))?);
            status(nt_delete_file(&p(ctx, &format!("save/d{i}.txt"))))
        }
        HookedOp::DeleteOnHandle => {
            let h = new_file(ctx, &format!("save/h{i}.txt"))?;
            let st = nt_set_disposition_delete(h);
            close(h);
            status(st)
        }
        HookedOp::Mkdir => {
            let (st, h) = nt_create(
                &p(ctx, &format!("save/dir{i}")),
                FILE_LIST_DIRECTORY,
                FILE_CREATE,
                FILE_DIRECTORY_FILE,
            );
            if st == 0 {
                close(h);
            }
            status(st)
        }
        HookedOp::StoredNames => stored_name(ctx, i),
        _ => unreachable!("{op:?} is not a file row"),
    }
}

fn section_fill(ctx: &Ctx) -> Seen {
    use windows_sys::Win32::System::Memory::{
        MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    };
    const SECTION_QUERY: u32 = 0x0001;
    const SECTION_MAP_READ: u32 = 0x0004;
    const PAGE_READONLY: u32 = 0x02;
    const SEC_COMMIT: u32 = 0x0800_0000;
    let (st, file) = nt_open_abs(&p(ctx, "save/big.bin"), FILE_READ_DATA);
    if st != 0 {
        return Err(format!("open: {st:#x}"));
    }
    let mut s: *mut c_void = core::ptr::null_mut();
    // SAFETY: a file handle this test opened; an unnamed section of all of it.
    let st = unsafe {
        NtCreateSection(
            &mut s,
            SECTION_QUERY | SECTION_MAP_READ,
            core::ptr::null(),
            core::ptr::null(),
            PAGE_READONLY,
            SEC_COMMIT,
            file,
        )
    };
    let mut seen = status(st);
    if st == 0 {
        // SAFETY: the section just created, mapped whole for reading.
        let v = unsafe { MapViewOfFile(s as _, FILE_MAP_READ, 0, 0, 0) };
        if v.Value.is_null() {
            seen = Err("success, but no view".into());
        } else {
            // SAFETY: the view holds the section's BIG_LEN bytes.
            let bytes = unsafe { core::slice::from_raw_parts(v.Value as *const u8, BIG_LEN) };
            if bytes != pattern(BIG_LEN).as_slice() {
                seen = Err("success with bytes that are not the file's".into());
            }
            // SAFETY: the view mapped above, once.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v.Value }) };
        }
        close(s);
    }
    close(file);
    seen
}

fn stored_name(ctx: &Ctx, i: usize) -> Seen {
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    // A name `i` the director does not hold fails the open below.
    let (st, h) = nt_open_abs(&p(ctx, &format!("names/n{i}/file.txt")), FILE_READ_DATA);
    if st != 0 {
        return Err(format!("open: {st:#x}"));
    }
    let mut buf = [0u16; 1024];
    // SAFETY: a handle this test opened, a buffer of its length.
    let n = unsafe { GetFinalPathNameByHandleW(h as _, buf.as_mut_ptr(), buf.len() as u32, 0) };
    close(h);
    if n == 0 || n as usize >= buf.len() {
        return Err("GetFinalPathNameByHandleW failed".into());
    }
    let name = String::from_utf16_lossy(&buf[..n as usize]);
    if name.ends_with(&format!(r"\Names\N{i}\File.txt")) {
        Ok(Expect::Nt(STATUS_SUCCESS))
    } else if name.ends_with(&format!(r"\n{i}\file.txt")) {
        // The program's spelling for what the director did not answer (the
        // shared `Names` may be known from an earlier answer).
        Ok(Expect::Fallback)
    } else {
        Err(format!("the name {name:?}"))
    }
}

macro_rules! file_rows {
    ($($name:ident: $op:ident,)*) => {$(
        #[test]
        fn $name() {
            isolate!();
            file_row(HookedOp::$op);
        }
    )*};
}

file_rows! {
    row_open_to_read: OpenRead,
    row_open_to_write: OpenWrite,
    row_attributes_by_name: GetAttr,
    row_read_inline: ReadInline,
    row_read_bulk: ReadBulk,
    row_section_filled_at_creation: SectionFill,
    row_write_first_chunk: WriteFirst,
    row_write_a_later_chunk: WriteLater,
    row_set_the_size: SetSize,
    row_list_a_directory: ReadDir,
    row_rename: Rename,
    row_delete_by_name: DeleteByName,
    row_delete_through_a_handle: DeleteOnHandle,
    row_create_a_directory: Mkdir,
    row_final_name: StoredNames,
}

// ---- attach ----------------------------------------------------------------

/// The attach is the shim's first HEARTBEAT on a client connected from the
/// environment, the one decision `try_init_from_env` makes; a row's calls
/// connect a client each, so one process can attach many times.
fn attach_call(_ctx: &'static Ctx, _op: HookedOp, _i: usize) -> Seen {
    let client = aether_hooks::director::connect_from_env()
        .map_err(|e| format!("connect: {e:?}"))?
        .with_deadline(DEADLINE);
    Ok(match client.heartbeat() {
        Ok(()) => Expect::Nt(STATUS_SUCCESS),
        Err(_) => Expect::LoadFails,
    })
}

#[test]
fn row_attach() {
    isolate!();
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    let served = fakedirector::install_with(
        &root,
        Fake::new(),
        Options {
            deadline: Some(DEADLINE),
            ..Options::default()
        },
    );
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
        root,
        key: 0,
        image: PathBuf::new(),
    }));
    run_row(served, ctx, HookedOp::Attach, attach_call, true);
}

// ---- stage exec ----------------------------------------------------------

/// The served image's name under root 0.
const IMAGE: &str = "aether-probe.exe";

/// `CreateProcessW` of the image under the root. A child that starts is
/// killed (the shim the hooks inject is this test executable, which never
/// says it is ready, so its start ends `ERROR_PROCESS_ABORTED`): either is
/// a start; any other error is the refusal.
pub(crate) fn stage_call(ctx: &'static Ctx, _op: HookedOp, _i: usize) -> Seen {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_PROCESS_ABORTED};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let app = ctx.image.to_string_lossy().into_owned();
    let app_w: Vec<u16> = app.encode_utf16().chain(Some(0)).collect();
    let mut cmd: Vec<u16> = format!("\"{app}\"").encode_utf16().chain(Some(0)).collect();
    // SAFETY: zeroed is a valid STARTUPINFOW once `cb` is set.
    let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
    si.cb = core::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: zeroed is a valid out-parameter.
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: NUL-terminated strings, a mutable command line, no attributes.
    let ok = unsafe {
        CreateProcessW(
            app_w.as_ptr(),
            cmd.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            0,
            core::ptr::null(),
            core::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: plain FFI.
        let e = unsafe { GetLastError() };
        return Ok(if e == ERROR_PROCESS_ABORTED {
            Expect::Nt(STATUS_SUCCESS)
        } else {
            Expect::Win32(e)
        });
    }
    // SAFETY: the handles the successful call returned, closed once.
    unsafe {
        TerminateProcess(pi.hProcess, 1);
        WaitForSingleObject(pi.hProcess, 10_000);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(Expect::Nt(STATUS_SUCCESS))
}

/// `aether-probe.exe` as cargo built it. `None` prints SKIP, or panics
/// under `VFS_TEST_REQUIRE_ALL=1`.
pub(crate) fn probe() -> Option<PathBuf> {
    let exe = std::env::current_exe().expect("current exe");
    let deps = exe.parent().expect("test exe folder");
    let profile = if deps.file_name().is_some_and(|n| n == "deps") {
        deps.parent().expect("profile folder")
    } else {
        deps
    };
    let p = profile.join(IMAGE);
    if p.is_file() {
        return Some(p);
    }
    if aether_env::opt_in("VFS_TEST_REQUIRE_ALL") {
        panic!("{} is missing: cargo build -p aether-inject", p.display());
    }
    eprintln!(
        "SKIP: {} is missing (cargo build -p aether-inject)",
        p.display()
    );
    None
}

#[test]
fn row_start_a_child() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (served, ctx) = stage_setup(&probe, None);
    // A start takes the ready wait: the unfaulted call is made once, at the
    // end, and after the late reply.
    run_row(served, ctx, HookedOp::StageExec, stage_call, false);
}

/// How long a child's start waits for its shim to say it is ready
/// ([`stage_setup`]'s `READY_TIMEOUT_SECS`); a start that succeeds takes it.
pub(crate) const READY_WAIT: Duration = Duration::from_secs(1);

/// The image `probe` served under the root and staged, at the client
/// deadline [`DEADLINE`], the hooks installed: what [`stage_call`] needs.
pub(crate) fn stage_setup(probe: &Path, tap: Option<Tapped>) -> (&'static Served, &'static Ctx) {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let base = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (root, backing, stage) = (base.join("root"), base.join("backing"), base.join("stage"));
    for d in [&root, &backing, &stage] {
        std::fs::create_dir_all(d).unwrap();
    }
    // A real copy at the path under the root: a start the shim let the
    // kernel make from what is really there would succeed.
    std::fs::copy(probe, root.join(IMAGE)).unwrap();
    let staged = stage.join(IMAGE);
    std::fs::copy(probe, &staged).unwrap();
    std::env::set_var(
        aether_env::ROOT_BACKINGS,
        format!("0={}", backing.display()),
    );
    std::env::set_var(
        aether_env::READY_TIMEOUT_SECS,
        READY_WAIT.as_secs().to_string(),
    );
    std::env::set_var(aether_env::CHILD_REFUSED_LOG, base.join("refused.log"));
    let fake = Fake::new()
        .with(IMAGE, b"MZ served".to_vec(), ReadStyle::Whole)
        .staging(IMAGE, &staged.to_string_lossy());
    let served = fakedirector::install_with(
        &root,
        fake,
        Options {
            deadline: Some(DEADLINE),
            tap,
            ..Options::default()
        },
    );
    std::mem::forget(aether_hooks::install().expect("install"));
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
        image: root.join(IMAGE),
        root,
        key: 0,
    }));
    (served, ctx)
}

// ---- registry ------------------------------------------------------------

/// This process's key `HKCU\Software\AetherFaultTable-<pid>`: real, with
/// `v = 1`, and `v = 2` in the overlay.
static KEY: crate::reg::Base = crate::reg::Base::new(r"Software\AetherFaultTable");

/// What a registry read of `v` saw when it read the value `v`: the overlay's
/// 2, the real key's 1, or something else.
pub(crate) fn reg_value_seen(v: u32) -> Seen {
    match v {
        2 => Ok(Expect::Nt(STATUS_SUCCESS)),
        1 => Ok(Expect::Fallback),
        v => Err(format!("NtQueryValueKey: the value {v}")),
    }
}

pub(crate) fn reg_call(ctx: &'static Ctx, op: HookedOp, i: usize) -> Seen {
    let k = ctx.key as isize;
    match op {
        HookedOp::RegRead => {
            // KEY_VALUE_PARTIAL_INFORMATION: TitleIndex, Type, DataLength, Data.
            const KEY_VALUE_PARTIAL_INFORMATION: u32 = 2;
            let mut info = [0u8; 16];
            let mut ret = 0u32;
            let st = crate::reg::with_us("v", |name| {
                // SAFETY: a key this test opened, a name and a 16-byte buffer.
                unsafe {
                    NtQueryValueKey(
                        k,
                        name.cast(),
                        KEY_VALUE_PARTIAL_INFORMATION,
                        info.as_mut_ptr().cast(),
                        info.len() as u32,
                        &mut ret,
                    )
                }
            });
            if st != STATUS_SUCCESS {
                return status(st);
            }
            reg_value_seen(u32::from_le_bytes(info[12..16].try_into().unwrap()))
        }
        HookedOp::RegWrite => {
            const REG_DWORD: u32 = 4;
            let data = 7u32.to_le_bytes();
            let st = crate::reg::with_us(&format!("w{i}"), |name| {
                // SAFETY: a key this test opened, a name and 4 bytes of data.
                unsafe { NtSetValueKey(k, name.cast(), 0, REG_DWORD, data.as_ptr().cast(), 4) }
            });
            status(st)
        }
        _ => unreachable!("{op:?} is not a registry row"),
    }
}

fn registry_row(op: HookedOp) {
    let (served, ctx) = registry_setup(None);
    run_row(served, ctx, op, reg_call, true);
}

/// This process's scratch key, real `v = 1` and overlay `v = 2`, served at
/// the client deadline [`DEADLINE`] with nothing cached, the hooks installed
/// and the key open through them: what [`reg_call`] needs.
pub(crate) fn registry_setup(tap: Option<Tapped>) -> (&'static Served, &'static Ctx) {
    use windows_sys::Win32::System::Registry::{
        RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_DWORD,
    };
    KEY.reset(&[]);
    // Made before the hooks: the real key and its value.
    let real = crate::reg::reg_create(&KEY);
    let one = 1u32.to_le_bytes();
    // SAFETY: a key just created, 4 bytes of data.
    let st = unsafe {
        RegSetValueExW(
            real,
            crate::reg::wide("v").as_ptr(),
            0,
            REG_DWORD,
            one.as_ptr(),
            4,
        )
    };
    assert_eq!(st, 0, "the real value");
    // SAFETY: closed once.
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(real) };
    let served = crate::reg::install_hooks_with(
        "fault-table",
        Options {
            arena_len: 0,
            deadline: Some(DEADLINE),
            tap,
            // Nothing cached: every read asks the director.
            reg_gen: fakedirector::RegGen::Fixed(0),
            ..Options::default()
        },
    );
    let mut k: HKEY = core::ptr::null_mut();
    // SAFETY: an out-parameter of this frame.
    let st = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            crate::reg::wide(&KEY).as_ptr(),
            0,
            KEY_ALL_ACCESS,
            &mut k,
        )
    };
    assert_eq!(st, 0, "open through the hooks");
    let two = 2u32.to_le_bytes();
    // SAFETY: the key just opened, 4 bytes of data.
    let st = unsafe {
        RegSetValueExW(
            k,
            crate::reg::wide("v").as_ptr(),
            0,
            REG_DWORD,
            two.as_ptr(),
            4,
        )
    };
    assert_eq!(st, 0, "the overlay's value");
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
        root: PathBuf::new(),
        key: k as usize,
        image: PathBuf::new(),
    }));
    (served, ctx)
}

#[test]
fn row_registry_read() {
    isolate!();
    registry_row(HookedOp::RegRead);
}

#[test]
fn row_registry_write() {
    isolate!();
    registry_row(HookedOp::RegWrite);
}
