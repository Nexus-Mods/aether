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
//! SETATTR (truncate), DELETE, RENAME, MKDIR, CLOSE, STORED_NAMES and
//! RENAME_X, and the registry opcodes when it has a registry layer
//! ([`Reference::with_registry`]). STAGE_EXEC is `Status::NotSupported`: it
//! stages nothing. A name keeps the spelling it was created with (a fixture's
//! vpath, a creating OPEN, MKDIR, a RENAME or RENAME_X target), and READDIR
//! and STORED_NAMES answer with it (wire.md 3.2). Where the fake it was
//! moved from differed from `docs/wire.md`, it answers as wire.md says: an
//! opcode outside the table or
//! a payload that does not decode is `Status::BadRequest` (3.3, 3.5); a path
//! that leaves its root is `Status::BadRequest` (3.2, 3.5.1); a root it does
//! not serve is `Status::NotFound`, or OK with nothing found for GETATTR
//! (3.4.1, 3.5.1); an unknown handle is `Status::BadFh` and a directory
//! handle `Status::IsDir` (3.4.4, 3.4.10, 3.5); names match by
//! `aether_casefold::fold` (3.2).
//!
//! A test controls how a READ is answered ([`ReadStyle`]), because
//! a read loop's correctness is mostly about awkward answers: a short read
//! that is not EOF, a read that fails part-way, a file shorter than OPEN
//! said.
//!
//! **The write half** says the four things a real provider graph says to a
//! write open: here you are (inside a [`Reference::writable_under`] prefix),
//! no such place (`Status::NotFound`), not allowed (`Status::ReadOnly`, served
//! but outside every writable prefix) and already there (`Status::Exists` for
//! `OpenFlags::EXCL`). DELETE, RENAME, RENAME_X and MKDIR refuse the same
//! way, so a test of a
//! sealed path cannot pass because the director said yes to everything.
//! WRITE and SETATTR through a read handle on a file outside every writable
//! prefix are `Status::ReadOnly` too, so such a file never changes, and a
//! read open of one is answered `immutable` (wire.md 3.4.3), which lets the
//! shim's read cache hold it.
//!
//! **Roots.** Root 0's files are keyed by their bare vpath; any other root's
//! by `@<id>/<vpath>` ([`key`]). A request that lost its root then finds the
//! wrong file. Root 0 is always served; another root is served once the
//! reference is built with a file, directory or prefix under it, or named
//! with [`Reference::with_root`].
//!
//! **Size.** No file grows past [`MAX_FILE_LEN`]: a WRITE or SETATTR past it
//! is `Status::NoSpace` (storage full, wire.md 3.5), so hostile offsets and
//! lengths cannot make the reference allocate without bound.
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
//! ([`aether_rpc::serve_with_arena`]); an inline answer holds at most
//! `ReadSlot::inline_cap` (`payload_cap - 8`) bytes. A real copy-up of a
//! large file takes the bulk path, so a fixture with no arena tests the
//! transport production does not use.
//!
//! **Handles.** A [`Reference`] is one handle on a graph; [`Reference::share`]
//! makes another (the server thread's, say) with the same files, handles,
//! registry and [`Tally`]. Each handle has its own reply buffer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_casefold::fold;
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

/// The longest file the reference holds. A WRITE ending past it, or a
/// SETATTR to a larger size, is `Status::NoSpace`.
pub const MAX_FILE_LEN: usize = 64 * 1024 * 1024;

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
    /// The file's identity, which its handles hold: it moves with the file
    /// through RENAME and RENAME_X, and a file created later at the same
    /// name has another (`docs/wire.md` 3.4.7, 3.4.8).
    id: u64,
    bytes: Vec<u8>,
    style: ReadStyle,
    /// The vpath as it was spelled when the name was created, without the
    /// root's `@<id>/` ([`spelling`]): what READDIR and STORED_NAMES answer.
    name: String,
}

/// The spelling a name is stored with: the vpath as given, without a
/// [`key`]'s `@<id>/` root prefix.
fn spelling(vpath: &str) -> String {
    match vpath.strip_prefix('@').and_then(|r| r.split_once('/')) {
        Some((_, rest)) => rest.to_string(),
        None => vpath.to_string(),
    }
}

/// Component `i` of a stored spelling, if it has that many.
fn component(spelled: &str, i: usize) -> Option<&str> {
    spelled.split('/').nth(i)
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
        m.lock().unwrap().get(&fold(vpath)).copied().unwrap_or(0)
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
/// Folded (`aether_casefold::fold`, the wire's fold, `docs/wire.md` 3.2),
/// because a director looks names up case-insensitively and the shim does not
/// always send the folded spelling: a write open carries the caller's. So the
/// reference finds every name by its fold, and keeps the spelling it was
/// created with beside it, for READDIR and STORED_NAMES.
pub fn key(root: u32, vpath: &str) -> String {
    let vpath = fold(vpath);
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
    /// Vpaths that resolve as directories, by table key ([`key`]), each with
    /// its stored spelling. MKDIR adds to it. Locked before `files` wherever
    /// both are held.
    dirs: Mutex<HashMap<String, String>>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    /// The next file identity ([`Entry::id`]).
    next_id: AtomicU64,
    writable: Vec<String>,
    /// Prefixes under which a write open fails with `Status::IoError`: a
    /// provider that broke, not one that refused.
    failing: Vec<String>,
    /// Answers the registry opcodes when set ([`Reference::with_registry`]).
    registry: Option<Registry>,
    /// The roots besides 0 that are served. Fixed once built.
    roots: Vec<u32>,
}

impl Graph {
    /// A root itself (`"."` on the wire) is always a directory.
    fn is_dir(&self, vpath: &str) -> bool {
        is_dir_in(&self.dirs.lock().unwrap(), vpath)
    }

    fn is_writable(&self, vpath: &str) -> bool {
        self.writable.iter().any(|p| vpath.starts_with(p.as_str()))
    }

    fn write_fails(&self, vpath: &str) -> bool {
        self.failing.iter().any(|p| vpath.starts_with(p.as_str()))
    }

    fn handle(&self, fh: u64) -> Option<Handle> {
        self.handles.lock().unwrap().get(&fh).cloned()
    }

    /// The root a table key or prefix is under ([`key`]'s `@<id>/`).
    fn root_of(k: &str) -> Option<u32> {
        match k.strip_prefix('@') {
            None => Some(0),
            Some(rest) => rest.split_once('/')?.0.parse().ok(),
        }
    }

    fn serves(&self, root: u32) -> bool {
        root == 0 || self.roots.contains(&root)
    }

    /// Whether any writable provider is mounted on `root` (an empty prefix
    /// covers every root).
    fn root_writable(&self, root: u32) -> bool {
        self.writable
            .iter()
            .any(|p| p.is_empty() || Self::root_of(p) == Some(root))
    }

    /// The table key for a request's path: `BadRequest` for a path that
    /// leaves its root, `NotFound` for a root not served.
    fn resolve(&self, root: RootId, path: &str) -> Result<String, Status> {
        if escapes(path) {
            return Err(Status::BadRequest);
        }
        if !self.serves(root.0) {
            return Err(Status::NotFound);
        }
        Ok(key(root.0, path))
    }

    fn issue(&self, vpath: String, file: Option<u64>, append: bool, tally: &Tally) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        Tally::bump(&tally.opened, &vpath);
        self.handles.lock().unwrap().insert(
            fh,
            Handle {
                vpath,
                file,
                append,
            },
        );
        fh
    }

    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// The table key the file with identity `id` is at now: `hint` (where its
/// handle was opened) while it is still there, else wherever a RENAME or
/// RENAME_X moved it. `None` once it was deleted or replaced.
fn locate(files: &HashMap<String, Entry>, hint: &str, id: u64) -> Option<String> {
    if files.get(hint).is_some_and(|e| e.id == id) {
        return Some(hint.to_string());
    }
    files
        .iter()
        .find(|(_, e)| e.id == id)
        .map(|(k, _)| k.clone())
}

/// Whether table key `k` names a root itself (`"."` on the wire).
fn is_root_key(k: &str) -> bool {
    k.is_empty() || k == "." || k.ends_with("/.") || (k.starts_with('@') && k.ends_with('/'))
}

/// Whether anything, directory or file, is below table key `k`.
fn has_children(dirs: &HashMap<String, String>, files: &HashMap<String, Entry>, k: &str) -> bool {
    let below = format!("{k}/");
    dirs.keys()
        .chain(files.keys())
        .any(|p| p.starts_with(&below))
}

/// Whether table key `vpath` is a directory in `dirs`; a root itself is.
fn is_dir_in(dirs: &HashMap<String, String>, vpath: &str) -> bool {
    vpath == "." || vpath.ends_with("/.") || dirs.contains_key(vpath)
}

/// The stored spelling of each component of `path` under `root`
/// (`docs/wire.md` 3.2, 3.4.12): taken from the entry the component names,
/// else from any entry below it (a directory the graph implies but does not
/// hold), else as `path` spells it.
fn stored_components(
    dirs: &HashMap<String, String>,
    files: &HashMap<String, Entry>,
    root: u32,
    path: &str,
) -> Vec<String> {
    let sent: Vec<&str> = path.split('/').collect();
    (0..sent.len())
        .map(|i| {
            let k = key(root, &sent[..=i].join("/"));
            let below = format!("{k}/");
            let exact = dirs
                .get(&k)
                .map(String::as_str)
                .or_else(|| files.get(&k).map(|e| e.name.as_str()));
            let under = || {
                dirs.iter()
                    .map(|(k, n)| (k, n.as_str()))
                    .chain(files.iter().map(|(k, e)| (k, e.name.as_str())))
                    .filter(|(k, _)| k.starts_with(below.as_str()))
                    // The first by key, so the answer does not depend on
                    // the map's order when two entries spell it apart.
                    .min_by(|a, b| a.0.cmp(b.0))
                    .map(|(_, n)| n)
            };
            exact
                .or_else(under)
                .and_then(|n| component(n, i))
                .unwrap_or(sent[i])
                .to_string()
        })
        .collect()
}

/// The spelling a name created at `path` is stored with: the components
/// above it as they are stored, its own as the request spells it (3.2,
/// "Keep the spelling a name was created with").
fn created_spelling(
    dirs: &HashMap<String, String>,
    files: &HashMap<String, Entry>,
    root: u32,
    path: &str,
) -> String {
    match path.rsplit_once('/') {
        None => path.to_string(),
        Some((parent, leaf)) => {
            let mut c = stored_components(dirs, files, root, parent);
            c.push(leaf.to_string());
            c.join("/")
        }
    }
}

/// Whether `path` walks above its root: a `..` with no component before it
/// to undo (`docs/wire.md` 3.2).
fn escapes(path: &str) -> bool {
    let mut depth = 0usize;
    for c in path.split('/') {
        match c {
            ".." => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return true,
            },
            "" | "." => {}
            _ => depth += 1,
        }
    }
    false
}

/// An open handle: the table key it was opened at (its tallies are kept
/// under it), the identity of the file it holds (`None` for a directory
/// handle), and whether it was opened `OpenFlags::APPEND`. A file handle
/// acts on its file wherever the file is now, and on nothing once the file
/// is gone (`docs/wire.md` 3.4.7, 3.4.8).
#[derive(Clone)]
struct Handle {
    vpath: String,
    file: Option<u64>,
    append: bool,
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
    /// A STORED_NAMES reply.
    names_buf: String,
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
                dirs: Mutex::new(HashMap::new()),
                handles: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(1),
                next_id: AtomicU64::new(1),
                writable: Vec::new(),
                failing: Vec::new(),
                registry: None,
                roots: Vec::new(),
            }),
            tally: Arc::new(Tally::default()),
            read_buf: Vec::new(),
            names_buf: String::new(),
        }
    }

    /// The graph, for a builder method: only before it is shared.
    fn building(&mut self) -> &mut Graph {
        Arc::get_mut(&mut self.graph).expect("a Reference is built before it is shared")
    }

    /// Serve the root of table key or prefix `k`.
    fn note_root(&mut self, k: &str) {
        if let Some(r) = Graph::root_of(k).filter(|&r| r != 0) {
            let g = self.building();
            if !g.roots.contains(&r) {
                g.roots.push(r);
            }
        }
    }

    /// Serve root `id` even with nothing under it. Root 0 is always served,
    /// and so is any root a file, directory or prefix was added under.
    pub fn with_root(mut self, id: u32) -> Reference {
        self.note_root(&key(id, ""));
        self
    }

    /// Another handle on the same graph, registry and tally, with its own
    /// reply buffer: the one a server thread answers from.
    pub fn share(&self) -> Reference {
        Reference {
            graph: self.graph.clone(),
            tally: self.tally.clone(),
            read_buf: Vec::new(),
            names_buf: String::new(),
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
    /// The directory keeps `vpath`'s spelling, as one MKDIR created would.
    pub fn with_dir(mut self, vpath: &str) -> Reference {
        self.note_root(vpath);
        self.building()
            .dirs
            .get_mut()
            .unwrap()
            .insert(fold(vpath), spelling(vpath));
        self
    }

    /// Declare a vpath prefix that a writable provider is mounted over: a
    /// create inside it is honoured, and writes to files inside it stick.
    /// Outside every declared prefix the graph is read-only: a served file
    /// answers `Status::ReadOnly` to a write open and an unserved one
    /// `Status::NotFound` to a create, except on a root with no writable
    /// prefix at all, where every write open is `Status::ReadOnly`
    /// (`docs/wire.md` 3.4.3). With no prefix declared (the default) the
    /// whole graph is read-only. An empty prefix covers every root.
    pub fn writable_under(mut self, prefix: &str) -> Reference {
        self.note_root(prefix);
        self.building().writable.push(fold(prefix));
        self
    }

    /// Declare a vpath prefix whose write opens (and MKDIRs) fail with
    /// `Status::IoError`, the answer a provider gives when its own I/O
    /// breaks. Checked before anything else a write open looks at. Such a
    /// prefix does not count as a writable provider: it makes neither its
    /// paths nor its root writable (see [`Reference::writable_under`]).
    pub fn failing_writes_under(mut self, prefix: &str) -> Reference {
        self.note_root(prefix);
        self.building().failing.push(fold(prefix));
        self
    }

    /// Add a file to the provider graph. `vpath` is the `/`-joined remainder
    /// the shim builds from the path (`Data\A.esp` is `Data/A.esp`); it is
    /// found by its fold and stored with this spelling, as a name created
    /// through the ring would be.
    pub fn with(mut self, vpath: &str, bytes: Vec<u8>, style: ReadStyle) -> Reference {
        self.note_root(vpath);
        let name = spelling(vpath);
        let id = self.graph.new_id();
        self.graph.files.lock().unwrap().insert(
            fold(vpath),
            Entry {
                id,
                bytes,
                style,
                name,
            },
        );
        self
    }

    /// The bytes the graph holds for `vpath` now: what a WRITE that crossed
    /// the ring left behind.
    pub fn contents(&self, vpath: &str) -> Option<Vec<u8>> {
        self.graph
            .files
            .lock()
            .unwrap()
            .get(&fold(vpath))
            .map(|e| e.bytes.clone())
    }

    /// Move the entry at table key `from` to `to` under `to_root`, stored
    /// with `to`'s spelling: RENAME's and RENAME_X's common part, once both
    /// paths are resolved. A source with no entry is `Status::NotFound`;
    /// either end outside every writable prefix `Status::ReadOnly`.
    ///
    /// Within a root (`across` false) it does what `docs/wire.md` 3.4.7
    /// says: the root at either end, or a target
    /// inside the source, is `Status::BadRequest`; a directory moves with
    /// everything under it; a non-empty directory at the target is
    /// `Status::IoError`, and an empty one or a file is replaced. Across
    /// roots (RENAME_X, which refuses a directory source before this) a
    /// directory at the target, the target root itself included, is
    /// `Status::IsDir` and a file is replaced.
    fn move_entry(
        &self,
        from: String,
        to_root: RootId,
        to: &str,
        across: bool,
    ) -> Result<(), Status> {
        let g = &self.graph;
        let to_key = key(to_root.0, to);
        let mut dirs = g.dirs.lock().unwrap();
        let mut files = g.files.lock().unwrap();
        if !across
            && (is_root_key(&from)
                || is_root_key(&to_key)
                || to_key.starts_with(&format!("{from}/")))
        {
            return Err(Status::BadRequest);
        }
        let is_file = files.contains_key(&from);
        if !is_file && !dirs.contains_key(&from) {
            return Err(Status::NotFound);
        }
        if !g.is_writable(&from) || !g.is_writable(&to_key) {
            return Err(Status::ReadOnly);
        }
        // A root is a directory too, though `dirs` does not hold it: within a
        // root it was refused above, across roots it is `IsDir` here.
        if to_key != from && (is_dir_in(&dirs, &to_key) || is_root_key(&to_key)) {
            if across {
                return Err(Status::IsDir);
            }
            if has_children(&dirs, &files, &to_key) {
                return Err(Status::IoError);
            }
            dirs.remove(&to_key);
        }
        if to_key != from {
            files.remove(&to_key);
        }
        if is_file {
            let mut e = files.remove(&from).expect("just checked");
            e.name = created_spelling(&dirs, &files, to_root.0, to);
            files.insert(to_key, e);
            return Ok(());
        }
        // A directory: it and everything under it, each respelled under
        // the new name.
        let old = dirs.remove(&from).expect("just checked");
        let depth = old.split('/').count();
        let name = created_spelling(&dirs, &files, to_root.0, to);
        let below = format!("{from}/");
        let moved = |k: &str, n: &str| {
            let tail = n.splitn(depth + 1, '/').nth(depth).unwrap_or("");
            (
                format!("{to_key}{}", &k[from.len()..]),
                format!("{name}/{tail}"),
            )
        };
        let sub: Vec<String> = dirs
            .keys()
            .filter(|k| k.starts_with(&below))
            .cloned()
            .collect();
        for k in sub {
            let n = dirs.remove(&k).expect("listed");
            let (k, n) = moved(&k, &n);
            dirs.insert(k, n);
        }
        let sub: Vec<String> = files
            .keys()
            .filter(|k| k.starts_with(&below))
            .cloned()
            .collect();
        for k in sub {
            let mut e = files.remove(&k).expect("listed");
            let (k, n) = moved(&k, &e.name);
            e.name = n;
            files.insert(k, e);
        }
        dirs.insert(to_key, name);
        Ok(())
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

    /// A missing path, and any path on a root not served, is OK with
    /// nothing found (`docs/wire.md` 3.4.1).
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        let vpath = match self.graph.resolve(root, path) {
            Err(Status::NotFound) => {
                return Ok(AttrResp {
                    found: false,
                    is_dir: false,
                    size: 0,
                    mtime: 0,
                })
            }
            r => r?,
        };
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
            "." | "" => self.graph.resolve(root, "")?,
            v => self.graph.resolve(root, v)?,
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
        // (folded name, stored name, is_dir, size): one row per fold, the
        // stored spelling listed (wire.md 3.2, 3.4.2), ordered by the fold.
        let mut rows: Vec<(String, String, bool, u64)> = Vec::new();
        let mut push = |folded: String, stored: &str, is_dir: bool, size: u64| {
            if !rows.iter().any(|(n, ..)| *n == folded) {
                let stored = stored.rsplit('/').next().unwrap_or(stored);
                // A spelling whose leaf is not this name's fold (a fixture
                // key that is not a plain path) lists the fold.
                let name = if fold(stored) == folded {
                    stored.to_string()
                } else {
                    folded.clone()
                };
                rows.push((folded, name, is_dir, size));
            }
        };
        let dirs = self.graph.dirs.lock().unwrap();
        for (d, stored) in dirs.iter() {
            if let Some(name) = child(d) {
                push(name, stored, true, 0);
            }
        }
        for (p, e) in self.graph.files.lock().unwrap().iter() {
            if let Some(name) = child(p) {
                push(name, &e.name, false, e.bytes.len() as u64);
            }
        }
        drop(dirs);
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, name, is_dir, size) in &rows {
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
        let vpath = g.resolve(root, path)?;
        let write = flags.contains(OpenFlags::WRITE);
        let append = flags.contains(OpenFlags::APPEND);
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
            let fh = g.issue(vpath, None, append, &self.tally);
            return Ok(OpenResp {
                fh,
                size: 0,
                is_dir: true,
                ..Default::default()
            });
        }
        let dirs = g.dirs.lock().unwrap();
        let mut files = g.files.lock().unwrap();
        let exists = files.contains_key(&vpath);
        if write {
            // The access check first (a read-only mount refuses the write
            // whether or not the file is there, and so does a root with no
            // writable provider, wire.md 3.4.3), then the disposition bits.
            if !g.is_writable(&vpath) {
                return Err(if exists || !g.root_writable(root.0) {
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
                // A create keeps the caller's spelling (wire.md 3.2).
                let name = created_spelling(&dirs, &files, root.0, path);
                files.insert(
                    vpath.clone(),
                    Entry {
                        id: g.new_id(),
                        bytes: Vec::new(),
                        style: ReadStyle::Whole,
                        name,
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
        let id = e.id;
        drop(files);
        drop(dirs);
        // A read open of a file a read-only provider holds is immutable
        // (wire.md 3.4.3): nothing the reference serves changes it (a write
        // open, DELETE, RENAME, RENAME_X and MKDIR of it are refused, and so
        // are WRITE and SETATTR through the read handle), and
        // the reference never replaces a root's content, so `mount_gen`
        // stays 0. Never on a write open, a directory, or a file the
        // writable provider holds.
        let immutable = !write && !g.is_writable(&vpath);
        let fh = g.issue(vpath, Some(id), append, &self.tally);
        Ok(OpenResp {
            fh,
            size,
            is_dir: false,
            immutable,
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
        let Some(Handle { vpath, file, .. }) = self.graph.handle(fh) else {
            return Err(Status::BadFh);
        };
        Tally::bump(&self.tally.reads, &vpath);
        let Some(id) = file else {
            return Err(Status::IsDir);
        };
        let files = self.graph.files.lock().unwrap();
        // A file deleted, or replaced by a rename onto its name, under an
        // open handle (wire.md 3.4.8).
        let Some(e) = locate(&files, &vpath, id).and_then(|k| files.get(&k)) else {
            return Err(Status::IoError);
        };
        let limit = match e.style {
            ReadStyle::Error => return Err(Status::IoError),
            ReadStyle::Whole | ReadStyle::Short(_) => e.bytes.len() as u64,
            // Never past the bytes it has, whatever `n` claims.
            ReadStyle::ShorterThanClaimed(n) => n.min(e.bytes.len() as u64),
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
        // Inline: at most what the reply holds, and that much when the file
        // has it (wire.md 3.4.4: `n <= min(len, payload_cap - 8)`).
        let src = &src[..src.len().min(at.inline_cap)];
        self.read_buf.clear();
        self.read_buf.extend_from_slice(src);
        drop(files);
        Ok(ReadReply::Inline(&self.read_buf))
    }

    /// On an append handle the offset is ignored (`docs/wire.md` 3.4.3).
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        let Some(Handle {
            vpath,
            file,
            append,
        }) = self.graph.handle(fh)
        else {
            return Err(Status::BadFh);
        };
        Tally::bump(&self.tally.writes, &vpath);
        let Some(id) = file else {
            return Err(Status::IsDir);
        };
        // Only a read open reaches a file no writable provider holds, and
        // its bytes stay as they are: OPEN called them immutable. Such a
        // file never moves, so the key it was opened at says.
        if !self.graph.is_writable(&vpath) {
            return Err(Status::ReadOnly);
        }
        let mut files = self.graph.files.lock().unwrap();
        let Some(e) = locate(&files, &vpath, id).and_then(|k| files.get_mut(&k)) else {
            return Err(Status::NotFound);
        };
        let at = if append { e.bytes.len() as u64 } else { offset };
        let end = at
            .checked_add(data.len() as u64)
            .filter(|&end| end <= MAX_FILE_LEN as u64)
            .ok_or(Status::NoSpace)? as usize;
        let at = at as usize;
        if e.bytes.len() < end {
            e.bytes.resize(end, 0);
        }
        e.bytes[at..end].copy_from_slice(data);
        Ok(data.len() as u32)
    }

    /// Truncate or extend: how the shim's `truncate` reaches the director.
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status> {
        let Some(Handle { vpath, file, .. }) = self.graph.handle(fh) else {
            return Err(Status::BadFh);
        };
        let Some(id) = file else {
            return Err(Status::IsDir);
        };
        if !self.graph.is_writable(&vpath) {
            return Err(Status::ReadOnly);
        }
        if size > MAX_FILE_LEN as u64 {
            return Err(Status::NoSpace);
        }
        let mut files = self.graph.files.lock().unwrap();
        let Some(e) = locate(&files, &vpath, id).and_then(|k| files.get_mut(&k)) else {
            return Err(Status::NotFound);
        };
        e.bytes.resize(size as usize, 0);
        Ok(())
    }

    /// RENAME and DELETE refuse as a real overlay-backed graph does: a vpath
    /// nothing serves is `Status::NotFound`, and one outside every writable
    /// mount `Status::ReadOnly`. The target keeps the caller's spelling
    /// (`docs/wire.md` 3.2), so a rename to another case respells the file.
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        Tally::bump(&self.tally.renames, &key(root.0, from));
        if escapes(to) {
            return Err(Status::BadRequest);
        }
        let from_key = self.graph.resolve(root, from)?;
        self.graph.resolve(root, to)?;
        self.move_entry(from_key, root, to, false)
    }

    /// A file, or a directory with nothing under it, as `docs/wire.md` 3.4.8
    /// deletes them: a directory with anything
    /// under it is `Status::IoError`, the root `Status::BadRequest`.
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        Tally::bump(&self.tally.deletes, &key(root.0, path));
        let vpath = self.graph.resolve(root, path)?;
        if is_root_key(&vpath) {
            return Err(Status::BadRequest);
        }
        let mut dirs = self.graph.dirs.lock().unwrap();
        let mut files = self.graph.files.lock().unwrap();
        let is_dir = dirs.contains_key(&vpath);
        if !is_dir && !files.contains_key(&vpath) {
            return Err(Status::NotFound);
        }
        if !self.graph.is_writable(&vpath) {
            return Err(Status::ReadOnly);
        }
        if !is_dir {
            files.remove(&vpath);
        } else if has_children(&dirs, &files, &vpath) {
            return Err(Status::IoError);
        } else {
            dirs.remove(&vpath);
        }
        Ok(())
    }

    /// Creates the directory with the caller's spelling (`docs/wire.md`
    /// 3.2, 3.4.9); the mode is ignored, as a director ignores it. A
    /// broken provider's prefix is `Status::IoError`; an existing directory
    /// `Status::Exists` (wire.md 3.4.9 lets a director answer OK or
    /// `ST_EXISTS`); an existing file `Status::IoError` where a writable
    /// provider serves it and `Status::Exists` where none does (3.4.9);
    /// outside every writable prefix it refuses as a write
    /// open does (3.4.3). Parents are not checked, as a creating OPEN does
    /// not check them.
    fn mkdir(&mut self, root: RootId, _: u32, path: &str) -> Result<(), Status> {
        let g = &self.graph;
        let vpath = g.resolve(root, path)?;
        if g.write_fails(&vpath) {
            return Err(Status::IoError);
        }
        let mut dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        if is_dir_in(&dirs, &vpath) {
            return Err(Status::Exists);
        }
        if files.contains_key(&vpath) {
            return Err(if g.is_writable(&vpath) {
                Status::IoError
            } else {
                Status::Exists
            });
        }
        if !g.is_writable(&vpath) {
            return Err(if g.root_writable(root.0) {
                Status::NotFound
            } else {
                Status::ReadOnly
            });
        }
        let name = created_spelling(&dirs, &files, root.0, path);
        drop(files);
        dirs.insert(vpath, name);
        Ok(())
    }

    /// Releases and counts the handle; an unknown one is `Status::BadFh`
    /// (`docs/wire.md` 3.4.10).
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        let Some(h) = self.graph.handles.lock().unwrap().remove(&fh) else {
            return Err(Status::BadFh);
        };
        Tally::bump(&self.tally.closed, &h.vpath);
        Ok(())
    }

    /// The stored spelling of each component of `path` from `skip` on,
    /// joined by `/` (`docs/wire.md` 3.4.12). A component with no entry comes
    /// back as sent (the SHOULD), so the reply always has as many names as
    /// components past `skip`. A `skip` past the last component is an empty
    /// reply (wire.md does not say).
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        self.graph.resolve(root, path)?;
        let dirs = self.graph.dirs.lock().unwrap();
        let files = self.graph.files.lock().unwrap();
        let names = stored_components(&dirs, &files, root.0, path);
        drop(files);
        drop(dirs);
        let skip = (skip as usize).min(names.len());
        self.names_buf.clear();
        self.names_buf.push_str(&names[skip..].join("/"));
        Ok(&self.names_buf)
    }

    /// A file moves to `to_root` (replacing what is there) and leaves
    /// `from_root`; a directory is `Status::CrossRoot`; equal roots are a
    /// RENAME (`docs/wire.md` 3.4.13). Otherwise it refuses as RENAME does,
    /// both ends checked before anything moves.
    fn rename_x(
        &mut self,
        from_root: RootId,
        to_root: RootId,
        from: &str,
        to: &str,
    ) -> Result<(), Status> {
        if escapes(from) || escapes(to) {
            return Err(Status::BadRequest);
        }
        if from_root == to_root {
            return self.rename(from_root, from, to);
        }
        let from_key = self.graph.resolve(from_root, from)?;
        self.graph.resolve(to_root, to)?;
        if self.graph.is_dir(&from_key) {
            return Err(Status::CrossRoot);
        }
        self.move_entry(from_key, to_root, to, true)
    }

    /// The reference stages nothing, which wire.md 3.4.14 says is answered
    /// first.
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

    /// A payload that does not decode is `Status::BadRequest` whatever the
    /// opcode (`docs/wire.md` 3.5); this override only keeps the registry
    /// tally: a registry request whose leading path decodes is counted when
    /// the reference has a registry layer.
    fn undecodable(&mut self, op: Opcode, _: u32, payload: &[u8]) -> Status {
        let registry = matches!(
            op,
            Opcode::RegLookup
                | Opcode::RegKey
                | Opcode::RegSetValue
                | Opcode::RegDeleteValue
                | Opcode::RegCreateKey
                | Opcode::RegDeleteKey
                | Opcode::RegRenameKey
                | Opcode::RegChanged
        );
        if registry && self.graph.registry.is_some() {
            if let Some(path) = leading_path(payload) {
                self.tally.bump_reg(op, path);
            }
        }
        Status::BadRequest
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
