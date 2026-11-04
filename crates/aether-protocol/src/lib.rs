//! The pure wire contract for the VFS stack: the protocol table (opcodes,
//! requests, replies and their codecs), statuses, flags and the root id. No
//! OS I/O. The vocabulary is FUSE-style RPC, but nothing here touches
//! `/dev/fuse`. Registry ops use `aether-registry`'s portable node model.
//!
//! The wire is defined once, in `table.rs`: [`Request`] and [`Reply`] decode
//! borrowing from a payload and encode into any [`Buf`]. The `OP_*`, `ST_*`
//! and `OPEN_*` constants and the `encode_*`/`decode_*` functions below are
//! the older numeric API, kept as thin wrappers over the table until their
//! callers move to the typed one.
#![forbid(unsafe_code)]

mod buf;
mod describe;
mod field;
mod root;
pub mod shimcfg;
mod status;
mod table;
mod wire;

pub use aether_registry::Lookup;
pub use buf::{Buf, Overflow, SliceBuf};
pub use describe::contract_json;
pub use root::RootId;
pub use status::{
    OpenFlags, RequestFlags, Status, OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC,
    OPEN_WRITE, STATUSES, ST_BAD_FH, ST_BAD_REQUEST, ST_CROSS_ROOT, ST_EXISTS, ST_IO_ERROR,
    ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED, ST_NO_SPACE, ST_OK,
    ST_READ_ONLY, ST_REPLY_TOO_LARGE,
};
#[cfg(feature = "testing")]
pub use table::{any_reply_for, any_request, any_request_for};
pub use table::{
    AttrResp, ChangedReply, DirEntries, DirEntry, DirIter, KeyReply, LookupReply, Opcode, OpenResp,
    ReadReply, Reply, Request, OPCODES, READ_RESP_BULK_BIT,
};
pub use wire::DecodeError;

use aether_registry::Node;
use std::borrow::Cow;

/// The ring's wire version, as `contract/protocol.json` records it. It is the
/// ring header's `version` word (`aether_ipc::layout::VERSION`, which this
/// crate cannot name: that crate depends on this one), and
/// `tests/contract.rs` checks the two equal.
pub const WIRE_VERSION: u32 = 4;

// ---------------------------------------------------------------------------
// The numeric API, on top of the table. Every function here encodes or
// decodes through `Request`/`Reply`; none has a layout of its own.
// ---------------------------------------------------------------------------

pub const OP_GETATTR: u32 = Opcode::GetAttr as u32;
pub const OP_READDIR: u32 = Opcode::ReadDir as u32;
pub const OP_OPEN: u32 = Opcode::Open as u32;
pub const OP_READ: u32 = Opcode::Read as u32;
pub const OP_WRITE: u32 = Opcode::Write as u32;
pub const OP_SETATTR: u32 = Opcode::SetAttr as u32;
pub const OP_RENAME: u32 = Opcode::Rename as u32;
pub const OP_DELETE: u32 = Opcode::Delete as u32;
pub const OP_MKDIR: u32 = Opcode::Mkdir as u32;
pub const OP_CLOSE: u32 = Opcode::Close as u32;
pub const OP_HEARTBEAT: u32 = Opcode::Heartbeat as u32;
pub const OP_STORED_NAMES: u32 = Opcode::StoredNames as u32;
pub const OP_REG_LOOKUP: u32 = Opcode::RegLookup as u32;
pub const OP_REG_KEY: u32 = Opcode::RegKey as u32;
pub const OP_REG_SET_VALUE: u32 = Opcode::RegSetValue as u32;
pub const OP_REG_DELETE_VALUE: u32 = Opcode::RegDeleteValue as u32;
pub const OP_REG_CREATE_KEY: u32 = Opcode::RegCreateKey as u32;
pub const OP_REG_DELETE_KEY: u32 = Opcode::RegDeleteKey as u32;
pub const OP_REG_RENAME_KEY: u32 = Opcode::RegRenameKey as u32;
pub const OP_REG_CHANGED: u32 = Opcode::RegChanged as u32;
pub const OP_RENAME_X: u32 = Opcode::RenameX as u32;
pub const OP_STAGE_EXEC: u32 = Opcode::StageExec as u32;

/// Ring/request flag: prefer bulk-arena READ (data in shared arena, not ring payload).
pub const FLAG_READ_BULK: u32 = RequestFlags::READ_BULK.bits();
/// Bit 0 of an OPEN reply's flags byte: [`OpenResp::immutable`].
pub const OPEN_RESP_IMMUTABLE: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryWire {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadReq {
    pub fh: u64,
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReq {
    pub fh: u64,
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetattrReq {
    pub fh: u64,
    pub size: u64,
}

fn req(r: Request<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    r.encode(&mut b);
    b
}

fn rep(r: Reply<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    r.encode(&mut b);
    b
}

fn dreq(op: Opcode, p: &[u8]) -> Option<Request<'_>> {
    Request::decode(op, 0, p).ok()
}

fn drep(op: Opcode, p: &[u8]) -> Option<Reply<'_>> {
    Reply::decode(op, 0, p).ok()
}

/// Path-carrying request: `root:u32 | path_utf8…`, used by GETATTR, READDIR,
/// DELETE and STAGE_EXEC. The root rides ahead of the path so one director
/// can serve several roots with one code path per operation.
pub fn encode_path_req(root: u32, vpath: &str) -> Vec<u8> {
    req(Request::GetAttr {
        root: RootId(root),
        path: vpath,
    })
}

pub fn decode_path_req(payload: &[u8]) -> Option<(u32, String)> {
    let Request::GetAttr { root, path } = dreq(Opcode::GetAttr, payload)? else {
        return None;
    };
    Some((root.0, path.to_string()))
}

pub fn encode_getattr_resp(r: &AttrResp) -> Vec<u8> {
    rep(Reply::GetAttr(r.clone()))
}

pub fn decode_getattr_resp(p: &[u8]) -> Option<AttrResp> {
    let Reply::GetAttr(a) = drep(Opcode::GetAttr, p)? else {
        return None;
    };
    Some(a)
}

pub fn encode_readdir_resp(entries: &[DirEntryWire]) -> Vec<u8> {
    let list: Vec<DirEntry<'_>> = entries
        .iter()
        .map(|e| DirEntry {
            name: &e.name,
            is_dir: e.is_dir,
            size: e.size,
            mtime: e.mtime,
        })
        .collect();
    rep(Reply::ReadDir(DirEntries::new(&list)))
}

pub fn decode_readdir_resp(p: &[u8]) -> Option<Vec<DirEntryWire>> {
    let Reply::ReadDir(list) = drep(Opcode::ReadDir, p)? else {
        return None;
    };
    Some(
        list.iter()
            .map(|e| DirEntryWire {
                name: e.name.to_string(),
                is_dir: e.is_dir,
                size: e.size,
                mtime: e.mtime,
            })
            .collect(),
    )
}

/// OPEN req: `root:u32 | flags:u32 | path_utf8…`
pub fn encode_open_req(root: u32, flags: u32, path: &str) -> Vec<u8> {
    req(Request::Open {
        root: RootId(root),
        flags: OpenFlags::from_bits_retain(flags),
        path,
    })
}

/// Returns `(root, flags, path)`.
pub fn decode_open_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let Request::Open { root, flags, path } = dreq(Opcode::Open, p)? else {
        return None;
    };
    Some((root.0, flags.bits(), path.to_string()))
}

pub fn encode_open_resp(r: &OpenResp) -> Vec<u8> {
    rep(Reply::Open(r.clone()))
}

pub fn decode_open_resp(p: &[u8]) -> Option<OpenResp> {
    let Reply::Open(o) = drep(Opcode::Open, p)? else {
        return None;
    };
    Some(o)
}

/// READ req: `fh:u64 | offset:u64 | len:u32 | pad:u32`
pub fn encode_read_req(r: &ReadReq) -> Vec<u8> {
    req(Request::Read {
        slot_flags: RequestFlags::empty(),
        fh: r.fh,
        offset: r.offset,
        len: r.len,
    })
}

pub fn decode_read_req(p: &[u8]) -> Option<ReadReq> {
    let Request::Read {
        fh, offset, len, ..
    } = dreq(Opcode::Read, p)?
    else {
        return None;
    };
    Some(ReadReq { fh, offset, len })
}

/// READ resp: `bytes_read:u32 | pad:u32 | data[bytes_read]`
pub fn encode_read_resp(data: &[u8]) -> Vec<u8> {
    rep(Reply::Read(ReadReply::Inline(data)))
}

fn read_reply(p: &[u8]) -> Option<ReadReply<'_>> {
    let Reply::Read(r) = drep(Opcode::Read, p)? else {
        return None;
    };
    Some(r)
}

pub fn decode_read_resp(p: &[u8]) -> Option<Vec<u8>> {
    match read_reply(p)? {
        ReadReply::Inline(data) => Some(data.to_vec()),
        ReadReply::Bulk { .. } => None,
    }
}

/// Copy READ response data into `out` without allocating a second Vec.
/// Returns bytes copied (may be less than `out.len()` on short/EOF reads).
/// Inline responses only (not bulk).
pub fn decode_read_resp_into(p: &[u8], out: &mut [u8]) -> Option<usize> {
    let ReadReply::Inline(data) = read_reply(p)? else {
        return None; // use decode_read_bulk_resp + arena
    };
    let n = data.len().min(out.len());
    out[..n].copy_from_slice(&data[..n]);
    Some(n)
}

/// Bulk READ response: `bytes_read|BULK_BIT : u32 | pad:u32 | arena_offset:u64`
pub fn encode_read_resp_bulk(bytes_read: u32, arena_offset: u64) -> Vec<u8> {
    rep(Reply::Read(ReadReply::Bulk {
        len: bytes_read,
        arena_offset,
    }))
}

/// Returns `(bytes_read, arena_offset)` for a bulk response, or `None` if inline/malformed.
pub fn decode_read_bulk_resp(p: &[u8]) -> Option<(u32, u64)> {
    match read_reply(p)? {
        ReadReply::Bulk { len, arena_offset } => Some((len, arena_offset)),
        ReadReply::Inline(_) => None,
    }
}

/// A well-formed bulk READ reply. (A malformed one is `false`, and fails the
/// inline decoders too.)
pub fn is_read_resp_bulk(p: &[u8]) -> bool {
    matches!(read_reply(p), Some(ReadReply::Bulk { .. }))
}

/// WRITE req: `fh:u64 | offset:u64 | len:u32 | pad:u32 | data[len]`; `len`
/// is `data.len()`, whatever `r.len` says.
pub fn encode_write_req(r: &WriteReq, data: &[u8]) -> Vec<u8> {
    req(Request::Write {
        fh: r.fh,
        offset: r.offset,
        data,
    })
}

pub fn decode_write_req(p: &[u8]) -> Option<(WriteReq, Vec<u8>)> {
    let Request::Write { fh, offset, data } = dreq(Opcode::Write, p)? else {
        return None;
    };
    let len = data.len() as u32;
    Some((WriteReq { fh, offset, len }, data.to_vec()))
}

/// WRITE resp: `bytes_written:u32 | pad:u32`
pub fn encode_write_resp(n: u32) -> Vec<u8> {
    rep(Reply::Write(n))
}

pub fn decode_write_resp(p: &[u8]) -> Option<u32> {
    let Reply::Write(n) = drep(Opcode::Write, p)? else {
        return None;
    };
    Some(n)
}

/// MKDIR req: `root:u32 | mode:u32 | path_utf8…`
pub fn encode_mkdir_req(root: u32, mode: u32, path: &str) -> Vec<u8> {
    req(Request::Mkdir {
        root: RootId(root),
        mode,
        path,
    })
}

/// Returns `(root, mode, path)`.
pub fn decode_mkdir_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let Request::Mkdir { root, mode, path } = dreq(Opcode::Mkdir, p)? else {
        return None;
    };
    Some((root.0, mode, path.to_string()))
}

/// STORED_NAMES req: `root:u32 | skip:u32 | path_utf8`. Asks how the
/// components of `path` from the `skip`-th on are spelled where they are
/// stored. The reply is those names joined by `/`, one per component asked
/// about (a name has no `/` in it); a component nothing has comes back as it
/// was sent.
pub fn encode_names_req(root: u32, skip: u32, path: &str) -> Vec<u8> {
    req(Request::StoredNames {
        root: RootId(root),
        skip,
        path,
    })
}

/// Returns `(root, skip, path)`.
pub fn decode_names_req(p: &[u8]) -> Option<(u32, u32, String)> {
    let Request::StoredNames { root, skip, path } = dreq(Opcode::StoredNames, p)? else {
        return None;
    };
    Some((root.0, skip, path.to_string()))
}

/// STORED_NAMES reply: the names joined by `/` (a name has no `/` in it).
pub fn encode_names_resp(names: &[String]) -> Vec<u8> {
    rep(Reply::StoredNames(&names.join("/")))
}

/// The names of a STORED_NAMES reply, or `None` if it is not UTF-8. An empty
/// payload is one empty name, as `str::split` has it.
pub fn decode_names_resp(p: &[u8]) -> Option<Vec<String>> {
    let Reply::StoredNames(text) = drep(Opcode::StoredNames, p)? else {
        return None;
    };
    Some(text.split('/').map(str::to_string).collect())
}

/// RENAME req: `root:u32 | from_len:u32 | from_utf8 | to_utf8`
pub fn encode_rename_req(root: u32, from: &str, to: &str) -> Vec<u8> {
    req(Request::Rename {
        root: RootId(root),
        from,
        to,
    })
}

/// Returns `(root, from, to)`.
pub fn decode_rename_req(p: &[u8]) -> Option<(u32, String, String)> {
    let Request::Rename { root, from, to } = dreq(Opcode::Rename, p)? else {
        return None;
    };
    Some((root.0, from.to_string(), to.to_string()))
}

/// RENAME_X req: `from_root:u32 | to_root:u32 | from_len:u32 | from | to`.
pub fn encode_rename_x_req(from_root: u32, from: &str, to_root: u32, to: &str) -> Vec<u8> {
    req(Request::RenameX {
        from_root: RootId(from_root),
        to_root: RootId(to_root),
        from,
        to,
    })
}

/// Returns `(from_root, from, to_root, to)`.
pub fn decode_rename_x_req(p: &[u8]) -> Option<(u32, String, u32, String)> {
    let Request::RenameX {
        from_root,
        to_root,
        from,
        to,
    } = dreq(Opcode::RenameX, p)?
    else {
        return None;
    };
    Some((from_root.0, from.to_string(), to_root.0, to.to_string()))
}

/// SETATTR req: `fh:u64 | size:u64`
pub fn encode_setattr_req(r: &SetattrReq) -> Vec<u8> {
    req(Request::SetAttr {
        fh: r.fh,
        size: r.size,
    })
}

pub fn decode_setattr_req(p: &[u8]) -> Option<SetattrReq> {
    let Request::SetAttr { fh, size } = dreq(Opcode::SetAttr, p)? else {
        return None;
    };
    Some(SetattrReq { fh, size })
}

pub fn encode_close_req(fh: u64) -> Vec<u8> {
    req(Request::Close { fh })
}

pub fn decode_close_req(p: &[u8]) -> Option<u64> {
    let Request::Close { fh } = dreq(Opcode::Close, p)? else {
        return None;
    };
    Some(fh)
}

// ---------------------------------------------------------------------------
// Registry overlay codecs. Strings are `len:u32 LE | utf8`; booleans are one
// byte, 0 or 1. Every decoder consumes its input exactly and returns `None`
// for anything else (truncation, trailing bytes, bad UTF-8, out-of-range
// flags or lengths); none of them can panic or over-allocate.
// ---------------------------------------------------------------------------

/// A request that is only a path (`REG_KEY`, `REG_LOOKUP`, `REG_DELETE_KEY`).
pub fn encode_reg_path(path: &str) -> Vec<u8> {
    req(Request::RegKey { path })
}

pub fn decode_reg_path(b: &[u8]) -> Option<&str> {
    let Request::RegKey { path } = dreq(Opcode::RegKey, b)? else {
        return None;
    };
    Some(path)
}

/// `REG_SET_VALUE` req: `path | name | ty:u32 | data_len:u32 | data`.
pub fn encode_reg_set_value(path: &str, name: &str, ty: u32, data: &[u8]) -> Vec<u8> {
    req(Request::RegSetValue {
        path,
        name,
        ty,
        data,
    })
}

/// Returns `(path, name, ty, data)`.
pub fn decode_reg_set_value(b: &[u8]) -> Option<(&str, &str, u32, &[u8])> {
    let Request::RegSetValue {
        path,
        name,
        ty,
        data,
    } = dreq(Opcode::RegSetValue, b)?
    else {
        return None;
    };
    Some((path, name, ty, data))
}

/// `REG_DELETE_VALUE` req: `path | name`.
pub fn encode_reg_delete_value(path: &str, name: &str) -> Vec<u8> {
    req(Request::RegDeleteValue { path, name })
}

pub fn decode_reg_delete_value(b: &[u8]) -> Option<(&str, &str)> {
    let Request::RegDeleteValue { path, name } = dreq(Opcode::RegDeleteValue, b)? else {
        return None;
    };
    Some((path, name))
}

/// `REG_CREATE_KEY` req: `path | volatile:u8`.
pub fn encode_reg_create_key(path: &str, volatile: bool) -> Vec<u8> {
    req(Request::RegCreateKey { path, volatile })
}

pub fn decode_reg_create_key(b: &[u8]) -> Option<(&str, bool)> {
    let Request::RegCreateKey { path, volatile } = dreq(Opcode::RegCreateKey, b)? else {
        return None;
    };
    Some((path, volatile))
}

/// `REG_RENAME_KEY` req: `path | new_leaf`.
pub fn encode_reg_rename_key(path: &str, new_leaf: &str) -> Vec<u8> {
    req(Request::RegRenameKey { path, new_leaf })
}

pub fn decode_reg_rename_key(b: &[u8]) -> Option<(&str, &str)> {
    let Request::RegRenameKey { path, new_leaf } = dreq(Opcode::RegRenameKey, b)? else {
        return None;
    };
    Some((path, new_leaf))
}

/// `REG_CHANGED` req: `path | subtree:u8 | since:u64`.
pub fn encode_reg_changed(path: &str, subtree: bool, version: u64) -> Vec<u8> {
    req(Request::RegChanged {
        path,
        subtree,
        since: version,
    })
}

/// Returns `(path, subtree, since)`.
pub fn decode_reg_changed(b: &[u8]) -> Option<(&str, bool, u64)> {
    let Request::RegChanged {
        path,
        subtree,
        since,
    } = dreq(Opcode::RegChanged, b)?
    else {
        return None;
    };
    Some((path, subtree, since))
}

/// `REG_CHANGED` reply: `version:u64 | changed:u8`.
pub fn encode_reg_changed_reply(changed: bool, version: u64) -> Vec<u8> {
    rep(Reply::RegChanged(ChangedReply { version, changed }))
}

/// Returns `(changed, version)`.
pub fn decode_reg_changed_reply(b: &[u8]) -> Option<(bool, u64)> {
    let Reply::RegChanged(c) = drep(Opcode::RegChanged, b)? else {
        return None;
    };
    Some((c.changed, c.version))
}

/// Reply of the mutating ops: just the overlay version after the change.
pub fn encode_reg_version_reply(version: u64) -> Vec<u8> {
    rep(Reply::RegVersion(version))
}

pub fn decode_reg_version_reply(b: &[u8]) -> Option<u64> {
    let Reply::RegVersion(v) = drep(Opcode::RegSetValue, b)? else {
        return None;
    };
    Some(v)
}

/// `REG_LOOKUP` reply: `version:u64 | state:u8 | below:u8`. State is 0 absent,
/// 1 present, 2 present-created, 3 tombstoned; `below` says the overlay has
/// anything underneath the key. A state above 3 is not one and is sent as 3.
pub fn encode_reg_lookup_reply(state: u8, below: bool, version: u64) -> Vec<u8> {
    debug_assert!(state <= 3, "lookup state {state} out of range");
    let state = match state {
        0 => Lookup::Absent,
        1 => Lookup::Present { created: false },
        2 => Lookup::Present { created: true },
        _ => Lookup::Tombstoned,
    };
    rep(Reply::RegLookup(LookupReply {
        version,
        state,
        below,
    }))
}

/// Returns `(state, below, version)`.
pub fn decode_reg_lookup_reply(b: &[u8]) -> Option<(u8, bool, u64)> {
    let Reply::RegLookup(l) = drep(Opcode::RegLookup, b)? else {
        return None;
    };
    let state = match l.state {
        Lookup::Absent => 0,
        Lookup::Present { created: false } => 1,
        Lookup::Present { created: true } => 2,
        Lookup::Tombstoned => 3,
    };
    Some((state, l.below, l.version))
}

/// `REG_KEY` reply: `version:u64 | has_node:u8 | node?` (see [`KeyReply`]).
pub fn encode_reg_key_reply(node: Option<&Node>, version: u64) -> Vec<u8> {
    rep(Reply::RegKey(KeyReply {
        version,
        node: node.map(Cow::Borrowed),
    }))
}

/// Returns `(node, version)`; the node is `None` for a "no node" reply.
pub fn decode_reg_key_reply(b: &[u8]) -> Option<(Option<Node>, u64)> {
    let Reply::RegKey(k) = drep(Opcode::RegKey, b)? else {
        return None;
    };
    Some((k.node.map(Cow::into_owned), k.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resp_roundtrip() {
        let names = vec!["Data".to_string(), "Skyrim.esm".to_string()];
        assert_eq!(encode_names_resp(&names), b"Data/Skyrim.esm");
        assert_eq!(decode_names_resp(&encode_names_resp(&names)), Some(names));
        assert_eq!(decode_names_resp(&[0xff]), None);
    }

    #[test]
    fn open_req_roundtrip() {
        let p = encode_open_req(0, OPEN_READ, "Data/Skyrim.esm");
        let (root, f, path) = decode_open_req(&p).unwrap();
        assert_eq!(root, 0);
        assert_eq!(f, OPEN_READ);
        assert_eq!(path, "Data/Skyrim.esm");
    }

    /// Stage 2b task 5, step 1: the same relative path carried for two
    /// different roots must produce two distinguishable payloads, and each
    /// must decode back to its own root. Without this the shim can classify a
    /// path as belonging to root 1 and has no field in which to say so.
    #[test]
    fn path_carrying_payloads_carry_the_root() {
        let a = encode_path_req(0, "same.txt");
        let b = encode_path_req(1, "same.txt");
        assert_ne!(a, b, "two roots must not encode to identical bytes");
        assert_eq!(decode_path_req(&a), Some((0, "same.txt".to_string())));
        assert_eq!(decode_path_req(&b), Some((1, "same.txt".to_string())));

        let o = encode_open_req(7, OPEN_READ, "same.txt");
        assert_eq!(
            decode_open_req(&o),
            Some((7, OPEN_READ, "same.txt".to_string()))
        );

        let m = encode_mkdir_req(2, 493, "sub/dir");
        assert_eq!(decode_mkdir_req(&m), Some((2, 493, "sub/dir".to_string())));

        let r = encode_rename_req(3, "old.txt", "new.txt");
        assert_eq!(
            decode_rename_req(&r),
            Some((3, "old.txt".to_string(), "new.txt".to_string()))
        );
    }

    /// The exact byte layout, pinned. This is a contract with the injected
    /// DLL: a shim built against the pre-task-5 shape (bare path / bare
    /// `flags|path`) would have its first four path bytes decoded as a root
    /// id. `aether_ipc::layout::VERSION` was bumped to 2 in the same change so
    /// that shim is rejected at ring open rather than silently misparsed —
    /// see `aether_ipc::ring::tests::open_rejects_a_stale_wire_version`.
    #[test]
    fn root_leads_the_wire_layout() {
        assert_eq!(encode_path_req(1, "a"), vec![1, 0, 0, 0, b'a']);
        assert_eq!(
            encode_open_req(1, 2, "a"),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            encode_mkdir_req(1, 2, "a"),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            encode_rename_req(1, "a", "b"),
            vec![1, 0, 0, 0, 1, 0, 0, 0, b'a', b'b']
        );
    }

    #[test]
    fn open_resp_roundtrip() {
        let r = OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
            immutable: true,
            mount_gen: 0xA1B2_C3D4,
        };
        assert_eq!(decode_open_resp(&encode_open_resp(&r)), Some(r));
    }

    /// A reply from a director that predates the flags byte has zeros there:
    /// it must decode as mutable, generation 0 — never as cacheable.
    #[test]
    fn an_open_reply_with_zero_padding_is_mutable() {
        let mut b = Vec::new();
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&9u64.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]);
        let r = decode_open_resp(&b).unwrap();
        assert_eq!((r.fh, r.size, r.immutable, r.mount_gen), (7, 9, false, 0));
    }

    #[test]
    fn read_req_resp_roundtrip() {
        let req = ReadReq {
            fh: 7,
            offset: 10,
            len: 4,
        };
        assert_eq!(decode_read_req(&encode_read_req(&req)), Some(req));
        let data = b"abcd";
        assert_eq!(
            decode_read_resp(&encode_read_resp(data)).as_deref(),
            Some(&data[..])
        );
        let mut out = [0u8; 8];
        assert_eq!(
            decode_read_resp_into(&encode_read_resp(data), &mut out),
            Some(4)
        );
        assert_eq!(&out[..4], b"abcd");
    }

    #[test]
    fn write_req_resp_roundtrip() {
        let req = WriteReq {
            fh: 7,
            offset: 10,
            len: 3,
        };
        let data = b"abc";
        let encoded = encode_write_req(&req, data);
        let (decoded_req, decoded_data) = decode_write_req(&encoded).unwrap();
        assert_eq!(decoded_req, req);
        assert_eq!(decoded_data, data);
        assert_eq!(decode_write_resp(&encode_write_resp(3)), Some(3));
    }

    #[test]
    fn close_req_roundtrip() {
        assert_eq!(decode_close_req(&encode_close_req(99)), Some(99));
    }

    #[test]
    fn mkdir_req_roundtrip() {
        let p = encode_mkdir_req(0, 493, "sub/dir");
        assert_eq!(decode_mkdir_req(&p), Some((0, 493, "sub/dir".to_string())));
    }

    #[test]
    fn rename_req_roundtrip() {
        let p = encode_rename_req(0, "old.txt", "new.txt");
        assert_eq!(
            decode_rename_req(&p),
            Some((0, "old.txt".to_string(), "new.txt".to_string()))
        );
    }

    #[test]
    fn setattr_req_roundtrip() {
        let req = SetattrReq { fh: 5, size: 100 };
        assert_eq!(decode_setattr_req(&encode_setattr_req(&req)), Some(req));
    }

    /// The wire numbers as literals, taken from the retired protocol
    /// descriptor (75db467). A change here is a wire break.
    #[test]
    fn wire_numbers_are_the_historical_ones() {
        let want: &[(&str, u32)] = &[
            ("getattr", 1),
            ("readdir", 2),
            ("open", 3),
            ("read", 5),
            ("write", 6),
            ("setattr", 7),
            ("rename", 8),
            ("delete", 9),
            ("mkdir", 10),
            ("close", 11),
            ("heartbeat", 13),
            ("stored-names", 14),
            ("reg-lookup", 15),
            ("reg-key", 16),
            ("reg-set-value", 17),
            ("reg-delete-value", 18),
            ("reg-create-key", 19),
            ("reg-delete-key", 20),
            ("reg-rename-key", 21),
            ("reg-changed", 22),
            ("rename-x", 23),
            ("stage-exec", 24),
        ];
        assert_eq!(OPCODES, want);
        let sts: &[(&str, i32)] = &[
            ("ok", 0),
            ("not-found", -1),
            ("not-a-directory", -2),
            ("bad-request", -3),
            ("io-error", -4),
            ("is-dir", -5),
            ("bad-fh", -6),
            ("no-space", -7),
            ("not-supported", -8),
            ("read-only", -9),
            ("exists", -10),
            ("reply-too-large", -11),
            ("cross-root", -12),
        ];
        assert_eq!(STATUSES, sts);
        assert_eq!(OPEN_READ, 1);
        assert_eq!(OPEN_WRITE, 2);
        assert_eq!(OPEN_CREATE, 4);
        assert_eq!(OPEN_EXCL, 8);
        assert_eq!(OPEN_TRUNC, 16);
        assert_eq!(OPEN_APPEND, 32);
        assert_eq!(FLAG_READ_BULK, 1);
        assert_eq!(READ_RESP_BULK_BIT, 0x8000_0000);
        assert_eq!(OPEN_RESP_IMMUTABLE, 1);
    }

    #[test]
    fn short_buffers_decode_none() {
        assert!(decode_open_req(&[1, 2]).is_none());
        // A payload carrying only the root and no flags is still short: the
        // pre-task-5 `flags|path` shape decoded a 4-byte prefix, so a bare
        // 4-byte buffer must not now look like a valid OPEN.
        assert!(decode_open_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_path_req(&[1, 2]).is_none());
        assert!(decode_mkdir_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_rename_req(&[0, 0, 0, 0]).is_none());
        assert!(decode_read_req(&[0u8; 10]).is_none());
        assert!(decode_read_resp(&[1, 0, 0]).is_none());
    }

    /// The file-op decoders are deliberately lenient (unlike the registry
    /// ones): trailing bytes are ignored, any non-zero byte is true, and a
    /// READ request may omit its trailing pad. Pinned so the shared cursor
    /// cannot tighten them by accident.
    #[test]
    fn file_op_decoders_stay_lenient() {
        let mut g = encode_getattr_resp(&AttrResp {
            found: true,
            is_dir: true,
            size: 5,
            mtime: 6,
        });
        g[0] = 7;
        g[1] = 0xff;
        g.extend_from_slice(&[9, 9]);
        let a = decode_getattr_resp(&g).unwrap();
        assert!(a.found && a.is_dir);
        assert_eq!((a.size, a.mtime), (5, 6));

        let mut c = encode_close_req(3);
        c.push(0);
        assert_eq!(decode_close_req(&c), Some(3));
        let mut s = encode_setattr_req(&SetattrReq { fh: 1, size: 2 });
        s.push(0);
        assert_eq!(decode_setattr_req(&s), Some(SetattrReq { fh: 1, size: 2 }));
        let req = ReadReq {
            fh: 1,
            offset: 2,
            len: 3,
        };
        let r = encode_read_req(&req);
        assert_eq!(decode_read_req(&r[..20]), Some(req));
        assert_eq!(decode_write_resp(&encode_write_resp(4)[..4]), Some(4));
        let mut rr = encode_read_resp(b"xy");
        rr.push(0);
        assert_eq!(decode_read_resp(&rr).as_deref(), Some(&b"xy"[..]));
        let mut o = encode_open_resp(&OpenResp::default());
        o[16] = 2;
        o[18] = 0xaa;
        o.push(0);
        assert!(decode_open_resp(&o).unwrap().is_dir);
        let mut d = encode_readdir_resp(&[DirEntryWire {
            name: "a".into(),
            is_dir: true,
            size: 1,
            mtime: 2,
        }]);
        d.push(0);
        assert_eq!(decode_readdir_resp(&d).unwrap().len(), 1);
        let mut w = encode_write_req(
            &WriteReq {
                fh: 1,
                offset: 2,
                len: 2,
            },
            b"hi",
        );
        w.extend_from_slice(&[7, 7]);
        let (wr, data) = decode_write_req(&w).unwrap();
        assert_eq!((wr.len, data), (2, b"hi".to_vec()));
        let mut rq = encode_read_req(&req);
        rq.push(9);
        assert_eq!(decode_read_req(&rq), Some(req));
        // bulk read reply: trailing bytes ignored, inline decoders refuse it
        let mut bulk = encode_read_resp_bulk(5, 65536);
        bulk.push(1);
        assert_eq!(decode_read_bulk_resp(&bulk), Some((5, 65536)));
        assert!(is_read_resp_bulk(&bulk));
        assert!(decode_read_resp_into(&bulk, &mut [0u8; 8]).is_none());
        assert!(decode_read_bulk_resp(&encode_read_resp(b"ab")).is_none());
        // readdir is_dir: any non-zero byte is true
        let mut dd = encode_readdir_resp(&[DirEntryWire {
            name: "a".into(),
            is_dir: false,
            size: 1,
            mtime: 2,
        }]);
        let at = 4 + 4 + 1;
        dd[at] = 2;
        assert!(decode_readdir_resp(&dd).unwrap()[0].is_dir);
        // a READ reply whose data is cut short is still malformed
        assert!(decode_read_resp(&encode_read_resp(b"abc")[..10]).is_none());
        assert!(decode_write_req(
            &encode_write_req(
                &WriteReq {
                    fh: 1,
                    offset: 0,
                    len: 3
                },
                b"abc"
            )[..25]
        )
        .is_none());
    }

    #[test]
    fn getattr_resp_roundtrip() {
        let r = AttrResp {
            found: true,
            is_dir: false,
            size: 123,
            mtime: -7,
        };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&r)), Some(r));
    }

    #[test]
    fn readdir_resp_roundtrip() {
        let entries = vec![
            DirEntryWire {
                name: "a.esp".into(),
                is_dir: false,
                size: 10,
                mtime: 1,
            },
            DirEntryWire {
                name: "sub".into(),
                is_dir: true,
                size: 0,
                mtime: 0,
            },
        ];
        assert_eq!(
            decode_readdir_resp(&encode_readdir_resp(&entries)),
            Some(entries)
        );
    }

    // ---- registry ops (15-22) ----

    fn sample_node() -> aether_registry::Node {
        use aether_registry::{Child, Node, Value};
        let mut n = Node {
            values: vec![
                Value {
                    name: "Path".into(),
                    ty: 1,
                    data: vec![1, 2, 3],
                },
                Value {
                    name: String::new(),
                    ty: 4,
                    data: vec![],
                },
            ],
            value_tombstones: vec!["gone".into()],
            created: true,
            volatile: true,
            last_write: 0x0123_4567_89ab_cdef,
            ..Node::default()
        };
        n.children
            .insert("sub".into(), ("Sub".into(), Child::Present));
        n.children
            .insert("dead".into(), ("Dead".into(), Child::Tombstone));
        n
    }

    #[test]
    fn reg_opcodes_are_15_to_22() {
        assert_eq!(
            [
                OP_REG_LOOKUP,
                OP_REG_KEY,
                OP_REG_SET_VALUE,
                OP_REG_DELETE_VALUE,
                OP_REG_CREATE_KEY,
                OP_REG_DELETE_KEY,
                OP_REG_RENAME_KEY,
                OP_REG_CHANGED
            ],
            [15, 16, 17, 18, 19, 20, 21, 22]
        );
    }

    #[test]
    fn reg_path_roundtrip_and_malformed() {
        let p = encode_reg_path("\\Registry\\Machine\\Sóftware");
        assert_eq!(decode_reg_path(&p), Some("\\Registry\\Machine\\Sóftware"));
        assert_eq!(decode_reg_path(&encode_reg_path("")), Some(""));
        assert_eq!(decode_reg_path(&[]), None);
        assert_eq!(decode_reg_path(&[1, 0, 0]), None);
        assert_eq!(
            decode_reg_path(&[5, 0, 0, 0, b'a']),
            None,
            "length past end"
        );
        let mut t = encode_reg_path("a");
        t.push(0);
        assert_eq!(decode_reg_path(&t), None, "trailing bytes");
        assert_eq!(decode_reg_path(&[1, 0, 0, 0, 0xff]), None, "bad utf8");
        assert_eq!(
            decode_reg_path(&[0xff, 0xff, 0xff, 0xff]),
            None,
            "huge length"
        );
    }

    #[test]
    fn reg_set_value_roundtrip_and_malformed() {
        let b = encode_reg_set_value("\\Registry\\A", "Name", 3, &[9, 8, 7]);
        assert_eq!(
            decode_reg_set_value(&b),
            Some(("\\Registry\\A", "Name", 3, &[9u8, 8, 7][..]))
        );
        let e = encode_reg_set_value("p", "", 0, &[]);
        assert_eq!(decode_reg_set_value(&e), Some(("p", "", 0, &[][..])));
        for n in 0..b.len() {
            assert_eq!(decode_reg_set_value(&b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(decode_reg_set_value(&t), None);
        // data length larger than the registry allows
        let mut big = encode_reg_set_value("p", "n", 1, &[]);
        let at = big.len() - 4;
        big[at..].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
        assert_eq!(decode_reg_set_value(&big), None);
    }

    #[test]
    fn reg_delete_value_and_rename_roundtrip() {
        let b = encode_reg_delete_value("p", "v");
        assert_eq!(decode_reg_delete_value(&b), Some(("p", "v")));
        assert_eq!(decode_reg_delete_value(&b[..b.len() - 1]), None);
        let r = encode_reg_rename_key("\\Registry\\A", "B");
        assert_eq!(decode_reg_rename_key(&r), Some(("\\Registry\\A", "B")));
        assert_eq!(decode_reg_rename_key(&r[..3]), None);
        let mut t = r.clone();
        t.push(1);
        assert_eq!(decode_reg_rename_key(&t), None);
    }

    #[test]
    fn reg_create_key_roundtrip_and_malformed() {
        for v in [false, true] {
            let b = encode_reg_create_key("p", v);
            assert_eq!(decode_reg_create_key(&b), Some(("p", v)));
        }
        let mut b = encode_reg_create_key("p", true);
        *b.last_mut().unwrap() = 2;
        assert_eq!(decode_reg_create_key(&b), None, "flag must be 0 or 1");
        assert_eq!(decode_reg_create_key(&b[..b.len() - 1]), None);
    }

    #[test]
    fn reg_changed_roundtrip_and_malformed() {
        let b = encode_reg_changed("p", true, 77);
        assert_eq!(decode_reg_changed(&b), Some(("p", true, 77)));
        let b = encode_reg_changed("p", false, u64::MAX);
        assert_eq!(decode_reg_changed(&b), Some(("p", false, u64::MAX)));
        for n in 0..b.len() {
            assert_eq!(decode_reg_changed(&b[..n]), None);
        }
        let r = encode_reg_changed_reply(true, 5);
        assert_eq!(decode_reg_changed_reply(&r), Some((true, 5)));
        assert_eq!(decode_reg_changed_reply(&r[..r.len() - 1]), None);
        let mut bad = r.clone();
        bad[8] = 7;
        assert_eq!(decode_reg_changed_reply(&bad), None);
    }

    #[test]
    fn reg_version_and_lookup_reply_roundtrip() {
        assert_eq!(
            decode_reg_version_reply(&encode_reg_version_reply(9)),
            Some(9)
        );
        assert_eq!(decode_reg_version_reply(&[0; 7]), None);
        assert_eq!(decode_reg_version_reply(&[0; 9]), None);
        for state in 0..=3u8 {
            for below in [false, true] {
                let b = encode_reg_lookup_reply(state, below, 42);
                assert_eq!(decode_reg_lookup_reply(&b), Some((state, below, 42)));
            }
        }
        let mut b = encode_reg_lookup_reply(1, false, 1);
        b[8] = 4;
        assert_eq!(decode_reg_lookup_reply(&b), None, "state out of range");
        let mut b = encode_reg_lookup_reply(1, false, 1);
        b[9] = 2;
        assert_eq!(decode_reg_lookup_reply(&b), None, "bool out of range");
        assert_eq!(decode_reg_lookup_reply(&b[..9]), None);
    }

    #[test]
    fn reg_key_reply_roundtrip() {
        let n = sample_node();
        let b = encode_reg_key_reply(Some(&n), 12);
        assert_eq!(decode_reg_key_reply(&b), Some((Some(n), 12)));
        let none = encode_reg_key_reply(None, 13);
        assert_eq!(decode_reg_key_reply(&none), Some((None, 13)));
        let empty = aether_registry::Node::default();
        let b = encode_reg_key_reply(Some(&empty), 0);
        assert_eq!(decode_reg_key_reply(&b), Some((Some(empty), 0)));
    }

    #[test]
    fn reg_key_reply_malformed_is_none_never_panics() {
        let b = encode_reg_key_reply(Some(&sample_node()), 12);
        for n in 0..b.len() {
            assert_eq!(decode_reg_key_reply(&b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(decode_reg_key_reply(&t), None, "trailing bytes");
        let mut bad_tag = b.clone();
        bad_tag[8] = 9;
        assert_eq!(decode_reg_key_reply(&bad_tag), None);
        // a "no node" reply with trailing bytes
        let mut none = encode_reg_key_reply(None, 1);
        none.push(0);
        assert_eq!(decode_reg_key_reply(&none), None);
        // every single-byte corruption and a huge count must not panic
        for i in 0..b.len() {
            let mut c = b.clone();
            c[i] = 0xff;
            let _ = decode_reg_key_reply(&c);
        }
        let mut huge = encode_reg_key_reply(Some(&aether_registry::Node::default()), 0);
        // values count sits after tag, created, volatile, last_write
        let at = 8 + 1 + 1 + 1 + 8;
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_reg_key_reply(&huge), None);
    }

    #[test]
    fn reg_key_reply_enforces_registry_limits() {
        use aether_registry::{Child, Node, Value};
        let long_value = Node {
            values: vec![Value {
                name: "x".repeat(16384),
                ty: 1,
                data: vec![],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&long_value), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "value name over limit");
        let mut long_child = Node::default();
        long_child
            .children
            .insert("k".into(), ("y".repeat(256), Child::Present));
        let b = encode_reg_key_reply(Some(&long_child), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "child name over limit");
        let ok_value = Node {
            values: vec![Value {
                name: "x".repeat(16383),
                ty: 1,
                data: vec![0; 1 << 20],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&ok_value), 1);
        assert!(decode_reg_key_reply(&b).is_some());
        let too_big = Node {
            values: vec![Value {
                name: "a".into(),
                ty: 1,
                data: vec![0; (1 << 20) + 1],
            }],
            ..Node::default()
        };
        let b = encode_reg_key_reply(Some(&too_big), 1);
        assert_eq!(decode_reg_key_reply(&b), None, "data over limit");
    }
}
