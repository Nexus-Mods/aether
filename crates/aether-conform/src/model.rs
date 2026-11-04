//! What a generated case is, and how it becomes a scenario: the generator
//! behind `aether-conform run --generate` ([`crate::generate`]).
//!
//! A case is a [`CaseInput`]: a [`TreeSpec`] (the roots, files, directories,
//! writable entries and registry keys to mount) and a list of [`RawOp`]s,
//! each an operation kind and four numbers. Both are plain proptest values,
//! so proptest generates and shrinks them. [`build`] turns one into a
//! [`Scenario`]: it builds the tree, mounts it in an in-process [`Oracle`]
//! (the reference director, `aether_refdirector::Reference`, behind a ring
//! in memory), and reads the ops one at a time against the oracle's state.
//! Each op's numbers pick its targets among what exists at that point (an
//! open handle, an existing file, a missing name under an existing
//! directory), so every shrunk case is still a valid one; an op with nothing
//! to act on is dropped. The oracle's answer to each request becomes that
//! step's expectation.
//!
//! The generator only sends what `docs/wire.md` fixes the answer to, as
//! `contract/scenarios/README.md` reads a tree ("Who holds what"), and where
//! wire.md allows several answers the expectation is a `one_of` of exactly
//! those, citing the section. For each request it also states which answer
//! wire.md gives ([`Want`]); an oracle that answers otherwise is a
//! disagreement between the reference and wire.md, and [`build`] fails
//! with both, rather than make the reference's answer the expectation.
//!
//! What it leaves out, and why, is listed in `docs/conformance.md`
//! ("Generated cases"). In short: names whose answer depends on the
//! director's layering (a writable entry's own directory, its ancestors);
//! creates under a missing parent; writes past the end and reads of the gap
//! SETATTR leaves; WRITE and SETATTR on a handle opened read-only, and READ
//! on one opened write-only; anything but CLOSE on a handle whose file was
//! changed through another handle or a path (rename, delete, truncate);
//! READDIR of a missing path or a file; a path below a file; paths that
//! leave their root (`escape-paths` covers them); a RENAME_X onto a
//! directory or with a read-only end; registry requests near a limit.

use std::collections::{BTreeMap, BTreeSet};

use aether_casefold::fold;
use aether_ipc::{ring, DataArena, Geom, OwnedSeg, RingServer, SpinNotifier};
use aether_protocol::{OpenFlags, Reply, RequestFlags, RootId, Status};
use aether_refdirector::tree::{fill, Contents, File, RegKey, RegValue, Registry, Root, Tree};
use aether_refdirector::{Reference, BULK_THRESHOLD};
use aether_rpc::Scratch;
use proptest::prelude::*;

use crate::scenario::{Expect, OwnedReply, OwnedRequest, Scenario, Step};

// ---------------------------------------------------------------------------
// The oracle.

/// The oracle's ring: one slot of the reference adapter's payload cap, and a
/// 1 MiB bank for bulk reads.
const ORACLE_CAP: u32 = aether_ipc::DEFAULT_PAYLOAD_CAP;
const ORACLE_BANK: usize = 1024 * 1024;

/// The reference director serving a tree behind a ring held in memory, and
/// answering one request at a time on the caller's thread: the request is
/// published, served with the loop a Rust director uses
/// (`aether_rpc::serve_one_with_arena`, so a registry reply's
/// `payload_cap - 8` rule and the status for an undecodable payload are the
/// serve loop's), and its reply taken.
pub struct Oracle {
    seg: OwnedSeg,
    geom: Geom,
    arena_offset: usize,
    d: Reference,
    scratch: Scratch,
}

impl Oracle {
    /// The reference serving `tree` ([`Reference::from_tree`]).
    pub fn new(tree: &Tree) -> Result<Oracle, String> {
        let d = Reference::from_tree(tree)?;
        let stride = (32 + ORACLE_CAP as usize).next_multiple_of(8);
        let arena_offset = aether_ipc::layout::RING_HEADER_SIZE + stride;
        let seg = OwnedSeg::new(arena_offset + ORACLE_BANK);
        let geom = ring::init(seg.seg(), 1, ORACLE_CAP).map_err(|e| format!("{e:?}"))?;
        Ok(Oracle {
            seg,
            geom,
            arena_offset,
            d,
            scratch: Scratch::default(),
        })
    }

    /// The status and reply payload for a request with these slot fields,
    /// and for a bulk READ reply the data it points at.
    fn raw(&mut self, opcode: u32, flags: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        let seg = self.seg.seg();
        let slot = ring::claim_free(seg, &self.geom).expect("the oracle's one slot is free");
        let req_id = ring::publish_request(seg, &self.geom, slot, opcode, flags, payload)
            .expect("a request that fits the oracle's slot");
        let server = RingServer::new(seg, SpinNotifier).expect("the oracle's ring opens");
        let arena = DataArena::new(seg, self.arena_offset, ORACLE_BANK, 1);
        let served =
            aether_rpc::serve_one_with_arena(&server, Some(&arena), &mut self.d, &mut self.scratch);
        assert_eq!(
            served,
            Ok(true),
            "the oracle serves the request it was given"
        );
        let reply = ring::take_response(seg, &self.geom, slot, req_id)
            .expect("the oracle's reply")
            .expect("answered");
        ring::free_slot(seg, &self.geom, slot).expect("the slot is the caller's");
        reply
    }

    /// The reference's answer to `req`; a bulk READ reply is read from the
    /// arena.
    pub fn ask(&mut self, req: &OwnedRequest) -> Result<OwnedReply, Status> {
        let request = req.as_request();
        let mut body = Vec::new();
        request.encode(&mut body);
        let (op, flags) = (request.opcode(), request.flags());
        let (status, payload) = self.raw(op as u32, flags, &body);
        if status != Status::Ok.code() {
            return Err(Status::try_from(status).expect("the reference answers table statuses"));
        }
        let reply = Reply::decode(op, flags, &payload).expect("the reference's reply decodes");
        let seg = self.seg.seg();
        Ok(OwnedReply::from_reply(&reply).resolve_bulk(|len, off| {
            seg.read_bytes(off as usize, len as usize)
                .expect("a bulk reply inside the oracle's bank")
        }))
    }
}

// ---------------------------------------------------------------------------
// What proptest generates.

/// One generated case: a tree and the operations to read against it.
#[derive(Debug, Clone)]
pub struct CaseInput {
    pub tree: TreeSpec,
    pub ops: Vec<RawOp>,
}

/// The tree to mount, as numbers proptest can shrink; [`build`] makes a
/// valid [`Tree`] of any of them.
#[derive(Debug, Clone)]
pub struct TreeSpec {
    /// Every root writable as a whole (`"."`). The reference cannot make
    /// root 0 writable as a whole and another root not, so it is all or
    /// none.
    pub whole: bool,
    /// Root 0, then up to two more (ids [`ROOT_IDS`]).
    pub roots: Vec<RootSpec>,
    /// A registry layer and the keys under the base key; `None` is none.
    pub registry: Option<Vec<KeySpec>>,
}

/// One root: which of its directories are writable entries (`None`: no
/// writable provider) and what is under it.
#[derive(Debug, Clone)]
pub struct RootSpec {
    pub writable: Option<u32>,
    pub entries: Vec<EntrySpec>,
}

/// A file or directory: under the directory `parent` picks, named from
/// [`NAMES`] by `name` and spelled by `case`.
#[derive(Debug, Clone)]
pub struct EntrySpec {
    pub parent: u8,
    pub name: u8,
    pub case: u8,
    pub file: Option<FileSpec>,
}

/// A file's contents.
#[derive(Debug, Clone)]
pub enum FileSpec {
    /// One of [`TEXTS`].
    Text(u8),
    /// `size` generated bytes.
    Fill { size: u32, seed: u8 },
}

/// A registry key under the key `parent` picks, named from [`KEY_NAMES`].
#[derive(Debug, Clone)]
pub struct KeySpec {
    pub parent: u8,
    pub name: u8,
    pub volatile: bool,
    pub values: Vec<ValueSpec>,
}

/// A value: a name from [`VALUE_NAMES`], a type from [`VALUE_TYPES`], and
/// `len % 24` generated bytes.
#[derive(Debug, Clone)]
pub struct ValueSpec {
    pub name: u8,
    pub ty: u8,
    pub len: u8,
    pub seed: u8,
}

/// One operation: its kind and four numbers that pick its targets and
/// arguments against the state it meets.
#[derive(Debug, Clone, Copy)]
pub struct RawOp {
    pub kind: Kind,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
}

/// The kinds of operation. Earlier ones are what proptest shrinks towards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heartbeat,
    GetAttr,
    ReadDir,
    OpenRead,
    OpenWrite,
    Read,
    Write,
    SetAttr,
    Close,
    StaleHandle,
    InvalidHandle,
    Rename,
    RenameX,
    Delete,
    Mkdir,
    StoredNames,
    StageExec,
    RegLookup,
    RegKey,
    RegSetValue,
    RegDeleteValue,
    RegCreateKey,
    RegDeleteKey,
    RegRenameKey,
    RegChanged,
    RegBadPath,
}

/// The ids of root 0 and the two others a tree may have; [`UNKNOWN_ROOTS`]
/// are never served.
pub const ROOT_IDS: [u32; 3] = [0, 1, 5];
const UNKNOWN_ROOTS: [u32; 2] = [7, u32::MAX];

/// File and directory names. Unicode ones whose case variants fold back to
/// them, none that a Windows file system refuses or folds differently from
/// `aether_casefold::fold` (no final sigma, no dotted capital I).
pub const NAMES: [&str; 13] = [
    "a",
    "b.txt",
    "Data",
    "saves",
    "Mods",
    "x.esp",
    "Readme.md",
    "été.txt",
    "ΣΑΣ",
    "жук.bin",
    "two words",
    "UPPER.DAT",
    "q",
];

/// Text contents.
pub const TEXTS: [&str; 7] = [
    "",
    "a",
    "abc",
    "hello, world",
    "0123456789",
    "élan vital",
    "line one\nline two\n",
];

const REG_BASE: &str = r"\Registry\Machine\Software\Conform";
pub const KEY_NAMES: [&str; 6] = ["Sub", "Ünï", "deep", "Other", "x", "Größe"];
pub const VALUE_NAMES: [&str; 4] = ["v", "Value", "Größe", "count"];
pub const VALUE_TYPES: [u32; 6] = [1, 3, 4, 7, 11, 0x1234];

/// Registry paths that are not canonical (wire.md 4.2), as
/// `contract/scenarios/registry-bad-path.json` sends them.
const BAD_KEY_PATHS: [&str; 9] = [
    "",
    "\\",
    r"Registry\Machine\Software",
    r"\Machine\Software",
    r"\Registry\\Machine",
    r"\Registry\Machine\Software\",
    r"\Registry\Machine\.",
    r"\Registry\Machine\..\Software",
    "/Registry/Machine/Software",
];

fn file_spec() -> impl Strategy<Value = FileSpec> {
    prop_oneof![
        4 => any::<u8>().prop_map(FileSpec::Text),
        3 => (0u32..2000, any::<u8>()).prop_map(|(size, seed)| FileSpec::Fill { size, seed }),
        1 => (65_536u32..100_000, any::<u8>()).prop_map(|(size, seed)| FileSpec::Fill { size, seed }),
    ]
}

fn entry_spec() -> impl Strategy<Value = EntrySpec> {
    (
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        proptest::option::weighted(0.6, file_spec()),
    )
        .prop_map(|(parent, name, case, file)| EntrySpec {
            parent,
            name,
            case,
            file,
        })
}

fn root_spec() -> impl Strategy<Value = RootSpec> {
    (
        proptest::option::weighted(0.7, any::<u32>()),
        proptest::collection::vec(entry_spec(), 0..12),
    )
        .prop_map(|(writable, entries)| RootSpec { writable, entries })
}

fn key_spec() -> impl Strategy<Value = KeySpec> {
    let value =
        (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(name, ty, len, seed)| {
            ValueSpec {
                name,
                ty,
                len,
                seed,
            }
        });
    (
        any::<u8>(),
        any::<u8>(),
        proptest::bool::weighted(0.2),
        proptest::collection::vec(value, 0..3),
    )
        .prop_map(|(parent, name, volatile, values)| KeySpec {
            parent,
            name,
            volatile,
            values,
        })
}

fn tree_spec() -> impl Strategy<Value = TreeSpec> {
    (
        proptest::bool::weighted(0.25),
        proptest::collection::vec(root_spec(), 1..=3),
        proptest::option::weighted(0.4, proptest::collection::vec(key_spec(), 0..5)),
    )
        .prop_map(|(whole, roots, registry)| TreeSpec {
            whole,
            roots,
            registry,
        })
}

fn kind() -> impl Strategy<Value = Kind> {
    use Kind as K;
    prop_oneof![
        1 => Just(K::Heartbeat),
        6 => Just(K::GetAttr),
        3 => Just(K::ReadDir),
        6 => Just(K::OpenRead),
        7 => Just(K::OpenWrite),
        8 => Just(K::Read),
        7 => Just(K::Write),
        3 => Just(K::SetAttr),
        5 => Just(K::Close),
        2 => Just(K::StaleHandle),
        1 => Just(K::InvalidHandle),
        5 => Just(K::Rename),
        3 => Just(K::RenameX),
        4 => Just(K::Delete),
        4 => Just(K::Mkdir),
        3 => Just(K::StoredNames),
        1 => Just(K::StageExec),
        2 => Just(K::RegLookup),
        2 => Just(K::RegKey),
        2 => Just(K::RegSetValue),
        1 => Just(K::RegDeleteValue),
        2 => Just(K::RegCreateKey),
        1 => Just(K::RegDeleteKey),
        1 => Just(K::RegRenameKey),
        2 => Just(K::RegChanged),
        1 => Just(K::RegBadPath),
    ]
}

fn raw_op() -> impl Strategy<Value = RawOp> {
    (
        kind(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(|(kind, a, b, c, d)| RawOp { kind, a, b, c, d })
}

/// Cases of at most `max_ops` operations.
pub fn case_strategy(max_ops: usize) -> impl Strategy<Value = CaseInput> {
    (
        tree_spec(),
        proptest::collection::vec(raw_op(), 1..=max_ops.max(1)),
    )
        .prop_map(|(tree, ops)| CaseInput { tree, ops })
}

// ---------------------------------------------------------------------------
// Spelling.

/// `s` with the case of some characters changed, as `bits` picks, keeping
/// its fold: a character changes only to a single character whose fold is
/// its own.
pub fn respell(s: &str, bits: u32) -> String {
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            let own = c.to_string();
            if bits >> (i % 32) & 1 == 0 {
                return own;
            }
            let other: String = if c.is_lowercase() {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            };
            if other.chars().count() == 1 && fold(&other) == fold(&own) {
                other
            } else {
                own
            }
        })
        .collect()
}

/// Every component of `path` respelled ([`respell`]), each with its own
/// bits; `"."` as it is.
fn respell_path(path: &str, bits: u32) -> String {
    if path == "." {
        return path.to_string();
    }
    path.split('/')
        .enumerate()
        .map(|(i, c)| respell(c, bits.rotate_left(7 * i as u32)))
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` under `dir` (`"."` for the root).
fn join(dir: &str, leaf: &str) -> String {
    if dir == "." || dir.is_empty() {
        leaf.to_string()
    } else {
        format!("{dir}/{leaf}")
    }
}

/// The parent of a folded path, `"."` for a top-level name.
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or(".")
}

/// The last component.
fn leaf(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, l)| l).unwrap_or(path)
}

/// Whether folded `path` is `top` or below it (everything is below the root).
fn under(path: &str, top: &str) -> bool {
    top == "." || path == top || path.starts_with(&format!("{top}/"))
}

// ---------------------------------------------------------------------------
// From a spec to a tree.

/// What the generator keeps of a root's writability: the tree's `writable`
/// entries, folded, or the whole root.
#[derive(Debug, Clone)]
struct RootInfo {
    id: u32,
    whole: bool,
    entries: Vec<String>,
}

/// The tree `spec` stands for, and what the generator keeps of it. Names
/// that fold like one already there are skipped, so the tree is valid
/// whatever proptest made.
fn build_tree(spec: &TreeSpec) -> (Tree, Vec<RootInfo>) {
    let mut roots = Vec::new();
    let mut info = Vec::new();
    for (n, r) in spec.roots.iter().take(ROOT_IDS.len()).enumerate() {
        let id = ROOT_IDS[n];
        let mut taken: BTreeSet<String> = BTreeSet::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<File> = Vec::new();
        for e in &r.entries {
            let at = e.parent as usize % (dirs.len() + 1);
            let dir = if at == 0 { "." } else { dirs[at - 1].as_str() };
            if dir.split('/').count() >= 3 {
                continue;
            }
            let name = respell(NAMES[e.name as usize % NAMES.len()], u32::from(e.case));
            let path = join(dir, &name);
            if !taken.insert(fold(&path)) {
                continue;
            }
            match &e.file {
                None => dirs.push(path),
                Some(f) => files.push(File {
                    path,
                    contents: match f {
                        FileSpec::Text(t) => {
                            Contents::Text(TEXTS[*t as usize % TEXTS.len()].to_string())
                        }
                        FileSpec::Fill { size, seed } => Contents::Fill {
                            size: u64::from(*size),
                            seed: u64::from(*seed),
                        },
                    },
                    mtime: 0,
                }),
            }
        }
        let mut writable = Vec::new();
        if spec.whole {
            writable.push(".".to_string());
        } else if let Some(mask) = r.writable {
            writable = dirs
                .iter()
                .enumerate()
                .filter(|(i, _)| mask >> (i % 32) & 1 == 1)
                .map(|(_, d)| d.clone())
                .collect();
            if writable.is_empty() {
                let w = ["saves", "w0", "w1", "w2"]
                    .into_iter()
                    .find(|w| !taken.contains(&fold(w)))
                    .expect("a free name");
                taken.insert(fold(w));
                dirs.push(w.to_string());
                writable.push(w.to_string());
            }
        }
        info.push(RootInfo {
            id,
            whole: spec.whole,
            entries: if spec.whole {
                Vec::new()
            } else {
                writable.iter().map(|w| fold(w)).collect()
            },
        });
        roots.push(Root {
            id,
            files,
            dirs,
            writable,
        });
    }
    let registry = spec.registry.as_ref().map(|keys| {
        let mut out = vec![RegKey {
            path: REG_BASE.to_string(),
            volatile: false,
            values: Vec::new(),
        }];
        let mut taken: BTreeSet<String> = [fold(REG_BASE)].into_iter().collect();
        for k in keys {
            let p = &out[k.parent as usize % out.len()];
            let (parent_path, parent_volatile) = (p.path.clone(), p.volatile);
            let name = respell(
                KEY_NAMES[k.name as usize % KEY_NAMES.len()],
                u32::from(k.name) >> 3,
            );
            let path = format!("{parent_path}\\{name}");
            if !taken.insert(fold(&path)) {
                continue;
            }
            let values = k
                .values
                .iter()
                .map(|v| RegValue {
                    name: VALUE_NAMES[v.name as usize % VALUE_NAMES.len()].to_string(),
                    ty: VALUE_TYPES[v.ty as usize % VALUE_TYPES.len()],
                    data: Contents::Hex(fill(v.len as usize % 24, u64::from(v.seed))),
                })
                .collect();
            out.push(RegKey {
                path,
                // A key under a volatile one is volatile too, as Windows
                // has it.
                volatile: k.volatile || parent_volatile,
                values,
            });
        }
        Registry { keys: out }
    });
    (Tree { roots, registry }, info)
}

// ---------------------------------------------------------------------------
// From ops to steps.

/// What wire.md says a generated request is answered, checked against the
/// oracle before its answer becomes the expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    /// `ST_OK`, with the oracle's reply.
    Ok,
    /// This status.
    Status(Status),
    /// Whatever the oracle answers: the overlay's semantics, which
    /// `crates/aether-registry/src/overlay.rs` defines (wire.md 4.3).
    Overlay,
    /// MKDIR of an existing directory: `ST_OK` or `ST_EXISTS` (3.4.9).
    OkOrExists,
    /// STORED_NAMES of a name the director does not have: the components as
    /// sent (SHOULD) or `ST_NOT_FOUND` (MAY), 3.4.12.
    NamesOrNotFound,
    /// STAGE_EXEC: `ST_NOT_SUPPORTED`, `ST_NOT_FOUND` from a stager that
    /// does not stage the file, or a path that is not compared (3.4.14).
    StageExec,
    /// An unknown root on RENAME or RENAME_X: `ST_NOT_FOUND` or
    /// `ST_BAD_REQUEST` (3.5.1's table).
    NotFoundOrBadRequest,
}

/// What a step changes in the generator's state when it succeeds.
#[derive(Debug, Clone)]
enum Effect {
    None,
    Opened {
        root: u32,
        path: String,
        dir: bool,
        read: bool,
        write: bool,
        append: bool,
        /// The open created the file or truncated it.
        emptied: bool,
    },
    Wrote {
        fh: u64,
        offset: u64,
        n: u64,
    },
    Resized {
        fh: u64,
        size: u64,
    },
    Closed(u64),
    Moved {
        from_root: u32,
        from: String,
        to_root: u32,
        to: String,
    },
    Deleted {
        root: u32,
        path: String,
    },
    Changed,
}

/// A request the generator chose, what wire.md says it gets, and why.
struct Plan {
    req: OwnedRequest,
    want: Want,
    wire: &'static str,
    note: String,
    effect: Effect,
}

/// An entry the oracle has: its stored spelling and folded path.
#[derive(Debug, Clone)]
struct Node {
    path: String,
    folded: String,
    dir: bool,
    size: u64,
}

/// An open handle, by the oracle's number.
#[derive(Debug, Clone)]
struct Handle {
    root: u32,
    /// Folded path of what it opened.
    folded: String,
    dir: bool,
    read: bool,
    write: bool,
    append: bool,
    /// Nothing but this handle has changed its file since it was opened, so
    /// what it reads is fixed by wire.md. A handle that is not clean is only
    /// closed.
    clean: bool,
}

/// A path a request names, and what the oracle has there.
#[derive(Debug, Clone)]
struct Target {
    root: u32,
    /// As sent.
    path: String,
    folded: String,
    node: Option<Node>,
    /// Its parent is an existing directory.
    parent_dir: bool,
}

/// What the director's ring holds, which bounds the requests sent: inline
/// READ data is at most `payload_cap - 8` bytes (wire.md 3.4.4) and WRITE
/// data at most `payload_cap - 24` (3.4.5); without an arena every READ is
/// inline (2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The longest READ answered inline: `payload_cap - 8`.
    pub inline_read: u32,
    /// The most data in a WRITE: `payload_cap - 24`.
    pub write: usize,
    /// Whether the ring has a bulk arena (`VFS_ARENA_LEN` above 0).
    pub arena: bool,
}

impl Limits {
    /// The limits of a ring with this payload cap and arena length.
    pub fn of(payload_cap: u32, arena_len: usize) -> Limits {
        Limits {
            inline_read: payload_cap.saturating_sub(8),
            write: payload_cap.saturating_sub(24) as usize,
            arena: arena_len > 0,
        }
    }
}

impl Default for Limits {
    /// The reference adapter's ring: the default payload cap and an arena.
    fn default() -> Limits {
        Limits::of(aether_ipc::DEFAULT_PAYLOAD_CAP, 1)
    }
}

/// The state the ops are read against: the oracle and what the generator
/// knows of it.
struct Gen {
    oracle: Oracle,
    limits: Limits,
    roots: Vec<RootInfo>,
    registry: bool,
    /// Every entry of every root, from the oracle's listings, with the root
    /// itself first.
    nodes: BTreeMap<u32, Vec<Node>>,
    /// How many leading bytes of each file wire.md fixes: all of them,
    /// except after a SETATTR that extended it (what the extension holds is
    /// the director's).
    known: BTreeMap<(u32, String), u64>,
    handles: BTreeMap<u64, Handle>,
    closed: Vec<u64>,
    /// Registry versions seen so far, in the oracle's numbering.
    versions: Vec<u64>,
    /// Key paths to pick from: the tree's and every one sent since.
    keys: Vec<String>,
    steps: Vec<Step>,
}

fn pick<T>(v: &[T], n: u32) -> Option<&T> {
    if v.is_empty() {
        None
    } else {
        Some(&v[n as usize % v.len()])
    }
}

/// The scenario `input` stands for, named `name`: its tree and a step for
/// each op that has something to act on, each expecting the reference's
/// answer. An `Err` when the reference answers a request otherwise than
/// wire.md says (the generator names the section).
pub fn build(input: &CaseInput, name: &str, description: &str) -> Result<Scenario, String> {
    build_with(input, name, description, Limits::default())
}

/// [`build`] for a director whose ring has `limits`.
pub fn build_with(
    input: &CaseInput,
    name: &str,
    description: &str,
    limits: Limits,
) -> Result<Scenario, String> {
    let (tree, roots) = build_tree(&input.tree);
    tree.validate()
        .map_err(|e| format!("generator: an invalid tree: {e}"))?;
    let mut g = Gen::new(&tree, roots, limits)?;
    for op in &input.ops {
        if let Some(plan) = g.plan(op) {
            g.step(plan)?;
        }
    }
    Ok(Scenario {
        name: name.to_string(),
        description: description.to_string(),
        tree,
        steps: g.steps,
    })
}

impl Gen {
    /// The generator for `tree`, mounted in a fresh oracle.
    fn new(tree: &Tree, roots: Vec<RootInfo>, limits: Limits) -> Result<Gen, String> {
        let keys = tree
            .registry
            .as_ref()
            .map(|r| r.keys.iter().map(|k| k.path.clone()).collect())
            .unwrap_or_default();
        let mut g = Gen {
            oracle: Oracle::new(tree)?,
            limits,
            roots,
            registry: tree.registry.is_some(),
            nodes: BTreeMap::new(),
            known: BTreeMap::new(),
            handles: BTreeMap::new(),
            closed: Vec::new(),
            versions: Vec::new(),
            keys,
            steps: Vec::new(),
        };
        g.refresh();
        Ok(g)
    }

    // --- What is there.

    fn info(&self, root: u32) -> Option<&RootInfo> {
        self.roots.iter().find(|r| r.id == root)
    }

    fn root_ids(&self) -> Vec<u32> {
        self.roots.iter().map(|r| r.id).collect()
    }

    /// Whether the writable provider holds folded `path`: anything under a
    /// writable entry, or anything but the root itself in a root writable
    /// as a whole.
    fn held(&self, root: u32, path: &str) -> bool {
        let Some(r) = self.info(root) else {
            return false;
        };
        if r.whole {
            return path != ".";
        }
        r.entries.iter().any(|w| path.starts_with(&format!("{w}/")))
    }

    /// Whether a read-only provider alone holds folded `path`: not the
    /// writable provider's, and neither a writable entry nor above one
    /// (whose answers depend on how a director layers its providers).
    fn read_only(&self, root: u32, path: &str) -> bool {
        let Some(r) = self.info(root) else {
            return false;
        };
        !r.whole
            && path != "."
            && !self.held(root, path)
            && !r.entries.iter().any(|w| under(w, path))
    }

    /// Whether the root has any writable provider.
    fn has_writable(&self, root: u32) -> bool {
        self.info(root)
            .is_some_and(|r| r.whole || !r.entries.is_empty())
    }

    fn node(&self, root: u32, folded: &str) -> Option<&Node> {
        self.nodes.get(&root)?.iter().find(|n| n.folded == folded)
    }

    fn all(&self, root: u32) -> Vec<Node> {
        self.nodes.get(&root).cloned().unwrap_or_default()
    }

    fn has_children(&self, root: u32, folded: &str) -> bool {
        self.all(root)
            .iter()
            .any(|n| n.folded != folded && n.folded != "." && under(&n.folded, folded))
    }

    /// The target for `path` (as sent) under `root`.
    fn target(&self, root: u32, path: String) -> Target {
        let folded = fold(&path);
        let node = self.node(root, &folded).cloned();
        let parent_dir = folded != "." && self.node(root, parent(&folded)).is_some_and(|n| n.dir);
        Target {
            root,
            path,
            folded,
            node,
            parent_dir,
        }
    }

    /// An existing entry of `root` that `ok` accepts, spelled as `bits`
    /// picks (a lookup finds any spelling with the same fold, wire.md 3.2).
    fn existing(&self, root: u32, n: u32, bits: u32, ok: impl Fn(&Node) -> bool) -> Option<Target> {
        let all: Vec<Node> = self.all(root).into_iter().filter(|x| ok(x)).collect();
        let node = pick(&all, n)?;
        Some(self.target(root, respell_path(&node.path, bits)))
    }

    /// A name `root` does not have, under an existing directory that `ok`
    /// accepts. The parent is spelled as stored, since a request that
    /// creates a name keeps the spelling it is sent with; the leaf is from
    /// [`NAMES`], spelled as `bits` picks.
    fn missing(&self, root: u32, n: u32, bits: u32, ok: impl Fn(&Node) -> bool) -> Option<Target> {
        let dirs: Vec<Node> = self
            .all(root)
            .into_iter()
            .filter(|x| x.dir && ok(x))
            .collect();
        let dir = pick(&dirs, n)?;
        if dir.path.split('/').count() >= 4 {
            return None;
        }
        (0..NAMES.len() as u32).find_map(|i| {
            let leaf = respell(
                NAMES[((bits >> 4).wrapping_add(i)) as usize % NAMES.len()],
                bits,
            );
            let t = self.target(root, join(&dir.path, &leaf));
            t.node.is_none().then_some(t)
        })
    }

    /// A missing name the writable provider would hold.
    fn missing_held(&self, root: u32, n: u32, bits: u32) -> Option<Target> {
        self.missing(root, n, bits, |d| {
            self.held(root, &fold(&join(&d.path, "x")))
        })
    }

    /// A served root, as `n` picks.
    fn root(&self, n: u32) -> u32 {
        *pick(&self.root_ids(), n).expect("root 0 is always there")
    }

    /// A served root with a writable provider, if any.
    fn writable_root(&self, n: u32) -> Option<u32> {
        let ids: Vec<u32> = self
            .root_ids()
            .into_iter()
            .filter(|&r| self.has_writable(r))
            .collect();
        pick(&ids, n).copied()
    }

    fn unknown_root(n: u32) -> u32 {
        UNKNOWN_ROOTS[n as usize % UNKNOWN_ROOTS.len()]
    }

    /// Re-reads every root's entries from the oracle.
    fn refresh(&mut self) {
        let mut nodes = BTreeMap::new();
        for id in self.root_ids() {
            let mut out = vec![Node {
                path: ".".into(),
                folded: ".".into(),
                dir: true,
                size: 0,
            }];
            let mut todo = vec![".".to_string()];
            while let Some(dir) = todo.pop() {
                let req = OwnedRequest::ReadDir {
                    root: RootId(id),
                    path: dir.clone(),
                };
                let Ok(OwnedReply::ReadDir(entries)) = self.oracle.ask(&req) else {
                    continue;
                };
                for e in entries {
                    let path = join(&dir, &e.name);
                    if e.is_dir {
                        todo.push(path.clone());
                    }
                    out.push(Node {
                        folded: fold(&path),
                        path,
                        dir: e.is_dir,
                        size: e.size,
                    });
                }
            }
            out[1..].sort_by(|a, b| a.folded.cmp(&b.folded));
            nodes.insert(id, out);
        }
        self.nodes = nodes;
        let files: BTreeMap<(u32, String), u64> = self
            .nodes
            .iter()
            .flat_map(|(r, ns)| {
                ns.iter()
                    .filter(|n| !n.dir)
                    .map(move |n| ((*r, n.folded.clone()), n.size))
            })
            .collect();
        self.known.retain(|k, _| files.contains_key(k));
        for (k, size) in files {
            let known = self.known.entry(k).or_insert(size);
            *known = (*known).min(size);
        }
    }

    // --- Handles.

    fn open_handles(&self, ok: impl Fn(&Handle) -> bool) -> Vec<(u64, Handle)> {
        self.handles
            .iter()
            .filter(|(_, h)| ok(h))
            .map(|(fh, h)| (*fh, h.clone()))
            .collect()
    }

    /// A clean handle to act on: a file handle `ok` accepts, or now and then
    /// (or when there is none) a directory handle.
    fn handle_for(&self, a: u32, d: u32, ok: impl Fn(&Handle) -> bool) -> Option<(u64, Handle)> {
        let files = self.open_handles(|h| h.clean && !h.dir && ok(h));
        let dirs = self.open_handles(|h| h.clean && h.dir);
        let list = if files.is_empty() || (!dirs.is_empty() && (d >> 4).is_multiple_of(8)) {
            dirs
        } else {
            files
        };
        pick(&list, a).cloned()
    }

    /// Marks every handle on `root`'s `path` or below it, but `except`, as
    /// changed under it.
    fn taint(&mut self, root: u32, path: &str, except: Option<u64>) {
        for (fh, h) in self.handles.iter_mut() {
            if Some(*fh) != except && h.root == root && under(&h.folded, path) {
                h.clean = false;
            }
        }
    }

    fn size_of(&self, root: u32, folded: &str) -> u64 {
        self.node(root, folded).map(|n| n.size).unwrap_or(0)
    }

    fn known_of(&self, root: u32, folded: &str) -> u64 {
        self.known
            .get(&(root, folded.to_string()))
            .copied()
            .unwrap_or(0)
    }

    // --- Choosing a request.

    fn plan(&self, op: &RawOp) -> Option<Plan> {
        let RawOp { kind, a, b, c, d } = *op;
        let plan = |req, want, wire, note: &str, effect| {
            Some(Plan {
                req,
                want,
                wire,
                note: note.to_string(),
                effect,
            })
        };
        match kind {
            Kind::Heartbeat => plan(
                OwnedRequest::Heartbeat,
                Want::Ok,
                "3.4.11",
                "",
                Effect::None,
            ),
            Kind::GetAttr => {
                if c % 10 == 0 {
                    let root = Gen::unknown_root(d);
                    return plan(
                        OwnedRequest::GetAttr {
                            root: RootId(root),
                            path: "a".into(),
                        },
                        Want::Ok,
                        "3.4.1",
                        "a root not served: found = 0",
                        Effect::None,
                    );
                }
                let root = self.root(a);
                let t = if b % 4 == 0 {
                    self.missing(root, a >> 8, c, |_| true)?
                } else {
                    self.existing(root, b >> 2, c, |_| true)?
                };
                let note = if t.node.is_some() {
                    ""
                } else {
                    "a missing path: found = 0"
                };
                plan(
                    OwnedRequest::GetAttr {
                        root: RootId(root),
                        path: t.path,
                    },
                    Want::Ok,
                    "3.4.1",
                    note,
                    Effect::None,
                )
            }
            Kind::ReadDir => {
                if c % 8 == 0 {
                    return plan(
                        OwnedRequest::ReadDir {
                            root: RootId(Gen::unknown_root(d)),
                            path: ".".into(),
                        },
                        Want::Status(Status::NotFound),
                        "3.4.2",
                        "unknown root",
                        Effect::None,
                    );
                }
                let root = self.root(a);
                let t = self.existing(root, b, c, |n| n.dir)?;
                plan(
                    OwnedRequest::ReadDir {
                        root: RootId(root),
                        path: t.path,
                    },
                    Want::Ok,
                    "3.4.2",
                    "",
                    Effect::None,
                )
            }
            Kind::OpenRead => self.plan_open_read(a, b, c, d),
            Kind::OpenWrite => self.plan_open_write(a, b, c, d),
            Kind::Read => self.plan_read(a, b, c, d),
            Kind::Write => self.plan_write(a, b, c, d),
            Kind::SetAttr => self.plan_setattr(a, b, c),
            Kind::Close => {
                let hs = self.open_handles(|_| true);
                let (fh, h) = pick(&hs, a)?;
                let note = if h.clean {
                    ""
                } else {
                    "a handle whose file changed under it"
                };
                plan(
                    OwnedRequest::Close { fh: *fh },
                    Want::Ok,
                    "3.4.10",
                    note,
                    Effect::Closed(*fh),
                )
            }
            Kind::StaleHandle | Kind::InvalidHandle => {
                let fh = if kind == Kind::StaleHandle {
                    *pick(&self.closed, a)?
                } else {
                    1_000_000 + u64::from(a % 1000)
                };
                let req = match b % 4 {
                    0 => OwnedRequest::Read {
                        slot_flags: RequestFlags::empty(),
                        fh,
                        offset: 0,
                        len: 8,
                    },
                    1 => OwnedRequest::Write {
                        fh,
                        offset: 0,
                        data: b"x".to_vec(),
                    },
                    2 => OwnedRequest::SetAttr { fh, size: 0 },
                    _ => OwnedRequest::Close { fh },
                };
                let note = if kind == Kind::StaleHandle {
                    "a closed handle"
                } else {
                    "a handle never issued"
                };
                plan(req, Want::Status(Status::BadFh), "3.5", note, Effect::None)
            }
            Kind::Rename => self.plan_rename(a, b, c, d, false),
            Kind::RenameX => {
                if d & 1 == 0 || self.roots.len() < 2 {
                    self.plan_rename(a, b, c, d, true)
                } else {
                    self.plan_rename_x(a, b, c, d)
                }
            }
            Kind::Delete => self.plan_delete(a, b, c, d),
            Kind::Mkdir => self.plan_mkdir(a, b, c, d),
            Kind::StoredNames => self.plan_stored_names(a, b, c, d),
            Kind::StageExec => {
                let root = self.root(a);
                let t = self.existing(root, b, c, |n| !n.dir)?;
                plan(
                    OwnedRequest::StageExec {
                        root: RootId(root),
                        path: t.path,
                    },
                    Want::StageExec,
                    "3.4.14",
                    "",
                    Effect::None,
                )
            }
            _ => self.plan_registry(kind, a, b, c, d),
        }
    }

    fn plan_open_read(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        // `oflags == 0` is read as OPEN_READ (3.4.3).
        let flags = if d & 1 == 1 {
            OpenFlags::empty()
        } else {
            OpenFlags::READ
        };
        if c.is_multiple_of(8) {
            return Some(Plan {
                req: OwnedRequest::Open {
                    root: RootId(Gen::unknown_root(d >> 1)),
                    flags,
                    path: "a".into(),
                },
                want: Want::Status(Status::NotFound),
                wire: "3.4.3",
                note: "unknown root".into(),
                effect: Effect::None,
            });
        }
        let root = self.root(a);
        let t = match b % 4 {
            0 => self.missing(root, a >> 8, c, |_| true)?,
            1 => self.existing(root, b >> 2, c, |_| true)?,
            _ => self
                .existing(root, b >> 2, c, |n| !n.dir)
                .or_else(|| self.existing(root, b >> 2, c, |_| true))?,
        };
        let (want, note, effect) = match &t.node {
            None => (
                Want::Status(Status::NotFound),
                "missing, without OPEN_CREATE",
                Effect::None,
            ),
            Some(n) => (
                Want::Ok,
                "",
                Effect::Opened {
                    root,
                    path: t.folded.clone(),
                    dir: n.dir,
                    read: true,
                    write: false,
                    append: false,
                    emptied: false,
                },
            ),
        };
        Some(Plan {
            req: OwnedRequest::Open {
                root: RootId(root),
                flags,
                path: t.path,
            },
            want,
            wire: "3.4.3",
            note: note.into(),
            effect,
        })
    }

    fn plan_open_write(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let mut flags = OpenFlags::WRITE;
        for (bit, f) in [
            (1, OpenFlags::READ),
            (2, OpenFlags::CREATE),
            (4, OpenFlags::EXCL),
            (8, OpenFlags::TRUNC),
        ] {
            if d & bit != 0 {
                flags |= f;
            }
        }
        if d & 0x30 == 0x30 {
            flags |= OpenFlags::APPEND;
        }
        let root = self.root(a);
        let t = match b % 5 {
            0 | 1 => self.existing(root, b >> 3, c, |n| !n.dir && self.held(root, &n.folded)),
            2 => self.missing_held(root, b >> 3, c),
            3 => self.existing(root, b >> 3, c, |n| {
                !n.dir && self.read_only(root, &n.folded)
            }),
            _ if !self.has_writable(root) => self.missing(root, b >> 3, c, |_| true),
            _ => self.missing_held(root, b >> 3, c),
        }?;
        let (want, note) = match &t.node {
            Some(n) if n.dir => return None,
            Some(_) if self.held(root, &t.folded) => {
                if flags.contains(OpenFlags::EXCL) {
                    (
                        Want::Status(Status::Exists),
                        "OPEN_EXCL on an existing file",
                    )
                } else {
                    (Want::Ok, "")
                }
            }
            Some(_) if self.read_only(root, &t.folded) => (
                Want::Status(Status::ReadOnly),
                "an existing file no writable provider holds",
            ),
            Some(_) => return None,
            None if !self.has_writable(root) && t.parent_dir => (
                Want::Status(Status::ReadOnly),
                "a root with no writable provider",
            ),
            None if self.held(root, &t.folded) && t.parent_dir => {
                if flags.contains(OpenFlags::CREATE) {
                    (Want::Ok, "creates the file")
                } else {
                    (
                        Want::Status(Status::NotFound),
                        "missing, without OPEN_CREATE",
                    )
                }
            }
            None => return None,
        };
        let effect = if want == Want::Ok {
            Effect::Opened {
                root,
                path: t.folded.clone(),
                dir: false,
                read: flags.contains(OpenFlags::READ),
                write: true,
                append: flags.contains(OpenFlags::APPEND),
                emptied: t.node.is_none() || flags.contains(OpenFlags::TRUNC),
            }
        } else {
            Effect::None
        };
        Some(Plan {
            req: OwnedRequest::Open {
                root: RootId(root),
                flags,
                path: t.path,
            },
            want,
            wire: "3.4.3",
            note: note.into(),
            effect,
        })
    }

    fn plan_read(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let (fh, h) = &self.handle_for(a, d, |h| h.read)?;
        let slot_flags = if d & 1 == 1 {
            RequestFlags::READ_BULK
        } else {
            RequestFlags::empty()
        };
        let n = c >> 3;
        if h.dir {
            return Some(Plan {
                req: OwnedRequest::Read {
                    slot_flags,
                    fh: *fh,
                    offset: u64::from(b % 64),
                    len: 1 + n % 64,
                },
                want: Want::Status(Status::IsDir),
                wire: "3.4.4",
                note: "a directory handle".into(),
                effect: Effect::None,
            });
        }
        let size = self.size_of(h.root, &h.folded);
        let known = self.known_of(h.root, &h.folded);
        let mut offset = match b % 6 {
            0 => 0,
            1 => size,
            2 => size + u64::from(b >> 3) % 50,
            3 => u64::from(b >> 3) % (size + 1),
            4 => (u64::from(b >> 3) % 7) | 1,
            _ => size.saturating_sub(1),
        };
        let mut len: u64 = match c % 8 {
            0 => 0,
            1 => 1,
            2 => u64::from(1 + n % 63) | 1,
            3 => size + 16,
            4 => size.saturating_sub(offset),
            5 => 65_536 + u64::from(n % 4465),
            6 => 4096,
            _ => u64::from(n % 300),
        };
        len = len.min(70_000);
        // A READ the director answers inline fits its slot (3.4.4); without
        // an arena every READ is inline (2.6).
        let inline =
            !self.limits.arena || (slot_flags.is_empty() && len < u64::from(BULK_THRESHOLD));
        if inline {
            len = len.min(u64::from(self.limits.inline_read));
        }
        if known < size {
            // Only the bytes wire.md fixes.
            offset = offset.min(known);
            len = len.min(known - offset);
        }
        Some(Plan {
            req: OwnedRequest::Read {
                slot_flags,
                fh: *fh,
                offset,
                len: len as u32,
            },
            want: Want::Ok,
            wire: "3.4.4",
            note: String::new(),
            effect: Effect::None,
        })
    }

    fn plan_write(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let (fh, h) = &self.handle_for(a, d, |h| h.write)?;
        if h.dir {
            return Some(Plan {
                req: OwnedRequest::Write {
                    fh: *fh,
                    offset: 0,
                    data: b"x".to_vec(),
                },
                want: Want::Status(Status::IsDir),
                wire: "3.4.5",
                note: "a directory handle".into(),
                effect: Effect::None,
            });
        }
        let known = self.known_of(h.root, &h.folded);
        let offset = if h.append {
            // Ignored on an append handle (3.4.3, 3.4.5).
            u64::from(b % 1000)
        } else {
            match b % 4 {
                0 => 0,
                1 => known,
                2 => u64::from(b >> 2) % (known + 1),
                _ => known.saturating_sub(1),
            }
        };
        let n = match (c >> 4) % 8 {
            0 => 0,
            1..=4 => 1 + c % 16,
            5 => 17 + c % 284,
            6 => 4096,
            _ => 20_000,
        }
        .min(self.limits.write as u32) as usize;
        let data = fill(n, u64::from(d));
        Some(Plan {
            req: OwnedRequest::Write {
                fh: *fh,
                offset,
                data,
            },
            want: Want::Ok,
            wire: "3.4.5",
            note: if h.append {
                "an OPEN_APPEND handle: the offset is ignored".into()
            } else {
                String::new()
            },
            effect: Effect::Wrote {
                fh: *fh,
                offset,
                n: n as u64,
            },
        })
    }

    fn plan_setattr(&self, a: u32, b: u32, c: u32) -> Option<Plan> {
        let (fh, h) = &self.handle_for(a, c, |h| h.write)?;
        if h.dir {
            return Some(Plan {
                req: OwnedRequest::SetAttr { fh: *fh, size: 0 },
                want: Want::Status(Status::IsDir),
                wire: "3.4.5",
                note: "a directory handle".into(),
                effect: Effect::None,
            });
        }
        let size = self.size_of(h.root, &h.folded);
        let to = match b % 5 {
            0 => 0,
            1 => size,
            2 => size / 2,
            3 => size + u64::from(c % 100),
            _ => u64::from(c) % (size + 1),
        };
        Some(Plan {
            req: OwnedRequest::SetAttr { fh: *fh, size: to },
            want: Want::Ok,
            wire: "3.4.6",
            note: String::new(),
            effect: Effect::Resized { fh: *fh, size: to },
        })
    }

    /// RENAME, or (`as_x`) RENAME_X with both roots equal, which behaves as
    /// RENAME (3.4.13).
    fn plan_rename(&self, a: u32, b: u32, c: u32, d: u32, as_x: bool) -> Option<Plan> {
        let wire = if as_x { "3.4.13, 3.4.7" } else { "3.4.7" };
        let mk = |root: u32, from: String, to: String| {
            if as_x {
                OwnedRequest::RenameX {
                    from_root: RootId(root),
                    to_root: RootId(root),
                    from,
                    to,
                }
            } else {
                OwnedRequest::Rename {
                    root: RootId(root),
                    from,
                    to,
                }
            }
        };
        if b % 12 == 11 {
            let root = Gen::unknown_root(d);
            return Some(Plan {
                req: mk(root, "a".into(), "b".into()),
                want: Want::NotFoundOrBadRequest,
                wire: "3.5.1",
                note: "unknown root".into(),
                effect: Effect::None,
            });
        }
        let root = self.writable_root(a)?;
        let held_node = |n: &Node| self.held(root, &n.folded);
        let (from, to) = match b % 12 {
            0 => (
                self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?,
                self.missing_held(root, b >> 4, d)?,
            ),
            1 => (
                self.existing(root, a >> 8, c, held_node)?,
                self.existing_to(root, b >> 4, d, |n| !n.dir && held_node(n))?,
            ),
            2 => (
                self.existing(root, a >> 8, c, held_node)?,
                self.existing_to(root, b >> 4, d, |n| {
                    n.dir && held_node(n) && !self.has_children(root, &n.folded)
                })?,
            ),
            3 => (
                self.existing(root, a >> 8, c, held_node)?,
                self.existing_to(root, b >> 4, d, |n| {
                    n.dir && held_node(n) && self.has_children(root, &n.folded)
                })?,
            ),
            4 => {
                let from = self.existing(root, a >> 8, c, held_node)?;
                let stored = from.node.as_ref()?.path.clone();
                let respelled = respell(leaf(&stored), d | 1);
                if respelled == leaf(&stored) {
                    return None;
                }
                let to = self.target(root, join(parent(&stored), &respelled));
                (from, to)
            }
            5 => (
                self.existing(root, a >> 8, c, |n| n.dir && held_node(n))?,
                self.missing_held(root, b >> 4, d)?,
            ),
            6 => {
                let from = self.existing(root, a >> 8, c, |n| n.dir && held_node(n))?;
                let stored = from.node.as_ref()?.path.clone();
                let to = self.target(root, join(&stored, NAMES[d as usize % NAMES.len()]));
                (from, to)
            }
            7 => (
                self.missing_held(root, a >> 8, c)?,
                self.missing_held(root, b >> 4, d)?,
            ),
            8 => (
                self.existing(root, a >> 8, c, |n| self.read_only(root, &n.folded))?,
                self.missing_held(root, b >> 4, d)?,
            ),
            9 => (
                self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?,
                self.missing(root, b >> 4, d, |n| {
                    self.read_only(root, &fold(&join(&n.path, "x")))
                })?,
            ),
            _ => {
                if !self.info(root)?.whole {
                    return None;
                }
                if d & 2 == 0 {
                    (
                        self.target(root, ".".into()),
                        self.missing_held(root, b >> 4, d)?,
                    )
                } else {
                    (
                        self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?,
                        self.target(root, ".".into()),
                    )
                }
            }
        };
        let (want, note) = self.predict_rename(&from, &to)?;
        let effect = if want == Want::Ok {
            Effect::Moved {
                from_root: root,
                from: from.folded.clone(),
                to_root: root,
                to: to.folded.clone(),
            }
        } else {
            Effect::None
        };
        Some(Plan {
            req: mk(root, from.path, to.path),
            want,
            wire,
            note: note.into(),
            effect,
        })
    }

    /// An existing entry as the target of a request that names it with the
    /// caller's spelling: the parent as stored, the leaf respelled.
    fn existing_to(
        &self,
        root: u32,
        n: u32,
        bits: u32,
        ok: impl Fn(&Node) -> bool,
    ) -> Option<Target> {
        let all: Vec<Node> = self
            .all(root)
            .into_iter()
            .filter(|x| x.folded != "." && ok(x))
            .collect();
        let node = pick(&all, n)?;
        Some(self.target(
            root,
            join(parent(&node.path), &respell(leaf(&node.path), bits)),
        ))
    }

    /// RENAME's answer by wire.md 3.4.7, for the combinations the
    /// generator sends; `None` where more than one rule could apply or
    /// wire.md does not say.
    fn predict_rename(&self, from: &Target, to: &Target) -> Option<(Want, &'static str)> {
        let root = from.root;
        let whole = self.info(root)?.whole;
        if from.folded == "." || to.folded == "." {
            return whole.then_some((Want::Status(Status::BadRequest), "the root at an end"));
        }
        let held_ends = self.held(root, &from.folded) && self.held(root, &to.folded);
        if from.node.is_some() && to.folded.starts_with(&format!("{}/", from.folded)) {
            return held_ends.then_some((Want::Status(Status::BadRequest), "`to` inside `from`"));
        }
        let to_fresh = to.node.is_none() && to.parent_dir;
        let Some(f) = &from.node else {
            let ok = held_ends && from.parent_dir && to_fresh;
            return ok.then_some((Want::Status(Status::NotFound), "a missing `from`"));
        };
        if self.read_only(root, &from.folded) {
            let ok = self.held(root, &to.folded) && to_fresh;
            return ok.then_some((Want::Status(Status::ReadOnly), "`from` is read-only"));
        }
        if !self.held(root, &from.folded) {
            return None;
        }
        if self.read_only(root, &to.folded) {
            return to_fresh.then_some((Want::Status(Status::ReadOnly), "`to` is read-only"));
        }
        if !self.held(root, &to.folded) || !to.parent_dir {
            return None;
        }
        if to.folded == from.folded {
            let respelled = leaf(&to.path) != leaf(&f.path);
            return respelled.then_some((Want::Ok, "a new spelling of the same name"));
        }
        match &to.node {
            None => Some((Want::Ok, "")),
            Some(_) if from.folded.starts_with(&format!("{}/", to.folded)) => None,
            Some(t) if !t.dir => Some((Want::Ok, "replaces a file")),
            Some(t) if self.has_children(root, &t.folded) => Some((
                Want::Status(Status::IoError),
                "onto a directory with something under it",
            )),
            Some(_) => Some((Want::Ok, "replaces an empty directory")),
        }
    }

    fn plan_rename_x(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let writable: Vec<u32> = self
            .root_ids()
            .into_iter()
            .filter(|&r| self.has_writable(r))
            .collect();
        if b % 5 == 4 {
            let known = self.root(a);
            let unknown = Gen::unknown_root(c);
            let (from_root, to_root) = if d & 2 == 0 {
                (known, unknown)
            } else {
                (unknown, known)
            };
            return Some(Plan {
                req: OwnedRequest::RenameX {
                    from_root: RootId(from_root),
                    to_root: RootId(to_root),
                    from: "a".into(),
                    to: "b".into(),
                },
                want: Want::NotFoundOrBadRequest,
                wire: "3.5.1",
                note: "an unknown root at one end".into(),
                effect: Effect::None,
            });
        }
        let from_root = *pick(&writable, a)?;
        let others: Vec<u32> = writable
            .iter()
            .copied()
            .filter(|&r| r != from_root)
            .collect();
        let to_root = *pick(&others, a >> 4)?;
        let held_from = |n: &Node| self.held(from_root, &n.folded);
        let (from, to) = match b % 5 {
            0 => (
                self.existing(from_root, a >> 8, c, |n| !n.dir && held_from(n))?,
                self.missing_held(to_root, b >> 4, d)?,
            ),
            // Onto an existing file the stored spelling is sent: Cairn
            // opens the destination for writing (`copy-across!`) and keeps
            // its name, and 3.4.13 does not say whose spelling wins.
            1 => (
                self.existing(from_root, a >> 8, c, |n| !n.dir && held_from(n))?,
                self.existing(to_root, b >> 4, 0, |n| {
                    !n.dir && self.held(to_root, &n.folded)
                })?,
            ),
            2 => (
                self.existing(from_root, a >> 8, c, |n| n.dir && held_from(n))?,
                self.missing_held(to_root, b >> 4, d)?,
            ),
            _ => (
                self.missing_held(from_root, a >> 8, c)?,
                self.missing_held(to_root, b >> 4, d)?,
            ),
        };
        if from.folded == "." || to.folded == "." {
            return None;
        }
        let held_ends = self.held(from_root, &from.folded) && self.held(to_root, &to.folded);
        if !held_ends || !to.parent_dir {
            return None;
        }
        let (want, note) = match (&from.node, &to.node) {
            (None, None) if from.parent_dir => (Want::Status(Status::NotFound), "a missing `from`"),
            (Some(f), None) if f.dir => (Want::Status(Status::CrossRoot), "a directory"),
            (Some(f), None) if !f.dir => (Want::Ok, "a file to another root"),
            (Some(f), Some(t)) if !f.dir && !t.dir => (Want::Ok, "replacing a file there"),
            _ => return None,
        };
        let effect = if want == Want::Ok {
            Effect::Moved {
                from_root,
                from: from.folded.clone(),
                to_root,
                to: to.folded.clone(),
            }
        } else {
            Effect::None
        };
        Some(Plan {
            req: OwnedRequest::RenameX {
                from_root: RootId(from_root),
                to_root: RootId(to_root),
                from: from.path,
                to: to.path,
            },
            want,
            wire: "3.4.13",
            note: note.into(),
            effect,
        })
    }

    fn plan_delete(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        if b % 7 == 6 {
            return Some(Plan {
                req: OwnedRequest::Delete {
                    root: RootId(Gen::unknown_root(d)),
                    path: "a".into(),
                },
                want: Want::Status(Status::NotFound),
                wire: "3.4.8",
                note: "unknown root".into(),
                effect: Effect::None,
            });
        }
        let root = self.root(a);
        let held = |n: &Node| self.held(root, &n.folded);
        let t = match b % 7 {
            0 => self.existing(root, b >> 3, c, |n| !n.dir && held(n)),
            1 => self.existing(root, b >> 3, c, |n| {
                n.dir && held(n) && !self.has_children(root, &n.folded)
            }),
            2 => self.existing(root, b >> 3, c, |n| {
                n.dir && held(n) && self.has_children(root, &n.folded)
            }),
            3 => self.missing_held(root, b >> 3, c),
            4 => self.existing(root, b >> 3, c, |n| self.read_only(root, &n.folded)),
            _ => Some(self.target(root, ".".into())),
        }?;
        let (want, note) = if t.folded == "." {
            if !self.info(root)?.whole {
                return None;
            }
            (Want::Status(Status::BadRequest), "the root itself")
        } else {
            match &t.node {
                Some(_) if self.read_only(root, &t.folded) => (
                    Want::Status(Status::ReadOnly),
                    "no writable provider holds it",
                ),
                Some(n) if self.held(root, &t.folded) => {
                    if n.dir && self.has_children(root, &t.folded) {
                        (
                            Want::Status(Status::IoError),
                            "a directory with something under it",
                        )
                    } else {
                        (Want::Ok, "")
                    }
                }
                None if self.held(root, &t.folded) && t.parent_dir => {
                    (Want::Status(Status::NotFound), "missing")
                }
                _ => return None,
            }
        };
        let effect = if want == Want::Ok {
            Effect::Deleted {
                root,
                path: t.folded.clone(),
            }
        } else {
            Effect::None
        };
        Some(Plan {
            req: OwnedRequest::Delete {
                root: RootId(root),
                path: t.path,
            },
            want,
            wire: "3.4.8",
            note: note.into(),
            effect,
        })
    }

    fn plan_mkdir(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        // The mode is ignored (3.4.9); the shim sends 0o755.
        let mode = if d & 1 == 0 { 0o755 } else { d };
        if b % 6 == 5 {
            return Some(Plan {
                req: OwnedRequest::Mkdir {
                    root: RootId(Gen::unknown_root(d >> 1)),
                    mode,
                    path: "a".into(),
                },
                want: Want::Status(Status::NotFound),
                wire: "3.5.1",
                note: "unknown root".into(),
                effect: Effect::None,
            });
        }
        let root = self.root(a);
        let held = |n: &Node| self.held(root, &n.folded);
        let t = match b % 6 {
            0 => self.missing_held(root, b >> 3, c),
            1 => self.existing(root, b >> 3, c, |n| n.dir && held(n)),
            2 => self.existing(root, b >> 3, c, |n| !n.dir && held(n)),
            3 => self.existing(root, b >> 3, c, |n| {
                !n.dir && self.read_only(root, &n.folded)
            }),
            _ if !self.has_writable(root) => self.missing(root, b >> 3, c, |_| true),
            _ => self.missing_held(root, b >> 3, c),
        }?;
        let (want, note) = match &t.node {
            Some(n) if n.dir && self.held(root, &t.folded) => {
                (Want::OkOrExists, "an existing directory")
            }
            Some(n) if !n.dir && self.held(root, &t.folded) => (
                Want::Status(Status::IoError),
                "an existing file a writable provider holds",
            ),
            Some(n) if !n.dir && self.read_only(root, &t.folded) => (
                Want::Status(Status::Exists),
                "an existing file no writable provider holds",
            ),
            None if t.parent_dir && !self.has_writable(root) => (
                Want::Status(Status::ReadOnly),
                "a root with no writable provider",
            ),
            None if t.parent_dir && self.held(root, &t.folded) => (Want::Ok, ""),
            _ => return None,
        };
        let wire = if want == Want::Status(Status::ReadOnly) {
            "3.4.9, 3.5"
        } else {
            "3.4.9"
        };
        let effect = if want == Want::Ok {
            Effect::Changed
        } else {
            Effect::None
        };
        Some(Plan {
            req: OwnedRequest::Mkdir {
                root: RootId(root),
                mode,
                path: t.path,
            },
            want,
            wire,
            note: note.into(),
            effect,
        })
    }

    fn plan_stored_names(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        if b % 4 == 3 {
            return Some(Plan {
                req: OwnedRequest::StoredNames {
                    root: RootId(Gen::unknown_root(d)),
                    skip: 0,
                    path: "a".into(),
                },
                want: Want::Status(Status::NotFound),
                wire: "3.5.1",
                note: "unknown root".into(),
                effect: Effect::None,
            });
        }
        let root = self.root(a);
        let (t, want) = if b % 4 == 2 {
            let mut t = self.missing(root, b >> 3, c, |_| true)?;
            // A lookup: the parent in any spelling.
            t.path = join(&respell_path(parent(&t.path), d), leaf(&t.path));
            (t, Want::NamesOrNotFound)
        } else {
            (
                self.existing(root, b >> 3, c, |n| n.folded != ".")?,
                Want::Ok,
            )
        };
        if t.path == "." || t.path.is_empty() {
            return None;
        }
        // Fewer than all of its components: wire.md says nothing of a reply
        // with no names.
        let components = t.path.split('/').count() as u32;
        let skip = d % components;
        Some(Plan {
            req: OwnedRequest::StoredNames {
                root: RootId(root),
                skip,
                path: t.path,
            },
            want,
            wire: "3.4.12",
            note: if want == Want::Ok {
                String::new()
            } else {
                "a name the director does not have".into()
            },
            effect: Effect::None,
        })
    }

    fn plan_registry(&self, kind: Kind, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let path = || -> Option<String> {
            let base = pick(&self.keys, a)?;
            let p = if b.is_multiple_of(3) {
                format!("{base}\\{}", KEY_NAMES[(b >> 2) as usize % KEY_NAMES.len()])
            } else {
                base.clone()
            };
            // Keys compare by fold (4.2). `\Registry\Machine` keeps its
            // canonical spelling here (`registry-case` covers other
            // spellings of it); the components below are respelled.
            let parts: Vec<&str> = p.split('\\').collect();
            let respelled: Vec<String> = parts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i < 3 {
                        s.to_string()
                    } else {
                        respell(s, c.rotate_left(5 * i as u32))
                    }
                })
                .collect();
            Some(respelled.join("\\"))
        };
        let value = || VALUE_NAMES[(c >> 8) as usize % VALUE_NAMES.len()].to_string();
        let req = match kind {
            Kind::RegLookup => OwnedRequest::RegLookup { path: path()? },
            Kind::RegKey => OwnedRequest::RegKey { path: path()? },
            Kind::RegSetValue => OwnedRequest::RegSetValue {
                path: path()?,
                name: value(),
                ty: VALUE_TYPES[d as usize % VALUE_TYPES.len()],
                data: fill((d >> 8) as usize % 24, u64::from(d >> 16)),
            },
            Kind::RegDeleteValue => OwnedRequest::RegDeleteValue {
                path: path()?,
                name: value(),
            },
            Kind::RegCreateKey => OwnedRequest::RegCreateKey {
                path: path()?,
                volatile: d & 1 == 1,
            },
            Kind::RegDeleteKey => OwnedRequest::RegDeleteKey { path: path()? },
            Kind::RegRenameKey => {
                if d.is_multiple_of(10) {
                    let new_leaf = if d & 16 == 0 { "." } else { ".." };
                    return Some(Plan {
                        req: OwnedRequest::RegRenameKey {
                            path: path()?,
                            new_leaf: new_leaf.into(),
                        },
                        want: self.registry_want(Want::Status(Status::BadRequest)),
                        wire: "4.3",
                        note: "a new leaf of `.` or `..`".into(),
                        effect: Effect::None,
                    });
                }
                OwnedRequest::RegRenameKey {
                    path: path()?,
                    new_leaf: respell(KEY_NAMES[d as usize % KEY_NAMES.len()], d >> 8),
                }
            }
            Kind::RegChanged => OwnedRequest::RegChanged {
                path: path()?,
                subtree: d & 1 == 1,
                since: *pick(&self.versions, d >> 1)?,
            },
            Kind::RegBadPath => {
                let bad = BAD_KEY_PATHS[a as usize % BAD_KEY_PATHS.len()].to_string();
                let req = match b % 4 {
                    0 => OwnedRequest::RegLookup { path: bad },
                    1 => OwnedRequest::RegKey { path: bad },
                    2 => OwnedRequest::RegCreateKey {
                        path: bad,
                        volatile: false,
                    },
                    _ => OwnedRequest::RegDeleteKey { path: bad },
                };
                return Some(Plan {
                    req,
                    want: self.registry_want(Want::Status(Status::BadRequest)),
                    wire: if self.registry { "4.2" } else { "4" },
                    note: "a key path that is not canonical".into(),
                    effect: Effect::None,
                });
            }
            _ => return None,
        };
        let effect = match kind {
            Kind::RegLookup | Kind::RegKey | Kind::RegChanged => Effect::None,
            _ => Effect::Changed,
        };
        Some(Plan {
            req,
            want: self.registry_want(Want::Overlay),
            wire: if self.registry { "4.3" } else { "4" },
            note: String::new(),
            effect,
        })
    }

    /// `want`, or `ST_NOT_SUPPORTED` without a registry layer (wire.md 4).
    fn registry_want(&self, want: Want) -> Want {
        if self.registry {
            want
        } else {
            Want::Status(Status::NotSupported)
        }
    }

    // --- Taking a step.

    /// Asks the oracle, checks its answer against `plan.want`, records the
    /// step and moves the state on.
    fn step(&mut self, plan: Plan) -> Result<(), String> {
        let got = self.oracle.ask(&plan.req);
        let disagree = || {
            format!(
                "generator: step {}: {:?}: wire.md {} says {:?}, but the reference \
                 answered {}",
                self.steps.len(),
                plan.req,
                plan.wire,
                plan.want,
                crate::compare::render_got(&got)
            )
        };
        // A REG_KEY node's `last_write` is the overlay's clock and is not
        // compared (wire.md 4.3); written as 0, so a seed gives the same
        // case every time.
        let reply = |r: &OwnedReply| {
            let mut r = r.clone();
            if let OwnedReply::RegKey { node: Some(n), .. } = &mut r {
                n.last_write = 0;
            }
            Expect::Reply(r)
        };
        let expect = match (plan.want, &got) {
            (Want::Ok, Ok(r)) | (Want::Overlay, Ok(r)) => reply(r),
            (Want::Status(s), Err(g)) if s == *g => Expect::Status(s),
            (Want::Overlay, Err(s)) => Expect::Status(*s),
            (Want::OkOrExists, Ok(OwnedReply::Empty) | Err(Status::Exists)) => Expect::OneOf(vec![
                Expect::Reply(OwnedReply::Empty),
                Expect::Status(Status::Exists),
            ]),
            (Want::NamesOrNotFound, Ok(r @ OwnedReply::StoredNames(_))) => {
                Expect::OneOf(vec![reply(r), Expect::Status(Status::NotFound)])
            }
            (
                Want::StageExec,
                Err(Status::NotSupported | Status::NotFound) | Ok(OwnedReply::StageExec(_)),
            ) => Expect::OneOf(vec![
                Expect::Status(Status::NotSupported),
                Expect::Status(Status::NotFound),
                Expect::Reply(OwnedReply::StageExec(String::new())),
            ]),
            (Want::NotFoundOrBadRequest, Err(Status::NotFound | Status::BadRequest)) => {
                Expect::OneOf(vec![
                    Expect::Status(Status::NotFound),
                    Expect::Status(Status::BadRequest),
                ])
            }
            _ => return Err(disagree()),
        };
        let cite = matches!(expect, Expect::OneOf(_)).then(|| plan.wire.to_string());
        let note = if plan.note.is_empty() {
            format!("wire.md {}", plan.wire)
        } else {
            format!("wire.md {}: {}", plan.wire, plan.note)
        };
        self.steps.push(Step {
            request: plan.req.clone(),
            expect,
            cite,
            note: Some(note),
        });
        if let Ok(r) = &got {
            self.record_version(r);
            self.apply(&plan, r);
        }
        if let OwnedRequest::RegCreateKey { path, .. } = &plan.req {
            if !self.keys.contains(path) {
                self.keys.push(path.clone());
            }
        }
        Ok(())
    }

    fn record_version(&mut self, r: &OwnedReply) {
        let v = match r {
            OwnedReply::RegLookup(l) => l.version,
            OwnedReply::RegKey { version, .. } => *version,
            OwnedReply::RegVersion(v) => *v,
            OwnedReply::RegChanged(c) => c.version,
            _ => return,
        };
        if !self.versions.contains(&v) {
            self.versions.push(v);
        }
    }

    /// The effect of a step that succeeded.
    fn apply(&mut self, plan: &Plan, r: &OwnedReply) {
        match &plan.effect {
            Effect::None => {}
            Effect::Opened {
                root,
                path,
                dir,
                read,
                write,
                append,
                emptied,
            } => {
                let OwnedReply::Open(o) = r else { return };
                if *emptied {
                    self.taint(*root, path, None);
                    self.known.insert((*root, path.clone()), 0);
                }
                self.handles.insert(
                    o.fh,
                    Handle {
                        root: *root,
                        folded: path.clone(),
                        dir: *dir,
                        read: *read,
                        write: *write,
                        append: *append,
                        clean: true,
                    },
                );
                self.refresh();
            }
            Effect::Wrote { fh, offset, n } => {
                let Some(h) = self.handles.get(fh).cloned() else {
                    return;
                };
                let size = self.size_of(h.root, &h.folded);
                let key = (h.root, h.folded.clone());
                let known = self.known_of(h.root, &h.folded);
                let at = if h.append { size } else { *offset };
                if at <= known {
                    self.known.insert(key, known.max(at + n));
                }
                self.taint(h.root, &h.folded, Some(*fh));
                self.refresh();
            }
            Effect::Resized { fh, size } => {
                let Some(h) = self.handles.get(fh).cloned() else {
                    return;
                };
                let known = self.known_of(h.root, &h.folded);
                self.known
                    .insert((h.root, h.folded.clone()), known.min(*size));
                self.taint(h.root, &h.folded, Some(*fh));
                self.refresh();
            }
            Effect::Closed(fh) => {
                self.handles.remove(fh);
                self.closed.push(*fh);
            }
            Effect::Moved {
                from_root,
                from,
                to_root,
                to,
            } => {
                self.taint(*from_root, from, None);
                self.taint(*to_root, to, None);
                if (from_root, from) != (to_root, to) {
                    self.known
                        .retain(|(r, p), _| !(r == to_root && under(p, to)));
                    let moved: Vec<((u32, String), u64)> = self
                        .known
                        .iter()
                        .filter(|((r, p), _)| r == from_root && under(p, from))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    for ((_, p), v) in moved {
                        self.known.remove(&(*from_root, p.clone()));
                        let tail = &p[from.len()..];
                        self.known.insert((*to_root, format!("{to}{tail}")), v);
                    }
                }
                self.refresh();
            }
            Effect::Deleted { root, path } => {
                self.taint(*root, path, None);
                self.known
                    .retain(|(r, p), _| !(r == root && under(p, path)));
                self.refresh();
            }
            Effect::Changed => self.refresh(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_respelled_name_folds_as_the_name_does() {
        for name in NAMES.iter().chain(KEY_NAMES.iter()) {
            for bits in [0, 1, 0b1010_1010, u32::MAX, 0x1234_5678] {
                let r = respell(name, bits);
                assert_eq!(fold(&r), fold(name), "{name} as {r}");
                assert_eq!(r.chars().count(), name.chars().count());
            }
        }
        assert_eq!(respell("abc", u32::MAX), "ABC");
        assert_eq!(respell("ABC", 0b010), "AbC");
    }

    #[test]
    fn the_oracle_answers_as_the_reference_does() {
        let tree = Tree {
            roots: vec![Root {
                id: 0,
                files: vec![File {
                    path: "data/a.txt".into(),
                    contents: Contents::Fill {
                        size: 100_000,
                        seed: 4,
                    },
                    mtime: 0,
                }],
                dirs: vec![],
                writable: vec![],
            }],
            registry: None,
        };
        let mut o = Oracle::new(&tree).unwrap();
        let Ok(OwnedReply::Open(open)) = o.ask(&OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: "DATA/A.TXT".into(),
        }) else {
            panic!("opens");
        };
        assert_eq!(open.size, 100_000);
        // In bulk, by size: the data comes back from the arena.
        let got = o.ask(&OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh: open.fh,
            offset: 10,
            len: 70_000,
        });
        assert_eq!(
            got,
            Ok(OwnedReply::Read(fill(100_000, 4)[10..70_010].to_vec()))
        );
        assert_eq!(
            o.ask(&OwnedRequest::Close { fh: open.fh + 1 }),
            Err(Status::BadFh)
        );
        assert_eq!(
            o.ask(&OwnedRequest::RegLookup {
                path: REG_BASE.into()
            }),
            Err(Status::NotSupported)
        );
    }

    #[test]
    fn every_built_tree_is_valid() {
        use proptest::strategy::ValueTree;
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        for _ in 0..200 {
            let spec = tree_spec().new_tree(&mut runner).unwrap().current();
            let (tree, _) = build_tree(&spec);
            tree.validate().unwrap();
            Reference::from_tree(&tree).unwrap();
        }
    }

    /// Two roots writable as a whole, each with files spelled oddly.
    fn two_roots() -> TreeSpec {
        let file = |name: u8, case: u8| EntrySpec {
            parent: 0,
            name,
            case,
            file: Some(FileSpec::Text(2)),
        };
        TreeSpec {
            whole: true,
            roots: vec![
                RootSpec {
                    writable: None,
                    entries: vec![file(1, 0b1010), file(5, 0b0101)],
                },
                RootSpec {
                    writable: None,
                    entries: vec![file(1, 0b0110), file(11, 0b1001)],
                },
            ],
            registry: None,
        }
    }

    /// Cairn's RENAME_X opens an existing destination write|create|trunc and
    /// keeps its stored name, and wire.md 3.4.13 does not say whose spelling
    /// wins, so onto an existing file the stored spelling is sent.
    #[test]
    fn a_rename_x_onto_a_file_sends_its_stored_spelling() {
        let (tree, roots) = build_tree(&two_roots());
        let g = Gen::new(&tree, roots, Limits::default()).unwrap();
        let mut seen = 0;
        for a in 0..64u32 {
            for b in (1..200u32).step_by(5) {
                for d in [0, 1, 0xFF, 0x1234_5678] {
                    let Some(p) = g.plan_rename_x(a, b, 0x5555, d) else {
                        continue;
                    };
                    if p.note != "replacing a file there" {
                        continue;
                    }
                    let OwnedRequest::RenameX { to_root, to, .. } = &p.req else {
                        panic!("{:?}", p.req)
                    };
                    let stored = g.node(to_root.0, &fold(to)).unwrap().path.clone();
                    assert_eq!(to, &stored);
                    seen += 1;
                }
            }
        }
        assert!(seen > 0, "no RENAME_X onto a file was planned");
    }

    /// A director with a stager answers ST_NOT_FOUND for a file it does not
    /// stage (Cairn, wire.md 3.4.14).
    #[test]
    fn stage_exec_allows_not_found() {
        let mut checked = 0;
        for s in crate::generate::cases(5, 64, 32).unwrap() {
            for st in &s.steps {
                if let OwnedRequest::StageExec { .. } = st.request {
                    let Expect::OneOf(alts) = &st.expect else {
                        panic!("{:?}", st.expect)
                    };
                    assert!(alts.contains(&Expect::Status(Status::NotFound)), "{alts:?}");
                    assert!(alts.contains(&Expect::Status(Status::NotSupported)));
                    checked += 1;
                }
            }
        }
        assert!(checked > 0);
    }

    /// Inline READs and WRITEs fit the director's slot: `payload_cap - 8`
    /// bytes of read data, `payload_cap - 24` of write data (wire.md 3.4.4,
    /// 3.4.5).
    #[test]
    fn reads_and_writes_fit_the_limits() {
        use proptest::strategy::ValueTree;
        for arena in [false, true] {
            let limits = Limits {
                inline_read: 100,
                write: 50,
                arena,
            };
            let mut runner = proptest::test_runner::TestRunner::deterministic();
            let (mut reads, mut writes) = (0, 0);
            for i in 0..200 {
                let input = case_strategy(32).new_tree(&mut runner).unwrap().current();
                let s = build_with(&input, &format!("c{i}"), "", limits).unwrap();
                for st in &s.steps {
                    match &st.request {
                        OwnedRequest::Read {
                            slot_flags, len, ..
                        } => {
                            let inline = !arena || (slot_flags.is_empty() && *len < 65_536);
                            if inline {
                                assert!(*len <= 100, "{:?}", st.request);
                            }
                            reads += 1;
                        }
                        OwnedRequest::Write { data, .. } => {
                            assert!(data.len() <= 50, "{}", data.len());
                            writes += 1;
                        }
                        _ => {}
                    }
                }
            }
            assert!(reads > 0 && writes > 0);
        }
    }
}
