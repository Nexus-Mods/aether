//! `aether-conform run --concurrent N`: N clients on one ring at once, each
//! playing a generated case over a subtree of its own, with their requests
//! in flight together across every slot of the ring.
//!
//! wire.md 2.5 to 2.8.1 are what concurrent clients rest on: slots are
//! independent and "a director may take and answer them in any order and
//! from any number of threads"; a reply goes into the slot of the request it
//! answers, a bulk reply into that slot's bank (2.6); `fh` is unique across
//! every client of the ring (2.8.1); and every request is answered. A run
//! that sends one request at a time from one thread checks none of it.
//!
//! **A round.** For each of `--concurrent-cases` rounds, every client `k`
//! gets a generated case ([`crate::model`]) whose tree is one root with a
//! writable provider and no registry layer, and the case's tree and paths
//! are moved under the directory `c<k>` of root 0 (a STORED_NAMES `skip`
//! moved on by one; expected handle numbers moved into a range of their own,
//! `(k + 1) << 40` up, so the clients' numbers never meet in the round's
//! scenario file). The clients' subtrees are disjoint, so each client's
//! answers are its own case's whatever the others do, and the reference
//! predicts them exactly; the round is checked against an in-process
//! reference serving the combined tree before it is played, and a case that
//! answers otherwise there is a generator error. Root 0 also holds one file
//! per slot under `sweep/`, of distinct sizes and bytes.
//!
//! **Playing it.** The combined tree is mounted through the adapter. The
//! clients, one thread each with its own mapping of the ring
//! ([`Attached`]), its own HEARTBEAT, its own comparison session
//! ([`Session::among_others`]) and its own registry generation rules
//! ([`RegGen`]), send their steps one at a time, each claiming from the next
//! slot of a cursor the clients share, with their data requests (READ,
//! WRITE, SETATTR, write OPENs) held below the data limit, as the shim holds
//! them (2.8: `data_limit(worker_hint, slot_count)`). Then the sweep, from
//! one client: an OPEN of each sweep file claimed from its own slot, so
//! every slot carries a request; a GETATTR in every slot at once; and bulk
//! READs of each file from its own slot, in waves of the data limit, each
//! wave in flight together, so every bank carries a bulk reply and each
//! slot's answer is told apart from the others' by its size or bytes.
//!
//! **What fails a round**, besides any step whose answer is not its
//! expectation: a request not answered within the timeout (2.5); a bulk
//! reply outside its slot's bank (2.6); a completed slot whose `ack` is
//! another request's (2.5); an `fh` the director issues to one client while
//! another still has it open (2.8.1). Each request names its own client's
//! paths and contents, so a reply written into another request's slot is
//! another client's answer, and fails that client's comparison.
//!
//! The round that fails is written to the failures folder as a scenario
//! file, its steps marked with their clients (`"client": k`), which `run
//! --scenarios` replays one step at a time; a fault that needs requests in
//! flight together may not show then. At the end, every slot must have
//! carried a request; how many banks carried a bulk reply, and the most
//! requests seen in flight at once, are in the report's `file` and on
//! stderr (a director may answer any READ inline, 3.4.4, so an unused bank
//! is not a failure).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use aether_protocol::{OpenFlags, RequestFlags, RootId};
use aether_refdirector::tree::{fill, Contents, File, Root, Tree};
use proptest::strategy::{Strategy, ValueTree};

use crate::adapter::{Adapter, Env};
use crate::attach::Attached;
use crate::clients::{call_batch, call_on, fit_to_ring, is_data, Answer, Outcome};
use crate::compare::{render_expect, render_got, Session};
use crate::generate::GenerateError;
use crate::model::{self, CaseInput, Limits, Oracle};
use crate::run::{RegGen, ScenarioResult, StepResult};
use crate::scenario::{
    request_json, to_json_string, Expect, OwnedReply, OwnedRequest, Scenario, Step,
};

/// Cases each client plays when `--concurrent-cases` is not given.
pub const DEFAULT_CASES: u32 = 64;

/// What a concurrent run does.
#[derive(Debug, Clone)]
pub struct ConcurrentOptions {
    /// Clients at once.
    pub clients: u32,
    /// Rounds, each a case per client.
    pub cases: u32,
    pub seed: u64,
    /// The most operations per case.
    pub steps: usize,
    /// Where the failing round is written.
    pub failures: PathBuf,
}

/// A sweep file's length before its slot number is added, and how much of
/// it a sweep READ asks for.
const SWEEP_LEN: u64 = 70_000;
const SWEEP_READ: u32 = 65_536;
/// Client `k`'s expected handle numbers start at `(k + 1) * FH_STRIDE`.
const FH_STRIDE: u64 = 1 << 40;

/// The directory client `k`'s case lives under.
fn prefix(k: usize) -> String {
    format!("c{k}")
}

fn moved(p: &str, path: &str) -> String {
    if path == "." {
        p.to_string()
    } else {
        format!("{p}/{path}")
    }
}

/// `req` with root 0's paths moved under `p` and handle numbers moved up by
/// `off`.
fn relocate(req: &OwnedRequest, p: &str, off: u64) -> OwnedRequest {
    use OwnedRequest as O;
    let mv = |root: RootId, path: String| {
        if root.0 == 0 {
            moved(p, &path)
        } else {
            path
        }
    };
    match req.clone() {
        O::GetAttr { root, path } => O::GetAttr {
            root,
            path: mv(root, path),
        },
        O::ReadDir { root, path } => O::ReadDir {
            root,
            path: mv(root, path),
        },
        O::Open { root, flags, path } => O::Open {
            root,
            flags,
            path: mv(root, path),
        },
        O::Delete { root, path } => O::Delete {
            root,
            path: mv(root, path),
        },
        O::StageExec { root, path } => O::StageExec {
            root,
            path: mv(root, path),
        },
        O::Mkdir { root, mode, path } => O::Mkdir {
            root,
            mode,
            path: mv(root, path),
        },
        O::Rename { root, from, to } => O::Rename {
            root,
            from: mv(root, from),
            to: mv(root, to),
        },
        O::RenameX {
            from_root,
            to_root,
            from,
            to,
        } => O::RenameX {
            from_root,
            to_root,
            from: mv(from_root, from),
            to: mv(to_root, to),
        },
        // One more component in front: the names from the same one on.
        O::StoredNames { root, skip, path } if root.0 == 0 => O::StoredNames {
            root,
            skip: skip + 1,
            path: moved(p, &path),
        },
        O::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => O::Read {
            slot_flags,
            fh: fh + off,
            offset,
            len,
        },
        O::Write { fh, offset, data } => O::Write {
            fh: fh + off,
            offset,
            data,
        },
        O::SetAttr { fh, size } => O::SetAttr { fh: fh + off, size },
        O::Close { fh } => O::Close { fh: fh + off },
        other => other,
    }
}

/// `e` with an OPEN reply's handle number moved up by `off`.
fn relocate_expect(e: &Expect, off: u64) -> Expect {
    match e {
        Expect::Reply(OwnedReply::Open(o)) => {
            let mut o = o.clone();
            o.fh += off;
            Expect::Reply(OwnedReply::Open(o))
        }
        Expect::OneOf(alts) => {
            Expect::OneOf(alts.iter().map(|a| relocate_expect(a, off)).collect())
        }
        e => e.clone(),
    }
}

/// A case for one client: one root, a writable provider in it, no registry
/// layer (the overlay's versions are one counter for the whole ring, so one
/// client's writes would move another's answers).
fn confine(input: &mut CaseInput) {
    input.tree.whole = false;
    input.tree.roots.truncate(1);
    if let Some(r) = input.tree.roots.first_mut() {
        if r.writable.is_none() {
            // No writable entry picked: the generator adds one of its own.
            r.writable = Some(0);
        }
    }
    input.tree.registry = None;
}

/// `steps` without the ones that present a handle number the case has no
/// open handle for (a use after CLOSE, a number never issued). Sent, such a
/// step needs a stand-in number, and a client knows only its own handles:
/// wire.md 3.4.3 lets a director issue "any u64", so whatever number one
/// client picks may be another client's live handle, which would answer it
/// (and a CLOSE would close it). The sequential cases send these steps; here
/// they are left out. A handle counts as open from an OPEN that may succeed
/// (a `one_of` with a reply among its answers) to a CLOSE that does. A step
/// left out is one whose expectation is `ST_BAD_FH`, which changes nothing,
/// so the answers of the steps kept are the same.
fn without_stand_ins(steps: &[Step]) -> Vec<&Step> {
    fn opened(e: &Expect) -> Vec<u64> {
        match e {
            Expect::Reply(OwnedReply::Open(o)) => vec![o.fh],
            Expect::OneOf(alts) => alts.iter().flat_map(opened).collect(),
            _ => Vec::new(),
        }
    }
    fn may_succeed(e: &Expect) -> bool {
        match e {
            Expect::Reply(OwnedReply::Empty) => true,
            Expect::OneOf(alts) => alts.iter().any(may_succeed),
            _ => false,
        }
    }
    let mut open = std::collections::BTreeSet::new();
    let mut kept = Vec::new();
    for st in steps {
        use OwnedRequest as O;
        match &st.request {
            O::Read { fh, .. } | O::Write { fh, .. } | O::SetAttr { fh, .. } | O::Close { fh }
                if !open.contains(fh) =>
            {
                continue
            }
            O::Close { fh } if may_succeed(&st.expect) => {
                open.remove(fh);
            }
            O::Open { .. } => open.extend(opened(&st.expect)),
            _ => {}
        }
        kept.push(st);
    }
    kept
}

/// One round: the combined scenario (its steps marked with their clients)
/// and each client's steps with their places in it.
struct Round {
    scenario: Scenario,
    clients: Vec<Vec<(usize, Step)>>,
}

/// The sweep file for slot `n`.
fn sweep_path(n: u32) -> String {
    format!("sweep/s{n}.bin")
}

fn sweep_bytes(n: u32) -> Vec<u8> {
    fill((SWEEP_LEN + u64::from(n)) as usize, 1000 + u64::from(n))
}

/// The tree with every case under its client's directory, and the sweep
/// files.
fn combine(cases: &[Scenario], slots: u32) -> Result<Tree, String> {
    let mut root = Root {
        id: 0,
        ..Root::default()
    };
    for (k, s) in cases.iter().enumerate() {
        let p = prefix(k);
        let [r] = s.tree.roots.as_slice() else {
            return Err(format!(
                "client {k}'s case has {} roots, not one",
                s.tree.roots.len()
            ));
        };
        if r.id != 0 || r.writable.iter().any(|w| w == ".") {
            return Err(format!(
                "client {k}'s case is not a root 0 with writable entries"
            ));
        }
        root.dirs.push(p.clone());
        root.files.extend(r.files.iter().map(|f| File {
            path: moved(&p, &f.path),
            contents: f.contents.clone(),
            mtime: f.mtime,
        }));
        root.dirs.extend(r.dirs.iter().map(|d| moved(&p, d)));
        root.writable
            .extend(r.writable.iter().map(|w| moved(&p, w)));
    }
    root.files.extend((0..slots).map(|n| File {
        path: sweep_path(n),
        contents: Contents::Fill {
            size: SWEEP_LEN + u64::from(n),
            seed: 1000 + u64::from(n),
        },
        mtime: 0,
    }));
    let tree = Tree {
        roots: vec![root],
        registry: None,
    };
    tree.validate()
        .map_err(|e| format!("an invalid combined tree: {e}"))?;
    Ok(tree)
}

/// Round `r`'s cases, one per client, drawn from `runner`, combined.
fn round(
    runner: &mut proptest::test_runner::TestRunner,
    opts: &ConcurrentOptions,
    limits: Limits,
    r: u32,
    slots: u32,
) -> Result<Round, GenerateError> {
    let generator = |e: String| GenerateError::Generator(format!("generator: {e}"));
    let strategy = model::case_strategy(opts.steps);
    let mut cases = Vec::new();
    for k in 0..opts.clients {
        let tree = strategy
            .new_tree(runner)
            .map_err(|e| generator(e.to_string()))?;
        let mut input = tree.current();
        confine(&mut input);
        let name = format!("concurrent-{}-{r}-c{k}", opts.seed);
        cases.push(model::build_with(&input, &name, "", limits).map_err(GenerateError::Generator)?);
    }
    let tree = combine(&cases, slots).map_err(generator)?;
    let mut steps = Vec::new();
    let mut clients = Vec::new();
    for (k, s) in cases.iter().enumerate() {
        let (p, off) = (prefix(k), FH_STRIDE * (k as u64 + 1));
        let mut mine = Vec::new();
        for st in without_stand_ins(&s.steps) {
            let step = Step {
                request: relocate(&st.request, &p, off),
                expect: relocate_expect(&st.expect, off),
                cite: st.cite.clone(),
                note: st.note.clone(),
                client: k as u32,
            };
            mine.push((steps.len(), step.clone()));
            steps.push(step);
        }
        clients.push(mine);
    }
    let scenario = Scenario {
        name: format!("concurrent-{}-{r}", opts.seed),
        description: format!(
            "Round {r} of `aether-conform run --concurrent {} --seed {}`: a generated case per \
             client, each under its own directory c<k> of root 0, played with the clients' \
             requests in flight together (docs/conformance.md, \"Concurrent clients\"). \
             Replayed from this file the steps go one at a time.",
            opts.clients, opts.seed
        ),
        tree,
        steps,
    };
    let round = Round { scenario, clients };
    check_round(&round).map_err(GenerateError::Generator)?;
    Ok(round)
}

/// Plays every client's steps, one client after another, against the
/// reference serving the combined tree: moving a case under its client's
/// directory must not change one answer.
fn check_round(round: &Round) -> Result<(), String> {
    let mut oracle = Oracle::new(&round.scenario.tree)?;
    for (k, steps) in round.clients.iter().enumerate() {
        let mut session = Session::new();
        for (i, st) in steps {
            let req = session.translate(&st.request);
            let got = oracle.ask(&req);
            session
                .check(*i, &st.request, &st.expect, &got)
                .map_err(|m| {
                    format!(
                        "generator: client {k}'s case, moved under {}/, is answered otherwise by \
                     the reference serving the combined tree: {m}",
                        prefix(k)
                    )
                })?;
        }
    }
    Ok(())
}

/// A counting gate on the data requests in flight (wire.md 2.8).
struct Permits {
    free: Mutex<u32>,
    moved: Condvar,
}

impl Permits {
    fn take(&self, patience: Duration) -> bool {
        let until = Instant::now() + patience;
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            free = self
                .moved
                .wait_timeout(free, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *free -= 1;
        true
    }

    fn give(&self) {
        *self.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.moved.notify_one();
    }
}

/// What the clients of a round share.
struct Shared {
    slots: u32,
    /// Where the next request's claim starts.
    cursor: AtomicU32,
    permits: Permits,
    /// The director's open handles, by the client that opened them.
    open: Mutex<BTreeMap<u64, u32>>,
    in_flight: AtomicU32,
    most_in_flight: AtomicU32,
    coverage: Mutex<Coverage>,
}

impl Shared {
    fn next_slot(&self) -> u32 {
        self.cursor.fetch_add(1, Ordering::Relaxed) % self.slots.max(1)
    }

    fn cover(&self, o: &Outcome) {
        let mut c = self.coverage.lock().unwrap_or_else(|e| e.into_inner());
        c.note(o);
    }
}

/// Requests per slot and bulk replies per bank, across the run.
#[derive(Debug, Clone, Default)]
struct Coverage {
    slots: Vec<u64>,
    banks: Vec<u64>,
    most_in_flight: u32,
}

impl Coverage {
    fn new(slots: u32) -> Coverage {
        Coverage {
            slots: vec![0; slots as usize],
            banks: vec![0; slots as usize],
            most_in_flight: 0,
        }
    }

    fn note(&mut self, o: &Outcome) {
        if let Some(n) = self.slots.get_mut(o.slot as usize) {
            *n += 1;
        }
        if let Some(n) = o.bank.and_then(|b| self.banks.get_mut(b as usize)) {
            *n += 1;
        }
    }

    fn add(&mut self, other: &Coverage) {
        for (a, b) in self.slots.iter_mut().zip(&other.slots) {
            *a += b;
        }
        for (a, b) in self.banks.iter_mut().zip(&other.banks) {
            *a += b;
        }
        self.most_in_flight = self.most_in_flight.max(other.most_in_flight);
    }

    fn used(v: &[u64]) -> usize {
        v.iter().filter(|n| **n > 0).count()
    }
}

/// What the ring of an empty tree's mount says: the limits generated cases
/// keep to, the slot count, and the data limit.
struct Probe {
    limits: Limits,
    slots: u32,
}

fn probe(adapter: &mut Adapter, file: &Path, timeout: Duration) -> Result<Probe, GenerateError> {
    let s = Scenario {
        name: "probe".into(),
        description: "An empty root, to learn the ring's geometry".into(),
        tree: Tree {
            roots: vec![Root::default()],
            registry: None,
        },
        steps: Vec::new(),
    };
    write(file, &s)?;
    let env = match adapter.mount(file)? {
        Ok(env) => env,
        Err(why) => {
            return Err(GenerateError::Generator(format!(
                "the adapter refused to mount an empty tree: {why}"
            )))
        }
    };
    let attached = Attached::from_env(&env, timeout).map(|r| r.geom().slot_count);
    adapter.unmount()?;
    let slots = attached.map_err(|e| GenerateError::Generator(format!("attach failed: {e}")))?;
    let (cap, arena) = crate::attach::payload_cap_and_arena(&env);
    Ok(Probe {
        limits: Limits::of(cap, arena),
        slots,
    })
}

fn write(file: &Path, s: &Scenario) -> Result<(), GenerateError> {
    std::fs::write(file, to_json_string(s))
        .map_err(|e| GenerateError::Generator(format!("cannot write {}: {e}", file.display())))
}

/// Plays `opts.cases` rounds of `opts.clients` clients at once. The result
/// is one entry, `concurrent`: passed, or the failing round's steps and the
/// file it was written to.
pub fn run(
    adapter: &mut Adapter,
    opts: &ConcurrentOptions,
    timeout: Duration,
) -> Result<ScenarioResult, GenerateError> {
    let work = crate::generate::scratch()?;
    let outcome = rounds(adapter, opts, timeout, &work.join("round.json"));
    let _ = std::fs::remove_dir_all(&work);
    outcome
}

fn rounds(
    adapter: &mut Adapter,
    opts: &ConcurrentOptions,
    timeout: Duration,
    file: &Path,
) -> Result<ScenarioResult, GenerateError> {
    let probe = probe(adapter, file, timeout)?;
    let mut runner = crate::generate::runner(opts.seed);
    let mut total = Coverage::new(probe.slots);
    let failed = |file: PathBuf, steps: Vec<StepResult>, error: Option<String>| ScenarioResult {
        name: "concurrent".into(),
        file,
        passed: false,
        steps,
        error,
    };
    for r in 0..opts.cases {
        let round = round(&mut runner, opts, probe.limits, r, probe.slots)?;
        write(file, &round.scenario)?;
        let env = match adapter.mount(file)? {
            Ok(env) => env,
            Err(why) => {
                return Ok(failed(
                    file.to_path_buf(),
                    Vec::new(),
                    Some(format!("round {r}: mount refused: {why}")),
                ))
            }
        };
        let played = play(&env, timeout, &round, probe.slots);
        adapter.unmount()?;
        total.add(&played.coverage);
        let passed = played.error.is_none() && played.steps.iter().all(|s| s.passed);
        if !passed {
            let out = write_failure(opts, &round.scenario)?;
            eprintln!(
                "aether-conform: concurrent round {r} of seed {} failed; its scenario is {} \
                 (replay with run --scenarios {}, one step at a time)",
                opts.seed,
                out.display(),
                opts.failures.display()
            );
            return Ok(failed(
                out,
                played.steps,
                played.error.map(|e| format!("round {r}: {e}")),
            ));
        }
    }
    let slots_used = Coverage::used(&total.slots);
    let banks_used = Coverage::used(&total.banks);
    let summary = format!(
        "{} clients x {} cases, seed {}: {slots_used} of {} slots and {banks_used} banks used, \
         at most {} requests in flight at once",
        opts.clients, opts.cases, opts.seed, probe.slots, total.most_in_flight
    );
    eprintln!("aether-conform: concurrent: {summary}");
    if slots_used < probe.slots as usize {
        let unused: Vec<usize> = (0..total.slots.len())
            .filter(|&s| total.slots[s] == 0)
            .collect();
        return Ok(failed(
            PathBuf::from(summary),
            Vec::new(),
            Some(format!(
                "slots {unused:?} never carried a request, so they were not tested"
            )),
        ));
    }
    Ok(ScenarioResult {
        name: "concurrent".into(),
        file: PathBuf::from(summary),
        passed: true,
        steps: Vec::new(),
        error: None,
    })
}

fn write_failure(opts: &ConcurrentOptions, s: &Scenario) -> Result<PathBuf, GenerateError> {
    std::fs::create_dir_all(&opts.failures).map_err(|e| {
        GenerateError::Generator(format!(
            "cannot create the folder {}: {e}",
            opts.failures.display()
        ))
    })?;
    let out = opts.failures.join(format!("{}.json", s.name));
    write(&out, s)?;
    Ok(out)
}

/// What playing a round gave.
struct Played {
    steps: Vec<StepResult>,
    error: Option<String>,
    coverage: Coverage,
}

/// Plays a mounted round: the sweep, then every client at once.
fn play(env: &Env, timeout: Duration, round: &Round, slots: u32) -> Played {
    let mut played = Played {
        steps: Vec::new(),
        error: None,
        coverage: Coverage::new(slots),
    };
    let ring = match Attached::from_env(env, timeout) {
        Ok(r) => r,
        Err(e) => {
            played.error = Some(format!("attach failed: {e}"));
            return played;
        }
    };
    let limit = aether_ipc::data_limit(aether_ipc::ring::worker_hint(ring.seg()), slots);
    let shared = Shared {
        slots,
        cursor: AtomicU32::new(0),
        permits: Permits {
            free: Mutex::new(limit),
            moved: Condvar::new(),
        },
        open: Mutex::new(BTreeMap::new()),
        in_flight: AtomicU32::new(0),
        most_in_flight: AtomicU32::new(0),
        coverage: Mutex::new(Coverage::new(slots)),
    };
    let results: Vec<(Vec<StepResult>, Option<String>)> = std::thread::scope(|sc| {
        let threads: Vec<_> = round
            .clients
            .iter()
            .enumerate()
            .map(|(k, steps)| {
                let shared = &shared;
                sc.spawn(move || client(env, timeout, k as u32, steps, shared))
            })
            .collect();
        threads
            .into_iter()
            .enumerate()
            .map(|(k, t)| {
                t.join()
                    .unwrap_or_else(|_| (Vec::new(), Some(format!("client {k} panicked"))))
            })
            .collect()
    });
    for (steps, error) in results {
        played.steps.extend(steps);
        if played.error.is_none() {
            played.error = error;
        }
    }
    played.steps.sort_by_key(|s| s.index);
    let mut c = shared
        .coverage
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    c.most_in_flight = shared.most_in_flight.load(Ordering::Relaxed);
    played.coverage.add(&c);
    // After the clients, so what they find is reported first; a ring that
    // stopped answering them is not swept.
    if played.error.is_none() {
        if let Err(e) = sweep(&ring, slots, limit, &mut played.coverage) {
            played.error = Some(format!("sweep: {e}"));
        }
    }
    played
}

/// Every slot carries a request, and every bank a bulk reply: an OPEN of
/// each sweep file from its own slot, a GETATTR in every slot at once, and
/// bulk READs of each file from its own slot in waves of `limit`, every
/// answer checked against the file's size or bytes.
fn sweep(ring: &Attached, slots: u32, limit: u32, cov: &mut Coverage) -> Result<(), String> {
    let mut handles = Vec::new();
    for n in 0..slots {
        let open = OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: sweep_path(n),
        };
        let o = call_on(ring, n, &open)?;
        cov.note(&o);
        match &o.answer {
            Answer::Got(Ok(OwnedReply::Open(r)))
                if r.size == SWEEP_LEN + u64::from(n) && !r.is_dir =>
            {
                handles.push(r.fh)
            }
            other => return Err(format!("OPEN of {} got {other:?}", sweep_path(n))),
        }
    }
    let attrs: Vec<OwnedRequest> = (0..slots)
        .map(|n| OwnedRequest::GetAttr {
            root: RootId(0),
            path: sweep_path(n),
        })
        .collect();
    let batch: Vec<(u32, &OwnedRequest)> = attrs
        .iter()
        .enumerate()
        .map(|(n, r)| (n as u32, r))
        .collect();
    for (n, o) in call_batch(ring, &batch)?.iter().enumerate() {
        cov.note(o);
        let want = SWEEP_LEN + n as u64;
        match &o.answer {
            Answer::Got(Ok(OwnedReply::GetAttr(a))) if a.found && a.size == want => {}
            other => {
                return Err(format!(
                    "GETATTR of {} in slot {}, sent with a GETATTR in every other slot, got \
                     {other:?}, not size {want} (wire.md 2.5: a reply goes into the slot of the \
                     request it answers)",
                    sweep_path(n as u32),
                    o.slot
                ))
            }
        }
    }
    let bulk = ring.arena_len() > 0;
    let inline_max = ring.geom().payload_cap.saturating_sub(8);
    let reads: Vec<(u32, OwnedRequest, Vec<u8>)> = (0..slots)
        .map(|n| {
            let offset = u64::from(n) * 13;
            let len = if bulk {
                SWEEP_READ
            } else {
                SWEEP_READ.min(inline_max)
            };
            let all = sweep_bytes(n);
            let want =
                all[offset as usize..(offset as usize + len as usize).min(all.len())].to_vec();
            let req = OwnedRequest::Read {
                slot_flags: if bulk {
                    RequestFlags::READ_BULK
                } else {
                    RequestFlags::empty()
                },
                fh: handles[n as usize],
                offset,
                len,
            };
            (n, req, want)
        })
        .collect();
    for wave in reads.chunks(limit.max(1) as usize) {
        let batch: Vec<(u32, &OwnedRequest)> = wave.iter().map(|(n, r, _)| (*n, r)).collect();
        let outs = call_batch(ring, &batch)?;
        for ((n, _, want), o) in wave.iter().zip(&outs) {
            cov.note(o);
            match &o.answer {
                Answer::Got(Ok(OwnedReply::Read(data))) if data == want => {}
                other => {
                    return Err(format!(
                        "READ of {} in slot {}, in flight with {} others, got {}, not its {} \
                         bytes (wire.md 2.5, 2.6: each slot's reply, and bulk data, is its own)",
                        sweep_path(*n),
                        o.slot,
                        wave.len() - 1,
                        match other {
                            Answer::Got(g) => render_got(g),
                            Answer::Bad(why) => why.clone(),
                        },
                        want.len()
                    ))
                }
            }
        }
    }
    for (n, fh) in handles.iter().enumerate() {
        let o = call_on(ring, n as u32, &OwnedRequest::Close { fh: *fh })?;
        cov.note(&o);
        if o.answer != Answer::Got(Ok(OwnedReply::Empty)) {
            return Err(format!(
                "CLOSE of {} got {:?}",
                sweep_path(n as u32),
                o.answer
            ));
        }
    }
    Ok(())
}

/// Client `k`: attaches, sends its HEARTBEAT, then plays `steps` one at a
/// time. Its step results, and what stopped it, if anything.
fn client(
    env: &Env,
    timeout: Duration,
    k: u32,
    steps: &[(usize, Step)],
    shared: &Shared,
) -> (Vec<StepResult>, Option<String>) {
    let mut out = Vec::new();
    let ring = match Attached::from_env(env, timeout) {
        Ok(r) => r,
        Err(e) => return (out, Some(format!("client {k}: attach failed: {e}"))),
    };
    match call_on(&ring, shared.next_slot(), &OwnedRequest::Heartbeat) {
        Ok(o) if o.answer == Answer::Got(Ok(OwnedReply::Empty)) => shared.cover(&o),
        Ok(o) => {
            return (
                out,
                Some(format!(
                    "client {k}: the HEARTBEAT sent on attach got {:?}",
                    o.answer
                )),
            )
        }
        Err(e) => {
            return (
                out,
                Some(format!(
                    "client {k}: the HEARTBEAT sent on attach failed: {e}"
                )),
            )
        }
    }
    let mut session = Session::among_others();
    let mut reg_gen = RegGen::new(false, ring.reg_gen());
    for (i, step) in steps {
        let mut notes = vec![format!("client {k}")];
        let mut req = session.translate(&step.request);
        if fit_to_ring(&ring, &mut req) {
            notes.push("sent without read_bulk: the ring has no arena (wire.md 2.6)".into());
        }
        if let OwnedRequest::Close { fh } = &req {
            // Released as it is sent: the director may issue the number
            // again as soon as it has closed it.
            let mut open = shared.open.lock().unwrap_or_else(|e| e.into_inner());
            if open.get(fh) == Some(&k) {
                open.remove(fh);
            }
        }
        let data = is_data(&req);
        if data && !shared.permits.take(timeout) {
            return (
                out,
                Some(format!(
                    "client {k}: step {i}: no data permit within the timeout"
                )),
            );
        }
        let now = shared.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        shared.most_in_flight.fetch_max(now, Ordering::AcqRel);
        let gen_before = ring.reg_gen();
        let sent = call_on(&ring, shared.next_slot(), &req);
        let gen_after = ring.reg_gen();
        shared.in_flight.fetch_sub(1, Ordering::AcqRel);
        if data {
            shared.permits.give();
        }
        let o = match sent {
            Ok(o) => o,
            Err(e) => {
                out.push(StepResult {
                    index: *i,
                    request: request_json(&step.request),
                    expected: render_expect(&step.expect),
                    got: format!("no answer: {e}"),
                    passed: false,
                    note: Some(notes.join("; ")),
                });
                return (
                    out,
                    Some(format!(
                        "client {k}: step {i}: the ring gave no answer: {e}"
                    )),
                );
            }
        };
        shared.cover(&o);
        let (got, mut passed) = match &o.answer {
            Answer::Got(a) => {
                let r = session.check(*i, &step.request, &step.expect, a);
                if let Err(m) = &r {
                    notes.insert(0, m.why.clone());
                }
                (render_got(a), r.is_ok())
            }
            Answer::Bad(why) => {
                notes.insert(0, why.clone());
                (why.clone(), false)
            }
        };
        if let Answer::Got(Ok(OwnedReply::Open(opened))) = &o.answer {
            let mut open = shared.open.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(j) = open.insert(opened.fh, k) {
                notes.insert(
                    0,
                    format!(
                        "the director issued fh {} to client {k} while client {j} still has it \
                         open (wire.md 2.8.1: \"a director must issue them uniquely across all \
                         processes of the ring\"; 3.4.3: unique among open handles)",
                        opened.fh
                    ),
                );
                passed = false;
            }
        }
        let succeeded = matches!(o.answer, Answer::Got(Ok(_)));
        let broken = reg_gen.before(&step.request, gen_before);
        let after = reg_gen.after(&step.request, succeeded, gen_before, gen_after);
        if let Some(why) = broken.or(after) {
            notes.insert(0, why);
            passed = false;
        }
        notes.push(format!("slot {}", o.slot));
        if let Some(n) = &step.note {
            notes.push(n.clone());
        }
        out.push(StepResult {
            index: *i,
            request: request_json(&step.request),
            expected: render_expect(&step.expect),
            got,
            passed,
            note: Some(notes.join("; ")),
        });
    }
    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_moves_under_its_client_with_its_handles_out_of_the_others_way() {
        let open = OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: ".".into(),
        };
        assert_eq!(
            relocate(&open, "c2", 3 * FH_STRIDE),
            OwnedRequest::Open {
                root: RootId(0),
                flags: OpenFlags::READ,
                path: "c2".into(),
            }
        );
        let names = OwnedRequest::StoredNames {
            root: RootId(0),
            skip: 1,
            path: "Data/x.esp".into(),
        };
        assert_eq!(
            relocate(&names, "c0", FH_STRIDE),
            OwnedRequest::StoredNames {
                root: RootId(0),
                skip: 2,
                path: "c0/Data/x.esp".into(),
            }
        );
        // A root that is not served stays as it is.
        let other = OwnedRequest::GetAttr {
            root: RootId(7),
            path: "a".into(),
        };
        assert_eq!(relocate(&other, "c0", FH_STRIDE), other);
        assert_eq!(
            relocate(&OwnedRequest::Close { fh: 4 }, "c1", 2 * FH_STRIDE),
            OwnedRequest::Close {
                fh: 4 + 2 * FH_STRIDE
            }
        );
    }

    /// Generated cases, confined and moved under their clients, answer the
    /// same in the combined tree as alone.
    #[test]
    fn generated_rounds_answer_the_same_combined() {
        let opts = ConcurrentOptions {
            clients: 3,
            cases: 0,
            seed: 7,
            steps: 32,
            failures: PathBuf::new(),
        };
        let mut runner = crate::generate::runner(opts.seed);
        for r in 0..24 {
            let round = match round(&mut runner, &opts, Limits::default(), r, 8) {
                Ok(round) => round,
                Err(GenerateError::Generator(e)) => panic!("round {r}: {e}"),
                Err(GenerateError::Adapter(e)) => panic!("round {r}: {e}"),
            };
            assert_eq!(round.clients.len(), 3);
            // Loads back as a scenario: the clients' handle numbers apart.
            let text = to_json_string(&round.scenario);
            let dir = aether_testkit::tempdir().unwrap();
            let f = dir.path().join("round.json");
            std::fs::write(&f, text).unwrap();
            let back = crate::scenario::load(&f).unwrap_or_else(|e| panic!("round {r}: {e}"));
            assert_eq!(back, round.scenario);
        }
    }
}
