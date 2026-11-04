# Aether wire contract

This is the byte-level contract between the Aether shim (the DLL injected into
a game) and a **director** (the process that serves it). It is written so that
a director can be implemented in any language from this file alone. Every
number here was read from the source; each table names the file it comes from,
relative to the repository root.

Wire version: 4 (`crates/aether-ipc/src/layout.rs`, `VERSION`).

Conventions used throughout:

- **Little-endian.** Every integer in shared memory, in a message, in
  `shim.cfg` and in the golden vectors is little-endian. The shim is x86/x64
  only.
- Offsets in shared memory are byte offsets from the start of the mapping.
  Nothing in shared memory is a pointer.
- `u8`/`u32`/`u64` are unsigned, `i32`/`i64` two's-complement signed.
- `utf8…` means "the rest of the payload, as UTF-8, no length, no
  terminator". `str` means `len:u32 | utf8[len]` (no terminator).
- Where prose and code disagree, the code is normative (section 8).

The three crates that define the wire are `aether-ipc` (shared memory, ring,
arena), `aether-protocol` (opcodes, statuses, payload codecs, `shim.cfg`) and
`aether-env` (environment names). An older Rust director lived in this
repository as the shim's test counterpart; it is removed, and the shim's tests
run against their own fake director (`crates/aether-hooks/tests/fakedirector`).
Where it is mentioned below as "the old Rust director (removed)", and where
"both directors" means it and Cairn's, it shows how a director has answered,
not a rule. Cairn's Clojure director (`aether.director.*` in the Cairn repository)
is the production director; its wire code is `src/aether/director/proto.clj`,
`ring.clj`, `ring/backing.clj`, `registry.clj`, `env.clj`, `launch.clj` and
`java/src/aether/director/Native.java` (the atomics on the mapping, VarHandle
`cas`/`getAcquire`/`setRelease`/`fetchMaxLong`, and `createSection`/
`createEvent`, which refuse a name that already exists,
`ERROR_ALREADY_EXISTS`).

---

## 1. Overview

### 1.1 The parties

| Party | Binary | Role |
|---|---|---|
| Director | any process, any language | Creates the shared memory, initialises the ring header, writes the handshake environment and `shim.cfg`, starts the injector, then serves requests until the launch ends. It owns all content: the shim never opens an archive or store itself. |
| Injector | `aether-injector.exe` (x64), `aether-injector32.exe` (x86) | Started by the director. Creates the target process suspended and loads the shim with a remote `LoadLibrary` (the only activation), waits for the shim's ready report, releases or kills the target, then waits for the target's process tree and exits with the first process's exit code. |
| Shim | `aether_shim.dll` (x64), `aether_shim32.dll` (x86) | Loaded into the target before any of its code runs. Reads its environment, attaches to the ring, sends one `HEARTBEAT`, installs NT detours, and from then on asks the director about every file and (optionally) registry operation under a managed root. Injects itself into child processes the target starts. |

Binary names: `contract/artifacts.json` (`natives`).

### 1.2 What flows

```
director                                  injector                 target + shim
   |  create section/file, init header       |                          |
   |  write shim.cfg, set VFS_* env          |                          |
   |---- spawn: <target> <shim> <cfg> <ready> ->                        |
   |                                         |-- create, load shim ---->|
   |                                         |                          | read VFS_* env, map ring,
   |<================ HEARTBEAT over the ring ===========================| check magic+version
   |================= ST_OK ===========================================>|
   |                                         |<-- ready file "ready" ---|
   |                                         |-- resume --------------->|
   |<================ GETATTR / OPEN / READ / ... ======================|
   |                                         |  waits for process tree  |
   |<--- exit code (target's, or 2/3) -------|                          |
```

Everything between director and shim crosses one shared-memory ring. The
director and the injector communicate only through the command line, the
environment, files beside the ready file, and the injector's exit code.

### 1.3 The trust rule

The director trusts nothing in a request: every length is checked against the
payload, every path is normalised and must stay under its root, every handle is
looked up. A request that fails these checks gets a non-OK status; which one is
not uniform across the existing directors (section 3.5.1), the shim treats
every non-OK status as a failure, and a new director should answer
`ST_BAD_REQUEST`. The shim trusts the director's answers as content but bounds-checks
every reply (`crates/aether-ipc/src/ring.rs`, `take_response`; the decoders in
`crates/aether-protocol/src/lib.rs` return `None` on anything short or
malformed). The shim fails closed: a shim that cannot attach to its director
fails its own load, and the injector kills the target before it runs
(sections 5 and 6). There is no mode in which a launched program runs
un-virtualised because the director was unreachable.

---

## 2. Shared memory

### 2.1 Layout of the mapping

```
offset 0                  48                         48 + slot_count*slot_stride
+-------------------------+-------------- ... -------+------------------- ... --+
| ring header (48 bytes)  | slot 0 | slot 1 | ...    | bulk arena (optional)     |
+-------------------------+-------------- ... -------+------------------- ... --+
                          each slot = slot header (32) + payload (payload_cap),
                          rounded up to a multiple of 8
```

Source: `crates/aether-ipc/src/layout.rs`, `crates/aether-ipc/src/ring.rs`
(`Geom::slot_off`, `init`).

- `slot_stride = align8(32 + payload_cap)` where `align8(n) = (n + 7) & !7`.
- `slot_off(s) = 48 + s * slot_stride`; the slot's payload starts at
  `slot_off(s) + 32`.
- The arena, if any, starts at `VFS_ARENA_OFFSET` (the old Rust director and
  Cairn's both put it at `48 + slot_count * slot_stride`, directly after the
  last slot) and is `VFS_ARENA_LEN` bytes long.
- The mapping must be 8-byte aligned (`crates/aether-ipc/src/seg.rs`,
  `SharedSeg::from_raw`); section views and `mmap` are page-aligned.

### 2.2 Ring header (48 bytes)

Source: `crates/aether-ipc/src/layout.rs` (`RingHeader`, `RH_*`, the
`layout_numbers_are_the_historical_ones` test).

| Offset | Size | Type | Field | Written by | Meaning |
|---:|---:|---|---|---|---|
| 0 | 4 | u32 | `magic` | director | `0x56464950`. In memory the bytes are `50 49 46 56` (`"PIFV"`); the constant reads as `"VFIP"`. |
| 4 | 4 | u32 | `version` | director | Wire version, `4`. |
| 8 | 4 | u32 | `slot_count` | director | Number of slots. |
| 12 | 4 | u32 | `slot_stride` | director | Bytes per slot; must equal `align8(32 + payload_cap)`. |
| 16 | 4 | u32 | `payload_cap` | director | Largest request or reply payload a slot carries. |
| 20 | 4 | u32 | `worker_hint` | director | How many threads serve this ring; 0 = not stated. Written after init, before any client attaches. |
| 24 | 8 | u64 | `req_seq` | clients | Request id counter: each client does an atomic fetch-add of 1 to take a `req_id`. Starts at 0. |
| 32 | 4 | u32 | `submit_seq` | clients | Count of published requests (atomic fetch-add after each publish). Diagnostic; neither director reads it. |
| 36 | 4 | u32 | `_pad2` | — | Zero. |
| 40 | 8 | u64 | `reg_gen` | director | Registry generation (section 4.5). 0 = none published. Only ever moved upward (atomic fetch-max). |

`ring::init` writes magic, version, slot_count, slot_stride, payload_cap,
`worker_hint = 0`, `req_seq = 0`, `submit_seq = 0`, `reg_gen = 0`, and sets
every slot's `state` to `FREE`. It does not zero payloads.

### 2.3 Slot header (32 bytes)

Source: `crates/aether-ipc/src/layout.rs` (`SlotHeader`, `SH_*`).

| Offset in slot | Size | Type | Field | Meaning |
|---:|---:|---|---|---|
| 0 | 4 | u32 | `state` | Slot state, section 2.5. The only field accessed atomically. |
| 4 | 4 | u32 | `opcode` | Request opcode (section 3). Written by the client. |
| 8 | 4 | u32 | `flags` | Request flags (section 3.6). Written by the client. |
| 12 | 4 | u32 | `payload_len` | Request length when submitted; overwritten by the director with the reply length. Must be `<= payload_cap`. |
| 16 | 4 | i32 | `status` | Reply status (section 3.5). Written by the director. |
| 20 | 4 | u32 | `ack` | Low 32 bits of the `req_id` this reply answers. Written by the director with the reply. |
| 24 | 8 | u64 | `req_id` | Request id, from `req_seq`. Written by the client. |
| 32 | `payload_cap` | bytes | payload | Request payload; the reply payload is written over it. |

### 2.4 Attaching: what a client checks

`crates/aether-ipc/src/ring.rs`, `open`. The shim refuses the ring (and so
fails its load) unless all of these hold:

1. The mapping is at least 48 bytes.
2. `magic == 0x56464950` and `version == 4`.
3. `slot_stride == align8(32 + payload_cap)`.
4. `48 + slot_count * slot_stride <= mapping length`.

The mapping length the shim uses is `VFS_RING_BYTES` (section 5.3). Nothing
else in the header is validated.

### 2.5 Slot states and transitions

States (`crates/aether-ipc/src/layout.rs`):

| Value | Name | Owner of payload (and arena bank) |
|---:|---|---|
| 0 | `FREE` | nobody |
| 1 | `CLAIMED` | the client that claimed it |
| 2 | `SUBMITTED` | nobody writes; the request is published |
| 3 | `PROCESSING` | the director thread that took it |
| 4 | `COMPLETED` | the client (reads the reply, and the bank for a bulk read) |
| 5 | `ABANDONED` | still the director thread; the client has stopped waiting |

Transitions (`crates/aether-ipc/src/ring.rs`: `claim_free_from`,
`publish_request`, `server_take_from`, `server_complete`, `drain`,
`take_response`, `free_slot`, `abandon`):

| From | To | Who | How | When |
|---|---|---|---|---|
| FREE | CLAIMED | client | CAS (acquire) | To start a request. A client scans from a per-thread hint, wrapping. |
| CLAIMED | SUBMITTED | client | store (release) | After writing `opcode`, `flags`, `payload_len`, `req_id` and the payload. Then `submit_seq += 1`. |
| SUBMITTED | PROCESSING | director | CAS (acquire) | A director thread takes the request. |
| PROCESSING | COMPLETED | director | CAS (release) | After writing the reply payload, `status`, `payload_len`, `ack`. |
| COMPLETED | FREE | client | store (release) | After reading the reply (and, for a bulk read, copying the arena bank). |
| CLAIMED | FREE | client | store or CAS | Gives up before publishing. |
| SUBMITTED | FREE | client | CAS | Gives up on a request no director took (`abandon`). |
| COMPLETED | FREE | client | CAS | Gives up on a request answered a moment ago (`abandon`). |
| PROCESSING | ABANDONED | client | CAS | Gives up on a request a director is still processing (`abandon`). The slot stays the director's. |
| ABANDONED | FREE | director | CAS (release) | The director finishes an abandoned request: drops its reply, frees the slot (`drain`). |

No other transition is legal. In particular **a client must never free a slot
in `PROCESSING`**, and **a director must never store `COMPLETED` over
`ABANDONED`**: the first would let the next request in that slot (and its arena
bank) be overwritten by a late reply; the second would leave the slot unusable
for ever. Version 3 introduced `ABANDONED`; that is why a version-2 peer is
refused (`layout.rs`, the `VERSION` doc).

**The director's completion, step by step** (`ring::server_complete`):

1. Read `req_id` from the slot. If it is no longer the id it took, write
   nothing (a non-conforming client recycled the slot).
2. If `state == ABANDONED`: CAS `ABANDONED -> FREE`, done.
3. If the reply is longer than `payload_cap`: `status = i32::MIN`
   (`0x80000000`), `payload_len = 0`. Otherwise copy the reply to the payload
   area and set `status` and `payload_len`.
4. Store `ack = req_id & 0xFFFFFFFF`.
5. CAS `PROCESSING -> COMPLETED`. If that fails because the client moved the
   slot to `ABANDONED` meanwhile, CAS `ABANDONED -> FREE`.

Cairn's `serve-slot` (`ring.clj`) checks neither `req_id` nor `ABANDONED`
first: its handlers write the reply payload in place while the slot is
`PROCESSING` (or `ABANDONED`, which is still the director's), then it stores
`status`, `payload_len` and `ack`, CASes `PROCESSING -> COMPLETED`, and if that
CAS fails, CASes `ABANDONED -> FREE`. Both are correct against a conforming
client, because an `ABANDONED` slot and its bank stay the director's.

**Every taken slot MUST be completed.** A director that has moved a slot to
`PROCESSING` must eventually run step 3-5 (or 2) for it, whatever happens:

- A slot whose `payload_len > payload_cap` MUST be completed with
  `ST_BAD_REQUEST` and payload length 0 (Cairn's `dispatch` does this). The
  old Rust director (removed) did not: `read_request` returns `None`,
  `RingServer::serve_one` returns `Err(BadResponse)`
  (`crates/aether-ipc/src/endpoint.rs`), and its worker loop exited on it,
  leaving the slot `PROCESSING` for ever and the director one thread short.
  That was a defect, not behaviour to copy.
- A handler that fails (throws, panics) MUST still complete its slot, with
  `ST_IO_ERROR` (Cairn: `Guard/call ... (reply p/io-error 0)`).

A slot left in `PROCESSING` is lost to the ring, and when its client gives up
it goes `ABANDONED` and stays there; the client's data-gate permit for it is
retired until the slot drains, so it never comes back either
(`crates/aether-ipc/src/concurrent.rs`, `DataGate`, `Permits::retire`,
`DataGate::reclaim`). Enough of them and the process can no longer read.

**The client's read** (`ring::take_response`): load `state` with acquire; if it
is `COMPLETED`, check `ack == low32(req_id)` (else the reply is refused as
`BadResponse`), read `status`, read `payload_len` (refused if `> payload_cap`),
copy the payload.

**Memory ordering.** The state word is the synchronisation point: every
non-atomic write to a slot happens before the release store or CAS that
publishes it, and every read happens after an acquire load or CAS that observed
it. On x86/x64 a plain aligned 32-bit load is an acquire and a `lock cmpxchg`
is a full barrier; a director in another language needs a real atomic CAS on
the 4-byte `state` field (it is 4-aligned, since slots start at 48 and strides
are multiples of 8) and an atomic 8-byte fetch-max on `reg_gen` (offset 40,
8-aligned).

**Deadlines** (`crates/aether-ipc/src/lib.rs`, `endpoint.rs`):
`RESPONSE_DEADLINE` is 60 s. A client that gets no free slot, or no reply,
within it abandons its slots and reports `ST_IO_ERROR` to the hooked call. A
client spins for `CLIENT_SPIN_BUDGET` (1 ms) before it starts yielding; the
shim then calls `SwitchToThread` until 20 ms have passed and `Sleep(1)` after
(`crates/aether-hooks/src/director.rs`, `IDLE_YIELD`).

**Order of service.** Slots are independent: a director may take and answer
them in any order and from any number of threads. Requests from one shim
thread are issued one at a time, except a pipelined read, which publishes up
to 4 requests at once (8 for reads of 4 MiB or more) and waits for all of them
(`crates/aether-hooks/src/director.rs`, `PIPELINE_DEPTH`,
`PIPELINE_DEPTH_STREAM`, `STREAM_BYTES`).

### 2.6 The bulk arena and its banks

Source: `crates/aether-ipc/src/arena.rs`, `crates/aether-ipc/src/concurrent.rs`
(`read_fragmented`).

The arena is cut into banks, one per slot:

- `banks = slot_count` (Cairn uses `slot-count`).
- `bank_size = max(arena_len / banks, 4096)`.
- Slot `s` owns bank `s % banks`, at mapping offset
  `arena_offset + (s % banks) * bank_size`.

A bulk `READ` reply carries the mapping offset of the data (section 3.4.4),
not a bank number, so a client never computes bank positions itself. The bank
follows the slot's ownership (section 2.5): the director writes it while the
slot is `PROCESSING`, the client reads it while the slot is `COMPLETED`, and
the client frees the slot only after copying it out (`submit_many_held` /
`release_slots`).

Sizing rules a director must meet, from what the shim sends:

- The shim's bulk chunk is `clamp(VFS_ARENA_LEN / slot_count, 256 KiB,
  1 MiB)` bytes (`FuseClient::bulk_bank_bytes`). A director answers at most
  `min(len, bank_size)` bytes, and the shim reads a short answer as end of
  file. So **each bank must be at least the shim's chunk**: with
  `bank_size = arena_len / slot_count`, that means
  `arena_len >= 256 KiB * slot_count`.
- Every arena byte must lie inside the first `VFS_RING_BYTES` bytes of the
  mapping: the shim bounds-checks the copy and fails the read with
  `ST_IO_ERROR` otherwise.
- With `VFS_ARENA_LEN` unset or 0, the shim never sets `FLAG_READ_BULK` and
  sends inline reads of up to `payload_cap - 8` bytes; a director with an
  arena may still answer those in bulk (section 3.4.4), and the shim accepts
  either form.

### 2.7 Default geometry

Both directors use these numbers (`crates/aether-ipc/src/arena.rs`
`DEFAULT_PAYLOAD_CAP`, `DEFAULT_ARENA_BYTES`, `DEFAULT_WORKER_COUNT`; Cairn
`proto.clj` `geometry`).
Nothing in the shim requires them; it reads the geometry from the header and
the environment.

| Quantity | Formula | Default |
|---|---|---:|
| `slot_count` | — | 32 |
| `payload_cap` | — | 1 048 576 |
| `slot_stride` | `align8(32 + payload_cap)` | 1 048 608 |
| ring bytes = `arena_offset` | `48 + slot_count * slot_stride` | 33 555 504 |
| `arena_len` | — | 33 554 432 (32 MiB) |
| `bank_size` | `max(arena_len / slot_count, 4096)` | 1 048 576 |
| map size (`VFS_RING_BYTES`) | `max(round_up(ring + arena, 65536), 2 MiB)` | 67 174 400 |
| workers | old Rust director 4 (`DEFAULT_WORKER_COUNT`), clamped to `1..=slot_count`; Cairn default 16 | — |

### 2.8 Client concurrency and `worker_hint`

Source: `crates/aether-ipc/src/concurrent.rs` (`data_limit`, `DataGate`).

A director thread runs one request to completion, so a slow read holds a
thread. The shim keeps its *data* requests (READ, WRITE, SETATTR and write
OPENs) in flight below the director's thread count, so that some thread is
always free for a stat or a listing:

```
workers = (worker_hint == 0 ? 4 : worker_hint), capped at slot_count
reserve = max(workers / 4, 1)
limit   = max(workers - reserve, 1)
```

A director should therefore publish its real thread count in `worker_hint`
before the shim attaches, and run at least that many threads. Each shim reads
`worker_hint` once, when it attaches (`DataGate::for_ring`, called from
`FuseClient::connect_source`); a later change is not seen by processes already
attached. The gate is process-local; nothing about it crosses the ring.

### 2.8.1 One ring per session

A session has one ring, and every process of the launch shares it. A child
the target starts inherits the `VFS_*` environment, gets the shim injected
(section 6.4), attaches the same ring and sends its own HEARTBEAT. Nothing
in a request says which process sent it, so:

- file handles (`fh`) are ring-global: a director must issue them uniquely
  across all processes of the ring, and any process may present any handle;
- a process that is killed never sends CLOSE for its open handles; the
  director cannot tell them from live ones and reclaims them when the session
  ends;
- `reg_gen` in the shared header is how one process's registry write
  invalidates every other process's cache (section 4.5).

### 2.9 Windows backing: a named section and two events

Source: `crates/aether-win/src/mapping.rs`, `crates/aether-win/src/event_notifier.rs`,
`crates/aether-hooks/src/director.rs` (`connect_source`).

| Object | Director creates with | Shim opens with | Named by |
|---|---|---|---|
| Section | `CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, size_hi, size_lo, name)`; an existing section of that name is an error | `OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, name)` then `MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, VFS_RING_BYTES)` | `VFS_RING_SECTION` |
| Server event | `CreateEventW(NULL, FALSE /*auto-reset*/, FALSE, name)`; existing is an error | `OpenEventW(EVENT_MODIFY_STATE = 0x0002, FALSE, name)`; failure is tolerated (no wake) | `VFS_SERVER_EV` |
| Client event | as the server event | not opened by the shim | `VFS_CLIENT_EV` |

Names are free-form. The old Rust director (removed) used `<section>_srv` and
`<section>_cli`; Cairn names its section
`Local\vfs_ring_clj_<pid>_<ms>_<counter>` and its events the same plus `_srv`
and `_cli` (`ring.clj`, `ring-name`; `ring/backing.clj`). A name must be unique
to the session: both directors refuse to open a pre-existing one.

A director may also serve a Windows shim from a file (section 2.10): the shim
honours `VFS_RING_PATH` on Windows too.

### 2.10 Linux under Wine: a file both sides map

Source: `crates/aether-filering/src/lib.rs` (`FileRing`), `crates/aether-win/src/mapping.rs`
(`open_file_backed`), `crates/aether-hooks/src/director.rs` (`ring_source`).

A page-file section has no identity outside one Windows (or Wine) session, so a
native Linux director and a shim under Wine share a file:

- The director creates the file, grows it to at least the map size before
  mapping it (an `mmap` past end of file faults on touch), never truncates an
  existing file in place, and maps it `MAP_SHARED`, `PROT_READ | PROT_WRITE`.
  An existing file is removed and a new one created in its place, so a stale
  reader keeps the old file's pages: the Rust `FileRing::create` and Cairn
  (`ring/backing.clj`) both do this.
- The shim opens `VFS_RING_PATH` with `CreateFileW(FILE_GENERIC_READ |
  FILE_GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, OPEN_EXISTING)`, refuses
  a file shorter than `VFS_RING_BYTES`, and maps it with an unnamed
  `CreateFileMappingW` over that handle.
- `VFS_RING_PATH` is the path as the shim sees it (a Wine `C:` or `Z:`
  path); the director maps the same file by its host path.
- **`VFS_RING_PATH` wins over `VFS_RING_SECTION`** when both are set; an empty
  `VFS_RING_PATH` counts as unset. In file mode the shim opens no event.

### 2.11 Waking

Waking is advisory. Correctness rests on the state word alone, and both ends
re-check it in a loop; a lost wake costs latency, never a request
(`crates/aether-ipc/src/notifier.rs`, `Notifier`).

| Mode | Shim (client) | Director (server) |
|---|---|---|
| Section | `SetEvent(server_ev)` after publishing and after freeing a slot; spins for its reply (`crates/aether-hooks/src/director.rs`, `WakeServerSpinClient`) | `EventNotifier`: after serving a request, spins for `VFS_RING_SPIN_US` µs (default 400); otherwise `WaitForSingleObject(server_ev, 1)`. Sets `client_ev` after completing (the shim does not wait on it). |
| File | no event; spins | `AdaptiveNotifier`: spins for `VFS_RING_SPIN_US` µs (default 400) after the last completed request, then sleeps 100 µs, 500 µs once quiet for more than 5 ms, 2000 µs once quiet for more than 50 ms. |

Sources: `crates/aether-win/src/event_notifier.rs` (`spin_budget`,
`wait_server`), `crates/aether-ipc/src/notifier.rs` (`spin_window`,
`idle_sleep`). Cairn's `EventWaker` and `ParkWaker` follow the same scheme
(`ring.clj` docstring; park values `[100 500 2000]` µs in `ring/backing.clj`).
`VFS_RING_SPIN_US` is read by the director side only; `0` disables the spin.

### 2.12 Worked example: an initialised header

Golden vector `ring-header-slots4-cap256` (`ring::init(seg, 4, 256)`):

```
50494656 04000000 04000000 20010000 00010000 00000000
0000000000000000 00000000 00000000 0000000000000000
```

| Offset | Bytes | Value |
|---:|---|---|
| 0 | `50 49 46 56` | magic `0x56464950` |
| 4 | `04 00 00 00` | version 4 |
| 8 | `04 00 00 00` | slot_count 4 |
| 12 | `20 01 00 00` | slot_stride 288 = align8(32 + 256) |
| 16 | `00 01 00 00` | payload_cap 256 |
| 20 | `00 00 00 00` | worker_hint 0 |
| 24 | 8 x `00` | req_seq 0 |
| 32 | 4 x `00` | submit_seq 0 |
| 36 | 4 x `00` | pad |
| 40 | 8 x `00` | reg_gen 0 |

---

## 3. Messages

### 3.1 Framing

A request is the slot header's `opcode`, `flags`, `payload_len`, `req_id` and
`payload_len` bytes of payload. A reply is `status`, `payload_len`, `ack` and
the reply payload, written over the request payload in the same slot. There is
no other framing, no per-message version and no checksum.

A reply's payload is meaningful only when `status == ST_OK (0)`. Every
director error path in both directors replies with an empty payload. The shim
decodes a reply payload only after checking `status == ST_OK`; a reply that
then fails to decode is treated as `ST_BAD_REQUEST`
(`crates/aether-hooks/src/director.rs`).

### 3.2 Shared encodings

Source: `crates/aether-protocol/src/field.rs`, `crates/aether-protocol/src/wire.rs`.

**Root ids.** A session has several managed roots, each a `u32` id. Root 0 is
`VFS_VIRTUAL_DIR`; others come from `VFS_VIRTUAL_ROOTS` (section 5.3). Every
path-carrying file request starts with the root it addresses. Handle-keyed
requests (READ, WRITE, SETATTR, CLOSE) carry no root: the handle implies it.

**Path request** (`Request`'s `root` and `path`), used by GETATTR, READDIR, DELETE,
STAGE_EXEC:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | rest | utf8… | `vpath` |

**vpaths** as the shim sends them (`crates/aether-hooks/src/director.rs`,
`vpath_under_root`, `vpath_as_spelled`, `wire_vpath`):

- Relative to the root, components joined by `/`, no leading or trailing `/`.
- The root itself is `"."`.
- Folded (lowercased by `aether_casefold::fold`: ASCII lowercase for ASCII
  strings, else `char::to_lowercase` of every character,
  `crates/aether-casefold/src/lib.rs`) except in a request that creates a
  name (a write OPEN, MKDIR, RENAME targets), which keeps the caller's
  spelling so that the name is stored as created.
- A named stream suffix (`file.esp:stream`) is carried through, folded.

A director must look paths up case-insensitively under the same fold, and must
treat a path that would leave its root (`..` above the root) as an error.

#### Case folding

The fold is part of the wire: the shim sends folded paths, and a director has
to agree with it about which names are the same name.

**The function.** `fold(s)` replaces every character of `s` with its Rust
`char::to_lowercase` (the Unicode lowercase mapping, including the
unconditional multi-character mappings of `SpecialCasing.txt`), character by
character. An all-ASCII string is lowercased byte by byte, which gives the same
result. There is no locale, no context and no normalisation:

- `Σ` always folds to `σ`; the final-sigma rule of `str::to_lowercase` is not
  applied, because the fold works one character at a time.
- Composed and decomposed forms stay different names: `é` (U+00E9) and `e`
  plus U+0301 do not fold to the same string.
- The mapping is the one of the Unicode version in the Rust standard library
  the release was built with (`rust-toolchain.toml`).

Source: `crates/aether-casefold/src/lib.rs`.

**Two properties it does not have:**

1. It does not preserve length. `İ` (U+0130) is two bytes in UTF-8 and folds
   to three (`i` followed by U+0307). Never cut a folded string at an offset
   measured on the unfolded one; compare component by component.
2. It is not Windows' case equivalence. NTFS compares names through its
   uppercase table, one UTF-16 unit at a time. `İ` folds to a different name
   under this fold, not to a case variant of itself, so "the folded spelling
   names the same file on NTFS" is not a valid argument for any name that
   reaches a real filesystem.

**What a director must do:**

- Treat two names as the same entry exactly when their folds are equal: key
  and compare by `fold`, never by a different lowercasing (ASCII-only, a
  locale's, or NTFS's). The shim's caches depend on this equality. The read
  cache keeps a file's blocks under (root, folded path), the name cache keeps
  stored spellings under folded paths, and the registry cache keeps keys under
  folded paths. A delete or rename drops entries by folded path. If the
  director considered two spellings one file while their folds differed, a
  rename through one spelling would leave the other's cached blocks in place,
  and the game would read stale bytes.
- Keep the spelling a name was created with. A request that creates a name (a
  write OPEN, MKDIR, a RENAME's target) carries the caller's spelling, and
  STORED_NAMES and READDIR answer with stored spellings. The fold is for
  matching only.
- Fold `Registry`, `Machine`, `User` and every key and value name the same way
  (section 4).

Changing the fold changes the wire: a director built against one fold
disagrees with a shim built against another about which names are equal. Any
change to it is a wire version bump (section 7).

**Path syntax a director should accept:** `/`-separated, no leading slash,
`"."` for the root; that is the only form the shim sends. Directors have
differed on anything else, so a client must not rely on it:

- The old Rust director (removed) normalised every path (`\` to `/`, strip
  `/` at both ends, drop empty and `.` components, resolve `..`, error if `..`
  leaves the root).
- Cairn normalises the same way (`src/aether/director/path.clj`, `normalize`)
  except that its OPEN refuses, with `ST_BAD_REQUEST`, a path that starts
  with `/` or `\` or contains `\` at all (`ring.clj`, `wellformed?`).

**Booleans.** In file-op replies a flag byte is read as `!= 0` (`Flag` in
`field.rs`). In registry messages a boolean must be exactly 0 or 1 (`Bool`).

**Lenient vs strict.** The file-op decoders ignore trailing bytes after the
fields they read. Separately, the final `pad:u32` of a READ request is
optional: `Request::decode` stops after READ's `len`, so a 20-byte READ decodes
(`PadAfter` in `crates/aether-protocol/src/field.rs`). WRITE's pad is **not**
optional: WRITE's `LenPadBytes` reads it before the data, so a WRITE must have its 24-byte
header. The registry decoders reject trailing bytes, truncation, bad UTF-8
and out-of-range values.

**Timestamps.** `mtime` is seconds since the Unix epoch, signed; 0 or less
means "no time" and the shim substitutes 2024-01-01T00:00:00Z
(`crates/aether-ntlayout/src/dirinfo.rs`, `filetime_of`, `SYNTH_FILETIME`).

### 3.3 Opcode table

Source: the protocol table in `crates/aether-protocol/src/table.rs` (`Opcode`,
`Opcode::ALL`). Numbers are never renumbered. The table is also published as data,
`contract/protocol.json`: every opcode with its request and reply fields in
wire order (padding included, as `pad` entries), strictness, the statuses and
the flag bits, generated from the table by `aether_protocol::contract_json()`
and checked equal by `crates/aether-protocol/tests/contract.rs`
(`UPDATE_CONTRACT=1` rewrites it). It is the machine-readable form of the
layouts below, for building a director's tables in another language.

The file defines its own field types, so it can be read without this
document. Its `"types"` section gives each field type's wire form, and
`"attributes"` the keys a field may carry besides `name` and `type`; both are
generated from the code, and the types are:

| Type | Wire form |
|---|---|
| `u32` | little-endian unsigned 32-bit integer |
| `u64` | little-endian unsigned 64-bit integer |
| `i64` | little-endian signed 64-bit integer |
| `root_id` | u32, a root's id |
| `open_flags` | u32 of the open_flags masks |
| `bool` | one byte, exactly 0 or 1 (a decoder refuses others) |
| `bool_lenient` | one byte, written 0 or 1, any non-zero reads as true |
| `bool_bit0` | one byte, bit 0 is the value, the rest written 0 and ignored |
| `lookup_state` | one byte, the values in the values attribute |
| `child_state` | one byte, the values in the values attribute |
| `pad` | size bytes, written 0, skipped on read; optional means a reader may find them missing |
| `str_u32` | len:u32 then len bytes of UTF-8; max_utf16 is a limit in UTF-16 units |
| `str_rest` | the rest of the payload as UTF-8 |
| `bytes_u32` | len:u32, then pad_after_len zero bytes if given, then len bytes; max is a limit on len |
| `list_u32` | count:u32 then count items, each laid out as the fields in of |
| `optional_node` | has_node:u8 (a bool) then, if 1, the fields in node |
| `inline_or_bulk` | a READ reply: if the bulk_bit mask is set in its first u32 the reply is the fields in bulk, else the fields in inline |

Attributes (`size`, `optional`, `max`, `max_utf16`, `pad_after_len`, `values`,
`of`, `node`, `bulk_bit`, `inline`, `bulk`, `bit31`) are defined in the file's
`"attributes"` section, and `bit31` marks the `len` of a bulk READ reply, whose
top bit is the bulk flag. The `"open_flags"` and `"request_flags"` lists give
each flag's `"mask"`, the value it has in the word (`append` is 32), not a bit
index.

| # | Name | Request | Reply (on ST_OK) | Shim sends flags |
|---:|---|---|---|---|
| 1 | GETATTR | path request | 18 bytes, 3.4.1 | 0 |
| 2 | READDIR | path request | listing, 3.4.2 | 0 |
| 3 | OPEN | `root \| oflags \| vpath` | 24 bytes, 3.4.3 | 0 |
| 4 | *reserved* (was `materialize`) | — | `ST_BAD_REQUEST` | never sent |
| 5 | READ | 24 bytes | inline or bulk, 3.4.4 | 0 or `FLAG_READ_BULK` |
| 6 | WRITE | 24 + data | 8 bytes, 3.4.5 | 0 |
| 7 | SETATTR | 16 bytes | empty | 0 |
| 8 | RENAME | `root \| from_len \| from \| to` | empty | 0 |
| 9 | DELETE | path request | empty | 0 |
| 10 | MKDIR | `root \| mode \| vpath` | empty | 0 |
| 11 | CLOSE | 8 bytes | empty | 0 |
| 12 | *reserved* (was `register-process`) | — | `ST_BAD_REQUEST` | never sent |
| 13 | HEARTBEAT | empty | empty | 0 |
| 14 | STORED_NAMES | `root \| skip \| vpath` | names joined by `/` | 0 |
| 15 | REG_LOOKUP | section 4 | section 4 | 0 |
| 16 | REG_KEY | section 4 | section 4 | 0 |
| 17 | REG_SET_VALUE | section 4 | section 4 | 0 |
| 18 | REG_DELETE_VALUE | section 4 | section 4 | 0 |
| 19 | REG_CREATE_KEY | section 4 | section 4 | 0 |
| 20 | REG_DELETE_KEY | section 4 | section 4 | 0 |
| 21 | REG_RENAME_KEY | section 4 | section 4 | 0 |
| 22 | REG_CHANGED | section 4 | section 4 | 0 |
| 23 | RENAME_X | `from_root \| to_root \| from_len \| from \| to` | empty | 0 |
| 24 | STAGE_EXEC | path request | real path, utf8… | 0 |

**Any other opcode** (including 0, 4, 12 and 25 and up) is answered
`ST_BAD_REQUEST` with an empty payload by both directors (Cairn `ring.clj`,
`dispatch`).

### 3.4 Each opcode

#### 3.4.1 GETATTR (1)

Request: path request. Reply (`Reply::GetAttr`), 18 bytes:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 1 | u8 | `found` (0/1) |
| 1 | 1 | u8 | `is_dir` (0/1) |
| 2 | 8 | u64 | `size` |
| 10 | 8 | i64 | `mtime` |

A missing path is **`ST_OK` with `found = 0`** and the other fields zero, not
an error status. A root the director does not serve is also `found = 0` (both
directors). The shim treats `ST_NOT_FOUND` the same as `found = 0`; any other
non-OK status fails the hooked call with `STATUS_UNSUCCESSFUL`
(`crates/aether-hooks/src/hook/file_attr.rs`).

#### 3.4.2 READDIR (2)

Request: path request (the directory; `"."` for the root). Reply
(`Reply::ReadDir`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `count` |
| 4 | … | entry x `count` | entries, packed, no padding |

Entry:

| Size | Type | Field |
|---:|---|---|
| 4 | u32 | `name_len` |
| `name_len` | utf8 | `name` (stored spelling, one component) |
| 1 | u8 | `is_dir` |
| 8 | u64 | `size` |
| 8 | i64 | `mtime` |

The shim presents the listing to the program whole and unaltered (the cursor
over it is `EnumState`, `crates/aether-hooks/src/hook/dirquery.rs`), apart from
dropping old `<name>.__vfs_wh__` whiteout markers. It adds no `.` or `..`
entries, so a director should not include them either; it does not sort or
otherwise interpret the order, so the program sees entries in the order sent.
`size` is copied into both `EndOfFile` and `AllocationSize` of the NT
directory record for every entry, directories included
(`crates/aether-ntlayout/src/dirinfo.rs`, `write_dir_info`); send 0 for a
directory (as the golden vector does).

The whole listing is one reply and must fit `payload_cap`. Unknown root:
`ST_NOT_FOUND`. Both directors reply to an oversized listing with the
overflow status `i32::MIN` (section 2.5, step 3; Cairn `p/overflow`).

#### 3.4.3 OPEN (3)

Request (`Request::Open`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | 4 | u32 | `oflags` (table below) |
| 8 | rest | utf8… | `vpath` |

Open flags (`crates/aether-protocol/src/status.rs`):

| Bit | Value | Name | Meaning |
|---:|---:|---|---|
| 0 | 1 | `OPEN_READ` | read access |
| 1 | 2 | `OPEN_WRITE` | write access |
| 2 | 4 | `OPEN_CREATE` | create if absent |
| 3 | 8 | `OPEN_EXCL` | fail with `ST_EXISTS` if present |
| 4 | 16 | `OPEN_TRUNC` | truncate on open |
| 5 | 32 | `OPEN_APPEND` | writes go to end of file; the director ignores WRITE offsets |

`oflags == 0` is read as `OPEN_READ` by both directors. The shim sends
`OPEN_READ` for a read open, with a folded path; and for a write open
`OPEN_WRITE` plus the create bits for the NT disposition
(`crates/aether-ntlayout/src/disposition.rs`, `open_create_flags`):
`FILE_SUPERSEDE`/`FILE_OVERWRITE_IF` = `CREATE|TRUNC`, `FILE_CREATE` =
`CREATE|EXCL`, `FILE_OPEN_IF` = `CREATE`, `FILE_OVERWRITE` = `TRUNC`,
`FILE_OPEN` = none. The current shim never sends `OPEN_APPEND`.

Reply (`Reply::Open`), 24 bytes:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh`: the director's handle |
| 8 | 8 | u64 | `size` |
| 16 | 1 | u8 | `is_dir` |
| 17 | 1 | u8 | `flags`: bit 0 is `immutable` (1); other bits zero |
| 18 | 2 | — | zero |
| 20 | 4 | u32 | `mount_gen` |

- `fh` is any u64 the director chooses, unique among open handles. Both
  directors start at 1 and count up.
- `immutable`: the handle's bytes cannot change while it is open. The shim's
  read cache (`crates/aether-ipc/src/readcache/mod.rs`) caches small reads only
  through read-only, immutable handles. Never set it for a write open or a
  directory (the old Rust director cleared it for both).
- `mount_gen`: a generation the director bumps whenever a root's content is
  replaced. The cache keys a file's blocks by (root, folded path, size,
  `mount_gen`), so two immutable opens of one path with equal size and
  `mount_gen` must have identical content. The old Rust director (removed)
  started at 1 and bumped on every mount/unmount; Cairn always sends 1.
- Errors: unknown root `ST_NOT_FOUND`; `OPEN_WRITE` on a root with no writable
  provider `ST_READ_ONLY`; `OPEN_EXCL` on an existing path `ST_EXISTS`;
  missing path without `OPEN_CREATE` `ST_NOT_FOUND`.

#### 3.4.4 READ (5)

Request (`Request::Read`), 24 bytes (a director must accept 20):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh` |
| 8 | 8 | u64 | `offset` |
| 16 | 4 | u32 | `len` |
| 20 | 4 | — | zero |

The director answers in bulk when (`flags & FLAG_READ_BULK`) or `len >= 65536`
(`BULK_THRESHOLD`, both directors), and it has an arena; otherwise inline.

Inline reply (`ReadReply::Inline`), `8 + n` bytes, `n <= min(len, payload_cap - 8)`:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `n` (bit 31 clear) |
| 4 | 4 | — | zero |
| 8 | `n` | bytes | data |

Bulk reply (`ReadReply::Bulk`), 16 bytes, `n <= min(len, bank_size)`:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `n \| 0x80000000` (`READ_RESP_BULK_BIT`) |
| 4 | 4 | — | zero |
| 8 | 8 | u64 | `arena_offset`: mapping offset of the data (the slot's bank) |

The client tells the forms apart by bit 31 of the first word
(`ReadReply`'s decoder), so it must accept either whatever flag it sent. `n <
len` means end of file; the shim stops there. Errors: unknown `fh`
`ST_BAD_FH`; a directory handle `ST_IS_DIR`.

#### 3.4.5 WRITE (6)

Request (`Request::Write`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh` |
| 8 | 8 | u64 | `offset` (ignored on an `OPEN_APPEND` handle) |
| 16 | 4 | u32 | `len` |
| 20 | 4 | — | zero |
| 24 | `len` | bytes | data |

A request whose payload is shorter than `24 + len` is `ST_BAD_REQUEST`. The
shim sends at most `VFS_RING_PAYLOAD_CAP - 24` bytes per WRITE and repeats
until done or a short write.

Reply (`Reply::Write`), 8 bytes: `written:u32 | zero:u32`. The shim reads
only the first 4 bytes.

#### 3.4.6 SETATTR (7)

Request (`Request::SetAttr`), 16 bytes: `fh:u64 | size:u64`. Sets the file's
length (truncate or extend) on an open handle; there is no path-keyed form.
Reply: status only. Unknown `fh` `ST_BAD_FH`.

#### 3.4.7 RENAME (8)

Request (`Request::Rename`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` (both paths are under it) |
| 4 | 4 | u32 | `from_len` |
| 8 | `from_len` | utf8 | `from` |
| 8 + `from_len` | rest | utf8… | `to` |

Reply: status only.

`to` is stored with the caller's spelling (section 3.2), so a rename that
changes only the case respells the name. A file or a directory moves; a
directory moves with everything under it. A file or an empty directory at
`to` is replaced; a directory at `to` with anything under it is
`ST_IO_ERROR`, and nothing moves. The root itself at either end, or a `to`
inside `from`, is `ST_BAD_REQUEST`. A missing `from` is `ST_NOT_FOUND`; either
end outside every writable provider `ST_READ_ONLY`. (Cairn's writable layer,
`provider/layer.clj`, `rename!`; the reference director.)

#### 3.4.8 DELETE (9)

Request: path request. Reply: status only. Unknown root `ST_NOT_FOUND`.

A file, or a directory with nothing under it, is deleted. A directory with
anything under it is `ST_IO_ERROR`, and nothing is deleted. The root itself
is `ST_BAD_REQUEST`. A missing path is `ST_NOT_FOUND`; a path outside every
writable provider `ST_READ_ONLY`. (Cairn's writable layer,
`provider/layer.clj`, `delete!`; the reference director.)

#### 3.4.9 MKDIR (10)

Request (`Request::Mkdir`): `root:u32 | mode:u32 | vpath utf8…`. The shim
sends `mode = 0o755` (493); both directors ignore it. Reply: status only.

The directory is stored with the caller's spelling (section 3.2). On an
existing path the directors differ:

- An existing directory: a director may answer `ST_OK` or `ST_EXISTS`, and a
  client must accept either. Cairn's writable providers answer `ST_OK`
  (`provider/layer.clj` and `provider/disk.clj`, `mkdir!`), its read-only ones
  and the reference director `ST_EXISTS`. The shim, on any failure, asks
  GETATTR whether a directory is there, so both give the program the same
  answer for `FILE_OPEN_IF`; for `FILE_CREATE` only `ST_EXISTS` reports the
  name collision.
- An existing file: `ST_IO_ERROR` where a writable provider serves the path
  (Cairn's writable providers), `ST_EXISTS` where none does (Cairn's
  read-only providers). The reference director answers the same.

#### 3.4.10 CLOSE (11)

Request (`Request::Close`), 8 bytes: `fh:u64`. Reply: status only. Unknown
`fh` `ST_BAD_FH`.

#### 3.4.11 HEARTBEAT (13)

Request: empty payload, flags 0. Reply: `ST_OK`, empty. The shim sends exactly
one, right after attaching, and fails its load unless it gets `ST_OK`
(`crates/aether-hooks/src/director.rs`, `try_init_from_env`).

#### 3.4.12 STORED_NAMES (14)

Request (`Request::StoredNames`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | 4 | u32 | `skip`: components the client already knows |
| 8 | rest | utf8… | `vpath` (folded) |

Reply (`Reply::StoredNames`): the stored spelling of each component of `vpath`
from index `skip` on, joined by `/`, no length prefix, exactly
`components - skip` names. A component the director has no entry for SHOULD
come back as it was sent, as the old Rust director's did. A director MAY
instead answer `ST_NOT_FOUND` for a path it does not have (Cairn does, `ring.clj`, `op-stored-names`). The shim
discards a reply with the wrong number of names, and on any non-OK status or
discarded reply uses the caller's own spelling (`FuseClient::stored_names`).
Used for `GetFinalPathNameByHandle`.

An empty reply payload is one empty name (the shim splits `Reply::StoredNames`
with `str::split`). The shim never asks for zero names: when it already
knows every component it sends no request.

#### 3.4.13 RENAME_X (23)

Request (`Request::RenameX`):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `from_root` |
| 4 | 4 | u32 | `to_root` |
| 8 | 4 | u32 | `from_len` |
| 12 | `from_len` | utf8 | `from` |
| 12 + `from_len` | rest | utf8… | `to` |

A rename between two roots. A file is copied to `to_root` (replacing what is
there) and removed from `from_root`; a directory is refused with
`ST_CROSS_ROOT`, which the shim reports as `STATUS_NOT_SAME_DEVICE`
(`crates/aether-protocol/src/status.rs`). Equal roots behave as RENAME. Reply:
status only.

#### 3.4.14 STAGE_EXEC (24)

Request: path request (the image the program is starting). Reply: the real,
on-disk Windows path to start it from, UTF-8, the whole payload. The director
puts the program on real disk first (a child's image is opened by the kernel
before any hook runs). `ST_NOT_SUPPORTED` when the director stages nothing.

How the shim uses it (`crates/aether-hooks/src/hook/child_start.rs`): for each
candidate image path that routes under a root it first sends GETATTR, and
sends STAGE_EXEC only for the first candidate that is a file (`found`, not
`is_dir`). On success the child is started from the returned path. If no
candidate was staged, every candidate path is absolute, and at least one was
under a root, the child is refused (`out.refuse`), not started from
whatever is really on disk at that path. Otherwise (a relative name the
loader may resolve elsewhere) the image path is left as it was.

### 3.5 Status values

Source: `crates/aether-protocol/src/status.rs` (`Status`, `Status::ALL`),
`crates/aether-ipc/src/ring.rs` (overflow).

| Value | Name | Meaning |
|---:|---|---|
| 0 | `ST_OK` | Success; the reply payload is valid. |
| -1 | `ST_NOT_FOUND` | No such path, root, key or value. |
| -2 | `ST_NOT_A_DIRECTORY` | A path component is a file. |
| -3 | `ST_BAD_REQUEST` | The payload did not decode, a path escaped its root (recommended; see 3.5.1), an unknown opcode, or a registry limit was exceeded. |
| -4 | `ST_IO_ERROR` | Anything without a better status. The shim also uses it locally for "not sent / not answered". |
| -5 | `ST_IS_DIR` | A file operation on a directory handle. |
| -6 | `ST_BAD_FH` | Unknown handle. |
| -7 | `ST_NO_SPACE` | Storage full. |
| -8 | `ST_NOT_SUPPORTED` | The director does not implement this (no registry layer, no stager). |
| -9 | `ST_READ_ONLY` | No writable provider serves this path. |
| -10 | `ST_EXISTS` | `OPEN_EXCL` on an existing path; registry key already exists. |
| -11 | `ST_REPLY_TOO_LARGE` | A registry answer larger than `payload_cap - 8` (in practice only REG_KEY's can be; section 4.3). |
| -12 | `ST_CROSS_ROOT` | RENAME_X of a directory. |
| `i32::MIN` (`-2147483648`) | overflow | Written by the ring layer when a reply exceeds `payload_cap`; payload length 0. Not in `Status::ALL`. |

Values 0 to -11 are fixed by the shim, which matches statuses by number. New
statuses take the next free negative number.

The `ST_*` names are this document's names for the wire values. In Rust
each is a `Status` variant (`ST_NOT_FOUND` is `Status::NotFound`), and in
`contract/protocol.json` its lowercase, hyphenated name (`"not-found"`). The
`OPEN_*` flags of 3.4.3 map the same way: `OPEN_CREATE` is
`OpenFlags::CREATE` in Rust and `"create"` in the JSON's `open_flags`, whose
`"mask"` is the value in the table (`OPEN_APPEND` is 32).

#### 3.5.1 Malformed requests: what each director answers

The status for a payload too short for its fixed fields, a path that escapes
its root (`..` above it), or a root the director does not serve is not
uniform across the existing directors. The shim treats every non-OK status
as a failure of the call (only `ST_NOT_FOUND`, `ST_EXISTS`, `ST_READ_ONLY`,
`ST_CROSS_ROOT` and the like get specific NT statuses), so none of these
differences changes what a well-formed shim request gets. A new director
should answer `ST_BAD_REQUEST` for a short payload or an escaping path, and
`ST_NOT_FOUND` for an unknown root.

| Op | Old Rust director (removed) | Cairn (`ring.clj`) |
|---|---|---|
| GETATTR | short or escaping: `ST_BAD_REQUEST`; unknown root: OK, `found = 0` | short, escaping or unknown root: OK, `found = 0` |
| READDIR | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` | all three: `ST_NOT_FOUND` |
| OPEN | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` | short, escaping, or not `wellformed?`: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` |
| READ, WRITE, SETATTR, CLOSE | short: `ST_BAD_REQUEST` | short: `ST_BAD_REQUEST` |
| DELETE, MKDIR | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` | all three: `ST_NOT_FOUND` |
| RENAME | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_BAD_REQUEST` | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` |
| RENAME_X | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` | as RENAME |
| STORED_NAMES | short or escaping: `ST_BAD_REQUEST`; unknown root: `ST_NOT_FOUND` | all three: `ST_NOT_FOUND` |
| STAGE_EXEC | escaping: `ST_BAD_REQUEST`; no stager: `ST_NOT_SUPPORTED` | no stager: `ST_NOT_SUPPORTED` (checked first); escaping or unknown root: `ST_NOT_FOUND` |

Invalid UTF-8 in a path is `ST_BAD_REQUEST` in the Rust decoders; Cairn
decodes it with replacement characters and carries on.

### 3.6 Flags

| Where | Bit | Value | Name | Source |
|---|---:|---:|---|---|
| slot `flags` of READ | 0 | `0x1` | `FLAG_READ_BULK`: prefer a bulk-arena reply | `RequestFlags::READ_BULK` in `crates/aether-protocol/src/status.rs` |
| READ reply word 0 | 31 | `0x8000_0000` | `READ_RESP_BULK_BIT`: the reply is bulk | `READ_RESP_BULK_BIT` in `crates/aether-protocol/src/table.rs` |
| OPEN reply byte 17 | 0 | `0x01` | `immutable` | `OpenResp::immutable` in `crates/aether-protocol/src/table.rs` |
| OPEN request `oflags` | 0-5 | 1-32 | `OPEN_*` (3.4.3) | `OpenFlags` in `crates/aether-protocol/src/status.rs` |

The slot `flags` word is ignored for every opcode but READ, and bits other
than bit 0 are ignored for READ, by both directors.

---

## 4. Registry encodings

Opcodes 15-22 virtualise the registry through an overlay the director keeps.
The shim sends them only when `VFS_REGISTRY` is affirmative (section 5.3) and
every registry detour installed (`crates/aether-hooks/src/regclient.rs`,
`enabled`). A director without a registry layer answers all eight
`ST_NOT_SUPPORTED`.

### 4.1 Field encodings and limits

Source: `crates/aether-protocol/src/table.rs` (registry messages),
`crates/aether-protocol/src/wire.rs`, `crates/aether-registry/src/overlay.rs`.

- `str` = `len:u32 | utf8[len]`. Limits count UTF-16 code units
  (`utf16_len`), the registry's own unit.
- Booleans are one byte, exactly 0 or 1.
- Every registry decoder consumes its input exactly; trailing bytes are
  `ST_BAD_REQUEST`.

| Limit | Value | Applies to |
|---|---:|---|
| `MAX_KEY_NAME` | 255 UTF-16 units | a key name component; `new_leaf`; child names in a REG_KEY reply |
| `MAX_VALUE_NAME` | 16 383 UTF-16 units | value names |
| `MAX_DATA` | 1 048 576 bytes (`1 << 20`) | value data |
| `MAX_WRITE_DEPTH` | 512 components below `\Registry` | keys a write may create |

The shim never sends a registry request larger than `payload_cap`:
`RegClient::request` refuses it locally with `ST_BAD_REQUEST`
(`crates/aether-hooks/src/regclient.rs`). For a write that becomes
`STATUS_INVALID_PARAMETER` (section 4.4); for REG_LOOKUP or REG_KEY it goes
through `read_failed`, and the hook falls back to the real key.

### 4.2 Key paths

Source: `crates/aether-registry/src/path.rs` (`canonical`, `to_nt`),
`crates/aether-registry/src/overlay.rs` (`valid_path`).

A key path is the canonical NT form: `\Registry` then non-empty components,
single `\` separators, no trailing `\`, no `.` or `..` components. `Registry`,
`Machine` and `User` take that spelling; components below keep the caller's.
Under `\Registry\User`, the current user's SID becomes the literal
`<CurrentUser>` and `<SID>_Classes` becomes `<CurrentUser>_Classes`, so a layer
written under one account reads back under another. Keys compare
case-insensitively by `fold`, and that includes the `Registry`, `Machine` and
`User` components: both directors accept `\REGISTRY\MACHINE\...` as readily as
`\Registry\Machine\...` (Rust `valid_path` compares `fold(canonical(p))` with
`fold(p)`; Cairn `overlay-path?` folds the `Registry` component and compares
everything else through `fold`). The shim sends the canonical spelling. A
director must answer `ST_BAD_REQUEST` for a path that is not canonical (the
codec checks only length and UTF-8).

Value names arrive as UTF-8 converted from the caller's UTF-16; value `ty` is
the Windows `REG_*` type number; value data is the raw bytes the caller passed
to `NtSetValueKey` (for `REG_SZ`, UTF-16LE with whatever terminator the caller
included) (`crates/aether-hooks/src/regwrite.rs`).

### 4.3 Requests and replies

Every reply starts with `version:u64`, the overlay's version after (or at) the
operation.

| # | Op | Request | Reply |
|---:|---|---|---|
| 15 | REG_LOOKUP | `path:str` | `version:u64 \| state:u8 \| below:u8` |
| 16 | REG_KEY | `path:str` | `version:u64 \| has_node:u8 \| node?` |
| 17 | REG_SET_VALUE | `path:str \| name:str \| ty:u32 \| data_len:u32 \| data[data_len]` | `version:u64` |
| 18 | REG_DELETE_VALUE | `path:str \| name:str` | `version:u64` |
| 19 | REG_CREATE_KEY | `path:str \| volatile:u8` | `version:u64` |
| 20 | REG_DELETE_KEY | `path:str` | `version:u64` |
| 21 | REG_RENAME_KEY | `path:str \| new_leaf:str` | `version:u64` |
| 22 | REG_CHANGED | `path:str \| subtree:u8 \| since:u64` | `version:u64 \| changed:u8` |

REG_LOOKUP `state` (`LookupReply`):

| Value | Meaning |
|---:|---|
| 0 | absent: the overlay says nothing; the real key decides |
| 1 | present, overlaying a real key |
| 2 | present, created in the overlay (no real counterpart shows through) |
| 3 | tombstoned: deleted in the overlay |

A decoder rejects `state > 3`. `below` is 1 when the overlay has anything
strictly under the key.

REG_KEY `node`, present when `has_node == 1` (`KeyReply`):

| Size | Type | Field |
|---:|---|---|
| 1 | u8 | `created` (0/1) |
| 1 | u8 | `volatile` (0/1) |
| 8 | u64 | `last_write`: FILETIME (100 ns ticks since 1601) |
| 4 | u32 | `nvalues` |
| … | value x `nvalues` | `name:str \| ty:u32 \| data_len:u32 \| data` (in insertion order) |
| 4 | u32 | `ntombstones` |
| … | `str` x `ntombstones` | folded names of values deleted here |
| 4 | u32 | `nchildren` |
| … | child x `nchildren` | `folded:str \| spelling:str \| state:u8` (0 present, 1 tombstone), sorted by `folded` in byte order |

With `has_node == 0` the reply is exactly 9 bytes. The decoder inserts the
children into a map keyed by `folded` (`Reply::decode` for REG_KEY), so their
order is not checked and a duplicate `folded` key keeps the last one; send them
sorted and unique, as both encoders do.

A REG_KEY reply longer than `payload_cap - 8` must be answered
`ST_REPLY_TOO_LARGE` instead; the shim then serves
the real key alone. A director may apply the same rule to every registry reply
(Cairn does, `op-registry`); the other replies are at most 10 bytes, so only
REG_KEY's can ever reach it.

Errors: name too
long, data too large, invalid path, too deep: `ST_BAD_REQUEST`; not found:
`ST_NOT_FOUND`; already exists: `ST_EXISTS`. REG_RENAME_KEY with a `new_leaf`
of `.` or `..` is `ST_BAD_REQUEST`.

The overlay's semantics (what a create over a real key records, how tombstones
and `changed_since` behave) are those of `crates/aether-registry/src/overlay.rs`,
which is normative for them; Cairn's `aether.director.registry` reimplements
it. The persisted file format (`crates/aether-registry/src/format.rs`,
`AEREG`) is the director's own business and is not on the wire.

### 4.4 Shim handling of registry replies

`crates/aether-hooks/src/regclient.rs`: a read (REG_LOOKUP, REG_KEY) that fails
for any reason is a fallback: the hook serves the real key alone. A write that
fails is not retried against the real registry. Its status becomes the NT
status the caller gets (`crates/aether-hooks/src/regwrite.rs`, `write_status`):

| Director status | NT status |
|---|---|
| `ST_BAD_REQUEST` | `STATUS_INVALID_PARAMETER` |
| `ST_NOT_FOUND` (the key is tombstoned) | `STATUS_KEY_DELETED` |
| `ST_EXISTS` (rename, create) | `STATUS_CANNOT_DELETE` |
| anything else | `STATUS_UNSUCCESSFUL` |

The exception is a key create (`crates/aether-hooks/src/regkeys.rs`): a
REG_CREATE_KEY answered `ST_EXISTS` (another thread or process created the key
first) is followed by a fresh REG_LOOKUP, and if that says present the key is
opened; otherwise the create fails `STATUS_UNSUCCESSFUL`. There
`ST_BAD_REQUEST` is `STATUS_INVALID_PARAMETER` and anything else
`STATUS_UNSUCCESSFUL`.

A write reply that is not exactly 8 bytes counts as a failed write
(`ST_IO_ERROR`).

### 4.5 Registry generation (`reg_gen`)

Distinct from the overlay `version` in replies. The director keeps one
generation per session (the old Rust director's `RegistryGeneration` and
Cairn's both start at 1) and publishes it into every ring header it serves (offset 40, atomic
fetch-max, never backward). It moves on **every successful registry write,
before the write's reply is published**, and whenever a registry layer is
attached or detached.

The shim caches REG_LOOKUP and REG_KEY answers per folded path under the
generation it read before asking, and uses a cached answer only while the
header still holds that generation. Generation 0 means "nothing published":
the shim then caches nothing and asks the director every time
(`RegClient::cached`). So a director that serves the registry must publish a
non-zero generation before the shim attaches, and must bump it before
replying to any write, or other processes will keep serving stale answers.

Directors have differed when no registry is served. The old Rust director
(removed) registered every ring with its `RegistryGeneration` at start, which
published the current generation, 1, at once, registry or not. Cairn publishes only
when it has a registry (`ring.clj`, `start`), so its header keeps `reg_gen =
0`. Neither matters to the shim without `VFS_REGISTRY`, which then sends no
registry request at all.

---

## 5. Bootstrap config and handshake environment

### 5.1 `shim.cfg` (VFSC, version 2)

Source: `crates/aether-protocol/src/shimcfg.rs`. The director writes it; its
path reaches the shim as the injector's `<config>` argument, which the
injector puts in `VFS_SHIM_CONFIG`.

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | bytes | magic `"VFSC"` (`56 46 53 43`) |
| 4 | 4 | u32 | version, `2` |
| 8 | 4 | u32 | `root_len` |
| 12 | `root_len` | utf8 | `root`: root 0's path as the program sees it |
| 12 + `root_len` | 4 | u32 | `n_static` |
| … | … | entry x `n_static` | `name_len:u32 \| dll_name utf8 \| backing_len:u32 \| backing_path utf8` |

Nothing may follow the table; trailing bytes are an error. `dll_name` is a
final path component (`d3d11.dll`); `backing_path` is an absolute Win32 path
(`\??\` form accepted).

Decoding outcomes (`decode_config`): fewer than 4 bytes or truncated/over-long
or bad UTF-8: `Malformed`; first 4 bytes not `VFSC`: `Unversioned` (a version-1
config began with a `u32` root length); version not 2: `Version { found,
expected }`. Any of these fails the shim's load with ready-file content
`bootstrap-failed:<message>`.

What the current shim does with it: it decodes it (as a version and shape
check) and uses neither field. The client's roots come from the environment
(`crates/aether-hooks/src/bootstrap.rs`). Write `root` as root 0 and the static
imports the launch needs; both directors write it with no static imports.

### 5.2 The handshake tables

Source: `crates/aether-env/src/lib.rs` (`handshake` module); the same tables
are shipped as `contract/env.json`.

| Table | Names | Rule |
|---|---|---|
| `TRANSPORT` | `VFS_RING_PATH`, `VFS_RING_SECTION`, `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_OFFSET`, `VFS_ARENA_LEN`, `VFS_SERVER_EV`, `VFS_CLIENT_EV`, `VFS_FUSE_CFG`, `VFS_VIRTUAL_DIR`, `VFS_VIRTUAL_ROOTS`, `VFS_ROOT_BACKINGS`, `VFS_REGISTRY` | A launch sets **or removes** every one of these, so nothing inherited from an earlier session reaches the child. |
| `INJECT` | `VFS_INJECT_CWD`, `VFS_INJECT_STEAM_HELPER` | Read only by the injector. A Wine launch sets or removes them like `TRANSPORT`. |
| `CONFIG` | `VFS_STATE_DIR`, `VFS_HOME`, `VFS_LAUNCH_IMAGE`, `VFS_DISCOVERY_PATH`, `VFS_SHIM_CONFIG`, `VFS_SHIM_READY`, `VFS_CHILD_REFUSED_LOG`, `VFS_FORCE_INJECTION` | Other handshake configuration; not cleared by a launch. |

Names compare without ASCII case on the Windows side (Wine included), so a
removal must remove every spelling (`is_handshake`).

### 5.3 Each name's value

Sources: `crates/aether-env/src/lib.rs` (constants and `ALL`), readers in
`crates/aether-hooks/src/director.rs`, `crates/aether-shim/src/lib.rs`,
`crates/aether-inject/src/`. Booleans: an **opt-in** switch is on only for
`1`, `true`, `yes`, `on` (ASCII case-insensitive); an **opt-out** switch is
off only for `0`, `false`, `no`, `off` (`opt_in`, `opt_out`).

| Name | Set by | Read by | Value |
|---|---|---|---|
| `VFS_RING_SECTION` | director | shim | Section name (2.9). Used when `VFS_RING_PATH` is unset or empty. |
| `VFS_RING_PATH` | director | shim | Ring file path as the shim sees it (2.10). Wins over the section. |
| `VFS_RING_BYTES` | director | shim | Decimal bytes to map. Must cover the ring and the arena. Shim default if unparsable: 2 097 152. |
| `VFS_RING_PAYLOAD_CAP` | director | shim | Decimal. Must equal the header's `payload_cap`: the shim sizes WRITE chunks and inline reads from this value, and a request over the header's cap is refused locally. Shim default: 1 048 576. |
| `VFS_ARENA_OFFSET` | director | nobody | Decimal mapping offset of the arena. Informational: the shim takes data offsets from bulk replies. |
| `VFS_ARENA_LEN` | director | shim | Decimal arena length; 0 or unset disables bulk reads (2.6). |
| `VFS_SERVER_EV` | director | shim | Server event name (section mode only). |
| `VFS_CLIENT_EV` | director | nobody | Client event name. The shim does not open it. |
| `VFS_FUSE_CFG` | director | nobody | Path of a `key=value` text file describing the ring, for tools (5.4). |
| `VFS_VIRTUAL_DIR` | director (or the injector, from `VFS_INJECT_CWD`) | shim | Root 0's path as the program sees it. **Required** whenever a ring is named: unset fails the load. |
| `VFS_VIRTUAL_ROOTS` | director | shim | Roots beyond 0: `id=path` entries joined by `;`, e.g. `1=C:\Users\me\Documents\My Games\Skyrim`. Whitespace around id and path is trimmed; an entry that does not parse, or has an empty path, is skipped; an entry for id 0 replaces `VFS_VIRTUAL_DIR`. Unset and `""` mean the same: root 0 only (Rust's `encode_roots` leaves it unset; Cairn sets `""`). |
| `VFS_ROOT_BACKINGS` | director | shim | Each root's real backing directory where it differs from its location, same `id=path;…` syntax. The shim treats a backing directory as a second spelling of its root for routing, never for naming. |
| `VFS_REGISTRY` | director | shim | Opt-in. Set to `1` only while a registry layer is attached; otherwise remove it. |
| `VFS_INJECT_CWD` | director | injector | The target's working directory as the target sees it. Unset or empty: the injector's own. |
| `VFS_INJECT_STEAM_HELPER` | director | injector | Under Wine: a Steam helper command line to run before the target, or `off` to only clear a stale helper pid. On native Windows nothing is run, but the injector still writes `<ready>.steam-helper` with `disabled:not running under Wine` (section 6.3). |
| `VFS_STATE_DIR` | director | shim | Session state directory (the shim's panic log goes to `<dir>\shim-panic.log`). |
| `VFS_HOME` | director | director side | Aether's own data directory. |
| `VFS_LAUNCH_IMAGE` | director | shim | Absolute path of the staged EXE; its parent directory is another spelling of root 0. |
| `VFS_DISCOVERY_PATH` | — | — | Registered, no reader in this repository. |
| `VFS_SHIM_CONFIG` | injector | shim | Path of `shim.cfg`. Unset: the shim fails its load. |
| `VFS_SHIM_READY` | injector | shim | Path of the ready file (section 6.2). The shim removes it from its own environment after writing, so children do not inherit it. |
| `VFS_CHILD_REFUSED_LOG` | injector | shim | `<ready>.child-refused`: the shim appends `<image> <reason>\n` for each child it had to kill. |
| `VFS_FORCE_INJECTION` | director | — | Accepted and ignored: remote-thread `LoadLibrary` is the only activation (Cairn still sets `1`). |

Also read, outside the tables: `VFS_READY_TIMEOUT_SECS` (decimal seconds,
default 180, minimum 1) bounds the injector's wait for the shim and the shim's
wait for each child; the injector sets it in the target's environment
(`crates/aether-env/src/lib.rs`, `ready_timeout_secs`). `VFS_RING_SPIN_US`
(director side, 2.11). The remaining `VFS_*` names in `aether-env` are
behaviour, diagnostic, harness and fixture switches and are not part of the
handshake.

### 5.4 `fuse.cfg` (informational)

Written by both directors and named by `VFS_FUSE_CFG`; nothing in the shim or
the injector reads it. Lines of `key=value`, `\n`-terminated
(Cairn `launch.clj`, `fuse-cfg`):

```
section=<name>          (file mode, Cairn only: ring_path=<path>)
root=<root 0>
payload_cap=<n>
ring_bytes=<map size>
arena_offset=<n>
arena_len=<n>
server_ev=<name>        (section mode only)
client_ev=<name>        (section mode only)
```

---

## 6. The injector

### 6.1 Command line

Source: `crates/aether-inject/src/cli.rs`, `crates/aether-inject/src/bin/aether-injector.rs`.

```
aether-injector <target> <shim_dll> <config> <ready> [-- args...]
```

| Position | Meaning |
|---:|---|
| 1 | Target EXE (full path). |
| 2 | Shim DLL (full path), of the target's bitness: `aether_shim.dll` for x64, `aether_shim32.dll` with `aether-injector32.exe` for x86. |
| 3 | Path of `shim.cfg`; becomes `VFS_SHIM_CONFIG`. |
| 4 | Path of the ready file; becomes `VFS_SHIM_READY`. Other files are named by appending a suffix to it. |
| 5… | Target arguments. If argument 5 is exactly `--` it is dropped. |

Fewer than four arguments: the usage line on stderr, exit 2.

The target's command line is `"<target>"` followed by ` "<arg>"` for each
argument, with no escaping (`crates/aether-inject/src/inject.rs`,
`command_line_for`). An argument that contains `"` or ends in `\` is mangled
by the target's command-line parser (a trailing `\` escapes the closing
quote). A director must not pass such arguments, or must accept that they
arrive changed.

Under Wine, every injector argument (target, shim DLL, config, ready file)
and every path in a `VFS_*` variable is in the Windows form the target
sees (`C:\...` or `Z:\...`), not the host path. The director reads the ready
file and its siblings by the corresponding host path.

**The shim's own DLL dependencies must resolve in the target.** The shim is
loaded by full path, but the loader finds its imports through the target's
DLL search path. The release natives are built with `+crt-static`
(`bin/build-natives.ps1`, `RUSTFLAGS`), so they need no C runtime DLL; a build
without it does. Cairn puts the natives folder first on the target's `PATH`
(`src/aether/director/launch.clj`, `launch-env`).

The injector adds to the target's environment (`crates/aether-inject/src/inject.rs`,
`launch_env_block`): `VFS_SHIM_CONFIG`, `VFS_SHIM_READY`,
`VFS_CHILD_REFUSED_LOG = <ready>.child-refused`, `VFS_READY_TIMEOUT_SECS`, and
`VFS_VIRTUAL_DIR = VFS_INJECT_CWD` when a working directory is given and
`VFS_VIRTUAL_DIR` is not already set. Everything else the target sees is
inherited from the injector, so the director sets the transport names in the
injector's environment. Before starting it deletes `<ready>`,
`<ready>.child-refused`, `<ready>.injector-error` and `<ready>.steam-helper`.

### 6.2 The ready file

Written by the shim to `VFS_SHIM_READY` before its `DllMain` returns
(`crates/aether-shim/src/lib.rs`); classified by the injector
(`crates/aether-inject/src/inject.rs`, `classify_ready`). Contents are the
whole file, no newline:

| Content | Meaning | Injector |
|---|---|---|
| `ready` (`READY_OK`) | Hooks live, director attached. | Releases the target. |
| `fuse-failed:<reason>` | A ring was named (or none was) and the client could not attach, or the heartbeat failed. | Kills the target. |
| `bootstrap-failed:<reason>` | Config unreadable or from another build, or a hook would not install. | Kills the target. |
| anything else, or missing | Not decided yet. | Keeps waiting until the timeout. With remote-thread injection the verdict is read once the shim's `LoadLibrary` has returned; if there is still none, the injector kills the target and reports `bootstrap-failed:the shim loaded but never wrote its ready file` (or `target-exited:` if the target is already gone). |

The shim also writes a one-line diagnostic to `<ready>.boot.log` on failure.

### 6.3 The error report and exit codes

On any injection failure the injector writes one line to
`<ready>.injector-error` and exits 3 (`aether-injector.rs`; prefixes in
`crates/aether-env/src/lib.rs`):

| Line | Cause |
|---|---|
| `target-exited:<code as 0x… hex>` | The target exited before reporting ready (e.g. `0xc0000135`). |
| `ready-timeout:<seconds>` | No verdict within `VFS_READY_TIMEOUT_SECS`. |
| `fuse-failed:<reason>` | The shim reported `fuse-failed:`. |
| `bootstrap-failed:<reason>` | The shim reported `bootstrap-failed:`. |
| `inject:<error>` | Any other `InjectError`, in Rust debug form (`CreateProcessFailed(…)`, `RemoteThread`, `Wait`, …). |

| Exit code | Meaning |
|---:|---|
| 2 | Usage error. |
| 3 | Injection failed; see `<ready>.injector-error`. |
| other | The first target process's exit code, after **every** process in its job has exited (`ProcessTree::wait`). |

A target exit code of 2 or 3 is indistinguishable from the injector's own by
the code alone. What distinguishes them is `<ready>.injector-error`: the
injector deletes it at start and writes it only for its own exit 3 (exit 2,
a usage error, writes none). A director should treat 2 or 3 as the injector's
own failure only when that file exists. Cairn's Linux path does not: it
treats any 2 or 3 as the injector's (`src/aether/director/proton/launch.clj`,
`finish`), and only uses the file for the reason.

`<ready>.steam-helper`, written before the target starts when
`VFS_INJECT_STEAM_HELPER` is set: `started:<pid>:<milliseconds>`, `cleared`,
`disabled:<why>` or `failed:<why>` (`crates/aether-env/src/lib.rs`).

### 6.4 Child processes: the ready and failed events

Source: `crates/aether-hooks/src/child.rs`. When the target starts a child, the
shim in the parent injects the shim into it and waits. The child's shim
signals with two named events. Both sides create them with
`CreateEventW(NULL, TRUE /*manual reset*/, FALSE, name)`: the parent creates
both before it waits (`wait_ready`), the child creates the one it sets
(`signal_ready`, `signal_bootstrap_failed`). Either side may be first;
`CreateEventW` opens the existing event for the second.

| Event | Set when |
|---|---|
| `Local\vfs_shim_ready_<pid>` | The child's hooks are installed (`signal_ready`). |
| `Local\vfs_shim_failed_<pid>` | The child's bootstrap failed (`signal_bootstrap_failed`). |

`<pid>` is the child's process id in decimal. The parent waits up to
`VFS_READY_TIMEOUT_SECS` and kills the child on failure, timeout, or exit.
These events are internal to the shim; a director never needs them, but a
tool that starts processes under the shim itself may wait on them.

---

## 7. Versions

**Aether's release version** is semver (`Cargo.toml` workspace `version`;
releases are tags `v<semver>`, README "Releases"). It is separate from the
**wire version**, which is the ring header's `version` field, now 4
(`crates/aether-ipc/src/layout.rs`), also published as `wire_version` in
`contract/artifacts.json` (checked equal by `crates/aether-env/tests/contract.rs`).
`shim.cfg` has its own version, 2 (`CONFIG_VERSION`).

Rules:

- **A wire-version bump is always an Aether major version.** The shim refuses
  a ring of any other version at attach, so an old director and a new shim (or
  the reverse) cannot run together. `layout.rs` bumps it whenever a
  payload layout or the slot state machine changes.
- **A minor version may add an opcode or a flag only where an old director's
  answer to it is safe.** An old director answers an unknown opcode
  `ST_BAD_REQUEST` with an empty payload (both directors), ignores unknown
  slot flag bits and READ flag bits other than bit 0, and writes zero in the
  OPEN reply's flag bits 1-7 and padding. So a new opcode is minor only if the
  shim treats `ST_BAD_REQUEST` from it as "not available" and falls back; a new
  OPEN reply flag is minor only if zero means the old behaviour (as
  `immutable` and `mount_gen` were added: an old director's zeros decode as
  mutable, generation 0). Opcode numbers are never reused (4 and 12 stay
  reserved).
- **A patch changes no byte on the wire**: no opcode, status, flag, layout,
  environment name or value format, file name, event name, or magic.

---

## 8. Conformance

Until a director conformance harness exists, **the code is normative**: where
this document and `crates/aether-ipc`, `crates/aether-protocol` or the shim
disagree, the code is right and this document is wrong.

`contract/protocol.json` is the machine-readable form of the protocol table
(3.3): a director in another language builds its opcode, status and field
tables from it instead of from the prose.

The executable part of the contract is
`crates/aether-protocol/tests/golden/vectors.txt`: one `name hex` line per
vector, produced by the encoders in `crates/aether-protocol/tests/golden.rs`
and checked in CI. A changed existing line is a wire break. A director in
another language should encode the same inputs and compare bytes, and decode
the vectors and compare fields.

| Vector | Encodes |
|---|---|
| `shim-config-v2-root-runtime` | `shim.cfg`, root `C:\GameLayers\runtime`, no imports |
| `shim-config-v2-one-static-import` | `shim.cfg` with `d3d11.dll` -> `C:\GameLayers\d3d11.dll` |
| `ring-header-slots4-cap256` | an initialised ring header (2.12) |
| `open-req-read-skyrim` | OPEN root 0, `OPEN_READ`, `Data/Skyrim.esm`. This tests the codec; the shim itself sends a read open folded (`data/skyrim.esm`, section 3.2) |
| `open-resp-fh42-size1000` | OPEN reply, fh 42, size 1000, mutable, gen 0 |
| `open-resp-immutable-gen-0xa1b2c3d4` | OPEN reply with `immutable` and `mount_gen` |
| `getattr-resp-file-123` | GETATTR reply, found file, size 123, mtime -7 |
| `path-req-root1-a` | path request, root 1, `a` |
| `read-req-fh7-off10-len4` | READ request |
| `read-resp-abcd` | inline READ reply |
| `read-resp-bulk-len5-off65536` | bulk READ reply |
| `readdir-resp-two` | READDIR reply, two entries (walked below) |
| `write-req-fh7-off10-abc`, `write-resp-3` | WRITE request and reply |
| `setattr-req-fh5-size100` | SETATTR request |
| `close-req-99` | CLOSE request |
| `mkdir-req-mode493-dir` | MKDIR request |
| `rename-req-a-b` | RENAME request, `old.txt` -> `new.txt` |
| `names-req-root2-skip1`, `names-resp-two` | STORED_NAMES request and reply |
| `reg-path`, `reg-set-value`, `reg-delete-value`, `reg-create-key-volatile`, `reg-rename-key`, `reg-changed-subtree-since77` | registry requests |
| `reg-changed-reply-true-5`, `reg-version-reply-9`, `reg-lookup-reply-state1-below-42`, `reg-key-reply-none-13`, `reg-key-reply-sample-node-12` | registry replies |

### 8.1 Worked example: `readdir-resp-two`

Inputs (`golden.rs`): entries `{name "a.esp", is_dir false, size 10, mtime 1}`
and `{name "sub", is_dir true, size 0, mtime 0}`. The line is 54 bytes:

```
02000000 05000000 612e657370 00 0a00000000000000 0100000000000000
03000000 737562 01 0000000000000000 0000000000000000
```

| Offset | Bytes | Field | Value |
|---:|---|---|---|
| 0 | `02 00 00 00` | `count` | 2 |
| 4 | `05 00 00 00` | entry 0 `name_len` | 5 |
| 8 | `61 2e 65 73 70` | entry 0 `name` | `a.esp` |
| 13 | `00` | entry 0 `is_dir` | false |
| 14 | `0a 00 00 00 00 00 00 00` | entry 0 `size` | 10 |
| 22 | `01 00 00 00 00 00 00 00` | entry 0 `mtime` | 1 |
| 30 | `03 00 00 00` | entry 1 `name_len` | 3 |
| 34 | `73 75 62` | entry 1 `name` | `sub` |
| 37 | `01` | entry 1 `is_dir` | true |
| 38 | `00 00 00 00 00 00 00 00` | entry 1 `size` | 0 |
| 46 | `00 00 00 00 00 00 00 00` | entry 1 `mtime` | 0 |
| 54 | — | end | |

The entries are packed: nothing aligns `size` or `mtime`. As a reply in a
slot this would be `status = 0`, `payload_len = 54`, `ack = low32(req_id)`,
these 54 bytes at the slot's payload offset, then `state` CAS
`PROCESSING -> COMPLETED`.
