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
//! per slot under `sweep/`, of distinct sizes and bytes, and the writable
//! directory `meet/`; the combined tree has a registry layer with one key,
//! for the meetings (the cases' own probe of an absent layer is left out). For
//! a director whose mount says `"registry": false` the combined tree has no
//! registry layer and the meetings make no registry request.
//!
//! **Meetings.** After the clients, requests on one name, file or key, in
//! flight together each from a slot of its own ([`meet`]), whose outcome
//! wire.md 2.8.1 makes predictable since each request takes effect
//! atomically: of OPEN_EXCL creates of one name, or REG_CREATE_KEYs of one
//! key, exactly one succeeds; MKDIRs of one name are each `ST_OK` or
//! `ST_EXISTS` and leave a directory; WRITEs to disjoint ranges all land;
//! append WRITEs each land whole at an end; a GETATTR, OPEN or READDIR in
//! flight with a RENAME or DELETE sees the state before it or after it,
//! never a mixture; of a create and a MKDIR of one name exactly one
//! succeeds and the name is one entry of its kind; and a create in a
//! directory being renamed lands under its new name or is refused. Each
//! round meets in a directory of its own, `meet/r<round>`.
//!
//! **Playing it.** The combined tree is mounted through the adapter. The
//! clients, one thread each with its own mapping of the ring
//! ([`Attached`]), its own HEARTBEAT, its own comparison session
//! ([`Session::among_others`]) and its own registry generation rules
//! ([`RegGen`]), send their steps one at a time, each claiming from the next
//! slot of a cursor the clients share, with their data requests (READ,
//! WRITE, SETATTR, write OPENs) held below the data limit, as the shim holds
//! them (2.8: `data_limit(worker_hint, slot_count)`). Then the meetings and
//! the sweep, from
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

use aether_protocol::Status;
use aether_protocol::{OpenFlags, RequestFlags, RootId};
use aether_refdirector::tree::{fill, Contents, File, Layer, RegKey, Registry, Root, Tree};
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
/// they are left out. A handle counts as open from an OPEN that issues it
/// to a CLOSE that may succeed. A step left out is one whose expectation is
/// `ST_BAD_FH`, which changes nothing, so the answers of the steps kept are
/// the same.
///
/// An OPEN that may answer a handle or an error (a `one_of` with both) is
/// the case's last step: which one the director chooses decides whether the
/// later steps have a handle to send, and what the tree then holds (a create
/// open), while the case's later expectations are the reference's, made
/// after its own choice.
fn without_stand_ins(steps: &[Step]) -> Vec<&Step> {
    fn opened(e: &Expect) -> Vec<u64> {
        match e {
            Expect::Reply(OwnedReply::Open(o)) => vec![o.fh],
            Expect::OneOf(alts) => alts.iter().flat_map(opened).collect(),
            _ => Vec::new(),
        }
    }
    fn may_fail(e: &Expect) -> bool {
        match e {
            Expect::Reply(OwnedReply::Open(_)) => false,
            Expect::OneOf(alts) => alts.iter().any(may_fail),
            _ => true,
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
            O::Open { .. } => {
                let fhs = opened(&st.expect);
                if !fhs.is_empty() && may_fail(&st.expect) {
                    kept.push(st);
                    break;
                }
                open.extend(fhs);
            }
            _ => {}
        }
        kept.push(st);
    }
    kept
}

/// One round: the combined scenario (its steps marked with their clients)
/// and each client's steps with their places in it.
struct Round {
    number: u32,
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
fn combine(cases: &[Scenario], slots: u32, registry: bool) -> Result<Tree, String> {
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
        // Layer `i` of every case is layer `i` of the combined root.
        for (i, l) in r.lower.iter().enumerate() {
            if root.lower.len() <= i {
                root.lower.resize_with(i + 1, Layer::default);
            }
            let to = &mut root.lower[i];
            to.files.extend(l.files.iter().map(|f| File {
                path: moved(&p, &f.path),
                contents: f.contents.clone(),
                mtime: f.mtime,
            }));
            to.dirs.extend(l.dirs.iter().map(|d| moved(&p, d)));
        }
    }
    root.dirs.push(MEET_DIR.into());
    root.writable.push(MEET_DIR.into());
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
        registry: registry.then(|| Registry {
            keys: vec![RegKey {
                path: MEET_KEY.into(),
                volatile: false,
                values: Vec::new(),
            }],
        }),
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
    let tree = combine(&cases, slots, limits.registry).map_err(generator)?;
    let mut steps = Vec::new();
    let mut clients = Vec::new();
    for (k, s) in cases.iter().enumerate() {
        let (p, off) = (prefix(k), FH_STRIDE * (k as u64 + 1));
        let mut mine = Vec::new();
        // Not the case's probe of a registry layer it does not have: the
        // combined tree has one, for the meetings.
        for st in without_stand_ins(&s.steps)
            .into_iter()
            .filter(|st| !st.request.is_registry())
        {
            let step = Step {
                request: relocate(&st.request, &p, off),
                expect: relocate_expect(&st.expect, off),
                cite: st.cite.clone(),
                note: st.note.clone(),
                client: k as u32,
                literal_fh: false,
                // Each client's labels its own.
                choice: st.choice.as_ref().map(|c| format!("c{k}-{c}")),
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
    let round = Round {
        number: r,
        scenario,
        clients,
    };
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
        limits: Limits {
            layers: adapter.serves_layers(),
            registry: adapter.serves_registry(),
            ..Limits::of(cap, arena)
        },
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
        skipped: None,
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
        skipped: None,
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
        let n = round.clients.len();
        let registry = round.scenario.tree.registry.is_some();
        if let Err(e) = meet(
            &ring,
            round.number,
            n,
            limit,
            registry,
            &mut played.coverage,
        ) {
            played.error = Some(format!("meeting: {e}"));
        }
    }
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
                    answer: None,
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
            answer: None,
        });
    }
    (out, None)
}

// ---------------------------------------------------------------------------
// Meetings: requests of several clients on one name, file or key at once.

/// The writable directory of root 0 the meetings are held in, and the key
/// below which they create keys.
const MEET_DIR: &str = "meet";
const MEET_KEY: &str = r"\Registry\Machine\Software\Conform";
/// Records each append meeting's client writes, and their length.
const RECORDS: usize = 3;
const RECORD_LEN: usize = 37;

/// One meeting's requests, in flight together, each from a slot of its own:
/// the outcomes, with the coverage noted. A reply that is not an answer
/// (wire.md 2.5) fails the meeting.
fn together(
    ring: &Attached,
    reqs: &[OwnedRequest],
    cov: &mut Coverage,
) -> Result<Vec<Result<OwnedReply, Status>>, String> {
    let batch: Vec<(u32, &OwnedRequest)> = reqs
        .iter()
        .enumerate()
        .map(|(n, r)| (n as u32, r))
        .collect();
    let mut out = Vec::new();
    for (o, req) in call_batch(ring, &batch)?.into_iter().zip(reqs) {
        cov.note(&o);
        match o.answer {
            Answer::Got(g) => out.push(g),
            Answer::Bad(why) => return Err(format!("{req:?}: {why}")),
        }
    }
    Ok(out)
}

/// One request, alone.
fn alone(
    ring: &Attached,
    req: &OwnedRequest,
    cov: &mut Coverage,
) -> Result<Result<OwnedReply, Status>, String> {
    Ok(together(ring, std::slice::from_ref(req), cov)?.remove(0))
}

/// An OPEN's handle, or why there is none.
fn opened(req: &OwnedRequest, got: &Result<OwnedReply, Status>) -> Result<u64, String> {
    match got {
        Ok(OwnedReply::Open(o)) => Ok(o.fh),
        other => Err(format!("{req:?} got {}", render_got(other))),
    }
}

/// The whole of a file through handle `fh` of `size` bytes, read inline in
/// pieces the ring's slot holds, and (with an arena) in bulk too: each must
/// be `want`.
fn read_back(
    ring: &Attached,
    fh: u64,
    want: &[u8],
    what: &str,
    cov: &mut Coverage,
) -> Result<(), String> {
    let inline = ring.geom().payload_cap.saturating_sub(8) as usize;
    let mut got = Vec::new();
    while got.len() < want.len() + 1 {
        let req = OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh,
            offset: got.len() as u64,
            len: inline.min(4096) as u32,
        };
        match alone(ring, &req, cov)? {
            Ok(OwnedReply::Read(d)) if d.is_empty() => break,
            Ok(OwnedReply::Read(d)) => got.extend(d),
            other => return Err(format!("{what}: {req:?} got {}", render_got(&other))),
        }
    }
    if got != want {
        return Err(format!(
            "{what}: read back inline, {} bytes, not the {} expected{}",
            got.len(),
            want.len(),
            first_difference(&got, want)
        ));
    }
    if ring.arena_len() > 0 {
        let req = OwnedRequest::Read {
            slot_flags: RequestFlags::READ_BULK,
            fh,
            offset: 0,
            len: want.len() as u32 + 1,
        };
        match alone(ring, &req, cov)? {
            Ok(OwnedReply::Read(d)) if d == want => {}
            other => {
                return Err(format!(
                    "{what}: read back in bulk, got {}",
                    render_got(&other)
                ))
            }
        }
    }
    Ok(())
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => format!(" (first difference at byte {i})"),
        None => String::new(),
    }
}

/// The record client `k` appends `j`th: its numbers, then a fill of its
/// own.
fn record(k: usize, j: usize) -> Vec<u8> {
    let mut r = fill(RECORD_LEN, 7000 + (k * RECORDS + j) as u64);
    r[0] = k as u8;
    r[1] = j as u8;
    r
}

/// The meetings of round `r`, `n` clients at each, at most `limit` data
/// requests in flight (wire.md 2.8), each checked as wire.md 2.8.1 says
/// requests in flight together take effect: atomically. The REG_CREATE_KEYs
/// are met only when `registry` says the tree has a registry layer.
fn meet(
    ring: &Attached,
    r: u32,
    n: usize,
    limit: u32,
    registry: bool,
    cov: &mut Coverage,
) -> Result<(), String> {
    let data = n.min(limit.max(1) as usize).max(1);
    // Each round in a directory of its own, so a listing of it holds this
    // round's names only, however many rounds came before.
    let here = format!("{MEET_DIR}/r{r}");
    let path = |what: &str| format!("{here}/{what}{r}");
    let fails = |what: &str, why: String| Err(format!("{what} (wire.md 2.8.1): {why}"));
    let made = OwnedRequest::Mkdir {
        root: RootId(0),
        mode: 0o755,
        path: here.clone(),
    };
    alone(ring, &made, cov)?.map_err(|st| format!("{made:?}: {st:?}"))?;

    // OPEN_EXCL creates of one name: exactly one succeeds, the rest find it.
    let excl = OwnedRequest::Open {
        root: RootId(0),
        flags: OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCL,
        path: path("excl"),
    };
    let got = together(ring, &vec![excl.clone(); data.max(2)], cov)?;
    let won: Vec<u64> = got
        .iter()
        .filter_map(|g| match g {
            Ok(OwnedReply::Open(o)) => Some(o.fh),
            _ => None,
        })
        .collect();
    let lost = got.iter().filter(|g| **g == Err(Status::Exists)).count();
    for fh in &won {
        let _ = alone(ring, &OwnedRequest::Close { fh: *fh }, cov)?;
    }
    if won.len() != 1 || lost != got.len() - 1 {
        return fails(
            &format!(
                "{} OPEN_EXCL creates of {} at once",
                got.len(),
                path("excl")
            ),
            format!(
                "{} succeeded and {lost} were ST_EXISTS, not one and the rest: {}",
                won.len(),
                got.iter().map(render_got).collect::<Vec<_>>().join(", ")
            ),
        );
    }

    // MKDIRs of one name: each ST_OK or ST_EXISTS, one directory after.
    let mkdir = OwnedRequest::Mkdir {
        root: RootId(0),
        mode: 0o755,
        path: path("dir"),
    };
    let got = together(ring, &vec![mkdir; n.max(2)], cov)?;
    let ok = got.iter().filter(|g| **g == Ok(OwnedReply::Empty)).count();
    let exists = got.iter().filter(|g| **g == Err(Status::Exists)).count();
    let attr = OwnedRequest::GetAttr {
        root: RootId(0),
        path: path("dir"),
    };
    let after = alone(ring, &attr, cov)?;
    let dir = matches!(&after, Ok(OwnedReply::GetAttr(a)) if a.found && a.is_dir);
    if ok == 0 || ok + exists != got.len() || !dir {
        return fails(
            &format!("{} MKDIRs of {} at once", got.len(), path("dir")),
            format!(
                "{ok} ST_OK and {exists} ST_EXISTS of {}, then GETATTR {}",
                got.len(),
                render_got(&after)
            ),
        );
    }

    // REG_CREATE_KEYs of one key: exactly one succeeds. Not for a director
    // that serves no registry.
    if registry {
        let key = format!("{MEET_KEY}\\Meet{r}");
        let create = OwnedRequest::RegCreateKey {
            path: key.clone(),
            volatile: false,
        };
        let got = together(ring, &vec![create; n.max(2)], cov)?;
        let ok = got
            .iter()
            .filter(|g| matches!(g, Ok(OwnedReply::RegVersion(_))))
            .count();
        let exists = got.iter().filter(|g| **g == Err(Status::Exists)).count();
        if ok != 1 || exists != got.len() - 1 {
            return fails(
                &format!("{} REG_CREATE_KEYs of {key} at once", got.len()),
                format!(
                    "{ok} succeeded and {exists} were ST_EXISTS: {}",
                    got.iter().map(render_got).collect::<Vec<_>>().join(", ")
                ),
            );
        }
    }

    // WRITEs to disjoint, unaligned ranges of one file, each through a
    // handle of its own: all land.
    let file = path("disjoint");
    let open = |flags: OpenFlags| OwnedRequest::Open {
        root: RootId(0),
        flags,
        path: file.clone(),
    };
    let create = open(OpenFlags::WRITE | OpenFlags::CREATE);
    let first = opened(&create, &alone(ring, &create, cov)?)?;
    let mut fhs = vec![first];
    for _ in 1..data {
        let o = open(OpenFlags::WRITE | OpenFlags::READ);
        fhs.push(opened(&o, &alone(ring, &o, cov)?)?);
    }
    let (skew, len) = (3 + (r as usize % 5), 1000 + 13 * (r as usize % 7));
    let parts: Vec<Vec<u8>> = (0..data)
        .map(|k| fill(len, 5000 + (r as usize * 8 + k) as u64))
        .collect();
    let writes: Vec<OwnedRequest> = (0..data)
        .map(|k| OwnedRequest::Write {
            fh: fhs[k],
            offset: (skew + k * len) as u64,
            data: parts[k].clone(),
        })
        .collect();
    for (k, g) in together(ring, &writes, cov)?.iter().enumerate() {
        if *g != Ok(OwnedReply::Write(len as u32)) {
            return fails(
                &format!("WRITE {k} of {data} to {file} at once"),
                format!("got {}", render_got(g)),
            );
        }
    }
    let mut want = vec![0u8; skew];
    for p in &parts {
        want.extend(p);
    }
    read_back(
        ring,
        fhs[0],
        &want,
        &format!("{data} WRITEs to disjoint ranges of {file} at once (wire.md 2.8.1)"),
        cov,
    )?;
    for fh in fhs {
        let _ = alone(ring, &OwnedRequest::Close { fh }, cov)?;
    }

    // Append WRITEs, each a record of its own: each lands whole at an end.
    let file = path("append");
    let appender = |create: bool| OwnedRequest::Open {
        root: RootId(0),
        flags: if create {
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND
        } else {
            OpenFlags::WRITE | OpenFlags::READ | OpenFlags::APPEND
        },
        path: file.clone(),
    };
    let mut fhs = Vec::new();
    for k in 0..data {
        let o = appender(k == 0);
        fhs.push(opened(&o, &alone(ring, &o, cov)?)?);
    }
    for j in 0..RECORDS {
        let writes: Vec<OwnedRequest> = (0..data)
            .map(|k| OwnedRequest::Write {
                fh: fhs[k],
                offset: 0,
                data: record(k, j),
            })
            .collect();
        for (k, g) in together(ring, &writes, cov)?.iter().enumerate() {
            if *g != Ok(OwnedReply::Write(RECORD_LEN as u32)) {
                return fails(
                    &format!("append WRITE {k} of {data} to {file} at once"),
                    format!("got {}", render_got(g)),
                );
            }
        }
    }
    let size = data * RECORDS * RECORD_LEN;
    let read = OwnedRequest::Read {
        slot_flags: RequestFlags::empty(),
        fh: fhs[0],
        offset: 0,
        len: (size + 1) as u32,
    };
    let body = match alone(ring, &read, cov)? {
        Ok(OwnedReply::Read(d)) => d,
        other => return fails(&format!("READ of {file}"), render_got(&other)),
    };
    let mut seen: Vec<Vec<u8>> = body.chunks(RECORD_LEN).map(<[u8]>::to_vec).collect();
    let mut all: Vec<Vec<u8>> = (0..data)
        .flat_map(|k| (0..RECORDS).map(move |j| record(k, j)))
        .collect();
    seen.sort();
    all.sort();
    if body.len() != size || seen != all {
        return fails(
            &format!("{data} clients appending {RECORDS} records each to {file} at once"),
            if body.len() != size {
                format!("the file is {} bytes, not {size}", body.len())
            } else {
                "a record is not whole, or not where an end was".into()
            },
        );
    }
    for fh in fhs {
        let _ = alone(ring, &OwnedRequest::Close { fh }, cov)?;
    }

    // A RENAME, and a DELETE, with lookups of both names in flight: each
    // lookup sees the state before or the state after, and a listing never
    // shows both names or neither.
    let (a, b) = (path("from"), path("to"));
    let bytes = fill(100 + r as usize, 9000 + u64::from(r));
    let mk = OwnedRequest::Open {
        root: RootId(0),
        flags: OpenFlags::WRITE | OpenFlags::CREATE,
        path: a.clone(),
    };
    let fh = opened(&mk, &alone(ring, &mk, cov)?)?;
    let w = OwnedRequest::Write {
        fh,
        offset: 0,
        data: bytes.clone(),
    };
    alone(ring, &w, cov)?.map_err(|s| format!("{w:?}: {s:?}"))?;
    alone(ring, &OwnedRequest::Close { fh }, cov)?.map_err(|s| format!("CLOSE: {s:?}"))?;
    let size = bytes.len() as u64;
    let lookups = |name: &str| {
        vec![
            OwnedRequest::GetAttr {
                root: RootId(0),
                path: name.to_string(),
            },
            OwnedRequest::Open {
                root: RootId(0),
                flags: OpenFlags::READ,
                path: name.to_string(),
            },
        ]
    };
    for deleting in [false, true] {
        let change = if deleting {
            OwnedRequest::Delete {
                root: RootId(0),
                path: b.clone(),
            }
        } else {
            OwnedRequest::Rename {
                root: RootId(0),
                from: a.clone(),
                to: b.clone(),
            }
        };
        let mut reqs = vec![change.clone()];
        let watched: Vec<&str> = if deleting { vec![&b] } else { vec![&a, &b] };
        for name in &watched {
            reqs.extend(lookups(name));
        }
        reqs.push(OwnedRequest::ReadDir {
            root: RootId(0),
            path: here.clone(),
        });
        let got = together(ring, &reqs, cov)?;
        if got[0] != Ok(OwnedReply::Empty) {
            return fails(&format!("{change:?}"), render_got(&got[0]));
        }
        let mut i = 1;
        for name in &watched {
            // Each lookup whole, the file as it was or no file: separate
            // requests may land on either side of the change.
            for g in &got[i..i + 2] {
                match g {
                    Ok(OwnedReply::GetAttr(x)) if !x.found => {}
                    Ok(OwnedReply::GetAttr(x)) if x.size == size && !x.is_dir => {}
                    Err(Status::NotFound) => {}
                    Ok(OwnedReply::Open(o)) if o.size == size && !o.is_dir => {
                        let _ = alone(ring, &OwnedRequest::Close { fh: o.fh }, cov)?;
                    }
                    other => {
                        return fails(
                            &format!("a lookup of {name} in flight with {change:?}"),
                            format!("got {}", render_got(other)),
                        )
                    }
                }
            }
            i += 2;
        }
        if let Ok(OwnedReply::ReadDir(entries)) = &got[i] {
            let has = |p: &str| {
                let leaf = p.rsplit('/').next().unwrap_or(p);
                entries.iter().any(|e| e.name == leaf)
            };
            let mixed = if deleting {
                has(&a)
            } else {
                has(&a) == has(&b)
            };
            if mixed {
                return fails(
                    &format!("READDIR of {here} in flight with {change:?}"),
                    format!(
                        "it lists {}, neither the state before nor the state after",
                        entries
                            .iter()
                            .map(|e| e.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
        } else {
            return fails(
                &format!("READDIR of {here} in flight with {change:?}"),
                render_got(&got[i]),
            );
        }
    }

    kinds_meet(ring, &here, &path("kind"), cov)?;
    parent_meet(ring, &path("d"), &path("e"), cov)?;
    Ok(())
}

/// A create and a MKDIR of one missing name in flight together: at most one
/// succeeds, and the name is then one entry of the winner's kind, listed
/// once in `dir` (wire.md 2.8.1).
fn kinds_meet(ring: &Attached, dir: &str, name: &str, cov: &mut Coverage) -> Result<(), String> {
    let fails = |why: String| {
        Err(format!(
            "a create and a MKDIR of {name} at once (wire.md 2.8.1): {why}"
        ))
    };
    let create = OwnedRequest::Open {
        root: RootId(0),
        flags: OpenFlags::WRITE | OpenFlags::CREATE,
        path: name.to_string(),
    };
    let mkdir = OwnedRequest::Mkdir {
        root: RootId(0),
        mode: 0o755,
        path: name.to_string(),
    };
    let got = together(ring, &[create, mkdir], cov)?;
    let file = match &got[0] {
        Ok(OwnedReply::Open(o)) if !o.is_dir => {
            let _ = alone(ring, &OwnedRequest::Close { fh: o.fh }, cov)?;
            true
        }
        Err(Status::IsDir | Status::IoError) => false,
        other => return fails(format!("the create got {}", render_got(other))),
    };
    let made_dir = match &got[1] {
        Ok(OwnedReply::Empty) => true,
        Err(Status::IoError | Status::Exists) => false,
        other => return fails(format!("the MKDIR got {}", render_got(other))),
    };
    if file == made_dir {
        return fails(format!(
            "the create {} and the MKDIR {}: one of them, never both or neither",
            render_got(&got[0]),
            render_got(&got[1])
        ));
    }
    let attr = alone(
        ring,
        &OwnedRequest::GetAttr {
            root: RootId(0),
            path: name.to_string(),
        },
        cov,
    )?;
    if !matches!(&attr, Ok(OwnedReply::GetAttr(a)) if a.found && a.is_dir == made_dir) {
        return fails(format!(
            "the {} won, then GETATTR {}",
            if file { "create" } else { "MKDIR" },
            render_got(&attr)
        ));
    }
    let leaf = name.rsplit('/').next().unwrap_or(name);
    match alone(
        ring,
        &OwnedRequest::ReadDir {
            root: RootId(0),
            path: dir.to_string(),
        },
        cov,
    )? {
        Ok(OwnedReply::ReadDir(es)) => {
            let n = es.iter().filter(|e| e.name == leaf).count();
            if n != 1 {
                return fails(format!("READDIR of {dir} lists it {n} times"));
            }
        }
        other => return fails(format!("READDIR of {dir} got {}", render_got(&other))),
    }
    Ok(())
}

/// A create of `from/x` in flight with a RENAME of the directory `from` to
/// `to`: the file lands under `to` (the create first) or nowhere (the
/// RENAME first, the create's parent missing), never under `from`, which
/// is gone (wire.md 2.8.1).
fn parent_meet(ring: &Attached, from: &str, to: &str, cov: &mut Coverage) -> Result<(), String> {
    let fails = |why: String| {
        Err(format!(
            "a create in {from} with a RENAME of it to {to} at once (wire.md 2.8.1): {why}"
        ))
    };
    let made = OwnedRequest::Mkdir {
        root: RootId(0),
        mode: 0o755,
        path: from.to_string(),
    };
    alone(ring, &made, cov)?.map_err(|st| format!("{made:?}: {st:?}"))?;
    let create = OwnedRequest::Open {
        root: RootId(0),
        flags: OpenFlags::WRITE | OpenFlags::CREATE,
        path: format!("{from}/x"),
    };
    let rename = OwnedRequest::Rename {
        root: RootId(0),
        from: from.to_string(),
        to: to.to_string(),
    };
    let got = together(ring, &[create, rename], cov)?;
    if got[1] != Ok(OwnedReply::Empty) {
        return fails(format!("the RENAME got {}", render_got(&got[1])));
    }
    let created = match &got[0] {
        Ok(OwnedReply::Open(o)) => {
            let _ = alone(ring, &OwnedRequest::Close { fh: o.fh }, cov)?;
            true
        }
        Err(Status::NotFound) => false,
        other => return fails(format!("the create got {}", render_got(other))),
    };
    let found = |p: String, cov: &mut Coverage| -> Result<bool, String> {
        match alone(
            ring,
            &OwnedRequest::GetAttr {
                root: RootId(0),
                path: p.clone(),
            },
            cov,
        )? {
            Ok(OwnedReply::GetAttr(a)) => Ok(a.found),
            Err(Status::NotFound) => Ok(false),
            other => Err(format!("GETATTR {p} got {}", render_got(&other))),
        }
    };
    let (old, new, old_dir) = (
        found(format!("{from}/x"), cov)?,
        found(format!("{to}/x"), cov)?,
        found(from.to_string(), cov)?,
    );
    if old || old_dir || new != created {
        return fails(format!(
            "the create {}, then {from} {}, {from}/x {}, {to}/x {}",
            render_got(&got[0]),
            if old_dir { "found" } else { "missing" },
            if old { "found" } else { "missing" },
            if new { "found" } else { "missing" },
        ));
    }
    Ok(())
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

    use aether_protocol::Status;

    fn step(request: OwnedRequest, expect: Expect) -> Step {
        Step {
            request,
            expect,
            cite: None,
            note: None,
            client: 0,
            literal_fh: false,
            choice: None,
        }
    }

    /// An OPEN that may answer a handle or an error: the director's choice
    /// decides whether later steps have a handle to send, and what the
    /// tree holds, so the client's case stops after it. A definite OPEN, a
    /// use after CLOSE and a never-issued number are handled as any other step.
    #[test]
    fn a_case_stops_after_an_open_that_may_fail() {
        let open = |path: &str| OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: path.into(),
        };
        let opened = |fh| {
            Expect::Reply(OwnedReply::Open(aether_protocol::OpenResp {
                fh,
                ..Default::default()
            }))
        };
        let read = |fh| OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh,
            offset: 0,
            len: 4,
        };
        let data = Expect::Reply(OwnedReply::Read(b"abcd".to_vec()));
        let ok = Expect::Reply(OwnedReply::Empty);
        let steps = vec![
            step(open("a"), opened(1)),
            step(read(1), data.clone()),
            step(read(9), Expect::Status(Status::BadFh)),
            step(OwnedRequest::Close { fh: 1 }, ok.clone()),
            step(read(1), Expect::Status(Status::BadFh)),
            step(
                open("b"),
                Expect::OneOf(vec![opened(2), Expect::Status(Status::NotFound)]),
            ),
            step(read(2), data.clone()),
            step(OwnedRequest::Heartbeat, ok.clone()),
        ];
        let kept: Vec<&Step> = without_stand_ins(&steps);
        let want: Vec<&Step> = [0, 1, 3, 5].iter().map(|&i| &steps[i]).collect();
        assert_eq!(kept, want);

        // A one_of of handles alone is definite enough: each is a handle.
        let steps = vec![
            step(open("c"), Expect::OneOf(vec![opened(3), opened(4)])),
            step(read(3), data),
        ];
        assert_eq!(without_stand_ins(&steps).len(), 2);
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
