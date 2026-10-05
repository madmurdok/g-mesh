# GM-324 TypeScript Rust port: measurements

Slice S24 (measure) of GM-324, the four measures of
`docs/architecture/gm-324-typescript-rust-port.md` section 5 "M". Branch
`feat/GM-324-325-ts-port` at `0a61d0f`, release build
(`cargo build --release --workspace`). No production code or test was changed
to produce these numbers.

## Summary

| Measure (excalidraw `1acf66ed`, 3 reps, median) | Rust | Node (control) | Node / Rust |
|---|---|---|---|
| 1. Watch-file save -> swap in the daemon log (real) | **4.26 s** | 9.31 s | 2.2x |
| 1. Re-walk (`--bulk-index` child) user / sys | **1.57 / 0.11 s** | 7.29 / 0.47 s | 4.6x user |
| 2. Cold `--bulk-index` real / user / sys | **1.69 / 1.53 / 0.13 s** | 7.24 / 7.40 / 0.60 s | 4.8x user |
| 3. `fileChanged`, `App.tsx` (largest by lines) | **128 ms** | 513 ms | 4.0x |
| 3. `fileChanged`, three largest `.ts` by bytes | 9 / 8 / 37 ms | 17 / 16 / 134 ms | 1.9-3.6x |
| 4. Node ids / edge ids | 16,650 / 27,064 | 16,650 / 27,064 | **identical sets** |
| 4. Field differences on shared ids | 10 nodes, `docComment` only | | one cause (below) |

GM-350 M4's reference for the Node walk was `real 24.53 user 19.68 sys 1.23` at
load average 83.7. Here, at load ~3, the same Node walk takes user 7.3-7.6 s.
The Rust walk takes user 1.5-1.6 s.

## Machine and method

- MacBook, Intel i7-1068NG7 (8 logical CPUs), 32 GB, macOS. Node v20.6.1.
  `uptime` at the start of the run was `load averages: 3.38 10.06 19.22`, and
  at the end `3.06 7.73 17.13`. The 1-minute load at each rep is in the
  tables. Other users' daemons were running and were left alone.
- Corpus: a local `git clone --shared` of `~/Projects/excalidraw`, detached at
  `1acf66edabc2ac5bbd4aed0714aed7dca7cc2aab` (658 TS/JS files, no
  `node_modules`) in the scratchpad, so the user's checkout was never touched.
- Arms: the Rust binary `target/release/g-mesh-plugin-typescript`, and the
  Node plugin `node plugins/typescript/dist/src/index.js` (built in the
  worktree). Measures 2 to 4 spawn the plugin directly. Measure 1 runs a
  daemon with `G_MESH_PLUGIN_ROOTS_OVERRIDE` pointing at a scratch plugin
  root that holds one manifest:
  - Rust: the branch's `plugins/typescript/plugin.toml` with only `command`
    changed (to a timing wrapper).
  - Node: `git show 1ff22f7:plugins/typescript/plugin.toml` with three
    changes: `command` set to the wrapper, `semantic_pass = false`, and
    `watch_files` set to the Rust manifest's list.
- Fairness: both arms are structural only. Node's `--bulk-index` is
  structural by construction, its semantic pass is a separate request, and
  `semantic_pass = false` stops core from sending one. The unmodified Node
  manifest declares `watch_files = []`, so on the shipped manifest a
  `tsconfig.base.json` save triggers no reindex at all. The control therefore
  borrows the Rust watch list to measure what the same reindex costs on Node.
- Embeddings were off in every daemon (`G_MESH_MODEL_DIR` set to a missing
  directory), so no inference ran alongside the walk. The daemon logs contain
  0 `embeddings` lines.
- Arm proof: every measure records which plugin answered. The stderr prefix
  is `[typescript]` for Rust and `[g-mesh-js-ts]` for Node, in the bulk logs,
  the `fileChanged` sessions and the daemon logs (15 vs 0 and 0 vs 7 lines).
- One script (`run.py`, in the slice scratchpad) ran every arm and rep in
  131 s (`real 131.40 user 85.09 sys 10.25`). Measures 2 and 3 alternate the
  arms within each rep. Measure 1 runs one daemon per arm, in sequence.

## 1. Whole-language reindex per watch-file save

Method: start the daemon, activate it with one MCP `find_definition` call
through `g-mesh mcp-shim`, and wait for `initial index built`. Then, per rep,
`touch packages/tsconfig.base.json`, with 5 s of idle time between reps.
`save->swap` runs from the `utime` call to the arrival of `typescript reindex
swapped in` on the daemon's stderr, which a pump thread stamps on arrival.
Bulk-child user/sys comes from `/usr/bin/time -p` in the wrapper around the
re-walk's `--bulk-index` child. "ctl cpu" is the `ps -o time` delta of the
long-lived control-plane plugin process over the rep.

| arm | rep | save->swap real | bulk child real | user | sys | ctl cpu | load1 |
|---|---|---|---|---|---|---|---|
| rust | 1 | 4.327 | 2.60 | 1.57 | 0.11 | +0.06 | 2.87 |
| rust | 2 | 4.186 | 2.57 | 1.55 | 0.11 | +0.03 | 2.50 |
| rust | 3 | 4.257 | 2.60 | 1.57 | 0.11 | +0.04 | 2.27 |
| node | 1 | 9.305 | 7.39 | 7.29 | 0.45 | +0.14 | 3.22 |
| node | 2 | 8.973 | 7.37 | 7.29 | 0.47 | +0.00 | 3.01 |
| node | 3 | 9.381 | 7.65 | 7.51 | 0.47 | +0.00 | 3.06 |

Cold activation (first tool call to `initial index built`): Rust 4.89 s (bulk
child real 3.42, user 1.65), Node 10.05 s (real 8.01, user 7.32).

The Rust plugin logged `workspace changed` 0.32-0.37 s after the save, which
is the watcher's latency. Node logs no such line. The Rust re-walk shows real
2.6 s against user 1.57 s. The same binary run standalone (measure 2) shows
real 1.69 s, so the extra second is time spent waiting, not CPU: in the
daemon, the child's stdout pipe drains only as fast as core ingests into the
staging index. In Node's re-walk, user ≈ real, so the walk is CPU-bound. The
rest of save->swap (about 1.4-1.7 s in both arms) is core's link, plan and
swap plus the watcher delay.

## 2. Cold `--bulk-index`

Method: `/usr/bin/time -p <plugin> --bulk-index <corpus> > out.ndjson`, a
fresh process per rep, arms alternated. The file cache is warm.

| arm | rep | real | user | sys | load1 |
|---|---|---|---|---|---|
| rust | 1 | 1.66 | 1.52 | 0.12 | 3.38 |
| node | 1 | 7.49 | 7.63 | 0.60 | 3.38 |
| rust | 2 | 1.71 | 1.56 | 0.13 | 3.25 |
| node | 2 | 7.21 | 7.40 | 0.60 | 3.25 |
| rust | 3 | 1.69 | 1.53 | 0.13 | 3.30 |
| node | 3 | 7.24 | 7.33 | 0.61 | 3.30 |

Both arms report `658 files, 16650 nodes, 27064 edges`, and each arm's output
was byte-identical across its 3 reps.

## 3. `fileChanged` latency

Method: spawn the plugin's control plane (`<plugin> <corpus>`, under
`/usr/bin/time -p`), read the handshake, then send Content-Length-framed
`fileChanged` requests one at a time. Each latency runs from the write to the
matching response. One process per rep: the first request (on
`packages/math/src/point.ts`) carries any lazy project-model load and is
reported apart from the rest. The three largest `.ts` files by bytes are
base64 wasm blobs, each answered with 2 nodes and 2 edges. `App.tsx`
(13,961 lines, the largest `.tsx`) and `binding.ts` (the largest `.ts` file of
real code) were added so the table includes real re-extract work. Both arms
returned the same diff sizes for every file.

| file (diff size) | rust ms (r1/r2/r3) | node ms (r1/r2/r3) |
|---|---|---|
| first request, point.ts (32 n / 81 e) | 20 / 20 / 20 | 42 / 44 / 47 |
| woff2-wasm.ts, 972 KB (2 / 2) | 10 / 9 / 9 | 17 / 17 / 18 |
| harfbuzz-wasm.ts, 790 KB (2 / 2) | 8 / 8 / 8 | 16 / 15 / 16 |
| woff2-bindings.ts, 132 KB (2 / 2) | 37 / 38 / 37 | 133 / 134 / 135 |
| App.tsx (777 n / 1,869 e) | 128 / 128 / 133 | 526 / 513 / 478 |
| binding.ts (161 n / 557 e) | 29 / 30 / 34 | 106 / 105 / 104 |
| whole process (6 requests + startup) real / user / sys | 0.25 / 0.20 / 0.02 (all reps within 0.01) | 0.93 / 1.08 / 0.09 (±0.04) |

## 4. Id parity

Method: compare rep 1 of each arm's measure-2 output, so each plugin uses its
own resolver. The comparison covers node and edge id sets by kind, then every
field of every shared id. Open sites are fields on the emitted nodes, so the
field comparison covers them. Script: `compare.py` in the slice scratchpad.

| kind | rust | node | only rust | only node |
|---|---|---|---|---|
| File | 658 | 658 | 0 | 0 |
| Function | 2,642 | 2,642 | 0 | 0 |
| Module | 11,507 | 11,507 | 0 | 0 |
| Type | 727 | 727 | 0 | 0 |
| Variable | 1,116 | 1,116 | 0 | 0 |
| CALLS | 5,874 | 5,874 | 0 | 0 |
| DEFINES | 4,499 | 4,499 | 0 | 0 |
| EXPORTS | 2,235 | 2,235 | 0 | 0 |
| IMPORTS | 4,113 | 4,113 | 0 | 0 |
| REFERENCES | 10,329 | 10,329 | 0 | 0 |
| SUPERTYPE_OF | 14 | 14 | 0 | 0 |

Edges: no field difference on any shared id. Nodes: 10 differ, all on
`docComment`, and all from one cause.

- **JSDoc on an interface method signature.** Rust attaches the preceding
  `/** ... */` to a method signature inside an `interface` body. Node emits
  `docComment: null` there. Affected: `DeltaContainer.{inverse, applyTo,
  squash, isEmpty}` (`packages/element/src/delta.ts`),
  `TTDPersistenceAdapter.{loadChats, saveChats}`
  (`components/TTDDialog/types.ts`), `LibraryPersistenceAdapter.{load, save}`
  and `LibraryMigrationAdapter.{load, clear}` (`data/library.ts`). Ids are
  unaffected. The text feeds the node's embedding input, so these 10 nodes
  would embed differently across a plugin switch. Whether Rust's behaviour
  (arguably the more useful one) is accepted as a deliberate divergence from
  Node is a decision for the owner.

## Caveats

- 3 reps per arm on a lightly loaded machine (load 2.3-3.4): enough to order
  the arms, not to bound the variance tightly.
- Measure 1's Node arm runs a modified manifest. The shipped Node manifest
  never reindexes on a watch-file save, so "Node per save" here means "Node,
  if it had watch files".
- The page cache was warm in every rep, including the "cold" bulk index:
  "cold" means a fresh process, not a cold disk.
