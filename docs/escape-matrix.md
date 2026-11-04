# Escape matrix

The escape matrix is the set of path spellings a program can use to try to
reach a file under a managed root without going through the director, and
what the shim does with each. `aether-fixture-escape` builds every spelling
and reports one outcome line per vector; a director's end-to-end suite runs it
under a real session (director, ring, injected shim) and asserts the
outcomes below. The invariants the code keeps are in
[`shim-invariants.md`](shim-invariants.md).

## The two canaries

- **Positive canary**: `escape-positive-canary.esp`, identical bytes served
  by the director and written physically onto the session's real, on-disk
  managed root. Whichever way a spelling is served the bytes are the same,
  and the fixture reads every successful open back and compares it with a
  baseline read of the literal path, failing with
  `error:content-mismatch:...` on any difference. So `opened` below means the
  same file's real bytes, not merely "some handle came back".
- **Negative canary**: `escape-negative-canary.bin`, a real file physically
  on the managed root that no provider serves.

A spelling is contained when the positive canary opens through it (access is
not broken) and the negative canary does not (the real disk is not reached).
Each negative-canary vector is also re-run alone (`VFS_ESCAPE_ONLY_VECTOR`,
every other vector skipped, and the fixture's own baseline reads skipped too)
and must land in one of the shim's `under-root open outcomes` buckets in its
`VFS_SHIM_STATS_LOG` report, so a spelling cannot pass by being invisible to
every counter. A shared, combined-run set of classified paths would let one
vector ride on another's entry with the same file name.

Every row is asserted against a second root (`RootId(1)`) of a two-root
session as well as root 0.

## The read matrix

`opened✓` = opened, byte-identical. `not-found` = refused. `unbuildable`
lines carry the fixture's own reason.

| # | Vector | Positive canary | Negative canary |
|---|---|---|---|
| 1 | 8.3 short name (`GetShortPathNameW` + open) | opened✓ | `unbuildable: GetShortPathNameW failed: win32:2` |
| 2 | Extended-length prefix (`\\?\C:\...`) | opened✓ | not-found |
| 3 | NT device path (`\\?\GLOBALROOT\Device\HarddiskVolumeN\...`) | opened✓ | not-found |
| 3b | `\\?\GLOBALROOT\GLOBAL??\C:\...` | opened✓ | not-found |
| 3c | `\\?\GLOBALROOT\??\C:\...` | opened✓ | not-found |
| 3d | `\\?\Global\C:\...` (`Global` is a DosDevices symlink to `\GLOBAL??`) | opened✓ | not-found |
| 4 | Volume-GUID path (`\\?\Volume{guid}\...`) | opened✓ | not-found |
| 5 | Handle-relative open (`OBJECT_ATTRIBUTES.RootDirectory` = a real directory handle) | opened✓ | not-found |
| 5b | Handle-relative open against a handle `GetFinalPathNameByHandleW` cannot resolve (an anonymous pipe) | `error:ntstatus:0xC0000033` | not asserted |
| 6 | CWD-relative (plain file name, cwd set to the parent) | opened✓ | not-found |
| 7 | Junction | opened✓ | not-found |
| 8 | Hardlink (new file name, same bytes) | not-found | `unbuildable:std::fs::hard_link failed: ... (os error 2)` |
| 9 | UNC admin share (`\\localhost\C$\...`) | opened✓ | not-found |
| 10a | Case-flipped, `\\?\`-prefixed | opened✓ | not-found |
| 10b | Trailing dot, verbatim (`...\name.esp.`) | opened✓ | not-found |
| 10c | Trailing space, verbatim (`...\name.esp `) | opened✓ | not-found |
| 11 | Alternate data stream (`name.esp:probe`) | not-found | not-found |
| 12a | `.` component, verbatim | opened✓ | not-found |
| 12b | `..` through a non-existent intermediate name, verbatim | opened✓ | not-found |
| 12c | Doubled separator, verbatim | opened✓ | not-found |
| 13 | Handle opened before the managed root was registered | reported | reported |
| 14 | Child process | reported | reported |

Notes on the rows that are not plain `opened✓` / `not-found`:

- **1, negative.** `GetShortPathNameW` itself queries the target through the
  hooks, and the negative canary is not found there, so the short name cannot
  be built and no open is attempted.
- **5b.** No path can be decoded for this shape, so the open lands in the
  shim's "undecodable opens" counter, not in `under-root open outcomes`. A
  handle-relative open is classified only when its root handle resolves to a
  real path.
- **8, positive.** A hardlink is a new name under the root that no provider
  has; the director answers not-found and the shim does not fall through to
  the real, linked bytes. **8, negative.** `CreateHardLinkW` must open its
  source, the negative canary, and that open is refused, so the link cannot be
  built.
- **10b, 10c, 12a, 12b, 12c.** Under a session these resolve to the root's
  canonical spelling and reach the director, so they open the positive
  canary; standalone, without the shim, Windows answers them differently
  (`12c` is `ERROR_INVALID_NAME`).
- **11.** The fixture never creates the stream, so `not-found` means the
  stream does not exist. The canonicaliser drops a stream suffix (a stream is
  a spelling of its file), and the client puts it back on the vpath it
  sends, so a named stream is never answered with the base file's bytes.
- **13** needs a handle that exists before the root map is built, a timing
  the fixture cannot construct; its line only confirms ordinary reachability
  inside an injected process. It is not asserted.
- **14.** The child is injected: the shim follows `CreateProcessInternalW`
  and fails closed (architecture §4.3), so the negative canary's
  `error:cmd-exit:1` is containment (an uninjected `type` would have printed
  the real bytes). It is not asserted, because the fixture's child line
  depends on the child's own exit code.

## How the spellings are recognised

The shim routes and classifies with one predicate: the client's root map
(`aether-redirect`'s `RootMap`, held by `FuseClient`), built over every
declared root plus the staged launch directory as a spelling of root 0. Every
alternate spelling is canonicalised to the root's own form before it is
compared, so a spelling that is recognised is also routed to the director.

- **NT prefixes.** Prefix handling is a bounded token loop over `?`, `??`,
  `GLOBAL??`, `Global` and `GLOBALROOT`, so any nesting of them resolves. It
  over-accepts (`\??\GLOBALROOT\GLOBALROOT\...` is stripped though the object
  manager rejects it), because over-accepting means "answered by the
  director", the fail-closed direction. A `GLOBALROOT`-wrapped device or
  volume-GUID prefix resolves exactly as its bare form.
- **Streams.** Stream detection is scoped to the final path component, with a
  drive-letter colon skipped, so `\??\GLOBALROOT\??\C:\...` is not read as a
  stream on `C`.
- **Devices and volume GUIDs.** A table of every mounted drive's device name
  and volume-GUID mount point, resolved once per session.
- **The UNC admin share** (vector 9). `\??\UNC\localhost\<drive>$`, the NT
  spelling of `\\localhost\<drive>$\...`, is registered as an alias for
  `<drive>:` for every drive. The alias is keyed with the `\??\` spelling a
  real open presents, never `\\?\`, which would match nothing.
- **Junctions** (vector 7). The managed root's ancestor chain is scanned, one
  non-recursive listing per level, up to two levels: a sibling directory
  reparse point whose target lies inside the root is registered as an alias
  from its own location to that target. The scan reads each reparse point's
  substitute name (`FSCTL_GET_REPARSE_POINT` on a handle opened with
  `FILE_FLAG_OPEN_REPARSE_POINT`) and never follows it, since following an
  offline target blocks for the OS's device or network timeout. Entries are
  pre-filtered by the type the listing already returned, so only reparse
  points cost a query. An ancestor of the root is never aliased (the root's
  own components are its literal spelling, and aliasing an ancestor would
  rewrite every ordinary open), and the root's own subtree is not scanned.
- **8.3 short names** (vector 1). A path holding `~` is expanded through the
  OS. That consult opens its own handle, which re-enters the hooks; a
  thread-local guard makes a re-entrant call answer "not recognised here",
  so the nested open reaches the real `NtCreateFile` instead of recursing.

The alias table is built at the client's first decision, not at DLL load,
once per session, with its own re-entrancy guard: a junction that exists when
the program first touches the file system is seen. A director's test creates
vector 7's junction before the fixture starts (`VFS_ESCAPE_VECTOR7_LINK_DIR`),
the shape of a junction a mod manager set up before the game started.

## Spellings and shapes not covered

- **Other loopback names.** Only `localhost` is aliased for the admin share:
  `\??\UNC\127.0.0.1\C$\...`, `\\[::1]\C$\...` and the machine's own names
  name the same object and are not recognised. Nor is
  `\Device\Mup\localhost\C$\...`, the MUP device without the `\??\UNC`
  symlink.
- **Deeper junctions.** A junction more than two ancestor levels above the
  root is never scanned and is not recognised.
- **Staleness.** A drive, `subst`, or junction created, retargeted or removed
  after the table is built is not reflected until the session is rebuilt.
- **Network drives.** The admin-share alias is registered for every drive
  letter, network-mapped ones included, without checking that
  `\\localhost\<drive>$` resolves to the same object.
- **Scaling.** The alias table has no bound and is scanned linearly, folding
  each entry, on every canonicalisation cache miss.
- **The chain-node exclusion** in the junction scan compares paths
  byte-for-byte; the separate check that an alias target lies inside the root
  is what keeps an ancestor from being aliased.

## The write matrix

With `VFS_ESCAPE_ACCESS=write` every vector builds the same spelling and opens
it with `OPEN_ALWAYS`/`FILE_OPEN_IF` (create if absent, never truncate),
writes a payload that encodes its vector id, and re-opens the same spelling
to read it back, so `written` means "this vector's own bytes are durable
through this name". The disposition creates, so a spelling that escapes
leaves a file for the harness to find, and preserves, so it asks the director
for a copy-up.

The source is mounted at `Games/Skyrim/Data` and the negative canary
(`escape-write-negative-canary.bin`) sits in a sibling directory,
`Games/Skyrim/Unserved`, physically on the root and covered by no mount: a
create under a writable mount is legitimately stored, so only an unmounted
directory can show a write being blocked.

| # | Positive (write) | Negative (write) |
|---|---|---|
| 1 | `written` | `unbuildable:GetShortPathNameW failed: win32:2` |
| 2, 3, 4, 5, 6, 7, 9, 10a, 10b, 10c, 11, 12a, 12b, 12c | `written` | not-found |
| 5b | `error:ntstatus:0xC0000033` | `error:ntstatus:0xC0000033` |
| 8 | `written` | `unbuildable:std::fs::hard_link failed: os error 2` |
| 13 | `written` (reported) | not-found (reported) |
| 14 | `written` (reported) | `error:cmd-exit:1` (reported) |

The status is half the claim; the other half is real disk, checked from the
harness, which is never injected: after the negative run the canary's bytes
are unchanged, its directory holds nothing but the canary, and no named
stream was created on it; after the positive run the provider's copy holds a
payload, the physically mirrored copy still holds its seed, and there are no
strays. Both canary directories are first proved writable by creating and
deleting a probe file, since "nothing was created" means nothing for a
directory that could not be written.

## Enumeration

A listing under a managed root shows only what the director serves.
`serve_dir_query` answers a directory the client recognises with the
director's `readdir`, whole and unmerged. With no client, or for a directory
under a root that the client does not recognise, the listing is empty: no code
reads a real directory into a served listing. The stats report records which
mechanism answered each enumeration (`director`, `contained` or `OS`).

The fixture's opt-in `enum` vector lists a directory holding a served canary
(served and physically present) and an unserved one (physically present
only); the harness checks both are on disk, then that the listing has the
served name and not the unserved one and that every enumeration of that
directory was answered by `director`.

## Operations by path or handle

- **`NtDeleteFile`** takes only an `OBJECT_ATTRIBUTES`, so it is hooked and
  decided on the path like an open: under a root it goes to the director's
  DELETE (a not-found there is the caller's not-found, and `ST_NOT_FOUND`,
  `ST_READ_ONLY` and `ST_IS_DIR` map to the NT statuses the open path uses),
  and the managed root itself is refused. Outside every root it reaches the
  OS unchanged, and a relative name that climbs out of a root through a
  synthetic directory handle is rebuilt as an absolute path first. Tests:
  `seal/nt_delete_file_sealed.rs`.
- **Renames into a root.** A rename whose target is under a managed root is
  refused with `STATUS_ACCESS_DENIED` wherever its source is: RENAME carries
  one root and two paths under it, so there is no import from outside. Tests:
  `seal/rename_into_root_sealed.rs`, for `MoveFileExW`,
  `FILE_RENAME_INFORMATION` and `FILE_RENAME_INFORMATION_EX`.
- **Handle operations on a file under a root.** A delete or rename through a
  handle the shim never saw opened (inherited, duplicated, or opened before
  injection) recovers the handle's path (the shim's own tables, then
  `GetFinalPathNameByHandleW`) and is decided like any other. A rename out of
  a root to a target outside every root, and a delete of the root itself, are
  refused. Tests: `seal/handle_ops_out_of_root_sealed.rs`.
