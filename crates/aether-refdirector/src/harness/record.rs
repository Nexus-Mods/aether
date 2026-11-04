//! The [`Recorder`]: every request that crossed the ring and the reply it
//! got, as bytes.

use super::lock;
use aether_ipc::RequestView;
use aether_protocol::{Opcode, ReadReply, Reply, Request, Status};
use aether_rpc::Tap;
use std::sync::{Arc, Mutex};

/// One request and its reply, as they crossed the ring: copied bytes,
/// decoded only when a test asks ([`Recorded::request`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The opcode word as sent, known to the table or not.
    pub opcode: u32,
    /// The slot flags.
    pub flags: u32,
    /// The request payload.
    pub payload: Vec<u8>,
    /// The ring slot the request came in on: which arena bank a bulk reply
    /// to it belongs in (`docs/wire.md` 2.6).
    pub slot: u32,
    /// The reply status as the taps behind the recorder left it.
    pub status: i32,
    /// The reply payload as the taps behind the recorder left it. One longer
    /// than `payload_cap` is recorded whole, though the ring sends the
    /// client its overflow status and no payload instead (wire.md 2.5).
    pub reply: Vec<u8>,
}

impl Recorded {
    /// The opcode, if the table has it.
    pub fn op(&self) -> Option<Opcode> {
        Opcode::try_from(self.opcode).ok()
    }

    /// The request, decoded; `None` for an unknown opcode or a payload that
    /// does not decode.
    pub fn request(&self) -> Option<Request<'_>> {
        Request::decode(self.op()?, self.flags, &self.payload).ok()
    }

    /// The reply, decoded; `None` unless the status is `Status::Ok` and the
    /// payload is a reply to the opcode.
    pub fn reply(&self) -> Option<Reply<'_>> {
        if self.status != Status::Ok.code() {
            return None;
        }
        Reply::decode(self.op()?, self.flags, &self.reply).ok()
    }

    /// The length of a bulk READ reply's data, which crossed in the arena
    /// rather than the slot.
    pub fn bulk_len(&self) -> Option<u32> {
        match self.reply()? {
            Reply::Read(ReadReply::Bulk { len, .. }) => Some(len),
            _ => None,
        }
    }

    /// The bytes this round trip moved: request payload, reply payload and,
    /// for a bulk READ, the data in the arena.
    pub fn ring_bytes(&self) -> u64 {
        (self.payload.len() + self.reply.len()) as u64 + u64::from(self.bulk_len().unwrap_or(0))
    }
}

/// A point in a [`Recorder`]'s log: what came after it is what a test's
/// calls sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mark(usize);

/// A [`Tap`] that keeps every request and its reply, in the order the server
/// answered them. A clone is another handle on the same log, so a test
/// thread reads what the serve thread writes.
///
/// It records in [`Tap::on_reply`], so it sees the reply as every tap behind
/// it left it: put it first in a chain (`(recorder, rest)`) to record what
/// the client got. In a pair both taps see every request, so a request a
/// later tap answered is recorded too. A request whose taps or director
/// panicked before the recorder's `on_reply` ran is not recorded (the ring
/// still answers it, `Status::IoError`). Copying the bytes is all it does on
/// the serve thread; nothing is decoded there.
#[derive(Clone, Default)]
pub struct Recorder {
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Now: [`Recorder::since`] a mark is what was answered after it.
    pub fn mark(&self) -> Mark {
        Mark(lock(&self.log).len())
    }

    /// Every record after `m`, in order.
    pub fn since(&self, m: Mark) -> Vec<Recorded> {
        lock(&self.log).get(m.0..).unwrap_or_default().to_vec()
    }

    /// The opcode of every record after `m`, in order (`None` for one the
    /// table does not have).
    pub fn ops_since(&self, m: Mark) -> Vec<Option<Opcode>> {
        self.fold_since(m, Vec::new(), |mut v, r| {
            v.push(r.op());
            v
        })
    }

    /// How many requests with opcode `op` were answered after `m`.
    pub fn count_since(&self, m: Mark, op: Opcode) -> usize {
        self.fold_since(m, 0, |n, r| n + usize::from(r.opcode == op as u32))
    }

    /// How many requests were answered after `m`: the round trips.
    pub fn total_since(&self, m: Mark) -> usize {
        self.fold_since(m, 0, |n, _| n + 1)
    }

    /// The bytes the requests after `m` moved ([`Recorded::ring_bytes`]).
    pub fn ring_bytes_since(&self, m: Mark) -> u64 {
        self.fold_since(m, 0, |n, r| n + r.ring_bytes())
    }

    fn fold_since<T>(&self, m: Mark, init: T, f: impl FnMut(T, &Recorded) -> T) -> T {
        lock(&self.log)
            .get(m.0..)
            .unwrap_or_default()
            .iter()
            .fold(init, f)
    }
}

impl Tap for Recorder {
    fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        let r = Recorded {
            opcode: view.opcode,
            flags: view.flags,
            payload: view.payload.to_vec(),
            slot: view.slot,
            status: *status,
            reply: payload.clone(),
        };
        lock(&self.log).push(r);
    }
}
