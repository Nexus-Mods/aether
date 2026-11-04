# Conformance scenarios

A scenario is a tree for a director to mount and a list of steps, each a
request and the answer it must get. `aether-conform` plays them against a
director and against the reference (`aether_refdirector::Reference`, built
with `Reference::from_tree`) and compares the answers by the rules below.
Every name is `contract/protocol.json`'s: opcodes, request and reply fields,
statuses, flags. The loader is `aether_conform::scenario::load` (one file) and
`load_dir` (every `*.json` in a folder, in name order).

## Format

```json
{
  "name": "read-and-write",
  "description": "A mounted file read whole; a new file written under a writable prefix",
  "tree": {
    "roots": [
      {"id": 0,
       "files": [{"path": "data/a.esp", "contents": "abc", "mtime": 0},
                 {"path": "data/b.bsa", "contents": {"size": 4096, "seed": 1}}],
       "writable": ["saves"]}
    ]
  },
  "steps": [
    {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
     "expect": {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": true, "mount_gen": 0}}},
    {"request": {"op": "read", "fh": 1, "offset": 0, "len": 16},
     "expect": {"reply": {"data": "abc"}}},
    {"request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}},
    {"request": {"op": "open", "root": 0, "flags": ["write", "create"], "path": "saves/new.txt"},
     "expect": {"reply": {"fh": 2, "size": 0, "is_dir": false, "immutable": false, "mount_gen": 0}}},
    {"request": {"op": "write", "fh": 2, "offset": 0, "data": {"hex": "6869"}},
     "expect": {"reply": {"written": 2}}},
    {"request": {"op": "close", "fh": 2}, "expect": {"status": "ok"}},
    {"request": {"op": "readdir", "root": 0, "path": "."},
     "expect": {"reply": {"entries": [
       {"name": "data", "is_dir": true, "size": 0, "mtime": 0},
       {"name": "saves", "is_dir": true, "size": 0, "mtime": 0}]}}},
    {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "nowhere"},
     "expect": {"status": "not-found"}},
    {"request": {"op": "stored-names", "root": 0, "skip": 0, "path": "zz/q"},
     "expect": {"one_of": [{"reply": {"names": "zz/q"}}, {"status": "not-found"}], "wire": "3.4.12"},
     "note": "a path the director does not have: echoed (SHOULD) or not-found (MAY)"}
  ]
}
```

**Tree.** `roots` lists the roots served, each with `id`, `files` (`path`,
`contents`, optional `mtime`), `dirs` (directories besides the ones a file's
path implies) and `writable` (directories, or `"."` for the whole root,
under which a write open is honoured; each is a directory too). Optional
`registry` is `{"keys": [{"path", "volatile", "values": [{"name", "ty",
"data"}]}]}`, keys created in the overlay; without it the session has no
registry layer (wire.md 4: every registry opcode `not-supported`). Paths
are canonical. A limitation of the reference director, not a property of
directors, and one that may be lifted: it makes every root writable when
root 0 is writable as a whole, so a tree with root 0 writable as a whole
must make every root writable as a whole, or the reference refuses it.

**Who holds what.** Everything under a `writable` entry (the files and
directories the tree lists there, and any made there later) is held by the
writable provider itself, not by a read-only layer beneath it, so wire.md
3.4.9's allowances for "names only in a read-only lower" never apply, and an
adapter must mount them that way (a director that serves a writable root as an
overlay must have the tree's writable part in the upper). A file or directory
outside every `writable` entry belongs to a read-only provider: a write open,
DELETE, RENAME or MKDIR of it is `read-only` (wire.md 3.5). A root with no
`writable` entry has no writable provider at all. A scenario that needs "the
root itself is held by the writable provider" (wire.md 3.4.7, 3.4.8) makes the
root writable as a whole, `"."`.

**Bytes** (`contents`, WRITE's `data`, a value's `data`, READ's reply
`data`): a JSON string is its UTF-8, `{"hex": "00ff"}` is those bytes, and
`{"size": N, "seed": S}` is N bytes generated from S
(`aether_refdirector::tree::fill`; seed 0 is `aether_refdirector::pattern`).

**Requests.** `op` and every request field of that opcode in
`protocol.json`, padding aside; nothing else, except READ's optional
`slot_flags` (`["read_bulk"]`). `flags` is a list of `open_flags` names.
File paths (`path`, `from`, `to` of file opcodes) must be canonical: `/`
between components, none empty, `.` or `..`, no `\`, and `"."` for the root.
wire.md 3.2 says other syntax is director-defined ("a client must not rely
on it"), so a scenario never sends it. The one other path it sends is one
that leaves its root: wire.md 3.2 says a director "must treat a path that
would leave its root (`..` above the root) as an error", so a path whose
first component is `..` (`../x`, `..`, `../../a/b`, `../a/../b`) is
allowed, the other components being names or `..` (not empty, not `.`), and
escapes under any way of normalising. A path that goes up only after other
components (`a/../..`) is not sent. Registry paths are sent as written.

**Expectations.** One of:

- `{"reply": {...}}`: status OK and the opcode's reply fields, all of them
  (`readdir` has `entries`, a list of `{name, is_dir, size, mtime}`;
  `reg-lookup`'s `state` is `absent`, `present`, `created` or `tombstoned`;
  `reg-key`'s `node` is null or `{created, volatile, last_write, values,
  value_tombstones, children}` with children `{folded, spelling, state}`,
  state `present` or `tombstone`).
- `{"status": "<name>"}`: that error status. `"ok"` is allowed only for an
  opcode whose reply is empty (SETATTR, RENAME, DELETE, MKDIR, CLOSE,
  HEARTBEAT, RENAME_X).
- `{"one_of": [...], "wire": "<section>"}`: any of the listed answers, only
  where wire.md allows more than one, and `wire` names the section that does.

A step may carry a `note`, and `"client": n` (default 0): which client of
the ring sends it. The run attaches one client per number named, every one
but 0 a process of its own claiming from slot `n`, so a scenario can show
what one process of a session sees of another's requests (wire.md 2.8.1;
`docs/conformance.md`, "Several clients"). Handle numbers belong to the
scenario, not to a client: any client may present a handle another opened,
and a number open in one client is open for all.

An unknown op, field, flag, state or status, a missing field, a
non-canonical path, a bad tree, or an OPEN that expects a handle number
still open in the scenario, in any client (wire.md 3.4.3: unique among open
handles; 2.8.1: across all processes), is a load error naming the file, the
step and the field (`saves.json: step 3: request.flags: unknown flag
"sideways", ...`).

## Comparison rules

The rule is equality. These are the exceptions, each allowed by the wire.md
text quoted from the section named; there are no others
(`crates/aether-conform/src/compare.rs`).

| What | Rule | wire.md |
|---|---|---|
| OPEN `fh` | Paired with the expected fh at the OPEN that issues it, one to one while open; a successful CLOSE ends the pairing, so the number may come back. Requests carry the director's number (`Session::translate`): the paired one while open; after CLOSE the number the director closed, so a use-after-close tests that handle (unless the director has issued it again); otherwise, and for a number never paired, a stand-in no open handle of the director has, counted down from `u64::MAX`. | 3.4.3: "`fh` is any u64 the director chooses, unique among open handles" |
| OPEN `mount_gen` | Not compared. | 3.4.3: "a generation the director bumps whenever a root's content is replaced"; the directors start it differently |
| OPEN `immutable` | Not compared, except that it must be clear on a write open and on a directory; reads through immutable handles are checked against each other (below). | 3.4.3: it permits caching; "Never set it for a write open or a directory" |
| READDIR order | Entries compared as a multiset. | 3.4.2: "A director may send the entries in any order." |
| `mtime` (GETATTR, READDIR) | Not compared, and not checked for changed or unchanged after a write either (an earlier plan's idea, dropped: the time is the director's). A mounted file's `mtime` in the tree is input for a director that keeps it; the reference reports 0. | 3.2: "Which time a director reports for a path, if any, is the director's to choose" |
| READ inline or bulk | Compared by data; a bulk reply is read from the arena first (`OwnedReply::resolve_bulk`). | 3.4.4: the client "must accept either whatever flag it sent" |
| STAGE_EXEC `path` | Not compared. | 3.4.14: "the real, on-disk Windows path to start it from", wherever the director staged it |
| Registry `version` | Compared up to one offset per session, fixed by the first registry reply; REG_CHANGED's `since` is sent shifted by it, except `u64::MAX`, the shim's probe for the current version (`regnotify.rs`), which is sent as it is. | 4.3: "The number a director's overlay starts from when it mounts a layer is the director's", versions may differ "by a constant", and "a REG_CHANGED `since` is in the numbering of the director it is sent to" |
| REG_KEY `last_write` | Not compared. | 4.3: a FILETIME, which the overlay stamps with the director's clock |

A `one_of` is the only way a scenario allows several answers, and it cites
its section in `wire`.

## Checks across steps

Rules no single answer shows are checked across a scenario's steps and
clients, and fail the step that breaks them, whatever its expectation:

| What | Rule | wire.md |
|---|---|---|
| Read cache | Every READ through a handle whose OPEN reply set `immutable` is recorded under (root, path folded as 3.2 folds it, the reply's `size` and `mount_gen`, offset, length) for the rest of the scenario, bulk reads once read from the arena; a READ of the same key through an immutable handle that returns other bytes fails. Reads through other handles are not recorded (`compare.rs`). | 3.4.3: "two immutable opens of one path with equal size and `mount_gen` must have identical content"; "the handle's bytes cannot change while it is open" |
| `reg_gen` | Read from the ring header before every request and after every reply (`run.rs`, `RegGen`), by the client that sends the step, in its own mapping: with a `registry` in the tree it is non-zero before the first registry request; after a successful REG_SET_VALUE, REG_DELETE_VALUE, REG_CREATE_KEY, REG_DELETE_KEY or REG_RENAME_KEY it is greater than before the request; it never goes backward, across all the clients. Without a `registry` only the last applies. | 4.5: "a director that serves the registry must publish a non-zero generation before the shim attaches, and must bump it before replying to any write"; "never backward"; 2.8.1: it is how one process's write reaches the others |
| A client's slot | A step of client `n` is claimed from slot `n`: an answer must come within the timeout, a bulk reply must lie in that slot's bank, and the slot's `ack` must be its request's (`clients.rs`). | 2.5: every taken slot is completed; 2.6: "Slot `s` owns bank `s % banks`" |

To play scenarios against a director, see `docs/conformance.md`. A generated
case that fails (`aether-conform run --generate`) is written in this format
too, with the reference's answers as its expectations, so it replays as any
other scenario.
