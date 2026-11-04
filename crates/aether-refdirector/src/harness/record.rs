//! The [`Recorder`]: every request that crossed the ring and the reply the
//! client got.

use super::lock;
use aether_ipc::RequestView;
use aether_protocol::{Opcode, Reply, Request, Status, READ_RESP_BULK_BIT};
use aether_rpc::{Tap, TapAction};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One request and the reply the ring sent for it: copied bytes, decoded
/// only when a test asks ([`Recorded::request`]). A [`Recorder::headers_only`]
/// record keeps the lengths and leaves `payload` and `reply` empty.
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
    /// The status the ring sent: the overflow `i32::MIN` for a reply past
    /// `payload_cap` (wire.md 2.5), whatever the taps and director answered.
    pub status: i32,
    /// The reply payload the ring sent (empty for an overflow).
    pub reply: Vec<u8>,
    /// The request payload's length.
    pub request_len: u32,
    /// The sent reply payload's length.
    pub reply_len: u32,
    /// For a bulk READ answered OK, the data's length, which crossed in the
    /// arena rather than the slot; 0 otherwise.
    pub bulk_len: u32,
}

impl Recorded {
    /// The opcode, if the table has it.
    pub fn op(&self) -> Option<Opcode> {
        Opcode::try_from(self.opcode).ok()
    }

    /// The request, decoded; `None` for an unknown opcode, a payload that
    /// does not decode, or a headers-only record.
    pub fn request(&self) -> Option<Request<'_>> {
        if self.payload.len() != self.request_len as usize {
            return None;
        }
        Request::decode(self.op()?, self.flags, &self.payload).ok()
    }

    /// The reply, decoded; `None` unless the status is `Status::Ok` and the
    /// payload is a reply to the opcode (never for a headers-only record).
    pub fn reply(&self) -> Option<Reply<'_>> {
        if self.status != Status::Ok.code() || self.reply.len() != self.reply_len as usize {
            return None;
        }
        Reply::decode(self.op()?, self.flags, &self.reply).ok()
    }

    /// The bytes this round trip moved: request payload, sent reply payload
    /// and, for a bulk READ, the data in the arena.
    pub fn ring_bytes(&self) -> u64 {
        u64::from(self.request_len) + u64::from(self.reply_len) + u64::from(self.bulk_len)
    }
}

/// The length a sent READ reply says its bulk data has: bit 31 of its first
/// word (`docs/wire.md` 3.4.4). One word read, nothing decoded.
fn bulk_len(opcode: u32, status: i32, reply: &[u8]) -> u32 {
    if opcode != Opcode::Read as u32 || status != Status::Ok.code() || reply.len() < 16 {
        return 0;
    }
    let word = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
    if word & READ_RESP_BULK_BIT != 0 {
        word & !READ_RESP_BULK_BIT
    } else {
        0
    }
}

/// A point in a [`Recorder`]'s log: what came after it is what a test's
/// calls sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mark(usize);

#[derive(Default)]
struct Log {
    records: Vec<Recorded>,
    /// Requests taken (`on_request`) and not yet sent (`on_sent`).
    in_flight: usize,
    max_in_flight: usize,
}

#[derive(Default)]
struct Shared {
    log: Mutex<Log>,
    /// Signalled whenever a request is taken or sent.
    changed: Condvar,
    headers_only: bool,
}

/// A [`Tap`] that keeps every request and the reply the client got, in the
/// order the server sent them, and how many requests are in flight. A clone
/// is another handle on the same log, so a test thread reads what the serve
/// threads write; one recorder serves any number of threads.
///
/// A request is in flight from its `on_request` to its `on_sent`, and it is
/// recorded in `on_sent` ([`Tap::on_sent`]), after every tap's `on_reply`
/// and with the ring's own completion rule applied: a reply past
/// `payload_cap` is recorded as the overflow the client got. So its place in
/// a chain does not change what it records; first (`(recorder, rest)`) is
/// the convention. In a pair both taps see every request, so a request a
/// later tap answered is recorded too. A request whose director, tap or
/// [`super::Fault::Rewrite`] panicked is not recorded (the ring answers it
/// `Status::IoError`) and stays counted in flight.
///
/// Copying bytes is all it does on a serve thread; nothing is decoded there.
/// [`Recorder::headers_only`] copies none.
#[derive(Clone, Default)]
pub struct Recorder {
    shared: Arc<Shared>,
}

impl Recorder {
    /// A recorder that keeps every request's and reply's bytes.
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// A recorder that keeps opcodes, flags, slots, statuses and lengths,
    /// and copies no payload: for timing a path, where a copy would be
    /// measured with it. [`Recorded::request`] and [`Recorded::reply`] are
    /// `None`; counts and [`Recorder::ring_bytes_since`] are as full.
    pub fn headers_only() -> Recorder {
        Recorder {
            shared: Arc::new(Shared {
                headers_only: true,
                ..Shared::default()
            }),
        }
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        lock(&self.shared.log)
    }

    /// Now: [`Recorder::since`] a mark is what was sent after it.
    pub fn mark(&self) -> Mark {
        Mark(self.log().records.len())
    }

    /// Every record after `m`, in order.
    pub fn since(&self, m: Mark) -> Vec<Recorded> {
        self.log().records.get(m.0..).unwrap_or_default().to_vec()
    }

    /// The opcode of every record after `m`, in order (`None` for one the
    /// table does not have).
    pub fn ops_since(&self, m: Mark) -> Vec<Option<Opcode>> {
        self.fold_since(m, Vec::new(), |mut v, r| {
            v.push(r.op());
            v
        })
    }

    /// How many requests with opcode `op` were sent after `m`.
    pub fn count_since(&self, m: Mark, op: Opcode) -> usize {
        self.fold_since(m, 0, |n, r| n + usize::from(r.opcode == op as u32))
    }

    /// How many requests were sent after `m`: the round trips.
    pub fn total_since(&self, m: Mark) -> usize {
        self.fold_since(m, 0, |n, _| n + 1)
    }

    /// The bytes the requests after `m` moved ([`Recorded::ring_bytes`]).
    pub fn ring_bytes_since(&self, m: Mark) -> u64 {
        self.fold_since(m, 0, |n, r| n + r.ring_bytes())
    }

    /// Requests taken off the ring and not yet answered, now.
    pub fn in_flight(&self) -> usize {
        self.log().in_flight
    }

    /// The most requests ever in flight at once (since
    /// [`Recorder::reset_max_in_flight`]). More than 1 needs more than one
    /// serve thread ([`super::serve_threads`]) and requests that overlap.
    pub fn max_in_flight(&self) -> usize {
        self.log().max_in_flight
    }

    /// Starts [`Recorder::max_in_flight`] again from what is in flight now.
    pub fn reset_max_in_flight(&self) {
        let mut log = self.log();
        log.max_in_flight = log.in_flight;
    }

    /// Waits until `n` records follow `m`, or `timeout` passes: whether they
    /// do. How a test knows a late reply was sent (a delay past the
    /// client's deadline) before it makes the next call.
    pub fn wait_for(&self, m: Mark, n: usize, timeout: Duration) -> bool {
        self.wait_until(timeout, |log| log.records.len().saturating_sub(m.0) >= n)
    }

    /// Waits until at least `n` requests are in flight at once, or `timeout`
    /// passes: whether they are.
    pub fn wait_in_flight(&self, n: usize, timeout: Duration) -> bool {
        self.wait_until(timeout, |log| log.in_flight >= n)
    }

    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&Log) -> bool) -> bool {
        let end = Instant::now() + timeout;
        let mut log = self.log();
        while !done(&log) {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            log = self
                .shared
                .changed
                .wait_timeout(log, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    fn fold_since<T>(&self, m: Mark, init: T, f: impl FnMut(T, &Recorded) -> T) -> T {
        self.log()
            .records
            .get(m.0..)
            .unwrap_or_default()
            .iter()
            .fold(init, f)
    }
}

impl Tap for Recorder {
    fn on_request(&mut self, _: &RequestView<'_>) -> TapAction {
        let mut log = self.log();
        log.in_flight += 1;
        log.max_in_flight = log.max_in_flight.max(log.in_flight);
        drop(log);
        self.shared.changed.notify_all();
        TapAction::Pass
    }

    fn on_sent(&mut self, view: &RequestView<'_>, status: i32, payload: &[u8]) {
        let keep = |b: &[u8]| {
            if self.shared.headers_only {
                Vec::new()
            } else {
                b.to_vec()
            }
        };
        let r = Recorded {
            opcode: view.opcode,
            flags: view.flags,
            payload: keep(view.payload),
            slot: view.slot,
            status,
            reply: keep(payload),
            request_len: view.payload.len() as u32,
            reply_len: payload.len() as u32,
            bulk_len: bulk_len(view.opcode, status, payload),
        };
        let mut log = self.log();
        log.records.push(r);
        log.in_flight = log.in_flight.saturating_sub(1);
        drop(log);
        self.shared.changed.notify_all();
    }
}
