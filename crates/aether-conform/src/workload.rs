//! Workloads: what a director is benchmarked on, as data
//! (`contract/workloads/*.json`, the format in `contract/workloads/README.md`).
//!
//! A workload is a tree to mount ([`Tree`], the scenarios' tree, plus
//! `generate` entries that stand for many files of one shape) and a list of
//! phases. A phase is run by `threads` client threads at once, each sending
//! the requests [`Workload::expand`] gives it, `repeat` times over:
//!
//! ```json
//! {
//!   "name": "random-small",
//!   "description": "4 KiB reads at random offsets of a 64 MiB archive",
//!   "tree": {"roots": [{"id": 0, "files": [{"path": "Data/Big.ba2",
//!                                           "contents": {"size": 67108864, "seed": 3}}]}]},
//!   "generate": [{"dir": "Data/Meshes", "count": 100, "name": "Rock{n}.nif",
//!                 "pad": 3, "size": 2048, "seed": 10}],
//!   "phases": [{"name": "reads", "threads": 4, "repeat": 1,
//!               "kind": "random_read", "path": "Data/Big.ba2",
//!               "len": 4096, "count": 4096, "seed": 7}]
//! }
//! ```
//!
//! Every path is on root 0 and canonical (`docs/wire.md` 3.2). A phase's
//! `kind` and its fields:
//!
//! - `storm`: `ops` (`getattr`, `open`, `close`, `stored_names`), sent in
//!   that order for each file of `paths` (`"all"`, `{"prefix": "Data/"}`
//!   compared by the fold, or `{"list": [..]}`); a `close` closes the handle
//!   the last `open` before it got.
//! - `sequential_read`: OPEN, the whole file in READs of `chunk` bytes
//!   (`bulk` asks for bulk replies), CLOSE.
//! - `random_read`: OPEN, `count` READs of `len` bytes at offsets that are
//!   multiples of `len`, drawn from `seed` and the thread, CLOSE.
//! - `readdir`: one READDIR of `path`.
//! - `write`: OPEN `path` for writing (created, truncated), WRITEs of
//!   `chunk` bytes up to `total`, then a CLOSE and a RENAME onto
//!   `rename_to` when `then_close`, or the RENAME while the handle is open
//!   and the CLOSE after it otherwise. `rename_to` may be left out. Thread
//!   `t > 0` writes and renames to the names with `.t` appended, so threads
//!   never share a file.
//!
//! Anything else (an unknown field or kind, a path the tree does not have, a
//! `close` with no `open` before it) is a [`LoadError`] naming the file and
//! the field, as the scenario loader's are.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use aether_protocol::{OpenFlags, RequestFlags, RootId};
use aether_refdirector::tree::{fill, is_canonical, Contents, File, Root, Tree};
use serde_json::{Map, Value};

use crate::scenario::OwnedRequest;

/// The most client threads one phase may ask for.
pub const MAX_THREADS: u32 = 64;

/// A benchmark workload: what to mount and what to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub name: String,
    pub description: String,
    /// The tree, `generate` entries expanded into its root 0.
    pub tree: Tree,
    pub phases: Vec<Phase>,
    /// Root 0's entries by folded path.
    index: BTreeMap<String, Entry>,
}

/// One phase: `threads` client threads, each sending its requests `repeat`
/// times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub name: String,
    pub threads: u32,
    pub repeat: u32,
    pub kind: PhaseKind,
}

/// What a phase sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseKind {
    Storm {
        ops: Vec<StormOp>,
        paths: PathSet,
    },
    SequentialRead {
        path: String,
        chunk: u32,
        bulk: bool,
    },
    RandomRead {
        path: String,
        len: u32,
        count: u32,
        seed: u64,
    },
    ReadDir {
        path: String,
    },
    Write {
        path: String,
        chunk: u32,
        total: u64,
        then_close: bool,
        rename_to: Option<String>,
    },
}

/// One request of a storm, for each file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StormOp {
    GetAttr,
    Open,
    Close,
    StoredNames,
}

/// The files a storm visits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSet {
    /// Every file of root 0.
    All,
    /// The files whose path starts with this, compared by the fold.
    Prefix(String),
    /// These files.
    List(Vec<String>),
}

/// What is at a path of root 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    File {
        size: u64,
    },
    /// A directory and how many names it holds.
    Dir {
        entries: usize,
    },
}

/// Why a workload did not load: the file, the field (dotted, as
/// `phases[1].kind`) and what was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub file: PathBuf,
    pub field: String,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if !self.field.is_empty() {
            write!(f, ": {}", self.field)?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for LoadError {}

/// Reads the workload in `path`.
pub fn load(path: &Path) -> Result<Workload, LoadError> {
    let err = |message: String| LoadError {
        file: path.to_path_buf(),
        field: String::new(),
        message,
    };
    let text = std::fs::read(path).map_err(|e| err(e.to_string()))?;
    let v: Value = serde_json::from_slice(&text).map_err(|e| err(e.to_string()))?;
    Workload::from_json(&v).map_err(|e| LoadError {
        file: path.to_path_buf(),
        ..e
    })
}

/// Reads every `*.json` file in `dir` as a workload, in file-name order.
pub fn load_dir(dir: &Path) -> Result<Vec<Workload>, LoadError> {
    let entries = std::fs::read_dir(dir).map_err(|e| LoadError {
        file: dir.to_path_buf(),
        field: String::new(),
        message: e.to_string(),
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files.iter().map(|p| load(p)).collect()
}

impl Workload {
    /// A workload from its JSON; the error's `file` is empty.
    pub fn from_json(v: &Value) -> Result<Workload, LoadError> {
        parse(v).map_err(|(field, message)| LoadError {
            file: PathBuf::new(),
            field,
            message,
        })
    }

    /// What is at `path` on root 0 (`"."` is the root), by the fold.
    pub fn lookup(&self, path: &str) -> Option<Entry> {
        self.index.get(&fold(path)).copied()
    }

    /// The files of root 0 `set` names, in the tree's order.
    pub fn paths<'a>(&'a self, set: &'a PathSet) -> Vec<&'a str> {
        let files = root0(&self.tree).map(|r| r.files.as_slice()).unwrap_or(&[]);
        match set {
            PathSet::All => files.iter().map(|f| f.path.as_str()).collect(),
            PathSet::Prefix(p) => {
                let p = fold(p);
                files
                    .iter()
                    .filter(|f| fold(&f.path).starts_with(&p))
                    .map(|f| f.path.as_str())
                    .collect()
            }
            PathSet::List(l) => l.iter().map(String::as_str).collect(),
        }
    }

    /// How many requests [`Workload::expand`] gives one thread of `phase`.
    pub fn requests_per_thread(&self, phase: &Phase) -> u64 {
        let pass = match &phase.kind {
            PhaseKind::Storm { ops, paths } => ops.len() as u64 * self.paths(paths).len() as u64,
            PhaseKind::SequentialRead { path, chunk, .. } => {
                2 + self.size(path).div_ceil(u64::from(*chunk))
            }
            PhaseKind::RandomRead { count, .. } => 2 + u64::from(*count),
            PhaseKind::ReadDir { .. } => 1,
            PhaseKind::Write {
                chunk,
                total,
                rename_to,
                ..
            } => 2 + total.div_ceil(u64::from(*chunk)) + u64::from(rename_to.is_some()),
        };
        pass * u64::from(phase.repeat)
    }

    /// The requests thread `thread` of `phase` sends, in order: the same
    /// ones every time for a given workload, phase and thread. A handle is
    /// numbered by the OPEN that made it, counting from 1 in this thread's
    /// requests (the first OPEN's handle is 1); whoever sends them puts the
    /// director's handle in its place, as a scenario's handles are paired.
    pub fn expand<'w>(
        &'w self,
        phase: &'w Phase,
        thread: u32,
    ) -> impl Iterator<Item = OwnedRequest> + 'w {
        let mut next_fh = 0u64;
        let mut open = move || {
            next_fh += 1;
            next_fh
        };
        let mut out: Vec<OwnedRequest> = Vec::new();
        let root = RootId(0);
        match &phase.kind {
            PhaseKind::Storm { ops, paths } => {
                let files = self.paths(paths);
                // Each thread starts at its own place in the list, so threads
                // do not ask for one file at the same moment.
                let start = if files.is_empty() {
                    0
                } else {
                    (splitmix(u64::from(thread)) % files.len() as u64) as usize
                };
                for _ in 0..phase.repeat {
                    for i in 0..files.len() {
                        let path = files[(start + i) % files.len()];
                        let mut fh = 0;
                        for op in ops {
                            out.push(match op {
                                StormOp::GetAttr => OwnedRequest::GetAttr {
                                    root,
                                    path: path.to_string(),
                                },
                                StormOp::Open => {
                                    fh = open();
                                    OwnedRequest::Open {
                                        root,
                                        flags: OpenFlags::READ,
                                        path: path.to_string(),
                                    }
                                }
                                StormOp::Close => OwnedRequest::Close { fh },
                                StormOp::StoredNames => OwnedRequest::StoredNames {
                                    root,
                                    skip: 0,
                                    path: path.to_string(),
                                },
                            });
                        }
                    }
                }
            }
            PhaseKind::SequentialRead { path, chunk, bulk } => {
                let size = self.size(path);
                let slot_flags = if *bulk {
                    RequestFlags::READ_BULK
                } else {
                    RequestFlags::empty()
                };
                for _ in 0..phase.repeat {
                    let fh = open();
                    out.push(OwnedRequest::Open {
                        root,
                        flags: OpenFlags::READ,
                        path: path.clone(),
                    });
                    let mut offset = 0;
                    while offset < size {
                        let len = u64::from(*chunk).min(size - offset) as u32;
                        out.push(OwnedRequest::Read {
                            slot_flags,
                            fh,
                            offset,
                            len,
                        });
                        offset += u64::from(len);
                    }
                    out.push(OwnedRequest::Close { fh });
                }
            }
            PhaseKind::RandomRead {
                path,
                len,
                count,
                seed,
            } => {
                let units = (self.size(path) / u64::from(*len)).max(1);
                let mut state = seed ^ splitmix(u64::from(thread).wrapping_add(0x5EED));
                for _ in 0..phase.repeat {
                    let fh = open();
                    out.push(OwnedRequest::Open {
                        root,
                        flags: OpenFlags::READ,
                        path: path.clone(),
                    });
                    for _ in 0..*count {
                        state = splitmix(state);
                        out.push(OwnedRequest::Read {
                            slot_flags: RequestFlags::empty(),
                            fh,
                            offset: (state % units) * u64::from(*len),
                            len: *len,
                        });
                    }
                    out.push(OwnedRequest::Close { fh });
                }
            }
            PhaseKind::ReadDir { path } => {
                for _ in 0..phase.repeat {
                    out.push(OwnedRequest::ReadDir {
                        root,
                        path: path.clone(),
                    });
                }
            }
            PhaseKind::Write {
                path,
                chunk,
                total,
                then_close,
                rename_to,
            } => {
                let own = |p: &str| {
                    if thread == 0 {
                        p.to_string()
                    } else {
                        format!("{p}.{thread}")
                    }
                };
                let (path, rename_to) = (own(path), rename_to.as_deref().map(own));
                let data = fill(*chunk as usize, u64::from(thread));
                for _ in 0..phase.repeat {
                    let fh = open();
                    out.push(OwnedRequest::Open {
                        root,
                        flags: OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNC,
                        path: path.clone(),
                    });
                    let mut offset = 0;
                    while offset < *total {
                        let n = u64::from(*chunk).min(total - offset) as usize;
                        out.push(OwnedRequest::Write {
                            fh,
                            offset,
                            data: data[..n].to_vec(),
                        });
                        offset += n as u64;
                    }
                    let rename = rename_to.as_ref().map(|to| OwnedRequest::Rename {
                        root,
                        from: path.clone(),
                        to: to.clone(),
                    });
                    if *then_close {
                        out.push(OwnedRequest::Close { fh });
                        out.extend(rename);
                    } else {
                        out.extend(rename);
                        out.push(OwnedRequest::Close { fh });
                    }
                }
            }
        }
        out.into_iter()
    }

    /// The size of the file at `path`, 0 if it is not one.
    fn size(&self, path: &str) -> u64 {
        match self.lookup(path) {
            Some(Entry::File { size }) => size,
            _ => 0,
        }
    }
}

/// splitmix64's step: a well-mixed `u64` from any `u64`.
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fold(path: &str) -> String {
    aether_casefold::fold(path)
}

fn root0(tree: &Tree) -> Option<&Root> {
    tree.roots.iter().find(|r| r.id == 0)
}

fn contents_len(c: &Contents) -> u64 {
    match c {
        Contents::Text(s) => s.len() as u64,
        Contents::Hex(b) => b.len() as u64,
        Contents::Fill { size, .. } => *size,
    }
}

/// Root 0's files and directories by folded path, each directory with the
/// number of names directly in it.
fn index(tree: &Tree) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    out.insert(".".to_string(), Entry::Dir { entries: 0 });
    let Some(r) = root0(tree) else {
        return out;
    };
    let mut add_dir = |d: &str, out: &mut BTreeMap<String, Entry>| {
        let comps: Vec<&str> = d.split('/').collect();
        for n in 1..=comps.len() {
            let here = fold(&comps[..n].join("/"));
            let parent = if n == 1 {
                ".".to_string()
            } else {
                fold(&comps[..n - 1].join("/"))
            };
            children.entry(parent).or_default().insert(here.clone());
            out.entry(here).or_insert(Entry::Dir { entries: 0 });
        }
    };
    for d in r.dirs.iter().chain(r.writable.iter()).filter(|d| *d != ".") {
        add_dir(d, &mut out);
    }
    for f in &r.files {
        if let Some((parent, _)) = f.path.rsplit_once('/') {
            add_dir(parent, &mut out);
        }
    }
    for f in &r.files {
        let here = fold(&f.path);
        let parent = match here.rsplit_once('/') {
            Some((p, _)) => p.to_string(),
            None => ".".to_string(),
        };
        children.entry(parent).or_default().insert(here.clone());
        out.insert(
            here,
            Entry::File {
                size: contents_len(&f.contents),
            },
        );
    }
    for (dir, names) in children {
        if let Some(Entry::Dir { entries }) = out.get_mut(&dir) {
            *entries = names.len();
        }
    }
    out
}

type R<T> = Result<T, (String, String)>;

/// The fields of one JSON object, read by name and checked for leftovers.
struct Obj<'v> {
    map: &'v Map<String, Value>,
    at: String,
    used: BTreeSet<&'v str>,
}

impl<'v> Obj<'v> {
    fn new(v: &'v Value, at: &str) -> R<Obj<'v>> {
        let map = v.as_object().ok_or_else(|| {
            (
                at.trim_end_matches('.').to_string(),
                format!("an object expected, found {v}"),
            )
        })?;
        Ok(Obj {
            map,
            at: at.to_string(),
            used: BTreeSet::new(),
        })
    }

    fn bad(&self, field: &str, message: impl Into<String>) -> (String, String) {
        (format!("{}{field}", self.at), message.into())
    }

    fn opt(&mut self, name: &str) -> Option<&'v Value> {
        let (k, v) = self.map.get_key_value(name)?;
        self.used.insert(k.as_str());
        Some(v)
    }

    fn get(&mut self, name: &str) -> R<&'v Value> {
        self.opt(name).ok_or_else(|| self.bad(name, "missing"))
    }

    fn u64(&mut self, name: &str) -> R<u64> {
        let v = self.get(name)?;
        v.as_u64()
            .ok_or_else(|| self.bad(name, format!("an unsigned integer expected, found {v}")))
    }

    /// A count of at least 1 and at most `max`.
    fn count(&mut self, name: &str, max: u64) -> R<u32> {
        let n = self.u64(name)?;
        if n == 0 || n > max {
            return Err(self.bad(name, format!("{n}: 1 to {max} expected")));
        }
        Ok(n as u32)
    }

    fn bool(&mut self, name: &str) -> R<bool> {
        let v = self.get(name)?;
        v.as_bool()
            .ok_or_else(|| self.bad(name, format!("true or false expected, found {v}")))
    }

    fn str(&mut self, name: &str) -> R<String> {
        let v = self.get(name)?;
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| self.bad(name, format!("a string expected, found {v}")))
    }

    fn vpath(&mut self, name: &str) -> R<String> {
        let p = self.str(name)?;
        if !is_canonical(&p) {
            return Err(self.bad(
                name,
                format!(
                    "{p:?} is not a canonical path (docs/wire.md 3.2: `/`-joined, no empty, \
                     `.` or `..` component, `.` for the root)"
                ),
            ));
        }
        Ok(p)
    }

    /// Fails on any field not read.
    fn done(self) -> R<()> {
        match self.map.keys().find(|k| !self.used.contains(k.as_str())) {
            None => Ok(()),
            Some(k) => Err(self.bad(k, format!("unknown field {k:?}"))),
        }
    }
}

/// The largest chunk a READ or WRITE of a workload may ask for.
const MAX_CHUNK: u64 = 16 * 1024 * 1024;

fn parse(v: &Value) -> R<Workload> {
    let mut o = Obj::new(v, "")?;
    let name = o.str("name")?;
    if name.is_empty() {
        return Err(o.bad("name", "empty"));
    }
    let description = o.str("description")?;
    let tree_v = o.get("tree")?;
    let mut tree: Tree =
        serde_json::from_value(tree_v.clone()).map_err(|e| o.bad("tree", e.to_string()))?;
    if let Some(g) = o.opt("generate") {
        let list = g
            .as_array()
            .ok_or_else(|| o.bad("generate", "a list expected"))?;
        let mut files = Vec::new();
        for (i, g) in list.iter().enumerate() {
            files.extend(generated(g, &format!("generate[{i}]."))?);
        }
        match tree.roots.iter_mut().find(|r| r.id == 0) {
            Some(r) => r.files.extend(files),
            None => tree.roots.push(Root {
                id: 0,
                files,
                ..Default::default()
            }),
        }
    }
    tree.validate().map_err(|e| o.bad("tree", e))?;
    let phases_v = o.get("phases")?;
    let list = phases_v
        .as_array()
        .ok_or_else(|| o.bad("phases", "a list of phases expected"))?;
    if list.is_empty() {
        return Err(o.bad("phases", "no phases"));
    }
    o.done()?;
    let mut w = Workload {
        name,
        description,
        index: index(&tree),
        tree,
        phases: Vec::new(),
    };
    for (i, p) in list.iter().enumerate() {
        let phase = parse_phase(&w, p, &format!("phases[{i}]."))?;
        w.phases.push(phase);
    }
    Ok(w)
}

/// A `generate` entry's files: `count` files in `dir`, named by `name` with
/// `{n}` the file's number from 0 (padded with zeros to `pad` digits), each
/// `size` bytes generated from `seed + n`.
fn generated(v: &Value, at: &str) -> R<Vec<File>> {
    let mut o = Obj::new(v, at)?;
    let dir = o.vpath("dir")?;
    let count = o.count("count", 100_000)?;
    let name = o.str("name")?;
    if name.matches("{n}").count() != 1 || name.contains('/') {
        return Err(o.bad("name", format!("{name:?}: one name holding `{{n}}` once")));
    }
    let pad = match o.opt("pad") {
        None => 0,
        Some(_) => o.count("pad", 12)? as usize,
    };
    let size = o.u64("size")?;
    let seed = o.u64("seed")?;
    o.done()?;
    Ok((0..count)
        .map(|n| {
            let file = name.replacen("{n}", &format!("{n:0pad$}"), 1);
            File {
                path: if dir == "." {
                    file
                } else {
                    format!("{dir}/{file}")
                },
                contents: Contents::Fill {
                    size,
                    seed: seed.wrapping_add(u64::from(n)),
                },
                mtime: 0,
            }
        })
        .collect())
}

fn parse_phase(w: &Workload, v: &Value, at: &str) -> R<Phase> {
    let mut o = Obj::new(v, at)?;
    let name = o.str("name")?;
    let threads = o.count("threads", u64::from(MAX_THREADS))?;
    let repeat = o.count("repeat", 1_000_000)?;
    let kind_name = o.str("kind")?;
    let file = |o: &mut Obj<'_>, field: &str| -> R<String> {
        let p = o.vpath(field)?;
        match w.lookup(&p) {
            Some(Entry::File { .. }) => Ok(p),
            _ => Err(o.bad(field, format!("{p:?} is not a file of the tree's root 0"))),
        }
    };
    let writable_place = |o: &mut Obj<'_>, field: &str| -> R<String> {
        let p = o.vpath(field)?;
        let parent = p.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        if !matches!(w.lookup(parent), Some(Entry::Dir { .. })) || !writable(&w.tree, &p) {
            return Err(o.bad(
                field,
                format!("{p:?} is not in a directory of the tree under a `writable` entry"),
            ));
        }
        if matches!(w.lookup(&p), Some(Entry::Dir { .. })) {
            return Err(o.bad(field, format!("{p:?} is a directory")));
        }
        Ok(p)
    };
    let kind = match kind_name.as_str() {
        "storm" => {
            let ops_v = o.get("ops")?;
            let names = ops_v
                .as_array()
                .ok_or_else(|| o.bad("ops", "a list of operations expected"))?;
            let mut ops = Vec::new();
            let mut opened = false;
            for n in names {
                let op = match n.as_str() {
                    Some("getattr") => StormOp::GetAttr,
                    Some("open") => StormOp::Open,
                    Some("close") => StormOp::Close,
                    Some("stored_names") => StormOp::StoredNames,
                    _ => {
                        return Err(o.bad(
                            "ops",
                            format!("unknown operation {n}: getattr, open, close or stored_names"),
                        ))
                    }
                };
                match op {
                    StormOp::Open if opened => {
                        return Err(o.bad("ops", "an open before the last one was closed"))
                    }
                    StormOp::Open => opened = true,
                    StormOp::Close if !opened => {
                        return Err(o.bad("ops", "a close with no open before it"))
                    }
                    StormOp::Close => opened = false,
                    _ => {}
                }
                ops.push(op);
            }
            if ops.is_empty() {
                return Err(o.bad("ops", "no operations"));
            }
            if opened {
                return Err(o.bad("ops", "an open that is never closed"));
            }
            let paths_v = o.get("paths")?;
            let paths = match paths_v {
                Value::String(s) if s == "all" => PathSet::All,
                Value::Object(m)
                    if m.len() == 1 && m.get("prefix").is_some_and(Value::is_string) =>
                {
                    PathSet::Prefix(m["prefix"].as_str().unwrap_or_default().to_string())
                }
                Value::Object(m) if m.len() == 1 && m.get("list").is_some_and(Value::is_array) => {
                    let mut l = Vec::new();
                    for p in m["list"].as_array().into_iter().flatten() {
                        let p = p.as_str().unwrap_or_default();
                        if !matches!(w.lookup(p), Some(Entry::File { .. })) {
                            return Err(o.bad(
                                "paths.list",
                                format!("{p:?} is not a file of the tree's root 0"),
                            ));
                        }
                        l.push(p.to_string());
                    }
                    PathSet::List(l)
                }
                other => {
                    return Err(o.bad(
                        "paths",
                        format!(
                            "\"all\", {{\"prefix\": \"..\"}} or {{\"list\": [..]}} expected, \
                             found {other}"
                        ),
                    ))
                }
            };
            if w.paths(&paths).is_empty() {
                return Err(o.bad("paths", "names no file of the tree"));
            }
            PhaseKind::Storm { ops, paths }
        }
        "sequential_read" => PhaseKind::SequentialRead {
            path: file(&mut o, "path")?,
            chunk: o.count("chunk", MAX_CHUNK)?,
            bulk: o.bool("bulk")?,
        },
        "random_read" => {
            let path = file(&mut o, "path")?;
            let len = o.count("len", MAX_CHUNK)?;
            let size = w.size(&path);
            if u64::from(len) > size {
                return Err(o.bad(
                    "len",
                    format!("{len} is longer than {path:?} ({size} bytes)"),
                ));
            }
            PhaseKind::RandomRead {
                path,
                len,
                count: o.count("count", 100_000_000)?,
                seed: o.u64("seed")?,
            }
        }
        "readdir" => {
            let path = o.vpath("path")?;
            if !matches!(w.lookup(&path), Some(Entry::Dir { .. })) {
                return Err(o.bad(
                    "path",
                    format!("{path:?} is not a directory of the tree's root 0"),
                ));
            }
            PhaseKind::ReadDir { path }
        }
        "write" => {
            let path = writable_place(&mut o, "path")?;
            let chunk = o.count("chunk", MAX_CHUNK)?;
            let total = o.u64("total")?;
            if total == 0 || total > aether_refdirector::MAX_FILE_LEN as u64 {
                return Err(o.bad(
                    "total",
                    format!(
                        "{total}: 1 to {} expected (the reference's largest write)",
                        aether_refdirector::MAX_FILE_LEN
                    ),
                ));
            }
            let then_close = o.bool("then_close")?;
            let rename_to = match o.opt("rename_to") {
                None => None,
                Some(_) => Some(writable_place(&mut o, "rename_to")?),
            };
            PhaseKind::Write {
                path,
                chunk,
                total,
                then_close,
                rename_to,
            }
        }
        other => {
            return Err(o.bad(
                "kind",
                format!(
                    "unknown kind {other:?}: storm, sequential_read, random_read, readdir \
                     or write"
                ),
            ))
        }
    };
    o.done()?;
    Ok(Phase {
        name,
        threads,
        repeat,
        kind,
    })
}

/// Whether `path` lies under one of root 0's `writable` entries.
fn writable(tree: &Tree, path: &str) -> bool {
    let p = fold(path);
    root0(tree).is_some_and(|r| {
        r.writable.iter().any(|w| {
            w == "." || {
                let w = fold(w);
                p.starts_with(&format!("{w}/"))
            }
        })
    })
}
