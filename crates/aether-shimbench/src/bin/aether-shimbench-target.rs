//! Makes one kind of Win32 call many times and times it:
//!
//! ```text
//! aether-shimbench-target [--lenient] [--out FILE] [--sync NAME] <case> <iterations> <path>
//! ```
//!
//! `<path>` is what the case works on (a file, or `data/list` for
//! `findfirst-100`). The target makes [`aether_shimbench::warmup`] untimed
//! calls, then at least `<iterations>` timed ones, and writes one JSON line
//! (the case, the counts, how they were timed and the per-call nanoseconds'
//! summary) to `FILE`, or to standard output without `--out`.
//!
//! Calls are timed in batches ([`aether_shimbench::batch_size`]): each batch
//! lasts at least 100 steps of the timer (`QueryPerformanceCounter`, whose
//! step the line gives as `timer_ns`), and a sample is the batch's time over
//! its calls, so a call shorter than the timer's step is measured, not
//! rounded to it. The batch size follows the warm-up's mean call time.
//!
//! A process the injector starts writes to the console, not to a pipe, so
//! the launcher always passes `--out`.
//!
//! Every call is checked (it succeeds, a read returns its length, a listing
//! has its 100 names, a final path ends in the file's name); the first that
//! fails ends the run with exit 1. `--lenient` times calls however they end,
//! for a director that answers nothing (the null director finds no file).
//!
//! `--sync NAME`: around the timed loop the target sets the event
//! `NAME-begin` and waits for `NAME-go`, and after it sets `NAME-end` and
//! waits for `NAME-done`, so the launcher can count the requests the loop
//! alone sent. The events are the launcher's (auto-reset).
//!
//! Exit codes: 0 done, 1 a call failed, 2 usage, 4 a sync wait timed out.

#[cfg(not(windows))]
fn main() {
    eprintln!("aether-shimbench-target runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    windows::main()
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 calls
mod windows {
    use aether_shimbench::{batch_size, target_line, warmup, Case, Summary, Timing, LISTED, MIB};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NO_MORE_FILES, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FindClose, FindFirstFileW, FindNextFileW, GetFileAttributesExW,
        GetFileExInfoStandard, GetFinalPathNameByHandleW, ReadFile, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WIN32_FILE_ATTRIBUTE_DATA,
        WIN32_FIND_DATAW,
    };
    use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
    use windows_sys::Win32::System::Threading::{
        OpenEventW, SetEvent, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// How long a sync wait may take before the target gives up.
    const SYNC_WAIT: Duration = Duration::from_secs(60);
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const KIB4: usize = 4096;

    struct Args {
        lenient: bool,
        out: Option<String>,
        sync: Option<String>,
        case: Case,
        iterations: u64,
        path: String,
    }

    fn usage() -> ! {
        let names: Vec<&str> = Case::ALL.iter().map(|c| c.name()).collect();
        eprintln!(
            "usage: aether-shimbench-target [--lenient] [--out FILE] [--sync NAME] <case> \
             <iterations> <path>\ncases: {}",
            names.join(", ")
        );
        std::process::exit(2);
    }

    fn parse() -> Args {
        let mut a = std::env::args().skip(1);
        let (mut lenient, mut out, mut sync) = (false, None, None);
        let mut rest = Vec::new();
        while let Some(x) = a.next() {
            match x.as_str() {
                "--lenient" => lenient = true,
                "--out" => out = Some(a.next().unwrap_or_else(|| usage())),
                "--sync" => sync = Some(a.next().unwrap_or_else(|| usage())),
                _ => rest.push(x),
            }
        }
        let [case, iterations, path] = <[String; 3]>::try_from(rest).unwrap_or_else(|_| usage());
        Args {
            lenient,
            out,
            sync,
            case: Case::parse(&case).unwrap_or_else(|| usage()),
            iterations: iterations.parse().unwrap_or_else(|_| usage()),
            path,
        }
    }

    pub fn main() {
        let args = parse();
        let mut call = match setup(args.case, &args.path, args.lenient) {
            Ok(c) => c,
            Err(e) => fail(args.case, &format!("setup: {e}")),
        };
        let warm = warmup(args.iterations);
        let t = Instant::now();
        for _ in 0..warm {
            check(&args, call.run());
        }
        let warm_mean = t.elapsed().as_nanos() as u64 / warm;
        let timer_ns = timer_resolution_ns();
        let batch = batch_size(warm_mean, timer_ns);
        let timing = Timing {
            batch,
            batches: args.iterations.div_ceil(batch),
            timer_ns,
        };
        let sync = args.sync.as_deref().map(Handshake::open);
        if let Some(s) = &sync {
            s.handshake("begin", "go");
        }
        let mut ns = vec![0u64; timing.batches as usize];
        for slot in ns.iter_mut() {
            let t = Instant::now();
            for _ in 0..batch {
                let r = call.run();
                check(&args, r);
            }
            *slot = t.elapsed().as_nanos() as u64 / batch;
        }
        if let Some(s) = &sync {
            s.handshake("end", "done");
        }
        drop(call);
        let line = target_line(
            args.case,
            args.iterations,
            timing,
            args.lenient,
            Summary::of(&mut ns),
        );
        let text = format!("{line}\n");
        match &args.out {
            Some(p) => {
                if let Err(e) = std::fs::write(p, text) {
                    fail(args.case, &format!("write {p}: {e}"));
                }
            }
            None => print!("{text}"),
        }
    }

    fn check(args: &Args, r: Result<(), String>) {
        if let Err(e) = r {
            if !args.lenient {
                fail(args.case, &e);
            }
        }
    }

    fn fail(case: Case, why: &str) -> ! {
        eprintln!("aether-shimbench-target {}: {why}", case.name());
        std::process::exit(1);
    }

    /// The launcher's events around the timed loop.
    struct Handshake(String);

    impl Handshake {
        fn open(name: &str) -> Handshake {
            Handshake(name.to_string())
        }

        /// Set `NAME-<set>`, then wait for `NAME-<wait>`.
        fn handshake(&self, set: &str, wait: &str) {
            let open = |suffix: &str| {
                let w = wide_str(&format!("{}-{suffix}", self.0));
                // SAFETY: a NUL-terminated name.
                let h = unsafe {
                    OpenEventW(
                        EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                        0,
                        w.as_ptr(),
                    )
                };
                if h.is_null() {
                    eprintln!("aether-shimbench-target: open event {}-{suffix}", self.0);
                    std::process::exit(4);
                }
                h
            };
            let (s, w) = (open(set), open(wait));
            // SAFETY: two event handles this function opened and closes.
            unsafe {
                SetEvent(s);
                let r = WaitForSingleObject(w, SYNC_WAIT.as_millis() as u32);
                CloseHandle(s);
                CloseHandle(w);
                if r != WAIT_OBJECT_0 {
                    eprintln!("aether-shimbench-target: no {}-{wait}", self.0);
                    std::process::exit(4);
                }
            }
        }
    }

    fn wide_str(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn wide(p: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .chain(core::iter::once(0))
            .collect()
    }

    /// The step of `Instant` (`QueryPerformanceCounter`), in nanoseconds.
    fn timer_resolution_ns() -> u64 {
        let mut freq = 0i64;
        // SAFETY: writes one i64.
        unsafe { QueryPerformanceFrequency(&mut freq) };
        (1_000_000_000u64).div_ceil(freq.max(1) as u64)
    }

    fn last_error() -> u32 {
        // SAFETY: reads this thread's last-error value.
        unsafe { GetLastError() }
    }

    fn open_read(w: &[u16]) -> Result<HANDLE, String> {
        // SAFETY: a NUL-terminated name; no security attributes or template.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                SHARE_ALL,
                core::ptr::null(),
                OPEN_EXISTING,
                0,
                core::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("CreateFileW: error {}", last_error()));
        }
        Ok(h)
    }

    /// A case's call, with what it needs opened and allocated beforehand.
    enum Call {
        OpenClose(Vec<u16>),
        Getattr(Vec<u16>),
        Read {
            h: HANDLE,
            buf: Vec<u8>,
        },
        List(Vec<u16>),
        FinalPath {
            h: HANDLE,
            buf: Vec<u16>,
            suffix: Vec<u16>,
        },
    }

    fn setup(case: Case, path: &str, lenient: bool) -> Result<Call, String> {
        let p = Path::new(path);
        Ok(match case {
            Case::OpenCloseInRoot => Call::OpenClose(wide(p)),
            Case::GetattrInRoot | Case::GetattrOutside => Call::Getattr(wide(p)),
            Case::Read4kCached | Case::Read4kUncached | Case::Read1mBulk => {
                let len = if case == Case::Read1mBulk { MIB } else { KIB4 };
                let h = open_read(&wide(p))?;
                let mut call = Call::Read {
                    h,
                    buf: vec![0; len],
                };
                // The bytes are checked once, here; each timed call checks
                // its count.
                if call.run().is_ok() && !lenient {
                    if let Call::Read { buf, .. } = &call {
                        if buf[..] != aether_refdirector::pattern(len)[..] {
                            return Err("the bytes read are not the file's".into());
                        }
                    }
                }
                call
            }
            Case::Findfirst100 => Call::List(wide(&p.join("*"))),
            Case::FinalPath => {
                let h = open_read(&wide(p))?;
                let suffix: Vec<u16> = r"\data\a.esp".encode_utf16().collect();
                Call::FinalPath {
                    h,
                    buf: vec![0; 1024],
                    suffix,
                }
            }
        })
    }

    impl Call {
        fn run(&mut self) -> Result<(), String> {
            match self {
                Call::OpenClose(w) => {
                    let h = open_read(w)?;
                    // SAFETY: the handle just opened.
                    unsafe { CloseHandle(h) };
                    Ok(())
                }
                Call::Getattr(w) => {
                    // SAFETY: plain data, all-zero is valid.
                    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { core::mem::zeroed() };
                    // SAFETY: a NUL-terminated name and a buffer of the
                    // standard class's type.
                    let ok = unsafe {
                        GetFileAttributesExW(
                            w.as_ptr(),
                            GetFileExInfoStandard,
                            (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
                        )
                    };
                    if ok == 0 {
                        return Err(format!("GetFileAttributesExW: error {}", last_error()));
                    }
                    Ok(())
                }
                Call::Read { h, buf } => {
                    // SAFETY: plain data, all-zero is valid (offset 0).
                    let mut ov: OVERLAPPED = unsafe { core::mem::zeroed() };
                    let mut got = 0u32;
                    // SAFETY: `buf` holds its length; a synchronous handle,
                    // so the OVERLAPPED only carries the offset.
                    let ok = unsafe {
                        ReadFile(
                            *h,
                            buf.as_mut_ptr().cast(),
                            buf.len() as u32,
                            &mut got,
                            &mut ov,
                        )
                    };
                    if ok == 0 {
                        return Err(format!("ReadFile: error {}", last_error()));
                    }
                    if got as usize != buf.len() {
                        return Err(format!("ReadFile read {got} of {}", buf.len()));
                    }
                    Ok(())
                }
                Call::List(w) => {
                    // SAFETY: plain data, all-zero is valid.
                    let mut fd: WIN32_FIND_DATAW = unsafe { core::mem::zeroed() };
                    // SAFETY: a NUL-terminated pattern and a find-data buffer.
                    let h = unsafe { FindFirstFileW(w.as_ptr(), &mut fd) };
                    if h == INVALID_HANDLE_VALUE {
                        return Err(format!("FindFirstFileW: error {}", last_error()));
                    }
                    let mut names = 0usize;
                    loop {
                        let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
                        let name = &fd.cFileName[..n];
                        if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                            names += 1;
                        }
                        // SAFETY: the find handle and the same buffer.
                        if unsafe { FindNextFileW(h, &mut fd) } == 0 {
                            break;
                        }
                    }
                    let err = last_error();
                    // SAFETY: the find handle opened above.
                    unsafe { FindClose(h) };
                    if err != ERROR_NO_MORE_FILES {
                        return Err(format!("FindNextFileW: error {err}"));
                    }
                    if names != LISTED {
                        return Err(format!("listed {names} names, not {LISTED}"));
                    }
                    Ok(())
                }
                Call::FinalPath { h, buf, suffix } => {
                    // SAFETY: `buf` holds its length in UTF-16 units.
                    let n = unsafe {
                        GetFinalPathNameByHandleW(*h, buf.as_mut_ptr(), buf.len() as u32, 0)
                    } as usize;
                    if n == 0 || n >= buf.len() {
                        return Err(format!("GetFinalPathNameByHandleW: error {}", last_error()));
                    }
                    let got = &buf[..n];
                    let ends = got.len() >= suffix.len()
                        && got[got.len() - suffix.len()..]
                            .iter()
                            .zip(suffix.iter())
                            .all(|(a, b)| lower(*a) == lower(*b));
                    if !ends {
                        return Err(format!(
                            "GetFinalPathNameByHandleW gave {}",
                            String::from_utf16_lossy(got)
                        ));
                    }
                    Ok(())
                }
            }
        }
    }

    fn lower(c: u16) -> u16 {
        if (b'A' as u16..=b'Z' as u16).contains(&c) {
            c + 32
        } else {
            c
        }
    }

    impl Drop for Call {
        fn drop(&mut self) {
            if let Call::Read { h, .. } | Call::FinalPath { h, .. } = self {
                // SAFETY: the handle `setup` opened, closed once.
                unsafe { CloseHandle(*h) };
            }
        }
    }
}
