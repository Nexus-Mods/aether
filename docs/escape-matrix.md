# Escape matrix

The escape matrix is the set of path spellings a program can use to try to
reach a file under a managed root without going through the director.
`aether-fixture-escape` builds each spelling and reports what happened; the
shim's job is that every spelling of a file under a root reaches the director
or nothing. The invariants the code keeps are in
[`shim-invariants.md`](shim-invariants.md).

## What the fixture probes

`aether-fixture-escape.exe <target> [out]` takes the path of a file and writes
one tab-separated line per vector to `out` (else stdout):

```text
<vector-id>\t<spelling-attempted>\t<outcome>\t<note>
```

`<outcome>` is `opened` (read mode: the spelling opened the file and its bytes
matched a baseline read of the literal path), `written` (write mode: the
vector's own payload was written and read back through the same spelling),
`not-found`, `error:<detail>` (`content-mismatch:` among them: something
opened, with other bytes), `unbuildable:<reason>` (the environment could not
construct the spelling), or, for `enum`, `listed:<count>` with the sorted
entry names in `<note>`, separated by `|`.

| # | Spelling |
|---|---|
| 1 | 8.3 short name (`GetShortPathNameW`, then an open) |
| 2 | Extended-length prefix (`\\?\C:\...`) |
| 3 | NT device path (`\\?\GLOBALROOT\Device\HarddiskVolumeN\...`) |
| 3b | `\\?\GLOBALROOT\GLOBAL??\C:\...` |
| 3c | `\\?\GLOBALROOT\??\C:\...` |
| 3d | `\\?\Global\C:\...` |
| 4 | Volume-GUID path (`\\?\Volume{guid}\...`) |
| 5 | Handle-relative open (`OBJECT_ATTRIBUTES.RootDirectory` a real directory handle) |
| 5b | Handle-relative open against a handle `GetFinalPathNameByHandleW` cannot resolve (an anonymous pipe) |
| 6 | CWD-relative (a plain file name, the working directory its parent) |
| 7 | Junction |
| 8 | Hardlink (a new name for the same bytes) |
| 9 | UNC admin share (`\\localhost\C$\...`) |
| 10a, 10b, 10c | Case-flipped, trailing dot and trailing space, each `\\?\`-prefixed so Win32 does not normalise it before `NtCreateFile` |
| 11 | Alternate data stream (`name.esp:probe`), never created by the fixture |
| 12a, 12b, 12c | A `.` component, `..` through a non-existent name, a doubled separator, each `\\?\`-prefixed |
| 13 | A handle opened earlier, re-confirming ordinary reachability (the fixture cannot open a handle before a root is registered) |
| 14 | A child process (`cmd /C type`, or in write mode `cmd /C echo` to a sibling `<target>.v14-child-write.txt`) |
| 4m | Opt-in: vector 4's spelling through `GetFileAttributesW` rather than an open |
| enum | Opt-in: a `read_dir` listing of the target's parent directory |

The fixture is a Windows program (`bin/build-natives.ps1 -Fixtures`, or
`bin/build-windows` on Linux), and it says something about containment only
when run under a session: started by a director's injector against a target
under a managed root. Its switches:

- `VFS_ESCAPE_ACCESS`: `read` (the default) or `write`. Every vector builds
  the same spelling either way; in write mode it opens with `OPEN_ALWAYS`
  (create if absent, never truncate), so an escape leaves a file on disk and
  the open asks the director for a copy-up.
- `VFS_ESCAPE_ONLY_VECTOR`: run only that vector, so the shim's
  `VFS_SHIM_STATS_LOG` report reflects that one attempt alone; `4m` and
  `enum` run only this way.
- `VFS_ESCAPE_VECTOR7_LINK_DIR`: a junction the caller made before launching,
  for vector 7 to open through; unset, the fixture makes its own.

A run against two targets tells containment from broken access: a *positive
canary* the director serves (with the same bytes physically under the root),
which every spelling should open or write, and a *negative canary* physically
under the root that no provider serves, which no spelling should reach. This
repository has no session runner, so no test here asserts the fixture's
outcomes; a director's end-to-end suite does.

## What this repository checks

The shim routes and classifies with one predicate, the client's root map
(`aether-redirect`'s `RootMap`, held by `FuseClient`), so a spelling that is
recognised as under a root is also routed to the director. What recognises
each spelling, and the tests that hold it:

- **NT prefixes** (vectors 2, 3, 3b-3d). Prefix handling is a token loop over
  `?`, `??`, `GLOBAL??`, `Global` and `GLOBALROOT`, up to 16 tokens
  (`MAX_PREFIX_STEPS`); it over-accepts (`\??\GLOBALROOT\GLOBALROOT\...` is
  stripped though the object manager rejects it), because "answered by the
  director" is the fail-closed direction. Tests: `nested_prefixes_are_fully_stripped`,
  `globalroot_wrapped_device_prefix_resolves_like_the_bare_form`,
  `a_dosdevices_entry_resolves_through_any_spelling_of_that_directory`,
  `an_unmapped_device_stays_outside_behind_every_wrapper`, and the
  `NT_SPELLING_VECTORS` table in `aether-redirect/src/tests.rs`, which checks
  each spelling both under a root and outside it.
- **Streams** (vector 11). Only the final component can carry a stream, with
  a drive-letter colon skipped, so `\??\GLOBALROOT\??\C:\...` is not read as a
  stream on `C`; the client puts a stream suffix back on the vpath it sends.
  Tests: `a_drive_colon_behind_a_namespace_token_is_not_a_stream_separator`,
  `strips_an_alternate_data_stream_suffix`.
- **Devices and volume GUIDs** (vectors 3, 4), resolved once per session and
  keyed with the `\??\` spelling a real open presents. Tests:
  `under_root_recognises_a_device_path_spelling`,
  `volume_guid_key_uses_the_nt_prefix_a_real_open_presents`.
- **The UNC admin share** (vector 9). `\??\UNC\localhost\<drive>$` is an alias
  for `<drive>:`. Tests: `resolve_volume_map_registers_the_localhost_admin_share`,
  `insert_alias_resolves_the_unc_admin_share_nt_spelling`.
- **Junctions** (vector 7). The root's ancestor chain is scanned two levels
  up (`MAX_ANCESTOR_LEVELS`), one listing per level; a sibling reparse point
  whose target lies inside the root becomes an alias. The scan reads each
  reparse point's substitute name and never follows it; an ancestor of the
  root is never aliased, and the root's own subtree is not scanned. Tests:
  `junction_pointing_outside_root_is_not_aliased`,
  `root_itself_being_a_reparse_point_is_never_aliased`,
  `reparse_point_target_reads_a_junction_without_opening_its_target`.
- **8.3 short names** (vector 1). A path holding `~` is expanded through the
  OS; the consult re-enters the hooks, and a thread-local guard makes a
  re-entrant call answer "not recognised here". Tests:
  `expand_long_path_round_trips_a_short_name`,
  `os_consult_guard_refuses_reentry_and_releases_on_drop`.

Spellings the root map does not recognise: loopback names other than
`localhost` for the admin share (`\??\UNC\127.0.0.1\C$\...`), the MUP device
without the `\??\UNC` symlink (`\Device\Mup\localhost\C$\...`), a junction
more than two levels above the root, and a drive, `subst` or junction changed
after the table is built.

What reaches the director rather than real disk, tested through the real
detours against the fake director in `aether-hooks/tests/`:

- a directory listing under a root is the director's alone
  (`file_hooks/hook_direnum.rs`, `file_hooks/hook_enum_parity.rs`);
- name-based attribute queries are the director's (`file_hooks/hook_attrs.rs`,
  `file_hooks/hook_stat_agreement.rs`);
- handle-relative names resolve as absolute ones do
  (`file_hooks/hook_relative_paths.rs`, `file_hooks/hook_handle_relative_unseen.rs`);
- a write the director does not take creates nothing under the root
  (`seal/write_seal.rs`, `seal/write_seal_no_overlay.rs`);
- `NtDeleteFile` under a root is the director's DELETE, the root itself is
  refused, and a delete outside every root reaches the OS
  (`seal/nt_delete_file_sealed.rs`);
- a rename whose target is under a root is refused wherever its source is
  (`seal/rename_into_root_sealed.rs`), as is a hard link touching a root
  (`seal/link_into_root_sealed.rs`);
- a delete or rename through a handle the shim never saw opened is decided
  like any other (`seal/handle_ops_out_of_root_sealed.rs`).
