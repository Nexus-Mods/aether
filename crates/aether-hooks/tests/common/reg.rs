//! The registry tests' shared fixture pieces: NT key calls, scratch real keys, the unhooked
//! checker process, and the director plus hooks every registry test runs against.
//!
//! A test file keeps only what is its own: its `BASE` key (a [`Base`]), the real keys it
//! makes, and the extras its `Fixture` carries. Declare this module in the test binary as
//! `#[path = "../common/reg.rs"] mod reg;` (the checker is then `reg::reg_checker`, see
//! [`CHECKER_TEST`]).

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{LocalFree, FILETIME, HANDLE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetKeySecurity, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ,
    REG_OPTION_NON_VOLATILE,
};

use super::fakedirector::{self, Fake};
use aether_hooks::{install, regclient};

pub(crate) const STATUS_SUCCESS: i32 = 0;
pub(crate) const OBJ_CASE_INSENSITIVE: u32 = 0x40;
pub(crate) const OBJECT_NAME_INFORMATION: u32 = 1;

/// Marks the checker process (the same test binary running [`reg_checker`]).
const CHECKER_ENV: &str = "AETHER_VFS_REG_CHECKER";
/// The libtest name of [`reg_checker`] when this file is `mod reg;` at a binary's root.
const CHECKER_TEST: &str = "reg::reg_checker";

#[repr(C)]
pub(crate) struct UnicodeString {
    pub(crate) length: u16,
    pub(crate) maximum_length: u16,
    pub(crate) buffer: *const u16,
}

#[repr(C)]
pub(crate) struct ObjectAttributes {
    pub(crate) length: u32,
    pub(crate) root_directory: isize,
    pub(crate) object_name: *const UnicodeString,
    pub(crate) attributes: u32,
    pub(crate) security_descriptor: *const c_void,
    pub(crate) security_qos: *const c_void,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtOpenKeyEx(key: *mut isize, access: u32, oa: *const ObjectAttributes, options: u32) -> i32;
    fn NtClose(h: isize) -> i32;
    fn NtQueryObject(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `f` with a `UNICODE_STRING` for `s`.
pub(crate) fn with_us<R>(s: &str, f: impl FnOnce(*const UnicodeString) -> R) -> R {
    let w: Vec<u16> = s.encode_utf16().collect();
    let us = UnicodeString {
        length: (w.len() * 2) as u16,
        maximum_length: (w.len() * 2) as u16,
        buffer: w.as_ptr(),
    };
    f(&us)
}

/// `f` with `OBJECT_ATTRIBUTES` naming `name` relative to `root` (0: absolute).
pub(crate) fn with_oa<R>(
    root: isize,
    name: &str,
    f: impl FnOnce(*const ObjectAttributes) -> R,
) -> R {
    with_us(name, |us| {
        let oa = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: root,
            object_name: us,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null(),
            security_qos: std::ptr::null(),
        };
        f(&oa)
    })
}

/// `NtOpenKeyEx` of the absolute NT name `name`.
pub(crate) fn open_abs(name: &str, access: u32) -> (i32, isize) {
    let mut h = 0isize;
    let st = with_oa(0, name, |oa| unsafe { NtOpenKeyEx(&mut h, access, oa, 0) });
    (st, h)
}

pub(crate) fn close(h: isize) -> i32 {
    unsafe { NtClose(h) }
}

/// `NtQueryObject` for a class whose answer starts with a `UNICODE_STRING`.
pub(crate) fn object_string(h: isize, class: u32) -> Result<String, i32> {
    let mut buf = vec![0u64; 512];
    let mut ret = 0u32;
    let st = unsafe { NtQueryObject(h, class, buf.as_mut_ptr().cast(), 4096, &mut ret) };
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    let s = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
    Ok(String::from_utf16_lossy(s))
}

/// `NtQueryObject(ObjectNameInformation)`.
pub(crate) fn object_name(h: isize) -> Result<String, i32> {
    object_string(h, OBJECT_NAME_INFORMATION)
}

/// Create (or open) `HKCU\<sub>` with `class`, KEY_ALL_ACCESS.
pub(crate) fn reg_create_class(sub: &str, class: Option<&str>) -> HKEY {
    let mut k: HKEY = std::ptr::null_mut();
    let class_w = class.map(wide);
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(sub).as_ptr(),
            0,
            class_w.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut k,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(st, 0, "RegCreateKeyExW {sub}");
    k
}

pub(crate) fn reg_create(sub: &str) -> HKEY {
    reg_create_class(sub, None)
}

/// The current user's SID as a string.
pub(crate) fn user_sid() -> String {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let mut buf = vec![0u64; 64];
        let mut need = 0;
        assert_ne!(
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), 512, &mut need),
            0
        );
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        assert_ne!(ConvertSidToStringSidW(tu.User.Sid, &mut s), 0);
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s.cast());
        out
    }
}

/// Apply the SDDL string `sddl` to the open key `k`.
unsafe fn apply_sddl(k: HKEY, sddl: &str) {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    let mut sd: *mut c_void = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        },
        0
    );
    assert_eq!(
        unsafe { RegSetKeySecurity(k, DACL_SECURITY_INFORMATION, sd) },
        0
    );
    unsafe { LocalFree(sd) };
}

/// Set `HKCU\<sub>`'s DACL from SDDL, creating the key if need be.
pub(crate) fn set_dacl(sub: &str, sddl: &str) {
    let k = reg_create(sub);
    unsafe {
        apply_sddl(k, sddl);
        RegCloseKey(k);
    }
}

/// Give back everyone full access to `HKCU\<sub>` (so it can be deleted), if it exists.
fn unlock(sub: &str) {
    const WRITE_DAC: u32 = 0x4_0000;
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(sub).as_ptr(), 0, WRITE_DAC, &mut k) };
    if st == 0 {
        unsafe {
            apply_sddl(k, "D:(A;;GA;;;WD)");
            RegCloseKey(k);
        }
    }
}

/// A test file's scratch key under `HKCU\Software`: `<name>-<pid>`, so two test processes
/// that run at once (two `cargo test` runs on one machine, each test in a process of its own)
/// never share a real key: one process's [`Base::reset`] never deletes and remakes the keys
/// another is reading.
///
/// Declare it as `static BASE: reg::Base = reg::Base::new(r"Software\AetherVfsFooTest");`; it
/// is a `&str` (`Deref`) and formats as the full subkey (`format!(r"{BASE}\{rel}")`).
pub(crate) struct Base {
    name: &'static str,
    full: OnceLock<String>,
}

impl Base {
    pub(crate) const fn new(name: &'static str) -> Base {
        Base {
            name,
            full: OnceLock::new(),
        }
    }

    /// Delete this process's key (panicking if it is still there), then delete what earlier test
    /// processes left under the same name (a key whose process has exited). `locked`
    /// names the subkeys (relative to the base) a test gives a restrictive DACL: Wine's server
    /// keeps a key's DACL for as long as it runs, so locked keys survive a delete until they are
    /// unlocked.
    pub(crate) fn reset(&self, locked: &[&str]) {
        delete_scratch(self, locked);
        if let Some(k) = reg_open(self, KEY_READ) {
            unsafe { RegCloseKey(k) };
            panic!(r"HKCU\{self} could not be deleted");
        }
        let (parent, prefix) = self
            .name
            .rsplit_once('\\')
            .expect(r"a name under HKCU\Software");
        let Some(p) = reg_open(parent, KEY_READ) else {
            return;
        };
        let prefix = format!("{prefix}-");
        let mut stale = Vec::new();
        let mut buf = [0u16; 256];
        for i in 0.. {
            let mut len = buf.len() as u32;
            let st = unsafe {
                RegEnumKeyExW(
                    p,
                    i,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if st != 0 {
                break;
            }
            let leaf = String::from_utf16_lossy(&buf[..len as usize]);
            let pid = leaf
                .strip_prefix(&prefix)
                .and_then(|s| s.parse::<u32>().ok());
            if pid.is_some_and(|pid| !process_running(pid)) {
                stale.push(format!(r"{parent}\{leaf}"));
            }
        }
        unsafe { RegCloseKey(p) };
        for s in stale {
            delete_scratch(&s, locked);
        }
    }
}

impl std::ops::Deref for Base {
    type Target = str;
    fn deref(&self) -> &str {
        self.full
            .get_or_init(|| format!("{}-{}", self.name, std::process::id()))
    }
}

impl std::fmt::Display for Base {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

/// `HKCU\<sub>` opened with `access`, if it opens.
fn reg_open(sub: &str, access: u32) -> Option<HKEY> {
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(sub).as_ptr(), 0, access, &mut k) };
    (st == 0).then_some(k)
}

/// Unlock `locked` under `HKCU\<base>` and delete the tree.
fn delete_scratch(base: &str, locked: &[&str]) {
    for rel in locked {
        unlock(&format!(r"{base}\{rel}"));
    }
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(base).as_ptr()) };
}

/// Whether process `pid` is still running. A pid that cannot be opened is taken as gone (a
/// process of the same user can always be opened for this right).
fn process_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        !ok || code == STILL_ACTIVE as u32
    }
}

/// The NT and canonical spellings of a test's keys under `HKCU\<base>`.
pub(crate) struct Paths {
    pub(crate) sid: String,
    base: &'static str,
}

impl Paths {
    pub(crate) fn new(base: &'static str) -> Paths {
        Paths {
            sid: user_sid(),
            base,
        }
    }

    /// `HKCU\<base>\<rel>` as an absolute NT name.
    pub(crate) fn nt(&self, rel: &str) -> String {
        format!(r"\REGISTRY\USER\{}\{}\{rel}", self.sid, self.base)
    }

    /// ... and as the canonical path the overlay stores.
    pub(crate) fn canon(&self, rel: &str) -> String {
        format!(r"\Registry\User\<CurrentUser>\{}\{rel}", self.base)
    }
}

/// The unhooked process that answers what is really in the registry: this test binary, started
/// before the hooks went in, running the ignored [`reg_checker`].
pub(crate) struct Checker {
    _child: Child,
    /// The checker process's handle, for tests that duplicate handles into it.
    pub(crate) process: isize,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>)>,
}

impl Checker {
    /// Start it. Call before the hooks are installed.
    pub(crate) fn spawn() -> Checker {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                CHECKER_TEST,
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHECKER_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("checker");
        use std::os::windows::io::AsRawHandle;
        let process = child.as_raw_handle() as isize;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Checker {
            _child: child,
            process,
            io: Mutex::new((stdin, stdout)),
        }
    }

    pub(crate) fn ask(&self, q: &str) -> String {
        let mut io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(io.0, "{q}").unwrap();
        io.0.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(io.1.read_line(&mut line).unwrap(), 0, "checker exited");
            // libtest's own `test reg_checker ... ` may lead the first answer's line.
            if let Some(i) = line.find("CHECK:") {
                return line[i + 6..].trim().to_string();
            }
        }
    }

    /// Whether `HKCU\<key>` exists in the real registry.
    pub(crate) fn really_exists(&self, key: &str) -> bool {
        self.ask(&format!("K|{key}")) == "1"
    }

    /// The real value's data as hex, or `none` (`nokey` when the key is missing).
    pub(crate) fn real_value(&self, key: &str, name: &str) -> String {
        self.ask(&format!("V|{key}|{name}"))
    }

    /// The real key's last-write time, its value count and its subkey count.
    pub(crate) fn real_info(&self, key: &str) -> String {
        self.ask(&format!("I|{key}"))
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Not a test of its own: the checker process [`Checker`] asks. Each stdin line is a query on
/// the real registry under HKCU: `K|key` (exists: 1 or 0), `V|key|name` (the value's data in
/// hex, or `none`), `I|key` (last-write time, value and subkey counts).
#[test]
#[ignore]
fn reg_checker() {
    if std::env::var_os(CHECKER_ENV).is_none() {
        return;
    }
    let mut out = std::io::stdout();
    for line in std::io::stdin().lines() {
        let line = line.unwrap();
        let parts: Vec<&str> = line.trim().split('|').collect();
        let mut k: HKEY = std::ptr::null_mut();
        let st = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(parts[1]).as_ptr(),
                0,
                KEY_READ,
                &mut k,
            )
        };
        let answer = match (parts[0], st) {
            ("K", st) => (if st == 0 { "1" } else { "0" }).to_string(),
            (_, st) if st != 0 => "nokey".to_string(),
            ("V", _) => {
                let mut buf = [0u8; 256];
                let mut len = 256u32;
                let mut ty = 0u32;
                let r = unsafe {
                    RegQueryValueExW(
                        k,
                        wide(parts[2]).as_ptr(),
                        std::ptr::null(),
                        &mut ty,
                        buf.as_mut_ptr(),
                        &mut len,
                    )
                };
                if r == 0 {
                    hex(&buf[..len as usize])
                } else {
                    "none".to_string()
                }
            }
            _ => {
                let (mut subs, mut vals) = (0u32, 0u32);
                let mut ft = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let n = std::ptr::null_mut();
                unsafe {
                    RegQueryInfoKeyW(
                        k,
                        std::ptr::null_mut(),
                        n,
                        std::ptr::null(),
                        &mut subs,
                        n,
                        n,
                        &mut vals,
                        n,
                        n,
                        n,
                        &mut ft,
                    )
                };
                format!("{}:{}:{vals}:{subs}", ft.dwHighDateTime, ft.dwLowDateTime)
            }
        };
        if st == 0 {
            unsafe { RegCloseKey(k) };
        }
        writeln!(out, "CHECK:{answer}").unwrap();
        out.flush().unwrap();
    }
}

/// Start the fake director with its registry behind the ring and switch the registry
/// overlay on (`VFS_REGISTRY`, which `regclient::enabled` reads once). Returns the virtual
/// root's directory too.
pub(crate) fn start_director(tag: &str) -> (&'static Fake, std::path::PathBuf) {
    let (served, root) = start_director_with(
        tag,
        fakedirector::Options {
            arena_len: 0,
            ..fakedirector::Options::default()
        },
    );
    (served.fake, root)
}

/// [`start_director`] on a ring and with a registry generation as `opts` says, returning
/// what the harness taps saw and can do (`fakedirector::install_with`).
pub(crate) fn start_director_with(
    tag: &str,
    opts: fakedirector::Options,
) -> (&'static fakedirector::Served, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("aether-shim-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var(aether_env::REGISTRY, "1");
    let served = fakedirector::install_with(&root, Fake::new().with_registry(), opts);
    (served, root)
}

/// [`start_director`], then the shim's hooks over an empty root. Never uninstalled.
pub(crate) fn install_hooks(tag: &str) -> &'static Fake {
    install_hooks_with(
        tag,
        fakedirector::Options {
            arena_len: 0,
            ..fakedirector::Options::default()
        },
    )
    .fake
}

/// [`start_director_with`], then the shim's hooks over an empty root. Never uninstalled.
pub(crate) fn install_hooks_with(
    tag: &str,
    opts: fakedirector::Options,
) -> &'static fakedirector::Served {
    let (served, _root) = start_director_with(tag, opts);
    std::mem::forget(install().expect("install"));
    assert_eq!(
        aether_hooks::reg_overlay_disabled_by(),
        None,
        "every registry detour is in"
    );
    assert!(regclient::enabled());
    served
}

/// Whether this test runs under Wine: Wine's ntdll exports `wine_get_version`, Windows' does
/// not. For the few answers the two hosts give differently.
pub(crate) fn host_is_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: both names are NUL-terminated; a missing module or export is a null return.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
}
