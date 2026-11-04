//! The adapters built into the binary: `reference-adapter`, which serves a
//! scenario's tree from the reference director over a real ring, and the
//! hidden `test-adapter`, which misbehaves on purpose for the tests.
//!
//! The reference adapter is what a director's own adapter looks like: on
//! `mount` it builds [`Reference::from_tree`] from the scenario's tree,
//! creates a ring (a file by default, a named section with `--ring section`
//! on Windows), starts the same serve loop a Rust director runs
//! ([`aether_rpc::serve_with_arena`]) on a thread of its own, and answers the
//! handshake variables; on `unmount` it stops the loop, joins the thread and
//! removes the ring. A hidden, test-only `--break <rule>` makes it break one
//! rule on purpose ([`Break`]), for the tests that show a check finds it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use aether_ipc::{ring, DataArena, IpcError, Notifier, RingBacking, RingServer};
use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, LookupReply, Opcode, OpenFlags, OpenResp, ReadReply, Reply,
    Request, RequestFlags, RootId, Status,
};
use aether_refdirector::Reference;
use aether_rpc::{Director, Listing, ReadSlot};
use serde_json::{json, Value};

use crate::adapter::{answer_requests, AdapterRequest, Env};

/// The ring's shape: eight slots of the default payload cap, and a bank of
/// 1 MiB per slot (`docs/wire.md` 2.6 asks for at least 256 KiB).
const SLOTS: u32 = 8;
const PAYLOAD_CAP: u32 = aether_ipc::DEFAULT_PAYLOAD_CAP;
const BANK: usize = 1024 * 1024;
/// The threads serving a ring, each with its own handle on the reference
/// (`Reference::share`), and the `worker_hint` published (wire.md 2.8).
const WORKERS: u32 = 4;

/// Where the reference adapter's ring lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingKind {
    /// A file both sides map (`docs/wire.md` 2.10).
    File,
    /// A named section and two events (2.9). Windows only.
    Section,
}

/// One rule the reference adapter breaks on purpose, so that the tests can
/// show the checks find a director that gets it wrong. Test-only: reached
/// only through the hidden `reference-adapter --break <rule>` switch, which
/// the usage does not list (and, for the two rules only checks across steps
/// find, the hidden `test-adapter --misbehave stale-immutable` and
/// `frozen-reg-gen`); without it the adapter serves the reference as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Break {
    /// GETATTR of a missing path answers `found = 1` (wire.md 3.4.1 says
    /// `found = 0`).
    FoundForMissing,
    /// An unknown opcode or a payload that does not decode is answered
    /// `ST_IO_ERROR` (wire.md 3.3 and 3.5.1 say `ST_BAD_REQUEST`).
    IoErrorForBadRequest,
    /// Every read open of a file is answered `immutable`, writable ones
    /// included, with the `mount_gen` it always has: an overwrite of the file
    /// at the same size leaves the shim's read cache serving the old bytes
    /// (wire.md 3.4.3).
    StaleImmutable,
    /// The registry generation is published once, at attach, and never moved
    /// by a write (wire.md 4.5 says every successful write moves it).
    FrozenRegGen,
    /// When two requests are in flight, each one's reply goes into the
    /// other's slot (under the right `ack`): a director that mixes up its
    /// threads' answers (wire.md 2.5: a reply is written into the slot of
    /// the request it answers).
    SwapReplies,
    /// Handles are numbered per slot, as a director that keeps a handle table
    /// per connection would, so two clients on two slots get the same `fh`
    /// and a handle presented on another slot is `ST_BAD_FH` (wire.md 2.8.1:
    /// handles are ring-global, unique across every process, and any process
    /// may present any handle).
    PerSlotFh,
    /// Only slot 0 is ever taken; a request in any other slot waits for ever
    /// (wire.md 2.5: "Every taken slot MUST be completed", and slots are
    /// taken "in any order and from any number of threads"). A client that
    /// sends one request at a time from one thread only ever uses slot 0.
    SlotZeroOnly,
    /// Every bulk READ reply is written into bank 0, whatever slot it answers
    /// (wire.md 2.6: "Slot `s` owns bank `s % banks`").
    BankZero,
    /// Not a broken rule: handles are numbered down from `u64::MAX` (the
    /// reference's 1 is `u64::MAX`, its 2 is `u64::MAX - 1`), which wire.md
    /// 3.4.3 allows ("any u64 the director chooses"). A run must pass it,
    /// whatever numbers it sends for handles it has no number of its own for.
    DownwardFh,
    /// A bulk READ answers at most 256 KiB, as a director whose banks are
    /// smaller than `arena_len / slot_count` would (wire.md 2.6: each bank
    /// holds the shim's chunk; 3.4.4: `n < len` means end of file).
    ShortBulk,
    /// A READDIR listing larger than `payload_cap` is cut to the entries that
    /// fit and answered OK (wire.md 3.4.2: it must fit, and is answered with
    /// the overflow status otherwise; 2.5 step 3).
    TruncateReaddir,
    /// Each READ first writes over the banks of earlier bulk replies in other
    /// slots, as a director staging read-ahead in a bank it already answered
    /// from would (wire.md 2.6: the bank is the client's while its slot is
    /// `COMPLETED`).
    LateBank,
    /// READs of 256 KiB or more take 300 ms, and a request given up while
    /// being processed is completed anyway, `COMPLETED` stored over
    /// `ABANDONED` (wire.md 2.5: "a director must never store COMPLETED over
    /// ABANDONED").
    CompleteAbandoned,
    /// A registry write moves `reg_gen` first and makes the write visible
    /// 2 ms later (wire.md 4.5: it moves "before the write's reply is
    /// published", and the shim caches an answer under the generation it
    /// read before asking, so the new generation must not come before the
    /// write).
    BumpBeforeWrite,
}

impl Break {
    /// The rule a `--break` value names.
    pub fn parse(name: &str) -> Option<Break> {
        match name {
            "found-for-missing" => Some(Break::FoundForMissing),
            "io-error-for-bad-request" => Some(Break::IoErrorForBadRequest),
            "stale-immutable" => Some(Break::StaleImmutable),
            "frozen-reg-gen" => Some(Break::FrozenRegGen),
            "swap-replies" => Some(Break::SwapReplies),
            "per-slot-fh" => Some(Break::PerSlotFh),
            "slot-zero-only" => Some(Break::SlotZeroOnly),
            "bank-zero" => Some(Break::BankZero),
            "downward-fh" => Some(Break::DownwardFh),
            "short-bulk" => Some(Break::ShortBulk),
            "truncate-readdir" => Some(Break::TruncateReaddir),
            "late-bank" => Some(Break::LateBank),
            "complete-abandoned" => Some(Break::CompleteAbandoned),
            "bump-before-write" => Some(Break::BumpBeforeWrite),
            _ => None,
        }
    }
}

/// Runs the reference adapter until EOF on stdin. The exit code. `broken`
/// is the test-only rule to break ([`Break`]); `None` serves the reference
/// as it is.
pub fn reference_adapter(kind: RingKind, broken: Option<Break>) -> i32 {
    if kind == RingKind::Section && !cfg!(windows) {
        eprintln!("reference-adapter: --ring section needs Windows");
        return 2;
    }
    let mut mounted: Option<Mounted> = None;
    answer_requests(|req| match req {
        AdapterRequest::Mount { scenario } => {
            if mounted.is_some() {
                return json!({"ok": false, "error": "a tree is already mounted"});
            }
            match mount(Path::new(&scenario), kind, broken) {
                Ok(m) => {
                    let env = m.env.clone();
                    mounted = Some(m);
                    json!({"ok": true, "env": env})
                }
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        AdapterRequest::Unmount => match mounted.take().map(Mounted::unmount) {
            Some(Err(e)) => json!({"ok": false, "error": e}),
            _ => json!({"ok": true}),
        },
    });
    if let Some(m) = mounted.take() {
        let _ = m.unmount();
    }
    0
}

/// A mounted tree: the serve threads, how to stop them, and the ring.
struct Mounted {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Result<(), IpcError>>>,
    backing: Arc<dyn RingBacking>,
    file: Option<PathBuf>,
    env: Env,
}

impl Mounted {
    fn unmount(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        let served: Vec<_> = self.threads.into_iter().map(|t| t.join()).collect();
        drop(self.backing);
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
        for s in served {
            match s {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(format!("a serve loop ended with {e:?}")),
                Err(_) => return Err("a serve loop panicked".into()),
            }
        }
        Ok(())
    }
}

/// A name no other mount of this or another process has.
fn unique(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("aether-conform-{tag}-{}-{n}-{t}", std::process::id())
}

fn mount(scenario: &Path, kind: RingKind, broken: Option<Break>) -> Result<Mounted, String> {
    let s = crate::scenario::load(scenario).map_err(|e| e.to_string())?;
    let d = Served {
        reference: Reference::from_tree(&s.tree)?,
        broken,
        ring: None,
        banks: Arc::default(),
    };

    let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
    let arena_offset = aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride;
    let arena_len = SLOTS as usize * BANK;
    let ring_bytes = (arena_offset + arena_len)
        .next_multiple_of(65536)
        .max(2 * 1024 * 1024);

    let mut env = Env::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (backing, file, threads): (Arc<dyn RingBacking>, _, _) = match kind {
        RingKind::File => {
            let path = std::env::temp_dir().join(format!("{}.ring", unique("ring")));
            let f = aether_filering::FileRing::create(&path, ring_bytes)
                .map_err(|e| format!("create {}: {e}", path.display()))?;
            let b: Arc<dyn RingBacking> = Arc::new(f);
            init(&*b)?;
            env.insert(
                aether_env::RING_PATH.into(),
                path.to_string_lossy().into_owned(),
            );
            let t = serve(
                b.clone(),
                Arc::new(aether_ipc::AdaptiveNotifier),
                d,
                arena_offset,
                arena_len,
                stop.clone(),
            );
            (b, Some(path), t)
        }
        RingKind::Section => section(&mut env, d, ring_bytes, arena_offset, arena_len, &stop)?,
    };
    env.insert(aether_env::RING_BYTES.into(), ring_bytes.to_string());
    env.insert(aether_env::RING_PAYLOAD_CAP.into(), PAYLOAD_CAP.to_string());
    env.insert(aether_env::ARENA_OFFSET.into(), arena_offset.to_string());
    env.insert(aether_env::ARENA_LEN.into(), arena_len.to_string());
    // Where the program would see each root. Nothing on the ring names
    // them; they are here because a shim needs them (wire.md 5.3).
    let root = |id: u32| format!(r"C:\aether-conform\root{id}");
    env.insert(aether_env::VIRTUAL_DIR.into(), root(0));
    let extra: Vec<String> = s
        .tree
        .roots
        .iter()
        .filter(|r| r.id != 0)
        .map(|r| format!("{}={}", r.id, root(r.id)))
        .collect();
    if !extra.is_empty() {
        env.insert(aether_env::VIRTUAL_ROOTS.into(), extra.join(";"));
    }
    if s.tree.registry.is_some() {
        env.insert(aether_env::REGISTRY.into(), "1".into());
    }
    Ok(Mounted {
        stop,
        threads,
        backing,
        file,
        env,
    })
}

/// A started ring: its backing, its file (if it is one) and the serve
/// threads.
type Started = (
    Arc<dyn RingBacking>,
    Option<PathBuf>,
    Vec<JoinHandle<Result<(), IpcError>>>,
);

/// Initialises the ring and states the threads serving it (wire.md 2.8).
fn init(b: &dyn RingBacking) -> Result<(), String> {
    ring::init(b.seg(), SLOTS, PAYLOAD_CAP).map_err(|e| format!("ring init: {e:?}"))?;
    ring::set_worker_hint(b.seg(), WORKERS);
    Ok(())
}

#[cfg(windows)]
fn section(
    env: &mut Env,
    d: Served,
    ring_bytes: usize,
    arena_offset: usize,
    arena_len: usize,
    stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    let name = format!(r"Local\{}", unique("section"));
    let (srv, cli) = (format!("{name}_srv"), format!("{name}_cli"));
    let m = aether_win::SharedMapping::create(&name, ring_bytes)
        .map_err(|e| format!("create section {name}: {e}"))?;
    let events = aether_win::EventNotifier::create(&srv, &cli)
        .map_err(|e| format!("create events {srv}, {cli}: {e}"))?;
    let b: Arc<dyn RingBacking> = Arc::new(m);
    init(&*b)?;
    env.insert(aether_env::RING_SECTION.into(), name);
    env.insert(aether_env::SERVER_EV.into(), srv);
    env.insert(aether_env::CLIENT_EV.into(), cli);
    let t = serve(
        b.clone(),
        Arc::new(events),
        d,
        arena_offset,
        arena_len,
        stop.clone(),
    );
    Ok((b, None, t))
}

#[cfg(not(windows))]
fn section(
    _env: &mut Env,
    _d: Served,
    _ring_bytes: usize,
    _arena_offset: usize,
    _arena_len: usize,
    _stop: &Arc<AtomicBool>,
) -> Result<Started, String> {
    Err("a named section needs Windows".into())
}

/// Serves `d` on `backing` until `stop`, on [`WORKERS`] threads, each with
/// its own handle on the reference and its own `RingServer`, sharing one
/// notifier: as a Rust director serves (`aether_rpc::serve_with_arena`), or,
/// for the breaks that need to see slots ([`Break::SwapReplies`],
/// [`Break::PerSlotFh`], [`Break::SlotZeroOnly`]), with [`interposed`].
fn serve<N: Notifier + Send + Sync + 'static>(
    backing: Arc<dyn RingBacking>,
    notifier: Arc<N>,
    d: Served,
    arena_offset: usize,
    arena_len: usize,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<Result<(), IpcError>>> {
    let published = backing.clone();
    let frozen = d.broken == Some(Break::FrozenRegGen);
    let once = AtomicBool::new(false);
    d.reference.publish_generation(move |g| {
        if frozen && once.swap(true, Ordering::AcqRel) {
            return;
        }
        ring::publish_reg_generation(published.seg(), g)
    });
    let handles = Arc::new(Mutex::new(SlotHandles::default()));
    let banks = Arc::new(Mutex::new(Vec::new()));
    (0..WORKERS)
        .map(|_| {
            let backing = backing.clone();
            let notifier = SharedNotifier(notifier.clone());
            let stop = stop.clone();
            let handles = handles.clone();
            let mut d = Served {
                reference: d.reference.share(),
                broken: d.broken,
                ring: Some(backing.clone()),
                banks: banks.clone(),
            };
            std::thread::spawn(move || {
                let seg = backing.seg();
                let served = match d.broken {
                    Some(
                        b @ (Break::SwapReplies
                        | Break::PerSlotFh
                        | Break::SlotZeroOnly
                        | Break::CompleteAbandoned),
                    ) => interposed(seg, &notifier, &mut d, b, &handles, &stop),
                    _ => RingServer::new(seg, notifier).and_then(|server| {
                        let arena = DataArena::new(seg, arena_offset, arena_len, SLOTS as usize);
                        aether_rpc::serve_with_arena(&server, Some(&arena), &mut d, &stop)
                    }),
                };
                // The reference holds the generation callback, and with it
                // the ring.
                drop(d);
                served
            })
        })
        .collect()
}

/// One notifier shared by every serve thread of a ring.
struct SharedNotifier<N>(Arc<N>);

impl<N: Notifier> Notifier for SharedNotifier<N> {
    fn notify_server(&self) {
        self.0.notify_server()
    }
    fn wait_server(&self) {
        self.0.wait_server()
    }
    fn notify_client(&self, slot: u32) {
        self.0.notify_client(slot)
    }
    fn wait_client(&self, slot: u32) {
        self.0.wait_client(slot)
    }
    fn idle_client(&self, slot: u32, waited: std::time::Duration) {
        self.0.idle_client(slot, waited)
    }
    fn notify_slot_free(&self) {
        self.0.notify_slot_free()
    }
    fn now(&self) -> std::time::Instant {
        self.0.now()
    }
}

/// [`Break::PerSlotFh`]'s handle tables: the reference's handle for each
/// (slot, number that slot's client was given), and each slot's last number.
#[derive(Default)]
struct SlotHandles {
    open: BTreeMap<(u32, u64), u64>,
    next: BTreeMap<u32, u64>,
}

/// A serve loop for the breaks that need the slot a request came in on.
/// Each request is taken off the ring by hand, answered by the reference
/// through a ring of one slot held in memory ([`Inner`], so the answers are
/// the serve loop's, and every READ is answered inline, which wire.md 3.4.4
/// allows), and completed with `ring::server_complete` as a director
/// completes it (2.5), except for what `how` breaks.
fn interposed(
    seg: &aether_ipc::SharedSeg,
    notifier: &impl Notifier,
    d: &mut Served,
    how: Break,
    handles: &Mutex<SlotHandles>,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    let geom = ring::open(seg)?;
    // A slot count of one: only slot 0 is looked at.
    let take_geom = match how {
        Break::SlotZeroOnly => ring::Geom {
            slot_count: 1,
            ..geom
        },
        _ => geom,
    };
    let mut inner = Inner::new()?;
    let mut next = 0;
    let mut payload = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let Some(a) = ring::server_take_from(seg, &take_geom, next) else {
            notifier.wait_server();
            continue;
        };
        next = a.wrapping_add(1);
        let mut taken = vec![a];
        if how == Break::SwapReplies {
            // Another request in flight with this one, if one comes soon. A
            // fixed busy-wait, not an event: nothing can signal "another
            // request is about to be published", and 300 µs per request is
            // nothing to a test break.
            let since = std::time::Instant::now();
            while since.elapsed() < std::time::Duration::from_micros(300) {
                if let Some(b) = ring::server_take_from(seg, &geom, next) {
                    next = b.wrapping_add(1);
                    taken.push(b);
                    break;
                }
                core::hint::spin_loop();
            }
        }
        let mut answers = Vec::new();
        for &slot in &taken {
            let head =
                ring::read_request_into(seg, &geom, slot, &mut payload).ok_or(IpcError::Layout)?;
            if how == Break::CompleteAbandoned
                && head.opcode == Opcode::Read as u32
                && payload
                    .get(16..20)
                    .is_some_and(|l| u32::from_le_bytes(l.try_into().unwrap()) >= 256 * 1024)
            {
                // Slow, so a client gives it up while it is processed.
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            let (status, reply) = if head.fits(&geom) {
                let mut table = handles.lock().unwrap_or_else(|e| e.into_inner());
                let req = Raw {
                    slot,
                    opcode: head.opcode,
                    flags: head.flags,
                    payload: &payload,
                };
                answer_on(&mut inner, d, how, &mut table, &req)
            } else {
                (Status::BadRequest.code(), Vec::new())
            };
            answers.push((slot, head.req_id, status, reply));
        }
        if let [a, b] = answers.as_mut_slice() {
            // Each reply into the other's slot, under its own ack.
            std::mem::swap(&mut a.2, &mut b.2);
            std::mem::swap(&mut a.3, &mut b.3);
        }
        for (slot, req_id, status, reply) in answers {
            let given_up =
                ring::slot_state(seg, &geom, slot) == Some(aether_ipc::layout::ST_ABANDONED);
            ring::server_complete(seg, &geom, slot, req_id, status, &reply)?;
            if how == Break::CompleteAbandoned && given_up {
                // COMPLETED stored over the slot the client gave up.
                seg.write_bytes(
                    geom.slot_off(slot) + aether_ipc::layout::SH_STATE,
                    &aether_ipc::layout::ST_COMPLETED.to_le_bytes(),
                );
            }
            notifier.notify_client(slot);
        }
    }
    Ok(())
}

/// A request as [`interposed`] took it off the ring.
struct Raw<'p> {
    slot: u32,
    opcode: u32,
    flags: u32,
    payload: &'p [u8],
}

/// The status and reply payload for `req`, from the reference, with
/// [`Break::PerSlotFh`]'s numbering applied.
fn answer_on(
    inner: &mut Inner,
    d: &mut Served,
    how: Break,
    table: &mut SlotHandles,
    req: &Raw<'_>,
) -> (i32, Vec<u8>) {
    let (slot, opcode, flags) = (req.slot, req.opcode, req.flags);
    let passed = |inner: &mut Inner, d: &mut Served| inner.ask(d, opcode, flags, req.payload);
    if how != Break::PerSlotFh {
        return passed(inner, d);
    }
    let Ok(op) = Opcode::try_from(opcode) else {
        return passed(inner, d);
    };
    let Ok(decoded) = Request::decode(op, flags, req.payload) else {
        return passed(inner, d);
    };
    let bad_fh = (Status::BadFh.code(), Vec::new());
    let theirs = |fh: u64| table.open.get(&(slot, fh)).copied();
    let mut body = Vec::new();
    let mut closing = None;
    match decoded {
        Request::Read {
            slot_flags,
            fh,
            offset,
            len,
        } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::Read {
                slot_flags,
                fh,
                offset,
                len,
            }
            .encode(&mut body);
        }
        Request::Write { fh, offset, data } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::Write { fh, offset, data }.encode(&mut body);
        }
        Request::SetAttr { fh, size } => {
            let Some(fh) = theirs(fh) else { return bad_fh };
            Request::SetAttr { fh, size }.encode(&mut body);
        }
        Request::Close { fh: ext } => {
            let Some(fh) = theirs(ext) else { return bad_fh };
            Request::Close { fh }.encode(&mut body);
            closing = Some(ext);
        }
        _ => body.extend_from_slice(req.payload),
    }
    let (status, reply) = inner.ask(d, opcode, flags, &body);
    if status != Status::Ok.code() {
        return (status, reply);
    }
    if let Some(ext) = closing {
        table.open.remove(&(slot, ext));
        return (status, reply);
    }
    match Reply::decode(op, flags, &reply) {
        Ok(Reply::Open(o)) => {
            let n = table.next.entry(slot).or_insert(0);
            *n += 1;
            let ext = *n;
            table.open.insert((slot, ext), o.fh);
            let mut out = Vec::new();
            Reply::Open(OpenResp { fh: ext, ..o }).encode(&mut out);
            (status, out)
        }
        _ => (status, reply),
    }
}

/// The reference behind a ring of one slot held in memory, with no arena:
/// what [`interposed`] asks, so the answers are the serve loop's.
struct Inner {
    seg: aether_ipc::OwnedSeg,
    geom: ring::Geom,
    scratch: aether_rpc::Scratch,
}

impl Inner {
    fn new() -> Result<Inner, IpcError> {
        let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
        let seg = aether_ipc::OwnedSeg::new(aether_ipc::layout::RING_HEADER_SIZE + stride);
        let geom = ring::init(seg.seg(), 1, PAYLOAD_CAP)?;
        Ok(Inner {
            seg,
            geom,
            scratch: aether_rpc::Scratch::default(),
        })
    }

    fn ask(&mut self, d: &mut Served, opcode: u32, flags: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        let seg = self.seg.seg();
        let io = (Status::IoError.code(), Vec::new());
        let Some(slot) = ring::claim_free(seg, &self.geom) else {
            return io;
        };
        let Ok(req_id) = ring::publish_request(seg, &self.geom, slot, opcode, flags, payload)
        else {
            let _ = ring::free_slot(seg, &self.geom, slot);
            return io;
        };
        let Ok(server) = RingServer::new(seg, aether_ipc::SpinNotifier) else {
            return io;
        };
        let _ = aether_rpc::serve_one_with(&server, d, &mut self.scratch);
        let reply = ring::take_response(seg, &self.geom, slot, req_id);
        let _ = ring::free_slot(seg, &self.geom, slot);
        match reply {
            Ok(Some(r)) => r,
            _ => io,
        }
    }
}

/// What the adapter serves: the reference, and the rule it breaks if the
/// test-only `--break` switch named one.
struct Served {
    reference: Reference,
    broken: Option<Break>,
    /// The ring served, for the breaks that write its header themselves.
    ring: Option<Arc<dyn RingBacking>>,
    /// [`Break::LateBank`]'s (slot, length) of bulk replies already given.
    banks: Arc<Mutex<Vec<(u32, usize)>>>,
}

impl Served {
    /// The number the client is given for the reference's handle `fh`.
    fn fh_out(&self, fh: u64) -> u64 {
        match self.broken {
            Some(Break::DownwardFh) => u64::MAX - (fh - 1),
            _ => fh,
        }
    }

    /// The reference's handle for the number `fh` a client sent.
    fn fh_in(&self, fh: u64) -> u64 {
        match self.broken {
            // 0 becomes the reference's 2^64, which wraps to 0: never issued.
            Some(Break::DownwardFh) => (u64::MAX - fh).wrapping_add(1),
            _ => fh,
        }
    }
}

impl Director for Served {
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        let mut a = self.reference.get_attr(root, path)?;
        if self.broken == Some(Break::FoundForMissing) && !a.found {
            a.found = true;
        }
        Ok(a)
    }
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        self.reference.read_dir(root, path, out)?;
        if self.broken == Some(Break::TruncateReaddir) {
            // `count`, then 21 bytes and the name per entry (wire.md 3.4.2).
            let mut size = 4usize;
            let kept: Vec<(String, bool, u64, i64)> = out
                .iter()
                .take_while(|e| {
                    size += 21 + e.name.len();
                    size <= PAYLOAD_CAP as usize
                })
                .map(|e| (e.name.to_string(), e.is_dir, e.size, e.mtime))
                .collect();
            *out = Listing::default();
            for (name, is_dir, size, mtime) in &kept {
                out.push(aether_protocol::DirEntry {
                    name,
                    is_dir: *is_dir,
                    size: *size,
                    mtime: *mtime,
                });
            }
        }
        Ok(())
    }
    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        let mut o = self.reference.open(root, flags, path)?;
        if self.broken == Some(Break::StaleImmutable)
            && !o.is_dir
            && !flags.contains(OpenFlags::WRITE)
        {
            o.immutable = true;
        }
        o.fh = self.fh_out(o.fh);
        Ok(o)
    }
    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        let at = match self.broken {
            Some(Break::BankZero) => ReadSlot { slot: 0, ..at },
            _ => at,
        };
        let fh = self.fh_in(fh);
        if let (Some(Break::LateBank), Some(arena)) = (self.broken, at.arena) {
            let mut banks = self.banks.lock().unwrap_or_else(|e| e.into_inner());
            for (slot, n) in banks.drain(..).filter(|&(s, _)| s != at.slot) {
                let _ = arena.fill_bank(slot, n, |b| {
                    b.fill(0xEE);
                    Ok(b.len())
                });
            }
        }
        let broken = self.broken;
        let banks = self.banks.clone();
        match self.reference.read(at, slot_flags, fh, offset, len) {
            Ok(ReadReply::Bulk { len, arena_offset }) => {
                let len = match broken {
                    Some(Break::ShortBulk) => len.min(256 * 1024),
                    Some(Break::LateBank) => {
                        banks
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((at.slot, len as usize));
                        len
                    }
                    _ => len,
                };
                Ok(ReadReply::Bulk { len, arena_offset })
            }
            other => other,
        }
    }
    fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status> {
        self.reference.write(self.fh_in(fh), offset, data)
    }
    fn set_attr(&mut self, fh: u64, size: u64) -> Result<(), Status> {
        self.reference.set_attr(self.fh_in(fh), size)
    }
    fn rename(&mut self, root: RootId, from: &str, to: &str) -> Result<(), Status> {
        self.reference.rename(root, from, to)
    }
    fn delete(&mut self, root: RootId, path: &str) -> Result<(), Status> {
        self.reference.delete(root, path)
    }
    fn mkdir(&mut self, root: RootId, mode: u32, path: &str) -> Result<(), Status> {
        self.reference.mkdir(root, mode, path)
    }
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        self.reference.close(self.fh_in(fh))
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        self.reference.heartbeat()
    }
    fn stored_names(&mut self, root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        self.reference.stored_names(root, skip, path)
    }
    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status> {
        self.reference.reg_lookup(path)
    }
    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status> {
        self.reference.reg_key(path)
    }
    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status> {
        if let (Some(Break::BumpBeforeWrite), Some(ring)) = (self.broken, &self.ring) {
            let now = ring::reg_generation(ring.seg());
            ring::publish_reg_generation(ring.seg(), now + 1);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.reference.reg_set_value(path, name, ty, data)
    }
    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status> {
        self.reference.reg_delete_value(path, name)
    }
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status> {
        self.reference.reg_create_key(path, volatile)
    }
    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status> {
        self.reference.reg_delete_key(path)
    }
    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status> {
        self.reference.reg_rename_key(path, new_leaf)
    }
    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status> {
        self.reference.reg_changed(path, subtree, since)
    }
    fn rename_x(
        &mut self,
        from_root: RootId,
        to_root: RootId,
        from: &str,
        to: &str,
    ) -> Result<(), Status> {
        self.reference.rename_x(from_root, to_root, from, to)
    }
    fn stage_exec(&mut self, root: RootId, path: &str) -> Result<&str, Status> {
        self.reference.stage_exec(root, path)
    }
    fn unknown(&mut self, opcode: u32) -> Status {
        match self.broken {
            Some(Break::IoErrorForBadRequest) => Status::IoError,
            _ => self.reference.unknown(opcode),
        }
    }
    fn undecodable(&mut self, op: Opcode, flags: u32, payload: &[u8]) -> Status {
        match self.broken {
            Some(Break::IoErrorForBadRequest) => Status::IoError,
            _ => self.reference.undecodable(op, flags, payload),
        }
    }
}

/// How the test adapter misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misbehave {
    /// Exits at once, before reading anything.
    Exit,
    /// Answers every request with a line that is not JSON.
    Garbage,
    /// Reads a request and never answers.
    Hang,
    /// Starts a second test adapter that hangs (the same binary, holding
    /// this one's stdout and stderr), reports its pid on stderr, then hangs
    /// itself.
    HangGrandchild,
    /// Refuses every mount; answers unmount.
    Refuse,
    /// The reference adapter breaking a rule no single reply shows, which
    /// the checks across steps must find: [`Break::StaleImmutable`] or
    /// [`Break::FrozenRegGen`].
    Director(Break),
}

/// Runs the test adapter. The exit code.
pub fn test_adapter(how: Misbehave) -> i32 {
    use std::io::BufRead;
    match how {
        Misbehave::Exit => {
            eprintln!("test adapter: exiting");
            3
        }
        Misbehave::Garbage => {
            eprintln!("test adapter: answering garbage");
            for line in std::io::stdin().lock().lines() {
                if line.is_err() {
                    break;
                }
                println!("this is not json");
            }
            0
        }
        Misbehave::Hang => {
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            eprintln!("test adapter: not answering");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        Misbehave::HangGrandchild => {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("test adapter: no current exe: {e}");
                    return 2;
                }
            };
            match std::process::Command::new(exe)
                .args(["test-adapter", "--misbehave", "hang"])
                .stdin(std::process::Stdio::null())
                .spawn()
            {
                Ok(g) => eprintln!("test adapter: grandchild pid {}", g.id()),
                Err(e) => {
                    eprintln!("test adapter: cannot start the grandchild: {e}");
                    return 2;
                }
            }
            test_adapter(Misbehave::Hang)
        }
        Misbehave::Director(b) => reference_adapter(RingKind::File, Some(b)),
        Misbehave::Refuse => {
            answer_requests(|req| -> Value {
                match req {
                    AdapterRequest::Mount { .. } => {
                        json!({"ok": false, "error": "no trees today"})
                    }
                    AdapterRequest::Unmount => json!({"ok": true}),
                }
            });
            0
        }
    }
}
