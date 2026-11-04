//! The comparison rules: when a director's answer to a step is the answer
//! expected (the scenario's, or the reference's).
//!
//! The rule is equality, except where `docs/wire.md` lets directors differ.
//! Each exception below names the wire.md text that allows it (line
//! numbers as of this writing), and
//! `contract/scenarios/README.md` lists them all:
//!
//! - **Handles** (wire.md 3.4.3, line 735: "`fh` is any u64 the director
//!   chooses, unique among open handles"). An expected fh and the
//!   director's are paired at the OPEN that issues them, and the pairing
//!   must stay one to one while the handle is open; a successful CLOSE ends
//!   it, so the director may issue the number again. Requests are sent with
//!   the director's numbers ([`Session::translate`]).
//! - **`mount_gen`** (wire.md 3.4.3, lines 741-745: "a generation the director bumps
//!   whenever a root's content is replaced", with the directors' differing
//!   starting values). Not compared.
//! - **`immutable`** (wire.md 3.4.3, lines 737-740: "Never set it for a write open or a
//!   directory"; otherwise it only lets the shim cache). Not compared,
//!   except that it must be clear on a write open and on a directory.
//! - **Listing order** (wire.md 3.4.2, lines 684-685, gives the listing no order; the shim
//!   "does not sort or otherwise interpret the order"). Listings compare as
//!   multisets of entries.
//! - **`mtime`** (wire.md 3.2, Timestamps, lines 564-566: the encoding, and "0 or less means
//!   no time"; nothing says which time a director reports). Not compared, in
//!   GETATTR or READDIR.
//! - **READ transport** (wire.md 3.4.4, lines 780-781: "it must accept either whatever flag
//!   it sent"). Inline and bulk replies compare by their data; a bulk reply
//!   must be resolved from the arena first ([`OwnedReply::resolve_bulk`]).
//! - **STAGE_EXEC's path** (wire.md 3.4.14, lines 886-887: "the real, on-disk Windows path
//!   to start it from", which is wherever the director staged it). Not
//!   compared.
//! - **Registry versions** (wire.md 4.3, lines 1030 and 1086-1090: the overlay's version after the
//!   operation, with overlay.rs normative for how it moves; "The persisted
//!   file format ... is the director's own business", and the version a
//!   mounted layer starts from is part of it). Compared up to one offset per
//!   session, fixed by the first registry reply; a REG_CHANGED `since` is
//!   sent shifted by it.
//! - **`last_write`** of a REG_KEY node (wire.md 4.3, line 1062: a FILETIME the overlay
//!   stamps with the director's clock). Not compared.

use std::collections::BTreeMap;
use std::fmt;

use aether_protocol::{OpenFlags, Status};

use crate::scenario::{Expect, OwnedReply, OwnedRequest, Step};

/// The comparison state across one scenario's steps: the handle pairing
/// and the registry version offset.
#[derive(Debug, Clone)]
pub struct Session<'s> {
    steps: &'s [Step],
    /// Expected fh to the director's, for the open handles.
    fh: BTreeMap<u64, u64>,
    /// The director's fh to the expected one.
    fh_back: BTreeMap<u64, u64>,
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

impl<'s> Session<'s> {
    /// A session over `steps`: [`Session::check`]'s `step` indexes them, for
    /// the request a reply answers. A step past the end is compared without
    /// its request.
    pub fn new(steps: &'s [Step]) -> Session<'s> {
        Session {
            steps,
            fh: BTreeMap::new(),
            fh_back: BTreeMap::new(),
            reg_offset: None,
        }
    }

    /// `req` as the director should get it: expected handles replaced by
    /// the director's they are paired with, and a REG_CHANGED `since` shifted
    /// by the registry version offset. A handle with no pairing (one a step
    /// expects to be refused) is sent as it is.
    pub fn translate(&self, req: &OwnedRequest) -> OwnedRequest {
        let mut r = req.clone();
        let fh = |n: &mut u64| {
            if let Some(d) = self.fh.get(n) {
                *n = *d;
            }
        };
        match &mut r {
            OwnedRequest::Read { fh: n, .. }
            | OwnedRequest::Write { fh: n, .. }
            | OwnedRequest::SetAttr { fh: n, .. }
            | OwnedRequest::Close { fh: n } => fh(n),
            OwnedRequest::RegChanged { since, .. } => {
                if let Some(off) = self.reg_offset {
                    *since = (i128::from(*since) + off).clamp(0, i128::from(u64::MAX)) as u64;
                }
            }
            _ => {}
        }
        r
    }

    /// Whether `got`, the director's answer to step `step`, is `expected`
    /// under the rules above. On success the session moves on (a handle
    /// paired or released, the registry offset fixed); on a mismatch it is
    /// unchanged.
    pub fn check(
        &mut self,
        step: usize,
        expected: &Expect,
        got: &Result<OwnedReply, Status>,
    ) -> Result<(), Mismatch> {
        let mut next = self.clone();
        next.check_one(step, expected, got)?;
        *self = next;
        Ok(())
    }

    fn check_one(
        &mut self,
        step: usize,
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
                    if trial.check_one(step, alt, got).is_ok() {
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
            (Expect::Reply(e), Ok(g)) => self.reply(step, e, g).map_err(miss),
        }
    }

    fn request(&self, step: usize) -> Option<&'s OwnedRequest> {
        self.steps.get(step).map(|s| &s.request)
    }

    fn reply(&mut self, step: usize, e: &OwnedReply, g: &OwnedReply) -> Result<(), String> {
        use OwnedReply as O;
        match (e, g) {
            (O::Open(e), O::Open(g)) => {
                let write = matches!(
                    self.request(step),
                    Some(OwnedRequest::Open { flags, .. }) if flags.contains(OpenFlags::WRITE)
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
                if let Some(OwnedRequest::Close { fh }) = self.request(step) {
                    if let Some(d) = self.fh.remove(fh) {
                        self.fh_back.remove(&d);
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

fn render_expect(e: &Expect) -> String {
    match e {
        Expect::Reply(r) => render_reply(r),
        Expect::Status(s) => format!("status {}", s.name()),
        Expect::OneOf(alts) => {
            let v: Vec<String> = alts.iter().map(render_expect).collect();
            format!("one of [{}]", v.join(" | "))
        }
    }
}

fn render_got(g: &Result<OwnedReply, Status>) -> String {
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
