//! [`Faults`]: replies a director should not give, armed by a test and given
//! to the requests that match.

use super::lock;
use crate::key;
use aether_casefold::fold;
use aether_ipc::{DataArena, RequestView};
use aether_protocol::{Opcode, ReadReply, Reply, Request, Status, READ_RESP_BULK_BIT};
use aether_rpc::{Tap, TapAction};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The longest a [`Fault::Delay`] holds a reply: a longer one is cut to
/// this, so every slot is completed and a serve loop always gets back to
/// its stop flag. [`Faults::release`] ends a delay sooner.
pub const MAX_DELAY: Duration = Duration::from_secs(10);

/// Which requests a fault is given to. Every field left `None` matches
/// everything.
///
/// Matches are counted from 1 per armed fault, over every request that
/// crossed the ring, whichever tap answered it. `nth` alone fires on that
/// match only; `times` alone fires on the first `times` matches; both fire
/// on `times` matches from the `nth` on; neither fires on every match. An
/// `nth` or `times` of 0 never fires.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Match {
    pub op: Option<Opcode>,
    /// A prefix of the request's path, compared folded (`aether_casefold`)
    /// and spelled as [`key`] spells a table key: bare for root 0,
    /// `@<root>/` before it for another root. A request on a handle (READ,
    /// WRITE, SETATTR, CLOSE) has the path its OPEN named, when the OPEN
    /// crossed this tap and was answered OK; RENAME and RENAME_X match by
    /// their source. A request with no path never matches a prefix.
    pub path_prefix: Option<String>,
    pub nth: Option<u32>,
    pub times: Option<u32>,
}

impl Match {
    /// Whether the `m`-th match (from 1) fires.
    fn fires(&self, m: u32) -> bool {
        let first = self.nth.unwrap_or(1);
        let times = match (self.nth, self.times) {
            (_, Some(t)) => t,
            (Some(_), None) => 1,
            (None, None) => u32::MAX,
        };
        first != 0 && m >= first && m - first < times
    }
}

/// How a bulk READ reply lies about where its data is (`docs/wire.md` 2.6,
/// 3.4.4). The reply's `len` is the one the director answered (at least 1),
/// except for `LenOverRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkLie {
    /// The offset is the end of the arena: no byte of it is in the arena.
    PastArena,
    /// The offset is the start of another slot's bank (the next one). With
    /// one bank there is no other, and it is the end of the arena instead.
    PastBank,
    /// The offset is `u64::MAX`, so `offset + len` overflows.
    Wrapping,
    /// The slot's own bank, and a `len` one more than the request asked for.
    LenOverRequest,
}

/// A reply, as bytes, for [`Fault::Rewrite`] to edit: the request it answers
/// and the status and payload the director gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedReplyBytes {
    pub opcode: u32,
    pub flags: u32,
    /// The request payload.
    pub request: Vec<u8>,
    pub status: i32,
    pub payload: Vec<u8>,
}

impl OwnedReplyBytes {
    pub fn op(&self) -> Option<Opcode> {
        Opcode::try_from(self.opcode).ok()
    }

    /// The reply, decoded, when it is `Status::Ok` and decodes.
    pub fn reply(&self) -> Option<Reply<'_>> {
        if self.status != Status::Ok.code() {
            return None;
        }
        Reply::decode(self.op()?, self.flags, &self.payload).ok()
    }
}

/// A STORED_NAMES reply with its last name dropped: one name fewer than the
/// request has components (`docs/wire.md` 3.4.12). A reply of one name
/// becomes the empty payload, which is one empty name. Anything else is left
/// alone.
pub fn drop_last_name(r: &mut OwnedReplyBytes) {
    let Some(Reply::StoredNames(names)) = r.reply() else {
        return;
    };
    let kept = names.rsplit_once('/').map_or(0, |(head, _)| head.len());
    r.payload.truncate(kept);
}

/// An OPEN reply with the reserved bits 1 to 7 of its flags byte set, bit 0
/// (`immutable`) as the director sent it (`docs/wire.md` 3.4.3). Anything
/// else is left alone.
pub fn set_reserved_open_flag_bits(r: &mut OwnedReplyBytes) {
    if matches!(r.reply(), Some(Reply::Open(_))) {
        r.payload[17] |= 0xFE;
    }
}

/// What a matched request gets instead of the director's reply.
#[derive(Debug, Clone)]
pub enum Fault {
    /// This status and no payload; the director is not asked, so the
    /// request changes nothing (a faulted WRITE writes nothing).
    Status(Status),
    /// [`Fault::Status`] with any code: one the table lacks, a positive one,
    /// the ring's overflow `i32::MIN`.
    RawStatus(i32),
    /// The director's payload cut to its first `n` bytes.
    Truncate(usize),
    /// The director's payload with these bytes after it.
    Trailing(Vec<u8>),
    /// `Status::Ok` with these bytes for a payload.
    Garbage(Vec<u8>),
    /// A payload one byte past `payload_cap`, which the ring sends as its
    /// overflow status, `i32::MIN`, with no payload (`docs/wire.md` 2.5).
    Oversize,
    /// A bulk READ reply naming data outside its bank, `Status::Ok`, given
    /// to a READ whatever the director answered. On any other opcode it
    /// changes nothing (it still counts as applied): arm it with
    /// `op: Some(Opcode::Read)`.
    BulkOutside(BulkLie),
    /// The director's reply, held this long first (at most [`MAX_DELAY`])
    /// or until [`Faults::release`] or [`Faults::clear`]. The serve thread
    /// that took the request waits, as a director that stalls does: with
    /// one serve thread, other requests wait behind it. A test waits on the
    /// [`super::Recorder`] (`wait_in_flight`, `wait_for`), not on the clock,
    /// to know where the delayed request is.
    Delay(Duration),
    /// The director's reply as this function leaves it. Called on the serve
    /// thread with no lock held, so one that panics poisons nothing (its
    /// request is then answered `Status::IoError` and not recorded). A plain
    /// `fn`, so it captures nothing: what it needs it reads from the
    /// [`OwnedReplyBytes`] (the request is there); an edit of a typed field
    /// decodes `payload`, changes it and encodes the reply back.
    Rewrite(fn(&mut OwnedReplyBytes)),
}

/// Names one armed fault, for [`Faults::applied`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaultId(usize);

/// Where the server's bulk arena lies in the mapping (`docs/wire.md` 2.6):
/// what a [`BulkLie`] lies about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaShape {
    /// The mapping offset of bank 0.
    pub offset: u64,
    pub bank_size: u64,
    pub banks: u32,
}

impl ArenaShape {
    /// The shape of `arena`, as the server hands it to the director.
    pub fn of(arena: &DataArena<'_>) -> ArenaShape {
        ArenaShape {
            offset: arena.mapping_offset as u64,
            bank_size: arena.bank_size as u64,
            banks: arena.banks as u32,
        }
    }

    fn end(&self) -> u64 {
        self.offset + self.bank_size * u64::from(self.banks)
    }

    fn bank(&self, slot: u32) -> u64 {
        self.offset + u64::from(slot % self.banks.max(1)) * self.bank_size
    }
}

struct Armed {
    when: Match,
    /// The `when.path_prefix`, folded once.
    prefix: Option<String>,
    fault: Fault,
    live: bool,
    matched: u32,
    applied: u32,
}

#[derive(Default)]
struct State {
    armed: Vec<Armed>,
    /// The table key each open handle was opened on, from OPEN replies.
    handles: HashMap<u64, String>,
    /// Moved by [`Faults::release`]: a delay that began before it ends.
    released: u64,
}

/// A [`Tap`] that gives armed faults to the requests they match. A clone is
/// another handle on the same faults, so a test arms them while the serve
/// thread applies them.
///
/// A request is matched in [`Tap::on_request`], against every armed fault in
/// the order armed; each counts its own matches, and the first that fires is
/// the one given. A status fault answers there, so the director never sees
/// the request; the others change the director's reply in
/// [`Tap::on_reply`], which is where [`Faults::applied`] counts. Put it last
/// in a chain, behind every tap that changes replies, so the reply it
/// changes is the director's: `(recorder, (overrides, faults))`. A tap in
/// front that answers a request itself does not stop the count (the request
/// still crossed the ring), but the fault then applies to that tap's reply.
///
/// Its lock is never held across a [`Fault::Rewrite`] or while a delay waits
/// (the wait is on a condition variable, which lets it go), and a poisoned
/// one is taken anyway, so a tap or a rewrite that panics leaves it working.
/// One `Faults` serves any number of serve threads, a clone on each.
pub struct Faults {
    state: Arc<Mutex<State>>,
    /// Signalled by [`Faults::release`].
    wake: Arc<Condvar>,
    payload_cap: u32,
    arena: Option<ArenaShape>,
    /// The fault chosen for the request being served, by this handle.
    pending: Option<(usize, Fault)>,
}

impl Clone for Faults {
    fn clone(&self) -> Faults {
        Faults {
            state: self.state.clone(),
            wake: self.wake.clone(),
            payload_cap: self.payload_cap,
            arena: self.arena,
            pending: None,
        }
    }
}

impl Faults {
    /// Nothing armed, for a ring whose `payload_cap` is this (what
    /// [`Fault::Oversize`] passes) and with no arena: every [`BulkLie`] then
    /// names `u64::MAX - len`, outside any mapping.
    pub fn new(payload_cap: u32) -> Faults {
        Faults {
            state: Arc::new(Mutex::new(State::default())),
            wake: Arc::new(Condvar::new()),
            payload_cap,
            arena: None,
            pending: None,
        }
    }

    /// The server's arena, for [`Fault::BulkOutside`].
    pub fn with_arena(mut self, arena: ArenaShape) -> Faults {
        self.arena = Some(arena);
        self
    }

    /// Gives `fault` to the requests `when` matches, from the next request
    /// on.
    pub fn arm(&self, when: Match, fault: Fault) -> FaultId {
        let mut s = lock(&self.state);
        s.armed.push(Armed {
            prefix: when.path_prefix.as_deref().map(fold),
            when,
            fault,
            live: true,
            matched: 0,
            applied: 0,
        });
        FaultId(s.armed.len() - 1)
    }

    /// How many replies fault `id` has changed (a status fault: answered).
    pub fn applied(&self, id: FaultId) -> u32 {
        lock(&self.state).armed.get(id.0).map_or(0, |a| a.applied)
    }

    /// Disarms every fault and ends every delay now holding a reply
    /// ([`Faults::release`]). Their [`Faults::applied`] counts stay.
    pub fn clear(&self) {
        for a in &mut lock(&self.state).armed {
            a.live = false;
        }
        self.release();
    }

    /// Waits until fault `id` has applied `n` times, or `timeout` passes:
    /// whether it has. A delay counts as applied as it begins, so this is
    /// how a test knows a serve thread is holding a reply.
    pub fn wait_applied(&self, id: FaultId, n: u32, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut s = lock(&self.state);
        loop {
            if s.armed.get(id.0).is_some_and(|a| a.applied >= n) {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            s = self
                .wake
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Ends every [`Fault::Delay`] now holding a reply: each sends it at
    /// once. A delay that begins later waits as usual.
    pub fn release(&self) {
        lock(&self.state).released += 1;
        self.wake.notify_all();
    }

    /// Holds the reply for `d` (at most [`MAX_DELAY`]) or until a release
    /// after `epoch`, the release count when the fault was applied.
    fn delay(&self, d: Duration, epoch: u64) {
        let end = Instant::now() + d.min(MAX_DELAY);
        let mut s = lock(&self.state);
        while s.released == epoch {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = self
                .wake
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The fault the request in `view` gets, if any, counting a match on
    /// every live fault it matches.
    fn choose(&self, view: &RequestView<'_>) -> Option<(usize, Fault)> {
        let mut s = lock(&self.state);
        if !s.armed.iter().any(|a| a.live) {
            return None;
        }
        let op = Opcode::try_from(view.opcode).ok();
        let path = if s.armed.iter().any(|a| a.live && a.prefix.is_some()) {
            op.and_then(|op| Request::decode(op, view.flags, view.payload).ok())
                .and_then(|r| path_of(&r, &s.handles))
        } else {
            None
        };
        let mut chosen = None;
        for (i, a) in s.armed.iter_mut().enumerate() {
            let matches = a.live
                && a.when.op.is_none_or(|o| op == Some(o))
                && a.prefix.as_ref().is_none_or(|p| {
                    path.as_ref()
                        .is_some_and(|path| path.starts_with(p.as_str()))
                });
            if !matches {
                continue;
            }
            a.matched = a.matched.saturating_add(1);
            if chosen.is_none() && a.when.fires(a.matched) {
                chosen = Some((i, a.fault.clone()));
            }
        }
        chosen
    }

    /// Keeps the table key each OPEN answered OK named, for matching the
    /// requests on its handle; forgets it at CLOSE.
    fn track(&self, view: &RequestView<'_>, status: i32, payload: &[u8]) {
        if status != Status::Ok.code() {
            return;
        }
        match Opcode::try_from(view.opcode) {
            Ok(Opcode::Open) => {
                let req = Request::decode(Opcode::Open, view.flags, view.payload);
                let reply = Reply::decode(Opcode::Open, view.flags, payload);
                if let (Ok(Request::Open { root, path, .. }), Ok(Reply::Open(o))) = (req, reply) {
                    lock(&self.state).handles.insert(o.fh, key(root.0, path));
                }
            }
            Ok(Opcode::Close) => {
                if let Ok(Request::Close { fh }) =
                    Request::decode(Opcode::Close, view.flags, view.payload)
                {
                    lock(&self.state).handles.remove(&fh);
                }
            }
            _ => {}
        }
    }

    /// `fault` applied to the reply in `status` and `payload`.
    fn apply(
        &self,
        view: &RequestView<'_>,
        fault: Fault,
        epoch: u64,
        status: &mut i32,
        payload: &mut Vec<u8>,
    ) {
        match fault {
            Fault::Status(s) => {
                *status = s.code();
                payload.clear();
            }
            Fault::RawStatus(code) => {
                *status = code;
                payload.clear();
            }
            Fault::Truncate(n) => payload.truncate(n),
            Fault::Trailing(bytes) => payload.extend_from_slice(&bytes),
            Fault::Garbage(bytes) => {
                *status = Status::Ok.code();
                *payload = bytes;
            }
            Fault::Oversize => payload.resize(self.payload_cap as usize + 1, 0),
            Fault::BulkOutside(lie) if view.opcode == Opcode::Read as u32 => {
                let reply = self.bulk_lie(view, lie, *status, payload);
                *status = Status::Ok.code();
                payload.clear();
                Reply::Read(reply).encode(payload);
            }
            Fault::BulkOutside(_) => {}
            Fault::Delay(d) => self.delay(d, epoch),
            Fault::Rewrite(f) => {
                let mut r = OwnedReplyBytes {
                    opcode: view.opcode,
                    flags: view.flags,
                    request: view.payload.to_vec(),
                    status: *status,
                    payload: std::mem::take(payload),
                };
                f(&mut r);
                *status = r.status;
                *payload = r.payload;
            }
        }
    }

    /// The bulk reply that tells `lie` about the director's answer.
    fn bulk_lie(
        &self,
        view: &RequestView<'_>,
        lie: BulkLie,
        status: i32,
        payload: &[u8],
    ) -> ReadReply<'static> {
        let asked = match Request::decode(Opcode::Read, view.flags, view.payload) {
            Ok(Request::Read { len, .. }) => len,
            _ => 0,
        };
        let answered = if status == Status::Ok.code() {
            match Reply::decode(Opcode::Read, view.flags, payload) {
                Ok(Reply::Read(ReadReply::Inline(b))) => b.len() as u32,
                Ok(Reply::Read(ReadReply::Bulk { len, .. })) => len,
                _ => asked,
            }
        } else {
            asked
        };
        let len = answered.max(1);
        let Some(arena) = self.arena else {
            // No arena: no offset is in one; this one is not in the mapping
            // either, and `offset + len` does not overflow.
            let len = match lie {
                BulkLie::LenOverRequest => asked.saturating_add(1).min(READ_RESP_BULK_BIT - 1),
                _ => len,
            };
            return ReadReply::Bulk {
                len,
                arena_offset: u64::MAX - u64::from(len),
            };
        };
        let (len, arena_offset) = match lie {
            BulkLie::PastArena => (len, arena.end()),
            BulkLie::PastBank if arena.banks > 1 => (len, arena.bank(view.slot.wrapping_add(1))),
            BulkLie::PastBank => (len, arena.end()),
            BulkLie::Wrapping => (len, u64::MAX),
            BulkLie::LenOverRequest => (
                asked.saturating_add(1).min(READ_RESP_BULK_BIT - 1),
                arena.bank(view.slot),
            ),
        };
        ReadReply::Bulk { len, arena_offset }
    }
}

/// The table key a request names: its path, or for a request on a handle
/// the path the handle was opened on.
fn path_of(r: &Request<'_>, handles: &HashMap<u64, String>) -> Option<String> {
    let at = |root: aether_protocol::RootId, p: &str| Some(key(root.0, p));
    match *r {
        Request::GetAttr { root, path }
        | Request::ReadDir { root, path }
        | Request::Open { root, path, .. }
        | Request::Delete { root, path }
        | Request::Mkdir { root, path, .. }
        | Request::StoredNames { root, path, .. }
        | Request::StageExec { root, path } => at(root, path),
        Request::Rename { root, from, .. } => at(root, from),
        Request::RenameX {
            from_root, from, ..
        } => at(from_root, from),
        Request::Read { fh, .. }
        | Request::Write { fh, .. }
        | Request::SetAttr { fh, .. }
        | Request::Close { fh } => handles.get(&fh).cloned(),
        Request::RegLookup { path }
        | Request::RegKey { path }
        | Request::RegSetValue { path, .. }
        | Request::RegDeleteValue { path, .. }
        | Request::RegCreateKey { path, .. }
        | Request::RegDeleteKey { path }
        | Request::RegRenameKey { path, .. }
        | Request::RegChanged { path, .. } => Some(fold(path)),
        Request::Heartbeat {} => None,
    }
}

impl Tap for Faults {
    fn on_request(&mut self, view: &RequestView<'_>) -> TapAction {
        self.pending = self.choose(view);
        match self.pending {
            Some((_, Fault::Status(s))) => TapAction::Answer { status: s.code() },
            Some((_, Fault::RawStatus(status))) => TapAction::Answer { status },
            _ => TapAction::Pass,
        }
    }

    fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        self.track(view, *status, payload);
        let Some((i, fault)) = self.pending.take() else {
            return;
        };
        // Counted before it runs, so a rewrite that panics still shows.
        // The release count is read with it, so a release after a test saw
        // it applied ends its delay.
        let epoch = {
            let mut s = lock(&self.state);
            if let Some(a) = s.armed.get_mut(i) {
                a.applied = a.applied.saturating_add(1);
            }
            s.released
        };
        self.wake.notify_all();
        self.apply(view, fault, epoch, status, payload);
    }
}
