//! Pre-init injection fixture: statically imports vproxy.dll. Reaching main
//! means the loader bound that import during process init. Exit 0 iff the
//! backing export value (4242) is observed.
#![allow(unsafe_code)]

// Force a PE import of vproxy.dll (not a static link of the rlib). Combined
// with build.rs link-search for the import library.
#[cfg(windows)]
#[link(name = "vproxy.dll", kind = "dylib")]
extern "C" {
    fn vproxy_value() -> i32;
}

// There is deliberately no `use vproxy` here: that would require the package to
// also emit an rlib, and a cdylib+rlib package builds as two units that both
// write `vproxy.dll` (cargo#6313) — the collision that made `--all-targets`
// unusable.
//
// The cost of dodging that collision is that **nothing orders this link**. With
// no Cargo edge to aether-fixture-vproxy, cargo is free to link this crate before
// the cdylib that produces `vproxy.dll.lib` exists, and on a clean target dir it
// does: LNK1181. Whoever builds this crate has to build aether-fixture-vproxy
// first, in an earlier cargo invocation: CI does it in the "Build inject
// artifacts" step, and `aether-inject`'s `tests/common` does it at test
// runtime. build.rs warns if the lib is missing, because the linker's own error
// names a file and not a cause.
//
// There is no Cargo dependency on aether-fixture-vproxy, on purpose. A developer
// tree usually has the import lib lying around, so a missing build order shows
// only on a clean target dir, such as CI's.

#[cfg(not(windows))]
fn main() {
    eprintln!("aether-staticimp is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

#[cfg(windows)]
fn main() {
    let v = unsafe { vproxy_value() };
    // `VFS_FIXTURE_PROBE_OUT`: also write where the loader says the import came from
    // (`vproxy <GetModuleFileNameW of vproxy.dll>`), for a launch at a virtual location.
    if let Some(out) = std::env::var_os("VFS_FIXTURE_PROBE_OUT") {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
        let name: Vec<u16> = "vproxy.dll\0".encode_utf16().collect();
        let mut buf = [0u16; 1024];
        // SAFETY: a NUL-terminated name; a module handle (or null) and a buffer of its length.
        let n = unsafe {
            GetModuleFileNameW(
                GetModuleHandleW(name.as_ptr()),
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        let _ = std::fs::write(
            out,
            format!("vproxy {}\n", String::from_utf16_lossy(&buf[..n])),
        );
    }
    let line = format!("vproxy_value={v}\n");
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.get(1) {
        let _ = std::fs::write(path, line.as_bytes());
    } else {
        print!("{line}");
    }
    std::process::exit(if v == 4242 { 0 } else { 3 });
}
