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
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// How long a call may take before it is called hung.
    pub(crate) watchdog: Duration,
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
            watchdog,
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
        if let Ok(seen) = rx.recv_timeout(self.watchdog) {
            return Ok(seen);
        }
        self.served.faults.clear();
        match rx.recv_timeout(MAX_DELAY) {
            Ok(seen) => Err(format!(
                "the call hung past {:?}; it returned {seen:?} once the faults were let go",
                self.watchdog
            )),
            Err(_) => {
                eprintln!(
                    "{op:?} hung past {:?} and did not return once every fault was let go",
                    self.watchdog + MAX_DELAY
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
        // A reply already no longer than the cut is left whole: a final
        // name's reply is shorter once a component is known and not asked
        // again.
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
/// no reply held. A watchdog shorter than the client's deadline makes a
/// reply held past the deadline a hang.
#[test]
fn a_hang_is_let_go_before_the_next_case() {
    isolate!();
    let op = Op::Call(HookedOp::GetAttr);
    let s = file_subject(0, DEADLINE / 3, &[op]);
    let hung = run_case(
        s,
        &[Step {
            op,
            fault: Some(StepFault::DelayPast(Lands::BeforeTheNextCall)),
        }],
    );
    assert!(hung.as_ref().is_err_and(|e| e.contains("hung")), "{hung:?}");
    assert_eq!(s.served.recorder.in_flight(), 0, "no reply is held");
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

/// The case count: `PROPTEST_CASES`, else [`CASES`], over `divisor` (at
/// least 4), for a property whose cases are slower.
pub(crate) fn cases(divisor: u32) -> u32 {
    let all = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CASES);
    (all / divisor).max(4)
}
