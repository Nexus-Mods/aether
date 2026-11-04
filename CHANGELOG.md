# Changelog

What each Aether release changes for a director or a program built on
Aether's crates. Newest first.

## Unreleased

The wire is unchanged: the wire version (4), the golden vectors,
`contract/env.json` and `contract/artifacts.json` are as in 1.0.0.

### Added

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
  an arena. `docs/conformance.md` describes the adapter protocol and every
  rule that relaxes equality.
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
- The natives' sources change (the shim compiles in `aether-rpc`), so the
  next release signs new natives. The natives key leaves out the crates no
  native depends on, and releases check that exclusion with
  `bin/natives-key.tests.ps1`.

## 1.0.0

The first release.

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
