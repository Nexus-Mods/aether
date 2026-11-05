# Aether wire contract

This is the byte-level contract between the Aether shim (the DLL injected into
a game) and a **director** (the process that serves it). It is written so that
a director can be implemented in any language from this file alone, together
with the machine-readable contract files that ship beside it
(`contract/protocol.json`, `contract/env.json`, `contract/artifacts.json`) and
the golden vectors (section 8).

Wire version: 4.

Conventions used throughout:

- **Little-endian.** Every integer in shared memory, in a message, in
  `shim.cfg` and in the golden vectors is little-endian. The shim is x86/x64
  only.
- Offsets in shared memory are byte offsets from the start of the mapping.
  Nothing in shared memory is a pointer.
- `u8`/`u32`/`u64` are unsigned, `i32`/`i64` two's-complement signed.
- `utf8…` means "the rest of the payload, as UTF-8, no length, no
  terminator". `str` means `len:u32 | utf8[len]` (no terminator).
- For the bytes, the golden vectors are normative; for what a director must
  answer, this document is (section 8).

The reference director, which `aether-conform reference-adapter` serves,
follows this document and is the oracle conformance compares a director with
(section 8). Where this document says what "the reference" answers, it
describes that director, not a rule; where directors may differ, it says so
and names each answer allowed.

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
looked up. A request that fails these checks gets a non-OK status; which one
may differ from director to director (section 3.5.1), the shim treats every
non-OK status as a failure, and a new director should answer `ST_BAD_REQUEST`.
The shim trusts the director's answers as content but bounds-checks every
reply: a reply whose `ack`, length or fields do not check out is refused, and
a payload that is short or malformed for its opcode does not decode. The shim
fails closed: a shim that cannot attach to its director fails its own load,
and the injector kills the target before it runs (sections 5 and 6). There is
no mode in which a launched program runs un-virtualised because the director
was unreachable.

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

- `slot_stride = align8(32 + payload_cap)` where `align8(n) = (n + 7) & !7`.
- `slot_off(s) = 48 + s * slot_stride`; the slot's payload starts at
  `slot_off(s) + 32`.
- The arena, if any, starts at `VFS_ARENA_OFFSET` (usually `48 + slot_count *
  slot_stride`, directly after the last slot) and is `VFS_ARENA_LEN` bytes
  long.
- The mapping must be 8-byte aligned; section views and `mmap` are
  page-aligned.

### 2.2 Ring header (48 bytes)

| Offset | Size | Type | Field | Written by | Meaning |
|---:|---:|---|---|---|---|
| 0 | 4 | u32 | `magic` | director | `0x56464950`. In memory the bytes are `50 49 46 56` (`"PIFV"`); the constant reads as `"VFIP"`. |
| 4 | 4 | u32 | `version` | director | Wire version, `4`. |
| 8 | 4 | u32 | `slot_count` | director | Number of slots. |
| 12 | 4 | u32 | `slot_stride` | director | Bytes per slot; must equal `align8(32 + payload_cap)`. |
| 16 | 4 | u32 | `payload_cap` | director | Largest request or reply payload a slot carries. |
| 20 | 4 | u32 | `worker_hint` | director | How many threads serve this ring; 0 = not stated. Written after init, before any client attaches. |
| 24 | 8 | u64 | `req_seq` | clients | Request id counter: each client does an atomic fetch-add of 1 to take a `req_id`. Starts at 0. |
| 32 | 4 | u32 | `submit_seq` | clients | Count of published requests (atomic fetch-add after each publish). Diagnostic; a director need not read it. |
| 36 | 4 | u32 | `_pad2` | — | Zero. |
| 40 | 8 | u64 | `reg_gen` | director | Registry generation (section 4.5). 0 = none published. Only ever moved upward (atomic fetch-max). |

Initialising a ring writes magic, version, slot_count, slot_stride,
payload_cap, `worker_hint = 0`, `req_seq = 0`, `submit_seq = 0`, `reg_gen =
0`, and sets every slot's `state` to `FREE`. It need not zero payloads.

### 2.3 Slot header (32 bytes)

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

The shim refuses the ring (and so fails its load) unless all of these hold:

1. The mapping is at least 48 bytes.
2. `magic == 0x56464950` and `version == 4`.
3. `slot_stride == align8(32 + payload_cap)`.
4. `48 + slot_count * slot_stride <= mapping length`.

Both are worked without wrapping, on 32-bit clients too: a `payload_cap`
within 32 bytes of `2^32` has no stride a u32 holds, and is refused.

The mapping length the shim uses is `VFS_RING_BYTES` (section 5.3). Nothing
else in the header is validated.

### 2.5 Slot states and transitions

States:

| Value | Name | Owner of payload (and arena bank) |
|---:|---|---|
| 0 | `FREE` | nobody |
| 1 | `CLAIMED` | the client that claimed it |
| 2 | `SUBMITTED` | nobody writes; the request is published |
| 3 | `PROCESSING` | the director thread that took it |
| 4 | `COMPLETED` | the client (reads the reply, and the bank for a bulk read) |
| 5 | `ABANDONED` | still the director thread; the client has stopped waiting |

Transitions:

| From | To | Who | How | When |
|---|---|---|---|---|
| FREE | CLAIMED | client | CAS (acquire) | To start a request. A client scans from a per-thread hint, wrapping. |
| CLAIMED | SUBMITTED | client | store (release) | After writing `opcode`, `flags`, `payload_len`, `req_id` and the payload. Then `submit_seq += 1`. |
| SUBMITTED | PROCESSING | director | CAS (acquire) | A director thread takes the request. |
| PROCESSING | COMPLETED | director | CAS (release) | After writing the reply payload, `status`, `payload_len`, `ack`. |
| COMPLETED | FREE | client | store (release) | After reading the reply (and, for a bulk read, copying the arena bank). |
| CLAIMED | FREE | client | store or CAS | Gives up before publishing. |
| SUBMITTED | FREE | client | CAS | Gives up on a request no director took. |
| COMPLETED | FREE | client | CAS | Gives up on a request answered a moment ago. |
| PROCESSING | ABANDONED | client | CAS | Gives up on a request a director is still processing. The slot stays the director's. |
| ABANDONED | FREE | director | CAS (release) | The director finishes an abandoned request: drops its reply, frees the slot. |

No other transition is legal. In particular **a client must never free a slot
in `PROCESSING`**, and **a director must never store `COMPLETED` over
`ABANDONED`**: the first would let the next request in that slot (and its arena
bank) be overwritten by a late reply; the second would leave the slot unusable
for ever. A peer of a wire version without `ABANDONED` is refused like any
other version (section 2.4).

**The director's completion, step by step:**

1. Read `req_id` from the slot. If it is no longer the id it took, write
   nothing (a non-conforming client recycled the slot).
2. If `state == ABANDONED`: CAS `ABANDONED -> FREE`, done.
3. If the reply is longer than `payload_cap`: `status = i32::MIN`
   (`0x80000000`), `payload_len = 0`. Otherwise copy the reply to the payload
   area and set `status` and `payload_len`.
4. Store `ack = req_id & 0xFFFFFFFF`.
5. CAS `PROCESSING -> COMPLETED`. If that fails because the client moved the
   slot to `ABANDONED` meanwhile, CAS `ABANDONED -> FREE`.

A director may instead check neither `req_id` nor `ABANDONED` first: write
the reply payload in place while the slot is `PROCESSING` (or `ABANDONED`,
which is still the director's), then store `status`, `payload_len` and
`ack`, CAS `PROCESSING -> COMPLETED`, and if that CAS fails, CAS `ABANDONED
-> FREE`. Both orders are correct against a conforming client, because an
`ABANDONED` slot and its bank stay the director's.

**Every taken slot MUST be completed.** A director that has moved a slot to
`PROCESSING` must eventually run step 3-5 (or 2) for it, whatever happens:

- A slot whose `payload_len > payload_cap` MUST be completed with
  `ST_BAD_REQUEST` and payload length 0. A worker that stops on such a slot
  leaves it `PROCESSING` for ever and the director one thread short.
- A handler that fails (throws, panics) MUST still complete its slot, with
  `ST_IO_ERROR`.

A slot left in `PROCESSING` is lost to the ring, and when its client gives up
it goes `ABANDONED` and stays there; the client's data-gate permit for it
(section 2.8) is retired until the slot drains, so it never comes back
either. Enough of them and the process can no longer read.

**The client's read:** load `state` with acquire; if it is `COMPLETED`, check
`ack == low32(req_id)` (else the reply is refused), read `status`, read
`payload_len` (refused if `> payload_cap`), copy the payload.

**Memory ordering.** The state word is the synchronisation point: every
non-atomic write to a slot happens before the release store or CAS that
publishes it, and every read happens after an acquire load or CAS that observed
it. On x86/x64 a plain aligned 32-bit load is an acquire and a `lock cmpxchg`
is a full barrier; a director in another language needs a real atomic CAS on
the 4-byte `state` field (it is 4-aligned, since slots start at 48 and strides
are multiples of 8) and an atomic 8-byte fetch-max on `reg_gen` (offset 40,
8-aligned).

**Deadlines.** A client that gets no free slot, or no reply, within 60 s
abandons its slots and reports `ST_IO_ERROR` to the hooked call. A client
spins for 1 ms before it starts yielding; the shim then calls `SwitchToThread`
until 20 ms have passed and `Sleep(1)` after.

**Order of service.** Slots are independent: a director may take and answer
them in any order and from any number of threads. Requests from one shim
thread are issued one at a time, except a pipelined read, which publishes up
to 4 requests at once (8 for reads of 4 MiB or more) and waits for all of
them.

### 2.6 The bulk arena and its banks

The arena is cut into banks, one per slot:

- `banks = slot_count`.
- `bank_size = max(arena_len / banks, 4096)`.
- Slot `s` owns bank `s % banks`, at mapping offset
  `arena_offset + (s % banks) * bank_size`.

A bulk `READ` reply carries the mapping offset of the data (section 3.4.4),
not a bank number, so a client never computes bank positions itself. The bank
follows the slot's ownership (section 2.5): the director writes it while the
slot is `PROCESSING`, the client reads it while the slot is `COMPLETED`, and
the client frees the slot only after copying it out. A pipelined read holds
all of its slots until every reply is in, then copies each bank out and frees
each slot.

Sizing rules a director must meet, from what the shim sends:

- The shim's bulk chunk is `clamp(VFS_ARENA_LEN / slot_count, 256 KiB,
  1 MiB)` bytes. A director answers at most `min(len, bank_size)` bytes, and
  the shim reads a short answer as end of file. So **each bank must be at
  least the shim's chunk**: with `bank_size = arena_len / slot_count`, that
  means `arena_len >= 256 KiB * slot_count`. The shim uses a ring whose
  `VFS_ARENA_LEN` is smaller as if it had no arena, except that its inline
  READs stay below 65 536 bytes so that no director answers them in bulk.
- Every arena byte must lie inside the first `VFS_RING_BYTES` bytes of the
  mapping. The shim copies a bulk reply's data only from inside the
  slot's own bank, and only as many bytes as the request asked for; with
  no arena in use (`VFS_ARENA_LEN` unset, 0, or below the rule above),
  only from inside the mapping. It fails the read otherwise (section 3.7).
- With `VFS_ARENA_LEN` unset or 0, the shim never sets `FLAG_READ_BULK` and
  sends inline reads of up to `payload_cap - 8` bytes; a director with an
  arena may still answer those in bulk (section 3.4.4), and the shim accepts
  either form.

### 2.7 Default geometry

A director may choose any geometry that meets sections 2.4 and 2.6; the shim
reads it from the header and the environment. The table is one consistent
geometry: the payload cap, arena and worker count are the defaults of
Aether's Rust ring code, and the slot count is an example:

| Quantity | Formula | Default |
|---|---|---:|
| `slot_count` | — (example) | 32 |
| `payload_cap` | — | 1 048 576 |
| `slot_stride` | `align8(32 + payload_cap)` | 1 048 608 |
| ring bytes = `arena_offset` | `48 + slot_count * slot_stride` | 33 555 504 |
| `arena_len` | — | 33 554 432 (32 MiB) |
| `bank_size` | `max(arena_len / slot_count, 4096)` | 1 048 576 |
| map size (`VFS_RING_BYTES`) | `max(round_up(ring + arena, 65536), 2 MiB)` | 67 174 400 |
| workers | `worker_hint`; the shim caps it at `slot_count` (2.8) | 4 |

### 2.8 Client concurrency and `worker_hint`

A director thread runs one request to completion, so a slow read holds a
thread. The shim keeps its *data* requests (READ, WRITE, SETATTR and write
OPENs) in flight below the director's thread count, so that some thread is
always free for a stat or a listing. Its data gate allows:

```
workers = (worker_hint == 0 ? 4 : worker_hint), capped at slot_count
reserve = max(workers / 4, 1)
limit   = max(workers - reserve, 1)
```

A director should therefore publish its real thread count in `worker_hint`
before the shim attaches, and run at least that many threads. Each shim reads
`worker_hint` once, when it attaches; a later change is not seen by processes
already attached. The gate is process-local; nothing about it crosses the
ring.

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

Each request takes effect atomically with respect to every other on the
ring, from any client:

- of concurrent `OPEN_EXCL` creates of one missing name, or REG_CREATE_KEYs
  of one missing key, exactly one succeeds;
- concurrent WRITEs to disjoint ranges all land;
- each append WRITE lands whole at an end;
- a request concurrent with a RENAME or DELETE sees the state before it or
  the state after it, never a mixture;
- of a create (`OPEN_CREATE`) and a MKDIR of one missing name, at most one
  succeeds, and the name is then one entry of one kind; a create under a
  directory concurrent with a RENAME of that directory lands under one of
  its two names, or is refused as a create under a missing parent.

The reference serves each file request under one lock over its tree, and
each registry request under one lock over its overlay.

### 2.9 Windows backing: a named section and two events

| Object | Director creates with | Shim opens with | Named by |
|---|---|---|---|
| Section | `CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, size_hi, size_lo, name)`; an existing section of that name is an error | `OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, name)` then `MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, VFS_RING_BYTES)` | `VFS_RING_SECTION` |
| Server event | `CreateEventW(NULL, FALSE /*auto-reset*/, FALSE, name)`; existing is an error | `OpenEventW(EVENT_MODIFY_STATE = 0x0002, FALSE, name)`; failure is tolerated (no wake) | `VFS_SERVER_EV` |
| Client event | as the server event | not opened by the shim | `VFS_CLIENT_EV` |

Names are free-form; `<section>_srv` and `<section>_cli` for the events is
one convention. A name must be unique to the session: a director creates each
object new and refuses a pre-existing one.

A director may also serve a Windows shim from a file (section 2.10): the shim
honours `VFS_RING_PATH` on Windows too.

### 2.10 Linux under Wine: a file both sides map

A page-file section has no identity outside one Windows (or Wine) session, so a
native Linux director and a shim under Wine share a file:

- The director creates the file, grows it to at least the map size before
  mapping it (an `mmap` past end of file faults on touch), never truncates an
  existing file in place, and maps it `MAP_SHARED`, `PROT_READ | PROT_WRITE`.
  An existing file is removed and a new one created in its place, so a stale
  reader keeps the old file's pages.
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
re-check it in a loop; a lost wake costs latency, never a request.

| Mode | Shim (client) | Director (server) |
|---|---|---|
| Section | `SetEvent(server_ev)` after publishing and after freeing a slot; spins for its reply (2.5, "Deadlines") | After serving a request, spins for `VFS_RING_SPIN_US` µs (default 400); otherwise `WaitForSingleObject(server_ev, 1)`. Sets `client_ev` after completing (the shim does not wait on it). |
| File | no event; spins | Spins for `VFS_RING_SPIN_US` µs (default 400) after the last completed request, then sleeps 100 µs, 500 µs once quiet for more than 5 ms, 2000 µs once quiet for more than 50 ms. |

`VFS_RING_SPIN_US` is read by the director side only; `0` disables the spin.
These numbers are the reference's; a director may wake on any schedule, since
only latency depends on it.

### 2.12 Worked example: an initialised header

Golden vector `ring-header-slots4-cap256` (a ring initialised with 4 slots
and `payload_cap` 256):

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

A reply's payload is meaningful only when `status == ST_OK (0)`. A director
answers every error with an empty payload. The shim decodes a reply payload
only after checking `status == ST_OK`; a reply that then fails to decode is
treated as `ST_BAD_REQUEST`.

### 3.2 Shared encodings

**Root ids.** A session has several managed roots, each a `u32` id. Root 0 is
`VFS_VIRTUAL_DIR`; others come from `VFS_VIRTUAL_ROOTS` (section 5.3). Every
path-carrying file request starts with the root it addresses. Handle-keyed
requests (READ, WRITE, SETATTR, CLOSE) carry no root: the handle implies it.

**Path request**, used by GETATTR, READDIR, DELETE, STAGE_EXEC:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | rest | utf8… | `vpath` |

**vpaths** as the shim sends them:

- Relative to the root, components joined by `/`, no leading or trailing `/`.
- The root itself is `"."`.
- Folded (below, "Case folding") except in a request that creates a name (a
  write OPEN, MKDIR, RENAME targets), which keeps the caller's spelling so
  that the name is stored as created.
- A named stream suffix (`file.esp:stream`) is carried through, folded.
  A director that holds no stream of that name must never answer it with
  the base file's attributes or bytes. A director either holds named
  streams, never listing them in READDIR and never changing the base file's
  size or bytes through them, or treats every request naming one as a name
  it does not have: GETATTR `found = 0`; OPEN (creating or not), MKDIR,
  DELETE, RENAME at either end, RENAME_X at either end and STORED_NAMES
  `ST_NOT_FOUND` or `ST_BAD_REQUEST`. It never stores a name containing `:`
  as an entry. The reference holds no named streams and answers
  `ST_NOT_FOUND` to every such request (GETATTR `found = 0`).

A director must look paths up case-insensitively under the same fold, and must
treat a path that would leave its root (`..` above the root) as an error.

#### Case folding

The fold is part of the wire: the shim sends folded paths, and a director has
to agree with it about which names are the same name.

**The function.** `fold(s)` replaces every character of `s` with its Unicode
lowercase mapping (the `Lowercase_Mapping` property, including the
unconditional multi-character mappings of `SpecialCasing.txt`), character by
character. An all-ASCII string is lowercased byte by byte, which gives the
same result. There is no locale, no context and no normalisation:

- `Σ` always folds to `σ`; the final-sigma rule is not applied, because the
  fold works one character at a time.
- Composed and decomposed forms stay different names: `é` (U+00E9) and `e`
  plus U+0301 do not fold to the same string.
- The mapping is Unicode 17.0.0's. This is the full lowercase mapping, not
  case folding (`CaseFolding.txt`) and not the simple one-character mapping:
  `İ` folds to two characters, and `ẞ` (U+1E9E) to `ß`, not `ss`. A change
  of Unicode version is a change of the fold (below).

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
- A request that creates a name stores the spelling of the component it
  creates; the components above it keep their stored spelling, however the
  request spells them.
- A director whose store would treat as one name two names whose folds
  differ (NTFS's uppercase table puts `ς` and `σ` together, which the fold
  keeps apart) may refuse the create of the second with `ST_EXISTS`,
  creating nothing; it never serves the two as one file. Names whose folds
  are equal are one name whatever the store does (NTFS keeps `ß` and `ẞ`
  apart, which the fold puts together).
- Fold `Registry`, `Machine`, `User` and every key and value name the same way
  (section 4).

Changing the fold changes the wire: a director built against one fold
disagrees with a shim built against another about which names are equal. Any
change to it is a wire version bump (section 7).

**Path syntax a director should accept:** `/`-separated, no leading slash,
`"."` for the root; that is the only form the shim sends. Directors may
differ on anything else, so a client must not rely on it. A director may
normalise every path (`\` to `/`, strip `/` at both ends, drop empty and `.`
components, resolve `..`, error if `..` leaves the root), and its OPEN may
refuse, with `ST_BAD_REQUEST`, a path that starts with `/` or `\` or contains
`\` at all.

**Booleans.** In file-op replies a flag byte is read as `!= 0`. In registry
messages a boolean must be exactly 0 or 1.

**Lenient vs strict.** The file-op decoders ignore trailing bytes after the
fields they read. Separately, the final `pad:u32` of a READ request is
optional: a decoder stops after READ's `len`, so a 20-byte READ decodes.
WRITE's pad is **not** optional: it is read before the data, so a WRITE must
have its 24-byte header. The registry decoders reject trailing bytes,
truncation, bad UTF-8 and out-of-range values.

**Timestamps.** `mtime` is seconds since the Unix epoch, signed; 0 or less
means "no time" and the shim substitutes 2024-01-01T00:00:00Z. Which time a
director reports for a path, if any, is the director's to choose: the shim
checks no time it is sent. A director should report one `mtime` for a path
in GETATTR and in READDIR until the path changes. The reference reports 0 everywhere.

A file's time is the same in every query that reports it. A directory listing
and a query by path (the attribute queries and the query by name, every class
that carries times) report the `mtime` its READDIR or GETATTR reply carried,
or the substitute when that is 0 or less. A query by handle (the basic,
network-open, all-information and stat classes, which `GetFileTime` and
`GetFileInformationByHandle` reach) has no time from OPEN, so the handle's
first time query sends one GETATTR for its path and reports its `mtime` the
same way. The handle keeps that answer until a WRITE that lands bytes or a
SETATTR reaches the same file (the same path, case apart) through any handle
in the process, after which its next time query asks again. A change made
outside the process after a handle kept its time is not seen through that
handle while it stays open.

The shim learns from these answers whether the director sends times. Once any
GETATTR or READDIR reply has carried an `mtime` above 0, every handle asks as
above. If a handle's first time query in a process comes before any reply has
carried a time and its GETATTR carries none, the shim takes it that the
director sends no times: later handle time queries report the substitute and
send nothing, until a GETATTR or READDIR reply carries a time. A director that
sends no times so pays at most one GETATTR per process for them.

### 3.3 Opcode table

Opcode numbers are never renumbered. The table is also published as data,
`contract/protocol.json`: every opcode with its request and reply fields in
wire order (padding included, as `pad` entries), strictness, the statuses and
the flag bits. It is generated from the same table the Rust code encodes and
decodes with, and Aether's tests check the two equal. It is the
machine-readable form of the layouts below, for
building a director's tables in another language.

The file defines its own field types, so it can be read without this
document. Its `"types"` section gives each field type's wire form, and
`"attributes"` the keys a field may carry besides `name` and `type`; the
types are:

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
| 4 | *reserved* | — | `ST_BAD_REQUEST` | never sent |
| 5 | READ | 24 bytes | inline or bulk, 3.4.4 | 0 or `FLAG_READ_BULK` |
| 6 | WRITE | 24 + data | 8 bytes, 3.4.5 | 0 |
| 7 | SETATTR | 16 bytes | empty | 0 |
| 8 | RENAME | `root \| from_len \| from \| to` | empty | 0 |
| 9 | DELETE | path request | empty | 0 |
| 10 | MKDIR | `root \| mode \| vpath` | empty | 0 |
| 11 | CLOSE | 8 bytes | empty | 0 |
| 12 | *reserved* | — | `ST_BAD_REQUEST` | never sent |
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
`ST_BAD_REQUEST` with an empty payload.

### 3.4 Each opcode

#### 3.4.1 GETATTR (1)

Request: path request. Reply, 18 bytes:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 1 | u8 | `found` (0/1) |
| 1 | 1 | u8 | `is_dir` (0/1) |
| 2 | 8 | u64 | `size` |
| 10 | 8 | i64 | `mtime` |

A missing path is **`ST_OK` with `found = 0`** and the other fields zero, not
an error status. A root the director does not serve is also `found = 0`. The
shim treats `ST_NOT_FOUND` the same as `found = 0`; any other non-OK status
fails the hooked call with `STATUS_UNSUCCESSFUL`.

The `size` of a directory is 0, as in a READDIR entry (3.4.2). A file's
`size` is its length now, WRITEs and SETATTRs through handles still open
included. The same holds for READDIR entries and OPEN replies.

#### 3.4.2 READDIR (2)

Request: path request (the directory; `"."` for the root). Reply:

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

The shim presents the listing to the program whole and unaltered, apart from
dropping `<name>.__vfs_wh__` whiteout markers. It adds no `.` or `..`
entries, so a director should not include them either; it does not sort or
otherwise interpret the order, so the program sees entries in the order sent.
A director may send the entries in any order. `size` is copied into both
`EndOfFile` and `AllocationSize` of the NT directory record for every entry,
directories included; send 0 for a directory (as the golden vector does).

The whole listing is one reply and must fit `payload_cap`. Unknown root:
`ST_NOT_FOUND`. A missing directory may be answered `ST_NOT_FOUND` or with
an empty listing; a file `ST_NOT_A_DIRECTORY`, `ST_NOT_FOUND` or an empty
listing. (The shim lists only through a directory handle it opened, and
shows `ST_NOT_FOUND` as an empty listing, so each answer gives the program
an empty or a failed listing.) The reference answers both with an empty
listing. An oversized listing is answered with the overflow status
`i32::MIN` (section 2.5, step 3).

The shim lists a directory answered `ST_NOT_FOUND` as empty (it is gone).
Any other failure (another status, the overflow status, a reply that does
not decode, no reply in time) fails the program's query with
`STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`) and keeps nothing, so its next
query on that handle sends READDIR again.

#### 3.4.3 OPEN (3)

Request:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | 4 | u32 | `oflags` (table below) |
| 8 | rest | utf8… | `vpath` |

Open flags:

| Bit | Value | Name | Meaning |
|---:|---:|---|---|
| 0 | 1 | `OPEN_READ` | read access |
| 1 | 2 | `OPEN_WRITE` | write access |
| 2 | 4 | `OPEN_CREATE` | create if absent |
| 3 | 8 | `OPEN_EXCL` | fail with `ST_EXISTS` if present |
| 4 | 16 | `OPEN_TRUNC` | truncate on open |
| 5 | 32 | `OPEN_APPEND` | each WRITE lands at the file's end as it is when the WRITE is served, after every other handle's WRITEs and SETATTRs; the director ignores WRITE offsets |

`oflags == 0` is read as `OPEN_READ`. The shim sends `OPEN_READ` for a read
open, with a folded path; and for a write open `OPEN_WRITE` plus the create
bits for the NT disposition: `FILE_SUPERSEDE`/`FILE_OVERWRITE_IF` =
`CREATE|TRUNC`, `FILE_CREATE` = `CREATE|EXCL`, `FILE_OPEN_IF` = `CREATE`,
`FILE_OVERWRITE` = `TRUNC`, `FILE_OPEN` = none. The shim never sends
`OPEN_APPEND`.
The shim never adds `OPEN_READ` to a write open, even when the program asked
for read access as well: a handle opened with `OPEN_WRITE` serves READ and
SETATTR too, and the shim reads and truncates through it. A director must
therefore serve READ on any handle it opened; the reference director checks
no access bits on READ.

`OPEN_TRUNC` or `OPEN_APPEND` without `OPEN_WRITE`, and `OPEN_CREATE`
without `OPEN_WRITE`, are never sent by the shim. A director may ignore
these bits on a read open, or answer `ST_BAD_REQUEST`. The reference ignores
them: a read open with `OPEN_CREATE` of a missing name is `ST_NOT_FOUND`.

Reply, 24 bytes:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh`: the director's handle |
| 8 | 8 | u64 | `size` |
| 16 | 1 | u8 | `is_dir` |
| 17 | 1 | u8 | `flags`: bit 0 is `immutable` (1); other bits zero |
| 18 | 2 | — | zero |
| 20 | 4 | u32 | `mount_gen` |

- `fh` is any u64 the director chooses except 0, unique among open handles.
  The reference starts at 1 and counts up. A client may send `CLOSE(0)` when a
  program closes a directory it created with MKDIR, which holds no director
  handle, so a director never issues 0. The shim sends no such CLOSE (3.4.10)
  and gives 0 no meaning of its own: it carries whatever `fh` an OPEN
  returned.
- `immutable`: the handle's bytes cannot change while it is open. The shim's
  read cache caches small reads only through read-only, immutable handles.
  Never set it for a write open or a directory. Since every handle of a file
  reads the file as it is now (3.4.4), a handle on a file any request can
  change cannot be immutable: never set it for a file a writable provider
  holds.
- `mount_gen`: a generation the director bumps whenever a root's content is
  replaced. The cache keys a file's blocks by (root, folded path, size,
  `mount_gen`), so two immutable opens of one path with equal size and
  `mount_gen` must have identical content. A director that never replaces a
  root's content may send the same value for every open.
- The `size` of a directory is 0, as in GETATTR (3.4.1).
- Errors: unknown root `ST_NOT_FOUND`; `OPEN_WRITE` on a root with no writable
  provider `ST_READ_ONLY`; `OPEN_EXCL` on an existing path `ST_EXISTS`;
  missing path without `OPEN_CREATE` `ST_NOT_FOUND`.
- A write open of an existing path no writable provider serves is
  `ST_READ_ONLY`, `OPEN_EXCL` or not. Where a writable provider serves it,
  `OPEN_EXCL` on an existing path, a directory included, is `ST_EXISTS`,
  checked before the kind of entry at the path. Any other write open of a
  directory is `ST_IS_DIR`; a director may answer `ST_IO_ERROR` instead. The
  reference answers `ST_IS_DIR` with `OPEN_CREATE` and `ST_IO_ERROR`
  without it.
- A create (`OPEN_CREATE` of a missing name) whose parent is missing is
  `ST_NOT_FOUND`; one whose parent is a file is `ST_NOT_FOUND` or
  `ST_NOT_A_DIRECTORY`. Nothing is created. The reference answers
  `ST_NOT_FOUND` for both.

#### 3.4.4 READ (5)

Request, 24 bytes (a director must accept 20):

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh` |
| 8 | 8 | u64 | `offset` |
| 16 | 4 | u32 | `len` |
| 20 | 4 | — | zero |

The director answers in bulk when (`flags & FLAG_READ_BULK`) or `len >=
65536`, and it has an arena; otherwise inline.

Inline reply, `8 + n` bytes, `n <= min(len, payload_cap - 8)`:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `n` (bit 31 clear) |
| 4 | 4 | — | zero |
| 8 | `n` | bytes | data |

Bulk reply, 16 bytes, `n <= min(len, bank_size)`:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `n \| 0x80000000` (`READ_RESP_BULK_BIT`) |
| 4 | 4 | — | zero |
| 8 | 8 | u64 | `arena_offset`: mapping offset of the data (the slot's bank) |

A handle serves READ whatever access it was opened with: the shim's write
opens send `OPEN_WRITE` and the create bits but never `OPEN_READ`, and a
program reads its save back through the handle that wrote it. The reference
does not look at the access.

The client tells the forms apart by bit 31 of the first word, so it must
accept either whatever flag it sent. `n < len` means end of file; the shim
stops there. Errors: unknown `fh` `ST_BAD_FH`; a directory handle
`ST_IS_DIR`.

So a READ comes back whole, `n = min(len, size - offset)`, whenever its form
can hold that many bytes; a shorter answer is a truncated file to the shim.
That holds at any offset, with no wrap: a READ at or past the end answers 0
bytes, whatever its `offset` and `len`.

Every handle of a file reads the file as it is now. A WRITE or SETATTR
through any handle, or a truncating OPEN, by any client, is seen by the next
READ through every other handle of that file, as NTFS shows it.
A director that gives an arena (`VFS_ARENA_LEN`) must answer a READ that sets
`FLAG_READ_BULK` of more than `payload_cap - 8` bytes in bulk: an inline
reply cannot hold it. The shim's own READs on a ring whose arena meets
section 2.6's rule are that section's bulk chunk and remainders of 64 KiB up
to it, with the flag, and inline READs of at most 65 535 bytes. With
`VFS_ARENA_LEN` unset or 0 it sends no flag and its inline READs are of at
most `payload_cap - 8` bytes. An arena below 2.6's rule counts as none, except
that the inline READs stay at 65 535 bytes at most, so that a director with
that arena still answers them inline. The read cache fetches runs of up to 16
blocks of 64 KiB the same way.

For a section over a file (a program's `CreateFileMappingW`), the shim takes
OPEN's `size` as the file's end: a READ that fails, or answers short of
`min(len, size - offset)`, fails the fill. A section filled at creation then
fails (`STATUS_UNSUCCESSFUL`); an image section (`SEC_IMAGE`) is read whole,
asked again up to three times, and then fails (`STATUS_UNSUCCESSFUL`); a page
filled on first touch is asked again up to three times and then raises
`STATUS_IN_PAGE_ERROR`, as NT does when a mapped file's paging read fails.
Each of those READs is bounded by the client's deadline. Past `size`, a page
holds zeros. An image section over a file larger than 256 MiB is refused
(`STATUS_INVALID_FILE_FOR_SECTION`) before any READ.

None of this changes what the director sees; it is how the shim holds the
bytes. A section filled on first
touch (a file over 256 MiB) is a view of a pagefile section of the shim's
own, so no thread reads a page before its fill is in place. That costs about
4 MiB of commit and 2 MiB of paged pool per GiB of file while it is mapped
(measured on Windows 11), before any page is filled. Where that section
cannot be had, or the host cannot fill through a second view, the shim
uses private memory instead, which costs nothing up front but can show a
page's zeros to another thread while the page fills; the stats report counts
each kind.

#### 3.4.5 WRITE (6)

Request:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 8 | u64 | `fh` |
| 8 | 8 | u64 | `offset` (ignored on an `OPEN_APPEND` handle) |
| 16 | 4 | u32 | `len` |
| 20 | 4 | — | zero |
| 24 | `len` | bytes | data |

A request whose payload is shorter than `24 + len` is `ST_BAD_REQUEST`. The
shim sends at most `VFS_RING_PAYLOAD_CAP - 24` bytes per WRITE and repeats
until done or a short write. A director writes every byte of a WRITE or
fails it: on `ST_OK`, `written` is `len`.

A WRITE past the end of the file grows it, and the bytes between the old end
and the WRITE's offset read as zeros, as NTFS fills them. The same holds for
SETATTR (3.4.6). A WRITE of `len = 0` is `ST_OK` with `written = 0` and
changes nothing, wherever its offset.

A WRITE whose end would pass `u64::MAX`, or the largest file the director
can hold, is `ST_NO_SPACE` or `ST_BAD_REQUEST`; the file is unchanged and
the slot is completed. The reference holds files of up to 64 MiB and
answers `ST_NO_SPACE`.

Through a handle opened without `OPEN_WRITE` a director may honour a WRITE
or SETATTR, or answer `ST_READ_ONLY`; on a file no writable provider holds
it is `ST_READ_ONLY`, and the file does not change. The reference honours
both on a file a writable provider holds. (The shim never sends either.)

Reply, 8 bytes: `written:u32 | zero:u32`. The shim reads only the first 4
bytes.

A WRITE that fails ends the program's write, and the shim sends no further
chunk. It reports what the chunks before it wrote in `IoStatus.Information`,
and the file's size grows by that, but the file pointer stays where the call
found it, as NTFS leaves it after a failed write, so a retry of the same
buffer lands where this write started (an append-only handle's end of file
still moves). The status is `STATUS_DISK_FULL` for `ST_NO_SPACE` and
`STATUS_UNSUCCESSFUL` for any other failure. A
`written` larger than the chunk's `len` is a reply the shim cannot use: that
chunk counts as not written and the write fails `STATUS_UNSUCCESSFUL`.

A WRITE on a directory handle the writable provider holds is `ST_IS_DIR`,
whatever access the directory was opened with; SETATTR (3.4.6) on one is too.
A directory no writable provider holds may answer `ST_READ_ONLY` instead.

#### 3.4.6 SETATTR (7)

Request, 16 bytes: `fh:u64 | size:u64`. Sets the file's length (truncate or
extend) on an open handle; there is no path-keyed form. Reply: status only.
Unknown `fh` `ST_BAD_FH`. An extension reads as zeros, bytes a truncation cut
off included when the file grows again. A SETATTR to a size beyond the
largest file the director can hold is `ST_NO_SPACE` or `ST_BAD_REQUEST`, and
the file is unchanged.

#### 3.4.7 RENAME (8)

Request:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` (both paths are under it) |
| 4 | 4 | u32 | `from_len` |
| 8 | `from_len` | utf8 | `from` |
| 8 + `from_len` | rest | utf8… | `to` |

Reply: status only.

For a name the writable provider holds (for names that exist only in a
read-only layer beneath it, see "Names only in a read-only lower" after
3.4.9): `to` is stored with the caller's spelling (section 3.2), so a rename
that changes only the case respells the name. A file or a directory moves; a
directory moves with everything under it. A file or an empty directory at
`to` is replaced; a directory at `to` with anything under it is
`ST_IO_ERROR`, and nothing moves. The root itself at either end, or a `to`
inside `from`, is `ST_BAD_REQUEST` (as the reference director answers) or
`ST_IO_ERROR` (as an overlay answers that never moves a directory only its
lower layer holds, the root included), and nothing moves. A missing `from` is
`ST_NOT_FOUND` whatever `to` is, the root at either end aside; either end
outside every writable provider `ST_READ_ONLY`.

- **Kinds.** A directory onto an existing file is `ST_IO_ERROR`, and nothing
  moves. A file onto an empty directory may replace it (`ST_OK`) or be
  refused with `ST_IO_ERROR`; the reference replaces it.
- **The same name.** A `to` equal to `from`, in the same spelling, is
  `ST_OK` and nothing changes.
- **An ancestor.** A `to` that is an ancestor of `from`, other than the
  root, is a directory with something under it: `ST_IO_ERROR`.
- **A missing parent.** A `to` whose parent is missing is `ST_NOT_FOUND`;
  one whose parent is a file is `ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`.
  Nothing moves. The reference answers `ST_NOT_FOUND` for both.

The reference checks in this order: the root at an end or a `to` inside
`from`, then a missing `from`, then a read-only end, then `to`'s parent,
then what is at `to`.

A file with open handles is renamed like any other. Its handles follow it:
a READ, WRITE or SETATTR through one acts on the file under its new name,
and so for a file under a renamed directory. After a RENAME_X a handle on
the moved file may follow it to `to_root` or answer as 3.4.8 says for a
deleted file; it never acts on a file later created at the old name. (The
shim renames through `FileRenameInformation` on the program's own handle
and goes on using that handle: a save writes, renames into place, then
truncates, flushes or writes through the same handle.) A file replaced by
the rename is, to its own handles, a deleted file (3.4.8).

#### 3.4.8 DELETE (9)

Request: path request. Reply: status only. Unknown root `ST_NOT_FOUND`.

For a name the writable provider holds (for names that exist only in a
read-only layer beneath it, see "Names only in a read-only lower" after
3.4.9): a file, or a directory with nothing under it, is deleted. A directory
with anything under it is `ST_IO_ERROR`, and nothing is deleted. The root itself
is `ST_BAD_REQUEST` (as the reference director answers) or `ST_IO_ERROR`,
and is never deleted. A missing path is `ST_NOT_FOUND`; a path outside every
writable provider `ST_READ_ONLY`. A file with open handles is deleted like
any other: a director that keeps files on disk should open them with
`FILE_SHARE_DELETE`, so the delete or rename of an open file succeeds.

After its file is deleted, or replaced by a RENAME or RENAME_X onto its
name, a handle's READ, WRITE and SETATTR may fail with `ST_IO_ERROR` or
`ST_NOT_FOUND`, or the director may keep the file for its open handles, as
NTFS keeps a file deleted with `FILE_SHARE_DELETE`. A kept file is still
one file to all of them: it starts with its last bytes, and a WRITE or
SETATTR through one of its handles is seen by a READ through any other
(3.4.4), but by nothing that reaches it through a name, since it has none.
A handle never reads or changes a file created at, or moved to, that name
later, and its CLOSE is `ST_OK`. The reference fails them: READ
`ST_IO_ERROR`, WRITE and SETATTR `ST_NOT_FOUND`.

#### 3.4.9 MKDIR (10)

Request: `root:u32 | mode:u32 | vpath utf8…`. The shim sends `mode = 0o755`
(493); a director ignores it. Reply: status only.

The directory is stored with the caller's spelling (section 3.2). On an
existing path the writable provider holds, directors may differ (for a path
that exists only in a read-only layer beneath it, see below):

- An existing directory: a director may answer `ST_OK` or `ST_EXISTS`, and a
  client must accept either. The reference director answers `ST_EXISTS`. For
  `FILE_CREATE` and `FILE_OPEN_IF` the shim asks GETATTR before MKDIR, and
  sends no MKDIR when a directory is there: `FILE_CREATE` reports the name
  collision and `FILE_OPEN_IF` opens it (`FILE_OPENED`). After any failed
  MKDIR it asks GETATTR again. So either answer gives the program the same
  result.
- An existing file: `ST_IO_ERROR` where a writable provider serves the path,
  `ST_EXISTS` where none does. The reference director answers the same.

An existing directory no writable provider serves, the root included, is
`ST_EXISTS` or `ST_READ_ONLY`; the reference answers `ST_EXISTS`. A MKDIR
whose parent is missing is `ST_NOT_FOUND`; one whose parent is a file is
`ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`. Nothing is created. The reference
answers `ST_NOT_FOUND` for both.

**Names only in a read-only lower.** A director may serve a writable root as
an overlay: a writable upper over a read-only lower. For a name that exists
only in the lower, not in the upper, such a director may answer DELETE,
RENAME and MKDIR as follows, where the rules above (written for names the
writable provider holds) say otherwise:

- DELETE of a lower-only name, a directory with children included, hides it
  (a whiteout in the upper) and answers `ST_OK`.
- RENAME of a lower-only directory to another name is `ST_IO_ERROR` (lower
  directories do not move). A lower-only file is copied up and renamed.
- RENAME of a lower-only name to another case of itself answers `ST_OK`
  without respelling it: the name keeps the lower's spelling.
- MKDIR over a lower-only file creates the directory in the upper and
  answers `ST_OK`; the directory then hides the file.

The reference director gives these answers for a root it serves with read-only
layers beneath its writable upper.

#### 3.4.10 CLOSE (11)

Request, 8 bytes: `fh:u64`. Reply: status only. Unknown `fh` `ST_BAD_FH`;
`CLOSE(0)` is `ST_BAD_FH` (3.4.3: 0 is never a handle).
The shim sends CLOSE only for an `fh` an OPEN returned: the handle it gives a
program for a directory it created with MKDIR (3.4.9) holds none, and closing
it sends nothing. A `ST_BAD_FH` answer to a CLOSE it sends is ignored.
A file over which a program made a section that fills on first touch keeps
its `fh` until the last of the program's file handle, the section and its
views is gone, and the CLOSE goes then, once: the section reads through it.

#### 3.4.11 HEARTBEAT (13)

Request: empty payload, flags 0. Reply: `ST_OK`, empty. The shim sends exactly
one, right after attaching, and fails its load unless it gets `ST_OK`.

#### 3.4.12 STORED_NAMES (14)

Request:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `root` |
| 4 | 4 | u32 | `skip`: components the client already knows |
| 8 | rest | utf8… | `vpath` (folded) |

Reply: the stored spelling of each component of `vpath` from index `skip` on,
joined by `/`, no length prefix, exactly `components - skip` names. A
component the director has no entry for SHOULD come back as it was sent. A
director MAY instead answer `ST_NOT_FOUND` for a path it does not have. The
shim discards a reply with the wrong number of names, or with a name that
does not fold to the component it answers (section 3.7), and on any non-OK
status or discarded reply uses the caller's own spelling. Used for
`GetFinalPathNameByHandle`.

An empty reply payload is one empty name (the shim splits the reply on
every `/`). The shim never asks for zero names: when it already knows every
component it sends no request.

A reply that names the components gives every component the director has in
its stored spelling, even when later components are unknown: an echo of a
known component as sent defeats `GetFinalPathNameByHandle`. A `skip` equal
to or greater than the number of components, which the shim never sends,
may be answered with an empty reply or `ST_BAD_REQUEST`; the reference
answers an empty reply. STORED_NAMES of `"."` names the root itself: the
reply is `.`, one name.

#### 3.4.13 RENAME_X (23)

Request:

| Offset | Size | Type | Field |
|---:|---:|---|---|
| 0 | 4 | u32 | `from_root` |
| 4 | 4 | u32 | `to_root` |
| 8 | 4 | u32 | `from_len` |
| 12 | `from_len` | utf8 | `from` |
| 12 + `from_len` | rest | utf8… | `to` |

A rename between two roots. A file is copied to `to_root` (replacing what is
there) and removed from `from_root`; a directory is refused with
`ST_CROSS_ROOT`, which the shim reports as `STATUS_NOT_SAME_DEVICE`. Equal
roots behave as RENAME. Reply: status only.

A directory at `to`, the target root included, is `ST_IS_DIR` or
`ST_IO_ERROR`; the reference answers `ST_IS_DIR`. Either end outside every
writable provider is `ST_READ_ONLY`. On any refusal both roots are
unchanged. The file at `to` takes the caller's spelling, as RENAME's does,
and the components above it keep theirs (3.2). A `to` whose parent is
missing, or is a file, is answered as RENAME answers it (3.4.7). Handles on
the moved file are as 3.4.7 says.

#### 3.4.14 STAGE_EXEC (24)

Request: path request (the image the program is starting). Reply: the real,
on-disk Windows path to start it from, UTF-8, the whole payload. The director
puts the program on real disk first (a child's image is opened by the kernel
before any hook runs). `ST_NOT_SUPPORTED` when the director stages nothing;
a director with a stager may answer `ST_NOT_FOUND` for a file it does not
stage. With a stager, a directory, a missing path or a file it does not
stage is `ST_NOT_FOUND`. A staged image holds the file's bytes as they are
when STAGE_EXEC is served, writes in the session included.

How the shim uses it: only when `VFS_ROOT_BACKINGS` names at least one
backing (otherwise the child starts as it would without the VFS). Then, for
each candidate image path that routes under a root it first sends GETATTR,
and sends STAGE_EXEC only for the first candidate that is a file (`found`,
not `is_dir`). On success the child is started from the returned path. If no
candidate was staged, every candidate path is absolute, and at least one was
under a root, the child is refused, not started from whatever is really on
disk at that path. Otherwise (a relative name the loader may resolve
elsewhere) the image path is left as it was.

### 3.5 Status values

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
| `i32::MIN` (`-2147483648`) | overflow | Written by the ring layer when a reply exceeds `payload_cap`; payload length 0. Not one of the statuses `contract/protocol.json` lists. |

Values 0 to -11 are fixed by the shim, which matches statuses by number. New
statuses take the next free negative number.

A path with a file as a component is a missing path: GETATTR `found = 0`,
and `ST_NOT_FOUND` from every other opcode (READDIR may answer it as 3.4.2
answers a missing directory). A director may answer
`ST_NOT_A_DIRECTORY` instead from every opcode but GETATTR. The reference
answers `ST_NOT_FOUND`. (The shim maps `ST_NOT_FOUND` to
`OBJECT_NAME_NOT_FOUND` and `ST_NOT_A_DIRECTORY` on an open to
`STATUS_UNSUCCESSFUL`.)

The `ST_*` names are this document's names for the wire values. In
`contract/protocol.json` each has its lowercase, hyphenated name
(`ST_NOT_FOUND` is `"not-found"`). The `OPEN_*` flags of 3.4.3 map the same
way: `OPEN_CREATE` is `"create"` in the JSON's `open_flags`, whose `"mask"` is
the value in the table (`OPEN_APPEND` is 32).

#### 3.5.1 Malformed requests: what a director may answer

The status for a payload too short for its fixed fields, a path that escapes
its root (`..` above it), or a root the director does not serve may differ
from director to director. The shim treats every non-OK status as a failure
of the call (only `ST_NOT_FOUND`, `ST_EXISTS`, `ST_READ_ONLY`,
`ST_CROSS_ROOT` and the like get specific NT statuses), so none of these
differences changes what a well-formed shim request gets. A new director
should answer `ST_BAD_REQUEST` for a short payload or an escaping path, and
`ST_NOT_FOUND` for an unknown root. A director may answer:

| Op | Short or escaping | Unknown root |
|---|---|---|
| GETATTR | `ST_BAD_REQUEST`, or OK with `found = 0` | OK, `found = 0` |
| READDIR | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | `ST_NOT_FOUND` |
| OPEN | `ST_BAD_REQUEST` | `ST_NOT_FOUND` |
| READ, WRITE, SETATTR, CLOSE | short: `ST_BAD_REQUEST` | (no root) |
| DELETE, MKDIR | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | `ST_NOT_FOUND` |
| RENAME | `ST_BAD_REQUEST` | `ST_BAD_REQUEST` or `ST_NOT_FOUND` |
| RENAME_X | `ST_BAD_REQUEST` | `ST_BAD_REQUEST` or `ST_NOT_FOUND`, as RENAME |
| STORED_NAMES | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | `ST_NOT_FOUND` |
| STAGE_EXEC | escaping: `ST_BAD_REQUEST` or `ST_NOT_FOUND` | `ST_NOT_FOUND` |

A director with no stager answers STAGE_EXEC `ST_NOT_SUPPORTED` whatever its
path, and may do so before it looks at the path.

Invalid UTF-8 in a path is `ST_BAD_REQUEST`, or a director may instead decode
it with replacement characters and carry on.

### 3.6 Flags

| Where | Bit | Value | Name |
|---|---:|---:|---|
| slot `flags` of READ | 0 | `0x1` | `FLAG_READ_BULK`: prefer a bulk-arena reply (`"read_bulk"` in `contract/protocol.json`'s `request_flags`) |
| READ reply word 0 | 31 | `0x8000_0000` | `READ_RESP_BULK_BIT`: the reply is bulk |
| OPEN reply byte 17 | 0 | `0x01` | `immutable` |
| OPEN request `oflags` | 0-5 | 1-32 | `OPEN_*` (3.4.3) |

The slot `flags` word is ignored for every opcode but READ, and bits other
than bit 0 are ignored for READ.

### 3.7 What the shim makes of a reply it cannot use

A reply can be one the shim cannot use: a status that says the request
failed, a status no table names, the ring's overflow status, an `ST_OK`
payload that does not decode, a bulk READ reply whose data is not where
section 2.6 puts it, or no reply within the shim's deadline (section 2.5,
"Deadlines"). A reply the shim refuses gives the program none of its data:
the shim answers the call itself, and goes on serving, so the next call
goes to the director as before. The shim accepts two kinds of reply that
are not exactly their form: bytes after a file reply's fields (section
3.2), and a WRITE reply cut short after its first word (3.4.5). The table
shows them as success. A reply that arrives after its
deadline lands in a slot the shim has given up, and answers nothing.

The table gives what the program gets for each kind of bad reply, by the
call it made. A cell names an NTSTATUS without its `STATUS_` prefix, or:

- **success**: the call goes on as if the reply had been good.
- **fallback**: the call succeeds without the director's answer. A
  registry read serves the real key alone (section 4.4); a final name
  keeps the program's own spelling of every component the director did
  not answer (3.4.12).
- **load fails**: the shim does not attach; its ready file says
  `fuse-failed:` (section 6.2), and the injector kills the target.
- **refused**: `CreateProcessW` fails with `ERROR_FILE_NOT_FOUND` and no
  process starts.
- **n/a**: the fault cannot reach the call.

The columns:

- A named status (`ST_NOT_FOUND` to `ST_REPLY_TOO_LARGE`): that status, with an
  empty payload, for a request the director never carried out.
- **any other status**: `ST_NOT_A_DIRECTORY`, `ST_IO_ERROR`, `ST_BAD_FH`,
  `ST_NOT_SUPPORTED`, a negative status no table names, or a positive
  status.
- **overflow**: the ring's overflow status, -2147483648 (section 2.5): the
  director carried out the request and its reply did not fit.
- **truncated**: an `ST_OK` payload one byte short.
- **trailing bytes**: an `ST_OK` payload with bytes after it. A file reply
  ignores them (section 3.2); a registry reply refuses them. The reply of
  STORED_NAMES or STAGE_EXEC is all text, so they are part of it; the
  table's trailing bytes are not UTF-8.
- **garbage**: `ST_OK` with a payload shorter than any fixed reply and not
  UTF-8.
- **bulk reply out of place**: an `ST_OK` bulk READ reply whose data does
  not lie inside the slot's own bank (another slot's bank, past the arena,
  or an `arena_offset + n` that overflows), or whose `n` is more than the
  request's `len`. An inline READ reply with more than `len` bytes is
  refused the same way.
- **no reply in time**: the deadline passes while the director holds the
  reply.
- **the call after a late reply**: the same call made again once the late
  reply has landed.

The calls, each the first of its kind on a handle or name:

- **attach**: the shim's one HEARTBEAT (3.4.11). An `ST_OK` reply's
  payload is not read.
- **open to read** and **open to write**: `NtCreateFile` or `NtOpenFile` of
  a file, for reading, or for writing with `FILE_OPEN` (3.4.3).
- **attributes by name**: `NtQueryFullAttributesFile` and the other calls
  answered by GETATTR (3.4.1).
- **read, inline** and **read, bulk**: `NtReadFile` of a file that may
  change, of 100 bytes (one inline READ) and of 128 KiB (one bulk READ).
  A read the read cache serves crosses the ring only to fill a block; a
  failed fill is read again uncached.
- **section filled at creation**: `NtCreateSection` over a file of up to
  256 MiB, whose READs fill it at once (3.4.4). A page of a larger section,
  filled on first touch, is asked again up to three times and then raises
  `STATUS_IN_PAGE_ERROR` (3.4.4); that ends the program, so it is not in
  the table.
- **write, first chunk** and **write, a later chunk**: `NtWriteFile`. When
  a later chunk fails, `IoStatus.Information` carries what the chunks
  before it wrote (3.4.5). The shim reads only the first word of a WRITE
  reply, so one cut short in its padding still succeeds.
- **set the size**: `FileEndOfFileInformation` (SETATTR, 3.4.6).
- **list a directory**: the first `NtQueryDirectoryFile` on a directory
  handle (READDIR, 3.4.2). A listing that failed is asked for again by the
  next query.
- **rename**: `FileRenameInformation` within one root (RENAME, 3.4.7). A
  rename between two roots (RENAME_X, 3.4.13) answers the same.
- **delete by name**: `NtDeleteFile`. **delete through a handle**:
  `FileDispositionInformation` (DELETE, 3.4.8).
- **create a directory**: `NtCreateFile` with `FILE_CREATE` and
  `FILE_DIRECTORY_FILE` (MKDIR, 3.4.9). After a failed MKDIR the shim asks
  GETATTR; a directory found there is a name collision. So a director that
  made the directory and then answered too late, or too large, gives
  `OBJECT_NAME_COLLISION`.
- **final name**: `GetFinalPathNameByHandleW` (STORED_NAMES, 3.4.12). A
  reply with a name that is not a spelling of the component asked about
  (it folds to another name) is not used either.
- **start a child**: `CreateProcessW` of an image under a root with a
  backing (STAGE_EXEC, 3.4.14).
- **registry read**: `NtQueryValueKey` and the other calls answered by
  REG_LOOKUP and REG_KEY (section 4.4). A REG_KEY answered
  `ST_REPLY_TOO_LARGE`, or with the ring's overflow, says the key's overlay
  holds more than one reply carries: the call fails
  `STATUS_REGISTRY_IO_FAILED` rather than serve the real key alone (4.4).
- **registry write**: `NtSetValueKey` and the other calls answered by
  REG_SET_VALUE to REG_RENAME_KEY (section 4.4; a key create answered
  `ST_EXISTS` is asked about again, as 4.4 says).

A reply that comes too late or too large does not undo what the director
did: any change it carried out (a rename, a delete, a write, a new size, a
registry write) is still done, though the program was told the call
failed. Creating a directory is the exception, as its call above says: the
shim finds the directory and answers `OBJECT_NAME_COLLISION`.

| Operation | `ST_NOT_FOUND` | `ST_EXISTS` | `ST_READ_ONLY` | `ST_IS_DIR` | `ST_NO_SPACE` | `ST_BAD_REQUEST` | `ST_CROSS_ROOT` | `ST_REPLY_TOO_LARGE` | any other status | overflow | truncated | trailing bytes | garbage | bulk reply out of place | no reply in time | the call after a late reply |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| attach (HEARTBEAT) | load fails | load fails | load fails | load fails | load fails | load fails | load fails | load fails | load fails | load fails | success | success | success | n/a | load fails | n/a |
| open to read (OPEN) | OBJECT_NAME_NOT_FOUND | OBJECT_NAME_COLLISION | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| open to write (OPEN) | OBJECT_NAME_NOT_FOUND | OBJECT_NAME_COLLISION | ACCESS_DENIED | FILE_IS_A_DIRECTORY | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| attributes by name (GETATTR) | OBJECT_NAME_NOT_FOUND | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| read, inline (READ) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success |
| read, bulk (READ) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success |
| section filled at creation (READ) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success |
| write, first chunk (WRITE) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | DISK_FULL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| write, a later chunk (WRITE) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | DISK_FULL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| set the size (SETATTR) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | success | n/a | UNSUCCESSFUL | success |
| list a directory (READDIR) | NO_MORE_FILES | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |
| rename (RENAME) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | NOT_SAME_DEVICE | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | success | n/a | UNSUCCESSFUL | success |
| delete by name (DELETE) | OBJECT_NAME_NOT_FOUND | UNSUCCESSFUL | ACCESS_DENIED | FILE_IS_A_DIRECTORY | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | success | n/a | UNSUCCESSFUL | success |
| delete through a handle (DELETE) | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | success | success | success | n/a | UNSUCCESSFUL | success |
| create a directory (MKDIR) | OBJECT_NAME_NOT_FOUND | UNSUCCESSFUL | ACCESS_DENIED | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | OBJECT_NAME_COLLISION | success | success | success | n/a | OBJECT_NAME_COLLISION | success |
| final name (STORED_NAMES) | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | fallback | n/a | fallback | success |
| start a child (STAGE_EXEC) | refused | refused | refused | refused | refused | refused | refused | refused | refused | refused | refused | refused | refused | n/a | refused | success |
| registry read (REG_KEY) | fallback | fallback | fallback | fallback | fallback | fallback | fallback | REGISTRY_IO_FAILED | fallback | REGISTRY_IO_FAILED | fallback | fallback | fallback | n/a | fallback | success |
| registry write (REG_SET_VALUE) | KEY_DELETED | CANNOT_DELETE | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | INVALID_PARAMETER | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | UNSUCCESSFUL | n/a | UNSUCCESSFUL | success |

---

## 4. Registry encodings

Opcodes 15-22 virtualise the registry through an overlay the director keeps.
The shim sends them only when `VFS_REGISTRY` is affirmative (section 5.3) and
every registry detour installed. A director without a registry layer answers
all eight `ST_NOT_SUPPORTED`.

### 4.1 Field encodings and limits

- `str` = `len:u32 | utf8[len]`. Limits count UTF-16 code units (the length
  of the string once converted to UTF-16), the registry's own unit.
- Booleans are one byte, exactly 0 or 1.
- Every registry decoder consumes its input exactly; trailing bytes are
  `ST_BAD_REQUEST`.

| Limit | Value | Applies to |
|---|---:|---|
| Key name | 255 UTF-16 units | a key name component; `new_leaf`; child names in a REG_KEY reply |
| Value name | 16 383 UTF-16 units | value names |
| Value data | 1 048 576 bytes (`1 << 20`) | value data |
| Write depth | 512 components below `\Registry` | keys a write may create or move |

The write depth limits writes only: a read (REG_LOOKUP, REG_KEY,
REG_CHANGED) deeper than it answers as for any key the overlay does not
hold.

The shim never sends a registry request larger than `payload_cap`: it refuses
it locally with `ST_BAD_REQUEST`. For a write that becomes
`STATUS_INVALID_PARAMETER` (section 4.4); for REG_LOOKUP or REG_KEY it is a
failed read, and the hook falls back to the real key.

### 4.2 Key paths

A key path is the canonical NT form: `\Registry` then non-empty components,
single `\` separators, no trailing `\`, no `.` or `..` components. `Registry`,
`Machine` and `User` take that spelling; components below keep the caller's.
Under `\Registry\User`, the current user's SID becomes the literal
`<CurrentUser>` and `<SID>_Classes` becomes `<CurrentUser>_Classes`, so a layer
written under one account reads back under another. Keys compare
case-insensitively by `fold`, and that includes the `Registry`, `Machine` and
`User` components: a director accepts `\REGISTRY\MACHINE\...` as readily as
`\Registry\Machine\...`. A path is valid when its fold equals the fold of its
canonical form. The shim sends the canonical spelling. A director must answer
`ST_BAD_REQUEST` for a path that is not canonical (the codec checks only
length and UTF-8).

Value names arrive as UTF-8 converted from the caller's UTF-16; value `ty` is
the Windows `REG_*` type number; value data is the raw bytes the caller passed
to `NtSetValueKey` (for `REG_SZ`, UTF-16LE with whatever terminator the caller
included).

### 4.3 Requests and replies

Every reply starts with `version:u64`, the overlay's version after (or at) the
operation.
The number a director's overlay starts from when it mounts a layer is the
director's (part of the persisted layer), so two directors serving one layer
may number its versions apart by a constant, and a REG_CHANGED `since` is in
the numbering of the director it is sent to; how the version moves from one
operation to the next is the overlay's (below).

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

REG_LOOKUP `state`:

| Value | Meaning |
|---:|---|
| 0 | absent: the overlay says nothing; the real key decides |
| 1 | present, overlaying a real key |
| 2 | present, created in the overlay (no real counterpart shows through) |
| 3 | tombstoned: deleted in the overlay |

A decoder rejects `state > 3`. `below` is 1 when the overlay has anything
strictly under the key.

REG_KEY `node`, present when `has_node == 1`:

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

With `has_node == 0` the reply is exactly 9 bytes. The shim reads the
children into a map keyed by `folded`, so their order is not checked and a
duplicate `folded` key keeps the last one; send them sorted and unique.

A REG_KEY reply longer than `payload_cap - 8` must be answered
`ST_REPLY_TOO_LARGE` instead; the shim then fails every query of the key
(section 4.4). A director may apply the same rule to every registry
reply; the other replies are at most 10 bytes, so only REG_KEY's can ever
reach it.

Errors: name too
long, data too large, invalid path, too deep: `ST_BAD_REQUEST`; not found:
`ST_NOT_FOUND`; already exists: `ST_EXISTS`. REG_RENAME_KEY with a `new_leaf`
of `.` or `..` is `ST_BAD_REQUEST`.

**The overlay.** The overlay is a tree of nodes keyed by folded canonical
path, each remembering its own spelling, its values (in insertion order,
matched by folded name), the folded names of values deleted at it (value
tombstones), its children (folded name to stored spelling and present or
tombstone), `created`, `volatile` and `last_write`. A deleted key leaves a
tombstone child in its parent and no node of its own, and everything below it
is dropped. Its semantics:

- **Version.** The overlay keeps one counter. Every successful write
  (REG_SET_VALUE, REG_DELETE_VALUE, REG_CREATE_KEY, REG_DELETE_KEY,
  REG_RENAME_KEY) adds 1 to it before it changes anything and answers the new
  value; a refused write leaves it alone; a read answers the current value.
- **Making a path exist.** A write to a key first makes every key on the way
  to it a node, keeping the spelling of nodes that already exist. A new node
  is `created` when its parent is `created`, or when it or its name in the
  parent was tombstoned (the chain is revived); otherwise it is a plain
  presence over the real key. A new node is listed as a present child of its
  parent.
- **REG_LOOKUP.** A node is present (`created` decides state 1 or 2); a key
  with no node that is tombstoned itself, or lies below a tombstoned
  ancestor, is state 3; anything else is absent, at any depth (4.1). `below`
  is whether a node or a tombstone lies strictly under the path; it is 0 for
  a tombstoned key.
- **REG_KEY.** The node at the path, or none (`has_node = 0`).
- **Limits.** Every write but REG_RENAME_KEY checks, in this order, before
  anything else: the path is a valid key path (4.2); every key-name component
  of the path is at most 255 UTF-16 units; the path is at most 512
  components below `\Registry`; then the write's own limits below. Any of them failing is
  `ST_BAD_REQUEST` and changes nothing. No write limits how many keys,
  values or children a node holds.
- **`last_write`.** A write stamps the director's clock (a FILETIME) on: every
  node that making a path exist creates; a node whose child list it changes;
  the key of REG_SET_VALUE and REG_DELETE_VALUE; the parent of REG_DELETE_KEY;
  and the moved key and its parent of REG_RENAME_KEY. Nothing else changes
  it.
- **REG_SET_VALUE.** Also checks the value name (at most 16 383 units) and the
  data (at most 1 048 576 bytes). Makes the path exist, drops a value
  tombstone of that name, and replaces the value of that folded name
  (keeping its stored name) or appends it.
- **REG_DELETE_VALUE.** Also checks the value name. `ST_NOT_FOUND` if the key
  is tombstoned (itself or under a tombstoned ancestor). Otherwise makes the
  path exist, removes the value of that name and records a value tombstone,
  which also hides a real value of that name.
- **REG_CREATE_KEY.** `ST_EXISTS` if the overlay already holds a node there.
  Otherwise makes the path exist, the key itself `created` (it then needs no
  real counterpart, and none shows through), with the request's `volatile`.
- **REG_DELETE_KEY.** `ST_BAD_REQUEST` for `\Registry` itself;
  `ST_NOT_FOUND` if the key is tombstoned (itself or under a tombstoned
  ancestor). Otherwise makes the parent exist, drops the key's node and
  everything below it, tombstones the key and lists it as a tombstone child
  of its parent.
- **REG_RENAME_KEY.** Checks, in this order: the path is valid; `new_leaf`
  is non-empty, holds no `\` and is at most 255 units; every key-name
  component of the path is at most 255 units; the path is not `\Registry`
  (each `ST_BAD_REQUEST`). The key must have a node in the overlay (`ST_NOT_FOUND` otherwise). Another
  node at the new name is `ST_EXISTS`, unless the new name differs only in
  case. A `new_leaf` that folds to the key's own leaf, in the same spelling
  or another, renames it in place. Every key it moves is created at its own
  depth under the new name, so
  a moved key deeper than 512 components is `ST_BAD_REQUEST`. The key and its
  subtree move to the new name under the same parent, every moved key
  `created` (the new name has no real counterpart), tombstones below it
  moving with it; the old name is tombstoned in the parent (not for a change
  of case only), and the new name is listed as present.
- **REG_CHANGED.** For a key with a node: whether its own values, existence
  or child list changed at a version after `since` (with `subtree` 1, or
  anything below it did). For a key with no node: whether it, or any
  ancestor, was deleted or renamed away at a version after `since`. A
  director may forget old deletions to bound its memory, as long as a
  forgotten one then reads as changed for any older `since`: a spurious
  change, never a missed one.

The persisted layer format is the director's own business and is not on the
wire.

### 4.4 Shim handling of registry replies

Section 3.7 gives what every kind of bad reply does, registry calls among
them.

A read (REG_LOOKUP, REG_KEY) that fails is a fallback: the hook serves the
real key alone, as if the overlay held nothing for it.

The exception is a key too large to read. A REG_KEY answered
`ST_REPLY_TOO_LARGE`, or with the ring's overflow status (section 2.5), says
the key's overlay holds more than one reply carries: values, subkeys or
tombstones the real key does not have, so the real key alone would answer
with wrong values. Every query of that key (a value, several values, the
key's information, an enumeration of its values or subkeys, a subkey's entry
in its parent's enumeration) fails `STATUS_REGISTRY_IO_FAILED`, the status NT
gives when the registry cannot read what backs a key. A program takes it as
a failed read. `STATUS_BUFFER_OVERFLOW` and `STATUS_BUFFER_TOO_SMALL` would
tell it to ask again with a larger buffer, which cannot help, and
`STATUS_INSUFFICIENT_RESOURCES` that the system is short of memory. A write
to the key that needs its node fails `STATUS_UNSUCCESSFUL`.

A write that fails is not retried against the real
registry. Its status becomes the NT status the caller gets:

| Director status | NT status |
|---|---|
| `ST_BAD_REQUEST` | `STATUS_INVALID_PARAMETER` |
| `ST_NOT_FOUND` (REG_DELETE_VALUE or REG_DELETE_KEY of a tombstoned key, REG_RENAME_KEY of a key with no node) | `STATUS_KEY_DELETED` |
| `ST_EXISTS` (rename, create) | `STATUS_CANNOT_DELETE` |
| anything else | `STATUS_UNSUCCESSFUL` |

The exception is a key create: a REG_CREATE_KEY answered `ST_EXISTS` (another
thread or process created the key first) is followed by a fresh REG_LOOKUP,
and if that says present the key is opened; otherwise the create fails
`STATUS_UNSUCCESSFUL`. There `ST_BAD_REQUEST` is `STATUS_INVALID_PARAMETER`
and anything else `STATUS_UNSUCCESSFUL`.

A write reply that is not exactly 8 bytes counts as a failed write
(`ST_IO_ERROR`).

The shim treats a key with no node below a `created` key as nonexistent, so
an absent REG_LOOKUP there hides the real key. For any other absent key it
checks the key's ancestors itself.

### 4.5 Registry generation (`reg_gen`)

Distinct from the overlay `version` in replies. The director keeps one
generation per session and publishes it into every ring header it serves
(offset 40, atomic fetch-max, never backward). It moves on **every successful
registry write, before the write's reply is published**, and whenever a
registry layer is attached or detached.

The shim caches REG_LOOKUP and REG_KEY answers per folded path under the
generation it read before asking, and uses a cached answer only while the
header still holds that generation. Generation 0 means "nothing published":
the shim then caches nothing and asks the director every time. So a director
that serves the registry must publish a non-zero generation before the shim
attaches, and must bump it before replying to any write, or other processes
will keep serving stale answers.

A director that serves no registry may leave `reg_gen = 0` or publish a
generation anyway. Neither matters to the shim without `VFS_REGISTRY`, which
then sends no registry request at all.

---

## 5. Bootstrap config and handshake environment

### 5.1 `shim.cfg` (VFSC, version 2)

The director writes it; its path reaches the shim as the injector's
`<config>` argument, which the injector puts in `VFS_SHIM_CONFIG`.

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

The shim refuses a config that is shorter than 4 bytes, truncated, over-long
or not UTF-8 where text is due, whose first 4 bytes are not `VFSC`, or whose
version is not 2. Any of these fails the shim's load with ready-file content
`bootstrap-failed:<message>`.

What the shim does with it: it decodes it (as a version and shape check) and
uses neither field. The client's roots come from the environment. Write
`root` as root 0 and the static imports the launch needs; a director may
write it with no static imports.

### 5.2 The handshake tables

The handshake names fall into three tables, shipped as `contract/env.json`
(its keys `transport`, `inject` and `config`):

| Table | Names | Rule |
|---|---|---|
| transport | `VFS_RING_PATH`, `VFS_RING_SECTION`, `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_OFFSET`, `VFS_ARENA_LEN`, `VFS_SERVER_EV`, `VFS_CLIENT_EV`, `VFS_FUSE_CFG`, `VFS_VIRTUAL_DIR`, `VFS_VIRTUAL_ROOTS`, `VFS_ROOT_BACKINGS`, `VFS_REGISTRY` | A launch sets **or removes** every one of these, so nothing inherited from an earlier session reaches the child. |
| inject | `VFS_INJECT_CWD`, `VFS_INJECT_STEAM_HELPER` | Read only by the injector. A Wine launch sets or removes them like the transport names. |
| config | `VFS_STATE_DIR`, `VFS_HOME`, `VFS_LAUNCH_IMAGE`, `VFS_DISCOVERY_PATH`, `VFS_SHIM_CONFIG`, `VFS_SHIM_READY`, `VFS_CHILD_REFUSED_LOG`, `VFS_FORCE_INJECTION` | Other handshake configuration; not cleared by a launch. |

Names compare without ASCII case on the Windows side (Wine included), so a
removal must remove every spelling.

### 5.3 Each name's value

Booleans: an **opt-in** switch is on only for `1`, `true`, `yes`, `on` (ASCII
case-insensitive); an **opt-out** switch is off only for `0`, `false`, `no`,
`off`.

| Name | Set by | Read by | Value |
|---|---|---|---|
| `VFS_RING_SECTION` | director | shim | Section name (2.9). Used when `VFS_RING_PATH` is unset or empty. |
| `VFS_RING_PATH` | director | shim | Ring file path as the shim sees it (2.10). Wins over the section. |
| `VFS_RING_BYTES` | director | shim | Decimal bytes to map. Must cover the ring and the arena. Shim default if unparsable: 2 097 152. |
| `VFS_RING_PAYLOAD_CAP` | director | shim | Decimal. Must equal the header's `payload_cap`: the shim sizes WRITE chunks and inline reads from this value, and a request over the header's cap is refused locally. Shim default: 1 048 576. |
| `VFS_ARENA_OFFSET` | director | shim | Decimal mapping offset of the arena. Authoritative when set: the shim locates each slot's bank from it (2.6) and fails a bulk reply whose data lies outside that bank, so a wrong value fails every bulk READ. Unset or unparsable: the arena is taken to start directly after the last slot (2.1). |
| `VFS_ARENA_LEN` | director | shim | Decimal arena length; 0 or unset disables bulk reads (2.6). |
| `VFS_SERVER_EV` | director | shim | Server event name (section mode only). |
| `VFS_CLIENT_EV` | director | nobody | Client event name. The shim does not open it. |
| `VFS_FUSE_CFG` | director | nobody | Path of a `key=value` text file describing the ring, for tools (5.4). |
| `VFS_VIRTUAL_DIR` | director (or the injector, from `VFS_INJECT_CWD`) | shim | Root 0's path as the program sees it. **Required** whenever a ring is named: unset fails the load. |
| `VFS_VIRTUAL_ROOTS` | director | shim | Roots beyond 0: `id=path` entries joined by `;`, e.g. `1=C:\Users\me\Documents\My Games\Skyrim`. Whitespace around id and path is trimmed; an entry that does not parse, or has an empty path, is skipped; an entry for id 0 replaces `VFS_VIRTUAL_DIR`. Unset and `""` mean the same: root 0 only. |
| `VFS_ROOT_BACKINGS` | director | shim | Each root's real backing directory where it differs from its location, same `id=path;…` syntax. The shim treats a backing directory as a second spelling of its root for routing, never for naming. |
| `VFS_REGISTRY` | director | shim | Opt-in. Set to `1` only while a registry layer is attached; otherwise remove it. |
| `VFS_INJECT_CWD` | director | injector | The target's working directory as the target sees it. Unset or empty: the injector's own. |
| `VFS_INJECT_STEAM_HELPER` | director | injector | Under Wine: a Steam helper command line to run before the target, or `off` to only clear a stale helper pid. On native Windows nothing is run, but the injector still writes `<ready>.steam-helper` with `disabled:not running under Wine` (section 6.3). |
| `VFS_STATE_DIR` | director | shim | Session state directory (the shim's panic log goes to `<dir>\shim-panic.log`). |
| `VFS_HOME` | director | director side | Aether's own data directory. |
| `VFS_LAUNCH_IMAGE` | director | shim | Absolute path of the staged EXE; its parent directory is another spelling of root 0. |
| `VFS_DISCOVERY_PATH` | — | — | Registered; neither the shim nor the injector reads it. |
| `VFS_SHIM_CONFIG` | injector | shim | Path of `shim.cfg`. Unset: the shim fails its load. |
| `VFS_SHIM_READY` | injector | shim | Path of the ready file (section 6.2). The shim removes it from its own environment after writing, so children do not inherit it. |
| `VFS_CHILD_REFUSED_LOG` | injector | shim | `<ready>.child-refused`: the shim appends `<image> <reason>\n` for each child it had to kill. |
| `VFS_FORCE_INJECTION` | director | — | Accepted and ignored: remote-thread `LoadLibrary` is the only activation. |

Also read, outside the tables: `VFS_READY_TIMEOUT_SECS` (decimal seconds,
default 180, minimum 1) bounds the injector's wait for the shim and the shim's
wait for each child; the injector sets it in the target's environment.
`VFS_RING_SPIN_US` (director side, 2.11). Other `VFS_*` names the shim and
injector read are behaviour, diagnostic and test switches and are not part of
the handshake.

### 5.4 `fuse.cfg` (informational)

Written by the director and named by `VFS_FUSE_CFG`; nothing in the shim or
the injector reads it. Lines of `key=value`, `\n`-terminated:

```
section=<name>          (file mode: ring_path=<path>)
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
argument, with no escaping. An argument that contains `"` or ends in `\` is
mangled by the target's command-line parser (a trailing `\` escapes the
closing quote). A director must not pass such arguments, or must accept that
they arrive changed.

Under Wine, every injector argument (target, shim DLL, config, ready file)
and every path in a `VFS_*` variable is in the Windows form the target
sees (`C:\...` or `Z:\...`), not the host path. The director reads the ready
file and its siblings by the corresponding host path.

**The shim's own DLL dependencies must resolve in the target.** The shim is
loaded by full path, but the loader finds its imports through the target's
DLL search path. The release natives are built with the C runtime linked in
statically, so they need no C runtime DLL; a build without it does, and a
director that launches one must put the folder that holds the natives first
on the target's `PATH`.

The injector adds to the target's environment: `VFS_SHIM_CONFIG`,
`VFS_SHIM_READY`, `VFS_CHILD_REFUSED_LOG = <ready>.child-refused`,
`VFS_READY_TIMEOUT_SECS`, and `VFS_VIRTUAL_DIR = VFS_INJECT_CWD` when a
working directory is given and `VFS_VIRTUAL_DIR` is not already set.
Everything else the target sees is inherited from the injector, so the
director sets the transport names in the injector's environment. Before
starting it deletes `<ready>`, `<ready>.child-refused`,
`<ready>.injector-error` and `<ready>.steam-helper`.

### 6.2 The ready file

Written by the shim to `VFS_SHIM_READY` before its `DllMain` returns, and
read by the injector. Contents are the whole file, no newline:

| Content | Meaning | Injector |
|---|---|---|
| `ready` | Hooks live, director attached. | Releases the target. |
| `fuse-failed:<reason>` | A ring was named (or none was) and the client could not attach, or the heartbeat failed. | Kills the target. |
| `bootstrap-failed:<reason>` | Config unreadable or from another build, or a hook would not install. | Kills the target. |
| anything else, or missing | Not decided yet. | Keeps waiting until the timeout. With remote-thread injection the verdict is read once the shim's `LoadLibrary` has returned; if there is still none, the injector kills the target and reports `bootstrap-failed:the shim loaded but never wrote its ready file` (or `target-exited:` if the target is already gone). |

The shim also writes a one-line diagnostic to `<ready>.boot.log` on failure.

### 6.3 The error report and exit codes

On any injection failure the injector writes one line to
`<ready>.injector-error` and exits 3:

| Line | Cause |
|---|---|
| `target-exited:<code as 0x… hex>` | The target exited before reporting ready (e.g. `0xc0000135`). |
| `ready-timeout:<seconds>` | No verdict within `VFS_READY_TIMEOUT_SECS`. |
| `fuse-failed:<reason>` | The shim reported `fuse-failed:`. |
| `bootstrap-failed:<reason>` | The shim reported `bootstrap-failed:`. |
| `inject:<error>` | Any other failure, described in free text (for example `CreateProcessFailed(…)`). |

| Exit code | Meaning |
|---:|---|
| 2 | Usage error. |
| 3 | Injection failed; see `<ready>.injector-error`. |
| other | The first target process's exit code, after **every** process in its job has exited. |

A target exit code of 2 or 3 is indistinguishable from the injector's own by
the code alone. What distinguishes them is `<ready>.injector-error`: the
injector deletes it at start and writes it only for its own exit 3 (exit 2,
a usage error, writes none). A director should treat 2 or 3 as the injector's
own failure only when that file exists.

`<ready>.steam-helper`, written before the target starts when
`VFS_INJECT_STEAM_HELPER` is set: `started:<pid>:<milliseconds>`, `cleared`,
`disabled:<why>` or `failed:<why>`.

### 6.4 Child processes: the ready and failed events

When the target starts a child, the shim in the parent injects the shim into
it and waits. The child's shim signals with two named events. Both sides
create them with `CreateEventW(NULL, TRUE /*manual reset*/, FALSE, name)`:
the parent creates both before it waits, the child creates the one it sets.
Either side may be first; `CreateEventW` opens the existing event for the
second.

| Event | Set when |
|---|---|
| `Local\vfs_shim_ready_<pid>` | The child's hooks are installed. |
| `Local\vfs_shim_failed_<pid>` | The child's bootstrap failed. |

`<pid>` is the child's process id in decimal. The parent waits up to
`VFS_READY_TIMEOUT_SECS` and kills the child on failure, timeout, or exit.
These events are internal to the shim; a director never needs them, but a
tool that starts processes under the shim itself may wait on them.

---

## 7. Versions

**Aether's release version** is semver; releases are tags `v<semver>`. It is
separate from the **wire version**, which is the ring header's `version`
field, now 4, also published as `wire_version` in `contract/artifacts.json`
and `contract/protocol.json` (Aether's tests check both against the code). `shim.cfg` has its own version, 2.

Rules:

- **A wire-version bump is always an Aether major version.** The shim refuses
  a ring of any other version at attach, so a director and a shim of
  different wire versions cannot run together. The wire version is bumped
  whenever a payload layout or the slot state machine changes.
- **A minor version may add an opcode or a flag only where an old director's
  answer to it is safe.** A director answers an unknown opcode
  `ST_BAD_REQUEST` with an empty payload, ignores unknown slot flag bits and
  READ flag bits other than bit 0, and writes zero in the OPEN reply's flag
  bits 1-7 and padding. So a new opcode is minor only if the shim treats
  `ST_BAD_REQUEST` from it as "not available" and falls back; a new OPEN reply
  flag is minor only if zero means the behaviour without it (as for
  `immutable` and `mount_gen`: a director that writes zeros there is read as
  mutable, generation 0). Opcode numbers are never reused (4 and 12 stay
  reserved).
- **A patch changes no byte on the wire**: no opcode, status, flag, layout,
  environment name or value format, file name, event name, or magic.

---

## 8. Conformance

**For the bytes** (layouts, codecs, the handshake), the golden vectors below
are normative: where this document disagrees with them, this document is
wrong.

**For what a director must answer**, this document is normative.
`contract/scenarios/` and `aether-conform` (`docs/conformance.md`) check it,
and the reference director follows it; where they disagree with this text,
they are fixed. Where this document says directors may differ, conformance
accepts each answer it names.

`contract/protocol.json` is the machine-readable form of the protocol table
(3.3), and is normative for the layouts as the golden vectors are for the
bytes: a director in another language builds its opcode, status and field
tables from it instead of from the prose.

The executable part of the contract is the golden vectors, `vectors.txt` in
each release's `contract.zip`: one `name hex` line per vector, each the bytes
of the message it names. A changed existing line is a wire break. A director
in another language should encode the same inputs and compare bytes, and
decode the vectors and compare fields.

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

Inputs: entries `{name "a.esp", is_dir false, size 10, mtime 1}` and `{name
"sub", is_dir true, size 0, mtime 0}`. The line is 54 bytes:

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
