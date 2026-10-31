//! The APC phase: `ReadFileEx`, whose completion is a callback run during the
//! caller's alertable wait. The read has finished by the time the call
//! returns, and the bytes are in the buffer, but the caller can't know until
//! the callback runs: Starfield's reader waits for that and nothing else.
//!
//! Driven by `VFS_FIXTURE_APC_PATH`, a file of at least two bytes, read from
//! its second byte on, and `VFS_FIXTURE_FILL`, the byte it holds. Exits 1 with
//! a `FIXTURE FAIL: apc:` line unless the callback ran, once, on that wait, and
//! reported the bytes that were read.

use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::process::exit;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Storage::FileSystem::{ReadFileEx, FILE_FLAG_OVERLAPPED};
use windows_sys::Win32::System::Threading::SleepEx;
use windows_sys::Win32::System::IO::OVERLAPPED;

/// `SleepEx`'s result when an APC ended the wait.
const WAIT_IO_COMPLETION: u32 = 0xC0;
/// How long the callback gets. It is due at once: this is only the failure's cost.
const PATIENCE_MS: u32 = 5000;
const LEN: usize = 4096;

static CALLS: AtomicU32 = AtomicU32::new(0);
static ERROR: AtomicU32 = AtomicU32::new(u32::MAX);
static BYTES: AtomicU32 = AtomicU32::new(0);

fn fail(msg: String) -> ! {
    eprintln!("FIXTURE FAIL: apc: {msg}");
    exit(1);
}

unsafe extern "system" fn done(error: u32, bytes: u32, _overlapped: *mut OVERLAPPED) {
    CALLS.fetch_add(1, Ordering::SeqCst);
    ERROR.store(error, Ordering::SeqCst);
    BYTES.store(bytes, Ordering::SeqCst);
}

pub fn run() {
    let Ok(path) = std::env::var("VFS_FIXTURE_APC_PATH") else {
        return;
    };
    let fill: u8 = std::env::var("VFS_FIXTURE_FILL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| fail("VFS_FIXTURE_FILL is not a byte".into()));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(&path)
        .unwrap_or_else(|e| fail(format!("open {path}: {e}")));
    let mut buf = vec![0u8; LEN];
    // SAFETY: all zero is an OVERLAPPED at offset 0, and both it and `buf` outlive the wait.
    let mut overlapped: OVERLAPPED = unsafe { core::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = 1;
    // SAFETY: as above; the handle is open and the routine has the documented signature.
    let started = unsafe {
        ReadFileEx(
            file.as_raw_handle() as _,
            buf.as_mut_ptr(),
            LEN as u32,
            &mut overlapped,
            Some(done),
        )
    };
    if started == 0 {
        fail(format!("ReadFileEx: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: an alertable sleep, with no memory of ours.
    let woke = unsafe { SleepEx(PATIENCE_MS, 1) };
    if woke != WAIT_IO_COMPLETION {
        fail(format!(
            "no completion after {PATIENCE_MS} ms (SleepEx gave {woke})"
        ));
    }
    let (calls, error, bytes) = (
        CALLS.load(Ordering::SeqCst),
        ERROR.load(Ordering::SeqCst),
        BYTES.load(Ordering::SeqCst) as usize,
    );
    if calls != 1 || error != 0 {
        fail(format!("{calls} completions, the last with error {error}"));
    }
    if bytes == 0 || bytes > LEN || buf[..bytes].iter().any(|b| *b != fill) {
        fail(format!("completion reported {bytes} bytes, not the file's"));
    }
}
