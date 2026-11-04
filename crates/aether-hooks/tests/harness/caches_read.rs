//! The read cache (`aether_ipc::readcache`, fed by the hooks) request by
//! request, as the recorder saw it: a hit costs no round trip, and the
//! cache fetches again exactly when its coherence rule says.
//!
//! The rule, as `crates/aether-ipc/src/readcache/mod.rs` states it: only
//! handles the director opened read-only and immutable are served; blocks
//! are kept per file (root and folded path) and per version (size and
//! `mount_gen` at open); a write open, an open the director reports
//! mutable, a write, a truncate, a delete or a rename through this process
//! drops the file and stops caching it for the process. A handle still
//! open on an older version reads uncached.
//!
//! The tree: `data/a.esp`, 1 MiB of `pattern` bytes on a read-only
//! provider, which the reference answers immutable; `saves/s.ess` on a
//! writable one (mutable); and `mods/`, writable, whose OPEN replies an
//! `Override` marks immutable where a test needs a file it can also write,
//! delete or rename. A block is 64 KiB; a small read is one under 64 KiB.
//! Reads the cache does not serve are 1 KiB, so that each is one inline
//! READ (the ring's 4 KiB cap holds 4088 bytes).
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{self, key, pattern, Fake, Fault, Match, Options, ReadStyle, Served};
use crate::ntapi;
use aether_protocol::{Opcode, Reply, Request, RequestFlags, Status};
use aether_refdirector::harness::{Mark, OpenEdit};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, MoveFileExW, ReadFile, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// The cache's unit, and what one miss fetches first.
const BLOCK: usize = 64 * KIB;
/// A read the cache does not serve crosses as one inline READ of this.
const SMALL: usize = KIB;

/// A virtual root (`<tmp>/root`, with the tree's folders on disk), kept for
/// the life of the test process.
fn root() -> PathBuf {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let root = dir.path().join("root");
    for d in ["data", "saves", "mods"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::mem::forget(dir);
    root
}

/// Other bytes of the same length as `pattern(len)`, differing at every
/// position: what a remount or a recreate puts at a path.
fn other(len: usize) -> Vec<u8> {
    pattern(len).into_iter().map(|b| !b).collect()
}

fn fake() -> Fake {
    Fake::new()
        .with("data/a.esp", pattern(MIB), ReadStyle::Whole)
        .with("saves/s.ess", pattern(MIB), ReadStyle::Whole)
        .writable_under("saves/")
        .with_dir("mods")
        .with_dir("mods/dir")
        .with("mods/m.esp", pattern(MIB), ReadStyle::Whole)
        .with("mods/n.esp", pattern(MIB), ReadStyle::Whole)
        .with("mods/dir/x.bin", pattern(MIB), ReadStyle::Whole)
        .writable_under("mods/")
}

/// Serve `fake` with the arena on and install the hooks.
fn serve_at(root: &Path, fake: Fake) -> &'static Served {
    let served = fakedirector::install_with(root, fake, Options::default());
    std::mem::forget(aether_hooks::install().expect("install"));
    served
}

fn serve(fake: Fake) -> (PathBuf, &'static Served) {
    let root = root();
    let served = serve_at(&root, fake);
    (root, served)
}

/// Every OPEN under `mods/` answered immutable, as a director whose
/// writable provider claimed it: the files a test can also change.
fn immutable_mods(served: &Served) {
    served.overrides.open(
        "mods/",
        OpenEdit {
            immutable: Some(true),
            ..OpenEdit::default()
        },
    );
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's last-error value.
    unsafe { GetLastError() }
}

fn open_with(p: &Path, access: u32) -> HANDLE {
    let w = wide(p);
    // SAFETY: a NUL-terminated name; no security attributes or template.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(
        h,
        INVALID_HANDLE_VALUE,
        "open {}: {}",
        p.display(),
        last_error()
    );
    h
}

fn open_read(p: &Path) -> HANDLE {
    open_with(p, GENERIC_READ)
}

fn close(h: HANDLE) {
    // SAFETY: a handle this test opened, closed once.
    assert_ne!(unsafe { CloseHandle(h) }, 0, "CloseHandle");
}

/// One `NtReadFile` of `len` bytes at `offset`: the bytes it returned.
fn read_at(h: HANDLE, offset: usize, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let (st, n) = ntapi::nt_read_at(h, offset as i64, &mut buf);
    assert!(st >= 0, "NtReadFile at {offset}: {st:#x}");
    buf.truncate(n.min(len));
    buf
}

/// One `ReadFile` of `len` bytes at the handle's position.
fn read_next(h: HANDLE, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut got = 0u32;
    // SAFETY: `buf` holds `len` bytes; synchronous, no OVERLAPPED.
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr().cast(),
            len as u32,
            &mut got,
            core::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "ReadFile: {}", last_error());
    buf.truncate(got as usize);
    buf
}

/// One READ as it crossed the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rd {
    bulk: bool,
    offset: u64,
    len: u32,
    status: i32,
}

/// A READ the cache fetched: one bulk block at `offset`, answered OK.
fn fetch(offset: usize) -> Rd {
    Rd {
        bulk: true,
        offset: offset as u64,
        len: BLOCK as u32,
        status: Status::Ok.code(),
    }
}

/// A read the cache did not serve: one inline READ of [`SMALL`], answered OK.
fn through(offset: usize) -> Rd {
    Rd {
        bulk: false,
        offset: offset as u64,
        len: SMALL as u32,
        status: Status::Ok.code(),
    }
}

/// Every request since `mark`, which must all be READs, as [`Rd`]s.
fn reads(served: &Served, mark: Mark) -> Vec<Rd> {
    let seen = served.recorder.since(mark);
    seen.iter()
        .map(|r| match r.request() {
            Some(Request::Read { offset, len, .. }) => Rd {
                bulk: r.flags & RequestFlags::READ_BULK.bits() != 0,
                offset,
                len,
                status: r.status,
            },
            other => panic!("not a READ: {other:?} in {seen:#?}"),
        })
        .collect()
}

/// The OPEN replies' `(immutable, mount_gen, size)` since `mark`.
fn opens(served: &Served, mark: Mark) -> Vec<(bool, u32, u64)> {
    served
        .recorder
        .since(mark)
        .iter()
        .filter_map(|r| match r.reply() {
            Some(Reply::Open(o)) => Some((o.immutable, o.mount_gen, o.size)),
            _ => None,
        })
        .collect()
}

fn stats() -> aether_ipc::CacheStats {
    aether_hooks::read_cache_stats_for_tests().expect("the read cache is on")
}

/// A small read at `offset` of the file `h` holds bytes `want` of, which
/// costs exactly `cost` READs.
fn assert_read(served: &Served, h: HANDLE, offset: usize, want: &[u8], cost: &[Rd]) {
    let mark = served.recorder.mark();
    let got = read_at(h, offset, SMALL);
    assert!(
        got == want[offset..offset + SMALL],
        "the bytes at {offset} are not the director's"
    );
    assert_eq!(reads(served, mark), cost, "the read at {offset}");
}

#[test]
fn a_small_read_of_an_immutable_file_fetches_one_block_then_hits_at_zero_round_trips() {
    isolate!();
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let h = open_read(&root.join("data").join("a.esp"));

    let mark = served.recorder.mark();
    assert_eq!(read_next(h, 4 * KIB), want[..4 * KIB]);
    assert_eq!(reads(served, mark), [fetch(0)]);

    let mark = served.recorder.mark();
    for i in 1..16 {
        let at = i * 4 * KIB;
        assert!(read_next(h, 4 * KIB) == want[at..at + 4 * KIB], "read {i}");
    }
    assert_eq!(
        served.recorder.total_since(mark),
        0,
        "{:#?}",
        served.recorder.since(mark)
    );
    let c = stats();
    assert_eq!((c.fetches, c.misses, c.hits), (1, 1, 15), "{c:?}");
    close(h);
}

#[test]
fn a_mutable_file_is_read_through_every_time() {
    isolate!();
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let h = open_read(&root.join("saves").join("s.ess"));
    for at in [0, 0, SMALL, 8 * KIB, 0] {
        assert_read(served, h, at, &want, &[through(at)]);
    }
    let c = stats();
    assert_eq!((c.fetches, c.hits, c.misses), (0, 0, 0), "{c:?}");
    close(h);
}

#[test]
fn an_immutable_answer_on_a_write_handle_is_not_cached() {
    isolate!();
    let (root, served) = serve(fake());
    immutable_mods(served);
    let want = pattern(MIB);
    let mark = served.recorder.mark();
    let h = open_with(
        &root.join("mods").join("m.esp"),
        GENERIC_READ | GENERIC_WRITE,
    );
    assert_eq!(
        opens(served, mark),
        [(true, 0, MIB as u64)],
        "the override answers the write open immutable"
    );
    for at in [0, 0, SMALL] {
        assert_read(served, h, at, &want, &[through(at)]);
    }
    assert_eq!(stats().fetches, 0);
    close(h);
}

#[test]
fn a_write_open_anywhere_poisons_the_file_for_the_process() {
    isolate!();
    let (root, served) = serve(fake());
    immutable_mods(served);
    let want = pattern(MIB);
    let path = root.join("mods").join("m.esp");
    let r = open_read(&path);
    assert_read(served, r, 0, &want, &[fetch(0)]);
    assert_read(served, r, SMALL, &want, &[]);

    let w = open_with(&path, GENERIC_WRITE);
    for at in [0, SMALL, 0] {
        assert_read(served, r, at, &want, &[through(at)]);
    }
    // Poisoned for the process: a read open after the write handle is gone
    // is not cached again either.
    close(w);
    let r2 = open_read(&path);
    for at in [0, 0] {
        assert_read(served, r2, at, &want, &[through(at)]);
    }
    close(r2);
    close(r);
}

#[test]
fn a_mutable_open_anywhere_poisons_the_file_for_the_process() {
    isolate!();
    let (root, served) = serve(fake());
    immutable_mods(served);
    let want = pattern(MIB);
    let path = root.join("mods").join("m.esp");
    let r = open_read(&path);
    assert_read(served, r, 0, &want, &[fetch(0)]);
    assert_read(served, r, SMALL, &want, &[]);

    // The director now calls the same file mutable.
    served.overrides.clear();
    let mark = served.recorder.mark();
    let m = open_read(&path);
    assert_eq!(opens(served, mark), [(false, 0, MIB as u64)]);
    for at in [0, SMALL] {
        assert_read(served, r, at, &want, &[through(at)]);
    }
    close(m);
    close(r);
}

#[test]
fn a_second_handle_spelled_differently_shares_the_blocks() {
    isolate!();
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let a = open_read(&root.join("Data").join("A.esp"));
    assert_read(served, a, 0, &want, &[fetch(0)]);
    let b = open_read(&root.join("data").join("a.ESP"));
    for at in [0, 8 * KIB, BLOCK - SMALL] {
        assert_read(served, b, at, &want, &[]);
    }
    close(b);
    close(a);
}

#[test]
fn the_same_path_under_another_root_is_another_file() {
    isolate!();
    let base = aether_testkit::tempdir().expect("temp folder");
    let root0 = base.path().join("root0");
    let root1 = base.path().join("root1");
    for r in [&root0, &root1] {
        std::fs::create_dir_all(r.join("data")).unwrap();
    }
    std::env::set_var(
        aether_env::VIRTUAL_ROOTS,
        format!("1={}", root1.to_string_lossy()),
    );
    let served = serve_at(
        &root0,
        Fake::new()
            .with(&key(0, "data/a.esp"), pattern(MIB), ReadStyle::Whole)
            .with(&key(1, "data/a.esp"), other(MIB), ReadStyle::Whole),
    );
    let (p, o) = (pattern(MIB), other(MIB));
    let h0 = open_read(&root0.join("data").join("a.esp"));
    let h1 = open_read(&root1.join("data").join("a.esp"));
    assert_read(served, h0, 0, &p, &[fetch(0)]);
    assert_read(served, h1, 0, &o, &[fetch(0)]);
    assert_read(served, h0, SMALL, &p, &[]);
    assert_read(served, h1, SMALL, &o, &[]);
    close(h1);
    close(h0);
    std::mem::forget(base);
}

#[test]
fn a_new_mount_gen_refetches_and_serves_the_new_bytes() {
    isolate!();
    let (root, served) = serve(fake());
    let path = root.join("data").join("a.esp");
    let (old, new) = (pattern(MIB), other(MIB));
    served.fake.set_mount_gen(1);
    let h1 = open_read(&path);
    assert_read(served, h1, 0, &old, &[fetch(0)]);
    assert_read(served, h1, SMALL, &old, &[]);

    // A remount: other bytes at the same path, the same size.
    served.fake.replace_content("data/a.esp", new.clone());
    served.fake.set_mount_gen(2);
    let mark = served.recorder.mark();
    let h2 = open_read(&path);
    assert_eq!(opens(served, mark), [(true, 2, MIB as u64)]);
    assert_read(served, h2, 0, &new, &[fetch(0)]);
    assert_read(served, h2, SMALL, &new, &[]);
    // The handle opened at generation 1 is not served from generation 2's
    // blocks, nor from its own (they are gone): it reads uncached, and gets
    // what the director answers now (`ReadCache::register`'s doc).
    for at in [0, SMALL] {
        assert_read(served, h1, at, &new, &[through(at)]);
    }
    close(h2);
    close(h1);
}

#[test]
fn a_changed_size_at_the_same_mount_gen_refetches() {
    isolate!();
    let (root, served) = serve(fake());
    let path = root.join("data").join("a.esp");
    let want = pattern(MIB);
    let h1 = open_read(&path);
    assert_read(served, h1, 0, &want, &[fetch(0)]);

    served.overrides.open(
        "data/a.esp",
        OpenEdit {
            size: Some(512 * KIB as u64),
            ..OpenEdit::default()
        },
    );
    let mark = served.recorder.mark();
    let h2 = open_read(&path);
    assert_eq!(opens(served, mark), [(true, 0, 512 * KIB as u64)]);
    assert_read(served, h2, 0, &want, &[fetch(0)]);
    assert_read(served, h2, SMALL, &want, &[]);
    // The 1 MiB handle's version is not the file's any more.
    for at in [0, SMALL] {
        assert_read(served, h1, at, &want, &[through(at)]);
    }
    close(h2);
    close(h1);
}

#[test]
fn delete_and_rename_through_this_process_drop_the_file() {
    isolate!();
    let (root, served) = serve(fake());
    immutable_mods(served);
    let (old, new) = (pattern(MIB), other(MIB));
    let mods = root.join("mods");

    // A delete: the open handle's blocks go. The director then puts other
    // bytes at the path, which the open handle reads (the reference reads
    // a handle by its path): served from the old blocks it would be the
    // old bytes, at no cost.
    let h = open_read(&mods.join("m.esp"));
    assert_read(served, h, 0, &old, &[fetch(0)]);
    let w = wide(&mods.join("m.esp"));
    // SAFETY: a NUL-terminated name.
    assert_ne!(unsafe { DeleteFileW(w.as_ptr()) }, 0, "{}", last_error());
    served.fake.replace_content("mods/m.esp", new.clone());
    assert_read(served, h, 0, &new, &[through(0)]);
    // And an immutable open of the path after the delete is not believed.
    let again = open_read(&mods.join("m.esp"));
    for at in [0, 0] {
        assert_read(served, again, at, &new, &[through(at)]);
    }
    close(again);
    close(h);

    // A rename of a file: the source's blocks go, and the target is not
    // cached either.
    let h = open_read(&mods.join("n.esp"));
    assert_read(served, h, 0, &old, &[fetch(0)]);
    let (from, to) = (wide(&mods.join("n.esp")), wide(&mods.join("n2.esp")));
    // SAFETY: NUL-terminated names.
    assert_ne!(
        unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) },
        0,
        "{}",
        last_error()
    );
    served.fake.replace_content("mods/n.esp", new.clone());
    assert_read(served, h, 0, &new, &[through(0)]);
    let target = open_read(&mods.join("n2.esp"));
    for at in [0, 0] {
        assert_read(served, target, at, &old, &[through(at)]);
    }
    close(target);
    close(h);

    // A rename of a directory: everything held under it goes.
    let h = open_read(&mods.join("dir").join("x.bin"));
    assert_read(served, h, 0, &old, &[fetch(0)]);
    let (from, to) = (wide(&mods.join("dir")), wide(&mods.join("moved")));
    // SAFETY: NUL-terminated names.
    assert_ne!(
        unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) },
        0,
        "{}",
        last_error()
    );
    served.fake.replace_content("mods/dir/x.bin", new.clone());
    assert_read(served, h, 0, &new, &[through(0)]);
    close(h);
}

#[test]
fn a_read_at_or_above_the_threshold_bypasses_the_cache() {
    isolate!();
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let h = open_read(&root.join("data").join("a.esp"));
    let mark = served.recorder.mark();
    assert!(read_at(h, 0, BLOCK) == want[..BLOCK]);
    assert_eq!(reads(served, mark), [fetch(0)], "one READ of its own size");
    let c = stats();
    assert_eq!(
        (c.fetches, c.hits, c.misses, c.declined),
        (0, 0, 0, 0),
        "{c:?}"
    );
    // Nothing of that range is held: a small read in it fetches its block.
    assert_read(served, h, 4 * KIB, &want, &[fetch(0)]);
    assert_read(served, h, 8 * KIB, &want, &[]);
    close(h);
}

#[test]
fn the_switch_turns_it_off() {
    isolate!();
    std::env::set_var(aether_env::SHIM_READ_CACHE, "0");
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let h = open_read(&root.join("data").join("a.esp"));
    for at in [0, 0, SMALL, 8 * KIB] {
        assert_read(served, h, at, &want, &[through(at)]);
    }
    assert!(aether_hooks::read_cache_stats_for_tests().is_none());
    close(h);
}

#[test]
fn a_failed_block_fetch_falls_back_to_an_uncached_read_with_the_same_bytes() {
    isolate!();
    let (root, served) = serve(fake());
    let want = pattern(MIB);
    let id = served.faults.arm(
        Match {
            op: Some(Opcode::Read),
            path_prefix: Some("data/a.esp".into()),
            nth: Some(1),
            ..Match::default()
        },
        Fault::Status(Status::IoError),
    );
    let h = open_read(&root.join("data").join("a.esp"));
    let failed = Rd {
        status: Status::IoError.code(),
        ..fetch(0)
    };
    assert_read(served, h, 0, &want, &[failed, through(0)]);
    assert_eq!(served.faults.applied(id), 1);
    let c = stats();
    assert_eq!((c.fetch_failures, c.fetches), (1, 0), "{c:?}");
    // One failure does not put the file off: the next small read fetches
    // the block, and the one after hits.
    assert_read(served, h, SMALL, &want, &[fetch(0)]);
    assert_read(served, h, 2 * SMALL, &want, &[]);
    close(h);
}
