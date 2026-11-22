//! The early layer (`aether-inject`'s `early.rs`, and its stub): what a
//! static import's `DllMain` does during process initialisation, before the
//! remote thread that loads the shim would have, goes through the VFS.
//!
//! `aether-staticimp.exe` statically imports `vproxy.dll`, as a game imports
//! a mod's proxy DLL (an ASI loader's `version.dll`, ReShade's `dxgi.dll`).
//! Both are copied into a stage folder, the launch image's folder, which is
//! root 0 as the director routes it. The director (`fakedirector`) serves a
//! few names under root 0 that the stage folder does not have on disk.
//! vproxy.dll's `DllMain` fails the load (the process then dies of
//! `STATUS_DLL_INIT_FAILED`) unless what it reads there comes from the
//! director (`VFS_FIXTURE_VPROXY_NEEDS`, `VFS_FIXTURE_VPROXY_LIST`), and
//! writes a file there that must reach the director
//! (`VFS_FIXTURE_VPROXY_WRITES`).
//!
//! The launch is the injector's own (`run_target_with_shim`), with the real
//! shim, built for this test's architecture: the i686 build of this binary
//! covers a 32-bit target, with the x86 stub. Each test runs staticimp
//! either as the launch's target or as a child of it ([`Start::Child`]),
//! which the parent's shim stages and injects itself. The fixtures come
//! from this test's profile folder: `cargo build -p aether-inject -p
//! aether-shim -p aether-fixture-vproxy`, then `-p aether-fixture-staticimp`
//! (staticimp links
//! vproxy's import library, with no Cargo edge to order it). Without them a
//! test prints SKIP, or fails under `VFS_TEST_REQUIRE_ALL=1`.
#![allow(unsafe_code)]

use crate::fakedirector::{self, Fake, Options, ReadStyle};
use aether_inject::{run_target_with_shim, InjectError, RunConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;

const STATICIMP: &str = "aether-staticimp.exe";
const VPROXY: &str = "vproxy.dll";
const SHIM: &str = "aether_shim.dll";
/// aether-inject's parent helper: starts a program, exits with its code.
const SPAWN: &str = "aether-spawn-child.exe";

/// `STATUS_DLL_INIT_FAILED`: what the target dies of when vproxy.dll's
/// `DllMain` returns `FALSE`.
const DLL_INIT_FAILED: u32 = 0xC000_0142;

/// What is served under root 0 and nowhere on disk.
const NEEDS: &str = "needs.dat";
const PLUGINS: [&str; 2] = ["plugins/one.asi", "plugins/two.asi"];

/// The fixtures, from this test's profile folder.
struct Fixtures {
    staticimp: PathBuf,
    vproxy: PathBuf,
    shim: PathBuf,
    spawn: PathBuf,
}

fn fixtures() -> Option<Fixtures> {
    let exe = std::env::current_exe().expect("current exe");
    let deps = exe.parent().expect("test exe folder");
    let profile = if deps.file_name().is_some_and(|n| n == "deps") {
        deps.parent().expect("profile folder")
    } else {
        deps
    };
    let [staticimp, vproxy, shim, spawn] =
        [STATICIMP, VPROXY, SHIM, SPAWN].map(|n| profile.join(n));
    let missing: Vec<&Path> = [&staticimp, &vproxy, &shim, &spawn]
        .into_iter()
        .filter(|p| !p.is_file())
        .map(PathBuf::as_path)
        .collect();
    if missing.is_empty() {
        return Some(Fixtures {
            staticimp,
            vproxy,
            shim,
            spawn,
        });
    }
    let how = "cargo build -p aether-inject -p aether-shim -p aether-fixture-vproxy, then -p aether-fixture-staticimp";
    if aether_env::opt_in("VFS_TEST_REQUIRE_ALL") {
        panic!("{missing:?} missing: {how}");
    }
    eprintln!("SKIP: {missing:?} missing ({how})");
    None
}

/// How staticimp starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// The launch's own target.
    Launch,
    /// A child: the launch's target is aether-spawn-child, which starts
    /// staticimp by its path under root 0, as a game starts a program the
    /// VFS holds. The parent's shim stages it (STAGE_EXEC: the director
    /// names the copy in root 0's backing directory, the stage folder, where
    /// vproxy.dll is beside it) and injects it itself.
    Child,
}

/// Launches staticimp (`start` says how) from a fresh stage folder, with
/// the director serving [`NEEDS`] and [`PLUGINS`] under root 0 and `env`
/// added to the target's environment. What the launch returned (a child's
/// exit code is its parent's), the director, and the stage folder.
fn launch(
    f: &Fixtures,
    start: Start,
    env: &[(&str, &str)],
) -> (
    Result<i32, InjectError>,
    &'static fakedirector::Served,
    PathBuf,
) {
    let dir = aether_testkit::tempdir().expect("temp folder");
    let base = dir.path().to_path_buf();
    std::mem::forget(dir);
    let stage = base.join("stage");
    let root = base.join("root");
    for d in [&stage, &root] {
        std::fs::create_dir_all(d).unwrap();
    }
    let staged = stage.join(STATICIMP);
    std::fs::copy(&f.staticimp, &staged).unwrap();
    std::fs::copy(&f.vproxy, stage.join(VPROXY)).unwrap();
    let (image, args) = match start {
        Start::Launch => (staged.clone(), Vec::new()),
        Start::Child => {
            let parent = stage.join(SPAWN);
            std::fs::copy(&f.spawn, &parent).unwrap();
            let child = root.join(STATICIMP).to_string_lossy().into_owned();
            (parent, vec![child, stage.to_string_lossy().into_owned()])
        }
    };

    let mut fake = Fake::new()
        .with(NEEDS, b"needed by DllMain".to_vec(), ReadStyle::Whole)
        .with_dir("plugins")
        // a write layer over all of it, as a director's overlay has
        .writable_under("");
    for (i, p) in PLUGINS.iter().enumerate() {
        // the second spans several inline READs
        let bytes = vec![b'a' + i as u8; 1 + i * 300_000];
        fake = fake.with(p, bytes, ReadStyle::Whole);
    }
    if start == Start::Child {
        // the kernel starts the staged copy, never these bytes
        fake = fake
            .with(STATICIMP, b"MZ served".to_vec(), ReadStyle::Whole)
            .staging(STATICIMP, &staged.to_string_lossy());
    }
    let served = fakedirector::install_with(&root, fake, Options::default());

    let config = base.join("shim.cfg");
    std::fs::write(&config, aether_hooks::encode_config(root.to_str().unwrap())).unwrap();
    let mut env_set: Vec<(String, String)> = vec![(
        aether_env::LAUNCH_IMAGE.to_string(),
        image.to_string_lossy().into_owned(),
    )];
    if start == Start::Child {
        env_set.push((
            aether_env::ROOT_BACKINGS.to_string(),
            format!("0={}", stage.display()),
        ));
    }
    env_set.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let r = run_target_with_shim(RunConfig {
        target_exe: image.to_string_lossy().into_owned(),
        args,
        current_dir: Some(stage.to_string_lossy().into_owned()),
        dll_path: f.shim.to_string_lossy().into_owned(),
        config_path: config.to_string_lossy().into_owned(),
        ready_path: base.join("ready.flag").to_string_lossy().into_owned(),
        ready_timeout: Duration::from_secs(60),
        detach: false,
        env_set,
        env_remove: Vec::new(),
        virtual_image: None,
        virtual_cwd: None,
        new_console: false,
    });
    (r, served, stage)
}

fn assert_ran(r: &Result<i32, InjectError>, why: &str) {
    assert!(
        matches!(r, Ok(0)),
        "staticimp exits 0 only when vproxy.dll's DllMain {why}: {r:?}"
    );
}

/// The file a proxy reads its settings from, as it loads.
#[test]
fn a_static_imports_dllmain_reads_a_file_only_the_director_has() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Launch,
        &[(aether_env::FIXTURE_VPROXY_NEEDS, NEEDS)],
    );
    assert_ran(&r, "read needs.dat from the VFS");
}

/// The test above, with the layer off: the load fails, so what makes it
/// pass is the layer.
#[test]
fn without_the_early_layer_the_dllmain_finds_nothing() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Launch,
        &[
            (aether_env::FIXTURE_VPROXY_NEEDS, NEEDS),
            (aether_env::EARLY_LAYER, "0"),
        ],
    );
    assert!(
        matches!(r, Err(InjectError::TargetExited(DLL_INIT_FAILED))),
        "without the early layer vproxy.dll's DllMain reads the stage folder on disk and fails: {r:?}"
    );
}

/// An ASI loader's listing of its plugins, each read whole.
#[test]
fn a_static_imports_dllmain_lists_plugins_only_the_director_has() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Launch,
        &[(aether_env::FIXTURE_VPROXY_LIST, r"plugins\*.asi")],
    );
    assert_ran(&r, r"listed plugins\*.asi and read each from the VFS");
}

/// A proxy's log, opened in `DllMain`: the shim is up by then (it loads from
/// the first hook once kernel32 is), so the write reaches the director.
#[test]
fn what_a_static_imports_dllmain_writes_reaches_the_director() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, served, stage) = launch(
        &f,
        Start::Launch,
        &[(aether_env::FIXTURE_VPROXY_WRITES, "written.log")],
    );
    assert_ran(&r, "wrote written.log");
    assert!(
        served.fake.tally.writes("written.log") > 0,
        "vproxy.dll's write never crossed the ring"
    );
    assert!(
        !stage.join("written.log").exists(),
        "vproxy.dll's write reached the stage folder on disk"
    );
}

/// A child's static import reads its settings as it loads: the child's
/// shim is up by then too, though its parent's shim, not the injector,
/// started and injected it.
#[test]
fn a_childs_static_imports_dllmain_reads_a_file_only_the_director_has() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Child,
        &[(aether_env::FIXTURE_VPROXY_NEEDS, NEEDS)],
    );
    assert_ran(&r, "read needs.dat from the VFS, in a child");
}

/// A child's static import lists its plugins as it loads.
#[test]
fn a_childs_static_imports_dllmain_lists_plugins_only_the_director_has() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Child,
        &[(aether_env::FIXTURE_VPROXY_LIST, r"plugins\*.asi")],
    );
    assert_ran(
        &r,
        r"listed plugins\*.asi and read each from the VFS, in a child",
    );
}

/// A child's static import opens its log as it loads: the write reaches the
/// director.
#[test]
fn what_a_childs_static_imports_dllmain_writes_reaches_the_director() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, served, stage) = launch(
        &f,
        Start::Child,
        &[(aether_env::FIXTURE_VPROXY_WRITES, "written.log")],
    );
    assert_ran(&r, "wrote written.log, in a child");
    assert!(
        served.fake.tally.writes("written.log") > 0,
        "the child's vproxy.dll write never crossed the ring"
    );
    assert!(
        !stage.join("written.log").exists(),
        "the child's vproxy.dll write reached the stage folder on disk"
    );
}

/// The child test above, with the layer off (the child inherits the
/// switch): the child dies in its initialisation, its injection is refused,
/// and aether-spawn-child's start of it fails (its exit code 1). So what
/// makes it pass is the layer in the child.
#[test]
fn without_the_early_layer_a_childs_dllmain_finds_nothing() {
    isolate!();
    let Some(f) = fixtures() else { return };
    let (r, _, _) = launch(
        &f,
        Start::Child,
        &[
            (aether_env::FIXTURE_VPROXY_NEEDS, NEEDS),
            (aether_env::EARLY_LAYER, "0"),
        ],
    );
    assert!(
        matches!(r, Ok(1)),
        "without the early layer the child's vproxy.dll DllMain reads the stage folder on disk, fails, and its start is refused: {r:?}"
    );
}
