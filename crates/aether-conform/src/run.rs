//! `aether-conform run`: plays every scenario of a folder against a director
//! under test, through its adapter ([`crate::adapter`]).
//!
//! For each scenario, in file-name order: mount it, attach to the ring the
//! mount's environment names ([`crate::attach`]), send HEARTBEAT as the shim
//! does once attached, play the steps through [`aether_rpc::Client`],
//! compare each answer with the step's expectation ([`Session::check`]),
//! detach, and unmount. Every step is played even after one fails, so the
//! report shows them all; a later failure may follow from an earlier one.
//! A ring that stops answering ends that scenario. An adapter error
//! ([`AdapterError`]) fails the scenario being run and ends the run. After
//! the files, a run sends the malformed requests ([`crate::malformed`]) and
//! plays generated cases ([`crate::generate`]) when asked, through the same
//! adapter; each adds one entry to the report.
//!
//! READ goes through the ring's held-slot calls, as the shim's reads do, so
//! a bulk reply's data is copied out of the arena before the slot is freed
//! (`docs/wire.md` 2.6); a director may answer any READ in bulk (3.4.4).
//! A step that asks for `read_bulk` on a ring with no arena is sent without
//! the flag, as the shim never sets it then (2.6), and its note says so.
//!
//! Around every step the runner also reads the registry generation the
//! director publishes in the ring header (`reg_gen`, wire.md 4.5), which no
//! reply carries, and fails the step that breaks one of its rules
//! ([`RegGen`]): in a session that serves a registry it is non-zero before
//! the first registry request; a successful registry write moves it forward
//! before its reply is published, so it is greater right after the reply
//! than right before the request; and it never goes backward.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aether_protocol::{Opcode, Reply, Request, Status};
use aether_rpc::CallError;

use crate::adapter::{Adapter, AdapterError};
use crate::attach::Attached;
use crate::compare::{render_expect, render_got, Session};
use crate::generate::{GenerateError, GenerateOptions};
use crate::scenario::{self, LoadError, OwnedReply, OwnedRequest, Scenario};

/// What `run` was asked to do.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The adapter's command line.
    pub director: String,
    /// The folder of `*.json` scenarios.
    pub scenarios: PathBuf,
    /// How long each adapter answer, and each ring request, may take.
    pub timeout: Duration,
    /// Also send the malformed requests ([`crate::malformed`]).
    pub malformed: bool,
    /// Also play the requests sized from the ring ([`crate::limits`]).
    pub limits: bool,
    /// Also play generated cases ([`crate::generate`]).
    pub generate: Option<GenerateOptions>,
    /// Also play rounds of clients at once ([`crate::concurrent`]).
    pub concurrent: Option<crate::concurrent::ConcurrentOptions>,
}

/// One step as played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// The step's position in the scenario, from 0.
    pub index: usize,
    /// The request as the scenario wrote it, in the scenario format
    /// ([`crate::scenario::request_json`]); for a malformed request, which
    /// that format cannot say, a string describing it.
    pub request: serde_json::Value,
    pub expected: String,
    pub got: String,
    pub passed: bool,
    /// Why it failed, what was changed on the way, and the scenario's note.
    pub note: Option<String>,
}

/// One scenario as played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioResult {
    pub name: String,
    pub file: PathBuf,
    pub passed: bool,
    pub steps: Vec<StepResult>,
    /// What stopped the scenario before or between its steps: a refused
    /// mount, a failed attach, a ring that stopped answering, an adapter
    /// error.
    pub error: Option<String>,
}

/// A whole run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub scenarios: Vec<ScenarioResult>,
    /// Set when the adapter failed and the run ended early.
    pub adapter_error: Option<String>,
    /// The generator's seed, for a run with generated cases.
    pub seed: Option<u64>,
}

impl RunReport {
    pub fn passed(&self) -> bool {
        self.adapter_error.is_none() && self.scenarios.iter().all(|s| s.passed)
    }
}

/// Why a run could not start.
#[derive(Debug)]
pub enum RunError {
    /// A scenario file did not load, or the folder had none.
    Scenarios(String),
    /// The adapter could not be started.
    Adapter(AdapterError),
    /// The generator could not make a case: the reference answered a
    /// request otherwise than wire.md says ([`crate::model::build`]).
    Generator(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Scenarios(s) => f.write_str(s),
            RunError::Adapter(e) => write!(f, "{e}"),
            RunError::Generator(e) => f.write_str(e),
        }
    }
}

/// The `*.json` files in `dir`, in name order (as [`scenario::load_dir`]).
pub fn scenario_files(dir: &Path) -> Result<Vec<PathBuf>, LoadError> {
    let entries = std::fs::read_dir(dir).map_err(|e| LoadError {
        file: dir.to_path_buf(),
        step: None,
        field: String::new(),
        message: e.to_string(),
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    Ok(files)
}

/// Runs every scenario in `opts.scenarios` against the adapter.
///
/// A scenario's clients other than 0 are processes of this program
/// (`std::env::current_exe()`, run as `client-worker`), so a caller of this
/// function other than the `aether-conform` binary can play only scenarios
/// whose steps are all client 0's.
pub fn run(opts: &RunOptions) -> Result<RunReport, RunError> {
    let files = scenario_files(&opts.scenarios).map_err(|e| RunError::Scenarios(e.to_string()))?;
    let scenarios: Vec<(PathBuf, Scenario)> = files
        .into_iter()
        .map(|f| scenario::load(&f).map(|s| (f, s)))
        .collect::<Result<_, _>>()
        .map_err(|e| RunError::Scenarios(e.to_string()))?;
    if scenarios.is_empty() {
        return Err(RunError::Scenarios(format!(
            "no scenarios (*.json) in {}",
            opts.scenarios.display()
        )));
    }
    let mut adapter = Adapter::start(&opts.director, opts.timeout).map_err(RunError::Adapter)?;
    let mut report = RunReport {
        scenarios: Vec::new(),
        adapter_error: None,
        seed: opts
            .generate
            .as_ref()
            .map(|g| g.seed)
            .or(opts.concurrent.as_ref().map(|c| c.seed)),
    };
    for (file, s) in &scenarios {
        let mut result = ScenarioResult {
            name: s.name.clone(),
            file: file.clone(),
            passed: false,
            steps: Vec::new(),
            error: None,
        };
        match one(&mut adapter, file, s, opts.timeout, &mut result) {
            Ok(()) => {
                result.passed = result.error.is_none() && result.steps.iter().all(|s| s.passed);
                report.scenarios.push(result);
            }
            Err(e) => {
                result.error = Some(e.to_string());
                report.scenarios.push(result);
                report.adapter_error = Some(e.to_string());
                return Ok(report);
            }
        }
    }
    if opts.malformed {
        match crate::malformed::run(&mut adapter, opts.timeout) {
            Ok(r) => report.scenarios.push(r),
            Err(e) => {
                report.adapter_error = Some(e.to_string());
                return Ok(report);
            }
        }
    }
    if opts.limits {
        match crate::limits::run(&mut adapter, opts.timeout) {
            Ok(r) => report.scenarios.push(r),
            Err(e) => {
                report.adapter_error = Some(e.to_string());
                return Ok(report);
            }
        }
    }
    if let Some(g) = &opts.generate {
        match crate::generate::run(&mut adapter, g, opts.timeout) {
            Ok(r) => report.scenarios.push(r),
            Err(GenerateError::Adapter(e)) => {
                report.adapter_error = Some(e.to_string());
                return Ok(report);
            }
            Err(GenerateError::Generator(e)) => return Err(RunError::Generator(e)),
        }
    }
    if let Some(c) = &opts.concurrent {
        match crate::concurrent::run(&mut adapter, c, opts.timeout) {
            Ok(r) => report.scenarios.push(r),
            Err(GenerateError::Adapter(e)) => {
                report.adapter_error = Some(e.to_string());
                return Ok(report);
            }
            Err(GenerateError::Generator(e)) => return Err(RunError::Generator(e)),
        }
    }
    adapter.finish();
    Ok(report)
}

/// Mounts `s` from `file` (which holds it), plays it and unmounts it. `Err`
/// only for an adapter error; everything else is in the result.
pub(crate) fn play_file(
    adapter: &mut Adapter,
    file: &Path,
    s: &Scenario,
    timeout: Duration,
) -> Result<ScenarioResult, AdapterError> {
    let mut result = ScenarioResult {
        name: s.name.clone(),
        file: file.to_path_buf(),
        passed: false,
        steps: Vec::new(),
        error: None,
    };
    one(adapter, file, s, timeout, &mut result)?;
    result.passed = result.error.is_none() && result.steps.iter().all(|s| s.passed);
    Ok(result)
}

/// Mounts, plays and unmounts one scenario. `Err` only for an adapter error;
/// everything else lands in `out`.
fn one(
    adapter: &mut Adapter,
    file: &Path,
    s: &Scenario,
    timeout: Duration,
    out: &mut ScenarioResult,
) -> Result<(), AdapterError> {
    let env = match adapter.mount(file)? {
        Ok(env) => env,
        Err(why) => {
            out.error = Some(format!("mount refused: {why}"));
            return Ok(());
        }
    };
    match Attached::from_env(&env, timeout) {
        Ok(ring) => {
            if let Err(e) = play(&ring, &env, timeout, s, &mut out.steps) {
                out.error = Some(format!("{}: {e}", ring.describe()));
            }
            // What the environment says of the tree, beyond what attaching
            // needs (`attach::against_tree`).
            if let Err(e) = crate::attach::against_tree(&env, &s.tree) {
                let e = format!("the mount's environment: {e}");
                out.error = Some(match out.error.take() {
                    Some(before) => format!("{before}; {e}"),
                    None => e,
                });
            }
            // Detached before the unmount, so the adapter can remove the
            // ring (a mapped file cannot be removed on Windows).
            drop(ring);
        }
        Err(e) => out.error = Some(format!("attach failed: {e}")),
    }
    adapter.unmount()
}

/// What sending one request gave.
enum Sent {
    /// An answer to compare.
    Answer(Result<OwnedReply, Status>),
    /// An answer no expectation can match (an unknown status, an OK payload
    /// that does not decode, a bulk reply outside the mapping).
    Bad(String),
}

/// Plays `s`'s steps on `ring`, the steps of client 0 from this process and
/// those of every other client number from a client process of its own
/// ([`crate::clients::ProcessClient`]) attached from `env`. One session
/// pairs the handles of all of them, since handles are ring-global (wire.md
/// 2.8.1), and one [`RegGen`] reads the generation each step's own client
/// read. `Err` when the ring stopped answering.
pub(crate) fn play(
    ring: &Attached,
    env: &crate::adapter::Env,
    timeout: Duration,
    s: &Scenario,
    out: &mut Vec<StepResult>,
) -> Result<(), String> {
    let client = ring.client();
    let mut scratch = Vec::new();
    client
        .call(&Request::Heartbeat {}, &mut scratch)
        .map_err(|e| format!("the HEARTBEAT sent on attach failed: {e}"))?;
    let mut others = Others::start(env, timeout, s)?;
    let mut session = Session::new();
    let mut reg_gen = RegGen::new(s.tree.registry.is_some(), ring.reg_gen());
    for (i, step) in s.steps.iter().enumerate() {
        let mut notes: Vec<String> = Vec::new();
        let mut req = session.translate(&step.request);
        if crate::clients::fit_to_ring(ring, &mut req) {
            notes.push("sent without read_bulk: the ring has no arena (wire.md 2.6)".into());
        }
        let (sent, gen_before, gen_now) = match step.client {
            0 => {
                let before = ring.reg_gen();
                let sent = send(ring, &client, &req, &mut scratch);
                (sent, before, ring.reg_gen())
            }
            k => match others.send(k, &req) {
                Ok((sent, slot, before, after)) => {
                    notes.push(format!("sent by client {k}, in slot {slot}"));
                    (Ok(sent), before, after)
                }
                Err(e) => (Err(e), 0, 0),
            },
        };
        let sent = match sent {
            Ok(sent) => sent,
            Err(e) => {
                out.push(StepResult {
                    index: i,
                    request: crate::scenario::request_json(&step.request),
                    expected: render_expect(&step.expect),
                    got: format!("no answer: {e}"),
                    passed: false,
                    note: Some(join(notes, step.note.as_deref())),
                });
                return Err(format!("step {i}: the ring gave no answer: {e}"));
            }
        };
        // Only once there is an answer: without one there are no generations
        // read either.
        let gen_broken = reg_gen.before(&step.request, gen_before);
        let (got, mut passed) = match &sent {
            Sent::Answer(got) => {
                let r = session.check(i, &step.request, &step.expect, got);
                if let Err(m) = &r {
                    notes.insert(0, m.why.clone());
                }
                (render_got(got), r.is_ok())
            }
            Sent::Bad(why) => {
                notes.insert(0, why.clone());
                (why.clone(), false)
            }
        };
        let succeeded = matches!(sent, Sent::Answer(Ok(_)));
        let gen_after = reg_gen.after(&step.request, succeeded, gen_before, gen_now);
        if let Some(why) = gen_broken.or(gen_after) {
            notes.insert(0, why);
            passed = false;
        }
        out.push(StepResult {
            index: i,
            request: crate::scenario::request_json(&step.request),
            expected: render_expect(&step.expect),
            got,
            passed,
            note: Some(join(notes, step.note.as_deref())).filter(|n| !n.is_empty()),
        });
    }
    Ok(())
}

/// The scenario's clients other than 0, each a process of its own.
struct Others {
    clients: std::collections::BTreeMap<u32, crate::clients::ProcessClient>,
}

impl Others {
    /// Starts a client process for every client number `s`'s steps name
    /// besides 0.
    fn start(env: &crate::adapter::Env, timeout: Duration, s: &Scenario) -> Result<Others, String> {
        let wanted: std::collections::BTreeSet<u32> = s
            .steps
            .iter()
            .map(|st| st.client)
            .filter(|&k| k != 0)
            .collect();
        let mut clients = std::collections::BTreeMap::new();
        if !wanted.is_empty() {
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot find this program to start clients: {e}"))?;
            for k in wanted {
                clients.insert(
                    k,
                    crate::clients::ProcessClient::start(&exe, k, env, timeout)?,
                );
            }
        }
        Ok(Others { clients })
    }

    /// Sends `req` from client `k`: the answer, the slot it went in, and the
    /// generations the client read before and after.
    fn send(&mut self, k: u32, req: &OwnedRequest) -> Result<(Sent, u32, u64, u64), String> {
        let c = self
            .clients
            .get_mut(&k)
            .ok_or_else(|| format!("client {k} was not started"))?;
        let r = c.send(req)?;
        let sent = match r.outcome.answer {
            crate::clients::Answer::Got(a) => Sent::Answer(a),
            crate::clients::Answer::Bad(why) => Sent::Bad(why),
        };
        Ok((sent, r.outcome.slot, r.gen_before, r.gen_after))
    }
}

/// wire.md 4.5's rules for the registry generation (`reg_gen`) in the ring
/// header, checked across one session's steps from what the runner reads
/// there before each request and after each reply.
#[derive(Debug, Clone)]
pub(crate) struct RegGen {
    /// The session serves a registry (its tree has one), so the shim would
    /// send registry requests (wire.md 4.5: "a director that serves the
    /// registry must publish a non-zero generation before the shim
    /// attaches"). Without one the shim sends none, and the header may stay
    /// 0, as Cairn's does.
    serves: bool,
    /// The last generation read.
    last: u64,
    /// Whether a registry request has been sent yet.
    asked: bool,
}

impl RegGen {
    /// The rules for a session that serves a registry or not, with `now` the
    /// generation read once attached.
    pub(crate) fn new(serves: bool, now: u64) -> RegGen {
        RegGen {
            serves,
            last: now,
            asked: false,
        }
    }

    /// Before `req` is sent, with `now` read from the header: why that
    /// breaks a rule, if it does.
    pub(crate) fn before(&mut self, req: &OwnedRequest, now: u64) -> Option<String> {
        let backward = self.seen(now);
        if backward.is_some() {
            return backward;
        }
        if self.serves && !self.asked && is_registry(req) {
            self.asked = true;
            if now == 0 {
                return Some(
                    "reg_gen in the ring header is 0 at the session's first registry request, \
                     so the shim would cache nothing (wire.md 4.5: a director that serves the \
                     registry must publish a non-zero generation before the shim attaches)"
                        .into(),
                );
            }
        }
        None
    }

    /// After the reply to `req` (`succeeded` when it was OK), with `before`
    /// read before the request and `now` after the reply.
    pub(crate) fn after(
        &mut self,
        req: &OwnedRequest,
        succeeded: bool,
        before: u64,
        now: u64,
    ) -> Option<String> {
        let backward = self.seen(now);
        if backward.is_some() {
            return backward;
        }
        if succeeded && is_registry_write(req) && now <= before {
            return Some(format!(
                "reg_gen in the ring header was {before} before this successful registry write \
                 and {now} after its reply, so other processes would keep serving stale \
                 answers (wire.md 4.5: it moves on every successful registry write, before \
                 the write's reply is published)"
            ));
        }
        None
    }

    /// Notes `now` as the latest generation; why, if it went backward.
    fn seen(&mut self, now: u64) -> Option<String> {
        let last = std::mem::replace(&mut self.last, now);
        (now < last).then(|| {
            format!(
                "reg_gen in the ring header went back from {last} to {now} (wire.md 4.5: \
                 \"never backward\")"
            )
        })
    }
}

/// The registry opcodes (wire.md 4.3).
fn is_registry(req: &OwnedRequest) -> bool {
    use OwnedRequest as O;
    matches!(
        req,
        O::RegLookup { .. } | O::RegKey { .. } | O::RegChanged { .. }
    ) || is_registry_write(req)
}

/// The registry writes (wire.md 3.3, 4.3: REG_SET_VALUE to REG_RENAME_KEY),
/// each of which moves the generation when it succeeds (4.5).
fn is_registry_write(req: &OwnedRequest) -> bool {
    use OwnedRequest as O;
    matches!(
        req,
        O::RegSetValue { .. }
            | O::RegDeleteValue { .. }
            | O::RegCreateKey { .. }
            | O::RegDeleteKey { .. }
            | O::RegRenameKey { .. }
    )
}

fn join(mut notes: Vec<String>, scenario_note: Option<&str>) -> String {
    if let Some(n) = scenario_note {
        notes.push(n.to_string());
    }
    notes.join("; ")
}

/// Sends `req`. `Err` when the ring gave no answer at all.
fn send<N: aether_ipc::Notifier>(
    ring: &Attached,
    client: &aether_rpc::Client<'_, N>,
    req: &OwnedRequest,
    scratch: &mut Vec<u8>,
) -> Result<Sent, String> {
    let request = req.as_request();
    if request.opcode() == Opcode::Read {
        return read(ring, client, &request);
    }
    Ok(match client.call(&request, scratch) {
        Ok(reply) => Sent::Answer(Ok(OwnedReply::from_reply(&reply))),
        Err(CallError::Status(s)) => Sent::Answer(Err(s)),
        Err(CallError::UnknownStatus(n)) => Sent::Bad(format!(
            "status {n}, which the protocol table does not have"
        )),
        Err(CallError::Undecodable) => Sent::Bad(
            "an OK reply whose payload does not decode as this opcode's reply (wire.md 3.1)".into(),
        ),
        Err(CallError::Ipc(e)) => return Err(format!("{e:?}")),
        Err(CallError::BulkRead) => unreachable!("READ is sent on a held slot"),
    })
}

/// A READ on a held slot: the reply, and a bulk reply's data, are read
/// before the slot goes back.
fn read<N: aether_ipc::Notifier>(
    ring: &Attached,
    client: &aether_rpc::Client<'_, N>,
    request: &Request<'_>,
) -> Result<Sent, String> {
    let mut body = Vec::new();
    request.encode(&mut body);
    let flags = request.flags();
    let (mut replies, slots) = client
        .ring()
        .submit_many_held(&[(Opcode::Read as u32, flags, body)])
        .map_err(|e| format!("{e:?}"))?;
    let reply = replies.pop().expect("one reply per request");
    let sent = if reply.status != Status::Ok.code() {
        match Status::try_from(reply.status) {
            Ok(s) => Sent::Answer(Err(s)),
            Err(_) => Sent::Bad(format!(
                "status {}, which the protocol table does not have",
                reply.status
            )),
        }
    } else {
        match Reply::decode(Opcode::Read, flags, &reply.payload) {
            Ok(r) => match OwnedReply::from_reply(&r) {
                OwnedReply::ReadBulk { len, arena_offset } => {
                    let end = arena_offset.saturating_add(u64::from(len));
                    match ring.seg().read_bytes(arena_offset as usize, len as usize) {
                        Some(data) if end <= ring.ring_bytes() as u64 => {
                            Sent::Answer(Ok(OwnedReply::Read(data)))
                        }
                        _ => Sent::Bad(format!(
                            "a bulk reply of {len} bytes at {arena_offset}, outside the first \
                             {} bytes of the mapping (wire.md 2.6)",
                            ring.ring_bytes()
                        )),
                    }
                }
                r => Sent::Answer(Ok(r)),
            },
            Err(_) => Sent::Bad(
                "an OK reply whose payload does not decode as a READ reply (wire.md 3.1)".into(),
            ),
        }
    };
    client.ring().release_slots(&slots);
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create() -> OwnedRequest {
        OwnedRequest::RegCreateKey {
            path: r"\Registry\Machine\Software\T".into(),
            volatile: false,
        }
    }

    fn lookup() -> OwnedRequest {
        OwnedRequest::RegLookup {
            path: r"\Registry\Machine\Software\T".into(),
        }
    }

    #[test]
    fn a_zero_generation_fails_the_first_registry_request_of_a_session_serving_one() {
        let mut g = RegGen::new(true, 0);
        assert_eq!(g.before(&OwnedRequest::Heartbeat, 0), None);
        let why = g.before(&lookup(), 0).expect("a failure");
        assert!(why.contains("wire.md 4.5"), "{why}");
        // Once, at the first registry request.
        assert_eq!(g.before(&lookup(), 0), None);
        // Without a registry the header may stay 0 (Cairn's does).
        let mut g = RegGen::new(false, 0);
        assert_eq!(g.before(&lookup(), 0), None);
    }

    #[test]
    fn a_successful_registry_write_must_move_the_generation() {
        let mut g = RegGen::new(true, 1);
        assert_eq!(g.before(&create(), 1), None);
        let why = g.after(&create(), true, 1, 1).expect("a failure");
        assert!(why.contains("wire.md 4.5"), "{why}");
        assert_eq!(g.after(&create(), true, 1, 2), None);
        // A refused write, and a read, need not move it.
        assert_eq!(g.after(&create(), false, 2, 2), None);
        assert_eq!(g.after(&lookup(), true, 2, 2), None);
    }

    #[test]
    fn the_generation_never_goes_backward() {
        let mut g = RegGen::new(false, 5);
        let why = g.before(&OwnedRequest::Heartbeat, 4).expect("a failure");
        assert!(why.contains("never backward"), "{why}");
        let mut g = RegGen::new(false, 5);
        assert_eq!(g.before(&OwnedRequest::Heartbeat, 5), None);
        assert!(g.after(&OwnedRequest::Heartbeat, true, 5, 3).is_some());
    }
}
