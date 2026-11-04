# Benchmarks, counted costs and fuzzing

What Aether costs a program, what the ring and the codecs cost, and what a
director costs are three different questions, and each has its own tool.
What can be counted exactly is counted and checked on every push. What can
only be timed is measured by hand or by the manual `bench` workflow, and a
timed number never fails a build.

| Cost | Question | Tool | Checked on every push |
|---|---|---|---|
| The shim, counted | How many requests, how many ring bytes and how many allocations does one hooked call cost? | `costs.txt`, checked by `aether-hooks`'s `harness` tests | Yes, exactly |
| The shim, timed | How long does a Win32 call take under the shim, against the same call on disk? | `aether-shimbench` | A smoke run that checks counts |
| The ring and the codecs, timed | How long do an encode, a fold, a path resolution, a read-cache hit or a ring round trip take? | the criterion benches (`cargo bench`) | They are built, not run |
| A director, timed | How long does a director take per opcode, over what the ring itself costs? | `aether-conform bench` | A smoke run that checks replies |

Timed numbers belong to the machine that took them. They are compared with
earlier numbers from the same machine, never with a fixed value, and the
tools refuse to compare reports from two machines.

## What is checked on every push

A change in what the shim sends a director, or in how much memory a hooked
call allocates, is a change a test can see exactly, so the tests see it:

- `crates/aether-hooks/tests/harness/costs.txt` records, per hooked call, the
  requests it sends, the bytes they move over the ring and the allocations
  the shim makes on the calling thread. "Costs.txt" below says how it is
  checked and rewritten.
- The fault table in `docs/wire.md` 3.7 says what the shim does with each
  reply a director should not give, per operation. A test renders the table
  from its own rows and compares it with the document cell by cell, so the
  document and the shim cannot differ.
- Generated faults (`tests/harness/faults_file.rs` and `faults_registry.rs`)
  arm a fault on a call in a sequence of calls and check that the program is
  neither crashed nor hung, that the fault was applied, and that the next call
  of the same kind succeeds.
- The decoders that read what a director sends are replayed on stable Rust on
  both operating systems (`aether-fuzz`'s tests, "Fuzzing" below).

## costs.txt

`crates/aether-hooks/tests/harness/costs.txt` has one line per case, sorted
by name:

```text
read_bulk_1_mib round_trips=4 ring_bytes=1048736 allocs<=26
```

A case is one hooked call made against the reference director in a process
of its own, once untimed (so one-time state is filled) and once measured.
`tests/harness/costs.rs` says what each case does. A case that is a Win32
call is the call; where the host decides how many NT calls a Win32 call
makes, the case is the NT calls.

- `round_trips` and `ring_bytes` must match the record exactly. They depend
  on what the shim sends and what the reference answers, not on the machine.
- `allocs` may not exceed the record. The count is the allocator's on the
  calling thread only, so the director's threads and the shim's own helper
  threads are not in it. A count that went down passes, and the test prints
  the command that lowers the record.

The record is taken with `TMP` and `TEMP` set to `C:\vfstmp`, as CI sets
them, because the temporary folder is part of every path the cases use and a
longer path can cost an allocation more.

A change that deliberately moves a cost rewrites the record in the same
commit:

```powershell
$env:TMP = 'C:\vfstmp'; $env:TEMP = 'C:\vfstmp'
$env:AETHER_COSTS_UPDATE = '1'
cargo test -p aether-hooks --test harness costs::
```

The rewrite is refused where `CI` is set. The diff of `costs.txt` is then the
review of the change: a call that sends one more request shows as one
changed number.

## aether-shimbench

`aether-shimbench` times one Win32 call at a time, first against files on
disk and then in a process the injector started with the shim loaded, whose
ring the benchmark serves in-process with the reference director. The shim's
cost is the difference. Windows only.

```powershell
cargo build --release -p aether-inject -p aether-shim -p aether-shimbench
target\release\aether-shimbench --iterations 100000 --out target\bench\shim.json
```

The injector and the shim are looked up beside the benchmark's own binary, so
build all three in the profile you run.

| Option | Meaning |
|---|---|
| `--director reference\|null` | The reference director (default), or the null director, whose answers are the smallest valid ones: the ring's floor. Calls are timed however they end, so only `open-close-in-root` and `getattr-in-root` mean much against it. |
| `--geometry production\|shim-tests` | The ring's shape. `production` (default) is the one `docs/wire.md` 2.7 gives a launch: 32 slots, a 1 MiB payload cap and a 32 MiB arena. `shim-tests` is the small ring the shim's tests use, the one `costs.txt` is counted on. |
| `--iterations N` | Timed calls per case. Default 10000. |
| `--cases LIST` | Comma-separated case names. Default all eight: `open-close-in-root`, `getattr-in-root`, `getattr-outside`, `read-4k-cached`, `read-4k-uncached`, `read-1m-bulk`, `findfirst-100` and `final-path`. |
| `--out FILE` | The JSON report. A table goes to stdout either way. |
| `--baseline FILE` | Print each case's change against an earlier report. A report from another machine (CPU, cores, OS), another director or another geometry is refused. |
| `--fail-over PCT` | With `--baseline`: exit 3 when a case's shim p50 rose more than PCT percent. |
| `--timeout SECS` | A limit on each child process. |

A case makes a tenth of its calls (at least 10, at most 10000) untimed, then
times the rest in batches whose length makes a batch last at least 100 steps
of the timer. A sample is a batch's time divided by its calls, so a call
shorter than the timer's step (100 ns on Windows) is measured, and the
quantiles are over batch means. The report's `timer_ns` states the step.

```json
{
  "machine": {"cpu": "...", "cores": 16, "os": "...",
              "build": {"profile": "release", "rustc": "...", "git": "..."}},
  "director": "reference", "iterations": 100000, "warmup": 10000, "timer_ns": 100,
  "geometry": {"name": "production", "slots": 32, "payload_cap": 1048576,
               "arena_len": 33554432, "serve_threads": 4},
  "cases": [
    {"name": "getattr-in-root",
     "native": {"count": 1000, "min": 0, "p50": 0, "p90": 0, "p99": 0, "p999": 0,
                "max": 0, "mean": 0, "batch": 1, "calls": 1000},
     "shim": {"...": "the same fields"},
     "overhead_ns": {"p50": 0, "p90": 0, "p99": 0},
     "round_trips_per_call": 1.0, "ring_bytes_per_call": 32.0},
    {"name": "...", "error": "..."}
  ]
}
```

Every latency is nanoseconds per call. `overhead_ns` is the shim's value less
the native one at the same quantile, and it is negative where the director
answers from memory faster than the file system does. `round_trips_per_call`
and `ring_bytes_per_call` are counted by the recorder during the timed loop,
so they are exact and, on the `shim-tests` geometry, equal the matching rows
of `costs.txt`; the benchmark's own tests check that. A case that fails to
run has an `error` and no numbers, and the run exits 1.

Exit codes: 0, 1 when a case failed, 2 on a usage error or a refused
baseline, 3 for a rise past `--fail-over`.

## aether-conform bench

`aether-conform bench` times a director on the workloads in
`contract/workloads/`, through the same adapter `aether-conform run` uses, so
it measures any director: the reference, or another one through its adapter.

```sh
cargo build --release -p aether-conform
# The reference director:
target/release/aether-conform bench --director "target/release/aether-conform reference-adapter" \
    --workloads contract/workloads --out target/bench/reference.json
# Another director, through its adapter, four clients per thread:
target/release/aether-conform bench --director "<your adapter command>" --clients 4 \
    --out target/bench/mine.json
# Against an earlier report from this machine:
target/release/aether-conform bench --director "<your adapter command>" \
    --baseline target/bench/mine.json
```

Each phase of each workload is played once as a warm-up and then recorded,
and every request is timed from before it is sent until its reply is decoded.
The same requests then go to a floor, `aether-conform null-adapter`, which
answers every request at once with the smallest reply on a ring of the same
shape. The report gives the director's p50, p99 and p99.9 per opcode, the
floor's, and the difference: what the director costs over the ring. The
workloads, the options, the report format and the checks a reply must pass
are in `docs/conformance.md` ("Benchmarks") and `contract/workloads/README.md`.

A comparison with `--baseline` refuses a report from another machine,
director, ring shape or `--clients`. `--fail-over PCT` exits 3 when an
opcode's p50 rose by more than PCT percent, for someone at that machine
checking their own change; CI never passes it.

## The criterion benches

Micro-benchmarks of the pieces a call passes through, one bench per crate:

| Crate | Bench | Measures |
|---|---|---|
| `aether-protocol` | `codec` | encode and decode of every golden vector |
| `aether-casefold` | `fold` | the fold, `fold_into` and `eq_folded` on short ASCII, a long path and non-ASCII names |
| `aether-redirect` | `paths` | `RootMap::resolve` inside, outside and by 8.3 spelling, cached and uncached; `normalize_vpath`; the spelling cache |
| `aether-ipc` | `readcache` | read-cache hits and misses of 4 KiB on one and four threads; registering a file |
| `aether-rpc` | `ring` | a typed client against the null director: heartbeat, GETATTR and a 4 KiB READ round trip, and GETATTR with 1, 2, 4 and 8 client threads |

```sh
cargo bench -p aether-casefold --bench fold
cargo bench -p aether-rpc --bench ring -- --warm-up-time 1 --measurement-time 3
```

Name the bench: a plain `cargo bench -p <crate>` also runs the library's own
test harness, which rejects criterion's options. Criterion keeps the last run
under `target/criterion` and prints the change against it, which is the
comparison to use on one machine. The multi-thread ring numbers need a quiet
machine: spinning client threads and a spinning server thread on a busy one
swing by orders of magnitude. The 8.3 path case depends on the host, since it
expands a short name on disk.

## Comparing two commits: the bench workflow

[`bench.yml`](../.github/workflows/bench.yml) runs by hand (Actions, "bench",
Run workflow) and measures a base commit and the head on one Windows runner,
in one job, so both see the same machine and the same neighbours. It builds
both, runs `aether-conform bench` against the reference director and
`aether-shimbench` for each, base first, then runs the head's tools with
`--baseline` set to the base's report so that the output carries the change.
The reports and the printed comparison are uploaded as the `bench` artifact
and written to the run's summary.

A shared runner is noisy and its absolute numbers mean little; the ratio of
head to base within one run is the signal, and a change smaller than the
spread between two runs of the same commit is not a finding. The workflow
never fails on a timing: `--fail-over` is not passed, a base that lacks a
tool is skipped with a notice, and the only way for it to fail is a reply the
director got wrong or a build that does not build.

## Fuzzing

`aether-fuzz` holds the bodies of six targets, and `fuzz/` is the cargo-fuzz
project that runs them under libFuzzer:

| Target | Input | The body checks |
|---|---|---|
| `reply_decode` | a reply to any opcode | every part of what decodes can be walked, and re-encoding gives the same reply |
| `request_decode` | a request of any opcode | the opcode is recognised, and re-encoding gives the same request |
| `shim_cfg` | the shim's bootstrap config | what decodes re-encodes to the same bytes |
| `ring_response` | a ring slot as a director might leave it | the client's reads of the slot return, never past the slot or the cap |
| `dir_records` | a directory listing and a buffer size | the records written never exceed the buffer |
| `shim_env` | the `VFS_*` variables | the parsing returns, and the root map the shim builds from it is built or refused |

The parsing of the `VFS_*` variables is `aether_env::session`: pure functions
of the text, shared by the shim, `aether-conform`'s attach check and the
`shim_env` target, so the code that is fuzzed is the code that runs.

**On every push**, on Windows and Linux, `cargo test -p aether-fuzz` replays
the bodies on stable Rust: every seed (the golden vectors, framed for each
target, and one environment per variable the contract names), every input kept
under `fuzz/regressions/<target>/`, every seed cut at each length and with
each byte flipped, a run of mutations (`AETHER_FUZZ_ROUNDS` per seed, default
40) and a property test per body. A counting allocator checks that no body
allocates more than a small factor of its input.

**To fuzz**, on Linux (cargo-fuzz needs a nightly compiler and its sanitizers
are not something this repository builds on Windows):

```sh
rustup toolchain install                              # the workspace's
(cd fuzz && rustup toolchain install)                 # the pinned nightly, fuzz/rust-toolchain.toml
cargo install cargo-fuzz --locked --version 0.13.2
cargo run -p aether-fuzz --example write-seeds -- fuzz/corpus
cd fuzz
cargo fuzz run reply_decode -- -max_total_time=300 -timeout=10 -rss_limit_mb=512
```

[`fuzz.yml`](../.github/workflows/fuzz.yml) does this for every target weekly
and by hand, with a `-max_len` per target sized to the largest input the
target can be given. A crash is kept as `fuzz/artifacts/<target>/crash-*`;
copy it to `fuzz/regressions/<target>/` with the fix, and the stable replay
runs it from then on.
