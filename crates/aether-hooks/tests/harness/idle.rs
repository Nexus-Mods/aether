//! What the fake director costs while nothing is asked of it: its idle serve
//! threads back off instead of polling the ring flat out (Task 3's review,
//! I1), so the test processes that run beside each other on CI's 2-vCPU
//! runners do not starve each other's timed waits.
#![allow(unsafe_code)] // GetProcessTimes

use crate::fakedirector::{self, Fake, Options};
use std::time::Duration;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// This process's CPU time so far, user and kernel.
fn cpu() -> Duration {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of this process and four FILETIMEs.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "GetProcessTimes");
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // 100 ns units.
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

#[test]
fn idle_serve_threads_use_little_cpu() {
    isolate!();
    let dir = aether_testkit::tempdir().expect("temp folder");
    let served = fakedirector::install_with(dir.path(), Fake::new(), Options::default());
    assert!(served.attach.is_ok());
    // Past the hot window after the attach's HEARTBEAT. No hooks are
    // installed, so `thread::sleep` is the host's.
    std::thread::sleep(Duration::from_millis(100));

    let before = cpu();
    std::thread::sleep(Duration::from_secs(1));
    let used = cpu() - before;

    println!("idle second: {used:?} of CPU on 4 serve threads");
    // Four threads polling flat out use a second each where they get a
    // processor (at least one second on two). Backed off, they use a few
    // milliseconds.
    assert!(used < Duration::from_millis(250), "{used:?}");
}
