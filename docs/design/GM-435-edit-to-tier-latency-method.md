# GM-435: how long one edit takes to reach each tool tier (method)

Design note for slice S1 (measurement method). No code, no measurement.
The later slices: S2 instrumentation (code, if approved), S3 measurement,
results in `docs/results/GM-435-edit-to-tier-latency.md`.

## 0. The question and the estimate under test

Scenario: a ~150-line Rust file loses 2 functions (-50 lines) and gains 3
(+60). The 2026-09-28 estimate, built from constants and never measured:

| tier | estimate after save | built from |
|---|---|---|
| (a) structural: `find_definition` finds a new fn, a removed one is gone | ~0.5 s | `DEBOUNCE_WINDOW` 300 ms + one file's parse |
| (b) `search_code` finds a new fn | ~1 s | only the changed doc+signature texts are embedded |
| (c) semantic: a new call edge resolved semantically | ~1.5-3.5 s | per-file pass, no settle (GM-290); rust-analyzer re-analysis is the least certain part |
| no rust-analyzer running | +~20 s | its start |

## 1. What one edit goes through (today's code path)

All of it runs on the watcher thread, serially, inside one
`Unit::WatcherApply` writer unit that commits per step, so a reader sees
each commit as soon as it lands:

1. **FS event.** `notify` 6's FSEvents watcher (latency `0.0`,
   `notify-6*/src/fsevent.rs:275`) delivers the event;
   `watch_and_route_once` records it (`core/src/daemon/mod.rs:754`).
2. **Debounce.** `next_change(DEBOUNCE_WINDOW)` (`mod.rs:753`, window
   300 ms at `mod.rs:83`) waits; `drain_ready` (`mod.rs:762`,
   `core/src/watcher/debounce.rs:52-60`) releases a path quiet for >= 300
   ms. The clock is checked only when `next_change` returns, so the floor
   is ~300 ms and an unrelated event inside the window can push it to ~600 ms.
3. **Route.** `route_settled_path` (`mod.rs:792/796`) ->
   `PluginRegistry::file_changed` (`core/src/daemon/registry.rs:1185`) ->
   `PluginSupervisor::file_changed` (`core/src/daemon/lifecycle.rs:322`):
   applied now if the plugin is awake, queued otherwise.
4. **Plugin parse + store write.** `apply_file_change_in`
   (`core/src/watcher/apply.rs:233`) -> `round_trip` (`apply.rs:959`):
   request written (`:974`), answer read (`:976`), diff committed and
   linked (`apply_file_diff_linked`, `:996`). **Tier (a) is visible here.**
5. **Embedding.** `embedding.compute` (`apply.rs:1019`; cache lookup, then
   the model for the misses, `core/src/embedding/pipeline.rs:449-525`),
   vectors stored (`apply.rs:1024`). **Tier (b) is visible here.**
6. **Re-extract affected files** (variant A only, if the plugin's `affected`
   answer names any): `reextract_affected` (`apply.rs:286`), one more
   `fileChanged` round trip + embedding per file.
7. **Per-file semantic pass.** `apply_semantic_pass_in` (`apply.rs:339`,
   body `:613-795`): linked edges read, `semanticPass` round trip, diff
   committed (`apply_semantic_diff_linked`, `:1000`), new nodes embedded,
   pending rows cleared (`:771`). In the plugin, `LspBridge::answer`
   (`plugins/sdk/src/lsp/bridge.rs:3122-3300`): `ensure_client` (`:3198`),
   `sync_documents` (`:3208`), `wait_ready` (`:3209`, body `:910-937`;
   latched, so a per-file pass pays no settle unless rust-analyzer is
   busy), `run_pass` (`:2432`; questions 8 at a time, `ContentModified`
   answers deferred and re-asked, `:2600`). **Tier (c) is visible at the
   semantic diff's commit.**

`search_code` does not wait for a file change's embedding: its bounded
wait (`G_MESH_SEARCH_EMBEDDING_WAIT_MS`, `core/src/mcp/mod.rs:586-618`)
covers only the bulk backfill phase (`core/src/embedding/backfill.rs:22-26`).
A `search_code` poll therefore answers at once from whatever vectors are
stored.

## 2. Which phases are observable today

`daemon.log` lines carry **no timestamp** (`core/src/log.rs:27-35`;
`log_line!` writes the text and a newline). A harness can stamp each line
when it reads it (tail at 5 ms, file `O_APPEND`, one `write` per line), so a
line's arrival time is its phase's end within ~5 ms.

| phase | observable today? | where | what it gives |
|---|---|---|---|
| save -> FS event | no | - | - |
| debounce (event -> settled) | no | `mod.rs:754/762` log nothing | - |
| route / queue behind a running pass | no | `registry.rs:1185`, `lifecycle.rs:322` log nothing on success | - |
| plugin parse (`fileChanged` round trip) | no | `apply.rs:974-976` | - |
| store write (commit + link) | no | `apply.rs:996` | - |
| tier (a) end-to-end | **yes, from outside** | `find_definition` over MCP, or `nodes` rows in `index.db` | flip time, at poll resolution |
| embedding: texts / cache hits / embedded / duration | **partly** | `log_stats` line `embeddings [fileChanged]: N texts, H cache hits, E embedded, ..., X.Xs` (`pipeline.rs:765-773`), emitted by `finish_file_change` (`pipeline.rs:542-546`) **only when E > 0**; an all-hit file is silent | counts; duration at 0.1 s |
| tier (b) end-to-end | **yes, from outside** | `search_code` over MCP, or `vectors` rows | flip time |
| re-extract of affected files | no (success is silent) | `apply.rs:286` | - |
| semantic pass, plugin side, total | **yes** | `[rust] semantic pass: F file(s), N node(s)/E edge(s) upserted, R retracted, in <Duration>` (`bridge.rs:3282-3291`) | total of sync + wait_ready + questions + answer build, µs resolution, no split |
| wait_ready | only on failure | `bridge.rs:926` (still indexing after X) | - |
| LSP requests (count, latency, `ContentModified` re-asks) | only on failure/timeout | `bridge.rs:2487, 2536, 2664` | - |
| semantic diff commit / its embedding / settle | no (success is silent) | `apply.rs:1000, 1019, 771` | - |
| tier (c) end-to-end | **yes, from outside** | `edges` row with `source = 'semantic'` (`core/src/storage/schema.rs:316-327`), or `find_callees` over MCP | flip time |
| warm gate: index ready, whole-project pass done | **yes** | `g-mesh status --json` in the project; `g-mesh: rust semantic pass over ... complete` (`core/src/daemon/semantic.rs:310`) | - |
| request tracing (tool side) | **yes, env** | `G_MESH_TRACE_CALLS` (`core/src/mcp/mod.rs:95-102`): per-call wait outcome and `waited_ms` | tool call's wait on the index |

Verdict: the three tier flips are observable from outside today, and so are
two phase totals (embedding when it computes something, the plugin's whole
semantic answer). The owner's split (debounce, parse, store write,
embedding with hits, semantic pass split into wait_ready / requests /
answering) is **not** observable: instrumentation is needed (section 3).

## 3. Instrumentation gap (a separate code slice, S2)

### D1. Env-gated timing lines

- **Today:** phase boundaries are invisible; the two existing lines have no
  timestamp, and one of them (`[rust] semantic pass:`) is matched by
  `core/tests/it/semantic_pass_trigger.rs`, so it should not change.
- **Change:** a new env var `G_MESH_EDIT_TIMING` (any non-empty value, read
  once into a `OnceLock`). When set, core and the plugin SDK write one extra
  line per phase boundary:
  `g-mesh timing: t=<unix seconds with µs> <event> key=value ...`.
  Plugins inherit the daemon's environment (`core/src/daemon/plugin.rs:611-621`
  adds variables and clears none), so one variable turns on both sides.
  Unset, nothing is written and nothing is measured (one cached bool check).
- **Example:**
  `g-mesh timing: t=1793520000.412305 rt_committed id=17 method=fileChanged file=core/src/storage/file_rows.rs nodes=9 edges=31`
- **Consequence:** every phase becomes a difference of two timestamps from
  one clock; correlation is by `file=` before the round trip and by `id=`
  after. Normal logs are unchanged. The alternative, always-on lines, adds
  ~12 lines per edit to every user's `daemon.log`; not proposed.

### Edit map (S2)

| # | file : symbol (line today) | event(s) | keys |
|---|---|---|---|
| 1 | `core/src/log.rs` (new, beside `write_line`) | `timing_enabled()`, `log_timing!(event, ...)` | prefixes `t=` from `SystemTime::now()` |
| 2 | `plugins/sdk/src/log.rs` | the same pair (the SDK keeps its own copy of `log_line!`) | - |
| 3 | `core/src/daemon/mod.rs` `watch_and_route_once`, `Change::Path` arm (`:754`) | `fs_event` | `path` |
| 4 | same, after `drain_ready` (`:762`), when non-empty | `settled` | `paths` (count + names) |
| 5 | `core/src/daemon/lifecycle.rs` `PluginSupervisor::file_changed` (`:322`), entry and after the plugin is taken | `route`, `plugin_taken` | `file`, `queued=true/false` |
| 6 | `core/src/watcher/apply.rs` `round_trip` after `write_message` (`:974`) | `rt_sent` | `id`, `method`, `file` (fileChanged) / `files` count (semanticPass) |
| 7 | same, after the response is read (`:976`) | `rt_answered` | `id`, `affected=unchanged/unknown/<n>` |
| 8 | same, after `apply_*_linked` (`:996/1000`) | `rt_committed` | `id`, `nodes`, `edges`, `deleted` |
| 9 | same, after `embedding.compute` (`:1019`) | `rt_embedded` | `id`, `texts`, `hits`, `embedded` (also when `embedded == 0`) |
| 10 | same, after `store_vectors` (`:1024`) | `rt_vectors` | `id` |
| 11 | `apply_file_change_in` around `reextract_affected` (`:286`) | `reextract_start`, `reextract_done` | `n` |
| 12 | `apply_file_change_in` around `apply_semantic_pass_in` (`:339`) | `semantic_start`, `semantic_done` | `id` (the semantic id), `requested`, `owed` |
| 13 | `plugins/sdk/src/lsp/bridge.rs` `LspBridge::answer` after `sync_documents` (`:3208`) and after `wait_ready` (`:3209`) | `lsp_synced`, `lsp_ready` | `files`, `waited_ms` |
| 14 | `bridge.rs` `run_pass` (`:2432`), at return | `lsp_answered` | `questions`, `answers`, `content_modified`, `max_request_ms`, `p50_request_ms` |
| 15 | `bridge.rs` `answer`, before its existing log line (`:3282`) | `lsp_done` | `edges`, `incomplete` |

Tests for S2: one unit test per side that the macro writes nothing when the
variable is unset and a parseable `t=` line when it is set; one control
(remove the gate check) that must fail. No behaviour test changes.

## 4. Harness plan

### D2. Where the worktrees live

- **Today:** worktrees go to `../g-mesh-wt-*`, inside the folder the
  installed g-mesh MCP server covers. Any agent's g-mesh call naming that
  worktree (or `select_project` on it) starts a second daemon under
  `~/.g-mesh`, with its own rust-analyzer, watching the same files.
- **Change:** both worktrees go outside `~/Projects/ClaudeProjects`, under
  `$SCRATCH/gm435/` (APFS, same volume): **B** (binaries) at the
  instrumented commit, built with its own `CARGO_TARGET_DIR=B/target`;
  **P** (the indexed project) detached at the same commit. B and P are
  separate so rust-analyzer's build-script runs in P never touch the
  binaries being measured (the GM-425 arrangement).
- **Example:** `git worktree add --detach $SCRATCH/gm435/P <sha>`.
- **Consequence:** no foreign daemon edits or analyses P during a run; the
  shared `target/` of the main checkout is never built into. Cost: ~6-8 GB
  for B's target, removed after the run with `wt-cleanup.sh`.

### D3. Isolated home, warm start

- **Today:** a fresh `G_MESH_HOME` embeds the whole project cold (GM-425:
  6,026 texts, 751 s).
- **Change:** `H=$SCRATCH/gm435/home`; `models` symlinked to
  `~/.g-mesh/models`; the embedding cache copied with
  `sqlite3 ~/.g-mesh/embedding-cache/cache.sqlite ".backup H/embedding-cache/cache.sqlite"`
  (a consistent copy while other daemons write it); no `config.toml`
  (defaults). The daemon is started directly, without a shell, from Python:
  `subprocess.Popen([B/target/release/g-mesh, "daemon", "--project-root", P], env=...)`
  with `G_MESH_HOME=H`, `G_MESH_EDIT_TIMING=1`, stderr appended to
  `H/daemon-P.log`; its real pid is recorded (a `$!` of a subshell is not
  the daemon). Default idle timeouts (plugin 1 h,
  `core/src/daemon/lifecycle.rs:46`) outlast the run; the script asserts
  the run stays under 50 min per daemon.
- **Warm gate (all must hold before rep 1):** `g-mesh status --json` in P
  says the index is ready with embeddings done; the log has
  `rust semantic pass over ... complete`; one `find_callees` on a known
  method returns semantic rows; rust-analyzer's CPU (sum of its pids,
  `ps -o %cpu=`) is under 10 % in two samples 1 s apart. Then one
  discarded warm-up rep per variant (D6).
- **Consequence:** the measured edits start from the state the estimate
  assumes: daemon up, index ready, rust-analyzer loaded and idle.

### D4. How "save" happens and is stamped

- **Today:** editors save either in place or by write-temp-and-rename; the
  rename shape yields create/delete events and a different routing order.
- **Change:** in place, one `open(O_TRUNC) / write / close` of the whole
  new text. `t_save = time.time()` taken just before `open`; `close`'s
  return time is recorded too (expected < 1 ms).
- **Example:** `t_save=1793520000.100211, t_closed=1793520000.100390`.
- **Consequence:** one FSEvents modify event per save; the
  `fs_event - t_save` gap is FSEvents' own latency. Both clocks are the
  host's realtime clock, so daemon `t=` and harness `t_save` subtract
  directly.

### D5. How each tier is observed

- **Today's options:** poll the MCP tools (what a user sees, but every
  `search_code` poll embeds the query and reranks 30 hits with the
  cross-encoder - CPU that competes with the very embedding and
  rust-analyzer work being timed), or read `index.db` directly (cheap, but
  not literally "the tool finds it").
- **Change:** both, split by role:
  - **db observer** (one thread, read-only `sqlite3` connection
    `file:<H>/projects/<id>/index.db?mode=ro`, WAL so it never blocks the
    writer), every 10 ms, one short read transaction each:
    (a) the 3 new fn nodes exist in the file and the 2 removed ones do not;
    (b) all 3 new nodes have a `vectors` row;
    (c) a `CALLS` edge with `source = 'semantic'` from the new caller fn to
    the target method exists.
  - **structural tool poller** (MCP session 1): `find_definition` of a new
    fn and of a removed fn by qualified name, every 25 ms from `t_save`
    (cheap lookups; each call's round-trip time recorded).
  - **search_code** (MCP session 2): first call when the db observer sees
    (b), then every 100 ms until the new fn is in the top 10 for a fixed
    query (its doc comment's wording).
  - **find_callees** (MCP session 3) of the new caller fn: first call when
    the db observer sees (c), then every 50 ms until the target is listed.
  - **log tail** (thread): new lines of `daemon-P.log` every 5 ms, kept
    with their arrival time (a cross-check of the `t=` stamps).
  Sessions are `g-mesh mcp-shim` processes with cwd P and
  `CLAUDE_PROJECT_DIR` removed, speaking newline-delimited JSON-RPC
  (the `scripts/mcp-smoke.py` `Shim` pattern, stdlib only).
- **Example row:** `a_db=0.392 a_tool=0.405 b_db=0.611 b_tool=0.734
  c_db=1.904 c_tool=1.941` (seconds after `t_save`).
- **Consequence:** resolution is 10 ms + one read for the db times and one
  poll period + call RTT for the tool times (reported beside them); the
  tool time is the user-facing number, the db time is the floor, and their
  gap is the tool's own cost. Search polls start only once the vectors
  exist, so they never compete with the embedding they wait for.

### D6. Scenario files and the edit

Edit applied in each rep: delete two named functions (with their doc
comments) and append three new functions (~60 lines) at the end of the
non-test code. The new text is the same in every rep except one salt token
in each doc comment (D7). New fns: `edit_probe_alpha`, `edit_probe_beta`
(plain helpers, ~20 lines each) and `edit_probe_gamma`, which calls a
method through a local variable:

```rust
let mut debouncer = crate::watcher::debounce::Debouncer::new(std::time::Duration::from_millis(1));
let ready = debouncer.drain_ready();
```

A method call through a variable receiver has no edge before the semantic
pass (the project guidance g-mesh serves says so), so the semantic edge
`edit_probe_gamma -> Debouncer::drain_ready` (target in another file,
`core/src/watcher/debounce.rs:40`, `pub`) marks tier (c) unambiguously.

| variant | file (lines) | removed (lines) | referenced elsewhere | added visibility |
|---|---|---|---|---|
| A: public, used elsewhere in the crate | `core/src/storage/file_rows.rs` (165) | `importers_of` (doc+fn `:106-139`) and `waiting_importers` (doc+fn `:141-158`), ~52 lines | yes: both called from `core/src/storage/index_store.rs:503,505` (g-mesh `find_references`, one file each) | `pub(crate)` |
| B: private | `core/src/cli/status/json.rs` (186) | `languages` (`:126-145`) and `outcome` (doc+fn `:147-186`), ~60 lines | no: only `to_json` in the same file calls them | private |
| TS (coarse) | `task-tracker-mcp/src/domain/taskStatus.ts` (182), in its own detached worktree outside ClaudeProjects | `findUnblockedDependents` (`:101-114`) and `displayStatus` (`:115-141`), ~41 lines | yes: imported by other files under `src/` (`domain/slices.ts`, `domain/lifecycle.ts`, `cli/runBatch.ts`, ...) | `export` |

Both Rust variants are in the `g-mesh` core crate, so rust-analyzer's
re-analysis scope is the same; the difference is cross-file references to
what was removed. Exact line ranges are pinned by the file's sha256 at
setup: a mismatch stops the script before rep 1.

Note on variant A: `index_store.rs` reaches the removed fns through the
module path (`use crate::storage::file_rows::{self, FileScope}`, `:43`),
not by name, so the plugin may answer `affected = unchanged` and re-extract
nothing. The run records `affected` and the re-extract count either way
(edit map #7, #11); see owner decision Q3.

Go and Python: not proposed. g-mesh has no ~150-line Go/Python source in
its own crate graph worth the setup; a Go run would need a third worktree
(`torpeek`, e.g. `internal/swarm/files.go`, 158 lines). Optional, owner Q4.

### D7. Embedding cache and rep-to-rep state

- **Today:** the cache is keyed by text hash; restoring a file re-adds the
  removed fns' exact texts, and re-adding the same new fns would be all
  cache hits from rep 2 on.
- **Change:** cache on (the default a user has). Each rep's three new doc
  comments carry a unique salt (`run <timestamp> rep <n>`), so every rep
  embeds 3 new texts (misses); the restore that follows is all hits.
  `rt_embedded` records texts/hits/embedded for both.
- **Consequence:** reps are comparable (each a first-time edit), and the
  run gets the hit case for free from the restores.

### D8. Reset between reps, order, count

- **Reset:** write the original text back the same way (D4) and observe it
  like an edit (removed fns back, new fns gone, its `semantic_done`),
  recorded as a `restore` row. Then the idle gate: rust-analyzer CPU < 10 %
  in two samples 1 s apart (cap 60 s; on cap the rep is recorded with
  `ra_busy=true`), plus 2 s quiet. Before each rep: file sha256 equals the
  original, and the db observer sees the original state.
- **Order:** one discarded warm-up rep per variant, then A1, B1, A2, B2,
  ..., A5, B5 (interleaved so drift spreads over both). TS: after the Rust
  daemon is stopped, its own daemon and home, 1 warm-up + 3 reps.
- **Fail-fast:** a tier not reached within 60 s, a whole-language reindex
  line (`reindexing all of rust after`), a daemon exit or a sha mismatch
  stops the script and writes the summary of what was measured.

### D9. Machine state and progress

- `uptime` and `sysctl -n hw.ncpu` at start, before each rep and at the
  end; the whole measure step under `/usr/bin/time -p` (`user`/`sys`
  against `real`); per rep, `%cpu`/`rss` of the daemon, the plugin and
  rust-analyzer, and the number of running `rustc` processes (another
  agent's build is the main interference risk, section 5).
- Progress: one line per rep to `$SCRATCH/gm435/progress.log` (never read
  by the agent; for the status line only), e.g.
  `GM-435 measure · rust A 3/5 · a=0.41 b=0.83 c=2.10`.

### One background script

`$SCRATCH/gm435/run.sh` (bash, `set -euo pipefail`), launched once with
`run_in_background`, does in order: worktrees B and P (and the TS one) ->
`npm ci` in B's `plugins/typescript` and `plugins/python`, `cargo build
--workspace --release` in B -> home H -> start daemon, warm gate ->
`measure.py rust` -> stop daemon (SIGTERM to the recorded pid, `ps`
afterwards) -> TS daemon, warm gate, `measure.py ts` -> stop ->
`summarize.py`. Outputs in `$SCRATCH/gm435/out/`: `raw.jsonl` (one row
per rep and restore: tier times, every timing event, machine state),
`daemon-*.log`, `summary.md`. The agent wakes once on its exit.

### Summary table format (`summary.md`, then `docs/results`)

Per variant, seconds after `t_save`, median [min-max] over 5 reps:

| | estimate | A: public | B: private | TS |
|---|---|---|---|---|
| FS event (`fs_event`) | - | | | |
| debounce (`settled - fs_event`) | 0.3 | | | |
| route/queue (`rt_sent - settled`) | - | | | |
| plugin parse (`rt_answered - rt_sent`) | (in 0.5) | | | |
| store write (`rt_committed - rt_answered`) | (in 0.5) | | | |
| **(a) db / tool** | **~0.5** | | | |
| embedding (`rt_embedded - rt_committed`), texts/hits/embedded | | | | |
| **(b) db / tool** | **~1** | | | |
| re-extract (n files) | - | | | |
| semantic: sync / wait_ready / questions (n, ContentModified) / answer build | | | | |
| semantic commit + embed + settle | | | | |
| **(c) db / tool** | **~1.5-3.5** | | | |
| restore (c) | - | | | |

Beside it: call RTTs per tool, machine state per rep, and one sentence per
tier on the estimate (held / off by how much / which phase made the
difference).

## 5. Risks to accuracy

1. **Other agents' builds.** `rustc` under sccache on the same machine
   competes with ONNX embedding and rust-analyzer, the two CPU-bound phases.
   Recorded per rep (load, `rustc` count); see Q2.
2. **Observer load.** Tool polling adds daemon work and store-lock waits;
   kept to cheap `find_definition` at 25 ms, `search_code`/`find_callees`
   only after the db flip. The db observer's reads are WAL reads, but a
   long read transaction would hold back checkpoints: each read is one
   short transaction.
3. **Structural tier resolving the probe call.** If the Rust structural
   tier already resolves `debouncer.drain_ready()`, (c) is not separable
   from (a). The warm-up rep checks the edge's `source`; on `syntactic` the
   probe moves to a call it cannot resolve (a trait method through a
   generic parameter).
4. **search_code ranking.** "Finds" depends on the new fn reaching the top
   10 for the query; if the vector exists and the fn does not rank, that is
   a ranking result, not latency: recorded as `b_tool=not_ranked`, not as
   a timeout. Checked in the warm-up rep.
5. **rust-analyzer background work** after an edit (re-analysis of the
   crate, especially when variant A leaves dangling calls in
   `index_store.rs`) can spill into the next rep: the idle gate (D8)
   covers it. `checkOnSave` is off in the plugin's options
   (`plugins/rust/src/semantic.rs:1380`), so no `cargo check` runs on save.
6. **Debounce extension.** An unrelated event within 300 ms (rust-analyzer
   or a build writing inside P) pushes settling to ~600 ms; `fs_event`
   lines show every path, so such a rep is identifiable.
7. **First-edit effects.** rust-analyzer's first question after start and
   first-touch caches: absorbed by the discarded warm-up rep.
8. **Whole-language reindex.** If the plugin answers `affected = unknown`
   for variant A, the edit becomes a reindex; the script stops on that
   line and reports it as a finding.
9. **Clock.** Daemon `t=` and harness times are the same realtime clock on
   one host; a slew over a few seconds is negligible.

## 6. Must confirm (in S2/S3, before trusting numbers)

1. S2's timing lines reach `daemon-P.log` from the plugin process too
   (environment inherited; checked by one `lsp_ready` line in warm-up).
2. The Rust structural tier leaves `debouncer.drain_ready()` without an
   edge until the semantic pass (risk 3).
3. The `search_code` query ranks the new fn in the top 10 (risk 4).
4. `fs_event - t_save` is milliseconds, not hundreds (FSEvents latency
   `0.0` in notify 6; measured, not assumed).
5. Line ranges of the removed fns match the pinned sha256 of each file at
   the commit S3 measures.
6. TS: the probe call's target in `task-tracker-mcp` (a class method in
   another file called through a local variable) and that its semantic tier
   runs in a worktree without `node_modules` (the type checker still
   resolves project-internal types).
7. The isolated daemon's plugins are B's builds (the plugin paths in the
   log's start-up lines).

## 7. Decisions for the owner

- **Q1. Instrumentation (D1):** env-gated `G_MESH_EDIT_TIMING` lines as a
  separate code slice (recommended), or measure only the three tier flips
  plus today's two totals (no code; no phase split).
- **Q2. Load:** run only when no other agent is building (recommended:
  phase numbers are CPU-bound), or run anytime and report load per rep.
- **Q3. Variant A:** accept `file_rows.rs` (removed fns reached by module
  path, likely no re-extract), or pick a file whose removed fns are
  imported by name, which exercises the re-extract path.
- **Q4. Languages:** Rust A/B + TS (recommended), or add Go (`torpeek`
  worktree, ~10 min more setup).
