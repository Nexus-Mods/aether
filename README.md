# Aether

Aether puts a Windows game in front of files that are not on disk. Two small
Windows programs do it:

- **`aether-injector.exe`** (crate `aether-inject`): starts the game's process and
  loads the shim into it by remote-thread injection.
- **`aether_shim.dll`** (crate `aether-shim`, hooks in `aether-hooks`): runs
  inside the game and detours its NT file and registry calls, answering every
  operation on a managed path from a director over a shared-memory ring.

The director, the program that decides what the game sees, is not here: it is
whatever speaks the wire contract (`aether-ipc`, `aether-protocol`). Cairn's
director (Clojure, [Nexus-Mods/cairn](https://github.com/Nexus-Mods/cairn)) is
one. Running a Windows game on Linux or macOS (Proton, Wine prefixes,
CrossOver) is the director's job too; the injector and the shim are the same
Windows binaries everywhere, running inside Wine there.

Both ship for x64 and x86 (32-bit games and tools), with the C runtime
linked in (`+crt-static`): the shim loads inside the game, where Windows
doesn't look beside it for `vcruntime140.dll`, and a PC may have no VC++
runtime. They are staged as `aether-injector.exe`, `aether_shim.dll`,
`aether-injector32.exe` and `aether_shim32.dll`.

Writing a director: [docs/wire.md](docs/wire.md), the wire contract byte by
byte. How it fits together: [docs/architecture.md](docs/architecture.md); the
rules the hooks keep: [docs/shim-invariants.md](docs/shim-invariants.md). The code
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

When `TMP`/`TEMP` contain an 8.3 short-name alias (a `~`, as in
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
| `aether-shim` | The shim DLL: `DllMain` bootstrap |
| `aether-hooks` | The NT detours and the director client |
| `aether-ipc` | The shared-memory control ring and bulk arena, OS-free |
| `aether-protocol` | Wire codecs, opcodes, statuses, open flags, `RootId`, the shim's bootstrap config |
| `aether-redirect` | Pure path core: root map, canonicalisation, vpath normalization, DOS wildcards, final names |
| `aether-casefold` | The case fold names cross the ring in (file paths and registry keys) |
| `aether-ntlayout` | Pure NT byte layouts and decisions for the hooks |
| `aether-registry` | Registry overlay tree and its merge with a real key |
| `aether-win` | Windows shared memory, events and volume lookups |
| `aether-pe` | Pure PE byte parsing |
| `aether-env` | Every `VFS_*` environment switch, defined once |

The test fixtures, Windows programs the tests run under the shim, are three
crates. `aether-fixtures` holds seven programs and the library they share:
`aether-fixture-read` (reads a virtual file), `-writepath` (the write path),
`-escape` (paths that try to escape the managed root), `-prefs` (the INI profile
API), `-steam`, `-nvapi` and `-registry` (probes for the Proton path and the
registry overlay). `aether-fixture-vproxy` and `aether-fixture-staticimp` are a
statically imported proxy DLL and the program that imports it (pre-init
injection); staticimp links vproxy's import library, so vproxy is built first,
in a cargo invocation of its own (cargo#6313). `aether-testkit` holds helpers
shared by the tests.

Directors live outside this repository. The shim's tests run against their own
fake director (`crates/aether-hooks/tests/fakedirector`), which answers the file
and registry opcodes from memory.

`panic = "unwind"` throughout (`aether-protocol/tests/unwind.rs` pins it). The
vendored `retour` in `vendor/` limits its "win64" hooks to x86-64 so the shim
builds for i686. [CHANGELOG.md](CHANGELOG.md) is the host-facing API history.

## Releases

A tag `v<semver>` makes a release ([`release.yml`](.github/workflows/release.yml))
with `natives.zip` (the four natives), `fixtures.zip` (the test fixtures and
the host-target injector and shim), `contract.zip` (`contract/artifacts.json`,
`contract/env.json` and the protocol's golden vectors), `natives-key.txt` and
`SHA256SUMS`. Check a download with `sha256sum -c SHA256SUMS`, and the
natives' signatures with `bin\check-signatures.ps1 -Path <files> -Signer
'CN=Black Tree Gaming Ltd, O=Black Tree Gaming Ltd, L=Exeter, C=GB'`.

A tag without a `-` is a full release, and its natives are signed (SSL.com
eSigner, [`bin/sign.ps1`](bin/sign.ps1)). A tag with one (`v0.2.0-rc.1`) is a
prerelease for testing, with unsigned natives unless a manual run asks for
signing. When an earlier full release has the same natives key, its signed
natives are reused byte for byte, so a release that changes no native signs
nothing. `bin\sign.tests.ps1` tests the signing script against fake tools.

## License

GPL-3.0-or-later ([LICENSE](LICENSE)).
