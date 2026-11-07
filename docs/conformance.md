# Director conformance

`aether-conform` checks a director against the wire contract
(`docs/wire.md`). It starts the director under test behind a small adapter
program, has it mount a scenario's tree, attaches to the ring the director
names exactly as the shim would, plays the scenario's requests, and compares
each answer with the one the scenario expects. Scenarios live in
`contract/scenarios/` and their format is described in
`contract/scenarios/README.md`. Through the same adapter, `aether-conform
bench` times a director on the workloads in `contract/workloads/`
("Benchmarks" below).

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
- **What the environment says of the tree**, which
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
  data, and must lie inside the mapping (2.6). An error status must come
  with an empty payload (3.1: "A director answers every error with an empty
  payload"), whatever the step expects.
- **`immutable` on a file a writable provider holds** (3.4.3: "never set
  it for a file a writable provider holds"): an OPEN reply that sets it for
  a path under one of the tree's `writable` entries fails, whatever the step
  expects.
- **What an OK reply holds beyond what the shim decodes.** The shim's file-op
  decoders ignore trailing bytes (3.2, "Lenient vs strict") and it reads
  REG_KEY's children into a map (4.3), so a reply can decode and still not be
  the one wire.md gives. Whatever the step expects, a GETATTR reply is 18
  bytes (3.4.1), an OPEN reply 24 with its reserved bits and bytes zero
  (3.4.3), a WRITE reply 8 with its second word zero (3.4.5), an inline READ
  reply `8 + n` bytes and a bulk one 16, each with its second word zero
  (3.4.4); a READDIR reply ends with its last entry (3.4.2); the opcodes whose
  reply is the status only send no payload; and REG_KEY's children are sorted
  and unique by `folded` (4.3: "sorted by `folded` in byte order", "send them
  sorted and unique").
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
syntax that "Directors may differ on anything else, so a client must not
rely on it", so the suite never sends it and a director may answer it however
it likes.

**Fields the contract leaves to the director.** The rule is equality; these
are the only exceptions, and each is allowed by the wire.md text quoted:

| What | Rule | wire.md |
|---|---|---|
| OPEN `fh` | Paired one to one with the scenario's number while open; requests carry the director's number. After a CLOSE the director's old number is sent, so a use after close tests that handle; a number never paired gets a stand-in no open handle has. Never 0. | 3.4.3: "`fh` is any u64 the director chooses except 0, unique among open handles". |
| OPEN `mount_gen` | Not compared. | 3.4.3: "a generation the director bumps whenever a root's content is replaced", and where a director starts it is its own. |
| OPEN `immutable` | Not compared, but must be clear on a write open, on a directory and on a file under a `writable` entry, and reads through immutable handles are checked against each other ("What the shim caches", above). The reply's other flag bits and bytes 18-19 must be zero. | 3.4.3: "Never set it for a write open or a directory"; "never set it for a file a writable provider holds"; "other bits zero"; 7. |
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

**What a tree means.** A root's own files and directories under a
`writable` entry are held by the writable provider itself, so wire.md 3.4.9's
"names only in a read-only lower" allowances never apply to them, and an
adapter must mount them that way (a director that serves a writable root as
an overlay puts them in the upper). A root's `lower` layers are read-only
layers beneath that upper, the nearest first, every name in them below a
writable entry: an adapter mounts them as its overlay's lowers, and their
names are the ones 3.4.9's allowances are about ("Layered roots" below). A
file or directory outside every `writable` entry belongs to a read-only
provider, and a root with no `writable` entry has none. Scenarios about "the
root itself" (3.4.7, 3.4.8) make the root writable as a whole.

`contract/scenarios/` holds the suite: what every director must do, as data,
one scenario per file, named by its file. Each `description` names the
wire.md section it checks. Where the contract lets directors answer
differently, the step lists the answers it allows (`one_of`) and cites the
section. The reference passes all of it over a file ring and, on Windows,
over a section (the tests and CI run both).

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
| `registry-limits`, `-key-too-large`, `-write-depth` | name, depth and reply limits, checked by every write; `ST_REPLY_TOO_LARGE` (4.1, 4.3) |
| `multi-handle-shared` | a handle opened by one process read, written and closed by another; closed for all once one closes it; a bulk READ from slot 1 in bank 1 (2.6, 2.8.1, 3.4.3) |
| `multi-handles-unique` | three processes' handles, all open at once, unique across them (2.8.1, 3.4.3) |
| `multi-write-visible` | one process's writes, truncation and new file seen by another in GETATTR, READDIR, OPEN and READ (2.8.1, 3.4.1 to 3.4.5) |
| `multi-registry-gen` | one process's registry writes move `reg_gen` in another's mapping and change its answers (2.8.1, 4.3, 4.5) |
| `multi-registry-overlay` | a key one process creates, gives values and renames, another reads under both names and deletes, and the first sees deleted (2.8.1, 4.3, 4.5) |
| `handles-two-on-one-file`, `append-tracks-end`, `regrow-after-truncate` | two handles open on one file: each sees the other's WRITEs and SETATTRs at once, sizes live while both are open, an append lands at the end as it is now, a regrown file reads zeros where a cut was (3.4.1, 3.4.3 to 3.4.6) |
| `handles-deleted-file`, `handles-deleted-file-shared`, `handles-follow-rename` | a handle on a deleted or replaced file never reaches the file later at its name, and two such handles share the file a director keeps; a handle follows its file through a rename, its own or a directory's (3.4.7, 3.4.8) |
| `rename-edges`, `rename-x-edges`, `mkdir-child-rename-parent` | RENAME onto an ancestor, onto itself, a directory onto a file, a missing parent, two writable entries of one root; RENAME_X onto a directory, with a read-only end, in the caller's spelling; a renamed directory's old name made again is empty (3.4.7, 3.4.9, 3.4.13) |
| `open-dirs-and-parents`, `paths-through-files` | write opens of directories, `OPEN_EXCL` and `OPEN_TRUNC` alone, read opens with write bits, creates under a missing parent or a file; paths below a file and READDIR of what is not a directory (3.4.2, 3.4.3, 3.4.9, 3.5) |
| `names-stream-writable`, `case-fold-created` | named streams on a writable path, never stored as entries; names created in non-ASCII case pairs and a name a store may fold onto another (3.2) |
| `write-offset-huge`, `write-read-handle`, `stored-names-edges` | sizes that cannot exist, zero-length WRITEs, READs at huge offsets; WRITE and SETATTR through a read handle; STORED_NAMES with unknown components, `skip` past the end, after a respell and a delete (3.4.4 to 3.4.6, 3.4.12) |
| `registry-delete-unknown-key`, `registry-refused-writes-keep-version` | REG_DELETE_KEY of a key the overlay never held tombstones it; a refused write leaves the version alone (4.3) |
| `write-then-read`, `write-rename-read`, `truncate-then-read`, `extend-then-read-gap`, `bulk-read-sees-writes` | a file written and read back through its writer, another handle and a new one, sizes live before any CLOSE; written, renamed and read under the new name while the old one is gone, two files swapped through a third name, a rename over a file; truncated by an OPEN and by SETATTR under open handles; grown past its end, the gap read as zeros; each change read back in bulk (3.4.1, 3.4.3 to 3.4.7) |
| `rename-dir-open-child`, `rename-dir-case-only`, `rename-x-read-both`, `delete-dir-after-open-child` | a directory renamed with a child open: the child by its new path and not its old, the open handle following it, the old name made again empty; a case-only rename of a directory and of a file, READDIR and STORED_NAMES below it; RENAME_X of a file written and cut through a handle still open, read in both roots, and onto a file with an open handle; a directory whose only file was deleted while open (3.4.7, 3.4.8, 3.4.12, 3.4.13) |
| `handles-fh-zero`, `read-offset-extremes` | `fh` 0, sent literally: CLOSE(0), READ, WRITE and SETATTR of it `ST_BAD_FH`; READs of a read-only file at offsets near 2^32, 2^63 and `u64::MAX` and of `u32::MAX` bytes, inline and in bulk (3.4.3, 3.4.4, 3.4.10) |
| `spelling-after-delete-recreate`, `respell-parent-components`, `delete-and-excl-edges`, `type-change-same-name` | names deleted and created again in another case, file and directory; creating requests (RENAME, MKDIR, OPEN, RENAME_X) with parents spelled otherwise; exclusive creates of names freed by DELETE and RENAME; a directory holding only an empty directory; a name that changes kind (3.2, 3.4.3, 3.4.8, 3.4.13) |
| `cache-content-change-same-size`, `roots-mutations-isolated`, `root-dot`, `root-dot-readonly` | a writable file changed at one size by a rename over it, a delete and create, a truncating OPEN and a RENAME_X, read inline and in bulk each time; DELETE, create, RENAME and MKDIR in one root leaving the other's same paths alone; GETATTR, MKDIR, OPEN and STORED_NAMES of the root `.`, held by a writable provider or not (3.2, 3.4.1, 3.4.3, 3.4.9, 3.4.12) |
| `stage-exec-after-write` | STAGE_EXEC of an image written, and of one created, in the session: a stager's copy holds the bytes as they are now; a directory and a missing path (3.4.14) |
| `overlay-*` | a root served as an overlay ("Layered roots" below): a copy-up read through handles opened before and after it, a lower-only file deleted and created again, renamed, truncated, a lower-only directory that does not move, listings and spellings from both layers, MKDIR over lower names, a written lower file that is the upper's (3.2, 3.4.2 to 3.4.13) |
| `registry-key-lifecycle`, `registry-under-tombstone`, `registry-rename-changed-recreate`, `registry-reads-and-limits`, `registry-value-order` | a key created, given values and a subkey, renamed, looked up and reported changed under both names, deleted, made again empty; writes under a tombstone, a parent of a tombstone, a value deleted from a key with no node; REG_CHANGED around a rename, tombstones moving with it, a rename onto a tombstoned name, `volatile` kept, a rename in place; reads past the write depth, value names counted in UTF-16 units, the current user's hive by the fold, `since` past the version; a value's place and spelling (4.1 to 4.3) |

**Where wire.md lets directors differ**, the step accepts each answer it
names (`one_of`), and nothing later in the scenario depends on which one a
director gave, unless the answers leave the same state:

| Request | Answers accepted | wire.md | Scenarios |
|---|---|---|---|
| READDIR of a missing directory | `ST_NOT_FOUND`, an empty listing | 3.4.2 | `paths-through-files` |
| READDIR of a file | `ST_NOT_A_DIRECTORY`, `ST_NOT_FOUND`, an empty listing | 3.4.2 | `paths-through-files` |
| A path below a file, any opcode but GETATTR | `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` (READDIR as above) | 3.5 | `paths-through-files` |
| A create (OPEN, MKDIR, RENAME or RENAME_X `to`) under a file | `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` | 3.4.3, 3.4.7, 3.4.9, 3.4.13 | `open-dirs-and-parents`, `rename-edges`, `rename-x-edges` |
| A write open of a directory without `OPEN_EXCL` | `ST_IS_DIR`, `ST_IO_ERROR` | 3.4.3 | `open-dirs-and-parents` |
| A read open with `OPEN_TRUNC` or `OPEN_CREATE` | the bit ignored, `ST_BAD_REQUEST` | 3.4.3 | `open-dirs-and-parents` |
| A file renamed onto an empty directory | `ST_OK` (replaced), `ST_IO_ERROR` | 3.4.7 | `rename-edges`, `handles-follow-rename` |
| RENAME_X onto a directory | `ST_IS_DIR`, `ST_IO_ERROR` | 3.4.13 | `rename-x-edges` |
| READ, WRITE or SETATTR through a handle whose file was deleted or replaced | the kept file's bytes (shared by its open handles, reached by no name), `ST_IO_ERROR`, `ST_NOT_FOUND` | 3.4.8 | `handles-deleted-file`, `handles-deleted-file-shared`, `handles-follow-rename` |
| A WRITE or SETATTR to a size that cannot exist | `ST_NO_SPACE`, `ST_BAD_REQUEST` | 3.4.5, 3.4.6 | `write-offset-huge` |
| WRITE through a handle opened without `OPEN_WRITE`, on a file a writable provider holds | honoured, `ST_READ_ONLY` | 3.4.5 | `write-read-handle` |
| MKDIR of an existing directory no writable provider serves | `ST_EXISTS`, `ST_READ_ONLY` | 3.4.9 | `mkdir-existing-readonly` |
| STORED_NAMES with `skip` at or past the number of components | an empty reply, `ST_BAD_REQUEST` | 3.4.12 | `stored-names-edges` |
| A request naming a named stream | `ST_NOT_FOUND`, `ST_BAD_REQUEST`, or the stream a director holds | 3.2 | `names-stream`, `names-stream-writable` |
| A create of a name a director's store would make one with another | created, `ST_EXISTS` | 3.2 | `case-fold-created` |
| MKDIR of a directory only a lower layer holds | `ST_OK`, `ST_EXISTS` | 3.4.9 | `overlay-mkdir` |
| DELETE of a lower-only directory with children; RENAME of a lower-only directory to another name; a case-only RENAME of a lower-only name; MKDIR over a lower-only file | 3.4.9's list's answer, or the general rules' (`ST_IO_ERROR`, moved whole, respelled, `ST_IO_ERROR`), later steps sharing a `choice` | 3.4.9 | `overlay-delete-dir`, `overlay-rename-dir`, `overlay-spelling`, `overlay-mkdir`, `overlay-written-file` |
| A case-only RENAME of a directory the upper and a lower both hold | `ST_OK` (respelled), `ST_IO_ERROR` | 3.4.7 | `overlay-rename-dir` |
| RENAME of a directory the upper and a lower both hold | `ST_IO_ERROR` with nothing moved, or the whole of it moved; each step that shows the outcome shares one `choice` | 3.4.7 | `overlay-rename-both` |
| READ through a handle on a lower-only file that a DELETE, a RENAME onto it or a MKDIR over it hid | the hidden file's last bytes, `ST_IO_ERROR`, `ST_NOT_FOUND` | 3.4.8 | `overlay-delete-recreate`, `overlay-rename-file`, `overlay-mkdir`, `overlay-delete-dir` |

GETATTR of a path below a file is found = 0 (or `ST_NOT_FOUND`, which the
shim reads the same, 3.4.1), never `ST_NOT_A_DIRECTORY` (3.5).

**Left out because wire.md does not fix the answer**, so no scenario
asserts it: a write open, DELETE or RENAME of a missing path outside every
writable provider in a root that has one; and, in a layered root, the
question "Layered roots" below lists as left to the director. (What a write past the end, or a
SETATTR, puts in the gap is zeros, and a handle opened for writing alone
serves READ: wire.md 3.4.4 to 3.4.6 say so, and `write-growth` and
`write-only-read` check them.)

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

## Layered roots

A tree's root may name `lower` layers (`contract/scenarios/README.md`,
"Tree"): read-only layers beneath its writable upper, the nearest first, as a
director serving a writable root as an overlay has them (wire.md 3.4.9,
"Names only in a read-only lower"). The reference serves such a root as an
overlay, the scenarios `overlay-*` play its rules, the generator makes
layered roots in about half its cases and predicts their answers, and a run
fails a director that breaks them. A director that serves no root as an
overlay says so ("The adapter protocol"), and the run then skips the layered
scenarios and generates no layered root.

**What wire.md fixes, and where.** The upper is over the lower, so at a name
both hold the upper's entry is the one served, its kind, size, bytes and
spelling; between layers the tree's order decides, and a file hides what
farther layers hold below its name, a directory over a file hides the file.
Every rule below is checked by the scenario named and by generated cases.

| Rule | wire.md | Scenarios |
|---|---|---|
| One listing of the upper and the layers, each name once, as the highest layer holding it has it; a name a DELETE or RENAME hid is not listed | 3.2, 3.4.2, 3.4.9 | `overlay-listing`, `overlay-delete-recreate`, `overlay-delete-dir` |
| A name keeps the spelling of the layer that shows it; a create keeps its parents' stored spelling, a lower's too | 3.2, 3.4.12 | `overlay-spelling` |
| A read open of a lower-only file may be immutable; once a write open of the file (or a RENAME or MKDIR onto its name) has succeeded, no read open of it is. The run checks it: `immutable` on a file a writable provider holds fails unless the tree's layers alone hold it and no write has reached it yet | 3.4.3, 3.4.9 | every `overlay-*` OPEN, by the run's immutable check |
| A copy-up keeps a file's identity: a write open of a lower-only file opens it with its bytes, and every handle of it opened without `immutable`, those opened before the copy-up included, reads WRITEs, SETATTRs and truncating OPENs at once; an immutable handle keeps the bytes it had. The OPEN and the READs after a change share a `choice`, so a handle reads as its OPEN said it would | 3.4.3, 3.4.4, 3.4.6, 3.4.9 | `overlay-copy-up-handles`, `overlay-truncate`, `overlay-rename-file` |
| DELETE of a lower-only file hides it (nothing a farther layer holds there shows); DELETE of a lower-only directory with children hides it, or is `ST_IO_ERROR` by the general rule; a DELETE of a name the upper holds leaves it missing, the lower's entry under it included | 3.4.8, 3.4.9 | `overlay-delete-recreate`, `overlay-delete-dir`, `overlay-listing` |
| A name made where a DELETE hid one is new: an empty file, an empty directory; a handle on the hidden file never reaches it | 3.4.8, 3.4.9 | `overlay-delete-recreate`, `overlay-delete-dir` |
| RENAME of a lower-only file copies it up and renames it; its handles follow it | 3.4.7, 3.4.9 | `overlay-rename-file` |
| RENAME of a lower-only directory to another name is `ST_IO_ERROR`, or by the general rule moves it whole; to another case of itself `ST_OK`, the lower's spelling kept or (the general rule) respelled, for a file too | 3.4.7, 3.4.9 | `overlay-rename-dir`, `overlay-spelling`, `overlay-written-file` |
| RENAME onto a lower-only file replaces it; RENAME_X of a lower-only file moves it, of a lower-only directory is `ST_CROSS_ROOT` | 3.4.7, 3.4.13 | `overlay-rename-file`, `overlay-rename-dir` |
| RENAME of a directory the upper and a lower both hold is `ST_IO_ERROR` with nothing moved, or moves the whole of it, every name under it from every layer; never part of it. A case-only RENAME of it respells it, or is refused | 3.4.7 | `overlay-rename-both`, `overlay-rename-dir` |
| MKDIR of a directory only a lower holds is `ST_OK` or `ST_EXISTS`, and its listing is unchanged | 3.4.9 | `overlay-mkdir` |
| MKDIR over a lower-only file makes the directory, which hides the file, or by the general rule is `ST_IO_ERROR` | 3.4.9 | `overlay-mkdir`, `overlay-written-file` |
| A lower-only file a WRITE or a truncating OPEN changed is the upper's (the lower is read-only): MKDIR over it is `ST_IO_ERROR`, a case-only RENAME respells it | 3.4.7, 3.4.9 | `overlay-written-file` |
| DELETE of a directory the upper holds with a lower's names under it is `ST_IO_ERROR` | 3.4.8 | `overlay-delete-dir` |

**Where wire.md lets an overlay answer otherwise**, the step accepts every
answer. 3.4.9 says an overlay "may" answer a lower-only name by its list, and
the answers the general rules give stay allowed, so for each of four
requests on a lower-only name either is accepted: DELETE of a directory with
children (hidden, or `ST_IO_ERROR`), RENAME of a directory to another name
(`ST_IO_ERROR`, or moved whole), RENAME to another case of itself (the
lower's spelling kept, or respelled) and MKDIR over a file (the directory
made, or `ST_IO_ERROR`). MKDIR of a directory only a lower holds is `ST_OK`
or `ST_EXISTS` (3.4.9). RENAME of a directory the upper and a lower both hold
is `ST_IO_ERROR` or the whole move (3.4.7), and a case-only RENAME of it is
respelled or refused. A handle on a lower-only file that was hidden answers
as 3.4.8 says of a deleted file. Where the answers leave different trees, the
step and every later step whose answer depends on it share a `choice`, so a
director must keep to the answer it gave (and one that moves part of a
directory fails `overlay-rename-both`); a generated case ends after such a
step. The `one_of` table above lists them. The reference answers by 3.4.9's
list, `ST_EXISTS` and `ST_IO_ERROR`; the test-only adapter modes
`lower-general-rules` (the general rules, and whole moves) and
`refuse-layered-dir-renames` (every RENAME of a directory both layers hold
refused) pass the suite and generated cases.

**Where wire.md is silent.** One layered question is left to the director,
and the reference's answer to it is its own choice: *when is a lower-only
name in the upper?* A file a write open reaches, unchanged, and a directory
below which a name is made are lower-only still, or the upper's. The
reference copies a file up at the write open, and puts a directory, and every
lower-only directory above it, in the upper when a name is made below it. Its
tests (`overlay`, the ones named `the_reference_...`) say so. No scenario or
generated case sends a request whose answer depends on it. It decides only
what DELETE of the directory and a case-only RENAME of either answer later.

**Generated cases** make roots with one or two layers below the writable
part (a root writable as a whole, or a writable entry), their names drawn
from the same list, so a name is often in the upper and a layer, or in both
layers, in another case. The generator keeps its own account of the layers,
apart from the reference: which names the upper holds, which a DELETE or
RENAME hid, and which the question left to the director leaves open. From it it predicts each
answer that depends on the layers (the statuses above), the bytes a READ
through any live handle gets (a lower-only file's from its layer, a handle
opened before a copy-up the file as it is now), the spelling of each
STORED_NAMES component only a layer holds, and, after every step, every
listing: each name the reference lists is the upper's or one a layer shows,
with the layer's spelling, kind and size, and each name only a layer shows is
listed. A reference that answers otherwise is a generator error. It does not
send DELETE, case-only RENAME or MKDIR of a name the question left to the
director leaves open, RENAME of a directory a layer holds too to another
name (the scenario `overlay-rename-both` plays it), or anything after a
MKDIR of a lower-only directory that depends on the answer it chose; a
request wire.md lets an overlay answer two ways that leave different trees
(above) is the case's last step.

**Proof it bites.** Test-only breaks of the reference adapter each make one
mistake a layered director plausibly makes, and the tests show the scenario
of its rule and the generator both fail it: `no-whiteout` (a deleted lower
name found again), `lists-hidden` (one listed again), `write-through` (a
written lower file still lower-only), `copy-up-loses-bytes`,
`copy-up-old-handle-lower` (a handle opened before the copy-up reads the
lower), `hidden-handle-reads-new` (the reverse: a hidden lower file's handle
reads the file made at its name later), `upper-loses-listing`,
`copy-up-opaque-parent` (a name made below a lower directory makes it
opaque, losing the lower's names there), `immutable-after-copy-up` (a read
open still immutable after a write open of the file) and
`immutable-sees-new-bytes` (an immutable handle that reads the copy-up's
new bytes). `copy-up-old-handle-lower` now breaks the handles opened
without `immutable`. The adapter mode `mutable-lower-reads` (no read open of
a lower-only file immutable) passes the suite and the generated cases.

**Immutable handles in generated cases.** The reference opens a lower-only
file immutable, and the generator predicts it (and predicts no other read
open of a file the writable provider holds is). Such an OPEN's step is a
`one_of` of the reply with and without `immutable`, labelled by its handle;
a READ through the handle once its bytes and the file's differ is a `one_of`
of the handle's own bytes and the file as it is now, with the same label.
The read-cache check still holds: an immutable handle keeps its bytes, and
no read open after a write of the file is immutable.

## Several clients

A step may carry `"client": n` (default 0, `contract/scenarios/README.md`).
The run attaches one client per number a scenario names. Client 0 is the
run itself. Every other client is a process of its own, `aether-conform
client-worker`:

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
CLOSE for its open handles"), so a test of `aether-conform` does it, on a
file ring and on a section. It kills a client process holding open handles, then
checks:

- the director goes on answering this process and a new client process;
- the new client reads through a handle the killed one left open (the
  director keeps them until the session ends);
- new handles do not collide with the old ones.

## Concurrent clients

`run --concurrent N` plays `--concurrent-cases C` rounds (default 64) of N
clients at once. The rounds come
from `--seed` (as `--generate`'s cases do, and the same seed if both are
given) and have at most `--steps` operations per client.

**A round.** Each client `k` gets a generated case ("Generated cases" below)
limited to one root with a writable provider and no registry layer. The
overlay's version is one counter for the whole session, so one client's
registry writes would change another's answers; the case's last step, its
probe of the registry layer it lacks, is left out, since the combined tree
has a layer for the meetings (below). The case's tree and paths
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
write OPENs) stay below 2.8's limit for the ring's `worker_hint` and slot
count, as the shim keeps them. A case's steps that present a closed handle, or a
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

Before the sweep, the clients meet: requests on one name, file or key, in
flight together, each from a slot of its own (data requests below the data
limit), whose outcome wire.md 2.8.1 makes predictable, since each request
takes effect atomically. Each round's meetings are in a directory of its
own, `meet/r<round>/`, made at the round's start in the writable directory
`meet/` of root 0 (so a listing holds that round's names only, however many
rounds came before), or below a key of the registry layer the combined tree
has for them (the cases' own trees have none):

| Meeting | What must hold (2.8.1) |
|---|---|
| `OPEN` with `write`, `create`, `excl` of one new name | exactly one handle, the rest `ST_EXISTS` |
| MKDIR of one name | each `ST_OK` or `ST_EXISTS`, at least one `ST_OK`, and a directory there after |
| REG_CREATE_KEY of one key | exactly one `ST_OK`, the rest `ST_EXISTS` |
| WRITEs to disjoint, unaligned ranges of one file, each through a handle of its own | every one `written` whole, and the file read back inline and in bulk holds them all |
| append WRITEs of records, each tagged with its client and number, in waves | the file is the records' total length, and every record is whole |
| RENAME, then DELETE, of a file with a GETATTR, an OPEN and a READDIR of its names | each lookup the file whole or not there, and the listing never both names or neither |
| a create (`write`, `create`) and a MKDIR of one new name | exactly one succeeds, GETATTR finds the winner's kind, and the round's listing has the name once |
| a create of `d/x` and a RENAME of the directory `d` to `e` | the RENAME `ST_OK`; the create `ST_OK` with `e/x` there, or `ST_NOT_FOUND` with `x` nowhere; never `d` or `d/x` after |

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

`run --limits` mounts an empty tree
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
| 4 and then up to 8 bulk READs of one handle (one slot kept free), and 4 inline READs of 65 535 bytes, each set published at once at offsets out of order, held until all are `COMPLETED`, another READ answered meanwhile, and only then copied out, as the shim's pipelined read does | each its own offset's bytes, in its own slot's bank | 2.5, "Order of service" (a pipelined read publishes up to 4 requests at once, 8 for reads of 4 MiB or more); 2.6: the bank is the client's while the slot is `COMPLETED` |
| A bulk READ given up (`abandon`) the moment the director takes it, tried up to 100 times until it is caught `PROCESSING` | the slot ends `FREE`, never `COMPLETED`; then a HEARTBEAT from every slot is answered. A director never caught processing is not failed: the step says the rule was not exercised | 2.5: "a director must never store `COMPLETED` over `ABANDONED`" |
| 300 REG_SET_VALUEs of one value from one client while another reads it with REG_KEY, reading `reg_gen` before each read | a read made at a generation a write's reply had already published sees that write (the writer reads `reg_gen` after each reply, which is at least the generation its write published) | 4.5: it moves "before the write's reply is published", and the shim caches an answer under the generation it read before asking. A director with no registry layer skips it |
| `worker_hint` | a warning, never a failure, when it is 0 or above the slot count | 2.8 |
| 200 HEARTBEATs one after another | the median and slowest round trip, a warning only | 2.11 |

## Malformed requests

`run --malformed` mounts two built-in trees in turn. The file cases go to
the file tree (root 0 with `data/a.txt` = `abc` and
the writable entry `saves` holding `saves/s.txt`, root 1 with `r.txt`), which
has no registry layer, since a director need not have one (wire.md 4); for
them `data/a.txt` is opened for reading and `saves/s.txt` for writing. The
registry cases go to the registry tree, the same roots and a registry layer
with `\Registry\Machine\Software\Conform` and its value `v`. Each case is
sent as raw slot fields in a slot the run holds (or, for the header case,
a header written by hand). Each case's payload is a valid request with
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
| GETATTR cut to 0 to 3 bytes | `ST_BAD_REQUEST`, or OK with `found = 0` | 3.5.1's table |
| READDIR cut to 0 to 3 bytes | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | 3.5.1 |
| OPEN cut to 0 to 7 bytes | `ST_BAD_REQUEST` | 3.5.1 |
| READ cut to 0 to 19, WRITE to 0 to 23, SETATTR to 0 to 15, CLOSE to 0 to 7 bytes | `ST_BAD_REQUEST` | 3.5.1 (READ, WRITE, SETATTR, CLOSE, short: `ST_BAD_REQUEST`); 3.2 for WRITE's pad |
| RENAME cut to 0 to 7, RENAME_X to 0 to 11 bytes; either with a `from_len` past the payload | `ST_BAD_REQUEST` | 3.5.1 |
| DELETE cut to 0 to 3, MKDIR to 0 to 7, STORED_NAMES to 0 to 7 bytes | `ST_BAD_REQUEST` or `ST_NOT_FOUND` | 3.5.1 |
| WRITE whose `len` is 10 with 3 data bytes | `ST_BAD_REQUEST` | 3.4.5: "A request whose payload is shorter than `24 + len` is `ST_BAD_REQUEST`" |
| GETATTR, OPEN, RENAME (`from`) of a missing path that is not UTF-8 | `ST_BAD_REQUEST`, or the missing-path answer (found = 0, `ST_NOT_FOUND`) | 3.5.1: "`ST_BAD_REQUEST`, or a director may instead decode it with replacement characters and carry on" |
| REG_LOOKUP of a key path that is not UTF-8 | `ST_BAD_REQUEST`, or the reference's answer for the path with replacement characters | 3.5.1: "Invalid UTF-8 in a path is `ST_BAD_REQUEST`, or a director may instead decode it with replacement characters and carry on", key paths included |
| REG_CREATE_KEY with `volatile` 2 and 255; REG_CHANGED with `subtree` 2 | `ST_BAD_REQUEST` | 4.1: "Booleans are one byte, exactly 0 or 1" |
| Each of the eight registry requests with a trailing byte | `ST_BAD_REQUEST` | 4.1: "Every registry decoder consumes its input exactly; trailing bytes are `ST_BAD_REQUEST`" |
| Each registry request cut by a byte; a REG_LOOKUP whose `len` runs past the payload | `ST_BAD_REQUEST` | 3.2 (truncation), 3.5 ("The payload did not decode") |
| GETATTR whose slot header claims `payload_cap + 1` and `0xffffffff` bytes | `ST_BAD_REQUEST`, payload length 0 | 2.5: "A slot whose `payload_len > payload_cap` MUST be completed with `ST_BAD_REQUEST` and payload length 0" |

Not sent: a short STAGE_EXEC (3.5.1 names no answer for it), trailing bytes
after a file request's fixed fields (3.2 describes the file-op decoders, which
ignore them, not a rule for directors), a HEARTBEAT with a payload, and
registry cases to a tree without a registry layer.

## Generated cases

`run --generate` plays `--cases N` generated cases (default 64), each a tree
and up to `--steps M` operations (default 32), all made from one seed
(`--seed S`; without it the seed comes from the clock). The seed is printed on
stderr, in the TAP report (`# seed S`) and in the JSON report's `seed`, and the
same seed gives the same cases, so `--seed S` replays a run.

**How a case is made.** proptest
generates a tree spec (up to three roots, files and directories with names
from a fixed list in varied case, writable entries or roots writable as a
whole, now and then two writable entries in one root, an optional registry
layer, and in about half the roots with a writable provider one or two
read-only lower layers beneath it, "Layered roots" above), a list of
operations, each a kind and five numbers, and whether
clients other than client 0 send some of the steps. The generator builds the
tree, mounts it in the reference director held in this process (behind a
ring in memory, served as a director serves a ring), and reads the
operations one at a time: each one's numbers pick its target among what
exists at that point (an open handle, an existing file, a missing name under
an existing directory, a key) and its arguments. An operation with nothing to
act on is dropped, so every case proptest makes, and every case it shrinks
to, is a valid one. The reference's answer to each request is that step's
expectation, or, where wire.md allows several answers, a `one_of` of exactly
those with the section cited. The generator also says, for each request,
which answer wire.md gives; a reference that answers otherwise is reported as
a generator error (exit 2) naming the section, never made an expectation.

- **Locality.** About half the time an operation picks its name or handle
  among the ones the last steps touched, the latest first: so a write, a
  rename and a read of one file, or a delete and a create of one name, are
  common rather than rare. Some operations are templates, sequences on the
  same names and handles: a save (a new file written, renamed over a file
  with its handle open, written again, closed and read back); a file deleted
  under its handle and created again; two files swapped through a third name
  with a handle open on each; a truncating open under a writer; a directory
  renamed with a file open below it; a file cut and grown again; a directory
  renamed away and made again.
- **Handles.** Each open handle holds its file, whatever it is called: a
  RENAME, or a RENAME of a directory above it, moves the file under its
  handles (3.4.7). While its file is there, a handle is read, written and
  resized freely, and each answer is the reference's, which serves every
  handle the file as it is now (3.4.4): two handles reading and writing in
  turn, an append after another handle's write, a write after another
  handle's truncate. Once its file is deleted, or replaced by a RENAME onto
  its name (3.4.8), a READ through it expects `one_of` the bytes the file had
  then, `ST_IO_ERROR`, `ST_NOT_FOUND`; a WRITE or SETATTR, `one_of` `ST_OK`,
  `ST_IO_ERROR`, `ST_NOT_FOUND`, after which it is only closed (what it
  reads then depends on which). A handle on a file a RENAME_X moved to
  another root may follow it or answer as for a deleted file (3.4.7), so it
  is read (`one_of` as for a deleted file) until anything changes the moved
  file, and never written. A later request on the name checks that nothing
  such a handle did reached the file there now.
- **Clients.** In about one case in ten, steps are sent by clients 1 and 2
  too, processes of the session ("Several clients" above), so handles opened
  by one are presented by another (2.8.1).
- **Steps after a step.** A step whose answer leaves a file as it was (a
  refused RENAME, a WRITE or SETATTR to a size no director holds, a request
  naming a named stream) is followed by a GETATTR of the file. A file renamed
  onto an empty directory beside it may be replaced or refused (3.4.7): the
  RENAME is sent again (`one_of` `ST_NOT_FOUND`, if the first moved it, and
  `ST_IO_ERROR`, if it refused again), then their directory is listed
  (`one_of` the listing after the move, the listing before it), and the case
  ends there, since which it was decides the tree.
- **The last step.** A case with a registry layer ends with a REG_DELETE_KEY
  of a key no layer holds, which tombstones it (4.3); one without, with a
  registry request answered `ST_NOT_SUPPORTED` (4), and its operations of
  registry kinds stand for file kinds.

The case is then an ordinary scenario: it is written to a scratch file,
mounted through the adapter, and played and compared exactly as a scenario
file is, so every comparison rule above applies.

**Shrinking and replay.** The first case that fails is shrunk the way proptest
shrinks (simplify while it still fails, complicate when it passes), each
candidate played against the director, up to 2048 candidates. Then each
template is taken apart into its operations, and operations are dropped one
at a time, and the other clients, while the case still fails, which
proptest's shrinking of a template as one operation cannot do. A candidate
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
| GETATTR | existing files and directories (root included) in any case spelling; missing names under existing directories; paths below a file; a named stream of a file (`a.esp:s`); unserved roots | the reference's, found = 0 for missing paths, paths below a file (3.5), streams no request made (3.2) and unserved roots (3.4.1) |
| READDIR | existing directories in any spelling; missing directories; files and paths below them; unserved roots | the listing (3.4.2, compared as a multiset); `one_of` an empty listing, `ST_NOT_FOUND` (and `ST_NOT_A_DIRECTORY` for a file or below one) (3.4.2, 3.5); `ST_NOT_FOUND` |
| OPEN, reading (`read`, or `oflags == 0`) | existing files and directories; missing names; paths below a file; a named stream; unserved roots | a handle (3.4.3); `ST_NOT_FOUND`; `one_of` `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` (3.5); `one_of` `ST_NOT_FOUND`, `ST_BAD_REQUEST`, then a GETATTR of the file (3.2) |
| OPEN, writing (`write` with any of `read`, `create`, `excl`, `trunc`, `append`) | files the writable provider holds; missing names it would hold; files a read-only provider holds; missing names in a root with no writable provider; directories; names in a missing directory or below a file | a handle, or `ST_EXISTS` with `excl`, or `ST_NOT_FOUND` without `create` (3.4.3); `ST_READ_ONLY` (3.4.3, 3.5); for a directory `ST_EXISTS` with `excl`, else `one_of` `ST_IS_DIR`, `ST_IO_ERROR`, and `ST_READ_ONLY` where no writable provider holds it (3.4.3); `ST_NOT_FOUND`, and below a file `one_of` `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` (3.4.3) |
| READ | every file handle, whatever access it was opened with (3.4.4), at offsets 0, odd, inside, at and past the end, in the zeros a growth left, and far past any end (2^32, 2^63, `u64::MAX` and just below), lengths 0, 1, odd, to the end, 4096 and 65536 to 70000, and `u32::MAX` in bulk where little is left, with and without `read_bulk`; handles whose file is gone; directory handles | the data (3.4.4, inline or bulk); `one_of` the file's last bytes, `ST_IO_ERROR`, `ST_NOT_FOUND` (3.4.8); `ST_IS_DIR` |
| WRITE | handles opened for writing, at offsets up to the end and up to 70000 bytes past it (any offset on an `append` handle), 0 to 20000 bytes, and with an end past 2^63 or `u64::MAX`; handles whose file is gone; directory handles | `written` (3.4.5, the gap reading as zeros); `one_of` `ST_NO_SPACE`, `ST_BAD_REQUEST`, then a GETATTR (3.4.5); `one_of` `written`, `ST_IO_ERROR`, `ST_NOT_FOUND` (3.4.8); `ST_IS_DIR` on a directory the writable provider holds, else `one_of` `ST_IS_DIR`, `ST_READ_ONLY` (3.4.5) |
| SETATTR | handles opened for writing: 0, half, the size, past it by up to 70000 bytes, 2^63 and `u64::MAX`; handles whose file is gone; directory handles | `ST_OK` (3.4.6, an extension reading as zeros); `one_of` `ST_NO_SPACE`, `ST_BAD_REQUEST`, then a GETATTR (3.4.6); as WRITE for the others |
| CLOSE | open handles, also ones whose file is gone | `ST_OK` (3.4.10) |
| READ, WRITE, SETATTR, CLOSE | closed handles and numbers never issued | `ST_BAD_FH` (3.5) |
| RENAME, and RENAME_X with equal roots | files and directories moved, onto existing files, directories onto empty and non-empty directories, a file onto an empty directory beside it, respelled (same fold), to the same name in its stored spelling, into themselves, onto an ancestor, from missing names, from or to read-only names, to a name in a missing directory or below a file, the root at an end (root writable as a whole); unserved roots | 3.4.7's answers (a directory onto a file and a `to` that is an ancestor `ST_IO_ERROR`, the same name `ST_OK`, a missing parent `ST_NOT_FOUND`, below a file `one_of` `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY`), a file onto an empty directory `one_of` `ST_OK`, `ST_IO_ERROR` (with the steps after it above), the root at an end and a `to` inside `from` as `one_of` `ST_BAD_REQUEST`, `ST_IO_ERROR`; `one_of` `ST_NOT_FOUND`, `ST_BAD_REQUEST` (3.5.1) |
| RENAME_X across roots | files to a missing name or onto a file, in the caller's spelling (which the file at `to` takes, 3.4.13); onto a directory; from or to a read-only name; directories; missing names; an unserved root at one end | `ST_OK`; `one_of` `ST_IS_DIR`, `ST_IO_ERROR`; `ST_READ_ONLY`, each refusal then a GETATTR of `from`; `ST_CROSS_ROOT`; `ST_NOT_FOUND` (3.4.13); `one_of` as above |
| DELETE | files, empty and non-empty directories the writable provider holds; missing names; paths below a file; a named stream; read-only names; the root (writable as a whole); unserved roots | 3.4.8's answers, below a file `one_of` `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` (3.5), a stream `one_of` `ST_NOT_FOUND`, `ST_BAD_REQUEST` then a GETATTR of the file (3.2), the root as `one_of` `ST_BAD_REQUEST`, `ST_IO_ERROR` |
| MKDIR | missing names, their parents respelled; existing directories, files the writable provider holds, files a read-only one holds; names in a missing directory or below a file; missing names in a root with no writable provider; unserved roots | `ST_OK`, the parents keeping their stored spelling (3.2); `one_of` `ST_OK`, `ST_EXISTS`; `ST_IO_ERROR`; `ST_EXISTS` (3.4.9); `ST_NOT_FOUND`, below a file `one_of` `ST_NOT_FOUND`, `ST_NOT_A_DIRECTORY` (3.4.9); `ST_READ_ONLY` (3.5); `ST_NOT_FOUND` (3.5.1) |
| STORED_NAMES | existing names in any spelling, `skip` below the number of components, and `skip` of every component and more; missing names; unserved roots | the stored spellings; `one_of` an empty reply, `ST_BAD_REQUEST`; `one_of` the names as sent, `ST_NOT_FOUND` (3.4.12); `ST_NOT_FOUND` (3.5.1) |
| STAGE_EXEC | existing files | `one_of` `ST_NOT_SUPPORTED`, `ST_NOT_FOUND` (a stager that does not stage it), a path (3.4.14) |
| The eight registry requests | keys of the tree, keys created or renamed since, keys a REG_DELETE_KEY tombstoned and keys below them, in any spelling below the hive, under a base key in each of `\Registry\Machine`, `\Registry\User\<CurrentUser>` and `\Registry\User\<CurrentUser>_Classes` (4.2), values from a fixed list, REG_CHANGED `since` a version an earlier reply gave, the one after the latest, or 0; non-canonical key paths; `.` and `..` as a new leaf; a REG_DELETE_KEY of a key no layer holds, in every case with a registry layer; one registry request in every case without one | the overlay's answers (4.3); `ST_BAD_REQUEST` (4.2, 4.3); `ST_NOT_SUPPORTED` (4) |

A create sends its parents respelled; the name keeps the caller's spelling
and the parents their stored one (3.2).

**What is not generated, and why:**

- Names whose answer depends on how a director layers its providers: a
  writable entry's own directory and its ancestors (an entry inside another
  included) are only read (GETATTR, READDIR, OPEN for reading), never
  written, deleted or renamed (README, "Who holds what").
- In a layered root, the requests whose answer depends on a question left to
  the director ("Layered roots" above).
- Writes with no fixed answer (as "Left out" above): a write open, DELETE
  or RENAME of a missing name outside every writable provider in a root that
  has one.
- WRITE and SETATTR through a handle opened without `write` (3.4.5 lets a
  director honour them or answer `ST_READ_ONLY`, and the steps after would
  depend on which; `write-read-handle` checks them).
- A request that creates a named stream (a director that holds streams
  creates it, one that does not refuses, and the steps after would depend
  on which; `names-stream-writable` checks it). Fold pairs a director's
  store may fold otherwise (3.2, `case-fold-created`).
- After a WRITE or SETATTR through a handle whose file is gone, READs
  through it (a director that keeps the file's last bytes has changed them,
  one that fails has not).
- Directory handles whose directory, or a directory above it, was renamed or
  deleted are only closed: wire.md says nothing of their answers.
- Paths that leave their root (`escape-paths` covers them) and every
  non-canonical file path (3.2).
- Registry requests near a limit (reply size, name length, depth), which
  `registry-limits` and `registry-key-too-large` cover, and a REG_CHANGED
  before any registry reply (its `since` has no numbering yet).
- Sizes a director may or may not hold: WRITE and SETATTR sizes are far
  below any cap, or past 2^63, which no file system holds.
- File names a Windows file system would refuse or compare differently from
  the wire's fold (no final sigma, no dotted capital I), so a director that
  stores names on disk is not failed for its file system.

Three rules the generator relies on are wire.md sentences: a director writes
every byte of a WRITE or fails it (3.4.5); GETATTR, READDIR and OPEN report a
file's size as it is now, writes through handles still open included
(3.4.1); a file with open handles is deleted or renamed like any other
(3.4.7, 3.4.8). No other assumption beyond wire.md remains.

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
    either may carry "layers":false (the director serves no layered roots)
    and "registry":false (the director serves no registry)
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
  like `aether-conform-stale-VFS_RING_SECTION` and `7`). A mount answer that holds one passed it
  on, as a launch built on the director's own environment would.
- A director that serves no root as an overlay (no `lower` layers,
  `contract/scenarios/README.md`) says so in its mount answers with
  `"layers": false` beside `ok`, and refuses (`"ok":false`) the mount of a
  tree that has lower layers; it never mounts one and ignores the layers.
  Without the field, a director serves them (`"layers": true`). Once a mount
  answer has said `false`, the run reports each scenario whose tree has
  lower layers as skipped (TAP `# SKIP`, JSON `"skipped"` with the reason,
  `"passed": false`, failing nothing) without mounting it, and the generated
  cases and concurrent rounds, which learn it from their first mount of an
  empty tree, make no layered root. A skipped scenario is not a passed one:
  such a director has not shown it serves overlays.
- A director that serves no registry (wire.md 4) says so in its mount
  answers with `"registry": false` beside `ok`, and refuses (`"ok":false`)
  the mount of a tree that has a registry layer. Without the field, a
  director serves one (`"registry": true`). It works as `"layers"` does:
  once a mount answer has said `false`, the run reports each scenario that
  has a registry layer or sends a registry request as skipped (TAP `# SKIP`,
  JSON `"skipped"` with the reason, `"passed": false`, failing nothing)
  without mounting it. Generated cases and concurrent rounds, which learn it
  from their first mount of an empty tree, have no registry layer and send
  no registry request, the probe of an absent layer included, and the
  concurrent meetings leave out the REG_CREATE_KEYs. `--malformed` leaves
  out its registry cases and `--limits` its registry writer against a
  reader, each with a step that says it was not exercised. No session serves
  a registry, so the `reg_gen` rules (wire.md 4.5) are not checked. A
  skipped scenario is not a passed one: such a director has not shown it
  serves a registry.
- `unmount` lets the mounted tree go. The run has detached from the ring
  before it sends this, so the adapter may remove a ring file.
- EOF on stdin means the run is over: the adapter should exit. It is killed
  if it has not exited within the adapter timeout.
- Whenever the run kills the adapter (a timeout, an error, the end of the
  run), it kills every process the adapter started too, so an adapter may be
  a launcher (a `.cmd` file, a shell script, `java`) that starts the real
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

Every answer must come within the adapter timeout (`--adapter-timeout`, 60
seconds by default), which is also how long the adapter has to exit after
EOF. It is not `--timeout`, which bounds the ring's requests: a mount builds
a tree, and a short `--timeout` (to see a hang quickly) must not fail a
mount on a loaded machine. An adapter that crashes, closes its stdout, writes a line that is
not a JSON object of the right shape, refuses an unmount, or does not answer
in time fails the scenario being run and ends the run, with exit code 2.

`VFS_RING_PATH` is the path as the conformance client sees it. A native
director gives its host path; a director that serves a shim under Wine still
gives the host path here, since `aether-conform` runs natively.

### Attaching

The run attaches the way the shim does:
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
| `--timeout SECS` | How long every ring request may take. Default 30. |
| `--adapter-timeout SECS` | How long each adapter answer (mount, unmount) may take, and the adapter's exit after EOF. Default 60, whatever `--timeout` is. `soak` takes it too. |
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
(`--no-arena`), on Windows. Measured on a 16-thread Windows machine with a
debug build: the concurrent rounds take about 16 s per ring; 1024 generated
cases about 30 s over a file ring (each mount creates a 16 MiB ring file)
and 25 s over a section, of which the cases with client processes are a
good part; each whole invocation about 55 s over a file ring, 55 s over a
section, and 11 s with no arena (seed 20261105, the 143 scenarios with the
layered ones). At two to three times slower on CI's
2-vCPU runners that is about five minutes for the Windows step.

`aether-conform reference-adapter [--ring file|section] [--no-arena]` is an
adapter for the reference director, served on four threads that share one
reference, with `worker_hint` 4. Its ring has a
bulk arena of 1 MiB per slot, or, with `--no-arena`, none (`VFS_ARENA_LEN`
unset, every READ answered inline). It is what a run is
checked against in this repository's tests, and an example of what an
adapter does. Its hidden `--break <rule>` switch, for this repository's tests
only, makes it break one rule on purpose, so the tests can show that generation and the malformed part find it; the
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

A bulk reply's bank is checked against the arena's start as the shim takes
it (wire.md 2.1, 2.6): `VFS_ARENA_OFFSET`, or right after the last slot when
it is not set. `--misbehave arena-elsewhere` (the arena one bank later, the
variable not set) and `--misbehave arena-offset-wrong` (the variable set to
the end of the last slot) fail every bulk READ, in the scenarios, the
generated cases, the concurrent clients and the limits.

## Reading a report

Every step is played even after one fails, so the report shows them all; a
later failure can follow from an earlier one, so read the first failure of a
scenario first. A ring that stops answering ends that scenario. Steps are
numbered from 0, as in the scenario's `steps` list.

TAP (version 13), one test point per scenario; a failed one has a YAML
block with its error, if any, and each failed step. After the test points,
a comment line per opcode counts the answers the director gave to the
scenario files' steps, listing every answer a step allows, so one that no
step drew shows as 0 (`# answers stage-exec: bad-request 0, not-found 0,
not-supported 4, ok 0` is a director that stages nothing):

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
  "answers": {"getattr": {"not-found": 0, "ok": 1}},
  "adapter_error": null
}
```

A scenario's `error` says what stopped it outside its steps: a refused mount
(`mount refused: ...`), a failed attach (`attach failed: ...`), a ring that
gave no answer, or the adapter error that ended the run, which is also in
`adapter_error`. `seed` is the generator's seed, and `null` for a run without
`--generate`. `answers` is the count the TAP comment lines give.

The malformed part is one entry, `malformed`, a step per case; its `request`
names the rule and shows the slot fields and payload in hex. The generated
part is one entry, `generated`: passed, with no steps and the case count and
seed as its `file`, or failed, with the shrunk case's steps and the file it
was written to.

## Soak runs

CI plays 1024 generated cases a seed. A failure that only a rare sequence
shows needs many more, so `aether-conform soak` plays batches of generated
cases and concurrent rounds for as long as it is given, each batch from a
fresh seed, and keeps every failure it finds as a case that replays.

```sh
# Two hours against the reference, a new seed each batch:
target/debug/aether-conform soak --director "target/debug/aether-conform reference-adapter"     --for 2h --failures soak-failures --summary soak-summary.json
# A number of generated cases instead of a time, from a seed that makes the
# whole soak repeat:
target/debug/aether-conform soak --director "..." --cases 20000 --seed 42
# Another director, behind its own adapter:
aether-conform soak --director "java -jar my-director-adapter.jar" --for 30m
```

**What a batch is.** One `run` against an adapter process of its own: 128
generated cases (`--batch-cases`) and 64 rounds of 4 clients at once
(`--concurrent`, `--concurrent-cases`; `--no-concurrent` for none), at most
`--steps` operations a case. The first batch also plays the scenarios, the
malformed requests and the limits (`--generated-only` leaves them out). The
soak ends after the batch that passes `--for` (`90s`, `10m`, `2h`, `1h30m`),
or once `--cases` generated cases are played, and stops early if ten batches
in a row play nothing (an adapter that exits at once). The first batch's seed
is `--seed`, else from the clock, and the others are drawn from it, so
`--seed` repeats a whole soak. The batch seeds are in the summary.

**What a failure is.** A failure ends its batch, not the soak. Each one is
shrunk as `run --generate` shrinks it and counted by its kind (`generated`,
`concurrent`, `suite` for a scenario, the malformed requests or the limits,
`adapter` for an adapter that failed, `generator` for a reference answer
wire.md does not give), its class (`mismatch at getattr`, `error: mount
refused`) and, for a generated case or round, the shape of its shrunk
scenario: its steps' opcodes in order and the client of each. The tree, the
paths, the bytes and the names are left out, since two seeds that meet one
defect seldom shrink to the same ones. One found again, from another seed,
is counted in `occurrences` and not written again, so a common defect is one
failure however many batches meet it.

**What is written.** `--failures DIR` (default a folder in the temporary
directory) gets a folder for each distinct failure, `DIR/<id>/`, holding its
scenario `<id>.json` (a generated case or round) and `replay.txt`: the kind,
the class, the seed, batch and case that found it, and the commands that
replay it. `--summary FILE` (and standard output) has the JSON summary:

| Field | Meaning |
|---|---|
| `director`, `seed`, `seeds`, `batches` | the command line, the first seed, every batch's seed |
| `budget` | `{"for_seconds": s}` or `{"cases": n}` |
| `cases`, `rounds`, `suite_scenarios` | generated cases, concurrent rounds and scenarios played (a failing batch counts up to its failing case) |
| `failures_seen`, `distinct_failures` | failures found, repeats included, and without them |
| `failures` | each distinct one: `id`, `kind`, `class`, `seed`, `batch`, `case`, `occurrences`, `file`, `replay`, `message` |
| `warnings` | things that were not the director's failure (too few rounds for every slot to carry a request) |
| `wall_seconds`, `passed`, `stopped` | the time, whether nothing failed, and `"no progress"` if it gave up |

The exit code is 1 when something failed (a summary that could not be
written does not hide it), 2 for a usage error, a summary that could not be
written, or a soak that stopped because its batches played nothing, and 0
otherwise. The workflow lists at most 30 failures in its job summary; every
one is in the uploaded `summary.json`.

**Replaying a failure.** The scenario in a failure's folder is an ordinary
scenario, so against the director that failed it fails the same way:

```sh
target/debug/aether-conform run --director "..." --scenarios soak-failures/generated-123-4
```

Against a correct director it passes: its expectations are the reference's
answers. `replay.txt` also has the command that finds it again from its
seed (`run --generate --cases N --seed S`), which plays the cases before it
too.

**Making one a regression.** A failure is the director's, the reference's,
or the generator's. Read the shrunk scenario against wire.md first: if its
expectation is not what wire.md says, the reference or the generator is
wrong, and the fix goes there, with this case as its test. If the director is
wrong, copy `<id>.json` to `contract/scenarios/` under a name that says what
it checks (`rename-dir-open-handle.json`), give it that `name` and a
`description` citing the wire.md rule, trim steps the failure does not
need, list it in the scenarios' README index and in "The scenarios" above,
and run the whole suite against the reference. A `one_of` is added only where
wire.md allows several answers. A failure of kind `suite` is already a
committed scenario or part of the malformed or limits requests.

**In CI.** `.github/workflows/soak.yml` runs the soak nightly and by hand
(`workflow_dispatch`, with the minutes to run, the director and a first
seed): against the reference over a ring file on Linux and on Windows and
over a named section on Windows, and the shim's fault soak below on Windows.
It never runs on a push or a pull request. A failure uploads the failures
folder and the summary and lists each failure with its replay command in the
job summary.

## Benchmarks

`aether-conform bench` times a director on the workloads in
`contract/workloads/` (their format is in `contract/workloads/README.md`):
the same adapter as `run`, the same attach, and then requests sent as fast
as client threads can send them, not compared with anything but the floor.

```sh
cargo build --release -p aether-conform
target/release/aether-conform bench --director "target/release/aether-conform reference-adapter" \
    --workloads contract/workloads --out target/bench/reference.json
# Over a named section (Windows), one workload, four clients per thread:
target/release/aether-conform bench --director "target/release/aether-conform reference-adapter --ring section" \
    --only launch-storm --clients 4 --out target/bench/storm.json
# Against an earlier report from this machine:
target/release/aether-conform bench --director "..." --baseline target/bench/reference.json
```

Per workload the adapter mounts its tree (written as a scenario file with
no steps, so an adapter needs nothing new). Each phase is played once as a
warm-up, not recorded, and then recorded: `threads x --clients` threads at
once, each with its own ring client, each sending the requests the workload
gives that thread in the same order every time. A request is timed from
before it is sent until its reply is decoded, a bulk READ's data copied out
of the arena as the shim copies it, into a histogram per opcode (1 ns to
60 s, three significant figures). Then the same requests go to the floor: by
default `aether-conform null-adapter`, which never reads the tree, on a ring
of the director's kind with the director's slot count, payload cap, arena
length and `worker_hint` as its serving threads. It answers every request OK
with the smallest reply, except READ. Each recorded READ goes to the floor
asking for as many bytes as the director's reply to it gave, with
`FLAG_READ_BULK` set exactly when that reply came in bulk, and the floor
answers that many bytes (zeros) in that form: written into the slot inline,
or into the slot's bank in bulk, and copied out as the director's were. So
the floor includes the round trip, the copies of the request and the reply
through the slot, and a READ's data movement through the slot or the arena;
what it leaves out is what the director does to answer. The report gives
both and, per opcode, the director's p50, p99 and p99.9 less the floor's.
`--floor "<command line>"` names another floor adapter, which is sent the
READs the same way.

A reply that is not what the phase expects stops the workload with an
error, and the workload has no numbers: a status other than OK, a GETATTR
of a file of the tree with `found = 0`, a READ shorter than the file the
OPEN described, a WRITE that took fewer bytes than sent, a READDIR with
another number of entries than the tree has. The floor is held to OK
statuses only. A workload the ring cannot carry (a WRITE whose 24-byte
header and data are past the payload cap, an inline READ whose reply is, a
bulk READ with no arena) fails the same way.

`bench`'s options:

| Option | Meaning |
|---|---|
| `--director "<command line>"` | The adapter to start, as for `run`. Required. |
| `--workloads DIR` | Default `contract/workloads`. |
| `--only NAME[,NAME...]` | Only these workloads. |
| `--floor "<command line>"` | The floor's adapter. Default: this program's `null-adapter` on a ring like the director's. |
| `--clients N` | Client threads per phase thread, 1 to 64. Default 1. |
| `--quick` | One repeat of at most 64 requests per thread (and the CLOSEs of what they opened): checks that every workload mounts and is answered, for CI. Its numbers are not timings. |
| `--out FILE` | Where the JSON report goes. A summary goes to stdout either way. |
| `--baseline FILE` | Print each value's change against an earlier report. A report from another machine (CPU model, logical cores, OS), director, `--clients` or `--quick` is refused, and so is one where a workload both ran has another ring geometry. |
| `--fail-over PCT` | With `--baseline`: exit 3 when an opcode's p50 rose more than PCT percent. |
| `--timeout SECS` | Every adapter answer and request. Default 120 (a mount builds large trees). |

Exit code: 0 when every workload ran, 1 when one failed, 2 on a usage or
adapter error or a refused baseline, 3 for a rise past `--fail-over`. A run
slower than its baseline exits 0: timed numbers are one machine's, and only
someone at that machine, asking with `--fail-over`, turns them into a
failure. CI never does; it runs the `--quick` pass over every workload but
`sequential-bulk` (whose 256 MiB tree is slow to build in a debug build),
which checks replies and counts and nothing that is timed.

The report:

```json
{
  "machine": {"cpu": "...", "cores": 16, "os": "...",
              "build": {"profile": "release", "rustc": "rustc ...", "git": "..."}},
  "director": "<command line>", "floor": "<command line>", "clients": 1, "quick": false,
  "workloads": [
    {"name": "random-small",
     "geometry": {"ring": "file", "slots": 8, "payload_cap": 1048576, "arena_len": 8388608,
                  "worker_hint": 4},
     "floor": "<command line>",
     "floor_geometry": {"...": "the same fields, for the floor's ring"},
     "phases": [
      {"name": "reads", "threads": 4, "requests": 32776, "seconds": 0.41,
       "requests_per_s": 79941.5, "read_bytes": 134217728, "bulk_bytes": 0,
       "bulk_mib_s": null,
       "floor": {"seconds": 0.07, "requests_per_s": 468228.6,
                 "read_bytes": 134217728, "bulk_bytes": 0},
       "per_op": {"read": {"count": 32768, "p50_us": 18.2, "p99_us": 61.0,
                           "p999_us": 140.3, "max_us": 802.0, "mean_us": 20.1,
                           "floor_p50_us": 3.1, "floor_p99_us": 9.4,
                           "floor_p999_us": 21.0, "floor_max_us": 95.1,
                           "over_floor_p50_us": 15.1, "over_floor_p99_us": 51.6,
                           "over_floor_p999_us": 119.3}}}]},
    {"name": "save-heavy", "error": "phase save, thread 0: request 3 (write ...): ..."}
  ]
}
```

Times are microseconds. `read_bytes` is the bytes READs returned and
`bulk_bytes` those that came in bulk, the floor's the same on its side;
`bulk_mib_s` is the bulk bytes over the phase's wall time, `null` for a
phase that sent none. Each workload is mounted on a ring of its own, so the
ring's geometry, the floor's command line and the floor's geometry are given
per workload; the top-level `floor` is the one asked for. The numbers above
show the shape, not a machine's results.

How to compare two commits, and the other benchmarks of this repository, are
in [benchmarks.md](benchmarks.md).

`aether-conform null-adapter [--ring file|section] [--slots N]
[--payload-cap N] [--arena-len N] [--workers N]` is the floor: the adapter
protocol with a director behind it that answers every opcode OK with the
smallest valid reply (GETATTR finds nothing, OPEN gives handle 1 of size 0),
except READ, which gives as many zeros as it asks for, up to what the reply
holds: in bulk when it sets `FLAG_READ_BULK` and the ring has an arena,
inline otherwise. Its ring has 8 slots, a 1 MiB payload cap, an 8 MiB arena
and 4 serving threads unless told otherwise.
