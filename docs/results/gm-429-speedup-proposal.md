# GM-429 semantic pass: ranked speed-up proposal

Slice S2 (design) of GM-429. Every number below comes from S1's
measurements in [`gm-429-semantic-pass-time.md`](gm-429-semantic-pass-time.md)
(commit `98cfbaf`); nothing was built or run for this slice. Where a saving
cannot be derived from S1 it is given as a range and the measurement that
would settle it is named. The owner picks what is implemented.

## Ranking

Expected saving = measured share of the phase x the fraction a change can
plausibly remove, per event. "Event" matters: a cold start, a version bump
and a daemon restart happen at different rates.

| # | candidate | event it helps | phase share (S1) | expected saving | edge risk | recommend |
|---|---|---|---|---|---|---|
| F1 | Fix finding 1 (settle latch + "content modified") | warm-RA bump; the next daemon start | answering 27.4-47.2s at 43-57k requests; 3 of 5 passes incomplete, rerun 62.6-63.8s | ~10-20s per warm bump, plus ~38s expected rerun avoided (0.6 x 63s) | none; today's passes are *less* complete | yes, as its own bug task (below) |
| 1 | Early RA start, overlapped with walk + go/python passes | cold start with a rust pass owed | 15.7-16.8s from activation to the rust request, RA idle | ~10-16s | none | yes |
| 1b | Start RA on every daemon activation, even with nothing owed | cold-RA bump (ac) | RA ready +35.5-44.8s vs +11.2-16.3s warm | ~24-29s per cold-RA bump | none | owner decides (2.2 GB idle RSS) |
| 2 | Answering concurrency A/B (8 -> 16/32), RA `numThreads` | every rust pass | answering 36.2-41.9s cold; RA at 1.78-1.86 of 8 CPUs | 0-20s; unknown until measured | none if the edge set is identical | yes, measure first, keep only on a win |
| 3 | `core/build.rs` skips `npm run build` when its inputs are unchanged | every version bump (RA's cargo *and* the developer's) | build-script run 6.86s of 7.8-11.6s | ~6.5-7s per bump, g-mesh repo only | none (build.rs emits no `rustc-env`/`rustc-cfg`) | yes |
| 4 | RA `cachePriming.numThreads` | every RA start | priming 11.1-12.8s | 0-4s; unknown | none | measure with #2, same A/B run |
| 5 | Python: cache the npx resolution across daemon lives | each daemon life of a python project resolved through npx | 1.2-2.1s resolution + probe | ~1-2s | none | low; only if cheap |
| 6 | Fewer questions (skip sites that cannot yield an edge) | every rust pass | 28,535 questions -> 2,618 edges | unknown; potentially the largest | **high** | measure only; no change in GM-429 |
| - | Keep RA alive across daemon restarts | owed restart | RA ready 19.6-21.3s | ~20s | none | no (see below) |
| - | Separate persistent RA target dir | fresh checkout only | build scripts 0.4-1.0s once built | 0s steady; costs 70s once | none | no, unless contention is measured |
| - | Go: persistent/shared `GOCACHE` | first load of a dependency set | `packages.Load` 2.9-3.2s warm | 0s steady | none | no: already the case |
| - | Env guard in `core/build.rs` set through RA's `cargo.extraEnv` | version bump | 6.86s | ~7s, then paid back | none | no: fingerprint flip-flop (see #3) |

Recommended set for GM-429: **#2 + #4 as one measure slice first** (one
constant and one init option, cheapest, and it touches the largest phase),
then **#1** and **#3**. **F1 as its own bug task**, scheduled before any
GM-429 measurement of the warm-bump path, because until it is fixed that
path cannot be measured cleanly.

## F1: finding 1, and where it belongs

What happens (S1 summary, finding 1): `LspClient::settle` latches on a
server's first quiet period, so on every later pass `wait_ready` checks only
`quiet_for(Duration::ZERO)`. After a version bump RA ends "Building
compile-time-deps", the progress set is empty for 13ms, `wait_ready` returns,
and RA then spends 3.4-4.5s on crate graph, roots, proc-macros and priming.
Every question asked in that window is answered empty and deferred
(14.6-28.5k of them), then re-asked after the 2s settle. Requests in flight
at the crate-graph switch get LSP error `-32801 ContentModified`, which
`run_pass` records as a refusal (bridge.rs, "the server refused a question
about ..."), so the pass reports itself incomplete, `semanticPassAt` stays
unset, and the next daemon start reruns the whole pass (r3, r4: 62.6-63.8s).

Fix, in two parts:

1. A `workspaceChanged` (not a per-file `didChange`) resets the latch, so
   the next whole-project pass waits for a full settle (2s) of quiet before
   asking. This keeps GM-290's guarantee that a per-file pass never pays a
   settle; only a pass after a project-model change does.
2. `ContentModified` is treated as "ask again after the server is quiet",
   i.e. it joins the `deferred` list, subject to the same once-only
   `re_asked` rule, instead of failing the pass.

Saving: the wasted first sweep goes. Answering was 27.4-47.2s for 43-57k
requests; half of those are the re-asks, so a single sweep is roughly
14-24s, and the pass pays +2s of settle: **~10-20s per warm bump**. The
larger gain is completeness: 3 of 5 warm bumps ended incomplete, each costing
a ~63s rerun at the next start (**~38s expected per bump**).

Risks: memory none. Edges: none lost - today's deferred questions are already
re-asked; the fix only stops asking them into a known-busy window, and
`ContentModified` questions become answered instead of failed. Other projects:
pyright is `readiness = "on-demand"` and starts latched; the reset must keep
that (an on-demand server should not start waiting 2s per workspace change,
or it should and that is measured - decide in the task). Windows/CI: none
specific.

Measure: warm-RA bump scenario x5 (as S1 b0-b4), before and after. The
observation that tells the arms apart: requests per pass (43-57k before,
~28.5k after), deferrals (14.6-28.5k before, ~0 after) and the incomplete
flag (3/5 before, 0/5 expected). Regression tests in
`plugins/sdk/tests/lsp_bridge.rs`: a scripted server that reports progress
end, is silent for a few ms, then begins new progress and answers `null`
during it - must fail with the latch reset reverted; and a scripted server
answering `-32801` once - must fail with the classification reverted.

Files: `plugins/sdk/src/lsp/client.rs` (`settle`, a reset alongside
`mark_edited`), `plugins/sdk/src/lsp/bridge.rs` (`wait_ready`, `run_pass`'s
error branch), `plugins/sdk/src/run.rs` (the `workspaceChanged` arm at
~line 416 must reach the engine) and possibly `plugins/sdk/src/lib.rs`
(`SemanticEngine` trait), `plugins/sdk/tests/lsp_bridge.rs`.

**Own task, recommended.** Reasons: (a) it is a correctness bug - passes end
incomplete and are rerun - and would be a bug even if it cost no time; (b)
its acceptance is "a warm bump pass is complete and asks each question
once", which is testable without any timing and should not wait on GM-429's
measurement slices; (c) it changes the SDK readiness rules that GM-289,
GM-290 and GM-309 each measured, so it deserves its own review and its own
entry in `git log`; (d) GM-429's warm-bump numbers are not interpretable
until it lands. Against: it is also the largest time saving on the bump
path, and folding it in keeps one measurement story. The owner decides.

## 1: early RA start

Today the rust bridge starts RA inside its first pass
(`LspBridge::ensure_client` -> `LspClient::start`, the only caller), and the
pass is requested only after the walk and the go and python passes: 15.7-16.8s
after activation on a cold start, during which RA does nothing. RA then needs
20.1-23.3s to be ready.

Change: the rust plugin starts its language server as soon as it knows a
semantic pass will follow (1a: when a rust pass is owed; 1b: on every
activation). Readiness is still decided by `wait_ready` inside the pass; only
the spawn moves earlier. RA's messages queue on the client's reader channel
until the pass drains them, and the settle is judged from drain time, so the
readiness rule is unchanged (worst case: one extra 2s settle).

Saving (1a): RA ready moves from ~+38-42s to ~+22-25s after activation:
**~10-16s per cold start**, less whatever CPU contention costs (RA used 1.65
CPUs while priming; the walk and passes used ~6 CPU-s over 18.7s in b4 on
an 8-CPU machine). Owed restart: 0 (its request comes at +0.7s).
Saving (1b): a cold-RA bump becomes a warm-RA bump: RA ready +11.2-16.3s
instead of +35.5-44.8s, **~24-29s per cold-RA bump** (ac1-ac4 75-104s end to
end, warm b0-b4 40-64s, and less once F1 is fixed).

Risks: memory - 1a none (RA would start seconds later anyway); 1b holds RA's
2.15-2.30 GB for the daemon's life even when no rust pass is ever owed,
which today only happens after the first pass. The server must be registered
in `LIVE_SERVERS` exactly as today (`LspClient::start` does that) so
`kill_live_servers` on control-stream close still reaps it - the known
leaked-daemon problem must not grow a new path. A disabled semantic tier or
a missing `rust-analyzer` must not start anything. Edges: none. Other
languages: the hook is in the SDK bridge, so pyright could use it too
(~0.9s spawn), but only rust is proposed. Windows/CI: no platform-specific
code; CI tests that count server spawns need to be checked.

Measure: cold start (S1 b1-b4) x3 per arm, metric "activation to rust
applied" and "walk to all four done" (73.3-82.3s today). Control that tells
the arms apart: the RA spawn timestamp relative to activation (after the
python pass vs before the walk ends). For 1b add the ac scenario and the
daemon's idle RSS.

Files: `plugins/sdk/src/lsp/bridge.rs` (a start-early entry on `LspBridge`
reusing `ensure_client`), `plugins/sdk/src/lib.rs` / `plugins/sdk/src/run.rs`
(the trigger), possibly core's pass scheduling if core is the one that knows
a pass is owed (not located in this slice).

## 2 + 4: answering concurrency and RA threads (measure first)

The bridge already pipelines: `Budgets::concurrency` is 8 (bridge.rs,
`Budgets::default`), `run_pass` keeps 8 requests in flight and blocks on a
channel (`recv_timeout`), not a sleep. So the question is not "parallelise"
but "why does 8 in flight give only 1.85 CPUs". From S1: 28,535 requests in
~38s at 8 in flight is a mean of ~10.7ms per request, while RA spent 73.7-76.4
CPU-s on them, ~2.7ms per request. About three quarters of each request's
life is waiting, and the plugin is at 0.06-0.09 CPUs, so the wait is inside
RA: either the in-flight cap (more requests would fill idle workers) or RA's
own serialisation (main-loop dispatch, its worker pool size). S1 did not try
more than 8, so the saving is **0-20s**: 76 CPU-s spread over 4 physical
cores is ~19s against 36-42s now.

Change: nothing until measured. Arms: `concurrency` 8 / 16 / 32, and RA's
`numThreads` / `cachePriming.numThreads` init options at default vs the
logical-CPU count (#4; priming is 11.1-12.8s and ran at ~1.65 CPUs). If a win
holds, the change is one constant (or a per-manifest value, so pyright, which
is single-threaded node, keeps 8) and one line of
`[plugin.semantic.initialization_options]`.

Risks: memory - RA works on more requests at once; the `Budgets` doc chose 8
partly so as not to multiply peak memory under `[plugin] memoryLimitMb`, so
record RA's peak RSS per arm (2.15-2.48 GB today). Latency - p99 is already
60-100ms, and under load (plugins/rust/plugin.toml records questions blowing
the budget at load average 693 with priming off) a deeper queue pushes requests toward the 10s `Budgets::request`
timeout; check p99/max per arm. Edges: none by construction, but verify:
dump the pass's edge set per arm and diff (must be empty). Other projects:
measure on one more rust repository (tokio was used by GM-319) so the
number is not g-mesh's alone. Windows/CI: none specific.

Measure: the owed-restart scenario (S1 r3/r4: nothing else runs, request at
+0.7s) x3 per arm; metric answering wall, RA CPUs, RA peak RSS, p50/p99,
edge-set diff; record `uptime` and `/usr/bin/time -p` per run. Control that
tells the arms apart: observed maximum in-flight count (instrumented) must
be 8/16/32, and the RA CPU share must move if the cap was the limit; if in
flight rises and CPU does not, the arms were distinguishable and the result
is a real "RA-bound", not a null measurement.

Files: `plugins/sdk/src/lsp/bridge.rs` (`Budgets::default`, maybe
`SemanticConfig` in `plugins/sdk/src/lsp/config.rs` for a per-manifest
value), `plugins/rust/plugin.toml`.

## 3: `core/build.rs` skips an unchanged TS build

After a version bump, `CARGO_PKG_VERSION` changes the package fingerprint and
cargo reruns `core/build.rs` (the only build script that reruns): 6.86s run,
5.3-5.9s of it `tsc` via `npm run build`, 0.3s `go build`. That runs in RA's
build-script phase (7.8-11.6s) and again in the developer's own next
`cargo build`. The script's output is a side effect (the TS plugin's `dist/`
and the Go plugin binary); it prints no `rustc-env`, `rustc-cfg` or
`OUT_DIR` use (grep of `core/build.rs`), so nothing RA analyses depends on it.

Change: `core/build.rs` hashes its TS inputs (the paths it already declares
with `rerun-if-changed`: `plugins/typescript/src`, `package.json`,
`tsconfig.json`) and skips `npm run build` when the hash equals a stamp it
wrote next to `dist/` after the last successful build and `dist/` exists.
A content hash, not mtimes, so an archive extraction or a restore that keeps
old mtimes cannot leave a stale `dist/`. `go build` stays (0.3s, go caches it).

Saving: **~6.5-7s per version bump** in RA's pass and the same again in the
developer's cargo. g-mesh repository only: 0 for any other project.

Risks: a stale `dist/` if the hash misses an input (e.g. a new top-level
file tsc reads); keep the input list identical to the `rerun-if-changed`
list so the two cannot drift. Edges: none. CI: fresh checkouts have no
stamp and build as today. Windows: `npm.cmd` path unchanged; hash over
relative paths with `/` separators so a stamp is portable.

Why not the env guard: setting a skip variable through RA's
`cargo.extraEnv` requires `rerun-if-env-changed`, and RA and the developer
share `target/`, so each would see the other's env as a change and rerun the
script - paying the 7s on every alternation instead of saving it - unless RA
also gets its own target dir (70s once, see below).

Measure: the replicated `cargo --timings` run from S1 (version bump): arm A
without the stamp logic, 6.86s `g-mesh` build-script run; arm B with it,
expected <0.3s. Control: in arm B, touch a file under
`plugins/typescript/src` with a content change - the build must run and
`dist/` must change; a test that fails if the skip ignores an input change.

Files: `core/build.rs`, `.gitignore` if the stamp is not under an ignored
`dist/`.

## 5: python, npx resolution

Each daemon life of a project whose pyright resolves only through `npx`
spends 1.2-2.1s resolving and probing (`pyright --version` through npx) and
0.85-0.92s spawning through npx. Caching the resolved candidate (origin
`npx`, the binary under npm's `_npx` cache) in `G_MESH_HOME` and skipping the
probe when that path still exists would save **~1-2s per daemon life**;
spawning the cached binary directly might save part of the spawn too.
Risks: npx cache eviction or a pyright upgrade leaves a stale path - fall
back to full resolution on any spawn or `initialize` failure; `.cmd`
spellings on Windows (plugins/python/src/semantic.rs already models them).
Measure: two consecutive daemon lives, column "before the pass"; control:
the first life resolves, the second does not (a log line per arm). Files:
`plugins/python/src/semantic.rs`. Low value; only if it is small.

## 6: fewer questions (measure only)

28,535 `definition` questions produce 1,166 nodes / 2,618 edges. If most
answers point outside the project (std, dependencies) or at the site's own
file where the structural pass already has the edge, a pre-filter would cut
the largest phase proportionally. S1 did not classify answers, so the saving
is unknown, and any filter can drop an edge that exists today. Proposed only
as instrumentation: count answers by outcome (empty, outside project, in
project and already known structurally, new edge). A filter would be its own
task, gated on an edge-set diff of zero across several corpora.

## Not recommended

- **Keep RA alive across daemon restarts.** RA already lives for the whole
  plugin process (the bridge holds the client; it survives a workspace
  reindex). Surviving a daemon restart means a detached RA the next daemon
  reconnects to: a new process-ownership protocol and a new way to leak a
  2 GB process, on a machine where leaked daemons are already a known problem.
  1b gives most of the warm-RA benefit without it.
- **A separate persistent RA target dir** (`cargo.targetDir`). With
  `target/` built, build scripts are 0.4-1.0s; a separate dir costs the 70s
  cold build (plus 10s of proc-macro loads) once per checkout and more disk.
  Its only case is contention with the developer's cargo on the shared
  `target/` (lock waits or fingerprint churn), which S1 did not measure. If
  wanted: run `cargo build` while RA's build scripts run and look for
  "Blocking waiting for file lock", and time a no-change `cargo build` right
  after an RA pass.
- **Go `GOCACHE`.** The Go plugin sets no Go environment (no `GOCACHE`/`GOFLAGS`
  in `plugins/go`, no `env_clear` in core), so `go list -export` uses the
  user's default, persistent cache shared with their own `go build`. The 85s
  and 118s runs were a first-seen dependency set and an emptied cache; steady
  state is 2.9-3.2s. Nothing to gain.
- **`cargo metadata` repeats** (4-5 per RA start, 5.3-9.7s summed, partly
  overlapped). They are RA's own reactions to its VFS load and build data;
  the knobs that would stop them (`cargo.noDeps`, disabling build scripts)
  change the crate graph and so the edges.

## Code questions answered with g-mesh

- `select_project "g-mesh"`.
- `find_callers settle`: ambiguous; candidates included
  `lsp::client::LspClient::settle` (plugins/sdk/src/lsp/client.rs).
  `find_callers` by its id: only `LspBridge::wait_ready`.
  `find_definition lsp::client::LspClient::settle`: the latch source quoted in F1.
- `find_callers wait_ready`: only `<LspBridge as SemanticEngine>::answer`.
- `find_definition run_pass`: plugins/sdk/src/lsp/bridge.rs; showed the
  existing pipeline (`in_flight < budgets.concurrency`). `find_references
  concurrency` returned only a semantic neighbour (a field, not indexed as a
  symbol); grep then found `Budgets::concurrency = 8` in bridge.rs.
- `get_file_outline plugins/sdk/src/lsp/client.rs`, then `find_callers` by
  id: `LspClient::start` <- only `LspBridge::ensure_client`;
  `LspClient::shutdown` <- only `<LspClient as Drop>::drop`;
  `kill_live_servers` <- only `run::read_control_stream` (plus one non-call
  reference in lsp/mod.rs). So the server is spawned lazily in the first
  pass and lives until the bridge's client is dropped or the plugin's
  control stream closes.
- Grep (non-symbol or single known file): `GOCACHE`/`GOFLAGS`/`env_clear` in
  plugins and core (none set); `npx` in plugins/python (resolution in
  src/semantic.rs); `rustc-env`/`OUT_DIR` in core/build.rs (none);
  `workspaceChanged` in plugins/sdk/src (run.rs:416); `-32801`/content
  modified in plugins/sdk/src/lsp (not handled).
