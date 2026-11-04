# Typed protocol and typed ends (phases 1 and 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The wire is defined once, as types, in `aether-protocol`; the shim, the ring's typed client and server, and the test directors all use those types, and no literal opcode or status number is left outside the table.

**Architecture:** A `macro_rules!` table in `aether-protocol` generates `Opcode`, `Status`, `Request<'a>`, `Reply<'a>`, the codec and proptest strategies. A new crate `aether-rpc` (above `aether-protocol` and `aether-ipc`) holds the typed `Client`, the `Director` trait and `serve`. A new crate `aether-refdirector` holds the reference director (from the shim tests' fake director) and a null director. A new crate `aether-filering` maps a file ring on any OS.

**Tech Stack:** Rust (toolchain in `rust-toolchain.toml`), `bitflags` 2 (already in `Cargo.lock`), `proptest` (dev / `testing` feature), `memmap2` for the file ring.

**Spec:** `docs/superpowers/specs/2026-11-03-protocol-and-conformance-design.md`

## Global Constraints

- No wire change: every line of `crates/aether-protocol/tests/golden/vectors.txt` stays byte-identical; wire version stays 4; `contract/env.json` and `contract/artifacts.json` unchanged.
- No behaviour change the shim's tests can see; runtime text (`vfs-shim hook stats`, `[vfs-injector]`, `VFS_*`) unchanged.
- The shim stays allocation-light: the typed layer adds no heap allocation on any path the shim takes. Requests and replies borrow from the payload; encoding writes into a caller-supplied buffer.
- `aether-ipc` gains no OS dependency; `aether-protocol` stays `#![forbid(unsafe_code)]` and OS-free.
- No literal opcode or status number outside the protocol table once Task 5 is done (tests that assert the historical numbers excepted).
- Every task ends green: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --no-fail-fast` (Windows; `TMP`/`TEMP` must have no 8.3 alias: use `C:\vfstmp`). Build the inject artifacts first, as CI does: `cargo build -p aether-inject -p aether-shim -p aether-fixtures -p aether-fixture-vproxy` then `cargo build -p aether-fixture-staticimp`.
- Commit messages: plain, no AI attribution lines. Add files by name, never `git add -A`.
- Doc comments and docs are plain prose (no marketing words, no em dashes).

## Review Focus

- A reply or request that is truncated, has trailing bytes, bad UTF-8, an out-of-range bool/enum byte, or a count larger than the input: decode must return an error, never panic or over-allocate (today's decoders promise this; the typed ones must too). Task 1 property test.
- READ replies: inline versus bulk is chosen by the high bit of `bytes_read`, and a request's `FLAG_READ_BULK`; a typed decode must keep that exact rule. Golden vectors `read-resp-abcd`, `read-resp-bulk-len5-off65536`.
- Reserved opcodes 4 and 12 and any number from 25 up are `BadRequest` with an empty payload from `serve`. Task 3 test.
- `RENAME`'s `from_len` framing and `STORED_NAMES`' `/`-joined reply: borrowed decoders must not split at the wrong place for names containing `/`-free but multi-byte UTF-8. Task 1 property test with non-ASCII paths.
- The shim decodes a reply only when status is OK, and a decode failure becomes `BadRequest`: keep that in `Client`. Task 3 test.

---

### Task 1: The protocol table in `aether-protocol` (model: opus)

**Files:**
- Create: `crates/aether-protocol/src/table.rs` (the macro and its invocation), `crates/aether-protocol/src/field.rs` (the `Field` trait and impls), `crates/aether-protocol/src/buf.rs`
- Modify: `crates/aether-protocol/src/lib.rs`, `status.rs`, `Cargo.toml`
- Test: `crates/aether-protocol/tests/golden.rs` (produce every vector through the typed API), `crates/aether-protocol/tests/typed.rs` (round trip, malformed input), `crates/aether-protocol/tests/alloc.rs` (counting allocator)

**Interfaces:**
- Produces:
  - `pub enum Opcode` `#[repr(u32)]`: `GetAttr=1, ReadDir=2, Open=3, Read=5, Write=6, SetAttr=7, Rename=8, Delete=9, Mkdir=10, Close=11, Heartbeat=13, StoredNames=14, RegLookup=15, RegKey=16, RegSetValue=17, RegDeleteValue=18, RegCreateKey=19, RegDeleteKey=20, RegRenameKey=21, RegChanged=22, RenameX=23, StageExec=24`; `impl TryFrom<u32>` (error for 0, 4, 12, 25+); `Opcode::ALL: &[Opcode]`; `Opcode::name(self) -> &'static str` (today's `OPCODES` names: `"getattr"`, `"stored-names"`, `"reg-lookup"`, `"rename-x"`, ...).
  - `pub enum Status` `#[repr(i32)]`: `Ok=0, NotFound=-1, NotADirectory=-2, BadRequest=-3, IoError=-4, IsDir=-5, BadFh=-6, NoSpace=-7, NotSupported=-8, ReadOnly=-9, Exists=-10, ReplyTooLarge=-11, CrossRoot=-12`; `TryFrom<i32>`, `ALL`, `name()` (today's `STATUSES` names).
  - `bitflags! pub struct OpenFlags: u32 { READ=1, WRITE=2, CREATE=4, EXCL=8, TRUNC=16, APPEND=32 }`; `pub struct RequestFlags` or consts for `FLAG_READ_BULK = 0x1`.
  - `pub trait Buf { fn put(&mut self, bytes: &[u8]); }` for `Vec<u8>`; and a fixed-capacity writer over `&mut [u8]` that reports overflow (for encoding straight into a ring slot).
  - `pub enum Request<'a>`: one variant per opcode with named fields, paths as `&'a str`, data as `&'a [u8]`; `fn opcode(&self) -> Opcode`; `fn flags(&self) -> u32`; `fn encode(&self, out: &mut impl Buf)`; `fn decode(op: Opcode, flags: u32, payload: &'a [u8]) -> Result<Request<'a>, DecodeError>`.
  - `pub enum Reply<'a>`: per opcode (`GetAttr(AttrResp)`, `ReadDir(DirEntries<'a>)` decoding lazily, `Open(OpenResp)`, `Read(ReadReply<'a>)` = `Inline(&'a [u8]) | Bulk { len: u32, arena_offset: u64 }`, `Write(u32)`, `Empty` for the ops that reply with nothing, `StoredNames(&'a str)`, `StageExec(&'a str)`, and the registry replies); `fn encode(&self, out: &mut impl Buf)`; `fn decode(op: Opcode, req_flags: u32, payload: &'a [u8]) -> Result<Reply<'a>, DecodeError>`. A registry `Node` reply may decode to an owned `Node` (the shim stores it owned already).
  - `pub struct DecodeError` (opaque, `Debug`, `Display`).
  - Behind feature `testing`: `proptest` strategies `any_request()`, `any_reply_for(Opcode)` (or `Arbitrary` impls), generated from the table's field list.
  - The old `OP_*`/`ST_*`/`OPEN_*` constants, `OPCODES`, `STATUSES` and the `encode_*`/`decode_*` functions stay for now, re-implemented on top of the typed codec (no second implementation); Task 5 removes them. `aether_ipc::layout`'s re-export keeps compiling.

- [ ] **Step 1: Write the failing tests.** `golden.rs`: every vector produced through `Request::encode`/`Reply::encode` (and decoded back to the same value). `typed.rs`: proptest round trip `decode(encode(x)) == x` for every request and reply (non-ASCII paths included); every opcode number from 0 to 64 through `Opcode::try_from` (only the 22 live ones succeed); every proper prefix of every golden vector, and every vector plus one trailing byte, decodes to `Err` (except where the format allows a trailing rest-of-payload field: those are listed in the test with the reason); `Opcode::ALL`'s names and numbers equal today's `OPCODES`, `Status::ALL`'s equal `STATUSES`. `alloc.rs`: with a counting global allocator, decoding each golden request and each non-registry reply allocates 0 times, and encoding into a `Vec` with enough capacity allocates 0 times.
- [ ] **Step 2: Run them, watch them fail** (`cargo test -p aether-protocol`): fails to compile on the missing types.
- [ ] **Step 3: Implement the table, `Field`, `Buf`, the enums and the codec.** Re-point the old free functions at it.
- [ ] **Step 4: Run** `cargo test -p aether-protocol --features testing` and the old tests: all pass, `vectors.txt` unchanged (`git diff --exit-code crates/aether-protocol/tests/golden/vectors.txt`).
- [ ] **Step 5: Whole-workspace gate** (fmt, clippy, inject artifacts, `cargo test --no-fail-fast`), then commit: `aether-protocol: the wire as one typed table`.

### Task 2: `contract/protocol.json` (model: sonnet)

**Files:**
- Modify: `crates/aether-protocol/src/table.rs` (a `describe` per field and message), `crates/aether-protocol/src/lib.rs` (`pub fn contract_json() -> String`)
- Create: `contract/protocol.json`, `crates/aether-protocol/tests/contract.rs`
- Modify: `.github/workflows/release.yml` (add `contract\protocol.json` to `contract.zip`), `docs/wire.md` (section 3.3 and section 8: point at `protocol.json`), `README.md` if it lists the contract files

**Interfaces:**
- Consumes: Task 1's table and `Field`.
- Produces: `aether_protocol::contract_json() -> String`, stable key order, pretty-printed, ending in a newline; shape: `{"wire_version":4,"opcodes":[{"name","number","request":[fields],"reply":[fields]}],"reserved_opcodes":[4,12],"statuses":[{"name","number"}],"open_flags":[{"name","bit"}],"request_flags":[...]}` where a field is `{"name","type"}` and `type` is one of a closed list (`u8`, `bool`, `u32`, `u64`, `i64`, `str_rest`, `str_u32`, `bytes_u32`, `bytes_rest`, ...; nested structures for readdir entries and registry nodes). Follow how `aether_env::contract_json()` and its test are written (`crates/aether-env`).

- [ ] **Step 1: Failing test** `checked_in_protocol_json_is_current`: `contract/protocol.json` equals `contract_json()`, with the message telling how to regenerate (same pattern as env.json's test).
- [ ] **Step 2: Run, watch it fail.**
- [ ] **Step 3: Implement `describe`/`contract_json`, generate the file, wire it into release.yml and the docs.**
- [ ] **Step 4: Run** `cargo test -p aether-protocol`; then the workspace gate.
- [ ] **Step 5: Commit** `contract/protocol.json: the protocol table as data`.

### Task 3: `aether-rpc`: typed `Client`, `Director`, `serve` (model: opus)

**Files:**
- Create: `crates/aether-rpc/{Cargo.toml,src/lib.rs,src/client.rs,src/serve.rs}`, `crates/aether-rpc/tests/roundtrip.rs`
- Modify: root `Cargo.toml` (member + workspace dependency, with a one-line comment like the others)

**Interfaces:**
- Consumes: Task 1's `Request`, `Reply`, `Opcode`, `Status`, `Buf`; `aether_ipc::{RingClient, RingServer, Notifier, Response}`.
- Produces:
  - `pub struct Client<'a, N: Notifier>` wrapping `RingClient<'a, N>`; `fn call<'r>(&self, req: &Request<'_>, scratch: &'r mut Vec<u8>) -> Result<Reply<'r>, CallError>` (or a shape that avoids the extra copy if `RingClient` can be extended without changing the ring protocol); `CallError = Status(Status) | Undecodable | Ipc(IpcError)`. A non-OK status never decodes the payload; an undecodable OK reply is reported as `Undecodable` (the shim maps it to `BadRequest`, as `docs/wire.md` 3.1 says).
  - `pub trait Director` with one method per `Request` variant, each returning `Result<Reply<'_>, Status>` or writing into a reply buffer (builder's choice, allocation-light), and a default `fn unknown(&mut self, opcode: u32) -> Status { Status::BadRequest }`.
  - `pub fn serve_one<N: Notifier>(server: &RingServer<'_, N>, d: &mut impl Director) -> Result<bool, IpcError>` and a loop helper; a payload that fails to decode, a reserved opcode or an unknown number is answered `BadRequest` with an empty payload; an error status always has an empty payload.
- [ ] **Step 1: Failing tests** over an in-process `OwnedSeg` ring with `SpinNotifier`: a tiny `Director` that echoes paths; `Client::call` for one request of each opcode gets the reply back; opcodes 0, 4, 12, 25 sent raw through `RingClient::submit` are `BadRequest` with empty payload; a director returning `Err(Status::NotFound)` reaches the client as `CallError::Status(NotFound)`; an OK reply with garbage payload is `Undecodable`.
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p aether-rpc`; workspace gate.
- [ ] **Step 5: Commit** `aether-rpc: a typed client and server over the ring`.

### Task 4: `aether-refdirector`: the reference and null directors (model: sonnet)

**Files:**
- Create: `crates/aether-refdirector/{Cargo.toml,src/lib.rs,src/reference.rs,src/null.rs}`, `crates/aether-refdirector/tests/`
- Modify: `crates/aether-hooks/tests/fakedirector/mod.rs` (becomes a thin layer: installing the ring for the shim tests, using the crate's `Reference` as the `Director`), root `Cargo.toml`, `crates/aether-hooks/Cargo.toml` (dev-dependency)

**Interfaces:**
- Consumes: Task 3's `Director`, `serve_one`; `aether_registry::Overlay`.
- Produces: `aether_refdirector::Reference` with the builder knobs the fake has today (`new`, `with_registry`, `with_dir`, `writable_under`, `failing_writes_under`, `with(vpath, bytes, ReadStyle)`, `contents`, `detach_registry`/`attach_registry`) and its `Tally` (opens, closes, reads, bulk reads, writes, deletes, renames, registry ops by `Opcode`, not by number); `impl Director for Reference`; `aether_refdirector::Null` (every op answered OK with a minimal valid reply: found=false attrs, empty listing, fh 1, zero-length read) for the ring floor.
- [ ] **Step 1:** Move the fake's behaviour tests (and add, for each opcode the fake answers, one direct `Director` test with the reply it gives today) into the new crate; they fail to compile.
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement** by moving the code, replacing the `match` on `P::OP_*` with the `Director` methods.
- [ ] **Step 4:** `cargo test -p aether-refdirector`; every `aether-hooks` test passes unchanged (same count as before the task: record it first); workspace gate.
- [ ] **Step 5: Commit** `aether-refdirector: the shim tests' director as a crate`.

### Task 5: The shim on the typed API; old constants and functions removed (model: opus)

**Files:**
- Modify: `crates/aether-hooks/src/director.rs`, `regclient.rs`, `hook/*.rs`, `hookstats/*.rs`, `synth_file.rs`; `crates/aether-ipc/src/{layout.rs,concurrent.rs,arena.rs}`; `crates/aether-inject` if it uses them; every test that names `OP_*`/`ST_*`; `crates/aether-protocol/src/lib.rs` (delete the old API); `docs/wire.md` (its "Source:" lines name the new items); `CHANGELOG.md` (an Unreleased entry for the Rust API change; the wire does not change)
- Test: `crates/aether-protocol/tests/no_literals.rs` or a testkit check: no `OP_`/`ST_` constant and no `encode_*`/`decode_*` free function is exported by `aether-protocol` any more (a compile-level check is enough: the test names the new API only)

**Interfaces:**
- Consumes: Tasks 1 and 3.
- Produces: the shim sends every request through `aether_rpc::Client` (or the typed encode into its existing slot path where `Client` doesn't fit, e.g. `DataGate`'s fragmented reads, with a ruling ledgered), matches statuses as `Status`, and holds no literal opcode/status number. Hookstats keys stay the same strings.
- [ ] **Step 1:** Record the workspace test count and the counted allocations of Task 1's `alloc.rs`. Add to `crates/aether-hooks` a test that the registry client and the director client encode exactly the golden bytes for one request each (if one exists already, reuse it).
- [ ] **Step 2:** Migrate the call sites crate by crate; delete the old API last, so the compiler finds every remaining user.
- [ ] **Step 3:** `rg -n "OP_[A-Z]|ST_[A-Z]" crates --type rust` finds only the historical-numbers test; the workspace gate passes with the same test count (plus new tests).
- [ ] **Step 4: Commit** `The shim speaks the typed protocol; the numeric API is gone`.

### Task 6: `aether-filering`: a file ring on any OS (model: sonnet, independent of Tasks 1 to 5)

**Files:**
- Create: `crates/aether-filering/{Cargo.toml,src/lib.rs,tests/ring.rs}`
- Modify: root `Cargo.toml`

**Interfaces:**
- Consumes: `aether_ipc::{RingBacking, SharedSeg, Geom, RingClient, RingServer, SpinNotifier}`; `docs/wire.md` 2.10 (the file ring under Wine) for what the file must look like.
- Produces: `pub struct FileRing` implementing `aether_ipc::RingBacking`; `FileRing::create(path, bytes) -> io::Result<FileRing>` (director side: creates and sizes the file) and `FileRing::open(path) -> io::Result<FileRing>` (client side: maps an existing file, length from the file); `#![allow(unsafe_code)]` only around the mapping, with a comment saying why it is sound (shared mapping, the ring's own atomics synchronise).
- [ ] **Step 1: Failing test:** create a file ring in a temp dir, initialise a ring on it (as a server would), open it a second time through `FileRing::open`, and round-trip a request between a `RingClient` on one mapping and a `RingServer` on the other on two threads.
- [ ] **Step 2: Run, watch it fail.**
- [ ] **Step 3: Implement** with `memmap2` (`MmapMut` over a `File`, `map_mut`), building a `SharedSeg` the way `aether-win`'s `SharedMapping` does.
- [ ] **Step 4:** `cargo test -p aether-filering` on Windows; if the Linux CI job builds the portable crates by list, add this crate to it (read `.github/workflows/ci.yml`); workspace gate.
- [ ] **Step 5: Commit** `aether-filering: a ring in a mapped file, on any OS`.

## Execution order

Task 6 runs in parallel with Tasks 1 to 3 (it touches only its own crate and the members list). Tasks 2 and 3 both follow Task 1 and may run in parallel. Task 4 follows 3; Task 5 follows 4. Phases 3 to 5 (`aether-conform`, the shim-side harness, benchmarks) get their own plan once this one is merged, built on `aether-rpc`, `aether-refdirector` and `aether-filering`.
