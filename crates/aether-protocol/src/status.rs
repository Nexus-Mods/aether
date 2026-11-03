//! Status codes a reply carries, and open-request flags.
//!
//! Values `0` through `-11` are fixed by the ring protocol — and by the
//! injected shim DLL, which matches statuses by number across the process
//! boundary — and must not be renumbered. New statuses append at the next
//! free negative number instead. Statuses stay plain `i32`, so a director
//! written outside this workspace needs no wrapper type.

pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;
pub const ST_IO_ERROR: i32 = -4;
pub const ST_IS_DIR: i32 = -5;
pub const ST_BAD_FH: i32 = -6;
pub const ST_NO_SPACE: i32 = -7;
/// The director does not implement this method.
pub const ST_NOT_SUPPORTED: i32 = -8;
/// No writable layer serves this path.
pub const ST_READ_ONLY: i32 = -9;
/// `OPEN_EXCL` (create-new) refused because the path already exists.
pub const ST_EXISTS: i32 = -10;
/// The answer does not fit an inline ring reply (`payload_cap - 8` bytes).
/// Used by the registry `REG_KEY` op for a key whose overlay data exceeds the
/// ring's payload; the shim treats it like a failed director read and serves
/// the real key.
pub const ST_REPLY_TOO_LARGE: i32 = -11;
/// A move between two roots of something the director does not copy across
/// (a directory). The shim answers `STATUS_NOT_SAME_DEVICE`, what Windows
/// says for a move between volumes, so a caller falls back to copy and delete.
pub const ST_CROSS_ROOT: i32 = -12;

/// Open wants read access.
pub const OPEN_READ: u32 = 1;
/// Open wants write access.
pub const OPEN_WRITE: u32 = 2;
/// Create if absent (`OPEN_ALWAYS` / `CREATE_ALWAYS`).
pub const OPEN_CREATE: u32 = 4;
/// Fail if present (`CREATE_NEW`).
pub const OPEN_EXCL: u32 = 8;
/// Truncate on open (`TRUNCATE_EXISTING`).
pub const OPEN_TRUNC: u32 = 16;
/// Append-only writes (`FILE_APPEND_DATA`); the director resolves the offset.
pub const OPEN_APPEND: u32 = 32;
