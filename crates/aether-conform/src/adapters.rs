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
//! removes the ring.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use aether_ipc::{ring, DataArena, IpcError, Notifier, RingBacking, RingServer};
use aether_refdirector::Reference;
use serde_json::{json, Value};

use crate::adapter::{answer_requests, AdapterRequest, Env};

/// The ring's shape: eight slots of the default payload cap, and a bank of
/// 1 MiB per slot (`docs/wire.md` 2.6 asks for at least 256 KiB).
const SLOTS: u32 = 8;
const PAYLOAD_CAP: u32 = aether_ipc::DEFAULT_PAYLOAD_CAP;
const BANK: usize = 1024 * 1024;

/// Where the reference adapter's ring lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingKind {
    /// A file both sides map (`docs/wire.md` 2.10).
    File,
    /// A named section and two events (2.9). Windows only.
    Section,
}

/// Runs the reference adapter until EOF on stdin. The exit code.
pub fn reference_adapter(kind: RingKind) -> i32 {
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
            match mount(Path::new(&scenario), kind) {
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

/// A mounted tree: the serve thread, how to stop it, and the ring.
struct Mounted {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), IpcError>>,
    backing: Arc<dyn RingBacking>,
    file: Option<PathBuf>,
    env: Env,
}

impl Mounted {
    fn unmount(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        let served = self.thread.join();
        drop(self.backing);
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
        match served {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("the serve loop ended with {e:?}")),
            Err(_) => Err("the serve loop panicked".into()),
        }
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

fn mount(scenario: &Path, kind: RingKind) -> Result<Mounted, String> {
    let s = crate::scenario::load(scenario).map_err(|e| e.to_string())?;
    let d = Reference::from_tree(&s.tree)?;

    let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
    let arena_offset = aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride;
    let arena_len = SLOTS as usize * BANK;
    let ring_bytes = (arena_offset + arena_len)
        .next_multiple_of(65536)
        .max(2 * 1024 * 1024);

    let mut env = Env::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (backing, file, thread): (Arc<dyn RingBacking>, _, _) = match kind {
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
                aether_ipc::AdaptiveNotifier,
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
        thread,
        backing,
        file,
        env,
    })
}

/// A started ring: its backing, its file (if it is one) and the serve thread.
type Served = (
    Arc<dyn RingBacking>,
    Option<PathBuf>,
    JoinHandle<Result<(), IpcError>>,
);

/// Initialises the ring and states the one thread serving it (wire.md 2.8).
fn init(b: &dyn RingBacking) -> Result<(), String> {
    ring::init(b.seg(), SLOTS, PAYLOAD_CAP).map_err(|e| format!("ring init: {e:?}"))?;
    ring::set_worker_hint(b.seg(), 1);
    Ok(())
}

#[cfg(windows)]
fn section(
    env: &mut Env,
    d: Reference,
    ring_bytes: usize,
    arena_offset: usize,
    arena_len: usize,
    stop: &Arc<AtomicBool>,
) -> Result<Served, String> {
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
    let t = serve(b.clone(), events, d, arena_offset, arena_len, stop.clone());
    Ok((b, None, t))
}

#[cfg(not(windows))]
fn section(
    _env: &mut Env,
    _d: Reference,
    _ring_bytes: usize,
    _arena_offset: usize,
    _arena_len: usize,
    _stop: &Arc<AtomicBool>,
) -> Result<Served, String> {
    Err("a named section needs Windows".into())
}

/// Serves `d` on `backing` until `stop`, on a thread of its own.
fn serve<N: Notifier + Send + 'static>(
    backing: Arc<dyn RingBacking>,
    notifier: N,
    d: Reference,
    arena_offset: usize,
    arena_len: usize,
    stop: Arc<AtomicBool>,
) -> JoinHandle<Result<(), IpcError>> {
    let published = backing.clone();
    d.publish_generation(move |g| ring::publish_reg_generation(published.seg(), g));
    std::thread::spawn(move || {
        let seg = backing.seg();
        let server = RingServer::new(seg, notifier)?;
        let arena = DataArena::new(seg, arena_offset, arena_len, SLOTS as usize);
        let mut d = d;
        let served = aether_rpc::serve_with_arena(&server, Some(&arena), &mut d, &stop);
        // The reference holds the generation callback, and with it the ring.
        drop(d);
        served
    })
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
    /// Refuses every mount; answers unmount.
    Refuse,
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
