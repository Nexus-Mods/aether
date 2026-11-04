//! Synthetic handles that store director FUSE file handles.

use crate::handle_tags::{is_file_tagged, SYNTH_FILE_SLOT_MASK, SYNTH_FILE_TAG};
use std::collections::BTreeMap;
use std::sync::Mutex;

struct FuseOpen {
    fh: u64,
    /// Whether `fh` is a handle the director issued, so closing this handle
    /// closes it there. Not for the directory handle `try_fuse_mkdir` makes
    /// without an OPEN: any u64 is a valid `fh` (docs/wire.md 3.4.3), 0
    /// included, so no value of `fh` can say "none".
    issued: bool,
    size: u64,
    is_dir: bool,
    position: u64,
    /// `FILE_APPEND_DATA` granted without `FILE_WRITE_DATA` (NT's
    /// append-only access). A real file object enforces "every write lands
    /// at the current end of file" at the kernel level, ignoring whatever
    /// offset the caller passes; a synthetic handle has no kernel FCB to do
    /// that, so `write_hook` does it here instead, keyed off this flag.
    append_only: bool,
    /// Absolute NT/Win path for relative-open resolution (esp. directories).
    abs_path: Option<String>,
    /// This handle's file in the read cache (`crate::read_cache`), when the
    /// cache is on and this is a file. Whether reads through it may be served
    /// from the cache is the ref's to say ([`aether_ipc::FileRef::cacheable`]);
    /// a write handle holds one too, so a write or truncate through it can
    /// drop the file.
    cache: Option<aether_ipc::FileRef>,
    /// Opened with `FILE_DELETE_ON_CLOSE`: closing it deletes `abs_path` at the director.
    delete_on_close: bool,
    /// The file's `mtime` as GETATTR last answered it for this handle
    /// (`crate::hook::file_info`), until a write or truncate to the file
    /// through any handle in this process; `None` when not asked since.
    mtime: Option<i64>,
    /// Writes and truncates to the file through any handle, so a GETATTR
    /// that crossed one is not kept ([`set_mtime`]).
    changes: u32,
    /// Whether this handle has asked for its time ([`time_of`]): only such
    /// handles are walked when another handle on the file writes
    /// ([`TIMED`]).
    timed: bool,
}

/// How many handles in [`TABLE`] are `timed`. While none is, a write or
/// truncate touches only its own handle's entry.
static TIMED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A write or truncate through `handle` changed its file: the time every
/// timed handle on the same file keeps is dropped, and their change counts
/// move, as NTFS keeps times per file. The same file is the same path, case
/// apart. Under the table lock the caller holds; allocates nothing (the
/// writer's path is lent out of its entry for the walk).
fn file_changed(g: &mut BTreeMap<usize, FuseOpen>, handle: isize) {
    let Some(e) = g.get_mut(&(handle as usize)) else {
        return;
    };
    e.mtime = None;
    e.changes = e.changes.wrapping_add(1);
    if TIMED.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return;
    }
    let Some(path) = e.abs_path.take() else {
        return;
    };
    for (k, other) in g.iter_mut() {
        if *k != handle as usize
            && other.timed
            && other
                .abs_path
                .as_deref()
                .is_some_and(|p| aether_casefold::eq_folded(p, &path))
        {
            other.mtime = None;
            other.changes = other.changes.wrapping_add(1);
        }
    }
    if let Some(e) = g.get_mut(&(handle as usize)) {
        e.abs_path = Some(path);
    }
}

/// What a read through a handle needs, under one lock: see [`lookup_read`].
pub(crate) struct ReadView {
    pub fh: u64,
    pub size: u64,
    pub position: u64,
    pub cache: Option<aether_ipc::FileRef>,
}

static TABLE: Mutex<BTreeMap<usize, FuseOpen>> = Mutex::new(BTreeMap::new());
/// Director handles a demand-paged section reads through
/// (`crate::lazy_section`), by `fh`. Locked only while [`TABLE`] is held, so
/// a close and a section's release cannot cross.
static HELD: Mutex<BTreeMap<u64, Held>> = Mutex::new(BTreeMap::new());

/// Who still needs one director handle besides the program's file handle.
struct Held {
    /// Demand-paged sections over it not yet released ([`hold`]).
    regions: u32,
    /// The program closed its file handle while a section held `fh`: the
    /// last [`release`] sends the CLOSE.
    file_closed: bool,
}
static NEXT: Mutex<usize> = Mutex::new(1);

pub(crate) fn is_fuse_synth(handle: isize) -> bool {
    is_file_tagged(handle as usize)
}

/// A directory handle that holds no director handle: closing it sends no
/// CLOSE (see `FuseOpen::issued`).
pub(crate) fn open_dir_unissued() -> Option<isize> {
    insert(0, false, 0, true, None, false)
}

/// A handle the director issued (`fh`, from an OPEN). `append_only`: opened
/// with NT append-only access
/// (`FILE_APPEND_DATA` without `FILE_WRITE_DATA`): the tracked position seeds
/// at the file's *current* size (end of file), matching what the kernel
/// would enforce for a real handle opened the same way. Seeding at `0` — what
/// every caller did before this existed — makes the first append on a
/// reopened handle overwrite from the start instead, which is silent data
/// corruption disguised as a successful append.
pub(crate) fn open_fuse_at_ex(
    fh: u64,
    size: u64,
    is_dir: bool,
    abs_path: Option<String>,
    append_only: bool,
) -> Option<isize> {
    insert(fh, true, size, is_dir, abs_path, append_only)
}

fn insert(
    fh: u64,
    issued: bool,
    size: u64,
    is_dir: bool,
    abs_path: Option<String>,
    append_only: bool,
) -> Option<isize> {
    let mut next = NEXT.lock().ok()?;
    let slot = *next;
    *next = next.wrapping_add(1);
    let handle = (slot & SYNTH_FILE_SLOT_MASK) | SYNTH_FILE_TAG;
    let mut g = TABLE.lock().ok()?;
    g.insert(
        handle,
        FuseOpen {
            fh,
            issued,
            size,
            is_dir,
            position: if append_only { size } else { 0 },
            append_only,
            abs_path,
            cache: None,
            delete_on_close: false,
            mtime: None,
            changes: 0,
            timed: false,
        },
    );
    Some(handle as isize)
}

/// Attach the handle's read-cache file (`crate::read_cache::register`).
pub(crate) fn set_cache(handle: isize, cache: aether_ipc::FileRef) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.cache = Some(cache);
        }
    }
}

/// The handle's read-cache file, if it has one.
pub(crate) fn cache(handle: isize) -> Option<aether_ipc::FileRef> {
    let g = TABLE.lock().ok()?;
    g.get(&(handle as usize))?.cache.clone()
}

/// [`lookup`] for the read path: the director handle, size, position and
/// read-cache file, taken under the one lock acquisition a read pays for.
pub(crate) fn lookup_read(handle: isize) -> Option<ReadView> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize))?;
    Some(ReadView {
        fh: e.fh,
        size: e.size,
        position: e.position,
        cache: e.cache.clone(),
    })
}

/// What a handle's table entry says about its file: see [`lookup`].
pub(crate) struct FileView {
    pub fh: u64,
    pub size: u64,
    pub is_dir: bool,
    pub position: u64,
    pub append_only: bool,
}

pub(crate) fn lookup(handle: isize) -> Option<FileView> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize))?;
    Some(FileView {
        fh: e.fh,
        size: e.size,
        is_dir: e.is_dir,
        position: e.position,
        append_only: e.append_only,
    })
}

/// Absolute path recorded for a FUSE handle (for relative RootDirectory opens).
pub(crate) fn abs_path(handle: isize) -> Option<String> {
    let g = TABLE.lock().ok()?;
    g.get(&(handle as usize))?.abs_path.clone()
}

/// Record that the file behind `handle` is now at `abs_path`: it was renamed
/// through this handle, and what the handle is finally named, and its file
/// id, follow the file.
pub(crate) fn set_abs_path(handle: isize, abs_path: String) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.abs_path = Some(abs_path);
        }
    }
}

pub(crate) fn set_position(handle: isize, pos: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.position = pos;
        }
    }
}

/// Update the cached size after a successful truncate so later reads on this
/// handle see the new EOF.
pub(crate) fn set_size(handle: isize, size: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.size = size;
            if e.position > size {
                e.position = size;
            }
        }
        file_changed(&mut g, handle);
    }
}

/// A write through `handle` landed bytes up to `end`: the cached size is
/// raised to at least `end`, and the time every handle on the file kept is
/// dropped, so their next time queries ask again ([`file_changed`]).
///
/// Not [`set_size`]: two writes on one handle can be in flight at once, each
/// having read the size before either finished, and whichever stores last
/// would win — the earlier-ending write shrinking the file under the other,
/// so that reads past it report end of file and the position is pulled back
/// for the next append to overwrite. The maximum, taken under the table
/// lock, does not depend on the order they finish in.
pub(crate) fn wrote(handle: isize, end: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.size = e.size.max(end);
        }
        file_changed(&mut g, handle);
    }
}

/// The `mtime` this handle keeps (`None` when it must ask), and its change
/// count to hand back to [`set_mtime`]. Marks the handle `timed`, so a write
/// to its file through another handle reaches it from now on. `None` for a
/// handle not in the table.
pub(crate) fn time_of(handle: isize) -> Option<(Option<i64>, u32)> {
    let mut g = TABLE.lock().ok()?;
    let e = g.get_mut(&(handle as usize))?;
    if !e.timed {
        e.timed = true;
        TIMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Some((e.mtime, e.changes))
}

/// Keep `mtime` for `handle`, unless a write or truncate through it came
/// after [`time_of`] gave `changes`: the answer may predate it.
pub(crate) fn set_mtime(handle: isize, mtime: i64, changes: u32) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            if e.changes == changes {
                e.mtime = Some(mtime);
            }
        }
    }
}

/// Record whether closing `handle` deletes its path: set when it was opened with
/// `FILE_DELETE_ON_CLOSE`, cleared once a set-info delete through it has already deleted the
/// path (a second `Opcode::Delete` at close could delete a file recreated under the same name). A
/// no-op for a handle that is not in the table.
pub(crate) fn set_delete_on_close(handle: isize, on: bool) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.delete_on_close = on;
        }
    }
}

/// Forget `handle`. Returns its director `fh` to close now (`None` for a handle the
/// director never issued, [`open_dir_unissued`], or one a demand-paged section still
/// holds, [`hold`]), and the path to delete now that it is closed
/// when it was opened with `FILE_DELETE_ON_CLOSE` ([`set_delete_on_close`]).
pub(crate) fn close_fuse(handle: isize) -> Option<(Option<u64>, Option<String>)> {
    let mut g = TABLE.lock().ok()?;
    g.remove(&(handle as usize)).map(|e| {
        if e.timed {
            TIMED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        // A section still reading through `fh` keeps it open at the
        // director; the CLOSE waits for the section's last release.
        let held = e.issued && HELD.lock().is_ok_and(|mut h| mark_closed(&mut h, e.fh));
        (
            (e.issued && !held).then_some(e.fh),
            e.abs_path.filter(|_| e.delete_on_close),
        )
    })
}

/// Note that the program closed its handle on `fh`; whether a section holds it.
fn mark_closed(held: &mut BTreeMap<u64, Held>, fh: u64) -> bool {
    match held.get_mut(&fh) {
        Some(h) => {
            h.file_closed = true;
            true
        }
        None => false,
    }
}

/// Keep the director handle behind `handle` open for a demand-paged section
/// over it, past the program's close of `handle`: its `fh`, or `None` for a
/// handle with none the director issued. Each hold is let go once, with
/// [`release`].
pub(crate) fn hold(handle: isize) -> Option<u64> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize)).filter(|e| e.issued)?;
    let mut held = HELD.lock().ok()?;
    held.entry(e.fh)
        .or_insert(Held {
            regions: 0,
            file_closed: false,
        })
        .regions += 1;
    Some(e.fh)
}

/// Let go of one [`hold`] on `fh`. `true` when it was the last and the
/// program's file handle is already closed: the caller sends the CLOSE. An
/// `fh` nothing holds is `false`.
pub(crate) fn release(fh: u64) -> bool {
    let Ok(_table) = TABLE.lock() else {
        return false;
    };
    let Ok(mut held) = HELD.lock() else {
        return false;
    };
    let Some(h) = held.get_mut(&fh) else {
        return false;
    };
    h.regions = h.regions.saturating_sub(1);
    if h.regions > 0 {
        return false;
    }
    let close = h.file_closed;
    held.remove(&fh);
    close
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two writes on one handle that both read the size before either
    /// finished: [0, 100) and [100, 200). Whichever reports last, the file
    /// is 200 bytes long and the position has not been pulled back.
    #[test]
    fn the_size_after_overlapping_writes_is_the_furthest_end_in_either_order() {
        for ends in [[100u64, 200], [200, 100]] {
            let h = open_fuse_at_ex(77, 0, false, None, false).unwrap();
            set_position(h, 200);
            for end in ends {
                wrote(h, end);
            }
            let v = lookup(h).unwrap();
            assert_eq!(
                (v.size, v.position),
                (200, 200),
                "ends reported as {ends:?}"
            );
            close_fuse(h);
        }
    }

    /// A held director handle is closed once, by whichever of the file's
    /// close and the last section's release comes last.
    #[test]
    fn a_held_fh_closes_once_with_the_last_of_file_and_sections() {
        // File first, then two sections.
        let h = open_fuse_at_ex(0x5EC7_0001, 10, false, None, false).unwrap();
        assert_eq!(hold(h), Some(0x5EC7_0001));
        assert_eq!(hold(h), Some(0x5EC7_0001));
        assert_eq!(close_fuse(h), Some((None, None)), "closed under a section");
        assert!(!release(0x5EC7_0001), "a section still holds it");
        assert!(release(0x5EC7_0001), "the last release closes it");
        assert!(
            !release(0x5EC7_0001),
            "a release past the last closes nothing"
        );

        // Section first, then the file.
        let h = open_fuse_at_ex(0x5EC7_0002, 10, false, None, false).unwrap();
        assert_eq!(hold(h), Some(0x5EC7_0002));
        assert!(!release(0x5EC7_0002), "the file is still open");
        assert_eq!(close_fuse(h), Some((Some(0x5EC7_0002), None)));

        // A handle the director never issued holds nothing.
        let d = open_dir_unissued().unwrap();
        assert_eq!(hold(d), None);
        close_fuse(d);
    }
}
