//! `aether-conform bench`: what each opcode costs a director, over the cost
//! of the ring itself.
//!
//! For each workload ([`crate::workload`]) the director's adapter mounts the
//! workload's tree (written as a scenario file with no steps, so any adapter
//! of the protocol in [`crate::adapter`] can mount it), and the bench
//! attaches to the ring as a run does ([`Attached`]). Each phase is then
//! played once as a warm-up, not recorded, and once recorded: `threads x
//! clients` client threads at once, each with its own ring client, sending
//! the requests [`Workload::expand`] gives it. Every request is timed from
//! before it is sent until its reply is decoded (a bulk READ's data copied
//! out of the arena, as the shim copies it) into a histogram per opcode
//! ([`hdrhistogram`], in nanoseconds from 1 ns to 60 s, three significant
//! figures).
//!
//! Then the same requests go to the floor: by default this program's
//! `null-adapter`, on a ring of the same kind and the same geometry as the
//! director's (slots, payload cap, arena, worker threads). It answers every
//! request OK with the smallest reply ([`aether_refdirector::Null`]), except
//! READ: each recorded READ goes to the floor asking for the bytes the
//! director's reply to it gave, with `FLAG_READ_BULK` set exactly when that
//! reply came in bulk, and the floor answers that many bytes in that form
//! (zeros, written into the slot or into its bank, and copied out by the
//! bench as the director's were). So the floor includes the ring's round
//! trip, the request's and reply's slot copies and a READ's data movement,
//! and leaves out only what the director does to answer: each opcode's p50,
//! p99 and p99.9 are reported for both and as the director's minus the
//! floor's.
//!
//! A workload whose director answers a request with anything but what the
//! phase expects (a status other than OK, a GETATTR of a file of the tree
//! with `found = 0`, a READ shorter than the file has, a WRITE that took
//! fewer bytes, a listing of another length) stops with an error and no
//! numbers: a benchmark of errors is not a benchmark. The floor is checked
//! for OK statuses only.
//!
//! The report:
//!
//! ```text
//! { "machine": { "cpu", "cores", "os", "build": { "profile", "rustc", "git" } },
//!   "director": "<cmd>", "floor": "<cmd>", "clients": 1, "quick": false,
//!   "workloads": [
//!     { "name",
//!       "geometry": { "ring": "file", "slots", "payload_cap", "arena_len", "worker_hint" },
//!       "floor": "<cmd>", "floor_geometry": { ... },
//!       "phases": [
//!         { "name", "threads", "requests", "seconds", "requests_per_s", "read_bytes",
//!           "bulk_bytes", "bulk_mib_s",
//!           "floor": { "seconds", "requests_per_s", "read_bytes", "bulk_bytes" },
//!           "per_op": { "<op>": { "count", "p50_us", "p99_us", "p999_us", "max_us",
//!                                 "mean_us", "floor_p50_us", "floor_p99_us",
//!                                 "floor_p999_us", "floor_max_us", "over_floor_p50_us",
//!                                 "over_floor_p99_us", "over_floor_p999_us" } } } ] },
//!     { "name", "error": "..." } ] }
//! ```
//!
//! Each workload is mounted on a ring of its own, so the geometry, the
//! floor's command line and the floor's geometry are the workload's; the
//! top-level `floor` is the command line asked for (`--floor`, or the
//! null adapter). A workload whose floor failed still names the director's
//! geometry and the floor it ran.
//!
//! `read_bytes` is the bytes READs returned, `bulk_bytes` those that came in
//! bulk, and `bulk_mib_s` the bulk bytes over the phase's seconds (null when
//! the phase sent none); the floor's are the same counts on its side. Timed numbers are this machine's: a
//! comparison ([`compare`]) refuses a baseline from another machine, and
//! nothing fails a run on them unless `--fail-over` asks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use aether_protocol::{Opcode, ReadReply, Reply, Request};
use hdrhistogram::Histogram;
use serde_json::{json, Value};

use crate::adapter::{Adapter, AdapterError, Env};
use crate::attach::Attached;
use crate::machine;
use crate::scenario::OwnedRequest;
use crate::workload::{self, Entry, Phase, PhaseKind, Workload};

/// What `--quick` leaves of each thread's requests: one repeat, and at most
/// this many requests (and the CLOSEs of the handles still open there).
pub const QUICK_CAP: usize = 64;

/// The histograms' range, in nanoseconds, and their precision.
const LOWEST_NS: u64 = 1;
const HIGHEST_NS: u64 = 60_000_000_000;
const SIGFIG: u8 = 3;

/// What a bench run is asked to do.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// The director's adapter command line.
    pub director: String,
    /// The workloads folder.
    pub workloads: PathBuf,
    /// The workloads to run, by name; empty runs all.
    pub only: Vec<String>,
    /// The floor's adapter command line; `None` is `<exe> null-adapter` on a
    /// ring like the director's.
    pub floor: Option<String>,
    /// How many client threads each of a phase's threads stands for.
    pub clients: u32,
    pub quick: bool,
    /// Every adapter answer and every request has this long.
    pub timeout: Duration,
    /// This program, for the default floor.
    pub exe: PathBuf,
}

/// Runs the bench: the report, or why it could not run at all (the
/// workloads did not load, an adapter could not be started or stopped
/// answering). A workload that failed is in the report with its error.
pub fn run(opts: &BenchOptions) -> Result<Value, String> {
    let mut all = workload::load_dir(&opts.workloads).map_err(|e| e.to_string())?;
    if !opts.only.is_empty() {
        for n in &opts.only {
            if !all.iter().any(|w| &w.name == n) {
                return Err(format!("no workload {n:?} in {}", opts.workloads.display()));
            }
        }
        all.retain(|w| opts.only.contains(&w.name));
    }
    if all.is_empty() {
        return Err(format!(
            "no workloads (*.json) in {}",
            opts.workloads.display()
        ));
    }
    let scratch = Scratch::new()?;
    let mut director = Adapter::start(&opts.director, opts.timeout).map_err(|e| e.to_string())?;
    let mut workloads = Vec::new();
    for w in &all {
        let file = scratch.path().join(format!("{}.json", w.name));
        std::fs::write(&file, crate::scenario::to_json_string(&as_scenario(w)))
            .map_err(|e| format!("write {}: {e}", file.display()))?;
        eprintln!("aether-conform: bench {}", w.name);
        let measured = match side(&mut director, &file, w, opts, None).map_err(|e| e.to_string())? {
            Ok(m) => m,
            Err(e) => {
                workloads.push(json!({ "name": w.name, "error": e }));
                continue;
            }
        };
        // A floor of its own per workload, on a ring shaped as the one the
        // director just answered on, sent the READs as the director answered
        // them.
        let cmd = opts
            .floor
            .clone()
            .unwrap_or_else(|| default_floor(&opts.exe, &measured.geometry));
        let mut floor = Adapter::start(&cmd, opts.timeout).map_err(|e| format!("floor {e}"))?;
        let floored =
            side(&mut floor, &file, w, opts, Some(&measured)).map_err(|e| format!("floor {e}"))?;
        floor.finish();
        match floored {
            Ok(f) => workloads.push(workload_json(w, &cmd, &measured, &f)),
            Err(e) => workloads.push(json!({
                "name": w.name,
                "geometry": measured.geometry,
                "floor": cmd,
                "error": format!("floor: {e}"),
            })),
        }
    }
    director.finish();
    let floor = opts
        .floor
        .clone()
        .unwrap_or_else(|| format!("\"{}\" null-adapter", opts.exe.display()));
    Ok(json!({
        "machine": machine::machine(),
        "director": opts.director,
        "floor": floor,
        "clients": opts.clients,
        "quick": opts.quick,
        "workloads": workloads,
    }))
}

/// The folder the workloads' scenario files are written to, removed when
/// the run ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("aether-conform-bench-{}-{t}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The scenario an adapter mounts for `w`: its tree and no steps.
fn as_scenario(w: &Workload) -> crate::Scenario {
    crate::Scenario {
        name: w.name.clone(),
        description: w.description.clone(),
        tree: w.tree.clone(),
        steps: Vec::new(),
    }
}

/// `<exe> null-adapter` on a ring of `geometry`'s kind and shape.
fn default_floor(exe: &Path, geometry: &Value) -> String {
    format!(
        "\"{}\" null-adapter --ring {} --slots {} --payload-cap {} --arena-len {} --workers {}",
        exe.display(),
        geometry["ring"].as_str().unwrap_or("file"),
        geometry["slots"],
        geometry["payload_cap"],
        geometry["arena_len"],
        geometry["worker_hint"]
            .as_u64()
            .filter(|&n| n > 0)
            .unwrap_or(4),
    )
}

/// One side's measurements of a workload: the ring's geometry and each
/// phase's numbers.
struct Measured {
    geometry: Value,
    phases: Vec<PhaseOut>,
}

/// A phase's numbers: a histogram per opcode, the wall time, the requests
/// and the bytes bulk READs returned, and each thread's READ replies.
struct PhaseOut {
    ops: BTreeMap<&'static str, Histogram<u64>>,
    seconds: f64,
    requests: u64,
    bulk_bytes: u64,
    threads: u32,
    reads: Vec<Shape>,
}

/// One thread's READ replies in the order sent: the bytes each gave and
/// whether it came in bulk.
type Shape = Vec<(u32, bool)>;

/// Mounts `file` in `adapter`, plays every phase of `w` (warm-up, then
/// recorded) and unmounts. `floor_of` is `None` for the director, whose
/// replies are checked (`strict`), and for the floor the director's
/// measurements: each recorded READ goes to the floor with the length and
/// the form the director's reply to it had, so the floor moves the same
/// bytes the same way. The outer `Err` is an adapter that can be asked
/// nothing more; the inner one a workload that failed.
fn side(
    adapter: &mut Adapter,
    file: &Path,
    w: &Workload,
    opts: &BenchOptions,
    floor_of: Option<&Measured>,
) -> Result<Result<Measured, String>, AdapterError> {
    let env = match adapter.mount(file)? {
        Ok(env) => env,
        Err(why) => return Ok(Err(format!("mount refused: {why}"))),
    };
    let result = match Attached::from_env(&env, opts.timeout) {
        Ok(ring) => {
            let r = play_all(&ring, &env, w, opts, floor_of);
            // Detached before the unmount, so the adapter can remove the
            // ring (a mapped file cannot be removed on Windows).
            drop(ring);
            r
        }
        Err(e) => Err(format!("attach failed: {e}")),
    };
    adapter.unmount()?;
    Ok(result)
}

fn play_all(
    ring: &Attached,
    env: &Env,
    w: &Workload,
    opts: &BenchOptions,
    floor_of: Option<&Measured>,
) -> Result<Measured, String> {
    let strict = floor_of.is_none();
    let geometry = geometry(ring, env);
    fits(ring, w)?;
    for p in &w.phases {
        let warm = Phase {
            repeat: 1,
            ..p.clone()
        };
        run_phase(ring, w, &warm, opts, strict, None).map_err(|e| format!("warm-up: {e}"))?;
    }
    let mut phases = Vec::new();
    for (i, p) in w.phases.iter().enumerate() {
        let p = if opts.quick {
            Phase {
                repeat: 1,
                ..p.clone()
            }
        } else {
            p.clone()
        };
        let shape = floor_of.and_then(|d| d.phases.get(i)).map(|d| &d.reads[..]);
        phases.push(run_phase(ring, w, &p, opts, strict, shape)?);
    }
    Ok(Measured { geometry, phases })
}

/// The ring's kind and shape, as the mount's environment and its header
/// give them.
fn geometry(ring: &Attached, env: &Env) -> Value {
    let file = env
        .get(aether_env::RING_PATH)
        .is_some_and(|p| !p.is_empty());
    let g = ring.geom();
    json!({
        "ring": if file { "file" } else { "section" },
        "slots": g.slot_count,
        "payload_cap": g.payload_cap,
        "arena_len": ring.arena_len(),
        "worker_hint": aether_ipc::ring::worker_hint(ring.seg()),
    })
}

/// Refuses a workload the ring cannot carry: a WRITE or an inline READ past
/// the payload cap, or a bulk READ on a ring with no arena.
fn fits(ring: &Attached, w: &Workload) -> Result<(), String> {
    let cap = u64::from(ring.geom().payload_cap);
    for p in &w.phases {
        let why = match &p.kind {
            // wire.md 3.4.5: a WRITE request is 24 bytes and its data.
            PhaseKind::Write { chunk, .. } if u64::from(*chunk) + 24 > cap => Some(format!(
                "its WRITEs of {chunk} bytes do not fit the payload cap {cap}"
            )),
            PhaseKind::SequentialRead { chunk, bulk, .. } if *bulk && ring.arena_len() == 0 => {
                let _ = chunk;
                Some("it asks for bulk READs and the ring has no arena".to_string())
            }
            PhaseKind::SequentialRead {
                chunk, bulk: false, ..
            }
            | PhaseKind::RandomRead { len: chunk, .. }
                if u64::from(*chunk) + 8 > cap =>
            {
                Some(format!(
                    "its inline READs of {chunk} bytes do not fit the payload cap {cap}"
                ))
            }
            _ => None,
        };
        if let Some(why) = why {
            return Err(format!("phase {}: {why}", p.name));
        }
    }
    Ok(())
}

/// Plays one phase on `threads x clients` threads and gathers their numbers.
/// With `shape`, thread `t` sends its READs as `shape[t]` says.
fn run_phase(
    ring: &Attached,
    w: &Workload,
    p: &Phase,
    opts: &BenchOptions,
    strict: bool,
    shape: Option<&[Shape]>,
) -> Result<PhaseOut, String> {
    let total = p.threads.saturating_mul(opts.clients.max(1));
    let streams: Vec<Vec<OwnedRequest>> = (0..total)
        .map(|t| {
            let mut v: Vec<OwnedRequest> = w.expand(p, t).collect();
            if opts.quick {
                cap(&mut v, QUICK_CAP);
            }
            v
        })
        .collect();
    let barrier = Barrier::new(streams.len());
    let outs: Vec<Result<ThreadOut, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = streams
            .iter()
            .enumerate()
            .map(|(t, reqs)| {
                let barrier = &barrier;
                let shape = shape.and_then(|s| s.get(t)).map(|s| &s[..]);
                s.spawn(move || play(ring, w, reqs, strict, shape, barrier))
            })
            .collect();
        handles
            .into_iter()
            .enumerate()
            .map(|(t, h)| {
                h.join()
                    .unwrap_or_else(|_| Err("the client thread panicked".into()))
                    .map_err(|e| format!("phase {}, thread {t}: {e}", p.name))
            })
            .collect()
    });
    let mut ops: BTreeMap<&'static str, Histogram<u64>> = BTreeMap::new();
    let (mut start, mut end) = (None::<Instant>, None::<Instant>);
    let (mut requests, mut bulk_bytes) = (0, 0);
    let mut reads = Vec::new();
    for o in outs {
        let o = o?;
        reads.push(o.reads);
        for (op, h) in o.ops {
            ops.entry(op.name())
                .or_insert_with(histogram)
                .add(&h)
                .map_err(|e| format!("merging histograms: {e}"))?;
        }
        start = Some(start.map_or(o.start, |s| s.min(o.start)));
        end = Some(end.map_or(o.end, |e| e.max(o.end)));
        requests += o.requests;
        bulk_bytes += o.bulk_bytes;
    }
    let seconds = match (start, end) {
        (Some(s), Some(e)) => e.duration_since(s).as_secs_f64(),
        _ => 0.0,
    };
    Ok(PhaseOut {
        ops,
        seconds,
        requests,
        bulk_bytes,
        threads: total,
        reads,
    })
}

/// Cuts `reqs` to its first `n` requests and closes the handles still open
/// there.
fn cap(reqs: &mut Vec<OwnedRequest>, n: usize) {
    if reqs.len() <= n {
        return;
    }
    reqs.truncate(n);
    let mut open = std::collections::BTreeSet::new();
    let mut next = 0u64;
    for r in reqs.iter() {
        match r {
            OwnedRequest::Open { .. } => {
                next += 1;
                open.insert(next);
            }
            OwnedRequest::Close { fh } => {
                open.remove(fh);
            }
            _ => {}
        }
    }
    reqs.extend(open.into_iter().map(|fh| OwnedRequest::Close { fh }));
}

fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(LOWEST_NS, HIGHEST_NS, SIGFIG).expect("valid histogram bounds")
}

/// One client thread's numbers.
struct ThreadOut {
    ops: Vec<(Opcode, Histogram<u64>)>,
    start: Instant,
    end: Instant,
    requests: u64,
    bulk_bytes: u64,
    reads: Shape,
}

/// What a reply said, as much as the checks need.
enum Answer {
    Attr { found: bool },
    Open { fh: u64, size: u64 },
    Read { n: u32, bulk: bool },
    Written(u32),
    Listing(usize),
    Other,
}

/// Plays `reqs` from one thread, after every thread is ready. With `shape`,
/// the `j`-th READ asks for `shape[j].0` bytes, with `FLAG_READ_BULK` set
/// exactly when `shape[j].1` is.
fn play(
    ring: &Attached,
    w: &Workload,
    reqs: &[OwnedRequest],
    strict: bool,
    shape: Option<&[(u32, bool)]>,
    barrier: &Barrier,
) -> Result<ThreadOut, String> {
    let client = ring.client();
    let mut scratch = Vec::with_capacity(64 * 1024);
    let longest = reqs
        .iter()
        .map(|r| match r {
            OwnedRequest::Read { len, .. } => *len as usize,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let mut data = vec![0u8; longest];
    // The director's handle and the size its OPEN gave, by this thread's
    // handle number (the first OPEN's is 1).
    let mut handles: Vec<(u64, u64)> = Vec::new();
    let mut ops: Vec<(Opcode, Histogram<u64>)> = Vec::new();
    let mut bulk_bytes = 0u64;
    let mut reads: Shape = Vec::new();
    let fh_of = |handles: &[(u64, u64)], local: u64| -> (u64, u64) {
        local
            .checked_sub(1)
            .and_then(|i| handles.get(i as usize))
            .copied()
            .unwrap_or((0, 0))
    };
    barrier.wait();
    let start = Instant::now();
    for (i, r) in reqs.iter().enumerate() {
        let mut request = mapped(r, |l| fh_of(&handles, l).0);
        if let (
            Some(shape),
            Request::Read {
                slot_flags, len, ..
            },
        ) = (shape, &mut request)
        {
            if let Some(&(n, bulk)) = shape.get(reads.len()) {
                *len = n;
                slot_flags.set(aether_protocol::RequestFlags::READ_BULK, bulk);
            }
        }
        let op = request.opcode();
        let at = || format!("request {i} ({} {})", op.name(), describe(r));
        let t0 = Instant::now();
        let answer = send(ring, &client, &request, &mut scratch, &mut data);
        let ns = t0.elapsed().as_nanos().min(u128::from(HIGHEST_NS)) as u64;
        let answer = answer.map_err(|e| format!("{}: {e}", at()))?;
        match ops.iter_mut().find(|(o, _)| *o == op) {
            Some((_, h)) => h.saturating_record(ns.max(LOWEST_NS)),
            None => {
                let mut h = histogram();
                h.saturating_record(ns.max(LOWEST_NS));
                ops.push((op, h));
            }
        }
        match (answer, r) {
            (Answer::Open { fh, size }, _) => {
                if strict && fh == 0 {
                    return Err(format!("{}: handle 0 (wire.md 3.4.3: never 0)", at()));
                }
                handles.push((fh, size));
            }
            (
                Answer::Read { n, bulk },
                OwnedRequest::Read {
                    fh, offset, len, ..
                },
            ) => {
                reads.push((n, bulk));
                if bulk {
                    bulk_bytes += u64::from(n);
                }
                let size = fh_of(&handles, *fh).1;
                let want = u64::from(*len).min(size.saturating_sub(*offset));
                if strict && u64::from(n) != want {
                    return Err(format!(
                        "{}: {n} bytes, where the file has {want} (its OPEN said {size} bytes)",
                        at()
                    ));
                }
            }
            (Answer::Attr { found }, _) if strict && !found => {
                return Err(format!("{}: found = 0 for a file of the tree", at()));
            }
            (Answer::Written(n), OwnedRequest::Write { data, .. })
                if strict && n as usize != data.len() =>
            {
                return Err(format!("{}: wrote {n} of {} bytes", at(), data.len()));
            }
            (Answer::Listing(n), OwnedRequest::ReadDir { path, .. }) if strict => {
                if let Some(Entry::Dir { entries }) = w.lookup(path) {
                    if n != entries {
                        return Err(format!(
                            "{}: {n} entries, where the tree has {entries}",
                            at()
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let end = Instant::now();
    Ok(ThreadOut {
        ops,
        start,
        end,
        requests: reqs.len() as u64,
        bulk_bytes,
        reads,
    })
}

/// A request's path or handle, for a message.
fn describe(r: &OwnedRequest) -> String {
    match r {
        OwnedRequest::GetAttr { path, .. }
        | OwnedRequest::ReadDir { path, .. }
        | OwnedRequest::Open { path, .. }
        | OwnedRequest::StoredNames { path, .. } => path.clone(),
        OwnedRequest::Rename { from, to, .. } => format!("{from} -> {to}"),
        OwnedRequest::Read {
            fh, offset, len, ..
        } => format!("handle {fh}, {len} bytes at {offset}"),
        OwnedRequest::Write { fh, offset, data } => {
            format!("handle {fh}, {} bytes at {offset}", data.len())
        }
        OwnedRequest::Close { fh } => format!("handle {fh}"),
        other => format!("{other:?}"),
    }
}

/// `r` to send, its handle replaced by the director's (`director(local)`).
fn mapped<'r>(r: &'r OwnedRequest, director: impl Fn(u64) -> u64) -> Request<'r> {
    match r {
        OwnedRequest::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => Request::Read {
            slot_flags: *slot_flags,
            fh: director(*fh),
            offset: *offset,
            len: *len,
        },
        OwnedRequest::Write { fh, offset, data } => Request::Write {
            fh: director(*fh),
            offset: *offset,
            data,
        },
        OwnedRequest::SetAttr { fh, size } => Request::SetAttr {
            fh: director(*fh),
            size: *size,
        },
        OwnedRequest::Close { fh } => Request::Close { fh: director(*fh) },
        other => other.as_request(),
    }
}

/// Sends `req` and reads its reply: READ on a held slot, its bulk data
/// copied into `data` before the slot goes back; anything else through the
/// typed client. A status other than OK is an error.
fn send<N: aether_ipc::Notifier>(
    ring: &Attached,
    client: &aether_rpc::Client<'_, N>,
    req: &Request<'_>,
    scratch: &mut Vec<u8>,
    data: &mut [u8],
) -> Result<Answer, String> {
    if let Request::Read { slot_flags, .. } = req {
        let mut body = Vec::with_capacity(24);
        req.encode(&mut body);
        let flags = slot_flags.bits();
        let (mut replies, slots) = client
            .ring()
            .submit_many_held(&[(Opcode::Read as u32, flags, body)])
            .map_err(|e| format!("the ring: {e:?}"))?;
        let reply = replies.pop().expect("one reply per request");
        let answer = read_answer(ring, flags, reply.status, &reply.payload, data);
        client.ring().release_slots(&slots);
        return answer;
    }
    match client.call(req, scratch) {
        Ok(Reply::GetAttr(a)) => Ok(Answer::Attr { found: a.found }),
        Ok(Reply::Open(o)) => Ok(Answer::Open {
            fh: o.fh,
            size: o.size,
        }),
        Ok(Reply::Write(n)) => Ok(Answer::Written(n)),
        Ok(Reply::ReadDir(es)) => Ok(Answer::Listing(es.iter().count())),
        Ok(_) => Ok(Answer::Other),
        Err(e) => Err(e.to_string()),
    }
}

fn read_answer(
    ring: &Attached,
    flags: u32,
    status: i32,
    payload: &[u8],
    data: &mut [u8],
) -> Result<Answer, String> {
    if status != aether_protocol::Status::Ok.code() {
        return Err(match aether_protocol::Status::try_from(status) {
            Ok(s) => format!("director answered {}", s.name()),
            Err(_) => format!("director answered unknown status {status}"),
        });
    }
    match Reply::decode(Opcode::Read, flags, payload) {
        Ok(Reply::Read(ReadReply::Inline(b))) => Ok(Answer::Read {
            n: b.len() as u32,
            bulk: false,
        }),
        Ok(Reply::Read(ReadReply::Bulk { len, arena_offset })) => {
            let end = arena_offset.saturating_add(u64::from(len));
            let copied = (end <= ring.ring_bytes() as u64)
                .then(|| data.get_mut(..len as usize))
                .flatten()
                .and_then(|d| ring.seg().copy_to(arena_offset as usize, d));
            match copied {
                Some(()) => Ok(Answer::Read { n: len, bulk: true }),
                None => Err(format!(
                    "a bulk reply of {len} bytes at {arena_offset}, outside the mapping or longer \
                     than asked (wire.md 2.6, 3.4.4)"
                )),
            }
        }
        _ => Err("an OK reply that does not decode as a READ reply (wire.md 3.1)".into()),
    }
}

/// Microseconds, to the nanosecond.
fn us(ns: u64) -> f64 {
    ns as f64 / 1000.0
}

fn workload_json(w: &Workload, floor: &str, d: &Measured, f: &Measured) -> Value {
    let phases: Vec<Value> = w
        .phases
        .iter()
        .zip(d.phases.iter().zip(&f.phases))
        .map(|(p, (d, f))| phase_json(p, d, f))
        .collect();
    json!({
        "name": w.name,
        "geometry": d.geometry,
        "floor": floor,
        "floor_geometry": f.geometry,
        "phases": phases,
    })
}

fn phase_json(p: &Phase, d: &PhaseOut, f: &PhaseOut) -> Value {
    let per_s = |o: &PhaseOut| {
        if o.seconds > 0.0 {
            o.requests as f64 / o.seconds
        } else {
            0.0
        }
    };
    let mut per_op = serde_json::Map::new();
    for (op, h) in &d.ops {
        let q = |h: &Histogram<u64>, q: f64| h.value_at_quantile(q);
        let mut v = json!({
            "count": h.len(),
            "p50_us": us(q(h, 0.5)),
            "p99_us": us(q(h, 0.99)),
            "p999_us": us(q(h, 0.999)),
            "max_us": us(h.max()),
            "mean_us": h.mean() / 1000.0,
        });
        if let Some(fh) = f.ops.get(op) {
            for (k, quant) in [("p50", 0.5), ("p99", 0.99), ("p999", 0.999)] {
                let (a, b) = (q(h, quant), q(fh, quant));
                v[format!("floor_{k}_us")] = json!(us(b));
                v[format!("over_floor_{k}_us")] = json!((a as f64 - b as f64) / 1000.0);
            }
            v["floor_max_us"] = json!(us(fh.max()));
        }
        per_op.insert((*op).to_string(), v);
    }
    let read_bytes =
        |o: &PhaseOut| -> u64 { o.reads.iter().flatten().map(|&(n, _)| u64::from(n)).sum() };
    json!({
        "name": p.name,
        "threads": d.threads,
        "requests": d.requests,
        "seconds": d.seconds,
        "requests_per_s": per_s(d),
        "read_bytes": read_bytes(d),
        "bulk_bytes": d.bulk_bytes,
        "bulk_mib_s": (d.bulk_bytes > 0 && d.seconds > 0.0)
            .then(|| d.bulk_bytes as f64 / (1024.0 * 1024.0) / d.seconds),
        "floor": {
            "seconds": f.seconds,
            "requests_per_s": per_s(f),
            "read_bytes": read_bytes(f),
            "bulk_bytes": f.bulk_bytes,
        },
        "per_op": per_op,
    })
}

/// The report as text: per phase its rate, and per opcode the director's
/// p50 and p99, the floor's p50 and what the director adds over it.
pub fn summary(report: &Value) -> String {
    let mut out = format!(
        "director {}\nfloor {}\n{} client(s) per thread{}; microseconds per request\n",
        report["director"].as_str().unwrap_or("?"),
        report["floor"].as_str().unwrap_or("?"),
        report["clients"],
        if report["quick"] == true {
            ", --quick (one repeat, capped): counts, not timings"
        } else {
            ""
        },
    );
    for w in report["workloads"].as_array().into_iter().flatten() {
        let name = w["name"].as_str().unwrap_or("?");
        if let Some(e) = w.get("error").and_then(Value::as_str) {
            out.push_str(&format!("{name}: error: {e}\n"));
            continue;
        }
        out.push_str(&format!("{name}: ring {}\n", w["geometry"]));
        for p in w["phases"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "{name}/{}: {} threads, {} requests in {:.3} s, {:.0} req/s (floor {:.0} req/s){}\n",
                p["name"].as_str().unwrap_or("?"),
                p["threads"],
                p["requests"],
                p["seconds"].as_f64().unwrap_or(0.0),
                p["requests_per_s"].as_f64().unwrap_or(0.0),
                p["floor"]["requests_per_s"].as_f64().unwrap_or(0.0),
                match p["bulk_mib_s"].as_f64() {
                    Some(b) => format!(", bulk {b:.0} MiB/s"),
                    None => String::new(),
                }
            ));
            out.push_str(&format!(
                "  {:<14} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}\n",
                "op", "count", "p50", "p99", "p99.9", "floor p50", "+p50"
            ));
            for (op, v) in p["per_op"].as_object().into_iter().flatten() {
                let f = |k: &str| v[k].as_f64().unwrap_or(f64::NAN);
                out.push_str(&format!(
                    "  {op:<14} {:>8} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}\n",
                    v["count"].as_u64().unwrap_or(0),
                    f("p50_us"),
                    f("p99_us"),
                    f("p999_us"),
                    f("floor_p50_us"),
                    f("over_floor_p50_us"),
                ));
            }
        }
    }
    out
}

/// What a comparison with a baseline found.
#[derive(Debug)]
pub struct Comparison {
    /// One line per value compared.
    pub lines: Vec<String>,
    /// The largest rise of an opcode's p50, in percent.
    pub worst_rise_pct: f64,
}

/// `now` against `base`, which must come from the same machine
/// ([`machine::same_machine`]), the same director, client count and
/// `--quick`, and in every workload both have, the same ring geometry.
pub fn compare(now: &Value, base: &Value) -> Result<Comparison, String> {
    machine::same_machine(now, base)?;
    for k in ["director", "clients", "quick"] {
        if now[k] != base[k] {
            return Err(format!(
                "the baseline was recorded with another {k} ({} here, {} there)",
                now[k], base[k]
            ));
        }
    }
    for w in now["workloads"].as_array().into_iter().flatten() {
        let found = base["workloads"].as_array().into_iter().flatten();
        let Some(b) = found.into_iter().find(|b| b["name"] == w["name"]) else {
            continue;
        };
        if w.get("error").is_some() || b.get("error").is_some() {
            continue;
        }
        let (g, bg) = (&w["geometry"], &b["geometry"]);
        if bg.is_null() {
            return Err(format!(
                "the baseline's {} names no ring geometry (a report from before workloads had \
                 their own)",
                w["name"]
            ));
        }
        if g != bg {
            return Err(format!(
                "the baseline's {} was recorded on another ring geometry ({g} here, {bg} there)",
                w["name"]
            ));
        }
    }
    let mut lines = Vec::new();
    let mut worst = f64::NEG_INFINITY;
    let empty = Vec::new();
    let base_ws = base["workloads"].as_array().unwrap_or(&empty);
    for w in now["workloads"].as_array().into_iter().flatten() {
        let name = w["name"].as_str().unwrap_or("?");
        let Some(bw) = base_ws.iter().find(|b| b["name"] == w["name"]) else {
            lines.push(format!("{name}: not in the baseline"));
            continue;
        };
        if w.get("error").is_some() || bw.get("error").is_some() {
            lines.push(format!("{name}: an error on one side, not compared"));
            continue;
        }
        let base_ps = bw["phases"].as_array().unwrap_or(&empty);
        for p in w["phases"].as_array().into_iter().flatten() {
            let at = format!("{name}/{}", p["name"].as_str().unwrap_or("?"));
            let Some(bp) = base_ps.iter().find(|b| b["name"] == p["name"]) else {
                lines.push(format!("{at}: not in the baseline"));
                continue;
            };
            let mut value = |label: String, was: Option<f64>, is: Option<f64>| {
                if let (Some(was), Some(is)) = (was, is) {
                    let pct = machine::percent(was, is);
                    lines.push(format!("{label}: {was:.3} -> {is:.3} ({pct:+.1}%)"));
                    pct
                } else {
                    0.0
                }
            };
            value(
                format!("{at} req/s"),
                bp["requests_per_s"].as_f64(),
                p["requests_per_s"].as_f64(),
            );
            value(
                format!("{at} bulk MiB/s"),
                bp["bulk_mib_s"].as_f64(),
                p["bulk_mib_s"].as_f64(),
            );
            for (op, v) in p["per_op"].as_object().into_iter().flatten() {
                let b = &bp["per_op"][op];
                for k in ["p50_us", "p99_us", "over_floor_p50_us"] {
                    let pct = value(format!("{at} {op} {k}"), b[k].as_f64(), v[k].as_f64());
                    if k == "p50_us" {
                        worst = worst.max(pct);
                    }
                }
            }
        }
    }
    Ok(Comparison {
        lines,
        worst_rise_pct: if worst.is_finite() { worst } else { 0.0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::RequestFlags;

    #[test]
    fn a_capped_stream_closes_what_it_left_open() {
        let open = |p: &str| OwnedRequest::Open {
            root: aether_protocol::RootId(0),
            flags: aether_protocol::OpenFlags::READ,
            path: p.into(),
        };
        let read = |fh| OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh,
            offset: 0,
            len: 1,
        };
        let mut v = vec![
            open("a"),
            OwnedRequest::Close { fh: 1 },
            open("b"),
            read(2),
            read(2),
            OwnedRequest::Close { fh: 2 },
        ];
        cap(&mut v, 4);
        assert_eq!(v.len(), 5);
        assert_eq!(v[4], OwnedRequest::Close { fh: 2 });
    }
}
