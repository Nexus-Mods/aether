//! Builds the early layer's stub (`early/src/lib.rs`) into `OUT_DIR`, and
//! co-locates PE artifacts under the profile dir (copy only, best effort).
//! Fixture *builds* happen at test runtime (see `tests/common`) — nested
//! `cargo build` from this script deadlocks on the package lock, which is
//! why the stub, one file with no dependencies, is built with `rustc` alone.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    early_stub();
    colocate_only();
}

/// `aether_early_x64.dll` or `aether_early_x86.dll`: a `no_std` DLL with no
/// imports and no entry point, which `src/early.rs` maps into a target by
/// hand. Built only for a Windows target, x64 or x86, for that target's
/// architecture (an injector serves targets of its own); with this crate's
/// toolchain (`RUSTC`) and linker (cargo-xwin's `lld-link` when cross
/// building, `link.exe` on Windows).
fn early_stub() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let (name, triple) = match (arch.as_str(), os.as_str()) {
        ("x86_64", "windows") => ("aether_early_x64", "x86_64-pc-windows-msvc"),
        ("x86", "windows") => ("aether_early_x86", "i686-pc-windows-msvc"),
        _ => return,
    };
    for f in ["early/src/lib.rs", "early/src/config.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let linker_var = format!(
        "CARGO_TARGET_{}_LINKER",
        triple.to_ascii_uppercase().replace('-', "_")
    );
    println!("cargo:rerun-if-env-changed={linker_var}");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let mut cmd = Command::new(rustc);
    cmd.args([
        "--edition",
        "2021",
        "--crate-type",
        "cdylib",
        "--crate-name",
        name,
        "--target",
        triple,
        "-C",
        "panic=abort",
        "-C",
        "opt-level=s",
        "-C",
        "lto",
        "-C",
        "codegen-units=1",
        "-C",
        "debuginfo=0",
        "-C",
        "strip=symbols",
        "-C",
        "link-arg=/NODEFAULTLIB",
        "-C",
        "link-arg=/NOENTRY",
        "-C",
        "link-arg=/Brepro",
        "--out-dir",
    ])
    .arg(&out)
    .arg("early/src/lib.rs");
    match std::env::var(&linker_var) {
        Ok(linker) if !linker.is_empty() => {
            cmd.arg("-C").arg(format!("linker={linker}"));
        }
        // Cross building without cargo-xwin's setting: LLVM's MSVC linker.
        _ if !cfg!(windows) => {
            cmd.args(["-C", "linker=lld-link"]);
        }
        _ => {}
    }
    let status = cmd
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("run rustc for the early stub");
    assert!(status.success(), "building the early stub failed: {status}");
}

fn profile_dir() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        let td = PathBuf::from(td);
        if td.ends_with(&profile) {
            return td;
        }
        return td.join(&profile);
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    out.ancestors()
        .nth(3)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("target").join(profile))
}

fn colocate_only() {
    let profile_dir = profile_dir();
    for name in ["aether_shim.dll", "vproxy.dll"] {
        let dest = profile_dir.join(name);
        if dest.is_file() {
            continue;
        }
        let src = profile_dir.join("deps").join(name);
        if src.is_file() {
            let _ = std::fs::copy(&src, &dest);
        }
    }
}
