//! Stand-in for a game's statically-imported d3d/dxgi proxy DLL.
//! The BACKING copy returns 4242 so the harness can prove the import bound
//! to our redirected image rather than any on-disk file in the app dir.
#![allow(unsafe_code)]

#[no_mangle]
pub extern "C" fn vproxy_value() -> i32 {
    4242
}

#[repr(C)]
struct FindData {
    attributes: u32,
    times: [u32; 6],
    size_high: u32,
    size_low: u32,
    reserved: [u32; 2],
    name: [u16; 260],
    alternate: [u16; 14],
    // `WIN32_FIND_DATAW`'s trailing fields on some SDKs; unused.
    _tail: [u32; 3],
}

#[link(name = "kernel32")]
extern "system" {
    fn FindFirstFileW(pattern: *const u16, data: *mut FindData) -> isize;
    fn FindNextFileW(find: isize, data: *mut FindData) -> i32;
    fn FindClose(find: isize) -> i32;
}

/// Whether `pattern` (relative to `dir`, wildcards in its last component)
/// matches at least one file, each of which reads whole: what an ASI loader
/// does with `*.asi` while its `DllMain` runs.
fn listing_reads(dir: &std::path::Path, pattern: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let full = dir.join(pattern);
    let wide: Vec<u16> = full.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data: FindData = unsafe { core::mem::zeroed() };
    // SAFETY: a NUL-terminated pattern and a FindData the call fills.
    let find = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if find == -1 {
        return false;
    }
    let mut files = 0;
    let mut ok = true;
    loop {
        if data.attributes & 0x10 == 0 {
            let len = data.name.iter().position(|&u| u == 0).unwrap_or(260);
            let name = std::ffi::OsString::from_wide(&data.name[..len]);
            let at = full.parent().unwrap_or(dir).join(name);
            let size = (u64::from(data.size_high) << 32) | u64::from(data.size_low);
            ok &= std::fs::read(&at).is_ok_and(|b| b.len() as u64 == size);
            files += 1;
        }
        // SAFETY: the live find handle.
        if unsafe { FindNextFileW(find, &mut data) } == 0 {
            break;
        }
    }
    // SAFETY: closed once.
    unsafe { FindClose(find) };
    ok && files > 0
}

/// With `VFS_FIXTURE_VPROXY_NEEDS` naming a file, the load fails (`FALSE`)
/// unless that file is beside the program — the way a game's proxy DLL (ENB's
/// `d3d11.dll` reading `enblocal.ini`) needs the game's files when the loader
/// initialises it, which is before the program's own code runs. A program
/// whose files are only in the VFS passes only if the shim is up by then.
/// `VFS_FIXTURE_VPROXY_LIST` does the same for a listing (`plugins\*.asi`),
/// and `VFS_FIXTURE_VPROXY_WRITES` writes a file there, as a proxy opens its
/// log.
#[no_mangle]
pub extern "system" fn DllMain(
    _module: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        if let Some(need) = std::env::var_os("VFS_FIXTURE_VPROXY_NEEDS") {
            let here = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join(&need)));
            if !here.is_some_and(|p| p.is_file()) {
                return 0;
            }
        }
        if let Some(name) = std::env::var_os("VFS_FIXTURE_VPROXY_WRITES") {
            let at = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join(&name)));
            if !at.is_some_and(|p| std::fs::write(p, "written in DllMain").is_ok()) {
                return 0;
            }
        }
        if let Some(pattern) = std::env::var_os("VFS_FIXTURE_VPROXY_LIST") {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.to_path_buf()));
            if !dir.is_some_and(|d| listing_reads(&d, &pattern)) {
                return 0;
            }
        }
    }
    1
}
