# Director conformance

`aether-conform` checks a director against the wire contract
(`docs/wire.md`). It starts the director under test behind a small adapter
program, has it mount a scenario's tree, attaches to the ring the director
names exactly as the shim would, plays the scenario's requests, and compares
each answer with the one the scenario expects. Scenarios live in
`contract/scenarios/` and their format is described in
`contract/scenarios/README.md`.

## What it checks

For every scenario:

- **The handshake.** The mount's environment must name a ring the shim
  could attach to (wire.md 2.4, 2.9, 2.10, 5.3): a ring file or, on
  Windows, a named section; a header with the right magic, version and
  stride whose slots fit in `VFS_RING_BYTES`; a ring file at least
  `VFS_RING_BYTES` long; `VFS_VIRTUAL_DIR` set, and the roots the shim
  builds from it, `VFS_VIRTUAL_ROOTS`, `VFS_LAUNCH_IMAGE` and
  `VFS_ROOT_BACKINGS` usable (the shim refuses, for example, an empty
  `VFS_VIRTUAL_DIR`: "managed root is not a usable path"). Two further rules a
  director must meet are checked too: the header's `payload_cap` equals
  `VFS_RING_PAYLOAD_CAP` (5.3: "Must equal the header's `payload_cap`"),
  and when `VFS_ARENA_OFFSET` is given the arena lies inside the mapping
  (2.6: "Every arena byte must lie inside the first `VFS_RING_BYTES` bytes
  of the mapping").
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
| GETATTR and OPEN `size` of a directory | Not compared when `is_dir` is set; `is_dir` is. | 3.4.1 and 3.4.3: "The size of a directory is the director's to choose" (3.4.2 fixes it only for READDIR entries). |
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
| `stage-exec` | STAGE_EXEC: `ST_NOT_SUPPORTED` or a path (3.4.14) |
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
rules are not expressible in the scenario format; the ones marked "malformed"
belong to the malformed-request mode of a later task:

- 3.4.4: a director must accept a 20-byte READ (no trailing pad) (malformed).
- 3.4.5: a WRITE whose payload is shorter than 24 + `len` is `ST_BAD_REQUEST`
  (malformed).
- 3.5.1: short payloads for every opcode, invalid UTF-8 in a path (malformed).
- 3.3: opcodes outside the table (0, 4, 12, 25 and up) are `ST_BAD_REQUEST`
  (malformed).
- 3.6: the slot `flags` word is ignored for every opcode but READ, and bits
  above bit 0 for READ (malformed: the format sends `read_bulk` only on READ).
- 4.1, 4.3: registry booleans are exactly 0 or 1, and trailing bytes in a
  registry request are `ST_BAD_REQUEST` (malformed).
- 3.4.2: a READDIR listing larger than `payload_cap` is answered with the
  overflow status `i32::MIN` and an empty payload. It is not in the protocol's
  status table, so a scenario cannot name it, and a tree large enough to
  overflow depends on the director's cap.
- A WRITE or request larger than the ring's payload, and a registry value over
  `MAX_DATA` (the request would not fit the ring); `payload_cap - 8` for
  REG_KEY is covered only as a `one_of`, since the cap is the director's.

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

```sh
cargo build -p aether-conform
# The reference director, over a ring file:
target/debug/aether-conform run --director "target/debug/aether-conform reference-adapter"
# The same over a named section (Windows):
target/debug/aether-conform run --director "target/debug/aether-conform reference-adapter --ring section"
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

Exit code: 0 when every scenario passes, 1 when any fails, 2 on a usage
error, a scenario that does not load, or an adapter error.

`aether-conform reference-adapter [--ring file|section]` is an adapter for
the reference director (`aether_refdirector::Reference`, built with
`Reference::from_tree`), served with the same loop a Rust director runs
(`aether_rpc::serve_with_arena`) on its own thread. It is what a run is
checked against in this repository's tests, and an example of what an
adapter does.

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
      request: "GetAttr { root: RootId(0), path: \"a.txt\" }"
      expected: "GetAttr(AttrResp { found: true, is_dir: false, size: 4, mtime: 0 })"
      got: "GetAttr(AttrResp { found: true, is_dir: false, size: 3, mtime: 0 })"
      note: "different attributes (mtime aside)"
  ...
```

`request` is the request as the scenario wrote it (before handles were
translated), `expected` and `got` the two answers, and `note` the rule that
failed followed by the scenario's own note. An adapter error ends the
report with `# ` lines holding the message and the adapter's stderr, and a
`Bail out!` line.

JSON has every step, passed or not:

```json
{
  "scenarios": [
    {"name": "wrong", "file": "contract/scenarios/wrong.json", "passed": false, "error": null,
     "steps": [
       {"index": 0, "request": "GetAttr { root: RootId(0), path: \"a.txt\" }",
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
`adapter_error`. `seed` is for generated runs and is `null` for scenarios
read from files.
