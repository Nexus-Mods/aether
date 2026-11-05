//! What a generated case is, and how it becomes a scenario: the generator
//! behind `aether-conform run --generate` ([`crate::generate`]).
//!
//! A case is a [`CaseInput`]: a [`TreeSpec`] (the roots, files, directories,
//! writable entries and registry keys to mount), a list of [`RawOp`]s, each
//! an operation kind and five numbers, and how many clients besides client
//! 0 send steps. All are plain proptest values, so proptest generates and
//! shrinks them. [`build`] turns one into a [`Scenario`]: it builds the
//! tree, mounts it in an in-process [`Oracle`] (the reference director,
//! `aether_refdirector::Reference`, behind a ring in memory), and reads the
//! ops one at a time against the oracle's state. Each op's numbers pick its
//! targets among what exists at that point (an open handle, an existing
//! file, a missing name under an existing directory), now and then among
//! what the last steps touched, so chains on one name are common; every
//! shrunk case is still a valid one, and an op with nothing to act on is
//! dropped. A [`Kind::Template`] op stands for a sequence of ops on the same
//! names ([`TEMPLATES`]). The oracle's answer to each request becomes that
//! step's expectation.
//!
//! A handle holds its file, not its name ([`Handle`]): the generator keeps
//! each open file's identity where the file is now, so a handle follows its
//! file through RENAMEs and its READs, WRITEs and SETATTRs are the oracle's,
//! which serves every handle the file as it is now (wire.md 3.4.4, 3.4.7).
//! A handle whose file was deleted or replaced keeps the bytes the file had
//! then ([`Gone`]), and its answers are the `one_of`s 3.4.8 allows.
//!
//! A root may be served as an overlay, with read-only `lower` layers beneath
//! its writable upper. The generator keeps its own account of those layers
//! ([`Layered`]), apart from the oracle: which names the upper holds, which
//! a DELETE or RENAME hid, and which a rule wire.md leaves open makes
//! unsure. From it it predicts the overlay's statuses (3.4.9, "Names only
//! in a read-only lower"), the bytes a READ through any live handle gets,
//! the layer's spelling of a lower-only name, and after every step every
//! listing ([`Gen::check_layers`]); the oracle answering otherwise fails
//! [`build`], as a status wire.md disagrees with does.
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
//! director's layering (a writable entry's own directory, its ancestors;
//! in a layered root, the question `docs/conformance.md` lists as left to
//! the director);
//! WRITE and SETATTR on a handle opened without write access; requests that
//! create a named stream; READs through a handle whose file is gone after a
//! WRITE or SETATTR through it; paths that leave their root (`escape-paths`
//! covers them); registry requests near a limit.

use std::collections::{BTreeMap, BTreeSet};

use aether_casefold::fold;
use aether_ipc::{ring, DataArena, Geom, OwnedSeg, RingServer, SpinNotifier};
use aether_protocol::{OpenFlags, Reply, RequestFlags, RootId, Status};
use aether_refdirector::tree::{
    fill, Contents, File, Layer, RegKey, RegValue, Registry, Root, Tree,
};
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

/// One generated case: a tree, the operations to read against it, and how
/// many clients besides client 0 send some of them (wire.md 2.8.1).
#[derive(Debug, Clone)]
pub struct CaseInput {
    pub tree: TreeSpec,
    pub ops: Vec<RawOp>,
    pub others: u8,
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
    /// At least two writable entries, so a RENAME can go from one to
    /// another.
    pub two: bool,
    pub entries: Vec<EntrySpec>,
    /// Read-only layers beneath the writable upper, the nearest first, each
    /// a list of entries placed below the writable part ([`build_tree`]); a
    /// root with no writable provider has none.
    pub lower: Vec<Vec<EntrySpec>>,
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
/// arguments against the state it meets, and a fifth, `e`, that picks
/// whether it acts on something a recent step touched (and which), and
/// which client sends it.
#[derive(Debug, Clone, Copy)]
pub struct RawOp {
    pub kind: Kind,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
    pub e: u32,
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
    /// A sequence of the kinds above on the same names and handles
    /// ([`TEMPLATES`]).
    Template,
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
/// The keys every generated registry layer has, one per hive.
const REG_BASES: [&str; 3] = [
    REG_BASE,
    r"\Registry\User\<CurrentUser>\Software\Conform",
    r"\Registry\User\<CurrentUser>_Classes\Conform",
];
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
        proptest::bool::weighted(0.3),
        proptest::collection::vec(entry_spec(), 0..12),
        prop_oneof![
            1 => Just(Vec::new()),
            1 => proptest::collection::vec(proptest::collection::vec(entry_spec(), 1..10), 1..=2),
        ],
    )
        .prop_map(|(writable, two, entries, lower)| RootSpec {
            writable,
            two,
            entries,
            lower,
        })
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
        3 => Just(K::Template),
    ]
}

fn raw_op() -> impl Strategy<Value = RawOp> {
    (
        kind(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(|(kind, a, b, c, d, e)| RawOp {
            kind,
            a,
            b,
            c,
            d,
            e,
        })
}

/// Cases of at most `max_ops` operations.
pub fn case_strategy(max_ops: usize) -> impl Strategy<Value = CaseInput> {
    (
        tree_spec(),
        proptest::collection::vec(raw_op(), 1..=max_ops.max(1)),
        prop_oneof![9 => Just(0u8), 1 => 1u8..=2],
    )
        .prop_map(|(tree, ops, others)| CaseInput { tree, ops, others })
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

/// The name `path` of a lower layer, by its folded path: as the layer spells
/// it, and a file's bytes (`None` for a directory).
type LayerNames = BTreeMap<String, (String, Option<Vec<u8>>)>;

/// The layers of `root`, each as [`LayerNames`] with every directory a
/// file's path implies.
fn layer_names(root: &Root) -> Vec<LayerNames> {
    root.lower
        .iter()
        .map(|l| {
            let mut m = LayerNames::new();
            for d in l.dirs_spelled() {
                m.entry(fold(&d)).or_insert((d, None));
            }
            for f in &l.files {
                m.insert(fold(&f.path), (f.path.clone(), Some(f.contents.bytes())));
            }
            m
        })
        .collect()
}

/// A name the lower layers show beneath an upper that holds nothing.
#[derive(Debug, Clone)]
struct LowerNode {
    /// As its layer spells it.
    path: String,
    dir: bool,
    bytes: Vec<u8>,
}

/// What the generator knows of a root served as an overlay, kept apart
/// from the oracle so that it can predict the overlay's answers
/// (`docs/wire.md` 3.4.9, "Names only in a read-only lower") rather than
/// take them: the layers as one tree, and which names the upper holds.
#[derive(Debug, Clone, Default)]
struct Layered {
    /// What the layers show by folded path, as the tree format combines
    /// them (`contract/scenarios/README.md`): the nearest layer holding a
    /// name, a file hiding what farther layers hold below it.
    view: BTreeMap<String, LowerNode>,
    /// Every folded path any layer holds, shown or not.
    raw: BTreeSet<String>,
    /// The names the upper holds: the tree's own, and every name made,
    /// moved there or changed since.
    upper: BTreeSet<String>,
    /// Names wire.md does not say are in the upper or not: a lower-only file
    /// a write open reached without changing it, a lower-only directory a
    /// name was made below or a MKDIR of it was answered. Requests whose
    /// answer would depend on it are not sent for them.
    unsure: BTreeSet<String>,
    /// Folded paths where, and below which, the upper hides the layers: a
    /// DELETE or RENAME of a name they hold, or the name an upper entry
    /// replaced one at.
    masked: BTreeSet<String>,
}

impl Layered {
    /// `root`'s layers as the generator keeps them, its upper as the tree
    /// has it.
    fn of(root: &Root) -> Layered {
        let layers = layer_names(root);
        let mut raw = BTreeSet::new();
        for l in &layers {
            raw.extend(l.keys().cloned());
        }
        // The layers making up each directory, from the root down, and the
        // nearest layer's entry at each name among its parent's.
        let mut keys: Vec<&String> = raw.iter().collect();
        keys.sort_by_key(|k| k.matches('/').count());
        let mut contrib: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        contrib.insert(".".into(), (0..layers.len()).collect());
        let mut view = BTreeMap::new();
        for k in keys {
            let Some(up) = contrib.get(parent(k)).cloned() else {
                continue;
            };
            let mut dirs = Vec::new();
            let mut shown = None;
            for i in up {
                match layers[i].get(k) {
                    Some((path, None)) => {
                        dirs.push(i);
                        shown.get_or_insert((path.clone(), None));
                    }
                    Some((path, Some(b))) => {
                        shown.get_or_insert((path.clone(), Some(b.clone())));
                        break;
                    }
                    None => {}
                }
            }
            if let Some((path, bytes)) = shown {
                if bytes.is_none() {
                    contrib.insert(k.clone(), dirs);
                }
                view.insert(
                    k.clone(),
                    LowerNode {
                        path,
                        dir: bytes.is_none(),
                        bytes: bytes.unwrap_or_default(),
                    },
                );
            }
        }
        let mut upper: BTreeSet<String> = BTreeSet::new();
        let ancestors = |p: &str| -> Vec<String> {
            let c: Vec<&str> = p.split('/').collect();
            (1..=c.len()).map(|n| fold(&c[..n].join("/"))).collect()
        };
        for p in root
            .files
            .iter()
            .map(|f| &f.path)
            .chain(&root.dirs)
            .chain(root.writable.iter().filter(|w| *w != "."))
        {
            upper.extend(ancestors(p));
        }
        Layered {
            view,
            raw,
            upper,
            ..Layered::default()
        }
    }

    /// Whether any layer holds folded `path` or anything below it.
    fn holds(&self, path: &str) -> bool {
        let below = format!("{path}/");
        self.raw.iter().any(|k| k == path || k.starts_with(&below))
    }

    /// The upper holds `path` now: no longer unsure.
    fn up(&mut self, path: &str) {
        self.unsure.remove(path);
        self.upper.insert(path.to_string());
    }

    /// What the upper held at and below `path`, gone, and the layers hidden
    /// there.
    fn gone(&mut self, path: &str) {
        self.upper.retain(|k| !under(k, path));
        self.unsure.retain(|k| !under(k, path));
        if self.holds(path) {
            self.masked.insert(path.to_string());
        }
    }
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
            let want = if r.two { 2 } else { 1 };
            while writable.len() < want {
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
        // The layers, each placed below the writable part: under a
        // writable entry, or anywhere in a root writable as a whole. A
        // name may be one the upper or a nearer layer has, in any case.
        let lower = if writable.is_empty() {
            Vec::new()
        } else {
            let below_writable =
                |d: &str| spec.whole || writable.iter().any(|w| under(&fold(d), &fold(w)));
            let mut upper_dirs: Vec<String> = if spec.whole {
                vec![".".to_string()]
            } else {
                Vec::new()
            };
            upper_dirs.extend(dirs.iter().filter(|d| below_writable(d)).cloned());
            r.lower
                .iter()
                .map(|entries| {
                    let mut layer = Layer::default();
                    let mut taken: BTreeSet<String> = BTreeSet::new();
                    let mut files_taken: BTreeSet<String> = BTreeSet::new();
                    let mut own: Vec<String> = Vec::new();
                    for e in entries {
                        let places: Vec<&String> = upper_dirs.iter().chain(&own).collect();
                        let dir = places[e.parent as usize % places.len()].clone();
                        if dir != "." && dir.split('/').count() >= 3 {
                            continue;
                        }
                        let name =
                            respell(NAMES[e.name as usize % NAMES.len()], u32::from(e.case) >> 2);
                        let path = join(&dir, &name);
                        let f = fold(&path);
                        // One name, of one kind, in a layer: nothing below
                        // a file of its own, no file over its own names.
                        let below_file = files_taken.iter().any(|t| under(&f, t));
                        let over_names = e.file.is_some()
                            && taken.iter().any(|t| t.starts_with(&format!("{f}/")));
                        if below_file || over_names || !taken.insert(f.clone()) {
                            continue;
                        }
                        if e.file.is_some() {
                            files_taken.insert(f);
                        }
                        match &e.file {
                            None => {
                                own.push(path.clone());
                                layer.dirs.push(path);
                            }
                            Some(fs) => layer.files.push(File {
                                path,
                                contents: match fs {
                                    FileSpec::Text(t) => Contents::Text(
                                        TEXTS[(*t as usize + 3) % TEXTS.len()].to_string(),
                                    ),
                                    FileSpec::Fill { size, seed } => Contents::Fill {
                                        size: u64::from(*size),
                                        seed: 100 + u64::from(*seed),
                                    },
                                },
                                mtime: 0,
                            }),
                        }
                    }
                    layer
                })
                .collect()
        };
        roots.push(Root {
            id,
            files,
            dirs,
            writable,
            lower,
        });
    }
    let registry = spec.registry.as_ref().map(|keys| {
        // A base key in each hive the shim virtualises (wire.md 4.2): HKLM,
        // the current user's, and the user's classes.
        let mut out: Vec<RegKey> = REG_BASES
            .iter()
            .map(|b| RegKey {
                path: b.to_string(),
                volatile: false,
                values: Vec::new(),
            })
            .collect();
        let mut taken: BTreeSet<String> = REG_BASES.iter().map(|b| fold(b)).collect();
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A WRITE or SETATTR on a directory handle no writable provider holds:
    /// `ST_IS_DIR` or `ST_READ_ONLY` (3.4.5).
    IsDirOrReadOnly,
    /// DELETE or RENAME of the root itself, or a RENAME `to` inside `from`:
    /// `ST_BAD_REQUEST` from a writable layer, `ST_IO_ERROR` from a
    /// disk-backed upper (3.4.7, 3.4.8).
    BadRequestOrIoError,
    /// Any of these statuses, the oracle's among them.
    Statuses(&'static [Status]),
    /// Any of these answers, the oracle's among them.
    Alts(Vec<Expect>),
}

/// A path below a file, to any opcode but GETATTR: `ST_NOT_FOUND` or
/// `ST_NOT_A_DIRECTORY` (3.5).
const BELOW_A_FILE: &[Status] = &[Status::NotFound, Status::NotADirectory];
/// A WRITE or SETATTR to a size no director can hold (3.4.5, 3.4.6).
const TOO_LARGE: &[Status] = &[Status::NoSpace, Status::BadRequest];
/// A request naming a named stream the director does not hold (3.2).
const A_STREAM: &[Status] = &[Status::NotFound, Status::BadRequest];
/// A write OPEN of a directory a writable provider serves (3.4.3).
const DIR_WRITE_OPEN: &[Status] = &[Status::IsDir, Status::IoError];
/// RENAME_X onto a directory (3.4.13).
const RENAME_X_ONTO_DIR: &[Status] = &[Status::IsDir, Status::IoError];

/// What the generator does right after a step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Then {
    Nothing,
    /// A GETATTR of this path, expecting the oracle's answer: what the step
    /// left (or did not change) is checked at once.
    GetAttr {
        root: u32,
        path: String,
    },
    /// The same request again, `ST_NOT_FOUND` (the first moved `from`) or
    /// `ST_IO_ERROR` (it refused again); then a READDIR of directory `dir`,
    /// as listed after the step or as listed before it; and the case ends.
    /// For a RENAME whose `one_of` answers leave different trees: the
    /// second answer shows whether the first moved anything, the listing
    /// that nothing else changed, and after it the oracle's tree may not be
    /// the director's.
    Listing {
        root: u32,
        dir: String,
    },
    /// The case ends: the step's allowed answers leave trees that differ,
    /// and the oracle's tree is one of them (wire.md 3.4.9: an overlay may
    /// answer a lower-only name by its list or by the general rules).
    End,
}

/// An answer that is `ST_OK` or this status, the oracle's first.
fn ok_or(first_ok: bool, status: Status) -> Want {
    let ok = Expect::Reply(OwnedReply::Empty);
    let st = Expect::Status(status);
    Want::Alts(if first_ok { vec![ok, st] } else { vec![st, ok] })
}

/// The notes of RENAMEs whose allowed answers leave trees that differ, after
/// which the case ends ([`Then::End`]).
const RENAME_ENDS: [&str; 4] = [
    "another case of a lower-only directory: the lower's spelling kept, or respelled",
    "another case of a lower-only file: the lower's spelling kept, or respelled",
    "a lower-only directory: refused, or moved whole",
    "another case of a directory both layers hold: respelled, or refused",
];

/// What a step changes in the generator's state when it succeeds.
#[derive(Debug, Clone)]
enum Effect {
    None,
    Opened {
        root: u32,
        path: String,
        dir: bool,
        write: bool,
        append: bool,
        /// The open created the file or truncated it.
        emptied: bool,
        /// The open created the file.
        created: bool,
        /// The open changed the file's bytes (a truncation of a file that
        /// had some).
        changed: bool,
    },
    /// A WRITE or SETATTR through this handle; `true` when it changed the
    /// file's bytes or size.
    Wrote(u64, bool),
    /// A WRITE or SETATTR through this handle, whose file is gone, may or
    /// may not have changed what it reads.
    Spent(u64),
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
    /// MKDIR made a directory at a missing name.
    Made {
        root: u32,
        path: String,
    },
    /// MKDIR made a directory over a lower-only file, which it hides: to
    /// the file's handles a deleted file (wire.md 3.4.9, 3.4.8).
    MadeOver {
        root: u32,
        path: String,
    },
    /// MKDIR of a directory only a lower holds, whatever it was answered:
    /// whether the upper holds it now is open ([`Layered::unsure`]).
    Unsure {
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
    then: Then,
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
    /// Folded path of what it opened: for a file handle whose file is still
    /// there, where the file is now ([`Gen::locate`]).
    folded: String,
    dir: bool,
    write: bool,
    append: bool,
    /// The file it holds, while the file is there (wire.md 3.4.7: a handle
    /// follows its file through RENAMEs); `None` for a directory handle and
    /// once the file is gone.
    file: Option<u64>,
    /// What a handle whose file is gone answers.
    gone: Option<Gone>,
    /// For a directory handle: nothing renamed or deleted the directory or
    /// a directory above it since it was opened (wire.md says nothing of
    /// such a handle's answers, so it is then only closed).
    clean: bool,
}

/// A file handle whose file was deleted, or replaced by a RENAME or
/// RENAME_X onto its name (wire.md 3.4.8), or moved to another root by a
/// RENAME_X (3.4.7: it may follow the file or answer as for a deleted
/// file).
#[derive(Debug, Clone)]
struct Gone {
    /// The file's bytes when it went: what a director that keeps it serves.
    snap: Vec<u8>,
    /// The file it held: every handle on one gone file shares it (wire.md
    /// 3.4.8: a kept file is one file to all its handles).
    of: u64,
    /// Deleted or replaced: a WRITE or SETATTR through it is `ST_OK`
    /// (seen by the file's other handles, by nothing through a name),
    /// `ST_IO_ERROR` or `ST_NOT_FOUND`. A handle a RENAME_X moved is never written through,
    /// since a director that follows the file writes the file at `to`.
    writes: bool,
    /// The file a RENAME_X moved, which a director may follow: once
    /// anything changes it, the handle's READs depend on whether the
    /// director followed, and the handle is spent.
    follows: Option<u64>,
    /// A WRITE or SETATTR went through it or another handle of its file, or
    /// the file it may follow changed: what it reads now depends on the
    /// director's choice, so it is only written to and closed.
    spent: bool,
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
    /// The identity of each file an open handle holds, by where the file is
    /// now: a RENAME moves it, a DELETE or a RENAME onto its name ends it.
    ids: BTreeMap<(u32, String), u64>,
    next_id: u64,
    handles: BTreeMap<u64, Handle>,
    closed: Vec<u64>,
    /// Registry versions seen so far, in the oracle's numbering.
    versions: Vec<u64>,
    /// Key paths to pick from: the tree's and every one sent since.
    keys: Vec<String>,
    /// Keys a REG_DELETE_KEY tombstoned.
    tombstoned: Vec<String>,
    /// The (root, folded path)s the last steps named, the latest last.
    recent: Vec<(u32, String)>,
    /// The handles the last steps used, the latest last.
    recent_fh: Vec<u64>,
    /// The op being planned's `e`: whether it picks among what the last
    /// steps touched.
    focus: u32,
    /// Clients besides client 0 that send steps.
    others: u8,
    /// A step whose allowed answers leave different trees ended the case.
    done: bool,
    steps: Vec<Step>,
    /// The roots served as overlays, by id ([`Layered`]).
    layered: BTreeMap<u32, Layered>,
}

/// How many recent paths and handles [`Gen`] keeps for its locality bias.
const RECENT: usize = 8;
/// The bits of an op's `e` that make it pick, among what the last steps
/// touched: an existing name ([`Gen::existing`]), a missing one
/// ([`Gen::missing`]), a RENAME's existing `to` ([`Gen::existing_to`]), a
/// handle ([`Gen::handle_for`]). Bits 8 and up pick which, the latest
/// first, and bits 16 and up the client.
const NEAR_EXISTING: u32 = 1;
const NEAR_MISSING: u32 = 2;
const NEAR_TO: u32 = 4;
const NEAR_HANDLE: u32 = 8;

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
    let mut g = Gen::new(&tree, roots.clone(), limits)?;
    g.others = input.others;
    for op in &input.ops {
        if op.kind == Kind::Template {
            for sub in expand(op, &roots) {
                g.op(&sub)?;
            }
        } else {
            g.op(op)?;
        }
    }
    g.finish()?;
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
            ids: BTreeMap::new(),
            next_id: 1,
            handles: BTreeMap::new(),
            closed: Vec::new(),
            versions: Vec::new(),
            keys,
            tombstoned: Vec::new(),
            recent: Vec::new(),
            recent_fh: Vec::new(),
            focus: 0,
            others: 0,
            done: false,
            steps: Vec::new(),
            layered: tree
                .roots
                .iter()
                .filter(|r| !r.lower.is_empty())
                .map(|r| (r.id, Layered::of(r)))
                .collect(),
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
    /// writable entry but an entry itself or its ancestors (an entry inside
    /// another), or anything but the root itself in a root writable as a
    /// whole.
    fn held(&self, root: u32, path: &str) -> bool {
        let Some(r) = self.info(root) else {
            return false;
        };
        if r.whole {
            return path != ".";
        }
        r.entries.iter().any(|w| path.starts_with(&format!("{w}/")))
            && !r.entries.iter().any(|w| under(w, path))
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

    // --- Layers (wire.md 3.4.9, "Names only in a read-only lower").

    /// What `root`'s layers show at folded `path` now, by the generator's
    /// own reckoning: nothing where a mask or a file of the upper's at or
    /// above it hides them, or where a directory above it is not a layer's.
    fn lower_shows(&self, root: u32, path: &str) -> Option<&LowerNode> {
        let l = self.layered.get(&root)?;
        let upper_file = |p: &str| {
            (l.upper.contains(p) || l.unsure.contains(p))
                && self.node(root, p).is_some_and(|n| !n.dir)
        };
        let c: Vec<&str> = path.split('/').collect();
        for n in 1..c.len() {
            let a = c[..n].join("/");
            if l.masked.contains(&a) || upper_file(&a) || !l.view.get(&a).is_some_and(|x| x.dir) {
                return None;
            }
        }
        if l.masked.contains(path) || upper_file(path) {
            return None;
        }
        l.view.get(path)
    }

    /// Whether the existing name at `root`'s folded `path` is one only the
    /// layers hold: `Some(false)` when the upper holds it (and in a root
    /// with no layers), `None` when wire.md leaves it open
    /// ([`Layered::unsure`]).
    fn lower_only(&self, root: u32, path: &str) -> Option<bool> {
        let Some(l) = self.layered.get(&root) else {
            return Some(false);
        };
        if l.unsure.contains(path) {
            return None;
        }
        if l.upper.contains(path) {
            return Some(false);
        }
        Some(self.lower_shows(root, path).is_some())
    }

    /// Whether a layer holds a directory at `root`'s folded `path`, the upper
    /// holding it too or not.
    fn lower_dir(&self, root: u32, path: &str) -> bool {
        self.lower_shows(root, path).is_some_and(|n| n.dir)
    }

    /// The directories above `root`'s folded `path` that only the layers
    /// show, which a name made at `path` may put in the upper.
    fn unsure_parents(&mut self, root: u32, path: &str) {
        let c: Vec<&str> = path.split('/').collect();
        let up: Vec<String> = (1..c.len())
            .map(|n| c[..n].join("/"))
            .filter(|a| self.lower_only(root, a) == Some(true))
            .collect();
        if let Some(l) = self.layered.get_mut(&root) {
            l.unsure.extend(up);
        }
    }

    /// The generator's reckoning of every layered root against the oracle's
    /// listings: each name the oracle lists is the upper's, or one the
    /// layers show (with the layer's spelling, kind and size), and each
    /// name only the layers show is listed. A difference is the reference
    /// answering an overlay otherwise than the generator predicts.
    fn check_layers(&self) -> Result<(), String> {
        for (&root, l) in &self.layered {
            for n in self.all(root).iter().filter(|n| n.folded != ".") {
                if l.upper.contains(&n.folded) || l.unsure.contains(&n.folded) {
                    continue;
                }
                let Some(low) = self.lower_shows(root, &n.folded) else {
                    return Err(format!(
                        "generator: after step {}, root {root} lists {:?}, which the upper \
                         does not hold and no layer shows (wire.md 3.4.9)",
                        self.steps.len(),
                        n.path
                    ));
                };
                let size = if low.dir { 0 } else { low.bytes.len() as u64 };
                if (low.dir, leaf(&low.path), size) != (n.dir, leaf(&n.path), n.size) {
                    return Err(format!(
                        "generator: after step {}, root {root} lists {:?} (directory {}, size {}), \
                         which only a layer holds, as {:?} (directory {}, size {size}) \
                         (wire.md 3.4.9, 3.2)",
                        self.steps.len(),
                        n.path,
                        n.dir,
                        n.size,
                        low.path,
                        low.dir
                    ));
                }
            }
            for k in l.view.keys() {
                let lower_only = !l.upper.contains(k) && !l.unsure.contains(k);
                if lower_only && self.lower_shows(root, k).is_some() && self.node(root, k).is_none()
                {
                    return Err(format!(
                        "generator: after step {}, root {root} does not list {k:?}, which a \
                         layer shows and nothing hid (wire.md 3.4.9)",
                        self.steps.len()
                    ));
                }
            }
        }
        Ok(())
    }

    /// What the generator predicts of the oracle's answer `got` to `req`,
    /// beyond its status: a READ through a live handle answers the file's
    /// bytes as they are now, the layer's while only a layer holds the file
    /// (wire.md 3.4.4); a STORED_NAMES component only a layer holds has
    /// the layer's spelling (3.2, 3.4.12); a read open of a file the
    /// writable provider holds, a lower-only one included, is not immutable
    /// (3.4.3). A difference is the reference answering otherwise.
    fn check_prediction(
        &self,
        req: &OwnedRequest,
        got: &Result<OwnedReply, Status>,
    ) -> Result<(), String> {
        let differ = |what: String| {
            Err(format!(
                "generator: step {}: {req:?}: the generator predicts {what}, but the \
                 reference answered {}",
                self.steps.len(),
                crate::compare::render_got(got)
            ))
        };
        match (req, got) {
            (
                OwnedRequest::Read {
                    fh, offset, len, ..
                },
                Ok(OwnedReply::Read(data)),
            ) => {
                let Some(h) = self.handles.get(fh) else {
                    return Ok(());
                };
                if h.dir || h.gone.is_some() {
                    return Ok(());
                }
                let bytes = match self.lower_only(h.root, &h.folded) {
                    Some(true) => match self.lower_shows(h.root, &h.folded) {
                        Some(n) => n.bytes.clone(),
                        None => return Ok(()),
                    },
                    _ => self.bytes_at(h.root, &h.folded),
                };
                let from = (*offset).min(bytes.len() as u64) as usize;
                let to = (from as u64)
                    .saturating_add(u64::from(*len))
                    .min(bytes.len() as u64) as usize;
                if data[..] != bytes[from..to] {
                    return differ(format!("the {} bytes the file holds at {from}", to - from));
                }
            }
            (
                OwnedRequest::StoredNames { root, skip, path },
                Ok(OwnedReply::StoredNames(names)),
            ) if self.layered.contains_key(&root.0) => {
                let c: Vec<&str> = path.split('/').collect();
                let got: Vec<&str> = names.split('/').collect();
                for i in (*skip as usize)..c.len() {
                    let k = fold(&c[..=i].join("/"));
                    if self.node(root.0, &k).is_none() || self.lower_only(root.0, &k) != Some(true)
                    {
                        continue;
                    }
                    let Some(n) = self.lower_shows(root.0, &k) else {
                        continue;
                    };
                    let want = leaf(&n.path);
                    if got.get(i - *skip as usize) != Some(&want) {
                        return differ(format!(
                            "component {i} spelled {want:?}, as its layer has it"
                        ));
                    }
                }
            }
            (OwnedRequest::Open { root, flags, path }, Ok(OwnedReply::Open(o)))
                if !flags.contains(OpenFlags::WRITE)
                    && !o.is_dir
                    && o.immutable
                    && self.held(root.0, &fold(path)) =>
            {
                return differ("a handle that is not immutable".into());
            }
            _ => {}
        }
        Ok(())
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
        let node = self
            .near(root, &all, NEAR_EXISTING)
            .or_else(|| pick(&all, n).cloned())?;
        Some(self.target(root, respell_path(&node.path, bits)))
    }

    /// With the op's `e` asking for it (`bit`), one of `candidates` that the
    /// last steps named, the latest first, as `e` picks: so chains of
    /// requests on the same names are common.
    fn near(&self, root: u32, candidates: &[Node], bit: u32) -> Option<Node> {
        if self.focus & bit == 0 {
            return None;
        }
        let mut seen = BTreeSet::new();
        let near: Vec<&Node> = self
            .recent
            .iter()
            .rev()
            .filter(|(r, p)| *r == root && seen.insert(p.clone()))
            .filter_map(|(_, p)| candidates.iter().find(|c| &c.folded == p))
            .collect();
        pick(&near, self.focus >> 8).map(|n| (*n).clone())
    }

    /// A name `root` does not have, under an existing directory that `ok`
    /// accepts: now and then (`e`) one the last steps named, which a DELETE
    /// or RENAME took away. The parent is respelled as `bits` picks (a
    /// creating request's parents keep their stored spelling, wire.md 3.2);
    /// the leaf is from [`NAMES`], spelled as `bits` picks.
    fn missing(&self, root: u32, n: u32, bits: u32, ok: impl Fn(&Node) -> bool) -> Option<Target> {
        let dirs: Vec<Node> = self
            .all(root)
            .into_iter()
            .filter(|x| x.dir && ok(x) && x.path.split('/').count() < 4)
            .collect();
        if self.focus & NEAR_MISSING != 0 {
            let gone: Vec<&(u32, String)> = self
                .recent
                .iter()
                .rev()
                .filter(|(r, p)| {
                    *r == root
                        && p != "."
                        && self.node(root, p).is_none()
                        && dirs.iter().any(|d| d.folded == parent(p))
                })
                .collect();
            if let Some((_, p)) = pick(&gone, self.focus >> 8) {
                let dir = dirs.iter().find(|d| d.folded == parent(p))?;
                let path = join(
                    &respell_path(&dir.path, bits.rotate_left(11)),
                    &respell(leaf(p), bits),
                );
                return Some(self.target(root, path));
            }
        }
        let dir = pick(&dirs, n)?;
        (0..NAMES.len() as u32).find_map(|i| {
            let leaf = respell(
                NAMES[((bits >> 4).wrapping_add(i)) as usize % NAMES.len()],
                bits,
            );
            let t = self.target(
                root,
                join(&respell_path(&dir.path, bits.rotate_left(11)), &leaf),
            );
            t.node.is_none().then_some(t)
        })
    }

    /// A path below an existing file of `root`: a missing path (3.5).
    fn below_file(&self, root: u32, n: u32, bits: u32) -> Option<Target> {
        let f = self.existing(root, n, bits, |x| !x.dir)?;
        let leaf = NAMES[(bits >> 9) as usize % NAMES.len()];
        Some(self.target(root, join(&f.path, leaf)))
    }

    /// A named stream of an existing file of `root` that no request made:
    /// a name the director does not have (3.2).
    fn stream_of(&self, root: u32, n: u32, bits: u32) -> Option<Target> {
        let f = self.existing(root, n, bits, |x| !x.dir)?;
        let stream = ["zone.identifier", "s", "x"][(bits >> 9) as usize % 3];
        let mut t = self.target(root, format!("{}:{stream}", f.path));
        // The base file, for the GETATTR after.
        t.folded = f.folded;
        Some(t)
    }

    /// `req`, naming stream target `t`: `ST_NOT_FOUND` or `ST_BAD_REQUEST`
    /// from a director that holds no named streams, and the base file is
    /// unchanged after it (3.2).
    fn stream_plan(&self, root: u32, req: OwnedRequest, t: &Target) -> Plan {
        Plan {
            req,
            want: Want::Statuses(A_STREAM),
            wire: "3.2",
            note: "a named stream the director does not hold; the base file is unchanged".into(),
            effect: Effect::None,
            then: Then::GetAttr {
                root,
                path: t.folded.clone(),
            },
        }
    }

    /// A name under a missing directory the writable provider would hold,
    /// or (`below_file`) under a file it holds: a create there finds no
    /// parent (wire.md 3.4.3, 3.4.7, 3.4.9).
    fn orphan_name(&self, root: u32, n: u32, bits: u32, below_file: bool) -> Option<Target> {
        let parent = if below_file {
            self.existing(root, n, bits, |x| !x.dir && self.held(root, &x.folded))?
        } else {
            self.missing_held(root, n, bits)?
        };
        let leaf = respell(NAMES[(bits >> 9) as usize % NAMES.len()], bits >> 3);
        Some(self.target(root, join(&parent.path, &leaf)))
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
        let files = self.open_handles(|h| !h.dir && ok(h));
        let dirs = self.open_handles(|h| h.clean && h.dir);
        let list = if files.is_empty() || (!dirs.is_empty() && (d >> 4).is_multiple_of(8)) {
            dirs
        } else {
            files
        };
        if self.focus & NEAR_HANDLE != 0 {
            let near: Vec<&(u64, Handle)> = self
                .recent_fh
                .iter()
                .rev()
                .filter_map(|fh| list.iter().find(|(f, _)| f == fh))
                .collect();
            if let Some(h) = pick(&near, self.focus >> 8) {
                return Some((*h).clone());
            }
        }
        pick(&list, a).cloned()
    }

    /// What a WRITE or SETATTR on directory handle `h` answers (3.4.5):
    /// `ST_IS_DIR` for a directory the writable provider holds, else
    /// `ST_IS_DIR` or `ST_READ_ONLY` (a read-only provider's directory
    /// handle may refuse the write as read-only).
    fn dir_handle_want(&self, h: &Handle) -> Want {
        if self.held(h.root, &h.folded) {
            Want::Status(Status::IsDir)
        } else {
            Want::IsDirOrReadOnly
        }
    }

    /// Marks every directory handle on `root`'s `path` or below it as
    /// renamed or deleted under it.
    fn taint_dirs(&mut self, root: u32, path: &str) {
        for h in self.handles.values_mut() {
            if h.dir && h.root == root && under(&h.folded, path) {
                h.clean = false;
            }
        }
    }

    fn size_of(&self, root: u32, folded: &str) -> u64 {
        self.node(root, folded).map(|n| n.size).unwrap_or(0)
    }

    /// The size of the file behind file handle `h`: the file where it is
    /// now, or the bytes it had when it went.
    fn handle_size(&self, h: &Handle) -> u64 {
        match &h.gone {
            Some(g) => g.snap.len() as u64,
            None => self.size_of(h.root, &h.folded),
        }
    }

    /// The identity of the file at `root`'s folded `path`, given one if it
    /// has none yet.
    fn id_at(&mut self, root: u32, path: &str) -> u64 {
        if let Some(id) = self.ids.get(&(root, path.to_string())) {
            return *id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.ids.insert((root, path.to_string()), id);
        id
    }

    /// The bytes the oracle holds for the file at `root`'s folded `path`.
    fn bytes_at(&self, root: u32, path: &str) -> Vec<u8> {
        self.oracle
            .d
            .contents(&aether_refdirector::key(root, path))
            .unwrap_or_default()
    }

    /// Every open file handle's file's bytes, by identity: taken before a
    /// step that may end or move files, for the handles it leaves behind.
    fn snapshots(&self) -> BTreeMap<u64, Vec<u8>> {
        self.ids
            .iter()
            .filter(|(_, id)| self.handles.values().any(|h| h.file == Some(**id)))
            .map(|((r, p), id)| (*id, self.bytes_at(*r, p)))
            .collect()
    }

    /// A WRITE or SETATTR went through gone handle `fh`. The oracle fails
    /// it, but a director that keeps the file answers it `ST_OK`, and then
    /// every handle of that file reads the change (wire.md 3.4.8): each of
    /// them is spent, whatever the oracle answered.
    fn spend(&mut self, fh: u64) {
        let of = self
            .handles
            .get(&fh)
            .and_then(|h| h.gone.as_ref())
            .map(|g| g.of);
        for h in self.handles.values_mut() {
            if let Some(g) = h.gone.as_mut() {
                if Some(g.of) == of {
                    g.spent = true;
                }
            }
        }
    }

    /// The file `id` changed: a handle that may follow it there (RENAME_X)
    /// is spent.
    fn changed_file(&mut self, id: u64) {
        for h in self.handles.values_mut() {
            if let Some(g) = &mut h.gone {
                if g.follows == Some(id) {
                    g.spent = true;
                }
            }
        }
    }

    /// The files at and below `root`'s `path` end (deleted, or replaced by
    /// a rename): their handles keep what `snaps` says they held.
    fn end_files(&mut self, root: u32, path: &str, snaps: &BTreeMap<u64, Vec<u8>>) {
        let ended: Vec<((u32, String), u64)> = self
            .ids
            .iter()
            .filter(|((r, p), _)| *r == root && under(p, path))
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (k, id) in ended {
            self.ids.remove(&k);
            for h in self.handles.values_mut() {
                if h.file == Some(id) {
                    h.file = None;
                    h.gone = Some(Gone {
                        snap: snaps.get(&id).cloned().unwrap_or_default(),
                        of: id,
                        writes: true,
                        follows: None,
                        spent: false,
                    });
                }
            }
        }
    }

    /// What a RENAME or RENAME_X of `from` to `to` does to the layers'
    /// bookkeeping: what the upper held at `to` is replaced and the layers
    /// hidden there, the upper's names below `from` move below `to`, `to`
    /// is the upper's (a lower-only file is copied up), and the layers are
    /// hidden at `from` (wire.md 3.4.7, 3.4.9, 3.4.13).
    fn moved_layers(&mut self, from_root: u32, from: &str, to_root: u32, to: &str) {
        let moved: Vec<String> = self
            .layered
            .get(&from_root)
            .map(|l| {
                l.upper
                    .iter()
                    .filter(|k| under(k, from) && *k != from)
                    .map(|k| format!("{to}{}", &k[from.len()..]))
                    .collect()
            })
            .unwrap_or_default();
        // A lower-only `from` is copied up first, its directories with it.
        self.unsure_parents(from_root, from);
        if let Some(l) = self.layered.get_mut(&from_root) {
            l.gone(from);
        }
        self.unsure_parents(to_root, to);
        if let Some(l) = self.layered.get_mut(&to_root) {
            l.gone(to);
            l.up(to);
            for k in moved {
                l.up(&k);
            }
        }
    }

    /// Moves every live handle's place to where its file is now.
    fn follow(&mut self) {
        let at: BTreeMap<u64, (u32, String)> =
            self.ids.iter().map(|(k, id)| (*id, k.clone())).collect();
        for h in self.handles.values_mut() {
            if let Some((r, p)) = h.file.and_then(|id| at.get(&id)) {
                h.root = *r;
                h.folded = p.clone();
            }
        }
    }

    // --- Choosing a request.

    fn plan(&self, op: &RawOp) -> Option<Plan> {
        let RawOp {
            kind, a, b, c, d, ..
        } = *op;
        let plan = |req, want, wire, note: &str, effect| {
            Some(Plan {
                req,
                want,
                wire,
                note: note.to_string(),
                effect,
                then: Then::Nothing,
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
                if b % 4 == 0 && (d >> 8) % 4 != 0 {
                    // A path below a file, or a named stream the director
                    // does not hold: missing (3.5, 3.2).
                    let stream = (d >> 8) % 4 == 2;
                    let (t, note) = if stream {
                        (
                            self.stream_of(root, a >> 8, c)?,
                            "a named stream: found = 0",
                        )
                    } else {
                        (self.below_file(root, a >> 8, c)?, "below a file: found = 0")
                    };
                    return Some(Plan {
                        req: OwnedRequest::GetAttr {
                            root: RootId(root),
                            path: t.path,
                        },
                        want: Want::Ok,
                        wire: if stream { "3.4.1, 3.2" } else { "3.4.1, 3.5" },
                        note: note.into(),
                        effect: Effect::None,
                        then: Then::Nothing,
                    });
                }
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
                let odd = match (d >> 8) % 6 {
                    1 => Some((self.missing(root, b, c, |_| true)?, false)),
                    2 => Some((self.existing(root, b, c, |n| !n.dir)?, true)),
                    3 => Some((self.below_file(root, b, c)?, true)),
                    _ => None,
                };
                if let Some((t, file)) = odd {
                    // A missing directory: an empty listing or
                    // ST_NOT_FOUND; a file, or a path below one, also
                    // ST_NOT_A_DIRECTORY (3.4.2, 3.5).
                    let mut alts = vec![
                        Expect::Reply(OwnedReply::ReadDir(Vec::new())),
                        Expect::Status(Status::NotFound),
                    ];
                    if file {
                        alts.push(Expect::Status(Status::NotADirectory));
                    }
                    let note = match (t.node.is_some(), file) {
                        (true, _) => "a file",
                        (false, true) => "below a file",
                        (false, false) => "a missing directory",
                    };
                    return Some(Plan {
                        req: OwnedRequest::ReadDir {
                            root: RootId(root),
                            path: t.path,
                        },
                        want: Want::Alts(alts),
                        wire: "3.4.2",
                        note: note.into(),
                        effect: Effect::None,
                        then: Then::Nothing,
                    });
                }
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
                let near: Vec<&(u64, Handle)> = self
                    .recent_fh
                    .iter()
                    .rev()
                    .filter_map(|fh| hs.iter().find(|(f, _)| f == fh))
                    .collect();
                let (fh, h) = match pick(&near, self.focus >> 8) {
                    Some(h) if self.focus & NEAR_HANDLE != 0 => *h,
                    _ => pick(&hs, a)?,
                };
                let note = if h.gone.is_some() {
                    "a handle whose file is gone"
                } else if !h.clean {
                    "a directory handle whose directory moved or went"
                } else {
                    ""
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
                then: Then::Nothing,
            });
        }
        let root = self.root(a);
        if b.is_multiple_of(4) && (d >> 8) % 4 == 1 {
            let t = self.below_file(root, a >> 8, c)?;
            return Some(Plan {
                req: OwnedRequest::Open {
                    root: RootId(root),
                    flags,
                    path: t.path,
                },
                want: Want::Statuses(BELOW_A_FILE),
                wire: "3.5",
                note: "below a file: a missing path".into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        if b.is_multiple_of(4) && (d >> 8) % 4 == 2 {
            let t = self.stream_of(root, a >> 8, c)?;
            return Some(self.stream_plan(
                root,
                OwnedRequest::Open {
                    root: RootId(root),
                    flags,
                    path: t.path.clone(),
                },
                &t,
            ));
        }
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
                    write: false,
                    append: false,
                    emptied: false,
                    created: false,
                    changed: false,
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
            then: Then::Nothing,
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
        let mut no_parent = None;
        let t = match (b % 5, (d >> 8) % 4) {
            (0 | 1, _) => self.existing(root, b >> 3, c, |n| !n.dir && self.held(root, &n.folded)),
            (2, _) => self.missing_held(root, b >> 3, c),
            (3, _) => self.existing(root, b >> 3, c, |n| {
                !n.dir && self.read_only(root, &n.folded)
            }),
            (_, 0) => self.existing(root, b >> 3, c, |n| {
                n.dir && (self.held(root, &n.folded) || self.read_only(root, &n.folded))
            }),
            (_, k @ (1 | 2)) => {
                no_parent = Some(k == 2);
                self.orphan_name(root, b >> 3, c, k == 2)
            }
            _ if !self.has_writable(root) => self.missing(root, b >> 3, c, |_| true),
            _ => self.missing_held(root, b >> 3, c),
        }?;
        if let Some(below_file) = no_parent {
            // No parent to create it in (3.4.3); a path below a file is a
            // missing path (3.5).
            let (want, note) = if below_file {
                (Want::Statuses(BELOW_A_FILE), "a name below a file")
            } else {
                (
                    Want::Status(Status::NotFound),
                    "a name in a missing directory",
                )
            };
            return Some(Plan {
                req: OwnedRequest::Open {
                    root: RootId(root),
                    flags,
                    path: t.path,
                },
                want,
                wire: "3.4.3, 3.5",
                note: note.into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        let (want, note) = match &t.node {
            Some(n) if n.dir && self.read_only(root, &t.folded) => (
                Want::Status(Status::ReadOnly),
                "a directory no writable provider holds",
            ),
            Some(n) if n.dir && self.held(root, &t.folded) => {
                if flags.contains(OpenFlags::EXCL) {
                    (
                        Want::Status(Status::Exists),
                        "OPEN_EXCL on an existing directory",
                    )
                } else {
                    (
                        Want::Statuses(DIR_WRITE_OPEN),
                        "a write open of a directory",
                    )
                }
            }
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
                write: true,
                append: flags.contains(OpenFlags::APPEND),
                emptied: t.node.is_none() || flags.contains(OpenFlags::TRUNC),
                created: t.node.is_none(),
                changed: flags.contains(OpenFlags::TRUNC)
                    && t.node.as_ref().is_some_and(|n| n.size > 0),
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
            then: Then::Nothing,
        })
    }

    fn plan_read(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        // Any file handle, whatever access it was opened with (3.4.4).
        let (fh, h) = &self.handle_for(a, d, |h| h.gone.as_ref().is_none_or(|g| !g.spent))?;
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
                then: Then::Nothing,
            });
        }
        let size = self.handle_size(h);
        let r = u64::from(b >> 4);
        let offset = match b % 9 {
            0 => 0,
            1 => size,
            2 => size + r % 50,
            3 => r % (size + 1),
            4 => (r % 7) | 1,
            5 => size.saturating_sub(1),
            // The later half, where a WRITE past the end or a SETATTR left
            // zeros.
            6 => size - r % (size / 2 + 1),
            // Far past any end: no wrap, 0 bytes (3.4.4).
            7 => [u64::MAX - r % 16, 1 << 63, 1 << 32, u64::MAX][(r >> 4) as usize % 4],
            _ => r % (size + 1),
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
        // `len` u32::MAX, answered in bulk, where what is left fits any
        // bank: the whole rest (3.4.4).
        let rest = size.saturating_sub(offset);
        if self.limits.arena && (c >> 24) % 6 == 5 && rest <= 256 {
            len = u64::from(u32::MAX);
        }
        let req = OwnedRequest::Read {
            slot_flags,
            fh: *fh,
            offset,
            len: len as u32,
        };
        if let Some(g) = &h.gone {
            let from = offset.min(g.snap.len() as u64) as usize;
            let to = (from as u64).saturating_add(len).min(g.snap.len() as u64) as usize;
            return Some(Plan {
                req,
                want: Want::Alts(vec![
                    Expect::Reply(OwnedReply::Read(g.snap[from..to].to_vec())),
                    Expect::Status(Status::IoError),
                    Expect::Status(Status::NotFound),
                ]),
                wire: "3.4.8",
                note: "a handle whose file is gone: its last bytes, or a failure".into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        Some(Plan {
            req,
            want: Want::Ok,
            wire: "3.4.4",
            note: String::new(),
            effect: Effect::None,
            then: Then::Nothing,
        })
    }

    /// What a WRITE or SETATTR through a handle whose file was deleted or
    /// replaced answers (3.4.8): `ST_OK`, seen by that file's other handles
    /// and by nothing through a name, or a failure. Every handle of the file
    /// is spent after it.
    fn orphan_change(&self, req: OwnedRequest, ok: OwnedReply, fh: u64) -> Option<Plan> {
        Some(Plan {
            req,
            want: Want::Alts(vec![
                Expect::Reply(ok),
                Expect::Status(Status::IoError),
                Expect::Status(Status::NotFound),
            ]),
            wire: "3.4.8",
            note: "a handle whose file is gone: its file's other handles see it, no name does; or it fails".into(),
            effect: Effect::Spent(fh),
            then: Then::Nothing,
        })
    }

    /// A WRITE or SETATTR to a size no director can hold, and a GETATTR of
    /// the file after it, which it did not change (3.4.5, 3.4.6).
    fn too_large(&self, req: OwnedRequest, h: &Handle, wire: &'static str) -> Option<Plan> {
        Some(Plan {
            req,
            want: Want::Statuses(TOO_LARGE),
            wire,
            note: "past the largest file a director can hold; the file is unchanged".into(),
            effect: Effect::None,
            then: Then::GetAttr {
                root: h.root,
                path: h.folded.clone(),
            },
        })
    }

    fn plan_write(&self, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let (fh, h) = &self.handle_for(a, d, |h| {
            h.write && h.gone.as_ref().is_none_or(|g| g.writes)
        })?;
        if h.dir {
            return Some(Plan {
                req: OwnedRequest::Write {
                    fh: *fh,
                    offset: 0,
                    data: b"x".to_vec(),
                },
                want: self.dir_handle_want(h),
                wire: "3.4.5",
                note: "a directory handle".into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        let n = match (c >> 4) % 8 {
            0 => 0,
            1..=4 => 1 + c % 16,
            5 => 17 + c % 284,
            6 => 4096,
            _ => 20_000,
        }
        .min(self.limits.write as u32) as usize;
        let data = fill(n, u64::from(d));
        let size = self.handle_size(h);
        let r = u64::from(b >> 4);
        if h.gone.is_some() {
            let req = OwnedRequest::Write {
                fh: *fh,
                offset: r % (size + 1),
                data,
            };
            return self.orphan_change(req, OwnedReply::Write(n as u32), *fh);
        }
        if b % 16 == 15 && n > 0 && !h.append {
            // Its end past u64::MAX, or past 2^63.
            let offset = if r & 1 == 0 {
                u64::MAX - (r >> 1) % n as u64
            } else {
                1 << 63
            };
            let req = OwnedRequest::Write {
                fh: *fh,
                offset,
                data,
            };
            return self.too_large(req, h, "3.4.5");
        }
        let offset = if h.append {
            // Ignored on an append handle (3.4.3, 3.4.5).
            r % 1000
        } else {
            match b % 6 {
                0 => 0,
                1 => size,
                2 => r % (size + 1),
                3 => size.saturating_sub(1),
                // Past the end: the gap reads as zeros (3.4.5).
                4 => size + 1 + r % 70_000,
                _ => size + r % 16,
            }
        };
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
            } else if offset > size && n > 0 {
                "past the end: the gap reads as zeros".into()
            } else {
                String::new()
            },
            effect: Effect::Wrote(*fh, n > 0),
            then: Then::Nothing,
        })
    }

    fn plan_setattr(&self, a: u32, b: u32, c: u32) -> Option<Plan> {
        let (fh, h) = &self.handle_for(a, c, |h| {
            h.write && h.gone.as_ref().is_none_or(|g| g.writes)
        })?;
        if h.dir {
            return Some(Plan {
                req: OwnedRequest::SetAttr { fh: *fh, size: 0 },
                want: self.dir_handle_want(h),
                wire: "3.4.5",
                note: "a directory handle".into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        let size = self.handle_size(h);
        let to = match b % 8 {
            0 => 0,
            1 => size,
            2 => size / 2,
            3 => size + u64::from(c % 100),
            4 => size + u64::from(c >> 8) % 70_000,
            7 if h.gone.is_none() => {
                let req = OwnedRequest::SetAttr {
                    fh: *fh,
                    size: [u64::MAX, 1 << 63][(c & 1) as usize],
                };
                return self.too_large(req, h, "3.4.6");
            }
            _ => u64::from(c) % (size + 1),
        };
        let req = OwnedRequest::SetAttr { fh: *fh, size: to };
        if h.gone.is_some() {
            return self.orphan_change(req, OwnedReply::Empty, *fh);
        }
        Some(Plan {
            req,
            want: Want::Ok,
            wire: "3.4.6",
            note: if to > size {
                "an extension: it reads as zeros".into()
            } else {
                String::new()
            },
            effect: Effect::Wrote(*fh, to != size),
            then: Then::Nothing,
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
                then: Then::Nothing,
            });
        }
        let root = self.writable_root(a)?;
        let held_node = |n: &Node| self.held(root, &n.folded);
        let sub = (a >> 24) % 8;
        let (from, to) = match b % 12 {
            // `to` in a missing directory, or below a file (3.4.7).
            0 | 7 if sub == 3 || sub == 4 => {
                let from = self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?;
                let to = self.orphan_name(root, b >> 4, d, sub == 4)?;
                if under(&to.folded, &from.folded) {
                    return None;
                }
                let (want, note) = if sub == 4 {
                    (Want::Statuses(BELOW_A_FILE), "`to` below a file")
                } else {
                    (
                        Want::Status(Status::NotFound),
                        "`to` in a missing directory",
                    )
                };
                return Some(Plan {
                    req: mk(root, from.path, to.path),
                    want,
                    wire,
                    note: note.into(),
                    effect: Effect::None,
                    then: Then::GetAttr {
                        root,
                        path: from.folded,
                    },
                });
            }
            0 => (
                self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?,
                self.missing_held(root, b >> 4, d)?,
            ),
            // A file onto an empty directory beside it (3.4.7: replaced or
            // refused).
            2 if sub == 1 || sub >= 5 => {
                let from = self.existing(root, a >> 8, c, |n| !n.dir && held_node(n))?;
                let up = parent(&from.folded).to_string();
                let to = self.existing_to(root, b >> 4, d, |n| {
                    n.dir
                        && held_node(n)
                        && !self.has_children(root, &n.folded)
                        && parent(&n.folded) == up
                })?;
                return Some(Plan {
                    req: mk(root, from.path, to.path),
                    want: Want::Alts(vec![
                        Expect::Reply(OwnedReply::Empty),
                        Expect::Status(Status::IoError),
                    ]),
                    wire,
                    note: "a file onto an empty directory: replaced or refused".into(),
                    effect: Effect::None,
                    then: Then::Listing { root, dir: up },
                });
            }
            // `to` an ancestor of `from` (3.4.7).
            3 if sub == 2 => {
                let from = self.existing(root, a >> 8, c, |n| {
                    held_node(n) && n.folded.contains('/') && self.held(root, parent(&n.folded))
                })?;
                let stored = from.node.as_ref()?.path.clone();
                let to = self.target(root, respell_path(parent(&stored), d));
                (from, to)
            }
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
                // Now and then the same spelling: ST_OK, and nothing
                // changes (3.4.7).
                let respelled = if sub == 0 {
                    leaf(&stored).to_string()
                } else {
                    respell(leaf(&stored), d | 1)
                };
                if sub != 0 && respelled == leaf(&stored) {
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
        // What a refused rename left, or a rename that changes nothing.
        let then = if RENAME_ENDS.contains(&note) {
            Then::End
        } else if want != Want::Ok || note == "the same name, the same spelling" {
            Then::GetAttr {
                root,
                path: from.folded.clone(),
            }
        } else {
            Then::Nothing
        };
        Some(Plan {
            req: mk(root, from.path, to.path),
            want,
            wire,
            note: note.into(),
            effect,
            then,
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
        let node = self
            .near(root, &all, NEAR_TO)
            .or_else(|| pick(&all, n).cloned())?;
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
            return whole.then_some((Want::BadRequestOrIoError, "the root at an end"));
        }
        let held_ends = self.held(root, &from.folded) && self.held(root, &to.folded);
        if from.node.is_some() && to.folded.starts_with(&format!("{}/", from.folded)) {
            return held_ends.then_some((Want::BadRequestOrIoError, "`to` inside `from`"));
        }
        // A root served as an overlay (wire.md 3.4.9, "Names only in a
        // read-only lower"), where the answer depends on which layer holds
        // `from`.
        if let Some(f) = from
            .node
            .as_ref()
            .filter(|_| self.layered.contains_key(&root))
        {
            let same = to.folded == from.folded;
            let lo = self.lower_only(root, &from.folded);
            if f.dir {
                match lo {
                    None => return None,
                    // wire.md 3.4.9's list, or the general rules (3.4.7):
                    // the spelling kept or respelled, refused or moved
                    // whole.
                    Some(true) if same => return Some((Want::Ok, RENAME_ENDS[0])),
                    Some(true) => {
                        let fresh =
                            to.node.is_none() && to.parent_dir && self.held(root, &to.folded);
                        return fresh.then_some((ok_or(false, Status::IoError), RENAME_ENDS[2]));
                    }
                    // A directory a layer holds too: refused or moved whole
                    // (3.4.7), which `overlay-rename-both` plays; a
                    // case-only RENAME of it is respelled or refused.
                    Some(false) if !same && self.lower_dir(root, &from.folded) => return None,
                    Some(false) if self.lower_dir(root, &from.folded) => {
                        let respelled = leaf(&to.path) != leaf(&f.path);
                        return respelled.then_some((ok_or(true, Status::IoError), RENAME_ENDS[3]));
                    }
                    Some(false) => {}
                }
            } else if same {
                match lo {
                    None => return None,
                    Some(true) => return Some((Want::Ok, RENAME_ENDS[1])),
                    Some(false) => {}
                }
            }
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
            if leaf(&to.path) != leaf(&f.path) {
                return Some((Want::Ok, "a new spelling of the same name"));
            }
            return (to.path == f.path).then_some((Want::Ok, "the same name, the same spelling"));
        }
        match &to.node {
            None => Some((Want::Ok, "")),
            Some(_) if from.folded.starts_with(&format!("{}/", to.folded)) => {
                Some((Want::Status(Status::IoError), "`to` an ancestor of `from`"))
            }
            Some(t) if !t.dir && f.dir => {
                Some((Want::Status(Status::IoError), "a directory onto a file"))
            }
            Some(t) if !t.dir => Some((Want::Ok, "replaces a file")),
            Some(t) if self.has_children(root, &t.folded) => Some((
                Want::Status(Status::IoError),
                "onto a directory with something under it",
            )),
            // A file onto an empty directory may replace it or be refused
            // (3.4.7), and the steps after it would depend on which.
            Some(_) if !f.dir => None,
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
                then: Then::Nothing,
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
        let sub = (a >> 24) % 4;
        if b % 5 < 2 && sub != 0 {
            // Onto a directory (`ST_IS_DIR` or `ST_IO_ERROR`), or with a
            // read-only end (`ST_READ_ONLY`); nothing changes (3.4.13).
            let from = if sub == 3 {
                self.existing(from_root, a >> 8, c, |n| {
                    !n.dir && self.read_only(from_root, &n.folded)
                })?
            } else {
                self.existing(from_root, a >> 8, c, |n| !n.dir && held_from(n))?
            };
            let (to, want, note) = match sub {
                1 => (
                    self.existing_to(to_root, b >> 4, d, |n| {
                        n.dir && self.held(to_root, &n.folded)
                    })?,
                    Want::Statuses(RENAME_X_ONTO_DIR),
                    "onto a directory",
                ),
                2 => (
                    self.missing(to_root, b >> 4, d, |n| {
                        self.read_only(to_root, &fold(&join(&n.path, "x")))
                    })?,
                    Want::Status(Status::ReadOnly),
                    "`to` is read-only",
                ),
                _ => (
                    self.missing_held(to_root, b >> 4, d)?,
                    Want::Status(Status::ReadOnly),
                    "`from` is read-only",
                ),
            };
            if !to.parent_dir {
                return None;
            }
            return Some(Plan {
                req: OwnedRequest::RenameX {
                    from_root: RootId(from_root),
                    to_root: RootId(to_root),
                    from: from.path,
                    to: to.path,
                },
                want,
                wire: "3.4.13",
                note: note.into(),
                effect: Effect::None,
                then: Then::GetAttr {
                    root: from_root,
                    path: from.folded,
                },
            });
        }
        let (from, to) = match b % 5 {
            0 => (
                self.existing(from_root, a >> 8, c, |n| !n.dir && held_from(n))?,
                self.missing_held(to_root, b >> 4, d)?,
            ),
            // Onto an existing file, in the caller's spelling, which the
            // file at `to` takes (3.4.13).
            1 => (
                self.existing(from_root, a >> 8, c, |n| !n.dir && held_from(n))?,
                self.existing_to(to_root, b >> 4, d, |n| {
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
            then: Then::Nothing,
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
                then: Then::Nothing,
            });
        }
        let root = self.root(a);
        let held = |n: &Node| self.held(root, &n.folded);
        if b % 7 == 3 && !(d >> 8).is_multiple_of(4) {
            let f = self.existing(root, b >> 3, c, |n| !n.dir && held(n))?;
            if (d >> 8) % 4 == 1 {
                let leaf = NAMES[(c >> 9) as usize % NAMES.len()];
                return Some(Plan {
                    req: OwnedRequest::Delete {
                        root: RootId(root),
                        path: join(&f.path, leaf),
                    },
                    want: Want::Statuses(BELOW_A_FILE),
                    wire: "3.4.8, 3.5",
                    note: "below a file: a missing path".into(),
                    effect: Effect::None,
                    then: Then::Nothing,
                });
            }
            let mut t = self.target(root, format!("{}:s", f.path));
            t.folded = f.folded;
            let req = OwnedRequest::Delete {
                root: RootId(root),
                path: t.path.clone(),
            };
            return Some(self.stream_plan(root, req, &t));
        }
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
            (Want::BadRequestOrIoError, "the root itself")
        } else {
            match &t.node {
                Some(_) if self.read_only(root, &t.folded) => (
                    Want::Status(Status::ReadOnly),
                    "no writable provider holds it",
                ),
                Some(n) if self.held(root, &t.folded) => {
                    let children = n.dir && self.has_children(root, &t.folded);
                    match (self.lower_only(root, &t.folded), children) {
                        // wire.md 3.4.9: hidden by a whiteout, or refused by the
                        // general rule (3.4.8).
                        (Some(true), true) => (
                            ok_or(true, Status::IoError),
                            "a lower-only directory with something under it: hidden, or refused",
                        ),
                        (Some(true), false) => (Want::Ok, "a lower-only name: hidden"),
                        (None, true) => return None,
                        (_, true) => (
                            Want::Status(Status::IoError),
                            "a directory with something under it",
                        ),
                        (_, false) => (Want::Ok, ""),
                    }
                }
                None if self.held(root, &t.folded) && t.parent_dir => {
                    (Want::Status(Status::NotFound), "missing")
                }
                _ => return None,
            }
        };
        // Hidden or refused: the trees differ, so the case ends there.
        let either = matches!(want, Want::Alts(_));
        let effect = if want == Want::Ok || either {
            Effect::Deleted {
                root,
                path: t.folded.clone(),
            }
        } else {
            Effect::None
        };
        let then = if either { Then::End } else { Then::Nothing };
        Some(Plan {
            req: OwnedRequest::Delete {
                root: RootId(root),
                path: t.path,
            },
            want,
            wire: "3.4.8",
            note: note.into(),
            effect,
            then,
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
                then: Then::Nothing,
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
            _ if !(d >> 8).is_multiple_of(3) => {
                let below_file = (d >> 8) % 3 == 2;
                let t = self.orphan_name(root, b >> 3, c, below_file)?;
                // No parent to make it in (3.4.9, 3.5).
                let (want, note) = if below_file {
                    (Want::Statuses(BELOW_A_FILE), "a name below a file")
                } else {
                    (
                        Want::Status(Status::NotFound),
                        "a name in a missing directory",
                    )
                };
                return Some(Plan {
                    req: OwnedRequest::Mkdir {
                        root: RootId(root),
                        mode,
                        path: t.path,
                    },
                    want,
                    wire: "3.4.9, 3.5",
                    note: note.into(),
                    effect: Effect::None,
                    then: Then::Nothing,
                });
            }
            _ if !self.has_writable(root) => self.missing(root, b >> 3, c, |_| true),
            _ => self.missing_held(root, b >> 3, c),
        }?;
        let made = Effect::Made {
            root,
            path: t.folded.clone(),
        };
        let (want, note, effect) = match &t.node {
            Some(n) if n.dir && self.held(root, &t.folded) => {
                if self.lower_only(root, &t.folded) == Some(false) {
                    (Want::OkOrExists, "an existing directory", Effect::None)
                } else {
                    // wire.md 3.4.9: a directory only a lower holds, `ST_OK`
                    // or `ST_EXISTS`.
                    (
                        Want::Alts(vec![
                            Expect::Reply(OwnedReply::Empty),
                            Expect::Status(Status::Exists),
                        ]),
                        "a directory only a lower holds",
                        Effect::Unsure {
                            root,
                            path: t.folded.clone(),
                        },
                    )
                }
            }
            Some(n) if !n.dir && self.held(root, &t.folded) => {
                match self.lower_only(root, &t.folded) {
                    // Or refused, by the general rule (3.4.9).
                    Some(true) => (
                        ok_or(true, Status::IoError),
                        "over a lower-only file: the directory hides it, or refused",
                        Effect::MadeOver {
                            root,
                            path: t.folded.clone(),
                        },
                    ),
                    None => return None,
                    Some(false) => (
                        Want::Status(Status::IoError),
                        "an existing file a writable provider holds",
                        Effect::None,
                    ),
                }
            }
            Some(n) if !n.dir && self.read_only(root, &t.folded) => (
                Want::Status(Status::Exists),
                "an existing file no writable provider holds",
                Effect::None,
            ),
            None if t.parent_dir && !self.has_writable(root) => (
                Want::Status(Status::ReadOnly),
                "a root with no writable provider",
                Effect::None,
            ),
            None if t.parent_dir && self.held(root, &t.folded) => (Want::Ok, "", made),
            _ => return None,
        };
        let wire = if want == Want::Status(Status::ReadOnly) {
            "3.4.9, 3.5"
        } else {
            "3.4.9"
        };
        Some(Plan {
            req: OwnedRequest::Mkdir {
                root: RootId(root),
                mode,
                path: t.path,
            },
            then: if note == "over a lower-only file: the directory hides it, or refused" {
                Then::End
            } else {
                Then::Nothing
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
                then: Then::Nothing,
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
        let components = t.path.split('/').count() as u32;
        if want == Want::Ok && (d >> 8).is_multiple_of(8) {
            // Every component known already: an empty reply or
            // ST_BAD_REQUEST (3.4.12).
            return Some(Plan {
                req: OwnedRequest::StoredNames {
                    root: RootId(root),
                    skip: components + (d >> 11) % 2,
                    path: t.path,
                },
                want: Want::Alts(vec![
                    Expect::Reply(OwnedReply::StoredNames(String::new())),
                    Expect::Status(Status::BadRequest),
                ]),
                wire: "3.4.12",
                note: "a `skip` of every component".into(),
                effect: Effect::None,
                then: Then::Nothing,
            });
        }
        let skip = d % components;
        let note = if want == Want::Ok {
            String::new()
        } else {
            "a name the director does not have".into()
        };
        Some(Plan {
            req: OwnedRequest::StoredNames {
                root: RootId(root),
                skip,
                path: t.path,
            },
            want,
            wire: "3.4.12",
            note,
            effect: Effect::None,
            then: Then::Nothing,
        })
    }

    fn plan_registry(&self, kind: Kind, a: u32, b: u32, c: u32, d: u32) -> Option<Plan> {
        let path = || -> Option<String> {
            // Now and then a key a REG_DELETE_KEY tombstoned, or below one.
            let base = match pick(&self.tombstoned, a >> 4) {
                Some(t) if (b >> 8).is_multiple_of(4) => t,
                _ => pick(&self.keys, a)?,
            };
            let p = if b.is_multiple_of(3) {
                format!("{base}\\{}", KEY_NAMES[(b >> 2) as usize % KEY_NAMES.len()])
            } else {
                base.clone()
            };
            // Keys compare by fold (4.2). The hive (`\Registry\Machine`, or
            // `\Registry\User` and its `<CurrentUser>` component) keeps its
            // canonical spelling here, as the shim sends it
            // (`registry-case` covers other spellings of `Machine`); the
            // components below are respelled.
            let parts: Vec<&str> = p.split('\\').collect();
            let hive = if parts.get(2) == Some(&"User") { 4 } else { 3 };
            let respelled: Vec<String> = parts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i < hive {
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
                        then: Then::Nothing,
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
                // A version seen, the one after the latest, or 0.
                since: match (d >> 1) % 6 {
                    4 => self.versions.iter().max()? + 1,
                    5 => 0,
                    _ => *pick(&self.versions, d >> 4)?,
                },
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
                    then: Then::Nothing,
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
            then: Then::Nothing,
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

    /// One op: planned against the state it meets and sent by the client
    /// its `e` picks. Without a registry layer a registry kind stands for a
    /// file kind (the case's one probe of the absent layer is
    /// [`Gen::finish`]'s).
    fn op(&mut self, op: &RawOp) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        let mut op = *op;
        if !self.registry && op.kind as u32 >= Kind::RegLookup as u32 && op.kind != Kind::Template {
            op.kind = FILE_KINDS[(op.d >> 24) as usize % FILE_KINDS.len()];
        }
        self.focus = op.e;
        let client = match self.others {
            0 => 0,
            n => (op.e >> 16) % (u32::from(n) + 1),
        };
        match self.plan(&op) {
            Some(plan) => self.step(plan, client),
            None => Ok(()),
        }
    }

    /// The case's last step: a REG_DELETE_KEY of a key no layer holds,
    /// which tombstones it (wire.md 4.3); or, without a registry layer, one
    /// registry request, `ST_NOT_SUPPORTED` (4).
    fn finish(&mut self) -> Result<(), String> {
        self.focus = 0;
        let path = format!("{REG_BASE}\\Absent");
        let (req, want, wire, note) = if self.registry {
            (
                OwnedRequest::RegDeleteKey { path },
                Want::Overlay,
                "4.3",
                "a key no layer holds: tombstoned",
            )
        } else {
            let req = match self.steps.len() % 4 {
                0 => OwnedRequest::RegLookup { path },
                1 => OwnedRequest::RegKey { path },
                2 => OwnedRequest::RegCreateKey {
                    path,
                    volatile: false,
                },
                _ => OwnedRequest::RegSetValue {
                    path,
                    name: "v".into(),
                    ty: 1,
                    data: Vec::new(),
                },
            };
            (
                req,
                Want::Status(Status::NotSupported),
                "4",
                "no registry layer",
            )
        };
        self.step(
            Plan {
                req,
                want,
                wire,
                note: note.into(),
                effect: Effect::None,
                then: Then::Nothing,
            },
            0,
        )
    }

    /// Asks the oracle, checks its answer against `plan.want`, records the
    /// step (sent by `client`), moves the state on, and takes the step
    /// `plan.then` asks for.
    fn step(&mut self, plan: Plan, client: u32) -> Result<(), String> {
        // What the handles a DELETE or RENAME may leave behind held.
        let snaps = match plan.effect {
            Effect::Moved { .. } | Effect::Deleted { .. } | Effect::MadeOver { .. } => {
                self.snapshots()
            }
            _ => BTreeMap::new(),
        };
        let listing = |root: u32, dir: &str| OwnedRequest::ReadDir {
            root: RootId(root),
            path: dir.to_string(),
        };
        let before = match &plan.then {
            Then::Listing { root, dir } => Some(self.oracle.ask(&listing(*root, dir))),
            _ => None,
        };
        let got = self.oracle.ask(&plan.req);
        self.check_prediction(&plan.req, &got)?;
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
        let statuses =
            |ss: &[Status]| Expect::OneOf(ss.iter().map(|s| Expect::Status(*s)).collect());
        let expect = match (&plan.want, &got) {
            (Want::Ok, Ok(r)) | (Want::Overlay, Ok(r)) => reply(r),
            (Want::Status(s), Err(g)) if s == g => Expect::Status(*s),
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
                statuses(&[Status::NotFound, Status::BadRequest])
            }
            (Want::IsDirOrReadOnly, Err(Status::IsDir | Status::ReadOnly)) => {
                statuses(&[Status::IsDir, Status::ReadOnly])
            }
            (Want::BadRequestOrIoError, Err(Status::BadRequest | Status::IoError)) => {
                statuses(&[Status::BadRequest, Status::IoError])
            }
            (Want::Statuses(ss), Err(g)) if ss.contains(g) => statuses(ss),
            (Want::Alts(alts), got)
                if alts.iter().any(|a| {
                    crate::compare::Session::new()
                        .check(0, &plan.req, a, got)
                        .is_ok()
                }) =>
            {
                Expect::OneOf(alts.clone())
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
            client,
            literal_fh: false,
            choice: None,
        });
        if let Ok(r) = &got {
            self.record_version(r);
            self.apply(&plan, r, &snaps);
        }
        if let Effect::Spent(fh) = plan.effect {
            self.spend(fh);
        }
        if let Effect::Unsure { root, path } = &plan.effect {
            if let Some(l) = self.layered.get_mut(root) {
                if !l.upper.contains(path) {
                    l.unsure.insert(path.clone());
                }
            }
        }
        self.check_layers()?;
        self.touch(&plan.req, &got);
        if got.is_ok() {
            match &plan.req {
                OwnedRequest::RegCreateKey { path, .. } => {
                    if !self.keys.contains(path) {
                        self.keys.push(path.clone());
                    }
                }
                OwnedRequest::RegRenameKey { path, new_leaf } => {
                    let renamed = match path.rsplit_once('\\') {
                        Some((up, _)) => format!("{up}\\{new_leaf}"),
                        None => new_leaf.clone(),
                    };
                    if !self.keys.contains(&renamed) {
                        self.keys.push(renamed);
                    }
                }
                OwnedRequest::RegDeleteKey { path } => self.tombstoned.push(path.clone()),
                _ => {}
            }
        }
        match plan.then {
            Then::Nothing => Ok(()),
            Then::End => {
                self.done = true;
                Ok(())
            }
            Then::GetAttr { root, path } => self.step(
                Plan {
                    req: OwnedRequest::GetAttr {
                        root: RootId(root),
                        path,
                    },
                    want: Want::Ok,
                    wire: "3.4.1",
                    note: "what the step before left".into(),
                    effect: Effect::None,
                    then: Then::Nothing,
                },
                client,
            ),
            Then::Listing { root, dir } => {
                let again = self.oracle.ask(&plan.req);
                if again != Err(Status::NotFound) {
                    return Err(format!(
                        "generator: {:?} sent again after it succeeded is not ST_NOT_FOUND: {}",
                        plan.req,
                        crate::compare::render_got(&again)
                    ));
                }
                self.steps.push(Step {
                    request: plan.req.clone(),
                    expect: Expect::OneOf(vec![
                        Expect::Status(Status::NotFound),
                        Expect::Status(Status::IoError),
                    ]),
                    cite: Some(plan.wire.to_string()),
                    note: Some(format!(
                        "wire.md {}: again: `from` is gone if the first moved it, and refused                          again if not",
                        plan.wire
                    )),
                    client,
                    literal_fh: false,
                    choice: None,
                });
                let after = self.oracle.ask(&listing(root, &dir));
                let (Some(Ok(b @ OwnedReply::ReadDir(_))), Ok(a @ OwnedReply::ReadDir(_))) =
                    (&before, &after)
                else {
                    return Err(format!(
                        "generator: READDIR of {dir} in root {root} around step {} is not a \
                         listing: {before:?}, {after:?}",
                        self.steps.len()
                    ));
                };
                let expect = if a == b {
                    Expect::Reply(a.clone())
                } else {
                    Expect::OneOf(vec![Expect::Reply(a.clone()), Expect::Reply(b.clone())])
                };
                self.steps.push(Step {
                    request: listing(root, &dir),
                    cite: matches!(expect, Expect::OneOf(_)).then(|| plan.wire.to_string()),
                    expect,
                    note: Some(format!(
                        "wire.md {}: the listing as either answer to the step before leaves \
                         it; the case ends, since which it is decides the tree",
                        plan.wire
                    )),
                    client,
                    literal_fh: false,
                    choice: None,
                });
                self.done = true;
                Ok(())
            }
        }
    }

    /// Notes the paths and handle `req` named, for the locality bias.
    fn touch(&mut self, req: &OwnedRequest, got: &Result<OwnedReply, Status>) {
        use OwnedRequest as O;
        let mut paths: Vec<(u32, String)> = Vec::new();
        let mut fh = None;
        match req {
            O::GetAttr { root, path }
            | O::ReadDir { root, path }
            | O::Open { root, path, .. }
            | O::Delete { root, path }
            | O::Mkdir { root, path, .. }
            | O::StoredNames { root, path, .. } => paths.push((root.0, fold(path))),
            O::Rename { root, from, to } => {
                paths.push((root.0, fold(from)));
                paths.push((root.0, fold(to)));
            }
            O::RenameX {
                from_root,
                to_root,
                from,
                to,
            } => {
                paths.push((from_root.0, fold(from)));
                paths.push((to_root.0, fold(to)));
            }
            O::Read { fh: f, .. } | O::Write { fh: f, .. } | O::SetAttr { fh: f, .. } => {
                fh = Some(*f);
                if let Some(h) = self.handles.get(f) {
                    if h.gone.is_none() {
                        paths.push((h.root, h.folded.clone()));
                    }
                }
            }
            _ => {}
        }
        if let (O::Open { .. }, Ok(OwnedReply::Open(o))) = (req, got) {
            fh = Some(o.fh);
        }
        for (root, p) in paths {
            // Not a named stream's: no request makes one (3.2).
            if p != "." && !p.contains(':') {
                for q in [parent(&p).to_string(), p] {
                    self.recent.retain(|x| x != &(root, q.clone()));
                    self.recent.push((root, q));
                }
            }
        }
        let excess = self.recent.len().saturating_sub(RECENT);
        self.recent.drain(..excess);
        if let Some(f) = fh.filter(|f| self.handles.contains_key(f)) {
            self.recent_fh.retain(|x| *x != f);
            self.recent_fh.push(f);
            let excess = self.recent_fh.len().saturating_sub(RECENT);
            self.recent_fh.drain(..excess);
        }
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

    /// The effect of a step that succeeded; `snaps` holds the bytes of every
    /// open handle's file from before it.
    fn apply(&mut self, plan: &Plan, r: &OwnedReply, snaps: &BTreeMap<u64, Vec<u8>>) {
        match &plan.effect {
            Effect::None => {}
            Effect::Opened {
                root,
                path,
                dir,
                write,
                append,
                emptied,
                created,
                changed,
            } => {
                let OwnedReply::Open(o) = r else { return };
                if *write && !*dir {
                    // A create is the upper's, and may put the layers'
                    // directories above it there; a write open of a
                    // lower-only file may copy it up, and does once it
                    // changes the bytes (the lower is read-only).
                    let lo = self.lower_only(*root, path);
                    self.unsure_parents(*root, path);
                    if let Some(l) = self.layered.get_mut(root) {
                        if *created || *changed {
                            l.up(path);
                        } else if lo != Some(false) {
                            l.unsure.insert(path.clone());
                        }
                    }
                }
                let file = if *dir {
                    None
                } else {
                    let id = self.id_at(*root, path);
                    if *emptied {
                        self.changed_file(id);
                    }
                    Some(id)
                };
                self.handles.insert(
                    o.fh,
                    Handle {
                        root: *root,
                        folded: path.clone(),
                        dir: *dir,
                        write: *write,
                        append: *append,
                        file,
                        gone: None,
                        clean: true,
                    },
                );
                self.refresh();
            }
            Effect::Wrote(fh, changed) => {
                if let Some(h) = self.handles.get(fh).filter(|h| h.gone.is_none()) {
                    let (root, path) = (h.root, h.folded.clone());
                    if let (true, Some(l)) = (*changed, self.layered.get_mut(&root)) {
                        l.up(&path);
                    }
                }
                if let Some(id) = self.handles.get(fh).and_then(|h| h.file) {
                    self.changed_file(id);
                }
                self.refresh();
            }
            // Spent whatever the oracle answered (`Gen::spend`).
            Effect::Spent(_) => {}
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
                self.taint_dirs(*from_root, from);
                self.taint_dirs(*to_root, to);
                if (from_root, from) != (to_root, to) {
                    self.moved_layers(*from_root, from, *to_root, to);
                    // What was at `to` is replaced: deleted, to its handles.
                    self.end_files(*to_root, to, snaps);
                    let moved: Vec<(String, u64)> = self
                        .ids
                        .iter()
                        .filter(|((r, p), _)| r == from_root && under(p, from))
                        .map(|((_, p), id)| (p.clone(), *id))
                        .collect();
                    for (p, id) in moved {
                        self.ids.remove(&(*from_root, p.clone()));
                        let tail = &p[from.len()..];
                        self.ids.insert((*to_root, format!("{to}{tail}")), id);
                        if from_root != to_root {
                            // RENAME_X: a handle may follow the file or
                            // answer as for a deleted one (3.4.7).
                            for h in self.handles.values_mut() {
                                if h.file == Some(id) {
                                    h.file = None;
                                    h.gone = Some(Gone {
                                        snap: snaps.get(&id).cloned().unwrap_or_default(),
                                        of: id,
                                        writes: false,
                                        follows: Some(id),
                                        spent: false,
                                    });
                                }
                            }
                        }
                    }
                    self.follow();
                }
                self.refresh();
            }
            Effect::Deleted { root, path } => {
                self.taint_dirs(*root, path);
                self.end_files(*root, path, snaps);
                if let Some(l) = self.layered.get_mut(root) {
                    l.gone(path);
                }
                self.refresh();
            }
            Effect::Made { root, path } => {
                self.unsure_parents(*root, path);
                if let Some(l) = self.layered.get_mut(root) {
                    l.up(path);
                }
                self.refresh();
            }
            Effect::MadeOver { root, path } => {
                self.end_files(*root, path, snaps);
                self.unsure_parents(*root, path);
                if let Some(l) = self.layered.get_mut(root) {
                    l.masked.insert(path.clone());
                    l.up(path);
                }
                self.refresh();
            }
            Effect::Unsure { .. } => {}
            Effect::Changed => self.refresh(),
        }
    }
}

/// The ops template `op` stands for ([`TEMPLATES`]) in a tree whose roots
/// are `roots`, its numbers its own: a root with a writable provider, and
/// each op's branch and locality the template's.
fn expand(op: &RawOp, roots: &[RootInfo]) -> Vec<RawOp> {
    let t = TEMPLATES[op.a as usize % TEMPLATES.len()];
    let ids: Vec<u32> = roots.iter().map(|r| r.id).collect();
    let writable: Vec<u32> = roots
        .iter()
        .filter(|r| r.whole || !r.entries.is_empty())
        .map(|r| r.id)
        .collect();
    // `a` such that `root(a)` and `writable_root(a)` are one root.
    let a = (0..64u32)
        .map(|i| (op.a >> 4).wrapping_add(i))
        .find(|&a| {
            !writable.is_empty()
                && ids[a as usize % ids.len()] == writable[a as usize % writable.len()]
        })
        .unwrap_or(op.a);
    t.iter()
        .enumerate()
        .map(|(i, &(kind, b, d, e))| RawOp {
            kind,
            a,
            b: b + 5040 * ((op.b >> 12).wrapping_add(i as u32) % 800_000),
            c: op.c.rotate_left(5 * i as u32),
            d: d | 0x40 | (op.d & 0xFFFF_0000),
            e: e | (op.e & 0xFFFF_0000),
        })
        .collect()
}

/// `input` with every [`Kind::Template`] op replaced by the ops it stands
/// for, which build the same case: what shrinking takes apart op by op.
pub fn flatten(input: &CaseInput) -> CaseInput {
    let (_, roots) = build_tree(&input.tree);
    let ops = input
        .ops
        .iter()
        .flat_map(|op| match op.kind {
            Kind::Template => expand(op, &roots),
            _ => vec![*op],
        })
        .collect();
    CaseInput {
        tree: input.tree.clone(),
        ops,
        others: input.others,
    }
}

/// The file kinds a registry kind stands for in a tree with no registry
/// layer.
const FILE_KINDS: [Kind; 12] = [
    Kind::GetAttr,
    Kind::ReadDir,
    Kind::OpenRead,
    Kind::OpenWrite,
    Kind::Read,
    Kind::Write,
    Kind::SetAttr,
    Kind::Close,
    Kind::Rename,
    Kind::Delete,
    Kind::Mkdir,
    Kind::Template,
];

/// Op sequences on the same names and handles, each op a (kind, `b`, `d`,
/// `e`): `b` picks its branch, `d` its open flags, and `e` which recent
/// name or handle it takes ([`NEAR_EXISTING`] and the rest; bits 8 and up,
/// which of them, the latest first). The template's other numbers are the
/// op's own ([`Gen::template`]).
const TEMPLATES: [&[(Kind, u32, u32, u32)]; 7] = [
    // A save: a new file written, renamed over a file with its handle
    // open, written again through that handle, closed and read back.
    &[
        (Kind::OpenWrite, 2, 2, 0),
        (Kind::Write, 0, 0, NEAR_HANDLE),
        (Kind::Rename, 1, 0, NEAR_EXISTING),
        (Kind::Write, 4, 0, NEAR_HANDLE),
        (Kind::Close, 0, 0, NEAR_HANDLE),
        (Kind::OpenRead, 2, 0, NEAR_EXISTING),
        (Kind::Read, 0, 0, NEAR_HANDLE),
    ],
    // A file deleted under a handle and created again: the old handle
    // never reaches the new file.
    &[
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::Delete, 0, 0, NEAR_EXISTING),
        (Kind::OpenWrite, 2, 2, NEAR_MISSING),
        (Kind::Read, 0, 0, NEAR_HANDLE | 1 << 8),
        (Kind::Write, 0, 0, NEAR_HANDLE),
        (Kind::Read, 0, 0, NEAR_HANDLE | 1 << 8),
        (Kind::Close, 0, 0, NEAR_HANDLE),
    ],
    // Two files swapped through a third name, a handle open on each.
    &[
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::Rename, 0, 0, NEAR_EXISTING | 1 << 8),
        (Kind::Rename, 0, 0, NEAR_EXISTING | NEAR_MISSING | 1 << 8),
        (Kind::Rename, 0, 0, NEAR_EXISTING | NEAR_MISSING | 1 << 8),
        (Kind::Read, 0, 0, NEAR_HANDLE | 1 << 8),
        (Kind::Read, 0, 0, NEAR_HANDLE | 1 << 8),
    ],
    // A truncating open under a writer, which then writes past the new end.
    &[
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::OpenWrite, 0, 8, NEAR_EXISTING),
        (Kind::Write, 4, 0, NEAR_HANDLE | 1 << 8),
        (Kind::Read, 0, 0, NEAR_HANDLE | 1 << 8),
        (Kind::SetAttr, 3, 0, NEAR_HANDLE | 1 << 8),
        (Kind::Read, 6, 0, NEAR_HANDLE | 1 << 8),
    ],
    // A directory renamed with a file open below it.
    &[
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::Rename, 5, 0, NEAR_EXISTING),
        (Kind::Write, 1, 0, NEAR_HANDLE),
        (Kind::ReadDir, 1, 0, NEAR_EXISTING),
        (Kind::Read, 0, 0, NEAR_HANDLE),
        (Kind::Close, 0, 0, NEAR_HANDLE),
    ],
    // A file cut by SETATTR and grown again by a WRITE past its end: the
    // gap reads as zeros, the cut bytes included.
    &[
        (Kind::OpenWrite, 0, 1, 0),
        (Kind::SetAttr, 2, 0, NEAR_HANDLE),
        (Kind::Write, 4, 0, NEAR_HANDLE),
        (Kind::Read, 6, 0, NEAR_HANDLE),
        (Kind::Read, 0, 0, NEAR_HANDLE),
    ],
    // A directory renamed away and made again at its old name: empty.
    &[
        (Kind::Rename, 5, 0, 0),
        (Kind::Mkdir, 0, 0, NEAR_MISSING),
        (Kind::ReadDir, 1, 0, NEAR_EXISTING),
        (Kind::ReadDir, 1, 0, NEAR_EXISTING | 1 << 8),
    ],
];

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
                lower: vec![],
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
                    two: false,
                    entries: vec![file(1, 0b1010), file(5, 0b0101)],
                    lower: vec![],
                },
                RootSpec {
                    writable: None,
                    two: false,
                    entries: vec![file(1, 0b0110), file(11, 0b1001)],
                    lower: vec![],
                },
            ],
            registry: None,
        }
    }

    /// RENAME_X onto an existing file sends the caller's spelling, which
    /// the file at `to` takes (wire.md 3.4.13), and the reference stores it.
    #[test]
    fn a_rename_x_onto_a_file_takes_the_callers_spelling() {
        let (tree, roots) = build_tree(&two_roots());
        let mut respelled = 0;
        for a in 0..64u32 {
            for b in (1..200u32).step_by(5) {
                for d in [0, 1, 0xFF, 0x1234_5678] {
                    let mut g = Gen::new(&tree, roots.clone(), Limits::default()).unwrap();
                    let Some(p) = g.plan_rename_x(a, b, 0x5555, d) else {
                        continue;
                    };
                    if p.note != "replacing a file there" {
                        continue;
                    }
                    let OwnedRequest::RenameX { to_root, to, .. } = p.req.clone() else {
                        panic!("{:?}", p.req)
                    };
                    let stored = g.node(to_root.0, &fold(&to)).unwrap().path.clone();
                    g.step(p, 0).unwrap();
                    assert_eq!(g.node(to_root.0, &fold(&to)).unwrap().path, to);
                    if to != stored {
                        respelled += 1;
                    }
                }
            }
        }
        assert!(
            respelled > 0,
            "no RENAME_X onto a file in another spelling was planned"
        );
    }

    /// A director with a stager answers ST_NOT_FOUND for a file it does not
    /// stage (wire.md 3.4.14).
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

    /// A WRITE or SETATTR on a directory handle is ST_IS_DIR where the
    /// writable provider holds the directory, and ST_IS_DIR or ST_READ_ONLY
    /// elsewhere (wire.md 3.4.5).
    #[test]
    fn a_directory_handle_write_outside_the_writable_provider_may_be_read_only() {
        let (mut strict, mut either) = (0, 0);
        for s in crate::generate::cases(11, 256, 48).unwrap() {
            for st in &s.steps {
                let dir = st
                    .note
                    .as_deref()
                    .is_some_and(|n| n.ends_with("a directory handle"));
                if !dir
                    || !matches!(
                        st.request,
                        OwnedRequest::Write { .. } | OwnedRequest::SetAttr { .. }
                    )
                {
                    continue;
                }
                match &st.expect {
                    Expect::Status(Status::IsDir) => strict += 1,
                    Expect::OneOf(alts) => {
                        assert_eq!(
                            alts,
                            &vec![
                                Expect::Status(Status::IsDir),
                                Expect::Status(Status::ReadOnly)
                            ]
                        );
                        assert_eq!(st.cite.as_deref(), Some("3.4.5"));
                        either += 1;
                    }
                    e => panic!("{e:?}"),
                }
            }
        }
        assert!(strict > 0 && either > 0, "strict {strict}, either {either}");
    }

    /// DELETE or RENAME of the root itself, and a RENAME `to` inside `from`,
    /// is ST_BAD_REQUEST or ST_IO_ERROR (wire.md 3.4.7, 3.4.8).
    #[test]
    fn the_root_and_a_rename_into_itself_may_be_io_errors() {
        let mut seen = 0;
        for s in crate::generate::cases(12, 256, 48).unwrap() {
            for st in &s.steps {
                let root = match &st.request {
                    OwnedRequest::Delete { path, .. } => path == ".",
                    OwnedRequest::Rename { from, to, .. } => {
                        from == "."
                            || to == "."
                            || fold(to).starts_with(&format!("{}/", fold(from)))
                    }
                    _ => false,
                };
                if !root {
                    continue;
                }
                assert!(
                    !matches!(st.expect, Expect::Status(Status::BadRequest)),
                    "{:?}",
                    st.request
                );
                if let Expect::OneOf(alts) = &st.expect {
                    if alts.contains(&Expect::Status(Status::BadRequest)) {
                        assert!(alts.contains(&Expect::Status(Status::IoError)), "{alts:?}");
                        seen += 1;
                    }
                }
            }
        }
        assert!(seen > 0);
    }

    /// A directory renamed onto a file is ST_IO_ERROR (wire.md 3.4.7).
    #[test]
    fn a_directory_onto_a_file_is_an_io_error() {
        let mut seen = 0;
        for s in crate::generate::cases(13, 256, 48).unwrap() {
            for st in &s.steps {
                let note = st.note.as_deref().unwrap_or("");
                if note.ends_with("a directory onto a file") {
                    assert_eq!(
                        st.expect,
                        Expect::Status(Status::IoError),
                        "{:?}",
                        st.request
                    );
                    seen += 1;
                }
            }
        }
        assert!(seen > 0);
    }

    /// Every interaction the generator sends is generated: each by its
    /// step's note, and steps from other clients.
    #[test]
    fn every_interaction_is_generated() {
        let notes = [
            "a handle whose file is gone: its last bytes, or a failure",
            "a handle whose file is gone: its file's other handles see it, no name does; or it fails",
            "past the end: the gap reads as zeros",
            "an extension: it reads as zeros",
            "past the largest file a director can hold; the file is unchanged",
            "a write open of a directory",
            "OPEN_EXCL on an existing directory",
            "a directory no writable provider holds",
            "a name in a missing directory",
            "a name below a file",
            "below a file: found = 0",
            "below a file: a missing path",
            "a named stream: found = 0",
            "a named stream the director does not hold; the base file is unchanged",
            "3.4.2: a file",
            "3.4.2: below a file",
            "3.4.2: a missing directory",
            "a file onto an empty directory: replaced or refused",
            "`to` an ancestor of `from`",
            "the same name, the same spelling",
            "`to` in a missing directory",
            "`to` below a file",
            "onto a directory",
            "`to` is read-only",
            "a `skip` of every component",
            "the case ends",
            RENAME_ENDS[0],
            RENAME_ENDS[1],
            RENAME_ENDS[2],
            RENAME_ENDS[3],
            "a lower-only directory with something under it: hidden, or refused",
            "a lower-only name: hidden",
            "over a lower-only file: the directory hides it, or refused",
            "a directory only a lower holds",
        ];
        let mut seen = vec![0; notes.len()];
        let mut others = 0;
        for s in crate::generate::cases(21, 512, 32).unwrap() {
            for st in &s.steps {
                let note = st.note.as_deref().unwrap_or("");
                for (i, n) in notes.iter().enumerate() {
                    if note.ends_with(n)
                        || (n.starts_with("3.4.2") && note.ends_with(&n[..]))
                        || note.contains(n)
                    {
                        seen[i] += 1;
                    }
                }
                if st.client != 0 {
                    others += 1;
                }
            }
        }
        let missing: Vec<_> = notes
            .iter()
            .zip(&seen)
            .filter(|(_, n)| **n == 0)
            .map(|(t, _)| *t)
            .collect();
        eprintln!("{:?}", notes.iter().zip(&seen).collect::<Vec<_>>());
        assert!(missing.is_empty(), "never generated: {missing:?}");
        assert!(others > 0);
    }

    /// A good share of generated trees serve a root as an overlay, with
    /// names in both layers, and every case of many seeds is one the
    /// generator's reckoning of the layers agrees with the reference on
    /// (`build` fails otherwise).
    #[test]
    fn layered_roots_are_generated_and_agree_with_the_reference() {
        let (mut layered, mut both, mut total) = (0, 0, 0);
        for seed in [1, 2, 3, 40] {
            for s in crate::generate::cases(seed, 300, 32).unwrap() {
                total += 1;
                let Some(r) = s.tree.roots.iter().find(|r| !r.lower.is_empty()) else {
                    continue;
                };
                layered += 1;
                let upper: BTreeSet<String> = r.files.iter().map(|f| fold(&f.path)).collect();
                if r.lower
                    .iter()
                    .flat_map(|l| &l.files)
                    .any(|f| upper.contains(&fold(&f.path)))
                {
                    both += 1;
                }
            }
        }
        assert!(layered * 4 >= total, "{layered} of {total} cases layered");
        assert!(
            both * 10 >= layered,
            "{both} of {layered} with a name in both layers"
        );
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
