//! The reference director: a provider graph and a registry overlay in memory.
//!
//! Every byte under a managed root comes from the director, so a test that
//! wants to know where the bytes came from has to put them somewhere only the
//! director can see. Files cannot do that: the claim under test is that
//! content on the real filesystem under a managed root is unreachable. So the
//! shim's tests serve a real ring from this table, and the shim's own client
//! talks to it exactly as it does in production; only the far side of the
//! ring is in memory.
//!
//! **What it answers.** HEARTBEAT, GETATTR, READDIR, OPEN, READ, WRITE,
//! SETATTR (truncate), DELETE, RENAME and CLOSE, and the registry opcodes when
//! it has a registry layer ([`Reference::with_registry`]). MKDIR,
//! STORED_NAMES, RENAME_X, STAGE_EXEC and every number outside the table are
//! `Status::NotSupported`, so an unexpected opcode never looks like a healthy
//! exchange. A test controls how a READ is answered ([`ReadStyle`]), because
//! a read loop's correctness is mostly about awkward answers: a short read
//! that is not EOF, a read that fails part-way, a file shorter than OPEN
//! said.
//!
//! **The write half** says the four things a real provider graph says to a
//! write open: here you are (inside a [`Reference::writable_under`] prefix),
//! no such place (`Status::NotFound`), not allowed (`Status::ReadOnly`, served
//! but outside every writable prefix) and already there (`Status::Exists` for
//! `OpenFlags::EXCL`). DELETE and RENAME refuse the same way, so a test of a
//! sealed path cannot pass because the director said yes to everything.
//!
//! **Roots.** Root 0's files are keyed by their bare vpath; any other root's
//! by `@<id>/<vpath>` ([`key`]). A request that lost its root then finds the
//! wrong file.
//!
//! **The registry half** keeps one session's overlay as `aether_registry`'s
//! own `Overlay`, so what is stored, merged and versioned is that crate's
//! code; this file adds a director's rules around it (`docs/wire.md` section
//! 4): the path check, the status for each overlay error, and the registry
//! generation, moved and published before a write's reply leaves. The
//! `payload_cap - 8` limit on a registry reply is applied by
//! `aether_rpc::serve_one_with`, which knows the slot size.
//!
//! **Both READ transports.** A READ is answered inline or in bulk (the data
//! in the slot's arena bank, the reply carrying length and offset), chosen as
//! a director chooses: `RequestFlags::READ_BULK`, or a request of at least
//! [`BULK_THRESHOLD`] bytes, when the server has an arena
//! ([`aether_rpc::serve_with_arena`]). A real copy-up of a large file takes
//! the bulk path, so a fixture with no arena tests the transport production
//! does not use.
//!
//! **Handles.** A [`Reference`] is one handle on a graph; [`Reference::share`]
//! makes another (the server thread's, say) with the same files, handles,
//! registry and [`Tally`]. Each handle has its own reply buffer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_protocol::{
    AttrResp, ChangedReply, DirEntry, KeyReply, LookupReply, Opcode, OpenFlags, OpenResp,
    ReadReply, RequestFlags, RootId, Status,
};
use aether_registry::{path as regpath, Overlay, RegError};
use aether_rpc::{Director, Listing, ReadSlot};

/// Requests of at least this many bytes go bulk, matching a director's and
/// the shim's `read_fragmented`'s own constant. Below it a READ stays inline
/// whatever the arena is, which is how one ring covers both transports.
pub const BULK_THRESHOLD: u32 = 64 * 1024;

/// How the reference answers READ for one file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadStyle {
    /// Serve as much as was asked for, up to EOF: an ordinary provider.
    Whole,
    /// Never serve more than `n` bytes per READ, however much was asked for
    /// and however far from EOF. A real provider does this whenever its own
    /// backing read comes back partial; it is not EOF, and a reader that
    /// treats it as EOF truncates.
    Short(usize),
    /// OPEN reports the real size; every READ fails with `Status::IoError`.
    Error,
    /// OPEN reports the real size but the file stops at `n` bytes: the
    /// director having less than it said it had.
    ShorterThanClaimed(u64),
}

struct Entry {
    bytes: Vec<u8>,
    style: ReadStyle,
}

/// Per-vpath request tallies. Keyed by vpath rather than kept as running
/// totals because the tests in a binary run in parallel against one
/// director: a global counter can only be asserted on as "at least". A vpath
/// used by one test gives that test an exact count.
#[derive(Default)]
pub struct Tally {
    opened: Mutex<HashMap<String, u64>>,
    closed: Mutex<HashMap<String, u64>>,
    reads: Mutex<HashMap<String, u64>>,
    bulk_reads: Mutex<HashMap<String, u64>>,
    writes: Mutex<HashMap<String, u64>>,
    deletes: Mutex<HashMap<String, u64>>,
    renames: Mutex<HashMap<String, u64>>,
    /// Registry requests, keyed by opcode and path as sent.
    reg: Mutex<HashMap<(Opcode, String), u64>>,
    /// Every OPEN's `(flags, vpath)` exactly as it crossed the ring, before
    /// the table folds it ([`key`]).
    wire_opens: Mutex<Vec<(u32, String)>>,
}

impl Tally {
    fn bump(m: &Mutex<HashMap<String, u64>>, vpath: &str) {
        *m.lock().unwrap().entry(vpath.to_string()).or_insert(0) += 1;
    }

    /// A file tally, keyed by the folded vpath as the table is ([`key`]).
    fn get_file(m: &Mutex<HashMap<String, u64>>, vpath: &str) -> u64 {
        m.lock()
            .unwrap()
            .get(&vpath.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
    }

    /// OPENs that issued a handle (a refused OPEN issues none).
    pub fn opens(&self, vpath: &str) -> u64 {
        Self::get_file(&self.opened, vpath)
    }

    /// CLOSEs of a handle this graph issued.
    pub fn closes(&self, vpath: &str) -> u64 {
        Self::get_file(&self.closed, vpath)
    }

    /// READ requests that reached the server for this file: the ring round
    /// trips a copy-up of it cost.
    pub fn reads(&self, vpath: &str) -> u64 {
        Self::get_file(&self.reads, vpath)
    }

    /// Of those, the ones answered through the arena rather than inline:
    /// whether a test covered the transport a real copy-up of a large file
    /// uses.
    pub fn bulk_reads(&self, vpath: &str) -> u64 {
        Self::get_file(&self.bulk_reads, vpath)
    }

    /// WRITE requests on a known handle that reached the server for this
    /// file. Zero here with bytes on disk somewhere means the write never
    /// crossed the ring.
    pub fn writes(&self, vpath: &str) -> u64 {
        Self::get_file(&self.writes, vpath)
    }

    /// DELETE requests that reached the server for this vpath, refused ones
    /// included. Zero here with the real file gone means the delete went to
    /// the filesystem.
    pub fn deletes(&self, vpath: &str) -> u64 {
        Self::get_file(&self.deletes, vpath)
    }

    /// RENAME requests that reached the server, counted against the source
    /// vpath.
    pub fn renames(&self, vpath: &str) -> u64 {
        Self::get_file(&self.renames, vpath)
    }

    /// `(flags, vpath)` of every OPEN so far, as sent.
    pub fn wire_opens(&self) -> Vec<(u32, String)> {
        self.wire_opens.lock().unwrap().clone()
    }

    /// Registry requests with `op` for `path` (as sent) that reached the
    /// server with a registry layer: zero for a read means it was answered
    /// from the shim's cache.
    pub fn reg(&self, op: Opcode, path: &str) -> u64 {
        self.reg
            .lock()
            .unwrap()
            .get(&(op, path.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn bump_reg(&self, op: Opcode, path: &str) {
        *self
            .reg
            .lock()
            .unwrap()
            .entry((op, path.to_string()))
            .or_insert(0) += 1;
    }
}

/// The table key for `vpath` under `root`: the bare vpath for root 0,
/// `@<root>/<vpath>` for any other. A fixture names a second root's files
/// with this (`key(1, "data/a.esp")`).
///
/// Folded (ASCII lowercase), because a real provider graph looks names up
/// case-insensitively and the shim does not always send the folded spelling:
/// a write open carries the caller's. So the reference stores and finds every
/// name folded, and does not model the stored case.
pub fn key(root: u32, vpath: &str) -> String {
    let vpath = vpath.to_ascii_lowercase();
    if root == 0 {
        vpath
    } else {
        format!("@{root}/{vpath}")
    }
}

/// A byte pattern no accidental fill can imitate, each position
/// distinguishable: a copy that drops, duplicates or reorders a fragment
/// fails a full comparison against it.
pub fn pattern(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x9E37_79B9;
    for i in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 24) as u8 ^ (i as u8));
    }
    v
}

/// What every handle on one graph shares.
struct Graph {
    /// Behind a `Mutex` because WRITE changes it and a creating OPEN adds to
    /// it, from whichever handle serves.
    files: Mutex<HashMap<String, Entry>>,
    /// Vpaths that resolve as directories. Fixed once built: nothing here
    /// creates one, since the shim takes directory creates before they reach
    /// a create open.
    dirs: Vec<String>,
    handles: Mutex<HashMap<u64, String>>,
    next_fh: AtomicU64,
    writable: Vec<String>,
    /// Prefixes under which a write open fails with `Status::IoError`: a
    /// provider that broke, not one that refused.
    failing: Vec<String>,
    /// Answers the registry opcodes when set ([`Reference::with_registry`]).
    registry: Option<Registry>,
}

impl Graph {
    /// A root itself (`"."` on the wire) is always a directory.
    fn is_dir(&self, vpath: &str) -> bool {
        vpath == "." || vpath.ends_with("/.") || self.dirs.iter().any(|d| d == vpath)
    }

    fn is_writable(&self, vpath: &str) -> bool {
        self.writable.iter().any(|p| vpath.starts_with(p.as_str()))
    }

    fn write_fails(&self, vpath: &str) -> bool {
        self.failing.iter().any(|p| vpath.starts_with(p.as_str()))
    }

    fn handle(&self, fh: u64) -> Option<String> {
        self.handles.lock().unwrap().get(&fh).cloned()
    }

    fn issue(&self, vpath: String, tally: &Tally) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        Tally::bump(&tally.opened, &vpath);
        self.handles.lock().unwrap().insert(fh, vpath);
        fh
    }
}

/// The reference director. Built with the methods below, then served
/// (`impl Director`) from one handle while tests read [`Reference::tally`]
/// and [`Reference::contents`] from another ([`Reference::share`]).
pub struct Reference {
    graph: Arc<Graph>,
    /// What reached the server, shared by every handle on the graph.
    pub tally: Arc<Tally>,
    /// An inline READ reply's bytes, copied out of the locked table.
    read_buf: Vec<u8>,
}

impl Default for Reference {
    fn default() -> Reference {
        Reference::new()
    }
}

impl Reference {
    /// An empty, read-only graph with no registry layer.
    pub fn new() -> Reference {
        Reference {
            graph: Arc::new(Graph {
                files: Mutex::new(HashMap::new()),
                dirs: Vec::new(),
                handles: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(1),
                writable: Vec::new(),
                failing: Vec::new(),
                registry: None,
            }),
            tally: Arc::new(Tally::default()),
            read_buf: Vec::new(),
        }
    }

    /// The graph, for a builder method: only before it is shared.
    fn building(&mut self) -> &mut Graph {
        Arc::get_mut(&mut self.graph).expect("a Reference is built before it is shared")
    }

    /// Another handle on the same graph, registry and tally, with its own
    /// reply buffer: the one a server thread answers from.
    pub fn share(&self) -> Reference {
        Reference {
            graph: self.graph.clone(),
            tally: self.tally.clone(),
            read_buf: Vec::new(),
        }
    }

    /// Serve the registry opcodes from an empty overlay. Without this they
    /// answer `Status::NotSupported`, as a director with no registry layer
    /// does.
    pub fn with_registry(mut self) -> Reference {
        self.building().registry = Some(Registry::default());
        self
    }

    /// Detach the registry layer, as a director whose layer went away: every
    /// registry opcode answers `Status::NotSupported` until
    /// [`Reference::attach_registry`], and the generation moves.
    pub fn detach_registry(&self) {
        self.registry().set_attached(false);
    }

    /// Attach the registry layer again, with the overlay it had; the
    /// generation moves.
    pub fn attach_registry(&self) {
        self.registry().set_attached(true);
    }

    fn registry(&self) -> &Registry {
        self.graph
            .registry
            .as_ref()
            .expect("Reference::with_registry")
    }

    /// Calls `publish` with the registry generation now and after every
    /// change to it, under the registry's lock, so a write's generation is
    /// published before its reply leaves. A server puts it in the ring header
    /// (`aether_ipc::ring::publish_reg_generation`). Does nothing without a
    /// registry layer.
    pub fn publish_generation(&self, publish: impl Fn(u64) + Send + Sync + 'static) {
        if let Some(r) = &self.graph.registry {
            r.publish_with(Box::new(publish));
        }
    }

    /// Add a directory to the provider graph.
    ///
    /// It answers a write open the way a real provider graph does: a
    /// writable mount answers `Status::IoError` (opening a directory for
    /// writing fails), a read-only one `Status::ReadOnly`, and a creating open
    /// on a writable mount `Status::IsDir`. On a directory handle the access
    /// bits the shim reads as write access are `FILE_ADD_FILE` and
    /// `FILE_ADD_SUBDIRECTORY`, which every backup-semantics directory open
    /// carries, so these answers matter.
    pub fn with_dir(mut self, vpath: &str) -> Reference {
        self.building().dirs.push(vpath.to_ascii_lowercase());
        self
    }

    /// Declare a vpath prefix that a writable provider is mounted over: a
    /// create inside it is honoured, and writes to files inside it stick.
    /// Outside every declared prefix the graph is read-only: a served file
    /// answers `Status::ReadOnly` to a write open and an unserved one
    /// `Status::NotFound` to a create. With no prefix declared (the default)
    /// the whole graph is read-only.
    pub fn writable_under(mut self, prefix: &str) -> Reference {
        self.building().writable.push(prefix.to_ascii_lowercase());
        self
    }

    /// Declare a vpath prefix whose write opens fail with `Status::IoError`,
    /// the answer a provider gives when its own I/O breaks. Checked before
    /// anything else a write open looks at.
    pub fn failing_writes_under(mut self, prefix: &str) -> Reference {
        self.building().failing.push(prefix.to_ascii_lowercase());
        self
    }

    /// Add a file to the provider graph. `vpath` is the folded, `/`-joined
    /// remainder the shim builds from the path (`Data\A.esp` is
    /// `data/a.esp`).
    pub fn with(self, vpath: &str, bytes: Vec<u8>, style: ReadStyle) -> Reference {
        self.graph
            .files
            .lock()
            .unwrap()
            .insert(vpath.to_ascii_lowercase(), Entry { bytes, style });
        self
    }

    /// The bytes the graph holds for `vpath` now: what a WRITE that crossed
    /// the ring left behind.
    pub fn contents(&self, vpath: &str) -> Option<Vec<u8>> {
        self.graph
            .files
            .lock()
            .unwrap()
            .get(&vpath.to_ascii_lowercase())
            .map(|e| e.bytes.clone())
    }

    /// The registry layer, counting the request; `NotSupported` without one.
    fn reg(&self, op: Opcode, path: &str) -> Result<&Registry, Status> {
        let r = self.graph.registry.as_ref().ok_or(Status::NotSupported)?;
        self.tally.bump_reg(op, path);
        Ok(r)
    }
}

impl Director for Reference {
    fn heartbeat(&mut self) -> Result<(), Status> {
        Ok(())
    }

    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        let vpath = key(root.0, path);
        if self.graph.is_dir(&vpath) {
            return Ok(AttrResp {
                found: true,
                is_dir: true,
                size: 0,
                mtime: 0,
            });
        }
        let files = self.graph.files.lock().unwrap();
        let found = files.get(&vpath);
        Ok(AttrResp {
            found: found.is_some(),
            is_dir: false,
            size: found.map(|e| e.bytes.len() as u64).unwrap_or(0),
            mtime: 0,
        })
    }

    /// The immediate children of `path`, as a composed provider graph
    /// reports them, including any name the graph holds that the shim would
    /// rather not see. Live, the director's write layer is an overlay whose
    /// upper is the directory the shim's local overlay writes into, and the
    /// two spell whiteouts differently (`.wh.<name>` there,
    /// `<name>.__vfs_wh__` here), so a shim-written marker is an ordinary
    /// zero-byte file to the director and comes back in its listing. A
    /// reference that filtered it would hide that defect.
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        // The shim sends "." for the root itself.
        let dir = match path {
            "." | "" => key(root.0, ""),
            v => key(root.0, v),
        };
        let prefix = if dir.is_empty() || dir.ends_with('/') {
            dir
        } else {
            format!("{dir}/")
        };
        let child = |p: &str| {
            p.strip_prefix(prefix.as_str())
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                .map(str::to_string)
        };
        let mut rows: Vec<(String, bool, u64)> = Vec::new();
        let mut push = |name: String, is_dir: bool, size: u64| {
            if !rows.iter().any(|(n, ..)| *n == name) {
                rows.push((name, is_dir, size));
            }
        };
        for d in &self.graph.dirs {
            if let Some(name) = child(d) {
                push(name, true, 0);
            }
        }
        for (p, e) in self.graph.files.lock().unwrap().iter() {
            if let Some(name) = child(p) {
                push(name, false, e.bytes.len() as u64);
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir, size) in &rows {
            out.push(DirEntry {
                name,
                is_dir: *is_dir,
                size: *size,
                mtime: 0,
            });
        }
        Ok(())
    }

    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        let g = &self.graph;
        self.tally
            .wire_opens
            .lock()
            .unwrap()
            .push((flags.bits(), path.to_string()));
        let vpath = key(root.0, path);
        let write = flags.contains(OpenFlags::WRITE);
        if write && g.write_fails(&vpath) {
            return Err(Status::IoError);
        }
        if g.is_dir(&vpath) {
            if write {
                // The three answers a real graph gives, each its own code
                // path (see `with_dir`): a read-only mount refuses on
                // access; an overlay whose base holds the directory refuses
                // a creating open; a writable disk provider tries to open
                // the directory for writing and fails.
                return Err(if !g.is_writable(&vpath) {
                    Status::ReadOnly
                } else if flags.contains(OpenFlags::CREATE) {
                    Status::IsDir
                } else {
                    Status::IoError
                });
            }
            let fh = g.issue(vpath, &self.tally);
            return Ok(OpenResp {
                fh,
                size: 0,
                is_dir: true,
                ..Default::default()
            });
        }
        let mut files = g.files.lock().unwrap();
        let exists = files.contains_key(&vpath);
        if write {
            // The access check first (a read-only mount refuses the write
            // whether or not the file is there), then the disposition bits.
            if !g.is_writable(&vpath) {
                return Err(if exists {
                    Status::ReadOnly
                } else {
                    Status::NotFound
                });
            }
            if exists && flags.contains(OpenFlags::EXCL) {
                return Err(Status::Exists);
            }
            if !exists {
                if !flags.contains(OpenFlags::CREATE) {
                    return Err(Status::NotFound);
                }
                files.insert(
                    vpath.clone(),
                    Entry {
                        bytes: Vec::new(),
                        style: ReadStyle::Whole,
                    },
                );
            } else if flags.contains(OpenFlags::TRUNC) {
                files.get_mut(&vpath).expect("just checked").bytes.clear();
            }
        }
        let Some(e) = files.get(&vpath) else {
            return Err(Status::NotFound);
        };
        let size = e.bytes.len() as u64;
        drop(files);
        let fh = g.issue(vpath, &self.tally);
        Ok(OpenResp {
            fh,
            size,
            is_dir: false,
            ..Default::default()
        })
    }

    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        let Some(vpath) = self.graph.handle(fh) else {
            return Err(Status::BadRequest);
        };
        Tally::bump(&self.tally.reads, &vpath);
        let files = self.graph.files.lock().unwrap();
        // A file deleted or renamed under an open handle.
        let Some(e) = files.get(&vpath) else {
            return Err(Status::IoError);
        };
        let limit = match e.style {
            ReadStyle::Error => return Err(Status::IoError),
            ReadStyle::Whole | ReadStyle::Short(_) => e.bytes.len() as u64,
            ReadStyle::ShorterThanClaimed(n) => n,
        };
        let start = offset.min(limit) as usize;
        let mut want = (limit as usize - start).min(len as usize);
        if let ReadStyle::Short(n) = e.style {
            want = want.min(n);
        }
        let src = &e.bytes[start..start + want];
        // Bulk chosen as a director chooses it, so a fixture gets
        // production's transport.
        let want_bulk = slot_flags.contains(RequestFlags::READ_BULK) || len >= BULK_THRESHOLD;
        if let (true, Some(arena)) = (want_bulk, at.arena) {
            Tally::bump(&self.tally.bulk_reads, &vpath);
            let max = arena.bank_size.min(src.len());
            return match arena.fill_bank(at.slot, max, |buf| {
                let n = buf.len().min(src.len());
                buf[..n].copy_from_slice(&src[..n]);
                Ok(n)
            }) {
                Ok((arena_offset, n)) => Ok(ReadReply::Bulk {
                    len: n as u32,
                    arena_offset,
                }),
                Err(st) => Err(Status::try_from(st).unwrap_or(Status::IoError)),
            };
        }
        self.read_buf.clear();
        self.read_buf.extend_from_slice(src);
        drop(files);
        Ok(ReadReply::Inline(&self.read_buf))
    }

    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        let Some(vpath) = self.graph.handle(fh) else {
            return Err(Status::BadFh);
        };
        Tally::bump(&self.tally.writes, &vpath);
        let mut files = self.graph.files.lock().unwrap();
        let Some(e) = files.get_mut(&vpath) else {
            return Err(Status::NotFound);
        };
        let at = offset as usize;
        if e.bytes.len() < at + data.len() {
            e.bytes.resize(at + data.len(), 0);
        }
        e.bytes[at..at + data.len()].copy_from_slice(data);
        Ok(data.len() as u32)
    }

    /// Truncate or extend: how the shim's `truncate` reaches the director.
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status> {
        let Some(vpath) = self.graph.handle(fh) else {
            return Err(Status::BadFh);
        };
        let mut files = self.graph.files.lock().unwrap();
        let Some(e) = files.get_mut(&vpath) else {
            return Err(Status::NotFound);
        };
        e.bytes.resize(size as usize, 0);
        Ok(())
    }

    /// RENAME and DELETE refuse as a real overlay-backed graph does: a vpath
    /// nothing serves is `Status::NotFound`, and one outside every writable
    /// mount `Status::ReadOnly`.
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        let (from, to) = (key(root.0, from), key(root.0, to));
        Tally::bump(&self.tally.renames, &from);
        let mut files = self.graph.files.lock().unwrap();
        if !files.contains_key(&from) {
            return Err(Status::NotFound);
        }
        if !self.graph.is_writable(&from) || !self.graph.is_writable(&to) {
            return Err(Status::ReadOnly);
        }
        let e = files.remove(&from).expect("just checked");
        files.insert(to, e);
        Ok(())
    }

    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        let vpath = key(root.0, path);
        Tally::bump(&self.tally.deletes, &vpath);
        let mut files = self.graph.files.lock().unwrap();
        if !files.contains_key(&vpath) {
            return Err(Status::NotFound);
        }
        if !self.graph.is_writable(&vpath) {
            return Err(Status::ReadOnly);
        }
        files.remove(&vpath);
        Ok(())
    }

    fn mkdir(&mut self, _: RootId, _: u32, _: &str) -> Result<(), Status> {
        Err(Status::NotSupported)
    }

    /// OK for any handle; a handle this graph issued is released and counted.
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        if let Some(vpath) = self.graph.handles.lock().unwrap().remove(&fh) {
            Tally::bump(&self.tally.closed, &vpath);
        }
        Ok(())
    }

    fn stored_names(&mut self, _: RootId, _: u32, _: &str) -> Result<&str, Status> {
        Err(Status::NotSupported)
    }

    fn rename_x(&mut self, _: RootId, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Err(Status::NotSupported)
    }

    fn stage_exec(&mut self, _: RootId, _: &str) -> Result<&str, Status> {
        Err(Status::NotSupported)
    }

    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status> {
        self.reg(Opcode::RegLookup, path)?.read(path, |o| {
            let (state, below) = o.lookup(path);
            LookupReply {
                version: o.version(),
                state,
                below,
            }
        })
    }

    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status> {
        // Owned: the overlay is behind the registry's lock.
        self.reg(Opcode::RegKey, path)?.read(path, |o| KeyReply {
            version: o.version(),
            node: o.node(path).cloned().map(Cow::Owned),
        })
    }

    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status> {
        self.reg(Opcode::RegChanged, path)?
            .read(path, |o| ChangedReply {
                version: o.version(),
                changed: o.changed_since(path, subtree, since),
            })
    }

    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status> {
        self.reg(Opcode::RegSetValue, path)?
            .write(path, |o, now| o.set_value(path, name, ty, data, now))
    }

    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status> {
        self.reg(Opcode::RegDeleteValue, path)?
            .write(path, |o, now| o.delete_value(path, name, now))
    }

    /// Always created here: the shim asks only for a key with no real
    /// counterpart.
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status> {
        self.reg(Opcode::RegCreateKey, path)?
            .write(path, |o, now| o.create_key(path, volatile, false, now))
    }

    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status> {
        self.reg(Opcode::RegDeleteKey, path)?
            .write(path, |o, now| o.delete_key(path, now))
    }

    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status> {
        let r = self.reg(Opcode::RegRenameKey, path)?;
        if new_leaf == "." || new_leaf == ".." {
            // Checked after the layer is known to be attached.
            r.check(path)?;
            return Err(Status::BadRequest);
        }
        r.write(path, |o, now| o.rename_key(path, new_leaf, now))
    }

    /// Opcodes outside the table are `Status::NotSupported`, not the
    /// framing default.
    fn unknown(&mut self, _: u32) -> Status {
        Status::NotSupported
    }

    /// What the fake answered a payload it could not decode: CLOSE is OK
    /// whatever it says; the opcodes it does not serve, and the registry
    /// ones with no attached layer, are `Status::NotSupported`; the rest
    /// `Status::BadRequest`. A registry request whose leading path decodes is
    /// counted, as it was.
    fn undecodable(&mut self, op: Opcode, _: u32, payload: &[u8]) -> Status {
        match op {
            Opcode::Close | Opcode::Heartbeat => Status::Ok,
            Opcode::Mkdir | Opcode::StoredNames | Opcode::RenameX | Opcode::StageExec => {
                Status::NotSupported
            }
            Opcode::RegLookup
            | Opcode::RegKey
            | Opcode::RegSetValue
            | Opcode::RegDeleteValue
            | Opcode::RegCreateKey
            | Opcode::RegDeleteKey
            | Opcode::RegRenameKey
            | Opcode::RegChanged => {
                let Some(r) = &self.graph.registry else {
                    return Status::NotSupported;
                };
                if let Some(path) = leading_path(payload) {
                    self.tally.bump_reg(op, path);
                }
                if r.state.lock().unwrap().attached {
                    Status::BadRequest
                } else {
                    Status::NotSupported
                }
            }
            Opcode::GetAttr
            | Opcode::ReadDir
            | Opcode::Open
            | Opcode::Read
            | Opcode::Write
            | Opcode::SetAttr
            | Opcode::Rename
            | Opcode::Delete => Status::BadRequest,
        }
    }
}

/// The path every registry request leads with (`len:u32 | utf8`).
fn leading_path(payload: &[u8]) -> Option<&str> {
    let n = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
    core::str::from_utf8(payload.get(4..4 + n)?).ok()
}

/// The registry layer. One lock over all of it: each answer is computed with
/// the version it reports, and a write's generation is published before the
/// lock (and so the reply) is let go.
#[derive(Default)]
struct Registry {
    state: Mutex<RegState>,
    /// Where the generation is published, once a server has said
    /// ([`Reference::publish_generation`]).
    publish: OnceLock<Box<dyn Fn(u64) + Send + Sync>>,
}

struct RegState {
    overlay: Overlay,
    attached: bool,
    /// Starts at 1, moved by every successful write and every attach or
    /// detach. Not the overlay's version: the shim trusts a cached answer
    /// only while this has not moved.
    generation: u64,
}

impl Default for RegState {
    fn default() -> RegState {
        RegState {
            overlay: Overlay::new(),
            attached: true,
            generation: 1,
        }
    }
}

impl Registry {
    fn publish_with(&self, publish: Box<dyn Fn(u64) + Send + Sync>) {
        let s = self.state.lock().unwrap();
        let _ = self.publish.set(publish);
        if let Some(p) = self.publish.get() {
            p(s.generation);
        }
    }

    fn changed(&self, s: &mut RegState) {
        s.generation += 1;
        if let Some(p) = self.publish.get() {
            p(s.generation);
        }
    }

    fn set_attached(&self, attached: bool) {
        let mut s = self.state.lock().unwrap();
        s.attached = attached;
        self.changed(&mut s);
    }

    /// `NotSupported` when detached, `BadRequest` for a path that is not
    /// canonical.
    fn check(&self, path: &str) -> Result<(), Status> {
        if !self.state.lock().unwrap().attached {
            return Err(Status::NotSupported);
        }
        if !valid_reg_path(path) {
            return Err(Status::BadRequest);
        }
        Ok(())
    }

    fn read<T>(&self, path: &str, f: impl FnOnce(&Overlay) -> T) -> Result<T, Status> {
        let s = self.state.lock().unwrap();
        if !s.attached {
            return Err(Status::NotSupported);
        }
        if !valid_reg_path(path) {
            return Err(Status::BadRequest);
        }
        Ok(f(&s.overlay))
    }

    fn write(
        &self,
        path: &str,
        f: impl FnOnce(&mut Overlay, u64) -> Result<u64, RegError>,
    ) -> Result<u64, Status> {
        let mut s = self.state.lock().unwrap();
        if !s.attached {
            return Err(Status::NotSupported);
        }
        if !valid_reg_path(path) {
            return Err(Status::BadRequest);
        }
        match f(&mut s.overlay, filetime_now()) {
            Err(e) => Err(reg_status(e)),
            Ok(v) => {
                self.changed(&mut s);
                Ok(v)
            }
        }
    }
}

/// A canonical key path, which a director demands of every registry request:
/// `\Registry` then non-empty components, none `.` or `..`, no doubled or
/// trailing separator.
fn valid_reg_path(p: &str) -> bool {
    regpath::canonical(p, None).is_ok_and(|c| regpath::fold(&c) == regpath::fold(p))
        && !p.split('\\').any(|c| c == "." || c == "..")
}

/// The status for an overlay error; the shim maps these to NT statuses.
fn reg_status(e: RegError) -> Status {
    match e {
        RegError::NameTooLong
        | RegError::DataTooLarge
        | RegError::InvalidPath
        | RegError::TooDeep => Status::BadRequest,
        RegError::NotFound => Status::NotFound,
        RegError::AlreadyExists => Status::Exists,
    }
}

/// Now as a FILETIME (100 ns ticks since 1601): a written key's last-write
/// time.
fn filetime_now() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + u64::from(d.subsec_nanos()) / 100
}
