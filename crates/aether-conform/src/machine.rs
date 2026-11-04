//! The machine a benchmark ran on, and whether two reports' machines are
//! the same one: timed numbers compare only on one machine.
//!
//! ```json
//! {"cpu": "AMD Ryzen 9 7950X 16-Core Processor", "cores": 32,
//!  "os": "Windows 11 Pro 24H2 build 26100.4061",
//!  "build": {"profile": "release", "rustc": "rustc 1.99.1 (...)", "git": "0123456789ab"}}
//! ```
//!
//! The CPU's model comes from `/proc/cpuinfo` on Linux and from the
//! registry's `HARDWARE\DESCRIPTION\System\CentralProcessor\0` on Windows;
//! `cores` is the logical processors this process may use. `build` says how
//! the program was built and is not compared: a baseline from another build
//! on the same machine is what a comparison is for.

use serde_json::{json, Value};

/// The fields of `machine` two reports must agree on to be compared.
pub const SAME: [&str; 3] = ["cpu", "cores", "os"];

/// This machine, and how this program was built.
pub fn machine() -> Value {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    json!({
        "cpu": os::cpu().unwrap_or_else(|| "unknown".into()),
        "cores": cores,
        "os": os::name(),
        "build": {
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "rustc": tool_line("rustc", &["-V"]),
            "git": tool_line("git", &["rev-parse", "--short=12", "HEAD"]),
        },
    })
}

/// `Ok` when the reports `now` and `base` were made on one machine (their
/// `machine` agrees on [`SAME`]); else the message refusing the baseline.
pub fn same_machine(now: &Value, base: &Value) -> Result<(), String> {
    for k in SAME {
        if now["machine"][k] != base["machine"][k] {
            return Err(format!(
                "the baseline is from another machine ({k}: {} here, {} there); \
                 compare only reports from the same machine",
                now["machine"][k], base["machine"][k]
            ));
        }
    }
    Ok(())
}

/// The change from `was` to `is` in percent; 0 when `was` is 0.
pub fn percent(was: f64, is: f64) -> f64 {
    if was == 0.0 {
        0.0
    } else {
        (is - was) * 100.0 / was
    }
}

/// The first line `program args` prints, run in this crate's folder when it
/// exists (so rustup picks the toolchain the repository pins), or
/// `unknown`.
fn tool_line(program: &str, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if here.is_dir() {
        cmd.current_dir(here);
    }
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(target_os = "linux")]
mod os {
    /// `model name` of the first processor in `/proc/cpuinfo`.
    pub fn cpu() -> Option<String> {
        let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        info.lines()
            .find_map(|l| l.strip_prefix("model name"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    }

    /// `PRETTY_NAME` of `/etc/os-release` and the kernel's release.
    pub fn name() -> String {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                    .map(|v| v.trim_matches('"').to_string())
            })
            .unwrap_or_else(|| "Linux".into());
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        format!("{pretty} kernel {}", kernel.trim())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // RegGetValueW
mod os {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    const NT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    pub fn cpu() -> Option<String> {
        reg_string(
            r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "ProcessorNameString",
        )
        .map(|s| s.trim().to_string())
    }

    /// The product, its version and its build with the update revision.
    pub fn name() -> String {
        format!(
            "{} {} build {}.{}",
            reg_string(NT, "ProductName").unwrap_or_else(|| "Windows".into()),
            reg_string(NT, "DisplayVersion").unwrap_or_default(),
            reg_string(NT, "CurrentBuildNumber").unwrap_or_default(),
            reg_dword(NT, "UBR").unwrap_or(0),
        )
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn reg_string(key: &str, value: &str) -> Option<String> {
        let (k, v) = (wide(key), wide(value));
        let mut buf = vec![0u16; 512];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: NUL-terminated names and a buffer of `bytes` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                k.as_ptr(),
                v.as_ptr(),
                RRF_RT_REG_SZ,
                core::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if r != 0 {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    fn reg_dword(key: &str, value: &str) -> Option<u32> {
        let (k, v) = (wide(key), wide(value));
        let mut out = 0u32;
        let mut bytes = 4u32;
        // SAFETY: NUL-terminated names and a 4-byte buffer.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                k.as_ptr(),
                v.as_ptr(),
                RRF_RT_REG_DWORD,
                core::ptr::null_mut(),
                (&mut out as *mut u32).cast(),
                &mut bytes,
            )
        };
        (r == 0).then_some(out)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod os {
    pub fn cpu() -> Option<String> {
        None
    }

    pub fn name() -> String {
        std::env::consts::OS.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_names_a_cpu_cores_an_os_and_a_build() {
        let m = machine();
        assert!(m["cores"].as_u64().unwrap() > 0);
        assert!(m["cpu"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(m["os"].as_str().is_some_and(|s| !s.is_empty()));
        for k in ["profile", "rustc", "git"] {
            assert!(m["build"][k].is_string(), "build.{k}");
        }
        if cfg!(windows) {
            assert!(m["os"].as_str().unwrap().contains("build"));
            assert_ne!(m["cpu"], "unknown");
        }
    }

    #[test]
    fn another_cpu_cores_or_os_is_another_machine_and_another_build_is_not() {
        let r = |cpu: &str, cores: u32, os: &str, git: &str| json!({"machine": {"cpu": cpu, "cores": cores, "os": os, "build": {"git": git}}});
        let base = r("a", 8, "w", "1");
        assert!(same_machine(&r("a", 8, "w", "2"), &base).is_ok());
        for other in [
            r("b", 8, "w", "1"),
            r("a", 4, "w", "1"),
            r("a", 8, "l", "1"),
        ] {
            let e = same_machine(&other, &base).unwrap_err();
            assert!(e.contains("another machine"), "{e}");
        }
    }

    #[test]
    fn a_change_is_in_percent() {
        assert_eq!(percent(100.0, 150.0), 50.0);
        assert_eq!(percent(200.0, 100.0), -50.0);
        assert_eq!(percent(0.0, 5.0), 0.0);
    }
}
