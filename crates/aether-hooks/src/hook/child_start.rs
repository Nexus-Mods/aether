//! Where a child of a purely virtual location really starts, and what it is
//! told.
//!
//! A program the launched program starts (`skse64_loader.exe` starting
//! `SkyrimSE.exe`) has the same problem the launched program had: the kernel
//! opens its image, and the loader its static imports, before any hook runs
//! in it, and `CreateProcess` needs a real working directory. When roots have
//! backing directories (`aether_env::ROOT_BACKINGS`), the process hook therefore
//!
//! - starts a child whose image only the VFS holds from a copy the director
//!   stages into the root's backing directory (`Opcode::StageExec`), leaving the
//!   command line, with its virtual `argv[0]`, as the caller wrote it;
//! - finds every child's image itself, as `CreateProcess` would, seeing
//!   what the VFS sees, and hands the host its full path, so the host never
//!   searches; a start that finds nothing fails as the system's would;
//! - starts a child whose working directory is under a root in that root's
//!   backing directory, and tells it the virtual one before it runs;
//! - gives a child the caller hands its own environment block the session's
//!   `VFS_*` variables too, so its shim finds the ring.
//!
//! The child's shim then renames its image path, as the launched program's
//! did (`whereami`).

use aether_protocol::RootId;

use crate::director::{backings_from_env, global, FuseClient};

/// `CREATE_UNICODE_ENVIRONMENT`.
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x400;

/// What the process hook changes about a child's creation.
#[derive(Default)]
pub(super) struct ChildStart {
    /// The image to create the child from, NUL-terminated: a staged copy,
    /// or the full path of a file outside the roots.
    pub app: Option<Vec<u16>>,
    /// The real working directory to create it in, NUL-terminated.
    pub cwd: Option<Vec<u16>>,
    /// That directory as a path: the one name the host's own check of it
    /// finds on the real disk ([`RealDirScope`]).
    pub backing: Option<String>,
    /// The virtual working directory to tell it, once created.
    pub virtual_cwd: Option<String>,
    /// The environment block to create it with (Unicode), when the caller
    /// handed one in.
    pub env: Option<Vec<u16>>,
    /// The start fails with this error, as the system's would: no image
    /// found, or one under a root that cannot be staged (refused rather
    /// than started from whatever is really on disk there).
    pub refuse: Option<u32>,
}

thread_local! {
    /// The backing directory a child is being started in on this thread,
    /// while [`RealDirScope`] holds it.
    static REAL_DIR: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
}

/// While held, an attribute query of exactly `dir` on this thread goes to
/// the real disk ([`is_real_dir`]). The process hook holds one around the
/// host's `CreateProcessInternalW` for a child started in a backing
/// directory: the host checks its working directory (`GetFileAttributesW`),
/// a real folder that routes to the root itself. Every other hooked call in
/// the host's call, the image search and registry reads included, stays
/// virtualised.
pub(super) struct RealDirScope(());

impl RealDirScope {
    pub(super) fn enter(dir: &str) -> RealDirScope {
        let _ = REAL_DIR.try_with(|d| {
            if let Ok(mut d) = d.try_borrow_mut() {
                *d = Some(dir.to_string());
            }
        });
        RealDirScope(())
    }
}

impl Drop for RealDirScope {
    fn drop(&mut self) {
        let _ = REAL_DIR.try_with(|d| {
            if let Ok(mut d) = d.try_borrow_mut() {
                *d = None;
            }
        });
    }
}

/// Whether `path` (NT or DOS spelling, any case, trailing separators apart)
/// is the directory a [`RealDirScope`] on this thread names.
pub(super) fn is_real_dir(path: &str) -> bool {
    fn bare(p: &str) -> &str {
        p.strip_prefix(r"\??\")
            .or_else(|| p.strip_prefix(r"\\?\"))
            .unwrap_or(p)
            .trim_end_matches(['\\', '/'])
    }
    REAL_DIR
        .try_with(|d| {
            d.try_borrow()
                .ok()
                .and_then(|d| {
                    d.as_deref()
                        .map(|dir| bare(dir).eq_ignore_ascii_case(bare(path)))
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// A NUL-terminated UTF-16 string, or `None` for a null pointer.
///
/// # Safety
/// `p` is null or a NUL-terminated UTF-16 string.
pub(super) unsafe fn wide_opt(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: NUL-terminated per the contract.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        Some(String::from_utf16_lossy(core::slice::from_raw_parts(p, n)))
    }
}

fn nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn join(dir: &str, name: &str) -> String {
    format!("{}\\{name}", dir.trim_end_matches(['\\', '/']))
}

/// `path` (a full DOS path) in NT spelling.
fn nt_of(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        format!(r"\??\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\") {
        format!(r"\??\UNC\{rest}")
    } else {
        format!(r"\??\{path}")
    }
}

/// `path`'s root and vpath when it is under one.
fn routed(client: &FuseClient, path: &str) -> Option<(RootId, crate::director::VPath)> {
    client.route(&nt_of(path))
}

/// The process's current directory as it was told it (the PEB's).
fn current_dir() -> Option<String> {
    // SAFETY: reads this process's own PEB.
    unsafe { super::path::cwd_from_peb() }.map(|(_, dos)| dos)
}

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED as ACCESS_DENIED, ERROR_FILE_NOT_FOUND as FILE_NOT_FOUND,
    ERROR_INVALID_PARAMETER as INVALID_PARAMETER, ERROR_PATH_NOT_FOUND as PATH_NOT_FOUND,
};

/// What a path holds, as the start sees it.
#[derive(Debug, PartialEq)]
enum Hit {
    Missing,
    Dir,
    /// A file, to create the child from this path: a staged copy for a file
    /// the VFS holds, the path itself for one on the real disk.
    File(String),
}

/// The folders a command line's program is searched in, in the order
/// `CreateProcess` searches them.
#[derive(Clone)]
struct Folders {
    /// The folder of the parent's image.
    app_dir: Option<String>,
    /// The parent's current directory (never the child's).
    cwd: Option<String>,
    /// `NoDefaultCurrentDirectoryInExePath` is set: the current directory
    /// is searched only for a name with a backslash in it.
    no_default_cwd: bool,
    /// The system folder, the 16-bit system folder, the Windows folder.
    system: Vec<String>,
    /// The parent's `PATH`, as it is.
    path: String,
}

impl Folders {
    fn of_this_process() -> Folders {
        Folders {
            app_dir: std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.to_string_lossy().into_owned())),
            cwd: current_dir(),
            no_default_cwd: std::env::var_os("NoDefaultCurrentDirectoryInExePath").is_some(),
            system: system_dirs(),
            path: std::env::var("PATH").unwrap_or_default(),
        }
    }
}

/// Whether the last component of `name` has an extension: a dot anywhere
/// in it (`.hidden` and `name.` have one), as the system's search decides
/// whether to add `.exe`.
fn has_extension(name: &str) -> bool {
    name.rsplit(['\\', '/'])
        .next()
        .is_some_and(|last| last.contains('.'))
}

/// A name the system resolves against the current directory alone: one that
/// starts with a drive, a separator, `.\` or `..\` (either separator).
fn is_resolved_directly(name: &str) -> bool {
    let mut c = name.chars();
    let (a, b, d) = (c.next(), c.next(), c.next());
    let sep = |x: Option<char>| matches!(x, Some('\\' | '/'));
    sep(a)
        || b == Some(':')
        || (a == Some('.') && sep(b))
        || (a == Some('.') && b == Some('.') && sep(d))
}

/// The names `CreateProcess` tries for a command line's program, in order:
/// a quoted first argument alone; otherwise the line up to each space or
/// tab in turn, then the whole line.
fn image_tokens(cmd: &str) -> Vec<&str> {
    if let Some(rest) = cmd.strip_prefix('"') {
        return vec![rest.split('"').next().unwrap_or("")];
    }
    cmd.char_indices()
        .filter(|&(_, c)| c == ' ' || c == '\t')
        .map(|(i, _)| &cmd[..i])
        .chain(Some(cmd))
        .collect()
}

/// Resolve the child's image as `CreateProcess` does, deciding each
/// candidate with `probe` (given a full path) and making full paths with
/// `full` (against the parent's current directory, and each drive's).
/// `Ok` is the path to create the child from; `Err` the error the start
/// fails with.
///
/// An application name is used as given, with no extension added and no
/// search. A command line's program is searched for: `.exe` added when its
/// last component has no extension; a name with a drive, a leading
/// separator, `.\` or `..\` resolved directly, any other searched for in
/// [`Folders`]' order, the first path that exists (file or directory)
/// ending the search. A directory or nothing found tries the next of
/// [`image_tokens`]; when none is a file, the start fails as the system's
/// does on the first.
fn resolve(
    app: Option<&str>,
    cmd: &str,
    folders: &dyn Fn() -> Folders,
    full: &dyn Fn(&str) -> Option<String>,
    probe: &mut dyn FnMut(&str, bool) -> Result<Hit, u32>,
) -> Result<String, u32> {
    let mut look = |p: &str, stage: bool| match full(p) {
        Some(f) => probe(&f, stage),
        None => Ok(Hit::Missing),
    };
    if let Some(app) = app {
        if app.is_empty() {
            return Err(PATH_NOT_FOUND);
        }
        return match look(app, true)? {
            Hit::File(image) => Ok(image),
            Hit::Dir => Err(ACCESS_DENIED),
            // Missing: the kernel's open fails on the folder, or the file.
            Hit::Missing => {
                let parent = app.rsplit_once(['\\', '/']).map(|(d, _)| d);
                match parent {
                    Some(d)
                        if !d.is_empty() && !d.ends_with(':') && look(d, false)? != Hit::Dir =>
                    {
                        Err(PATH_NOT_FOUND)
                    }
                    _ => Err(FILE_NOT_FOUND),
                }
            }
        };
    }
    let mut at = |p: &str| look(p, true);
    // Gathered only for a name that is searched for.
    let mut searched: Option<Folders> = None;
    let mut first = None;
    for token in image_tokens(cmd) {
        let hit = if token.is_empty() {
            Hit::Missing
        } else {
            let name = if has_extension(token) {
                token.to_string()
            } else {
                format!("{token}.exe")
            };
            if is_resolved_directly(&name) {
                at(&name)?
            } else {
                search(&name, searched.get_or_insert_with(folders), &mut at)?
            }
        };
        first.get_or_insert(match (&hit, token.is_empty()) {
            (_, true) => INVALID_PARAMETER,
            (Hit::Dir, _) => ACCESS_DENIED,
            _ => FILE_NOT_FOUND,
        });
        if let Hit::File(image) = hit {
            return Ok(image);
        }
    }
    Err(first.unwrap_or(INVALID_PARAMETER))
}

/// One name (its extension settled, not resolved directly) as the system's
/// search finds it.
fn search(
    name: &str,
    folders: &Folders,
    at: &mut dyn FnMut(&str) -> Result<Hit, u32>,
) -> Result<Hit, u32> {
    let cwd = (name.contains('\\') || !folders.no_default_cwd)
        .then_some(folders.cwd.as_deref())
        .flatten();
    let dirs = folders
        .app_dir
        .as_deref()
        .into_iter()
        .chain(cwd)
        .chain(folders.system.iter().map(String::as_str))
        // A relative entry is resolved against the current directory, as
        // the name in it is.
        .chain(folders.path.split(';').filter(|d| !d.is_empty()));
    for dir in dirs {
        let hit = at(&join(dir, name))?;
        if hit != Hit::Missing {
            return Ok(hit);
        }
    }
    Ok(Hit::Missing)
}

/// What `path` (full) holds. Under a root the director's view decides: a
/// file is staged (when `stage`), and the start refused when it cannot be,
/// or when the director does not answer; outside every root the real disk
/// does.
fn probe(client: &FuseClient, path: &str, stage: bool) -> Result<Hit, u32> {
    if let Some((root, vp)) = routed(client, path) {
        return match client.getattr(root, &vp) {
            Ok(a) if !a.found => Ok(Hit::Missing),
            Ok(a) if a.is_dir => Ok(Hit::Dir),
            Ok(_) if !stage => Ok(Hit::File(String::new())),
            Ok(_) => client
                .stage_exec(root, &vp)
                .map(Hit::File)
                .map_err(|_| FILE_NOT_FOUND),
            Err(_) => Err(FILE_NOT_FOUND),
        };
    }
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY, INVALID_FILE_ATTRIBUTES,
    };
    let wide = nul(path);
    // The system's own check: an attribute query, not an open. In a 32-bit
    // process this and the start that follows go through the same file
    // system redirection (the system folder is SysWOW64 to both).
    let attrs = {
        let _io = super::ShimIoGuard::enter();
        // SAFETY: a NUL-terminated path.
        unsafe { GetFileAttributesW(wide.as_ptr()) }
    };
    Ok(if attrs == INVALID_FILE_ATTRIBUTES {
        Hit::Missing
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Hit::Dir
    } else {
        Hit::File(path.to_string())
    })
}

/// `name` as a full path, against the parent's current directory (and, for
/// `C:name`, drive C's): the system's own resolution (`GetFullPathNameW`).
fn full_path(name: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let wide = nul(name);
    let call = |buf: &mut [u16]| {
        // SAFETY: a NUL-terminated name; `buf` holds `buf.len()` units.
        let n = unsafe {
            GetFullPathNameW(
                wide.as_ptr(),
                buf.len() as u32,
                buf.as_mut_ptr(),
                core::ptr::null_mut(),
            )
        };
        n as usize
    };
    let mut buf = [0u16; 520];
    let n = call(&mut buf);
    if n == 0 {
        return None;
    }
    if n < buf.len() {
        return Some(String::from_utf16_lossy(&buf[..n]));
    }
    let mut long = vec![0u16; n];
    let n = call(&mut long);
    (n > 0 && n < long.len()).then(|| String::from_utf16_lossy(&long[..n]))
}

/// The system folders `CreateProcess` searches, in its order: the system
/// folder, the 16-bit system folder, the Windows folder.
fn system_dirs() -> Vec<String> {
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetWindowsDirectoryW,
    };
    fn read(f: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<String> {
        let mut buf = [0u16; 512];
        // SAFETY: `buf` holds `buf.len()` units.
        let n = unsafe { f(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
    }
    let windows = read(GetWindowsDirectoryW);
    read(GetSystemDirectoryW)
        .into_iter()
        .chain(windows.as_deref().map(|w| join(w, "System")))
        .chain(windows)
        .collect()
}

/// Plan a child's creation. `None` when no root has a backing directory or
/// there is no director.
///
/// # Safety
/// `app`, `cmd` and `cur_dir` are null or NUL-terminated UTF-16 strings;
/// `env` is null or an environment block, Unicode when `flags` says so.
pub(super) unsafe fn plan(
    app: *const u16,
    cmd: *const u16,
    env: *const core::ffi::c_void,
    flags: u32,
    cur_dir: Option<String>,
) -> Option<ChildStart> {
    let backings = backings_from_env();
    if backings.is_empty() {
        return None;
    }
    let client = global()?;
    let mut out = ChildStart::default();

    // The image: found here, as the system would find it, and handed to the
    // host as a full path, so the host never searches for it (its search
    // would find a file only the VFS holds, which the kernel would then open
    // from whatever is really on disk there).
    // SAFETY: per this function's contract.
    let (app, cmd) = unsafe { (wide_opt(app), wide_opt(cmd).unwrap_or_default()) };
    let image = resolve(
        app.as_deref(),
        &cmd,
        &Folders::of_this_process,
        &full_path,
        &mut |p, stage| probe(client, p, stage),
    );
    match image {
        Ok(image) => out.app = Some(nul(&image)),
        Err(code) => {
            out.refuse = Some(code);
            return Some(out);
        }
    }

    // The working directory.
    if let Some(dir) = cur_dir.or_else(current_dir) {
        if let Some((root, _)) = routed(client, &dir) {
            if let Some((_, backing)) = backings.iter().find(|(id, _)| *id == root) {
                // A real folder: made on disk by the shim's own I/O. Through the
                // hooks the backing routes to the root itself, so the director
                // would be asked about it and a missing folder never made.
                let made = {
                    let _io = super::ShimIoGuard::enter();
                    std::fs::create_dir_all(backing)
                };
                if let Err(e) = made {
                    // The host then fails the start on its working directory.
                    crate::loadlog::note(&format!(
                        "child start: backing directory {backing} not made: {e}"
                    ));
                }
                out.cwd = Some(nul(backing));
                out.backing = Some(backing.clone());
                out.virtual_cwd = Some(dir);
            }
        }
    }
    // The environment, when the caller handed one in: theirs, with each of
    // this process's handshake variables (the ring, the roots, the shim's
    // config) that it lacks. Only those, and only where missing: everything
    // else in the block is the caller's choice, removals included.
    if !env.is_null() {
        // SAFETY: per this function's contract.
        let parent = unsafe { caller_block(env, flags & CREATE_UNICODE_ENVIRONMENT != 0) };
        let theirs: Vec<String> = block_entries(&parent)
            .into_iter()
            .filter_map(|e| e.split_once('=').map(|(k, _)| k.to_ascii_uppercase()))
            .collect();
        let handshake = |k: &str| {
            aether_env::ALL
                .iter()
                .any(|v| v.kind == aether_env::Kind::Handshake && v.name.eq_ignore_ascii_case(k))
        };
        let ours: Vec<(String, String)> = block_entries(&aether_inject::current_env_block())
            .into_iter()
            .filter_map(|e| {
                let (k, v) = e.split_once('=')?;
                (handshake(k) && !theirs.contains(&k.to_ascii_uppercase()))
                    .then(|| (k.to_string(), v.to_string()))
            })
            .collect();
        if !ours.is_empty() {
            out.env = Some(aether_inject::build_env_block(&parent, &ours, &[]));
        }
    }

    Some(out)
}

/// The caller's environment block as UTF-16, through its final empty entry.
///
/// # Safety
/// `env` is an environment block: UTF-16 when `unicode`, else 8-bit.
unsafe fn caller_block(env: *const core::ffi::c_void, unicode: bool) -> Vec<u16> {
    let mut out = Vec::new();
    let mut n = 0usize;
    // SAFETY: the block ends with two NULs; the scan stops there.
    unsafe {
        if unicode {
            let p = env as *const u16;
            while !(*p.add(n) == 0 && *p.add(n + 1) == 0) {
                n += 1;
            }
            out.extend_from_slice(core::slice::from_raw_parts(p, n + 2));
        } else {
            let p = env as *const u8;
            while !(*p.add(n) == 0 && *p.add(n + 1) == 0) {
                n += 1;
            }
            out.extend(
                core::slice::from_raw_parts(p, n + 2)
                    .iter()
                    .map(|&b| b as u16),
            );
        }
    }
    out
}

/// The `name=value` entries of an environment block (the `=C:` ones too).
fn block_entries(block: &[u16]) -> Vec<String> {
    block
        .split(|&u| u == 0)
        .take_while(|e| !e.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The flags to create the child with, given the caller's: Unicode
/// environment when the plan built one.
pub(super) fn flags_for(plan: Option<&ChildStart>, flags: u32) -> u32 {
    match plan {
        Some(p) if p.env.is_some() => flags | CREATE_UNICODE_ENVIRONMENT,
        _ => flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_dir_scope_names_one_directory_on_this_thread_while_held() {
        assert!(!is_real_dir(r"C:\Games\Backing"));
        {
            let _scope = RealDirScope::enter(r"C:\Games\Backing");
            for p in [
                r"C:\Games\Backing",
                r"\??\c:\games\backing\",
                r"\\?\C:\Games\Backing",
            ] {
                assert!(is_real_dir(p), "{p}");
            }
            assert!(!is_real_dir(r"C:\Games\Backing\file.txt"));
            assert!(!is_real_dir(r"C:\Games"));
            assert!(!std::thread::spawn(|| is_real_dir(r"C:\Games\Backing"))
                .join()
                .unwrap());
        }
        assert!(!is_real_dir(r"C:\Games\Backing"));
    }

    /// A disk: each path (lower case) a file or a directory, `Err` for a
    /// director that does not answer. The current directory is `C:\cwd`.
    struct Disk {
        entries: Vec<(&'static str, Result<bool, ()>)>,
        probed: Vec<String>,
    }

    fn disk(files: &[&'static str], dirs: &[&'static str]) -> Disk {
        let entries = files
            .iter()
            .map(|f| (*f, Ok(true)))
            .chain(dirs.iter().map(|d| (*d, Ok(false))))
            .collect();
        Disk {
            entries,
            probed: Vec::new(),
        }
    }

    fn full(p: &str) -> Option<String> {
        let p = p.replace('/', "\\");
        let b: Vec<char> = p.chars().collect();
        let abs = if p.starts_with(r"\\") || (b.len() > 2 && b[1] == ':' && b[2] == '\\') {
            p
        } else if let Some(rest) = p.strip_prefix(r".\") {
            format!(r"C:\cwd\{rest}")
        } else if p.starts_with('\\') {
            format!("C:{p}")
        } else if b.len() > 1 && b[1] == ':' {
            format!(r"{}\cwd\{}", &p[..2], &p[2..])
        } else {
            format!(r"C:\cwd\{p}")
        };
        Some(abs.to_ascii_lowercase())
    }

    fn folders() -> Folders {
        Folders {
            app_dir: Some(r"C:\app".into()),
            cwd: Some(r"C:\cwd".into()),
            no_default_cwd: false,
            system: vec![r"C:\sys".into(), r"C:\win".into()],
            path: r"C:\p1;;rel".into(),
        }
    }

    fn run(d: &mut Disk, f: &Folders, app: Option<&str>, cmd: &str) -> Result<String, u32> {
        resolve(app, cmd, &|| f.clone(), &full, &mut |p, _| {
            d.probed.push(p.to_string());
            match d.entries.iter().find(|(e, _)| *e == p) {
                None => Ok(Hit::Missing),
                Some((_, Ok(false))) => Ok(Hit::Dir),
                Some((_, Ok(true))) => Ok(Hit::File(p.to_string())),
                Some((_, Err(()))) => Err(FILE_NOT_FOUND),
            }
        })
    }

    #[test]
    fn a_command_line_program_is_searched_for_in_the_systems_order() {
        let f = folders();
        let order = [
            r"c:\app\x.exe",
            r"c:\cwd\x.exe",
            r"c:\sys\x.exe",
            r"c:\win\x.exe",
            r"c:\p1\x.exe",
            r"c:\cwd\rel\x.exe",
        ];
        let mut d = disk(&[], &[]);
        assert_eq!(run(&mut d, &f, None, "x"), Err(FILE_NOT_FOUND));
        assert_eq!(d.probed, order, "every folder once, `.exe` added");
        for (i, hit) in order.iter().enumerate() {
            let mut d = disk(&order[i..], &[]);
            assert_eq!(run(&mut d, &f, None, "x -a"), Ok(hit.to_string()));
        }
        // NoDefaultCurrentDirectoryInExePath: the current directory only
        // for a name with a backslash.
        let f = Folders {
            no_default_cwd: true,
            ..folders()
        };
        let mut d = disk(&[r"c:\cwd\x.exe"], &[]);
        assert_eq!(run(&mut d, &f, None, "x"), Err(FILE_NOT_FOUND));
        let mut d = disk(&[r"c:\cwd\s\x.exe"], &[]);
        assert_eq!(run(&mut d, &f, None, r"s\x"), Ok(r"c:\cwd\s\x.exe".into()));
        let mut d = disk(&[r"c:\cwd\s\x.exe"], &[]);
        assert_eq!(run(&mut d, &f, None, "s/x"), Err(FILE_NOT_FOUND));
    }

    #[test]
    fn exe_is_added_only_to_a_command_line_name_without_an_extension() {
        let f = folders();
        for (cmd, probed) in [
            ("x.y", r"c:\app\x.y"),
            (".hid", r"c:\app\.hid"),
            ("sub.d/x", r"c:\app\sub.d\x.exe"),
        ] {
            let mut d = disk(&[], &[]);
            let _ = run(&mut d, &f, None, cmd);
            assert_eq!(d.probed[0], probed, "{cmd}");
        }
        // An application name: as given, against the current directory.
        let mut d = disk(&[r"c:\cwd\x.exe", r"c:\app\x"], &[]);
        assert_eq!(run(&mut d, &f, Some("x"), "x"), Err(FILE_NOT_FOUND));
        assert_eq!(d.probed, [r"c:\cwd\x"]);
        let mut d = disk(&[r"c:\cwd\noext"], &[]);
        assert_eq!(
            run(&mut d, &f, Some("noext"), "y"),
            Ok(r"c:\cwd\noext".into())
        );
    }

    #[test]
    fn unquoted_spaces_are_tried_from_the_shortest_name() {
        let f = folders();
        let cmd = r"C:\My Tool\x.exe -a";
        let mut d = disk(&[r"c:\my.exe", r"c:\my tool\x.exe"], &[]);
        assert_eq!(run(&mut d, &f, None, cmd), Ok(r"c:\my.exe".into()));
        let mut d = disk(&[r"c:\my tool\x.exe"], &[r"c:\my.exe"]);
        assert_eq!(run(&mut d, &f, None, cmd), Ok(r"c:\my tool\x.exe".into()));
        // Quoted: that name alone.
        let mut d = disk(&[r"c:\my.exe"], &[]);
        assert_eq!(
            run(&mut d, &f, None, r#""C:\My Tool\x.exe" -a"#),
            Err(FILE_NOT_FOUND)
        );
        assert_eq!(d.probed, [r"c:\my tool\x.exe"]);
        assert_eq!(image_tokens("a b\tc"), ["a", "a b", "a b\tc"]);
    }

    #[test]
    fn names_with_a_drive_a_separator_or_dots_are_not_searched() {
        let f = folders();
        for (cmd, probed) in [
            (r".\x", r"c:\cwd\x.exe"),
            ("./x", r"c:\cwd\x.exe"),
            (r"\x", r"c:\x.exe"),
            ("C:x", r"c:\cwd\x.exe"),
            (r"C:\d\x", r"c:\d\x.exe"),
        ] {
            let mut d = disk(&[], &[]);
            assert_eq!(run(&mut d, &f, None, cmd), Err(FILE_NOT_FOUND), "{cmd}");
            assert_eq!(d.probed, [probed], "{cmd}");
        }
    }

    #[test]
    fn a_start_fails_as_the_systems_would() {
        let f = folders();
        // A directory as the first name, nothing later.
        let mut d = disk(&[], &[r"c:\cwd\d.exe"]);
        assert_eq!(run(&mut d, &f, None, "d -a"), Err(ACCESS_DENIED));
        // A directory later, never the first.
        let mut d = disk(&[], &[r"c:\app\n q.exe"]);
        assert_eq!(run(&mut d, &f, None, "n q"), Err(FILE_NOT_FOUND));
        for cmd in ["", "  x"] {
            let mut d = disk(&[], &[]);
            assert_eq!(
                run(&mut d, &f, None, cmd),
                Err(INVALID_PARAMETER),
                "{cmd:?}"
            );
        }
        // An application name: a directory, a missing file, a missing folder.
        let mut d = disk(&[], &[r"c:\cwd\d.exe", r"c:\cwd\s"]);
        assert_eq!(run(&mut d, &f, Some("d.exe"), "x"), Err(ACCESS_DENIED));
        assert_eq!(run(&mut d, &f, Some(r"s\x.exe"), "x"), Err(FILE_NOT_FOUND));
        assert_eq!(run(&mut d, &f, Some(r"n\x.exe"), "x"), Err(PATH_NOT_FOUND));
        assert_eq!(run(&mut d, &f, Some(""), "x"), Err(PATH_NOT_FOUND));
    }

    #[test]
    fn a_director_error_ends_the_search() {
        let f = folders();
        let mut d = disk(&[r"c:\p1\x.exe"], &[]);
        d.entries.push((r"c:\cwd\x.exe", Err(())));
        assert_eq!(run(&mut d, &f, None, "x"), Err(FILE_NOT_FOUND));
        assert_eq!(d.probed.last().map(String::as_str), Some(r"c:\cwd\x.exe"));
    }

    #[test]
    fn full_paths_take_their_nt_spelling() {
        assert_eq!(nt_of(r"C:\x"), r"\??\C:\x");
        assert_eq!(nt_of(r"\\?\C:\x"), r"\??\C:\x");
        assert_eq!(nt_of(r"\\srv\s\x"), r"\??\UNC\srv\s\x");
        assert!(has_extension(".hid") && has_extension("a.") && !has_extension(r"a.b\c"));
    }
}
