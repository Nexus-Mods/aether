//! The adapters built into the binary: `reference-adapter`, which serves a
//! scenario's tree from the reference director over a real ring, and the
//! hidden `test-adapter`, which misbehaves on purpose for the tests.
//!
//! The reference adapter is what a director's own adapter looks like: on
//! `mount` it builds [`Reference::from_tree`] from the scenario's tree,
//! creates a ring (a file by default, a named section with `--ring section`
//! on Windows), starts the same serve loop a Rust director runs
//! ([`aether_rpc::serve_with_arena`]) on a thread of its own, and answers the
//! handshake variables; on `unmount` it stops the loop, joins the thread and
//! removes the ring. A hidden, test-only `--break <rule>` makes it break one
//! rule on purpose ([`Break`]), for the tests that show a check finds it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use aether_ipc::{ring, DataArena, IpcError, Notifier, RingBacking, RingServer};
use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, LookupReply, Opcode, OpenFlags, OpenResp, ReadReply, Reply,
    Request, RequestFlags, RootId, Status,
};
use aether_refdirector::Reference;
use aether_rpc::{Director, Listing, ReadSlot};
use serde_json::{json, Value};

use crate::adapter::{answer_requests, AdapterRequest, Env};

/// The ring's shape: eight slots of the default payload cap, and a bank of
/// 1 MiB per slot (`docs/wire.md` 2.6 asks for at least 256 KiB).
const SLOTS: u32 = 8;
const PAYLOAD_CAP: u32 = aether_ipc::DEFAULT_PAYLOAD_CAP;
const BANK: usize = 1024 * 1024;
/// The longest [`Break::BumpBeforeWrite`] holds a write for readers.
const BUMP_HOLD: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest [`Break::CompleteAbandoned`] holds a slow READ for its client
/// to give it up.
const ABANDON_HOLD: std::time::Duration = std::time::Duration::from_millis(300);
/// [`Break::MtimeMillis`]'s time: 2020-01-01 in milliseconds.
const MILLIS: i64 = 1_577_836_800_000;
/// The threads serving a ring, each with its own handle on the reference
/// (`Reference::share`), and the `worker_hint` published (wire.md 2.8).
const WORKERS: u32 = 4;

/// Where the reference adapter's ring lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingKind {
    /// A file both sides map (`docs/wire.md` 2.10).
    File,
    /// A named section and two events (2.9). Windows only.
    Section,
}

/// One rule the reference adapter breaks on purpose, so that the tests can
/// show the checks find a director that gets it wrong. Test-only: reached
/// only through the hidden `reference-adapter --break <rule>` switch, which
/// the usage does not list (and, for the two rules only checks across steps
/// find, the hidden `test-adapter --misbehave stale-immutable` and
/// `frozen-reg-gen`); without it the adapter serves the reference as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Break {
    /// GETATTR of a missing path answers `found = 1` (wire.md 3.4.1 says
    /// `found = 0`).
    FoundForMissing,
    /// An unknown opcode or a payload that does not decode is answered
    /// `ST_IO_ERROR` (wire.md 3.3 and 3.5.1 say `ST_BAD_REQUEST`).
    IoErrorForBadRequest,
    /// Every read open of a file is answered `immutable`, writable ones
    /// included, with the `mount_gen` it always has: an overwrite of the file
    /// at the same size leaves the shim's read cache serving the old bytes
    /// (wire.md 3.4.3).
    StaleImmutable,
    /// The registry generation is published once, at attach, and never moved
    /// by a write (wire.md 4.5 says every successful write moves it).
    FrozenRegGen,
    /// When two requests are in flight, each one's reply goes into the
    /// other's slot (under the right `ack`): a director that mixes up its
    /// threads' answers (wire.md 2.5: a reply is written into the slot of
    /// the request it answers).
    SwapReplies,
    /// Handles are numbered per slot, as a director that keeps a handle table
    /// per connection would, so two clients on two slots get the same `fh`
    /// and a handle presented on another slot is `ST_BAD_FH` (wire.md 2.8.1:
    /// handles are ring-global, unique across every process, and any process
    /// may present any handle).
    PerSlotFh,
    /// Only slot 0 is ever taken; a request in any other slot waits for ever
    /// (wire.md 2.5: "Every taken slot MUST be completed", and slots are
    /// taken "in any order and from any number of threads"). A client that
    /// sends one request at a time from one thread only ever uses slot 0.
    SlotZeroOnly,
    /// Every bulk READ reply is written into bank 0, whatever slot it answers
    /// (wire.md 2.6: "Slot `s` owns bank `s % banks`").
    BankZero,
    /// The arena is one bank past the end of the last slot and
    /// `VFS_ARENA_OFFSET` is not set, so the shim, which then takes the arena
    /// to start right after the last slot, finds every bulk reply in the wrong
    /// bank (wire.md 2.1, 2.6).
    ArenaElsewhere,
    /// As [`Break::ArenaElsewhere`], but `VFS_ARENA_OFFSET` is set to the
    /// end of the last slot, which is not where the arena is (wire.md 2.6).
    ArenaOffsetWrong,
    /// Not a broken rule: handles are numbered down from `u64::MAX` (the
    /// reference's 1 is `u64::MAX`, its 2 is `u64::MAX - 1`), which wire.md
    /// 3.4.3 allows ("any u64 the director chooses"). A run must pass it,
    /// whatever numbers it sends for handles it has no number of its own for.
    DownwardFh,
    /// A bulk READ answers at most 256 KiB, as a director whose banks are
    /// smaller than `arena_len / slot_count` would (wire.md 2.6: each bank
    /// holds the shim's chunk; 3.4.4: `n < len` means end of file).
    ShortBulk,
    /// A READDIR listing larger than `payload_cap` is cut to the entries that
    /// fit and answered OK (wire.md 3.4.2: it must fit, and is answered with
    /// the overflow status otherwise; 2.5 step 3).
    TruncateReaddir,
    /// Each READ first writes over the banks of earlier bulk replies in other
    /// slots, as a director staging read-ahead in a bank it already answered
    /// from would (wire.md 2.6: the bank is the client's while its slot is
    /// `COMPLETED`).
    LateBank,
    /// READs of 256 KiB or more are held `PROCESSING` until the client gives
    /// them up (at most [`ABANDON_HOLD`]), so a client that gives one up
    /// as soon as it is taken always catches it, and a request given up
    /// while being processed is completed anyway, `COMPLETED` stored over
    /// `ABANDONED` (wire.md 2.5: "a director must never store COMPLETED over
    /// ABANDONED").
    CompleteAbandoned,
    /// A registry write moves `reg_gen` first and makes the write visible
    /// only once two more REG_KEYs have been answered (or after
    /// [`BUMP_HOLD`], when no reader comes), so a reader that reads the
    /// generation and then the key sees the new generation with the old
    /// value, whatever the timing (wire.md 4.5: it moves "before the write's
    /// reply is published", and the shim caches an answer under the
    /// generation it read before asking, so the new generation must not
    /// come before the write). Until a REG_KEY has been answered at all, a
    /// write is held for nothing.
    BumpBeforeWrite,
    /// `VFS_REGISTRY` is left out of the mount's environment though the tree
    /// has a registry layer (wire.md 5.3: set to 1 while a layer is
    /// attached; 4: the shim sends no registry request without it).
    NoRegistryEnv,
    /// `VFS_VIRTUAL_ROOTS` is left out, so roots other than 0 are served but
    /// never declared (wire.md 3.2, 5.3).
    UndeclaredRoots,
    /// The shim.cfg written has a byte after its table (wire.md 5.1:
    /// "Nothing may follow the table").
    BadShimCfg,
    /// Every TRANSPORT name the adapter does not set itself is passed on
    /// from its own environment, as a launch built on the director's
    /// environment would pass a stale session's on (wire.md 5.2: a launch
    /// sets or removes every one).
    InheritEnv,
    /// Handles are numbered from 0 (the reference's 1 is 0). wire.md 3.4.3:
    /// the shim keeps `fh` 0 for a directory it made with MKDIR, so a
    /// director never issues it.
    FhFromZero,
    /// GETATTR and READDIR report `mtime` in milliseconds (wire.md 3.2:
    /// "`mtime` is seconds since the Unix epoch").
    MtimeMillis,
    /// OPEN replies set bit 1 of the flags byte and byte 18 (wire.md 3.4.3:
    /// "other bits zero", and bytes 18-19 zero).
    OpenFlagBits,
    /// Not a broken rule: STAGE_EXEC copies the file to a folder of its own
    /// and answers that path, as a director with a stager does (wire.md
    /// 3.4.14), so the tests can show the staged copy is checked.
    StageCopy,
    /// STAGE_EXEC copies the file with its first byte changed (wire.md
    /// 3.4.14: the child is started from the copy, so it must be the file).
    StageWrongBytes,
    /// READ through a handle opened with `OPEN_WRITE` and not `OPEN_READ` is
    /// `ST_IO_ERROR` (wire.md 3.4.4: a handle serves READ whatever access it
    /// was opened with; the shim's write opens send `OPEN_WRITE` alone).
    WriteOnlyUnreadable,
    /// A WRITE past the end, or a SETATTR that grows a file, leaves `0xAA`
    /// in the gap (wire.md 3.4.5, 3.4.6: the gap reads as zeros).
    GrowthGarbage,
    /// A path with a stream suffix (`a.esp:s`) is served as its base file
    /// (wire.md 3.2: a stream the director does not hold is not the base
    /// file).
    StreamBase,
    /// Registry keys under the `User` hive are refused, `ST_BAD_REQUEST`
    /// (wire.md 4.2: the current user's hives are `<CurrentUser>` and
    /// `<CurrentUser>_Classes`, keys like any other).
    MachineOnlyRegistry,
    /// REG_DELETE_KEY of a key the overlay has no record of is
    /// `ST_NOT_FOUND`, so a real key can never be hidden (wire.md 4.3: any
    /// key not already tombstoned is tombstoned).
    DeleteUnrecordedNotFound,
    /// Each file handle reads a snapshot of the file taken at its OPEN,
    /// changed only by its own WRITEs and SETATTRs: what another handle
    /// writes reaches the file, GETATTR and new handles, never it (wire.md
    /// 3.4.4: "Every handle of a file reads the file as it is now").
    SnapshotReads,
    /// A SETATTR changes the file at once, but GETATTR, READDIR and OPEN
    /// report the size the file had before it until the handle's CLOSE, as
    /// a director that publishes sizes at close would (wire.md 3.4.1: a
    /// file's `size` is its length now, SETATTRs through handles still open
    /// included).
    SetattrAtClose,
    /// Every error is answered with a 4-byte payload (wire.md 3.1: "A
    /// director answers every error with an empty payload").
    ErrorPayload,
    /// An `OPEN_APPEND` handle writes at an end of its own: the size at its
    /// OPEN plus its own writes, whatever other handles did (wire.md 3.4.3:
    /// each WRITE lands at the file's end as it is when it is served).
    AppendOwnEnd,
    /// A SETATTR that cuts a file keeps the bytes it cut, and a later WRITE
    /// past the end through the same handle shows them again in the gap
    /// (wire.md 3.4.5, 3.4.6: the gap reads as zeros, "bytes a truncation
    /// cut off included").
    TruncateKeepsBytes,
    /// A refused registry write moves the overlay's version anyway (wire.md
    /// 4.3: "a refused write leaves it alone").
    RefusedWriteBumps,
    /// A RENAME of a directory leaves its children recorded under the old
    /// name, and a MKDIR that makes the old name again brings them back
    /// (wire.md 3.4.7: "a directory moves with everything under it").
    RenameDirGhosts,
    /// A file handle acts on whatever is at the name it was opened with: it
    /// loses its file to a RENAME, and reaches a file created at the name
    /// after its own was deleted (wire.md 3.4.7: "Its handles follow it";
    /// 3.4.8: "It never reads or changes a file created at, or moved to,
    /// that name later").
    HandlesByPath,
    /// An OPEN_CREATE or MKDIR in a missing directory makes the missing
    /// directories (wire.md 3.4.3, 3.4.9: `ST_NOT_FOUND`, and nothing is
    /// created).
    CreateMakesParents,
    /// An OPEN_CREATE or MKDIR below a file is `ST_IO_ERROR` (wire.md 3.4.3,
    /// 3.4.9: `ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`).
    FileParentIoError,
    /// A write OPEN of a directory, without OPEN_EXCL, opens it as a
    /// directory handle (wire.md 3.4.3: `ST_IS_DIR` or `ST_IO_ERROR`).
    DirWriteOpen,
    /// READDIR of a file lists the file (wire.md 3.4.2: an empty listing,
    /// `ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`).
    ReaddirFileListsItself,
    /// A RENAME of a file onto an empty directory answers `ST_OK` and moves
    /// nothing (wire.md 3.4.7: replaced, or refused with `ST_IO_ERROR`).
    FileOntoDirKept,
    /// A RENAME onto an ancestor of `from` is `ST_BAD_REQUEST` (wire.md
    /// 3.4.7: a directory with something under it, `ST_IO_ERROR`).
    AncestorBadRequest,
    /// A RENAME to `from` in the spelling it is stored with is `ST_EXISTS`
    /// (wire.md 3.4.7: `ST_OK`, and nothing changes).
    SameNameExists,
    /// A READ offset is taken modulo 2^32, as a 32-bit offset would be
    /// (wire.md 3.4.4: "no wrap: a READ at or past the end answers 0
    /// bytes").
    ReadOffsetWraps,
    /// A WRITE or SETATTR past the largest file the reference holds answers
    /// `ST_OK` and changes nothing (wire.md 3.4.5, 3.4.6: `ST_NO_SPACE` or
    /// `ST_BAD_REQUEST`).
    HugeWriteOk,
    /// A WRITE of more than one byte takes its first half and answers that
    /// count `ST_OK` (wire.md 3.4.5: on `ST_OK`, `written` is `len`).
    ShortWrite,
    /// RENAME_X between two roots onto a directory answers `ST_OK` and moves
    /// nothing (wire.md 3.4.13: `ST_IS_DIR` or `ST_IO_ERROR`).
    RenameXOntoDirOk,
    /// STORED_NAMES with a `skip` of every component answers the last one
    /// (wire.md 3.4.12: an empty reply or `ST_BAD_REQUEST`).
    SkipPastEndEcho,
    /// A create respells the existing directories above it to the caller's
    /// spelling (wire.md 3.2: a creating request's parents keep their stored
    /// spelling).
    ParentRespelled,
    /// OPEN_EXCL checks that the name is free, waits, then creates without
    /// checking again, so concurrent creates of one name all succeed
    /// (wire.md 2.8.1: "exactly one succeeds").
    RacyExcl,
    /// REG_CREATE_KEY checks that the key is absent, waits, then creates,
    /// and answers `ST_OK` when it finds the key made meanwhile (wire.md
    /// 2.8.1: "exactly one succeeds").
    RacyRegCreate,
    /// An append WRITE lands in two halves, each at the end as it is then,
    /// with a wait between (wire.md 2.8.1: "each append WRITE lands whole at
    /// an end").
    TornAppend,
    /// A WRITE reads the whole file, waits, and writes it back with its
    /// bytes in, so of concurrent WRITEs only the last lands (wire.md 2.8.1:
    /// "concurrent WRITEs to disjoint ranges all land").
    LostUpdate,
    /// A RENAME of a file to a free name copies it there, waits, then
    /// deletes `from`, so a request in flight with it sees both names
    /// (wire.md 2.8.1: "the state before it or the state after it, never a
    /// mixture").
    RenameInSteps,
    /// WRITEs are kept by the handle that made them and reach the file at
    /// its CLOSE: that handle reads them, nothing else sees them before
    /// (wire.md 3.4.1: a size is the file's length now, WRITEs through
    /// handles still open included; 3.4.4: every handle reads the file as it
    /// is now).
    WriteAtClose,
    /// GETATTR of a file's old name after a RENAME still finds it, as a
    /// lookup cache that RENAME does not clear would (wire.md 3.4.7: the file
    /// moves).
    RenameKeepsOldName,
    /// A RENAME that changes only a directory's case respells the directory,
    /// but STORED_NAMES of a path below it answers the old spelling, as a
    /// table of full stored paths not updated for a parent's respell would
    /// (wire.md 3.4.7: the rename respells the name; 3.4.12).
    CaseRenameStaleChildren,
    /// RENAME_X between roots copies the bytes the file had when the tree
    /// was mounted (none for a file made since), as a copy from the backing
    /// store that misses the session's writes would (wire.md 3.4.13: the
    /// file is copied).
    RenameXFromMount,
    /// REG_DELETE_KEY hides a key's values instead of dropping them, so a
    /// REG_CREATE_KEY of it brings them back (wire.md 4.3: "everything below
    /// it is dropped").
    DeleteKeepsSubtree,
    /// REG_CHANGED of a key renamed away says unchanged (wire.md 4.3: a key
    /// with no node is changed when it was "deleted or renamed away" after
    /// `since`).
    RenameUnchanged,
    /// REG_SET_VALUE under a tombstoned key or ancestor is `ST_NOT_FOUND`
    /// (wire.md 4.3: it makes the path exist, reviving the chain).
    SetUnderTombstoneNotFound,
    /// A value name's limit is counted in UTF-8 bytes (wire.md 4.1: limits
    /// count UTF-16 code units).
    ValueNameBytes,
    /// REG_SET_VALUE of a name a key holds deletes the value and appends a
    /// new one in the caller's spelling (wire.md 4.3: it replaces the value,
    /// "keeping its stored name").
    SetValueAppends,
    /// READ, WRITE, SETATTR and CLOSE of `fh` 0 answer `ST_OK` (wire.md
    /// 3.4.3: 0 is never issued; 3.4.10: `CLOSE(0)` is `ST_BAD_FH`).
    FhZeroOk,
    /// A name deleted and created again takes back the spelling it had
    /// before the delete, as a name table never cleaned of deleted names
    /// would (wire.md 3.2: a name keeps the spelling it was created with).
    SpellingOutlivesDelete,
    /// DELETE removes the path in every root that has it (wire.md 3.2: a
    /// request addresses its own root).
    DeleteIgnoresRoot,
    /// GETATTR of the root `.` is found = 0 (wire.md 3.2: the root is `.`;
    /// 3.4.1).
    RootNotFound,
    /// DELETE of a directory that holds only directories deletes them all
    /// (wire.md 3.4.8: a directory with anything under it is `ST_IO_ERROR`).
    DeleteDirIgnoresEmptyDirs,
    /// GETATTR answers the kind a path had the first time it was found, as a
    /// stat cache that DELETE does not clear would (wire.md 3.4.1).
    StaleKind,
    /// Not a broken rule but [`Break::StageCopy`]'s stager, copying the
    /// bytes the image had when the tree was mounted (wire.md 3.4.14: "A
    /// staged image holds the file's bytes as they are when STAGE_EXEC is
    /// served, writes in the session included").
    StageMountBytes,
    /// A bulk READ of a (file, offset, length) read in bulk before is
    /// answered with the bytes of that first read, as a read-ahead cache
    /// that writes do not clear would (wire.md 3.4.4: inline or bulk, a
    /// handle reads the file as it is now).
    BulkCache,
    /// DELETE of a directory under which a deleted file still has an open
    /// handle is `ST_IO_ERROR`, as a delete left pending until the last
    /// CLOSE would (wire.md 3.4.8: a file with open handles is deleted like
    /// any other).
    DeletePendingUntilClose,
    /// GETATTR replies carry two bytes after their 18 (wire.md 3.4.1:
    /// "Reply, 18 bytes"), which the shim's lenient decoder would ignore.
    ReplyTrailing,
    /// REG_KEY replies list a node's children in reverse order (wire.md 4.3:
    /// "sorted by `folded` in byte order"), which the shim's map would hide.
    UnsortedChildren,
    /// Not a broken rule: a file deleted, or replaced by a RENAME, while
    /// handles are open on it is kept for them, one file they all read and
    /// write, as NTFS keeps a file deleted with `FILE_SHARE_DELETE` (wire.md
    /// 3.4.8). A run must pass it.
    KeepDeleted,
    /// A file deleted, or replaced by a RENAME, while handles are open on it
    /// is kept as a copy per handle, so a WRITE through one is not seen
    /// through another (wire.md 3.4.8: a kept file is one file to all its
    /// handles; 3.4.4).
    GoneHandlesPrivate,
    /// An OPEN_CREATE of a missing name checks the name and its parent,
    /// waits, then makes the file whatever happened meanwhile: a directory
    /// made at the name is removed for it, a parent renamed away is made
    /// again (wire.md 2.8.1: a create and a MKDIR of one name leave one
    /// entry of one kind; a create under a renamed directory lands under one
    /// of its names or is refused).
    RacyCreate,
}

impl Break {
    /// The rule a `--break` value names.
    pub fn parse(name: &str) -> Option<Break> {
        match name {
            "found-for-missing" => Some(Break::FoundForMissing),
            "io-error-for-bad-request" => Some(Break::IoErrorForBadRequest),
            "stale-immutable" => Some(Break::StaleImmutable),
            "frozen-reg-gen" => Some(Break::FrozenRegGen),
            "swap-replies" => Some(Break::SwapReplies),
            "per-slot-fh" => Some(Break::PerSlotFh),
            "slot-zero-only" => Some(Break::SlotZeroOnly),
            "bank-zero" => Some(Break::BankZero),
            "arena-elsewhere" => Some(Break::ArenaElsewhere),
            "arena-offset-wrong" => Some(Break::ArenaOffsetWrong),
            "downward-fh" => Some(Break::DownwardFh),
            "short-bulk" => Some(Break::ShortBulk),
            "truncate-readdir" => Some(Break::TruncateReaddir),
            "late-bank" => Some(Break::LateBank),
            "complete-abandoned" => Some(Break::CompleteAbandoned),
            "bump-before-write" => Some(Break::BumpBeforeWrite),
            "no-registry-env" => Some(Break::NoRegistryEnv),
            "undeclared-roots" => Some(Break::UndeclaredRoots),
            "bad-shim-cfg" => Some(Break::BadShimCfg),
            "inherit-env" => Some(Break::InheritEnv),
            "fh-from-zero" => Some(Break::FhFromZero),
            "mtime-millis" => Some(Break::MtimeMillis),
            "open-flag-bits" => Some(Break::OpenFlagBits),
            "stage-copy" => Some(Break::StageCopy),
            "stage-wrong-bytes" => Some(Break::StageWrongBytes),
            "write-only-unreadable" => Some(Break::WriteOnlyUnreadable),
            "growth-garbage" => Some(Break::GrowthGarbage),
            "stream-base" => Some(Break::StreamBase),
            "machine-only-registry" => Some(Break::MachineOnlyRegistry),
            "delete-unrecorded-not-found" => Some(Break::DeleteUnrecordedNotFound),
            "snapshot-reads" => Some(Break::SnapshotReads),
            "setattr-at-close" => Some(Break::SetattrAtClose),
            "error-payload" => Some(Break::ErrorPayload),
            "append-own-end" => Some(Break::AppendOwnEnd),
            "truncate-keeps-bytes" => Some(Break::TruncateKeepsBytes),
            "refused-write-bumps" => Some(Break::RefusedWriteBumps),
            "rename-dir-ghosts" => Some(Break::RenameDirGhosts),
            "handles-by-path" => Some(Break::HandlesByPath),
            "create-makes-parents" => Some(Break::CreateMakesParents),
            "file-parent-io-error" => Some(Break::FileParentIoError),
            "dir-write-open" => Some(Break::DirWriteOpen),
            "readdir-file-lists-itself" => Some(Break::ReaddirFileListsItself),
            "file-onto-dir-kept" => Some(Break::FileOntoDirKept),
            "ancestor-bad-request" => Some(Break::AncestorBadRequest),
            "same-name-exists" => Some(Break::SameNameExists),
            "read-offset-wraps" => Some(Break::ReadOffsetWraps),
            "huge-write-ok" => Some(Break::HugeWriteOk),
            "short-write" => Some(Break::ShortWrite),
            "rename-x-onto-dir-ok" => Some(Break::RenameXOntoDirOk),
            "skip-past-end-echo" => Some(Break::SkipPastEndEcho),
            "parent-respelled" => Some(Break::ParentRespelled),
            "racy-excl" => Some(Break::RacyExcl),
            "racy-reg-create" => Some(Break::RacyRegCreate),
            "torn-append" => Some(Break::TornAppend),
            "lost-update" => Some(Break::LostUpdate),
            "rename-in-steps" => Some(Break::RenameInSteps),
            "write-at-close" => Some(Break::WriteAtClose),
            "rename-keeps-old-name" => Some(Break::RenameKeepsOldName),
            "case-rename-stale-children" => Some(Break::CaseRenameStaleChildren),
            "rename-x-from-mount" => Some(Break::RenameXFromMount),
            "delete-keeps-subtree" => Some(Break::DeleteKeepsSubtree),
            "rename-unchanged" => Some(Break::RenameUnchanged),
            "set-under-tombstone-not-found" => Some(Break::SetUnderTombstoneNotFound),
            "value-name-bytes" => Some(Break::ValueNameBytes),
            "set-value-appends" => Some(Break::SetValueAppends),
            "fh-zero-ok" => Some(Break::FhZeroOk),
            "spelling-outlives-delete" => Some(Break::SpellingOutlivesDelete),
            "delete-ignores-root" => Some(Break::DeleteIgnoresRoot),
            "root-not-found" => Some(Break::RootNotFound),
            "delete-dir-ignores-empty-dirs" => Some(Break::DeleteDirIgnoresEmptyDirs),
            "stale-kind" => Some(Break::StaleKind),
            "stage-mount-bytes" => Some(Break::StageMountBytes),
            "bulk-cache" => Some(Break::BulkCache),
            "delete-pending-until-close" => Some(Break::DeletePendingUntilClose),
            "reply-trailing" => Some(Break::ReplyTrailing),
            "unsorted-children" => Some(Break::UnsortedChildren),
            "keep-deleted" => Some(Break::KeepDeleted),
            "gone-handles-private" => Some(Break::GoneHandlesPrivate),
            "racy-create" => Some(Break::RacyCreate),
            _ => None,
        }
    }
}

/// Runs the reference adapter until EOF on stdin. The exit code. `arena`
/// false serves rings without a bulk arena (`VFS_ARENA_LEN` unset, every
/// READ inline: wire.md 2.6, 3.4.4). `broken` is the test-only rule to break
/// ([`Break`]); `None` serves the reference as it is.
pub fn reference_adapter(kind: RingKind, arena: bool, broken: Option<Break>) -> i32 {
    if kind == RingKind::Section && !cfg!(windows) {
        eprintln!("reference-adapter: --ring section needs Windows");
        return 2;
    }
    let mut mounted: Option<Mounted> = None;
    answer_requests(|req| match req {
        AdapterRequest::Mount { scenario } => {
            if mounted.is_some() {
                return json!({"ok": false, "error": "a tree is already mounted"});
            }
            match mount(Path::new(&scenario), kind, arena, broken) {
                Ok(m) => {
                    let env = m.env.clone();
                    mounted = Some(m);
                    json!({"ok": true, "env": env})
                }
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        AdapterRequest::Unmount => match mounted.take().map(Mounted::unmount) {
            Some(Err(e)) => json!({"ok": false, "error": e}),
            _ => json!({"ok": true}),
        },
    });
    if let Some(m) = mounted.take() {
        let _ = m.unmount();
    }
    0
}

/// A mounted tree: the serve threads, how to stop them, and the ring.
struct Mounted {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Result<(), IpcError>>>,
    backing: Arc<dyn RingBacking>,
    file: Option<PathBuf>,
    /// The shim.cfg written for the mount.
    cfg: PathBuf,
    /// The copies STAGE_EXEC made ([`Break::StageCopy`]).
    staged: Arc<Mutex<Vec<PathBuf>>>,
    env: Env,
}

impl Mounted {
    fn unmount(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        let served: Vec<_> = self.threads.into_iter().map(|t| t.join()).collect();
        drop(self.backing);
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_file(&self.cfg);
        for f in self
            .staged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = std::fs::remove_file(&f);
            if let Some(dir) = f.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        for s in served {
            match s {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(format!("a serve loop ended with {e:?}")),
                Err(_) => return Err("a serve loop panicked".into()),
            }
        }
        Ok(())
    }
}

/// The ring the null adapter serves: how many slots, the payload cap, the
/// arena's length (0 for none) and the threads serving it. The default is
/// the reference adapter's ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullGeometry {
    pub slots: u32,
    pub payload_cap: u32,
    pub arena_len: usize,
    pub workers: u32,
}

impl Default for NullGeometry {
    fn default() -> NullGeometry {
        NullGeometry {
            slots: SLOTS,
            payload_cap: PAYLOAD_CAP,
            arena_len: SLOTS as usize * BANK,
            workers: WORKERS,
        }
    }
}

/// Runs the null adapter until EOF on stdin: the adapter protocol with
/// [`Floor`] behind it, which answers every request OK with the smallest
/// reply and a READ with the bytes it asks for, on a ring of `geometry` (a
/// file, or on Windows a named section). The tree a mount names is never
/// read. What a request costs here is the ring's own cost, data movement
/// included, the floor `aether-conform bench` subtracts. The exit code.
pub fn null_adapter(kind: RingKind, geometry: NullGeometry) -> i32 {
    if kind == RingKind::Section && !cfg!(windows) {
        eprintln!("null-adapter: --ring section needs Windows");
        return 2;
    }
    let mut mounted: Option<Mounted> = None;
    answer_requests(|req| match req {
        AdapterRequest::Mount { .. } => {
            if mounted.is_some() {
                return json!({"ok": false, "error": "a tree is already mounted"});
            }
            match mount_null(kind, geometry) {
                Ok(m) => {
                    let env = m.env.clone();
                    mounted = Some(m);
                    json!({"ok": true, "env": env})
                }
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        AdapterRequest::Unmount => match mounted.take().map(Mounted::unmount) {
            Some(Err(e)) => json!({"ok": false, "error": e}),
            _ => json!({"ok": true}),
        },
    });
    if let Some(m) = mounted.take() {
        let _ = m.unmount();
    }
    0
}

/// A ring of `g` served by [`Floor`] on `g.workers` threads, and the
/// handshake variables naming it.
fn mount_null(kind: RingKind, g: NullGeometry) -> Result<Mounted, String> {
    if g.slots == 0 || g.workers == 0 {
        return Err("a ring needs at least one slot and one serving thread".into());
    }
    let stride = (32 + g.payload_cap as usize).next_multiple_of(8);
    let arena_offset = aether_ipc::layout::RING_HEADER_SIZE + g.slots as usize * stride;
    let ring_bytes = (arena_offset + g.arena_len)
        .next_multiple_of(65536)
        .max(2 * 1024 * 1024);
    let init = |b: &dyn RingBacking| -> Result<(), String> {
        ring::init(b.seg(), g.slots, g.payload_cap).map_err(|e| format!("ring init: {e:?}"))?;
        ring::set_worker_hint(b.seg(), g.workers);
        Ok(())
    };
    let mut env = Env::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (backing, file, threads): Started = match kind {
        RingKind::File => {
            let path = std::env::temp_dir().join(format!("{}.ring", unique("null-ring")));
            let f = aether_filering::FileRing::create(&path, ring_bytes)
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            let b: Arc<dyn RingBacking> = Arc::new(f);
            init(&*b)?;
            env.insert(
                aether_env::RING_PATH.into(),
                path.to_string_lossy().into_owned(),
            );
            let t = serve_null(
                b.clone(),
                Arc::new(aether_ipc::AdaptiveNotifier),
                g,
                arena_offset,
                stop.clone(),
            );
            (b, Some(path), t)
        }
        RingKind::Section => null_section(&mut env, g, ring_bytes, arena_offset, &init, &stop)?,
    };
    env.insert(aether_env::RING_BYTES.into(), ring_bytes.to_string());
    env.insert(
        aether_env::RING_PAYLOAD_CAP.into(),
        g.payload_cap.to_string(),
    );
    if g.arena_len > 0 {
        env.insert(aether_env::ARENA_OFFSET.into(), arena_offset.to_string());
        env.insert(aether_env::ARENA_LEN.into(), g.arena_len.to_string());
    }
    env.insert(
        aether_env::VIRTUAL_DIR.into(),
        r"C:\aether-conform\root0".into(),
    );
    Ok(Mounted {
        stop,
        threads,
        backing,
        file,
        cfg: PathBuf::new(),
        staged: Arc::default(),
        env,
    })
}

#[cfg(windows)]
fn null_section(
    env: &mut Env,
    g: NullGeometry,
    ring_bytes: usize,
    arena_offset: usize,
    init: &dyn Fn(&dyn RingBacking) -> Result<(), String>,
    stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    let name = format!(r"Local\{}", unique("null-section"));
    let (srv, cli) = (format!("{name}_srv"), format!("{name}_cli"));
    let m = aether_win::SharedMapping::create(&name, ring_bytes)
        .map_err(|e| format!("create section {name}: {e}"))?;
    let events = aether_win::EventNotifier::create(&srv, &cli)
        .map_err(|e| format!("create events {srv}, {cli}: {e}"))?;
    let b: Arc<dyn RingBacking> = Arc::new(m);
    init(&*b)?;
    env.insert(aether_env::RING_SECTION.into(), name);
    env.insert(aether_env::SERVER_EV.into(), srv);
    env.insert(aether_env::CLIENT_EV.into(), cli);
    let t = serve_null(b.clone(), Arc::new(events), g, arena_offset, stop.clone());
    Ok((b, None, t))
}

#[cfg(not(windows))]
fn null_section(
    _env: &mut Env,
    _g: NullGeometry,
    _ring_bytes: usize,
    _arena_offset: usize,
    _init: &dyn Fn(&dyn RingBacking) -> Result<(), String>,
    _stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    Err("a named section needs Windows".into())
}

/// The null adapter's director: [`aether_refdirector::Null`] for every
/// opcode but READ, which moves as many bytes as it is asked for, so that a
/// floor's READ costs the ring's data movement as a director's does. It
/// answers in bulk when the request sets `FLAG_READ_BULK` and the ring has
/// an arena, with `min(len, bank_size)` bytes of its slot's bank, and inline
/// otherwise, with `min(len, payload_cap - 8)` bytes; the bytes are zeros,
/// written each time. `aether-conform bench` sends the floor each READ with
/// the length and the form the director's reply had.
#[derive(Default)]
struct Floor {
    null: aether_refdirector::Null,
    zeros: Vec<u8>,
}

impl Director for Floor {
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        self.null.get_attr(root, path)
    }
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        self.null.read_dir(root, path, out)
    }
    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        self.null.open(root, flags, path)
    }
    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        _: u64,
        _: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        if let (true, Some(arena)) = (slot_flags.contains(RequestFlags::READ_BULK), at.arena) {
            return match arena.fill_bank(at.slot, len as usize, |buf| {
                buf.fill(0);
                Ok(buf.len())
            }) {
                Ok((arena_offset, n)) => Ok(ReadReply::Bulk {
                    len: n as u32,
                    arena_offset,
                }),
                Err(st) => Err(Status::try_from(st).unwrap_or(Status::IoError)),
            };
        }
        let n = (len as usize).min(at.inline_cap);
        self.zeros.clear();
        self.zeros.resize(n, 0);
        Ok(ReadReply::Inline(&self.zeros))
    }
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        self.null.write(fh, offset, data)
    }
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status> {
        self.null.set_attr(fh, size)
    }
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        self.null.rename(root, from, to)
    }
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        self.null.delete(root, path)
    }
    fn mkdir(&mut self, root: RootId, mode: u32, path: &str) -> Result<(), Status> {
        self.null.mkdir(root, mode, path)
    }
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        self.null.close(fh)
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        self.null.heartbeat()
    }
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        self.null.stored_names(root, skip, path)
    }
    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status> {
        self.null.reg_lookup(path)
    }
    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status> {
        self.null.reg_key(path)
    }
    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status> {
        self.null.reg_set_value(path, name, ty, data)
    }
    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status> {
        self.null.reg_delete_value(path, name)
    }
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status> {
        self.null.reg_create_key(path, volatile)
    }
    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status> {
        self.null.reg_delete_key(path)
    }
    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status> {
        self.null.reg_rename_key(path, new_leaf)
    }
    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status> {
        self.null.reg_changed(path, subtree, since)
    }
    fn rename_x(
        &mut self,
        from_root: RootId,
        to_root: RootId,
        from: &str,
        to: &str,
    ) -> Result<(), Status> {
        self.null.rename_x(from_root, to_root, from, to)
    }
    fn stage_exec(&mut self, root: RootId, path: &str) -> Result<&str, Status> {
        self.null.stage_exec(root, path)
    }
}

/// Serves [`Floor`] on `backing` until `stop`, on `g.workers` threads sharing
/// one notifier, as a Rust director serves (`aether_rpc::serve_with_arena`).
fn serve_null<N: Notifier + Send + Sync + 'static>(
    backing: Arc<dyn RingBacking>,
    notifier: Arc<N>,
    g: NullGeometry,
    arena_offset: usize,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<Result<(), IpcError>>> {
    (0..g.workers)
        .map(|_| {
            let backing = backing.clone();
            let notifier = SharedNotifier(notifier.clone());
            let stop = stop.clone();
            std::thread::spawn(move || {
                let seg = backing.seg();
                let server = RingServer::new(seg, notifier)?;
                let arena = (g.arena_len > 0)
                    .then(|| DataArena::new(seg, arena_offset, g.arena_len, g.slots as usize));
                aether_rpc::serve_with_arena(&server, arena.as_ref(), &mut Floor::default(), &stop)
            })
        })
        .collect()
}

/// A name no other mount of this or another process has.
fn unique(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("aether-conform-{tag}-{}-{n}-{t}", std::process::id())
}

fn mount(
    scenario: &Path,
    kind: RingKind,
    arena: bool,
    broken: Option<Break>,
) -> Result<Mounted, String> {
    let s = crate::scenario::load(scenario).map_err(|e| e.to_string())?;
    let staged = Arc::new(Mutex::new(Vec::new()));
    let d = Served {
        reference: Reference::from_tree(&s.tree)?,
        broken,
        ring: None,
        banks: Arc::default(),
        staged: String::new(),
        staged_files: staged.clone(),
        write_only: Arc::default(),
        reg_reads: Arc::default(),
        held: Arc::default(),
        view: Vec::new(),
        echo: String::new(),
        arrivals: Arc::default(),
        tree: Arc::new(Mounts::of(&s.tree)),
    };

    let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
    let after_slots = aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride;
    let misplaced = matches!(
        broken,
        Some(Break::ArenaElsewhere | Break::ArenaOffsetWrong)
    );
    let arena_offset = after_slots + if misplaced { BANK } else { 0 };
    let arena_len = if arena { SLOTS as usize * BANK } else { 0 };
    let ring_bytes = (arena_offset + arena_len)
        .next_multiple_of(65536)
        .max(2 * 1024 * 1024);

    let mut env = Env::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (backing, file, threads): (Arc<dyn RingBacking>, _, _) = match kind {
        RingKind::File => {
            let path = std::env::temp_dir().join(format!("{}.ring", unique("ring")));
            let f = aether_filering::FileRing::create(&path, ring_bytes)
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            let b: Arc<dyn RingBacking> = Arc::new(f);
            init(&*b)?;
            env.insert(
                aether_env::RING_PATH.into(),
                path.to_string_lossy().into_owned(),
            );
            let t = serve(
                b.clone(),
                Arc::new(aether_ipc::AdaptiveNotifier),
                d,
                arena_offset,
                arena_len,
                stop.clone(),
            );
            (b, Some(path), t)
        }
        RingKind::Section => section(&mut env, d, ring_bytes, arena_offset, arena_len, &stop)?,
    };
    env.insert(aether_env::RING_BYTES.into(), ring_bytes.to_string());
    env.insert(aether_env::RING_PAYLOAD_CAP.into(), PAYLOAD_CAP.to_string());
    if arena_len > 0 {
        match broken {
            Some(Break::ArenaElsewhere) => {}
            Some(Break::ArenaOffsetWrong) => {
                env.insert(aether_env::ARENA_OFFSET.into(), after_slots.to_string());
            }
            _ => {
                env.insert(aether_env::ARENA_OFFSET.into(), arena_offset.to_string());
            }
        }
        env.insert(aether_env::ARENA_LEN.into(), arena_len.to_string());
    }
    // Where the program would see each root. Nothing on the ring names
    // them; they are here because a shim needs them (wire.md 5.3).
    let root = |id: u32| format!(r"C:\aether-conform\root{id}");
    env.insert(aether_env::VIRTUAL_DIR.into(), root(0));
    let extra: Vec<String> = s
        .tree
        .roots
        .iter()
        .filter(|r| r.id != 0)
        .map(|r| format!("{}={}", r.id, root(r.id)))
        .collect();
    if !extra.is_empty() && broken != Some(Break::UndeclaredRoots) {
        env.insert(aether_env::VIRTUAL_ROOTS.into(), extra.join(";"));
    }
    if s.tree.registry.is_some() && broken != Some(Break::NoRegistryEnv) {
        env.insert(aether_env::REGISTRY.into(), "1".into());
    }
    // The shim.cfg a launch would hand the injector (wire.md 5.1): root 0,
    // no static imports.
    let cfg = std::env::temp_dir().join(format!("{}.cfg", unique("shimcfg")));
    let mut bytes = aether_protocol::shimcfg::encode_config(&root(0));
    if broken == Some(Break::BadShimCfg) {
        bytes.push(0);
    }
    std::fs::write(&cfg, &bytes).map_err(|e| format!("write {}: {e}", cfg.display()))?;
    env.insert(
        aether_env::SHIM_CONFIG.into(),
        cfg.to_string_lossy().into_owned(),
    );
    if broken == Some(Break::InheritEnv) {
        // What a launch built on the director's own environment passes on.
        for &name in aether_env::handshake::TRANSPORT {
            if let (false, Ok(v)) = (env.contains_key(name), std::env::var(name)) {
                env.insert(name.into(), v);
            }
        }
    }
    Ok(Mounted {
        stop,
        threads,
        backing,
        file,
        cfg,
        staged,
        env,
    })
}

/// A started ring: its backing, its file (if it is one) and the serve
/// threads.
type Started = (
    Arc<dyn RingBacking>,
    Option<PathBuf>,
    Vec<JoinHandle<Result<(), IpcError>>>,
);

/// Initialises the ring and states the threads serving it (wire.md 2.8).
fn init(b: &dyn RingBacking) -> Result<(), String> {
    ring::init(b.seg(), SLOTS, PAYLOAD_CAP).map_err(|e| format!("ring init: {e:?}"))?;
    ring::set_worker_hint(b.seg(), WORKERS);
    Ok(())
}

#[cfg(windows)]
fn section(
    env: &mut Env,
    d: Served,
    ring_bytes: usize,
    arena_offset: usize,
    arena_len: usize,
    stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    let name = format!(r"Local\{}", unique("section"));
    let (srv, cli) = (format!("{name}_srv"), format!("{name}_cli"));
    let m = aether_win::SharedMapping::create(&name, ring_bytes)
        .map_err(|e| format!("create section {name}: {e}"))?;
    let events = aether_win::EventNotifier::create(&srv, &cli)
        .map_err(|e| format!("create events {srv}, {cli}: {e}"))?;
    let b: Arc<dyn RingBacking> = Arc::new(m);
    init(&*b)?;
    env.insert(aether_env::RING_SECTION.into(), name);
    env.insert(aether_env::SERVER_EV.into(), srv);
    env.insert(aether_env::CLIENT_EV.into(), cli);
    let t = serve(
        b.clone(),
        Arc::new(events),
        d,
        arena_offset,
        arena_len,
        stop.clone(),
    );
    Ok((b, None, t))
}

#[cfg(not(windows))]
fn section(
    _env: &mut Env,
    _d: Served,
    _ring_bytes: usize,
    _arena_offset: usize,
    _arena_len: usize,
    _stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    Err("a named section needs Windows".into())
}

/// Serves `d` on `backing` until `stop`, on [`WORKERS`] threads, each with
/// its own handle on the reference and its own `RingServer`, sharing one
/// notifier: as a Rust director serves (`aether_rpc::serve_with_arena`), or,
/// for the breaks that need to see slots ([`Break::SwapReplies`],
/// [`Break::PerSlotFh`], [`Break::SlotZeroOnly`]), with [`interposed`].
fn serve<N: Notifier + Send + Sync + 'static>(
    backing: Arc<dyn RingBacking>,
    notifier: Arc<N>,
    d: Served,
    arena_offset: usize,
    arena_len: usize,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<Result<(), IpcError>>> {
    let published = backing.clone();
    let frozen = d.broken == Some(Break::FrozenRegGen);
    let once = AtomicBool::new(false);
    d.reference.publish_generation(move |g| {
        if frozen && once.swap(true, Ordering::AcqRel) {
            return;
        }
        ring::publish_reg_generation(published.seg(), g)
    });
    let handles = Arc::new(Mutex::new(SlotHandles::default()));
    let banks = Arc::new(Mutex::new(Vec::new()));
    (0..WORKERS)
        .map(|_| {
            let backing = backing.clone();
            let notifier = SharedNotifier(notifier.clone());
            let stop = stop.clone();
            let handles = handles.clone();
            let mut d = Served {
                reference: d.reference.share(),
                broken: d.broken,
                ring: Some(backing.clone()),
                banks: banks.clone(),
                staged: String::new(),
                staged_files: d.staged_files.clone(),
                write_only: d.write_only.clone(),
                reg_reads: d.reg_reads.clone(),
                held: d.held.clone(),
                view: Vec::new(),
                echo: String::new(),
                arrivals: d.arrivals.clone(),
                tree: d.tree.clone(),
            };
            std::thread::spawn(move || {
                let seg = backing.seg();
                let served = match d.broken {
                    Some(
                        b @ (Break::SwapReplies
                        | Break::PerSlotFh
                        | Break::SlotZeroOnly
                        | Break::CompleteAbandoned
                        | Break::OpenFlagBits
                        | Break::ErrorPayload
                        | Break::ReplyTrailing
                        | Break::UnsortedChildren),
                    ) => interposed(seg, &notifier, &mut d, b, &handles, &stop),
                    _ => RingServer::new(seg, notifier).and_then(|server| {
                        let arena = (arena_len > 0)
                            .then(|| DataArena::new(seg, arena_offset, arena_len, SLOTS as usize));
                        aether_rpc::serve_with_arena(&server, arena.as_ref(), &mut d, &stop)
                    }),
                };
                // The reference holds the generation callback, and with it
                // the ring.
                drop(d);
                served
            })
        })
        .collect()
}

/// One notifier shared by every serve thread of a ring.
struct SharedNotifier<N>(Arc<N>);

impl<N: Notifier> Notifier for SharedNotifier<N> {
    fn notify_server(&self) {
        self.0.notify_server()
    }
    fn wait_server(&self) {
        self.0.wait_server()
    }
    fn notify_client(&self, slot: u32) {
        self.0.notify_client(slot)
    }
    fn wait_client(&self, slot: u32) {
        self.0.wait_client(slot)
    }
    fn idle_client(&self, slot: u32, waited: std::time::Duration) {
        self.0.idle_client(slot, waited)
    }
    fn notify_slot_free(&self) {
        self.0.notify_slot_free()
    }
    fn now(&self) -> std::time::Instant {
        self.0.now()
    }
}

/// [`Break::PerSlotFh`]'s handle tables: the reference's handle for each
/// (slot, number that slot's client was given), and each slot's last number.
#[derive(Default)]
struct SlotHandles {
    open: BTreeMap<(u32, u64), u64>,
    next: BTreeMap<u32, u64>,
}

/// A serve loop for the breaks that need the slot a request came in on.
/// Each request is taken off the ring by hand, answered by the reference
/// through a ring of one slot held in memory ([`Inner`], so the answers are
/// the serve loop's, and every READ is answered inline, at most
/// `payload_cap - 8` bytes, as wire.md 3.4.4 has a director with no arena
/// answer), and completed with `ring::server_complete` as a director
/// completes it (2.5), except for what `how` breaks.
fn interposed(
    seg: &aether_ipc::SharedSeg,
    notifier: &impl Notifier,
    d: &mut Served,
    how: Break,
    handles: &Mutex<SlotHandles>,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    let geom = ring::open(seg)?;
    // A slot count of one: only slot 0 is looked at.
    let take_geom = match how {
        Break::SlotZeroOnly => ring::Geom {
            slot_count: 1,
            ..geom
        },
        _ => geom,
    };
    let mut inner = Inner::new()?;
    let mut next = 0;
    let mut payload = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let Some(a) = ring::server_take_from(seg, &take_geom, next) else {
            notifier.wait_server();
            continue;
        };
        next = a.wrapping_add(1);
        let mut taken = vec![a];
        if how == Break::SwapReplies {
            // Another request in flight with this one, if one comes soon. A
            // fixed busy-wait, not an event: nothing can signal "another
            // request is about to be published", and 300 µs per request is
            // nothing to a test break.
            let since = std::time::Instant::now();
            while since.elapsed() < std::time::Duration::from_micros(300) {
                if let Some(b) = ring::server_take_from(seg, &geom, next) {
                    next = b.wrapping_add(1);
                    taken.push(b);
                    break;
                }
                core::hint::spin_loop();
            }
        }
        let mut answers = Vec::new();
        for &slot in &taken {
            let head =
                ring::read_request_into(seg, &geom, slot, &mut payload).ok_or(IpcError::Layout)?;
            if how == Break::CompleteAbandoned
                && head.opcode == Opcode::Read as u32
                && payload
                    .get(16..20)
                    .is_some_and(|l| u32::from_le_bytes(l.try_into().unwrap()) >= 256 * 1024)
            {
                // Slow, so a client gives it up while it is processed: held
                // until it is given up, or for ABANDON_HOLD when it is not.
                let since = std::time::Instant::now();
                while ring::slot_state(seg, &geom, slot) != Some(aether_ipc::layout::ST_ABANDONED)
                    && since.elapsed() < ABANDON_HOLD
                {
                    std::thread::yield_now();
                }
            }
            let (status, reply) = if head.fits(&geom) {
                let mut table = handles.lock().unwrap_or_else(|e| e.into_inner());
                let req = Raw {
                    slot,
                    opcode: head.opcode,
                    flags: head.flags,
                    payload: &payload,
                };
                answer_on(&mut inner, d, how, &mut table, &req)
            } else {
                (Status::BadRequest.code(), Vec::new())
            };
            let mut reply = reply;
            if how == Break::ErrorPayload && status != Status::Ok.code() {
                reply = vec![0xEE; 4];
            }
            if how == Break::OpenFlagBits
                && head.opcode == Opcode::Open as u32
                && status == Status::Ok.code()
                && reply.len() >= 24
            {
                reply[17] |= 0x02;
                reply[18] = 1;
            }
            if how == Break::ReplyTrailing
                && head.opcode == Opcode::GetAttr as u32
                && status == Status::Ok.code()
            {
                reply.extend_from_slice(&[0, 0]);
            }
            if how == Break::UnsortedChildren
                && head.opcode == Opcode::RegKey as u32
                && status == Status::Ok.code()
            {
                reply = reversed_children(reply);
            }
            answers.push((slot, head.req_id, status, reply));
        }
        if let [a, b] = answers.as_mut_slice() {
            // Each reply into the other's slot, under its own ack.
            std::mem::swap(&mut a.2, &mut b.2);
            std::mem::swap(&mut a.3, &mut b.3);
        }
        for (slot, req_id, status, reply) in answers {
            let given_up =
                ring::slot_state(seg, &geom, slot) == Some(aether_ipc::layout::ST_ABANDONED);
            if how == Break::CompleteAbandoned && given_up {
                // COMPLETED stored over the slot the client gave up, straight
                // from ABANDONED: never FREE in between, which a client
                // watching the slot could see and pass.
                seg.write_bytes(
                    geom.slot_off(slot) + aether_ipc::layout::SH_STATE,
                    &aether_ipc::layout::ST_COMPLETED.to_le_bytes(),
                );
            } else {
                ring::server_complete(seg, &geom, slot, req_id, status, &reply)?;
            }
            notifier.notify_client(slot);
        }
    }
    Ok(())
}

/// [`Break::UnsortedChildren`]: a REG_KEY reply with its children in
/// reverse order (each `folded:str | spelling:str | state:u8`, wire.md 4.3).
fn reversed_children(reply: Vec<u8>) -> Vec<u8> {
    let Some((at, n)) = crate::shape::key_children(&reply) else {
        return reply;
    };
    let mut kids = Vec::new();
    let mut i = at;
    for _ in 0..n {
        let start = i;
        for _ in 0..2 {
            let Some(len) = reply.get(i..i + 4) else {
                return reply;
            };
            i += 4 + u32::from_le_bytes(len.try_into().unwrap()) as usize;
        }
        i += 1;
        if i > reply.len() {
            return reply;
        }
        kids.push(reply[start..i].to_vec());
    }
    let mut out = reply[..at].to_vec();
    for k in kids.iter().rev() {
        out.extend_from_slice(k);
    }
    out.extend_from_slice(&reply[i..]);
    out
}

/// A request as [`interposed`] took it off the ring.
struct Raw<'p> {
    slot: u32,
    opcode: u32,
    flags: u32,
    payload: &'p [u8],
}

/// The status and reply payload for `req`, from the reference, with
/// [`Break::PerSlotFh`]'s numbering applied.
fn answer_on(
    inner: &mut Inner,
    d: &mut Served,
    how: Break,
    table: &mut SlotHandles,
    req: &Raw<'_>,
) -> (i32, Vec<u8>) {
    let (slot, opcode, flags) = (req.slot, req.opcode, req.flags);
    let passed = |inner: &mut Inner, d: &mut Served| inner.ask(d, opcode, flags, req.payload);
    if how != Break::PerSlotFh {
        return passed(inner, d);
    }
    let Ok(op) = Opcode::try_from(opcode) else {
        return passed(inner, d);
    };
    let Ok(decoded) = Request::decode(op, flags, req.payload) else {
        return passed(inner, d);
    };
    let bad_fh = (Status::BadFh.code(), Vec::new());
    let theirs = |fh: u64| table.open.get(&(slot, fh)).copied();
    let mut body = Vec::new();
    let mut closing = None;
    match decoded {
        Request::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::Read {
                slot_flags,
                fh,
                offset,
                len,
            }
            .encode(&mut body);
        }
        Request::Write { fh, offset, data } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::Write { fh, offset, data }.encode(&mut body);
        }
        Request::SetAttr { fh, size } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::SetAttr { fh, size }.encode(&mut body);
        }
        Request::Close { fh: ext } => {
            let Some(fh) = theirs(ext) else { return bad_fh };
            Request::Close { fh }.encode(&mut body);
            closing = Some(ext);
        }
        _ => body.extend_from_slice(req.payload),
    }
    let (status, reply) = inner.ask(d, opcode, flags, &body);
    if status != Status::Ok.code() {
        return (status, reply);
    }
    if let Some(ext) = closing {
        table.open.remove(&(slot, ext));
        return (status, reply);
    }
    match Reply::decode(op, flags, &reply) {
        Ok(Reply::Open(o)) => {
            let n = table.next.entry(slot).or_insert(0);
            *n += 1;
            let ext = *n;
            table.open.insert((slot, ext), o.fh);
            let mut out = Vec::new();
            Reply::Open(OpenResp { fh: ext, ..o }).encode(&mut out);
            (status, out)
        }
        _ => (status, reply),
    }
}

/// The reference behind a ring of one slot held in memory, with no arena:
/// what [`interposed`] asks, so the answers are the serve loop's.
struct Inner {
    seg: aether_ipc::OwnedSeg,
    geom: ring::Geom,
    scratch: aether_rpc::Scratch,
}

impl Inner {
    fn new() -> Result<Inner, IpcError> {
        let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
        let seg = aether_ipc::OwnedSeg::new(aether_ipc::layout::RING_HEADER_SIZE + stride);
        let geom = ring::init(seg.seg(), 1, PAYLOAD_CAP)?;
        Ok(Inner {
            seg,
            geom,
            scratch: aether_rpc::Scratch::default(),
        })
    }

    fn ask(&mut self, d: &mut Served, opcode: u32, flags: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        let seg = self.seg.seg();
        let io = (Status::IoError.code(), Vec::new());
        let Some(slot) = ring::claim_free(seg, &self.geom) else {
            return io;
        };
        let Ok(req_id) = ring::publish_request(seg, &self.geom, slot, opcode, flags, payload)
        else {
            let _ = ring::free_slot(seg, &self.geom, slot);
            return io;
        };
        let Ok(server) = RingServer::new(seg, aether_ipc::SpinNotifier) else {
            return io;
        };
        let _ = aether_rpc::serve_one_with(&server, d, &mut self.scratch);
        let reply = ring::take_response(seg, &self.geom, slot, req_id);
        let _ = ring::free_slot(seg, &self.geom, slot);
        match reply {
            Ok(Some(r)) => r,
            _ => io,
        }
    }
}

/// What the adapter serves: the reference, and the rule it breaks if the
/// test-only `--break` switch named one.
struct Served {
    reference: Reference,
    broken: Option<Break>,
    /// The ring served, for the breaks that write its header themselves.
    ring: Option<Arc<dyn RingBacking>>,
    /// [`Break::LateBank`]'s (slot, length) of bulk replies already given.
    banks: Arc<Mutex<Vec<(u32, usize)>>>,
    /// The last STAGE_EXEC answer, and every copy made, for the unmount.
    staged: String,
    staged_files: Arc<Mutex<Vec<PathBuf>>>,
    /// [`Break::WriteOnlyUnreadable`]'s handles opened without `OPEN_READ`.
    write_only: Arc<Mutex<std::collections::BTreeSet<u64>>>,
    /// [`Break::BumpBeforeWrite`]'s count of REG_KEYs answered, shared by
    /// every serve thread, and signalled at each.
    reg_reads: Arc<(Mutex<u64>, std::sync::Condvar)>,
    /// What the breaks that keep state per handle or per name hold, shared
    /// by every serve thread.
    held: Arc<Mutex<Held>>,
    /// [`Break::SnapshotReads`]'s READ answer, from the handle's snapshot.
    view: Vec<u8>,
    /// [`Break::SkipPastEndEcho`]'s STORED_NAMES answer.
    echo: String,
    /// How many file and registry requests have reached the serve threads,
    /// shared by them: what the breaks of 2.8.1's atomicity wait on.
    arrivals: Arc<std::sync::atomic::AtomicU64>,
    /// The tree's files as mounted, by (root, folded path), and its roots:
    /// for the breaks that copy mounted bytes or act on every root.
    tree: Arc<Mounts>,
}

/// What [`Served::tree`] holds.
#[derive(Default)]
struct Mounts {
    files: BTreeMap<(u32, String), Vec<u8>>,
    roots: Vec<RootId>,
}

impl Mounts {
    fn of(t: &aether_refdirector::tree::Tree) -> Mounts {
        let mut m = Mounts::default();
        for r in &t.roots {
            m.roots.push(RootId(r.id));
            for f in &r.files {
                m.files
                    .insert((r.id, aether_casefold::fold(&f.path)), f.contents.bytes());
            }
        }
        m
    }
}

/// The state of [`Break::SnapshotReads`], [`Break::SetattrAtClose`],
/// [`Break::AppendOwnEnd`], [`Break::TruncateKeepsBytes`] and
/// [`Break::RenameDirGhosts`], by the reference's handle number.
#[derive(Default)]
struct Held {
    /// Each file handle's snapshot.
    snaps: BTreeMap<u64, Vec<u8>>,
    /// Each open handle's (root, folded path).
    paths: BTreeMap<u64, (u32, String)>,
    /// The size published for a (root, folded path) while a handle that
    /// SETATTR'd it is open.
    sizes: BTreeMap<(u32, String), u64>,
    /// An append handle's own end.
    ends: BTreeMap<u64, u64>,
    /// The bytes a SETATTR cut, and where they began.
    cut: BTreeMap<u64, (u64, Vec<u8>)>,
    /// A renamed directory's old (root, folded path) and what was under it:
    /// each path below it as stored, and a file's bytes (`None` for a
    /// directory), parents first.
    ghosts: Vec<(u32, String, Subtree)>,
    /// [`Break::HandlesByPath`]: each file handle's root, path and access,
    /// and the handle at that path it last acted through.
    by_path: BTreeMap<u64, (RootId, String, OpenFlags, Option<u64>)>,
    /// [`Break::TornAppend`]'s and [`Break::LostUpdate`]'s handles opened
    /// `OPEN_APPEND`.
    appends: std::collections::BTreeSet<u64>,
    /// [`Break::WriteAtClose`]'s WRITEs kept by each handle.
    pending: BTreeMap<u64, Vec<(u64, Vec<u8>)>>,
    /// [`Break::RenameKeepsOldName`]'s old names and what they were.
    old_names: BTreeMap<(u32, String), AttrResp>,
    /// [`Break::CaseRenameStaleChildren`]'s respelled directories: (root,
    /// folded path) and the stored path before the respell.
    respelled: Vec<(u32, String, String)>,
    /// [`Break::DeleteKeepsSubtree`]'s deleted keys' values, by folded path.
    reg_hidden: BTreeMap<String, Vec<(String, u32, Vec<u8>)>>,
    /// [`Break::RenameUnchanged`]'s keys renamed away, folded.
    renamed_away: std::collections::BTreeSet<String>,
    /// [`Break::SpellingOutlivesDelete`]'s deleted names' stored paths.
    spellings: BTreeMap<(u32, String), String>,
    /// [`Break::StaleKind`]'s kinds, by (root, folded path).
    kinds: BTreeMap<(u32, String), bool>,
    /// [`Break::BulkCache`]'s bulk replies, by (root, folded path, offset,
    /// length).
    bulk: BTreeMap<(u32, String, u64, u32), Vec<u8>>,
    /// [`Break::KeepDeleted`]'s and [`Break::GoneHandlesPrivate`]'s kept
    /// files, and which one each handle on a gone file reads.
    kept: Vec<Vec<u8>>,
    kept_by: BTreeMap<u64, usize>,
}

/// What was below a directory: each path below it as stored, and a file's
/// bytes (`None` for a directory), parents first.
type Subtree = Vec<(String, Option<Vec<u8>>)>;

/// How long the breaks of 2.8.1's atomicity hold the middle of a request
/// once another request has arrived, so that it is served in that middle.
const RACE_HOLD: std::time::Duration = std::time::Duration::from_millis(30);
/// How long they wait for another request to arrive at all: a meeting's
/// requests are in flight together, so one comes at once unless every
/// other was served already.
const RACE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// A registry key the breaks write to and delete, which no scenario names.
const HIDDEN_KEY: &str = r"\Registry\Machine\Software\AetherConformBreak";

impl Served {
    /// The number the client is given for the reference's handle `fh`.
    fn fh_out(&self, fh: u64) -> u64 {
        match self.broken {
            Some(Break::DownwardFh) => u64::MAX - (fh - 1),
            Some(Break::FhFromZero) => fh - 1,
            _ => fh,
        }
    }

    /// [`Break::StreamBase`]: `path` without a stream suffix.
    fn base<'p>(&self, path: &'p str) -> &'p str {
        match self.broken {
            Some(Break::StreamBase) => path.split(':').next().unwrap_or(path),
            _ => path,
        }
    }

    /// [`Break::MachineOnlyRegistry`]: refuses the user's hives.
    fn hive(&self, path: &str) -> Result<(), Status> {
        let user = aether_casefold::fold(path).starts_with("\\registry\\user\\");
        if self.broken == Some(Break::MachineOnlyRegistry) && user {
            return Err(Status::BadRequest);
        }
        Ok(())
    }

    /// The length of the reference's file behind handle `fh`.
    fn length(&mut self, fh: u64) -> Result<u64, Status> {
        let at = ReadSlot {
            slot: 0,
            arena: None,
            inline_cap: usize::MAX,
        };
        match self
            .reference
            .read(at, RequestFlags::empty(), fh, 0, u32::MAX)?
        {
            ReadReply::Inline(d) => Ok(d.len() as u64),
            ReadReply::Bulk { len, .. } => Ok(u64::from(len)),
        }
    }

    /// [`Break::GrowthGarbage`]: `0xAA` over `from..to` of handle `fh`.
    fn garbage(&mut self, fh: u64, from: u64, to: u64) -> Result<(), Status> {
        if to > from {
            self.reference
                .write(fh, from, &vec![0xAA; (to - from) as usize])?;
        }
        Ok(())
    }

    /// The whole file behind the reference's handle `fh`.
    fn bytes(&mut self, fh: u64) -> Result<Vec<u8>, Status> {
        let at = ReadSlot {
            slot: 0,
            arena: None,
            inline_cap: usize::MAX,
        };
        match self
            .reference
            .read(at, RequestFlags::empty(), fh, 0, u32::MAX)?
        {
            ReadReply::Inline(d) => Ok(d.to_vec()),
            ReadReply::Bulk { .. } => Err(Status::IoError),
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// [`Break::RefusedWriteBumps`]: after a refused registry write, two
    /// writes no scenario looks at, so the version moves.
    fn refused<T>(&mut self, r: Result<T, Status>) -> Result<T, Status> {
        if self.broken == Some(Break::RefusedWriteBumps)
            && matches!(r, Err(s) if s != Status::NotSupported)
        {
            let _ = self.reference.reg_create_key(HIDDEN_KEY, false);
            let _ = self.reference.reg_delete_key(HIDDEN_KEY);
        }
        r
    }

    /// [`Break::RenameDirGhosts`]: everything below directory `dir`, as
    /// stored, parents first, each file with its bytes.
    fn subtree(&mut self, root: RootId, dir: &str) -> Subtree {
        let mut out = Vec::new();
        let mut listing = Listing::default();
        if self.reference.read_dir(root, dir, &mut listing).is_err() {
            return out;
        }
        let rows: Vec<(String, bool)> = listing
            .iter()
            .map(|e| (e.name.to_string(), e.is_dir))
            .collect();
        for (name, is_dir) in rows {
            let path = format!("{dir}/{name}");
            if is_dir {
                out.push((name.clone(), None));
                for (rest, b) in self.subtree(root, &path) {
                    out.push((format!("{name}/{rest}"), b));
                }
            } else if let Ok(o) = self.reference.open(root, OpenFlags::READ, &path) {
                let b = self.bytes(o.fh).unwrap_or_default();
                let _ = self.reference.close(o.fh);
                out.push((name, Some(b)));
            }
        }
        out
    }

    /// [`Break::HandlesByPath`]: the reference's handle at the path file
    /// handle `fh` was opened with, opened now (for a write, with its
    /// access), so it is whatever is at the name; `None` for a handle the
    /// break does not track. A READ that finds nothing there is
    /// `ST_IO_ERROR`, a WRITE or SETATTR `ST_NOT_FOUND`.
    fn at_path(&mut self, fh: u64, write: bool) -> Result<Option<u64>, Status> {
        if self.broken != Some(Break::HandlesByPath) {
            return Ok(None);
        }
        let Some((root, path, flags, old)) = self.held().by_path.get(&fh).cloned() else {
            return Ok(None);
        };
        if let Some(t) = old {
            let _ = self.reference.close(t);
        }
        let access = if write {
            flags & (OpenFlags::WRITE | OpenFlags::APPEND)
        } else {
            OpenFlags::READ
        };
        let now = self.reference.open(root, access, &path).map(|o| o.fh).ok();
        if let Some(e) = self.held().by_path.get_mut(&fh) {
            e.3 = now;
        }
        match now {
            Some(t) => Ok(Some(t)),
            None if write => Err(Status::NotFound),
            None => Err(Status::IoError),
        }
    }

    /// For the breaks of a create with no parent:
    /// [`Break::FileParentIoError`] answers `ST_IO_ERROR` below a file;
    /// [`Break::CreateMakesParents`] makes the missing directories and asks
    /// for another try (`true`).
    fn no_parent(&mut self, root: RootId, path: &str) -> Result<bool, Status> {
        let parts: Vec<&str> = path.split('/').collect();
        let above: Vec<String> = (1..parts.len()).map(|i| parts[..i].join("/")).collect();
        match self.broken {
            Some(Break::FileParentIoError) => {
                for p in &above {
                    if self
                        .reference
                        .get_attr(root, p)
                        .is_ok_and(|a| a.found && !a.is_dir)
                    {
                        return Err(Status::IoError);
                    }
                }
                Ok(false)
            }
            Some(Break::CreateMakesParents) => {
                for p in &above {
                    let _ = self.reference.mkdir(root, 0o755, p);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// [`Break::ParentRespelled`]: each directory above `path` respelled as
    /// `path` spells it.
    fn respell_parents(&mut self, root: RootId, path: &str) {
        if self.broken != Some(Break::ParentRespelled) {
            return;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            let p = parts[..i].join("/");
            let _ = self.reference.rename(root, &p, &p);
        }
    }

    /// A request has reached this serve thread.
    fn arrive(&self) {
        self.arrivals.fetch_add(1, Ordering::AcqRel);
    }

    /// The middle of a request a 2.8.1 break splits: held until another
    /// request has arrived (at most [`RACE_WAIT`]) and then [`RACE_HOLD`]
    /// more, so that a request in flight with this one is served while the
    /// state is half changed, whatever the scheduling.
    fn hold_for_another(&self) {
        let at = self.arrivals.load(Ordering::Acquire);
        let since = std::time::Instant::now();
        while self.arrivals.load(Ordering::Acquire) == at && since.elapsed() < RACE_WAIT {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(RACE_HOLD);
    }

    /// For [`Break::KeepDeleted`] and [`Break::GoneHandlesPrivate`]: the
    /// bytes of every open file handle the reference still serves, before
    /// a request that may end a file.
    fn live_files(&mut self) -> BTreeMap<u64, Vec<u8>> {
        if !matches!(
            self.broken,
            Some(Break::KeepDeleted | Break::GoneHandlesPrivate)
        ) {
            return BTreeMap::new();
        }
        let fhs: Vec<u64> = self.held().paths.keys().copied().collect();
        let mut out = BTreeMap::new();
        for fh in fhs {
            if self.held().kept_by.contains_key(&fh) {
                continue;
            }
            if let Ok(b) = self.bytes(fh) {
                out.insert(fh, b);
            }
        }
        out
    }

    /// The handles of `before` whose file the request just ended keep it:
    /// one copy for all ([`Break::KeepDeleted`]) or one each
    /// ([`Break::GoneHandlesPrivate`]). A DELETE or RENAME ends at most one
    /// file.
    fn keep_gone(&mut self, before: BTreeMap<u64, Vec<u8>>) {
        let mut shared = None;
        for (fh, bytes) in before {
            if self.bytes(fh).is_ok() {
                continue;
            }
            let mut h = self.held();
            let at = match (self.broken, shared) {
                (Some(Break::KeepDeleted), Some(at)) => at,
                _ => {
                    h.kept.push(bytes);
                    h.kept.len() - 1
                }
            };
            shared = Some(at);
            h.kept_by.insert(fh, at);
        }
    }

    /// [`Break::WriteAtClose`]: the WRITEs handle `fh` kept, made now.
    fn flush(&mut self, fh: u64) -> Result<(), Status> {
        let kept = self.held().pending.remove(&fh).unwrap_or_default();
        for (offset, data) in kept {
            self.reference.write(fh, offset, &data)?;
        }
        Ok(())
    }

    /// [`Break::SpellingOutlivesDelete`]: a name just created respelled as
    /// it was before it was deleted.
    fn old_spelling(&mut self, root: RootId, path: &str) {
        if self.broken != Some(Break::SpellingOutlivesDelete) {
            return;
        }
        let old = self
            .held()
            .spellings
            .get(&(root.0, aether_casefold::fold(path)))
            .cloned();
        if let Some(old) = old {
            let _ = self.reference.rename(root, path, &old);
        }
    }

    /// Whether `root`'s `path` is a directory.
    fn is_dir(&mut self, root: RootId, path: &str) -> bool {
        self.reference
            .get_attr(root, path)
            .is_ok_and(|a| a.found && a.is_dir)
    }

    /// The reference's handle for the number `fh` a client sent.
    fn fh_in(&self, fh: u64) -> u64 {
        match self.broken {
            // 0 becomes the reference's 2^64, which wraps to 0: never issued.
            Some(Break::DownwardFh) => (u64::MAX - fh).wrapping_add(1),
            Some(Break::FhFromZero) => fh.wrapping_add(1),
            _ => fh,
        }
    }
}

impl Director for Served {
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        self.arrive();
        let path = self.base(path);
        if self.broken == Some(Break::RootNotFound) && path == "." {
            return Ok(AttrResp {
                found: false,
                is_dir: false,
                size: 0,
                mtime: 0,
            });
        }
        let mut a = self.reference.get_attr(root, path)?;
        let key = (root.0, aether_casefold::fold(path));
        if self.broken == Some(Break::RenameKeepsOldName) && !a.found {
            if let Some(old) = self.held().old_names.get(&key) {
                a = old.clone();
            }
        }
        if self.broken == Some(Break::StaleKind) && a.found {
            let mut h = self.held();
            match h.kinds.get(&key) {
                Some(&k) => a.is_dir = k,
                None => {
                    h.kinds.insert(key, a.is_dir);
                }
            }
        }
        if let Some(&size) = self
            .held()
            .sizes
            .get(&(root.0, aether_casefold::fold(path)))
        {
            a.size = size;
        }
        if self.broken == Some(Break::FoundForMissing) && !a.found {
            a.found = true;
        }
        if self.broken == Some(Break::MtimeMillis) && a.found {
            a.mtime = MILLIS;
        }
        Ok(a)
    }
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        self.arrive();
        if self.broken == Some(Break::ReaddirFileListsItself) {
            if let Ok(a) = self.reference.get_attr(root, path) {
                if a.found && !a.is_dir {
                    *out = Listing::default();
                    out.push(aether_protocol::DirEntry {
                        name: path.rsplit('/').next().unwrap_or(path),
                        is_dir: false,
                        size: a.size,
                        mtime: a.mtime,
                    });
                    return Ok(());
                }
            }
        }
        self.reference.read_dir(root, path, out)?;
        let millis = self.broken == Some(Break::MtimeMillis);
        let held = self.broken == Some(Break::SetattrAtClose);
        if self.broken == Some(Break::TruncateReaddir) || millis || held {
            // `count`, then 21 bytes and the name per entry (wire.md 3.4.2).
            let mut size = 4usize;
            let truncate = self.broken == Some(Break::TruncateReaddir);
            let kept: Vec<(String, bool, u64, i64)> = out
                .iter()
                .take_while(|e| {
                    size += 21 + e.name.len();
                    !truncate || size <= PAYLOAD_CAP as usize
                })
                .map(|e| {
                    let mtime = if millis { MILLIS } else { e.mtime };
                    let child = aether_casefold::fold(&format!("{path}/{}", e.name));
                    let child = child.strip_prefix("./").unwrap_or(&child).to_string();
                    let size = self
                        .held()
                        .sizes
                        .get(&(root.0, child))
                        .copied()
                        .unwrap_or(e.size);
                    (e.name.to_string(), e.is_dir, size, mtime)
                })
                .collect();
            *out = Listing::default();
            for (name, is_dir, size, mtime) in &kept {
                out.push(aether_protocol::DirEntry {
                    name,
                    is_dir: *is_dir,
                    size: *size,
                    mtime: *mtime,
                });
            }
        }
        Ok(())
    }
    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        self.arrive();
        let path = self.base(path);
        let own_end = self.broken == Some(Break::AppendOwnEnd) && flags.contains(OpenFlags::APPEND);
        let flags = if own_end {
            flags - OpenFlags::APPEND
        } else {
            flags
        };
        let flags = if self.broken == Some(Break::DirWriteOpen)
            && flags.contains(OpenFlags::WRITE)
            && !flags.contains(OpenFlags::EXCL)
            && self.is_dir(root, path)
        {
            OpenFlags::READ
        } else {
            flags
        };
        let existed = self.reference.get_attr(root, path).is_ok_and(|a| a.found);
        let flags = if self.broken == Some(Break::RacyExcl)
            && flags.contains(OpenFlags::CREATE | OpenFlags::EXCL)
            && !existed
        {
            self.hold_for_another();
            flags - OpenFlags::EXCL
        } else {
            flags
        };
        if self.broken == Some(Break::RacyCreate) && flags.contains(OpenFlags::CREATE) && !existed {
            let parent = path.rsplit_once('/').map(|(p, _)| p.to_string());
            self.hold_for_another();
            if self.is_dir(root, path) {
                let _ = self.reference.delete(root, path);
            }
            if let Some(p) = parent {
                if !self.is_dir(root, &p) {
                    let _ = self.reference.mkdir(root, 0o755, &p);
                }
            }
        }
        let mut opened = self.reference.open(root, flags, path);
        if opened == Err(Status::NotFound)
            && flags.contains(OpenFlags::CREATE)
            && self.no_parent(root, path)?
        {
            opened = self.reference.open(root, flags, path);
        }
        let mut o = opened?;
        if !existed && flags.contains(OpenFlags::CREATE) {
            self.respell_parents(root, path);
            self.old_spelling(root, path);
        }
        if matches!(self.broken, Some(Break::TornAppend | Break::LostUpdate))
            && flags.contains(OpenFlags::APPEND)
        {
            self.held().appends.insert(o.fh);
        }
        if self.broken == Some(Break::HandlesByPath) && !o.is_dir {
            let access = flags & (OpenFlags::WRITE | OpenFlags::APPEND);
            self.held()
                .by_path
                .insert(o.fh, (root, path.to_string(), access, None));
        }
        if own_end {
            self.held().ends.insert(o.fh, o.size);
        }
        {
            let key = (root.0, aether_casefold::fold(path));
            let mut h = self.held();
            if let Some(&size) = h.sizes.get(&key) {
                o.size = size;
            }
            h.paths.insert(o.fh, key);
        }
        if self.broken == Some(Break::SnapshotReads) && !o.is_dir {
            let snap = self.bytes(o.fh)?;
            self.held().snaps.insert(o.fh, snap);
        }
        if self.broken == Some(Break::WriteOnlyUnreadable)
            && flags.contains(OpenFlags::WRITE)
            && !flags.contains(OpenFlags::READ)
        {
            self.write_only
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(o.fh);
        }
        if self.broken == Some(Break::StaleImmutable)
            && !o.is_dir
            && !flags.contains(OpenFlags::WRITE)
        {
            o.immutable = true;
        }
        o.fh = self.fh_out(o.fh);
        Ok(o)
    }
    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        self.arrive();
        let at = match self.broken {
            Some(Break::BankZero) => ReadSlot { slot: 0, ..at },
            _ => at,
        };
        if self.broken == Some(Break::FhZeroOk) && fh == 0 {
            return Ok(ReadReply::Inline(&[]));
        }
        let slot_cap = at.inline_cap;
        let fh = self.fh_in(fh);
        let gone = self.held().kept_by.get(&fh).copied();
        if let Some(at) = gone {
            let b = self.held().kept[at].clone();
            let start = offset.min(b.len() as u64) as usize;
            let n = (b.len() - start).min(len as usize).min(slot_cap);
            self.view = b[start..start + n].to_vec();
            return Ok(ReadReply::Inline(&self.view));
        }
        let fh = self.at_path(fh, false)?.unwrap_or(fh);
        let kept = self.held().pending.get(&fh).cloned();
        if let Some(kept) = kept {
            let mut b = self.bytes(fh)?;
            for (o, d) in kept {
                let end = o as usize + d.len();
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[o as usize..end].copy_from_slice(&d);
            }
            let start = offset.min(b.len() as u64) as usize;
            let n = (b.len() - start).min(len as usize).min(at.inline_cap);
            self.view = b[start..start + n].to_vec();
            return Ok(ReadReply::Inline(&self.view));
        }
        let offset = match self.broken {
            Some(Break::ReadOffsetWraps) => offset & 0xFFFF_FFFF,
            _ => offset,
        };
        let snap = self.held().snaps.get(&fh).cloned();
        if let Some(b) = snap {
            let start = offset.min(b.len() as u64) as usize;
            let n = (b.len() - start).min(len as usize).min(at.inline_cap);
            self.view = b[start..start + n].to_vec();
            return Ok(ReadReply::Inline(&self.view));
        }
        if self.broken == Some(Break::WriteOnlyUnreadable)
            && self
                .write_only
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&fh)
        {
            return Err(Status::IoError);
        }
        if let (Some(Break::LateBank), Some(arena)) = (self.broken, at.arena) {
            let mut banks = self.banks.lock().unwrap_or_else(|e| e.into_inner());
            for (slot, n) in banks.drain(..).filter(|&(s, _)| s != at.slot) {
                let _ = arena.fill_bank(slot, n, |b| {
                    b.fill(0xEE);
                    Ok(b.len())
                });
            }
        }
        let broken = self.broken;
        let banks = self.banks.clone();
        // [`Break::BulkCache`]: what an earlier bulk READ of this range gave,
        // or the bytes now, to keep if this one is answered in bulk.
        let path_of = self.held().paths.get(&fh).cloned();
        let cache = match (broken, path_of) {
            (Some(Break::BulkCache), Some((r, p))) => {
                let key = (r, p, offset, len);
                let before = self.held().bulk.get(&key).cloned();
                let now = self.bytes(fh).unwrap_or_default();
                let start = offset.min(now.len() as u64) as usize;
                let end = start + (now.len() - start).min(len as usize);
                Some((key, before, now[start..end].to_vec()))
            }
            _ => None,
        };
        let held = self.held.clone();
        match self.reference.read(at, slot_flags, fh, offset, len) {
            Ok(ReadReply::Bulk { len, arena_offset }) => {
                if let (Some((key, before, now)), Some(arena)) = (cache, at.arena) {
                    let mut h = held.lock().unwrap_or_else(|e| e.into_inner());
                    match before {
                        Some(b) => {
                            let n = b.len();
                            let _ = arena.fill_bank(at.slot, n, |buf| {
                                buf.copy_from_slice(&b[..buf.len()]);
                                Ok(buf.len())
                            });
                            return Ok(ReadReply::Bulk {
                                len: n as u32,
                                arena_offset,
                            });
                        }
                        None => {
                            h.bulk.insert(key, now);
                        }
                    }
                }
                let len = match broken {
                    Some(Break::ShortBulk) => len.min(256 * 1024),
                    Some(Break::LateBank) => {
                        banks
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((at.slot, len as usize));
                        len
                    }
                    _ => len,
                };
                Ok(ReadReply::Bulk { len, arena_offset })
            }
            other => other,
        }
    }
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        self.arrive();
        if self.broken == Some(Break::FhZeroOk) && fh == 0 {
            return Ok(data.len() as u32);
        }
        let fh = self.fh_in(fh);
        let gone = self.held().kept_by.get(&fh).copied();
        if let Some(at) = gone {
            let mut h = self.held();
            let b = &mut h.kept[at];
            let end = offset as usize + data.len();
            if !data.is_empty() {
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[offset as usize..end].copy_from_slice(data);
            }
            return Ok(data.len() as u32);
        }
        let fh = self.at_path(fh, true)?.unwrap_or(fh);
        if self.broken == Some(Break::WriteAtClose) && !data.is_empty() {
            // Checked as the reference checks it, then kept.
            self.length(fh)?;
            self.held()
                .pending
                .entry(fh)
                .or_default()
                .push((offset, data.to_vec()));
            return Ok(data.len() as u32);
        }
        if self.broken == Some(Break::HugeWriteOk)
            && !data.is_empty()
            && offset
                .checked_add(data.len() as u64)
                .is_none_or(|end| end > aether_refdirector::MAX_FILE_LEN as u64)
        {
            return Ok(data.len() as u32);
        }
        if self.broken == Some(Break::ShortWrite) && data.len() > 1 {
            return self.reference.write(fh, offset, &data[..data.len() / 2]);
        }
        if self.broken == Some(Break::GrowthGarbage) {
            let end = self.length(fh)?;
            self.garbage(fh, end, offset)?;
        }
        if self.broken == Some(Break::TornAppend)
            && data.len() > 1
            && self.held().appends.contains(&fh)
        {
            let half = data.len() / 2;
            self.reference.write(fh, 0, &data[..half])?;
            self.hold_for_another();
            self.reference.write(fh, 0, &data[half..])?;
            return Ok(data.len() as u32);
        }
        if self.broken == Some(Break::LostUpdate)
            && !data.is_empty()
            && offset < (1 << 20)
            && !self.held().appends.contains(&fh)
        {
            let mut whole = self.bytes(fh)?;
            self.hold_for_another();
            let end = offset as usize + data.len();
            if whole.len() < end {
                whole.resize(end, 0);
            }
            whole[offset as usize..end].copy_from_slice(data);
            self.reference.write(fh, 0, &whole)?;
            return Ok(data.len() as u32);
        }
        match self.broken {
            Some(Break::SnapshotReads) => {
                let n = self.reference.write(fh, offset, data)?;
                let now = self.bytes(fh)?;
                if let Some(snap) = self.held().snaps.get_mut(&fh) {
                    // Its own write, at the place the file put it.
                    let at = if data.is_empty() {
                        snap.len()
                    } else {
                        (offset as usize).min(now.len().saturating_sub(data.len()))
                    };
                    let end = at + data.len();
                    if snap.len() < end {
                        snap.resize(end, 0);
                    }
                    snap[at..end].copy_from_slice(data);
                }
                return Ok(n);
            }
            Some(Break::AppendOwnEnd) => {
                let end = self.held().ends.get(&fh).copied();
                if let Some(end) = end {
                    let n = self.reference.write(fh, end, data)?;
                    self.held().ends.insert(fh, end + u64::from(n));
                    return Ok(n);
                }
            }
            Some(Break::TruncateKeepsBytes) => {
                let cut = self.held().cut.get(&fh).cloned();
                if let Some((from, kept)) = cut {
                    let end = self.length(fh)?;
                    let lo = end.max(from);
                    let hi = offset.min(from + kept.len() as u64);
                    if lo < hi {
                        let shown = kept[(lo - from) as usize..(hi - from) as usize].to_vec();
                        self.reference.write(fh, lo, &shown)?;
                    }
                }
            }
            _ => {}
        }
        self.reference.write(fh, offset, data)
    }
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status> {
        self.arrive();
        if self.broken == Some(Break::FhZeroOk) && fh == 0 {
            return Ok(());
        }
        let fh = self.fh_in(fh);
        let gone = self.held().kept_by.get(&fh).copied();
        if let Some(at) = gone {
            self.held().kept[at].resize(size as usize, 0);
            return Ok(());
        }
        let fh = self.at_path(fh, true)?.unwrap_or(fh);
        self.flush(fh)?;
        if self.broken == Some(Break::HugeWriteOk) && size > aether_refdirector::MAX_FILE_LEN as u64
        {
            return Ok(());
        }
        match self.broken {
            Some(Break::SnapshotReads) => {
                self.reference.set_attr(fh, size)?;
                if let Some(snap) = self.held().snaps.get_mut(&fh) {
                    snap.resize(size as usize, 0);
                }
                return Ok(());
            }
            Some(Break::SetattrAtClose) => {
                let before = self.length(fh)?;
                self.reference.set_attr(fh, size)?;
                let mut h = self.held();
                if let Some(key) = h.paths.get(&fh).cloned() {
                    h.sizes.entry(key).or_insert(before);
                }
                return Ok(());
            }
            Some(Break::TruncateKeepsBytes) => {
                let bytes = self.bytes(fh)?;
                if size < bytes.len() as u64 {
                    self.held()
                        .cut
                        .insert(fh, (size, bytes[size as usize..].to_vec()));
                }
            }
            _ => {}
        }
        let end = match self.broken {
            Some(Break::GrowthGarbage) => Some(self.length(fh)?),
            _ => None,
        };
        self.reference.set_attr(fh, size)?;
        match end {
            Some(end) => self.garbage(fh, end, size),
            None => Ok(()),
        }
    }
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        self.arrive();
        let (f, t) = (aether_casefold::fold(from), aether_casefold::fold(to));
        let broken = self.broken;
        match broken {
            Some(Break::AncestorBadRequest) if f.starts_with(&format!("{t}/")) => {
                return Err(Status::BadRequest)
            }
            Some(Break::SameNameExists)
                if from == to
                    && self
                        .reference
                        .stored_names(root, 0, &f)
                        .is_ok_and(|stored| stored == to) =>
            {
                return Err(Status::Exists)
            }
            Some(Break::RenameInSteps)
                if self
                    .reference
                    .get_attr(root, from)
                    .is_ok_and(|a| a.found && !a.is_dir)
                    && self.reference.get_attr(root, to).is_ok_and(|a| !a.found) =>
            {
                // Moved, then a copy left at `from` for a while.
                let src = self.reference.open(root, OpenFlags::READ, from)?.fh;
                let bytes = self.bytes(src);
                let _ = self.reference.close(src);
                self.reference.rename(root, from, to)?;
                if let Ok(o) = self
                    .reference
                    .open(root, OpenFlags::WRITE | OpenFlags::CREATE, from)
                {
                    let _ = self.reference.write(o.fh, 0, &bytes.unwrap_or_default());
                    let _ = self.reference.close(o.fh);
                    self.hold_for_another();
                    let _ = self.reference.delete(root, from);
                }
                return Ok(());
            }
            Some(Break::FileOntoDirKept)
                if self
                    .reference
                    .get_attr(root, from)
                    .is_ok_and(|a| a.found && !a.is_dir)
                    && self.is_dir(root, to) =>
            {
                let mut listing = Listing::default();
                let empty = self.reference.read_dir(root, to, &mut listing).is_ok()
                    && listing.iter().next().is_none();
                if empty {
                    return Ok(());
                }
            }
            _ => {}
        }
        let before = self.reference.get_attr(root, from).ok();
        let respell = (self.broken == Some(Break::CaseRenameStaleChildren)
            && f == t
            && before.as_ref().is_some_and(|a| a.found && a.is_dir))
        .then(|| {
            self.reference
                .stored_names(root, 0, from)
                .map(str::to_string)
                .ok()
        })
        .flatten();
        let ghosts = self.broken == Some(Break::RenameDirGhosts)
            && self
                .reference
                .get_attr(root, from)
                .is_ok_and(|a| a.found && a.is_dir);
        let below = if ghosts {
            self.subtree(root, from)
        } else {
            Vec::new()
        };
        let live = self.live_files();
        self.reference.rename(root, from, to)?;
        self.keep_gone(live);
        if ghosts {
            self.held()
                .ghosts
                .push((root.0, aether_casefold::fold(from), below));
        }
        if let Some(old) = respell {
            self.held().respelled.push((root.0, f.clone(), old));
        }
        if let (Some(Break::RenameKeepsOldName), Some(a)) = (self.broken, before) {
            if a.found && !a.is_dir && f != t {
                self.held().old_names.insert((root.0, f), a);
            }
        }
        Ok(())
    }
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        self.arrive();
        let key = (root.0, aether_casefold::fold(path));
        if self.broken == Some(Break::DeletePendingUntilClose) && self.is_dir(root, path) {
            let below = format!("{}/", key.1);
            if self
                .held()
                .paths
                .values()
                .any(|(r, p)| *r == root.0 && p.starts_with(&below))
            {
                return Err(Status::IoError);
            }
        }
        let spelled = (self.broken == Some(Break::SpellingOutlivesDelete))
            .then(|| {
                self.reference
                    .stored_names(root, 0, path)
                    .map(str::to_string)
                    .ok()
            })
            .flatten();
        let live = self.live_files();
        let r = self.reference.delete(root, path);
        if r.is_ok() {
            self.keep_gone(live);
        }
        if r == Err(Status::IoError)
            && self.broken == Some(Break::DeleteDirIgnoresEmptyDirs)
            && self.is_dir(root, path)
        {
            let below = self.subtree(root, path);
            if below.iter().all(|(_, b)| b.is_none()) {
                for (rest, _) in below.iter().rev() {
                    let _ = self.reference.delete(root, &format!("{path}/{rest}"));
                }
                return self.reference.delete(root, path);
            }
        }
        r?;
        if let Some(sp) = spelled {
            self.held().spellings.insert(key, sp);
        }
        if self.broken == Some(Break::DeleteIgnoresRoot) {
            let others: Vec<RootId> = self
                .tree
                .roots
                .iter()
                .copied()
                .filter(|r| *r != root)
                .collect();
            for other in others {
                let _ = self.reference.delete(other, path);
            }
        }
        Ok(())
    }
    fn mkdir(&mut self, root: RootId, mode: u32, path: &str) -> Result<(), Status> {
        self.arrive();
        let mut made = self.reference.mkdir(root, mode, path);
        if made == Err(Status::NotFound) && self.no_parent(root, path)? {
            made = self.reference.mkdir(root, mode, path);
        }
        made?;
        self.respell_parents(root, path);
        self.old_spelling(root, path);
        let folded = aether_casefold::fold(path);
        let back = self
            .held()
            .ghosts
            .iter()
            .find(|g| g.0 == root.0 && g.1 == folded)
            .map(|g| g.2.clone());
        for (rest, bytes) in back.unwrap_or_default() {
            let p = format!("{path}/{rest}");
            match bytes {
                None => {
                    let _ = self.reference.mkdir(root, mode, &p);
                }
                Some(b) => {
                    let flags = OpenFlags::WRITE | OpenFlags::CREATE;
                    if let Ok(o) = self.reference.open(root, flags, &p) {
                        let _ = self.reference.write(o.fh, 0, &b);
                        let _ = self.reference.close(o.fh);
                    }
                }
            }
        }
        Ok(())
    }
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        self.arrive();
        if self.broken == Some(Break::FhZeroOk) && fh == 0 {
            return Ok(());
        }
        let fh = self.fh_in(fh);
        let _ = self.flush(fh);
        self.held().kept_by.remove(&fh);
        self.held().appends.remove(&fh);
        let at = self.held().by_path.remove(&fh).and_then(|e| e.3);
        if let Some(t) = at {
            let _ = self.reference.close(t);
        }
        {
            let mut h = self.held();
            h.ends.remove(&fh);
            h.cut.remove(&fh);
            if let Some(key) = h.paths.remove(&fh) {
                h.sizes.remove(&key);
            }
            h.snaps.remove(&fh);
        };
        self.reference.close(fh)?;
        self.write_only
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh);
        Ok(())
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        self.reference.heartbeat()
    }
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        self.arrive();
        if self.broken == Some(Break::SkipPastEndEcho) && skip as usize >= path.split('/').count() {
            self.echo = path.rsplit('/').next().unwrap_or(path).to_string();
            return Ok(&self.echo);
        }
        if self.broken == Some(Break::CaseRenameStaleChildren) {
            let folded = aether_casefold::fold(path);
            let hit = self
                .held()
                .respelled
                .iter()
                .rev()
                .find(|(r, d, _)| *r == root.0 && folded.starts_with(&format!("{d}/")))
                .cloned();
            if let Some((_, _, old)) = hit {
                let full = self.reference.stored_names(root, 0, path)?.to_string();
                let mut parts: Vec<&str> = full.split('/').collect();
                for (i, o) in old.split('/').enumerate() {
                    if let Some(p) = parts.get_mut(i) {
                        *p = o;
                    }
                }
                let skip = (skip as usize).min(parts.len());
                self.echo = parts[skip..].join("/");
                return Ok(&self.echo);
            }
        }
        self.reference.stored_names(root, skip, path)
    }
    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status> {
        self.arrive();
        self.hive(path)?;
        self.reference.reg_lookup(path)
    }
    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status> {
        self.arrive();
        self.hive(path)?;
        // Counted once the answer is made, so a write let go by the count
        // cannot reach it.
        let reply = self.reference.reg_key(path);
        if self.broken == Some(Break::BumpBeforeWrite) {
            let (n, cv) = &*self.reg_reads;
            *n.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            cv.notify_all();
        }
        reply
    }
    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status> {
        self.arrive();
        if let (Some(Break::BumpBeforeWrite), Some(ring)) = (self.broken, &self.ring) {
            let (n, cv) = &*self.reg_reads;
            let reads = n.lock().unwrap_or_else(|e| e.into_inner());
            let before = *reads;
            let now = ring::reg_generation(ring.seg());
            ring::publish_reg_generation(ring.seg(), now + 1);
            if before > 0 {
                // The second REG_KEY answered from here was asked after the
                // first came back, so after the new generation was out.
                let _ = cv
                    .wait_timeout_while(reads, BUMP_HOLD, |r| *r < before + 2)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        if self.broken == Some(Break::SetUnderTombstoneNotFound)
            && self
                .reference
                .reg_lookup(path)
                .is_ok_and(|l| l.state == aether_protocol::Lookup::Tombstoned)
        {
            return Err(Status::NotFound);
        }
        if self.broken == Some(Break::ValueNameBytes) && name.len() > 16_383 {
            return Err(Status::BadRequest);
        }
        if self.broken == Some(Break::SetValueAppends) {
            let held = self.reference.reg_key(path).is_ok_and(|k| {
                k.node.as_ref().is_some_and(|n| {
                    n.values
                        .iter()
                        .any(|v| aether_casefold::fold(&v.name) == aether_casefold::fold(name))
                })
            });
            if held {
                let _ = self.reference.reg_delete_value(path, name);
            }
        }
        let r = self.reference.reg_set_value(path, name, ty, data);
        self.refused(r)
    }
    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status> {
        self.arrive();
        if self.broken == Some(Break::ValueNameBytes) && name.len() > 16_383 {
            return Err(Status::BadRequest);
        }
        let r = self.reference.reg_delete_value(path, name);
        self.refused(r)
    }
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status> {
        self.arrive();
        if self.broken == Some(Break::RacyRegCreate)
            && self
                .reference
                .reg_lookup(path)
                .is_ok_and(|l| l.state == aether_protocol::Lookup::Absent)
        {
            self.hold_for_another();
            return match self.reference.reg_create_key(path, volatile) {
                Err(Status::Exists) => self.reference.reg_lookup(path).map(|l| l.version),
                r => r,
            };
        }
        let r = self.reference.reg_create_key(path, volatile);
        if r.is_ok() && self.broken == Some(Break::DeleteKeepsSubtree) {
            let back = self
                .held()
                .reg_hidden
                .remove(&aether_casefold::fold(path))
                .unwrap_or_default();
            for (name, ty, data) in back {
                let _ = self.reference.reg_set_value(path, &name, ty, &data);
            }
        }
        self.refused(r)
    }
    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status> {
        self.arrive();
        if self.broken == Some(Break::DeleteUnrecordedNotFound)
            && path.split('\\').count() <= 64
            && path.split('\\').all(|c| c.chars().count() <= 255)
            && self
                .reference
                .reg_lookup(path)
                .is_ok_and(|l| l.state == aether_protocol::Lookup::Absent)
        {
            return Err(Status::NotFound);
        }
        let values: Vec<(String, u32, Vec<u8>)> = match self.broken {
            Some(Break::DeleteKeepsSubtree) => self
                .reference
                .reg_key(path)
                .ok()
                .and_then(|k| {
                    k.node.as_ref().map(|n| {
                        n.values
                            .iter()
                            .map(|v| (v.name.to_string(), v.ty, v.data.to_vec()))
                            .collect()
                    })
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let r = self.reference.reg_delete_key(path);
        if r.is_ok() && !values.is_empty() {
            self.held()
                .reg_hidden
                .insert(aether_casefold::fold(path), values);
        }
        self.refused(r)
    }
    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status> {
        self.arrive();
        let r = self.reference.reg_rename_key(path, new_leaf);
        if r.is_ok() && self.broken == Some(Break::RenameUnchanged) {
            self.held().renamed_away.insert(aether_casefold::fold(path));
        }
        self.refused(r)
    }
    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status> {
        self.arrive();
        let mut c = self.reference.reg_changed(path, subtree, since)?;
        if self.broken == Some(Break::RenameUnchanged)
            && self
                .held()
                .renamed_away
                .contains(&aether_casefold::fold(path))
        {
            c.changed = false;
        }
        Ok(c)
    }
    fn rename_x(
        &mut self,
        from_root: RootId,
        to_root: RootId,
        from: &str,
        to: &str,
    ) -> Result<(), Status> {
        self.arrive();
        if self.broken == Some(Break::RenameXOntoDirOk)
            && from_root != to_root
            && self.is_dir(to_root, to)
        {
            return Ok(());
        }
        self.reference.rename_x(from_root, to_root, from, to)?;
        if self.broken == Some(Break::RenameXFromMount) && from_root != to_root {
            let mounted = self
                .tree
                .files
                .get(&(from_root.0, aether_casefold::fold(from)))
                .cloned()
                .unwrap_or_default();
            if let Ok(o) = self
                .reference
                .open(to_root, OpenFlags::WRITE | OpenFlags::TRUNC, to)
            {
                let _ = self.reference.write(o.fh, 0, &mounted);
                let _ = self.reference.close(o.fh);
            }
        }
        Ok(())
    }
    fn stage_exec(&mut self, root: RootId, path: &str) -> Result<&str, Status> {
        let wrong = match self.broken {
            Some(Break::StageCopy | Break::StageMountBytes) => false,
            Some(Break::StageWrongBytes) => true,
            _ => return self.reference.stage_exec(root, path),
        };
        // wire.md 3.4.14: with a stager, a directory or a missing path is
        // ST_NOT_FOUND.
        if !self
            .reference
            .get_attr(root, path)
            .is_ok_and(|a| a.found && !a.is_dir)
        {
            return Err(Status::NotFound);
        }
        let fh = self.reference.open(root, OpenFlags::READ, path)?.fh;
        let at = ReadSlot {
            slot: 0,
            arena: None,
            inline_cap: usize::MAX,
        };
        let read = self
            .reference
            .read(at, RequestFlags::empty(), fh, 0, u32::MAX)
            .map(|r| match r {
                ReadReply::Inline(d) => d.to_vec(),
                ReadReply::Bulk { .. } => Vec::new(),
            });
        let _ = self.reference.close(fh);
        let mut bytes = read?;
        if self.broken == Some(Break::StageMountBytes) {
            if let Some(b) = self.tree.files.get(&(root.0, aether_casefold::fold(path))) {
                bytes = b.clone();
            }
        }
        if wrong {
            if let Some(b) = bytes.first_mut() {
                *b ^= 0xff;
            } else {
                bytes.push(1);
            }
        }
        let name = path.rsplit('/').next().unwrap_or("staged");
        let dir = std::env::temp_dir().join(unique("staged"));
        std::fs::create_dir_all(&dir).map_err(|_| Status::IoError)?;
        let to = dir.join(name);
        std::fs::write(&to, &bytes).map_err(|_| Status::IoError)?;
        self.staged_files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(to.clone());
        self.staged = std::path::absolute(&to)
            .unwrap_or(to)
            .to_string_lossy()
            .into_owned();
        Ok(&self.staged)
    }
    fn unknown(&mut self, opcode: u32) -> Status {
        match self.broken {
            Some(Break::IoErrorForBadRequest) => Status::IoError,
            _ => self.reference.unknown(opcode),
        }
    }
    fn undecodable(&mut self, op: Opcode, flags: u32, payload: &[u8]) -> Status {
        match self.broken {
            Some(Break::IoErrorForBadRequest) => Status::IoError,
            _ => self.reference.undecodable(op, flags, payload),
        }
    }
}

/// How the test adapter misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misbehave {
    /// Exits at once, before reading anything.
    Exit,
    /// Answers every request with a line that is not JSON.
    Garbage,
    /// Reads a request and never answers.
    Hang,
    /// Starts a second test adapter that hangs (the same binary, holding
    /// this one's stdout and stderr), reports its pid on stderr, then hangs
    /// itself.
    HangGrandchild,
    /// Refuses every mount; answers unmount.
    Refuse,
    /// The reference adapter breaking a rule no single reply shows, which
    /// the checks across steps must find: [`Break::StaleImmutable`] or
    /// [`Break::FrozenRegGen`].
    Director(Break),
}

/// Runs the test adapter. The exit code.
pub fn test_adapter(how: Misbehave) -> i32 {
    use std::io::BufRead;
    match how {
        Misbehave::Exit => {
            eprintln!("test adapter: exiting");
            3
        }
        Misbehave::Garbage => {
            eprintln!("test adapter: answering garbage");
            for line in std::io::stdin().lock().lines() {
                if line.is_err() {
                    break;
                }
                println!("this is not json");
            }
            0
        }
        Misbehave::Hang => {
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            eprintln!("test adapter: not answering");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        Misbehave::HangGrandchild => {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("test adapter: no current exe: {e}");
                    return 2;
                }
            };
            match std::process::Command::new(exe)
                .args(["test-adapter", "--misbehave", "hang"])
                .stdin(std::process::Stdio::null())
                .spawn()
            {
                Ok(g) => eprintln!("test adapter: grandchild pid {}", g.id()),
                Err(e) => {
                    eprintln!("test adapter: cannot start the grandchild: {e}");
                    return 2;
                }
            }
            test_adapter(Misbehave::Hang)
        }
        Misbehave::Director(b) => reference_adapter(RingKind::File, true, Some(b)),
        Misbehave::Refuse => {
            answer_requests(|req| -> Value {
                match req {
                    AdapterRequest::Mount { .. } => {
                        json!({"ok": false, "error": "no trees today"})
                    }
                    AdapterRequest::Unmount => json!({"ok": true}),
                }
            });
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = r"\Registry\Machine\Software\Held";

    /// Two handles on one reference with a registry, breaking `how`, over a
    /// file ring in `dir` that the reference publishes its generation to.
    fn pair(how: Break, dir: &Path) -> (Served, Served, Arc<dyn RingBacking>) {
        let f = aether_filering::FileRing::create(&dir.join("r.ring"), 16 << 20).unwrap();
        let b: Arc<dyn RingBacking> = Arc::new(f);
        init(&*b).unwrap();
        let reference = Reference::new().with_registry();
        let published = b.clone();
        reference.publish_generation(move |g| ring::publish_reg_generation(published.seg(), g));
        let one = Served {
            reference,
            broken: Some(how),
            ring: Some(b.clone()),
            banks: Arc::default(),
            staged: String::new(),
            staged_files: Arc::default(),
            write_only: Arc::default(),
            reg_reads: Arc::default(),
            held: Arc::default(),
            view: Vec::new(),
            echo: String::new(),
            arrivals: Arc::default(),
            tree: Arc::default(),
        };
        let two = Served {
            reference: one.reference.share(),
            ring: Some(b.clone()),
            banks: one.banks.clone(),
            staged: String::new(),
            staged_files: one.staged_files.clone(),
            write_only: one.write_only.clone(),
            reg_reads: one.reg_reads.clone(),
            held: one.held.clone(),
            view: Vec::new(),
            echo: String::new(),
            arrivals: one.arrivals.clone(),
            tree: one.tree.clone(),
            ..one
        };
        (one, two, b)
    }

    /// The value `n` of [`KEY`], as a reader sees it.
    fn n(d: &mut Served) -> Option<Vec<u8>> {
        let k = d.reg_key(KEY).unwrap();
        k.node
            .as_ref()
            .and_then(|node| node.values.iter().find(|v| v.name == "n"))
            .map(|v| v.data.to_vec())
    }

    /// [`Break::BumpBeforeWrite`] holds a write, its generation already
    /// published, until a reader has read twice after it: the reader sees
    /// the new generation with the old value however long it takes to look.
    #[test]
    fn bump_before_write_holds_the_write_until_a_reader_has_read_twice() {
        let dir = aether_testkit::tempdir().unwrap();
        let (mut writer, mut reader, ring) = pair(Break::BumpBeforeWrite, dir.path());
        writer.reg_create_key(KEY, false).unwrap();
        assert_eq!(n(&mut reader), None, "a reader has read once");
        let before = ring::reg_generation(ring.seg());
        let w = std::thread::spawn(move || {
            writer
                .reg_set_value(KEY, "n", 4, &7u32.to_le_bytes())
                .map(|_| ())
        });
        let began = std::time::Instant::now();
        while ring::reg_generation(ring.seg()) == before {
            assert!(began.elapsed() < BUMP_HOLD, "the generation never moved");
            std::thread::yield_now();
        }
        // However slow this reader is, the write is still held.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(n(&mut reader), None, "the first read after the bump");
        assert_eq!(n(&mut reader), None, "the second read after the bump");
        assert_eq!(w.join().unwrap(), Ok(()));
        assert_eq!(n(&mut reader), Some(7u32.to_le_bytes().to_vec()));
    }
}
