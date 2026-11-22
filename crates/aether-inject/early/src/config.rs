//! The early layer's shared block: the stub (`early/src/lib.rs`) holds it as
//! its exported `aether_early`, and the injector (`src/early.rs`) fills it in
//! before the target runs and reads the request fields while a hook waits.
//! One definition, included by both (`core` only).

/// The exports hooked, in [`Config::sites`] order.
pub const SITE_CREATE: usize = 0;
pub const SITE_OPEN: usize = 1;
pub const SITE_ATTRS: usize = 2;
pub const SITE_FULL_ATTRS: usize = 3;
pub const SITE_DIR: usize = 4;
pub const SITE_DIR_EX: usize = 5;
pub const SITES: usize = 6;

pub const PREFIXES: usize = 8;
pub const PATH_UNITS: usize = 1024;
pub const MASK_UNITS: usize = 260;
pub const DIRS: usize = 16;

/// What the injector is asked: the name of an open or a query, or a listing.
pub const OP_PATH: u32 = 1;
pub const OP_LIST: u32 = 2;

/// `"AETHEARL"`.
pub const MAGIC: u64 = 0x4C52_4145_4854_4541;
pub const VERSION: u32 = 2;

/// Room for the shim's path, NUL included.
pub const SHIM_UNITS: usize = 520;

/// [`Config::shim_state`].
pub const SHIM_NOT_LOADED: u32 = 0;
pub const SHIM_LOADING: u32 = 1;
pub const SHIM_LOADED_EARLY: u32 = 2;
pub const SHIM_EARLY_FAILED: u32 = 3;

#[repr(C)]
pub struct Site {
    /// The export's address, 0 for one not hooked.
    pub target: u64,
    /// Its first 16 bytes as they were, put back by `aether_early_start`.
    pub orig: [u8; 16],
}

#[repr(C)]
pub struct Prefix {
    pub len: u32,
    pub _pad: u32,
    pub units: [u16; PATH_UNITS],
}

/// A directory opened under a prefix, by handle, so that a listing of it is
/// asked for before the real query reads it.
#[repr(C)]
pub struct Dir {
    pub handle: u64,
    pub listed: u32,
    pub len: u32,
    pub units: [u16; PATH_UNITS],
}

/// A copy of each export's syscall thunk (position-independent on every
/// Windows and Wine ntdll, x64 and 32-bit WOW64), which the hooks call as
/// the original.
#[repr(C, align(16))]
pub struct Code(pub [[u8; 32]; SITES]);

#[repr(C)]
pub struct Config {
    pub magic: u64,
    pub version: u32,
    /// 1 from `aether_early_start` on: every hook passes straight through.
    pub taken_over: u32,
    pub lock: u32,
    pub nprefix: u32,
    pub req_event: u64,
    pub done_event: u64,
    pub nt_set_event: u64,
    pub nt_wait: u64,
    pub nt_protect: u64,
    pub load_library_w: u64,
    pub sites: [Site; SITES],
    pub code: Code,
    /// Upper-cased NT names (`\??\C:\STAGE`), no trailing separator.
    pub prefixes: [Prefix; PREFIXES],
    pub req_op: u32,
    pub req_len: u32,
    pub req_mask_len: u32,
    /// Counts requests, for diagnostics.
    pub req_seq: u32,
    pub req_path: [u16; PATH_UNITS],
    pub req_mask: [u16; MASK_UNITS],
    pub dirs: [Dir; DIRS],
    pub dir_next: u32,
    pub _pad: u32,
    /// 1: load the shim from the first hook that fires once kernel32 has
    /// initialised, before any other DLL's `DllMain` can call it; 0: only
    /// from the remote thread's start, after the process's initialisation.
    pub early_shim: u32,
    /// `SHIM_*`: whether a hook loaded the shim, and how that went.
    pub shim_state: u32,
    /// The shim's path, NUL-terminated.
    pub shim_path: [u16; SHIM_UNITS],
}

impl Config {
    /// All zero but its magic and version: the stub's initial value.
    pub const fn new() -> Config {
        // SAFETY: every field is an integer or an array of them.
        let mut c: Config = unsafe { core::mem::zeroed() };
        c.magic = MAGIC;
        c.version = VERSION;
        c
    }
}
