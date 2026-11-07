# Changelog

What each Aether release changes for a director or a program built on
Aether's crates. Newest first.

## Unreleased

### Added

- A director that serves no registry says `"registry": false` in its mount
  answers, as one that serves no overlays says `"layers": false`, and
  refuses the mount of a tree with a registry layer. The run skips the
  scenarios with a registry layer or registry steps, generated cases,
  concurrent rounds, `--malformed` and `--limits` send no registry request,
  and the `reg_gen` rules are not checked for it (`docs/conformance.md`,
  "The adapter protocol"; `contract/scenarios/README.md`, "Registry").

## 1.1.0

The wire is unchanged: the wire version (4), the golden vectors and
`contract/artifacts.json` are as in 1.0.0, and `contract/env.json` gains one
injector input (`VFS_INJECT_NEW_CONSOLE`, below). The shim
sends a director that follows `docs/wire.md` the same requests, except where
an entry below says otherwise (a `CreateDirectoryW` is a GETATTR then a
MKDIR, and starting a managed child process is two requests). What the shim
does when a director answers outside the contract is written down
(`docs/wire.md` 3.7), and several of the fixes below are what a program sees
when one does.

### Fixed

- `aether-registry`'s overlay, and with it the reference director, checks
  the key-name length and depth limits on every registry write
  (`docs/wire.md` 4.1, 4.3). REG_DELETE_VALUE and REG_DELETE_KEY created
  keys at any depth and with any name length, and REG_RENAME_KEY did not
  check its path's names or the depth of the keys it moves; each is now
  `ST_BAD_REQUEST`. Scenario `registry-write-depth` checks it.
- The shim, files and directories:
  - `CreateDirectoryW` of a directory that exists fails
    `ERROR_ALREADY_EXISTS`, and an open-if of one reports `FILE_OPENED`,
    whichever of `ST_OK` and `ST_EXISTS` the director answers to MKDIR
    (`docs/wire.md` 3.4.9). The shim asks GETATTR first, so a directory
    create is two requests.
  - A directory the shim creates has no handle at the director, so closing
    it sends no CLOSE. A director that issues handle 0 for a file keeps that
    file open when a program creates a directory.
  - A READDIR that fails, or whose listing does not fit `payload_cap`,
    fails the enumeration with `ERROR_GEN_FAILURE`, and the next query asks
    again. Only `ST_NOT_FOUND` is an empty directory (`docs/wire.md`
    3.4.2).
  - A write that fails part way reports the bytes that landed with the
    failure status, `STATUS_DISK_FULL` for `ST_NO_SPACE`, and leaves the
    file pointer where the call found it, as NTFS does.
  - A file's times agree across listings, path queries and handle queries;
    the first time query on a handle asks GETATTR, and a write drops the
    time every handle on the file kept (`docs/wire.md` 3.2).
  - A stored name that does not fold to the component asked about is
    ignored, not remembered as the file's spelling. A size or position set
    from a buffer that is not aligned is read where it lies.
  - Starting a managed child process costs two requests, GETATTR and
    STAGE_EXEC. The check of the child's backing directory reads the disk and
    asks the director nothing.
- The shim, mapped files:
  - A demand-paged section keeps its director handle until its file handle,
    the section and its views are all gone, so a mapping outlives the
    `CloseHandle` of its file. `UnmapViewOfFile` of a view of a director file
    succeeds (the shim hooks `NtUnmapViewOfSectionEx`).
  - A page fill that fails or comes back short is asked again, up to three
    more times, and then raises `STATUS_IN_PAGE_ERROR`; it is never filled
    with zeros. An eager section whose read fails or comes back short fails
    `STATUS_UNSUCCESSFUL`; a `SEC_IMAGE` section's read is asked again, up to
    three more times, before it does. A demand-paged chunk is
    readable only once its fill is in place, and a region whose own
    reservation cannot be had takes private memory (`docs/wire.md` 3.4.4).
  - A mapping larger than the address space of a 32-bit process fails
    `STATUS_SECTION_TOO_BIG`; sizes are 64-bit there.
- The ring: a header whose `payload_cap` is within 32 bytes of `2^32` is
  refused by a 32-bit client too (`docs/wire.md` 2.4). Its stride wrapped in
  32-bit arithmetic, so such a ring was opened with slots too small for the
  cap it stated.
- The shim, reads:
  - A bulk READ reply is copied only from its own slot's bank, found from
    `VFS_ARENA_OFFSET` (which the shim now reads, so it must be accurate
    when set: `docs/wire.md` 5.3), and only as many bytes as were asked; a
    reply that names other memory, or more bytes than the request, is a
    failed read and not file data.
  - An arena smaller than `docs/wire.md` 2.6 allows (256 KiB times the slot
    count) is not used: reads are inline and answered in full.
  - Nothing under a path the process deleted or renamed is cached again,
    whether it was opened before or not.
- The shim, the registry:
  - A key create the director answers `ST_EXISTS` looks the key up afresh.
  - A registry write finds a value as a read does.
  - A key whose overlay does not fit one reply (`ST_REPLY_TOO_LARGE`, or the
    ring's overflow) fails every read of it with
    `STATUS_REGISTRY_IO_FAILED` (`ERROR_REGISTRY_IO_FAILED`), and is not
    answered from the real key (`docs/wire.md` 4.4).
- `docs/wire.md` 3.7 is the table of what the shim does with each reply a
  director should not give, per operation: a status, a truncated, padded or
  oversize payload, a bulk reply out of place, a reply that is late or
  never comes.

### Added

- `VFS_INJECT_NEW_CONSOLE=1` makes `aether-injector` start its target with a
  console of its own (`CREATE_NEW_CONSOLE`). A console program that sets its
  title or reads keys fails without one, and the injector's, under Wine with
  its output piped, is no console. Unset, the target shares the injector's
  console as before. `aether_inject::RunConfig` gains the field `new_console`;
  `docs/wire.md` 5.2 and 5.3 list the variable and `contract/env.json` names it
  in its `inject` table.

- `aether-conform`, a tool that checks any director against the contract.
  `aether-conform run --director "<adapter command>"` mounts each scenario's
  tree through the director's adapter, attaches to its ring as the shim
  would, plays the requests and compares the answers. `--malformed` sends
  requests that are not valid ones, `--limits` sends requests sized from the
  director's own ring, `--generate` plays generated trees and operation
  sequences against the reference and shrinks the first failure into a
  scenario file, and `--concurrent N` plays N clients at once over every slot
  and bank. It also checks what the shim caches on a director's word (the
  `immutable` flag and `reg_gen`) and what the mount's environment says of
  its tree. `aether-conform reference-adapter` serves the reference director
  behind the adapter protocol, over a ring file or a section, with or without
  an arena. `--timeout` bounds each ring request; the adapter's own answers
  (mount, unmount) have `--adapter-timeout`, default 60 seconds, so a short
  `--timeout` does not fail a slow mount. `docs/conformance.md` describes the
  adapter protocol and every rule that relaxes equality.
- `contract/scenarios/`: what every director must do, as data, shipped in
  `contract.zip`.
- `contract/protocol.json`: the protocol table (opcodes, fields, statuses,
  flags and field types) as data.
- `aether-rpc`: typed ring ends, the `Client` the shim calls the director
  with and the `Director` trait a director in Rust implements.
  `aether_rpc::ReadSlot` carries `inline_cap`, the most an inline READ reply
  may hold.
- `aether-refdirector`: the reference director (an in-memory tree and
  registry overlay that answers as `docs/wire.md` says) and a null director.
- `aether-filering`: a ring in a memory-mapped file on any OS
  (`FileRing::create`, `FileRing::open`), the Wine arrangement of
  `docs/wire.md` 2.10 for a native tool. `create` replaces an existing file
  rather than truncating it.
- Releases carry `aether-conform-windows-x64.zip` and
  `aether-conform-linux-x64.tar.gz` (unsigned), listed in `SHA256SUMS`.
- `aether-conform bench --director "<adapter command>"` times a director on
  the workloads of `contract/workloads/` (shipped in `contract.zip`: launch
  storm, random small reads, large listings, saves, sequential bulk reads)
  through the same adapter as `run`, and reports each opcode's latency over
  the ring's floor, measured on `aether-conform null-adapter`, a director
  that answers every request with the smallest valid reply and each READ
  with the bytes the director's reply gave, in the same form. `--clients`,
  `--quick`, `--out`, `--baseline` and `--fail-over` are described in
  `docs/conformance.md` and `docs/benchmarks.md`.
- `aether-shimbench`, which times eight kinds of Win32 call natively and under
  the shim, and counts the requests and ring bytes per call
  (`docs/benchmarks.md`). Criterion benches of the codecs, the case fold,
  path resolution, the read cache and the ring
  (`cargo bench -p <crate> --bench <name>`).
- `crates/aether-hooks/tests/harness/costs.txt`, the requests, ring bytes and
  allocations of each hooked call, checked on every push;
  `AETHER_COSTS_UPDATE=1` rewrites it.
- `aether-fuzz` and `fuzz/`: cargo-fuzz targets for the reply and request
  decoders, the shim's config, the ring's header and response slot, directory
  records and the `VFS_*` environment, replayed on stable Rust on every push
  and fuzzed weekly by `fuzz.yml`. `bench.yml` compares a base commit with
  the head on one runner, by hand.
- `aether_rpc::Tap`, `TapAction`, `serve_tapped` and `serve_one_tapped`: a
  hook on the serve loop that sees each request and may answer it or change
  its reply before it is sent.
- `aether_refdirector::harness`: `Recorder` (what crossed the ring), `Faults`
  (replies a director should not give), `Override` (replies it may vary),
  `Geometry` and `serve_threads_of`, for testing a client against the
  reference.
- `aether_env::session`: the parsing of the `VFS_*` variables (the ring's
  size and cap, its source, the roots and their backings, the switches, the
  timeouts) as pure functions of their text. The shim and `aether-conform`'s
  attach check call them.
- `aether_casefold::eq_folded` compares two names by their fold without
  building either.

- `aether-conform soak --director "<adapter command>" --for 2h` plays
  generated cases and concurrent rounds in batches, each from a fresh seed,
  and writes every distinct shrunk failure as a scenario file with its seed
  and the command that replays it. The shim's generated fault properties have
  the same, set by `AETHER_FAULTS_SOAK` and its companions. Both run nightly
  in `.github/workflows/soak.yml`, never on a push (`docs/conformance.md`,
  "Soak runs"; `docs/benchmarks.md`).
- A scenario's `one_of` may carry a `choice` label, so that several steps
  show one of the outcomes `docs/wire.md` allows together (`overlay-rename-both`).
- The scenario tree format: a root may name `lower`, read-only layers beneath
  its writable upper (`contract/scenarios/README.md`, "Layers"). The JSON is
  backward compatible (absent, no layers; written only when there are some).
  `aether_refdirector::tree::Root` has a new public field, `lower`, so Rust
  code that builds a `Root` with a struct literal adds it (or uses
  `..Root::default()`), and an adapter built on an older crate refuses a tree
  that has one (unknown field), rather than mount it without its layers.
- The reference director serves a root with lower layers as an overlay
  (`docs/wire.md` 3.4.9), and the scenarios `overlay-*` and the generated
  cases check a director that does (`docs/conformance.md`, "Layered roots").
  A director that serves no overlays says `"layers": false` in its mount
  answers, and the run skips the layered scenarios and generates no layered
  roots for it (`docs/conformance.md`, "The adapter protocol").

### Changed

- `aether-protocol`'s API is one typed table: `Opcode`, `Status`,
  `OpenFlags` and `RequestFlags` (`ALL`, `name()`, `code()`, `TryFrom`), and
  `Request` and `Reply`, which encode into any `Buf` and decode borrowing
  from a payload. The numeric constants (`OP_*`, `ST_*`, `OPEN_*`,
  `FLAG_READ_BULK`, `OPEN_RESP_IMMUTABLE`), the `OPCODES` and `STATUSES`
  tables, the per-message structs and the `encode_*`/`decode_*` functions for
  ring messages are gone; `shimcfg::encode_config` and `decode_config` stay.
  `aether_ipc::layout` re-exports `Opcode`, and
  `aether_ntlayout::open_create_flags` returns `OpenFlags`.
- The shim's `FuseClient` and `RegClient` methods return `Status` as their
  error, except `FuseClient::delete` (`CallError`, which separates a
  director's refusal from a call no director answered),
  `FuseClient::read_fragmented` (the `i32` of the bulk read) and
  `FuseClient::read_cached` (`Option<usize>`).
- `docs/wire.md` says where directors may differ and which answers each
  rule allows.
- `aether_ipc::ReadPlan` has two more public fields, `arena_len` and
  `arena_offset`, so that a bulk reply is checked against its own bank. Code
  that builds a `ReadPlan` sets them.
- `aether-refdirector` mounts a file of up to 512 MiB (a write still stops at
  64 MiB).
- The natives' sources change (the shim compiles in `aether-rpc`), so the
  next release signs new natives. The natives key leaves out the crates no
  native depends on, and releases check that exclusion with
  `bin/natives-key.tests.ps1`.
- `docs/wire.md` 3.4.7: RENAME of a directory that an overlay's upper and a
  lower both hold may be refused with `ST_IO_ERROR` or moved whole, never in
  part. 3.4.9: MKDIR of a directory only a lower holds is `ST_OK` or
  `ST_EXISTS`, and its listing is unchanged (no longer `ST_READ_ONLY`).
  `rename-x-same-root` accepts `ST_IO_ERROR` as well as `ST_BAD_REQUEST` for
  the root at an end, as `rename-root` does.
- `docs/wire.md` 3.4.9: the general rules stay allowed answers for a
  lower-only name (conformance accepts both); a copy-up keeps a file's
  identity, so a handle opened before it without `immutable` reads the copy
  (3.4.4); a read open of a lower-only file may answer `immutable`, such a
  handle keeps the lower's bytes while it is open, and once the file has been
  copied up or a write open of it succeeded no read open of it does (3.4.3).
  The reference opens lower-only files immutable, so the shim's read cache
  serves a writable root's game files.

## 1.0.0

The first release. The prerelease tag `v1.0.1-rc.1` is the same commit,
made to check that a release whose natives are unchanged reuses 1.0.0's
signed ones.

- **The shim**, `aether_shim.dll` (x64) and `aether_shim32.dll` (x86): NT
  file and registry detours that answer every operation under a managed root
  from a director over a shared-memory ring, follow the program into its
  child processes, and fail closed.
- **The injector**, `aether-injector.exe` (x64) and `aether-injector32.exe`
  (x86): starts the target suspended, loads the shim by remote-thread
  `LoadLibrary` (the only activation), and waits for the whole process tree.
- **The contract**: `docs/wire.md` (wire version 4), `contract/env.json` (the
  handshake environment), `contract/artifacts.json` (the natives' names) and
  the protocol's golden vectors, shipped in `contract.zip`.
- **Signed natives**: the four natives, signed and built with the C runtime
  linked in, in `natives.zip`, with `natives-key.txt` and `SHA256SUMS`; the
  test fixtures in `fixtures.zip`.
- The crates the natives are built from: `aether-hooks`, `aether-shim`,
  `aether-inject`, `aether-ipc`, `aether-protocol`, `aether-env`,
  `aether-casefold`, `aether-redirect`, `aether-ntlayout`, `aether-registry`,
  `aether-win` and `aether-pe`.
