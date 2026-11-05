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
//! stages nothing.
//!
//! **Test knobs.** [`Reference::set_mount_gen`] and
//! [`Reference::replace_content`] act as a remount would, and the builder
//! methods from [`Reference::with_fh_base`] to [`Reference::with_mtime`]
//! give answers other directors give and `docs/wire.md` allows (each names
//! its section). Each is off until a test sets it; with none set, every
//! answer below is as described, and conformance never sets one.
//!
//! **Layers.** A root may be served as an overlay: the graph's own files
//! and directories are its writable upper, over read-only layers (a tree's
//! `lower`, [`crate::tree::Layer`], or [`Reference::with_lower`]). Every
//! request is answered from the one tree they make ([`View`]): the upper
//! wins at a name, then the nearest layer; a DELETE or RENAME of a name a
//! layer holds leaves a whiteout that hides it, and a lower-only name is
//! answered as `docs/wire.md` 3.4.9's "Names only in a read-only lower"
//! says. A write open copies a lower-only file up, keeping its identity, so
//! every handle of it reads it as it is now (3.4.4). Where wire.md leaves
//! an overlay's answer open the reference answers as `docs/conformance.md`
//! ("Layered roots") says, and says so where it does. With no layers
//! every answer is the upper's alone, as before.
//!
//! A name keeps the spelling it was created with (a fixture's vpath, a
//! creating OPEN, MKDIR, a RENAME or RENAME_X target), and READDIR
//! and STORED_NAMES answer with it (wire.md 3.2). Where the fake it was
//! moved from differed from `docs/wire.md`, it answers as wire.md says: an
//! opcode outside the table or
//! a payload that does not decode is `Status::BadRequest` (3.3, 3.5); a path
//! that leaves its root is `Status::BadRequest` (3.2, 3.5.1); a root it does
//! not serve is `Status::NotFound`, or OK with nothing found for GETATTR
//! (3.4.1, 3.5.1); an unknown handle is `Status::BadFh` and a directory
//! handle `Status::IsDir` (3.4.4, 3.4.10, 3.5); names match by
//! `aether_casefold::fold` (3.2); it holds no named streams, so a path
//! naming one is a name it does not have (3.2).
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
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

/// The longest file a mounted tree may hold ([`crate::tree::Tree`]): larger
/// than [`MAX_FILE_LEN`], so a benchmark can read an archive of a game's
/// size. Such a file is read like any other; a WRITE ending past
/// [`MAX_FILE_LEN`] is still `Status::NoSpace`, and so is a SETATTR to any
/// size above it, a shrink of such a file included.
pub const MAX_MOUNTED_LEN: usize = 512 * 1024 * 1024;

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
    /// Held for the whole of each file request, so a request is served
    /// whole (wire.md 2.8.1: "the state before it or the state after it,
    /// never a mixture"); taken before every other lock.
    serve: Mutex<()>,
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
    /// The `mount_gen` every OPEN reports: 0 until a test moves it
    /// ([`Reference::set_mount_gen`]).
    mount_gen: AtomicU32,
    /// [`Reference::mkdir_existing_ok`].
    mkdir_existing_ok: bool,
    /// [`Reference::general_lower_rules`].
    general_lower: bool,
    /// [`Reference::refuse_layered_dir_renames`].
    refuse_layered_renames: bool,
    /// [`Reference::inline_reads`]: the `payload_cap` every READ is answered
    /// inline within.
    inline_cap: Option<u32>,
    /// [`Reference::staging`]: the real path STAGE_EXEC answers, by table key.
    staging: HashMap<String, String>,
    /// [`Reference::with_mtime`]: the mtime GETATTR and READDIR report, by
    /// table key.
    mtimes: HashMap<String, i64>,
    /// The read-only layers beneath the upper (`dirs` and `files`), the
    /// nearest first: a tree's `lower` layers ([`crate::tree::Layer`]) and
    /// [`Reference::with_lower`]'s. Fixed once built.
    layers: Vec<LowerLayer>,
    /// Table keys at which, and below which, the upper hides the layers: a
    /// whiteout left by a DELETE or RENAME of a name a layer holds, or the
    /// name an upper entry replaced one at (`docs/wire.md` 3.4.9, "Names
    /// only in a read-only lower"). Locked after `dirs` and `files`.
    masked: Mutex<HashSet<String>>,
    /// [`Reference::reissue_fh`]: the closed handles to issue again, lowest
    /// first; `None` when off.
    reissue: Option<Mutex<BTreeSet<u64>>>,
}

impl Graph {
    /// A root itself (`"."` on the wire) is always a directory.
    fn is_dir(&self, vpath: &str) -> bool {
        let dirs = self.dirs.lock().unwrap();
        if self.layers.is_empty() {
            return is_dir_in(&dirs, vpath);
        }
        let files = self.files.lock().unwrap();
        let masked = self.masked.lock().unwrap();
        self.view(&dirs, &files, &masked).is_dir(vpath)
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

    /// The mtime GETATTR and READDIR report for table key `k`: 0 unless
    /// [`Reference::with_mtime`] set one.
    fn mtime(&self, k: &str) -> i64 {
        self.mtimes.get(k).copied().unwrap_or(0)
    }

    /// The names the upper `dirs` and `files` show, with the layers beneath
    /// them and what `masked` hides of those.
    fn view<'a>(
        &'a self,
        dirs: &'a HashMap<String, String>,
        files: &'a HashMap<String, Entry>,
        masked: &'a HashSet<String>,
    ) -> View<'a> {
        View {
            dirs,
            files,
            layers: &self.layers,
            masked,
        }
    }

    /// The directories above table key `k` that only a layer holds, put in
    /// the upper with their stored spellings: a name made below a lower-only
    /// directory puts the directory in the upper (the reference's answer to
    /// a question wire.md leaves open, `docs/conformance.md`, "Layered
    /// roots").
    fn copy_up_parents(
        &self,
        dirs: &mut HashMap<String, String>,
        files: &HashMap<String, Entry>,
        masked: &HashSet<String>,
        k: &str,
    ) {
        if self.layers.is_empty() {
            return;
        }
        let up: Vec<(String, String)> = {
            let v = self.view(dirs, files, masked);
            ancestors(k)
                .filter(|a| !dirs.contains_key(a) && matches!(v.lower_at(a), Some(Low::Dir(_))))
                .map(|a| {
                    let name = v.stored(&a);
                    (a, name)
                })
                .collect()
        };
        dirs.extend(up);
    }

    /// The file only a layer holds at table key `k`, put in the upper with
    /// its bytes, its identity (its handles go on reading it, wire.md
    /// 3.4.4) and its stored spelling, and the directories above it with
    /// it; nothing when the upper holds `k`. A write open copies a file up
    /// (wire.md leaves open when; `docs/conformance.md`, "Layered roots"),
    /// as does a RENAME of it and a WRITE or SETATTR through a read handle.
    fn copy_up_file(
        &self,
        dirs: &mut HashMap<String, String>,
        files: &mut HashMap<String, Entry>,
        masked: &HashSet<String>,
        k: &str,
    ) {
        let copy = {
            let v = self.view(dirs, files, masked);
            match v.lower_only(k) {
                Some(Low::File(i)) => {
                    let e = &self.layers[i].files[k];
                    Some(Entry {
                        id: e.id,
                        bytes: e.bytes.clone(),
                        style: e.style,
                        name: v.stored(k),
                    })
                }
                _ => None,
            }
        };
        if let Some(e) = copy {
            files.insert(k.to_string(), e);
            self.copy_up_parents(dirs, files, masked, k);
        }
    }

    /// The upper's key of the file with identity `id`, copied up first if a
    /// layer still shows it (a WRITE or SETATTR through a read handle,
    /// which wire.md 3.4.5 lets a director honour); `None` once the file is
    /// gone (3.4.8).
    fn upper_file(
        &self,
        dirs: &mut HashMap<String, String>,
        files: &mut HashMap<String, Entry>,
        hint: &str,
        id: u64,
    ) -> Option<String> {
        let masked = self.masked.lock().unwrap();
        match locate_in(&self.view(dirs, files, &masked), hint, id)? {
            Loc::Upper(k) => Some(k),
            Loc::Lower(_, k) => {
                self.copy_up_file(dirs, files, &masked, &k);
                Some(k)
            }
        }
    }

    /// The directory only the layers show at table key `k`, put in the upper
    /// with the directories above it, and (`all`) every name the layers show
    /// below it: what [`Reference::general_lower_rules`] moves.
    fn copy_up_dir(
        &self,
        dirs: &mut HashMap<String, String>,
        files: &mut HashMap<String, Entry>,
        masked: &HashSet<String>,
        k: &str,
        all: bool,
    ) {
        let below = format!("{k}/");
        let mut keys: Vec<String> = self
            .layers
            .iter()
            .flat_map(|l| l.dirs.keys().chain(l.files.keys()))
            .filter(|p| *p == k || (all && p.starts_with(&below)))
            .cloned()
            .collect();
        keys.sort_by_key(|p| p.matches('/').count());
        keys.dedup();
        for p in keys {
            let shown = {
                let v = self.view(dirs, files, masked);
                v.lower_only(&p).map(|low| (low, v.stored(&p)))
            };
            match shown {
                Some((Low::Dir(_), name)) => {
                    self.copy_up_parents(dirs, files, masked, &p);
                    dirs.insert(p, name);
                }
                Some((Low::File(_), _)) => self.copy_up_file(dirs, files, masked, &p),
                None => {}
            }
        }
    }

    /// Hides what the layers hold at and below table key `k` once the upper
    /// has removed or replaced its own entry there (wire.md 3.4.8, 3.4.9: a
    /// deleted name is missing, a lower-only one hidden by a whiteout).
    fn hide_lower(&self, masked: &mut HashSet<String>, k: &str) {
        if lower_holds(&self.layers, k) {
            masked.insert(k.to_string());
        }
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

    /// [`Graph::resolve`] for a request that names an entry: a path naming a
    /// named stream (a component holding `:`) is `NotFound` too, since the
    /// reference holds no streams and never stores such a name (wire.md
    /// 3.2).
    fn resolve_name(&self, root: RootId, path: &str) -> Result<String, Status> {
        let k = self.resolve(root, path)?;
        if path.split('/').any(|c| c.contains(':')) {
            return Err(Status::NotFound);
        }
        Ok(k)
    }

    fn issue(&self, vpath: String, file: Option<u64>, append: bool, tally: &Tally) -> u64 {
        let reused = self
            .reissue
            .as_ref()
            .and_then(|r| r.lock().unwrap().pop_first());
        let fh = reused.unwrap_or_else(|| self.next_fh.fetch_add(1, Ordering::Relaxed));
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

    /// `Status::NotFound` unless the parent of table key `k` is a directory
    /// (`docs/wire.md` 3.4.3, 3.4.7, 3.4.9, 3.4.13): a root, a directory the
    /// table holds, or one it implies (something below it, or a writable
    /// prefix inside it). A file there is a missing directory too.
    /// A directory a layer holds is one too, unless the upper's file hides
    /// it, and a layer's file there is a missing directory as the upper's is.
    fn parent_exists(&self, v: &View<'_>, k: &str) -> Result<(), Status> {
        let (root, rest) = split_root(k);
        let Some((parent, _)) = rest.rsplit_once('/') else {
            return Ok(());
        };
        let parent = format!("{root}{parent}");
        let (dirs, files) = (v.dirs, v.files);
        if files.contains_key(&parent) {
            return Err(Status::NotFound);
        }
        let below = format!("{parent}/");
        let implied = dirs.contains_key(&parent)
            || has_children(dirs, files, &parent)
            || self.writable.iter().any(|w| w.starts_with(&below))
            || matches!(v.lower_at(&parent), Some(Low::Dir(_)));
        if implied {
            Ok(())
        } else {
            Err(Status::NotFound)
        }
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

/// Where the file with identity `id` is shown now: in the upper
/// ([`locate`]), or, until a copy-up, in the layer that shows it at its
/// key; `None` once it was deleted, replaced or hidden.
fn locate_in(v: &View<'_>, hint: &str, id: u64) -> Option<Loc> {
    if let Some(k) = locate(v.files, hint, id) {
        return Some(Loc::Upper(k));
    }
    v.layers.iter().enumerate().find_map(|(i, l)| {
        l.files
            .iter()
            .find(|(k, e)| e.id == id && v.lower_only(k) == Some(Low::File(i)))
            .map(|(k, _)| Loc::Lower(i, k.clone()))
    })
}

/// Where a handle's file is: the upper's key, or a layer and its key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Loc {
    Upper(String),
    Lower(usize, String),
}

/// A read-only layer beneath the upper: files and directories by table key,
/// as the graph's own (`Graph::layers`).
#[derive(Default)]
struct LowerLayer {
    files: HashMap<String, Entry>,
    dirs: HashMap<String, String>,
}

/// What a layer shows at a key: a directory or a file, and which layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Low {
    Dir(usize),
    File(usize),
}

/// The upper's entries and the layers beneath them, as one tree: what
/// every request is answered from. The upper wins at a name; below it the
/// nearest layer holding the name. A file hides what farther layers hold
/// at and below its name; a directory over a directory merges with it, a
/// directory over a file hides the file; a masked key hides every layer at
/// and below it. With no layers it is the upper alone.
struct View<'a> {
    dirs: &'a HashMap<String, String>,
    files: &'a HashMap<String, Entry>,
    layers: &'a [LowerLayer],
    masked: &'a HashSet<String>,
}

impl<'a> View<'a> {
    /// The layers whose directories make up directory `k`, nearest first.
    fn lower_dirs(&self, k: &str) -> Vec<usize> {
        if self.layers.is_empty() {
            return Vec::new();
        }
        if is_root_key(k) {
            return (0..self.layers.len()).collect();
        }
        if self.files.contains_key(k) || self.masked.contains(k) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in self.lower_dirs(&parent_key(k)) {
            let l = &self.layers[i];
            if l.dirs.contains_key(k) {
                out.push(i);
            } else if l.files.contains_key(k) {
                break;
            }
        }
        out
    }

    /// What the layers show at `k` beneath the upper, if anything: the
    /// nearest layer holding `k` of those making up its parent, unless the
    /// upper's file or a mask hides it.
    fn lower_at(&self, k: &str) -> Option<Low> {
        if self.layers.is_empty()
            || is_root_key(k)
            || self.files.contains_key(k)
            || self.masked.contains(k)
        {
            return None;
        }
        self.lower_dirs(&parent_key(k)).into_iter().find_map(|i| {
            let l = &self.layers[i];
            if l.dirs.contains_key(k) {
                Some(Low::Dir(i))
            } else {
                l.files.contains_key(k).then_some(Low::File(i))
            }
        })
    }

    /// What the layers show at `k` when the upper holds nothing there.
    fn lower_only(&self, k: &str) -> Option<Low> {
        if self.dirs.contains_key(k) || self.files.contains_key(k) {
            return None;
        }
        self.lower_at(k)
    }

    /// Whether `k` is a directory: the upper's, or a layer's the upper's
    /// file does not hide. A root itself is.
    fn is_dir(&self, k: &str) -> bool {
        is_dir_in(self.dirs, k) || matches!(self.lower_at(k), Some(Low::Dir(_)))
    }

    /// The file shown at `k`: the upper's, or a layer's.
    fn file(&self, k: &str) -> Option<&'a Entry> {
        if let Some(e) = self.files.get(k) {
            return Some(e);
        }
        match self.lower_only(k)? {
            Low::File(i) => self.layers[i].files.get(k),
            Low::Dir(_) => None,
        }
    }

    /// The stored spelling of the entry shown at `k`, as its full path.
    fn name(&self, k: &str) -> Option<&'a str> {
        if let Some(n) = self.dirs.get(k) {
            return Some(n);
        }
        if let Some(e) = self.files.get(k) {
            return Some(&e.name);
        }
        match self.lower_at(k)? {
            Low::Dir(i) => self.layers[i].dirs.get(k).map(String::as_str),
            Low::File(i) => self.layers[i].files.get(k).map(|e| e.name.as_str()),
        }
    }

    /// Whether anything is shown below directory `k`.
    fn has_children(&self, k: &str) -> bool {
        has_children(self.dirs, self.files, k)
            || self.lower_dirs(k).into_iter().any(|i| {
                self.lower_children(i, k)
                    .any(|c| self.lower_at(c).is_some())
            })
    }

    /// The keys layer `i` holds directly below directory `k`.
    fn lower_children(&self, i: usize, k: &str) -> impl Iterator<Item = &'a String> {
        let prefix = if is_root_key(k) {
            split_root(k).0.to_string()
        } else {
            format!("{k}/")
        };
        let l: &'a LowerLayer = &self.layers[i];
        l.dirs.keys().chain(l.files.keys()).filter(move |c| {
            c.strip_prefix(prefix.as_str())
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
        })
    }

    /// The stored spelling of every component of key `k`, joined: what a
    /// copy-up stores ([`stored_components`]).
    fn stored(&self, k: &str) -> String {
        let root = Graph::root_of(k).unwrap_or(0);
        stored_components(self, root, split_root(k).1).join("/")
    }
}

/// Whether any layer holds anything at or below table key `k`, shown or
/// not.
fn lower_holds(layers: &[LowerLayer], k: &str) -> bool {
    if layers.is_empty() {
        return false;
    }
    let below = format!("{k}/");
    layers.iter().any(|l| {
        l.dirs
            .keys()
            .chain(l.files.keys())
            .any(|p| p == k || p.starts_with(&below))
    })
}

/// A table key's root prefix (`""` for root 0, `@<id>/` for another) and
/// the path after it.
fn split_root(k: &str) -> (&str, &str) {
    match k.strip_prefix('@').and_then(|r| r.split_once('/')) {
        Some((id, rest)) => (&k[..id.len() + 2], rest),
        None => ("", k),
    }
}

/// The key of `k`'s parent directory: its root's (`.` after the prefix)
/// for a name directly under it.
fn parent_key(k: &str) -> String {
    let (root, rest) = split_root(k);
    match rest.rsplit_once('/') {
        Some((p, _)) => format!("{root}{p}"),
        None => format!("{root}."),
    }
}

/// The keys of the directories above `k`, the root aside, outermost first.
fn ancestors(k: &str) -> impl Iterator<Item = String> + '_ {
    let (root, rest) = split_root(k);
    rest.match_indices('/')
        .map(move |(i, _)| format!("{root}{}", &rest[..i]))
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
fn stored_components(v: &View<'_>, root: u32, path: &str) -> Vec<String> {
    let (dirs, files) = (v.dirs, v.files);
    let sent: Vec<&str> = path.split('/').collect();
    (0..sent.len())
        .map(|i| {
            let k = key(root, &sent[..=i].join("/"));
            let below = format!("{k}/");
            let exact = v.name(&k);
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
fn created_spelling(v: &View<'_>, root: u32, path: &str) -> String {
    match path.rsplit_once('/') {
        None => path.to_string(),
        Some((parent, leaf)) => {
            let mut c = stored_components(v, root, parent);
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
                serve: Mutex::new(()),
                next_fh: AtomicU64::new(1),
                next_id: AtomicU64::new(1),
                writable: Vec::new(),
                failing: Vec::new(),
                registry: None,
                roots: Vec::new(),
                mount_gen: AtomicU32::new(0),
                mkdir_existing_ok: false,
                general_lower: false,
                refuse_layered_renames: false,
                inline_cap: None,
                staging: HashMap::new(),
                mtimes: HashMap::new(),
                layers: Vec::new(),
                masked: Mutex::new(HashSet::new()),
                reissue: None,
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
    /// writing fails), a read-only one `Status::ReadOnly`, an exclusive open
    /// on a writable mount `Status::Exists`, and a creating open there
    /// `Status::IsDir`. On a directory handle the access
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

    /// Test knob: the `mount_gen` every later OPEN reports, as a director
    /// that replaced a root's content does (`docs/wire.md` 3.4.3). Until it
    /// is called every OPEN reports 0. Conformance never calls it.
    pub fn set_mount_gen(&self, g: u32) {
        self.graph.mount_gen.store(g, Ordering::Relaxed);
    }

    /// Test knob: `vpath` now holds `bytes`, as a remount gives one file new
    /// content; it keeps its stored spelling and read style, and one the
    /// graph did not hold is added. Handles open on it read the new bytes.
    /// Pair it with [`Reference::set_mount_gen`] for a remount a cache may
    /// rely on. Conformance never calls it.
    pub fn replace_content(&self, vpath: &str, bytes: Vec<u8>) {
        // Between requests, never inside one (wire.md 2.8.1).
        let _whole = self.graph.serve.lock().unwrap();
        let mut files = self.graph.files.lock().unwrap();
        files
            .entry(fold(vpath))
            .and_modify(|e| e.bytes.clone_from(&bytes))
            .or_insert_with(|| Entry {
                id: self.graph.new_id(),
                bytes: bytes.clone(),
                style: ReadStyle::Whole,
                name: spelling(vpath),
            });
    }

    /// Handles counted up from `n` instead of 1. Any `n` but 0 is an answer
    /// other directors give (`docs/wire.md` 3.4.3); `n = 0` is a director
    /// outside it (a director never issues 0), for a test that checks the
    /// shim gives 0 no meaning of its own. Off by default; conformance never
    /// sets it.
    pub fn with_fh_base(mut self, n: u64) -> Reference {
        self.building().next_fh = AtomicU64::new(n);
        self
    }

    /// An answer other directors give: a closed handle is issued again, the
    /// lowest first, before a new one is counted up. An `fh` need only be
    /// unique among open handles (`docs/wire.md` 3.4.3), so a client must
    /// not carry anything over from a handle closed before. Off by default
    /// (handles only count up); conformance never sets it.
    pub fn reissue_fh(mut self) -> Reference {
        self.building().reissue = Some(Mutex::new(BTreeSet::new()));
        self
    }

    /// An answer other directors give: MKDIR of a directory that exists
    /// under a writable prefix is `Status::Ok`, as a writable provider may
    /// answer, instead of `Status::Exists` (`docs/wire.md` 3.4.9 allows
    /// either). Off by default; conformance never sets it.
    /// An answer other directors give: lower-only names are answered by the
    /// rules for names the writable provider holds, which `docs/wire.md`
    /// 3.4.9 leaves an overlay free to give ("may answer ... as follows"):
    /// DELETE of a lower-only directory with something under it is
    /// `Status::IoError`, a case-only RENAME of a lower-only name copies it
    /// up and respells it, MKDIR over a lower-only file is
    /// `Status::IoError`, and RENAME of a directory a layer holds (alone or
    /// with the upper) copies the whole of it up and moves it (3.4.7: "or
    /// move it whole"). Off by default; conformance never sets it.
    pub fn general_lower_rules(mut self) -> Reference {
        self.building().general_lower = true;
        self
    }

    /// An answer other directors give: every RENAME of a directory the upper
    /// and a layer both hold is `Status::IoError`, a case-only one included
    /// (`docs/wire.md` 3.4.7: such a director "may refuse to move it").
    /// Off by default; conformance never sets it.
    pub fn refuse_layered_dir_renames(mut self) -> Reference {
        self.building().refuse_layered_renames = true;
        self
    }

    pub fn mkdir_existing_ok(mut self) -> Reference {
        self.building().mkdir_existing_ok = true;
        self
    }

    /// A director outside `docs/wire.md` 3.4.4: every READ is answered
    /// inline, at most `payload_cap - 8` bytes, whatever its flags and length
    /// ask for and whether or not the server has an arena. `payload_cap` is
    /// the ring's, which a director method is not told. Off by default;
    /// conformance never sets it.
    pub fn inline_reads(mut self, payload_cap: u32) -> Reference {
        self.building().inline_cap = Some(payload_cap);
        self
    }

    /// An answer other directors give: STAGE_EXEC of `vpath` answers
    /// `real_path` (`docs/wire.md` 3.4.14). Once any path is staged, one that
    /// is not is `Status::NotFound`, as a director with a stager answers;
    /// with none (the default) every STAGE_EXEC is `Status::NotSupported`.
    /// Conformance never sets it.
    pub fn staging(mut self, vpath: &str, real_path: &str) -> Reference {
        self.note_root(vpath);
        self.building()
            .staging
            .insert(fold(vpath), real_path.to_string());
        self
    }

    /// An answer other directors give: GETATTR and READDIR report `mtime`
    /// for the file or directory at `vpath` instead of 0 (`docs/wire.md`
    /// 3.4.1, 3.4.2; OPEN carries no time). Kept by vpath: a rename does not
    /// take it along. Off by default; conformance never sets it.
    pub fn with_mtime(mut self, vpath: &str, mtime: i64) -> Reference {
        self.building().mtimes.insert(fold(vpath), mtime);
        self
    }

    /// A file held only in a read-only lower beneath the graph (layer 0, a
    /// tree's first `lower` layer), answered as `docs/wire.md` 3.4.9's
    /// "Names only in a read-only lower" says an overlay may answer: DELETE
    /// hides it, RENAME copies it up (to another case of itself, `Status::Ok`
    /// with the lower's spelling kept), MKDIR over it makes the directory. A
    /// write open copies it up, after which it is an ordinary file. The
    /// directories above it are the layer's too. Meant under a
    /// [`Reference::writable_under`] prefix, as a tree's layers are.
    pub fn with_lower(self, vpath: &str, bytes: Vec<u8>) -> Reference {
        self.with_layer_file(0, vpath, bytes)
    }

    /// [`Reference::with_lower`] for a directory: DELETE hides it and
    /// everything under it, and RENAME to another name is
    /// `Status::IoError` (lower directories do not move).
    pub fn with_lower_dir(self, vpath: &str) -> Reference {
        self.with_layer_dir(0, vpath)
    }

    /// Layer `layer` (0 the nearest the upper), made with the ones before it
    /// if they are missing.
    fn layer(&mut self, layer: usize) -> &mut LowerLayer {
        let g = self.building();
        if g.layers.len() <= layer {
            g.layers.resize_with(layer + 1, LowerLayer::default);
        }
        &mut g.layers[layer]
    }

    /// A directory, and the ones above it, in read-only layer `layer`,
    /// spelled as `vpath` spells them unless the layer spells them already.
    pub(crate) fn with_layer_dir(mut self, layer: usize, vpath: &str) -> Reference {
        self.note_root(vpath);
        let k = fold(vpath);
        let name = spelling(vpath);
        let l = self.layer(layer);
        let mut at = Vec::new();
        for (i, c) in name.split('/').enumerate() {
            at.push(c);
            let up = ancestors(&k).nth(i).unwrap_or_else(|| k.clone());
            l.dirs.entry(up).or_insert_with(|| at.join("/"));
        }
        self
    }

    /// A file in read-only layer `layer`, its directories with it.
    pub(crate) fn with_layer_file(
        mut self,
        layer: usize,
        vpath: &str,
        bytes: Vec<u8>,
    ) -> Reference {
        if split_root(&fold(vpath)).1.contains('/') {
            if let Some((up, _)) = vpath.rsplit_once('/') {
                self = self.with_layer_dir(layer, up);
            }
        }
        self.note_root(vpath);
        let id = self.graph.new_id();
        let name = spelling(vpath);
        self.layer(layer).files.insert(
            fold(vpath),
            Entry {
                id,
                bytes,
                style: ReadStyle::Whole,
                name,
            },
        );
        self
    }

    /// The bytes read-only layer `layer` holds for `vpath`, shown or not:
    /// what no request through the ring changes.
    pub fn lower_contents(&self, layer: usize, vpath: &str) -> Option<Vec<u8>> {
        self.graph
            .layers
            .get(layer)?
            .files
            .get(&fold(vpath))
            .map(|e| e.bytes.clone())
    }

    /// The bytes the graph holds for `vpath` now: what a WRITE that crossed
    /// the ring left behind.
    pub fn contents(&self, vpath: &str) -> Option<Vec<u8>> {
        let g = &self.graph;
        let dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        let masked = g.masked.lock().unwrap();
        g.view(&dirs, &files, &masked)
            .file(&fold(vpath))
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
    /// `Status::IoError`, and an empty one or a file is replaced, but a
    /// directory onto a file is `Status::IoError`. A `to`
    /// whose parent is missing, or a file, is `Status::NotFound`. Across
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
        let mut masked = g.masked.lock().unwrap();
        if !across
            && (is_root_key(&from)
                || is_root_key(&to_key)
                || to_key.starts_with(&format!("{from}/")))
        {
            return Err(Status::BadRequest);
        }
        let v = g.view(&dirs, &files, &masked);
        let lower = v.lower_only(&from);
        let is_file = files.contains_key(&from) || matches!(lower, Some(Low::File(_)));
        if !is_file && !dirs.contains_key(&from) && lower.is_none() {
            return Err(Status::NotFound);
        }
        if !g.is_writable(&from) || !g.is_writable(&to_key) {
            return Err(Status::ReadOnly);
        }
        // A lower-only name (wire.md 3.4.9, "Names only in a read-only
        // lower"): another case of itself keeps the lower's spelling, a
        // directory does not move, a file is copied up and moves below.
        let layered_dir = !is_file && !v.lower_dirs(&from).is_empty();
        if layered_dir && lower.is_none() && g.refuse_layered_renames {
            return Err(Status::IoError);
        }
        if lower.is_some() && !g.general_lower {
            if to_key == from {
                return Ok(());
            }
            if !is_file {
                return Err(Status::IoError);
            }
        }
        // A directory a layer holds as well as the upper does not move
        // either (wire.md 3.4.7: an overlay may refuse to move it).
        if layered_dir && to_key != from && !g.general_lower {
            return Err(Status::IoError);
        }
        g.parent_exists(&v, &to_key)?;
        // A directory never replaces a file (wire.md 3.4.7).
        if !is_file && to_key != from && v.file(&to_key).is_some() {
            return Err(Status::IoError);
        }
        // A root is a directory too, though `dirs` does not hold it: within a
        // root it was refused above, across roots it is `IsDir` here.
        let onto_dir = to_key != from && (v.is_dir(&to_key) || is_root_key(&to_key));
        if onto_dir {
            if across {
                return Err(Status::IsDir);
            }
            if v.has_children(&to_key) {
                return Err(Status::IoError);
            }
        }
        // Nothing below fails. What is at `to` is replaced, a layer's too;
        // the source, if lower-only, is copied up now and its name hidden
        // once it has moved.
        if onto_dir {
            dirs.remove(&to_key);
        }
        if to_key != from {
            files.remove(&to_key);
            g.hide_lower(&mut masked, &to_key);
        }
        if g.general_lower && !is_file {
            // The whole of it moves (wire.md 3.4.7), so the whole of it is
            // copied up first; a case-only RENAME copies the name alone.
            g.copy_up_dir(&mut dirs, &mut files, &masked, &from, to_key != from);
        }
        g.copy_up_file(&mut dirs, &mut files, &masked, &from);
        g.copy_up_parents(&mut dirs, &files, &masked, &to_key);
        if to_key != from {
            g.hide_lower(&mut masked, &from);
        }
        let v = g.view(&dirs, &files, &masked);
        let name = created_spelling(&v, to_root.0, to);
        if is_file {
            let mut e = files.remove(&from).expect("just checked");
            e.name = name;
            files.insert(to_key, e);
            return Ok(());
        }
        // A directory: it and everything under it, each respelled under
        // the new name.
        let old = dirs.remove(&from).expect("just checked");
        let depth = old.split('/').count();
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

impl Reference {
    /// RENAME, for a caller already holding the serve lock (RENAME and
    /// RENAME_X between equal roots).
    fn rename_whole(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        Tally::bump(&self.tally.renames, &key(root.0, from));
        if escapes(to) {
            return Err(Status::BadRequest);
        }
        let from_key = self.graph.resolve_name(root, from)?;
        self.graph.resolve_name(root, to)?;
        self.move_entry(from_key, root, to, false)
    }
}

impl Director for Reference {
    fn heartbeat(&mut self) -> Result<(), Status> {
        Ok(())
    }

    /// A missing path, and any path on a root not served, is OK with
    /// nothing found (`docs/wire.md` 3.4.1).
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        let vpath = match self.graph.resolve_name(root, path) {
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
                mtime: self.graph.mtime(&vpath),
            });
        }
        let g = &self.graph;
        let dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        let masked = g.masked.lock().unwrap();
        let v = g.view(&dirs, &files, &masked);
        let found = v.file(&vpath);
        Ok(AttrResp {
            found: found.is_some(),
            is_dir: false,
            size: found.map(|e| e.bytes.len() as u64).unwrap_or(0),
            mtime: found.map_or(0, |_| self.graph.mtime(&vpath)),
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
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        // The shim sends "." for the root itself.
        let dir = match path {
            "." | "" => self.graph.resolve(root, "")?,
            v => self.graph.resolve(root, v)?,
        };
        let dir_key = if is_root_key(&dir) {
            format!("{}.", split_root(&dir).0)
        } else {
            dir.clone()
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
        let files = self.graph.files.lock().unwrap();
        for (p, e) in files.iter() {
            if let Some(name) = child(p) {
                push(name, &e.name, false, e.bytes.len() as u64);
            }
        }
        // Then what the layers show below it, each name once, the upper's
        // and the nearer layer's first (wire.md 3.4.9: an upper over a
        // read-only lower).
        let masked = self.graph.masked.lock().unwrap();
        let v = self.graph.view(&dirs, &files, &masked);
        for i in v.lower_dirs(&dir_key) {
            for c in v.lower_children(i, &dir_key) {
                let (Some(name), Some(hit)) = (child(c), v.lower_at(c)) else {
                    continue;
                };
                match hit {
                    Low::Dir(j) => push(name, &v.layers[j].dirs[c], true, 0),
                    Low::File(j) => {
                        let e = &v.layers[j].files[c];
                        push(name, &e.name, false, e.bytes.len() as u64);
                    }
                }
            }
        }
        drop(masked);
        drop(files);
        drop(dirs);
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (folded, name, is_dir, size) in &rows {
            out.push(DirEntry {
                name,
                is_dir: *is_dir,
                size: *size,
                mtime: if self.graph.mtimes.is_empty() {
                    0
                } else {
                    self.graph.mtime(&format!("{prefix}{folded}"))
                },
            });
        }
        Ok(())
    }

    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        let g = &self.graph;
        self.tally
            .wire_opens
            .lock()
            .unwrap()
            .push((flags.bits(), path.to_string()));
        let vpath = g.resolve_name(root, path)?;
        let write = flags.contains(OpenFlags::WRITE);
        let append = flags.contains(OpenFlags::APPEND);
        if write && g.write_fails(&vpath) {
            return Err(Status::IoError);
        }
        if g.is_dir(&vpath) {
            if write {
                // The answers a real graph gives, each its own code path
                // (see `with_dir`): a read-only mount refuses on access; an
                // exclusive create finds the name taken, whatever is there
                // (wire.md 3.4.3); an overlay whose base holds the directory
                // refuses a creating open; a writable disk provider tries to
                // open the directory for writing and fails.
                return Err(if !g.is_writable(&vpath) {
                    Status::ReadOnly
                } else if flags.contains(OpenFlags::EXCL) {
                    Status::Exists
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
                mount_gen: g.mount_gen.load(Ordering::Relaxed),
                ..Default::default()
            });
        }
        let mut dirs = g.dirs.lock().unwrap();
        let mut files = g.files.lock().unwrap();
        let masked = g.masked.lock().unwrap();
        let exists = g.view(&dirs, &files, &masked).file(&vpath).is_some();
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
                let v = g.view(&dirs, &files, &masked);
                g.parent_exists(&v, &vpath)?;
                // A create keeps the caller's spelling (wire.md 3.2).
                let name = created_spelling(&v, root.0, path);
                g.copy_up_parents(&mut dirs, &files, &masked, &vpath);
                files.insert(
                    vpath.clone(),
                    Entry {
                        id: g.new_id(),
                        bytes: Vec::new(),
                        style: ReadStyle::Whole,
                        name,
                    },
                );
            } else {
                // A lower-only file a write open reaches is in the upper now,
                // with its bytes and its handles.
                g.copy_up_file(&mut dirs, &mut files, &masked, &vpath);
                if flags.contains(OpenFlags::TRUNC) {
                    files.get_mut(&vpath).expect("just checked").bytes.clear();
                }
            }
        }
        let Some(e) = g.view(&dirs, &files, &masked).file(&vpath) else {
            return Err(Status::NotFound);
        };
        let size = e.bytes.len() as u64;
        let id = e.id;
        drop(masked);
        drop(files);
        drop(dirs);
        // A read open of a file a read-only provider holds is immutable
        // (wire.md 3.4.3): nothing the reference serves changes it (a write
        // open, DELETE, RENAME, RENAME_X and MKDIR of it are refused, and so
        // are WRITE and SETATTR through the read handle), and
        // the reference never replaces a root's content, so `mount_gen`
        // stays 0 unless a test moves it ([`Reference::set_mount_gen`]).
        // Never on a write open, a directory, or a file the writable
        // provider holds.
        let immutable = !write && !g.is_writable(&vpath);
        let fh = g.issue(vpath, Some(id), append, &self.tally);
        Ok(OpenResp {
            fh,
            size,
            is_dir: false,
            immutable,
            mount_gen: g.mount_gen.load(Ordering::Relaxed),
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
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        let Some(Handle { vpath, file, .. }) = self.graph.handle(fh) else {
            return Err(Status::BadFh);
        };
        Tally::bump(&self.tally.reads, &vpath);
        let Some(id) = file else {
            return Err(Status::IsDir);
        };
        let g = &self.graph;
        let dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        let masked = g.masked.lock().unwrap();
        // A file deleted, or replaced by a rename onto its name, under an
        // open handle (wire.md 3.4.8); a layer's file until a copy-up, the
        // upper's after it.
        let e = match locate_in(&g.view(&dirs, &files, &masked), &vpath, id) {
            Some(Loc::Upper(k)) => &files[&k],
            Some(Loc::Lower(i, k)) => &g.layers[i].files[&k],
            None => return Err(Status::IoError),
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
        // An inline-only director ([`Reference::inline_reads`]) answers what
        // fits the slot, never in bulk.
        if let Some(cap) = self.graph.inline_cap {
            want = want.min(cap.saturating_sub(8) as usize);
        }
        let src = &e.bytes[start..start + want];
        // Bulk chosen as a director chooses it, so a fixture gets
        // production's transport.
        let want_bulk = self.graph.inline_cap.is_none()
            && (slot_flags.contains(RequestFlags::READ_BULK) || len >= BULK_THRESHOLD);
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
        drop(masked);
        drop(files);
        drop(dirs);
        Ok(ReadReply::Inline(&self.read_buf))
    }

    /// On an append handle the offset is ignored (`docs/wire.md` 3.4.3).
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
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
        let mut dirs = self.graph.dirs.lock().unwrap();
        let mut files = self.graph.files.lock().unwrap();
        let Some(k) = self.graph.upper_file(&mut dirs, &mut files, &vpath, id) else {
            return Err(Status::NotFound);
        };
        let e = files.get_mut(&k).expect("just located");
        // A zero-length WRITE changes nothing, wherever its offset (wire.md
        // 3.4.5): no growth to the offset, and no size limit to pass.
        if data.is_empty() {
            return Ok(0);
        }
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
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
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
        let mut dirs = self.graph.dirs.lock().unwrap();
        let mut files = self.graph.files.lock().unwrap();
        let Some(k) = self.graph.upper_file(&mut dirs, &mut files, &vpath, id) else {
            return Err(Status::NotFound);
        };
        let e = files.get_mut(&k).expect("just located");
        e.bytes.resize(size as usize, 0);
        Ok(())
    }

    /// RENAME and DELETE refuse as a real overlay-backed graph does: a vpath
    /// nothing serves is `Status::NotFound`, and one outside every writable
    /// mount `Status::ReadOnly`. The target keeps the caller's spelling
    /// (`docs/wire.md` 3.2), so a rename to another case respells the file.
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        self.rename_whole(root, from, to)
    }

    /// A file, or a directory with nothing under it, as `docs/wire.md` 3.4.8
    /// deletes them: a directory with anything
    /// under it is `Status::IoError`, the root `Status::BadRequest`.
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        Tally::bump(&self.tally.deletes, &key(root.0, path));
        let vpath = self.graph.resolve_name(root, path)?;
        if is_root_key(&vpath) {
            return Err(Status::BadRequest);
        }
        let g = &self.graph;
        let mut dirs = g.dirs.lock().unwrap();
        let mut files = g.files.lock().unwrap();
        let mut masked = g.masked.lock().unwrap();
        let v = g.view(&dirs, &files, &masked);
        let is_dir = dirs.contains_key(&vpath);
        let lower = v.lower_only(&vpath).is_some();
        if !is_dir && !files.contains_key(&vpath) && !lower {
            return Err(Status::NotFound);
        }
        if !g.is_writable(&vpath) {
            return Err(Status::ReadOnly);
        }
        if lower && g.general_lower && v.is_dir(&vpath) && v.has_children(&vpath) {
            return Err(Status::IoError);
        }
        if lower {
            // A whiteout hides a lower-only name and everything under it
            // (wire.md 3.4.9, "Names only in a read-only lower").
            masked.insert(vpath);
            return Ok(());
        }
        if is_dir && v.has_children(&vpath) {
            return Err(Status::IoError);
        }
        if is_dir {
            dirs.remove(&vpath);
        } else {
            files.remove(&vpath);
        }
        // A deleted name is missing (wire.md 3.4.8): a layer's entry there
        // does not show through.
        g.hide_lower(&mut masked, &vpath);
        Ok(())
    }

    /// Creates the directory with the caller's spelling (`docs/wire.md`
    /// 3.2, 3.4.9); the mode is ignored, as a director ignores it. A
    /// broken provider's prefix is `Status::IoError`; an existing directory
    /// `Status::Exists` (wire.md 3.4.9 lets a director answer OK or
    /// `ST_EXISTS`); an existing file `Status::IoError` where a writable
    /// provider serves it and `Status::Exists` where none does (3.4.9);
    /// outside every writable prefix it refuses as a write
    /// open does (3.4.3). A missing parent, or a file there, is
    /// `Status::NotFound`, as for a creating OPEN.
    fn mkdir(&mut self, root: RootId, _: u32, path: &str) -> Result<(), Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        let g = &self.graph;
        let vpath = g.resolve_name(root, path)?;
        if g.write_fails(&vpath) {
            return Err(Status::IoError);
        }
        let mut dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        let mut masked = g.masked.lock().unwrap();
        let v = g.view(&dirs, &files, &masked);
        if v.is_dir(&vpath) {
            // A writable provider may answer OK ([`mkdir_existing_ok`]). A
            // directory only a layer holds is answered as any other: wire.md
            // 3.4.9 allows OK or `ST_EXISTS` for an existing directory
            // (`docs/conformance.md`, "Layered roots").
            return if g.mkdir_existing_ok && g.is_writable(&vpath) {
                Ok(())
            } else {
                Err(Status::Exists)
            };
        }
        if matches!(v.lower_only(&vpath), Some(Low::File(_)))
            && g.is_writable(&vpath)
            && !g.general_lower
        {
            // The directory, made in the upper, hides the lower-only file
            // (wire.md 3.4.9, "Names only in a read-only lower").
            let name = created_spelling(&v, root.0, path);
            masked.insert(vpath.clone());
            g.copy_up_parents(&mut dirs, &files, &masked, &vpath);
            dirs.insert(vpath, name);
            return Ok(());
        }
        if v.file(&vpath).is_some() {
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
        g.parent_exists(&v, &vpath)?;
        let name = created_spelling(&v, root.0, path);
        g.copy_up_parents(&mut dirs, &files, &masked, &vpath);
        dirs.insert(vpath, name);
        Ok(())
    }

    /// Releases and counts the handle; an unknown one is `Status::BadFh`
    /// (`docs/wire.md` 3.4.10).
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        let Some(h) = self.graph.handles.lock().unwrap().remove(&fh) else {
            return Err(Status::BadFh);
        };
        if let Some(r) = &self.graph.reissue {
            r.lock().unwrap().insert(fh);
        }
        Tally::bump(&self.tally.closed, &h.vpath);
        Ok(())
    }

    /// The stored spelling of each component of `path` from `skip` on,
    /// joined by `/` (`docs/wire.md` 3.4.12). A component with no entry comes
    /// back as sent (the SHOULD), so the reply always has as many names as
    /// components past `skip`. A `skip` past the last component is an empty
    /// reply (wire.md does not say).
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        self.graph.resolve_name(root, path)?;
        let g = &self.graph;
        let dirs = g.dirs.lock().unwrap();
        let files = g.files.lock().unwrap();
        let masked = g.masked.lock().unwrap();
        let names = stored_components(&g.view(&dirs, &files, &masked), root.0, path);
        drop(masked);
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
        let graph = self.graph.clone();
        let _whole = graph.serve.lock().unwrap();
        if escapes(from) || escapes(to) {
            return Err(Status::BadRequest);
        }
        if from_root == to_root {
            return self.rename_whole(from_root, from, to);
        }
        let from_key = self.graph.resolve_name(from_root, from)?;
        self.graph.resolve_name(to_root, to)?;
        if self.graph.is_dir(&from_key) {
            return Err(Status::CrossRoot);
        }
        self.move_entry(from_key, to_root, to, true)
    }

    /// The reference stages nothing, which wire.md 3.4.14 says is answered
    /// first, unless a test staged a path ([`Reference::staging`]): then a
    /// path it did not stage is `Status::NotFound`, as a director that
    /// stages may answer.
    fn stage_exec(&mut self, root: RootId, path: &str) -> Result<&str, Status> {
        if self.graph.staging.is_empty() {
            return Err(Status::NotSupported);
        }
        let k = self.graph.resolve_name(root, path)?;
        self.graph
            .staging
            .get(&k)
            .map(String::as_str)
            .ok_or(Status::NotFound)
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
