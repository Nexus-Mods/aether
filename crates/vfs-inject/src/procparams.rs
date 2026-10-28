//! Telling a suspended target where it is: rewriting its command line and
//! current directory before any of its code runs.
//!
//! A target whose root location is purely virtual cannot be *started* there —
//! `CreateProcess` needs a real image and a real working directory — so it is
//! started from its root's backing directory and then, still suspended, has
//! its `RTL_USER_PROCESS_PARAMETERS` rewritten: `CommandLine` names the image
//! by its virtual path, and `CurrentDirectory.DosPath` is the virtual
//! directory. Both happen before kernelbase initialises in the target, so
//! `GetCommandLineW` and `GetCurrentDirectoryW` report them, and relative
//! paths resolve against the virtual directory. The current-directory
//! *handle* stays the real backing directory's, which the shim recognises as
//! its root's second spelling.
//!
//! `ImagePathName` is left alone: the loader takes the application directory
//! from it while it maps the static imports, which happens before any hook
//! exists and so must find real files. The shim renames it once the loader
//! is done (see the shim's bootstrap).
//!
//! The target has the injector's own bitness (the 32-bit injector starts
//! 32-bit programs), so its structures are laid out at this build's pointer
//! width.

use core::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualQueryEx, MEMORY_BASIC_INFORMATION};

/// `PEB.ProcessParameters`: after four bytes and three pointers.
const PEB_PROCESS_PARAMETERS: usize = 4 * size_of::<usize>();
/// `RTL_USER_PROCESS_PARAMETERS` fields.
const PP_FLAGS: usize = 0x08;
#[cfg(target_arch = "x86_64")]
const PP_CURDIR_DOS_PATH: usize = 0x38;
#[cfg(target_arch = "x86")]
const PP_CURDIR_DOS_PATH: usize = 0x24;
#[cfg(target_arch = "x86_64")]
const PP_COMMAND_LINE: usize = 0x70;
#[cfg(target_arch = "x86")]
const PP_COMMAND_LINE: usize = 0x40;
#[cfg(target_arch = "x86_64")]
const PP_ENVIRONMENT: usize = 0x80;
#[cfg(target_arch = "x86")]
const PP_ENVIRONMENT: usize = 0x48;
#[cfg(target_arch = "x86_64")]
const PP_ENVIRONMENT_SIZE: usize = 0x3F0;
#[cfg(target_arch = "x86")]
const PP_ENVIRONMENT_SIZE: usize = 0x290;
/// `RTL_USER_PROC_PARAMS_NORMALIZED`: the string buffers hold addresses, not
/// offsets from the block.
const PARAMS_NORMALIZED: u32 = 1;
/// The current directory's buffer is `MAX_PATH` characters: the runtime copies
/// a new directory into it in place (`RtlSetCurrentDirectory_U`), checking
/// against `MaximumLength`.
const CURDIR_BUFFER_BYTES: usize = 260 * 2;

/// `UNICODE_STRING`.
#[repr(C)]
#[derive(Default)]
struct RemoteUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: usize,
}

/// `PROCESS_BASIC_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct BasicInformation {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

type NtQueryInformationProcessFn =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

/// Rewrite the suspended `process`'s command line to `command_line` and its
/// current directory to `cwd` (a directory; a trailing `\` is added), each
/// only when given.
///
/// # Safety
/// `process` must be a suspended process this process created, with
/// `PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_VM_OPERATION |
/// PROCESS_QUERY_INFORMATION` (a `CreateProcessW` handle has all of them).
#[allow(unsafe_code)]
pub unsafe fn rewrite(
    process: HANDLE,
    command_line: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), String> {
    if command_line.is_none() && cwd.is_none() {
        return Ok(());
    }
    let params = process_parameters(process)?;
    let flags: u32 = read(process, params + PP_FLAGS)?;
    if flags & PARAMS_NORMALIZED == 0 {
        return Err("the target's process parameters are not normalized".into());
    }
    if let Some(cmd) = command_line {
        let units: Vec<u16> = cmd.encode_utf16().collect();
        put_string(
            process,
            params,
            params + PP_COMMAND_LINE,
            &units,
            units.len() * 2 + 2,
        )?;
    }
    if let Some(dir) = cwd {
        let mut dir = dir.trim_end_matches(['\\', '/']).to_string();
        dir.push('\\');
        let units: Vec<u16> = dir.encode_utf16().collect();
        if units.len() * 2 + 2 > CURDIR_BUFFER_BYTES {
            return Err(format!(
                "the virtual working directory {dir:?} is longer than MAX_PATH"
            ));
        }
        put_string(
            process,
            params,
            params + PP_CURDIR_DOS_PATH,
            &units,
            CURDIR_BUFFER_BYTES,
        )?;
    }
    Ok(())
}

/// The base address of `process`'s PEB.
///
/// # Safety
/// `process` is a live process handle with `PROCESS_QUERY_INFORMATION`.
#[allow(unsafe_code)]
pub(crate) unsafe fn peb_base(process: HANDLE) -> Result<u64, String> {
    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    let query = GetProcAddress(ntdll, c"NtQueryInformationProcess".as_ptr().cast())
        .ok_or("ntdll has no NtQueryInformationProcess")?;
    let query: NtQueryInformationProcessFn = core::mem::transmute(query);
    let mut info = BasicInformation::default();
    let st = query(
        process,
        0, // ProcessBasicInformation
        (&mut info as *mut BasicInformation).cast(),
        size_of::<BasicInformation>() as u32,
        core::ptr::null_mut(),
    );
    if st < 0 || info.peb_base_address == 0 {
        return Err(format!("NtQueryInformationProcess: {st:#x}"));
    }
    Ok(info.peb_base_address as u64)
}

/// The address of `process`'s `RTL_USER_PROCESS_PARAMETERS`.
#[allow(unsafe_code)]
unsafe fn process_parameters(process: HANDLE) -> Result<usize, String> {
    let peb = peb_base(process)? as usize;
    let params: usize = read(process, peb + PEB_PROCESS_PARAMETERS)?;
    if params == 0 {
        return Err("the target has no process parameters yet".into());
    }
    Ok(params)
}

/// Make the `UNICODE_STRING` at `at` (a field of the parameter block at
/// `params`) say `units`, with room for at least `capacity` bytes.
///
/// The new text stays **inside the parameter block**: process start refuses
/// a block whose strings point elsewhere (a command line in its own
/// allocation fails kernelbase's initialisation with
/// `STATUS_DLL_INIT_FAILED`; a working directory there faults). So it goes in
/// the string's own buffer when that is big enough, and otherwise after the
/// block's last byte, in the slack of the block's own allocation, with the
/// block's `Length` grown to cover it.
///
/// That slack is not necessarily free: the kernel lays a new process's
/// **environment** out directly after its parameter block, in the same
/// allocation. A string placed there overwrote the environment's first
/// entries, and when its terminating NUL fell just before an entry's own the
/// block ended there, hiding every later variable — `VFS_SHIM_CONFIG` among
/// them, so the shim's `DllMain` failed and the target died with
/// `STATUS_DLL_INIT_FAILED` before it ran, for some lengths of path and some
/// environments only. So an environment in the way is moved first, to just
/// after the grown block, keeping the kernel's own layout.
#[allow(unsafe_code)]
unsafe fn put_string(
    process: HANDLE,
    params: usize,
    at: usize,
    units: &[u16],
    capacity: usize,
) -> Result<(), String> {
    let need = units.len() * 2 + 2;
    let capacity = capacity.max(need);
    let mut text = units.to_vec();
    text.push(0);
    let old: RemoteUnicodeString = read(process, at)?;
    if old.buffer != 0 && old.maximum_length as usize >= capacity {
        write_bytes(process, old.buffer, text.as_ptr().cast(), need)?;
        let len = (units.len() * 2) as u16;
        return write_bytes(process, at, (&len as *const u16).cast(), 2);
    }
    if capacity > u16::MAX as usize {
        return Err("a process parameter longer than a UNICODE_STRING holds".into());
    }
    // The block: MaximumLength (u32 @ 0), Length (u32 @ 4).
    let max_len: u32 = read(process, params)?;
    let used: u32 = read(process, params + 4)?;
    let place = (params + used as usize + 7) & !7;
    let end = place + capacity;
    let mut mbi: MEMORY_BASIC_INFORMATION = core::mem::zeroed();
    let got = VirtualQueryEx(
        process,
        params as *const c_void,
        &mut mbi,
        size_of::<MEMORY_BASIC_INFORMATION>(),
    );
    let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
    let no_room =
        || format!("no room in the target's parameter block for a {capacity}-byte string");
    if got == 0 || end > region_end {
        return Err(no_room());
    }
    // An environment that starts where the string goes moves past its end.
    let env: usize = read(process, params + PP_ENVIRONMENT)?;
    if env >= params && env < end {
        let block = read_environment(process, params, env, region_end)?;
        let moved = (end + 7) & !7;
        if moved + block.len() > region_end {
            return Err(no_room());
        }
        write_bytes(process, moved, block.as_ptr().cast(), block.len())?;
        write_bytes(
            process,
            params + PP_ENVIRONMENT,
            (&moved as *const usize).cast(),
            size_of::<usize>(),
        )?;
    }
    write_bytes(process, place, text.as_ptr().cast(), need)?;
    let s = RemoteUnicodeString {
        length: (units.len() * 2) as u16,
        maximum_length: capacity as u16,
        buffer: place,
    };
    // Only the string itself: a `CURDIR`'s handle follows it.
    write_bytes(
        process,
        at,
        (&s as *const RemoteUnicodeString).cast(),
        size_of::<RemoteUnicodeString>(),
    )?;
    let used = (end - params) as u32;
    write_bytes(process, params + 4, (&used as *const u32).cast(), 4)?;
    let max_len = max_len.max(used);
    write_bytes(process, params, (&max_len as *const u32).cast(), 4)
}

/// The bytes of the environment block at `env`, which ends before
/// `region_end`: its entries through the empty one that ends them, or its
/// `EnvironmentSize` when that says more and still fits.
#[allow(unsafe_code)]
unsafe fn read_environment(
    process: HANDLE,
    params: usize,
    env: usize,
    region_end: usize,
) -> Result<Vec<u8>, String> {
    // The block ends at its first empty entry: a NUL at its start, or two
    // in a row. An empty block is two NULs all the same.
    let mut bytes: Vec<u8> = Vec::new();
    let len = loop {
        let from = env + bytes.len();
        let chunk = (region_end.saturating_sub(from) & !1).min(4096);
        if chunk == 0 {
            return Err("the target's environment has no end inside its allocation".into());
        }
        bytes.extend(read_bytes(process, from, chunk)?);
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        let end = units
            .iter()
            .enumerate()
            .position(|(i, &u)| u == 0 && (i == 0 || units[i - 1] == 0));
        if let Some(i) = end {
            break (i.max(1) + 1) * 2;
        }
    };
    let size: usize = read(process, params + PP_ENVIRONMENT_SIZE)?;
    if size > len && env + size <= region_end {
        return read_bytes(process, env, size);
    }
    bytes.truncate(len);
    Ok(bytes)
}

#[allow(unsafe_code)]
unsafe fn read_bytes(process: HANDLE, at: usize, len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; len];
    let mut got = 0usize;
    let ok = ReadProcessMemory(
        process,
        at as *const c_void,
        bytes.as_mut_ptr().cast(),
        len,
        &mut got,
    );
    if ok == 0 || got != len {
        return Err(format!("ReadProcessMemory at {at:#x}"));
    }
    Ok(bytes)
}

#[allow(unsafe_code)]
unsafe fn read<T: Default>(process: HANDLE, at: usize) -> Result<T, String> {
    let mut v = T::default();
    let mut got = 0usize;
    let ok = ReadProcessMemory(
        process,
        at as *const c_void,
        (&mut v as *mut T).cast(),
        size_of::<T>(),
        &mut got,
    );
    if ok == 0 || got != size_of::<T>() {
        return Err(format!("ReadProcessMemory at {at:#x}"));
    }
    Ok(v)
}

#[allow(unsafe_code)]
unsafe fn write_bytes(
    process: HANDLE,
    at: usize,
    src: *const c_void,
    len: usize,
) -> Result<(), String> {
    let mut put = 0usize;
    let ok = WriteProcessMemory(process, at as *const c_void, src, len, &mut put);
    if ok == 0 || put != len {
        return Err(format!("WriteProcessMemory at {at:#x}"));
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, TerminateProcess, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    /// A suspended `cmd.exe` started with `env` (entries, without the final
    /// empty one), killed when dropped.
    struct Suspended(PROCESS_INFORMATION);

    impl Suspended {
        fn start(env: &[String]) -> Self {
            let mut block: Vec<u16> = Vec::new();
            for e in env {
                block.extend(e.encode_utf16());
                block.push(0);
            }
            block.push(0);
            let exe = format!(
                r"{}\System32\cmd.exe",
                std::env::var("SystemRoot").expect("SystemRoot")
            );
            let app = wide(&exe);
            let mut cmd = wide(&format!("\"{exe}\""));
            // SAFETY: NUL-terminated buffers that outlive the call.
            unsafe {
                let mut si: STARTUPINFOW = zeroed();
                si.cb = size_of::<STARTUPINFOW>() as u32;
                let mut pi: PROCESS_INFORMATION = zeroed();
                let ok = CreateProcessW(
                    app.as_ptr(),
                    cmd.as_mut_ptr(),
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                    block.as_ptr().cast(),
                    core::ptr::null(),
                    &si,
                    &mut pi,
                );
                assert!(ok != 0, "CreateProcessW {exe}");
                Suspended(pi)
            }
        }

        /// The environment the target will start with: the entries of the
        /// block its parameters name.
        fn environment(&self) -> Vec<String> {
            // SAFETY: a live suspended process this test created.
            unsafe {
                let params = process_parameters(self.0.hProcess).unwrap();
                let mut at: usize = read(self.0.hProcess, params + PP_ENVIRONMENT).unwrap();
                let mut entries = Vec::new();
                let mut entry: Vec<u16> = Vec::new();
                loop {
                    let unit: u16 = read(self.0.hProcess, at).unwrap();
                    at += 2;
                    if unit != 0 {
                        entry.push(unit);
                        continue;
                    }
                    if entry.is_empty() {
                        return entries;
                    }
                    entries.push(String::from_utf16_lossy(&entry));
                    entry.clear();
                }
            }
        }

        /// The target's command line, and whether its text lies inside the
        /// parameter block (`params .. params + Length`).
        fn command_line(&self) -> (String, bool) {
            // SAFETY: as `environment`.
            unsafe {
                let params = process_parameters(self.0.hProcess).unwrap();
                let s: RemoteUnicodeString =
                    read(self.0.hProcess, params + PP_COMMAND_LINE).unwrap();
                let units: Vec<u16> = (0..s.length as usize / 2)
                    .map(|i| read(self.0.hProcess, s.buffer + 2 * i).unwrap())
                    .collect();
                let used: u32 = read(self.0.hProcess, params + 4).unwrap();
                let inside = s.buffer >= params
                    && s.buffer + s.maximum_length as usize <= params + used as usize;
                (String::from_utf16_lossy(&units), inside)
            }
        }
    }

    impl Drop for Suspended {
        fn drop(&mut self) {
            // SAFETY: the handles this test's `CreateProcessW` returned.
            unsafe {
                TerminateProcess(self.0.hProcess, 1);
                CloseHandle(self.0.hThread);
                CloseHandle(self.0.hProcess);
            }
        }
    }

    /// The kernel lays a new process's environment out directly after its
    /// parameter block, in the same allocation. A command line too long for its
    /// own buffer is placed after the block — which must not be on top of the
    /// environment. It was: the first entries were overwritten, and when the
    /// command line's terminating NUL landed just before an entry's own, the
    /// block ended there, hiding every later variable (`VFS_SHIM_CONFIG` among
    /// them), so the shim's `DllMain` failed and the target died with
    /// `STATUS_DLL_INIT_FAILED` (0xC0000142) before it ran.
    #[test]
    fn a_relocated_command_line_leaves_the_environment_whole() {
        let mut env: Vec<String> = vec![
            format!("AAA={}", "a".repeat(37)),
            "AAB=b".to_string(),
            format!("AAC={}", "c".repeat(11)),
        ];
        env.extend((0..40).map(|i| format!("PAD{i:02}={}", "p".repeat(i))));
        env.push(format!(
            "SystemRoot={}",
            std::env::var("SystemRoot").expect("SystemRoot")
        ));
        env.push("ZZZ_LAST=still here".to_string());
        // Every length of command line from barely-too-long to a long one: each
        // ends at a different place, so one of them lands its NUL against an
        // entry's.
        for extra in 0..48 {
            let target = Suspended::start(&env);
            let cmd = format!(r#""C:\virtual\{}\game.exe" -arg"#, "d".repeat(60 + extra));
            // SAFETY: a suspended process this test created.
            unsafe { rewrite(target.0.hProcess, Some(&cmd), Some(r"C:\virtual\dir")) }
                .expect("rewrite");
            assert_eq!(
                target.environment(),
                env,
                "the environment after a {}-unit command line",
                cmd.len()
            );
            let (got, inside) = target.command_line();
            assert_eq!(got, cmd);
            assert!(inside, "the command line lies inside the parameter block");
        }
    }
}
