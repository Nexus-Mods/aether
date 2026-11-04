//! A child started under a root, through STAGE_EXEC (`docs/wire.md`
//! 3.4.14, gap hunt S1): `hook/child_start.rs`'s `plan` and the process
//! hook (`hook/process.rs`).
//!
//! The test process calls `CreateProcessW` with an image path under root 0,
//! which has a backing directory (`VFS_ROOT_BACKINGS`), as a director's roots may.
//! A real copy of `aether-probe.exe` sits at that path on disk too, so a
//! child the shim let the kernel start from what is really there would show.
//! What the kernel started is read from the process itself
//! (`QueryFullProcessImageNameW`), through the test-only observer
//! `aether_hooks::on_child_created`, which the process hook calls with each
//! child it created, before injecting it.
//!
//! The shim the hook injects is this test executable, which never signals
//! ready (as in `diagnostics/child_inject_fails_closed.rs`), so a started
//! child is then killed and its `CreateProcessW` fails with
//! `ERROR_PROCESS_ABORTED`. That refusal is not what is tested here; which
//! image the child was created from is. Every child is waited for until it
//! has gone.
//!
//! Cross-reference: conformance C13 (another branch) checks that a
//! director's STAGE_EXEC answer is usable.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{self, Fake, Fault, Match, Options, ReadStyle};
use aether_protocol::{Opcode, Request, RootId, Status};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PROCESS_ABORTED,
    HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
};

/// The served image's name under root 0.
pub(crate) const IMAGE: &str = "aether-probe.exe";

/// An image on disk under root 0 that the director does not serve.
const HIDDEN: &str = "hidden-probe.exe";

/// How long a child the hook killed may take to be gone.
const GONE_MS: u32 = 10_000;

/// One child the process hook created: its image as the kernel has it, and
/// a handle of the test's own to wait for it (an address: `HANDLE` is not
/// `Send`).
struct Created {
    image: String,
    handle: usize,
}

/// Every child created since the hooks went in.
static CREATED: Mutex<Vec<Created>> = Mutex::new(Vec::new());

/// The observer: what the kernel started, read while the child exists.
fn observe(process: HANDLE, _pid: u32) {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    // SAFETY: `process` is the hook's handle of a child it just created;
    // `buf` holds `n` units.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n)
    };
    let image = if ok != 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    };
    let mut dup: HANDLE = core::ptr::null_mut();
    // SAFETY: a duplicate of a valid handle into this process, for waiting.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(
            me,
            process,
            me,
            &mut dup,
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            0,
        );
    }
    CREATED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Created {
            image,
            handle: dup as usize,
        });
}

/// Take what the observer saw, waiting for each child to be gone.
pub(crate) fn created() -> Vec<String> {
    let all = std::mem::take(&mut *CREATED.lock().unwrap_or_else(|e| e.into_inner()));
    all.into_iter()
        .map(|c| {
            let h = c.handle as HANDLE;
            assert!(!h.is_null(), "a handle to the child {:?}", c.image);
            // SAFETY: the test's own duplicate, closed once here.
            unsafe {
                let w = WaitForSingleObject(h, GONE_MS);
                if w != WAIT_OBJECT_0 {
                    TerminateProcess(h, 1);
                    WaitForSingleObject(h, GONE_MS);
                }
                CloseHandle(h);
                assert_eq!(w, WAIT_OBJECT_0, "the child {:?} was left running", c.image);
            }
            c.image
        })
        .collect()
}

/// `aether-probe.exe` as cargo built it, beside this test's profile folder.
/// `None` prints SKIP, or panics under `VFS_TEST_REQUIRE_ALL=1`.
pub(crate) fn probe() -> Option<PathBuf> {
    let exe = std::env::current_exe().expect("current exe");
    let deps = exe.parent().expect("test exe folder");
    let profile = if deps.file_name().is_some_and(|n| n == "deps") {
        deps.parent().expect("profile folder")
    } else {
        deps
    };
    let p = profile.join(IMAGE);
    if p.is_file() {
        return Some(p);
    }
    if aether_env::opt_in("VFS_TEST_REQUIRE_ALL") {
        panic!("{} is missing: cargo build -p aether-inject", p.display());
    }
    eprintln!(
        "SKIP: {} is missing (cargo build -p aether-inject)",
        p.display()
    );
    None
}

/// The test's folders, kept for the life of the test process.
pub(crate) struct Setup {
    /// Root 0's location, with a real `aether-probe.exe` in it.
    pub(crate) root: PathBuf,
    /// Where the director staged its copy.
    pub(crate) staged: PathBuf,
    /// Root 0's backing directory, on disk.
    pub(crate) backing: PathBuf,
    pub(crate) served: &'static fakedirector::Served,
    pub(crate) _hooks: aether_hooks::HookGuard,
}

/// What the director holds and how the roots are laid out.
pub(crate) struct Given {
    /// The reference stages `IMAGE` (else STAGE_EXEC is NotSupported).
    pub(crate) stages: bool,
    /// Root 0 has a backing directory.
    pub(crate) backed: bool,
    /// The backing directory is not on disk when the hooks go in.
    pub(crate) missing_backing: bool,
    /// Faults armed on STAGE_EXEC.
    pub(crate) fault: Option<Fault>,
}

pub(crate) fn setup(
    probe: &Path,
    given: Given,
) -> (Setup, Option<aether_refdirector::harness::FaultId>) {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let base = dir.path().to_path_buf();
    std::mem::forget(dir);
    let root = base.join("root");
    let backing = base.join("backing");
    let stage = base.join("stage");
    for d in [&root, &backing, &stage] {
        std::fs::create_dir_all(d).unwrap();
    }
    if given.missing_backing {
        std::fs::remove_dir(&backing).unwrap();
    }
    std::fs::copy(probe, root.join(IMAGE)).unwrap();
    // A real file under the root that the VFS does not hold.
    std::fs::copy(probe, root.join(HIDDEN)).unwrap();
    let staged = stage.join(IMAGE);
    std::fs::copy(probe, &staged).unwrap();

    if given.backed {
        std::env::set_var(
            aether_env::ROOT_BACKINGS,
            format!("0={}", backing.display()),
        );
    }
    // A child that never reports ready fails fast, and its refusal is logged
    // in the test's folder.
    std::env::set_var(aether_env::READY_TIMEOUT_SECS, "2");
    std::env::set_var(aether_env::CHILD_REFUSED_LOG, base.join("refused.log"));

    // What the director serves need not be an image: the kernel never reads
    // it, only the staged copy.
    let mut fake = Fake::new().with(IMAGE, b"MZ served".to_vec(), ReadStyle::Whole);
    if given.stages {
        fake = fake.staging(IMAGE, &staged.to_string_lossy());
    }
    let served = fakedirector::install_with(&root, fake, Options::default());
    let id = given.fault.map(|f| {
        served.faults.arm(
            Match {
                op: Some(Opcode::StageExec),
                ..Match::default()
            },
            f,
        )
    });
    assert!(aether_hooks::on_child_created(observe), "the observer");
    let hooks = aether_hooks::install().expect("install");
    (
        Setup {
            root,
            staged,
            backing,
            served,
            _hooks: hooks,
        },
        id,
    )
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// `CreateProcessW(app, "\"app\"")`: the process id, or the call's error.
/// A child that did start is killed and waited for.
pub(crate) fn create(app: &Path) -> Result<u32, u32> {
    start(Some(app), &format!("\"{}\"", app.display()), None)
}

/// `CreateProcessW(app, cmd)` in `cwd` (the caller's own when `None`), as
/// [`create`].
fn start(app: Option<&Path>, cmd: &str, cwd: Option<&Path>) -> Result<u32, u32> {
    let app_w = app.map(|a| wide(&a.to_string_lossy()));
    let cwd_w = cwd.map(|d| wide(&d.to_string_lossy()));
    let mut cmd = wide(cmd);
    // SAFETY: zeroed is a valid STARTUPINFOW once `cb` is set.
    let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
    si.cb = core::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: zeroed is a valid out-parameter.
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: NUL-terminated strings, a mutable command line, no attributes,
    // the caller's environment and directory.
    let ok = unsafe {
        CreateProcessW(
            app_w.as_ref().map_or(core::ptr::null(), |a| a.as_ptr()),
            cmd.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            0,
            core::ptr::null(),
            cwd_w.as_ref().map_or(core::ptr::null(), |d| d.as_ptr()),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: plain FFI.
        return Err(unsafe { GetLastError() });
    }
    // SAFETY: the handles the successful call returned, closed once.
    unsafe {
        TerminateProcess(pi.hProcess, 1);
        WaitForSingleObject(pi.hProcess, GONE_MS);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(pi.dwProcessId)
}

/// The STAGE_EXEC requests since `mark`: (root, path, status, reply bytes).
fn stage_execs(
    s: &Setup,
    mark: aether_refdirector::harness::Mark,
) -> Vec<(RootId, String, i32, Vec<u8>)> {
    s.served
        .recorder
        .since(mark)
        .into_iter()
        .filter(|r| r.op() == Some(Opcode::StageExec))
        .map(|r| {
            let (root, path) = match r.request() {
                Some(Request::StageExec { root, path }) => (root, path.to_string()),
                other => panic!("a STAGE_EXEC request, got {other:?}"),
            };
            (root, path, r.status, r.reply.clone())
        })
        .collect()
}

fn same_path(a: &str, b: &Path) -> bool {
    let b = b.to_string_lossy();
    a.trim_start_matches(r"\\?\")
        .eq_ignore_ascii_case(b.trim_start_matches(r"\\?\"))
}

#[test]
fn a_child_started_under_a_root_runs_the_staged_copy() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );

    let calls = stage_execs(&s, mark);
    assert_eq!(calls.len(), 1, "one STAGE_EXEC: {calls:?}");
    let (root, path, status, reply) = &calls[0];
    assert_eq!((*root, path.as_str()), (RootId(0), IMAGE));
    assert_eq!(*status, Status::Ok.code());
    assert_eq!(reply, s.staged.to_string_lossy().as_bytes());

    let images = created();
    assert_eq!(images.len(), 1, "one child: {images:?}");
    assert!(
        same_path(&images[0], &s.staged),
        "the child runs {:?}, not the staged copy {:?}",
        images[0],
        s.staged
    );
}

/// A child under a root starts in the root's backing directory, a real
/// folder. One that is missing is made on disk, by the shim's own I/O: the
/// director is not asked about it, so only the image's GETATTR and
/// STAGE_EXEC cross the ring.
#[test]
fn a_missing_backing_directory_is_made_on_disk() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: true,
            fault: None,
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );
    let images = created();
    assert_eq!(images.len(), 1, "one child: {images:?}");
    assert!(
        aether_hooks::as_shim_io_for_tests(|| s.backing.is_dir()),
        "{} is not on disk",
        s.backing.display()
    );
    assert_eq!(
        s.served.recorder.ops_since(mark),
        [Some(Opcode::GetAttr), Some(Opcode::StageExec)]
    );
}

/// A child started in a root and named only by its command line: the
/// host's search for the image sees what the VFS sees. `PATH` names the
/// root, whose real folder holds `hidden-probe.exe`, which the director does
/// not serve, so the start fails as it would for a file that is not there
/// and nothing is started from the real folder.
#[test]
fn an_image_search_never_finds_a_file_the_vfs_does_not_hold() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    );
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut path = s.root.as_os_str().to_owned();
    path.push(";");
    path.push(&old);
    std::env::set_var("PATH", &path);
    let r = start(None, HIDDEN, Some(&s.root));
    std::env::set_var("PATH", old);
    let images = created();
    assert!(images.is_empty(), "nothing may start: {images:?}");
    assert_eq!(r, Err(ERROR_FILE_NOT_FOUND), "refused as file not found");
}

/// The same start for an image the VFS holds: the host is handed the staged
/// copy's absolute path, and that copy runs, never the real file at the root.
#[test]
fn an_image_named_by_the_command_line_in_a_root_runs_the_staged_copy() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    );
    let r = start(None, IMAGE, Some(&s.root));
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );
    let images = created();
    assert_eq!(images.len(), 1, "one child: {images:?}");
    assert!(
        same_path(&images[0], &s.staged),
        "the child runs {:?}, not the staged copy {:?}",
        images[0],
        s.staged
    );
}

/// A refused start: `CreateProcessW` fails with `ERROR_FILE_NOT_FOUND` (the
/// hook's refusal), no process was created at all, and exactly one
/// STAGE_EXEC crossed the ring, answered `status`.
fn assert_refused(
    s: &Setup,
    mark: aether_refdirector::harness::Mark,
    r: Result<u32, u32>,
    status: i32,
) {
    assert_eq!(r, Err(ERROR_FILE_NOT_FOUND), "refused as file not found");
    let images = created();
    assert!(images.is_empty(), "no child may start: {images:?}");
    let calls = stage_execs(s, mark);
    assert_eq!(calls.len(), 1, "one STAGE_EXEC: {calls:?}");
    assert_eq!(calls[0].2, status, "{calls:?}");
}

#[test]
fn stage_exec_not_found_refuses_the_start() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, id) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: Some(Fault::Status(Status::NotFound)),
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert_eq!(s.served.faults.applied(id.unwrap()), 1, "the fault fired");
    assert_refused(&s, mark, r, Status::NotFound.code());
}

#[test]
fn no_stager_refuses_an_image_under_a_root_with_no_staged_copy() {
    isolate!();
    let Some(probe) = probe() else { return };
    // The reference stages nothing: every STAGE_EXEC is NotSupported.
    let (s, _) = setup(
        &probe,
        Given {
            stages: false,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert_refused(&s, mark, r, Status::NotSupported.code());
}

#[test]
fn an_undecodable_stage_exec_reply_refuses_the_start() {
    isolate!();
    let Some(probe) = probe() else { return };
    // Not UTF-8: a lone continuation byte, then a byte UTF-8 never has.
    let garbage = vec![b'C', b':', b'\\', 0x80, 0xFF, b'x'];
    let (s, id) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: Some(Fault::Garbage(garbage.clone())),
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert_eq!(s.served.faults.applied(id.unwrap()), 1, "the fault fired");
    assert_refused(&s, mark, r, Status::Ok.code());
    assert_eq!(
        stage_execs(&s, mark)[0].3,
        garbage,
        "the client got the garbage"
    );
}

/// A pin, not a rule of wire.md: `plan` stages only when some root has a
/// backing directory. Without one, nothing is asked of the director and the
/// child is created from the path as named, so the kernel starts what is
/// really on disk there (here the real copy at the root's location).
/// wire.md 3.4.14's refusal sentence does not state that precondition.
#[test]
fn without_root_backings_a_child_under_a_root_starts_as_named() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: false,
            missing_backing: false,
            fault: None,
        },
    );
    let mark = s.served.recorder.mark();
    let r = create(&s.root.join(IMAGE));
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );
    assert!(stage_execs(&s, mark).is_empty(), "no STAGE_EXEC");
    let images = created();
    assert_eq!(images.len(), 1, "one child: {images:?}");
    assert!(
        same_path(&images[0], &s.root.join(IMAGE)),
        "the child runs {:?}",
        images[0]
    );
}
