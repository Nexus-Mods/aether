# Director conformance

`aether-conform` checks a director against the wire contract
(`docs/wire.md`). It starts the director under test behind a small adapter
program, has it mount a scenario's tree, attaches to the ring the director
names exactly as the shim would, plays the scenario's requests, and compares
each answer with the one the scenario expects. Scenarios live in
`contract/scenarios/` and their format is described in
`contract/scenarios/README.md`.

A run has up to three parts, all through the same adapter:

- **Scenarios**, the files in `--scenarios` (always).
- **Malformed requests** (`--malformed`): payloads that are not valid
  requests, sent on the raw ring, and the answers wire.md gives them
  ("Malformed requests" below).
- **Generated cases** (`--generate`): trees and operation sequences made
  from a seed, each step compared with the reference director's answer, and
  the first failure shrunk and written as a scenario file ("Generated cases"
  below).

## What it checks

For every scenario, and for each generated case and the malformed part:

- **The handshake.** The mount's environment must name a ring the shim
  could attach to (wire.md 2.4, 2.9, 2.10, 5.3): a ring file or, on
  Windows, a named section; a header with the right magic, version and
  stride whose slots fit in `VFS_RING_BYTES`; a ring file at least
  `VFS_RING_BYTES` long; `VFS_VIRTUAL_DIR` set, and the roots the shim
  builds from it, `VFS_VIRTUAL_ROOTS`, `VFS_LAUNCH_IMAGE` and
  `VFS_ROOT_BACKINGS` usable (the shim refuses, for example, an empty
  `VFS_VIRTUAL_DIR`: "managed root is not a usable path"). Three further
  rules a director must meet are checked too: the header's `payload_cap`
  equals `VFS_RING_PAYLOAD_CAP` (5.3: "Must equal the header's
  `payload_cap`"); an arena, if `VFS_ARENA_LEN` gives one, has room for a
  bank of the shim's chunk per slot (2.6: "`arena_len >= 256 KiB *
  slot_count`"); and when `VFS_ARENA_OFFSET` is given the arena lies inside
  the mapping (2.6: "Every arena byte must lie inside the first
  `VFS_RING_BYTES` bytes of the mapping").
- **A HEARTBEAT on attach**, as the shim sends one once it has connected.
- **Every step's answer**: the status, and on `ok` every reply field, equal
  to the step's expectation except where wire.md lets directors differ
  (below). A bulk READ reply is read from the arena and compared by its
  data, and must lie inside the mapping (2.6).

## What it does not check

Conformance checks only what the contract guarantees.

**Input the contract leaves to the director.** A scenario sends only
canonical paths: `/` between components, none empty, `.` or `..`, no `\`,
and `"."` for the root, and paths that leave the root (first component `..`),
which wire.md 3.2 and 3.5.1 say how to answer. wire.md 3.2 says of any other
syntax that "Directors have differed on anything else, so a client must not
rely on it", so the suite never sends it and a director may answer it however
it likes.

**Fields the contract leaves to the director.** The rule is equality; these
are the only exceptions, and each is allowed by the wire.md text quoted
(`crates/aether-conform/src/compare.rs` applies them):

| What | Rule | wire.md |
|---|---|---|
| OPEN `fh` | Paired one to one with the scenario's number while open; requests carry the director's number. After a CLOSE the director's old number is sent, so a use after close tests that handle; a number never paired gets a stand-in no open handle has. | 3.4.3: "`fh` is any u64 the director chooses, unique among open handles." |
| OPEN `mount_gen` | Not compared. | 3.4.3: "a generation the director bumps whenever a root's content is replaced", and the directors start it differently. |
| OPEN `immutable` | Not compared, but must be clear on a write open and on a directory. | 3.4.3: "Never set it for a write open or a directory". |
| READDIR order | Entries compared as a multiset. | 3.4.2: "A director may send the entries in any order." |
| `mtime` (GETATTR, READDIR) | Not compared. | 3.2: "Which time a director reports for a path, if any, is the director's to choose". |
| READ inline or bulk | Compared by data. | 3.4.4: the client "must accept either whatever flag it sent". |
| STAGE_EXEC `path` | Not compared. | 3.4.14: the reply is the "on-disk Windows path to start it from", wherever the director staged it. |
| Registry `version` | Compared up to one constant offset per scenario, fixed by the first registry reply; a REG_CHANGED `since` is sent shifted by it. | 4.3: "two directors serving one layer may number its versions apart by a constant, and a REG_CHANGED `since` is in the numbering of the director it is sent to". |
| REG_KEY `last_write` | Not compared. | 4.3: a "FILETIME" the overlay stamps from the director's clock. |

Where wire.md allows more than one answer to a request, the scenario lists
them (`one_of`) and names the section that allows it.

**What a run does not exercise.** Several processes on one ring, concurrent
requests, the director's thread count against `worker_hint` (2.8), how
quickly a director wakes (2.11), `shim.cfg` and the injector (5.1, 6), and
the rule that two immutable opens with equal size and `mount_gen` have
identical content (3.4.3). The handshake variables a run does not use
(`VFS_VIRTUAL_ROOTS`, `VFS_REGISTRY` and the rest) are not checked against
the tree.

## The scenarios

**What a tree means.** Files and directories under a `writable` entry are
held by the writable provider itself, so wire.md 3.4.9's "names only in a
read-only lower" allowances never apply to them, and an adapter must mount
them that way (a director that serves a writable root as an overlay puts the
tree's writable part in the upper). A file or directory outside every
`writable` entry belongs to a read-only provider, and a root with no
`writable` entry has none. Scenarios about "the root itself" (3.4.7, 3.4.8)
make the root writable as a whole.

`contract/scenarios/` holds the suite: what every director must do, as data,
one scenario per file, named by its file. Each `description` names the
wire.md section it checks. Where the contract lets directors answer
differently, the step lists the answers it allows (`one_of`) and cites the
section. The reference passes all of it over a file ring and, on Windows,
over a section (`cargo test -p aether-conform` and CI run both).

| Files | Covers |
|---|---|
| `heartbeat` | HEARTBEAT (3.4.11) |
| `getattr-*` | GETATTR: found and size, a missing path or root is found=0 (3.4.1, 3.5.1) |
| `escape-paths` | a path above the root, for every path opcode (3.2, 3.5, 3.5.1) |
| `open-read`, `open-flags-zero`, `open-errors`, `open-write-existing-readonly-path` | OPEN: handles, `oflags == 0`, the statuses of 3.4.3 and 3.5 |
| `read-inline`, `read-eof`, `read-bulk`, `read-bulk-threshold` | READ: offsets, short reads, end of file, inline and bulk (3.4.4, 3.6) |
| `handles-*` | handles unique while open, unknown and closed handles (3.4.3, 3.4.10, 3.5) |
| `write-*` | write opens (create, excl, trunc, append), WRITE, SETATTR, spelling kept (3.4.3 to 3.4.6, 3.2) |
| `rename-*` | RENAME: replace, respell, directories, refusals, unknown root (3.4.7, 3.5.1) |
| `rename-x-*` | RENAME_X: files across roots, directories `ST_CROSS_ROOT`, equal roots, errors (3.4.13) |
| `mkdir-*`, `delete-*` | MKDIR and DELETE: spelling, existing names, non-empty directories, read-only, errors (3.4.8, 3.4.9) |
| `readdir-*` | READDIR: listing, case, changes, unknown root (3.4.2) |
| `stored-names-*` | STORED_NAMES: stored spelling, `skip`, unknown names, after changes (3.4.12) |
| `case-fold-*` | the fold: ASCII, Unicode (`Σ`, `İ`, composed and decomposed), fold-equal names are one entry (3.2) |
| `roots-*` | several roots kept apart, unserved roots (3.2, 3.5.1) |
| `stage-exec` | STAGE_EXEC: `ST_NOT_SUPPORTED`, `ST_NOT_FOUND` or a path (3.4.14) |
| `registry-none` | no registry layer: all eight opcodes `ST_NOT_SUPPORTED` (4) |
| `registry-lookup`, `-case`, `-bad-path` | REG_LOOKUP states, case-insensitive keys, non-canonical paths (4.2, 4.3) |
| `registry-values`, `-delete-value` | REG_SET_VALUE, REG_KEY values, value tombstones (4.3) |
| `registry-create-key`, `-delete-key`, `-rename-key` | key create, delete, rename, children (4.3) |
| `registry-changed` | REG_CHANGED, with `since` in the director's numbering (4.3) |
| `registry-limits`, `-key-too-large` | name, depth and reply limits; `ST_REPLY_TOO_LARGE` (4.1, 4.3) |

**Left out because wire.md does not fix the answer**, so no scenario
asserts it: READDIR of a path that is not a directory; a path below a file
(accepted as found=0, `ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`); a write open,
DELETE or RENAME of a missing path outside every writable provider; a create
whose parent directory is missing; what a write past the end puts in the gap;
WRITE or SETATTR on a handle opened read-only; `ST_NO_SPACE`.

**Left out because a scenario cannot send or receive it.** A scenario names
opcodes and fields of the table and the statuses in `protocol.json`, so these
rules are not expressible in the scenario format. The malformed part of a run
(`--malformed`, below) checks each of them:

- 3.4.4: a director must accept a 20-byte READ (no trailing pad).
- 3.4.5: a WRITE whose payload is shorter than 24 + `len` is `ST_BAD_REQUEST`.
- 3.5.1: short payloads for every opcode, invalid UTF-8 in a path.
- 3.3: opcodes outside the table (0, 4, 12, 25 and up) are `ST_BAD_REQUEST`.
- 3.6: the slot `flags` word is ignored for every opcode but READ, and bits
  above bit 0 for READ (the format sends `read_bulk` only on READ).
- 4.1, 4.3: registry booleans are exactly 0 or 1, and trailing bytes in a
  registry request are `ST_BAD_REQUEST`.
- 2.5: a slot whose header claims more than `payload_cap` bytes is completed
  with `ST_BAD_REQUEST` and no payload.

Still left out of every part of a run:

- 3.4.2: a READDIR listing larger than `payload_cap` is answered with the
  overflow status `i32::MIN` and an empty payload. A tree large enough to
  overflow depends on the director's cap, which the run learns only once the
  tree is mounted.
- A WRITE or request larger than the ring's payload, and a registry value over
  `MAX_DATA` (the request would not fit the ring); `payload_cap - 8` for
  REG_KEY is covered only as a `one_of`, since the cap is the director's.

## Malformed requests

`run --malformed` mounts two built-in trees in turn. The file cases go to
`aether_conform::malformed::file_tree` (root 0 with `data/a.txt` = `abc` and
the writable entry `saves` holding `saves/s.txt`, root 1 with `r.txt`), which
has no registry layer, since a director need not have one (wire.md 4); for
them `data/a.txt` is opened for reading and `saves/s.txt` for writing. The
registry cases go to `registry_tree`, the same roots and a registry layer
with `\Registry\Machine\Software\Conform` and its value `v`. Each case is
sent as raw slot fields, through the ring client's held-slot call (or, for the header
case, a header written by hand). Each case's payload is a valid request with
one thing wrong, so the defect named is the only one. An error status must
come with an empty payload (3.1, 3.3). After every case a HEARTBEAT must be
answered `ST_OK`: the director must keep serving. At the end a GETATTR of
`data/a.txt` must still find it, size 3. The report has one entry,
`malformed`, with a step per case, each naming its rule.

| Case | Answer | wire.md |
|---|---|---|
| Opcodes 0, 4, 12, 25, 26, 100, `0x7fffffff`, `0xffffffff`, with no payload and with a path request | `ST_BAD_REQUEST` | 3.3: "Any other opcode (including 0, 4, 12 and 25 and up) is answered `ST_BAD_REQUEST` with an empty payload" |
| GETATTR, READDIR, HEARTBEAT, STORED_NAMES with other slot flags set; READ with `0xfffffffe` and `0xffffffff` | As with flags 0 (READ inline or bulk, compared by data) | 3.6: "The slot `flags` word is ignored for every opcode but READ, and bits other than bit 0 are ignored for READ" |
| READ of 20 bytes (no pad), twice | The data | 3.4.4: "24 bytes (a director must accept 20)" |
| GETATTR cut to 0 to 3 bytes | `ST_BAD_REQUEST`, or OK with `found = 0` | 3.5.1's table (old Rust director; Cairn) |
| READDIR cut to 0 to 3 bytes | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | 3.5.1 |
| OPEN cut to 0 to 7 bytes | `ST_BAD_REQUEST` | 3.5.1 |
| READ cut to 0 to 19, WRITE to 0 to 23, SETATTR to 0 to 15, CLOSE to 0 to 7 bytes | `ST_BAD_REQUEST` | 3.5.1 ("READ, WRITE, SETATTR, CLOSE: short"); 3.2 for WRITE's pad |
| RENAME cut to 0 to 7, RENAME_X to 0 to 11 bytes; either with a `from_len` past the payload | `ST_BAD_REQUEST` | 3.5.1 |
| DELETE cut to 0 to 3, MKDIR to 0 to 7, STORED_NAMES to 0 to 7 bytes | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | 3.5.1 |
| WRITE whose `len` is 10 with 3 data bytes | `ST_BAD_REQUEST` | 3.4.5: "A request whose payload is shorter than `24 + len` is `ST_BAD_REQUEST`" |
| GETATTR, OPEN, RENAME (`from`) of a missing path that is not UTF-8 | `ST_BAD_REQUEST`, or the missing-path answer (found = 0, `ST_NOT_FOUND`) | 3.5.1: "`ST_BAD_REQUEST` in the Rust decoders; Cairn decodes it with replacement characters and carries on" |
| REG_LOOKUP of a key path that is not UTF-8 | `ST_BAD_REQUEST`, or the reference's answer for the path with replacement characters | 3.5.1: "Invalid UTF-8 in a path is `ST_BAD_REQUEST` in the Rust decoders; Cairn decodes it with replacement characters and carries on" (its registry reader too, `registry.clj`, `get-str`) |
| REG_CREATE_KEY with `volatile` 2 and 255; REG_CHANGED with `subtree` 2 | `ST_BAD_REQUEST` | 4.1: "Booleans are one byte, exactly 0 or 1" |
| Each of the eight registry requests with a trailing byte | `ST_BAD_REQUEST` | 4.1: "Every registry decoder consumes its input exactly; trailing bytes are `ST_BAD_REQUEST`" |
| Each registry request cut by a byte; a REG_LOOKUP whose `len` runs past the payload | `ST_BAD_REQUEST` | 3.2 (truncation), 3.5 ("The payload did not decode") |
| GETATTR whose slot header claims `payload_cap + 1` and `0xffffffff` bytes | `ST_BAD_REQUEST`, payload length 0 | 2.5: "A slot whose `payload_len > payload_cap` MUST be completed with `ST_BAD_REQUEST` and payload length 0" |

Not sent: a short STAGE_EXEC (3.5.1 names no answer for it), trailing bytes
after a file request's fixed fields (3.2 describes the Rust decoders, which
ignore them, not a rule for directors), a HEARTBEAT with a payload, and
registry cases to a tree without a registry layer.

## Generated cases

`run --generate` plays `--cases N` generated cases (default 64), each a tree
and up to `--steps M` operations (default 32), all made from one seed
(`--seed S`; without it the seed comes from the clock). The seed is printed on
stderr, in the TAP report (`# seed S`) and in the JSON report's `seed`, and the
same seed gives the same cases, so `--seed S` replays a run.

**How a case is made** (`crates/aether-conform/src/model.rs`). proptest
generates a tree spec (up to three roots, files and directories with names
from a fixed list in varied case, writable entries or roots writable as a
whole, an optional registry layer) and a list of operations, each a kind and
four numbers. The generator builds the tree, mounts it in the reference
director held in this process (behind a ring in memory, served by
`aether_rpc::serve_one_with_arena` as a Rust director is), and reads the
operations one at a time: each one's numbers pick its target among what
exists at that point (an open handle, an existing file, a missing name under
an existing directory, a key) and its arguments. An operation with nothing to
act on is dropped, so every case proptest makes, and every case it shrinks
to, is a valid one. The reference's answer to each request is that step's
expectation, or, where wire.md allows several answers, a `one_of` of exactly
those with the section cited. The generator also says, for each request,
which answer wire.md gives; a reference that answers otherwise is reported as
a generator error (exit 2) naming the section, never made an expectation.

The case is then an ordinary scenario: it is written to a scratch file,
mounted through the adapter, and played and compared exactly as a scenario
file is, so every comparison rule above applies.

**Shrinking and replay.** The first case that fails is shrunk the way proptest
shrinks (simplify while it still fails, complicate when it passes), each
candidate played against the director, up to 2048 candidates. A candidate
counts as failing only if it fails the same way: a mismatch whose first
failing step has the same opcode, or an error of the same kind, so shrinking
cannot drift to another failure. If shrinking stops on an error, the
smallest failing case found so far is still written. The smallest
failing case is written to `--failures DIR` (default a folder named after the
seed in the temporary directory) as `generated-<seed>-<case>.json`, a scenario
file whose expectations are the reference's answers; `run --scenarios DIR`
replays it and fails the same way. The report's `generated` entry names that
file and lists its steps.

proptest-state-machine is not used: its reference state machine wants a model
of the answers, which here is the reference director itself, and it would
shrink by running the system under test in process. A plain strategy for
(tree, operations), read against the oracle's state as above, keeps every
shrunk case valid and plays each through the adapter's own mount.

**What is generated**, with the answer wire.md gives it:

| Request | Generated | Answer (wire.md) |
|---|---|---|
| HEARTBEAT | | `ST_OK` (3.4.11) |
| GETATTR | existing files and directories (root included) in any case spelling; missing names under existing directories; unserved roots | the reference's, found = 0 for missing paths and unserved roots (3.4.1) |
| READDIR | existing directories in any spelling; unserved roots | the listing (3.4.2, compared as a multiset); `ST_NOT_FOUND` |
| OPEN, reading (`read`, or `oflags == 0`) | existing files and directories; missing names; unserved roots | a handle (3.4.3); `ST_NOT_FOUND` |
| OPEN, writing (`write` with any of `read`, `create`, `excl`, `trunc`, `append`) | files the writable provider holds; missing names it would hold; files a read-only provider holds; missing names in a root with no writable provider | a handle, or `ST_EXISTS` with `excl`, or `ST_NOT_FOUND` without `create` (3.4.3); `ST_READ_ONLY` (3.4.3, 3.5) |
| READ | open handles, at offsets 0, odd, inside, at and past the end, lengths 0, 1, odd, to the end, 4096 and 65536 to 70000, with and without `read_bulk`; directory handles | the data (3.4.4, inline or bulk); `ST_IS_DIR` |
| WRITE | handles opened for writing, at offsets up to the end (any offset on an `append` handle), 0 to 20000 bytes; directory handles | `written` (3.4.5); `ST_IS_DIR` on a directory the writable provider holds, else `one_of` `ST_IS_DIR`, `ST_READ_ONLY` (3.4.5) |
| SETATTR | handles opened for writing: 0, half, the size, past it; directory handles | `ST_OK` (3.4.6); as WRITE on a directory handle (3.4.5) |
| CLOSE | open handles, also ones whose file changed under them | `ST_OK` (3.4.10) |
| READ, WRITE, SETATTR, CLOSE | closed handles and numbers never issued | `ST_BAD_FH` (3.5) |
| RENAME, and RENAME_X with equal roots | files and directories moved, onto existing files, empty and non-empty directories, respelled (same fold), into themselves, from missing names, from or to read-only names, the root at an end (root writable as a whole); unserved roots | 3.4.7's answers, the root at an end and a `to` inside `from` as `one_of` `ST_BAD_REQUEST`, `ST_IO_ERROR`; `one_of` `ST_NOT_FOUND`, `ST_BAD_REQUEST` (3.5.1) |
| RENAME_X across roots | files to a missing name (the caller's spelling) or onto a file (its stored spelling: 3.4.13 does not say whose spelling a replaced file keeps, and Cairn keeps the stored one); directories; missing names; an unserved root at one end | `ST_OK`; `ST_CROSS_ROOT`; `ST_NOT_FOUND` (3.4.13); `one_of` as above |
| DELETE | files, empty and non-empty directories the writable provider holds; missing names; read-only names; the root (writable as a whole); unserved roots | 3.4.8's answers, the root as `one_of` `ST_BAD_REQUEST`, `ST_IO_ERROR` |
| MKDIR | missing names; existing directories, files the writable provider holds, files a read-only one holds; missing names in a root with no writable provider; unserved roots | `ST_OK`; `one_of` `ST_OK`, `ST_EXISTS`; `ST_IO_ERROR`; `ST_EXISTS` (3.4.9); `ST_READ_ONLY` (3.5); `ST_NOT_FOUND` (3.5.1) |
| STORED_NAMES | existing names in any spelling, `skip` below the number of components; missing names; unserved roots | the stored spellings; `one_of` the names as sent, `ST_NOT_FOUND` (3.4.12); `ST_NOT_FOUND` (3.5.1) |
| STAGE_EXEC | existing files | `one_of` `ST_NOT_SUPPORTED`, `ST_NOT_FOUND` (a stager that does not stage it), a path (3.4.14) |
| The eight registry requests | keys of the tree and below them in any spelling under `\Registry\Machine`, values from a fixed list, REG_CHANGED `since` a version an earlier reply gave; non-canonical key paths; `.` and `..` as a new leaf; every registry request without a registry layer | the overlay's answers (4.3, `overlay.rs` is normative); `ST_BAD_REQUEST` (4.2, 4.3); `ST_NOT_SUPPORTED` (4) |

**What is not generated, and why:**

- Names whose answer depends on how a director layers its providers: a
  writable entry's own directory and its ancestors are only read (GETATTR,
  READDIR, OPEN for reading), never written, deleted or renamed (README, "Who
  holds what").
- Writes and creates with no fixed answer (as "Left out" above): a write open,
  DELETE or RENAME of a missing name outside every writable provider in a
  root that has one; a create whose parent is missing; READDIR of a missing
  path or a file; a path below a file; a write open of a directory.
- Bytes wire.md does not fix: a WRITE starts at most at the end of the bytes
  that are fixed, and after a SETATTR extends a file only the bytes before the
  old end are read (what the extension holds is the director's).
- WRITE and SETATTR on a handle opened without `write`, READ on one opened
  without `read`: wire.md says nothing of them.
- A handle whose file changed through another handle or a path (a WRITE or
  SETATTR through another handle, a truncating open, a RENAME, RENAME_X or
  DELETE of it or of a directory above it) is only closed afterwards: what it
  reads then is not fixed.
- RENAME_X onto a directory, and RENAME_X with a read-only end (3.4.13 names
  no status).
- A RENAME whose `to` is an ancestor of `from`, or the same name spelled the
  same way.
- Paths that leave their root (`escape-paths` covers them) and every
  non-canonical file path (3.2).
- Registry requests near a limit (reply size, name length, depth), which
  `registry-limits` and `registry-key-too-large` cover, and a REG_CHANGED
  before any registry reply (its `since` has no numbering yet).
- `ST_NO_SPACE`: sizes stay far below any cap.
- File names a Windows file system would refuse or compare differently from
  the wire's fold (no final sigma, no dotted capital I), so a director that
  stores names on disk is not failed for its file system.

Three rules the generator relies on are wire.md sentences, each checked
against Cairn's providers: a director writes every byte of a WRITE or fails
it (3.4.5); GETATTR, READDIR and OPEN report a file's size as it is now,
writes through handles still open included (3.4.1); a file with open handles
is deleted or renamed like any other (3.4.7, 3.4.8). What a handle reads after
its file was deleted or renamed stays the director's, so such a handle is only
closed. No other assumption beyond wire.md remains.

Sizes follow the director's ring: the generator learns `VFS_RING_PAYLOAD_CAP`
and `VFS_ARENA_LEN` from a mount of an empty tree before the first case, and
keeps a READ the director answers inline (any READ without an arena; one
without `read_bulk` below 65536 bytes otherwise) to `payload_cap - 8` bytes
(3.4.4) and WRITE data to `payload_cap - 24` (3.4.5).

## The adapter protocol

An adapter is any program that can mount a tree in the director under test.
`aether-conform run` starts it as a child process and talks to it in JSON,
one object per line: one request on the adapter's stdin, then one answer on
its stdout, never two requests at once.

```text
{"op":"mount","scenario":"/abs/path/contract/scenarios/files.json"}
    -> {"ok":true,"env":{"VFS_RING_PATH":"/tmp/x.ring","VFS_RING_BYTES":"16777216",...}}
    or {"ok":false,"error":"why not"}
{"op":"unmount"}
    -> {"ok":true}
```

- `mount` names a scenario file by its absolute path. The adapter reads the
  file's `tree` (the format is `contract/scenarios/README.md`'s; the steps
  are not its business), mounts it in its director, and answers `env`: the
  handshake variables (wire.md 5.3) a shim launched into that tree would get,
  every value a string. At least the ring (`VFS_RING_PATH`, or
  `VFS_RING_SECTION` with `VFS_SERVER_EV` and `VFS_CLIENT_EV`),
  `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN` (and
  `VFS_ARENA_OFFSET`) for bulk reads, and `VFS_VIRTUAL_DIR`. A refusal
  (`"ok":false`) fails that scenario and the run goes on.
- `unmount` lets the mounted tree go. The run has detached from the ring
  before it sends this, so the adapter may remove a ring file.
- EOF on stdin means the run is over: the adapter should exit. It is killed
  if it has not exited within the timeout.
- Whenever the run kills the adapter (a timeout, an error, the end of the
  run), it kills every process the adapter started too, so an adapter may be
  a launcher (a `.cmd` file, a shell script, `clojure`) that starts the real
  director. On Windows the adapter runs in a Job Object that kills its
  members when closed; on Unix it leads a process group, which is sent
  `SIGKILL` while the adapter is still unreaped, so the number cannot
  belong to another group yet. Where there is no `/proc` (not Linux), an
  adapter that exits by itself is reaped before its group could be
  signalled, and what it left running survives. A process that leaves the
  group (`setsid`) is not followed. A failed group kill is said on stderr
  and in the run's error.
- Anything the adapter writes on stderr is kept; its last lines are shown
  when something goes wrong.

Every answer must come within the timeout (`--timeout`, 30 seconds by
default). An adapter that crashes, closes its stdout, writes a line that is
not a JSON object of the right shape, refuses an unmount, or does not answer
in time fails the scenario being run and ends the run, with exit code 2.

`VFS_RING_PATH` is the path as the conformance client sees it. A native
director gives its host path; a director that serves a shim under Wine still
gives the host path here, since `aether-conform` runs natively.

### Attaching

The run attaches the way the shim does (`crates/aether-hooks/src/director.rs`):
a non-empty `VFS_RING_PATH` wins over `VFS_RING_SECTION` (2.10); a file is
mapped whole, a section (Windows only) by name at `VFS_RING_BYTES`; numbers
the shim cannot parse take the shim's defaults. On a file ring nothing is
signalled and the client spins for its reply, so the director must poll
(2.11). On a section the client sets the event `VFS_SERVER_EV` names after
publishing a request and after freeing a slot, so a director that blocks on
that event is woken; like the shim, it goes on without the event if it does
not open. A reply that has not come within the ring's spin budget is waited
for by yielding, then sleeping a millisecond at a time, up to the timeout.
READs go through held slots, so bulk data is copied out of the arena before
the slot is freed (2.6); a step that asks for `read_bulk` on a ring with no
arena is sent without it, as the shim would, and its note says so.

## Running it

Every release has `aether-conform-windows-x64.zip` and `aether-conform-linux-x64.tar.gz`
(unsigned) and the scenarios in `contract.zip`; in the examples below, `target/debug/aether-conform`
is that binary and `contract/scenarios` the unpacked `scenarios` folder.

```sh
cargo build -p aether-conform
# The reference director, over a ring file:
target/debug/aether-conform run --director "target/debug/aether-conform reference-adapter"
# The same over a named section (Windows):
target/debug/aether-conform run --director "target/debug/aether-conform reference-adapter --ring section"
# With the malformed requests and 256 generated cases from seed 42:
target/debug/aether-conform run --director "target/debug/aether-conform reference-adapter" \
    --malformed --generate --cases 256 --seed 42 --failures conform-failures
# Replaying a shrunk failure:
target/debug/aether-conform run --director "..." --scenarios conform-failures
# Another director, behind its own adapter:
aether-conform run --director "java -jar my-director-adapter.jar" --scenarios contract/scenarios \
    --report json --out report.json --timeout 10
```

`run`'s options:

| Option | Meaning |
|---|---|
| `--director "<command line>"` | The adapter to start. Words split on spaces; double quotes group, and a backslash is literal except before a quote, so Windows paths need no doubling. Required. |
| `--scenarios DIR` | The `*.json` scenarios to play, in file-name order. Default `contract/scenarios`. A folder with none is an error, not a pass. |
| `--report tap\|json` | The report's format. Default `tap`. |
| `--out FILE` | Where the report goes. Default stdout. |
| `--timeout SECS` | How long every adapter answer and every ring request may take. Default 30. |
| `--malformed` | Also send the malformed requests ("Malformed requests"). |
| `--generate` | Also play generated cases ("Generated cases"). |
| `--cases N` | How many generated cases. Default 64. |
| `--seed S` | The seed the cases are made from. Default: from the clock, printed. |
| `--steps M` | The most operations per case. Default 32. |
| `--failures DIR` | Where the shrunk failing case is written. Default `aether-conform-failures-<seed>` in the temporary directory. |

Exit code: 0 when every scenario passes, 1 when any fails, 2 on a usage
error, a scenario that does not load, an adapter error, or a generator error
(the reference answered a generated request otherwise than wire.md says).

CI runs the committed scenarios, `--malformed` and `--generate --cases 1024`
against the reference, over a file ring on Linux and Windows and over a
section on Windows, with the workflow run's id as the seed. Measured on a
16-thread Windows machine with a debug build, 1024 cases take about 19 s over
a file ring (each mount creates a 16 MiB ring file) and 8 s over a section;
pinned to two cores, 512 take 8.4 s and 3.5 s. At two to three times slower
on CI's 2-vCPU runners that is about one minute for the Windows step.

`aether-conform reference-adapter [--ring file|section]` is an adapter for
the reference director (`aether_refdirector::Reference`, built with
`Reference::from_tree`), served with the same loop a Rust director runs
(`aether_rpc::serve_with_arena`) on its own thread. It is what a run is
checked against in this repository's tests, and an example of what an
adapter does. Its hidden `--break <rule>` switch, for this repository's tests
only, makes it break one rule on purpose (`aether_conform::adapters::Break`),
so the tests can show that generation and the malformed part find it.

## Reading a report

Every step is played even after one fails, so the report shows them all; a
later failure can follow from an earlier one, so read the first failure of a
scenario first. A ring that stops answering ends that scenario. Steps are
numbered from 0, as in the scenario's `steps` list.

TAP (version 13), one test point per scenario; a failed one has a YAML
block with its error, if any, and each failed step:

```text
TAP version 13
1..2
ok 1 - files
not ok 2 - wrong
  ---
  file: "contract/scenarios/wrong.json"
  failures:
    - index: 0
      request: {"op":"getattr","path":"a.txt","root":0}
      expected: "GetAttr(AttrResp { found: true, is_dir: false, size: 4, mtime: 0 })"
      got: "GetAttr(AttrResp { found: true, is_dir: false, size: 3, mtime: 0 })"
      note: "different attributes (mtime aside)"
  ...
```

`request` is the request as the scenario wrote it (before handles were
translated), in the scenario format, so it can be pasted into a scenario (a
malformed request, which that format cannot say, is described in a string);
`expected` and `got` the two answers, and `note` the rule that failed
followed by the scenario's own note. An adapter error ends the report with
`# ` lines holding the message and the adapter's stderr, and a `Bail out!`
line.

JSON has every step, passed or not:

```json
{
  "scenarios": [
    {"name": "wrong", "file": "contract/scenarios/wrong.json", "passed": false, "error": null,
     "steps": [
       {"index": 0, "request": {"op": "getattr", "path": "a.txt", "root": 0},
        "expected": "GetAttr(AttrResp { found: true, is_dir: false, size: 4, mtime: 0 })",
        "got": "GetAttr(AttrResp { found: true, is_dir: false, size: 3, mtime: 0 })",
        "passed": false, "note": "different attributes (mtime aside)"}]}
  ],
  "seed": null,
  "adapter_error": null
}
```

A scenario's `error` says what stopped it outside its steps: a refused mount
(`mount refused: ...`), a failed attach (`attach failed: ...`), a ring that
gave no answer, or the adapter error that ended the run, which is also in
`adapter_error`. `seed` is the generator's seed, and `null` for a run without
`--generate`.

The malformed part is one entry, `malformed`, a step per case; its `request`
names the rule and shows the slot fields and payload in hex. The generated
part is one entry, `generated`: passed, with no steps and the case count and
seed as its `file`, or failed, with the shrunk case's steps and the file it
was written to.
