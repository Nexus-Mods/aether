# Aether

Aether puts a Windows game in front of files that are not on disk. Two small
Windows programs do it:

- **`aether-injector.exe`** (crate `aether-inject`): starts the game's process and
  loads the shim into it by remote-thread injection.
- **`aether_shim_dll.dll`** (crate `aether-shim-dll`, hooks in `aether-shim`): runs
  inside the game and detours its NT file and registry calls, answering every
  operation on a managed path from a **director** over a shared-memory ring.

The director, the program that decides what the game sees, is not here: it is
whatever speaks the wire contract (`aether-ipc`, `aether-protocol`). Cairn's
director (Clojure, [Nexus-Mods/cairn](https://github.com/Nexus-Mods/cairn)) is
one. Running a Windows game on Linux or macOS (Proton, Wine prefixes,
CrossOver) is the director's job too; the injector and the shim are the same
Windows binaries everywhere, running inside Wine there.

Both ship for **x64 and x86** (32-bit games and tools), with the C runtime
linked in (`+crt-static`): the shim loads inside the game, where Windows
doesn't look beside it for `vcruntime140.dll`, and a PC may have no VC++
runtime. They are staged as `aether-injector.exe`, `aether_shim_dll.dll`,
`aether-injector32.exe` and `aether_shim_dll32.dll`.

How it fits together: Cairn's
[docs/aether-vfs/](https://github.com/Nexus-Mods/cairn/tree/master/docs/aether-vfs)
(start with `architecture.md`) until they move here. The code
here was split from Cairn's `rust/` on 2026-11-03, with its history; before
that it lived in [halgari/aether-vfs](https://github.com/halgari/aether-vfs).

## Build

On Windows, with the MSVC C++ build tools (the Rust toolchain and the
`i686-pc-windows-msvc` target come from [`rust-toolchain.toml`](rust-toolchain.toml)):

```powershell
rustup toolchain install
powershell -ExecutionPolicy Bypass -File bin\build-natives.ps1             # the four natives, into target\natives
powershell -ExecutionPolicy Bypass -File bin\build-natives.ps1 -Fixtures   # also the test fixtures and the host-target
                                                                           # injector and shim, into target\release
```

Options: `-Out <dir>` stages the natives elsewhere; `-RequireDumpbin` fails
(rather than warns) when Visual Studio's `dumpbin` isn't found for the check
that none of the four imports the VC++ runtime (on by default when `$env:CI`
is set). `bin\natives-key.ps1` prints the natives key, the hash of what the
four are built from.

On Linux and macOS, `bin/build-windows [--release]` cross-builds the same
natives and the fixtures with `cargo-xwin`, clang and lld (LLVM 19 or later:
the i686 shim's `EXPORTAS` export needs it).

## Test

```powershell
cargo test --no-fail-fast
```

Windows is where all of it builds and runs; the portable crates also build and
test on Linux (CI's `linux` job lists them). CI also runs `cargo fmt --all
--check` and `cargo clippy --all-targets -- -D warnings`
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

**8.3 TMP alias.** When `TMP`/`TEMP` contain a short-name alias (a `~`, as in
`C:\Users\LONGNA~1\AppData\Local\Temp`), eleven of these tests fail. Point them at
a path without one first:

```powershell
$env:TMP = 'C:\vfstmp'; $env:TEMP = 'C:\vfstmp'
```

Tests whose prerequisite is missing (a fixture not built) skip; with
`VFS_TEST_REQUIRE_ALL=1` a skip is a failure. The fixtures must be built
first (`cargo build` of the inject artifacts, as ci.yml does).

## Crates

The ones that ship:

| Crate | Role |
|-------|------|
| `aether-inject` | The injector: starts the target, injects the shim (the `aether-injector` binary) |
| `aether-shim-dll` | The shim DLL: `DllMain` bootstrap |
| `aether-shim` | The NT detours and the director client |
| `aether-ipc` | The shared-memory control ring and bulk arena, OS-free |
| `aether-protocol` | Wire codecs, opcodes, the shim's bootstrap config |
| `aether-provider` | The provider contract (`Capabilities`, `VPath`, conformance suite) |
| `aether-redirect` | Pure path core: root map and canonicalisation |
| `aether-ntlayout` | Pure NT byte layouts and decisions for the hooks |
| `aether-registry` | Registry overlay tree and its merge with a real key |
| `aether-win` | Windows shared memory, events and volume lookups |
| `aether-pe` | Pure PE byte parsing |
| `aether-core` | Pure merged-tree resolver (layers, tombstones, case folding) |
| `aether-env` | Every `VFS_*` environment switch, defined once |
| `aether-zip` | Stored-zip reading, used by the injector |

The **test fixtures**, Windows programs the tests run under the shim:
`aether-fixture-read` (reads a virtual file), `-writepath` (the write path),
`-escape` (paths that try to escape the managed root), `-prefs` (the INI profile
API), `-vproxy` and `-staticimp` (a statically imported proxy DLL, pre-init
injection), `-steam`, `-nvapi` and `-registry` (probes for the Proton path and
the registry overlay). `aether-testkit` holds helpers shared by the tests.

**Test-only:** `aether-director`, `aether-compose` and `aether-unix` are an old Rust
director kept as the shim's tests' counterpart (a dev-dependency of the
shim). Don't build new features on them: a director harness will replace
them.

`panic = "unwind"` throughout (`aether-protocol/tests/unwind.rs` pins it). The
vendored `retour` in `vendor/` limits its "win64" hooks to x86-64 so the shim
builds for i686. [CHANGELOG.md](CHANGELOG.md) is the host-facing API history.

## License

GPL-3.0-or-later ([LICENSE](LICENSE)).
