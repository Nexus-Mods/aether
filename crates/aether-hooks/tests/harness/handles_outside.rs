//! The open hooks' shortcut for a file outside every root, which records
//! nothing about the handle: which opens take it, and that a handle value
//! issued again never keeps another handle's record.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{pattern, Fake, ReadStyle};
use crate::fileops::{close, wide, SHARE_ALL};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};

const DELETE: u32 = 0x0001_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// A real file outside every root that this user may open with every right
/// (`GENERIC_ALL` asks for `WRITE_OWNER`, which an inherited Modify grant
/// lacks), then the director serving `data/a.esp` at a root, and the hooks.
fn setup() -> (PathBuf, PathBuf) {
    let outside = aether_testkit::tempdir().expect("temp folder");
    let file = outside.path().join("real-b.txt");
    std::fs::write(&file, b"on disk").unwrap();
    std::mem::forget(outside);
    // Everyone (S-1-1-0): full control.
    let granted = std::process::Command::new("icacls")
        .arg(&file)
        .args(["/grant", "*S-1-1-0:F"])
        .output()
        .expect("icacls");
    assert!(granted.status.success(), "icacls: {granted:?}");
    let (root, _served) =
        crate::fileops::serve(Fake::new().with("data/a.esp", pattern(64), ReadStyle::Whole));
    (root, file)
}

fn open(p: &Path, access: u32) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            SHARE_ALL,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(
        h,
        INVALID_HANDLE_VALUE,
        "open {access:#x}: {}",
        crate::fileops::last_error()
    );
    h
}

/// An open whose access can include `DELETE` records the handle's path, so
/// a delete or rename through it finds the path the file was opened as; an
/// open that cannot delete records nothing.
#[test]
fn an_open_that_may_delete_records_its_path_and_no_other() {
    isolate!();
    let (_root, file) = setup();
    for (access, records) in [
        (GENERIC_READ, false),
        (DELETE, true),
        (GENERIC_ALL, true),
        (MAXIMUM_ALLOWED, true),
        (FILE_ALL_ACCESS, true),
    ] {
        let before = aether_hooks::tracked_handle_count();
        let h = open(&file, access);
        let after = aether_hooks::tracked_handle_count();
        close(h);
        assert_eq!(
            after - before,
            usize::from(records),
            "access {access:#x}: recorded {}",
            after - before
        );
    }
}

/// A handle value that held a record of a file under a root, issued again
/// for a file outside every root, names the new file. The record's clear
/// waits for the handle table, also when another thread holds it.
#[test]
fn a_handle_value_issued_again_names_the_new_file() {
    isolate!();
    let (root, file) = setup();
    let stale = format!(r"\??\{}", root.join("data").join("a.esp").display());
    // Windows issues a closed handle's value again to the next handle as a
    // rule, not a promise: another thread may take it first. So the case is
    // tried until the value comes back, and a run where it never does says
    // nothing about the shim.
    let Some(named) = (0..20).find_map(|_| reissued_name(&file, &stale)) else {
        eprintln!("SKIP: Windows never issued the closed handle's value again");
        return;
    };
    assert!(
        named.to_ascii_lowercase().ends_with(r"\real-b.txt"),
        "the handle is named {named}, not as the new file"
    );
}

/// Open `file`, close it, give its handle value a stale record of `stale`,
/// and open `file` again while another thread holds the handle table: the
/// new handle's name, if it has the old value.
fn reissued_name(file: &Path, stale: &str) -> Option<String> {
    // The holder thread is started first: a thread is itself a handle.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        go_rx.recv().unwrap();
        aether_hooks::with_handle_table_held_for_tests(|| {
            held_tx.send(()).unwrap();
            // A bound only: the open below waits for this release when it
            // clears the record, and says it is done when it does not.
            let _ = done_rx.recv_timeout(Duration::from_millis(500));
        });
    });

    let h = open(file, GENERIC_READ);
    close(h);
    aether_hooks::plant_under_root_record_for_tests(h as isize, stale);
    go_tx.send(()).unwrap();
    held_rx.recv().unwrap();
    let again = open(file, GENERIC_READ);
    let _ = done_tx.send(());
    holder.join().unwrap();

    let named =
        (again == h).then(|| crate::reg::object_name(again as isize).expect("NtQueryObject"));
    close(again);
    named
}
