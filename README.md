# rust/: the injector and the shim

This Cargo workspace builds the two Windows programs Cairn's director needs to
put a game in front of its virtual filesystem:

- **`vfs-injector.exe`** (crate `vfs-inject`): starts the game's process and
  loads the shim into it by remote-thread injection.
- **`vfs_shim_dll.dll`** (crate `vfs-shim-dll`, hooks in `vfs-shim`): runs
  inside the game and detours its NT file and registry calls, answering every
  operation on a managed path from the director over a shared-memory ring.

Both ship for **x64 and x86** (32-bit games and tools such as Oblivion), with the
C runtime linked in (`+crt-static`): the shim loads inside the game, where Windows
doesn't look beside it for `vcruntime140.dll`, and a PC may have no VC++ runtime.
They are staged as `vfs-injector.exe`, `vfs_shim_dll.dll`, `vfs-injector32.exe`
and `vfs_shim_dll32.dll`, the names the director looks for
(`aether.director.natives`).

The director itself, the part that decides what the game sees, is Clojure
(`../src/aether`); the wire contract it and the shim share is
`vfs-ipc`/`vfs-protocol`. How it all fits: [`../docs/aether-vfs/`](../docs/aether-vfs)
(start with [architecture.md](../docs/aether-vfs/architecture.md); the rules the
hooks keep are in [shim-invariants.md](../docs/aether-vfs/shim-invariants.md)).

## Build

From the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File bin\build-natives.ps1             # the four natives, into rust\target\natives
powershell -ExecutionPolicy Bypass -File bin\build-natives.ps1 -Fixtures   # also the test fixtures and the host-target
                                                                           # injector and shim, into rust\target\release
```

Options: `-Out <dir>` stages the natives elsewhere; `-RequireDumpbin` fails
(rather than warns) when Visual Studio's `dumpbin` isn't found for the check that
none of the four imports the VC++ runtime (on by default when `$env:CI` is set).
`bin\bootstrap.ps1` runs it with `-Fixtures` after checking the toolchain. The
Rust toolchain and the `i686-pc-windows-msvc` target come from
[`rust-toolchain.toml`](rust-toolchain.toml); the MSVC C++ build tools are needed.

CI builds them only when something here (or `bin/build-natives.ps1`) changed:
`bin\natives-key.ps1` hashes every tracked file under `rust/` and the script,
and the built natives are cached under that key (`docs/release.md`, "The
natives key"). `powershell -ExecutionPolicy Bypass -File bin\natives-key.ps1`
prints it, to match a CI run's `natives key:` line or cache entry.

On Linux and macOS, `bin/build-windows [--release]` cross-builds the same natives
and the fixtures with `cargo-xwin`, clang and lld (LLVM 19 or later: the i686
shim's `EXPORTAS` export needs it). `bin/bootstrap` runs it.

## Test

```powershell
cd rust
cargo test --no-fail-fast
```

Windows is where all of it builds and runs. CI also runs `cargo fmt --all --check`
and `cargo clippy --all-targets -- -D warnings` (see `.github/workflows/test.yml`).

**8.3 TMP alias.** When `TMP`/`TEMP` contain a short-name alias (a `~`, as in
`C:\Users\LONGNA~1\AppData\Local\Temp`), eleven of these tests fail. Point them at
a path without one first:

```powershell
$env:TMP = 'C:\vfstmp'; $env:TEMP = 'C:\vfstmp'
```

Tests whose prerequisite is missing (a Windows fixture not built, GE-Proton not
installed) skip; with `VFS_TEST_REQUIRE_ALL=1` a skip is a failure. The
Clojure director's own tests that launch these programs are covered in the
[top-level README](../README.md#tests).

## Crates

The ones that ship:

| Crate | Role |
|-------|------|
| `vfs-inject` | The injector: starts the target, injects the shim (the `vfs-injector` binary) |
| `vfs-shim-dll` | The shim DLL: `DllMain` bootstrap |
| `vfs-shim` | The NT detours and the director client |
| `vfs-ipc` | The shared-memory control ring and bulk arena, OS-free |
| `vfs-protocol` | Wire codecs, opcodes, the shim's bootstrap config |
| `vfs-provider` | The provider contract (`Capabilities`, `VPath`, conformance suite) |
| `vfs-redirect` | Pure path core: root map and canonicalisation |
| `vfs-ntlayout` | Pure NT byte layouts and decisions for the hooks |
| `vfs-registry` | Registry overlay tree and its merge with a real key |
| `vfs-win` | Windows shared memory, events and volume lookups |
| `vfs-pe` | Pure PE byte parsing |
| `vfs-core` | Pure merged-tree resolver (layers, tombstones, case folding) |
| `vfs-env` | Every `VFS_*` environment switch, defined once |

The **test fixtures**, Windows programs the tests run under the shim:
`vfs-fixture-read` (reads a virtual file), `-writepath` (the write path),
`-escape` (paths that try to escape the managed root), `-prefs` (the INI profile
API), `-vproxy` and `-staticimp` (a statically imported proxy DLL, pre-init
injection), `-steam`, `-nvapi` and `-registry` (probes for the Proton path and
the registry overlay). `vfs-testkit` holds helpers shared by the tests.

**The Rust director, kept for now.** `vfs-director`, `vfs-embed`, `vfs-compose`,
`vfs-zip`, `vfs-unix` and `vfs-proton` are the original Rust director and its
Linux and Proton launch path. Cairn runs on the Clojure director; these crates
stay only because their tests still exercise the shim and injector end to end,
until those tests are ported to the Clojure director (the second import plan).
Don't build new features on them.

`panic = "unwind"` throughout (`vfs-protocol/tests/unwind.rs` pins it). The
vendored `retour` in `vendor/` limits its "win64" hooks to x86-64 so the shim
builds for i686. [CHANGELOG.md](CHANGELOG.md) is the host-facing API history of
the Rust director crates. License: the repository's [LICENSE](../LICENSE).
