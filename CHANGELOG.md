# Changelog

Changes to the host-facing API of `vfs-embed` and `vfs-proton` (both deleted
since), `vfs-provider`
and `vfs-env` that a host (Haskill) has to adapt to or can start using. Newest
first. Internal refactors are in the git history, not here.

## Unreleased (the old Rust director is removed)

Aether ships only the shim and the injector; directors live outside this
repository. The old Rust director and the crates only it used are deleted:
`aether-director`, `aether-compose`, `aether-zip` and `aether-unix`.
`aether-inject` no longer depends on `aether-core` or `aether-zip`. The shim's
tests use their own fake director, which now answers the registry opcodes too.

## Unreleased (remote-thread injection is the only activation)

The in-memory import patch is gone: the injector no longer writes a first
import of the shim into a suspended target's headers, the shim no longer does
it for children, and an EXE that imports the shim (staged with the import
written in) is no longer recognised and left alone. Every process gets the
shim by remote `LoadLibrary`. With it went `aether_pe`'s import-patch API
(`add_first_import`, `first_import_is`, `SHIM_IMPORT_DLL`,
`SHIM_IMPORT_SYMBOL`, the SteamStub helpers, `mem_patch`),
`aether_inject::patch_imports_in_memory` and `exe_imports_shim`,
`RunConfig::force_injection`, and the shim's `vfs_shim_activated` export.
`VFS_FORCE_INJECTION` is still a handshake name and has no effect. The shim's
load log loses `, import-patched …` from its `created from` line and
`(in-memory import patch refused: …)` from its `activation` line.

## Unreleased (Aether: crates and binaries renamed `vfs-*` to `aether-*`)

The code now lives in its own repository, Nexus-Mods/aether, split from
Cairn's `rust/`. Every crate is renamed `vfs-*` to `aether-*` (Rust paths
`vfs_*` to `aether_*`), and so are the binaries a director starts:
`aether-injector.exe`, `aether_shim_dll.dll` and their 32-bit
`aether-injector32.exe`, `aether_shim_dll32.dll`; the fixtures are
`aether-fixture-*.exe`, `aether-probe.exe`, `aether-staticimp.exe`
(`vproxy.dll` keeps its name). Nothing on the wire changes: the `VFS_*`
environment variables, the ring and config magics (`VFIP`, `VFSC`), the
wire version, the `Local\vfs_shim_ready_<pid>`/`vfs_shim_failed_<pid>`
events, the `vfs_shim_activated` export (since removed, above), and the text the shim and the
injector print (`vfs-shim hook stats`, `[vfs-injector] ...`) are as they
were. The entries below use the names of their time.

## Unreleased (the Rust director is removed)

`vfs-embed` and `vfs-proton` are deleted, and with them `Session`, `vfs_proton`
and the `vfs-proton` binary. Nothing in this repository hosts the director from
Rust any more: Cairn runs the Clojure director (`aether.director.*`), and the
end-to-end tests of the deleted crates now run against it
(`docs/aether-vfs/e2e-port.md` says where each went). The entries below describe
those crates as they were.

## Unreleased (Windows process-tree wait)

Windows launches now last as long as their whole process tree, as Proton
launches already did. A launcher chain (`skse64_loader.exe` starts
`SkyrimSE.exe` and exits) is one launch that ends when the game ends. Design:
`docs/superpowers/specs/2026-10-13-windows-process-tree-wait-design.md`.
The Windows behaviour is compile-checked only so far: the new Windows tests
have not been run on Windows.

### Added

- Windows: `Session::launch_detached(&LaunchOpts) -> Result<LaunchHandle, String>`,
  `Session::stop_launch() -> Result<bool, String>`, and `vfs_embed::LaunchHandle`
  (`pid`, `notes`, `stopper`, `is_running`, `try_wait`, `wait`, `stop`) and
  `vfs_embed::LaunchStopper` (`stop`, `was_stopped`), with the Proton handle's
  contract: the launch runs until every process it started has exited, the
  exit code is the first process's, and `stop` ends the whole tree
  (`LaunchExit::Stopped`). `LaunchHandle::pid` is the launched program's pid.
  The Steam helper accessors stay Proton-only.
- `vfs_inject::spawn_target_with_shim(RunConfig) -> Result<ProcessTree, InjectError>`
  with `ProcessTree` and `TreeKiller`: the target in a Job Object before any of
  its code runs. `run_target_with_shim` (used by `vfs-injector.exe` under Wine)
  is unchanged.

### Changed (Windows)

- `Session::launch` with `wait: true` returns when the whole process tree has
  exited, not when the first process has. The exit code is still the first
  process's.
- `Session::launch` with `wait: false` is held by the session: `stop_launch`
  stops it, and dropping the session stops it before the ring goes. Before,
  the child was left running against a ring that had gone away.
- The process-wide launch environment lock is released once the target is
  running, not held for the whole wait, so another session in the same host
  can launch while one launch is being waited on.
- **One launch per session, as on Proton.** While a launch runs (waited on,
  held for `wait: false`, or a `LaunchHandle` from `launch_detached` that has
  not been seen to end), `launch` and `launch_detached` return
  `Err("launch: a launch is already running in this session — stop_launch() first")`.
  Before, a Windows session allowed several launches at once. `stop_launch`
  stops the one running launch, and for a held `wait: false` launch waits up
  to 30 s for its job to empty.
- **Every child stays in the launch.** The shim removes
  `CREATE_BREAKAWAY_FROM_JOB` from every child it creates and the job allows no
  breakaway, so a child that asks to break away (a crash reporter, an updater)
  is waited for and stopped like any other, and its `CreateProcess` does not
  fail on that account. If the host's own job forbids nesting, the
  launch falls back to first-process tracking and says so in
  `LaunchHandle::notes()`. The job is killed when its handle closes, so a
  crashed host takes the game with it.

## Unreleased (add-ons)

### Removed

- The daemon stack: `vfs-directord` (the `vfs` CLI and gRPC daemon),
  `vfs-control` (its gRPC contract and TOML config schema) and `vfs-source`
  (`SourceSpec` → provider, and the out-of-process gRPC `RemoteProvider`).
  No host used them; a host composes sessions with `vfs_embed::Session`, and
  one that adds sources one at a time uses `vfs_embed::RootSources` with
  `Session::set_root_mounts`. `tools/python_source_plugin` went with
  `vfs-source`'s proto.

## Unreleased (cleanup pass)

### Breaking

- `vfs_embed::LaunchOpts::shim_dll` and `payload_dll` are `Option<PathBuf>`
  (were `Option<String>`). `cwd` stays a `String`: it names a place inside the
  Wine prefix (`C:\...`), not a host path.
- `vfs_embed::LaunchOpts` has a new field `injector: Option<PathBuf>`. Left
  `None`, `vfs-injector.exe` is looked for as before (beside `shim_dll`, else in
  `VFS_WINDOWS_ARTIFACTS`, else beside the executable). A host that builds
  `LaunchOpts` with `..LaunchOpts::default()` needs no change for this field.
- `vfs_provider`'s conformance suite (`conformance`, `assert_conformance`,
  `write_fixture_tree`, `RwMemFixture`, `FIXTURE_FILES`, the fixture providers)
  is behind the `conformance` feature. Enable it in `[dev-dependencies]`.
  `vfs_embed::{assert_conformance, write_fixture_tree, FIXTURE_FILES}` and
  `vfs_source::{assert_conformance, write_fixture_tree}` are behind a feature of
  the same name that forwards to it.
- `vfs_proton::launch::WineLaunch` is `#[non_exhaustive]`. Build one with
  `WineLaunch::new(runtime, prefix, target, virtual_dir, LaunchFiles, RingGeometry)`
  and set the public fields it needs. The literal-struct form, with
  `injector`, `shim_dll`, `payload_dll`, `config_file`, `ready_file` and the
  ring numbers as fields of `WineLaunch`, no longer compiles outside the crate.
- `vfs_proton::prefix::Prefix::map_drive` and `unmap_drive` are removed (nothing
  used them).
- The `#[doc(hidden)]` flat re-exports in `vfs_proton`'s root are removed
  (`InstallError`, `Installed`, `LaunchError`, `BASE_DLL_OVERRIDES`,
  `PrefixError`, `VerifyError`, `runtime_lib_env`, `STEAM_HELPER`, ...). Use the
  module paths (`vfs_proton::launch::LaunchError`, ...) or `vfs_embed::proton`.
- The errors of `Session::launch` are unchanged in text. `Session` no longer
  puts "launch: " inside the prefix step's messages; `launch` adds it.

### Added

- `Session::prepare_prefix(&self) -> Result<PathBuf, String>` (unix): the
  launch's prefix step on its own (newest runtime of the session's home, the
  session's prefix name and `PrefixInit`, under the prefix lock). The session
  need not be serving.
- `vfs_proton::runtime::newest_installed(&Root) -> io::Result<Option<PathBuf>>`.
- `vfs_embed::proton` (unix): `Root`, `Prefix`, `PrefixLock`, `PrefixInit`,
  `prefix_dir`, `nvapi::{status, Capability, GpuModel, NvapiStatus}`,
  `runtime::{cmp_tags, installed_dirs, newest_installed, verify_ge}` and
  `artifacts`. A host names only `vfs-embed`.
- `vfs_proton::artifacts`: the file names of the Windows build artefacts
  (`INJECTOR`, `SHIM_DLL`, `PAYLOAD_DLL`, `LAUNCH`, the `FIXTURE_*` names and
  `WINDOWS_ARTIFACTS`). A test asserts the list equals `bin/build-windows`'s.
