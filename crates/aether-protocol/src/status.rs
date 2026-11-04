//! Status codes a reply carries, and the flag words of OPEN and of a slot.
//!
//! Values `0` through `-11` are fixed by the ring protocol, and by the
//! injected shim DLL, which matches statuses by number across the process
//! boundary, and must not be renumbered. New statuses append at the next
//! free negative number instead. On the wire a status is a plain `i32`, so a
//! director written outside this workspace needs no wrapper type.

use crate::wire::DecodeError;

macro_rules! statuses {
    ($( $(#[$meta:meta])* $variant:ident = $num:literal $name:literal, )*) => {
        /// A reply's status word. The reply payload means something only
        /// for [`Status::Ok`]; every error status has an empty payload.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(i32)]
        pub enum Status {
            $( $(#[$meta])* $variant = $num, )*
        }

        impl Status {
            /// Every status, in number order (0 down).
            pub const ALL: &'static [Status] = &[$(Status::$variant),*];

            /// The status's name in the protocol table (`"not-found"`, ...).
            pub const fn name(self) -> &'static str {
                match self {
                    $( Status::$variant => $name, )*
                }
            }
        }

        /// Every status code with its name, in number order.
        pub const STATUSES: &[(&str, i32)] = &[$( ($name, $num) ),*];

        impl TryFrom<i32> for Status {
            type Error = DecodeError;
            fn try_from(n: i32) -> Result<Status, DecodeError> {
                match n {
                    $( $num => Ok(Status::$variant), )*
                    _ => Err(DecodeError::new("unknown status")),
                }
            }
        }
    };
}

statuses! {
    Ok = 0 "ok",
    /// No such path, root, key or value.
    NotFound = -1 "not-found",
    /// A path component is a file.
    NotADirectory = -2 "not-a-directory",
    /// The payload did not decode, the opcode is unknown, a path escaped its
    /// root, or a registry limit was exceeded.
    BadRequest = -3 "bad-request",
    /// Anything without a better status.
    IoError = -4 "io-error",
    /// A file operation on a directory handle.
    IsDir = -5 "is-dir",
    /// Unknown handle.
    BadFh = -6 "bad-fh",
    /// Storage full.
    NoSpace = -7 "no-space",
    /// The director does not implement this method.
    NotSupported = -8 "not-supported",
    /// No writable layer serves this path.
    ReadOnly = -9 "read-only",
    /// `OpenFlags::EXCL` (create-new) refused because the path already
    /// exists; a registry key that already exists.
    Exists = -10 "exists",
    /// The answer does not fit an inline ring reply (`payload_cap - 8`
    /// bytes). Used by the registry `REG_KEY` op for a key whose overlay data
    /// exceeds the ring's payload; the shim treats it like a failed director
    /// read and serves the real key.
    ReplyTooLarge = -11 "reply-too-large",
    /// A move between two roots of something the director does not copy
    /// across (a directory). The shim answers `STATUS_NOT_SAME_DEVICE`, what
    /// Windows says for a move between volumes, so a caller falls back to
    /// copy and delete.
    CrossRoot = -12 "cross-root",
}

impl Status {
    /// The number on the wire.
    pub const fn code(self) -> i32 {
        self as i32
    }
}

bitflags::bitflags! {
    /// OPEN's `oflags` word. Unknown bits survive a decode, so a director
    /// sees exactly what the client sent.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct OpenFlags: u32 {
        /// Read access.
        const READ = 1;
        /// Write access.
        const WRITE = 2;
        /// Create if absent (`OPEN_ALWAYS` / `CREATE_ALWAYS`).
        const CREATE = 4;
        /// Fail if present (`CREATE_NEW`).
        const EXCL = 8;
        /// Truncate on open (`TRUNCATE_EXISTING`).
        const TRUNC = 16;
        /// Append-only writes (`FILE_APPEND_DATA`); the director resolves
        /// the offset.
        const APPEND = 32;
    }
}

bitflags::bitflags! {
    /// The slot header's `flags` word. Only READ has one; directors ignore
    /// the word for every other opcode, and bits other than these for READ,
    /// so a decode drops unknown bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct RequestFlags: u32 {
        /// Prefer a bulk-arena READ reply (data in the shared arena, not the
        /// ring payload).
        const READ_BULK = 0x1;
    }
}

// The numeric API the workspace still uses, kept until its callers move to
// the typed one.

pub const ST_OK: i32 = Status::Ok.code();
pub const ST_NOT_FOUND: i32 = Status::NotFound.code();
pub const ST_NOT_A_DIRECTORY: i32 = Status::NotADirectory.code();
pub const ST_BAD_REQUEST: i32 = Status::BadRequest.code();
pub const ST_IO_ERROR: i32 = Status::IoError.code();
pub const ST_IS_DIR: i32 = Status::IsDir.code();
pub const ST_BAD_FH: i32 = Status::BadFh.code();
pub const ST_NO_SPACE: i32 = Status::NoSpace.code();
pub const ST_NOT_SUPPORTED: i32 = Status::NotSupported.code();
pub const ST_READ_ONLY: i32 = Status::ReadOnly.code();
pub const ST_EXISTS: i32 = Status::Exists.code();
pub const ST_REPLY_TOO_LARGE: i32 = Status::ReplyTooLarge.code();
pub const ST_CROSS_ROOT: i32 = Status::CrossRoot.code();

pub const OPEN_READ: u32 = OpenFlags::READ.bits();
pub const OPEN_WRITE: u32 = OpenFlags::WRITE.bits();
pub const OPEN_CREATE: u32 = OpenFlags::CREATE.bits();
pub const OPEN_EXCL: u32 = OpenFlags::EXCL.bits();
pub const OPEN_TRUNC: u32 = OpenFlags::TRUNC.bits();
pub const OPEN_APPEND: u32 = OpenFlags::APPEND.bits();
