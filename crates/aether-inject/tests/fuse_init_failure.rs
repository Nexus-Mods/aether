//! When the shim's FUSE client fails to attach to a configured director, the
//! launch must abort loudly, not log the failure and let the game run fully
//! un-virtualized.
//!
//! `run_target_with_shim` is the injector's launch path, so exercising it here
//! proves the production launch aborts, not just some lower-level detail.
#![allow(unsafe_code)]
mod common;

use aether_inject::{run_target_with_shim, InjectError, RunConfig};
use std::time::Duration;

/// Forcing `try_init_from_env` to fail (via the test-only
/// `VFS_TEST_FUSE_INIT_FAIL` switch — see its doc comment in `aether-env`) must
/// make the launch return `Err(InjectError::FuseInit(_))`, and the target
/// process must never have run: it is killed while its primary thread is
/// still suspended, before a single byte of game code executes.
///
/// A bootstrap that discarded this error would install hooks anyway, write
/// "ready" regardless, and return `Ok` with the process running fully
/// un-virtualized, with nothing anywhere reporting it.
#[test]
fn fuse_init_failure_aborts_the_launch() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-fuse-init-fail-{pid}"));
    let root = base.join("gameroot");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();

    // Content is irrelevant: the decisive assertion is that the process never
    // runs at all, so it never gets far enough to read anything.
    let config_bytes = aether_hooks::encode_config(root.to_str().unwrap());
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    let probe = env!("CARGO_BIN_EXE_aether-probe").to_string();
    let dll = common::locate_shim();

    let result = run_target_with_shim(RunConfig {
        target_exe: probe,
        current_dir: None,
        args: vec![
            root.join("does-not-matter.bin")
                .to_str()
                .unwrap()
                .to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(10),
        detach: false,
        env_set: vec![(aether_env::TEST_FUSE_INIT_FAIL.into(), "1".into())],
        env_remove: Vec::new(),
        virtual_image: None,
        virtual_cwd: None,
    });

    match result {
        Err(InjectError::FuseInit(msg)) => {
            assert!(
                msg.contains("VFS_TEST_FUSE_INIT_FAIL"),
                "expected the forced-failure reason in the error message, got: {msg}"
            );
        }
        other => panic!(
            "expected Err(InjectError::FuseInit(_)) when FUSE init is forced to fail \
             (a director was configured and its client could not attach); \
             got {other:?} instead — the launch must fail on its own, distinct terms, \
             not run the process anyway or time out generically"
        ),
    }

    // The outcome that actually matters, not a log line: aether-probe writes its
    // output file unconditionally as its very first and only externally
    // visible act (see its `main`). Its absence proves the process was killed
    // before it ever ran — not merely that some message string matched.
    assert!(
        !output_path.exists(),
        "probe output file exists at {output_path:?} — the un-virtualized process \
         was allowed to run to completion, which is exactly the bypass this test exists \
         to close"
    );
}
