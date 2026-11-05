//! This process's PEB and the fields of it the shim reads, at their offsets
//! for the build's pointer width: x64, and x86 (a 32-bit program, under
//! WOW64 on Windows or Wine).
//!
//! The PEB is reached through the thread's segment register: `gs:[0x60]` on
//! x64, `fs:[0x30]` on x86. Never `gs` on x86: under Wine's WOW64 mode `gs`
//! is not the TEB, and a read through it faults.

/// `PEB.Ldr`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_LDR: usize = 0x18;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_LDR: usize = 0x0c;

/// `PEB.ProcessParameters`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_PROCESS_PARAMETERS: usize = 0x20;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_PROCESS_PARAMETERS: usize = 0x10;

/// `PEB.LoaderLock`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_LOADER_LOCK: usize = 0x110;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_LOADER_LOCK: usize = 0xa0;

/// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory`: `{ UNICODE_STRING DosPath; HANDLE Handle }`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PP_CURRENT_DIRECTORY: usize = 0x38;
#[cfg(target_arch = "x86")]
pub(crate) const PP_CURRENT_DIRECTORY: usize = 0x24;

/// `RTL_USER_PROCESS_PARAMETERS.ImagePathName`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PP_IMAGE_PATH_NAME: usize = 0x60;
#[cfg(target_arch = "x86")]
pub(crate) const PP_IMAGE_PATH_NAME: usize = 0x38;

/// `PEB_LDR_DATA.InLoadOrderModuleList`.
#[cfg(target_arch = "x86_64")]
pub(crate) const LDR_IN_LOAD_ORDER: usize = 0x10;
#[cfg(target_arch = "x86")]
pub(crate) const LDR_IN_LOAD_ORDER: usize = 0x0c;

/// `LDR_DATA_TABLE_ENTRY.FullDllName`.
#[cfg(target_arch = "x86_64")]
pub(crate) const ENTRY_FULL_DLL_NAME: usize = 0x48;
#[cfg(target_arch = "x86")]
pub(crate) const ENTRY_FULL_DLL_NAME: usize = 0x24;

/// A `UNICODE_STRING`'s `Buffer`: after the two `u16` lengths, at the pointer's alignment.
pub(crate) const US_BUFFER: usize = core::mem::size_of::<usize>();

/// This process's PEB.
pub(crate) fn peb() -> usize {
    let peb: usize;
    // SAFETY: reads the current thread's TEB field through its segment
    // register; no memory is written.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly, preserves_flags))
    };
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::asm!("mov {}, fs:[0x30]", out(reg) peb, options(nostack, readonly, preserves_flags))
    };
    peb
}

/// This process's `RTL_USER_PROCESS_PARAMETERS`, or 0.
pub(crate) fn process_parameters() -> usize {
    let peb = peb();
    if peb == 0 {
        return 0;
    }
    // SAFETY: a field of this process's own PEB, which lives as long as it.
    unsafe { *((peb + PEB_PROCESS_PARAMETERS) as *const usize) }
}

/// How much of this thread's stack is committed: `NT_TIB.StackBase` less
/// `NT_TIB.StackLimit`, which the guard page moves down as the stack grows,
/// so the deepest the thread has been (and a page or two).
#[cfg(feature = "test-support")]
pub(crate) fn stack_committed() -> usize {
    let (base, limit): (usize, usize);
    // SAFETY: reads two fields of the current thread's TEB through its
    // segment register; no memory is written.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, gs:[0x08]", "mov {}, gs:[0x10]", out(reg) base, out(reg) limit, options(nostack, readonly, preserves_flags))
    };
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::asm!("mov {}, fs:[0x04]", "mov {}, fs:[0x08]", out(reg) base, out(reg) limit, options(nostack, readonly, preserves_flags))
    };
    base - limit
}

/// An ntdll export, looked up once.
fn ntdll_export(name: &core::ffi::CStr) -> Option<unsafe extern "system" fn() -> isize> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is loaded in every process; a NUL-terminated name.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        GetProcAddress(ntdll, name.as_ptr().cast())
    }
}

/// Whether this thread holds the loader lock (`PEB.LoaderLock`). A thread
/// started meanwhile cannot run until it is released (its
/// `DLL_THREAD_ATTACH` waits for the lock), so the holder must not wait for
/// one.
pub(crate) fn holds_loader_lock() -> bool {
    type Locked = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;
    static F: std::sync::OnceLock<Option<Locked>> = std::sync::OnceLock::new();
    let f = F.get_or_init(|| {
        ntdll_export(c"RtlIsCriticalSectionLockedByThread").map(|p| {
            // SAFETY: the export's documented signature.
            unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, Locked>(p) }
        })
    });
    let Some(f) = f else {
        return false;
    };
    let peb = peb();
    if peb == 0 {
        return false;
    }
    // SAFETY: the PEB's `LoaderLock` is the loader's critical section; both
    // are valid for the life of the process.
    unsafe {
        let lock = *((peb + PEB_LOADER_LOCK) as *const *mut core::ffi::c_void);
        !lock.is_null() && f(lock) != 0
    }
}

/// Whether the process is ending: `LdrShutdownProcess` has begun
/// (`RtlDllShutdownInProgress`). `ExitProcess` has then ended every other
/// thread, and this one, holding the loader lock, runs the DLLs' detach (a
/// DLL's CRT runs its `atexit` handlers there). No other thread will answer
/// anything this one waits for.
pub(crate) fn process_ending() -> bool {
    type InProgress = unsafe extern "system" fn() -> u8;
    static F: std::sync::OnceLock<Option<InProgress>> = std::sync::OnceLock::new();
    let f = F.get_or_init(|| {
        ntdll_export(c"RtlDllShutdownInProgress").map(|p| {
            // SAFETY: the export's documented signature, `BOOLEAN (void)`.
            unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, InProgress>(p) }
        })
    });
    // SAFETY: takes no arguments; reads a loader flag.
    f.is_some_and(|f| unsafe { f() } != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curdir_handle_follows_its_dos_path() {
        // CURDIR = { UNICODE_STRING DosPath; HANDLE Handle }: the handle two pointers on
        let handle = PP_CURRENT_DIRECTORY + 2 * core::mem::size_of::<usize>();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(0x48, handle);
        #[cfg(target_arch = "x86")]
        assert_eq!(0x2c, handle);
    }
}
