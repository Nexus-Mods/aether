# Director conformance (phase 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `aether-conform` tests any director, in any language, over the real ring: hand-written scenarios and generated operation sequences, compared against the reference director.

**Architecture:** A new crate `aether-conform` (library plus binary) built on `aether-rpc` (`Client`), `aether-refdirector` (`Reference`, the oracle), `aether-filering` (file rings) and, on Windows, `aether-win` (sections). It starts the director under test as a child process and drives it through a JSON-lines adapter protocol on stdin/stdout. Scenarios are JSON files in `contract/scenarios/`, parsed into the typed `Request`/`Reply`. Generative mode uses proptest-state-machine and compares each reply with the reference director's.

**Tech Stack:** Rust, `serde`/`serde_json` (in `aether-conform` only), `proptest`, `proptest-state-machine`.

**Spec:** `docs/superpowers/specs/2026-11-03-protocol-and-conformance-design.md`, sections 3, 4, 5 and 6.

## Global Constraints

- No wire change: golden vectors byte-identical, wire version 4, `contract/env.json`, `contract/artifacts.json` unchanged; `contract/protocol.json` changes only if the table does (it should not).
- The shim's code (`crates/aether-hooks/src`, `crates/aether-shim`) is not changed by this plan.
- Conformance checks only what the contract guarantees. `docs/wire.md` is the authority: where it says directors may differ (non-canonical path syntax, section 3.2 "a client must not rely on it"), the suite does not send that input or does not compare that reply. Every comparison rule that relaxes exact equality is named in `docs/conformance.md` with the wire.md line that allows it.
- The reference director answers as `docs/wire.md` says; where a scenario run shows wire.md and the reference disagree, the reference is fixed, not the scenario.
- `aether-conform` runs natively on Linux and Windows (file rings everywhere; sections on Windows too).
- Every task ends green on Windows: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, inject artifacts built, `cargo test --no-fail-fast` (TMP/TEMP=C:\vfstmp). New tests use `aether_testkit::tempdir()`.
- Commit messages plain, no AI attribution; files added by name.
- Docs and doc comments plain prose, no em dashes.

## Review Focus

- An adapter that crashes, hangs, writes garbage on stdout or never answers `mount`: `aether-conform` must report it as a failure with a timeout, never hang. Task 3 test.
- A director that answers with a non-canonical but equivalent value (another file handle number, another listing order): the comparison rules must accept exactly what wire.md allows and nothing more. Task 2 tests.
- A scenario file with an unknown operation or field name: load error naming the file and the field. Task 2 test.
- A generated case that fails: the written-out scenario must reproduce the failure when replayed through scenario mode. Task 5 test.
- The adapter's ring handshake on Windows (section plus events) and on Linux (file): both attach paths tested. Task 3 test.

---

### Task 1: The reference director fit for oracle use (model: opus)

**Files:** `crates/aether-refdirector/src/reference.rs`, its tests; `crates/aether-filering/src/lib.rs`, its tests.

**Interfaces:**
- Produces: `Reference` implements MKDIR, STORED_NAMES and RENAME_X as `docs/wire.md` 3.4 describes (today they answer `NotSupported` after their path checks); keeps the spelling a name was created with (write OPEN, MKDIR, RENAME and RENAME_X targets) and answers READDIR and STORED_NAMES with stored spellings (wire.md 3.2, "Keep the spelling a name was created with"); `ReadStyle::ShorterThanClaimed(n)` clamps to the file's length instead of panicking; `failing_writes_under`'s doc says such a prefix does not count as a writable provider; `FileRing::create` removes and recreates the file rather than truncating it in place (wire.md 2.10).
- `Reference::from_tree(&Tree)` is NOT this task (Task 2 defines `Tree`).

- [ ] **Step 1: Failing tests** in `crates/aether-refdirector/tests/wire_rules.rs`: MKDIR creates a directory that GETATTR then reports `is_dir` and READDIR lists with the created spelling; MKDIR on an existing path answers what wire.md says; STORED_NAMES answers stored spellings joined by `/` with `skip` applied as wire.md 3.4 says; RENAME_X moves a file between two served roots and answers `CrossRoot` where wire.md says; a file created as `Data/New.ESP` is listed as `Data/New.ESP` and found as `data/new.esp`; `ShorterThanClaimed(n > len)` reads the whole file without panicking and the reference keeps answering. In `crates/aether-filering/tests/ring.rs`: a second `create` on a path still mapped by an open `FileRing` leaves the first mapping readable (it maps the old file).
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.** Read wire.md 3.4 for each opcode's exact statuses before writing code.
- [ ] **Step 4: Workspace gate.** Every aether-hooks test still passes (same count).
- [ ] **Step 5: Commit** `aether-refdirector: mkdir, stored names and rename-x as wire.md has them`.

### Task 2: Scenarios: format, loader, tree, comparison (model: opus)

**Files:**
- Create: `crates/aether-conform/{Cargo.toml,src/lib.rs,src/scenario.rs,src/tree.rs,src/compare.rs}`, `crates/aether-conform/tests/scenario.rs`, `contract/scenarios/README.md` (the format, short)
- Modify: root `Cargo.toml` (member, workspace deps `serde`, `serde_json`), `crates/aether-refdirector/src/reference.rs` (`Reference::from_tree`)

**Interfaces:**
- Produces:
  - `pub struct Tree` (roots; per root: files with path, contents as UTF-8 text or hex or `{ "size": N, "seed": S }` generated bytes via `aether_refdirector::pattern`-style fill, mtime; directories; writable prefixes; registry keys and values), `Deserialize`.
  - `pub struct Scenario { name, description, tree: Tree, steps: Vec<Step> }`; `pub struct Step { request: OwnedRequest, expect: Expect }`; `Expect = Reply(OwnedReply) | Status(Status) | OneOf(Vec<Expect>)` (only where wire.md allows more than one answer, with the wire.md section named in the scenario).
  - Requests and replies in JSON use `protocol.json`'s names: `{"op":"open","root":0,"flags":["read"],"path":"data/a.esp"}`, statuses `"not-found"`. An unknown op, field or status name is a load error naming the file, the step index and the field.
  - `OwnedRequest::as_request(&self) -> Request<'_>`, `OwnedReply::from_reply(&Reply<'_>)`.
  - `pub fn load(path: &Path) -> Result<Scenario, LoadError>`, `pub fn load_dir(dir: &Path) -> Result<Vec<Scenario>, LoadError>`.
  - `compare::Session` holding the comparison state across one scenario's steps: file handles compared up to a consistent bijection (the director's fh 7 may be the reference's fh 1, but always the same one); listings compared as sets where wire.md says order is unspecified (check 3.4.2); mtimes of files the scenario wrote compared only for "changed"/"unchanged" if wire.md leaves them director-defined; `fn check(&mut self, step: usize, expected: &Expect, got: &Result<OwnedReply, Status>) -> Result<(), Mismatch>`; `Mismatch` carries both values, rendered readably.
  - `Reference::from_tree(&Tree) -> Reference` in aether-refdirector (behind a `scenario` feature if the `Tree` type lives in aether-conform; otherwise move `Tree` into aether-refdirector; choose the direction that avoids a dependency cycle and say which).
- [ ] **Step 1: Failing tests:** load a small scenario with every request shape; unknown op/field/status errors name file, step and field; `Session` accepts a consistent fh renaming and rejects an inconsistent one; listing order rule per wire.md; `from_tree` builds a reference that answers GETATTR/READ for the tree's files with the given bytes.
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Workspace gate.**
- [ ] **Step 5: Commit** `aether-conform: scenarios, trees and the comparison rules`.

### Task 3: The adapter protocol and `aether-conform run` (model: opus)

**Files:**
- Create: `crates/aether-conform/src/{adapter.rs,attach.rs,run.rs,report.rs,main.rs}` (binary `aether-conform`), `crates/aether-conform/tests/run.rs`, `docs/conformance.md`

**Interfaces:**
- Consumes: Task 2's `Scenario`, `Session`; `aether_rpc::Client`; `aether_filering::FileRing::open`; `aether_win`'s section open on Windows; `aether_env` names for the handshake variables.
- Produces:
  - Adapter protocol (JSON lines, one request then one answer): `{"op":"mount","scenario":"<abs path>"}` -> `{"ok":true,"env":{...VFS_* handshake variables a shim would get...}}` or `{"ok":false,"error":"..."}`; `{"op":"unmount"}` -> `{"ok":true}`; EOF on stdin means exit. Every answer has a timeout (default 30 s, `--timeout`); a crash, EOF on stdout, a non-JSON line or a timeout fails the current scenario with the adapter's stderr tail and ends the run.
  - `aether-conform reference-adapter`: a built-in adapter that serves scenarios with `Reference::from_tree` over a file ring (`--ring section` on Windows for a named section), so the whole child-process path is exercised without an external director.
  - `aether-conform run --director "<command line>" [--scenarios DIR] [--report json|tap] [--out FILE] [--timeout SECS]`: for each scenario: mount, attach with the returned env, play steps through `Client`, compare with `Session`, unmount. Exit code 0 when all pass, 1 when any fails, 2 on usage or adapter errors.
  - Reports: TAP on stdout by default; `--report json` writes `{"scenarios":[{"name","passed","steps":[{"index","request","expected","got","passed","note"}]}],"seed":null}`.
  - `docs/conformance.md`: what conformance checks and what it does not (non-canonical paths, director-defined fields, with wire.md references), the adapter protocol, how to run it against a director, how to read a report.
- [ ] **Step 1: Failing tests:** `run` against `reference-adapter` passes a two-scenario directory on a file ring; on Windows also with `--ring section`; an adapter that exits at once, one that prints garbage, and one that never answers (a tiny test adapter binary or a script under `tests/`) each fail with a clear message within the timeout; a scenario whose expectation is deliberately wrong fails with both values in the report.
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Workspace gate.**
- [ ] **Step 5: Commit** `aether-conform run: a director under test, behind an adapter`.

### Task 4: The scenario suite and CI (model: sonnet)

**Files:** `contract/scenarios/*.json`; `.github/workflows/ci.yml`; `.github/workflows/release.yml` (contract.zip gets `contract/scenarios/`); `docs/conformance.md` (index of scenarios)

**Interfaces:**
- Consumes: Tasks 2 and 3.
- Produces: at least one scenario per opcode and one per rule in wire.md sections 3.2 to 3.5 and 4.3 that a director must follow (case-insensitive lookup by fold, stored spelling kept, escape is an error, unknown root, GETATTR on a missing path is OK with found=0, every status the document assigns to a condition, inline and bulk READ, reads past EOF, writes through append handles, write opens on read-only roots, registry create/set/delete/rename/changed and the `payload_cap - 8` rule where reachable). Each scenario's `description` cites the wire.md section it checks. CI runs `aether-conform run --director "aether-conform reference-adapter"` over `contract/scenarios` on the Linux job and on the Windows job (Windows also with `--ring section`).
- [ ] **Step 1:** Write the scenarios; run them against `reference-adapter`. Any failure is either a wrong scenario (fix it against wire.md) or a reference bug (fix the reference, with a test in its crate). List every reference fix in the report.
- [ ] **Step 2:** Wire up CI and release packaging; check `release.yml`'s contract.zip step includes the scenarios directory.
- [ ] **Step 3: Workspace gate**, then commit `contract/scenarios: what a director must do, as data`.

### Task 5: Generative and malformed modes (model: opus)

**Files:** `crates/aether-conform/src/{generate.rs,model.rs,malformed.rs}`, `crates/aether-conform/tests/generate.rs`; `docs/conformance.md`

**Interfaces:**
- Consumes: Tasks 2 and 3; `aether_protocol` `testing` strategies where they fit.
- Produces:
  - `aether-conform run ... --generate [--cases N] [--seed S] [--failures DIR]`: proptest-state-machine over a generated `Tree` (only canonical paths, per the global constraint) and a sequence of operations: opens with flag combinations, reads at odd offsets and lengths (inline and bulk), writes, setattr, rename and rename-x over existing names, delete while open, mkdir, readdir, stored-names, case variants of existing names, stale and invalid handles, and registry operations. Each case: mount the generated tree in the director (written to a temp scenario file for the adapter) and in an in-process `Reference`; run each operation on both; compare with `Session`. The first mismatch fails the case; proptest shrinks it; the shrunk case is written to `--failures DIR` as a scenario file whose replay in scenario mode reproduces the mismatch. The run prints its seed; `--seed` replays it.
  - Malformed mode (always part of `run`, or `--malformed`): truncated payloads, trailing bytes where the format is strict, bad UTF-8, unknown and reserved opcodes, over-long payload headers; the director must answer as wire.md says (BadRequest, empty payload) and keep serving.
- [ ] **Step 1: Failing tests:** against `reference-adapter`, 64 generated cases pass; against a deliberately broken adapter (the reference with one rule flipped, e.g. a test-only flag that makes it answer `found=1` for a missing path), generation finds it, the shrunk scenario is written, and replaying it in scenario mode fails the same way; the same seed gives the same cases; malformed inputs against the reference all pass.
- [ ] **Step 2: Run, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** CI: the Linux and Windows conformance steps add `--generate --cases 256` (bounded so the step stays under about three minutes; measure and set). Workspace gate.
- [ ] **Step 5: Commit** `aether-conform: generated operation sequences against the reference`.

### Task 6: Ship it (model: sonnet)

**Files:** `.github/workflows/release.yml`, `contract/artifacts.json` only if a tools list is needed there (prefer not: the contract is unchanged), `README.md`, `CHANGELOG.md`, `docs/conformance.md`

**Interfaces:**
- Produces: each release carries `aether-conform-windows-x64.zip` (`aether-conform.exe`) and `aether-conform-linux-x64.tar.gz` (`aether-conform`), unsigned, listed in SHA256SUMS; README says how a director author runs it; CHANGELOG entry.
- [ ] **Step 1:** Add the Linux build of `aether-conform` to the release workflow (the Linux runner builds it natively) and the Windows build to the Windows job; include both in the published assets and SHA256SUMS.
- [ ] **Step 2:** Validate the workflow YAML (`python -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" .github/workflows/release.yml` or the repo's own check) and read the diff once more for PowerShell 5.1 pitfalls (`${var}:` in strings, `Add-Content -Encoding ascii` for `GITHUB_OUTPUT`).
- [ ] **Step 3: Commit** `Releases carry aether-conform for Windows and Linux`.

## Execution order

1 and 2 can run in parallel (2 touches `aether-refdirector` only to add `from_tree`; if 2 needs a field 1 adds, 2 waits). 3 follows 2; 4 follows 3; 5 follows 3; 6 follows 3. 4 and 5 may run in parallel (4 is data, CI and docs; 5 is code; both edit docs/conformance.md and ci.yml, so 5 goes after 4 if they collide).

Phase 4 (the shim-side harness: recording, fault injection, fuzz targets, counted round trips and allocations per hooked call) and phase 5 (benchmarks) get their own plan after this one.
