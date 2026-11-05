//! Generated faults never crash or hang the shim (`docs/wire.md` 3.7): a
//! proptest makes sequences of hooked file calls, each step's reply possibly
//! faulted by the harness's `Faults`, and runs them through the hooks.
//!
//! A step is a call ([`Op`]) and at most one fault ([`StepFault`]) on that
//! call's opcode, for one firing: every named status, a raw status from
//! `-99`, `1`, `i32::MAX` and `i32::MIN`, the ring's overflow, the reply cut
//! to every length below its own, 1 to 16 bytes after it, garbage up to
//! `payload_cap`, each bulk READ lie, and a delay under or past the client's
//! 300 ms deadline. A reply delayed past the deadline lands in a slot the
//! shim has given up, either before the next call or while it runs.
//!
//! Each faulted step asserts:
//!
//! - the call returned within its watchdog (the deadline plus 2 s);
//! - the fault fired once (`Faults::applied`), and the recorder saw the
//!   request;
//! - the result is wire.md 3.7's ([`fault_table::expected`]); a cut,
//!   added bytes or garbage that still decode give the director's answer
//!   when they decode to it, and otherwise what the decoded reply says
//!   (see [`want`]): only a reply carrying data (a size, bytes, a name) may
//!   give other data, and a registry reply never decodes changed;
//! - the next unfaulted call of the same operation succeeds.
//!
//! And each case asserts that no hook panicked (`hook_panics_total`). A
//! call past its watchdog is let go (every fault disarmed, every held reply
//! released) before the next case.
//! Every cut of every reply is also tried one by one (`every_cut_*`).
//!
//! The cases are fixed by a seed: `AETHER_FAULTS_SEED` (a `u64`), else
//! [`SEED`], printed at the start. `PROPTEST_CASES` sets the case count
//! (default [`CASES`]). On a failure proptest shrinks the sequence and the
//! test prints the shortest one that still fails.
//!
//! [`fault_table::expected`]: crate::fault_table::expected

use crate::fakedirector::{self, Fault, Match, Served};
use crate::fault_table::{
    self, close, expected, file_call, new_file, nt_write, stage_call, Ctx, Expect, FaultClass,
    HookedOp, Seen, CHUNK, DEADLINE, HELD, READY_WAIT, STATUS_SUCCESS,
};
use aether_ipc::RequestView;
use aether_protocol::{Opcode, Reply, Status};
use aether_refdirector::harness::{BulkLie, Mark, Recorded, MAX_DELAY};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

/// The cases in the unit suite; `PROPTEST_CASES` changes it.
pub(crate) const CASES: u32 = 128;

/// The seed when `AETHER_FAULTS_SEED` is not set.
pub(crate) const SEED: u64 = 0x5eed_fa17;

/// How many smaller sequences proptest tries after a failure.
const SHRINK_ITERS: u32 = 128;

/// The longest sequence a case makes.
const MAX_STEPS: usize = 4;

/// How long a call may take before the case calls it hung.
const WATCHDOG: Duration = Duration::from_millis(DEADLINE.as_millis() as u64 + 2000);

// ---- steps -----------------------------------------------------------------

/// A hooked call a step makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// The fault table's call ([`fault_table::file_call`] and the others).
    Call(HookedOp),
    /// `NtWriteFile` of three chunks (two of [`CHUNK`] bytes and 10), the
    /// fault on chunk `n` (1 to 3).
    WriteChunks(u32),
}

impl Op {
    /// The fault table's row.
    fn hooked(self) -> HookedOp {
        match self {
            Op::Call(op) => op,
            Op::WriteChunks(1) => HookedOp::WriteFirst,
            Op::WriteChunks(_) => HookedOp::WriteLater,
        }
    }

    fn opcode(self) -> Opcode {
        self.hooked().opcode()
    }

    /// The requests of the call the fault answers.
    fn matching(self) -> Match {
        match self {
            Op::WriteChunks(n) => Match {
                op: Some(self.opcode()),
                nth: Some(n),
                times: Some(1),
                ..Match::default()
            },
            Op::Call(op) => Match {
                op: Some(op.opcode()),
                times: op.times(),
                ..Match::default()
            },
        }
    }

    /// Whether a reply that decodes but is not the director's may give the
    /// program other data (a size, bytes, a name) than the director holds.
    fn lie_reaches_data(self) -> bool {
        matches!(
            self.hooked(),
            HookedOp::GetAttr
                | HookedOp::ReadInline
                | HookedOp::ReadBulk
                | HookedOp::SectionFill
                | HookedOp::ReadDir
                | HookedOp::StoredNames
        )
    }
}

/// When a reply held past the deadline is let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lands {
    /// Before the next call is made.
    BeforeTheNextCall,
    /// While the next call runs on other slots.
    DuringTheNextCall,
}

/// The fault a step puts on its call's reply.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum StepFault {
    /// A named status, no payload; the director is not asked.
    Status(Status),
    /// A raw status, no payload; the director is not asked.
    RawStatus(i32),
    /// A reply past `payload_cap`: the ring's overflow status.
    Oversize,
    /// The director's reply cut to this many bytes, fewer than it has.
    Truncate(usize),
    /// These bytes after the director's reply.
    Trailing(Vec<u8>),
    /// `ST_OK` with these bytes for a payload.
    Garbage(Vec<u8>),
    /// A bulk READ reply out of place.
    Bulk(BulkLie),
    /// The reply held this many milliseconds, under the deadline.
    DelayUnder(u64),
    /// The reply held past the deadline, and let go as this says.
    DelayPast(Lands),
}

/// Bytes as a length and hex, so a failing sequence prints on one line.
struct Hex<'a>(&'a [u8]);

impl std::fmt::Debug for Hex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes ", self.0.len())?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl std::fmt::Debug for StepFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepFault::Status(s) => f.debug_tuple("Status").field(s).finish(),
            StepFault::RawStatus(c) => f.debug_tuple("RawStatus").field(c).finish(),
            StepFault::Oversize => f.write_str("Oversize"),
            StepFault::Truncate(n) => f.debug_tuple("Truncate").field(n).finish(),
            StepFault::Trailing(b) => f.debug_tuple("Trailing").field(&Hex(b)).finish(),
            StepFault::Garbage(b) => f.debug_tuple("Garbage").field(&Hex(b)).finish(),
            StepFault::Bulk(l) => f.debug_tuple("Bulk").field(l).finish(),
            StepFault::DelayUnder(ms) => f.debug_tuple("DelayUnder").field(ms).finish(),
            StepFault::DelayPast(l) => f.debug_tuple("DelayPast").field(l).finish(),
        }
    }
}

impl StepFault {
    /// The fault's name, without its parameters.
    fn kind(&self) -> &'static str {
        match self {
            StepFault::Status(_) => "status",
            StepFault::RawStatus(_) => "raw status",
            StepFault::Oversize => "oversize",
            StepFault::Truncate(_) => "truncate",
            StepFault::Trailing(_) => "trailing",
            StepFault::Garbage(_) => "garbage",
            StepFault::Bulk(_) => "bulk lie",
            StepFault::DelayUnder(_) => "delay under the deadline",
            StepFault::DelayPast(_) => "delay past the deadline",
        }
    }

    fn harness(&self) -> Fault {
        match self {
            StepFault::Status(s) => Fault::Status(*s),
            StepFault::RawStatus(c) => Fault::RawStatus(*c),
            StepFault::Oversize => Fault::Oversize,
            StepFault::Truncate(n) => Fault::Truncate(*n),
            StepFault::Trailing(b) => Fault::Trailing(b.clone()),
            StepFault::Garbage(b) => Fault::Garbage(b.clone()),
            StepFault::Bulk(l) => Fault::BulkOutside(*l),
            StepFault::DelayUnder(ms) => Fault::Delay(Duration::from_millis(*ms)),
            StepFault::DelayPast(_) => Fault::Delay(HELD),
        }
    }
}

/// One call, and the fault on its reply if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) op: Op,
    pub(crate) fault: Option<StepFault>,
}

/// The raw statuses a step may answer with.
const RAW: [i32; 4] = [-99, 1, i32::MAX, i32::MIN];

/// The faults for `op`, whose director's reply is at least `reply_len`
/// bytes. A fault that cannot reach the call is left out: a cut needs a
/// reply to cut, and a bulk lie a READ.
fn fault_for(op: Op, reply_len: usize, payload_cap: usize) -> BoxedStrategy<Option<StepFault>> {
    // A fault that answers every request of the call of its kind (a final
    // name's) would answer the next call too while it is held.
    let lands = if op.matching().times.is_some() {
        vec![Lands::BeforeTheNextCall, Lands::DuringTheNextCall]
    } else {
        vec![Lands::BeforeTheNextCall]
    };
    // Every status but OK: `Fault::Status(Ok)` is OK with an empty payload,
    // which `Garbage` of no bytes makes.
    let statuses: Vec<Status> = Status::ALL
        .iter()
        .copied()
        .filter(|s| *s != Status::Ok)
        .collect();
    let under = DEADLINE.as_millis() as u64 / 4;
    let mut arms: Vec<(u32, BoxedStrategy<Option<StepFault>>)> = vec![
        (2, Just(None).boxed()),
        (
            3,
            proptest::sample::select(statuses)
                .prop_map(|s| Some(StepFault::Status(s)))
                .boxed(),
        ),
        (
            2,
            proptest::sample::select(RAW.to_vec())
                .prop_map(|c| Some(StepFault::RawStatus(c)))
                .boxed(),
        ),
        (1, Just(Some(StepFault::Oversize)).boxed()),
        (
            2,
            proptest::collection::vec(any::<u8>(), 1..=16)
                .prop_map(|b| Some(StepFault::Trailing(b)))
                .boxed(),
        ),
        (
            1,
            proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(|b| Some(StepFault::Garbage(b)))
                .boxed(),
        ),
        (
            1,
            proptest::collection::vec(any::<u8>(), 0..=payload_cap)
                .prop_map(|b| Some(StepFault::Garbage(b)))
                .boxed(),
        ),
        (
            1,
            (0..=under)
                .prop_map(|ms| Some(StepFault::DelayUnder(ms)))
                .boxed(),
        ),
        (
            1,
            proptest::sample::select(lands)
                .prop_map(|l| Some(StepFault::DelayPast(l)))
                .boxed(),
        ),
    ];
    if reply_len > 0 {
        arms.push((
            2,
            (0..reply_len)
                .prop_map(|n| Some(StepFault::Truncate(n)))
                .boxed(),
        ));
    }
    if op.opcode() == Opcode::Read {
        arms.push((
            4,
            proptest::sample::select(vec![
                BulkLie::PastArena,
                BulkLie::PastBank,
                BulkLie::Wrapping,
                BulkLie::LenOverRequest,
            ])
            .prop_map(|l| Some(StepFault::Bulk(l)))
            .boxed(),
        ));
    }
    proptest::strategy::Union::new_weighted(arms).boxed()
}

// ---- the subject -----------------------------------------------------------

/// A call as a step makes it, on fresh names `i`.
pub(crate) type Call = fn(&'static Ctx, Op, usize) -> Seen;

/// One reply as the director gave it: opcode, slot, request, reply.
type Given = (u32, u32, Vec<u8>, Vec<u8>);

/// The `ST_OK` replies as the director gave them, before any fault: a tap
/// behind the harness chain (`Options::tap`). The last [`KEPT`] are kept.
#[derive(Clone, Default)]
pub(crate) struct DirectorReplies(Arc<Mutex<VecDeque<Given>>>);

/// How many replies [`DirectorReplies`] keeps.
const KEPT: usize = 64;

impl aether_rpc::Tap for DirectorReplies {
    fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        if *status != Status::Ok.code() {
            return;
        }
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == KEPT {
            q.pop_front();
        }
        q.push_back((
            view.opcode,
            view.slot,
            view.payload.to_vec(),
            payload.clone(),
        ));
    }
}

impl DirectorReplies {
    /// The tap to put behind the chain.
    pub(crate) fn tap(&self) -> fault_table::Tapped {
        Box::new(self.clone())
    }

    /// The director's own reply to the request `r` crossed the ring as.
    fn to(&self, r: &Recorded) -> Option<Vec<u8>> {
        let q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        q.iter()
            .rev()
            .find(|(op, slot, req, _)| *op == r.opcode && *slot == r.slot && *req == r.payload)
            .map(|(.., reply)| reply.clone())
    }
}

/// What the cases run against: the served director, the hooks' context and
/// the calls, and each operation's shortest reply, measured unfaulted.
pub(crate) struct Subject {
    pub(crate) served: &'static Served,
    pub(crate) ctx: &'static Ctx,
    pub(crate) call: Call,
    /// How long a call may take before it is called hung, in milliseconds
    /// ([`Subject::set_watchdog`]).
    watchdog_ms: AtomicU64,
    /// The ring's `payload_cap`.
    pub(crate) payload_cap: usize,
    /// The director's replies, put behind the chain by the setup.
    director: DirectorReplies,
    /// Each operation and its reply's length, set once by [`Subject::new`].
    ops: OnceLock<Vec<(Op, usize)>>,
    next: AtomicUsize,
    /// Faulted steps run, by fault kind: how many had one answer, and how
    /// many replies still decoded to a lie about data ([`Want::AnyData`]).
    tally: Mutex<BTreeMap<&'static str, (usize, usize)>>,
}

impl Subject {
    /// The subject for `ops`, each called once unfaulted to find its
    /// reply's length (the shortest of the requests a fault answers).
    /// `director` is the tap the setup put behind the chain.
    pub(crate) fn new(
        served: &'static Served,
        ctx: &'static Ctx,
        call: Call,
        watchdog: Duration,
        ops: &[Op],
        director: DirectorReplies,
    ) -> &'static Subject {
        let s: &'static Subject = Box::leak(Box::new(Subject {
            served,
            ctx,
            call,
            watchdog_ms: AtomicU64::new(watchdog.as_millis() as u64),
            payload_cap: fakedirector::PAYLOAD_CAP as usize,
            director,
            ops: OnceLock::new(),
            next: AtomicUsize::new(0),
            tally: Mutex::new(BTreeMap::new()),
        }));
        let mut lens = Vec::new();
        for &op in ops {
            let len = s
                .reply_lens(op)
                .into_iter()
                .min()
                .unwrap_or_else(|| panic!("{op:?} sent no {:?}", op.opcode()));
            lens.push((op, len));
        }
        s.ops.set(lens).expect("measured once");
        s
    }

    /// How long a call may take before it is called hung.
    pub(crate) fn watchdog(&self) -> Duration {
        Duration::from_millis(self.watchdog_ms.load(Ordering::Relaxed))
    }

    /// The watchdog of the calls from now on: a case that must hang sets a
    /// short one, and the cases after it set the usual one back.
    pub(crate) fn set_watchdog(&self, d: Duration) {
        self.watchdog_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// The reply lengths of `op`'s faulted opcode in one unfaulted call.
    fn reply_lens(&'static self, op: Op) -> Vec<usize> {
        let mark = self.served.recorder.mark();
        let got = self.watched(op).expect("an unfaulted call returns");
        assert_eq!(got, success(op), "{op:?} without a fault");
        self.served
            .recorder
            .since(mark)
            .iter()
            .filter(|r| r.op() == Some(op.opcode()))
            .map(|r| r.reply.len())
            .collect()
    }

    /// The call `op` on fresh names, on a thread of its own, under the
    /// watchdog. A call past it is let go: every fault is disarmed and
    /// every held reply released, so the next case runs against a director
    /// that answers. A call still not back after that is a hang no case
    /// can follow, and ends the test process.
    fn watched(&'static self, op: Op) -> Result<Seen, String> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let (ctx, call) = (self.ctx, self.call);
        std::thread::spawn(move || {
            let _ = tx.send(call(ctx, op, i));
        });
        let watchdog = self.watchdog();
        if let Ok(seen) = rx.recv_timeout(watchdog) {
            return Ok(seen);
        }
        self.served.faults.clear();
        match rx.recv_timeout(MAX_DELAY) {
            Ok(seen) => Err(format!(
                "the call hung past {watchdog:?}; it returned {seen:?} once the faults were let go"
            )),
            Err(_) => {
                eprintln!(
                    "{op:?} hung past {:?} and did not return once every fault was let go",
                    watchdog + MAX_DELAY
                );
                std::process::exit(101);
            }
        }
    }

    /// Disarms every fault, releases every held reply, and waits until
    /// every request since `mark` has had its reply.
    fn settle(&self, mark: Mark) -> Result<(), String> {
        let landed = self.served.recorder.since(mark).len() + self.served.recorder.in_flight();
        self.served.faults.clear();
        match self.served.recorder.wait_for(mark, landed, MAX_DELAY) {
            true => Ok(()),
            false => Err("a late reply never landed".into()),
        }
    }

    /// The sequences the cases are drawn from.
    fn sequences(&self) -> BoxedStrategy<Vec<Step>> {
        let cap = self.payload_cap;
        let steps: Vec<BoxedStrategy<Step>> = self
            .ops
            .get()
            .expect("measured")
            .iter()
            .map(|&(op, len)| {
                fault_for(op, len, cap)
                    .prop_map(move |fault| Step { op, fault })
                    .boxed()
            })
            .collect();
        proptest::collection::vec(proptest::strategy::Union::new(steps), 1..=MAX_STEPS).boxed()
    }
}

/// The successful result of `op`.
fn success(op: Op) -> Seen {
    Ok(expected(op.hooked(), FaultClass::Status(Status::Ok)))
}

// ---- what a step should give --------------------------------------------

/// What a faulted call may give.
#[derive(Debug, PartialEq, Eq)]
enum Want {
    /// Exactly this.
    Is(Seen),
    /// One of these: a reply that decodes to another answer than the
    /// director's, to a call whose result carries none of its fields (an
    /// OPEN's handle, a staged path), is either taken as it is or refused.
    OneOf(Vec<Seen>),
    /// A staged path that decodes to another path than the director's: the
    /// start is made from it (wire.md 3.4.14), so it starts or fails with
    /// whatever Win32 error the system gives for that path.
    StartOrWin32,
    /// The reply decodes to other data than the director holds (a size,
    /// bytes, a name), which the call then gives: any answer, so long as
    /// nothing crashes, hangs or stops serving.
    AnyData,
}

impl Want {
    fn allows(&self, got: &Seen) -> bool {
        match self {
            Want::Is(w) => got == w,
            Want::OneOf(ws) => ws.contains(got),
            Want::StartOrWin32 => matches!(got, Ok(Expect::Nt(STATUS_SUCCESS) | Expect::Win32(_))),
            Want::AnyData => true,
        }
    }
}

/// Whether `op` is a registry call. Its replies are strict (wire.md 3.2):
/// a cut or added byte never decodes to another answer.
fn registry(op: Op) -> bool {
    matches!(op.hooked(), HookedOp::RegRead | HookedOp::RegWrite)
}

/// What `op` gives when `fault` answers it; `recs` are the requests the
/// call sent, as the client got their replies, and `director` holds them
/// as the director gave them.
fn want(
    op: Op,
    fault: &StepFault,
    recs: &[Recorded],
    director: &DirectorReplies,
) -> Result<Want, String> {
    let h = op.hooked();
    let table = |c: FaultClass| Want::Is(Ok(expected(h, c)));
    Ok(match fault {
        StepFault::Status(s) => table(FaultClass::Status(*s)),
        StepFault::RawStatus(-99) => table(FaultClass::UnknownNegative),
        // The overflow status, answered without asking the director: the
        // overflow column, but a directory the director never made is not
        // there for the shim's question after the failed MKDIR to find.
        StepFault::RawStatus(i32::MIN) if h == HookedOp::Mkdir => {
            table(FaultClass::UnknownNegative)
        }
        StepFault::RawStatus(i32::MIN) => table(FaultClass::Overflow),
        StepFault::RawStatus(_) => table(FaultClass::Positive),
        StepFault::Oversize => table(FaultClass::Overflow),
        StepFault::Bulk(l) => table(FaultClass::Bulk(*l)),
        StepFault::DelayUnder(_) => Want::Is(success(op)),
        StepFault::DelayPast(_) => table(FaultClass::NoReply),
        StepFault::Garbage(_) | StepFault::Truncate(_) | StepFault::Trailing(_)
            if h.empty_reply() =>
        {
            // Nothing to decode: the reply is as good as the director's.
            Want::Is(success(op))
        }
        StepFault::Garbage(bytes) => decoded(op, bytes, None)?,
        // The fault cuts every STORED_NAMES of a final name's query, and
        // the program gets the spelling of the last one asked: one asked
        // again because a component went stale between the system's two
        // name queries asks for more components (wire.md 3.4.12), so its
        // reply may be cut where the first was left whole, and the
        // caller's spelling stands (wire.md 3.7, final name, truncated).
        StepFault::Truncate(n) if h.times().is_none() => {
            let r = recs
                .iter()
                .rev()
                .find(|r| r.op() == Some(op.opcode()) && r.status == Status::Ok.code())
                .ok_or_else(|| format!("no {:?} answered OK", op.opcode()))?;
            match r.reply.len() < *n {
                true => Want::Is(success(op)),
                false => {
                    let own = director.to(r).ok_or_else(|| {
                        format!("no director's reply to the cut {:?}", op.opcode())
                    })?;
                    decoded(op, &r.reply, Some(&own))?
                }
            }
        }
        // A reply already no longer than the cut is left whole.
        StepFault::Truncate(n)
            if recs
                .iter()
                .filter(|r| r.op() == Some(op.opcode()))
                .all(|r| r.reply.len() < *n) =>
        {
            Want::Is(success(op))
        }
        StepFault::Truncate(n) => {
            let r = faulted(op, recs, |r| r.reply.len() == *n)?;
            let own = director
                .to(r)
                .ok_or_else(|| format!("no director's reply to the cut {:?}", op.opcode()))?;
            decoded(op, &r.reply, Some(&own))?
        }
        StepFault::Trailing(t) => {
            let r = faulted(op, recs, |r| {
                r.reply.len() > t.len() && r.reply.ends_with(t)
            })?;
            decoded(op, &r.reply, Some(&r.reply[..r.reply.len() - t.len()]))?
        }
    })
}

/// The faulted request among `recs`: the first of the call's opcode that
/// `is` picks out.
fn faulted(op: Op, recs: &[Recorded], is: impl Fn(&Recorded) -> bool) -> Result<&Recorded, String> {
    recs.iter()
        .find(|r| r.op() == Some(op.opcode()) && r.status == Status::Ok.code() && is(r))
        .ok_or_else(|| {
            let seen: Vec<_> = recs
                .iter()
                .map(|r| (r.op(), r.status, r.reply.len()))
                .collect();
            format!("no faulted {:?} among the records {seen:?}", op.opcode())
        })
}

/// What `op` gives for an `ST_OK` reply `bytes`; `director` is the
/// director's own reply when the fault changed it (a cut, bytes added).
fn decoded(op: Op, bytes: &[u8], director: Option<&[u8]>) -> Result<Want, String> {
    let h = op.hooked();
    let Ok(got) = Reply::decode(op.opcode(), 0, bytes) else {
        // A payload that does not decode: the garbage column.
        return Ok(Want::Is(Ok(expected(h, FaultClass::Garbage))));
    };
    if let Some(d) = director {
        if Reply::decode(op.opcode(), 0, d).ok() == Some(got.clone()) {
            return Ok(Want::Is(success(op)));
        }
        if registry(op) {
            return Err(format!(
                "a changed registry reply still decoded, to {got:?}"
            ));
        }
    }
    Ok(match (op, &got) {
        (Op::WriteChunks(n), Reply::Write(w)) => {
            // wire.md 3.4.5: `written` past the chunk's `len` is refused; a
            // shorter one is a short write, which ends the call there.
            let asked = chunk_len(n);
            let before = (n as usize - 1) * CHUNK;
            Want::Is(match (*w as usize).cmp(&asked) {
                std::cmp::Ordering::Equal => success(op),
                std::cmp::Ordering::Greater => Ok(Expect::Nt(fault_table::STATUS_UNSUCCESSFUL)),
                std::cmp::Ordering::Less => Err(short_write(before + *w as usize)),
            })
        }
        // The new overlay version: the write is done.
        (_, Reply::RegVersion(_)) => Want::Is(success(op)),
        (_, Reply::RegKey(k)) => Want::Is(read_of_v(k.node.as_deref())),
        _ if h == HookedOp::StageExec => Want::StartOrWin32,
        _ if op.lie_reaches_data() => Want::AnyData,
        _ => Want::OneOf(vec![success(op), Ok(expected(h, FaultClass::Garbage))]),
    })
}

/// What the registry read of `v` gives over the overlay node `node`
/// (wire.md 4.3): the node's own value, none where it hides the real one,
/// else the real key's.
fn read_of_v(node: Option<&aether_registry::Node>) -> Seen {
    let Some(n) = node else {
        return Ok(Expect::Fallback);
    };
    if let Some(v) = n.values.iter().find(|v| v.name.eq_ignore_ascii_case("v")) {
        // KEY_VALUE_PARTIAL_INFORMATION holds 4 bytes of data in the call's
        // 16-byte buffer; more is STATUS_BUFFER_OVERFLOW.
        if v.data.len() > 4 {
            return Ok(Expect::Nt(STATUS_BUFFER_OVERFLOW));
        }
        let mut word = [0u8; 4];
        word[..v.data.len()].copy_from_slice(&v.data);
        return fault_table::reg_value_seen(u32::from_le_bytes(word));
    }
    if n.created
        || n.value_tombstones
            .iter()
            .any(|t| t.eq_ignore_ascii_case("v"))
    {
        return Ok(Expect::Nt(fault_table::STATUS_OBJECT_NAME_NOT_FOUND));
    }
    Ok(Expect::Fallback)
}

/// `STATUS_BUFFER_OVERFLOW`: a value's data past the caller's buffer.
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;

// ---- running a case --------------------------------------------------------

/// Runs `steps` against `s`: `Err` names the step that went wrong and how.
fn run_case(s: &'static Subject, steps: &[Step]) -> Result<(), String> {
    let panics = aether_hooks::hook_panics_total();
    for (k, step) in steps.iter().enumerate() {
        run_step(s, step).map_err(|e| format!("step {k}, {step:?}: {e}"))?;
    }
    if aether_hooks::hook_panics_total() != panics {
        return Err("a hook panicked".into());
    }
    Ok(())
}

/// One step. Whatever it ends with, no fault stays armed and no reply
/// held, so the case after it (a smaller one, while shrinking) meets a
/// director that answers.
fn run_step(s: &'static Subject, step: &Step) -> Result<(), String> {
    let mark = s.served.recorder.mark();
    let r = step_body(s, step, mark);
    let settled = s.settle(mark);
    r.and(settled)
}

fn step_body(s: &'static Subject, step: &Step, mark: Mark) -> Result<(), String> {
    let op = step.op;
    let Some(fault) = &step.fault else {
        let got = s.watched(op)?;
        return match got == success(op) {
            true => Ok(()),
            false => Err(format!("unfaulted, got {got:?}")),
        };
    };
    let served = s.served;
    let id = served.faults.arm(op.matching(), fault.harness());
    let got = s.watched(op)?;
    let mut after = None;
    if let StepFault::DelayPast(lands) = fault {
        if *lands == Lands::DuringTheNextCall {
            after = Some(s.watched(op)?);
        }
        // Let the held reply go once it is held, and wait until it has
        // landed in its abandoned slot.
        if !served.faults.wait_applied(id, 1, MAX_DELAY) {
            return Err("the delay never held a reply".into());
        }
    }
    s.settle(mark)?;
    let applied = served.faults.applied(id);
    if applied == 0 || (applied > 1 && op.hooked().times().is_some()) {
        return Err(format!("the fault applied {applied} times"));
    }
    let recs = served.recorder.since(mark);
    if !recs.iter().any(|r| r.op() == Some(op.opcode())) {
        return Err(format!("the recorder saw no {:?}", op.opcode()));
    }
    let w = want(op, fault, &recs, &s.director)?;
    {
        let mut t = s.tally.lock().unwrap_or_else(|e| e.into_inner());
        let e = t.entry(fault.kind()).or_default();
        match w {
            Want::AnyData => e.1 += 1,
            _ => e.0 += 1,
        }
    }
    if !w.allows(&got) {
        return Err(format!("got {got:?}, want {w:?}"));
    }
    let after = match after {
        Some(a) => a,
        None => s.watched(op)?,
    };
    if after != success(op) {
        return Err(format!("got {got:?}; then the next call got {after:?}"));
    }
    Ok(())
}

/// Every cut of `op`'s replies (`Truncate(n)` for each `n` below the
/// longest reply its call gets), one step each: the failures, one line
/// each.
pub(crate) fn every_cut(s: &'static Subject, op: Op) -> Vec<String> {
    // Measured on the second call: a final name's later calls ask for fewer
    // components than its first.
    s.reply_lens(op);
    let longest = s.reply_lens(op).into_iter().max().unwrap_or(0);
    (0..longest)
        .filter_map(|n| {
            let step = Step {
                op,
                fault: Some(StepFault::Truncate(n)),
            };
            run_case(s, std::slice::from_ref(&step)).err()
        })
        .collect()
}

/// Runs the generated cases of `s` and fails with the shortest failing
/// sequence, over `cases` cases. `kinds`: the fault kinds a run of the
/// default size must have made at least once each.
///
/// The cases a seed draws are fixed. The shim and director they run against
/// are one per test, kept from case to case: fresh names count up, the
/// registry's values and the final names the shim learnt stay. So a shrunk
/// case run alone may not meet the state it failed in; the seed reruns the
/// whole run.
pub(crate) fn check(name: &str, s: &'static Subject, cases: u32, kinds: &[&str]) {
    let seed = std::env::var("AETHER_FAULTS_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(SEED);
    eprintln!("{name}: seed {seed} (AETHER_FAULTS_SEED), {cases} cases (PROPTEST_CASES)");
    let mut key = [0u8; 32];
    for c in key.chunks_mut(8) {
        c.copy_from_slice(&seed.to_le_bytes());
    }
    let config = Config {
        cases,
        failure_persistence: None,
        max_shrink_iters: SHRINK_ITERS,
        ..Config::default()
    };
    let mut runner =
        TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &key));
    // The first failure is printed as it is found: shrinking reruns cases
    // on the same shim, so a failure that leaves it broken shows here first.
    let first = std::sync::Once::new();
    let result = runner.run(&s.sequences(), |steps| {
        run_case(s, &steps).map_err(|e| {
            first.call_once(|| eprintln!("{name}: first failure: {e}\n{steps:#?}"));
            TestCaseError::fail(e)
        })
    });
    let tally = s.tally.lock().unwrap_or_else(|e| e.into_inner()).clone();
    eprintln!("{name}: faulted steps (one answer, data lies): {tally:?}");
    match result {
        Ok(()) => {
            if cases >= CASES {
                let missing: Vec<_> = kinds.iter().filter(|k| !tally.contains_key(*k)).collect();
                assert!(missing.is_empty(), "{name}: no step made {missing:?}");
            }
        }
        Err(TestError::Fail(why, steps)) => {
            panic!("{name} (seed {seed}): {why}\nthe shortest failing sequence:\n{steps:#?}")
        }
        Err(e) => panic!("{name} (seed {seed}): {e}"),
    }
}

// ---- the file calls --------------------------------------------------------

/// The length of chunk `n` (1 to 3) of [`write_chunks`]' write.
fn chunk_len(n: u32) -> usize {
    if n < 3 {
        CHUNK
    } else {
        10
    }
}

/// What [`write_chunks`] gives for a write that succeeded short, with
/// `Information` `n`.
fn short_write(n: usize) -> String {
    format!("a short write of {n} bytes")
}

/// `NtWriteFile` of three chunks to a new file. A failure must report the
/// chunks before chunk `n` (wire.md 3.4.5).
fn write_chunks(ctx: &'static Ctx, n: u32, i: usize) -> Seen {
    let h = new_file(ctx, &format!("save/g{i}.bin"))?;
    let len = 2 * CHUNK + 10;
    let (st, info) = nt_write(h, &vec![0x5Au8; len]);
    close(h);
    let before = (n as usize - 1) * CHUNK;
    match (st, info) {
        (STATUS_SUCCESS, n) if n == len => Ok(Expect::Nt(st)),
        (STATUS_SUCCESS, n) if n < len => Err(short_write(n)),
        (_, n) if st != STATUS_SUCCESS && n == before => Ok(Expect::Nt(st)),
        _ => Err(format!("{st:#x} with Information {info}")),
    }
}

fn file_op(ctx: &'static Ctx, op: Op, i: usize) -> Seen {
    match op {
        Op::Call(h) => file_call(ctx, h, i),
        Op::WriteChunks(n) => write_chunks(ctx, n, i),
    }
}

fn stage_op(ctx: &'static Ctx, op: Op, i: usize) -> Seen {
    match op {
        Op::Call(h) => stage_call(ctx, h, i),
        Op::WriteChunks(_) => unreachable!("a child's start writes nothing"),
    }
}

/// The file calls of the property: every file row of wire.md 3.7 but the
/// later write chunk, which [`Op::WriteChunks`] makes on each chunk.
const FILE_OPS: [Op; 16] = [
    Op::Call(HookedOp::OpenRead),
    Op::Call(HookedOp::OpenWrite),
    Op::Call(HookedOp::GetAttr),
    Op::Call(HookedOp::ReadInline),
    Op::Call(HookedOp::ReadBulk),
    // Eager only: a lazy fill that keeps failing ends the process.
    Op::Call(HookedOp::SectionFill),
    Op::WriteChunks(1),
    Op::WriteChunks(2),
    Op::WriteChunks(3),
    Op::Call(HookedOp::SetSize),
    Op::Call(HookedOp::ReadDir),
    Op::Call(HookedOp::Rename),
    Op::Call(HookedOp::DeleteByName),
    Op::Call(HookedOp::DeleteOnHandle),
    Op::Call(HookedOp::Mkdir),
    Op::Call(HookedOp::StoredNames),
];

/// Every fault kind but none: what a run of the file calls makes.
pub(crate) const KINDS: [&str; 9] = [
    "status",
    "raw status",
    "oversize",
    "truncate",
    "trailing",
    "garbage",
    "bulk lie",
    "delay under the deadline",
    "delay past the deadline",
];

/// The file calls' subject: one fresh final name per call `cases` cases
/// can make, shrinking included (16 names a case: the director grows with
/// `PROPTEST_CASES`).
fn file_subject(cases: u32, watchdog: Duration, ops: &[Op]) -> &'static Subject {
    let names = (cases + SHRINK_ITERS) as usize * MAX_STEPS * 2 + 64;
    let director = DirectorReplies::default();
    let (served, ctx) = fault_table::file_setup(
        fault_table::file_director_named(names),
        Some(director.tap()),
    );
    Subject::new(served, ctx, file_op, watchdog, ops, director)
}

#[test]
fn generated_faults_never_crash_or_hang_a_file_call() {
    isolate!();
    let s = file_subject(cases(1), WATCHDOG, &FILE_OPS);
    check("file calls", s, cases(1), &KINDS);
}

/// Every cut of every file call's replies, each `n` below the reply's
/// length: what the property only samples.
#[test]
fn every_cut_of_a_file_reply_is_the_table_s() {
    isolate!();
    let s = file_subject(0, WATCHDOG, &[]);
    let wrong: Vec<String> = FILE_OPS.iter().flat_map(|&op| every_cut(s, op)).collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

fn child_subject(probe: &std::path::Path) -> &'static Subject {
    let director = DirectorReplies::default();
    let (served, ctx) = fault_table::stage_setup(probe, Some(director.tap()));
    Subject::new(
        served,
        ctx,
        stage_op,
        WATCHDOG + READY_WAIT,
        &[Op::Call(HookedOp::StageExec)],
        director,
    )
}

/// A child's start through `CreateProcessW` (STAGE_EXEC). A start that
/// succeeds starts a process, so these cases are fewer.
#[test]
fn generated_faults_never_crash_or_hang_a_child_start() {
    isolate!();
    let Some(probe) = fault_table::probe() else {
        return;
    };
    check("a child's start", child_subject(&probe), cases(4), &[]);
}

#[test]
fn every_cut_of_a_child_start_reply_is_the_table_s() {
    isolate!();
    let Some(probe) = fault_table::probe() else {
        return;
    };
    let wrong = every_cut(child_subject(&probe), Op::Call(HookedOp::StageExec));
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A call held past its watchdog is let go before the next case: the case
/// fails, and the case after it meets a director with no fault armed and
/// no reply held. A watchdog shorter than the client's deadline, for the
/// hanging case only, makes a reply held past the deadline a hang; the
/// unfaulted calls around it have the usual watchdog.
#[test]
fn a_hang_is_let_go_before_the_next_case() {
    isolate!();
    let op = Op::Call(HookedOp::GetAttr);
    let s = file_subject(0, WATCHDOG, &[op]);
    s.set_watchdog(DEADLINE / 3);
    let hung = run_case(
        s,
        &[Step {
            op,
            fault: Some(StepFault::DelayPast(Lands::BeforeTheNextCall)),
        }],
    );
    assert!(hung.as_ref().is_err_and(|e| e.contains("hung")), "{hung:?}");
    assert_eq!(s.served.recorder.in_flight(), 0, "no reply is held");
    s.set_watchdog(WATCHDOG);
    let next = run_case(
        s,
        &[
            Step { op, fault: None },
            Step {
                op,
                fault: Some(StepFault::Status(Status::NotFound)),
            },
        ],
    );
    assert_eq!(next, Ok(()));
}

// ---- fixed sequences -------------------------------------------------------

/// The bytes `hex` spells, two digits a byte.
fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|k| u8::from_str_radix(&hex[k..k + 2], 16).expect("hex digits"))
        .collect()
}

/// The garbage of seed 14284789208369896287's shortest failing sequence.
const SOAK_GARBAGE: &str = "a24bf3ce01e48ec80720348f144ecf6efe8aa9fd7164dead099ecd0539bd916f2c378e3e243a3543a74a21d0fe7fdec4feb014027f0becbeeedbe5a2932b339baf778ac7f5050d000fb78fa4b740644fc00379c02466b67441d6c82a40679b8beb0593939d0e61859e0fd52c82816ebbc068041119c53ea5acf9881c6939189e9d5a1e22dfffe3b34fdb0d3c97e6dc7b4cdf81fd3b315cfe03d69e884ca8a827d055079ff288ab6224e738b1334e8ffee9ab0a00db41eedacbdf9dadb59aa452d56fd221c07646e4f25204faad9a1224156a619aca5fca58b44499103f1cba8681afaa06d29e43d8e49eb709267e54f29cda9aae6be389b058091f4d449d2490f827a20a8117d0e488e917710dab283044d3a598906eadc7cd8c172962da1a1da865ec8c35a487fec8a2b6191f64ee4305d06f6db0950c57b727db70b4013e577e9c6a83c4dc4b0bc6783064a426112b2076fe1e7164a762b6283cf789ca2164eae5ec3849c95578e1fe4d82d1b5d7bfacddbe7f5791df3609542cef1a6d0bb811973d99dbd5ef306c0704b6c1eeddb7cf98e0f96ed5ee76a60a119ccadfe8f4e976f4c4c58572dbea7859ca694123de5ae3c0a10edfd0d3227ab8c06e074f3ca8ed6eee27f0b7e54926da3f954a474de7f564bffe147c767f18c38244cc554845939772280e7daa5aed9996d95013a7440e08f58a43060411a0810d5811189a065bf2e40eae0f15f586318596bee9c9d3d87d15a02df1202bbf8c45ef08e6dbab5e30503af0dc48894ff61f2d9cd8dcb8a7fc311734ce0b62186c7d534be9790decc094447e0ed7b557204c5baeb67c8f4d4058ce7fc8362bc754ad169f4bfcd56bd94819a729a46149c8419f516a423940ceecf30d03bef107de18c03b17ed2d73e9ebf34c06de1880330afb52655a5fb924eeccbf2312ba1a6f4055f875727edc3fe23173539961ac2897c2a40e8cf62484f70da0d3f7d5b2386fc53eeb3b287ec199519b0599784fb5ce702747657cd292be932acbabfae67db96df75e020f3f9264f6862ba42336931a3a1b47c6dfa957e0877cd418fb3b1d9b4bf6ff06b4e3deb66819ca0af3f238e6d52fc59f2975f554979e31a8f7845e3fa6dfc3b17f88337955fcab631dce565a42711f1c38a08d1159c515a988eeb1d3f24425399c700db78e8c17cc7c4960de25c356e1891161ab09f785a5da04b9e531e1ca1ebf434f5db2ce82e979d3939aebcdf815501ce10e55a87b7e2ede0e9f5e6ec0a72fbb696c5637bc6281b17d4d30f46e0b88a444870ee24d9007cbe82876634095394b870db0aaa8e656be82f79ef825e896588fe681205ade58ecf81367d0ddff657e6427a71f956e6e7a851ca09c90f37b5a2efe522da800a02076beb59351165190a0f00621284845183341b8ffd9a062db4354998d3a6281164ebc7049cdc7c161fb1a98f4ac4a8c70b9adbbb122aa8164031683eea59a7597b107ee74e19a4f0c7dcf3ac7cf36f0d7e1e514736b3153c5e85f453c38ac8616aa2fcb689e454a6c3fa410e8ca5f88eb904e53236f31af87e9fa11eaa03d69c8b1a304bb113d85193c98797bcbc011d374daf4df73db4ae0bcc15c411b99e952d4441bdf0c4cdcd69578b51255029bd5ea02017ad49c592df18681c0b206e27cb38371ac43df474a05d7798015d74eabb669c04f9039c3b8298fe9fc48c5b358c221cb99443bb27065444ba0951204b06cada855a5faee9821587b4a00454fe8fb150a9a296c16aa7dd4c404c44e339b599b017d1be3a734731a1376ee16678f741a63aaa667dc4d3e07be13de34c48c01a8b8a8723c52e0f0fcbfb1d248d7cbd00c6706573849cb156a98add0d5af6198c3f915e4a8bba7b0f8a30f88ed5a2382a53b9a768c64bb2f9a9638408166ca2eaab278e0ba51f0f1aef30acd99f5e7cee8c40b4845c2820cedd571f1d852ce75b5934c269dbc0d577f1af1449e0289821ea94b078c1658a4d8cf1aa0fcd05965dffbb0017d974e22297971bdcfae209d4e215e9f306648b4cd303a8c50a9c59e119bff4df7391dde5d9e08710c0f4c7ba5430539e136c416c0949954107391987f732640aced56e6ac9c98583f5f3e2a450a0dc0dde2a152f78a3cdb0a0af77ba4464a076fd1482139c8e17821aadb1c06daa735830d9ef5c162334783d943e4edbc5ab45de62dce5264a6db7c0e7687890c56cfc1cecc0655ba1a5896fbe03612b7286be4adf3342727ce16eab935a7b21c5a4d9451439d474b4b1bff96c159279777352c666d309767af82421f2c40e5c08acaa5dde5a059537aa80b077345ab2f9808054ed69aef5635dc92d4764c1b8f9680874543d518dae664dfefe080f8ecc5201dcfb19dc58ff034677e21959bd6be3d5242fd0995e2e3733fc591a88559b7b875fc5e82b15843b703707ef22937f1a95c60b96287c73e7d04a14b83c6f43e57bdebfd9bf77e35d3169a05dfcfedd2748a2be007bcbed949a8aab9605f59b2c8f9bc3fc8e0fe0c3acd94c6e07b532392b676b043e84fd2a77488db2281c4f6ec8f0c67174e277a218337c5a9893ce066b20d1e9166c3f45b983dae38e7b4baad86836030023e7a1936af4dc1e30a1621e89d4577f5691ef3c5dc3933dc1d50cb4ec5eda5901a473068309f4c5d3931b87cc869fa9b2f45c81c01f94b8cca80c441f3d68f701fd2ea6425783ba636ca1dc4c937c05a02ce25890ac3b214c8adb50939b8e450deebcc415c61b5a010a1362ad6cbf216d5a37e856d9228a2b8bcdf60356a7e66f0e12804bb3435708efe9b4e0d9ff37c6ec85d67741a74c1e3777502936ea27fdc140679ef1ac2da25ec544c97d523ee36d7adca879f47b4b32ca3966ccceb5af9cdacc080de2e67eb0ef5c0a3f25895ab9808306d13f17f3751235a15da1f7109a5501b4d1e3db06c6dd7dcae36480b23529e2e2da73c03d89d85a0a19050c59fdd1cf8775425f09e4e563dff7e01e274972916e75a2d3f7b5d3879452d217f56d662deb9feb7544e89f2eaaaf02d3901ca3c8a1978781b77f7b055d95c6291222bd5e314907a1dde5e1539aedc8d046292208e23410b6f8ebff3e6dc0563bcc9251c5e6119d222ca681706cb3f59b7b8f73df87dbaab5422014ce8df3534161ef0aa434db64ce53176958b6e8cbf115650d6d7361509498e8309c3f081d583a538dce4553365fc9439ddc4822a83a2d93434f42b95636e6e1023882333bbc0672d566a0dc948bdca4e113bbd4d23a7e1044202c01d9c48952c2eed32888e6759af1bd9e9b2fa708594b45a6ea299c1f38d82ada3256b6d9e573966e3116f728ed9ec113a71c3415c6dff549ce089d7e8163e85ccad4a60e4a7cafb5a99a95c3b907f7a7a9b1cacaf184abd194d8fed097a155da855f65cffc9bea53b2cbb49ab88fe44d5bcae52b7260185f37fd29000cb19b7402e10fc96aa8c67a9f989b87f7115d0301c8ff3d4f610831f53d0cdc20b9cc821511b77401a893edca5c9a75e3ae35235f263051b57fbbfb140464957570053c8b820dd395d4d05681605e324a5daa46479cfbb4f5f54ae83d031271154f4bfdb914123bff19c5dcf7a103d892775403a000e6c70559256bf2897643a707fd6118f34c10e102577540142d8c8b5eb1d35c17e6d478a8c21d66f519519a07fcd8c1b207e638de1128e59a19a0704152d17687732af27e63d9b0303ebfda6f295161e3f71882a5d61bb1308c734b2cba09970ddfa32ca76a552bcdf284433832fcd8a27d242338ecd2a2f1a26e17a1cf45bbd367a80c9d1d657ae87e576b71ea977467791b9cd811715499de0d1d60307ee7c2a70d6dcdf6dc93dfd09be668550640da1ac3b4f106f67880cda1e48d0d928f4d558b53dc8ce3f1ba471a38c4d10463d15c3e72ec24e98f8507e92932c744dd037067c8b5bd0aa0346d779cde2fda9c025134559c6f081e1e44a7e63ff28b78957103ac5d7524fa27a591389ec023fbd3f4126d6dd027c3b32b153ba8f1367b853276253b4f2e3772b707bc93522e60edd3958bc918036e6b7dc713b016ffff31170eaab39d9434de7bf688b0f41a7c5de8c2fd0d6aadb44a084a8911bb32160dcb0f4f36f8c9492b5eebf5d4a20dde0ae613de73bc9dabc9a403cb5f16054d4b5b9cde3674bf3bc460f3588514e596b0bb2c06090472d7c8d16b895c2dbe20bc6cd8de3581ad8ee6430d277383358c0e4587047ea19c8dcbae49a041d4e530c84b12dce851ec30b8e2aa488fc";

/// The shortest failing sequence of seed 14284789208369896287: a final name
/// cut to 13 bytes, attributes by name answered with 6 garbage bytes, a
/// delete by name answered with 3015.
fn soak_sequence(cut: usize) -> [Step; 3] {
    [
        Step {
            op: Op::Call(HookedOp::StoredNames),
            fault: Some(StepFault::Truncate(cut)),
        },
        Step {
            op: Op::Call(HookedOp::GetAttr),
            fault: Some(StepFault::Garbage(unhex("222623f3734e"))),
        },
        Step {
            op: Op::Call(HookedOp::DeleteByName),
            fault: Some(StepFault::Garbage(unhex(SOAK_GARBAGE))),
        },
    ]
}

#[test]
fn the_soak_s_final_name_cut_then_garbage_sequence_is_the_table_s() {
    isolate!();
    let steps = soak_sequence(13);
    assert!(matches!(&steps[2].fault, Some(StepFault::Garbage(g)) if g.len() == 3015));
    let ops = steps.iter().map(|s| s.op).collect::<Vec<_>>();
    let s = file_subject(0, WATCHDOG, &ops);
    assert_eq!(run_case(s, &steps), Ok(()));
}

/// Leaves the reply whole and makes every spelling the shim remembers
/// stale: the next name query asks for every component again.
fn names_go_stale(_: &mut aether_refdirector::harness::OwnedReplyBytes) {
    aether_hooks::director::advance_name_clock(aether_hooks::director::NAME_TTL_MS_FOR_TESTS);
}

/// The soak's final name, made to happen: `GetFinalPathNameByHandleW`
/// queries the name more than once, the first query's STORED_NAMES asks for
/// the two components not yet known and is left whole by the cut, the
/// spellings go stale before the next, whose STORED_NAMES asks for all three
/// and is cut, as is every one after it. The program gets the caller's spelling (wire.md 3.7, final name,
/// truncated), and the property's expectation, which reads the last
/// STORED_NAMES of the call, allows it.
#[test]
fn a_final_name_asked_again_once_stale_and_cut_then_falls_back() {
    isolate!();
    let op = Op::Call(HookedOp::StoredNames);
    // Measuring the reply's length taught the shim the shared `Names`.
    let s = file_subject(0, WATCHDOG, &[op]);
    let i = s.next.load(Ordering::Relaxed);
    let whole = format!("N{i}/File.txt").len();
    let mark = s.served.recorder.mark();
    let stale = s.served.faults.arm(
        Match {
            op: Some(Opcode::StoredNames),
            nth: Some(1),
            ..Match::default()
        },
        Fault::Rewrite(names_go_stale),
    );
    let cut = s.served.faults.arm(op.matching(), Fault::Truncate(whole));
    let got = s.watched(op).expect("the call returns");
    s.settle(mark).expect("every reply landed");
    let recs = s.served.recorder.since(mark);
    let asked: Vec<_> = recs
        .iter()
        .filter(|r| r.op() == Some(Opcode::StoredNames))
        .map(|r| r.reply.len())
        .collect();
    // Each later query asks again: nothing was learnt from a cut reply.
    assert!(asked.len() >= 2, "{asked:?}");
    assert!(asked.iter().all(|&l| l == whole), "{asked:?}");
    assert_eq!(s.served.faults.applied(stale), 1);
    assert_eq!(
        s.served.faults.applied(cut) as usize,
        asked.len() - 1,
        "every request but the first is cut"
    );
    assert_eq!(got, Ok(Expect::Fallback));
    let w = want(op, &StepFault::Truncate(whole), &recs, &s.director).expect("a want");
    assert!(w.allows(&got), "got {got:?}, want {w:?}");
    assert_eq!(s.watched(op).expect("the call returns"), success(op));
}

/// The case count: `PROPTEST_CASES`, else [`CASES`], over `divisor` (at
/// least 4), for a property whose cases are slower.
pub(crate) fn cases(divisor: u32) -> u32 {
    let all = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CASES);
    (all / divisor).max(4)
}
