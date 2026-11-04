//! Win32 file calls and recorder views shared by the spelling, handle and
//! overlay tests (`caches_names`, `caches_handles`, `overlay_answers`).
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{self, Fake, Options, Served};
use aether_protocol::Opcode;
use aether_refdirector::harness::Mark;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindClose, FindFirstFileW, FindNextFileW, GetFinalPathNameByHandleW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};

/// Every share mode: the tests hold handles across deletes and renames.
pub(crate) const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// A virtual root (`<tmp>/root`), kept for the life of the test process.
/// Nothing under it is on disk: every name is the director's.
pub(crate) fn root() -> PathBuf {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::mem::forget(dir);
    root
}

/// Serve `fake` on the default ring at a new root and install the hooks.
pub(crate) fn serve(fake: Fake) -> (PathBuf, &'static Served) {
    let root = root();
    let served = fakedirector::install_with(&root, fake, Options::default());
    std::mem::forget(aether_hooks::install().expect("install"));
    (root, served)
}

pub(crate) fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

pub(crate) fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error value.
    unsafe { GetLastError() }
}

/// `CreateFileW` with every share mode: the handle, or the Win32 error.
pub(crate) fn create(p: &Path, access: u32, disposition: u32, flags: u32) -> Result<HANDLE, u32> {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            SHARE_ALL,
            core::ptr::null(),
            disposition,
            flags,
            core::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(h)
    }
}

/// An existing file or directory opened with `access`.
pub(crate) fn open_with(p: &Path, access: u32) -> HANDLE {
    create(p, access, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS)
        .unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
}

pub(crate) fn open_read(p: &Path) -> HANDLE {
    open_with(p, GENERIC_READ)
}

pub(crate) fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

/// `GetFinalPathNameByHandleW(h, .., 0)`: the final DOS path with its
/// `\\?\` prefix.
pub(crate) fn final_path(h: HANDLE) -> String {
    let mut buf = vec![0u16; 1024];
    // SAFETY: a buffer of `buf.len()` units.
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) };
    assert!(
        n > 0 && (n as usize) < buf.len(),
        "GetFinalPathNameByHandleW: {n}, {}",
        last_error()
    );
    String::from_utf16_lossy(&buf[..n as usize])
}

/// The part of `final_path(h)` under `root`, as spelled (`\Mods\Sub\F.esp`).
/// The root itself is matched case apart: its spelling is the declared one.
pub(crate) fn tail(root: &Path, h: HANDLE) -> String {
    let full = final_path(h);
    let want = format!(r"\\?\{}", root.display());
    assert!(
        full.len() >= want.len() && full[..want.len()].eq_ignore_ascii_case(&want),
        "{full} is not under {want}"
    );
    full[want.len()..].to_string()
}

/// `FindFirstFileW(dir\*)` then `FindNextFileW` to the end: each record,
/// or the first call's Win32 error.
pub(crate) fn find_all(dir: &Path) -> Result<Vec<WIN32_FIND_DATAW>, u32> {
    let w = wide(&dir.join("*"));
    // SAFETY: plain data, all-zero is valid.
    let mut data: WIN32_FIND_DATAW = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated pattern and a find-data buffer.
    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let mut out = vec![data];
    // SAFETY: the find handle from above, closed once below.
    while unsafe { FindNextFileW(h, &mut data) } != 0 {
        out.push(data);
    }
    // SAFETY: as above.
    unsafe { FindClose(h) };
    Ok(out)
}

/// A find record's name.
pub(crate) fn find_name(d: &WIN32_FIND_DATAW) -> String {
    let n = d.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
    String::from_utf16_lossy(&d.cFileName[..n])
}

/// The names `FindFirstFileW(dir\*)` lists.
pub(crate) fn names(dir: &Path) -> Vec<String> {
    find_all(dir)
        .unwrap_or_else(|e| panic!("list {}: {e}", dir.display()))
        .iter()
        .map(find_name)
        .collect()
}

/// The opcodes of every request since `mark`.
pub(crate) fn ops(served: &Served, mark: Mark) -> Vec<Option<Opcode>> {
    served.recorder.ops_since(mark)
}
