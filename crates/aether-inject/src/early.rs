//! The early layer: what a target reads before the shim exists comes from the
//! director anyway.
//!
//! The remote thread that loads the shim runs the process's initialisation
//! first, so the static imports' `DllMain`s run before any hook: a proxy DLL
//! a mod ships beside the program (an ASI loader, ReShade, RED4ext) reads its
//! settings and lists its plugins on real disk, in the folder the launch
//! staged the program into, where only the program and its import closure
//! are. Before the target runs, [`Early::install`] maps `early/src/lib.rs`'s
//! DLL into it and points six ntdll exports (the opens, the attribute
//! queries and the directory listings) at it. A name it sees under a staged
//! folder (the launch image's, or a root's backing directory) comes back
//! here while that call waits; [`Early::wait`] asks the director for it and
//! puts what the director has at that name on disk (a file's bytes, a
//! directory, a listing's matching entries), then lets the call go on.
//!
//! That serves only the loader's own probes in practice: the first hooked
//! call once kernel32 has initialised (before any other DLL's `DllMain`)
//! puts the exports back and loads the shim right there, on the thread
//! initialising the process, and the call goes on through the shim's hook;
//! from then on the static imports run with the hooks live. The remote
//! thread starts at the stub's `aether_early_start`, which runs after the
//! initialisation; its load of the shim only returns the module already
//! loaded. With [`EARLY_SHIM`] `0` it is that start which puts the exports
//! back and loads the shim, and a static import's writes reach the folder.
//!
//! What is put on disk stays there: the folder is the launch's staging, and
//! once the shim is up it routes every name under it to the director
//! (`aether-hooks`, `FuseClient::try_init_from_env`), so the program sees the
//! director's bytes either way. What the program writes there before the
//! shim, it writes to that folder.
//!
//! Each injector serves targets of its own architecture, so each carries the
//! stub built for it: x64's, or x86's for a 32-bit (WOW64) target.
//! The shim, which links this crate, injects the children it starts with
//! the stub too ([`install_for_child`]), loading only: nothing in the
//! parent answers a child's requests, and the child's image and import
//! closure are staged on disk before it is created.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::offset_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aether_ipc::{ring, Geom, Notifier, RingClient, SharedSeg};
use aether_protocol::{OpenFlags, ReadReply, Reply, Request, RootId};
use aether_redirect::RootMap;
use aether_rpc::Client;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualProtectEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenEventW, SetEvent, WaitForMultipleObjects,
    EVENT_MODIFY_STATE,
};

/// `early/src/lib.rs`, built by `build.rs` for this injector's architecture.
#[cfg(target_arch = "x86_64")]
static STUB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/aether_early_x64.dll"));
#[cfg(target_arch = "x86")]
static STUB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/aether_early_x86.dll"));

/// The block the stub exports and this fills in: one definition for both.
#[path = "../early/src/config.rs"]
#[allow(dead_code)]
mod config;
use config::{Config, MAGIC, MASK_UNITS, OP_LIST, OP_PATH, PATH_UNITS, PREFIXES, SITES};

use aether_env::{EARLY_LAYER, EARLY_SHIM, EARLY_TRACE};

/// The exports hooked, in the stub's `Config.sites` order, with the stub's
/// hook for each. `NtQueryDirectoryFileEx` is missing from older systems.
const HOOKS: [(&str, &str, bool); SITES] = [
    ("NtCreateFile", "hook_NtCreateFile", true),
    ("NtOpenFile", "hook_NtOpenFile", true),
    ("NtQueryAttributesFile", "hook_NtQueryAttributesFile", true),
    (
        "NtQueryFullAttributesFile",
        "hook_NtQueryFullAttributesFile",
        true,
    ),
    ("NtQueryDirectoryFile", "hook_NtQueryDirectoryFile", true),
    (
        "NtQueryDirectoryFileEx",
        "hook_NtQueryDirectoryFileEx",
        false,
    ),
];

/// Whether `t` is a syscall thunk whose copy is the original call: its body
/// is position-independent.
///
/// x64, on Windows and on Wine: `mov r10, rcx; mov eax, imm32; ...`.
#[cfg(target_arch = "x86_64")]
fn is_thunk(t: &[u8; 32]) -> bool {
    t[..4] == [0x4c, 0x8b, 0xd1, 0xb8]
}

/// x86 under WOW64, on Windows and on Wine: `mov eax, imm32; mov edx, imm32;
/// call edx; ret n`, the second immediate the absolute address of the
/// transition into 64-bit code. (A native 32-bit Windows' thunk calls a
/// relative address, and is refused.)
#[cfg(target_arch = "x86")]
fn is_thunk(t: &[u8; 32]) -> bool {
    t[0] == 0xb8 && t[5] == 0xba && t[10..12] == [0xff, 0xd2]
}

/// The bytes that send an export to `hook`: `mov rax, hook; jmp rax`.
#[cfg(target_arch = "x86_64")]
fn jump(hook: usize) -> Vec<u8> {
    let mut j = vec![0x48, 0xb8];
    j.extend_from_slice(&(hook as u64).to_le_bytes());
    j.extend_from_slice(&[0xff, 0xe0]);
    j
}

/// `mov eax, hook; jmp eax`: 7 bytes, inside the 15 of the thunk.
#[cfg(target_arch = "x86")]
fn jump(hook: usize) -> Vec<u8> {
    let mut j = vec![0xb8];
    j.extend_from_slice(&(hook as u32).to_le_bytes());
    j.extend_from_slice(&[0xff, 0xe0]);
    j
}

/// The early layer in one target: where the stub is, and what answers it.
pub(crate) struct Early {
    /// `aether_early` in the target.
    config: usize,
    /// `aether_early_start` in the target: the remote thread's start.
    start: usize,
    req_ev: HANDLE,
    done_ev: HANDLE,
    env: HashMap<String, String>,
    roots: RootMap,
    ring: Option<Result<Ring, String>>,
    /// Names already answered, folded: each is put on disk at most once.
    seen: HashSet<String>,
    trace: bool,
}

impl Drop for Early {
    fn drop(&mut self) {
        // SAFETY: our own event handles, closed once.
        unsafe {
            CloseHandle(self.req_ev);
            CloseHandle(self.done_ev);
        }
    }
}

fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The stub laid out as it is in memory: headers and sections at their
/// RVAs, base relocations not yet applied.
struct Image {
    bytes: Vec<u8>,
    base: u64,
    reloc: (usize, usize),
    exports: HashMap<String, usize>,
}

fn image(file: &[u8]) -> Result<Image, String> {
    let nt = rd32(file, 0x3c) as usize;
    if file.get(nt..nt + 4) != Some(b"PE\0\0") {
        return Err("the early stub is not a PE image".into());
    }
    let sections = rd16(file, nt + 6) as usize;
    let opt = nt + 24;
    // PE32+ (x64) or PE32 (x86): where the image base and the data
    // directories are
    let (base, dirs) = match rd16(file, opt) {
        0x20b => (rd64(file, opt + 24), opt + 112),
        0x10b => (u64::from(rd32(file, opt + 28)), opt + 96),
        m => return Err(format!("the early stub's optional header is {m:#x}")),
    };
    let size = rd32(file, opt + 56) as usize;
    let headers = rd32(file, opt + 60) as usize;
    let dir = |i: usize| {
        (
            rd32(file, dirs + i * 8) as usize,
            rd32(file, dirs + i * 8 + 4) as usize,
        )
    };
    let mut bytes = vec![0u8; size];
    bytes[..headers].copy_from_slice(&file[..headers]);
    let table = opt + rd16(file, nt + 20) as usize;
    for s in 0..sections {
        let h = table + s * 40;
        let va = rd32(file, h + 12) as usize;
        let raw = rd32(file, h + 16) as usize;
        let ptr = rd32(file, h + 20) as usize;
        let n = raw.min(size - va);
        bytes[va..va + n].copy_from_slice(&file[ptr..ptr + n]);
    }
    let (edir, _) = dir(0);
    let mut exports = HashMap::new();
    let count = rd32(&bytes, edir + 24) as usize;
    let (funcs, names, ords) = (
        rd32(&bytes, edir + 28) as usize,
        rd32(&bytes, edir + 32) as usize,
        rd32(&bytes, edir + 36) as usize,
    );
    for i in 0..count {
        let at = rd32(&bytes, names + i * 4) as usize;
        let end = at + bytes[at..].iter().position(|&b| b == 0).unwrap_or(0);
        let name = String::from_utf8_lossy(&bytes[at..end]).into_owned();
        let ord = rd16(&bytes, ords + i * 2) as usize;
        exports.insert(name, rd32(&bytes, funcs + ord * 4) as usize);
    }
    Ok(Image {
        bytes,
        base,
        reloc: dir(5),
        exports,
    })
}

impl Image {
    fn export(&self, name: &str) -> Result<usize, String> {
        self.exports
            .get(name)
            .copied()
            .ok_or_else(|| format!("the early stub exports no {name}"))
    }

    /// Applies the base relocations for a load at `at`.
    fn relocate(&mut self, at: u64) {
        let delta = at.wrapping_sub(self.base);
        let (start, len) = self.reloc;
        let mut off = start;
        while off + 8 <= start + len {
            let page = rd32(&self.bytes, off) as usize;
            let block = rd32(&self.bytes, off + 4) as usize;
            if block < 8 {
                break;
            }
            for e in (8..block).step_by(2) {
                let entry = rd16(&self.bytes, off + e);
                let p = page + (entry & 0xfff) as usize;
                match entry >> 12 {
                    // IMAGE_REL_BASED_DIR64
                    10 => {
                        let v = rd64(&self.bytes, p).wrapping_add(delta);
                        self.bytes[p..p + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    // IMAGE_REL_BASED_HIGHLOW
                    3 => {
                        let v = rd32(&self.bytes, p).wrapping_add(delta as u32);
                        self.bytes[p..p + 4].copy_from_slice(&v.to_le_bytes());
                    }
                    // ABSOLUTE (0) is padding
                    _ => {}
                }
            }
            off += block;
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `name=value` pairs of a `CreateProcessW` environment block.
fn env_map(block: &[u16]) -> HashMap<String, String> {
    block
        .split(|&u| u == 0)
        .take_while(|s| !s.is_empty())
        .filter_map(|s| {
            let s = String::from_utf16_lossy(s);
            // `=C:=C:\...` entries name drives, not variables: skip a
            // leading `=`
            let eq = s.char_indices().skip(1).find(|&(_, c)| c == '=')?.0;
            Some((s[..eq].to_ascii_uppercase(), s[eq + 1..].to_string()))
        })
        .filter(|(k, _): &(String, String)| !k.starts_with('='))
        .collect()
}

/// The staged folders, as the stub matches them: `\??\` and the path upper
/// cased, without a trailing separator.
fn staged_folders(env: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = env.get(aether_env::LAUNCH_IMAGE).and_then(|p| {
        Path::new(p)
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
    }) {
        out.push(dir);
    }
    for (_, p) in
        aether_env::session::backings(env.get(aether_env::ROOT_BACKINGS).map(|s| s.as_str()))
    {
        out.push(p);
    }
    let mut seen = HashSet::new();
    out.into_iter()
        .map(|p| {
            let p = p.trim_end_matches('\\');
            let p = p.strip_prefix(r"\\?\").unwrap_or(p);
            format!(r"\??\{}", p.to_ascii_uppercase())
        })
        .filter(|p| p.len() > 6 && seen.insert(p.clone()))
        .take(PREFIXES)
        .collect()
}

/// Every root the shim routes, as `FuseClient::try_init_from_env` builds them:
/// the roots, the launch image's folder as root 0, each backing directory.
fn routing(env: &HashMap<String, String>) -> Result<RootMap, String> {
    let get = |k: &str| env.get(k).map(String::as_str);
    let virtual_dir = get(aether_env::VIRTUAL_DIR).ok_or("VFS_VIRTUAL_DIR is not set")?;
    let mut roots: Vec<(RootId, String)> =
        aether_env::session::roots(virtual_dir, get(aether_env::VIRTUAL_ROOTS))
            .into_iter()
            .map(|(id, p)| (RootId(id), p))
            .collect();
    if let Some(dir) = get(aether_env::LAUNCH_IMAGE).and_then(|p| Path::new(p).parent()) {
        roots.push((RootId::DEFAULT, dir.to_string_lossy().into_owned()));
    }
    roots.extend(
        aether_env::session::backings(get(aether_env::ROOT_BACKINGS))
            .into_iter()
            .map(|(id, p)| (RootId(id), p)),
    );
    let scan: Vec<&str> = roots.iter().map(|(_, p)| p.as_str()).collect();
    let volumes = aether_redirect::resolve_volume_map_for(&scan);
    let refs: Vec<(RootId, &str)> = roots.iter().map(|(id, p)| (*id, p.as_str())).collect();
    RootMap::with_roots(&refs, volumes).map_err(|e| format!("the roots are not usable: {e:?}"))
}

unsafe fn proc_address(module: &str, name: &str) -> Option<u64> {
    let m = GetModuleHandleW(wide(module).as_ptr());
    if m.is_null() {
        return None;
    }
    let c = std::ffi::CString::new(name).ok()?;
    GetProcAddress(m, c.as_ptr().cast()).map(|f| f as usize as u64)
}

unsafe fn read(process: HANDLE, at: usize, buf: &mut [u8]) -> bool {
    let mut got = 0usize;
    ReadProcessMemory(
        process,
        at as *const c_void,
        buf.as_mut_ptr().cast(),
        buf.len(),
        &mut got,
    ) != 0
        && got == buf.len()
}

unsafe fn write(process: HANDLE, at: usize, buf: &[u8]) -> bool {
    let mut put = 0usize;
    WriteProcessMemory(
        process,
        at as *const c_void,
        buf.as_ptr().cast(),
        buf.len(),
        &mut put,
    ) != 0
        && put == buf.len()
}

/// Writes `bytes` over the start of `target`, an export (its [`jump`], or
/// its original bytes).
unsafe fn patch(process: HANDLE, target: usize, bytes: &[u8]) -> bool {
    let mut old = 0u32;
    if VirtualProtectEx(
        process,
        target as *const c_void,
        16,
        PAGE_EXECUTE_READWRITE,
        &mut old,
    ) == 0
    {
        return false;
    }
    let ok = write(process, target, bytes);
    VirtualProtectEx(process, target as *const c_void, 16, old, &mut old);
    FlushInstructionCache(process, target as *const c_void, 16);
    ok
}

/// The stub, mapped into a target and patched in: where its block and its
/// start are.
struct Placed {
    /// `aether_early` in the target.
    config: usize,
    /// `aether_early_start` in the target.
    start: usize,
}

/// Maps the stub into `process` and points the hooked exports at it, with
/// `shim` to load early (unless `early_shim` is false), `folders` to ask
/// about, and the events it asks and waits on (none with no `folders`: it
/// then never asks). `Err` says why the layer is off.
///
/// # Safety
/// As [`Early::install`].
unsafe fn place(
    process: HANDLE,
    shim: &str,
    early_shim: bool,
    folders: &[String],
    events: Option<(HANDLE, HANDLE)>,
) -> Result<Placed, String> {
    let mut img = image(STUB)?;
    let at = img.export("aether_early")?;
    let layout = rd32(&img.bytes, img.export("aether_early_layout")?) as usize;
    if layout != size_of::<Config>() || rd64(&img.bytes, at) != MAGIC {
        return Err(format!(
            "the early stub's Config is {layout} bytes, early.rs's {}",
            size_of::<Config>()
        ));
    }

    let remote = VirtualAllocEx(
        process,
        core::ptr::null(),
        img.bytes.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    ) as usize;
    if remote == 0 {
        return Err("VirtualAllocEx for the early stub failed".into());
    }
    img.relocate(remote as u64);

    // SAFETY: `at` is the stub's `aether_early`, a `Config` (checked by
    // size and magic above); the image buffer holds it whole.
    let layout = std::alloc::Layout::new::<Config>();
    let raw = std::alloc::alloc_zeroed(layout).cast::<Config>();
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let mut cfg: Box<Config> = Box::from_raw(raw);
    core::ptr::copy_nonoverlapping(
        img.bytes[at..].as_ptr(),
        (&mut *cfg as *mut Config).cast::<u8>(),
        size_of::<Config>(),
    );
    let nt = |name: &str| proc_address("ntdll.dll", name);
    cfg.nt_set_event = nt("NtSetEvent").ok_or("no NtSetEvent")?;
    cfg.nt_wait = nt("NtWaitForSingleObject").ok_or("no NtWaitForSingleObject")?;
    cfg.nt_protect = nt("NtProtectVirtualMemory").ok_or("no NtProtectVirtualMemory")?;
    cfg.load_library_w = proc_address("kernel32.dll", "LoadLibraryW").ok_or("no LoadLibraryW")?;
    let mut patches = Vec::new();
    for (i, (export, hook, required)) in HOOKS.iter().enumerate() {
        let Some(target) = nt(export) else {
            if *required {
                return Err(format!("no {export}"));
            }
            continue;
        };
        let mut thunk = [0u8; 32];
        if !read(process, target as usize, &mut thunk) || !is_thunk(&thunk) {
            return Err(format!(
                "{export} does not start as a syscall thunk ({:02x?})",
                &thunk[..4]
            ));
        }
        cfg.sites[i].target = target;
        cfg.sites[i].orig.copy_from_slice(&thunk[..16]);
        cfg.code.0[i] = thunk;
        patches.push((target as usize, jump(remote + img.export(hook)?)));
    }
    if early_shim {
        let units: Vec<u16> = shim.encode_utf16().collect();
        if units.len() < cfg.shim_path.len() {
            cfg.shim_path[..units.len()].copy_from_slice(&units);
            cfg.early_shim = 1;
        }
    }
    if let Some((req_ev, done_ev)) = events {
        for (i, f) in folders.iter().enumerate() {
            let units: Vec<u16> = f.encode_utf16().collect();
            if units.len() > PATH_UNITS {
                continue;
            }
            cfg.prefixes[i].len = units.len() as u32;
            cfg.prefixes[i].units[..units.len()].copy_from_slice(&units);
            cfg.nprefix = i as u32 + 1;
        }
        for (mine, theirs) in [(req_ev, &mut cfg.req_event), (done_ev, &mut cfg.done_event)] {
            let mut h: HANDLE = core::ptr::null_mut();
            if DuplicateHandle(
                GetCurrentProcess(),
                mine,
                process,
                &mut h,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return Err("DuplicateHandle of an early-layer event failed".into());
            }
            *theirs = h as u64;
        }
    }
    core::ptr::copy_nonoverlapping(
        (&*cfg as *const Config).cast::<u8>(),
        img.bytes[at..].as_mut_ptr(),
        size_of::<Config>(),
    );
    if !write(process, remote, &img.bytes) {
        return Err("writing the early stub failed".into());
    }
    for (n, (target, jump)) in patches.iter().enumerate() {
        if !patch(process, *target, jump) {
            // Put back what was patched: a half-hooked ntdll is worse than none.
            for (target, _) in &patches[..n] {
                let i = HOOKS
                    .iter()
                    .position(|h| proc_address("ntdll.dll", h.0) == Some(*target as u64));
                if let Some(i) = i {
                    patch(process, *target, &cfg.sites[i].orig);
                }
            }
            return Err("patching ntdll failed".into());
        }
    }
    Ok(Placed {
        config: remote + at,
        start: remote + img.export("aether_early_start")?,
    })
}

/// The early layer for a child the shim starts, which the shim injects
/// itself: the stub alone, loading the shim from its first hook once
/// kernel32 is up, so that the child's static imports run their `DllMain`s
/// with the hooks live. Nothing is served before that: the child's image
/// and its import closure were staged on disk before it was created, and
/// what else the loader probes then is the system's. The remote thread's
/// start (`aether_early_start`), or `Err` saying why the layer is off: when
/// [`EARLY_LAYER`] or [`EARLY_SHIM`] is `0` in this process's environment
/// (which the child inherits), or as [`Early::install`].
///
/// # Safety
/// `process` must be a suspended process of this injector's architecture
/// with all access, whose threads have not run.
pub(crate) unsafe fn install_for_child(process: HANDLE, shim: &str) -> Result<usize, String> {
    for name in [EARLY_LAYER, EARLY_SHIM] {
        if std::env::var(name).is_ok_and(|v| v == "0") {
            return Err(format!("{name} is 0"));
        }
    }
    place(process, shim, true, &[], None).map(|p| p.start)
}

impl Early {
    /// Installs the early layer into `process`, created suspended and not yet
    /// run, whose environment is `env` (its `CreateProcessW` block). `None`
    /// when the launch stages nothing (no `VFS_LAUNCH_IMAGE` or
    /// `VFS_ROOT_BACKINGS`), or when [`EARLY_LAYER`] is `0`; `Err` when it
    /// cannot be installed (an ntdll the layer does not know, say). Either
    /// way the launch goes on without it.
    ///
    /// # Safety
    /// `process` must be a suspended process of this injector's
    /// architecture with all access, whose threads have not run.
    pub(crate) unsafe fn install(
        process: HANDLE,
        env: &[u16],
        shim: &str,
    ) -> Result<Option<Early>, String> {
        let env = env_map(env);
        if env.get(EARLY_LAYER).is_some_and(|v| v == "0") {
            return Ok(None);
        }
        let folders = staged_folders(&env);
        if folders.is_empty() {
            return Ok(None);
        }
        let roots = routing(&env)?;
        let req_ev = CreateEventW(core::ptr::null(), 0, 0, core::ptr::null());
        let done_ev = CreateEventW(core::ptr::null(), 0, 0, core::ptr::null());
        let mut early = Early {
            config: 0,
            start: 0,
            req_ev,
            done_ev,
            env,
            roots,
            ring: None,
            seen: HashSet::new(),
            trace: false,
        };
        if req_ev.is_null() || done_ev.is_null() {
            return Err("CreateEventW for the early layer failed".into());
        }
        let early_shim = early.env.get(EARLY_SHIM).is_none_or(|v| v != "0");
        let placed = place(process, shim, early_shim, &folders, Some((req_ev, done_ev)))?;
        early.config = placed.config;
        early.start = placed.start;
        early.trace = early.env.get(EARLY_TRACE).is_some_and(|v| v == "1");
        Ok(Some(early))
    }

    /// The remote thread's start: `aether_early_start`, which takes the
    /// shim's path as `LoadLibraryW` does.
    pub(crate) fn start(&self) -> usize {
        self.start
    }

    /// Waits up to `timeout_ms` for `thread` to end, answering the stub's
    /// requests meanwhile. Whether it ended.
    ///
    /// # Safety
    /// `process` is the target `install` hooked; `thread` its remote thread.
    pub(crate) unsafe fn wait(&mut self, process: HANDLE, thread: HANDLE, timeout_ms: u32) -> bool {
        let end = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            let left = end.saturating_duration_since(Instant::now()).as_millis() as u32;
            let handles = [thread, self.req_ev];
            match WaitForMultipleObjects(2, handles.as_ptr(), 0, left) {
                WAIT_OBJECT_0 => {
                    if self.trace {
                        self.report(process);
                    }
                    return true;
                }
                w if w == WAIT_OBJECT_0 + 1 => {
                    self.answer(process);
                    SetEvent(self.done_ev);
                }
                _ => return false,
            }
        }
    }

    /// What the layer did, for `VFS_EARLY_TRACE`.
    unsafe fn report(&self, process: HANDLE) {
        let mut word = [0u8; 4];
        let mut at = |off: usize| {
            read(process, self.config + off, &mut word).then(|| u32::from_le_bytes(word))
        };
        let state = at(offset_of!(Config, shim_state));
        let requests = at(offset_of!(Config, req_seq));
        eprintln!(
            "vfs-inject: early: shim state {state:?} (2: loaded during initialisation), {requests:?} requests"
        );
    }

    unsafe fn answer(&mut self, process: HANDLE) {
        let field = |off: usize| self.config + off;
        let mut word = [0u8; 4];
        let mut u32_at =
            |off: usize| read(process, field(off), &mut word).then(|| u32::from_le_bytes(word));
        let (Some(op), Some(len), Some(mask_len)) = (
            u32_at(offset_of!(Config, req_op)),
            u32_at(offset_of!(Config, req_len)),
            u32_at(offset_of!(Config, req_mask_len)),
        ) else {
            return;
        };
        let units = |off: usize, n: usize| -> Option<String> {
            let mut b = vec![0u8; n * 2];
            read(process, field(off), &mut b).then(|| {
                let u: Vec<u16> = b
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&u)
            })
        };
        let Some(path) = units(offset_of!(Config, req_path), (len as usize).min(PATH_UNITS)) else {
            return;
        };
        let mask = units(
            offset_of!(Config, req_mask),
            (mask_len as usize).min(MASK_UNITS),
        )
        .unwrap_or_default();
        let what = match op {
            OP_PATH => self.put_path(&path),
            OP_LIST => self.put_listing(&path, &mask),
            _ => Ok(false),
        };
        match what {
            Ok(put) if self.trace => eprintln!("vfs-inject: early: {op} {path} {mask} -> {put}"),
            Ok(_) => {}
            Err(e) => eprintln!("vfs-inject: early: {path}: {e}"),
        }
    }

    fn ring(&mut self) -> Result<&Ring, String> {
        if self.ring.is_none() {
            self.ring = Some(Ring::attach(&self.env));
        }
        self.ring.as_ref().unwrap().as_ref().map_err(Clone::clone)
    }

    /// The root and vpath of `nt` (`\??\C:\...`), as the wire names it.
    fn route(&self, nt: &str) -> Option<(RootId, String)> {
        let (root, vpath) = self.roots.resolve_vpath(nt)?;
        let vpath = if vpath.is_empty() {
            ".".to_string()
        } else {
            vpath.to_string()
        };
        Some((root, vpath))
    }

    /// Puts what the director has at `nt` on disk, if the disk has nothing
    /// there. Whether it put anything.
    fn put_path(&mut self, nt: &str) -> Result<bool, String> {
        let host = host_path(nt);
        if !self.seen.insert(aether_casefold::fold(nt)) || host.exists() {
            return Ok(false);
        }
        let Some((root, vpath)) = self.route(nt) else {
            return Ok(false);
        };
        let (found, is_dir) = self
            .ring()?
            .call(&Request::GetAttr { root, path: &vpath }, |r| match r {
                Reply::GetAttr(a) => Ok((a.found, a.is_dir)),
                _ => Err("GETATTR answered with another reply".into()),
            })?;
        if !found {
            return Ok(false);
        }
        if is_dir {
            std::fs::create_dir_all(&host).map_err(|e| format!("mkdir: {e}"))?;
        } else {
            self.copy(root, &vpath, &host)?;
        }
        Ok(true)
    }

    /// Puts the entries of the directory `nt` that match `mask` on disk,
    /// each that the disk does not have. Whether it put any.
    fn put_listing(&mut self, nt: &str, mask: &str) -> Result<bool, String> {
        let key = format!(
            "{}|{}",
            aether_casefold::fold(nt),
            aether_casefold::fold(mask)
        );
        if !self.seen.insert(key) {
            return Ok(false);
        }
        let Some((root, vpath)) = self.route(nt) else {
            return Ok(false);
        };
        let listed = self
            .ring()?
            .call(&Request::ReadDir { root, path: &vpath }, |r| match r {
                Reply::ReadDir(list) => Ok(list
                    .iter()
                    .map(|e| (e.name.to_string(), e.is_dir))
                    .collect::<Vec<_>>()),
                _ => Err("READDIR answered with another reply".into()),
            });
        let entries = match listed {
            Ok(e) => e,
            Err(CallFailed::Status(_)) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let host = host_path(nt);
        let mut put = false;
        for (name, is_dir) in entries {
            if !mask.is_empty() && !aether_redirect::wildcard_match(mask, &name) {
                continue;
            }
            let at = host.join(&name);
            if at.exists() {
                continue;
            }
            self.seen
                .insert(aether_casefold::fold(&format!("{nt}\\{name}")));
            if is_dir {
                std::fs::create_dir_all(&at).map_err(|e| format!("mkdir {name}: {e}"))?;
            } else {
                let v = if vpath == "." {
                    aether_casefold::fold(&name)
                } else {
                    format!("{vpath}/{}", aether_casefold::fold(&name))
                };
                self.copy(root, &v, &at)?;
            }
            put = true;
        }
        Ok(put)
    }

    /// Copies the file `vpath` under `root` to `host`, through a name beside
    /// it so that nothing reads it half written.
    fn copy(&mut self, root: RootId, vpath: &str, host: &Path) -> Result<(), String> {
        let ring = self.ring()?;
        let fh = ring.call(
            &Request::Open {
                root,
                flags: OpenFlags::READ,
                path: vpath,
            },
            |r| match r {
                Reply::Open(o) => Ok(o.fh),
                _ => Err("OPEN answered with another reply".into()),
            },
        )?;
        let result = (|| {
            if let Some(dir) = host.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
            }
            let tmp = host.with_file_name(format!(
                "{}.aether-early-{}",
                host.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                std::process::id()
            ));
            let mut out = std::fs::File::create(&tmp).map_err(|e| format!("create: {e}"))?;
            let chunk = ring.chunk();
            let mut offset = 0u64;
            loop {
                let read = Request::Read {
                    slot_flags: aether_protocol::RequestFlags::empty(),
                    fh,
                    offset,
                    len: chunk,
                };
                let n = ring.call(&read, |r| match r {
                    Reply::Read(ReadReply::Inline(data)) => {
                        std::io::Write::write_all(&mut out, data)
                            .map_err(|e| format!("write: {e}"))?;
                        Ok(data.len())
                    }
                    _ => Err("READ answered in bulk or with another reply".to_string()),
                })?;
                offset += n as u64;
                if n < chunk as usize {
                    break;
                }
            }
            drop(out);
            if host.exists() {
                let _ = std::fs::remove_file(&tmp);
                return Ok(());
            }
            std::fs::rename(&tmp, host).map_err(|e| format!("rename: {e}"))
        })();
        let _ = ring.call(&Request::Close { fh }, |_| Ok(()));
        result
    }
}

/// `\??\C:\x` as the injector opens it.
fn host_path(nt: &str) -> PathBuf {
    PathBuf::from(nt.strip_prefix(r"\??\").unwrap_or(nt))
}

/// Why a request failed.
#[derive(Debug)]
enum CallFailed {
    Status(aether_protocol::Status),
    Other(String),
}

impl From<CallFailed> for String {
    fn from(e: CallFailed) -> String {
        match e {
            CallFailed::Status(s) => format!("the director answered {s:?}"),
            CallFailed::Other(s) => s,
        }
    }
}

/// The session's ring, attached as the shim attaches it.
struct Ring {
    mapping: aether_win::SharedMapping,
    geom: Geom,
    server_ev: HANDLE,
}

/// The shim's way of waiting: spin, then yield, then sleep a millisecond.
struct Wake {
    server_ev: HANDLE,
}

impl Notifier for Wake {
    fn notify_server(&self) {
        if !self.server_ev.is_null() {
            // SAFETY: an event handle the ring owns.
            unsafe { SetEvent(self.server_ev) };
        }
    }
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn idle_client(&self, _slot: u32, waited: Duration) {
        if waited < Duration::from_millis(20) {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn notify_slot_free(&self) {
        self.notify_server();
    }
}

impl Ring {
    fn attach(env: &HashMap<String, String>) -> Result<Ring, String> {
        let bytes: usize = env
            .get(aether_env::RING_BYTES)
            .and_then(|s| s.parse().ok())
            .unwrap_or(2 * 1024 * 1024);
        let path = env.get(aether_env::RING_PATH).filter(|p| !p.is_empty());
        let (mapping, server_ev) = match (path, env.get(aether_env::RING_SECTION)) {
            (Some(p), _) => (
                aether_win::SharedMapping::open_file_backed(Path::new(p), bytes)
                    .map_err(|e| format!("open ring file {p}: {e}"))?,
                core::ptr::null_mut(),
            ),
            (None, Some(name)) => {
                let m = aether_win::SharedMapping::open(name, bytes)
                    .map_err(|e| format!("open ring section {name}: {e}"))?;
                let ev = env
                    .get(aether_env::SERVER_EV)
                    // SAFETY: FFI with a NUL-terminated name.
                    .map(|n| unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide(n).as_ptr()) })
                    .unwrap_or(core::ptr::null_mut());
                (m, ev)
            }
            (None, None) => return Err("no ring is named".into()),
        };
        let geom = ring::open(mapping.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        Ok(Ring {
            mapping,
            geom,
            server_ev,
        })
    }

    fn seg(&self) -> &SharedSeg {
        self.mapping.seg()
    }

    /// An inline READ's length: below 64 KiB, so that no director answers
    /// it in bulk (wire.md 3.4.4).
    fn chunk(&self) -> u32 {
        (self.geom.payload_cap - 8).min(65_535)
    }

    /// Sends `req` and hands its reply to `f`.
    fn call<T>(
        &self,
        req: &Request<'_>,
        f: impl FnOnce(Reply<'_>) -> Result<T, String>,
    ) -> Result<T, CallFailed> {
        let wake = Wake {
            server_ev: self.server_ev,
        };
        let client = Client::new(
            RingClient::with_geom(self.seg(), self.geom, wake)
                .with_deadline(Duration::from_secs(60)),
        );
        let mut scratch = Vec::new();
        match client.call(req, &mut scratch) {
            Ok(r) => f(r).map_err(CallFailed::Other),
            Err(aether_rpc::CallError::Status(s)) => Err(CallFailed::Status(s)),
            Err(e) => Err(CallFailed::Other(format!("{e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_exports_what_the_injector_patches_in_and_shares_its_config_layout() {
        let img = image(STUB).unwrap();
        for (_, hook, _) in HOOKS {
            img.export(hook).unwrap();
        }
        img.export("aether_early_start").unwrap();
        let at = img.export("aether_early").unwrap();
        assert_eq!(rd64(&img.bytes, at), MAGIC);
        let layout = rd32(&img.bytes, img.export("aether_early_layout").unwrap()) as usize;
        assert_eq!(
            layout,
            size_of::<Config>(),
            "the stub was built from another Config"
        );
    }

    #[test]
    fn an_environment_block_reads_as_names_and_values_drives_skipped() {
        let block: Vec<u16> = "=C:=C:\\x\0Path=a;b\0VFS_LAUNCH_IMAGE=C:\\s\\g.exe\0é=ü\0\0"
            .encode_utf16()
            .collect();
        let env = env_map(&block);
        assert_eq!(env.get("PATH").map(String::as_str), Some("a;b"));
        assert_eq!(
            env.get("VFS_LAUNCH_IMAGE").map(String::as_str),
            Some("C:\\s\\g.exe")
        );
        assert_eq!(env.get("=C:"), None);
        assert_eq!(env.get("é").map(String::as_str), Some("ü"));
    }

    #[test]
    fn the_staged_folders_are_the_images_folder_and_each_backing_once() {
        let env: HashMap<String, String> = [
            (aether_env::LAUNCH_IMAGE, r"C:\vfs\stage\game.exe"),
            (aether_env::ROOT_BACKINGS, r"0=C:\vfs\stage\;1=Z:\b\docs"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(
            staged_folders(&env),
            vec![
                r"\??\C:\VFS\STAGE".to_string(),
                r"\??\Z:\B\DOCS".to_string()
            ]
        );
        assert!(staged_folders(&HashMap::new()).is_empty());
    }
}
