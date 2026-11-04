# Director conformance

`aether-conform` checks a director against the wire contract
(`docs/wire.md`). It starts the director under test behind a small adapter
program, has it mount a scenario's tree, attaches to the ring the director
names exactly as the shim would, plays the scenario's requests, and compares
each answer with the one the scenario expects. Scenarios live in
`contract/scenarios/` and their format is described in
`contract/scenarios/README.md`.

A run has up to five parts, all through the same adapter:

- **Scenarios**, the files in `--scenarios` (always). A step may name the
  client that sends it; every client but 0 is a process of its own
  ("Several clients" below).
- **Malformed requests** (`--malformed`): payloads that are not valid
  requests, sent on the raw ring, and the answers wire.md gives them
  ("Malformed requests" below).
- **Requests sized from the ring** (`--limits`): READs, a WRITE and
  listings of the director's own limits, READs of one handle in flight
  together, an abandoned slot, and a registry writer against a reader
  ("Requests sized from the ring" below).
- **Generated cases** (`--generate`): trees and operation sequences made
  from a seed, each step compared with the reference director's answer, and
  the first failure shrunk and written as a scenario file ("Generated cases"
  below).
- **Concurrent clients** (`--concurrent N`): N clients at once, each playing
  a generated case over a subtree of its own, their requests in flight
  together across every slot of the ring ("Concurrent clients" below).

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
- **What the environment says of the tree** (`attach::against_tree`), which
  fails the scenario (its `error` begins "the mount's environment"):
  `VFS_REGISTRY` is affirmative exactly when the tree has a registry layer
  (5.3: "Set to `1` only while a registry layer is attached; otherwise
  remove it"); every root of the tree is declared, root 0 by
  `VFS_VIRTUAL_DIR` and the others by `VFS_VIRTUAL_ROOTS` as the shim reads
  it, no two at one path (3.2, 5.3: a root the shim is not told of is never
  routed, and the program's files there go to the real disk); a
  `VFS_SHIM_CONFIG` the mount names decodes as the shim decodes it and its
  `root` is root 0's (5.1); and no value is one of the stale session's the
  run leaves in the adapter's own environment ("The adapter protocol"
  below; 5.2: "A launch sets or removes every one of these").
- **A HEARTBEAT on attach**, as the shim sends one once it has connected.
- **Every step's answer**: the status, and on `ok` every reply field, equal
  to the step's expectation except where wire.md lets directors differ
  (below). A bulk READ reply is read from the arena and compared by its
  data, and must lie inside the mapping (2.6).
- **What the shim caches on the director's word**, across a scenario's steps
  (or a generated case's), where no single answer shows it:
  - *The read cache* (3.4.3: "two immutable opens of one path with equal size
    and `mount_gen` must have identical content"). Every READ through a
    handle whose OPEN reply set `immutable` is recorded under (root, folded
    path, size, `mount_gen`, offset, length) for the rest of the scenario;
    another READ of that key through an immutable handle that returns other
    bytes fails, since the shim's read cache would serve the old ones.
  - *The registry generation* (4.5), read from the ring header before every
    request and after every reply: in a scenario whose tree has a registry it
    is non-zero before the first registry request; a successful
    REG_SET_VALUE, REG_DELETE_VALUE, REG_CREATE_KEY, REG_DELETE_KEY or
    REG_RENAME_KEY leaves it greater after the reply than before the
    request; and it never goes backward. With several clients, each step's
    generation is the one its own client read in its own mapping, so a write
    by one process must be visible to the next request of another.
- **What only a second client sees** (2.5, 2.6, 2.8.1): handles paired
  across every client of the ring, so one process may present another's
  handle and a number open in any process is open for all; a request in a
  slot other than 0 answered within the timeout; a bulk reply's data in the
  bank of the slot it answers ("Slot `s` owns bank `s % banks`"); a
  completed slot's `ack` naming the request it holds.

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
| OPEN `fh` | Paired one to one with the scenario's number while open; requests carry the director's number. After a CLOSE the director's old number is sent, so a use after close tests that handle; a number never paired gets a stand-in no open handle has. Never 0. | 3.4.3: "`fh` is any u64 the director chooses, unique among open handles"; the shim keeps 0 for a directory it made. |
| OPEN `mount_gen` | Not compared. | 3.4.3: "a generation the director bumps whenever a root's content is replaced", and the directors start it differently. |
| OPEN `immutable` | Not compared, but must be clear on a write open and on a directory, and reads through immutable handles are checked against each other ("What the shim caches", above). The reply's other flag bits and bytes 18-19 must be zero. | 3.4.3: "Never set it for a write open or a directory"; "other bits zero"; 7. |
| READDIR order | Entries compared as a multiset. | 3.4.2: "A director may send the entries in any order." |
| `mtime` (GETATTR, READDIR) | Not compared, but below 2^40 (the year 36812): a time in milliseconds is past it. | 3.2: "Which time a director reports for a path, if any, is the director's to choose"; "`mtime` is seconds since the Unix epoch". |
| READ inline or bulk | Compared by data. | 3.4.4: the client "must accept either whatever flag it sent". |
| STAGE_EXEC `path` | Not compared, but it must be an absolute Windows path (a drive letter's or UNC), and where the run's host can read it, its bytes must be the ones the director serves for the image (read through the ring). A mount without `VFS_ROOT_BACKINGS` gets a warning: the shim sends STAGE_EXEC only with one. | 3.4.14: the reply is the "on-disk Windows path to start it from", wherever the director staged it. |
| Registry `version` | Compared up to one constant offset per scenario, fixed by the first registry reply; a REG_CHANGED `since` is sent shifted by it, except `u64::MAX`, the shim's probe for the current version, sent as it is. | 4.3: "two directors serving one layer may number its versions apart by a constant, and a REG_CHANGED `since` is in the numbering of the director it is sent to". |
| REG_KEY `last_write` | Not compared. | 4.3: a "FILETIME" the overlay stamps from the director's clock. |

Where wire.md allows more than one answer to a request, the scenario lists
them (`one_of`) and names the section that allows it.

**What a run does not exercise.** The director's thread count against
`worker_hint` (2.8: a run keeps its data requests below the limit the hint
gives, as the shim does, but cannot count a director's threads; `--limits`
warns on a hint of 0 or above the slot count), how quickly a director wakes
(2.11; `--limits` reports a HEARTBEAT round trip, never failing it), the
injector (6), a
process killed while it holds a slot (a slot left `CLAIMED` is lost to the
ring, and wire.md says nothing to recover it), and the read cache's rule
(3.4.3) across a remount, which no scenario makes. A TRANSPORT name a
launch should remove is seen only if the adapter reports the launch's value
for it: an adapter that reports only what it sets hides a stale one it
passes on.

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
| `write-*` | write opens (create, excl, trunc, append), WRITE, SETATTR, spelling kept, the gap a WRITE past the end or a SETATTR leaves read as zeros (`write-growth`), READ through a handle opened for writing alone (`write-only-read`) (3.4.3 to 3.4.6, 3.2) |
| `rename-*` | RENAME: replace, respell, directories, refusals, unknown root (3.4.7, 3.5.1) |
| `rename-x-*` | RENAME_X: files across roots, directories `ST_CROSS_ROOT`, equal roots, errors (3.4.13) |
| `mkdir-*`, `delete-*` | MKDIR and DELETE: spelling, existing names, non-empty directories, read-only, errors (3.4.8, 3.4.9) |
| `readonly-root-refuses-changes` | a root with no writable provider: DELETE, RENAME, RENAME_X between equal roots `ST_READ_ONLY`, SETATTR on a directory handle (3.4.5, 3.4.7, 3.4.8, 3.4.13, 3.5) |
| `cache-*` | what the read-cache check rests on: a writable file overwritten at the same size reads the new bytes; reads of one read-only file agree across handles (3.4.3) |
| `readdir-*` | READDIR: listing, case, changes, unknown root (3.4.2) |
| `stored-names-*` | STORED_NAMES: stored spelling, `skip`, unknown names, after changes (3.4.12) |
| `case-fold-*` | the fold: ASCII, Unicode (`Σ`, `İ`, composed and decomposed), fold-equal names are one entry (3.2) |
| `names-stream` | a path with a named stream suffix is a name the director does not have, never its base file (3.2) |
| `names-non-bmp` | names with characters outside the BMP read, created, listed, respelled and renamed like any other (3.2) |
| `roots-*` | several roots kept apart, unserved roots (3.2, 3.5.1) |
| `stage-exec` | STAGE_EXEC: `ST_NOT_SUPPORTED`, `ST_NOT_FOUND` or a path (3.4.14) |
| `registry-none` | no registry layer: all eight opcodes `ST_NOT_SUPPORTED` (4) |
| `registry-lookup`, `-case`, `-bad-path` | REG_LOOKUP states, case-insensitive keys, non-canonical paths (4.2, 4.3) |
| `registry-values`, `-delete-value` | REG_SET_VALUE, REG_KEY values, value tombstones (4.3) |
| `registry-create-key`, `-delete-key`, `-rename-key` | key create, delete, rename, children (4.3) |
| `registry-changed` | REG_CHANGED, with `since` in the director's numbering, and the shim's `u64::MAX` probe (4.3) |
| `registry-current-user` | keys under `\Registry\User\<CurrentUser>` and `<CurrentUser>_Classes`, apart from HKLM and from each other (4.2, 4.3) |
| `registry-limits`, `-key-too-large` | name, depth and reply limits; `ST_REPLY_TOO_LARGE` (4.1, 4.3) |
| `multi-handle-shared` | a handle opened by one process read, written and closed by another; closed for all once one closes it; a bulk READ from slot 1 in bank 1 (2.6, 2.8.1, 3.4.3) |
| `multi-handles-unique` | three processes' handles, all open at once, unique across them (2.8.1, 3.4.3) |
| `multi-write-visible` | one process's writes, truncation and new file seen by another in GETATTR, READDIR, OPEN and READ (2.8.1, 3.4.1 to 3.4.5) |
| `multi-registry-gen` | one process's registry writes move `reg_gen` in another's mapping and change its answers (2.8.1, 4.3, 4.5) |

**Left out because wire.md does not fix the answer**, so no scenario
asserts it: READDIR of a path that is not a directory; a path below a file
(accepted as found=0, `ST_NOT_FOUND` or `ST_NOT_A_DIRECTORY`); a write open,
DELETE or RENAME of a missing path outside every writable provider; a create
whose parent directory is missing; WRITE or SETATTR on a handle opened
read-only; `ST_NO_SPACE`. (What a write past the end, or a SETATTR, puts in
the gap is zeros, and a handle opened for writing alone serves READ: wire.md
3.4.4 to 3.4.6 now say so, and `write-growth` and `write-only-read` check
them.)

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

Left out of the scenarios because their sizes are the director's (its
`payload_cap`, slot count and arena), and played by the limits part of a run
(`--limits`, "Requests sized from the ring" below): READs of the shim's own
sizes, a WRITE of exactly `payload_cap`, a READDIR listing at the cap and one
byte over it (3.4.2: the overflow status `i32::MIN`).

Still left out of every part of a run:

- A WRITE or request larger than the ring's payload, and a registry value over
  `MAX_DATA` (the request would not fit the ring); `payload_cap - 8` for
  REG_KEY is covered only as a `one_of`, since the cap is the director's.

## Several clients

A step may carry `"client": n` (default 0, `contract/scenarios/README.md`).
The run attaches one client per number a scenario names. Client 0 is the
run itself. Every other client is a process of its own, `aether-conform
client-worker` (`crates/aether-conform/src/clients.rs`):

- it starts with the mount's handshake variables as its environment (any
  `VFS_*` variable of the run's own removed first);
- it attaches from them exactly as the run does, to a file ring or to a
  named section and its events;
- it sends the HEARTBEAT the shim sends, then the requests the run gives it,
  one at a time.

Client `k` claims from slot `k` (mod the slot count), as a client thread
whose hint is `k` does (2.5). So a scenario with two clients uses two slots
and two banks, where one client sending one request at a time only ever
uses slot 0. The wire's promises are about processes (2.8.1: "every process
of the launch shares it"). That is why scenario clients are processes, each
mapping the ring for itself, and not threads sharing the run's mapping.

One comparison session serves all the clients of a scenario, so handles are
paired ring-global, as 2.8.1 has them. The loader refuses an OPEN that
expects a number still open in any client. A client process that cannot
start, attach or send its HEARTBEAT fails the scenario, as a failed attach
does.

A scenario cannot say that a process is killed (2.8.1: it "never sends
CLOSE for its open handles"), so a test does it
(`crates/aether-conform/tests/run.rs`, `a_killed_client`), on a file ring
and on a section. It kills a client process holding open handles, then
checks:

- the director goes on answering this process and a new client process;
- the new client reads through a handle the killed one left open (the
  director keeps them until the session ends);
- new handles do not collide with the old ones.

## Concurrent clients

`run --concurrent N` plays `--concurrent-cases C` rounds (default 64) of N
clients at once (`crates/aether-conform/src/concurrent.rs`). The rounds come
from `--seed` (as `--generate`'s cases do, and the same seed if both are
given) and have at most `--steps` operations per client.

**A round.** Each client `k` gets a generated case ("Generated cases" below)
limited to one root with a writable provider and no registry layer. The
overlay's version is one counter for the whole session, so one client's
registry writes would change another's answers. The case's tree and paths
move under the directory `c<k>` of root 0: a STORED_NAMES `skip` goes up by
one, and expected handle numbers move into a range of their own. The
clients' subtrees are disjoint, so each client's answers are its own case's
whatever the others do, and the reference predicts them exactly. Before a
round is played, it is checked against an in-process reference serving the
combined tree; a case that answers otherwise there is a generator error.
Root 0 also holds one file per slot under `sweep/`, each with its own size
and bytes.

**Playing it.** Every client is a thread with its own mapping of the ring,
its own HEARTBEAT, its own comparison session and its own registry
generation rules. Each sends its steps one at a time, claiming from the next
slot of a cursor the clients share. Data requests (READ, WRITE, SETATTR and
write OPENs) stay below 2.8's limit, `data_limit(worker_hint, slot_count)`,
as the shim keeps them. A case's steps that present a closed handle, or a
number never issued, are left out of its round (the generated cases send
them): a client knows only its own handles, and a director may issue any
u64 (3.4.3), so any number one client sent for a handle it does not have
could be another client's live handle, which would answer it, or which a
CLOSE would close. Such a step expects `ST_BAD_FH` and changes nothing, so
the other steps' answers stay the same. An OPEN that may answer a handle or
an error (a `one_of` with both) ends its client's case: the later steps were
made after the reference's choice, and the director may choose the other.
Then the sweep, which gives every slot and bank work:

- an OPEN of each sweep file, each claimed from its own slot, so every slot
  carries a request;
- a GETATTR in every slot at once;
- a bulk READ of each sweep file from its own slot, in waves of the data
  limit, each wave in flight together.

Each answer must have its own file's size or bytes.

**What fails a round**, besides a step whose answer is not its client's
expectation:

| Rule | wire.md |
|---|---|
| Every request is answered within `--timeout`, whichever slot it is in. | 2.5: "Every taken slot MUST be completed"; slots are taken "in any order and from any number of threads" |
| A reply is the answer to the request in its slot: each client's requests name its own paths and contents, so a reply written into another request's slot fails that client's comparison, and the sweep's answers differ slot by slot. | 2.5 |
| A completed slot's `ack` is the low 32 bits of its request's `req_id`. | 2.5, "The client's read" |
| A bulk reply's data lies in the bank of the slot it answers. | 2.6: "Slot `s` owns bank `s % banks`" |
| An `fh` is not issued to one client while another has it open; a number counts as closed once its CLOSE is sent. | 2.8.1: "a director must issue them uniquely across all processes of the ring"; 3.4.3 |
| `reg_gen` never goes backward in any client's mapping. | 4.5 |

At the end, every slot must have carried a request. A director may answer
any READ inline (3.4.4), so a bank that never carried a bulk reply is not a
failure. The report's `concurrent` entry, and a line on stderr, say how many
slots and banks were used and the most requests in flight at once. The first
round that fails is written to the failures folder as
`concurrent-<seed>-<round>.json`. It is a scenario whose steps are marked
with their clients, which `run --scenarios` replays one step at a time. A
fault that needs requests in flight together may not show in the replay.

Concurrent clients are threads of the run, not processes, since a round has
many of them. The processes the wire speaks of are what the `multi-*`
scenarios and the killed-client test cover, over both kinds of ring.

## Requests sized from the ring

`run --limits` (`crates/aether-conform/src/limits.rs`) mounts an empty tree
to learn the ring's `payload_cap`, slot count and `VFS_ARENA_LEN`, then a
tree sized from them: a file of eight of the shim's read chunks and a bit,
a directory whose READDIR reply is exactly `payload_cap` bytes and one whose
reply is a byte longer, a writable directory, and a registry key. The report
has one entry, `limits`: its steps first, played as a scenario's are (every
comparison rule applies), then the checks a step cannot say.

| What | Answer | wire.md |
|---|---|---|
| READs of the shim's bulk chunk `clamp(VFS_ARENA_LEN / slot_count, 256 KiB, 1 MiB)` with `FLAG_READ_BULK`, at the start, inside, at the last whole chunk and at the end; remainders of 64 KiB and of a chunk less a byte; the read cache's run of 16 blocks of 64 KiB; 65 535 bytes inline; `payload_cap - 8` and `payload_cap - 9` without the flag | the bytes, all of them to the end of the file | 2.6, 3.4.4: `n < len` means end of file, so a READ comes back whole when its form can hold it; a director with an arena answers a flagged READ over `payload_cap - 8` in bulk |
| A READ of the chunk plus one byte | all of it, or at most a bank | 3.4.4: a bulk reply's `n <= min(len, bank_size)` |
| With no arena, READs of `payload_cap - 7` bytes and of `2 * payload_cap` | `payload_cap - 8` bytes, inline | 3.4.4: an inline reply's `n <= min(len, payload_cap - 8)`, whole whenever its form can hold it |
| Two WRITEs of `payload_cap - 24` bytes (requests of exactly `payload_cap`), then GETATTR and READs back | `written` = the length; the size and bytes of both | 3.4.5 |
| READDIR of a listing of exactly `payload_cap` bytes | the whole listing | 3.4.2 |
| READDIR of a listing a byte longer | status `i32::MIN`, payload length 0 | 3.4.2, 2.5 step 3 |
| 4 and then up to 8 bulk READs of one handle (one slot kept free), and 4 inline READs of 65 535 bytes, each set published at once at offsets out of order, held until all are `COMPLETED`, another READ answered meanwhile, and only then copied out, as `read_fragmented` does | each its own offset's bytes, in its own slot's bank | 2.5, "Order of service" (a pipelined read publishes up to 4 requests at once, 8 for reads of 4 MiB or more); 2.6: the bank is the client's while the slot is `COMPLETED` |
| A bulk READ given up (`abandon`) the moment the director takes it, tried up to 100 times until it is caught `PROCESSING` | the slot ends `FREE`, never `COMPLETED`; then a HEARTBEAT from every slot is answered. A director never caught processing is not failed: the step says the rule was not exercised | 2.5: "a director must never store `COMPLETED` over `ABANDONED`" |
| 300 REG_SET_VALUEs of one value from one client while another reads it with REG_KEY, reading `reg_gen` before each read | a read made at a generation a write's reply had already published sees that write (the writer reads `reg_gen` after each reply, which is at least the generation its write published) | 4.5: it moves "before the write's reply is published", and the shim caches an answer under the generation it read before asking. A director with no registry layer skips it |
| `worker_hint` | a warning, never a failure, when it is 0 or above the slot count | 2.8 |
| 200 HEARTBEATs one after another | the median and slowest round trip, a warning only | 2.11 |

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
| The eight registry requests | keys of the tree and below them in any spelling below the hive, under a base key in each of `\Registry\Machine`, `\Registry\User\<CurrentUser>` and `\Registry\User\<CurrentUser>_Classes` (4.2), values from a fixed list, REG_CHANGED `since` a version an earlier reply gave; non-canonical key paths; `.` and `..` as a new leaf; every registry request without a registry layer | the overlay's answers (4.3, `overlay.rs` is normative); `ST_BAD_REQUEST` (4.2, 4.3); `ST_NOT_SUPPORTED` (4) |

**What is not generated, and why:**

- Names whose answer depends on how a director layers its providers: a
  writable entry's own directory and its ancestors are only read (GETATTR,
  READDIR, OPEN for reading), never written, deleted or renamed (README, "Who
  holds what").
- Writes and creates with no fixed answer (as "Left out" above): a write open,
  DELETE or RENAME of a missing name outside every writable provider in a
  root that has one; a create whose parent is missing; READDIR of a missing
  path or a file; a path below a file; a write open of a directory.
- Growth: a WRITE starts at most at the end of the bytes that are fixed, and
  after a SETATTR extends a file only the bytes before the old end are read.
  wire.md 3.4.5 and 3.4.6 now fix the gap as zeros (`write-growth` checks
  it); the generator has not been taught it yet.
- WRITE and SETATTR on a handle opened without `write`: wire.md says nothing
  of them. READ on one opened without `read`, which wire.md 3.4.4 now says a
  handle serves (`write-only-read` checks it), is not generated yet.
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
  `VFS_ARENA_OFFSET`) for bulk reads, and `VFS_VIRTUAL_DIR`;
  `VFS_VIRTUAL_ROOTS` for a tree with other roots and `VFS_REGISTRY` for one
  with a registry layer. It may give `VFS_SHIM_CONFIG`, the path of the
  `shim.cfg` its director writes for the launch (5.1; the injector passes it
  on), which the run then decodes. A refusal (`"ok":false`) fails that
  scenario and the run goes on.
- The run starts the adapter with a stale session's TRANSPORT variables in
  its environment (every name of 5.2's table but `VFS_REGISTRY`, with values
  like `aether-conform-stale-VFS_RING_SECTION` and `7`,
  `aether_conform::adapter::stale`). A mount answer that holds one passed it
  on, as a launch built on the director's own environment would.
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

Every release has `aether-conform-windows-x64.zip` and
`aether-conform-linux-x64.tar.gz` (unsigned, each smoke-run against the
reference before it is packaged) and the scenarios in `contract.zip`. The
Linux binary is built on GitHub's `ubuntu-latest`, so it needs a glibc at
least as new as that image's. With a release, unpack the binary and
`contract.zip`'s `scenarios` folder side by side:

```sh
./aether-conform run --director "./aether-conform reference-adapter" --scenarios scenarios
```

From a source build, the binary is `target/debug/aether-conform` and the
scenarios are in `contract/scenarios` (the default), as in the examples
below:

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
| `--limits` | Also play the requests sized from the ring ("Requests sized from the ring"). |
| `--generate` | Also play generated cases ("Generated cases"). |
| `--cases N` | How many generated cases. Default 64. |
| `--seed S` | The seed the cases are made from. Default: from the clock, printed. |
| `--steps M` | The most operations per case. Default 32. |
| `--failures DIR` | Where the shrunk failing case, and a failing concurrent round, are written. Default `aether-conform-failures-<seed>` in the temporary directory. |
| `--concurrent N` | Also play rounds of N clients at once ("Concurrent clients"). `--seed`, `--steps` and `--failures` apply to them too. |
| `--concurrent-cases C` | How many rounds, each a case per client. Default 64. |

Exit code: 0 when every scenario passes, 1 when any fails, 2 on a usage
error, a scenario that does not load, an adapter error, or a generator error
(the reference answered a generated request otherwise than wire.md says).

CI runs the committed scenarios, `--malformed`, `--limits`, `--generate --cases 1024`
and `--concurrent 4 --concurrent-cases 256` against the reference, over a
file ring on Linux and Windows and over a section on Windows, with the
workflow run's id as the seed; and the committed scenarios, `--malformed`,
`--limits` and `--generate --cases 256` over a ring file with no arena
(`--no-arena`), on Windows. The concurrent rounds take about 15 s per
ring with a debug build on a 16-thread Windows machine. Measured on a
16-thread Windows machine with a debug build, 1024 cases take about 19 s over
a file ring (each mount creates a 16 MiB ring file) and 8 s over a section;
pinned to two cores, 512 take 8.4 s and 3.5 s. At two to three times slower
on CI's 2-vCPU runners that is about one minute for the Windows step.

`aether-conform reference-adapter [--ring file|section] [--no-arena]` is an
adapter for the reference director (`aether_refdirector::Reference`, built
with `Reference::from_tree`), served with the same loop a Rust director runs
(`aether_rpc::serve_with_arena`) on four threads, each with its own handle
on the reference (`Reference::share`), with `worker_hint` 4. Its ring has a
bulk arena of 1 MiB per slot, or, with `--no-arena`, none (`VFS_ARENA_LEN`
unset, every READ answered inline). It is what a run is
checked against in this repository's tests, and an example of what an
adapter does. Its hidden `--break <rule>` switch, for this repository's tests
only, makes it break one rule on purpose (`aether_conform::adapters::Break`),
so the tests can show that generation and the malformed part find it; the
hidden `test-adapter --misbehave stale-immutable` (every read open of a file
`immutable`, under one `mount_gen`) and `--misbehave frozen-reg-gen` (the
registry generation never moved by a write) do the same for the checks
across steps. These four do it for the checks across clients:

- `--misbehave per-slot-fh`: handles numbered per slot, as a per-connection
  handle table would number them;
- `--misbehave slot-zero-only`: only slot 0 ever taken;
- `--misbehave bank-zero`: every bulk reply in bank 0;
- `--misbehave swap-replies`: two requests in flight together each get the
  other's reply, under their own `ack`.

Each passes every single-client scenario, since one client sending one
request at a time only uses slot 0. `swap-replies` passes every scenario,
since a scenario's steps go one at a time. Each fails the concurrent
clients.

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
