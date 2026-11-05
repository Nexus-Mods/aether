//! The comparison rules: when a director's answer to a step is the answer
//! expected (the scenario's, or the reference's).
//!
//! The rule is equality, except where `docs/wire.md` lets directors differ.
//! Each exception below names the wire.md section and quotes the text that
//! allows it, and `contract/scenarios/README.md` lists them all:
//!
//! - **Handles** (wire.md 3.4.3: "`fh` is any u64 the director chooses,
//!   unique among open handles"). An expected fh and the director's are
//!   paired at the OPEN that issues them, and the pairing must stay one to
//!   one while the handle is open; a successful CLOSE ends it, so the
//!   director may issue the number again. Requests are sent with the
//!   director's numbers ([`Session::translate`]): an open handle's pairing;
//!   after its CLOSE, the number the director closed (so a use-after-close
//!   tests that handle), unless the director has issued it again; and for a
//!   number never paired, or a closed one reissued, a stand-in no open handle
//!   of the director has, counted down from `u64::MAX` and the same for one
//!   number while it fits.
//! - **`mount_gen`** (wire.md 3.4.3: "a generation the director bumps
//!   whenever a root's content is replaced", with the directors' differing
//!   starting values). Not compared.
//! - **`immutable`** (wire.md 3.4.3: "Never set it for a write open or a
//!   directory"; otherwise it only lets the shim cache). Not compared, except
//!   that it must be clear on a write open and on a directory.
//! - **Listing order** (wire.md 3.4.2: "A director may send the entries in
//!   any order."). Listings compare as multisets of entries.
//! - **`mtime`** (wire.md 3.2, Timestamps: "Which time a director reports for
//!   a path, if any, is the director's to choose"). Not compared, in GETATTR
//!   or READDIR, and not checked for "changed" or "unchanged" after a write
//!   either.
//! - **READ transport** (wire.md 3.4.4: "it must accept either whatever flag
//!   it sent"). Inline and bulk replies compare by their data; a bulk reply
//!   must be resolved from the arena first ([`OwnedReply::resolve_bulk`]).
//! - **STAGE_EXEC's path** (wire.md 3.4.14: "the real, on-disk Windows path
//!   to start it from", which is wherever the director staged it). Not
//!   compared.
//! - **Registry versions** (wire.md 4.3: "The number a director's overlay
//!   starts from when it mounts a layer is the director's", so versions may
//!   differ "by a constant", and "a REG_CHANGED `since` is in the numbering
//!   of the director it is sent to"). Compared up to one offset per session,
//!   fixed by the first registry reply; a REG_CHANGED `since` is sent shifted
//!   by it, except `u64::MAX`, the shim's probe for the current version.
//! - **`last_write`** of a REG_KEY node (wire.md 4.3: a FILETIME the overlay
//!   stamps with the director's clock). Not compared.
//!
//! Some fields the rules do not compare must still be well formed, whatever
//! the expectation: an OPEN reply's `fh` is never 0 (wire.md 3.4.3: a client
//! may send `CLOSE(0)` for a directory it made with MKDIR), and an `mtime` is
//! below 2^40,
//! as a time in seconds is (3.2, Timestamps). The runner checks the raw
//! reply's shape (`crate::shape`: sizes, zero bytes, an OPEN reply's reserved
//! bits, REG_KEY's child order) and a STAGE_EXEC path (`run.rs`,
//! `stage_check`).
//!
//! Besides comparing, a session checks one rule across steps, which no single
//! answer shows:
//!
//! - **Read-cache coherence** (wire.md 3.4.3: "The cache keys a file's blocks
//!   by (root, folded path, size, `mount_gen`), so two immutable opens of one
//!   path with equal size and `mount_gen` must have identical content", and
//!   "the handle's bytes cannot change while it is open"). An OPEN whose
//!   reply sets `immutable` keys its handle by (root, path folded as wire.md
//!   3.2 folds it, the reply's `size` and `mount_gen`). Each successful READ
//!   through such a handle (a bulk one once read from the arena) records its
//!   bytes under that key, its offset and its length, for the rest of the
//!   session, as the shim's cache outlives the handles that filled it; a READ
//!   of the same key, offset and length through any immutable handle that
//!   returns other bytes fails, since the shim's cache would have served the
//!   old ones. READs through other handles are not recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use aether_casefold::fold;
use aether_protocol::{OpenFlags, Status};

use crate::scenario::{Expect, OwnedReply, OwnedRequest};

/// The comparison state across one scenario's steps (or one generated
/// sequence's): the handle pairing and the registry version offset. It owns
/// all of it, so steps can be made one at a time.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// Expected fh to the director's, for the open handles.
    fh: BTreeMap<u64, u64>,
    /// The director's fh to the expected one.
    fh_back: BTreeMap<u64, u64>,
    /// Expected fh to the director's number for it when it was closed.
    released: BTreeMap<u64, u64>,
    /// Expected fh to the stand-in sent for it when no live number fits.
    stand_in: BTreeMap<u64, u64>,
    /// The director's registry version minus the expected one.
    reg_offset: Option<i128>,
    /// Expected fh to its read-cache key, for the open handles whose OPEN
    /// reply set `immutable`.
    immutable: BTreeMap<u64, CacheKey>,
    /// What READs through immutable handles returned, by key, offset and
    /// length: the shim's read cache, which outlives the handles.
    cached: BTreeMap<(CacheKey, u64, u32), Vec<u8>>,
    /// Never send a closed handle's old number ([`Session::among_others`]).
    no_reuse: bool,
    /// For each label a step's `one_of` shares with others, the
    /// alternatives (by position) every step of the label has allowed so far.
    chosen: BTreeMap<String, BTreeSet<usize>>,
}

/// What the shim's read cache keys a file's blocks by (wire.md 3.4.3): the
/// root, the folded path, and the OPEN reply's `size` and `mount_gen`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    root: u32,
    path: String,
    size: u64,
    mount_gen: u32,
}

/// A director's answer that is not the one expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub step: usize,
    /// Which rule failed.
    pub why: String,
    pub expected: String,
    pub got: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "step {}: {}\n  expected: {}\n  got:      {}",
            self.step, self.why, self.expected, self.got
        )
    }
}

impl std::error::Error for Mismatch {}

impl Session {
    /// A session with nothing paired yet.
    pub fn new() -> Session {
        Session::default()
    }

    /// A session for one of several clients sending at once, each pairing
    /// its own handles: a closed handle, or a number never paired, is always
    /// sent as a stand-in, never as the number the director closed, since
    /// the director may already have issued that number to another client
    /// (wire.md 2.8.1: handles are ring-global), and this session cannot
    /// know.
    pub fn among_others() -> Session {
        Session {
            no_reuse: true,
            ..Session::default()
        }
    }

    /// `req` as the director should get it: an expected handle replaced as
    /// the module doc says (paired, closed, or a stand-in), and a
    /// REG_CHANGED `since` shifted by the registry version offset. Mutable
    /// because a stand-in, once chosen, is kept.
    pub fn translate(&mut self, req: &OwnedRequest) -> OwnedRequest {
        let mut r = req.clone();
        match &mut r {
            OwnedRequest::Read { fh: n, .. }
            | OwnedRequest::Write { fh: n, .. }
            | OwnedRequest::SetAttr { fh: n, .. }
            | OwnedRequest::Close { fh: n } => *n = self.director_fh(*n),
            OwnedRequest::RegChanged { since, .. } => {
                // `u64::MAX` is the probe the shim sends to learn the current
                // version (`crates/aether-hooks/src/regnotify.rs`): "after
                // every version", in any numbering, so it is not shifted.
                if let (Some(off), false) = (self.reg_offset, *since == u64::MAX) {
                    *since = (i128::from(*since) + off).clamp(0, i128::from(u64::MAX)) as u64;
                }
            }
            _ => {}
        }
        r
    }

    /// The director's number for expected handle `n`.
    fn director_fh(&mut self, n: u64) -> u64 {
        if let Some(&d) = self.fh.get(&n) {
            return d;
        }
        let reuse = if self.no_reuse {
            &[&self.stand_in][..]
        } else {
            &[&self.released, &self.stand_in][..]
        };
        for m in reuse {
            if let Some(&d) = m.get(&n) {
                if !self.fh_back.contains_key(&d) {
                    return d;
                }
            }
        }
        let d = (0..=u64::MAX)
            .rev()
            .find(|d| {
                !self.fh_back.contains_key(d)
                    && !self.released.values().any(|x| x == d)
                    && !self.stand_in.values().any(|x| x == d)
            })
            .expect("fewer than 2^64 handles");
        self.stand_in.insert(n, d);
        d
    }

    /// Whether `got`, the director's answer to step `step` (request `req`
    /// as the scenario wrote it, before [`Session::translate`]), is
    /// `expected` under the rules above. On success the session moves on (a
    /// handle paired or released, the registry offset fixed); on a mismatch
    /// it is unchanged.
    pub fn check(
        &mut self,
        step: usize,
        req: &OwnedRequest,
        expected: &Expect,
        got: &Result<OwnedReply, Status>,
    ) -> Result<(), Mismatch> {
        self.check_chosen(step, req, expected, got, None)
    }

    /// [`Session::check`] for a step whose `one_of` carries a `choice`
    /// label (`contract/scenarios/README.md`, "Expectations"). The steps
    /// of one label have lists of equal length, and take the alternative of
    /// one position all together: an answer that fits several alternatives
    /// leaves the others open for the steps after it, one that fits none
    /// that every earlier step of the label allows fails. It is how a
    /// scenario shows that a change happened whole or not at all where
    /// wire.md allows either.
    pub fn check_chosen(
        &mut self,
        step: usize,
        req: &OwnedRequest,
        expected: &Expect,
        got: &Result<OwnedReply, Status>,
        choice: Option<&str>,
    ) -> Result<(), Mismatch> {
        let (Some(label), Expect::OneOf(alts)) = (choice, expected) else {
            let mut next = self.clone();
            next.check_one(step, req, expected, got)?;
            *self = next;
            return Ok(());
        };
        let open: BTreeSet<usize> = (0..alts.len()).collect();
        let allowed = self.chosen.get(label).unwrap_or(&open).clone();
        let mut next = None;
        let mut matched = BTreeSet::new();
        for (n, alt) in alts.iter().enumerate().filter(|(n, _)| allowed.contains(n)) {
            let mut trial = self.clone();
            if trial.check_one(step, req, alt, got).is_ok() {
                matched.insert(n);
                next.get_or_insert(trial);
            }
        }
        let Some(mut next) = next else {
            // An answer no alternative takes fails as it would without a
            // label; one the label's earlier steps rule out says so.
            self.clone().check_one(step, req, expected, got)?;
            return Err(Mismatch {
                step,
                why: format!(
                    "none of the answers that agree with the earlier steps of choice {label:?}"
                ),
                expected: render_expect(expected),
                got: render_got(got),
            });
        };
        next.chosen.insert(label.to_string(), matched);
        *self = next;
        Ok(())
    }

    fn check_one(
        &mut self,
        step: usize,
        req: &OwnedRequest,
        expected: &Expect,
        got: &Result<OwnedReply, Status>,
    ) -> Result<(), Mismatch> {
        let miss = |why: String| Mismatch {
            step,
            why,
            expected: render_expect(expected),
            got: render_got(got),
        };
        match (expected, got) {
            (Expect::OneOf(alts), _) => {
                // A reply that breaks a rule whichever answer is expected
                // (mtime past 2^40 seconds) says so, not "none of them".
                let mut rule = None;
                for alt in alts {
                    let mut trial = self.clone();
                    match trial.check_one(step, req, alt, got) {
                        Ok(()) => {
                            *self = trial;
                            return Ok(());
                        }
                        Err(m) if !m.why.contains("different") => {
                            rule.get_or_insert(m.why);
                        }
                        Err(_) => {}
                    }
                }
                Err(miss(
                    rule.unwrap_or_else(|| "none of the allowed answers".into()),
                ))
            }
            (Expect::Status(e), Err(g)) if e == g => Ok(()),
            (Expect::Status(_), _) | (Expect::Reply(_), Err(_)) => {
                Err(miss("a different status".into()))
            }
            (Expect::Reply(e), Ok(g)) => self.reply(req, e, g).map_err(miss),
        }
    }

    fn reply(&mut self, req: &OwnedRequest, e: &OwnedReply, g: &OwnedReply) -> Result<(), String> {
        use OwnedReply as O;
        match (e, g) {
            (O::Open(e), O::Open(g)) => {
                let write = matches!(
                    req,
                    OwnedRequest::Open { flags, .. } if flags.contains(OpenFlags::WRITE)
                );
                if g.immutable && (write || g.is_dir) {
                    return Err(
                        "immutable set on a write open or a directory (wire.md 3.4.3)".into(),
                    );
                }
                if (e.size, e.is_dir) != (g.size, g.is_dir) {
                    return Err("a different size or is_dir".into());
                }
                if g.fh == 0 {
                    return Err(
                        "fh 0, which a director never issues (wire.md 3.4.3: a client may \
                         send CLOSE(0) for a directory it made with MKDIR)"
                            .into(),
                    );
                }
                self.pair(e.fh, g.fh)?;
                match req {
                    OwnedRequest::Open { root, path, .. } if g.immutable => {
                        self.immutable.insert(
                            e.fh,
                            CacheKey {
                                root: root.0,
                                path: fold(path),
                                size: g.size,
                                mount_gen: g.mount_gen,
                            },
                        );
                    }
                    _ => {
                        self.immutable.remove(&e.fh);
                    }
                }
                Ok(())
            }
            (O::Empty, O::Empty) => {
                if let OwnedRequest::Close { fh } = req {
                    if let Some(d) = self.fh.remove(fh) {
                        self.fh_back.remove(&d);
                        self.released.insert(*fh, d);
                    }
                    self.immutable.remove(fh);
                }
                Ok(())
            }
            (O::Read(e), O::Read(g)) => {
                if e != g {
                    return Err("a different reply".into());
                }
                match req {
                    OwnedRequest::Read {
                        fh, offset, len, ..
                    } => self.cache_read(*fh, *offset, *len, g),
                    _ => Ok(()),
                }
            }
            (O::GetAttr(e), O::GetAttr(g)) => {
                seconds(g.mtime)?;
                if (e.found, e.is_dir, e.size) == (g.found, g.is_dir, g.size) {
                    Ok(())
                } else {
                    Err("different attributes (mtime aside)".into())
                }
            }
            (O::ReadDir(e), O::ReadDir(g)) => {
                for x in g {
                    seconds(x.mtime).map_err(|why| format!("{:?}: {why}", x.name))?;
                }
                let set = |v: &[crate::scenario::OwnedDirEntry]| {
                    let mut v: Vec<_> = v
                        .iter()
                        .map(|x| (x.name.clone(), x.is_dir, x.size))
                        .collect();
                    v.sort();
                    v
                };
                if set(e) == set(g) {
                    Ok(())
                } else {
                    Err("a different listing (order and mtime aside)".into())
                }
            }
            (_, O::ReadBulk { .. }) | (O::ReadBulk { .. }, _) => {
                Err("a bulk READ reply not resolved from the arena".into())
            }
            (O::StageExec(_), O::StageExec(_)) => Ok(()),
            (O::RegLookup(e), O::RegLookup(g)) => {
                self.version(e.version, g.version)?;
                if (e.state, e.below) == (g.state, g.below) {
                    Ok(())
                } else {
                    Err("a different lookup".into())
                }
            }
            (
                O::RegKey {
                    version: ev,
                    node: en,
                },
                O::RegKey {
                    version: gv,
                    node: gn,
                },
            ) => {
                self.version(*ev, *gv)?;
                let strip = |n: &Option<aether_registry::Node>| {
                    n.clone().map(|mut n| {
                        n.last_write = 0;
                        n
                    })
                };
                if strip(en) == strip(gn) {
                    Ok(())
                } else {
                    Err("a different key node (last_write aside)".into())
                }
            }
            (O::RegVersion(e), O::RegVersion(g)) => self.version(*e, *g),
            (O::RegChanged(e), O::RegChanged(g)) => {
                self.version(e.version, g.version)?;
                if e.changed == g.changed {
                    Ok(())
                } else {
                    Err("a different changed".into())
                }
            }
            (e, g) if e == g => Ok(()),
            _ => Err("a different reply".into()),
        }
    }

    /// Pairs expected handle `e` with the director's `g`, one to one.
    fn pair(&mut self, e: u64, g: u64) -> Result<(), String> {
        match (self.fh.get(&e), self.fh_back.get(&g)) {
            (None, None) => {
                self.released.remove(&e);
                self.stand_in.remove(&e);
                self.fh.insert(e, g);
                self.fh_back.insert(g, e);
                Ok(())
            }
            (Some(&d), Some(&x)) if d == g && x == e => Ok(()),
            (Some(&d), _) if d != g => Err(format!(
                "handle {e} is already the director's {d}, not {g} (wire.md 3.4.3)"
            )),
            (_, Some(&x)) => Err(format!(
                "the director's handle {g} is already open as {x}, so it cannot also be {e} \
                 (wire.md 3.4.3: unique among open handles)"
            )),
            _ => unreachable!("covered above"),
        }
    }

    /// Records `data`, read through expected handle `fh` at `offset` for
    /// `len` bytes, if the handle is immutable; other bytes recorded under
    /// the same key fail (the module doc's read-cache coherence).
    fn cache_read(&mut self, fh: u64, offset: u64, len: u32, data: &[u8]) -> Result<(), String> {
        let Some(k) = self.immutable.get(&fh) else {
            return Ok(());
        };
        let at = (k.clone(), offset, len);
        match self.cached.get(&at) {
            Some(before) if before.as_slice() != data => Err(format!(
                "{} bytes at {offset} of root {} {:?} (size {}, mount_gen {}) differ from \
                 what an earlier READ of an immutable handle with the same key returned \
                 ({}), so the shim's read cache would serve stale bytes (wire.md 3.4.3: two \
                 immutable opens of one path with equal size and mount_gen must have \
                 identical content)",
                data.len(),
                k.root,
                k.path,
                k.size,
                k.mount_gen,
                render_reply(&OwnedReply::Read(before.clone())),
            )),
            Some(_) => Ok(()),
            None => {
                self.cached.insert(at, data.to_vec());
                Ok(())
            }
        }
    }

    /// Registry versions, up to the session's one offset.
    fn version(&mut self, e: u64, g: u64) -> Result<(), String> {
        let off = i128::from(g) - i128::from(e);
        match self.reg_offset {
            None => {
                self.reg_offset = Some(off);
                Ok(())
            }
            Some(o) if o == off => Ok(()),
            Some(o) => Err(format!(
                "registry version {g} is {e} shifted by {off}, but earlier replies were \
                 shifted by {o} (wire.md 4.3)"
            )),
        }
    }
}

/// The latest `mtime` taken as seconds: 2^40 seconds is the year 36812, and
/// a time in milliseconds is past it from 1970-02-13 on.
const MTIME_LIMIT: i64 = 1 << 40;

/// Why `mtime` is not a time in seconds, if it is not (wire.md 3.2,
/// Timestamps: "`mtime` is seconds since the Unix epoch, signed"; the shim
/// turns it into a FILETIME as seconds, so milliseconds give dates
/// thousands of years away).
fn seconds(mtime: i64) -> Result<(), String> {
    if mtime < MTIME_LIMIT {
        return Ok(());
    }
    Err(format!(
        "mtime {mtime} is past 2^40 seconds (the year 36812): not seconds since the Unix \
         epoch, milliseconds perhaps (wire.md 3.2, Timestamps: \"`mtime` is seconds since the \
         Unix epoch, signed\")"
    ))
}

/// Why an OPEN reply's payload is not one wire.md 3.4.3 allows beyond what
/// its decoder reads, if it is not: byte 17's bits 1 to 7 and bytes 18 and
/// 19 are zero ("other bits zero", "zero"; section 7: a newer field is read
/// only where "an old director writes zero").
pub fn open_reply_reserved(payload: &[u8]) -> Option<String> {
    let flags = *payload.get(17)?;
    let pad = payload.get(18..20)?;
    (flags & !1 != 0 || pad != [0, 0]).then(|| {
        format!(
            "an OPEN reply whose flags byte is {flags:#04x} and bytes 18 and 19 are {:02x}{:02x}: \
             only bit 0 (immutable) may be set, and bytes 18-19 are zero (wire.md 3.4.3; 7: a \
             later field is read only where an old director writes zero)",
            pad[0], pad[1]
        )
    })
}

/// `e` as a person reads it, as a mismatch and a report show it.
pub fn render_expect(e: &Expect) -> String {
    match e {
        Expect::Reply(r) => render_reply(r),
        Expect::Status(s) => format!("status {}", s.name()),
        Expect::OneOf(alts) => {
            let v: Vec<String> = alts.iter().map(render_expect).collect();
            format!("one of [{}]", v.join(" | "))
        }
    }
}

/// A director's answer as a person reads it, as a mismatch and a report
/// show it.
pub fn render_got(g: &Result<OwnedReply, Status>) -> String {
    match g {
        Ok(r) => render_reply(r),
        Err(s) => format!("status {}", s.name()),
    }
}

/// A reply as a person reads it: READ data as text when it is short UTF-8,
/// else its length and leading bytes in hex.
fn render_reply(r: &OwnedReply) -> String {
    match r {
        OwnedReply::Read(data) => match std::str::from_utf8(data) {
            Ok(s) if data.len() <= 64 => format!("Read({s:?})"),
            _ => {
                let head: String = data.iter().take(32).map(|b| format!("{b:02x}")).collect();
                let more = if data.len() > 32 { ".." } else { "" };
                format!("Read({} bytes: {head}{more})", data.len())
            }
        },
        r => format!("{r:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::{OpenResp, RequestFlags, RootId};

    fn open(path: &str) -> OwnedRequest {
        OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: path.into(),
        }
    }

    fn read(fh: u64) -> OwnedRequest {
        OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh,
            offset: 0,
            len: 16,
        }
    }

    fn close(fh: u64) -> OwnedRequest {
        OwnedRequest::Close { fh }
    }

    fn opened(fh: u64, size: u64, immutable: bool, mount_gen: u32) -> OwnedReply {
        OwnedReply::Open(OpenResp {
            fh,
            size,
            immutable,
            mount_gen,
            ..Default::default()
        })
    }

    /// Plays open, read and close of `path` as expected handle `fh`, the
    /// director answering with `got` (an OPEN reply) and `data`. The READ's
    /// expectation is `data` itself, so only the cache rule can fail it.
    fn open_read_close(
        sess: &mut Session,
        path: &str,
        fh: u64,
        got: OwnedReply,
        data: &[u8],
    ) -> Result<(), Mismatch> {
        let OwnedReply::Open(o) = &got else {
            panic!("an OPEN reply");
        };
        let expect_open = Expect::Reply(opened(fh, o.size, false, 0));
        sess.check(0, &open(path), &expect_open, &Ok(got))?;
        let bytes = OwnedReply::Read(data.to_vec());
        sess.check(1, &read(fh), &Expect::Reply(bytes.clone()), &Ok(bytes))?;
        sess.check(
            2,
            &close(fh),
            &Expect::Reply(OwnedReply::Empty),
            &Ok(OwnedReply::Empty),
        )
    }

    fn getattr(path: &str) -> OwnedRequest {
        OwnedRequest::GetAttr {
            root: RootId(0),
            path: path.into(),
        }
    }

    fn found(size: u64) -> OwnedReply {
        OwnedReply::GetAttr(aether_protocol::AttrResp {
            found: true,
            is_dir: false,
            size,
            mtime: 0,
        })
    }

    fn missing() -> OwnedReply {
        OwnedReply::GetAttr(aether_protocol::AttrResp {
            found: false,
            is_dir: false,
            size: 0,
            mtime: 0,
        })
    }

    /// Two alternatives, the first "moved" and the second "stayed".
    fn either(moved: OwnedReply, stayed: OwnedReply) -> Expect {
        Expect::OneOf(vec![Expect::Reply(moved), Expect::Reply(stayed)])
    }

    /// Steps that share a choice take the same alternative: a director that
    /// answers the first step as alternative 0 and the second as alternative
    /// 1 fails the second, though each answer is one the step allows.
    #[test]
    fn steps_sharing_a_choice_must_take_the_same_alternative() {
        let at_to = either(found(1), missing());
        let at_from = either(missing(), found(1));
        let mut sess = Session::new();
        sess.check_chosen(0, &getattr("to"), &at_to, &Ok(found(1)), Some("moved"))
            .unwrap();
        // Alternative 1 of the second step is not the first step's choice.
        let e = sess
            .check_chosen(1, &getattr("from"), &at_from, &Ok(found(1)), Some("moved"))
            .unwrap_err();
        assert_eq!(e.step, 1);
        assert!(e.why.contains("moved"), "{}", e.why);
        sess.check_chosen(1, &getattr("from"), &at_from, &Ok(missing()), Some("moved"))
            .unwrap();
    }

    /// Without a label, or under different labels, steps choose freely, and
    /// an answer that fits several alternatives leaves the choice open.
    #[test]
    fn labels_are_independent_and_a_shared_answer_leaves_the_choice_open() {
        let same = either(found(1), found(1));
        let mut sess = Session::new();
        sess.check_chosen(0, &getattr("a"), &same, &Ok(found(1)), Some("x"))
            .unwrap();
        sess.check_chosen(
            1,
            &getattr("b"),
            &either(found(2), missing()),
            &Ok(missing()),
            Some("x"),
        )
        .unwrap();
        sess.check_chosen(
            2,
            &getattr("c"),
            &either(found(3), missing()),
            &Ok(found(3)),
            Some("y"),
        )
        .unwrap();
        sess.check_chosen(
            3,
            &getattr("d"),
            &either(found(3), missing()),
            &Ok(found(3)),
            None,
        )
        .unwrap();
    }

    #[test]
    fn two_immutable_opens_with_one_key_and_different_bytes_fail_citing_wire_3_4_3() {
        let mut sess = Session::new();
        open_read_close(&mut sess, "Data/A.esp", 1, opened(7, 3, true, 2), b"abc").unwrap();
        let m =
            open_read_close(&mut sess, "data/a.esp", 2, opened(8, 3, true, 2), b"xyz").unwrap_err();
        assert_eq!(m.step, 1, "{m}");
        assert!(m.why.contains("wire.md 3.4.3"), "{m}");
        assert!(m.why.contains("stale bytes"), "{m}");
    }

    #[test]
    fn two_reads_of_one_immutable_handle_that_differ_fail() {
        let mut sess = Session::new();
        sess.check(
            0,
            &open("a"),
            &Expect::Reply(opened(1, 3, false, 0)),
            &Ok(opened(9, 3, true, 0)),
        )
        .unwrap();
        let abc = OwnedReply::Read(b"abc".to_vec());
        let abd = OwnedReply::Read(b"abd".to_vec());
        sess.check(1, &read(1), &Expect::Reply(abc.clone()), &Ok(abc))
            .unwrap();
        assert!(sess
            .check(2, &read(1), &Expect::Reply(abd.clone()), &Ok(abd))
            .is_err());
    }

    #[test]
    fn a_different_mount_gen_or_size_is_another_file_to_the_cache() {
        let mut sess = Session::new();
        open_read_close(&mut sess, "a", 1, opened(7, 3, true, 2), b"abc").unwrap();
        open_read_close(&mut sess, "a", 2, opened(8, 3, true, 3), b"xyz").unwrap();
        open_read_close(&mut sess, "a", 3, opened(9, 4, true, 2), b"wxyz").unwrap();
        // Another root is another file too.
        let mut sess2 = Session::new();
        open_read_close(&mut sess2, "a", 1, opened(7, 3, true, 2), b"abc").unwrap();
        let other_root = OwnedRequest::Open {
            root: RootId(1),
            flags: OpenFlags::READ,
            path: "a".into(),
        };
        sess2
            .check(
                0,
                &other_root,
                &Expect::Reply(opened(2, 3, false, 0)),
                &Ok(opened(8, 3, true, 2)),
            )
            .unwrap();
        let xyz = OwnedReply::Read(b"xyz".to_vec());
        sess2
            .check(1, &read(2), &Expect::Reply(xyz.clone()), &Ok(xyz))
            .unwrap();
    }

    #[test]
    fn reads_through_handles_not_marked_immutable_are_not_recorded() {
        let mut sess = Session::new();
        open_read_close(&mut sess, "a", 1, opened(7, 3, false, 0), b"abc").unwrap();
        open_read_close(&mut sess, "a", 2, opened(8, 3, true, 0), b"xyz").unwrap();
        open_read_close(&mut sess, "a", 3, opened(9, 3, false, 0), b"123").unwrap();
        assert_eq!(sess.cached.len(), 1, "{:?}", sess.cached);
    }

    #[test]
    fn a_handle_reissued_mutable_is_no_longer_immutable() {
        let mut sess = Session::new();
        open_read_close(&mut sess, "a", 1, opened(7, 3, true, 0), b"abc").unwrap();
        // The same expected fh, now for a mutable open of the same path.
        open_read_close(&mut sess, "a", 1, opened(7, 3, false, 0), b"xyz").unwrap();
    }

    fn changed(since: u64) -> OwnedRequest {
        OwnedRequest::RegChanged {
            path: r"\Registry\Machine\Software\T".into(),
            subtree: true,
            since,
        }
    }

    /// wire.md 4.3: `since` is in the numbering of the director it is sent
    /// to, so it is shifted by the session's offset; but `u64::MAX` is the
    /// probe the shim sends to learn the current version
    /// (`regnotify.rs`), in every numbering, and goes as it is.
    #[test]
    fn fh_0_is_never_a_handle() {
        let mut sess = Session::new();
        let m = sess
            .check(
                0,
                &open("a"),
                &Expect::Reply(opened(1, 3, false, 0)),
                &Ok(opened(0, 3, false, 0)),
            )
            .unwrap_err();
        assert!(m.why.contains("wire.md 3.4.3"), "{m}");
    }

    #[test]
    fn an_mtime_in_milliseconds_fails() {
        let attr = |mtime| {
            OwnedReply::GetAttr(aether_protocol::AttrResp {
                found: true,
                is_dir: false,
                size: 1,
                mtime,
            })
        };
        let get = OwnedRequest::GetAttr {
            root: RootId(0),
            path: "a".into(),
        };
        let mut sess = Session::new();
        for ok in [0, -5, 1_577_836_800, MTIME_LIMIT - 1] {
            sess.check(0, &get, &Expect::Reply(attr(0)), &Ok(attr(ok)))
                .unwrap();
        }
        let m = sess
            .check(
                0,
                &get,
                &Expect::Reply(attr(0)),
                &Ok(attr(1_577_836_800_000)),
            )
            .unwrap_err();
        assert!(m.why.contains("wire.md 3.2"), "{m}");
    }

    #[test]
    fn an_open_reply_with_reserved_bits_set_is_named() {
        let mut p = vec![0u8; 24];
        p[17] = 1;
        assert_eq!(open_reply_reserved(&p), None);
        p[17] = 3;
        assert!(open_reply_reserved(&p).unwrap().contains("wire.md 3.4.3"));
        p[17] = 1;
        p[19] = 1;
        assert!(open_reply_reserved(&p).is_some());
    }

    #[test]
    fn a_reg_changed_probe_of_u64_max_is_sent_unshifted() {
        for off in [-5i128, 5] {
            let mut sess = Session {
                reg_offset: Some(off),
                ..Session::default()
            };
            assert_eq!(
                sess.translate(&changed(u64::MAX)),
                changed(u64::MAX),
                "{off}"
            );
            assert_eq!(
                sess.translate(&changed(10)),
                changed((10 + off) as u64),
                "{off}"
            );
        }
    }
}
