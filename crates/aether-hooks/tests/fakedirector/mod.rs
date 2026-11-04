//! The shim tests' director: `aether_refdirector::Reference` served over a
//! real ring, the one the shim's own `FuseClient` attaches to through
//! `try_init_from_env` exactly as in production. Only the far side of the
//! ring is in memory.
//!
//! What the director answers lives in `crates/aether-refdirector`. This
//! module creates the ring (a named section, or a file for the Wine
//! transport), installs the handshake environment, serves it on several
//! threads with the arena laid out as a director lays it out, and makes the
//! extra clients a few tests need.
//!
//! Every request crosses the harness's taps, chained as
//! `((recorder, (overrides, faults)), extra)`: the [`Recorder`] keeps what
//! the ring sent, the [`Override`] changes the fields of OPEN replies a
//! director may give, and the [`Faults`] answer what one should not. A test
//! reads and arms them through the [`Served`] that [`install_with`] returns:
//!
//! ```text
//! let served = fakedirector::install_with(&root, fake, Options { deadline: Some(ms(300)), ..Default::default() });
//! let mark = served.recorder.mark();
//! // ... a hooked call ...
//! assert_eq!(served.recorder.ops_since(mark), [Some(Opcode::Open), Some(Opcode::Read), Some(Opcode::Close)]);
//! ```

// Compiled into several test binaries, each using a different subset.
#![allow(dead_code, unused_imports)]
// The wake event is a Win32 handle.
#![allow(unsafe_code)]

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use aether_hooks::director::FuseInitError;
use aether_ipc::{Notifier, RequestView, SharedSeg};
use aether_refdirector::harness::{serve_threads, ArenaShape, FaultId};
use aether_rpc::{Tap, TapAction};
use aether_win::SharedMapping;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

// The tests know the reference director by its old name here.
pub(crate) use aether_refdirector::harness::{Fault, Faults, Match, Override, Recorder};
pub(crate) use aether_refdirector::{key, pattern, ReadStyle, Reference as Fake, BULK_THRESHOLD};

/// Deliberately small: 4 KiB caps an inline READ response at 4088 bytes, so
/// any fixture bigger than that is guaranteed to span several ring round
/// trips. A test that only ever moved one payload's worth of data would pass
/// against an implementation that reads once and calls it done.
pub(crate) const PAYLOAD_CAP: u32 = 4096;
pub(crate) const SLOTS: u32 = 8;

/// An arena of `SLOTS` x 256 KiB. The bank size the client computes
/// (`arena_len / slot_count`, clamped to at least 256 KiB) then agrees exactly
/// with the one the server hands out, so a bulk read is not silently truncated
/// to a smaller bank and resumed, which would still pass a byte-exactness
/// test while hiding whether the bank sizing was right.
pub(crate) const ARENA_LEN: usize = SLOTS as usize * 256 * 1024;

/// How long a serve thread of a [`RingKind::SectionWithWake`] ring waits for
/// the event before it looks at the ring anyway. Only a bound: the client
/// signals the event for every request, and an auto-reset event keeps a
/// signal until a waiter takes it.
const WAKE_WAIT_MS: u32 = 1_000;

/// The ring's transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RingKind {
    /// A named section (`VFS_RING_SECTION`); the director's threads look at
    /// the ring without being woken (the shim's `VFS_SERVER_EV` unset).
    #[default]
    Section,
    /// A named section and the director's wake event (`VFS_SERVER_EV`): the
    /// serve threads wait on the event, which the shim signals per request.
    SectionWithWake,
    /// A ring in a real file (`VFS_RING_PATH`), served by `aether-filering`:
    /// the Wine and Proton transport, where no event reaches the director.
    File,
}

/// Who publishes the registry generation in the ring header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RegGen {
    /// The reference, before attach and before each write's reply
    /// (`docs/wire.md` 4.5).
    #[default]
    Reference,
    /// This value, once, before attach; nobody moves it. `Fixed(0)`: the
    /// header never holds a generation, so the shim caches nothing.
    Fixed(u64),
    /// Only the test, through [`Served::publish_reg_generation`]: a director
    /// that bumps when the test says (after its reply, say).
    Manual,
}

/// How [`install_with`] builds the ring and attaches the shim.
pub(crate) struct Options {
    /// The ring's slot count.
    pub slots: u32,
    /// The ring's `payload_cap`.
    pub payload_cap: u32,
    /// The arena's length, banked per slot; 0 forces every READ inline.
    pub arena_len: usize,
    /// The worker count the ring header states (`ring::set_worker_hint`); 0
    /// states none. The fake serves on that many threads, at most one per
    /// slot, and on `aether_ipc::DEFAULT_WORKER_COUNT` when it is 0.
    pub worker_hint: u32,
    /// The transport.
    pub ring: RingKind,
    /// `Some`: the shim's client gives up on a request after this
    /// ([`aether_hooks::director::FuseClient::with_deadline`]), heartbeat
    /// included, and is installed through `install_global`.
    pub deadline: Option<Duration>,
    /// Who publishes the registry generation.
    pub reg_gen: RegGen,
    /// Faults armed before the client's attach HEARTBEAT. With any here, a
    /// failed attach is the test's to look at ([`Served::attach`]) rather
    /// than a panic.
    pub attach_faults: Vec<(Match, Fault)>,
    /// One more tap, after the chain: it sees each request after the chain
    /// has and each reply before it does, as the director gave it.
    pub tap: Option<Box<dyn Tap + Send>>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            slots: SLOTS,
            payload_cap: PAYLOAD_CAP,
            arena_len: ARENA_LEN,
            worker_hint: 0,
            ring: RingKind::Section,
            deadline: None,
            reg_gen: RegGen::Reference,
            attach_faults: Vec::new(),
            tap: None,
        }
    }
}

/// What [`install_with`] serves, for the test to read and arm.
pub(crate) struct Served {
    /// The director.
    pub fake: &'static Fake,
    /// Every request that crossed the ring, as it was sent.
    pub recorder: Recorder,
    /// Faults to arm.
    pub faults: Faults,
    /// OPEN reply fields to change.
    pub overrides: Override,
    /// The ring's shared memory.
    pub seg: &'static SharedSeg,
    /// What attaching the shim's client gave: `try_init_from_env`'s result,
    /// or, with [`Options::deadline`], the same from the client connected
    /// with that deadline.
    pub attach: Result<(), FuseInitError>,
    /// The [`Options::attach_faults`], in order.
    pub attach_faults: Vec<FaultId>,
}

impl Served {
    /// Put `g` in the ring header as the registry generation, as a director
    /// does after a write (for [`RegGen::Manual`]).
    pub fn publish_reg_generation(&self, g: u64) {
        aether_ipc::ring::publish_reg_generation(self.seg, g);
    }
}

/// Control ring length for this geometry, and therefore the arena's offset
/// within the ring's memory: the layout a director uses
/// (`arena_offset = ring_bytes`).
fn ring_bytes(slots: u32, payload_cap: u32) -> usize {
    let stride = (32 + payload_cap as usize).next_multiple_of(8);
    aether_ipc::layout::RING_HEADER_SIZE + slots as usize * stride
}

/// Publish `fake` on a ring built as [`Options::default`] says, with an arena
/// of `arena_len`, and point the shim's `FuseClient` at it, once per test
/// process. Returns the fake so tests can read its counters.
///
/// `virtual_dir` becomes `VFS_VIRTUAL_DIR`: root 0 of the shim's client. A
/// test that declares more roots sets `VFS_VIRTUAL_ROOTS` before this.
///
/// `arena_len` of 0 forces every READ inline; [`ARENA_LEN`] gives the section
/// a real bulk arena laid out the way a director lays one out, so a
/// request at or above [`BULK_THRESHOLD`] takes the same transport it takes
/// live. Below the threshold reads stay inline either way, so one ring can
/// cover both.
pub(crate) fn install(virtual_dir: &Path, fake: Fake, arena_len: usize) -> &'static Fake {
    install_with(
        virtual_dir,
        fake,
        Options {
            arena_len,
            ..Options::default()
        },
    )
    .fake
}

/// Publish `fake` on a ring built as `opts` says and attach the shim's
/// client to it, once per test process (a second call returns the first's).
///
/// The ring, the serve threads and the fake are all leaked deliberately:
/// they must outlive every test in the binary, and the process is the only
/// thing that can decide when that is.
///
/// Panics if the attach fails, unless `opts.attach_faults` names faults for
/// it, when the result is [`Served::attach`].
pub(crate) fn install_with(virtual_dir: &Path, fake: Fake, opts: Options) -> &'static Served {
    static SERVED: OnceLock<Served> = OnceLock::new();
    SERVED.get_or_init(|| serve(virtual_dir, fake, opts))
}

fn serve(virtual_dir: &Path, fake: Fake, opts: Options) -> Served {
    let Options {
        slots,
        payload_cap,
        arena_len,
        worker_hint,
        ring,
        deadline,
        reg_gen,
        attach_faults,
        tap,
    } = opts;
    let fake: &'static Fake = Box::leak(Box::new(fake));
    let arena_offset = ring_bytes(slots, payload_cap);
    // Whole mapping, ring first then arena: `VFS_RING_BYTES` names the
    // whole thing because the shim maps all of it and a bulk response's
    // offset is mapping-absolute.
    let map_bytes = ((arena_offset + arena_len + 0xFFFF) & !0xFFFF).max(256 * 1024);
    let seg: &'static SharedSeg = match ring {
        RingKind::Section | RingKind::SectionWithWake => {
            let mapping: &'static SharedMapping = Box::leak(Box::new(
                SharedMapping::create(&section_name(), map_bytes).expect("section"),
            ));
            mapping.seg()
        }
        RingKind::File => {
            let path = ring_file();
            let file: &'static aether_filering::FileRing = Box::leak(Box::new(
                aether_filering::FileRing::create(&path, map_bytes).expect("ring file"),
            ));
            std::env::set_var(aether_env::RING_PATH, &path);
            aether_ipc::RingBacking::seg(file)
        }
    };
    aether_ipc::ring::init(seg, slots, payload_cap).expect("ring init");
    if worker_hint > 0 {
        aether_ipc::ring::set_worker_hint(seg, worker_hint);
    }
    // The registry generation is in the ring header before any client
    // attaches.
    match reg_gen {
        RegGen::Reference => {
            fake.publish_generation(move |g| aether_ipc::ring::publish_reg_generation(seg, g))
        }
        RegGen::Fixed(g) => aether_ipc::ring::publish_reg_generation(seg, g),
        RegGen::Manual => {}
    }

    let recorder = Recorder::new();
    let overrides = Override::new();
    // One bank per slot: a director worker's shape. `banks == slot_count` is
    // what makes the client's `arena_len / slot_count` bank size agree with
    // the server's.
    let arena = (arena_len > 0).then(|| ArenaShape {
        offset: arena_offset as u64,
        bank_size: (arena_len / slots as usize) as u64,
        banks: slots,
    });
    let faults = match arena {
        Some(a) => Faults::new(payload_cap).with_arena(a),
        None => Faults::new(payload_cap),
    };
    let attach_ids: Vec<FaultId> = attach_faults
        .into_iter()
        .map(|(when, fault)| faults.arm(when, fault))
        .collect();

    let wake = (ring == RingKind::SectionWithWake).then(|| {
        let name = format!("Local\\aether-shim-wake-{}", std::process::id());
        let w: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
        // SAFETY: the name is NUL-terminated; an auto-reset event,
        // unsignalled. Leaked with the ring.
        let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, w.as_ptr()) };
        assert!(!h.is_null(), "the director's wake event");
        std::env::set_var(aether_env::SERVER_EV, &name);
        h as usize
    });
    let threads = match worker_hint {
        0 => aether_ipc::DEFAULT_WORKER_COUNT as u32,
        n => n,
    }
    .clamp(1, slots.max(1)) as usize;
    let chain = (
        (recorder.clone(), (overrides.clone(), faults.clone())),
        Extra(tap.map(|t| Arc::new(Mutex::new(t)))),
    );
    std::thread::Builder::new()
        .name("fake-director".into())
        .spawn(move || {
            // Never set: the threads serve until the ring goes away with the
            // process.
            static STOP: AtomicBool = AtomicBool::new(false);
            let notifier = move || match wake {
                Some(h) => Wake::Event(h),
                None => Wake::Yield,
            };
            std::thread::scope(|s| {
                let workers = serve_threads(s, threads, seg, arena, &notifier, fake, &chain, &STOP);
                for w in workers {
                    let _ = w.join();
                }
            });
        })
        .expect("server thread");

    if ring != RingKind::File {
        std::env::set_var(aether_env::RING_SECTION, section_name());
    }
    std::env::set_var(aether_env::RING_BYTES, map_bytes.to_string());
    std::env::set_var(aether_env::RING_PAYLOAD_CAP, payload_cap.to_string());
    std::env::set_var(aether_env::ARENA_OFFSET, arena_offset.to_string());
    std::env::set_var(aether_env::ARENA_LEN, arena_len.to_string());
    std::env::set_var(aether_env::VIRTUAL_DIR, virtual_dir);
    let attach = match deadline {
        None => aether_hooks::director::try_init_from_env(),
        Some(d) => attach_with_deadline(d),
    };
    if attach_ids.is_empty() {
        if let Err(e) = &attach {
            panic!("fuse client: {e:?}");
        }
    }
    Served {
        fake,
        recorder,
        faults,
        overrides,
        seg,
        attach,
        attach_faults: attach_ids,
    }
}

/// What `try_init_from_env` does, with the client's deadline cut to `d`
/// before its heartbeat.
fn attach_with_deadline(d: Duration) -> Result<(), FuseInitError> {
    let client = aether_hooks::director::connect_from_env()?.with_deadline(d);
    client.heartbeat().map_err(FuseInitError::ConnectFailed)?;
    aether_hooks::director::install_global(client)
        .map_err(|_| FuseInitError::ConnectFailed("a client was already installed".into()))
}

/// The serve threads' notifier: look again at once (giving the processor to
/// whatever else is ready), or wait on the director's wake event.
enum Wake {
    Yield,
    /// The event's handle, as an address (a `HANDLE` is not `Send`).
    Event(usize),
}

impl Notifier for Wake {
    fn wait_server(&self) {
        match *self {
            Wake::Yield => std::thread::yield_now(),
            // SAFETY: the event is leaked with the ring, so the handle stays
            // valid for the process.
            Wake::Event(h) => unsafe {
                WaitForSingleObject(h as _, WAKE_WAIT_MS);
            },
        }
    }
}

/// [`Options::tap`], shared by every serve thread.
#[derive(Clone)]
struct Extra(Option<Arc<Mutex<Box<dyn Tap + Send>>>>);

impl Extra {
    fn with<R>(&self, f: impl FnOnce(&mut dyn Tap) -> R) -> Option<R> {
        self.0.as_ref().map(|t| {
            let mut t = t.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut **t)
        })
    }
}

impl Tap for Extra {
    fn on_request(&mut self, view: &RequestView<'_>) -> TapAction {
        self.with(|t| t.on_request(view)).unwrap_or(TapAction::Pass)
    }
    fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        self.with(|t| t.on_reply(view, status, payload));
    }
    fn on_sent(&mut self, view: &RequestView<'_>, status: i32, payload: &[u8]) {
        self.with(|t| t.on_sent(view, status, payload));
    }
}

/// The section [`install`] serves.
fn section_name() -> String {
    format!("Local\\aether-shim-cowseed-{}", std::process::id())
}

/// Where a [`RingKind::File`] ring lives: a folder of its own, leaked with
/// the ring.
fn ring_file() -> std::path::PathBuf {
    let dir = aether_testkit::tempdir().expect("ring folder");
    let path = dir.path().join("ring");
    std::mem::forget(dir);
    path
}

/// Another client on the ring [`install`] serves, as a second injected process of the session
/// would have: its own mapping of the section, its own `FuseClient`.
pub(crate) fn second_client(virtual_dir: &Path) -> aether_hooks::director::FuseClient {
    let env = |name: &str| -> String { std::env::var(name).unwrap() };
    aether_hooks::director::FuseClient::connect(
        &section_name(),
        &[(
            aether_redirect::RootId::DEFAULT,
            virtual_dir.to_string_lossy().into_owned(),
        )],
        env(aether_env::RING_PAYLOAD_CAP).parse().unwrap(),
        env(aether_env::RING_BYTES).parse().unwrap(),
        0,
    )
    .expect("second client")
}

/// A client on a ring nobody serves (a director that died), giving up on each request after
/// `deadline`.
pub(crate) fn unserved_client(
    virtual_dir: &Path,
    deadline: Duration,
) -> aether_hooks::director::FuseClient {
    let name = format!("Local\\aether-shim-dead-{}", std::process::id());
    let bytes = 256 * 1024;
    let mapping: &'static SharedMapping = Box::leak(Box::new(
        SharedMapping::create(&name, bytes).expect("section"),
    ));
    aether_ipc::ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP).expect("ring init");
    aether_hooks::director::FuseClient::connect(
        &name,
        &[(
            aether_redirect::RootId::DEFAULT,
            virtual_dir.to_string_lossy().into_owned(),
        )],
        PAYLOAD_CAP,
        bytes,
        0,
    )
    .expect("client on an unserved ring")
    .with_deadline(deadline)
}
