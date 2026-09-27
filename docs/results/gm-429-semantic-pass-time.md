# GM-429 semantic pass time: where it goes

Slice S1 (measure) of GM-429. It breaks the rust semantic pass on g-mesh
itself into phases, after (a) a `core/Cargo.toml` version bump under a live
daemon and (b) a cold start, and gives a coarser breakdown of the go and
python passes on projects where they have real work. Numbers only: the
ranked speed-up proposal is slice S2. No product code was changed; the
timings come from temporary instrumentation in a throwaway worktree.

## Summary

The rust pass on g-mesh is two things of similar size: **rust-analyzer
getting ready** (20-38s, most of it RA's own cache priming and, after a
version bump, `core/build.rs`) and **the bridge's 28,544 LSP questions**
(36-58s at a p50 of 6-9ms, eight in flight, with RA using ~1.85 of 8
logical CPUs while it answers them). Store writes are 0.3s.

| rust pass, g-mesh (237 files) | runs | end to end | RA ready | questions answered |
|---|---|---|---|---|
| Cold start, fresh index, target/ already built | b1-b4 | 58.9-67.8s (request to applied) | 20.1-23.3s after spawn | 36.2-41.9s |
| Version bump, RA already running (warm) | b0-b4 | 40.2-64.0s from the edit; **3 of 5 incomplete** | +11.2-16.3s after the edit, then 3.4-4.5s more RA work *after* the bridge believed it ready | 27.4-47.2s, **43-57k requests** (14.6-28.5k of them re-asks) |
| Version bump, RA not yet started (cold RA) | ac1-ac4 | 75.3-103.5s from the edit | 30.0-37.6s after spawn | 39.1-58.2s |
| Restart owing only the rust pass (no edit) | r3, r4 | 62.6-63.8s | 19.6-21.3s after spawn | 39.2-39.9s |
| Cold start, **empty target/** (fresh checkout) | b0 | 180.3s | 118.8s after spawn: build scripts 69.8s, proc-macro load 10.3s, priming 29.9s | 58.5s |

What dominates, in order of size:

1. **Answering the questions** - 36-58s every time, 28,535
   `textDocument/definition` + 9 `textDocument/implementation`. It is the
   largest single phase in every scenario, and after a warm-RA bump it is
   paid twice over (see finding 1).
2. **RA cache priming ("Indexing")** - 11.1-12.8s on a cold RA with a warm
   target, up to 19.7s under load, 29.9s with a cold target. Warm RA after a
   bump: 1.0-1.5s.
3. **Build scripts after a version bump** - 7.8-11.6s, of which
   `core/build.rs` (`npm run build` for the TS plugin plus `go build` for the
   Go plugin) is 6.86s run + 0.97s compile; no other crate's build script
   reruns. With a cold `target/`: 69.8s (82.0s replicated standalone), led
   by `onig_sys` 43.4s, `ring` 34.9s, `tree-sitter` 20.8s.
4. **`cargo metadata`** - RA runs it 4-5 times on start-up (sum 5.3-9.7s,
   partly overlapping the VFS scan), once after a bump (3.2-4.4s).
5. Everything on g-mesh's side is small: plugin hydrate before the pass
   1.2-2.4s, RA spawn + `initialize` 0.3-0.4s, `didOpen` of 217 files 0.6s
   (first pass only), diff apply 0.28-0.44s, sweep 0.07s.

Two findings are correctness, not speed, and are reported here because
they surfaced in the measurement:

- **Finding 1: a warm-RA bump pass asks every question twice, and 3 of 5
  such passes came back incomplete.** `LspClient::settle` latches after the
  server's first quiet period, so on the second pass `wait_ready` returns the
  moment the progress set is empty - which after a bump is the end of
  "Building compile-time-deps". RA starts "Building CrateGraph" 13ms later,
  then "Roots Scanned", "Loading proc-macros" and "Indexing" for another
  3.4-4.5s. Every question asked in that window is answered empty and
  deferred (28,530-28,533 of 28,533 in b1-b3, 14,582 in b0, 21,412 in b4),
  then re-asked. In b0, b3 and b4 some requests in flight at the crate-graph
  switch were answered with the LSP error "content modified"
  (`core/src/protocol/jsonrpc.rs` x8, `core/build.rs` x3,
  `core/tests/serving_while_indexing.rs` x8), which `run_pass` treats as a
  refusal, so the pass reported itself incomplete and core logged "the rust
  semantic pass after a workspace reindex failed". `semanticPassAt` stays
  unset; the next daemon start runs the whole pass again (r3, r4: 63s).
- **Finding 2: the go pass's cost is the Go build cache, not go/types.** On
  torpeek, `packages.Load` is 2.9-3.2s with a warm `GOCACHE` and 85.2s
  (first run, the machine's default cache not yet holding these deps) or
  117.8s (empty `GOCACHE`) cold; everything else in the pass is under 0.4s.

## Machine and method

- MacBook, Intel i7-1068NG7 (4 cores / 8 logical CPUs), 32 GB, macOS
  26.6.2. rust-analyzer 1.97.1 (8bab26f4 2026-07-14, the rustup component),
  go 1.27.0, node 20.6.1, pyright 1.1.414 (via `npx`, as on the owner's
  machine).
- Load average (1 minute) at each run's start is in the tables. It was 4.4-7.9
  for the rust repeats b1-b4, r3-r4, ac1-ac4. It was **not quiet** at the
  beginning (62.7 at 18:02, 93 at 15 minutes) and around b0 (11.9-14.8) and
  go1/py1 (14-22, right after the cold-target cargo replication), from
  processes outside these runs (the owner's g-mesh daemon at 300% CPU,
  among others); those runs are marked. Other daemons and rust-analyzers on
  the machine were left alone.
- Code: `release-3.15.0` at `805686f`. Two throwaway detached worktrees:
  a build worktree (`cargo build --workspace --release`, `npm ci && npm run
  build` in `plugins/typescript`, `npm ci` in `plugins/python`; build time
  not counted) and a separate project worktree P at the same commit, which
  was the indexed project, with `npm ci` in P's `plugins/typescript` so that
  P's `core/build.rs` does the real `npm run build` a developer's checkout
  does. P's `target/` started empty (b0) and was then kept, as a developer's
  is. Both worktrees and every scratch directory were removed afterwards.
- Isolated `G_MESH_HOME` with `models` linked to `~/.g-mesh/models` and a
  `.backup` copy of the owner's embedding cache, so embedding was never the
  bottleneck (6,771 of 6,771 cache hits). Daemons were started directly
  (`g-mesh daemon --project-root P`), activated by one MCP tool call through
  `g-mesh mcp-shim`, and stopped with `g-mesh stop`; every process of the
  daemon's tree still alive after that was SIGKILLed, and `ps` showed none of
  them left at the end.
- Instrumentation (throwaway worktree only, ~70 lines): `GM429 <epoch-ms>`
  lines on stderr from
  - `LspBridge::answer`: pass start, plan built (question count), client
    ready (spawn + `initialize`), documents synced, `wait_ready` done, finish;
  - `run_pass`: per-method count, total and p50/p90/p99/max request latency
    (send to answer), empty answers, deferrals, time to first answer;
  - `LspClient`: server spawn, `initialize` answered, every `$/progress`
    begin/report/end with its title, every server-to-client request;
  - `plugins/sdk/src/run.rs`: response written;
  - core `watcher/apply.rs` `round_trip` / `apply_semantic_pass_in`:
    request sent, response read, diff applied (with counts), vectors stored,
    sweep;
  - Go plugin `semantic.go`: walk, structural extraction, `packages.Load`
    per module, fold, answer.
- rust-analyzer's own log: `RA_LOG_FILE` with
  `RA_LOG=warn,rust_analyzer=info,project_model=info,proc_macro_api=info,load_cargo=info`
  (b0 ran with `RA_LOG=info`, 900 MB, and was filtered afterwards). Phase
  times for `cargo metadata` ("will fetch workspaces" to "did fetch
  workspaces"), build scripts ("Running build scripts" to "set build scripts
  to workspaces") and proc-macro loading come from it, because the bridge
  only reads RA's stdout while a pass is running: progress notifications RA
  sends between passes are timestamped when the next pass drains them.
- A 0.5s `ps` sampler of every process under the daemon (repeat 4, go and
  python runs) gives CPU by process per phase. It misses processes that live
  under 0.5s, so build-script CPU is a floor.
- Cargo: RA's build-script command, copied from its log, rerun by hand with
  `--timings` and `/usr/bin/time -p` (`cargo check --quiet --workspace
  --message-format=json -Zlockfile-path --keep-going --compile-time-deps
  --all-targets -Zunstable-options`, with
  `__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS=nightly` as RA sets it).
- `/usr/bin/time -p` wrapped every daemon, but its user/sys (e.g. b1: real
  137.97, user 6.98, sys 2.32) does not include rust-analyzer: the plugins
  exit with their core and are not reaped into its rusage. Seconds of real
  at single-digit user there mean the daemon waits on the plugin; the
  plugin (`g-mesh-plugin-rust`, 3.7-9.3s CPU per daemon life) waits on
  rust-analyzer, whose CPU is in the `ps` snapshots below.
- Scenarios. One repeat = b*i* (delete the index, start, activate, wait for
  all four passes), then a patch bump of P's `core/Cargo.toml` under the same
  daemon (warm RA), stop; if that pass failed, r*i* (restart, the owed rust
  pass runs, no edit), stop; then ac*i* (restart on the ready index, nothing
  owed so RA is not started, bump, which starts RA inside the reindex's
  pass), stop. Repeats b0 (cold `target/`), 1, 2, 3, 4 (4 with the sampler).

### Code questions answered with g-mesh

- `select_project g-mesh`.
- `search_code "core runs a language's semantic pass after workspace reindex
  swap and applies the diff to the store"` found
  `core/src/daemon/workspace_reindex.rs`, `core/src/daemon/semantic.rs` and
  `IndexStore::swap_language`: the reindex runs the pass itself through
  `PluginSupervisor::semantic_pass` on the same plugin process, so RA
  survives a workspace reindex if the plugin is alive.
- `find_references run_with_registry_and_progress`: called from
  `ActivationCtx::activate` and `ActivationCtx::walk` - the cold-start pass.
- `find_callers daemon::activation::ActivationCtx::activate`: only
  `activation::run`, triggered by the first tool call - which is why a daemon
  started without a client does nothing.
- `find_definition apply_semantic_pass`: `core/src/watcher/apply.rs`, where
  the diff is applied (instrumented there).
- The rest (`bridge.rs`, `client.rs`, `run.rs`, `semantic.go`) was one known
  file each, read with `grep -n` and excerpts.

## Rust pass, per phase

All times in seconds. "t0" is the version bump (bump rows) or the moment the
daemon was started and activated (cold rows). "RA ready" is when
`wait_ready` returned, relative to t0.

| scenario | run | load1 | t0 to pass request | plugin before pass | RA spawn+init | init to ready | cargo metadata (n, sum) | build scripts | proc-macro load | priming | RA ready | RA busy after ready | answering | requests (deferred) | definition p50/p90/p99 ms | answer to applied | t0 to applied |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| cold, empty target/ | b0 | 11.9 | 22.6 (activation not stamped; approx.) | 2.2 | 0.4 | 118.8 | 4, 9.6 | 69.8 | 23 dylibs, 10.3 | 29.9 | +144.1 | 0 | 58.5 | 28,535 (0) | 9.08/34.26/101.65 | 0.3 | 202.9 |
| cold | b1 | 7.9 | 15.7 | 1.9 | 0.3 | 20.1 | 4, 5.3 | 1.0 | 23, 0.1 | 11.1 | +38.0 | 0 | 36.2 | 28,535 (0) | 6.07/18.55/62.58 | 0.3 | 74.6 |
| cold | b2 | 4.7 | 15.7 | 1.9 | 0.3 | 20.6 | 4, 5.7 | 0.9 | 23, 0.1 | 11.3 | +38.4 | 0 | 37.3 | 28,535 (0) | 6.34/18.73/64.14 | 0.3 | 76.1 |
| cold | b3 | 7.0 | 16.6 | 2.1 | 0.4 | 22.2 | 4, 6.6 | 0.7 | 46, 1.1 | 12.5 | +41.3 | 0 | 38.4 | 28,535 (0) | 6.47/19.40/61.81 | 0.3 | 80.0 |
| cold | b4 | 4.4 | 16.8 | 1.9 | 0.3 | 23.3 | 4, 7.2 | 0.8 | 46, 1.5 | 12.8 | +42.4 | 0 | 41.9 | 28,535 (0) | 6.97/22.19/75.75 | 0.3 | 84.6 |
| warm-RA bump | b0 | 12.8 | 5.3 | 1.7 | - | (RA reloading since +0.3) | 1, 4.4 | 11.6 | cached | - | +16.3 | 4.5 | 47.2 | 43,107 (14,582) | 5.08/16.77/67.92 | 0.5 | 64.0 **incomplete** |
| warm-RA bump | b1 | 5.9 | 3.7 | 1.5 | - | | 1, 3.2 | 8.7 | cached | - | +12.1 | 3.6 | 37.7 | 57,059 (28,533) | 2.15/8.47/48.87 | 0.4 | 50.2 |
| warm-RA bump | b2 | 6.2 | 5.1 | 1.3 | - | | 1, 3.6 | 8.7 | cached | - | +12.5 | 3.9 | 27.4 | 57,058 (28,532) | 2.21/6.56/19.79 | 0.3 | 40.2 |
| warm-RA bump | b3 | 5.9 | 4.0 | 1.2 | - | | 1, 3.2 | 7.8 | cached | - | +11.2 | 3.5 | 38.1 | 57,053 (28,530) | 2.08/11.15/38.68 | 0.4 | 49.8 **incomplete** |
| warm-RA bump | b4 | 6.7 | 5.0 | 1.4 | - | | 1, 3.5 | 8.0 | cached | - | +11.7 | 3.4 | 37.6 | 49,937 (21,412) | 3.64/12.92/46.15 | 0.3 | 49.5 **incomplete** |
| owed restart | r3 | 5.9 | 0.7 | 2.0 | 0.3 | 21.3 | 4, 6.5 | 0.7 | 46, 0.9 | 11.7 | +24.3 | 0 | 39.2 | 28,535 (0) | 6.61/20.42/65.39 | 0.3 | 63.8 |
| owed restart | r4 | 6.3 | 0.7 | 1.9 | 0.3 | 19.6 | 5, 6.5 | 0.4 | 46, 2.6 | 11.1 | +22.5 | 0 | 39.9 | 28,535 (0) | 6.74/20.49/69.75 | 0.3 | 62.6 |
| cold-RA bump | ac1 | 6.9 | 4.4 | 2.4 | 0.4 | 37.6 | 5, 7.4 | 9.0 | 23, 0.3 | 19.7 | +44.8 | 0 | 58.2 | 28,535 (0) | 9.29/36.57/94.50 | 0.4 | 103.5 |
| cold-RA bump | ac2 | 6.1 | 3.9 | 2.2 | 0.3 | 35.2 | 5, 9.7 | 9.9 | 23, 0.2 | 13.9 | +41.7 | 0 | 51.9 | 28,535 (0) | 8.36/29.66/88.63 | 0.4 | 93.9 |
| cold-RA bump | ac3 | 5.0 | 3.5 | 2.1 | 0.3 | 30.0 | 5, 8.0 | 8.4 | 23, 0.2 | 11.4 | +35.9 | 0 | 39.1 | 28,535 (0) | 6.70/20.10/65.67 | 0.3 | 75.3 |
| cold-RA bump | ac4 | 5.9 | 3.4 | 1.9 | 0.3 | 30.0 | 5, 8.0 | 8.3 | 23, 0.2 | 11.5 | +35.5 | 0 | 42.7 | 28,535 (0) | 7.06/22.48/79.37 | 0.4 | 78.6 |

Column notes:

- "t0 to pass request": cold rows, the structural walk (5.5-6.3s) plus the
  go (3.0s) and python (6.8-7.5s) passes, which run before rust; bump rows,
  the workspace reindex (`workspace changed` +0.2-0.7s, rust bulk index
  +2.5-3.9s, swap +3.4-5.3s).
- "plugin before pass": core's `semanticPass` request to the bridge's
  first line. On a bump the plugin re-parses every file it was told to
  forget on `workspaceChanged` (`self.index.clear()`, then `hydrate`); on
  a cold plugin the engine factory's `rust-analyzer --version` probe is in
  it too.
- "cargo metadata": RA's workspace fetches up to "RA ready". On start-up RA
  fetches 4-5 times: the initial fetch, then "project structure change" when
  its own VFS load sees the manifests, then once more after build data
  arrives ("build scripts do not match the version of the active
  workspace"). Each fetch is 0.7-3.4s; they overlap the VFS scan ("Roots
  Scanned", 2.4-3.0s).
- "build scripts": `cargo check --compile-time-deps` (RA 1.97 builds only
  build scripts and proc-macros, not a full check), into P's own `target/`.
  0.4-1.0s when nothing changed; 7.8-11.6s after a version bump; 69.8s cold.
- "priming": the "Indexing" progress (`cachePriming`), live-timestamped.
- "RA busy after ready": warm-RA bump only - progress RA began after
  `wait_ready` had already returned (crate graph, roots, proc-macros,
  priming), during which every answer was empty.
- "answering": `run_pass` wall time. Plan building (13ms), document sync
  (628ms first pass, 2ms after) and the response write (18-28ms) are
  negligible.
- "answer to applied": core reading the response and committing the diff
  (1,166 nodes, 2,618 edges: 283-441ms) plus the sweep (66-72ms). Nothing
  owed a vector.

### Build scripts after a version bump (cargo `--timings`, replicated)

| run | real | user | sys | what ran |
|---|---|---|---|---|
| no change | 1.31 | 0.23 | 0.23 | freshness check only |
| after `version = "3.15.99"` in `core/Cargo.toml` | 8.59 | 7.93 | 1.27 | `g-mesh` build-script (run) **6.86s**, build-script compile 0.97s; the other 469 units fresh |
| cold target dir | 81.98 | 155.61 | 32.59 | 471 units, 477.1s summed; `onig_sys` 43.4, `ring` 34.9, `tree-sitter` 20.8, `syn` 14.1, `libsqlite3-sys` 13.2, `tree-sitter-python` 10.8, `rustls` 10.5, `tree-sitter-rust` 9.7, `g-mesh` build-script 9.0 (load average rose to 38 during this run) |

The sampler shows what `core/build.rs` spends its 6.9s on: `node` (the TS
plugin's `tsc` via `npm run build`) 5.3-5.9s CPU, `npm` 0.5-0.6s, `go` (the
Go plugin build) 0.3s, in the bump's build-script window (b4, ac4).

### CPU by phase (sampler, repeat 4)

| run | phase | wall | rust-analyzer (+ proc-macro-srv) | build processes | g-mesh (daemon + plugins) |
|---|---|---|---|---|---|
| b4 cold | activation to rust pass start (walk, go and python passes) | 18.7 | - | 6.1 (node 3.7: tsserver/pyright; go 0.6) | 4.9 |
| b4 cold | pass start to RA ready | 23.7 | 39.1 (1.65 CPUs) | 1.3 | 0.3 |
| b4 cold | answering | 41.9 | 76.4 (1.83 CPUs) | 0 | 2.6 (plugin) |
| b4 warm bump | bump to pass start | 6.3 | 4.1 | 1.4 | 6.8 |
| b4 warm bump | pass start to ready | 5.3 | 0.0 | 6.7 (node 5.9) | 0.2 |
| b4 warm bump | answering (incl. the wasted first sweep) | 37.6 | 69.8 (1.86 CPUs) | 0.8 | 3.5 |
| r4 owed | pass start to RA ready | 19.9 | 38.3 (1.92 CPUs) | 0.8 | 0.2 |
| r4 owed | answering | 39.9 | 73.7 (1.85 CPUs) | 0 | 2.7 |
| ac4 cold-RA bump | pass start to RA ready | 30.3 | 37.8 (1.25 CPUs) | 7.6 (node 5.3) | 0.2 |
| ac4 cold-RA bump | answering | 42.7 | 76.1 (1.78 CPUs) | 0 | 2.7 |

RA's resident size after a pass: 2.15-2.30 GB cold, 2.35-2.48 GB after a
warm bump on top. Cumulative RA CPU at the end of a cold pass: 102-111s
(b1-b3), 158s with the cold target (b0).

## Go and python passes

Projects chosen because they have real work for that language and nothing
else competes:

- **Go: torpeek** (`2809275`, `git archive` copy): 159 Go files, 144k lines,
  one module with heavy third-party deps (`anacrolix/torrent`, `dht`, ...).
  g-mesh's own Go (39 files, deps-light) passes in 3.0s every time and says
  little.
- **Python: GoogleAgenticHackaton** (`fd6139d`, `git archive` copy): 158
  Python files, 15k lines, mostly `src/gatekeeper`. No virtualenv, so
  third-party imports stay unresolved (as on a checkout without one).
  g-mesh's own Python is 14 files, 25 questions.

Three fresh-index cold starts each, plus one go run with an empty `GOCACHE`.

| run | load1 | walk done | pass request to swept | inside the plugin |
|---|---|---|---|---|
| go1 | 21.7 | +5.4 | 85.8 | `packages.Load` **85.2s** (50 packages), structural 0.17, fold 0.17, answer 0.01; apply 0.18 |
| go2 | 11.4 | +3.3 | 3.5 | `packages.Load` 2.9s, structural 0.16 |
| go3 | 8.4 | +3.7 | 3.8 | `packages.Load` 3.2s, structural 0.15 |
| gocold (`GOCACHE` empty) | 7.0 | +3.5 | 118.6 | `packages.Load` **117.8s** |

The go pass is `go list -export` compiling export data for the
dependencies. go1 was the first time this machine's default `GOCACHE` saw
torpeek's current dependency set at this Go version (so the run also filled
that shared cache); from go2 on it is 3s. The walk, the structural
extraction and the answers are under 0.4s together.

| run | load1 | pass request to swept | before the pass (npx resolution + `pyright --version` probe) | server spawn + init (`npx pyright-langserver`) | answering | requests (deferred) | p50/p90/p99 ms | pyright's own progress |
|---|---|---|---|---|---|---|---|---|
| py1 | 14.3 | 17.5 | 1.2 | 0.85 | 15.3 | 1,640 (416) | 32.2/129.4/249.0 | 12.2 |
| py2 | 10.0 | 19.3 | 2.1 | 0.90 | 17.2 | 1,640 (416) | 39.0/148.9/286.3 | 13.9 |
| py3 | 7.5 | 20.9 | 2.1 | 0.92 | 18.7 | 1,640 (416) | 48.3/171.0/336.6 | 15.4 |

pyright is `readiness = "on-demand"`, so `wait_ready` returns at once and
the first 1,224 questions are asked while pyright analyses in the
background ("progress" 12-15s): 416 empty answers are deferred and re-asked.
pyright's node process spent 22.8s CPU over py2's ~17s, about 1.3 CPUs. On
g-mesh's own 14 Python files the pass is 6.8-7.5s, of which ~2.1s is the npx
resolution and probe and ~1.2s the npx spawn.

## Raw observations for ranking speed-ups

Facts only; ranking is S2.

- **Keeping RA alive between passes.** RA already survives a workspace
  reindex (same plugin process). Warm-RA bump: RA was ready +11.2-16.3s
  after the edit against +35.5-44.8s cold-RA; the pass ended at 40-64s
  against 75-104s. But the warm path currently re-asks every question
  (finding 1) and failed 3 of 5 times on "content modified", so its end to
  end is not yet what a warm server can give: answering took 27-47s for
  43-57k requests where one clean sweep is 28.5k.
- **RA priming is the largest start-up phase** with a built target: 11-13s
  of the 20-23s RA takes to become ready (and 20s under load in ac1).
  None of it survived an RA restart in these runs: every cold RA primed
  again for 11-13s.
- **Persistent RA target dir.** RA builds into P's own `target/`; with it
  cold, build scripts take 69.8s and first proc-macro loads 10.3s. Once
  built it is 0.4-1.0s. RA runs cargo with `__CARGO_TEST_CHANNEL_OVERRIDE_...
  =nightly -Zunstable-options --compile-time-deps` against the same target
  dir a developer's own `cargo` uses (not measured: whether the two
  invalidate each other's fingerprints or wait on the build-directory lock).
- **No plugin rebuild from `core/build.rs` during analysis.** After a
  version bump (`CARGO_PKG_VERSION` changes) the only build script cargo
  reruns is `core/build.rs`: 6.86s run + 0.97s compile out of RA's 7.8-11.6s
  build-script phase, most of it `tsc` for the TS plugin. On a checkout
  without `plugins/typescript/node_modules` this would be a fast failure
  with a warning instead.
- **Answering: RA is not saturated.** With 8 requests in flight RA uses
  1.78-1.86 of 8 logical CPUs (4 physical cores) and the plugin 0.06-0.09.
  Latency p50 6-9ms cold, 2-5ms warm; p99 60-100ms. 28,535 of the 28,544
  questions are `definition`, one per open site: the plan's 28,524 `definition` + 9
  `implementation`, plus 11 second-hop `definition` questions from the
  implementation answers. Only 1,166 nodes / 2,618
  edges come out of them. Concurrency above 8 was not tried.
- **cargo metadata repeats.** 4-5 fetches per RA start (5.3-9.7s summed,
  partly overlapping), triggered by RA's own VFS load ("project structure
  change") and by build data. RA is given no `didChangeWatchedFiles`
  capability, so it watches the project itself; it saw the bump 0.2-0.3s
  after it, before g-mesh's reindex asked anything.
- **Ordering in a cold start.** Passes run go, python, rust, typescript,
  sequentially: 3.0 + 6.8-7.5 + 58.9-67.8 + 3.3-3.8s; walk to all four done
  73.3-82.3s (GM-424 measured ~74s). The rust pass does not start its RA
  until the go and python passes are done (t0 to rust request 15.7-16.8s).
- **Plugin hydrate before every whole-project pass after a
  `workspaceChanged`**: 1.2-1.7s re-parse on g-mesh.
- **The core side is small** everywhere: diff apply 0.28-0.44s, sweep
  0.07s, response serialisation under 30ms.
- **Python**: ~3s per daemon life are npx (resolution, `pyright --version`
  probe, `npx` spawn); a third of the questions are re-asked because the
  first sweep runs during pyright's background analysis.
- **Go**: the whole cost is `GOCACHE` state; the pass itself is 3s on a
  144k-line module.
- Incidental: `sed -i` on `core/Cargo.toml` left a transient
  `core/.!<pid>!Cargo.toml`, which the daemon's watcher reported as an
  unclaimed `.toml` file; and RA's `cargo` writing `target/` during a cold
  start made the watcher report `targetGz2S07` (cargo's temporary target
  directory) as unclaimed files. Neither changed any result.
