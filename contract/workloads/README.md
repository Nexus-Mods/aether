# Benchmark workloads

A workload is a tree for a director to mount and the phases of requests
`aether-conform bench` times against it (`docs/conformance.md`,
"Benchmarks"). A run loads every `*.json` file in the workloads folder, in
name order. Names are `contract/protocol.json`'s; every path is on root 0
and canonical (`docs/wire.md` 3.2).

## Format

```json
{
  "name": "random-small",
  "description": "4 KiB reads at random offsets of a 64 MiB archive",
  "tree": {"roots": [{"id": 0, "files": [{"path": "Data/Big.ba2",
                                          "contents": {"size": 67108864, "seed": 3}}],
                      "writable": ["Saves"]}]},
  "generate": [{"dir": "Data/Meshes", "count": 100, "name": "Rock{n}.nif",
                "pad": 3, "size": 2048, "seed": 10}],
  "phases": [{"name": "reads", "threads": 4, "repeat": 1, "kind": "random_read",
              "path": "Data/Big.ba2", "len": 4096, "count": 4096, "seed": 7}]
}
```

- `tree` is a scenario's tree (`contract/scenarios/README.md`). A file may
  be up to 512 MiB.
- `generate` (optional) adds `count` files to root 0 in `dir`, named by
  `name` with `{n}` their number from 0, zero-padded to `pad` digits
  (optional), each `size` bytes generated from `seed + n`.
- Each phase has a `name`, `threads` (1 to 64: client threads at once, each
  with its own ring client), `repeat` (how many times each thread sends its
  requests) and a `kind`:

| `kind` | Fields | Each thread sends, `repeat` times |
|---|---|---|
| `storm` | `ops` (`getattr`, `open`, `close`, `stored_names`), `paths` (`"all"`, `{"prefix": ".."}`, `{"list": [..]}`) | for each file of `paths`, the `ops` in order; a `close` closes the last `open`'s handle. Each thread starts at its own place in the list. |
| `sequential_read` | `path`, `chunk`, `bulk` | OPEN, READs of `chunk` bytes from offset 0 to the end (`read_bulk` when `bulk`), CLOSE |
| `random_read` | `path`, `len`, `count`, `seed` | OPEN, `count` READs of `len` bytes at multiples of `len`, drawn from `seed` and the thread number, CLOSE |
| `readdir` | `path` | READDIR of `path` |
| `write` | `path`, `chunk`, `total`, `then_close`, `rename_to` (optional) | OPEN `path` (`write`, `create`, `trunc`), WRITEs of `chunk` bytes up to `total`; with `then_close` a CLOSE and then a RENAME onto `rename_to`, without it the RENAME first. Thread `t > 0` uses the names with `.t` appended. |

A prefix is compared by the fold (`docs/wire.md` 3.2). A path a phase reads
or lists must be in the tree; a `write` path must be in a directory under a
`writable` entry. An unknown field or kind is a load error naming the file
and the field.

## The workloads

| File | What |
|---|---|
| `launch-storm` | GETATTR, OPEN and CLOSE of about 3,000 files under `Data/` from four threads, then STORED_NAMES of the textures |
| `sequential-bulk` | a 256 MiB archive in 1 MiB bulk READs, one reader and four |
| `random-small` | 4 KiB inline READs at random offsets of a 64 MiB archive, four threads |
| `readdir-large` | READDIR of two folders of 5,000 entries |
| `save-heavy` | an 8 MiB save written in 64 KiB chunks and renamed over the old one, one thread and two |
