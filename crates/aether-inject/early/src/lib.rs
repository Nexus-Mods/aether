//! The early layer's stub: an import-free DLL (x64 or x86, as the injector
//! that maps it) the injector maps by hand into a suspended target, before
//! any of its threads has run.
//!
//! Aether injects the shim with `LoadLibraryW` on a remote thread, and that
//! thread runs the process's initialisation first: the static imports load
//! and their `DllMain`s run before the shim exists. A proxy a mod ships as a
//! static import (an ASI loader's `version.dll`, ReShade's `dxgi.dll`,
//! RED4ext's `winmm.dll`) reads its settings and lists its plugins from
//! there, beside the program on real disk, where only the staged import
//! closure is.
//!
//! The injector patches six ntdll exports in the target to jump to the
//! `hook_*` functions here. A name under one of the directories the launch
//! staged into ([`Config::prefixes`]) is handed to the injector
//! ([`request`]), which asks the director for it and puts what the director
//! has there on disk (`aether-inject`'s `early.rs`); then the real call runs
//! on a copy of the export's syscall thunk ([`Config::code`]). A directory
//! listed under one of them gets its matching entries put there the same way
//! first. The first hook to fire once kernel32 has initialised (before any
//! other DLL's `DllMain`, [`Config::early_shim`]) puts the exports' bytes
//! back and loads the shim there, which then hooks a clean ntdll as it
//! always has, and the call goes on through the shim. The remote thread
//! starts at [`aether_early_start`], after the process's initialisation,
//! which does the same if no hook did.
//!
//! Only ntdll is used, through addresses the injector writes into
//! [`Config`] (ntdll and kernel32 lie at the same address in every process
//! of a session). No `std`, no CRT, no imports and no entry point:
//! aether-inject's `build.rs` compiles this file with `rustc` alone
//! (`/NODEFAULTLIB`, `/NOENTRY`), and `#![no_builtins]` keeps the compiler
//! from turning the `mem*` routines below into calls to themselves.
#![no_std]
#![no_builtins]
#![allow(non_snake_case, clippy::missing_safety_doc)]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

#[path = "config.rs"]
mod config;
use config::*;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// The routines the compiler calls for copies and fills: the CRT's, which
// this DLL does without.
// (The lint expects them to come from a CRT, with C's signatures; these are
// those signatures.)
#[allow(unknown_lints, suspicious_runtime_symbol_definitions)]
mod mem {
    #[no_mangle]
    pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
        let mut i = 0;
        while i < n {
            *dst.add(i) = *src.add(i);
            i += 1;
        }
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
        if (dst as usize) < (src as usize) {
            return memcpy(dst, src, n);
        }
        let mut i = n;
        while i > 0 {
            i -= 1;
            *dst.add(i) = *src.add(i);
        }
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
        let mut i = 0;
        while i < n {
            *dst.add(i) = c as u8;
            i += 1;
        }
        dst
    }

    #[no_mangle]
    pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
        let mut i = 0;
        while i < n {
            let (x, y) = (*a.add(i), *b.add(i));
            if x != y {
                return i32::from(x) - i32::from(y);
            }
            i += 1;
        }
        0
    }
}

#[repr(C)]
pub struct UnicodeString {
    len: u16,
    max: u16,
    buf: *const u16,
}

#[repr(C)]
pub struct ObjectAttributes {
    len: u32,
    root: *mut c_void,
    name: *const UnicodeString,
    attrs: u32,
    sd: *mut c_void,
    sqos: *mut c_void,
}

type Handle = *mut c_void;
type Iosb = c_void;

/// The block the injector fills in, exported as `aether_early`.
#[repr(transparent)]
pub struct Shared(UnsafeCell<Config>);

// SAFETY: every field the hooks share is written under `lock` or atomically.
unsafe impl Sync for Shared {}

#[no_mangle]
pub static aether_early: Shared = Shared(UnsafeCell::new(Config::new()));

/// `size_of::<Config>()`, which `early.rs` checks against its own.
#[no_mangle]
pub static aether_early_layout: u32 = core::mem::size_of::<Config>() as u32;

fn cfg() -> *mut Config {
    aether_early.0.get()
}

unsafe fn atomic<'a>(p: *mut u32) -> &'a AtomicU32 {
    AtomicU32::from_ptr(p)
}

unsafe fn code(site: usize) -> usize {
    addr_of_mut!((*cfg()).code.0[site]) as usize
}

unsafe fn active() -> bool {
    atomic(addr_of_mut!((*cfg()).taken_over)).load(Ordering::Acquire) == 0
}

/// What a hook calls as the original: the thunk's copy while the layer is
/// active; once the exports' bytes are back, the export itself, which the
/// shim may have hooked since (so a call that came in as the shim loaded
/// goes through the shim).
unsafe fn real(site: usize) -> usize {
    if active() {
        code(site)
    } else {
        (*cfg()).sites[site].target as usize
    }
}

/// Whether kernel32 has run its `DllMain`: the loader sets
/// `LDRP_PROCESS_ATTACH_CALLED` (0x80000, Wine's `LDR_PROCESS_ATTACHED`) in
/// its entry of the PEB's load-order list. The loader starts kernel32 before
/// any other DLL, so from then on the shim can load, and no proxy has run.
unsafe fn kernel32_ready() -> bool {
    let peb = peb();
    if peb.is_null() {
        return false;
    }
    let ldr = *(peb.add(LDR) as *const *const u8);
    if ldr.is_null() {
        return false;
    }
    // PEB_LDR_DATA.InLoadOrderModuleList; each entry's links are its start
    let head = ldr.add(LOAD_ORDER);
    let mut e = *(head as *const *const u8);
    let mut n = 0;
    while !e.is_null() && e != head && n < 1024 {
        // LDR_DATA_TABLE_ENTRY.BaseDllName, .Flags
        let name = &*(e.add(BASE_NAME) as *const UnicodeString);
        if !name.buf.is_null() {
            let units = core::slice::from_raw_parts(name.buf, usize::from(name.len / 2));
            let want = b"KERNEL32.DLL";
            if units.len() == want.len()
                && units
                    .iter()
                    .zip(want)
                    .all(|(&u, &w)| upper(u) == u16::from(w))
            {
                return *(e.add(FLAGS) as *const u32) & 0x80000 != 0;
            }
        }
        e = *(e as *const *const u8);
        n += 1;
    }
    false
}

// The offsets `kernel32_ready` reads: PEB.Ldr, PEB_LDR_DATA's
// InLoadOrderModuleList, and LDR_DATA_TABLE_ENTRY's BaseDllName and Flags.
#[cfg(target_arch = "x86_64")]
const LDR: usize = 0x18;
#[cfg(target_arch = "x86_64")]
const LOAD_ORDER: usize = 0x10;
#[cfg(target_arch = "x86_64")]
const BASE_NAME: usize = 0x58;
#[cfg(target_arch = "x86_64")]
const FLAGS: usize = 0x68;
#[cfg(target_arch = "x86")]
const LDR: usize = 0x0c;
#[cfg(target_arch = "x86")]
const LOAD_ORDER: usize = 0x0c;
#[cfg(target_arch = "x86")]
const BASE_NAME: usize = 0x2c;
#[cfg(target_arch = "x86")]
const FLAGS: usize = 0x34;

/// This thread's PEB (a 32-bit process's own, under WOW64).
#[cfg(target_arch = "x86_64")]
unsafe fn peb() -> *const u8 {
    let peb: *const u8;
    core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly, preserves_flags));
    peb
}

#[cfg(target_arch = "x86")]
unsafe fn peb() -> *const u8 {
    let peb: *const u8;
    core::arch::asm!("mov {}, fs:[0x30]", out(reg) peb, options(nostack, readonly, preserves_flags));
    peb
}

/// Takes the layer down: marks it inactive and puts every hooked export's
/// bytes back, once (a second time would undo the shim's own hooks).
/// Whether this call did it.
unsafe fn take_over() -> bool {
    let c = cfg();
    if atomic(addr_of_mut!((*c).taken_over))
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let protect: ProtectFn = core::mem::transmute((*c).nt_protect as usize);
    let me = usize::MAX as Handle; // NtCurrentProcess
    for i in 0..SITES {
        let s = &*addr_of_mut!((*c).sites[i]);
        if s.target == 0 {
            continue;
        }
        let (mut base, mut size, mut old) = (s.target as usize, 16usize, 0u32);
        if protect(me, &mut base, &mut size, 0x40, &mut old) < 0 {
            continue;
        }
        let at = s.target as usize as *mut u8;
        for (k, &b) in s.orig.iter().enumerate() {
            core::ptr::write_volatile(at.add(k), b);
        }
        let (mut base, mut size) = (s.target as usize, 16usize);
        protect(me, &mut base, &mut size, old, &mut old);
    }
    true
}

/// With [`Config::early_shim`], the first hook to fire once kernel32 is up
/// takes the layer down and loads the shim right there, on the thread that
/// is initialising the process, before the call it came in with goes on.
unsafe fn maybe_load_shim() {
    let c = cfg();
    if (*c).early_shim == 0 || !active() || !kernel32_ready() {
        return;
    }
    let state = atomic(addr_of_mut!((*c).shim_state));
    if state
        .compare_exchange(
            SHIM_NOT_LOADED,
            SHIM_LOADING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    if !take_over() {
        state.store(SHIM_NOT_LOADED, Ordering::Release);
        return;
    }
    let load: LoadLibraryFn = core::mem::transmute((*c).load_library_w as usize);
    let module = load(addr_of_mut!((*c).shim_path) as *const c_void);
    state.store(
        if module != 0 {
            SHIM_LOADED_EARLY
        } else {
            SHIM_EARLY_FAILED
        },
        Ordering::Release,
    );
}

unsafe fn lock() {
    let l = atomic(addr_of_mut!((*cfg()).lock));
    while l
        .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

unsafe fn unlock() {
    atomic(addr_of_mut!((*cfg()).lock)).store(0, Ordering::Release);
}

fn upper(c: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&c) {
        c - 32
    } else {
        c
    }
}

/// The units of `oa`'s name when it is absolute (no root handle) and lies
/// under a prefix, else `None`.
unsafe fn staged<'a>(oa: *const ObjectAttributes) -> Option<&'a [u16]> {
    if oa.is_null() || !(*oa).root.is_null() || (*oa).name.is_null() {
        return None;
    }
    let name = &*(*oa).name;
    if name.buf.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(name.buf, usize::from(name.len / 2));
    if units.len() > PATH_UNITS {
        return None;
    }
    let c = cfg();
    for i in 0..((*c).nprefix as usize).min(PREFIXES) {
        let p = &*addr_of_mut!((*c).prefixes[i]);
        let n = p.len as usize;
        if n == 0 || units.len() < n {
            continue;
        }
        let same = p.units[..n]
            .iter()
            .zip(&units[..n])
            .all(|(&want, &got)| want == upper(got));
        if same && (units.len() == n || units[n] == u16::from(b'\\')) {
            return Some(units);
        }
    }
    None
}

type SetEventFn = unsafe extern "system" fn(Handle, *mut i32) -> i32;
type WaitFn = unsafe extern "system" fn(Handle, u8, *const i64) -> i32;

/// Hands `op` on `path` (and a listing's `mask`) to the injector and waits
/// for it, at most a minute; one request at a time.
unsafe fn request(op: u32, path: &[u16], mask: &[u16]) {
    lock();
    // SAFETY: the request fields are this thread's while it holds the lock.
    let c = &mut *cfg();
    c.req_op = op;
    c.req_path[..path.len()].copy_from_slice(path);
    c.req_len = path.len() as u32;
    let n = mask.len().min(MASK_UNITS);
    c.req_mask[..n].copy_from_slice(&mask[..n]);
    c.req_mask_len = n as u32;
    c.req_seq = c.req_seq.wrapping_add(1);
    let set: SetEventFn = core::mem::transmute(c.nt_set_event as usize);
    let wait: WaitFn = core::mem::transmute(c.nt_wait as usize);
    let timeout: i64 = -60 * 10_000_000;
    if set(c.req_event as Handle, core::ptr::null_mut()) >= 0 {
        wait(c.done_event as Handle, 0, &timeout);
    }
    unlock();
}

unsafe fn remember_dir(handle: Handle, path: &[u16]) {
    lock();
    let c = cfg();
    let next = (*c).dir_next;
    (*c).dir_next = next.wrapping_add(1);
    let d = &mut *addr_of_mut!((*c).dirs[next as usize % DIRS]);
    d.handle = handle as u64;
    d.listed = 0;
    d.len = path.len() as u32;
    d.units[..path.len()].copy_from_slice(path);
    unlock();
}

unsafe fn list_dir(handle: Handle, mask: *const UnicodeString) {
    let h = handle as u64;
    let c = cfg();
    for i in 0..DIRS {
        let d = &mut *addr_of_mut!((*c).dirs[i]);
        if d.handle == 0 || d.handle != h || d.listed != 0 {
            continue;
        }
        d.listed = 1;
        let m: &[u16] = if mask.is_null() || (*mask).buf.is_null() {
            &[]
        } else {
            core::slice::from_raw_parts((*mask).buf, usize::from((*mask).len / 2))
        };
        request(OP_LIST, &d.units[..d.len as usize], m);
        return;
    }
}

const FILE_DIRECTORY_FILE: u32 = 0x1;

type CreateFn = unsafe extern "system" fn(
    *mut Handle,
    u32,
    *const ObjectAttributes,
    *mut Iosb,
    *mut i64,
    u32,
    u32,
    u32,
    u32,
    *mut c_void,
    u32,
) -> i32;
type OpenFn = unsafe extern "system" fn(
    *mut Handle,
    u32,
    *const ObjectAttributes,
    *mut Iosb,
    u32,
    u32,
) -> i32;
type AttrsFn = unsafe extern "system" fn(*const ObjectAttributes, *mut c_void) -> i32;
type DirFn = unsafe extern "system" fn(
    Handle,
    Handle,
    *mut c_void,
    *mut c_void,
    *mut Iosb,
    *mut c_void,
    u32,
    u32,
    u8,
    *const UnicodeString,
    u8,
) -> i32;
type DirExFn = unsafe extern "system" fn(
    Handle,
    Handle,
    *mut c_void,
    *mut c_void,
    *mut Iosb,
    *mut c_void,
    u32,
    u32,
    u32,
    *const UnicodeString,
) -> i32;

#[no_mangle]
pub unsafe extern "system" fn hook_NtCreateFile(
    h: *mut Handle,
    access: u32,
    oa: *const ObjectAttributes,
    io: *mut Iosb,
    alloc: *mut i64,
    attrs: u32,
    share: u32,
    disp: u32,
    opts: u32,
    ea: *mut c_void,
    ealen: u32,
) -> i32 {
    maybe_load_shim();
    let name = if active() { staged(oa) } else { None };
    if let Some(n) = name {
        request(OP_PATH, n, &[]);
    }
    let real: CreateFn = core::mem::transmute(real(SITE_CREATE));
    let st = real(
        h, access, oa, io, alloc, attrs, share, disp, opts, ea, ealen,
    );
    if let Some(n) = name {
        if st >= 0 && opts & FILE_DIRECTORY_FILE != 0 {
            remember_dir(*h, n);
        }
    }
    st
}

#[no_mangle]
pub unsafe extern "system" fn hook_NtOpenFile(
    h: *mut Handle,
    access: u32,
    oa: *const ObjectAttributes,
    io: *mut Iosb,
    share: u32,
    opts: u32,
) -> i32 {
    maybe_load_shim();
    let name = if active() { staged(oa) } else { None };
    if let Some(n) = name {
        request(OP_PATH, n, &[]);
    }
    let real: OpenFn = core::mem::transmute(real(SITE_OPEN));
    let st = real(h, access, oa, io, share, opts);
    if let Some(n) = name {
        if st >= 0 && opts & FILE_DIRECTORY_FILE != 0 {
            remember_dir(*h, n);
        }
    }
    st
}

#[no_mangle]
pub unsafe extern "system" fn hook_NtQueryAttributesFile(
    oa: *const ObjectAttributes,
    info: *mut c_void,
) -> i32 {
    maybe_load_shim();
    if active() {
        if let Some(n) = staged(oa) {
            request(OP_PATH, n, &[]);
        }
    }
    let real: AttrsFn = core::mem::transmute(real(SITE_ATTRS));
    real(oa, info)
}

#[no_mangle]
pub unsafe extern "system" fn hook_NtQueryFullAttributesFile(
    oa: *const ObjectAttributes,
    info: *mut c_void,
) -> i32 {
    maybe_load_shim();
    if active() {
        if let Some(n) = staged(oa) {
            request(OP_PATH, n, &[]);
        }
    }
    let real: AttrsFn = core::mem::transmute(real(SITE_FULL_ATTRS));
    real(oa, info)
}

#[no_mangle]
pub unsafe extern "system" fn hook_NtQueryDirectoryFile(
    h: Handle,
    ev: Handle,
    apc: *mut c_void,
    ctx: *mut c_void,
    io: *mut Iosb,
    buf: *mut c_void,
    len: u32,
    class: u32,
    single: u8,
    mask: *const UnicodeString,
    restart: u8,
) -> i32 {
    maybe_load_shim();
    if active() {
        list_dir(h, mask);
    }
    let real: DirFn = core::mem::transmute(real(SITE_DIR));
    real(h, ev, apc, ctx, io, buf, len, class, single, mask, restart)
}

#[no_mangle]
pub unsafe extern "system" fn hook_NtQueryDirectoryFileEx(
    h: Handle,
    ev: Handle,
    apc: *mut c_void,
    ctx: *mut c_void,
    io: *mut Iosb,
    buf: *mut c_void,
    len: u32,
    class: u32,
    flags: u32,
    mask: *const UnicodeString,
) -> i32 {
    maybe_load_shim();
    if active() {
        list_dir(h, mask);
    }
    let real: DirExFn = core::mem::transmute(real(SITE_DIR_EX));
    real(h, ev, apc, ctx, io, buf, len, class, flags, mask)
}

type ProtectFn = unsafe extern "system" fn(Handle, *mut usize, *mut usize, u32, *mut u32) -> i32;
type LoadLibraryFn = unsafe extern "system" fn(*const c_void) -> usize;

/// The remote thread's start: by now the process's initialisation has run.
/// Puts every hooked export back as it was, unless a hook loaded the shim
/// already (`maybe_load_shim`), then loads the shim (`path`; a load of a
/// shim already loaded only returns it),
/// returning `LoadLibraryW`'s module truncated as its exit code, as a thread
/// started at `LoadLibraryW` itself would.
#[no_mangle]
pub unsafe extern "system" fn aether_early_start(path: *const c_void) -> u32 {
    take_over();
    let c = cfg();
    let load: LoadLibraryFn = core::mem::transmute((*c).load_library_w as usize);
    load(path) as u32
}
