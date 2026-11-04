# Shim-side harness and benchmarks (phases 4 and 5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The shim is tested against a server that records every request and controls or corrupts its answers: its caches are shown to cost zero round trips on a hit and to refetch exactly when the contract says, every generated fault is shown never to crash or hang it, and each hooked call's round trips and allocations are counted and gated in CI. Then both ends get benchmarks: the ring floor, a director's cost per opcode (`aether-conform bench`), in-process micro-benchmarks, and the shim's cost per Win32 call, all off the main CI's pass/fail.

**Architecture:** `aether-rpc` gains a byte-level `Tap` on its serve loop. `aether-refdirector` gains the harness layers built on it (`Recorder`, `Faults`, `Override`) and two knobs on `Reference` (mount generation, content replacement). The shim tests' `fakedirector` serves through those layers, and a new aether-hooks test binary (`tests/harness/`) holds the recording, cache, cost and fault tests, with a counting allocator. A new portable crate `aether-fuzz` holds the fuzz bodies (replayed on stable in CI); `fuzz/` holds the cargo-fuzz targets (nightly, Linux, scheduled). Phase 5 adds workloads (`contract/workloads/*.json`) and `aether-conform bench` with HDR histograms, criterion benches in the crates whose code they time, and a Windows-only crate `aether-shimbench` that runs a program natively and under the shim.

**Tech Stack:** Rust (toolchain in `rust-toolchain.toml`, 1.99.1), `proptest` (already a workspace dependency), `hdrhistogram` and `criterion` (new workspace dependencies, latest releases on crates.io when the task starts), `cargo-fuzz` with `libfuzzer-sys` (nightly, `fuzz/` only, outside the workspace).

**Spec:** `docs/superpowers/specs/2026-11-03-protocol-and-conformance-design.md`, sections 7 and 8, phases 4 and 5. Contract: `docs/wire.md`. Phase 3's tool: `docs/conformance.md`.

## Global Constraints

- No wire change: every line of `crates/aether-protocol/tests/golden/vectors.txt` byte-identical, wire version 4, `contract/env.json`, `contract/artifacts.json` and `contract/protocol.json` unchanged. No new handshake variable (test-only switches live in Rust APIs or in test-binary environment names outside `aether-env`'s handshake tables).
- What the shim does toward a well-behaved director does not change. Code compiled into the shim may change (spec, "The code's shape may change"): a test-only entry point, the gap-hunt fixes of Tasks 3A, 3B and 3D (each ruled in "Rulings on the spec"), and fixes for a crash or hang a fault test finds. A fix that would change a reply a well-behaved director gets is not made in this plan: stop and report it.
- The shim stays allocation-light: nothing added to a path the shim takes allocates. The counted-allocation records start at today's counts.
- Timed metrics never fail a shared CI run. Counted metrics (round trips, allocations, ring bytes) run in `cargo test` and gate CI.
- Rust only; no Python, also not in workflows (YAML checks use the repo's own tools or `cargo`).
- Tests that run the hooks follow the existing pattern: `isolate!` (one process per test), `fakedirector` for the far side, `aether_testkit::tempdir()` for files.
- Every task ends green on Windows: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, the inject artifacts built (`cargo build -p aether-inject -p aether-shim -p aether-fixtures -p aether-fixture-vproxy`, then `cargo build -p aether-fixture-staticimp`), `cargo test --no-fail-fast` with `TMP`/`TEMP` set to `C:\vfstmp`. A task that touches a portable crate also passes the Linux job's commands for it (`cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo test -p <crate>`), run under WSL or left to CI and checked there.
- Commit messages plain, with no AI attribution of any kind. Add files by name, never `git add -A` or `git add .`.
- Docs and doc comments are plain prose: no marketing words, no em dashes.
- Another agent may still be editing `crates/aether-conform`, `crates/aether-refdirector` and `contract/scenarios` when this plan starts. Tasks 2, 11 and 12 begin from the branch tip after that work is committed, and re-read those files first.

## Review Focus

- The `Tap` must not cost the no-op case anything: `serve_with_arena` is `serve_tapped` with `()`, and `crates/aether-rpc/tests/alloc.rs` still counts 0. Task 1.
- A cache hit is exactly zero requests on the ring, measured by the `Recorder`, not inferred from a tally keyed by path. Tasks 4 to 6.
- The read cache's coherence rule as `crates/aether-ipc/src/readcache/mod.rs` states it: only read-only immutable handles, keyed by (root, folded path, size, mount_gen), poisoned by any write, truncate, delete or rename through this process. Every clause has a test through the hooks, not only through `ReadCache` directly. Task 4.
- `RegClient::cached`: generation 0 caches nothing; an answer is used only while the header holds the generation read before asking; a failed read is never cached. The mid-flight race (the generation moves while the reply is in transit) is tested with a delaying `Tap`. Task 5.
- The bug fixes (Tasks 3A, 3B, 3D) change nothing a well-behaved director's replies produce: every existing aether-hooks test passes unchanged, and each fix's commit names the wire.md sentence it follows. The one fix that adds a request (B3: GETATTR before MKDIR) is visible in Task 7's record.
- B1's reference count: a CLOSE sent twice, or never, when a file handle, a section and several views close in every order. Task 3B tests the orders file-first and section-first.
- B9's in-page error is raised only for a fill that failed or came back short before the file's size; the zero tail past the end of a file stays. Task 3B.
- A fault test that passes because the fault never fired: every case asserts the `Recorder` saw the faulted request and the `Faults` layer reports it applied. Task 9.
- A hang: every hooked call in a fault case runs under a watchdog shorter than the case budget, with the client deadline cut to 300 ms through `install_global`. Task 9.
- Allocation gates that flake across machines: allocation counts are gated as "at most the record" (a path's length differs by machine and can add a reallocation), round trips and ring bytes exactly. Task 7.
- Benchmarks that silently measure the wrong thing: `aether-conform bench` reports the ring floor next to the director's numbers and the difference, and refuses a workload whose replies fail (a benchmark of errors is not a benchmark). Task 12.

---

## Phase 4: the shim-side harness

### Task 1: A tap on the serve loop (model: sonnet)

**Files:**
- Modify: `crates/aether-rpc/src/serve.rs`, `crates/aether-rpc/src/lib.rs`
- Test: `crates/aether-rpc/tests/tap.rs`

**Interfaces:**
- Consumes: `aether_ipc::{RingServer, RequestView, DataArena}`, the existing `dispatch`.
- Produces:
  - `pub trait Tap { fn on_request(&mut self, view: &RequestView<'_>) -> TapAction { TapAction::Pass } fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {} }`
  - `pub enum TapAction { Pass, Answer { status: i32 } }` (`Answer` skips the director; the payload is whatever `on_reply` then leaves in `payload`, empty by default).
  - `impl Tap for ()`, and `impl<A: Tap, B: Tap> Tap for (A, B)` (A sees the request first and the reply last).
  - `pub fn serve_one_tapped<N, D, T>(server, arena: Option<&DataArena<'_>>, d: &mut D, tap: &mut T, scratch: &mut Scratch) -> Result<bool, IpcError>` and `pub fn serve_tapped<N, D, T>(server, arena, d, tap, stop: &AtomicBool) -> Result<(), IpcError>`. `serve_one_with_arena` and `serve_with_arena` become calls of these with `&mut ()`.
  - The tap runs after decode and dispatch, before `server_complete`: a payload longer than `payload_cap` still becomes the ring's overflow (`i32::MIN`, empty), as `docs/wire.md` 2.5 says. A tap cannot make the slot header lie (wrong ack, a length past the slot); those faults are the ring's, tested in Task 10.
- [ ] **Step 1: Failing tests** in `tap.rs` (OwnedSeg ring, spin notifier, a server thread): `a_tap_sees_every_request_with_its_opcode_and_payload`; `on_reply_can_replace_the_status_and_the_payload` (the client gets exactly the rewritten bytes); `answer_skips_the_director` (a counting director sees 0 calls); `a_payload_past_the_cap_becomes_the_overflow_status`; `a_pair_of_taps_runs_in_order` (A's request hook before B's, B's reply hook before A's).
- [ ] **Step 2: Run them, watch them fail** (`cargo test -p aether-rpc --test tap`): missing items.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Verify** `cargo test -p aether-rpc`: all pass, `alloc.rs`'s `a_call_with_a_buffer_that_has_room_allocates_nothing` still asserts 0. Workspace gate.
- [ ] **Step 5: Commit** `aether-rpc: a tap on the serve loop` (add `crates/aether-rpc/src/serve.rs`, `crates/aether-rpc/src/lib.rs`, `crates/aether-rpc/tests/tap.rs`).

### Task 2: Recorder, faults and overrides; two reference knobs (model: opus)

**Files:**
- Create: `crates/aether-refdirector/src/harness.rs` (or `harness/{mod,record,fault,override_}.rs` if it passes 400 lines)
- Modify: `crates/aether-refdirector/src/lib.rs`, `crates/aether-refdirector/src/reference.rs`, `crates/aether-refdirector/Cargo.toml` (`aether-ipc` for `RequestView` if not already a dependency)
- Test: `crates/aether-refdirector/tests/harness.rs`

**Interfaces:**
- Consumes: Task 1's `Tap`; `aether_protocol::{Opcode, Request, Reply, Status, ReadReply}`.
- Produces (all `Clone` handles over `Arc<Mutex<_>>`, so a test thread reads what the serve thread writes):
  - `pub struct Recorder` implementing `Tap`: `fn mark(&self) -> Mark`; `fn since(&self, m: Mark) -> Vec<Recorded>`; `fn count_since(&self, m: Mark, op: Opcode) -> usize`; `fn total_since(&self, m: Mark) -> usize`; `fn ring_bytes_since(&self, m: Mark) -> u64` (request payload plus reply payload plus, for a bulk READ, its `len`).
  - `pub struct Recorded { pub opcode: u32, pub flags: u32, pub payload: Vec<u8>, pub status: i32, pub reply: Vec<u8> }` with `fn op(&self) -> Option<Opcode>` and `fn request(&self) -> Option<Request<'_>>` (decoded on demand, so recording copies bytes and never parses on the serve thread).
  - `pub struct Faults` implementing `Tap`: `fn arm(&self, when: Match, fault: Fault) -> FaultId`; `fn applied(&self, id: FaultId) -> u32`; `fn clear(&self)`.
  - `pub struct Match { pub op: Option<Opcode>, pub path_prefix: Option<String>, pub nth: Option<u32>, pub times: Option<u32> }` (path read from the decoded request; folded comparison).
  - `pub enum Fault { Status(Status), RawStatus(i32), Truncate(usize), Trailing(Vec<u8>), Garbage(Vec<u8>), Oversize, BulkOutside(BulkLie), Delay(Duration), Rewrite(fn(&mut OwnedReplyBytes)) }` and `pub enum BulkLie { PastArena, PastBank, Wrapping, LenOverRequest }`. `Rewrite` is for the typed edits tests need (a STORED_NAMES reply with one name too few); prefer a named variant where one recurs.
  - `pub struct Override` implementing `Tap`: per path prefix, rewrites an OPEN reply's `immutable`, `mount_gen` and `size` (`fn open(&self, prefix: &str, f: OpenEdit)` with `OpenEdit { immutable: Option<bool>, mount_gen: Option<u32>, size: Option<u64> }`), decoding and re-encoding through the typed codec.
  - `Reference::set_mount_gen(&self, g: u32)` (every later OPEN reports it) and `Reference::replace_content(&self, vpath: &str, bytes: Vec<u8>)` (a remount's effect on one file; never called by conformance). Both doc-commented as test knobs; the reference's oracle behaviour with neither called is unchanged.
  - The answers other directors may give and the reference does not (each a builder method, off by default, doc-commented with the wire.md section that allows it; conformance never sets them): `with_fh_base(n: u64)` (handles from `n`, `0` included; S9, B5); `mkdir_existing_ok()` (MKDIR of an existing directory answers `ST_OK`, as a writable provider may, wire.md 3.4.9; S5, B3); `inline_reads()` (every READ answered inline, at most `payload_cap - 8`, whatever the flag: a director outside wire.md 3.4.4, for S3 and B2); `staging(vpath, real_path)` (STAGE_EXEC answers that path; S1); `with_mtime(vpath, mtime)` (S13); `with_lower(vpath, bytes)` and `with_lower_dir(vpath)` (names held only in a read-only lower of a writable root, answered as wire.md 3.4.9's "Names only in a read-only lower" says; S11).
- [ ] **Step 1: Failing tests** (`harness.rs`, OwnedSeg ring, `serve_tapped`, typed `Client`): `the_recorder_keeps_every_request_in_order`; `count_since_counts_one_opcode_after_a_mark`; `ring_bytes_counts_a_bulk_reads_length`; `a_status_fault_fires_on_the_nth_match_only` (and `applied` says 1); `truncate_and_trailing_reach_the_client_as_bytes` (the client's decode fails: `CallError::Undecodable`); `oversize_becomes_overflow`; `each_bulk_lie_names_an_offset_the_arena_does_not_hold` (`PastArena`: offset at or past `arena_offset + arena_len`; `PastBank`: offset in another slot's bank; `Wrapping`: `offset + len` overflows u64); `a_delay_holds_the_reply` (elapsed at least the delay); `an_override_sets_open_fields_and_leaves_the_rest`; `set_mount_gen_reaches_open_replies`; `replace_content_serves_new_bytes_at_a_new_size`; one test per knob above (`fh_base_zero_issues_zero_first`, `mkdir_existing_ok_answers_ok`, `inline_reads_never_answer_bulk`, `staging_answers_the_path`, `with_mtime_reaches_getattr_readdir_and_open`, `a_lower_only_name_is_answered_as_wire_md_3_4_9_says` covering its four rules); `the_reference_without_knobs_answers_as_before` (`tests/reference.rs` and `tests/wire_rules.rs` pass unchanged).
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.** Keep the layers OS-free (`#![forbid(unsafe_code)]` stays).
- [ ] **Step 4: Verify** `cargo test -p aether-refdirector`, `cargo test -p aether-conform` (the oracle unchanged), workspace gate.
- [ ] **Step 5: Commit** `aether-refdirector: a recorder, faults and overrides for the shim's tests`.

### Task 3: The shim tests serve through the harness (model: opus)

**Files:**
- Modify: `crates/aether-hooks/tests/fakedirector/mod.rs`, `crates/aether-hooks/src/director.rs` (one test-only entry point), `crates/aether-testkit/src/lib.rs`
- Create: `crates/aether-testkit/src/alloc.rs`, `crates/aether-hooks/tests/harness/main.rs`, `crates/aether-hooks/tests/harness/recording.rs`
- Modify (mechanical): `crates/aether-protocol/tests/alloc.rs`, `crates/aether-rpc/tests/alloc.rs` (use the shared counter)

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces:
  - `fakedirector::install_with(virtual_dir: &Path, fake: Fake, opts: Options) -> &'static Served` where `Options { slots: u32, payload_cap: u32, arena_len: usize, worker_hint: u32, ring: RingKind, deadline: Option<Duration>, reg_gen: RegGen, attach_faults: Vec<(Match, Fault)>, tap: Option<Box<dyn Tap + Send>> }` (defaults: today's `SLOTS`, `PAYLOAD_CAP`, section ring, hint 0), `RingKind::{Section, SectionWithWake, File}` (`SectionWithWake` creates the server event and sets `VFS_SERVER_EV`, the serve thread waiting on it; `File` serves an `aether-filering` ring and sets `VFS_RING_PATH`), `attach_faults` armed before the client's attach HEARTBEAT (S15), `RegGen::{Reference, Fixed(u64), Manual}` (Fixed(0): the header never holds a generation; Manual: only the test publishes, S16), and `Served { fake: &'static Fake, recorder: Recorder, faults: Faults, overrides: Override }`. The tap chain is always `(recorder, (overrides, faults))`. `install` keeps its signature and calls `install_with` (every existing test unchanged).
  - `aether_hooks::director::install_global(client: FuseClient) -> Result<(), FuseClient>`, `#[doc(hidden)]`: sets the process's client once, for a test that needs `with_deadline`. `install_with` with `deadline: Some(d)` connects the client from the same environment `try_init_from_env` reads, applies `with_deadline(d)` and installs it. No environment variable is added.
  - `aether_testkit::alloc`: `pub struct Counting` (a `GlobalAlloc` forwarding to `System`, counting on armed threads only, thread-local, const-initialised cells) and `pub fn allocations<R>(f: impl FnOnce() -> R) -> (R, usize)`. A test binary declares `#[global_allocator] static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;`. The two existing copies (protocol, rpc) switch to it.
  - Test binary `harness` (`#![cfg(windows)]`, `isolate!`, the counting allocator, modules added by Tasks 4 to 9).
- [ ] **Step 1: Failing tests** in `harness/recording.rs`: `an_open_and_read_cross_the_ring_as_open_read_close` (`CreateFileW` read-only on `Data/a.esp`, `ReadFile` 100 bytes, `CloseHandle`; the recorder since the mark holds exactly `[Open, Read, Close]`, the open's path folded, its flags `READ`); `getattr_of_a_missing_file_is_one_request`; `a_path_outside_the_root_sends_nothing`; `install_global_s_deadline_is_the_client_s` (a `Delay(2 s)` fault on GETATTR with a 300 ms deadline fails `GetFileAttributesExW` in under 1 s); S15: `a_heartbeat_answered_non_ok_fails_the_attach_and_the_hooks_pass_through` (`attach_faults` with `Status(IoError)` on HEARTBEAT: `try_init_from_env` is `Err(ConnectFailed)`, `director::global()` is `None`, a later open under the root sends nothing) and `a_heartbeat_past_the_deadline_fails_the_attach_within_it` (`Delay(2 s)`, deadline 300 ms: `Err` in under 1 s); `allocations_counts_only_this_thread` (in `aether-testkit`'s own test).
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Verify** `cargo test -p aether-hooks` (same pass count as before plus the new ones), `cargo test -p aether-protocol -p aether-rpc -p aether-testkit`, workspace gate.
- [ ] **Step 5: Commit** `aether-hooks tests: the fake director records and can be told what to answer`.

### Task 3A: File-call bugs: mkdir, close, readdir, write (model: opus)

The suspected bugs B3, B4, B5, B6 and B8 of the gap hunt (`gaps-merged.md` section 3). Each starts as a failing test; one that passes on today's shim stays as a test pinning the behaviour, and the task report says so.

**Files:**
- Create: `crates/aether-hooks/tests/harness/bugs_files.rs`
- Modify: `crates/aether-hooks/src/hook/file_open.rs` (`try_fuse_mkdir`), `crates/aether-hooks/src/hook/close.rs`, `crates/aether-hooks/src/synth_file.rs`, `crates/aether-hooks/src/hook/dirquery.rs` (`serve_dir_query`), `crates/aether-hooks/src/director.rs` (`FuseClient::write`), `crates/aether-hooks/src/hook/file_io.rs` (`write_hook_body`), `docs/wire.md` (3.4.2, 3.4.3, 3.4.5, 3.4.9: what the shim does, never what a director must do)

**Interfaces:**
- Consumes: Task 2's knobs (`mkdir_existing_ok`, `with_fh_base`), Task 3's `install_with` (with `payload_cap: 1 MiB` where a listing must not overflow), `Faults`, `Recorder`.
- Produces (fix directions, each under the rulings below):
  - **B3.** `try_fuse_mkdir` asks GETATTR before MKDIR when the disposition is `FILE_CREATE` or `FILE_OPEN_IF`: a directory already there is `STATUS_OBJECT_NAME_COLLISION` for `FILE_CREATE` and success with `FILE_OPENED` for `FILE_OPEN_IF`, without sending MKDIR. wire.md 3.4.9's last sentence of the first bullet becomes the shim's new behaviour (either director answer now gives the same result).
  - **B4.** `serve_dir_query`: a READDIR answered `ST_NOT_FOUND` stays an empty listing (the directory is gone); any other failure (each non-OK status, the overflow status, an undecodable reply, no reply in time) fails the first query with `STATUS_UNSUCCESSFUL` and keeps no `EnumState`, so the next query asks again. wire.md 3.4.2 says so.
  - **B5.** The handle `try_fuse_mkdir` makes is marked as holding no director handle (`synth_file::open_fuse` gains a way to say so, e.g. `fh: Option<u64>` or a flag); `close.rs` sends CLOSE only for a handle the director issued. Any u64 stays a valid `fh` (wire.md 3.4.3 unchanged).
  - **B6.** Pinned, not changed: a write open sends `OPEN_WRITE` plus the create bits (wire.md 3.4.3) and the shim reads and truncates through it. Before writing the test, check the production director (its ring server and its providers) and the reference serve READ and SETATTR on such a handle; if both do, wire.md 3.4.3 gains "a handle opened with `OPEN_WRITE` serves READ and SETATTR too; the shim reads through it". If the production director refuses, stop and report: the fix is then the shim sending `OPEN_READ | OPEN_WRITE` when the program asked for read access, which is a request change to bring to the user. Cross-reference: conformance C19 (other branch) checks the director side.
  - **B8.** `FuseClient::write` returns what landed when a later chunk fails: `Err(WriteFailed { written, status })` (or an equivalent pair); `write_hook_body` sets `IoStatus.Information` to `written`, advances the position and raises the handle's size by it, and returns `STATUS_DISK_FULL` for `ST_NO_SPACE`, `STATUS_UNSUCCESSFUL` for every other status. A `written` larger than the chunk is a reply the shim cannot use: that chunk counts as not written and the call fails `STATUS_UNSUCCESSFUL`.
- [ ] **Step 1: Failing tests** in `bugs_files.rs`:
  - `create_directory_of_an_existing_directory_fails_even_when_mkdir_answers_ok` (B3, S5: `mkdir_existing_ok`; `CreateDirectoryW` of an existing directory: `false`, `GetLastError() == ERROR_ALREADY_EXISTS`; recorder: GETATTR, no MKDIR). `open_if_of_an_existing_directory_reports_opened` (`NtCreateFile` with `FILE_OPEN_IF | FILE_DIRECTORY_FILE`: `Information == FILE_OPENED`). `create_directory_of_a_new_name_still_costs_getattr_then_mkdir`.
  - `a_readdir_that_fails_fails_the_enumeration` (B4, S2: `Status(IoError)` on READDIR: `FindFirstFileW` returns `INVALID_HANDLE_VALUE`, `GetLastError()` the Win32 error of `STATUS_UNSUCCESSFUL`, `ERROR_GEN_FAILURE`); `an_overflowing_listing_fails_the_enumeration` (a directory whose listing passes the 4 KiB cap: same result; the overflow status reached the shim, per the recorder); `a_readdir_of_a_vanished_directory_is_empty` (`Status(NotFound)`: `ERROR_FILE_NOT_FOUND` from `FindFirstFileW`, as for an empty directory today); `a_failed_listing_is_asked_again` (fault once, then the next `FindFirstFileW` lists every entry).
  - `a_listing_of_3000_entries_is_resumed_over_many_buffers_complete_and_in_order` (S2: `payload_cap: 1 MiB`, `NtQueryDirectoryFile` with a 4 KiB buffer in a loop: every name once, in the director's order, one READDIR on the recorder).
  - `closing_a_new_directory_sends_no_close` (B5, S9: `with_fh_base(0)`; open a file (fh 0), `CreateDirectoryW`, close the directory handle; recorder: no CLOSE; `ReadFile` on the file succeeds).
  - `a_read_write_handle_reads_back_what_it_wrote` (B6, S12: `CreateFileW(GENERIC_READ | GENERIC_WRITE)` in a writable prefix, write, seek, read: the bytes; `SetEndOfFile` works; the recorded OPEN's flags are `WRITE` plus create bits).
  - `a_write_of_three_chunks_and_a_byte_sends_four_writes_at_the_right_offsets` (S4: `payload_cap` 4096, so chunks of 4072; recorder offsets `0, 4072, 8144, 12216`); `a_later_chunk_failing_reports_what_landed` (B8: `Status(IoError)` with `nth: 2` on WRITE: `WriteFile` fails, `lpNumberOfBytesWritten == 4072`, the file pointer is at 4072, `GetFileSizeEx` 4072); `no_space_is_disk_full` (`Status(NoSpace)` on the first chunk: `ERROR_DISK_FULL`); `a_written_count_past_the_chunk_is_not_believed` (`Rewrite` of the reply to `len + 100`: failure, pointer not past the data).
- [ ] **Step 2: Run** `cargo test -p aether-hooks --test harness bugs_files`; record which fail (the bugs confirmed) and which pass (pinned).
- [ ] **Step 3: Fix** the confirmed ones as above; every existing aether-hooks test still passes; update wire.md's shim sentences.
- [ ] **Step 4: Workspace gate; commit** one commit per bug, e.g. `aether-hooks: CreateDirectoryW of an existing directory fails whatever MKDIR answers`, `aether-hooks: a failed READDIR fails the enumeration`, `aether-hooks: a new directory's handle closes nothing at the director`, `aether-hooks: a write that fails part way reports what landed`, and `aether-hooks tests: reading through a write handle, pinned`.

### Task 3B: Sections through a director (model: opus)

The section paths with a director behind them (S6) and the bugs B1 and B9. `hook/section.rs` fills a section up to 256 MiB eagerly at `NtCreateSection`; above that `lazy_section.rs` fills pages on fault through the stored `fh`.

**Files:**
- Create: `crates/aether-hooks/tests/harness/sections.rs`
- Modify: `crates/aether-hooks/tests/common/mod.rs` (`isolate_expecting_exit!(code)`: the child must end with that exit code), `crates/aether-hooks/src/lazy_section.rs`, `crates/aether-hooks/src/hook/section.rs`, `crates/aether-hooks/src/hook/close.rs`, `crates/aether-hooks/src/synth_file.rs`

**Interfaces:**
- Consumes: Tasks 2 and 3. The lazy cases need a file over `EAGER_MAX` (256 MiB): a 260 MiB `pattern` file in the reference (if `EAGER_MAX` gains a test seam under the `test-support` feature of ruling 3 in "Open questions, ruled", use 8 MiB instead and say so).
- Produces (fix directions):
  - **B1.** A lazy region holds its director handle: `close.rs` sends CLOSE for an `fh` only when no lazy region refers to it; the last region's release (section and last view gone) sends it. Reference counted in one place (`lazy_section` or `synth_file`), under the lock the close already takes.
  - **B9.** A fill that fails, or comes back short of `min(readable, size - offset)` (a short answer at the file's end is the end, wire.md 3.4.4), commits nothing for that page. Lazy: the fault handler retries the READ up to 3 times within the client's deadline; then it raises `STATUS_IN_PAGE_ERROR` (0xC0000006) with the faulting address, as NT does when a mapped file's paging read fails. Eager: `NtCreateSection` fails `STATUS_UNSUCCESSFUL` (the shim's generic status, Task 8's table) and leaves no section behind.
- [ ] **Step 1: Failing tests** in `sections.rs`:
  - `a_1_mib_section_over_a_director_file_maps_its_bytes` (`CreateFileMappingW` and `MapViewOfFile`: the view equals `pattern`; recorder: bulk READs only, no read after the section call).
  - `an_image_section_of_a_served_dll_loads` (`LoadLibraryExW(..., LOAD_LIBRARY_AS_IMAGE_RESOURCE)` or `SEC_IMAGE` through `NtCreateSection` on a served PE fixture: succeeds, the headers match).
  - `a_lazy_section_fills_on_first_touch_only` (260 MiB: touching a page at 100 MiB sends READs for that chunk; touching it again sends none).
  - `a_lazy_section_still_reads_after_its_file_handle_is_closed` (B1: map, `CloseHandle(file)`, read the page at 100 MiB: equals `pattern`; recorder: no CLOSE until `UnmapViewOfFile` and `CloseHandle(section)`, then exactly one).
  - `a_failed_eager_fill_fails_the_section` (B9: `Status(IoError)` on the second bulk READ of a 4 MiB section: `CreateFileMappingW` returns null; the next try without the fault maps the right bytes).
  - `a_failed_lazy_fill_is_retried_at_the_next_touch` (B9: one `IoError` on the fill's READ, then none: the page holds `pattern`, never zeros).
  - `a_lazy_fill_that_keeps_failing_raises_an_in_page_error` (B9: `IoError` with no `times` limit; `isolate_expecting_exit!(0xC0000006)`).
  - `a_short_fill_at_the_file_s_end_is_zero_filled_as_nt_does` (pins the correct case: the tail page past the file's size reads zeros).
- [ ] **Step 2: Run, record which fail; Step 3: fix** B1 and B9 as above (other aether-hooks tests unchanged).
- [ ] **Step 4: Workspace gate; commit** `aether-hooks: a lazy section keeps its director handle until its last view goes`, `aether-hooks: a failed page fill is retried, then an in-page error, never zeros`, `aether-hooks tests: sections over director files`.

### Task 3C: Child start through STAGE_EXEC (model: opus)

S1: no test presents STAGE_EXEC today, and every launcher-to-game start goes through it in the production director.

**Files:**
- Create: `crates/aether-hooks/tests/harness/child_start.rs`

**Interfaces:**
- Consumes: Task 2's `staging`; `hook/child_start.rs` (`plan`), `hook/process.rs`, `FuseClient::stage_exec`; the child-injection setup of `tests/diagnostics/child_inject_fails_closed.rs` (reuse it; the child here may be refused injection, which is not what is tested).
- Produces: tests (wire.md 3.4.14 and 5.3 are the authority; a failure is reported and fixed only under the Global Constraints' rule):
  - `a_child_started_under_a_root_runs_the_staged_copy` (a served `aether-probe.exe` staged to a real temp path; `CreateProcessW` with the virtual path: the started process's image (`QueryFullProcessImageNameW`) is the staged path; recorder: one STAGE_EXEC).
  - `stage_exec_not_found_refuses_the_start` (`Status(NotFound)`: `CreateProcessW` fails, and no process ran the real file under the root).
  - `no_stager_refuses_an_image_under_a_root_with_no_staged_copy` (`NotSupported`, no `VFS_ROOT_BACKINGS`: refused as `child_start.rs` says).
  - `an_undecodable_stage_exec_reply_refuses_the_start` (`Garbage` of invalid UTF-8).
  - Cross-reference: conformance C13 (other branch) checks that a director's STAGE_EXEC answer is usable.
- [ ] **Step 1: Write the tests; run; Step 2: workspace gate; commit** `aether-hooks tests: child starts through STAGE_EXEC`.

### Task 3D: Large reads (model: opus)

S3, S7 and B2.

**Files:**
- Create: `crates/aether-hooks/tests/harness/reads_large.rs`
- Modify: `crates/aether-hooks/src/director.rs` (`FuseClient::connect_source`: the arena rule below), `docs/wire.md` 2.6 (one sentence on what the shim does with an arena too small)

**Interfaces:**
- Consumes: Task 2's `inline_reads`; Task 3's geometry options.
- Produces:
  - **B2, the inline half: pinned, not changed.** wire.md 3.4.4 already says a director answers a bulk-flagged READ in bulk and that `n < len` is end of file, and 2.6 that each bank must be at least the shim's chunk. A director that answers a 1 MiB bulk request inline breaks those rules; conformance C1 (other branch) catches it. The shim keeps reading a short answer as end of file.
  - **B2, the bank half: fixed.** A ring whose `VFS_ARENA_LEN` is below `256 KiB x slot_count` (wire.md 2.6's rule) is used without its arena: the shim sends inline reads of `payload_cap - 8`, which such a director answers in full, instead of 256 KiB bulk requests it answers short. A well-behaved director's ring is unaffected.
- [ ] **Step 1: Failing tests:**
  - `one_read_of_1_mib_returns_1_mib_in_one_call` (a single `NtReadFile` of 1 MiB on a 2 MiB file, arena on: `Information == 1 MiB`; not `std::fs::read`, which loops).
  - `an_inline_only_director_truncates_a_bulk_read_as_the_contract_says` (B2 pin: `inline_reads`, the same call: `Information == 1_048_568` at a 1 MiB cap; the test's doc names wire.md 3.4.4 and C1).
  - `an_arena_below_the_rule_is_not_used` (B2 fix: `arena_len = SLOTS x 128 KiB`: the recorder shows no bulk flag, and one-call and `std::fs::read` reads of 2 MiB are byte-exact).
  - `a_section_over_an_inline_only_director_is_short_filled_as_the_contract_says` (pin, eager section of 2 MiB).
  - `a_5_mib_read_uses_the_deep_pipeline_and_is_byte_exact` (S7: recorder shows up to 8 READs in flight, i.e. 8 distinct slots between two completions; bytes equal).
  - `a_stream_read_does_not_starve_another_thread_s_getattr` (S7: one thread reads 5 MiB through `NtReadFile` and another through `NtCreateSection` while a third calls `GetFileAttributesExW` in a loop: every getattr completes in under the 300 ms deadline).
- [ ] **Step 2: Run; Step 3: implement the arena rule; Step 4: workspace gate; commit** `aether-hooks: an arena smaller than wire.md 2.6 allows is not used` and `aether-hooks tests: large reads, one call and pipelined`.

### Task 3E: Production geometry and the file ring (model: sonnet)

S10.

**Files:**
- Create: `crates/aether-hooks/tests/harness/geometry.rs`

**Interfaces:**
- Consumes: Task 3's `Options` (`slots`, `payload_cap`, `arena_len`, `worker_hint`, `RingKind`).
- Produces: one battery, `fn battery(served: &Served)` (open, small read, 2 MiB read, getattr, readdir of 200 entries, write of 3 chunks, rename, delete, close; each result checked against the reference's contents), run per geometry in its own `isolate!` test: `production_geometry_1_mib_cap_32_slots_1_mib_banks`; `banks_of_4_mib`; `one_slot`; `two_slots`; `worker_hint_1`; `worker_hint_16`; `worker_hint_above_slot_count`; `the_server_event_wakes_the_director` (`RingKind::SectionWithWake`, the serve thread waits on the event only, no spin: the battery completes); `a_file_ring_through_the_hooks` (`RingKind::File`: the Wine and Proton transport, `VFS_RING_PATH` winning over a stale `VFS_RING_SECTION` also set).
- [ ] **Step 1: Write; run** `cargo test -p aether-hooks --test harness geometry`; a failure is reported, then fixed under the Global Constraints' rule.
- [ ] **Step 2: Workspace gate; commit** `aether-hooks tests: every geometry a director may choose, and the file ring`.

### Task 4: The read cache through the hooks (model: opus)

The shim's caches, surveyed in `crates/aether-hooks/src` and what it calls (each gets its tests in Tasks 4 to 7):

| Cache | Where | Key and coherence | Ring cost of a hit |
|---|---|---|---|
| Read cache | `aether_ipc::readcache` (`ReadCache`), fed by `crates/aether-hooks/src/read_cache.rs`, `hook/file_open.rs` (register), `hook/file_io.rs` (read, write), `hook/file_mutate.rs` and `hook/close.rs` (invalidate) | (root, folded path) per file, version (size, mount_gen); only read-only immutable handles; poisoned by a write open, a mutable open, write, truncate, delete, rename | 0 |
| Registry answers | `RegClient::cached`, `crates/aether-hooks/src/regclient.rs` | REG_LOOKUP and REG_KEY per folded path, under the `reg_gen` read before asking; nothing at `reg_gen` 0; 4096 entries then cleared | 0 |
| Spellings | `aether_redirect::finalname::NameCache` in `FuseClient::names` (`director.rs`) | (root, folded path) per component; 2 s TTL; 8192 entries; forgotten on create, rename, delete through this process (`names_changed`) | 0 when every component is known; else one STORED_NAMES with `skip` |
| Root membership | `aether_redirect` `PathCache` in `RootMap` (`crates/aether-redirect/src/cache.rs`) | raw path string; not filled under `UncachedScope` (OS-consulted answers) | none (never crosses the ring) |
| Per-handle state | `crate::synth_file` table (size, position, append, delete-on-close) | per synthetic handle, from the OPEN reply, raised by writes, set by truncate | 0 for handle queries (`NtQueryInformationFile` size classes) |
| Not-a-key handles | `regkeys.rs` `NOT_OURS` | handle value; dropped on close; 1024 then cleared | none (saves a syscall) |
| Image copies | `hook/section.rs` `pe-cache` on disk | vpath plus the image's bytes (`aether_pe::image_cache_name`) | not a coherence cache of director answers: content-addressed |
| None | GETATTR by path, a failed OPEN | no attribute or negative cache: every query crosses | recorded in Task 7, so adding one shows as a lower count |

**Files:**
- Create: `crates/aether-hooks/tests/harness/caches_read.rs`
- Modify: `crates/aether-hooks/tests/harness/main.rs`

**Interfaces:**
- Consumes: Task 3's `install_with`, `Recorder`, `Override`, `Reference::set_mount_gen`, `replace_content`.
- Produces: tests only. Files are 1 MiB `pattern` bytes in a read-only part of the tree (the reference answers them immutable), the arena on (`ARENA_LEN`), reads through `ReadFile`/`seek_read` with the hooks installed.
- [ ] **Step 1: Write the tests** (they should pass against today's shim; a failure is a finding, reported before anything is changed):
  - `a_small_read_of_an_immutable_file_fetches_one_block_then_hits_at_zero_round_trips` (first 4 KiB read: one READ with the bulk flag, length 64 KiB; the next 15 reads inside that block: `total_since == 0`; bytes equal `pattern`).
  - `a_mutable_file_is_read_through_every_time` (writable prefix: every small read is one READ).
  - `an_immutable_answer_on_a_write_handle_is_not_cached` (`Override` immutable on a writable file opened for write: reads cross).
  - `a_write_open_anywhere_poisons_the_file_for_the_process` (cached, then a write open of the same path under an `Override`: later reads through the first handle cross).
  - `a_second_handle_spelled_differently_shares_the_blocks` (`Data/A.esp` then `data/a.ESP`: the second handle's read of a held block costs 0).
  - `the_same_path_under_another_root_is_another_file`.
  - `a_new_mount_gen_refetches_and_serves_the_new_bytes` (cached at gen 1; `replace_content` and `set_mount_gen(2)`; reopen: one READ, new bytes; the old handle, still open, serves its own version or crosses, as `readcache` documents: assert what the doc says).
  - `a_changed_size_at_the_same_mount_gen_refetches` (`Override` size).
  - `delete_and_rename_through_this_process_drop_the_file` (and everything under a renamed directory).
  - `a_read_at_or_above_the_threshold_bypasses_the_cache` (one READ of its own size, nothing cached: a later small read in that range still fetches).
  - `the_switch_turns_it_off` (`VFS_SHIM_READ_CACHE=0`: every small read crosses).
  - `a_failed_block_fetch_falls_back_to_an_uncached_read_with_the_same_bytes` (`Faults`: IoError on the first bulk READ only).
- [ ] **Step 2: Run** `cargo test -p aether-hooks --test harness caches_read`. Expected: all pass. A test that fails on today's shim is reported with the recorder's request list; it is fixed only under the Global Constraints' rule for shim changes.
- [ ] **Step 3: Workspace gate; commit** `aether-hooks tests: the read cache, request by request`.

### Task 5: The registry answer cache (model: opus)

**Files:**
- Create: `crates/aether-hooks/tests/harness/caches_registry.rs`
- Modify: `crates/aether-hooks/tests/harness/main.rs`; reuse `crates/aether-hooks/tests/common/reg.rs` (move what this binary needs; no copy)

**Interfaces:**
- Consumes: Task 3 (`RegGen::Fixed`, `Faults::Delay`), `RegClient`, `regclient::detours_installed`.
- Produces: tests:
  - `a_repeated_lookup_and_key_read_cost_zero_round_trips` (recorder total, not the per-path tally).
  - `at_generation_zero_nothing_is_cached` (`RegGen::Fixed(0)`: each of five reads is one request).
  - `a_generation_bump_from_outside_refetches` (`aether_ipc::ring::publish_reg_generation` by the test, as another process's write would: the next read crosses, the one after hits).
  - `an_answer_in_flight_while_the_generation_moves_is_not_used_after_it` (a `Delay` on REG_KEY; the test bumps the generation during the delay; the read returns the answer, and the next read crosses).
  - `a_failed_read_is_not_cached` (`NotSupported` once, then OK: two requests, the second answer served, the third read hits).
  - `a_write_through_this_process_moves_the_generation_before_its_reply` (after `set_value`, the next `key` crosses and sees the value).
  - `the_cache_starts_over_past_its_bound` (4097 distinct keys, then the first again: it crosses).
  - Through the hooks (`RegOpenKeyExW` + `RegQueryValueExW` twice on an overlay key): the second query costs 0 round trips.
  - S14: `a_create_answered_exists_then_lookup_absent_fails_unsuccessful` (`Status(Exists)` on REG_CREATE_KEY, the key really absent: `RegCreateKeyExW` returns the Win32 error of `STATUS_UNSUCCESSFUL`; recorder: CREATE_KEY then one REG_LOOKUP, wire.md 4.4); `a_create_answered_exists_then_lookup_present_opens_the_key`.
  - S16: `a_director_that_bumps_after_replying_leaves_a_second_client_stale` (`RegGen::Manual`: client B writes, the test has not bumped yet, client A's cached answer is served stale; then the test bumps and A refetches). This pins why wire.md 4.5 requires the bump before the reply; cross-reference conformance C9 (other branch), which checks directors do it.
- [ ] **Step 1: Write the tests; run** `cargo test -p aether-hooks --test harness caches_registry`. Expected: pass; failures reported as in Task 4.
- [ ] **Step 2: Workspace gate; commit** `aether-hooks tests: the registry cache against the published generation`.

### Task 6: Spellings and per-handle state (model: opus)

**Files:**
- Create: `crates/aether-hooks/tests/harness/caches_names.rs`, `crates/aether-hooks/tests/harness/caches_handles.rs`, `crates/aether-hooks/tests/harness/overlay_answers.rs`
- Modify: `crates/aether-hooks/src/director.rs` and `crates/aether-hooks/Cargo.toml` (the clock seam, feature `test-support`, off in the shipped DLL)
- Modify: `crates/aether-hooks/tests/harness/main.rs`

**Interfaces:**
- Consumes: Task 3; `GetFinalPathNameByHandleW` (the `NameCache` path), `GetFileInformationByHandleEx`.
- Produces: tests:
  - `caches_names.rs`: `a_repeated_final_path_query_costs_one_stored_names_then_none`; `a_file_under_a_known_directory_asks_only_for_its_last_component` (the recorded STORED_NAMES has `skip` equal to the known depth); `a_spelling_is_asked_again_after_its_ttl` (no sleep: `FuseClient`'s clock through the `test-support` seam of "Open questions, ruled" 3, advanced past `NAME_TTL_MS`; `crates/aether-hooks/src/director.rs` and `Cargo.toml` gain the seam in this task); `creating_renaming_or_deleting_through_this_process_forgets_the_path_and_below`; `a_reply_with_the_wrong_number_of_names_teaches_nothing` (`Fault::Rewrite` drops one name: the answer is the caller's spelling, and the next query crosses); `a_failed_stored_names_leaves_the_caller_s_spelling_and_caches_nothing`; S8: `stored_names_not_found_falls_back_and_teaches_nothing` (`Status(NotFound)`, as the production director answers for a path it lacks: `GetFinalPathNameByHandleW` gives the caller's spelling, and the same query again sends STORED_NAMES again); S11, with `with_lower`: `a_case_only_rename_of_a_lower_only_name_keeps_the_lower_s_spelling` (`MoveFileExW("Data/Lower.esp", "Data/LOWER.esp")` succeeds; the next `GetFinalPathNameByHandleW` and `FindFirstFileW` give `Lower.esp`: `names_changed` dropped the old entry and STORED_NAMES was asked again).
  - `caches_handles.rs` also gets the S9 handle values (`with_fh_base`): `handles_above_u32_max_and_with_the_top_bit_set_work` (open, read, write, close each with `fh = 2^32 + 1` and `2^63 + 1`); `an_fh_reissued_after_close_inherits_nothing` (close a delete-on-close handle and a read-cached handle, open another file the reference gives the same `fh`: closing it deletes nothing, and its reads are not served from the first file's blocks).
  - `caches_handles.rs`, S13: `a_nonzero_mtime_agrees_across_basic_info_directory_records_and_full_attributes` (`with_mtime`: `FileBasicInformation`, the `FindFirstFileW` record and `NtQueryFullAttributesFile` report one `LastWriteTime`).
  - `overlay_answers.rs` (new module, S11 and S17, `with_lower`): `deleting_a_lower_only_directory_with_children_hides_it` (a delete of the directory through `NtSetInformationFile(FileDispositionInformation)`: success, then `FindFirstFileW` of the parent omits it); `renaming_a_lower_only_directory_fails_as_io_error_maps` (the status of Task 8's table for RENAME `ST_IO_ERROR`); `mkdir_over_a_lower_only_file_makes_a_directory` (`CreateDirectoryW` succeeds, `GetFileAttributesW` has `FILE_ATTRIBUTE_DIRECTORY`); S17: `an_open_reply_with_reserved_flag_bits_set_is_read_as_its_bit_0` (`Override` setting bits 1 to 7: the handle works, immutable read as bit 0 only); `a_directory_size_that_is_not_zero_is_shown_as_sent` (pins wire.md 3.4.2's copy into both record fields); `rename_or_delete_of_the_root_answered_io_error_fails_as_the_table_says`.
  - `caches_handles.rs`: `size_and_position_queries_on_a_director_handle_cost_zero_round_trips`; `a_write_past_the_end_raises_the_handle_s_size_without_asking`; `getattr_by_path_crosses_every_time` (three `GetFileAttributesExW` calls: three GETATTR; documents that there is no attribute cache); `a_failed_open_s_requests_are_what_they_are` (records the requests an open of a missing file sends, OPEN and any GETATTR the open hook adds, as the baseline Task 7 gates).
- [ ] **Step 1: Write the tests; run** `cargo test -p aether-hooks --test harness caches_`. Expected: pass.
- [ ] **Step 2: Workspace gate; commit** `aether-hooks tests: spellings and handle state cost no round trips`.

### Task 7: Counted costs per hooked call (model: opus)

**Files:**
- Create: `crates/aether-hooks/tests/harness/costs.rs`, `crates/aether-hooks/tests/harness/costs.txt`
- Modify: `crates/aether-hooks/tests/harness/main.rs`

**Interfaces:**
- Consumes: Task 3 (`Recorder`, `aether_testkit::alloc::allocations`).
- Produces:
  - `costs.txt`: one line per case, `<case> round_trips=<n> ring_bytes=<n> allocs<=<n>`, sorted, with a header comment saying how to update it. Cases (each measured warm, after one untimed call that fills one-time state, and a `_cold` row where the first call is the point): open read-only in the root; open of a missing file; open for write in a writable prefix; close; `GetFileAttributesExW` in the root and outside it (passthrough: 0 round trips); small read, read-cache hit; small read, mutable file; bulk read of 1 MiB; write of 4 KiB; `FindFirstFileW`+`FindNextFileW` over 100 entries; `GetFinalPathNameByHandleW` cold and warm; `MoveFileExW`; `DeleteFileW`; `NtQueryInformationFile` size class on a director handle; `RegQueryValueExW` on an overlay key cold and warm; `NtQueryKey` on a not-a-key handle; from the gap hunt: `WriteFile` of three chunks and a byte (S4: 4 round trips); `CreateDirectoryW` (after Task 3A's B3 fix: GETATTR plus MKDIR); a 1 MiB eager section and one lazy page fill (S6); a 5 MiB stream read (S7); `CreateProcessW` under a root (S1: one STAGE_EXEC).
  - One `#[test]` per case (so `isolate!` gives each its own process), each calling `check(case, measured)`: round trips and ring bytes must equal the record; allocations must be at most the record. A count that went up fails with the old and new value; one that went down passes and prints a line asking for the record to be lowered. `AETHER_COSTS_UPDATE=1` makes `check` write the measured values into `costs.txt` instead (the parent test process merges the children's lines; or each child appends to a temp file the parent folds in: builder's choice, documented in the file header).
  - `docs/wire.md` is not touched; the costs are the shim's, documented in the header of `costs.rs`.
- [ ] **Step 1: Write `costs.rs` with an empty `costs.txt`; run with `AETHER_COSTS_UPDATE=1`** to record today's values. Read every row: a number that looks wrong (a cache hit with a round trip, a passthrough with one, an allocation count in the hundreds) is reported before it is recorded.
- [ ] **Step 2: Run without the switch.** Expected: all pass. Then raise one recorded value by hand and lower another, run again, and see one fail and one print; restore.
- [ ] **Step 3: Workspace gate; commit** `aether-hooks tests: round trips and allocations per hooked call, recorded` (add `costs.rs`, `costs.txt`, `main.rs`).

### Task 8: What the shim makes of a reply it cannot use (model: opus)

**Files:**
- Modify: `docs/wire.md` (new section 3.7; 4.4 cross-referenced)
- Create: `crates/aether-hooks/tests/harness/fault_table.rs` (the table as data, shared by Task 9, and a test that it matches wire.md)

**Interfaces:**
- Consumes: the shim's code (`director.rs` `call`, `call_empty`, `read_fragmented`, `CallError`, `wire_code`, `failed`; `hook/file_attr.rs`, `hook/file_open.rs`, `hook/file_io.rs`, `hook/dirquery.rs`, `hook/file_mutate.rs`, `regwrite.rs` `write_status`, `regkeys.rs`), wire.md 3.4.x, 3.5, 4.4.
- Produces:
  - wire.md 3.7, "What the shim makes of a reply it cannot use": per hooked operation (attach HEARTBEAT (S15: the load fails, `fuse-failed:` in the ready file, wire.md 6.2), open, getattr by path, read inline, read bulk, a section fill (Task 3B's rule), write (first chunk and a later chunk, Task 3A's B8 rule), setattr, readdir (Task 3A's B4 rule), rename, delete, mkdir, stored names, stage exec, registry read, registry write), the NTSTATUS for each fault class: each `Status`; an unknown negative status; a positive status; the overflow status; a payload that does not decode (truncated, trailing bytes, garbage); a bulk reply outside the arena or past the request; no reply within the deadline; a late reply. Where the code already states it (3.4.1's GETATTR rule, 3.4.13's `STATUS_NOT_SAME_DEVICE`, 4.4's registry table), 3.7 points there. This documents today's behaviour; it does not change it.
  - `pub(crate) fn expected(op: HookedOp, fault: FaultClass) -> Expect` where `Expect::{Nt(NTSTATUS), OneOf(&'static [NTSTATUS]), Fallback}` (`Fallback`: the call succeeds from the real system, as a registry read does).
  - `the_wire_document_s_fault_table_is_the_test_s`: parses the 3.7 table from `docs/wire.md` and compares it cell by cell with `expected`.
- [ ] **Step 1: Survey** the shim's error paths and write the table. Where the survey finds a fault that today panics, hangs or returns success with wrong data, write that row as what the contract should say (an error status), mark it in the task report, and leave it to Task 9 to fix.
- [ ] **Step 2: Failing test** `the_wire_document_s_fault_table_is_the_test_s`, then the table in both places; run it, pass.
- [ ] **Step 3: Workspace gate; commit** `wire.md: what the shim does with a reply it cannot use`.

### Task 9: Generated faults never crash or hang the shim (model: opus)

**Files:**
- Create: `crates/aether-hooks/tests/harness/faults_file.rs`, `crates/aether-hooks/tests/harness/faults_registry.rs`
- Modify: `crates/aether-hooks/tests/harness/main.rs`, `crates/aether-hooks/Cargo.toml` (dev-dependency `proptest`); shim code only where a fault finds a crash or hang

**Interfaces:**
- Consumes: Tasks 3 and 8; `aether_hooks::hookstats::hook_panics_total`.
- Produces:
  - A proptest per binary module (`ProptestConfig { cases: 128, failure_persistence: None, .. }`, seed printed, `PROPTEST_CASES` respected) over `(HookedOp, FaultClass, parameters)`: `Truncate(n)` for every `n` below the reply's length, `Trailing` of 1 to 16 bytes, `Garbage` up to `payload_cap`, every `Status`, `RawStatus` from a small set (`-99`, `1`, `i32::MAX`, `i32::MIN`), each `BulkLie`, `Delay` either under the deadline or past it.
  - Each case: arm the fault on the operation's opcode for one firing, run the Win32 call on a worker thread, join with a watchdog of deadline plus 2 s. Asserts: the watchdog did not fire; `hook_panics_total()` did not move; the recorder saw the request; `faults.applied(id) == 1` (or 0 only for a case documented as not reaching the ring, with the reason in the strategy); the result is `expected(op, fault)`; the next unfaulted call of the same operation succeeds (the shim keeps serving).
  - The operations include the gap hunt's: READDIR with `RawStatus(i32::MIN)` and every status (S2, against Task 3A's B4 rule); WRITE of three chunks with the fault on chunk `nth` 1, 2 or 3 (S4, Task 3A's B8 rule); STAGE_EXEC through `CreateProcessW` (S1); a section fill, eager only (a lazy fill that keeps failing ends the process, which Task 3B tests on its own). The attach HEARTBEAT is not in the property (one attach per process); Task 3 tests it.
  - Client deadline 300 ms (`Options::deadline`). Late replies land in abandoned slots; the case after them must still succeed.
  - A fault that crashes, hangs or corrupts is a shim bug: fixed in this task in `crates/aether-hooks/src` (or `aether-ipc` for the client half of the ring), with the failing case kept as a named regression test (`#[test] fn regression_<op>_<fault>`), and the fix checked against Task 7's costs (no row goes up).
- [ ] **Step 1: Write the properties; run** `cargo test -p aether-hooks --test harness faults_`. Each failure: shrink, add the regression test, fix, rerun.
- [ ] **Step 2: Measure the run time** on the Windows CI runner shape; keep both modules under about 90 s together (lower `cases` rather than shorten the deadline).
- [ ] **Step 3: Workspace gate; commit** `aether-hooks tests: generated faults against the shim` (and, if a fix was needed, a separate earlier commit per fix: `aether-hooks: <what the fault did> no longer <crashes|hangs>`).

### Task 10: Fuzz targets for the decoders (model: sonnet)

**Files:**
- Create: `crates/aether-fuzz/{Cargo.toml,src/lib.rs}`, `crates/aether-fuzz/tests/replay.rs`, `fuzz/Cargo.toml` (own `[workspace]`), `fuzz/rust-toolchain.toml` (a dated nightly, e.g. `nightly-2026-10-28`, with a comment saying why it is pinned), `fuzz/fuzz_targets/{reply_decode,request_decode,shim_cfg,ring_response,dir_records}.rs`, `fuzz/regressions/README.md` (one line: crashes found go here, by target), `.github/workflows/fuzz.yml`
- Modify: root `Cargo.toml` (member `crates/aether-fuzz`, `exclude = ["fuzz"]`), `.github/workflows/ci.yml` (`-p aether-fuzz` in `LINUX_CRATES`)

**Interfaces:**
- Consumes: `aether_protocol::{Reply, Request, Opcode, decode_config}`, `aether_ipc::{OwnedSeg, ring}`, `aether_ntlayout` directory records.
- Produces:
  - `aether_fuzz` (portable, no unsafe): `pub fn reply_decode(data: &[u8])` (first 5 bytes choose opcode and request flags; decode the rest as that reply; for READDIR iterate every entry; for REG_KEY walk the node); `pub fn request_decode(data: &[u8])` (the director side's decoder, same framing); `pub fn shim_cfg(data: &[u8])`; `pub fn ring_response(data: &[u8])` (an initialised ring in an `OwnedSeg`, one slot COMPLETED with header fields and payload from `data`; `take_response_into` must return `Ok`, `Err` or `None`, never panic or read outside the segment); `pub fn dir_records(data: &[u8])` (a decoded READDIR listing written as NT directory records into a fixed buffer of a length from `data`). Each body's contract: it returns; a panic is the bug.
  - `pub fn seeds() -> Vec<(&'static str, Vec<u8>)>`: target name and input, built from `vectors.txt` (included with `include_str!`), so no binary seeds are checked in; `fuzz/regressions/<target>/*` holds crashes once found.
  - `tests/replay.rs` (stable, every push, both OSes): every seed and every file under `fuzz/regressions/` through its body; plus a proptest per body over random bytes (`cases: 2048`).
  - cargo-fuzz targets: one `fuzz_target!` each, calling the body.
  - `fuzz.yml`: weekly schedule and `workflow_dispatch`, `ubuntu-latest`, `cargo install cargo-fuzz --locked --version <pinned>`, seeds the corpus with `cargo run -p aether-fuzz --example write-seeds -- fuzz/corpus` (an example in `aether-fuzz`), runs each target `-- -max_total_time=300`, uploads `fuzz/artifacts` on failure. Never part of `ci.yml`.
  - Ruling (in `fuzz/Cargo.toml`'s comment and the task report): cargo-fuzz runs on Linux only. It needs nightly, and its sanitizer support on `x86_64-pc-windows-msvc` is not something to build CI on; the decoders are portable, so Linux covers them, and the Windows-only code that handles their output is covered by Task 9.
- [ ] **Step 1: Failing test** `tests/replay.rs` (missing crate items).
- [ ] **Step 2: Implement** the bodies and seeds; `cargo test -p aether-fuzz` passes on Windows; on Linux, `cd fuzz && cargo fuzz build` and `cargo fuzz run reply_decode -- -runs=100000` (by hand once, recorded in the report). A crash found here goes into `fuzz/regressions/` and gets fixed in the crate it is in.
- [ ] **Step 3: Workspace gate** (fmt and clippy do not see `fuzz/`; run `cargo fmt --check` inside `fuzz/` too); validate both workflow files by pushing to a branch or with `actionlint` if available.
- [ ] **Step 4: Commit** `Fuzz targets for every decoder the shim runs on director output` (add each file by name).

---

## Phase 5: benchmarks

### Task 11: Workloads as data (model: opus)

**Files:**
- Create: `crates/aether-conform/src/workload.rs`, `crates/aether-conform/tests/workload.rs`, `contract/workloads/{launch-storm,sequential-bulk,random-small,readdir-large,save-heavy}.json`, `contract/workloads/README.md` (the format, short)
- Modify: `crates/aether-conform/src/lib.rs`, `.github/workflows/release.yml` (contract.zip gets `workloads/` beside `scenarios/`)

**Interfaces:**
- Consumes: phase 3's `Tree` (files by `{ "size": N, "seed": S }`), `OwnedRequest`.
- Produces:
  - `pub struct Workload { name, description, tree: Tree, phases: Vec<Phase> }`; `pub struct Phase { name, threads: u32, repeat: u32, kind: PhaseKind }`; `pub enum PhaseKind { Storm { ops: Vec<StormOp>, paths: PathSet }, SequentialRead { path, chunk: u32, bulk: bool }, RandomRead { path, len: u32, count: u32, seed: u64 }, ReadDir { path }, Write { path, chunk: u32, total: u64, then_close: bool } }`, `StormOp::{GetAttr, Open, Close, StoredNames}`, `PathSet::{All, Prefix(String), List(Vec<String>)}`.
  - `pub fn load(path) -> Result<Workload, LoadError>`, `load_dir`; unknown fields and kinds are load errors naming file and field (the scenario loader's rule).
  - `pub fn expand(&self, phase: &Phase, thread: u32) -> impl Iterator<Item = OwnedRequest>`: deterministic for a given workload, phase and thread.
  - The five files: a launch-shaped open/getattr storm over a few thousand files under `Data/` (spelled as a game would, mixed case), sequential bulk reads of a 256 MiB file, 4 KiB random reads of a 64 MiB file, READDIR over 5,000-entry folders, a save written in 64 KiB chunks then renamed over the old one.
- [ ] **Step 1: Failing tests:** each checked-in workload loads; an unknown kind and an unknown field fail naming both; `expand` gives the same requests twice and different ones per thread for `RandomRead`; the storm's request count is `ops x paths x repeat`; every path a workload names exists in its tree.
- [ ] **Step 2: Implement; Step 3: workspace gate** (Windows and the Linux crate list); release.yml's contract.zip step lists `workloads/` and the diff is read for PowerShell 5.1 pitfalls.
- [ ] **Step 4: Commit** `contract/workloads: what a director is benchmarked on, as data`.

### Task 12: `aether-conform bench` and the ring floor (model: opus)

**Files:**
- Create: `crates/aether-conform/src/bench.rs`, `crates/aether-conform/src/machine.rs`, `crates/aether-conform/tests/bench.rs`
- Modify: `crates/aether-conform/src/main.rs`, `crates/aether-conform/src/adapters.rs` (`null-adapter`), `crates/aether-conform/Cargo.toml` (`hdrhistogram`), root `Cargo.toml` (workspace dependency), `docs/conformance.md` (a "Benchmarks" section)

**Interfaces:**
- Consumes: Task 11; phase 3's `Adapter`, `Attached`, `Client`; `aether_refdirector::Null`.
- Produces:
  - `aether-conform null-adapter [--ring file|section]`: the adapter protocol with `Null` behind it (the tree is ignored).
  - `aether-conform bench --director "<cmd>" --workloads DIR [--only NAME] [--floor "<cmd>"] [--clients N] [--quick] [--out FILE] [--baseline FILE] [--fail-over PCT]`. Per workload: mount, warm up (one pass, not recorded), run each phase with `threads x clients` client threads, each with its own `Client` on the attached ring; record latency per opcode in an HDR histogram (1 us to 60 s, 3 significant figures); bulk READ bandwidth (bytes per second over the phase); requests per second per phase. Then the same requests against the floor (`null-adapter` by default, same ring kind) and per opcode the director's p50/p99/p99.9 minus the floor's.
  - A reply that is not OK (other than what the workload's phase expects, e.g. GETATTR `found = 0`) stops the workload with an error: no numbers for a workload that failed.
  - Report JSON: `{ "machine": { "cpu", "cores", "os", "build": { "profile", "rustc", "git" } }, "director": "<cmd>", "workloads": [{ "name", "phases": [{ "name", "threads", "requests", "seconds", "per_op": { "<op>": { "count", "p50_us", "p99_us", "p999_us", "max_us", "floor_p50_us", ... } }, "bulk_mib_s" }] }] }`. `--baseline` prints per value the change against a report from the same machine (refusing, with a message, one whose `machine` differs); it exits non-zero only with `--fail-over PCT`, which CI never passes.
  - `machine.rs`: CPU model and core count (from `/proc/cpuinfo` on Linux, the registry's `HARDWARE\DESCRIPTION\System\CentralProcessor\0` through `aether-win` or `std` only on Windows: no new OS crate), OS version, build profile.
- [ ] **Step 1: Failing tests** (`tests/bench.rs`, tiny workloads built in the test, `--quick` meaning one repeat and a capped request count): `bench_against_the_reference_writes_every_opcode_the_workload_sends`; `the_floor_is_measured_and_subtracted` (the report has floor fields for each op); `a_workload_whose_replies_fail_reports_an_error_and_no_numbers` (a broken adapter from phase 3's `--break` knobs); `a_baseline_from_another_machine_is_refused`; `fail_over_is_the_only_way_to_a_non_zero_exit`; `null_adapter_answers_mount_and_unmount`.
- [ ] **Step 2: Implement.**
- [ ] **Step 3: Workspace gate.** By hand, once: `cargo run --release -p aether-conform -- bench --director "target/release/aether-conform reference-adapter" --workloads contract/workloads --out target/bench/reference.json`; put the run's headline numbers in the task report (not in a doc: they are this machine's).
- [ ] **Step 4: Commit** `aether-conform bench: a director's cost per opcode, over the ring floor`.

### Task 13: Criterion micro-benchmarks (model: sonnet, mechanical)

**Files:**
- Create: `crates/aether-protocol/benches/codec.rs`, `crates/aether-casefold/benches/fold.rs`, `crates/aether-redirect/benches/paths.rs`, `crates/aether-ipc/benches/readcache.rs`, `crates/aether-rpc/benches/ring.rs`
- Modify: each crate's `Cargo.toml` (`criterion` dev-dependency, `[[bench]] harness = false`), root `Cargo.toml` (workspace dependency `criterion`, default features off where the HTML reports are not wanted)

**Interfaces:**
- Produces benches (names are criterion group/function ids):
  - `codec`: `encode/<vector>` and `decode/<vector>` for every golden vector, into a reused buffer.
  - `fold`: `fold/ascii-short`, `fold/ascii-long-path`, `fold/non-ascii`.
  - `paths`: `RootMap::under_root` on a path in the root, outside it, and an 8.3 spelling (cache hit and `UncachedScope` miss); `split_stream_suffix`; the vpath normalization the open hook runs.
  - `readcache`: `ReadCache::read` hit of 4 KiB, miss with an in-memory fetch, under 1 and 4 threads.
  - `ring`: the in-process ring floor, typed `Client` against `Null` over an `OwnedSeg` with the spin notifier: one round trip per opcode shape (empty, GETATTR, 4 KiB inline READ), and throughput with 1, 2, 4, 8 client threads. (The cross-process floor is Task 12's `null-adapter`.)
- [ ] **Step 1: Write the benches.** `cargo bench -p <crate> -- --test` (each bench run once as a test) passes for each crate. Clippy `--all-targets` compiles them on both CI jobs, which keeps them from rotting; nothing runs them on CI.
- [ ] **Step 2: Workspace gate; commit** `Micro-benchmarks for the codec, the fold, paths, the read cache and the ring`.

### Task 14: The shim's cost per Win32 call (model: opus)

**Files:**
- Create: `crates/aether-shimbench/{Cargo.toml,src/main.rs,src/launch.rs,src/report.rs,src/bin/aether-shimbench-target.rs}`, `crates/aether-shimbench/tests/smoke.rs`
- Modify: root `Cargo.toml` (member, with a comment: Windows only, a benchmark, not shipped and not in `contract/artifacts.json`), `.github/workflows/ci.yml` (Windows job: build it with the inject artifacts; one smoke step)

**Interfaces:**
- Consumes: `aether_refdirector::{Reference, Null}`, Task 2's `Recorder`, `aether_rpc::serve_tapped`, `aether_win::SharedMapping`, the injector and shim located with `aether_testkit::artifacts::locate`, `aether_env` names. The section setup follows `crates/aether-hooks/tests/fakedirector/mod.rs` (geometry, arena banks, the generation published before attach); if the code would be a copy, move that setup into a small `pub` function both use (in `aether-shimbench` as a library target, or in `aether-win` if it fits there) and point `fakedirector` at it.
- Produces:
  - `aether-shimbench-target <case> <iterations> <path...>`: makes one kind of Win32 call `iterations` times and writes a JSON line of per-call nanoseconds (an HDR histogram summary) to stdout. Cases: `open-close-in-root`, `getattr-in-root`, `getattr-outside` (passthrough), `read-4k-cached` (immutable file, read-cache hits after the first block), `read-4k-uncached` (mutable file), `read-1m-bulk`, `findfirst-100`, `final-path`.
  - `aether-shimbench [--director reference|null] [--iterations N] [--cases LIST] [--out FILE] [--baseline FILE]`: per case runs the target natively against real files on disk (the same tree written to a temp dir) and under the shim (injector plus shim, its ring served in-process by the chosen director behind a `Recorder`), reports native and shim latencies, the difference, and round trips per call from the recorder. Same report conventions as Task 12 (`machine`, baseline from the same machine, `--fail-over` only by hand). A case that fails under the shim (target exit not 0) is an error, not a number.
  - `tests/smoke.rs`: `every_case_runs_natively_and_under_the_shim` with `--iterations 10`, asserting only exit 0, the JSON shape and round trips per call matching Task 7's records for the same calls (a cross-process check of the in-process numbers). Timed values are not asserted. Also `a_director_that_refuses_heartbeat_fails_the_launch_with_fuse_failed` (S15, the cross-process half: the launcher arms `Status(IoError)` on HEARTBEAT; the ready file begins `fuse-failed:` and the injector exits as wire.md 6.3 says). CI runs these tests as part of `cargo test` on Windows.
- [ ] **Step 1: Failing smoke test.**
- [ ] **Step 2: Implement.**
- [ ] **Step 3: Workspace gate.** By hand, once, `cargo run --release -p aether-shimbench -- --iterations 100000 --out target/bench/shim.json`; headline numbers in the task report.
- [ ] **Step 4: Commit** `aether-shimbench: what a Win32 call costs under the shim`.

### Task 15: Docs, manual workflow, changelog (model: sonnet, mechanical)

**Files:**
- Create: `docs/benchmarks.md`, `.github/workflows/bench.yml`
- Modify: `docs/conformance.md` (link), `README.md` (one paragraph), `CHANGELOG.md`, `docs/architecture.md` (crate table: `aether-fuzz`, `aether-shimbench`; the shim tests' harness)

**Interfaces:**
- Produces:
  - `docs/benchmarks.md`: the three costs and which tool measures each; how to run `aether-conform bench` (against the reference, against another director through its adapter), the criterion benches and `aether-shimbench`; the report format; comparing against a baseline from the same machine; why timed numbers never gate CI and what does (Task 7's `costs.txt`, Task 9, Task 10's replay); how to update `costs.txt` (`AETHER_COSTS_UPDATE=1`) and that a deliberate change updates it in the same commit; how to run the fuzz targets (Linux, the pinned nightly).
  - `bench.yml`: `workflow_dispatch` only, Windows and Linux, runs `aether-conform bench` on the reference and the null director and (Windows) `aether-shimbench`, uploads the JSON. Its numbers are a shared runner's: comparable only to its own earlier runs, stated in the file.
  - CHANGELOG entry: the wire, the contract files, and the shim's behaviour toward a well-behaved director are unchanged; the shim's code changed (test entry point, the gap-hunt fixes of Tasks 3A, 3B and 3D, each named with what a program now sees, fault fixes if any), so the natives key changes; new crates and tools.
- [ ] **Step 1: Write; validate workflow YAML; read the docs for em dashes and marketing words** (`LC_ALL=C.UTF-8 grep -nP "\x{2014}" docs/benchmarks.md` prints nothing).
- [ ] **Step 2: Commit** `Benchmarks and the shim harness, documented`.

## Execution order

Phase 4: 1, then 2, then 3. The gap-hunt bugs come next, before any cache or cost work, so that what Task 7 records is the fixed shim: 3A, then 3B (both edit `hook/close.rs` and `synth_file.rs`); 3C, 3D and 3E may run beside them (own modules; 3D edits `director.rs`, which 3A also edits, so 3D goes after 3A if both are in flight). Then 4, 5, 6 in any order (or in parallel: each adds its own module, and they collide only on `harness/main.rs`'s `mod` lines). 7 after 3A to 3E and 4 to 6 (it records the cases they made reachable). 8 may run beside 4 to 7, after 3A, 3B and 3D (it writes their rules into wire.md 3.7). 9 after 7 and 8. 10 is independent of 2 to 9 and may run any time after 1.

Phase 5: 11, then 12. 13 is independent. 14 after 2 and 7. 15 last.

## Rulings on the spec

- **Slot-header lies** (an ack for another request, a payload length past the slot) cannot come out of `serve`, which always completes a slot honestly; a tap can only change status and payload. They are tested where they are handled: `aether-ipc`'s client checks (`take_response_into` returns `BadResponse`) and Task 10's `ring_response` fuzz target. The shim-level property covers statuses, payloads, bulk replies and timing.
- **"No reply"** is a delay past the client's deadline: the slot is abandoned and the late reply drained, which is what a director that stalls produces. A shorter deadline for tests comes from `install_global`, not a new environment variable (which would change `contract/env.json`).
- **"Bytes copied per read"** is counted as bytes over the ring per call (request plus reply payload plus bulk length, from the `Recorder`) and the read cache's `bytes_fetched`. Counting the shim's own `memcpy`s would put instrumentation in the hot path; not done.
- **Allocation gates** fail only when a count goes up (spec), and print when one goes down; round trips and ring bytes must match exactly, since they do not depend on the machine.
- **cargo-fuzz** runs on Linux, nightly pinned in `fuzz/rust-toolchain.toml`, weekly and by hand. The bodies, the seeds from the golden vectors and any crash found are replayed on stable in every CI run on both OSes.
- **Fault tests run the hooks in the test process**, as every aether-hooks test does, because the fault layer must sit in the same process as the test that arms it. The injected path (injector, shim DLL, a separate process) is exercised by Task 14's smoke test.
- **The shim-cost program is its own crate** (`aether-shimbench`), not a new `aether-fixtures` binary, so `contract/artifacts.json`'s fixture list does not change.
- **There is no attribute or negative cache for files** today; Task 6 and Task 7 record GETATTR and failed-open costs as they are, so adding one later shows up as a lower count.

Rulings on the gap hunt's suspected bugs (`gaps-merged.md` section 3; B7 is conformance's, on its own branch). None changes the wire; each changes what a program sees only where a director's answer is one wire.md allows and the shim mishandled it.

- **B3, MKDIR answered `ST_OK` on an existing directory:** the shim asks GETATTR first for `FILE_CREATE` and `FILE_OPEN_IF`, so `CreateDirectoryW` fails `ERROR_ALREADY_EXISTS` (`STATUS_OBJECT_NAME_COLLISION`) and `FILE_OPEN_IF` reports `FILE_OPENED`, whichever of the two answers wire.md 3.4.9 allows the director gives. Cost: one round trip per directory create (rare in a game's run); if wrong, a race with another process creating the same directory between GETATTR and MKDIR reports `FILE_CREATED`, as it does today.
- **B4, a READDIR failure shown as an empty directory:** fail the enumeration with `STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`), the shim's status for any director failure (wire.md 3.4.1, 3.5.1), except `ST_NOT_FOUND`, which stays an empty listing (the directory is gone). Cost if wrong: a program that tolerated a silently empty folder now gets an error from `FindFirstFileW`; that happens only when the director failed or the listing passed `payload_cap`, where today's answer hides content without telling anyone.
- **B5, `CLOSE(0)` after `CreateDirectoryW`:** the shim stops sending CLOSE for a handle the director never issued; `fh` 0 stays valid (wire.md 3.4.3). Cost if wrong: none found; the CLOSE it drops never closed anything a director issued for that handle.
- **B6, reads through a write-only OPEN:** not a shim bug if the production director and the reference serve READ and SETATTR on an `OPEN_WRITE` handle; wire.md 3.4.3 then documents it and a test pins it. If the production director does not, the task stops for a decision (sending `OPEN_READ | OPEN_WRITE` changes requests). Cost if wrong: a third-party director that enforces access bits breaks a save read back through its write handle; conformance C19 (other branch) is where that shows.
- **B8, a write failing part way:** report what landed (`Information` = bytes written, the size raised by them) with the failure status, and leave the file pointer where the call found it, as NTFS does (Task 3A review I2, the coordinator's ruling; an append-only handle's end of file still moves); `ST_NO_SPACE` is `STATUS_DISK_FULL`, every other status `STATUS_UNSUCCESSFUL`; a `written` past the chunk is an unusable reply. Cost if wrong: a program that expected the pointer moved past the bytes that landed, which NTFS never does; a retry of the same buffer at the current position lands where the failed write started.
- **B1, a lazy section after its file handle closes:** the section holds the director handle; CLOSE goes when the last of file handle, section and views goes. Cost if wrong: a director handle lives as long as a mapping, which is what NT does with a file object.
- **B9, a failed or short page fill:** never committed as zeros. Lazy: retried at the fault up to 3 times, then `STATUS_IN_PAGE_ERROR` raised, as NT raises for a mapped file whose paging read fails. Eager: `NtCreateSection` fails `STATUS_UNSUCCESSFUL`. Cost if wrong: a game that ran on with zeroed pages now stops with an in-page error; zeros in mapped game data are corruption it would have hit later, without a cause.
- **B2, a bulk READ answered inline or from a small bank:** the inline half is pinned, not changed: wire.md 3.4.4 says a bulk-flagged READ is answered in bulk and `n < len` is end of file, and conformance C1 (other branch) checks directors. The bank half is fixed: an arena below wire.md 2.6's `256 KiB x slot_count` is not used, so such a director gets inline reads it answers in full. Cost if wrong: a director with a small arena loses bulk transfer speed instead of truncating files.

Gap-hunt shim tests ruled out or narrowed:

- **S15's ready-file text through a real faulting director** is Task 14's smoke test, not an aether-hooks test: the hooks tests attach in-process, where no ready file is written.
- **S6's lazy cases use a 260 MiB file** unless `EAGER_MAX` gets a test seam; the memory is the test process's for one isolated test.

## Open questions, ruled

1. **Allocation records** fail only on an increase, as the spec says, and a decrease prints the command that rewrites `costs.txt`. Cost if wrong: a record that drifts above the real count until someone rewrites it.
2. **Timed baselines**: no fixed machine. `bench.yml` (manual) builds the base commit and the head on the same runner, in the same job, and reports the ratio; absolute numbers are trend only. A developer's machine by hand stays possible. Cost if wrong: noisier comparisons than a self-hosted runner.
3. **Spelling cache TTL**: no sleep. `FuseClient` takes its clock through a test-only seam (a `cfg(any(test, feature = "test-support"))` hook, absent from the shipped DLL), per the repo's rule against fixed sleeps. Cost if wrong: a small shim change.
4. **Fuzz cadence**: weekly plus manual, and corpus replay on stable in every CI run. Cost if wrong: crashes found up to a week later.

## Gap-hunt findings

Three independent gap hunts (opus, sonnet, fable) list what the shim relies on that conformance does not check, and what directors may answer that the shim's tests never present. Their merged list (`gaps-merged.md`) lands in this plan as follows; section 1 (conformance) is on another branch and is cross-referenced only where a shim test needs the same director behaviour.

| Item | Where | Cross-reference |
|---|---|---|
| S1 STAGE_EXEC | Task 2 (`staging`), Task 3C, Task 7 row, Task 9 op | C13 |
| S2 READDIR failure, large listings | Task 3A (B4, resumed listing), Task 8 row, Task 9 (`RawStatus(i32::MIN)`) | C6 |
| S3 short or inline bulk READ, one-call reads | Task 2 (`inline_reads`), Task 3D (B2) | C1 |
| S4 multi-chunk WRITE | Task 3A (B8), Task 7 row, Task 9 (`nth`) | C11 |
| S5 MKDIR `ST_OK` on existing | Task 2 (`mkdir_existing_ok`), Task 3A (B3) | |
| S6 sections through a director | Task 3B (B1, B9), Task 7 rows | |
| S7 4 MiB pipeline, two threads | Task 3D, Task 7 row | C2 |
| S8 STORED_NAMES not found | Task 6 | |
| S9 odd `fh` values | Task 2 (`with_fh_base`), Task 3A (B5), Task 6 | C12 |
| S10 geometry, wake event, file ring | Task 3 (`Options`), Task 3E | C17 |
| S11 lower-only overlay answers | Task 2 (`with_lower`), Task 6 (`overlay_answers.rs`, `caches_names.rs`) | |
| S12 read through a write handle | Task 3A (B6) | C19 |
| S13 non-zero mtime agreement | Task 2 (`with_mtime`), Task 6 | C18 |
| S14 CREATE_KEY `Exists` then absent | Task 5 | |
| S15 HEARTBEAT refused or slow | Task 3 (`attach_faults`), Task 8 row, Task 14 smoke | |
| S16 bump after reply | Task 3 (`RegGen::Manual`), Task 5 | C9 |
| S17 small alternatives | Task 6 (`overlay_answers.rs`) | C22 |
| B1, B9 | Task 3B | |
| B2 | Task 3D | C1 |
| B3, B4, B5, B6, B8 | Task 3A | C6, C12, C19 |
