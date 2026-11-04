//! The serving end: a [`Director`] answers typed requests taken off a ring.
//!
//! Each method's return type is the reply shape of its opcode, so a director
//! cannot answer a request with another opcode's reply: `get_attr` can only
//! give an [`AttrResp`], the five registry writes a version, and the requests
//! whose reply is empty a `()`. Which [`Reply`] variant carries each answer
//! is decided once, in the match in `answer`, and checked against the table's
//! own decoder by the round-trip tests.

use aether_ipc::{DataArena, IpcError, Notifier, RequestView, RingServer};
use aether_protocol::{
    AttrResp, ChangedReply, DirEntries, DirEntry, KeyReply, LookupReply, Opcode, OpenFlags,
    OpenResp, ReadReply, Reply, Request, RequestFlags, RootId, Status,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// A server's answers, one method per opcode. An `Err` is sent as that
/// status with an empty payload; `Err(Status::Ok)` is a director bug and is
/// sent as `Status::IoError`. Replies that carry text or bytes borrow from
/// the director.
pub trait Director {
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status>;
    /// The entries of the directory `path`, pushed onto `out` (empty when
    /// called).
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status>;
    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status>;
    /// `at` is the ring slot the request came in on and, when the server was
    /// given one ([`serve_with_arena`]), the bulk arena: a bulk reply's data
    /// goes in that slot's bank (`docs/wire.md` 2.6).
    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status>;
    /// The number of bytes written.
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status>;
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status>;
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status>;
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status>;
    fn mkdir(&mut self, root: RootId, mode: u32, path: &str) -> Result<(), Status>;
    fn close(&mut self, fh: u64) -> Result<(), Status>;
    fn heartbeat(&mut self) -> Result<(), Status>;
    /// The stored spellings of `path`'s components from the `skip`-th on,
    /// joined by `/`.
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status>;
    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status>;
    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status>;
    /// The registry writes answer the overlay's version after the write.
    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status>;
    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status>;
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status>;
    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status>;
    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status>;
    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status>;
    fn rename_x(
        &mut self,
        from_root: RootId,
        to_root: RootId,
        from: &str,
        to: &str,
    ) -> Result<(), Status>;
    /// The real path the program at `path` was staged to.
    fn stage_exec(&mut self, root: RootId, path: &str) -> Result<&str, Status>;

    /// The status for an opcode number the table does not have (0, the
    /// reserved 4 and 12, 25 and up). Sent with an empty payload.
    fn unknown(&mut self, opcode: u32) -> Status {
        let _ = opcode;
        Status::BadRequest
    }

    /// The status for a payload that does not decode as `op`'s request
    /// (`flags` are the slot flags it came with). Sent with an empty payload,
    /// `Status::Ok` included.
    fn undecodable(&mut self, op: Opcode, flags: u32, payload: &[u8]) -> Status {
        let _ = (op, flags, payload);
        Status::BadRequest
    }
}

/// Where a READ came in: its ring slot and the server's bulk arena, if it
/// has one. A director that answers in bulk fills the slot's bank
/// ([`DataArena::fill_bank`] with `slot`) and replies with the offset it
/// gives.
#[derive(Clone, Copy)]
pub struct ReadSlot<'s> {
    pub slot: u32,
    pub arena: Option<&'s DataArena<'s>>,
}

/// A READDIR reply being built: [`Director::read_dir`] pushes entries, and
/// the names are copied in, so they need not outlive the call. Kept in a
/// [`Scratch`] and reused.
#[derive(Debug, Default)]
pub struct Listing {
    names: String,
    rows: Vec<Row>,
}

#[derive(Debug)]
struct Row {
    name_end: usize,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

impl Listing {
    /// The entries pushed so far, in order.
    pub fn iter(&self) -> impl Iterator<Item = DirEntry<'_>> {
        let mut start = 0;
        self.rows.iter().map(move |r| {
            let name = &self.names[start..r.name_end];
            start = r.name_end;
            DirEntry {
                name,
                is_dir: r.is_dir,
                size: r.size,
                mtime: r.mtime,
            }
        })
    }

    pub fn push(&mut self, entry: DirEntry<'_>) {
        self.names.push_str(entry.name);
        self.rows.push(Row {
            name_end: self.names.len(),
            is_dir: entry.is_dir,
            size: entry.size,
            mtime: entry.mtime,
        });
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn clear(&mut self) {
        self.names.clear();
        self.rows.clear();
    }

    /// Appends the READDIR reply to `out`.
    fn encode(&self, out: &mut Vec<u8>) {
        // The table encodes a listing from a slice of entries that borrow
        // their names; one is built per reply (the server's only allocation
        // beyond the ring's, once the scratch buffers are warm).
        let entries: Vec<DirEntry<'_>> = self.iter().collect();
        Reply::ReadDir(DirEntries::new(&entries)).encode(out);
    }
}

/// The buffers [`serve_one_with`] reuses from one request to the next: the
/// request payload, the reply payload and the READDIR listing.
#[derive(Debug, Default)]
pub struct Scratch {
    req: Vec<u8>,
    reply: Vec<u8>,
    listing: Listing,
}

/// Answers at most one request on `server` from `d`. `Ok(false)` when none
/// was waiting. Allocates its buffers afresh; a loop uses [`serve_one_with`]
/// or [`serve`].
pub fn serve_one<N: Notifier, D: Director + ?Sized>(
    server: &RingServer<'_, N>,
    d: &mut D,
) -> Result<bool, IpcError> {
    serve_one_with(server, d, &mut Scratch::default())
}

/// [`serve_one`] with its buffers in `scratch`.
///
/// A reserved or unknown opcode is answered by [`Director::unknown`]; a
/// payload that does not decode as its opcode's request, by
/// [`Director::undecodable`] (both `Status::BadRequest` unless the director
/// says otherwise). Every status but `Status::Ok` goes with an empty payload.
///
/// A registry reply longer than `payload_cap - 8` is answered
/// `Status::ReplyTooLarge` (`docs/wire.md` 4.3: required for REG_KEY, and
/// applied to every registry reply as Cairn does). Any other reply longer
/// than `payload_cap` is left to the ring, which answers it with its
/// overflow status `i32::MIN` and no payload (2.5, step 3), as both
/// directors do for an oversized READDIR listing (3.4.2).
///
/// Every slot taken is completed ([`RingServer::serve_one_into`]): a header
/// claiming a payload past the slot is `Status::BadRequest`, and a director
/// method that panics is `Status::IoError`.
pub fn serve_one_with<N: Notifier, D: Director + ?Sized>(
    server: &RingServer<'_, N>,
    d: &mut D,
    scratch: &mut Scratch,
) -> Result<bool, IpcError> {
    serve_one_with_arena(server, None, d, scratch)
}

/// [`serve_one_with`] for a server with a bulk arena, which
/// [`Director::read`] is handed with the request's slot.
pub fn serve_one_with_arena<N: Notifier, D: Director + ?Sized>(
    server: &RingServer<'_, N>,
    arena: Option<&DataArena<'_>>,
    d: &mut D,
    scratch: &mut Scratch,
) -> Result<bool, IpcError> {
    let registry_cap = (server.geom().payload_cap as usize).saturating_sub(8);
    let Scratch {
        req,
        reply,
        listing,
    } = scratch;
    server.serve_one_into(req, reply, |view, out| {
        let status = dispatch(d, view, arena, registry_cap, listing, out);
        if status != Status::Ok {
            out.clear();
        }
        status.code()
    })
}

/// Answers requests on `server` from `d` until `stop` is set. Returns the
/// first ring error.
pub fn serve<N: Notifier, D: Director + ?Sized>(
    server: &RingServer<'_, N>,
    d: &mut D,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    serve_with_arena(server, None, d, stop)
}

/// [`serve`] for a server with a bulk arena ([`serve_one_with_arena`]).
pub fn serve_with_arena<N: Notifier, D: Director + ?Sized>(
    server: &RingServer<'_, N>,
    arena: Option<&DataArena<'_>>,
    d: &mut D,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    let mut scratch = Scratch::default();
    while !stop.load(Ordering::Acquire) {
        serve_one_with_arena(server, arena, d, &mut scratch)?;
    }
    Ok(())
}

/// The status for one request, its reply payload (on `Status::Ok`) in `out`.
/// `registry_cap` is the longest registry reply sent (`payload_cap - 8`).
fn dispatch<D: Director + ?Sized>(
    d: &mut D,
    view: &RequestView<'_>,
    arena: Option<&DataArena<'_>>,
    registry_cap: usize,
    listing: &mut Listing,
    out: &mut Vec<u8>,
) -> Status {
    let Ok(op) = Opcode::try_from(view.opcode) else {
        return d.unknown(view.opcode);
    };
    let Ok(req) = Request::decode(op, view.flags, view.payload) else {
        return d.undecodable(op, view.flags, view.payload);
    };
    let at = ReadSlot {
        slot: view.slot,
        arena,
    };
    match answer(d, at, req, listing, out) {
        Ok(()) if is_registry(op) && out.len() > registry_cap => Status::ReplyTooLarge,
        Ok(()) => Status::Ok,
        Err(Status::Ok) => Status::IoError,
        Err(s) => s,
    }
}

/// The registry opcodes (`docs/wire.md` section 4), whose replies have the
/// `payload_cap - 8` limit.
fn is_registry(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::RegLookup
            | Opcode::RegKey
            | Opcode::RegSetValue
            | Opcode::RegDeleteValue
            | Opcode::RegCreateKey
            | Opcode::RegDeleteKey
            | Opcode::RegRenameKey
            | Opcode::RegChanged
    )
}

/// Asks `d` for `req`'s answer and encodes it into `out` as the reply
/// variant of its opcode.
fn answer<D: Director + ?Sized>(
    d: &mut D,
    at: ReadSlot<'_>,
    req: Request<'_>,
    listing: &mut Listing,
    out: &mut Vec<u8>,
) -> Result<(), Status> {
    let reply = match req {
        Request::GetAttr { root, path } => Reply::GetAttr(d.get_attr(root, path)?),
        Request::ReadDir { root, path } => {
            listing.clear();
            d.read_dir(root, path, listing)?;
            listing.encode(out);
            return Ok(());
        }
        Request::Open { root, flags, path } => Reply::Open(d.open(root, flags, path)?),
        Request::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => Reply::Read(d.read(at, slot_flags, fh, offset, len)?),
        Request::Write { fh, offset, data } => Reply::Write(d.write(fh, offset, data)?),
        Request::SetAttr { fh, size } => empty(d.set_attr(fh, size))?,
        Request::Rename { root, from, to } => empty(d.rename(root, from, to))?,
        Request::Delete { root, path } => empty(d.delete(root, path))?,
        Request::Mkdir { root, mode, path } => empty(d.mkdir(root, mode, path))?,
        Request::Close { fh } => empty(d.close(fh))?,
        Request::Heartbeat {} => empty(d.heartbeat())?,
        Request::StoredNames { root, skip, path } => {
            Reply::StoredNames(d.stored_names(root, skip, path)?)
        }
        Request::RegLookup { path } => Reply::RegLookup(d.reg_lookup(path)?),
        Request::RegKey { path } => Reply::RegKey(d.reg_key(path)?),
        Request::RegSetValue {
            path,
            name,
            ty,
            data,
        } => Reply::RegVersion(d.reg_set_value(path, name, ty, data)?),
        Request::RegDeleteValue { path, name } => {
            Reply::RegVersion(d.reg_delete_value(path, name)?)
        }
        Request::RegCreateKey { path, volatile } => {
            Reply::RegVersion(d.reg_create_key(path, volatile)?)
        }
        Request::RegDeleteKey { path } => Reply::RegVersion(d.reg_delete_key(path)?),
        Request::RegRenameKey { path, new_leaf } => {
            Reply::RegVersion(d.reg_rename_key(path, new_leaf)?)
        }
        Request::RegChanged {
            path,
            subtree,
            since,
        } => Reply::RegChanged(d.reg_changed(path, subtree, since)?),
        Request::RenameX {
            from_root,
            to_root,
            from,
            to,
        } => empty(d.rename_x(from_root, to_root, from, to))?,
        Request::StageExec { root, path } => Reply::StageExec(d.stage_exec(root, path)?),
    };
    reply.encode(out);
    Ok(())
}

fn empty(r: Result<(), Status>) -> Result<Reply<'static>, Status> {
    r.map(|()| Reply::Empty)
}
