//! Scenarios: a tree to mount and a list of steps, each a request and the
//! answer expected for it, read from JSON in the protocol table's own names
//! (`contract/protocol.json`). The format is in `contract/scenarios/README.md`;
//! in short:
//!
//! ```json
//! {
//!   "name": "read-a-file",
//!   "description": "OPEN, READ and CLOSE of a mounted file",
//!   "tree": {"roots": [{"id": 0, "files": [{"path": "data/a.esp", "contents": "abc"}]}]},
//!   "steps": [
//!     {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
//!      "expect": {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}}},
//!     {"request": {"op": "read", "fh": 1, "offset": 0, "len": 16},
//!      "expect": {"reply": {"data": "abc"}}},
//!     {"request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}}
//!   ]
//! }
//! ```
//!
//! A request is `op` (an opcode's name) and that opcode's request fields;
//! READ may add `slot_flags` (`["read_bulk"]`), and a handle may be written
//! `{"literal": n}` to be sent as it is ([`Step::literal_fh`]). An
//! expectation is a
//! `reply` (the opcode's reply fields), a `status` name, or `one_of`
//! several, which needs `wire`, the `docs/wire.md` section that allows more
//! than one answer. `{"status": "ok"}` is the empty reply, for the opcodes
//! that have one. Bytes are a string (UTF-8), `{"hex": ".."}` or
//! `{"size": N, "seed": S}`, and in a step also `{"zeros": N}` and
//! `{"concat": [..]}`. File paths in requests are canonical
//! (`docs/wire.md` 3.2: other syntax is director-defined). Anything else (an
//! unknown op, field, flag or status, a missing field, a non-canonical path)
//! is a [`LoadError`] naming the file, the step and the field.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use aether_protocol::{
    AttrResp, ChangedReply, Lookup, LookupReply, Opcode, OpenFlags, OpenResp, ReadReply, Reply,
    Request, RequestFlags, RootId, Status,
};
use aether_refdirector::tree::{is_canonical, Contents, Tree};
use aether_registry::{Child, Node, Value as RegValue};
use serde_json::{Map, Value};

/// One scenario: what to mount and what to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub tree: Tree,
    pub steps: Vec<Step>,
}

/// A request and the answer it must get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub request: OwnedRequest,
    pub expect: Expect,
    /// The `docs/wire.md` section that allows an [`Expect::OneOf`].
    pub cite: Option<String>,
    /// What the step is for, if the scenario says.
    pub note: Option<String>,
    /// Which client sends it (`"client"`, default 0): the run attaches one
    /// client per number, each a process of the session as the shim's are
    /// (`docs/wire.md` 2.8.1), and pairs handles across all of them.
    pub client: u32,
    /// The request's `fh` was written `{"literal": n}`: sent as it is, not
    /// paired with a director's handle, so a scenario can send a number no
    /// director issues (`fh` 0, wire.md 3.4.3, 3.4.10).
    pub literal_fh: bool,
}

/// What a step expects: a reply (status OK), an error status, or any of
/// several where `docs/wire.md` allows more than one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Reply(OwnedReply),
    Status(Status),
    OneOf(Vec<Expect>),
}

/// A [`Request`] owning its strings and bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedRequest {
    GetAttr {
        root: RootId,
        path: String,
    },
    ReadDir {
        root: RootId,
        path: String,
    },
    Open {
        root: RootId,
        flags: OpenFlags,
        path: String,
    },
    Read {
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    },
    Write {
        fh: u64,
        offset: u64,
        data: Vec<u8>,
    },
    SetAttr {
        fh: u64,
        size: u64,
    },
    Rename {
        root: RootId,
        from: String,
        to: String,
    },
    Delete {
        root: RootId,
        path: String,
    },
    Mkdir {
        root: RootId,
        mode: u32,
        path: String,
    },
    Close {
        fh: u64,
    },
    Heartbeat,
    StoredNames {
        root: RootId,
        skip: u32,
        path: String,
    },
    RegLookup {
        path: String,
    },
    RegKey {
        path: String,
    },
    RegSetValue {
        path: String,
        name: String,
        ty: u32,
        data: Vec<u8>,
    },
    RegDeleteValue {
        path: String,
        name: String,
    },
    RegCreateKey {
        path: String,
        volatile: bool,
    },
    RegDeleteKey {
        path: String,
    },
    RegRenameKey {
        path: String,
        new_leaf: String,
    },
    RegChanged {
        path: String,
        subtree: bool,
        since: u64,
    },
    RenameX {
        from_root: RootId,
        to_root: RootId,
        from: String,
        to: String,
    },
    StageExec {
        root: RootId,
        path: String,
    },
}

impl OwnedRequest {
    /// The borrowed request, to send.
    pub fn as_request(&self) -> Request<'_> {
        use OwnedRequest as O;
        match self {
            O::GetAttr { root, path } => Request::GetAttr { root: *root, path },
            O::ReadDir { root, path } => Request::ReadDir { root: *root, path },
            O::Open { root, flags, path } => Request::Open {
                root: *root,
                flags: *flags,
                path,
            },
            O::Read {
                slot_flags,
                fh,
                offset,
                len,
            } => Request::Read {
                slot_flags: *slot_flags,
                fh: *fh,
                offset: *offset,
                len: *len,
            },
            O::Write { fh, offset, data } => Request::Write {
                fh: *fh,
                offset: *offset,
                data,
            },
            O::SetAttr { fh, size } => Request::SetAttr {
                fh: *fh,
                size: *size,
            },
            O::Rename { root, from, to } => Request::Rename {
                root: *root,
                from,
                to,
            },
            O::Delete { root, path } => Request::Delete { root: *root, path },
            O::Mkdir { root, mode, path } => Request::Mkdir {
                root: *root,
                mode: *mode,
                path,
            },
            O::Close { fh } => Request::Close { fh: *fh },
            O::Heartbeat => Request::Heartbeat {},
            O::StoredNames { root, skip, path } => Request::StoredNames {
                root: *root,
                skip: *skip,
                path,
            },
            O::RegLookup { path } => Request::RegLookup { path },
            O::RegKey { path } => Request::RegKey { path },
            O::RegSetValue {
                path,
                name,
                ty,
                data,
            } => Request::RegSetValue {
                path,
                name,
                ty: *ty,
                data,
            },
            O::RegDeleteValue { path, name } => Request::RegDeleteValue { path, name },
            O::RegCreateKey { path, volatile } => Request::RegCreateKey {
                path,
                volatile: *volatile,
            },
            O::RegDeleteKey { path } => Request::RegDeleteKey { path },
            O::RegRenameKey { path, new_leaf } => Request::RegRenameKey { path, new_leaf },
            O::RegChanged {
                path,
                subtree,
                since,
            } => Request::RegChanged {
                path,
                subtree: *subtree,
                since: *since,
            },
            O::RenameX {
                from_root,
                to_root,
                from,
                to,
            } => Request::RenameX {
                from_root: *from_root,
                to_root: *to_root,
                from,
                to,
            },
            O::StageExec { root, path } => Request::StageExec { root: *root, path },
        }
    }

    /// The request's opcode.
    pub fn opcode(&self) -> Opcode {
        self.as_request().opcode()
    }
}

/// One READDIR entry, owned.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnedDirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// A [`Reply`] owning its strings, bytes and registry node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedReply {
    GetAttr(AttrResp),
    ReadDir(Vec<OwnedDirEntry>),
    Open(OpenResp),
    /// A READ's data, however it was carried.
    Read(Vec<u8>),
    /// A bulk READ reply not yet read from the arena
    /// ([`OwnedReply::resolve_bulk`]); it compares with nothing.
    ReadBulk {
        len: u32,
        arena_offset: u64,
    },
    Write(u32),
    StoredNames(String),
    StageExec(String),
    RegLookup(LookupReply),
    RegKey {
        version: u64,
        node: Option<Node>,
    },
    RegVersion(u64),
    RegChanged(ChangedReply),
    Empty,
}

impl OwnedReply {
    /// The owned form of `r`. A bulk READ reply's data is in the arena,
    /// which `r` cannot reach: it becomes [`OwnedReply::ReadBulk`].
    pub fn from_reply(r: &Reply<'_>) -> OwnedReply {
        match r {
            Reply::GetAttr(a) => OwnedReply::GetAttr(a.clone()),
            Reply::ReadDir(es) => OwnedReply::ReadDir(
                es.iter()
                    .map(|e| OwnedDirEntry {
                        name: e.name.to_string(),
                        is_dir: e.is_dir,
                        size: e.size,
                        mtime: e.mtime,
                    })
                    .collect(),
            ),
            Reply::Open(o) => OwnedReply::Open(o.clone()),
            Reply::Read(ReadReply::Inline(b)) => OwnedReply::Read(b.to_vec()),
            Reply::Read(ReadReply::Bulk { len, arena_offset }) => OwnedReply::ReadBulk {
                len: *len,
                arena_offset: *arena_offset,
            },
            Reply::Write(n) => OwnedReply::Write(*n),
            Reply::StoredNames(s) => OwnedReply::StoredNames(s.to_string()),
            Reply::StageExec(s) => OwnedReply::StageExec(s.to_string()),
            Reply::RegLookup(l) => OwnedReply::RegLookup(*l),
            Reply::RegKey(k) => OwnedReply::RegKey {
                version: k.version,
                node: k.node.as_ref().map(|n| n.as_ref().clone()),
            },
            Reply::RegVersion(v) => OwnedReply::RegVersion(*v),
            Reply::RegChanged(c) => OwnedReply::RegChanged(*c),
            Reply::Empty => OwnedReply::Empty,
        }
    }

    /// A [`OwnedReply::ReadBulk`] with its data read by `read(len,
    /// arena_offset)`; any other reply as it is.
    pub fn resolve_bulk(self, read: impl FnOnce(u32, u64) -> Vec<u8>) -> OwnedReply {
        match self {
            OwnedReply::ReadBulk { len, arena_offset } => OwnedReply::Read(read(len, arena_offset)),
            r => r,
        }
    }
}

/// Why a scenario did not load: the file, the step (if the error is in
/// one), the field, and what was wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub file: PathBuf,
    pub step: Option<usize>,
    /// Dotted, from the scenario or step (`request.flags`, `tree`).
    pub field: String,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if let Some(i) = self.step {
            write!(f, ": step {i}")?;
        }
        if !self.field.is_empty() {
            write!(f, ": {}", self.field)?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for LoadError {}

/// Reads the scenario in `path`.
pub fn load(path: &Path) -> Result<Scenario, LoadError> {
    let err = |field: &str, message: String| LoadError {
        file: path.to_path_buf(),
        step: None,
        field: field.to_string(),
        message,
    };
    let text = std::fs::read(path).map_err(|e| err("", e.to_string()))?;
    let v: Value = serde_json::from_slice(&text).map_err(|e| err("", e.to_string()))?;
    parse_scenario(&v).map_err(|e| LoadError {
        file: path.to_path_buf(),
        step: e.step,
        field: e.field,
        message: e.message,
    })
}

/// Reads every `*.json` file in `dir` as a scenario, in file-name order.
pub fn load_dir(dir: &Path) -> Result<Vec<Scenario>, LoadError> {
    let entries = std::fs::read_dir(dir).map_err(|e| LoadError {
        file: dir.to_path_buf(),
        step: None,
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

/// The request fields of the opcode named `op`, as `contract/protocol.json`
/// names them (padding aside), plus `slot_flags` for READ; `None` for a name
/// that is not an opcode's.
pub fn request_fields(op: &str) -> Option<&'static [&'static str]> {
    Some(match opcode(op)? {
        Opcode::GetAttr | Opcode::ReadDir | Opcode::Delete | Opcode::StageExec => &["root", "path"],
        Opcode::Open => &["root", "flags", "path"],
        Opcode::Read => &["slot_flags", "fh", "offset", "len"],
        Opcode::Write => &["fh", "offset", "data"],
        Opcode::SetAttr => &["fh", "size"],
        Opcode::Rename => &["root", "from", "to"],
        Opcode::Mkdir => &["root", "mode", "path"],
        Opcode::Close => &["fh"],
        Opcode::Heartbeat => &[],
        Opcode::StoredNames => &["root", "skip", "path"],
        Opcode::RegLookup | Opcode::RegKey | Opcode::RegDeleteKey => &["path"],
        Opcode::RegSetValue => &["path", "name", "ty", "data"],
        Opcode::RegDeleteValue => &["path", "name"],
        Opcode::RegCreateKey => &["path", "volatile"],
        Opcode::RegRenameKey => &["path", "new_leaf"],
        Opcode::RegChanged => &["path", "subtree", "since"],
        Opcode::RenameX => &["from_root", "to_root", "from", "to"],
    })
}

/// The reply fields of `op` on OK, as `contract/protocol.json` names them
/// (padding aside).
fn reply_fields(op: Opcode) -> &'static [&'static str] {
    match op {
        Opcode::GetAttr => &["found", "is_dir", "size", "mtime"],
        Opcode::ReadDir => &["entries"],
        Opcode::Open => &["fh", "size", "is_dir", "immutable", "mount_gen"],
        Opcode::Read => &["data"],
        Opcode::Write => &["written"],
        Opcode::StoredNames => &["names"],
        Opcode::StageExec => &["path"],
        Opcode::RegLookup => &["version", "state", "below"],
        Opcode::RegKey => &["version", "node"],
        Opcode::RegSetValue
        | Opcode::RegDeleteValue
        | Opcode::RegCreateKey
        | Opcode::RegDeleteKey
        | Opcode::RegRenameKey => &["version"],
        Opcode::RegChanged => &["version", "changed"],
        Opcode::SetAttr
        | Opcode::Rename
        | Opcode::Delete
        | Opcode::Mkdir
        | Opcode::Close
        | Opcode::Heartbeat
        | Opcode::RenameX => &[],
    }
}

fn opcode(name: &str) -> Option<Opcode> {
    Opcode::ALL.iter().copied().find(|o| o.name() == name)
}

fn status(name: &str) -> Option<Status> {
    Status::ALL.iter().copied().find(|s| s.name() == name)
}

/// An error inside a scenario, before the file is attached.
struct Bad {
    step: Option<usize>,
    field: String,
    message: String,
}

type R<T> = Result<T, Bad>;

/// The fields of one JSON object, read by name and checked for leftovers.
struct Obj<'v> {
    map: &'v Map<String, Value>,
    /// Dotted prefix of every field named here (`request.`).
    at: String,
    step: Option<usize>,
    used: BTreeSet<&'v str>,
}

impl<'v> Obj<'v> {
    fn new(v: &'v Value, at: &str, step: Option<usize>) -> R<Obj<'v>> {
        let map = v.as_object().ok_or_else(|| Bad {
            step,
            field: at.trim_end_matches('.').to_string(),
            message: format!("an object expected, found {v}"),
        })?;
        Ok(Obj {
            map,
            at: at.to_string(),
            step,
            used: BTreeSet::new(),
        })
    }

    fn bad(&self, field: &str, message: impl Into<String>) -> Bad {
        Bad {
            step: self.step,
            field: format!("{}{field}", self.at),
            message: message.into(),
        }
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

    fn u32(&mut self, name: &str) -> R<u32> {
        let n = self.u64(name)?;
        u32::try_from(n).map_err(|_| self.bad(name, format!("{n} does not fit a u32")))
    }

    fn i64(&mut self, name: &str) -> R<i64> {
        let v = self.get(name)?;
        v.as_i64()
            .ok_or_else(|| self.bad(name, format!("an integer expected, found {v}")))
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

    fn root(&mut self, name: &str) -> R<RootId> {
        self.u32(name).map(RootId)
    }

    /// A file path: canonical (`docs/wire.md` 3.2), or one that leaves its
    /// root ([`is_escape`]), the one other path wire.md 3.2 says what to do
    /// with.
    fn vpath(&mut self, name: &str) -> R<String> {
        let p = self.str(name)?;
        if !is_canonical(&p) && !is_escape(&p) {
            return Err(self.bad(
                name,
                format!(
                    "{p:?} is not a canonical path (docs/wire.md 3.2: `/`-joined, \
                     no empty, `.` or `..` component, `.` for the root), nor a \
                     path that leaves its root (it starts with `..`)"
                ),
            ));
        }
        Ok(p)
    }

    fn bytes(&mut self, name: &str) -> R<Vec<u8>> {
        let v = self.get(name)?;
        step_bytes(v).map_err(|e| self.bad(name, e))
    }

    /// A handle field: a number, or `{"literal": n}` (sets `literal`).
    fn fh(&mut self, literal: &mut bool) -> R<u64> {
        let v = self.get("fh")?;
        if let Some(m) = v.as_object() {
            let n = m.get("literal").and_then(Value::as_u64);
            return match (n, m.len()) {
                (Some(n), 1) => {
                    *literal = true;
                    Ok(n)
                }
                _ => Err(self.bad(
                    "fh",
                    format!("a handle number or {{\"literal\": n}} expected, found {v}"),
                )),
            };
        }
        self.u64("fh")
    }

    /// A list of names, each one of `known`'s, as their masks or'ed.
    fn flags(&mut self, name: &str, known: &[(&str, u32)]) -> R<u32> {
        let v = self.get(name)?;
        let list = v
            .as_array()
            .ok_or_else(|| self.bad(name, format!("a list of names expected, found {v}")))?;
        let mut bits = 0;
        for f in list {
            let n = f
                .as_str()
                .ok_or_else(|| self.bad(name, format!("a flag name expected, found {f}")))?;
            let (_, mask) = known.iter().find(|(k, _)| *k == n).ok_or_else(|| {
                let names: Vec<&str> = known.iter().map(|(k, _)| *k).collect();
                self.bad(
                    name,
                    format!("unknown flag {n:?}, expected one of {}", names.join(", ")),
                )
            })?;
            bits |= mask;
        }
        Ok(bits)
    }

    /// Fails on any field not read.
    fn done(self) -> R<()> {
        match self.map.keys().find(|k| !self.used.contains(k.as_str())) {
            None => Ok(()),
            Some(k) => Err(self.bad(k, format!("unknown field {k:?}"))),
        }
    }
}

/// The most bytes `{"zeros": n}` may stand for: the reference's largest file.
const MAX_ZEROS: u64 = aether_refdirector::MAX_FILE_LEN as u64;

/// Bytes in a step (WRITE's `data`, READ's reply `data`, a value's `data`):
/// the tree's forms ([`Contents`]), and two more, `{"zeros": n}` (n zero
/// bytes) and `{"concat": [..]}` (each element's bytes, in order), so a
/// file spliced from several fills can be written down.
fn step_bytes(v: &Value) -> Result<Vec<u8>, String> {
    if let Some(m) = v.as_object() {
        if let (Some(z), 1) = (m.get("zeros"), m.len()) {
            return match z.as_u64() {
                Some(n) if n <= MAX_ZEROS => Ok(vec![0; n as usize]),
                _ => Err(format!(
                    "zeros: a count of at most {MAX_ZEROS} expected, found {z}"
                )),
            };
        }
        if let (Some(c), 1) = (m.get("concat"), m.len()) {
            let parts = c
                .as_array()
                .ok_or_else(|| format!("concat: a list of bytes expected, found {c}"))?;
            let mut out = Vec::new();
            for (n, p) in parts.iter().enumerate() {
                out.extend(step_bytes(p).map_err(|e| format!("concat[{n}]: {e}"))?);
            }
            return Ok(out);
        }
    }
    match serde_json::from_value::<Contents>(v.clone()).map_err(|e| e.to_string())? {
        // A mounted file may be larger (`MAX_MOUNTED_LEN`); a step's bytes
        // go no further than the reference's largest file.
        Contents::Fill { size, .. } if size > MAX_ZEROS => Err(format!(
            "size {size}: larger than {MAX_ZEROS}, the longest file a write can make"
        )),
        c => Ok(c.bytes()),
    }
}

/// Whether `path` leaves its root however a director normalises paths: its
/// first component is `..`, and every other component is a name or `..` (not
/// empty, not `.`, no backslash). wire.md 3.2: "a director must treat a path
/// that would leave its root (`..` above the root) as an error". Later `..`
/// components are allowed: they cannot bring a path that starts above the root
/// back inside it. A path that goes up only after other components is not
/// sent, since directors differ on where it ends up.
fn is_escape(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("..") && !path.contains('\\') && parts.all(|c| !c.is_empty() && c != ".")
}

/// The names and masks of `OpenFlags`, as `contract/protocol.json`'s
/// `open_flags`.
fn open_flag_names() -> Vec<(String, u32)> {
    OpenFlags::all()
        .iter_names()
        .map(|(n, f)| (n.to_ascii_lowercase(), f.bits()))
        .collect()
}

fn request_flag_names() -> Vec<(String, u32)> {
    RequestFlags::all()
        .iter_names()
        .map(|(n, f)| (n.to_ascii_lowercase(), f.bits()))
        .collect()
}

fn as_refs(v: &[(String, u32)]) -> Vec<(&str, u32)> {
    v.iter().map(|(n, m)| (n.as_str(), *m)).collect()
}

fn parse_scenario(v: &Value) -> R<Scenario> {
    let mut o = Obj::new(v, "", None)?;
    let name = o.str("name")?;
    let description = o.str("description")?;
    let tree_v = o.get("tree")?;
    let tree: Tree =
        serde_json::from_value(tree_v.clone()).map_err(|e| o.bad("tree", e.to_string()))?;
    tree.validate().map_err(|e| o.bad("tree", e))?;
    // A trial mount: what only the registry overlay can refuse (a key path,
    // a name or data past its limits) is a load error, not a failed run.
    aether_refdirector::Reference::from_tree(&tree).map_err(|e| o.bad("tree", e))?;
    let steps_v = o.get("steps")?;
    let list = steps_v
        .as_array()
        .ok_or_else(|| o.bad("steps", "a list of steps expected"))?;
    o.done()?;
    let steps = list
        .iter()
        .enumerate()
        .map(|(i, s)| parse_step(i, s))
        .collect::<R<Vec<Step>>>()?;
    check_handles(&steps)?;
    Ok(Scenario {
        name,
        description,
        tree,
        steps,
    })
}

/// Refuses an OPEN that expects a handle number still open in the scenario
/// (opened by an earlier OPEN and not closed since by a CLOSE that may
/// succeed). wire.md 3.4.3: `fh` is "unique among open handles", so such a
/// step could only pass for a director that issued one handle twice.
fn check_handles(steps: &[Step]) -> R<()> {
    fn opened(e: &Expect) -> Vec<u64> {
        match e {
            Expect::Reply(OwnedReply::Open(o)) => vec![o.fh],
            Expect::OneOf(alts) => alts.iter().flat_map(opened).collect(),
            _ => Vec::new(),
        }
    }
    fn may_succeed(e: &Expect) -> bool {
        match e {
            Expect::Reply(OwnedReply::Empty) => true,
            Expect::OneOf(alts) => alts.iter().any(may_succeed),
            _ => false,
        }
    }
    let mut open = BTreeSet::new();
    for (i, st) in steps.iter().enumerate() {
        match &st.request {
            OwnedRequest::Open { .. } => {
                for fh in opened(&st.expect) {
                    if open.contains(&fh) {
                        return Err(Bad {
                            step: Some(i),
                            field: "expect.reply.fh".into(),
                            message: format!(
                                "handle {fh} is still open (wire.md 3.4.3: unique among open \
                                 handles); close it first or expect another number"
                            ),
                        });
                    }
                }
                open.extend(opened(&st.expect));
            }
            OwnedRequest::Close { fh } if may_succeed(&st.expect) => {
                open.remove(fh);
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_step(i: usize, v: &Value) -> R<Step> {
    let mut o = Obj::new(v, "", Some(i))?;
    let mut literal_fh = false;
    let request = parse_request(i, o.get("request")?, &mut literal_fh)?;
    let note = match o.opt("note") {
        None => None,
        Some(_) => Some(o.str("note")?),
    };
    let (expect, cite) = parse_expect(i, request.opcode(), o.get("expect")?, "expect.", true)?;
    let client = match o.opt("client") {
        None => 0,
        Some(_) => o.u32("client")?,
    };
    o.done()?;
    Ok(Step {
        request,
        expect,
        cite,
        note,
        client,
        literal_fh,
    })
}

/// A request in the scenario format ([`request_json`]) read back, as a
/// client worker receives it; the error says which field is wrong.
pub fn parse_request_value(v: &Value) -> Result<OwnedRequest, String> {
    parse_request(0, v, &mut false).map_err(|b| format!("{}: {}", b.field, b.message))
}

/// A director's answer in the scenario's expectation format: `{"reply":
/// ...}`, `{"status": "ok"}` for an empty reply, or `{"status": name}`. How a
/// client worker hands an answer back ([`parse_answer`] reads it).
pub fn answer_json(a: &Result<OwnedReply, Status>) -> Value {
    match a {
        Ok(r) => expect_json(&Expect::Reply(r.clone()), None),
        Err(s) => expect_json(&Expect::Status(*s), None),
    }
}

/// [`answer_json`]'s output for a request with opcode `op`, read back.
pub fn parse_answer(op: Opcode, v: &Value) -> Result<Result<OwnedReply, Status>, String> {
    match parse_expect(0, op, v, "", false) {
        Ok((Expect::Reply(r), _)) => Ok(Ok(r)),
        Ok((Expect::Status(s), _)) => Ok(Err(s)),
        Ok((Expect::OneOf(_), _)) => Err("an answer is not a one_of".into()),
        Err(b) => Err(format!("{}: {}", b.field, b.message)),
    }
}

/// A request; `literal` is set when its `fh` is written `{"literal": n}`.
fn parse_request(i: usize, v: &Value, literal: &mut bool) -> R<OwnedRequest> {
    use OwnedRequest as O;
    let mut o = Obj::new(v, "request.", Some(i))?;
    let name = o.str("op")?;
    let op = opcode(&name).ok_or_else(|| {
        let all: Vec<&str> = Opcode::ALL.iter().map(|o| o.name()).collect();
        o.bad(
            "op",
            format!("unknown op {name:?}, expected one of {}", all.join(", ")),
        )
    })?;
    let open_flags = open_flag_names();
    let req = match op {
        Opcode::GetAttr => O::GetAttr {
            root: o.root("root")?,
            path: o.vpath("path")?,
        },
        Opcode::ReadDir => O::ReadDir {
            root: o.root("root")?,
            path: o.vpath("path")?,
        },
        Opcode::Open => O::Open {
            root: o.root("root")?,
            flags: OpenFlags::from_bits_retain(o.flags("flags", &as_refs(&open_flags))?),
            path: o.vpath("path")?,
        },
        Opcode::Read => O::Read {
            slot_flags: match o.opt("slot_flags") {
                None => RequestFlags::empty(),
                Some(_) => RequestFlags::from_bits_retain(
                    o.flags("slot_flags", &as_refs(&request_flag_names()))?,
                ),
            },
            fh: o.fh(literal)?,
            offset: o.u64("offset")?,
            len: o.u32("len")?,
        },
        Opcode::Write => O::Write {
            fh: o.fh(literal)?,
            offset: o.u64("offset")?,
            data: o.bytes("data")?,
        },
        Opcode::SetAttr => O::SetAttr {
            fh: o.fh(literal)?,
            size: o.u64("size")?,
        },
        Opcode::Rename => O::Rename {
            root: o.root("root")?,
            from: o.vpath("from")?,
            to: o.vpath("to")?,
        },
        Opcode::Delete => O::Delete {
            root: o.root("root")?,
            path: o.vpath("path")?,
        },
        Opcode::Mkdir => O::Mkdir {
            root: o.root("root")?,
            mode: o.u32("mode")?,
            path: o.vpath("path")?,
        },
        Opcode::Close => O::Close { fh: o.fh(literal)? },
        Opcode::Heartbeat => O::Heartbeat,
        Opcode::StoredNames => O::StoredNames {
            root: o.root("root")?,
            skip: o.u32("skip")?,
            path: o.vpath("path")?,
        },
        Opcode::RegLookup => O::RegLookup {
            path: o.str("path")?,
        },
        Opcode::RegKey => O::RegKey {
            path: o.str("path")?,
        },
        Opcode::RegSetValue => O::RegSetValue {
            path: o.str("path")?,
            name: o.str("name")?,
            ty: o.u32("ty")?,
            data: o.bytes("data")?,
        },
        Opcode::RegDeleteValue => O::RegDeleteValue {
            path: o.str("path")?,
            name: o.str("name")?,
        },
        Opcode::RegCreateKey => O::RegCreateKey {
            path: o.str("path")?,
            volatile: o.bool("volatile")?,
        },
        Opcode::RegDeleteKey => O::RegDeleteKey {
            path: o.str("path")?,
        },
        Opcode::RegRenameKey => O::RegRenameKey {
            path: o.str("path")?,
            new_leaf: o.str("new_leaf")?,
        },
        Opcode::RegChanged => O::RegChanged {
            path: o.str("path")?,
            subtree: o.bool("subtree")?,
            since: o.u64("since")?,
        },
        Opcode::RenameX => O::RenameX {
            from_root: o.root("from_root")?,
            to_root: o.root("to_root")?,
            from: o.vpath("from")?,
            to: o.vpath("to")?,
        },
        Opcode::StageExec => O::StageExec {
            root: o.root("root")?,
            path: o.vpath("path")?,
        },
    };
    o.done()?;
    Ok(req)
}

/// An expectation for a request with opcode `op`, and the wire section a
/// top-level `one_of` cites.
fn parse_expect(
    i: usize,
    op: Opcode,
    v: &Value,
    at: &str,
    top: bool,
) -> R<(Expect, Option<String>)> {
    let mut o = Obj::new(v, at, Some(i))?;
    let (reply, st, one_of) = (o.opt("reply"), o.opt("status"), o.opt("one_of"));
    let out = match (reply, st, one_of) {
        (Some(r), None, None) => (
            Expect::Reply(parse_reply(i, op, r, &format!("{at}reply."))?),
            None,
        ),
        (None, Some(_), None) => {
            let name = o.str("status")?;
            let s = status(&name).ok_or_else(|| {
                let all: Vec<&str> = Status::ALL.iter().map(|s| s.name()).collect();
                o.bad(
                    "status",
                    format!(
                        "unknown status {name:?}, expected one of {}",
                        all.join(", ")
                    ),
                )
            })?;
            if s == Status::Ok {
                if !reply_fields(op).is_empty() {
                    return Err(o.bad(
                        "status",
                        format!(
                            "\"ok\" stands only for an empty reply; {} replies with {}, \
                             so write {{\"reply\": {{...}}}}",
                            op.name(),
                            reply_fields(op).join(", ")
                        ),
                    ));
                }
                (Expect::Reply(OwnedReply::Empty), None)
            } else {
                (Expect::Status(s), None)
            }
        }
        (None, None, Some(list)) => {
            if !top {
                return Err(o.bad("one_of", "one_of does not nest"));
            }
            let list = list
                .as_array()
                .ok_or_else(|| o.bad("one_of", "a list of expectations expected"))?;
            let cite = match o.opt("wire") {
                None => {
                    return Err(o.bad(
                        "wire",
                        "one_of needs \"wire\": the docs/wire.md section that allows \
                         more than one answer",
                    ))
                }
                Some(_) => o.str("wire")?,
            };
            let alts = list
                .iter()
                .enumerate()
                .map(|(n, e)| {
                    parse_expect(i, op, e, &format!("{at}one_of[{n}]."), false).map(|(e, _)| e)
                })
                .collect::<R<Vec<_>>>()?;
            (Expect::OneOf(alts), Some(cite))
        }
        _ => {
            return Err(o.bad(
                at.trim_end_matches('.')
                    .rsplit('.')
                    .next()
                    .unwrap_or("expect"),
                "exactly one of \"reply\", \"status\" or \"one_of\" expected",
            ))
        }
    };
    o.done()?;
    Ok(out)
}

fn parse_reply(i: usize, op: Opcode, v: &Value, at: &str) -> R<OwnedReply> {
    let mut o = Obj::new(v, at, Some(i))?;
    let r = match op {
        Opcode::GetAttr => OwnedReply::GetAttr(AttrResp {
            found: o.bool("found")?,
            is_dir: o.bool("is_dir")?,
            size: o.u64("size")?,
            mtime: o.i64("mtime")?,
        }),
        Opcode::ReadDir => {
            let list = o.get("entries")?;
            let list = list
                .as_array()
                .ok_or_else(|| o.bad("entries", "a list of entries expected"))?;
            let entries = list
                .iter()
                .enumerate()
                .map(|(n, e)| {
                    let mut e = Obj::new(e, &format!("{at}entries[{n}]."), Some(i))?;
                    let entry = OwnedDirEntry {
                        name: e.str("name")?,
                        is_dir: e.bool("is_dir")?,
                        size: e.u64("size")?,
                        mtime: e.i64("mtime")?,
                    };
                    e.done()?;
                    Ok(entry)
                })
                .collect::<R<Vec<_>>>()?;
            OwnedReply::ReadDir(entries)
        }
        Opcode::Open => OwnedReply::Open(OpenResp {
            fh: o.u64("fh")?,
            size: o.u64("size")?,
            is_dir: o.bool("is_dir")?,
            immutable: o.bool("immutable")?,
            mount_gen: o.u32("mount_gen")?,
        }),
        Opcode::Read => OwnedReply::Read(o.bytes("data")?),
        Opcode::Write => OwnedReply::Write(o.u32("written")?),
        Opcode::StoredNames => OwnedReply::StoredNames(o.str("names")?),
        Opcode::StageExec => OwnedReply::StageExec(o.str("path")?),
        Opcode::RegLookup => {
            let version = o.u64("version")?;
            let name = o.str("state")?;
            let state = match name.as_str() {
                "absent" => Lookup::Absent,
                "present" => Lookup::Present { created: false },
                "created" => Lookup::Present { created: true },
                "tombstoned" => Lookup::Tombstoned,
                _ => {
                    return Err(o.bad(
                        "state",
                        format!(
                            "unknown state {name:?}, expected absent, present, created \
                             or tombstoned"
                        ),
                    ))
                }
            };
            OwnedReply::RegLookup(LookupReply {
                version,
                state,
                below: o.bool("below")?,
            })
        }
        Opcode::RegKey => {
            let version = o.u64("version")?;
            let node = o.get("node")?;
            let node = if node.is_null() {
                None
            } else {
                Some(parse_node(i, node, &format!("{at}node."))?)
            };
            OwnedReply::RegKey { version, node }
        }
        Opcode::RegSetValue
        | Opcode::RegDeleteValue
        | Opcode::RegCreateKey
        | Opcode::RegDeleteKey
        | Opcode::RegRenameKey => OwnedReply::RegVersion(o.u64("version")?),
        Opcode::RegChanged => OwnedReply::RegChanged(ChangedReply {
            version: o.u64("version")?,
            changed: o.bool("changed")?,
        }),
        Opcode::SetAttr
        | Opcode::Rename
        | Opcode::Delete
        | Opcode::Mkdir
        | Opcode::Close
        | Opcode::Heartbeat
        | Opcode::RenameX => OwnedReply::Empty,
    };
    o.done()?;
    Ok(r)
}

fn parse_node(i: usize, v: &Value, at: &str) -> R<Node> {
    let mut o = Obj::new(v, at, Some(i))?;
    let list = |o: &mut Obj<'_>, name: &str| -> R<Vec<Value>> {
        let v = o.get(name)?;
        v.as_array()
            .cloned()
            .ok_or_else(|| o.bad(name, "a list expected"))
    };
    let created = o.bool("created")?;
    let volatile = o.bool("volatile")?;
    let last_write = o.u64("last_write")?;
    let mut values = Vec::new();
    for (n, v) in list(&mut o, "values")?.iter().enumerate() {
        let mut e = Obj::new(v, &format!("{at}values[{n}]."), Some(i))?;
        values.push(RegValue {
            name: e.str("name")?,
            ty: e.u32("ty")?,
            data: e.bytes("data")?,
        });
        e.done()?;
    }
    let mut value_tombstones = Vec::new();
    for v in list(&mut o, "value_tombstones")? {
        value_tombstones.push(
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| o.bad("value_tombstones", "a list of names expected"))?,
        );
    }
    let mut children = std::collections::BTreeMap::new();
    for (n, v) in list(&mut o, "children")?.iter().enumerate() {
        let mut e = Obj::new(v, &format!("{at}children[{n}]."), Some(i))?;
        let folded = e.str("folded")?;
        let spelling = e.str("spelling")?;
        let state = match e.str("state")?.as_str() {
            "present" => Child::Present,
            "tombstone" => Child::Tombstone,
            s => {
                return Err(e.bad(
                    "state",
                    format!("unknown state {s:?}, expected present or tombstone"),
                ))
            }
        };
        e.done()?;
        children.insert(folded, (spelling, state));
    }
    o.done()?;
    Ok(Node {
        values,
        value_tombstones,
        children,
        created,
        volatile,
        last_write,
    })
}

/// `s` as a scenario file says it, in the format [`load`] reads: what a
/// generated run writes for a case to mount, and for a failure to replay.
/// [`load`] of the result gives `s` back.
pub fn to_json(s: &Scenario) -> Value {
    let steps: Vec<Value> = s.steps.iter().map(step_json).collect();
    serde_json::json!({
        "name": s.name,
        "description": s.description,
        "tree": tree_json(&s.tree),
        "steps": steps,
    })
}

/// [`to_json`], laid out for a person, with a final newline.
pub fn to_json_string(s: &Scenario) -> String {
    let mut out = serde_json::to_string_pretty(&to_json(s)).expect("a JSON value");
    out.push('\n');
    out
}

/// A tree as a scenario file writes it (`contract/scenarios/README.md`).
pub fn tree_json(t: &Tree) -> Value {
    use serde_json::json;
    let roots: Vec<Value> = t
        .roots
        .iter()
        .map(|r| {
            let files: Vec<Value> = r
                .files
                .iter()
                .map(|f| {
                    json!({"path": f.path, "contents": contents_json(&f.contents), "mtime": f.mtime})
                })
                .collect();
            json!({"id": r.id, "files": files, "dirs": r.dirs, "writable": r.writable})
        })
        .collect();
    let mut tree = json!({ "roots": roots });
    if let Some(reg) = &t.registry {
        let keys: Vec<Value> = reg
            .keys
            .iter()
            .map(|k| {
                let values: Vec<Value> = k
                    .values
                    .iter()
                    .map(|v| json!({"name": v.name, "ty": v.ty, "data": contents_json(&v.data)}))
                    .collect();
                json!({"path": k.path, "volatile": k.volatile, "values": values})
            })
            .collect();
        tree["registry"] = json!({ "keys": keys });
    }
    tree
}

fn contents_json(c: &Contents) -> Value {
    match c {
        Contents::Text(s) => Value::String(s.clone()),
        Contents::Hex(b) => serde_json::json!({ "hex": hex(b) }),
        Contents::Fill { size, seed } => serde_json::json!({"size": size, "seed": seed}),
    }
}

/// Bytes as a scenario writes them: a string when they are UTF-8 (the
/// loader reads a string as its UTF-8), else hex.
fn bytes_json(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => serde_json::json!({ "hex": hex(b) }),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The names in `known` whose masks `bits` has.
fn names(bits: u32, known: &[(String, u32)]) -> Vec<String> {
    known
        .iter()
        .filter(|(_, m)| bits & m != 0)
        .map(|(n, _)| n.clone())
        .collect()
}

fn step_json(s: &Step) -> Value {
    let mut v = serde_json::json!({
        "request": request_json(&s.request),
        "expect": expect_json(&s.expect, s.cite.as_deref()),
    });
    if let Some(n) = &s.note {
        v["note"] = Value::String(n.clone());
    }
    if s.client != 0 {
        v["client"] = Value::from(s.client);
    }
    if s.literal_fh {
        if let Some(fh) = v["request"].get("fh").cloned() {
            v["request"]["fh"] = serde_json::json!({ "literal": fh });
        }
    }
    v
}

/// A step's request in the scenario format, as the step wrote it (a
/// literal handle as `{"literal": n}`).
pub fn step_request_json(s: &Step) -> Value {
    step_json(s)["request"].take()
}

/// A request in the scenario format: `op` and its fields.
pub fn request_json(r: &OwnedRequest) -> Value {
    use serde_json::json;
    use OwnedRequest as O;
    let mut v = match r {
        O::GetAttr { root, path }
        | O::ReadDir { root, path }
        | O::Delete { root, path }
        | O::StageExec { root, path } => json!({"root": root.0, "path": path}),
        O::Open { root, flags, path } => {
            json!({"root": root.0, "flags": names(flags.bits(), &open_flag_names()), "path": path})
        }
        O::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => {
            let mut v = json!({"fh": fh, "offset": offset, "len": len});
            if !slot_flags.is_empty() {
                v["slot_flags"] = json!(names(slot_flags.bits(), &request_flag_names()));
            }
            v
        }
        O::Write { fh, offset, data } => {
            json!({"fh": fh, "offset": offset, "data": bytes_json(data)})
        }
        O::SetAttr { fh, size } => json!({"fh": fh, "size": size}),
        O::Rename { root, from, to } => json!({"root": root.0, "from": from, "to": to}),
        O::Mkdir { root, mode, path } => json!({"root": root.0, "mode": mode, "path": path}),
        O::Close { fh } => json!({ "fh": fh }),
        O::Heartbeat => json!({}),
        O::StoredNames { root, skip, path } => {
            json!({"root": root.0, "skip": skip, "path": path})
        }
        O::RegLookup { path } | O::RegKey { path } | O::RegDeleteKey { path } => {
            json!({ "path": path })
        }
        O::RegSetValue {
            path,
            name,
            ty,
            data,
        } => json!({"path": path, "name": name, "ty": ty, "data": bytes_json(data)}),
        O::RegDeleteValue { path, name } => json!({"path": path, "name": name}),
        O::RegCreateKey { path, volatile } => json!({"path": path, "volatile": volatile}),
        O::RegRenameKey { path, new_leaf } => json!({"path": path, "new_leaf": new_leaf}),
        O::RegChanged {
            path,
            subtree,
            since,
        } => json!({"path": path, "subtree": subtree, "since": since}),
        O::RenameX {
            from_root,
            to_root,
            from,
            to,
        } => json!({"from_root": from_root.0, "to_root": to_root.0, "from": from, "to": to}),
    };
    v["op"] = Value::String(r.opcode().name().to_string());
    v
}

/// An expectation in the scenario format; a top-level `one_of` carries
/// `cite` as its `wire`.
pub fn expect_json(e: &Expect, cite: Option<&str>) -> Value {
    use serde_json::json;
    match e {
        Expect::Reply(OwnedReply::Empty) => json!({"status": "ok"}),
        Expect::Reply(r) => json!({ "reply": reply_json(r) }),
        Expect::Status(s) => json!({"status": s.name()}),
        Expect::OneOf(alts) => {
            let alts: Vec<Value> = alts.iter().map(|a| expect_json(a, None)).collect();
            json!({"one_of": alts, "wire": cite.unwrap_or("")})
        }
    }
}

fn reply_json(r: &OwnedReply) -> Value {
    use serde_json::json;
    match r {
        OwnedReply::GetAttr(a) => {
            json!({"found": a.found, "is_dir": a.is_dir, "size": a.size, "mtime": a.mtime})
        }
        OwnedReply::ReadDir(es) => {
            let entries: Vec<Value> = es
                .iter()
                .map(|e| {
                    json!({"name": e.name, "is_dir": e.is_dir, "size": e.size, "mtime": e.mtime})
                })
                .collect();
            json!({ "entries": entries })
        }
        OwnedReply::Open(o) => json!({
            "fh": o.fh, "size": o.size, "is_dir": o.is_dir,
            "immutable": o.immutable, "mount_gen": o.mount_gen
        }),
        OwnedReply::Read(d) => json!({ "data": bytes_json(d) }),
        OwnedReply::ReadBulk { len, arena_offset } => {
            // Never written: a bulk reply is read from the arena first. Said
            // as what it is rather than as data it does not have.
            json!({"bulk_len": len, "arena_offset": arena_offset})
        }
        OwnedReply::Write(n) => json!({ "written": n }),
        OwnedReply::StoredNames(s) => json!({ "names": s }),
        OwnedReply::StageExec(s) => json!({ "path": s }),
        OwnedReply::RegLookup(l) => {
            let state = match l.state {
                Lookup::Absent => "absent",
                Lookup::Present { created: false } => "present",
                Lookup::Present { created: true } => "created",
                Lookup::Tombstoned => "tombstoned",
            };
            json!({"version": l.version, "state": state, "below": l.below})
        }
        OwnedReply::RegKey { version, node } => {
            json!({"version": version, "node": node.as_ref().map(node_json)})
        }
        OwnedReply::RegVersion(v) => json!({ "version": v }),
        OwnedReply::RegChanged(c) => json!({"version": c.version, "changed": c.changed}),
        OwnedReply::Empty => json!({}),
    }
}

fn node_json(n: &Node) -> Value {
    use serde_json::json;
    let values: Vec<Value> = n
        .values
        .iter()
        .map(|v| json!({"name": v.name, "ty": v.ty, "data": bytes_json(&v.data)}))
        .collect();
    let children: Vec<Value> = n
        .children
        .iter()
        .map(|(folded, (spelling, state))| {
            let state = match state {
                Child::Present => "present",
                Child::Tombstone => "tombstone",
            };
            json!({"folded": folded, "spelling": spelling, "state": state})
        })
        .collect();
    json!({
        "created": n.created,
        "volatile": n.volatile,
        "last_write": n.last_write,
        "values": values,
        "value_tombstones": n.value_tombstones,
        "children": children,
    })
}
