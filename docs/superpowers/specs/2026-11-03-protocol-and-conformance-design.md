# One protocol definition, two test harnesses, and benchmarks

Status: approved in conversation, 2026-11-03.

## Why

Aether's wire is defined three times over: `aether-protocol`'s `OP_*`/`ST_*`
constants and hand-written `encode_*`/`decode_*` functions, the `match` arms
in the shim and in the test fake director, and Cairn's `proto.clj` table.
Nothing makes them agree. Adding an opcode compiles without any of them
noticing, and the only test a director in another language has is the golden
vectors, which check bytes and not behaviour.

Cairn's director (Clojure) and any third-party director need a harness that
speaks the protocol the way the shim does and tells them where they differ.
The shim needs a server that is hostile rather than polite. Both need
performance numbers that separate the ring's cost, the shim's cost and the
director's cost.

## Goals

1. **One definition of the protocol**, in `aether-protocol`, as types:
   `Opcode`, `Status` and `OpenFlags` are enums or bitflags, not loose
   integers, and every request and reply is a variant of a typed `Request` or
   `Reply`. Everything else is generated from that definition or checked
   against it. No literal opcode or status number appears outside the table.
2. **Two typed ends** over `aether-ipc`: a `Client` (what the shim uses) and
   a `Director` trait plus a `serve` loop (what every Rust-side server uses).
   The dispatch `match` is exhaustive, so a new opcode stops every director
   from compiling until it handles it.
3. **A reference director**, made from the shim tests' fake director, which
   loads its tree from a scenario file. It is the oracle for differential
   testing and the server for the shim's tests.
4. **`aether-conform`**, a binary that tests any director, in any language,
   over the real ring: hand-written scenarios plus generated operation
   sequences, compared against the reference director. It is the only
   protocol-layer test of Cairn's director (Cairn's protocol tests are retired
   once the conformance suite covers what they check).
5. **A shim-side harness**: the reference director behind `serve`, recording
   every request and injecting faults, so generated faults are checked never
   to crash the shim.
6. **Benchmarks for both ends**, kept off the main CI's pass/fail, plus
   counted metrics (allocations, round trips) that are deterministic and do
   gate CI.

## Non-goals and hard constraints

- **No contract change.** The wire bytes are identical: every line of
  `crates/aether-protocol/tests/golden/vectors.txt` stays as it is, the wire
  version stays 4, `contract/env.json` and `contract/artifacts.json` do not
  change, and the behaviour the shim expects of a director does not change.
  Writing down behaviour that was implicit (which statuses an operation may
  return, which reply fields are director-defined) is documenting the
  contract, not changing it; where the reference director and Cairn's
  director disagree, the shim's expectations decide, and the decision is
  recorded in `docs/wire.md`.
- **The shim stays allocation-light.** Typed requests and replies borrow from
  the payload (`Request<'a>`, `Reply<'a>`); encoding writes into a
  caller-supplied buffer. The typed layer adds no heap allocation to any path
  the shim takes; the counted-allocation tests (goal 6) record today's counts
  and fail if one goes up.
- **The code's shape may change**, including code compiled into the shim DLL.
  The natives key changes as a result, so the next release signs new natives.
- **Timed benchmarks are not on the main CI.** They run by hand or on a fixed
  machine. Counted metrics run on CI.
- Aether stays the shim and the injector. The reference director is a test
  and benchmark tool, never shipped as a director for games.
- Rust only. No Python; generative testing uses proptest.

## Design

### 1. The protocol table (`aether-protocol`)

One `macro_rules!` invocation lists every operation once: its name, its
opcode number, its request fields and its reply shape. Each field type
(`u32`, `u64`, `i64`, `bool` byte, `RootId`, a rest-of-payload UTF-8 path, a
length-prefixed string, bytes, a registry `Node`, a readdir listing, ...)
implements one trait, `Field`, which knows how to encode itself into a
buffer, decode itself borrowing from the payload, generate itself for
proptest (behind the `testing` feature), and describe itself for
`protocol.json`.

From the table the macro emits:

- `#[repr(u32)] enum Opcode` (reserved numbers 4 and 12 are not variants;
  `TryFrom<u32>` turns any unknown number into a decode error, never a
  panic), and `#[repr(i32)] enum Status`; `OpenFlags` as bitflags.
- `enum Request<'a>` with one variant per operation, `Request::opcode()`,
  `Request::encode(&self, &mut impl Buf)` and
  `Request::decode(Opcode, flags, &'a [u8])`.
- `enum Reply<'a>` likewise; decoding a reply needs the request's opcode
  (and, for READ, its flags: inline or bulk).
- proptest strategies for every request and reply, generated from the same
  field list, so the generators cannot drift from the codec.
- `contract/protocol.json`: opcodes, statuses, flags and each message's
  field layout. A test fails if the checked-in file differs from what the
  code produces, as `env.json`'s test does. It ships in `contract.zip`.

The existing `encode_*`/`decode_*` functions and `OP_*`/`ST_*` constants are
replaced by the typed API. Every golden vector is produced by the typed
encoder and must come out byte-identical; the golden test is the proof that
the refactor changed no byte. The opcode and status numbers are never
renumbered (`wire_numbers_are_the_historical_ones` stays).

`shim.cfg` (VFSC v2) gets the same treatment where it helps: a typed struct
with one encoder and decoder, checked against its two golden vectors.

### 2. The typed ends

- `Client` wraps `aether_ipc::RingClient`: it takes a `Request`, encodes it
  into the slot (no intermediate `Vec` where the ring API allows), and
  decodes the reply into a borrowed `Reply` or a `Status`. The shim's
  director calls and its registry client go through it.
- `trait Director` has one method per operation, taking the decoded request
  and returning the reply or a `Status`. `serve(server, &mut impl Director)`
  decodes, dispatches with an exhaustive `match`, and encodes. A request
  that fails to decode is answered `Status::BadRequest` with an empty
  payload, as today.
- Where these live (in `aether-protocol` behind a feature, or a small
  `aether-wire` crate between `aether-protocol` and `aether-ipc`) is the
  builder's call, constrained by the dependency graph: `aether-ipc` must not
  gain OS dependencies.

### 3. File-backed ring on any OS

`aether-conform` attaches to a director's ring the way the shim does: it
reads the handshake environment (`VFS_RING_PATH` or `VFS_RING_SECTION`,
`VFS_RING_BYTES`, ...). A Windows section is opened through `aether-win`.
A file ring needs a `RingBacking` over a mapped file on Linux, macOS and
Windows; Aether lost its Unix one with `aether-unix`, so a small one is added
(in the conformance crate or its own crate; not in `aether-ipc`, which
imports no OS API). With the spin notifier this runs natively on Linux, so
conformance runs on Linux CI without Wine.

### 4. The reference director (`aether-refdirector`)

The fake director in `crates/aether-hooks/tests/fakedirector/` moves into a
crate, implements `Director`, and keeps the knobs the shim tests use (read
styles, writable and failing-write prefixes, the registry overlay, request
tallies). It gains loading from a scenario's tree. It stays in memory and
small. It is the oracle in differential tests, so its behaviour on every
operation must match what `docs/wire.md` says the shim expects; where it was
sloppy, it is fixed and the shim's tests show the fix changes nothing they
rely on.

A **null director** (answers everything at once with a fixed reply) also
implements `Director`, for measuring the ring floor.

### 5. Scenarios

`contract/scenarios/*.json`, shipped in `contract.zip`. A scenario is:

- `tree`: roots, each with files (path, contents or size plus a seed,
  mtime), directories, which prefixes are writable, and registry keys;
- `steps`: requests, written as JSON forms of `Request` (names, not
  numbers), each with its expected reply or status;
- per-field comparison rules where the contract leaves a value to the
  director: file handles are compared up to a consistent renaming, and
  any other director-defined field is named in the scenario and in
  `docs/wire.md`.

The same file drives the reference director (it serves the tree) and
`aether-conform` (it plays the steps). Scenario JSON is parsed into the typed
`Request`/`Reply`, so a scenario that names an unknown operation or field
fails to load.

### 6. `aether-conform`

```
aether-conform run   --director "<command>" [--scenarios DIR] [--seed N] [--cases N] [--report json|tap]
aether-conform bench --director "<command>" --workloads DIR [--out FILE]
aether-conform self-test   # runs against the in-process reference director
```

**Adapter protocol.** `aether-conform` starts the director under test as a
child process, once per run, and talks to it over stdin/stdout in JSON lines:

- `{"op":"mount","scenario":"<path>"}` → the adapter creates a fresh ring,
  writes its `shim.cfg`, mounts the scenario's tree, and answers
  `{"ok":true,"env":{"VFS_RING_PATH":"...", ...}}` with the handshake
  variables a shim would get. `aether-conform` attaches with them.
- `{"op":"unmount"}` → the adapter tears the ring down and answers
  `{"ok":true}`.
- stdin closing means exit.

One process per run, not per case: a JVM director starts once, and every
generated case still gets a fresh ring and a fresh tree. The adapter is the
only code a director writes for conformance.

**Scenario mode** plays every scenario's steps and checks every reply.

**Generative mode** (proptest with proptest-state-machine) generates a tree
and a sequence of operations: opens with every flag combination, reads at
odd offsets and lengths (inline and bulk), writes, setattr, rename and
rename-x over existing names, delete while open, mkdir, readdir, stored-names,
the same names in different case, invalid handles, and registry operations.
Each case is run against the director under test and against the reference
director in-process; the first reply that differs (after the comparison
rules) fails the case, proptest shrinks it, and the shrunk case is written
out as a scenario file ready to check into `contract/scenarios/`. Each run
prints its seed; `--seed` replays it.

It also sends malformed requests (truncated payloads, bad UTF-8, unknown and
reserved opcodes, trailing bytes) and checks the director answers
`BadRequest` and keeps serving.

**Reports** are JSON (and TAP for CI logs): per scenario and per generated
case, pass or fail, with the request, both replies and the seed.

`aether-conform` ships in each release for Windows x64 and Linux x64, beside
the natives, unsigned (it is a test tool, not loaded into games).

### 7. The shim-side harness

The shim's tests serve through `serve` with the reference director wrapped
in two layers:

- **recording**: every request the shim sends is kept, so a test can assert
  what crossed the ring (opcodes as `Opcode`, paths, flags), not only what
  Win32 returned;
- **fault injection**: replies replaced by generated faults: every `Status`
  for every operation, replies that are truncated, oversized or have
  trailing bytes, bulk replies pointing outside the arena, and slow replies.
  The property is that the shim never crashes or hangs past its deadline,
  and every fault surfaces as the NTSTATUS `docs/wire.md` says.

The in-process decoders the shim runs on director output (`Reply` decoding,
`shim.cfg`, registry nodes) also get `cargo-fuzz` targets. Those run by hand
or on a schedule, not on every push.

### 8. Benchmarks

Three costs, measured separately:

| Cost | How |
|---|---|
| Ring floor | typed `Client` against the null director: latency per round trip, throughput as 1..N client threads contend for slots |
| Director | `aether-conform bench` against a director (reference or Cairn's): latency per opcode (p50/p99/p99.9, HDR histograms), bulk-read bandwidth, throughput under N clients; minus the ring floor |
| Shim | fixture programs making Win32 calls under the shim (null or reference director) against the same calls made natively: paths in the root, paths outside it (passthrough), read-cache hits |

Workloads are data: `contract/workloads/*.json` (an open/getattr storm shaped
like a game launch, sequential and random bulk reads, readdir over large
folders, a write-heavy save), so every director runs the same ones.
Micro-benchmarks for in-process code (the codec, the case fold, path
normalization) use criterion.

**Counted metrics** run in the normal test suite and gate CI: heap
allocations per encode and decode, and per hooked call on the shim's paths
(a counting allocator in the tests), round trips per Win32 call, bytes copied
per read. Each has a recorded expected value; a test fails if the count goes
up, and a deliberate change updates the value in the same commit.

**Timed metrics** are written as JSON with the machine's details (CPU, OS,
build) and compared against a baseline from the same machine. They never fail
a shared CI run.

## Phases

1. Typed protocol table, golden vectors byte-identical, `protocol.json`,
   counted allocations for the codec.
2. Typed `Client`, `Director` and `serve`; the shim and the fake director
   moved onto them; the reference and null directors as a crate; the file
   ring backing.
3. Scenarios and `aether-conform` (scenario, generative and malformed modes,
   adapter protocol, reports), run in CI against the reference director on
   Linux and Windows, shipped in releases.
4. The shim-side harness: recording, fault injection, fuzz targets, counted
   round trips and allocations per hooked call.
5. Benchmarks: null-director ring floor, `aether-conform bench`, workloads,
   criterion micro-benchmarks, the shim cost fixtures.
6. Later, in Cairn and on its own plan: the conformance adapter, the tables
   in `proto.clj` built from `protocol.json`, and Cairn's protocol tests
   retired once the suite covers them (inventory first).

## Left to the builder

Macro versus hand-written tables if the macro gets unreadable (the
requirement is one definition, checked exhaustively, not a particular
technique); crate boundaries within the dependency rules above; the exact
JSON shape of scenarios, reports and workloads; which proptest settings
(cases, shrink iterations) CI uses, as long as a CI run stays under a few
minutes.
