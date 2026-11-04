//! What crosses the ring, request by request, as the recorder saw it: an
//! open, read and close; a stat of a missing file; a path outside every root
//! (nothing); the client's deadline set through `install_global`; and an
//! attach whose HEARTBEAT is refused or late (wire.md 3.4.11), after which
//! the hooks pass through.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{self, Fake, Fault, Match, Options, ReadStyle, RingKind};
use aether_hooks::director::FuseInitError;
use aether_protocol::{Opcode, OpenFlags, Request, Status};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesExW, GetFileExInfoStandard, ReadFile, FILE_SHARE_READ,
    OPEN_EXISTING, WIN32_FILE_ATTRIBUTE_DATA,
};

/// A virtual root (`<tmp>/root`, with `Data` on disk so the folder exists)
/// and the folder that holds it, kept for the life of the test process.
fn root() -> PathBuf {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::mem::forget(dir);
    root
}

/// The reference with `Data/a.esp`: 1 KiB of `pattern` bytes, read-only, so
/// a block the read cache fetches fits one inline reply.
fn fake() -> Fake {
    Fake::new().with("Data/a.esp", fakedirector::pattern(1024), ReadStyle::Whole)
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

fn open_read(p: &Path) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    }
}

fn read(h: HANDLE, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let mut got = 0u32;
    // SAFETY: `buf` holds `n` bytes; synchronous, no OVERLAPPED.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            n as u32,
            &mut got,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "ReadFile");
    buf.truncate(got as usize);
    buf
}

/// `GetFileAttributesExW` on `p`: whether it succeeded.
fn stat(p: &Path) -> bool {
    let w = wide(p);
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated name and a buffer of the standard class's type.
    unsafe {
        GetFileAttributesExW(
            w.as_ptr(),
            GetFileExInfoStandard,
            (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        ) != 0
    }
}

fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn on(op: Opcode) -> Match {
    Match {
        op: Some(op),
        ..Match::default()
    }
}

#[test]
fn an_open_and_read_cross_the_ring_as_open_read_close() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, fake(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let h = open_read(&root.join("Data").join("A.esp"));
    assert_ne!(h, INVALID_HANDLE_VALUE, "CreateFileW");
    assert_eq!(read(h, 100), fakedirector::pattern(1024)[..100]);
    close(h);

    let seen = served.recorder.since(mark);
    let ops: Vec<_> = seen.iter().map(|r| r.op()).collect();
    assert_eq!(
        ops,
        [Some(Opcode::Open), Some(Opcode::Read), Some(Opcode::Close)],
        "{seen:#?}"
    );
    match seen[0].request() {
        Some(Request::Open { flags, path, .. }) => {
            assert_eq!(path, "data/a.esp", "the path crosses folded");
            assert_eq!(flags, OpenFlags::READ);
        }
        other => panic!("not an OPEN: {other:?}"),
    }
}

#[test]
fn getattr_of_a_missing_file_is_one_request() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(&root, fake(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    assert!(!stat(&root.join("Data").join("missing.esp")));

    let seen = served.recorder.since(mark);
    let ops: Vec<_> = seen.iter().map(|r| r.op()).collect();
    assert_eq!(ops, [Some(Opcode::GetAttr)], "{seen:#?}");
}

#[test]
fn a_path_outside_the_root_sends_nothing() {
    isolate!();
    let root = root();
    let outside = aether_testkit::tempdir().expect("temp folder");
    let file = outside.path().join("real.txt");
    std::fs::write(&file, b"on disk, outside every root").unwrap();
    let served = fakedirector::install_with(&root, fake(), Options::default());
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();

    let h = open_read(&file);
    assert_ne!(h, INVALID_HANDLE_VALUE, "CreateFileW");
    assert_eq!(read(h, 100), b"on disk, outside every root");
    close(h);
    assert!(stat(&file));

    assert_eq!(
        served.recorder.total_since(mark),
        0,
        "{:#?}",
        served.recorder.since(mark)
    );
}

/// How long the deadline tests hold a reply: as long as a delay can.
const HOLD: Duration = aether_refdirector::harness::MAX_DELAY;
/// A call that gave up at its 300 ms deadline is back by this, on any
/// runner, and one that waited for the reply is not: it is well under
/// [`HOLD`].
const GAVE_UP_BY: Duration = Duration::from_secs(3);

#[test]
fn install_global_s_deadline_is_the_client_s() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(
        &root,
        fake(),
        Options {
            deadline: Some(ms(300)),
            ..Options::default()
        },
    );
    assert!(served.attach.is_ok(), "{:?}", served.attach);
    let _hooks = aether_hooks::install().expect("install");
    let slow = served.faults.arm(on(Opcode::GetAttr), Fault::Delay(HOLD));
    let mark = served.recorder.mark();

    let t = Instant::now();
    let ok = stat(&root.join("Data").join("a.esp"));
    let took = t.elapsed();
    // The serve thread holds the reply until the delay is released.
    assert!(served.faults.wait_applied(slow, 1, Duration::from_secs(10)));
    served.faults.release();

    assert!(!ok, "a GETATTR past the deadline fails the call");
    assert!(took < GAVE_UP_BY, "gave up after {took:?}");
    assert!(served.recorder.wait_for(mark, 1, Duration::from_secs(10)));
    assert_eq!(served.recorder.count_since(mark, Opcode::GetAttr), 1);
}

#[test]
fn a_heartbeat_answered_non_ok_fails_the_attach_and_the_hooks_pass_through() {
    isolate!();
    let root = root();
    let served = fakedirector::install_with(
        &root,
        fake(),
        Options {
            attach_faults: vec![(on(Opcode::Heartbeat), Fault::Status(Status::IoError))],
            ..Options::default()
        },
    );
    assert!(
        matches!(served.attach, Err(FuseInitError::ConnectFailed(_))),
        "{:?}",
        served.attach
    );
    assert_eq!(served.faults.applied(served.attach_faults[0]), 1);
    assert!(aether_hooks::director::global().is_none());

    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();
    let h = open_read(&root.join("Data").join("a.esp"));
    if h != INVALID_HANDLE_VALUE {
        close(h);
    }
    assert!(!stat(&root.join("Data").join("a.esp")));
    assert_eq!(
        served.recorder.total_since(mark),
        0,
        "{:#?}",
        served.recorder.since(mark)
    );
}

#[test]
fn a_heartbeat_past_the_deadline_fails_the_attach_within_it() {
    isolate!();
    let root = root();
    let t = Instant::now();
    let served = fakedirector::install_with(
        &root,
        fake(),
        Options {
            deadline: Some(ms(300)),
            attach_faults: vec![(on(Opcode::Heartbeat), Fault::Delay(HOLD))],
            ..Options::default()
        },
    );
    let took = t.elapsed();
    assert!(served
        .faults
        .wait_applied(served.attach_faults[0], 1, Duration::from_secs(10)));
    served.faults.release();

    assert!(
        matches!(served.attach, Err(FuseInitError::ConnectFailed(_))),
        "{:?}",
        served.attach
    );
    assert!(took < GAVE_UP_BY, "gave up after {took:?}");
    assert!(aether_hooks::director::global().is_none());
}

/// An open, read and close over `ring`: what each transport carries.
fn open_read_close_over(ring: RingKind) {
    let root = root();
    let served = fakedirector::install_with(
        &root,
        fake(),
        Options {
            ring,
            ..Options::default()
        },
    );
    let _hooks = aether_hooks::install().expect("install");
    let mark = served.recorder.mark();
    let h = open_read(&root.join("Data").join("a.esp"));
    assert_ne!(h, INVALID_HANDLE_VALUE, "CreateFileW");
    assert_eq!(read(h, 100), fakedirector::pattern(1024)[..100]);
    close(h);
    assert_eq!(served.recorder.total_since(mark), 3);
}

#[test]
fn the_wake_event_ring_serves_the_hooks() {
    isolate!();
    open_read_close_over(RingKind::SectionWithWake);
    assert!(std::env::var_os(aether_env::SERVER_EV).is_some());
}

#[test]
fn a_file_ring_serves_the_hooks() {
    isolate!();
    open_read_close_over(RingKind::File);
    assert!(std::env::var_os(aether_env::RING_PATH).is_some());
    assert!(std::env::var_os(aether_env::RING_SECTION).is_none());
}
