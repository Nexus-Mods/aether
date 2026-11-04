//! Running the target: natively, as a child of this process, and under the
//! shim, as `aether-injector`'s target with a ring this process serves.
//!
//! A shim run lays out a fresh ring in a named section at
//! [`Geometry::SHIM_TESTS`] (the geometry the shim's counted costs are
//! recorded at, so a case's requests per call line up with that record),
//! serves it on [`SERVE_THREADS`] threads, each with its own
//! `aether_win::EventNotifier` on the director's two events (`docs/wire.md`
//! 2.9), and starts
//!
//! ```text
//! aether-injector <target> <aether_shim.dll> <run>\shim.cfg <run>\ready -- --out <run>\out.json --sync <events> <case> <n> <path>
//! ```
//!
//! with the transport names set (and the others of `docs/wire.md` 5.2's
//! transport and inject tables removed). Every request crosses a
//! headers-only [`Recorder`] (no payload is copied); the target's events
//! around its timed loop (`--sync`) say when to take the recorder's mark
//! and when to count. Faults are in the chain only when a run arms some
//! ([`ShimOptions::attach_faults`]), never in a timed one.
//!
//! Every child is waited for. One still running at the run's deadline is
//! killed and reaped: the target directly, or the injector, whose job
//! (kill on close) takes the target with it.
#![allow(unsafe_code)] // events, waits and process handles

use crate::{Case, Summary};
use aether_refdirector::harness::{serve_threads_of, Fault, Faults, Geometry, Match};
use aether_refdirector::{Null, ReadStyle, Recorder, Reference};
use aether_rpc::Tap;
use aether_win::{EventNotifier, SharedMapping};
use serde_json::Value;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

/// The director's serve threads: `aether_ipc::DEFAULT_WORKER_COUNT`, as the
/// shim's tests serve with the ring header's worker hint left at 0.
pub const SERVE_THREADS: usize = aether_ipc::DEFAULT_WORKER_COUNT;

/// How long the injector waits for the shim's ready report.
const READY_TIMEOUT_SECS: u64 = 30;

/// Who answers the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorKind {
    /// [`Reference`] serving [`crate::tree_files`].
    Reference,
    /// [`Null`]: every request OK with nothing in it. The target runs
    /// `--lenient`, since no file is found.
    Null,
}

impl DirectorKind {
    pub fn name(self) -> &'static str {
        match self {
            DirectorKind::Reference => "reference",
            DirectorKind::Null => "null",
        }
    }

    pub fn parse(s: &str) -> Option<DirectorKind> {
        match s {
            "reference" => Some(DirectorKind::Reference),
            "null" => Some(DirectorKind::Null),
            _ => None,
        }
    }
}

/// The programs a run starts.
#[derive(Debug, Clone)]
pub struct Artifacts {
    pub injector: PathBuf,
    pub shim: PathBuf,
    pub target: PathBuf,
}

impl Artifacts {
    /// `aether-injector.exe`, `aether_shim.dll` and
    /// `aether-shimbench-target.exe` in the running program's cargo profile
    /// folder (`aether_testkit::artifacts::locate`).
    pub fn locate() -> Result<Artifacts, String> {
        let profile = aether_testkit::artifacts::profile_dir();
        let release = profile.file_name().is_some_and(|n| n == "release");
        let flag = if release { " --release" } else { "" };
        let find = |name: &str, package: &str| {
            aether_testkit::artifacts::locate(name).ok_or_else(|| {
                format!(
                    "{name} not found near {}: cargo build{flag} -p {package}",
                    profile.display()
                )
            })
        };
        Ok(Artifacts {
            injector: find("aether-injector.exe", "aether-inject")?,
            shim: find("aether_shim.dll", "aether-shim")?,
            target: find("aether-shimbench-target.exe", "aether-shimbench")?,
        })
    }
}

/// The folders a bench works in, removed on drop.
pub struct Workspace {
    dir: tempfile::TempDir,
    /// The tree on disk, for the native runs.
    pub native: PathBuf,
    /// Root 0 of the shim runs: an empty folder, served by the director.
    pub virtual_root: PathBuf,
    /// Outside every root: `real.txt`.
    pub outside: PathBuf,
}

impl Workspace {
    pub fn new() -> std::io::Result<Workspace> {
        let dir = aether_testkit::tempdir()?;
        let native = dir.path().join("native");
        let virtual_root = dir.path().join("virtual");
        let outside = dir.path().join("outside");
        crate::write_tree(&native)?;
        std::fs::create_dir_all(&virtual_root)?;
        std::fs::create_dir_all(&outside)?;
        std::fs::write(outside.join("real.txt"), crate::OUTSIDE_BYTES)?;
        Ok(Workspace {
            dir,
            native,
            virtual_root,
            outside,
        })
    }

    /// A new, empty folder for one run's files.
    fn run_dir(&self) -> std::io::Result<PathBuf> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let p = self
            .dir
            .path()
            .join(format!("run-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    /// What `case` works on: under `top` (the native tree or the virtual
    /// root), or outside every root.
    pub fn path(&self, case: Case, top: &Path) -> PathBuf {
        let base = if case.outside() { &self.outside } else { top };
        base.join(case.rel_path().replace('/', "\\"))
    }
}

/// The reference serving [`crate::tree_files`]: `data/` read-only (so its
/// files are immutable), `saves/` writable.
pub fn reference() -> Reference {
    let mut r = Reference::new()
        .with_dir("data")
        .with_dir("data/list")
        .with_dir("saves")
        .writable_under("saves/");
    for (rel, bytes) in crate::tree_files() {
        r = r.with(&rel, bytes, ReadStyle::Whole);
    }
    r
}

/// The target's line, read back.
#[derive(Debug, Clone)]
pub struct TargetOutput {
    pub ns: Summary,
    pub line: Value,
}

fn parse_output(text: &str) -> Result<TargetOutput, String> {
    let line: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("the target's line: {e}"))?;
    let ns = line
        .get("ns")
        .and_then(Summary::from_json)
        .ok_or_else(|| format!("the target's line has no summary: {line}"))?;
    Ok(TargetOutput { ns, line })
}

/// Requests counted during a timed loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counted {
    pub requests: u64,
    pub ring_bytes: u64,
}

/// How a shim run is served.
pub struct ShimOptions {
    pub director: DirectorKind,
    /// Faults armed before the target attaches (its HEARTBEAT); empty for a
    /// timed run, which then has no `Faults` in its chain.
    pub attach_faults: Vec<(Match, Fault)>,
}

/// What a shim run gave.
#[derive(Debug)]
pub struct ShimOutcome {
    /// The injector's exit code (`docs/wire.md` 6.3), `None` if it had to be
    /// killed.
    pub exit: Option<i32>,
    /// The ready file's content, if it is there.
    pub ready: Option<String>,
    /// `<ready>.injector-error`, if the injector wrote it.
    pub injector_error: Option<String>,
    /// The target's line, if it wrote one.
    pub output: Option<Result<TargetOutput, String>>,
    /// The timed loop's requests, if the target reached its loop's end.
    pub counted: Option<Counted>,
    /// The injector's standard error.
    pub log: String,
}

impl ShimOutcome {
    /// The target's numbers, or why there are none.
    pub fn result(&self) -> Result<(TargetOutput, Counted), String> {
        let why = || {
            let mut s = format!("injector exit {:?}", self.exit);
            if let Some(e) = &self.injector_error {
                s.push_str(&format!(", {e}"));
            }
            if let Some(r) = &self.ready {
                s.push_str(&format!(", ready file {r:?}"));
            }
            let tail: Vec<&str> = self.log.lines().rev().take(5).collect();
            if !tail.is_empty() {
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                s.push_str(&format!("; injector log: {}", tail.join(" | ")));
            }
            s
        };
        if self.exit != Some(0) {
            return Err(why());
        }
        match (&self.output, self.counted) {
            (Some(Ok(o)), Some(c)) => Ok((o.clone(), c)),
            (Some(Err(e)), _) => Err(e.clone()),
            _ => Err(format!("the target wrote no line ({})", why())),
        }
    }
}

/// Runs cases.
pub struct Bench {
    pub artifacts: Artifacts,
    pub workspace: Workspace,
    /// How long one run may take before its process is killed.
    pub timeout: Duration,
}

impl Bench {
    pub fn new(artifacts: Artifacts, timeout: Duration) -> std::io::Result<Bench> {
        Ok(Bench {
            artifacts,
            workspace: Workspace::new()?,
            timeout,
        })
    }

    /// The target natively, on the tree on disk.
    pub fn run_native(&self, case: Case, iterations: u64) -> Result<TargetOutput, String> {
        let run = self.workspace.run_dir().map_err(|e| e.to_string())?;
        let out = run.join("out.json");
        let path = self.workspace.path(case, &self.workspace.native);
        let mut child = Command::new(&self.artifacts.target)
            .arg("--out")
            .arg(&out)
            .arg(case.name())
            .arg(iterations.to_string())
            .arg(&path)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("start the target: {e}"))?;
        let exit = wait_or_kill(&mut child, Instant::now() + self.timeout);
        match exit {
            Some(0) => {}
            Some(c) => return Err(format!("the native target exited {c}")),
            None => return Err(format!("the native target ran past {:?}", self.timeout)),
        }
        let text = std::fs::read_to_string(&out).map_err(|e| format!("read {out:?}: {e}"))?;
        parse_output(&text)
    }

    /// The target under the shim, its ring served as `opts` says.
    pub fn run_shim(
        &self,
        case: Case,
        iterations: u64,
        opts: &ShimOptions,
    ) -> Result<ShimOutcome, String> {
        let recorder = Recorder::headers_only();
        let g = Geometry::SHIM_TESTS;
        if opts.attach_faults.is_empty() {
            self.serve_and_launch(case, iterations, opts.director, recorder.clone(), &recorder)
        } else {
            let faults = Faults::new(g.payload_cap);
            let faults = match g.arena() {
                Some(a) => faults.with_arena(a),
                None => faults,
            };
            for (when, fault) in &opts.attach_faults {
                faults.arm(when.clone(), fault.clone());
            }
            let chain = (recorder.clone(), faults.clone());
            let r = self.serve_and_launch(case, iterations, opts.director, chain, &recorder);
            faults.clear();
            r
        }
    }

    fn serve_and_launch<T: Tap + Clone + Send>(
        &self,
        case: Case,
        iterations: u64,
        director: DirectorKind,
        tap: T,
        recorder: &Recorder,
    ) -> Result<ShimOutcome, String> {
        fn io(what: &'static str) -> impl Fn(std::io::Error) -> String {
            move |e| format!("{what}: {e}")
        }
        let g = Geometry::SHIM_TESTS;
        let name = unique_name();
        let section = format!("{name}-ring");
        let (server_ev, client_ev) = (format!("{name}-server"), format!("{name}-client"));
        let mapping = SharedMapping::create(&section, g.map_bytes()).map_err(io("the section"))?;
        g.init(mapping.seg(), 0)
            .map_err(|e| format!("ring init: {e:?}"))?;
        // The events exist (and stay) while the threads open their own
        // handles on them.
        let _events = EventNotifier::create(&server_ev, &client_ev).map_err(io("the events"))?;
        let notifier = || EventNotifier::open(&server_ev, &client_ev).expect("the ring's events");
        let sync = SyncEvents::create(&name).map_err(io("the sync events"))?;

        let run = self.workspace.run_dir().map_err(io("the run folder"))?;
        let virtual_root = &self.workspace.virtual_root;
        let config = run.join("shim.cfg");
        let root_text = virtual_root.to_str().ok_or("a root that is not UTF-8")?;
        std::fs::write(&config, aether_protocol::shimcfg::encode_config(root_text))
            .map_err(io("shim.cfg"))?;
        let ready = run.join("ready");
        let out = run.join("out.json");
        let log_path = run.join("injector.log");
        let log = std::fs::File::create(&log_path).map_err(io("injector.log"))?;

        let mut cmd = Command::new(&self.artifacts.injector);
        cmd.arg(&self.artifacts.target)
            .arg(&self.artifacts.shim)
            .arg(&config)
            .arg(&ready)
            .arg("--");
        if director == DirectorKind::Null {
            cmd.arg("--lenient");
        }
        cmd.arg("--out")
            .arg(&out)
            .arg("--sync")
            .arg(&name)
            .arg(case.name())
            .arg(iterations.to_string())
            .arg(self.workspace.path(case, virtual_root));
        for n in aether_env::handshake::TRANSPORT
            .iter()
            .chain(aether_env::handshake::INJECT.iter())
        {
            cmd.env_remove(n);
        }
        cmd.env(aether_env::RING_SECTION, &section)
            .env(aether_env::RING_BYTES, g.map_bytes().to_string())
            .env(aether_env::RING_PAYLOAD_CAP, g.payload_cap.to_string())
            .env(aether_env::ARENA_OFFSET, g.arena_offset().to_string())
            .env(aether_env::ARENA_LEN, g.arena_len.to_string())
            .env(aether_env::SERVER_EV, &server_ev)
            .env(aether_env::CLIENT_EV, &client_ev)
            .env(aether_env::VIRTUAL_DIR, virtual_root)
            .env(aether_env::STATE_DIR, &run)
            .env(
                aether_env::READY_TIMEOUT_SECS,
                READY_TIMEOUT_SECS.to_string(),
            )
            .stdin(Stdio::null())
            .stderr(Stdio::from(log));

        let stop = AtomicBool::new(false);
        let reference = reference();
        let deadline = Instant::now() + self.timeout;
        let (exit, counted) = std::thread::scope(|s| {
            let workers = match director {
                DirectorKind::Reference => serve_threads_of(
                    s,
                    SERVE_THREADS,
                    mapping.seg(),
                    g.arena(),
                    &notifier,
                    || reference.share(),
                    &tap,
                    &stop,
                ),
                DirectorKind::Null => serve_threads_of(
                    s,
                    SERVE_THREADS,
                    mapping.seg(),
                    g.arena(),
                    &notifier,
                    || Null,
                    &tap,
                    &stop,
                ),
            };
            let result = match cmd.spawn() {
                Ok(mut child) => {
                    let counted = sync.count(&child, recorder, deadline);
                    let exit = wait_or_kill(&mut child, deadline);
                    Ok((exit, counted))
                }
                Err(e) => Err(format!("start the injector: {e}")),
            };
            stop.store(true, Ordering::Release);
            for w in workers {
                let _ = w.join();
            }
            result
        })?;

        let read = |p: &Path| std::fs::read_to_string(p).ok();
        let error_file = PathBuf::from(format!(
            "{}{}",
            ready.display(),
            aether_env::INJECTOR_ERROR_SUFFIX
        ));
        Ok(ShimOutcome {
            exit,
            ready: read(&ready),
            injector_error: read(&error_file),
            output: read(&out).map(|t| parse_output(&t)),
            counted,
            log: read(&log_path).unwrap_or_default(),
        })
    }
}

/// A name no other run (in this process or another) uses.
fn unique_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "Local\\aether-shimbench-{}-{}-{nanos}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Wait for `child` until `deadline`; past it, kill it. Either way it is
/// reaped. Its exit code, or `None` when it was killed.
fn wait_or_kill(child: &mut Child, deadline: Instant) -> Option<i32> {
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.code(),
            Ok(None) => {}
            Err(_) => break,
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let h = child.as_raw_handle() as HANDLE;
        // SAFETY: the child's process handle, alive while `child` is.
        unsafe { WaitForMultipleObjects(1, &h, 0, left.as_millis().min(1_000) as u32) };
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// The four auto-reset events the target sets and waits on around its timed
/// loop (`aether-shimbench-target --sync`).
struct SyncEvents {
    begin: HANDLE,
    go: HANDLE,
    end: HANDLE,
    done: HANDLE,
}

impl SyncEvents {
    fn create(name: &str) -> std::io::Result<SyncEvents> {
        let make = |suffix: &str| -> std::io::Result<HANDLE> {
            let w: Vec<u16> = format!("{name}-{suffix}")
                .encode_utf16()
                .chain(core::iter::once(0))
                .collect();
            // SAFETY: a NUL-terminated name; auto-reset, unsignalled.
            let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, w.as_ptr()) };
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            Ok(h)
        };
        let mut made = SyncEvents {
            begin: core::ptr::null_mut(),
            go: core::ptr::null_mut(),
            end: core::ptr::null_mut(),
            done: core::ptr::null_mut(),
        };
        made.begin = make("begin")?;
        made.go = make("go")?;
        made.end = make("end")?;
        made.done = make("done")?;
        Ok(made)
    }

    /// The requests between the target's `begin` and `end`: at `begin` take
    /// the recorder's mark and let it go; at `end` count since the mark and
    /// let it finish. `None` if the child exited first or the deadline came.
    fn count(&self, child: &Child, recorder: &Recorder, deadline: Instant) -> Option<Counted> {
        let process = child.as_raw_handle() as HANDLE;
        if !wait_event(self.begin, process, deadline) {
            return None;
        }
        let mark = recorder.mark();
        // SAFETY: an event this struct owns.
        unsafe { SetEvent(self.go) };
        if !wait_event(self.end, process, deadline) {
            return None;
        }
        let counted = Counted {
            requests: recorder.total_since(mark) as u64,
            ring_bytes: recorder.ring_bytes_since(mark),
        };
        // SAFETY: as above.
        unsafe { SetEvent(self.done) };
        Some(counted)
    }
}

/// Wait for `event` while `process` runs, until `deadline`: whether the
/// event came.
fn wait_event(event: HANDLE, process: HANDLE, deadline: Instant) -> bool {
    let handles = [event, process];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        // SAFETY: two live handles; wait for either.
        let r = unsafe {
            WaitForMultipleObjects(2, handles.as_ptr(), 0, left.as_millis().min(1_000) as u32)
        };
        if r == WAIT_OBJECT_0 {
            return true;
        }
        if r == WAIT_OBJECT_0 + 1 {
            return false;
        }
    }
}

impl Drop for SyncEvents {
    fn drop(&mut self) {
        for h in [self.begin, self.go, self.end, self.done] {
            if !h.is_null() {
                // SAFETY: an event this struct created, closed once.
                unsafe { CloseHandle(h) };
            }
        }
    }
}
