//! A child started under a root, through STAGE_EXEC (`docs/wire.md`
//! 3.4.14): `hook/child_start.rs`'s `plan` and the process
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
//! `aether-conform` checks the other side: that a director's STAGE_EXEC
//! answer is usable.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{self, Fake, Fault, Match, Options, ReadStyle};
use aether_protocol::{Opcode, Request, RootId, Status};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
    ERROR_PROCESS_ABORTED, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetExitCodeProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
};

/// The served image's name under root 0.
pub(crate) const IMAGE: &str = "aether-probe.exe";

/// An image on disk under root 0 that the director does not serve.
const HIDDEN: &str = "hidden-probe.exe";

/// A served image in a folder with a space in its name, staged as
/// [`IMAGE`] is.
const SPACED: &str = "My Tool/aether-probe.exe";

/// A directory the director serves, named like an image.
const DIR_EXE: &str = "adir.exe";

/// An image with no extension, served under root 0 and real in the outside
/// folder.
const BARE: &str = "tool";

/// An image with no extension beside its `.exe` twin, both served under
/// root 0 and both real in the outside folder.
const TWIN: &str = "twin";
const TWIN_EXE: &str = "twin.exe";

/// How long a child the hook killed may take to be gone.
const GONE_MS: u32 = 10_000;

/// One child the process hook created: its image as the kernel has it, and
/// a handle of the test's own to wait for it (an address: `HANDLE` is not
/// `Send`).
struct Created {
    image: String,
    handle: usize,
    /// Its exit code, when the observer let it run ([`RUN_CHILDREN`]).
    exit: Option<u32>,
}

/// Every child created since the hooks went in.
static CREATED: Mutex<Vec<Created>> = Mutex::new(Vec::new());

/// The observer resumes each child and waits for it to end before the hook
/// injects it, so that what the child does is seen; the hook's injection of
/// a process that has ended then fails, as it would for any child that died.
static RUN_CHILDREN: AtomicBool = AtomicBool::new(false);

#[link(name = "ntdll")]
extern "system" {
    fn NtResumeProcess(process: HANDLE) -> i32;
}

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
    let exit = RUN_CHILDREN.load(Ordering::SeqCst).then(|| {
        let mut code = 0u32;
        // SAFETY: the hook's handle of a suspended child it just created,
        // with every access right.
        unsafe {
            assert_eq!(NtResumeProcess(process), 0, "the child resumed");
            assert_eq!(WaitForSingleObject(process, GONE_MS), WAIT_OBJECT_0);
            GetExitCodeProcess(process, &mut code);
        }
        code
    });
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
            exit,
        });
}

/// Take what the observer saw, waiting for each child to be gone.
pub(crate) fn created() -> Vec<String> {
    created_with_exits().into_iter().map(|(i, _)| i).collect()
}

/// [`created`], with each child's exit code when it was let run.
fn created_with_exits() -> Vec<(String, Option<u32>)> {
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
            (c.image, c.exit)
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
    /// A folder outside every root, with a space in its name, holding a
    /// real `aether-probe.exe`.
    pub(crate) outside: PathBuf,
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
    setup_in(base, probe, given)
}

/// [`setup`] in `base`, a folder that exists: the root, the backing
/// directory, the staged copy and the refusal log go in it.
pub(crate) fn setup_in(
    base: PathBuf,
    probe: &Path,
    given: Given,
) -> (Setup, Option<aether_refdirector::harness::FaultId>) {
    let root = base.join("root");
    let backing = base.join("backing");
    let stage = base.join("stage");
    let outside = base.join("out side");
    for d in [&root, &backing, &stage, &outside] {
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
    for name in [IMAGE, BARE, TWIN, TWIN_EXE] {
        std::fs::copy(probe, outside.join(name)).unwrap();
    }

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
    let mut fake = Fake::new()
        .with(IMAGE, b"MZ served".to_vec(), ReadStyle::Whole)
        .with(SPACED, b"MZ served".to_vec(), ReadStyle::Whole)
        .with_dir(DIR_EXE);
    for name in [BARE, TWIN, TWIN_EXE] {
        fake = fake.with(name, b"MZ served".to_vec(), ReadStyle::Whole);
    }
    if given.stages {
        for name in [IMAGE, SPACED, BARE, TWIN, TWIN_EXE] {
            fake = fake.staging(name, &staged.to_string_lossy());
        }
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
            outside,
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
    let app = app.to_string_lossy();
    start(Some(&app), &format!("\"{app}\""), None)
}

/// `CreateProcessW(app, cmd)` in `cwd` (the caller's own when `None`), as
/// [`create`].
fn start(app: Option<&str>, cmd: &str, cwd: Option<&Path>) -> Result<u32, u32> {
    let app_w = app.map(wide);
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
    in_dir(&s.root);
    let r = start(None, IMAGE, Some(&s.root));
    assert_runs(r, &s.staged);
}

/// The parent's current directory is `dir`, and the system searches it for
/// a name with no backslash too.
fn in_dir(dir: &Path) {
    std::env::remove_var("NoDefaultCurrentDirectoryInExePath");
    std::env::set_current_dir(dir).expect("current directory");
}

/// The start created one child, from `image`.
fn assert_runs(r: Result<u32, u32>, image: &Path) {
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );
    let images = created();
    assert_eq!(images.len(), 1, "one child: {images:?}");
    assert!(
        same_path(&images[0], image),
        "the child runs {:?}, not {:?}",
        images[0],
        image
    );
}

/// The start failed with `code` and created nothing.
fn assert_fails(r: Result<u32, u32>, code: u32) {
    assert_eq!(r, Err(code));
    let images = created();
    assert!(images.is_empty(), "nothing may start: {images:?}");
}

fn served(probe: &Path) -> Setup {
    setup(
        probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: None,
        },
    )
    .0
}

/// `PATH` is `dirs`, then what it was.
fn path_first(dirs: &[&Path]) {
    let mut path = std::ffi::OsString::new();
    for d in dirs {
        path.push(d.as_os_str());
        path.push(";");
    }
    path.push(std::env::var_os("PATH").unwrap_or_default());
    std::env::set_var("PATH", path);
}

/// The search is the parent's: its current directory, not the one the child
/// is given. The root holds the image too, and is the child's working
/// directory; the real copy in the parent's runs.
#[test]
fn the_parents_current_directory_is_searched_not_the_childs() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.outside);
    let mark = s.served.recorder.mark();
    let r = start(None, IMAGE, Some(&s.root));
    assert_runs(r, &s.outside.join(IMAGE));
    assert!(stage_execs(&s, mark).is_empty(), "no STAGE_EXEC");
}

/// A folder under a root earlier in the search (the current directory)
/// holds the name, with `.exe` added: its staged copy runs, not the real
/// copy in a `PATH` folder.
#[test]
fn a_root_earlier_in_the_search_than_path_wins() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.root);
    path_first(&[&s.outside]);
    assert_runs(start(None, "aether-probe -x", None), &s.staged);
}

/// A relative `PATH` entry is the parent's current directory's: `root`,
/// from the folder above it, finds the image the VFS holds there.
#[test]
fn a_relative_path_entry_is_searched_from_the_current_directory() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(s.root.parent().unwrap());
    path_first(&[Path::new("root")]);
    assert_runs(start(None, "aether-probe", None), &s.staged);
}

/// `.\name` is the current directory's alone.
#[test]
fn a_dot_relative_name_is_the_current_directorys() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.root);
    assert_runs(start(None, r".\aether-probe", None), &s.staged);
}

/// An unquoted name with a space: the shorter name first (`<root>\My`,
/// which the VFS does not hold), then the longer, held and staged.
#[test]
fn an_unquoted_name_with_a_space_is_tried_as_the_system_tries_it() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    let cmd = format!(r"{}\My Tool\aether-probe.exe -x", s.root.display());
    let mark = s.served.recorder.mark();
    assert_runs(start(None, &cmd, None), &s.staged);
    let calls = stage_execs(&s, mark);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].1.ends_with("aether-probe.exe"), "{calls:?}");
    // Outside the roots, the real disk decides the same way.
    let cmd = format!(r"{}\{IMAGE} -x", s.outside.display());
    assert_runs(start(None, &cmd, None), &s.outside.join(IMAGE));
}

/// An application name is used as given, against the parent's current
/// directory: no search, and no `.exe` added.
#[test]
fn a_relative_application_name_is_the_current_directorys_as_given() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.root);
    assert_runs(start(Some(IMAGE), IMAGE, None), &s.staged);
    assert_fails(
        start(Some("aether-probe"), IMAGE, None),
        ERROR_FILE_NOT_FOUND,
    );
}

/// A name found nowhere fails as the system's search does, and the host is
/// never left to search for it.
#[test]
fn a_name_found_nowhere_fails_file_not_found() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.root);
    assert_fails(
        start(None, "no-such-program -x", None),
        ERROR_FILE_NOT_FOUND,
    );
}

/// A directory the VFS holds, named as the program: the system's answer for
/// a directory, `ERROR_ACCESS_DENIED`.
#[test]
fn a_directory_named_as_the_program_is_access_denied() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    let cmd = format!(r"{}\adir", s.root.display());
    assert_fails(start(None, &cmd, None), ERROR_ACCESS_DENIED);
}

/// A program named by a path with no extension: the file as written starts,
/// outside the roots (unquoted, in a folder with a space) and in one (its
/// staged copy, staged by that name). `.\` is the current directory's.
#[test]
fn a_path_to_an_image_with_no_extension_starts_it() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    let cmd = format!(r"{}\{BARE} -x", s.outside.display());
    assert_runs(start(None, &cmd, None), &s.outside.join(BARE));
    in_dir(&s.root);
    let mark = s.served.recorder.mark();
    assert_runs(start(None, &format!(r".\{BARE} -x"), None), &s.staged);
    let calls = stage_execs(&s, mark);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].1, BARE, "staged as written");
}

/// A path to a name with no extension beside its `.exe` twin: the name as
/// written wins, as the system's lookup tries it first; a searched name
/// (no path) is only ever the `.exe`.
#[test]
fn a_path_names_the_file_as_written_before_its_exe_twin() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    let cmd = format!(r#""{}\{TWIN}" -x"#, s.outside.display());
    assert_runs(start(None, &cmd, None), &s.outside.join(TWIN));
    in_dir(&s.outside);
    assert_runs(
        start(None, &format!(r".\{TWIN}"), None),
        &s.outside.join(TWIN),
    );
    assert_runs(start(None, TWIN, None), &s.outside.join(TWIN_EXE));
    in_dir(&s.root);
    for (cmd, staged_as) in [(format!(r".\{TWIN}"), TWIN), (TWIN.to_string(), TWIN_EXE)] {
        let mark = s.served.recorder.mark();
        assert_runs(start(None, &cmd, None), &s.staged);
        let calls = stage_execs(&s, mark);
        assert_eq!(calls.len(), 1, "{cmd}: {calls:?}");
        assert_eq!(calls[0].1, staged_as, "{cmd}");
    }
}

/// A directory named by a path, as written: the system's answer for a
/// directory, never its `.exe` twin.
#[test]
fn a_path_to_a_directory_is_access_denied() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    std::fs::copy(&probe, s.outside.with_extension("exe")).unwrap();
    let cmd = format!(r#""{}" -x"#, s.outside.display());
    assert_fails(start(None, &cmd, None), ERROR_ACCESS_DENIED);
}

/// A held name the director cannot stage ends the search refused: the real
/// copy later on `PATH` is never started instead.
#[test]
fn a_held_name_that_cannot_be_staged_refuses_the_search() {
    isolate!();
    let Some(probe) = probe() else { return };
    let (s, _) = setup(
        &probe,
        Given {
            stages: true,
            backed: true,
            missing_backing: false,
            fault: Some(Fault::Status(Status::NotFound)),
        },
    );
    in_dir(&s.root);
    path_first(&[&s.outside]);
    assert_fails(start(None, "aether-probe", None), ERROR_FILE_NOT_FOUND);
}

/// A director that does not answer a candidate's GETATTR refuses the start:
/// the search never moves on to a real copy on `PATH`.
#[test]
fn a_director_error_refuses_the_search() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    let id = s.served.faults.arm(
        Match {
            op: Some(Opcode::GetAttr),
            ..Match::default()
        },
        Fault::Status(Status::IoError),
    );
    in_dir(&s.root);
    path_first(&[&s.outside]);
    assert_fails(start(None, "aether-probe", None), ERROR_FILE_NOT_FOUND);
    assert!(s.served.faults.applied(id) >= 1, "the fault fired");
}

/// A program the VFS does not hold, named by the command line alone, by a
/// parent whose current directory is in a root, and started in that root:
/// it is found where it would be without the VFS (`cmd.exe` in the system
/// folder: `SysWOW64` for a 32-bit parent), and it runs with the command
/// line as written. Nothing is staged.
#[test]
fn a_program_outside_the_roots_named_by_the_command_line_in_a_root_starts() {
    isolate!();
    let Some(probe) = probe() else { return };
    let s = served(&probe);
    in_dir(&s.root);
    RUN_CHILDREN.store(true, Ordering::SeqCst);
    let mark = s.served.recorder.mark();
    let r = start(None, "cmd /c exit 7", Some(&s.root));
    assert!(
        matches!(r, Ok(_) | Err(ERROR_PROCESS_ABORTED)),
        "started, then refused only for its injection: {r:?}"
    );
    assert!(stage_execs(&s, mark).is_empty(), "no STAGE_EXEC");
    let children = created_with_exits();
    assert_eq!(children.len(), 1, "one child: {children:?}");
    let (image, exit) = &children[0];
    let system = if cfg!(target_pointer_width = "32") {
        "SysWOW64"
    } else {
        "System32"
    };
    let windows = std::env::var_os("SystemRoot").expect("SystemRoot");
    let cmd = Path::new(&windows).join(system).join("cmd.exe");
    assert!(
        same_path(image, &cmd),
        "the child runs {image:?}, not {cmd:?}"
    );
    assert_eq!(*exit, Some(7), "cmd ran `exit 7`");
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
