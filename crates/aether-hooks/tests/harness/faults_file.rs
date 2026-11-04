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
//! - the result is wire.md 3.7's ([`fault_table::expected`]), or, for a
//!   reply that still decodes, a well-formed answer (see [`want`]);
//! - the next unfaulted call of the same operation succeeds.
//!
//! And each case asserts that no hook panicked (`hook_panics_total`).
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
use aether_protocol::{Opcode, Reply, Status};
use aether_refdirector::harness::{BulkLie, Recorded, MAX_DELAY};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
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
                | HookedOp::RegRead
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Each operation and its reply's length, set once by [`Subject::new`].
    ops: OnceLock<Vec<(Op, usize)>>,
    next: AtomicUsize,
    /// Faulted steps run, by fault kind: how many the table decided and
    /// how many replies still decoded ([`Want::WellFormed`]).
    tally: Mutex<BTreeMap<&'static str, (usize, usize)>>,
}

impl Subject {
    /// The subject for `ops`, each called once unfaulted to find its
    /// reply's length (the shortest of the requests a fault answers).
    pub(crate) fn new(
        served: &'static Served,
        ctx: &'static Ctx,
        call: Call,
        watchdog: Duration,
        ops: &[Op],
    ) -> &'static Subject {
        let s: &'static Subject = Box::leak(Box::new(Subject {
            served,
            ctx,
            call,
            watchdog,
            payload_cap: fakedirector::PAYLOAD_CAP as usize,
            ops: OnceLock::new(),
            next: AtomicUsize::new(0),
            tally: Mutex::new(BTreeMap::new()),
        }));
        let mut lens = Vec::new();
        for &op in ops {
            let mark = served.recorder.mark();
            let got = s.watched(op).expect("an unfaulted call returns");
            assert_eq!(got, success(op), "{op:?} without a fault");
            let len = served
                .recorder
                .since(mark)
                .iter()
                .filter(|r| r.op() == Some(op.opcode()))
                .map(|r| r.reply.len())
                .min()
                .unwrap_or_else(|| panic!("{op:?} sent no {:?}", op.opcode()));
            lens.push((op, len));
        }
        s.ops.set(lens).expect("measured once");
        s
    }

    /// The call `op` on fresh names, on a thread of its own, under the
    /// watchdog.
    fn watched(&'static self, op: Op) -> Result<Seen, String> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let (ctx, call) = (self.ctx, self.call);
        std::thread::spawn(move || {
            let _ = tx.send(call(ctx, op, i));
        });
        rx.recv_timeout(self.watchdog)
            .map_err(|_| format!("the call hung past {:?}", self.watchdog))
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
    /// The reply decodes, though it is not the director's: any answer
    /// (for a call whose data the reply carries, the wrong data too), so
    /// long as nothing crashes, hangs or stops serving.
    WellFormed,
}

/// What `op` gives when `fault` answers it; `recs` are the requests the
/// call sent, as the client got their replies.
fn want(op: Op, fault: &StepFault, recs: &[Recorded]) -> Result<Want, String> {
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
        StepFault::Garbage(bytes) => decoded(op, bytes, None),
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
            decoded(op, &r.reply, None)
        }
        StepFault::Trailing(t) => {
            let r = faulted(op, recs, |r| {
                r.reply.len() > t.len() && r.reply.ends_with(t)
            })?;
            decoded(op, &r.reply, Some(&r.reply[..r.reply.len() - t.len()]))
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
/// director's own reply when the fault only added to it.
fn decoded(op: Op, bytes: &[u8], director: Option<&[u8]>) -> Want {
    let h = op.hooked();
    let Ok(got) = Reply::decode(op.opcode(), 0, bytes) else {
        // A payload that does not decode: the garbage column.
        return Want::Is(Ok(expected(h, FaultClass::Garbage)));
    };
    if let Some(d) = director {
        if Reply::decode(op.opcode(), 0, d).ok() == Some(got.clone()) {
            return Want::Is(success(op));
        }
    }
    if let (Op::WriteChunks(n), Reply::Write(w)) = (op, &got) {
        // wire.md 3.4.5: `written` past the chunk's `len` is refused; a
        // shorter one is a short write, which ends the call there.
        let asked = chunk_len(n);
        let before = (n as usize - 1) * CHUNK;
        return Want::Is(match (*w as usize).cmp(&asked) {
            std::cmp::Ordering::Equal => success(op),
            std::cmp::Ordering::Greater => Ok(Expect::Nt(fault_table::STATUS_UNSUCCESSFUL)),
            std::cmp::Ordering::Less => Err(short_write(before + *w as usize)),
        });
    }
    Want::WellFormed
}

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

fn run_step(s: &'static Subject, step: &Step) -> Result<(), String> {
    let op = step.op;
    let Some(fault) = &step.fault else {
        let got = s.watched(op)?;
        return match got == success(op) {
            true => Ok(()),
            false => Err(format!("unfaulted, got {got:?}")),
        };
    };
    let served = s.served;
    let mark = served.recorder.mark();
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
        let landed = served.recorder.since(mark).len() + served.recorder.in_flight();
        served.faults.release();
        if !served.recorder.wait_for(mark, landed, MAX_DELAY) {
            return Err("the late reply never landed".into());
        }
    }
    served.faults.clear();
    let applied = served.faults.applied(id);
    if applied == 0 || (applied > 1 && op.hooked().times().is_some()) {
        return Err(format!("the fault applied {applied} times"));
    }
    let recs = served.recorder.since(mark);
    if !recs.iter().any(|r| r.op() == Some(op.opcode())) {
        return Err(format!("the recorder saw no {:?}", op.opcode()));
    }
    let w = want(op, fault, &recs)?;
    {
        let mut t = s.tally.lock().unwrap_or_else(|e| e.into_inner());
        let e = t.entry(fault.kind()).or_default();
        match w {
            Want::Is(_) => e.0 += 1,
            Want::WellFormed => e.1 += 1,
        }
    }
    match w {
        Want::Is(w) if got != w => return Err(format!("got {got:?}, want {w:?}")),
        Want::WellFormed if got.is_err() && !op.lie_reaches_data() => {
            return Err(format!("a reply that decodes gave {got:?}"));
        }
        _ => {}
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

/// Runs the generated cases of `s` and fails with the shortest failing
/// sequence, over `cases` cases.
pub(crate) fn check(name: &str, s: &'static Subject, cases: u32) {
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
            first.call_once(|| {
                eprintln!(
                    "{name}: first failure: {e}
{steps:#?}"
                )
            });
            TestCaseError::fail(e)
        })
    });
    let tally = s.tally.lock().unwrap_or_else(|e| e.into_inner()).clone();
    eprintln!("{name}: faulted steps (as the table says, well-formed lies): {tally:?}");
    match result {
        Ok(()) => {}
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

#[test]
fn generated_faults_never_crash_or_hang_a_file_call() {
    isolate!();
    // One fresh final name per call the run can make, shrinking included.
    let names = (cases(1) + SHRINK_ITERS) as usize * MAX_STEPS * 2 + 64;
    let (served, ctx) = fault_table::file_setup(fault_table::file_director_named(names));
    let s = Subject::new(served, ctx, file_op, WATCHDOG, &FILE_OPS);
    check("file calls", s, cases(1));
}

/// A child's start through `CreateProcessW` (STAGE_EXEC). A start that
/// succeeds starts a process, so these cases are fewer.
#[test]
fn generated_faults_never_crash_or_hang_a_child_start() {
    isolate!();
    let Some(probe) = fault_table::probe() else {
        return;
    };
    let (served, ctx) = fault_table::stage_setup(&probe);
    let s = Subject::new(
        served,
        ctx,
        stage_op,
        WATCHDOG + READY_WAIT,
        &[Op::Call(HookedOp::StageExec)],
    );
    check("a child's start", s, cases(4));
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
