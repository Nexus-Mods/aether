//! The tree a scenario mounts, and the reference built from it.
//!
//! A [`Tree`] is plain data: the roots a session serves, the files and
//! directories under each, the prefixes a write may land under, and the
//! registry overlay's starting keys. [`Reference::from_tree`] builds the
//! reference director that serves it; a conformance tool mounts the same
//! tree in the director under test by whatever means that director has. With
//! the `scenario` feature the types deserialise from a scenario file
//! (`contract/scenarios/README.md`):
//!
//! ```json
//! {
//!   "roots": [
//!     {"id": 0,
//!      "files": [{"path": "data/a.esp", "contents": "text", "mtime": 0},
//!                {"path": "data/b.bin", "contents": {"hex": "00ff"}},
//!                {"path": "data/c.bsa", "contents": {"size": 4096, "seed": 1}}],
//!      "dirs": ["data/empty"],
//!      "writable": ["saves"]}
//!   ],
//!   "registry": {"keys": [{"path": "\\Registry\\Machine\\Software\\T",
//!                          "values": [{"name": "v", "ty": 4, "data": {"hex": "01000000"}}]}]}
//! }
//! ```
//!
//! Paths are canonical vpaths (`docs/wire.md` 3.2): `/`-joined components
//! relative to the root, none empty, `.` or `..`, no `\`. A file's ancestors
//! are directories without being listed. A `writable` entry is a directory
//! (or `"."`, the whole root) under which a write open is honoured; it is a
//! directory too. Without `registry` the session has no registry layer and
//! every registry opcode is `Status::NotSupported` (wire.md 4).
//!
//! A root may name `lower` layers, read-only, beneath its writable upper
//! (its own `files` and `dirs`), the nearest first, each `{"files": [...],
//! "dirs": [...]}` with every name below a writable entry:
//!
//! ```json
//! {"id": 0, "writable": ["."],
//!  "files": [{"path": "Data/Saved.esp", "contents": "mine"}],
//!  "lower": [{"files": [{"path": "Data/Mod.esp", "contents": "mod"}]},
//!            {"files": [{"path": "Data/Base.esm", "contents": "base"}],
//!             "dirs": ["Data/Empty"]}]}
//! ```
//!
//! The reference serves such a root as an overlay (`docs/wire.md` 3.4.9,
//! "Names only in a read-only lower").

use aether_rpc::Director;
use std::collections::{BTreeMap, BTreeSet};

use crate::reference::{ReadStyle, Reference, MAX_MOUNTED_LEN};

/// What a scenario mounts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct Tree {
    pub roots: Vec<Root>,
    /// The registry overlay's starting keys; `None` is no registry layer.
    #[cfg_attr(feature = "scenario", serde(default))]
    pub registry: Option<Registry>,
}

/// One managed root and what is under it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct Root {
    pub id: u32,
    #[cfg_attr(feature = "scenario", serde(default))]
    pub files: Vec<File>,
    /// Directories besides the ones a file's path implies.
    #[cfg_attr(feature = "scenario", serde(default))]
    pub dirs: Vec<String>,
    /// Directories (or `"."`) under which a write open is honoured.
    #[cfg_attr(feature = "scenario", serde(default))]
    pub writable: Vec<String>,
    /// Read-only layers beneath the writable upper (the root's own `files`
    /// and `dirs`), the nearest first: the root is served as an overlay
    /// (`docs/wire.md` 3.4.9, "Names only in a read-only lower"). Every
    /// name a layer lists lies below a writable entry.
    #[cfg_attr(feature = "scenario", serde(default))]
    pub lower: Vec<Layer>,
}

/// One read-only layer of a root: files, and directories besides the ones
/// a file's path implies. Where two layers hold a name the nearer one's
/// entry is the one served; a file in a nearer layer, or the upper, hides
/// whatever farther layers hold below its name, and a directory over a
/// file hides the file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct Layer {
    #[cfg_attr(feature = "scenario", serde(default))]
    pub files: Vec<File>,
    #[cfg_attr(feature = "scenario", serde(default))]
    pub dirs: Vec<String>,
}

/// One file: its path, its bytes and its modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct File {
    pub path: String,
    pub contents: Contents,
    /// Seconds since the Unix epoch, for a director that keeps mounted
    /// times. The reference reports no time (0, `docs/wire.md` 3.2), which a
    /// comparison allows (`contract/scenarios/README.md`).
    #[cfg_attr(feature = "scenario", serde(default))]
    pub mtime: i64,
}

/// Bytes as a scenario writes them: UTF-8 text (a JSON string), hex
/// (`{"hex": "00ff"}`) or `size` generated bytes from `seed`
/// (`{"size": 4096, "seed": 1}`, [`fill`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    Text(String),
    Hex(Vec<u8>),
    Fill { size: u64, seed: u64 },
}

impl Contents {
    /// The bytes these contents stand for.
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Contents::Text(s) => s.as_bytes().to_vec(),
            Contents::Hex(b) => b.clone(),
            Contents::Fill { size, seed } => fill(*size as usize, *seed),
        }
    }
}

/// The registry overlay's starting state: keys created in the overlay, each
/// with its values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct Registry {
    #[cfg_attr(feature = "scenario", serde(default))]
    pub keys: Vec<RegKey>,
}

/// A key created in the overlay: a canonical key path (wire.md 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct RegKey {
    pub path: String,
    #[cfg_attr(feature = "scenario", serde(default))]
    pub volatile: bool,
    #[cfg_attr(feature = "scenario", serde(default))]
    pub values: Vec<RegValue>,
}

/// A value: its name, its `REG_*` type number and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "scenario", derive(serde::Deserialize))]
#[cfg_attr(feature = "scenario", serde(deny_unknown_fields))]
pub struct RegValue {
    pub name: String,
    pub ty: u32,
    pub data: Contents,
}

/// `len` bytes generated from `seed`, each position distinguishable as in
/// [`crate::pattern`], which is `fill(len, 0)`. Different seeds give
/// different bytes, so two generated files are told apart by content.
pub fn fill(len: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x9E37_79B9 ^ (seed as u32) ^ ((seed >> 32) as u32);
    for i in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 24) as u8 ^ (i as u8));
    }
    v
}

/// Whether `path` is a canonical vpath (`docs/wire.md` 3.2): `"."` for the
/// root, else `/`-joined components, none empty, `.` or `..`, and no `\`.
/// The only path syntax every director must accept, so the only one a
/// scenario sends.
pub fn is_canonical(path: &str) -> bool {
    path == "."
        || (!path.is_empty()
            && !path.contains('\\')
            && path
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".."))
}

impl Tree {
    /// Checks what a scenario's tree must satisfy: unique root ids,
    /// canonical paths, sizes the reference can hold, no path both a file
    /// and a directory, and nothing the reference cannot express. The
    /// message names the offending entry.
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = BTreeSet::new();
        for r in &self.roots {
            if !ids.insert(r.id) {
                return Err(format!("roots: root {} twice", r.id));
            }
            let mut files = BTreeSet::new();
            for f in &r.files {
                if f.path == "." || !is_canonical(&f.path) {
                    return Err(format!(
                        "root {} file {:?}: not a canonical path",
                        r.id, f.path
                    ));
                }
                if let Contents::Fill { size, .. } = f.contents {
                    if size > MAX_MOUNTED_LEN as u64 {
                        return Err(format!(
                            "root {} file {:?}: larger than {MAX_MOUNTED_LEN} bytes",
                            r.id, f.path
                        ));
                    }
                }
                if !files.insert(aether_casefold::fold(&f.path)) {
                    return Err(format!("root {} file {:?}: twice", r.id, f.path));
                }
            }
            let dirs = r.dirs_folded();
            for f in &r.files {
                if dirs.contains(&aether_casefold::fold(&f.path)) {
                    return Err(format!(
                        "root {} {:?}: both a file and a directory",
                        r.id, f.path
                    ));
                }
            }
            for (what, list) in [("dir", &r.dirs), ("writable", &r.writable)] {
                for d in list {
                    if !is_canonical(d) || (what == "dir" && d == ".") {
                        return Err(format!("root {} {what} {d:?}: not a canonical path", r.id));
                    }
                }
            }
            for (i, layer) in r.lower.iter().enumerate() {
                layer.validate(r, i)?;
            }
        }
        // The reference's one prefix covering a whole root 0 covers every
        // root (`Reference::writable_under("")`), so a tree that makes root 0
        // writable and another root not would be served wrongly.
        let all0 = self
            .roots
            .iter()
            .any(|r| r.id == 0 && r.writable.iter().any(|w| w == "."));
        if all0 {
            if let Some(r) = self
                .roots
                .iter()
                .find(|r| r.id != 0 && !r.writable.iter().any(|w| w == "."))
            {
                return Err(format!(
                    "root {}: root 0 is writable as a whole, so every root must be \
                     (the reference cannot tell them apart)",
                    r.id
                ));
            }
        }
        if let Some(reg) = &self.registry {
            for k in &reg.keys {
                if !k.path.starts_with('\\') {
                    return Err(format!("registry key {:?}: not a key path", k.path));
                }
            }
        }
        Ok(())
    }
}

impl Layer {
    /// What lower layer `i` of `root` must satisfy: canonical paths, sizes
    /// the reference can hold, each name once and of one kind, and every
    /// file and listed directory strictly below one of the root's writable
    /// entries (the entries themselves are the upper's; a read-only provider
    /// beside the upper has no layers).
    fn validate(&self, root: &Root, i: usize) -> Result<(), String> {
        let at = |p: &str| format!("root {} lower {i} {p:?}", root.id);
        let below_writable = |p: &str| {
            let p = aether_casefold::fold(p);
            root.writable
                .iter()
                .any(|w| w == "." || p.starts_with(&format!("{}/", aether_casefold::fold(w))))
        };
        let mut files = BTreeSet::new();
        for f in &self.files {
            if f.path == "." || !is_canonical(&f.path) {
                return Err(format!("{}: not a canonical path", at(&f.path)));
            }
            if let Contents::Fill { size, .. } = f.contents {
                if size > MAX_MOUNTED_LEN as u64 {
                    return Err(format!(
                        "{}: larger than {MAX_MOUNTED_LEN} bytes",
                        at(&f.path)
                    ));
                }
            }
            if !files.insert(aether_casefold::fold(&f.path)) {
                return Err(format!("{}: twice", at(&f.path)));
            }
        }
        for d in &self.dirs {
            if d == "." || !is_canonical(d) {
                return Err(format!("{}: not a canonical path", at(d)));
            }
        }
        for p in self.files.iter().map(|f| &f.path).chain(&self.dirs) {
            if !below_writable(p) {
                return Err(format!(
                    "{}: not below a writable entry (a lower layer lies beneath the \
                     writable upper)",
                    at(p)
                ));
            }
        }
        let dirs = folded(&self.dirs_spelled());
        if let Some(f) = self
            .files
            .iter()
            .find(|f| dirs.contains(&aether_casefold::fold(&f.path)))
        {
            return Err(format!("{}: both a file and a directory", at(&f.path)));
        }
        Ok(())
    }

    /// Every directory of this layer, each once (by fold) with the spelling
    /// the layer gives it: the listed ones, then every file's ancestors as
    /// the file's path spells them. The first spelling of a name wins.
    pub(crate) fn dirs_spelled(&self) -> Vec<String> {
        spelled(self.dirs.iter().cloned(), &self.files)
    }
}

/// `listed`, then every ancestor of `files`, each once by fold, the first
/// spelling winning; never `"."`.
fn spelled(listed: impl Iterator<Item = String>, files: &[File]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let ancestors = files.iter().flat_map(|f| {
        let comps: Vec<&str> = f.path.split('/').collect();
        (1..comps.len()).map(move |n| comps[..n].join("/"))
    });
    for d in listed.chain(ancestors).filter(|d| d != ".") {
        if seen.insert(aether_casefold::fold(&d)) {
            out.push(d);
        }
    }
    out
}

fn folded(dirs: &[String]) -> BTreeSet<String> {
    dirs.iter().map(|d| aether_casefold::fold(d)).collect()
}

impl Root {
    /// Every directory under this root, each once (by fold) with the
    /// spelling the tree gives it: the listed ones, then the writable ones,
    /// then every file's ancestors as the file's path spells them. The first
    /// spelling of a name wins.
    fn dirs_spelled(&self) -> Vec<String> {
        spelled(
            self.dirs.iter().chain(self.writable.iter()).cloned(),
            &self.files,
        )
    }

    /// The same directories, folded.
    fn dirs_folded(&self) -> BTreeSet<String> {
        folded(&self.dirs_spelled())
    }
}

/// The reference's name for `path` under `root`: the path itself on root 0,
/// `@<root>/<path>` on any other (as [`crate::key`], without its fold, which
/// the builder applies).
fn under(root: u32, path: &str) -> String {
    if root == 0 {
        path.to_string()
    } else {
        format!("@{root}/{path}")
    }
}

impl Reference {
    /// The reference serving `tree`: every root it names, each file with its
    /// bytes (answered [`ReadStyle::Whole`]), every directory (listed,
    /// writable, or a file's ancestor), the writable prefixes, and a registry
    /// layer seeded with the tree's keys when it has one.
    ///
    /// The files' `mtime`s are not kept: the reference reports no time.
    /// Seeding the registry moves the overlay's version, as any director's
    /// mount of a layer sets it; how far is the director's business
    /// (`contract/scenarios/README.md`).
    ///
    /// An error, naming the key or value, if the overlay refuses one of
    /// the tree's registry keys or values (a path that is not canonical, a
    /// name or data past wire.md 4.1's limits), or if a key is listed twice
    /// with different `volatile`. [`Tree::validate`] checks the rest.
    pub fn from_tree(tree: &Tree) -> Result<Reference, String> {
        let mut d = Reference::new();
        for r in &tree.roots {
            d = d.with_root(r.id);
            for dir in r.dirs_spelled() {
                d = d.with_dir(&under(r.id, &dir));
            }
            for f in &r.files {
                d = d.with(&under(r.id, &f.path), f.contents.bytes(), ReadStyle::Whole);
            }
            for (i, layer) in r.lower.iter().enumerate() {
                for dir in layer.dirs_spelled() {
                    d = d.with_layer_dir(i, &under(r.id, &dir));
                }
                for f in &layer.files {
                    d = d.with_layer_file(i, &under(r.id, &f.path), f.contents.bytes());
                }
            }
            for w in &r.writable {
                let prefix = if w == "." {
                    under(r.id, "")
                } else {
                    under(r.id, &format!("{w}/"))
                };
                d = d.writable_under(&prefix);
            }
        }
        if let Some(reg) = &tree.registry {
            d = d.with_registry();
            let mut keys: Vec<&RegKey> = reg.keys.iter().collect();
            // Parents first, so a parent's own flags are the tree's.
            keys.sort_by_key(|k| k.path.split('\\').count());
            let mut seen: BTreeMap<String, (&str, bool)> = BTreeMap::new();
            for k in keys {
                let folded = aether_casefold::fold(&k.path);
                if let Some(&(first, volatile)) = seen.get(&folded) {
                    if volatile != k.volatile {
                        return Err(format!(
                            "registry key {:?}: listed again as {first:?} with a different \
                             volatile",
                            k.path
                        ));
                    }
                } else {
                    seen.insert(folded, (&k.path, k.volatile));
                    d.reg_create_key(&k.path, k.volatile).map_err(|st| {
                        format!("registry key {:?}: refused, {}", k.path, st.name())
                    })?;
                }
                for v in &k.values {
                    d.reg_set_value(&k.path, &v.name, v.ty, &v.data.bytes())
                        .map_err(|st| {
                            format!(
                                "registry value {:?} of {:?}: refused, {}",
                                v.name,
                                k.path,
                                st.name()
                            )
                        })?;
                }
            }
        }
        Ok(d)
    }
}

#[cfg(feature = "scenario")]
mod de {
    use super::{Contents, MAX_MOUNTED_LEN};
    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use std::fmt;

    impl<'de> serde::Deserialize<'de> for Contents {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Contents, D::Error> {
            d.deserialize_any(ContentsVisitor)
        }
    }

    struct ContentsVisitor;

    impl<'de> Visitor<'de> for ContentsVisitor {
        type Value = Contents;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(r#"a string, {"hex": "..."} or {"size": N, "seed": S}"#)
        }

        fn visit_str<E: de::Error>(self, s: &str) -> Result<Contents, E> {
            Ok(Contents::Text(s.to_string()))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Contents, A::Error> {
            let (mut hex, mut size, mut seed) = (None::<String>, None::<u64>, None::<u64>);
            while let Some(k) = m.next_key::<String>()? {
                match k.as_str() {
                    "hex" => hex = Some(m.next_value()?),
                    "size" => size = Some(m.next_value()?),
                    "seed" => seed = Some(m.next_value()?),
                    other => return Err(de::Error::unknown_field(other, &["hex", "size", "seed"])),
                }
            }
            match (hex, size, seed) {
                (Some(h), None, None) => decode_hex(&h)
                    .map(Contents::Hex)
                    .map_err(|e| de::Error::custom(format!("hex {h:?}: {e}"))),
                (None, Some(size), seed) => {
                    if size > MAX_MOUNTED_LEN as u64 {
                        return Err(de::Error::custom(format!(
                            "size {size}: larger than {MAX_MOUNTED_LEN}"
                        )));
                    }
                    Ok(Contents::Fill {
                        size,
                        seed: seed.unwrap_or(0),
                    })
                }
                _ => Err(de::Error::custom(
                    r#"contents are {"hex": "..."} or {"size": N, "seed": S}"#,
                )),
            }
        }
    }

    fn decode_hex(s: &str) -> Result<Vec<u8>, &'static str> {
        let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if !digits.len().is_multiple_of(2) {
            return Err("an odd number of digits");
        }
        digits
            .chunks(2)
            .map(|p| {
                let d = |c: u8| (c as char).to_digit(16).ok_or("not a hex digit");
                Ok((d(p[0])? * 16 + d(p[1])?) as u8)
            })
            .collect()
    }
}
