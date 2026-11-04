//! Shim-owned section address space, including lazy FUSE data sections that
//! emulate mmap without preloading multi‑GiB BSAs.
//!
//! CreateSection reserves address space (and optionally warms the first window).
//! MapView returns that base. Further first-touch faults commit 256 KiB chunks
//! and stream them from the director via the shared bulk arena.
//!
//! **Lifetime.** The reservation belongs to the *section*, never to a view —
//! that is NT's model and the game relies on it: BSA readers slide views over a
//! big archive, so `NtUnmapViewOfSection` on one window must leave every other
//! window (and any later remap of the still-open section) valid. The VA is
//! released only once the section handle is closed *and* the last view is gone.
//!
//! **Filling.** No thread may see a chunk before its bytes are in place. A
//! demand-paged region is a view of a pagefile section of its own
//! (`SEC_RESERVE`, so only what is filled is committed). A fault commits its
//! chunk `PAGE_NOACCESS` in that view, the fill writes the director's bytes
//! through a second, writable view of just that chunk, and only then is the
//! chunk made readable. Another thread that touches the chunk meanwhile
//! faults, waits for the fill in the handler (`FILL_LOCK`), and then reads
//! the bytes. Where a second view cannot fill ([`second_view_fills`]: a host
//! whose second view does not see the first view's commit), or the region's
//! section cannot be had (the commit or pool limit, 32-bit address space),
//! the region is private memory committed readable before its fill, as
//! before, and there another thread can read the chunk's zeros while it
//! fills. Each region's backing is counted (`hookstats::lazy_region_count`).
//!
//! **Cost.** A private `MEM_RESERVE` charges nothing until a chunk is
//! committed. A mapped `SEC_RESERVE` section does: measured on Windows 11
//! (`GetPerformanceInfo` around a mapping), about 4 MiB of commit and 2 MiB
//! of paged pool per GiB of file, before any chunk is filled (a 64 GiB
//! region: 257 MiB and 128 MiB), most likely page tables and prototype PTEs.
//! Created but not mapped it costs about nothing. So a load order with tens
//! of GiB of archives above the eager limit pays a few hundred MiB of commit
//! and about 100 MiB of paged pool for them. A region whose section cannot
//! be had at that price takes the private fallback, not a failed section.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualProtect,
    FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE,
    PAGE_NOACCESS, PAGE_READWRITE, SEC_RESERVE,
};

const PAGE: usize = 4096;
/// Commit/fill this many bytes per fault.
const CHUNK: usize = 256 * 1024;
/// Warm this many bytes at CreateSection so header/index peeks don't depend on VEH.
const WARM_BYTES: usize = 2 * 1024 * 1024;
/// Largest file we back with a demand-paged section. Only address space is
/// reserved up front, so this is bounded by 64-bit VA, not by RAM — it exists
/// to keep a corrupt size from reserving something absurd. Heavily modded load
/// orders ship BSAs well past the old 3 GiB ceiling, and returning
/// `STATUS_SECTION_TOO_BIG` for those makes the game fail the archive outright.
pub(crate) const MAX_LAZY: u64 = 64 * 1024 * 1024 * 1024;

/// Largest data section filled whole at `NtCreateSection`; a larger one is
/// filled on demand (this module).
const EAGER_MAX: u64 = 256 * 1024 * 1024;

/// A test's [`EAGER_MAX`], 0 for none ([`set_eager_max`]).
#[cfg(feature = "test-support")]
static EAGER_MAX_FOR_TESTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The size above which a data section over a director file is demand-paged.
pub(crate) fn eager_max() -> u64 {
    #[cfg(feature = "test-support")]
    match EAGER_MAX_FOR_TESTS.load(Ordering::Relaxed) {
        0 => {}
        n => return n,
    }
    EAGER_MAX
}

/// Demand-page every data section larger than `bytes` (0: back to
/// [`EAGER_MAX`]), so a test reaches the lazy path without a 256 MiB file.
#[cfg(feature = "test-support")]
pub(crate) fn set_eager_max(bytes: u64) {
    EAGER_MAX_FOR_TESTS.store(bytes, Ordering::Relaxed);
}

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
use crate::ntdef::{STATUS_ACCESS_VIOLATION, STATUS_IN_PAGE_ERROR, STATUS_UNSUCCESSFUL};

/// How many times a failed fill is asked again within one fault before the
/// fault becomes `STATUS_IN_PAGE_ERROR`, and a failed image read before its
/// section fails.
pub(crate) const FILL_RETRIES: u32 = 3;

/// A shim-owned VA range backing one synthetic section.
struct OwnedRegion {
    base: usize,
    reserved: usize,
    file_size: u64,
    /// The director handle a demand-paged region reads through, held for it
    /// ([`crate::synth_file::hold`]) until the region is dropped.
    fh: u64,
    /// The pagefile section behind a demand-paged region ([`reserve_shared`]),
    /// as an address; 0 when the region is private memory (eager, or
    /// demand-paged where a second view cannot fill, [`second_view_fills`]).
    section: usize,
    /// Demand-paged: reserved `PAGE_NOACCESS`, chunks committed by the VEH.
    /// Eager regions are fully committed and filled at creation.
    lazy: bool,
    /// The section handle is still open.
    section_open: bool,
    committed: HashSet<usize>,
}

impl OwnedRegion {
    /// NT keeps a section's pages alive while any view is mapped, even after
    /// the handle is closed. `synth_section` owns the view refcounts, so ask it
    /// rather than keeping a second tally that can drift out of step.
    fn is_dead(&self) -> bool {
        !self.section_open && !crate::synth_section::has_view_in(self.base, self.reserved)
    }
}

static REGIONS: Mutex<BTreeMap<usize, OwnedRegion>> = Mutex::new(BTreeMap::new());
static VEH_INSTALLED: AtomicBool = AtomicBool::new(false);
static FILL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static IN_VEH: Cell<bool> = const { Cell::new(false) };
    /// Set on the fill worker: a fault there must never re-enter the fill path
    /// (it would wait on itself), so the VEH declines it.
    static IS_FILL_WORKER: Cell<bool> = const { Cell::new(false) };
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn ensure_veh() {
    if VEH_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let h = unsafe { AddVectoredExceptionHandler(1, Some(veh_handler)) };
    if h.is_null() {
        VEH_INSTALLED.store(false, Ordering::SeqCst);
    }
}

// ── fill worker ────────────────────────────────────────────────────────────
//
// Faults are serviced by one long-lived thread rather than a thread spawned per
// chunk. Creating a thread inside a page-fault handler runs the loader's
// DLL_THREAD_ATTACH callbacks, so it deadlocks outright if the faulting thread
// happens to hold the loader lock — and at 256 KiB per chunk a multi-GiB BSA
// would otherwise burn tens of thousands of thread creations.

/// Where a fill writes.
#[derive(Debug, Clone, Copy)]
enum Target {
    /// Committed, writable memory at this address.
    At(usize),
    /// A second view of a region's section (its handle, as an address),
    /// mapped for the fill at `offset` and unmapped after it.
    View { section: usize, offset: u64 },
}

struct FillJob {
    fh: u64,
    file_off: u64,
    target: Target,
    /// Bytes the director has there.
    len: usize,
    /// Bytes the target spans: zeros past `len`.
    span: usize,
    reply: Sender<Option<usize>>,
}

// SAFETY: the target is committed memory owned by the requesting thread, or a
// section its region keeps open, for the duration of the job; the requester
// blocks on `reply` until the worker is done with it.
unsafe impl Send for FillJob {}

static FILL_WORKER: OnceLock<Option<Mutex<Sender<FillJob>>>> = OnceLock::new();

/// Start the fill worker. Call from a normal call context (section creation),
/// never from the VEH — see the note above.
fn ensure_worker() {
    FILL_WORKER.get_or_init(|| {
        let (tx, rx) = channel::<FillJob>();
        std::thread::Builder::new()
            .name("vfs-lazy-fill".into())
            // Deep FUSE I/O must never run on the game's primary stack.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                IS_FILL_WORKER.with(|c| c.set(true));
                while let Ok(job) = rx.recv() {
                    // SAFETY: the requester owns the target and blocks until reply.
                    let n =
                        unsafe { fill_into(job.fh, job.file_off, job.target, job.len, job.span) };
                    let _ = job.reply.send(n);
                }
            })
            .ok()
            .map(|_| Mutex::new(tx))
    });
}

/// Stream `[file_off, file_off+len)` of `fh` into `target`, then zeros up to
/// `span` when all `len` came, returning the bytes read.
///
/// Uses the worker when available; falls back to an inline read (aether-inject
/// expands the primary stack for exactly this) if the worker never started or
/// we are already *on* it.
fn fill_bytes(fh: u64, file_off: u64, target: Target, len: usize, span: usize) -> Option<usize> {
    let on_worker = IS_FILL_WORKER.with(|c| c.get());
    if !on_worker {
        if let Some(Some(tx)) = FILL_WORKER.get() {
            let (reply, wait) = channel();
            let queued = tx.lock().ok().and_then(|tx| {
                tx.send(FillJob {
                    fh,
                    file_off,
                    target,
                    len,
                    span,
                    reply,
                })
                .ok()
            });
            if queued.is_some() {
                return wait.recv().ok().flatten();
            }
        }
    }
    // SAFETY: the caller owns the target until this returns.
    unsafe { fill_into(fh, file_off, target, len, span) }
}

/// The fill itself, on whichever thread runs it.
///
/// # Safety
/// `target` is `At` committed writable memory of `span` bytes, or `View` an
/// open section at least `offset + span` bytes long whose pages there are
/// committed; the caller keeps either until this returns.
unsafe fn fill_into(
    fh: u64,
    file_off: u64,
    target: Target,
    len: usize,
    span: usize,
) -> Option<usize> {
    let client = crate::director::global()?;
    let dest = match target {
        Target::At(a) => a,
        // SAFETY: an open section, mapped for this fill only.
        Target::View { section, offset } => unsafe { second_view(section, offset, span) }?,
    };
    // SAFETY: `span` writable bytes at `dest`, per the contract or the view.
    let out = unsafe { core::slice::from_raw_parts_mut(dest as *mut u8, span) };
    let n = client.read_fragmented(fh, file_off, &mut out[..len]).ok();
    if n == Some(len) {
        out[len..].fill(0);
    }
    if let Target::View { .. } = target {
        // SAFETY: the view mapped above, unmapped once.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: dest as _ }) };
    }
    n
}

/// A writable view of `span` bytes of `section` at `offset`, only where
/// those pages are committed and writable in it (`VirtualQuery`); `None`,
/// with nothing mapped, otherwise. So a host whose second view does not see
/// the first view's commit refuses the fill rather than taking an access
/// violation the handler would not serve (the fill worker's, or one inside
/// the program's `NtCreateSection`).
///
/// # Safety
/// `section` is an open section at least `offset + span` bytes long.
unsafe fn second_view(section: usize, offset: u64, span: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
    // SAFETY: per the contract; the view is unmapped here unless returned.
    unsafe {
        let v = MapViewOfFile(
            section as _,
            FILE_MAP_WRITE,
            (offset >> 32) as u32,
            offset as u32,
            span,
        );
        if v.Value.is_null() {
            return None;
        }
        let mut m: MEMORY_BASIC_INFORMATION = core::mem::zeroed();
        let n = VirtualQuery(
            v.Value,
            &mut m,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if n == 0 || m.State != MEM_COMMIT || m.Protect != PAGE_READWRITE || m.RegionSize < span {
            UnmapViewOfFile(v);
            return None;
        }
        Some(v.Value as usize)
    }
}

/// The probe's answer, once it is definite: 0 not yet, 1 works, 2 cannot.
static SECOND_VIEW: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Whether a chunk can be filled through a second view while the region's
/// own view of it stays inaccessible: a commit `PAGE_NOACCESS` inside a view
/// of a `SEC_RESERVE` section, a write through another view (only once
/// [`second_view`] has seen it committed), a protection change, and the byte
/// read back through the first.
///
/// `Some` is definite and kept for the process: the host does it, or the
/// sequence ran and misbehaved. `None` (a section, commit or view that could
/// not be had, at the commit or pool limit say) is asked again at the next
/// section, which meanwhile takes private memory.
fn second_view_fills() -> Option<bool> {
    match SECOND_VIEW.load(Ordering::Acquire) {
        1 => return Some(true),
        2 => return Some(false),
        _ => {}
    }
    // SAFETY: a section and views this function creates and releases.
    let answer = unsafe {
        let (base, section) = reserve_shared(CHUNK)?;
        let answer = 'probe: {
            if VirtualAlloc(base as _, PAGE, MEM_COMMIT, PAGE_NOACCESS).is_null() {
                break 'probe None;
            }
            let Some(v) = second_view(section, 0, PAGE) else {
                // Committed in the first view, not in the second.
                break 'probe Some(false);
            };
            core::ptr::write_volatile(v as *mut u8, 0x5A);
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v as _ });
            let mut old = 0;
            Some(
                VirtualProtect(base as _, PAGE, PAGE_READWRITE, &mut old) != 0
                    && old == PAGE_NOACCESS
                    && core::ptr::read_volatile(base as *const u8) == 0x5A,
            )
        };
        release_va(base, section);
        answer
    }?;
    SECOND_VIEW.store(if answer { 1 } else { 2 }, Ordering::Release);
    Some(answer)
}

/// A test's "no pagefile section can be had" ([`fail_shared_reserve`]).
#[cfg(feature = "test-support")]
static FAIL_SHARED: AtomicBool = AtomicBool::new(false);

/// While `on`, [`reserve_shared`] fails, as at the commit or pool limit.
#[cfg(feature = "test-support")]
pub(crate) fn fail_shared_reserve(on: bool) {
    FAIL_SHARED.store(on, Ordering::Relaxed);
}

/// A pagefile section of `len` bytes with nothing committed (`SEC_RESERVE`),
/// and one writable view of all of it: the view's address and the section's
/// handle.
unsafe fn reserve_shared(len: usize) -> Option<(usize, usize)> {
    #[cfg(feature = "test-support")]
    if FAIL_SHARED.load(Ordering::Relaxed) {
        return None;
    }
    let len64 = len as u64;
    // SAFETY: an unnamed pagefile section; the view is of all of it.
    unsafe {
        let section = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            core::ptr::null(),
            PAGE_READWRITE | SEC_RESERVE,
            (len64 >> 32) as u32,
            len64 as u32,
            core::ptr::null(),
        );
        if section.is_null() {
            return None;
        }
        let v = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len);
        if v.Value.is_null() {
            CloseHandle(section);
            return None;
        }
        Some((v.Value as usize, section as usize))
    }
}

/// Give back a region's address space: its view and section, or its private
/// reservation.
///
/// # Safety
/// `base` and `section` are a region's, which nothing uses any more.
unsafe fn release_va(base: usize, section: usize) {
    // SAFETY: per the contract.
    unsafe {
        if section == 0 {
            VirtualFree(base as *mut c_void, 0, MEM_RELEASE);
        } else {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: base as _ });
            CloseHandle(section as _);
        }
    }
}

// ── section creation ───────────────────────────────────────────────────────

/// Reserve VA for a director-backed data section; warm first [`WARM_BYTES`].
///
/// Returns a synthetic section handle for [`crate::synth_section::map_view`].
///
/// The caller holds `fh` for the region ([`crate::synth_file::hold`]); the
/// region lets it go when it is dropped (section closed, last view gone). On
/// `None` there is no region, and the caller lets it go.
pub(crate) unsafe fn create_lazy_data_section(fh: u64, file_size: u64) -> Option<isize> {
    if file_size == 0 || file_size > MAX_LAZY {
        return None;
    }
    let reserved = align_up(file_size as usize, PAGE);
    if reserved == 0 {
        return None;
    }
    // Install VEH only if not disabled (VFS_LAZY_NO_VEH=1 for debug).
    if !aether_env::present(aether_env::LAZY_NO_VEH) {
        ensure_veh();
    }
    ensure_worker();
    // A section of its own where the host fills through a second view and
    // one can be had; otherwise private memory, which maps the file all the
    // same but can show a chunk's zeros mid-fill. Decided per section and
    // counted (`hookstats::lazy_region_count`).
    use crate::hookstats::{note_lazy_region, LazyBacking};
    let shared = match second_view_fills() {
        Some(true) => match reserve_shared(reserved) {
            Some(s) => Ok(s),
            None => Err(LazyBacking::PrivateReserveFailed),
        },
        Some(false) => Err(LazyBacking::PrivateNoSecondView),
        None => Err(LazyBacking::PrivateReserveFailed),
    };
    let (base_u, section) = match shared {
        Ok(s) => {
            note_lazy_region(LazyBacking::Shared);
            s
        }
        Err(why) => {
            let base = VirtualAlloc(core::ptr::null(), reserved, MEM_RESERVE, PAGE_NOACCESS);
            if base.is_null() {
                return None;
            }
            note_lazy_region(why);
            (base as usize, 0)
        }
    };
    if !track(base_u, reserved, file_size, fh, true, section) {
        release_va(base_u, section);
        return None;
    }
    // Warm header/index window so first peeks work even if VEH is slow/racy.
    let warm = (file_size as usize).min(WARM_BYTES);
    if warm > 0 {
        let _ = ensure_range(base_u, 0, warm);
    }
    match crate::synth_section::register_mapped_image(base_u, file_size) {
        Some(h) => Some(h),
        None => {
            forget(base_u);
            release_va(base_u, section);
            None
        }
    }
}

/// Record an eagerly-filled section allocation so closing the section frees it.
///
/// Without this the shim leaks every eager mapping (up to 256 MiB each) for the
/// life of the process.
pub(crate) fn track_eager_section(base: usize, size: u64) -> bool {
    track(base, align_up(size as usize, PAGE), size, 0, false, 0)
}

fn track(
    base: usize,
    reserved: usize,
    file_size: u64,
    fh: u64,
    lazy: bool,
    section: usize,
) -> bool {
    match REGIONS.lock() {
        Ok(mut g) => {
            g.insert(
                base,
                OwnedRegion {
                    base,
                    reserved,
                    file_size,
                    fh,
                    section,
                    lazy,
                    section_open: true,
                    committed: HashSet::new(),
                },
            );
            true
        }
        Err(_) => false,
    }
}

fn forget(base: usize) {
    if let Ok(mut g) = REGIONS.lock() {
        g.remove(&base);
    }
}

/// Key of the owned region containing `addr`, if any.
fn region_key(g: &BTreeMap<usize, OwnedRegion>, addr: usize) -> Option<usize> {
    g.range(..=addr)
        .next_back()
        .filter(|(_, r)| addr < r.base + r.reserved)
        .map(|(k, _)| *k)
}

/// Drop the region at `key` if nothing references it any more. Returns the
/// director handle a dropped demand-paged region held, for [`let_go`] once
/// `REGIONS` is unlocked.
fn reap(g: &mut BTreeMap<usize, OwnedRegion>, key: usize) -> Option<u64> {
    if !g.get(&key).is_some_and(|r| r.is_dead()) {
        return None;
    }
    let r = g.remove(&key)?;
    // SAFETY: `base` came from this module's reservation and no view or
    // open section handle refers to it any more.
    unsafe { release_va(r.base, r.section) };
    r.lazy.then_some(r.fh)
}

/// Let go of a dropped region's hold on its director handle
/// ([`crate::synth_file::hold`]), closing it at the director when the
/// program's file handle is already gone (docs/wire.md 3.4.10). Not under
/// `REGIONS`: the CLOSE is a round trip, and a fault on another thread must
/// not wait for it.
pub(crate) fn let_go(fh: Option<u64>) {
    let Some(fh) = fh else {
        return;
    };
    if crate::synth_file::release(fh) {
        if let Some(c) = crate::director::global() {
            let _ = c.close(fh);
        }
    }
}

// ── view / section lifetime ────────────────────────────────────────────────

/// Note that a mapped view over `addr` went away. Frees the region only if its
/// section is closed and no other view remains.
///
/// Call *after* retiring the view in [`crate::synth_section::unmap_view`].
pub(crate) fn on_view_unmapped(addr: usize) {
    let dropped = match REGIONS.lock() {
        Ok(mut g) => region_key(&g, addr).and_then(|key| reap(&mut g, key)),
        Err(_) => None,
    };
    let_go(dropped);
}

/// Note that the section handle covering `base` was closed. Frees the region
/// once the last view is unmapped.
pub(crate) fn on_section_closed(base: usize) {
    let dropped = match REGIONS.lock() {
        Ok(mut g) => region_key(&g, base).and_then(|key| {
            if let Some(r) = g.get_mut(&key) {
                r.section_open = false;
            }
            reap(&mut g, key)
        }),
        Err(_) => None,
    };
    let_go(dropped);
}

/// Whether `addr` falls in a demand-paged region (so a fault there is ours).
pub(crate) fn is_lazy_base(addr: usize) -> bool {
    REGIONS
        .lock()
        .map(|g| {
            g.range(..=addr)
                .next_back()
                .is_some_and(|(_, r)| r.lazy && addr < r.base + r.reserved)
        })
        .unwrap_or(false)
}

// ── demand paging ──────────────────────────────────────────────────────────

/// Ensure `[offset, offset+len)` within the lazy region is committed and filled.
pub(crate) unsafe fn ensure_range(base: usize, offset: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = offset.saturating_add(len);
    let mut off = offset / CHUNK * CHUNK;
    while off < end {
        if fill_chunk_at(base + off) != Fill::Done {
            return false;
        }
        off += CHUNK;
    }
    true
}

/// Raises [`IN_VEH`] for as long as it is alive, and lowers it on `Drop`.
///
/// **Raising the flag without holding one of these is deliberately not
/// expressible** — the same shape `ShimIoGuard` has in [`crate::hook`], for the
/// same reason. The pair of raw `set(true)`/`set(false)` calls this replaces was
/// the exact defect task 1 removed from every ntdll hook: a panic (or any early
/// return added later) between them leaves `IN_VEH` latched at `true`, and
/// `veh_handler`'s first check then declines **every** later fault on that
/// thread. Demand paging silently stops for the thread's whole life, the game
/// takes the access violation itself, and nothing in the shim reports anything.
struct VehGuard;

impl VehGuard {
    /// `None` if this thread is already inside the handler — the re-entrancy
    /// check and the flag-raise are one operation, so they cannot disagree.
    fn enter() -> Option<VehGuard> {
        if IN_VEH.with(|c| c.get()) {
            return None;
        }
        IN_VEH.with(|c| c.set(true));
        Some(VehGuard)
    }
}

impl Drop for VehGuard {
    fn drop(&mut self) {
        IN_VEH.with(|c| c.set(false));
    }
}

/// The vectored exception handler: an `extern "system"` entry point Windows
/// calls, and therefore one that must contain its own panic.
///
/// It is not an ntdll detour, so `hook_entry_points!` does not generate it — and
/// that is exactly why it was missed. Task 1's structural guard read
/// `include_str!` of the one hook source file and nothing else, so this file was outside the check
/// for the whole of stage 4 while holding an uncontained entry point. The guard
/// now derives its file set from `src/`; see
/// `no_extern_hook_bypasses_the_panic_containment_macro` in [`crate::hook`].
///
/// `EXCEPTION_CONTINUE_SEARCH` is the right answer on a panic for the same
/// reason `STATUS_UNSUCCESSFUL` is right for a hook: it says "not mine" and lets
/// the next handler — ultimately the game's own — see the fault, rather than
/// claiming a page was filled that was not. `EXCEPTION_CONTINUE_EXECUTION` after
/// a panic would resume the faulting instruction on a page that is still
/// `PAGE_NOACCESS`, i.e. an infinite fault loop.
unsafe extern "system" fn veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    crate::hook::contain_panic(
        "VehHandler",
        || unsafe { veh_handler_body(info) },
        || EXCEPTION_CONTINUE_SEARCH,
    )
}

unsafe fn veh_handler_body(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Re-entrancy and worker faults are not ours to service, and this rejection
    // has to happen **before** `is_lazy_base` — that call takes `REGIONS`, which
    // a re-entrant fault may already be holding one level down, and
    // `std::sync::Mutex` is not reentrant. `VehGuard::enter` re-checks the same
    // flag at the raise; this read is the cheap early reject, not the guard.
    if IN_VEH.with(|c| c.get()) || IS_FILL_WORKER.with(|c| c.get()) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rec = (*info).ExceptionRecord;
    if rec.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if (*rec).ExceptionCode != STATUS_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if (*rec).NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = (*rec).ExceptionInformation[1];
    // Quick reject before taking locks.
    if !is_lazy_base(fault_addr) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let Some(_veh) = VehGuard::enter() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let mut fails = 0;
    let deadline = crate::director::global()
        .map(|c| c.deadline())
        .unwrap_or(aether_ipc::RESPONSE_DEADLINE);
    let started = std::time::Instant::now();
    loop {
        match fill_chunk_at(fault_addr) {
            Fill::Done => return EXCEPTION_CONTINUE_EXECUTION,
            Fill::NotOurs => return EXCEPTION_CONTINUE_SEARCH,
            Fill::Failed => {
                fails += 1;
                // The first try and up to FILL_RETRIES more, none started
                // after the client's deadline has passed since the first.
                if fails > FILL_RETRIES || started.elapsed() >= deadline {
                    break;
                }
            }
        }
    }
    // The page holds nothing the file holds, and it is still `PAGE_NOACCESS`:
    // the fault becomes the one NT raises when a mapped file's paging read
    // fails, for the program's handlers (or the end of the process) to see.
    // Parameters as NT gives them: the access type (kept), the address, and
    // the status the read failed with.
    crate::hookstats::note_in_page_error(fault_addr);
    (*rec).ExceptionCode = STATUS_IN_PAGE_ERROR;
    (*rec).NumberParameters = 3;
    (*rec).ExceptionInformation[1] = fault_addr;
    (*rec).ExceptionInformation[2] = STATUS_UNSUCCESSFUL as u32 as usize;
    EXCEPTION_CONTINUE_SEARCH
}

/// What [`fill_chunk_at`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// The chunk holds the file's bytes (now, or already).
    Done,
    /// The address is in no demand-paged region, or the chunk could not be
    /// committed: not a fault this module can serve.
    NotOurs,
    /// The READ failed, or came back short of what the file holds there.
    /// The chunk is inaccessible (`PAGE_NOACCESS`, or reserved again), and
    /// the next touch faults and asks again.
    Failed,
}

/// Commit + stream the CHUNK covering `addr` (addr may be anywhere in the chunk).
///
/// A fill is committed only whole: `min(CHUNK, size - offset)` bytes from
/// the director, then zeros past the end of the file, as NT gives the tail of
/// a mapped file's last page. A short answer before the file's size is not
/// the end of the file here (OPEN gave the size; docs/wire.md 3.4.4's
/// `n < len` end of file is only at that size), so it is [`Fill::Failed`].
unsafe fn fill_chunk_at(addr: usize) -> Fill {
    let _guard = match FILL_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return Fill::NotOurs,
    };

    let (base, file_size, fh, section, chunk_idx, commit_off, commit_len) = {
        let g = match REGIONS.lock() {
            Ok(g) => g,
            Err(_) => return Fill::NotOurs,
        };
        let Some((_, region)) = g
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| r.lazy && addr < r.base + r.reserved)
        else {
            return Fill::NotOurs;
        };
        let off = addr - region.base;
        let chunk_idx = off / CHUNK;
        if region.committed.contains(&chunk_idx) {
            return Fill::Done;
        }
        let commit_off = chunk_idx * CHUNK;
        let commit_len = CHUNK.min(region.reserved - commit_off);
        (
            region.base,
            region.file_size,
            region.fh,
            region.section,
            chunk_idx,
            commit_off,
            commit_len,
        )
    };

    let page = (base + commit_off) as *mut c_void;
    // Align commit length to pages (required).
    let commit_len = align_up(commit_len, PAGE);
    // Over a section the chunk stays inaccessible in the program's view
    // until its bytes are in place (the module's "Filling").
    let protect = if section != 0 {
        PAGE_NOACCESS
    } else {
        PAGE_READWRITE
    };
    let committed = VirtualAlloc(page, commit_len, MEM_COMMIT, protect);
    if committed.is_null() {
        // Might already be committed by a racing fill that dropped lock early.
        if REGIONS
            .lock()
            .ok()
            .and_then(|g| g.get(&base).map(|r| r.committed.contains(&chunk_idx)))
            .unwrap_or(false)
        {
            return Fill::Done;
        }
        return Fill::NotOurs;
    }

    let file_off = commit_off as u64;
    let readable = if file_off >= file_size {
        0usize
    } else {
        ((file_size - file_off) as usize).min(commit_len)
    };

    if readable > 0 {
        // Bracket the fill: a started-without-completed pair is the signature of
        // a wedged faulting thread, which is invisible everywhere else.
        crate::hookstats::note_fill_start();
        let t0 = std::time::Instant::now();
        let target = if section != 0 {
            Target::View {
                section,
                offset: file_off,
            }
        } else {
            Target::At(page as usize)
        };
        let n_read = fill_bytes(fh, file_off, target, readable, commit_len);
        let whole = n_read == Some(readable);
        crate::hookstats::note_fill_end(n_read.unwrap_or(0), t0.elapsed().as_nanos() as u64, whole);
        if !whole {
            // Inaccessible, so no thread reads the part-filled chunk, and the
            // next touch faults to fill it again. Private memory goes back to
            // reserved; a view's pages cannot be decommitted, and stay
            // committed `PAGE_NOACCESS`.
            if section == 0 {
                VirtualFree(page, commit_len, MEM_DECOMMIT);
            }
            return Fill::Failed;
        }
    }
    // Readable only now, every byte in place (zeros past the file's end).
    let mut old = 0;
    if section != 0 && VirtualProtect(page, commit_len, PAGE_READWRITE, &mut old) == 0 {
        return Fill::Failed;
    }

    if let Ok(mut g) = REGIONS.lock() {
        if let Some(region) = g.get_mut(&base) {
            region.committed.insert(chunk_idx);
        }
    }
    Fill::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_FREE};

    /// Serialises the tests that reserve and release virtual address space.
    ///
    /// Each asserts that a specific base became `MEM_FREE`, and a freed base is
    /// immediately available to anyone — including a sibling test reserving on
    /// another thread. When that happens the region reads back as `MEM_RESERVE`
    /// and the test blames the release it was checking. Nothing about the
    /// product is racy here; only the observation is.
    ///
    /// Convention: cargo runs the `#[test]`s in one binary across many threads.
    /// Any test in this crate that *asserts on process-global state* — the VA
    /// reservation map here, the hook tables, the fuse-synth handle table, the
    /// stats counters — must take a lock like this one rather than assume test
    /// order, or it must be an integration test (under `tests/`, whose
    /// `#[test]`s each re-execute in their own process, see `tests/common/mod.rs`) so the
    /// OS process boundary provides the isolation. Take the guard as the first line of the test:
    /// `let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());`
    /// (`unwrap_or_else(into_inner)` so one panicking test does not poison the
    /// lock and cascade into spurious failures in its siblings).
    static VA_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Region state for `addr`: `MEM_FREE` once the VA has been released.
    fn va_state(addr: usize) -> u32 {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let n = unsafe {
            VirtualQuery(
                addr as *const c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert!(n != 0, "VirtualQuery failed for {addr:#x}");
        mbi.State
    }

    /// 8 MiB standin for a BSA — big enough to span many CHUNKs, small enough
    /// that a reserve in the test process is free.
    const SIZE: u64 = 8 * 1024 * 1024;

    /// Mirrors `map_view_hook` / `unmap_view_hook`. Keep in step with `hook/section.rs`.
    fn hook_map(h: isize, off: u64, want: u64) -> usize {
        let (base, _) = crate::synth_section::map_view(h, off, want).expect("map_view");
        base
    }

    fn hook_unmap(base: usize) {
        crate::synth_section::unmap_view(base);
        on_view_unmapped(base);
    }

    fn hook_close(h: isize) {
        if let Some(window) = crate::synth_section::close_section(h) {
            on_section_closed(window);
        }
    }

    #[test]
    fn unmapping_a_view_leaves_the_open_section_usable() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        hook_unmap(base);

        // The section handle is still open, so its address space must survive.
        assert_ne!(
            va_state(base),
            MEM_FREE,
            "unmapping one view released the whole section's VA"
        );
        let again = hook_map(h, 0, 0);
        assert!(
            is_lazy_base(again),
            "remapped view is no longer demand-pageable — next touch faults to a crash"
        );

        hook_unmap(again);
        hook_close(h);
    }

    #[test]
    fn unmapping_an_offset_view_keeps_sibling_views_alive() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let head = hook_map(h, 0, 64 * 1024);
        let off = 4 * 1024 * 1024u64;
        let tail = hook_map(h, off, 64 * 1024);
        assert_eq!(tail, head + off as usize);

        // Drop only the tail window; the head window is still in use.
        hook_unmap(tail);

        assert_ne!(
            va_state(head),
            MEM_FREE,
            "unmapping an offset view freed the base view still in use"
        );
        assert!(is_lazy_base(head), "head view lost its demand-paging");

        hook_unmap(head);
        hook_close(h);
    }

    #[test]
    fn closing_the_section_releases_the_reservation() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        hook_unmap(base);
        hook_close(h);

        assert_eq!(
            va_state(base),
            MEM_FREE,
            "closing the section leaked its reservation"
        );
    }

    #[test]
    fn a_closed_section_with_a_live_view_keeps_its_pages() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        // NT keeps section pages alive while a view is mapped.
        hook_close(h);
        assert_ne!(
            va_state(base),
            MEM_FREE,
            "closing the handle freed pages still mapped into the process"
        );

        hook_unmap(base);
        assert_eq!(
            va_state(base),
            MEM_FREE,
            "last unmap should reap the region"
        );
    }

    /// A second view is written only where its pages are committed and
    /// writable: over an uncommitted range it is refused (not written, so a
    /// host whose views do not share commit cannot crash the process), and
    /// once the first view commits the range it is given.
    #[test]
    fn a_second_view_of_an_uncommitted_range_is_refused() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (base, section) = unsafe { reserve_shared(CHUNK) }.expect("a section");
        assert!(
            unsafe { second_view(section, 0, PAGE) }.is_none(),
            "a view of uncommitted pages was handed out"
        );
        assert!(!unsafe { VirtualAlloc(base as _, PAGE, MEM_COMMIT, PAGE_NOACCESS) }.is_null());
        let v = unsafe { second_view(section, 0, PAGE) }.expect("a view of committed pages");
        unsafe {
            core::ptr::write_volatile(v as *mut u8, 7);
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: v as _ });
            release_va(base, section);
        }
    }

    #[test]
    fn two_views_at_one_base_are_refcounted() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let a = hook_map(h, 0, 0);
        let b = hook_map(h, 0, 0);
        assert_eq!(a, b);

        hook_unmap(a);
        assert!(
            crate::synth_section::is_synth_view(b),
            "first unmap forgot a base the process still has mapped"
        );
        assert_ne!(va_state(b), MEM_FREE);

        hook_unmap(b);
        hook_close(h);
        assert_eq!(va_state(a), MEM_FREE);
    }

    #[test]
    fn eager_sections_are_freed_on_close() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let size = 1024 * 1024u64;
        let base = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                size as usize,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert!(base != 0);
        assert!(track_eager_section(base, size));
        let h = crate::synth_section::register_mapped_image(base, size).expect("register");

        let mapped = hook_map(h, 0, 0);
        assert!(
            !is_lazy_base(mapped),
            "eager region must not be demand-paged"
        );
        hook_unmap(mapped);
        hook_close(h);

        assert_eq!(
            va_state(base),
            MEM_FREE,
            "eager section allocation leaked on close"
        );
    }

    /// The fill through a second view (the module's "Filling") is what
    /// this host does: a demand-paged region is a section view, and a chunk
    /// outside the warm window is inaccessible until filled.
    #[test]
    fn a_demand_paged_region_is_a_section_view_here() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            second_view_fills(),
            Some(true),
            "this host cannot fill through a second view"
        );
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);
        let section = REGIONS.lock().unwrap().get(&base).map(|r| r.section);
        assert!(matches!(section, Some(s) if s != 0), "{section:?}");
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let at = base + 6 * 1024 * 1024;
        unsafe {
            VirtualQuery(
                at as *const c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert_ne!(mbi.Protect, PAGE_READWRITE, "an unfilled chunk is readable");
        hook_unmap(base);
        hook_close(h);
        assert_eq!(va_state(base), MEM_FREE);
    }

    #[test]
    fn oversized_files_are_still_mappable() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Well past the old 3 GiB ceiling; reservation only, so this is cheap.
        // A const assertion — the value is known at compile time, so a runtime
        // `assert!` would never have been able to fail.
        const _: () = assert!(MAX_LAZY > 8 * 1024 * 1024 * 1024);
    }

    /// A panic taken while the VEH's re-entrancy flag is raised must lower it.
    ///
    /// This is the latch the raw `IN_VEH.set(true)` / `set(false)` pair could
    /// not survive, and the symptom is the worst shape this project keeps
    /// finding: `IN_VEH` stuck at `true` makes `veh_handler` decline **every**
    /// later fault on that thread, so demand paging silently stops for the
    /// thread's whole life and the game takes the access violation itself. No
    /// counter moves and nothing is logged.
    ///
    /// `IN_VEH` is a thread-local, so this needs no lock: it runs on the test's
    /// own thread and cannot be observed by a sibling.
    #[test]
    fn a_panic_inside_the_veh_does_not_latch_the_reentrancy_flag() {
        assert!(
            !IN_VEH.with(|c| c.get()),
            "test thread started inside the VEH"
        );

        let outcome = std::panic::catch_unwind(|| {
            let _veh = VehGuard::enter().expect("the flag was already raised");
            assert!(IN_VEH.with(|c| c.get()), "entering did not raise the flag");
            panic!("a fill-path panic");
        });
        assert!(outcome.is_err(), "the panic did not happen");

        assert!(
            !IN_VEH.with(|c| c.get()),
            "IN_VEH stayed raised after a panic — demand paging is now silently off for \
             this thread for the rest of its life"
        );
        // And the flag is genuinely re-acquirable, which is the property the
        // handler needs on its next fault.
        assert!(
            VehGuard::enter().is_some(),
            "no further fault on this thread could be serviced"
        );
    }

    /// Raising the flag is only expressible by acquiring the guard, so a
    /// re-entrant acquisition is refused rather than nesting — and the refusal
    /// is what `veh_handler_body` turns into `EXCEPTION_CONTINUE_SEARCH`.
    #[test]
    fn the_veh_guard_refuses_reentry_and_releases_on_drop() {
        assert!(!IN_VEH.with(|c| c.get()));
        {
            let _outer = VehGuard::enter().expect("first entry");
            assert!(
                VehGuard::enter().is_none(),
                "a re-entrant fault was accepted"
            );
        }
        assert!(
            !IN_VEH.with(|c| c.get()),
            "the guard did not release on drop"
        );
    }
}
