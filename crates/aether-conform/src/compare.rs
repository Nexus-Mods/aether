//! The comparison rules: when a director's answer to a step is the answer
//! expected (the scenario's, or the reference's).
//!
//! The rule is equality, except where `docs/wire.md` lets directors differ.
//! Each exception below quotes the wire.md text that allows it (line numbers
//! as of the commit that wrote this; the quote is what to search for), and
//! `contract/scenarios/README.md` lists them all:
//!
//! - **Handles** (wire.md 3.4.3, line 738: "`fh` is any u64 the director
//!   chooses, unique among open handles"). An expected fh and the
//!   director's are paired at the OPEN that issues them, and the pairing
//!   must stay one to one while the handle is open; a successful CLOSE ends
//!   it, so the director may issue the number again. Requests are sent with
//!   the director's numbers ([`Session::translate`]): an open handle's
//!   pairing; after its CLOSE, the number the director closed (so a
//!   use-after-close tests that handle), unless the director has issued it
//!   again; and for a number never paired, or a closed one reissued, a
//!   stand-in no open handle of the director has, counted down from
//!   `u64::MAX` and the same for one number while it fits.
//! - **`mount_gen`** (wire.md 3.4.3, lines 744-748: "a generation the
//!   director bumps whenever a root's content is replaced", with the
//!   directors' differing starting values). Not compared.
//! - **`immutable`** (wire.md 3.4.3, lines 740-743: "Never set it for a
//!   write open or a directory"; otherwise it only lets the shim cache). Not
//!   compared, except that it must be clear on a write open and on a
//!   directory.
//! - **Listing order** (wire.md 3.4.2, line 688: "A director may send the
//!   entries in any order."). Listings compare as multisets of entries.
//! - **`mtime`** (wire.md 3.2, Timestamps, lines 567-568: "Which time a
//!   director reports for a path, if any, is the director's to choose").
//!   Not compared, in GETATTR or READDIR, and not checked for "changed" or
//!   "unchanged" after a write either.
//! - **READ transport** (wire.md 3.4.4, lines 783-784: "it must accept
//!   either whatever flag it sent"). Inline and bulk replies compare by
//!   their data; a bulk reply must be resolved from the arena first
//!   ([`OwnedReply::resolve_bulk`]).
//! - **STAGE_EXEC's path** (wire.md 3.4.14, lines 889-890: "the real,
//!   on-disk Windows path to start it from", which is wherever the director
//!   staged it). Not compared.
//! - **Registry versions** (wire.md 4.3, lines 1035-1039: "The number a
//!   director's overlay starts from when it mounts a layer is the
//!   director's", so versions may differ "by a constant", and "a
//!   REG_CHANGED `since` is in the numbering of the director it is sent
//!   to"). Compared up to one offset per session, fixed by the first
//!   registry reply; a REG_CHANGED `since` is sent shifted by it.
//! - **`last_write`** of a REG_KEY node (wire.md 4.3, line 1070: a FILETIME
//!   the overlay stamps with the director's clock). Not compared.

use std::collections::BTreeMap;
use std::fmt;

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
                if let Some(off) = self.reg_offset {
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
        for m in [&self.released, &self.stand_in] {
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
        let mut next = self.clone();
        next.check_one(step, req, expected, got)?;
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
                for alt in alts {
                    let mut trial = self.clone();
                    if trial.check_one(step, req, alt, got).is_ok() {
                        *self = trial;
                        return Ok(());
                    }
                }
                Err(miss("none of the allowed answers".into()))
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
                self.pair(e.fh, g.fh)
            }
            (O::Empty, O::Empty) => {
                if let OwnedRequest::Close { fh } = req {
                    if let Some(d) = self.fh.remove(fh) {
                        self.fh_back.remove(&d);
                        self.released.insert(*fh, d);
                    }
                }
                Ok(())
            }
            (O::GetAttr(e), O::GetAttr(g)) => {
                if (e.found, e.is_dir, e.size) == (g.found, g.is_dir, g.size) {
                    Ok(())
                } else {
                    Err("different attributes (mtime aside)".into())
                }
            }
            (O::ReadDir(e), O::ReadDir(g)) => {
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
