# GM-727: starvation watchdog with a stat sweep (design, S1)

Follow-up to GM-725 section 5, option F1. Owner on GM-725 (2026-11-18): "F3
сейчас, F1 в бэклог (Recommended)". F3 (the "No file change reported for N s"
hint) shipped in 4.33.0. This note designs F1.

## 0. The problem in one paragraph

During a heavy build, macOS FSEvents can deliver **no event at all** for tens
of seconds (GM-725 measured the whole synthetic flood plus 10-39 s after it).
It raises no `Rescan` either, so the GM-616 dropped-event path never fires.
The file-anchored tools stay fresh, because `ensure_file_fresh` stats the file
they name. Every symbol-anchored tool (`find_definition` by name,
`find_references`, `find_callers`, `find_callees`, `find_implementations`,
`search_code`) answers from the old index for any file edited meanwhile. A
by-name lookup of a newly added symbol misses, or returns
`semanticNeighbours` guesses with no error flag.

## 1. What the code does today

All line numbers are 1-based, in this branch (off `release-4.36.0`, b20f0ab7).

- **Event reception and dispatch.** `ProjectWatcher::new_inner`
  (`core/src/watcher/mod.rs:75-156`) creates one notify watcher whose
  callback calls `forward_event` (`:346-361`). That function puts
  `WatchSignal::Path`/`Rescan` on an mpsc channel. One consumer thread,
  `spawn_watch_consumer` (`core/src/daemon/watch_loop.rs:93-151`), loops on
  `watch_and_route_once` (`:215-289`). It calls `next_change` (`watcher/mod.rs:243-283`)
  with a 300 ms timeout and stamps the `WatcherClock` (`watch_loop.rs:33-70`)
  when `take_signal_seen` says any signal arrived. Ignored paths count. It then
  debounces and routes settled paths through `PluginRegistry::route_settled_path` ->
  `file_changed` -> `PluginSupervisor::file_changed`
  (`core/src/daemon/lifecycle.rs:311`) -> `PluginProcess::apply_file_change`
  (`core/src/daemon/plugin.rs:889`). Finally it runs a due Rescan.
- **The disk reconcile already exists.** A GM-616 Rescan goes to
  `RescanScheduler` (`core/src/watcher/rescan.rs`), and when due to
  `PluginRegistry::rescan_subtrees` (`core/src/daemon/registry.rs:1252-1268`).
  That reloads the ignore layers, rechecks `.gitignore` files, then calls
  `reconcile_since_index` (`:1308-1433`) with `ReconcileScope::Rescan`:
  - It reads the live set (`graph::files::known_file_paths`, `indexed_files ∪ File nodes`).
  - It walks with the bulk walker (`project_walk::project_files[_under]`,
    `.gitignore` honoured, every-language excludes pruned).
  - It routes deletions, announces creations, then runs
    `ensure_fresh_with` (`:1846-1879`) on every walked file.
  - `ensure_fresh_with` stats, compares the mtime with the
    `indexed_files` baseline, hashes on mismatch, and reindexes through
    `PluginSupervisor::ensure_fresh` -> `staleness::ensure_fresh` ->
    `apply_file_change`.

  **That is exactly F1's sweep.** It only needs a trigger other than an OS
  Rescan.
- **Periodic work.** The only timer loop is `lifecycle::supervise`
  (`lifecycle.rs:861`): idle sleep, memory limits, orphan check. It runs on the
  daemon's main thread, not on the watcher's consumer thread.
- **Owners of `indexed_files` reads.** `storage::file_rows`:
  - `lookup_indexed_file` (`file_rows.rs:178`) is called only by
    `staleness::decide` and `has_indexed_baseline`.
  - `recorded_mtimes` (`:189-199`) reads all baselines in one query. It is
    used only by `index_health`.
  - `graph::files::known_file_paths` (`graph/files.rs:74`) is used only by
    `reconcile_since_index`.
- **A baseline is not written by the watcher path.** `upsert_indexed_file` is
  called only by the walk's `record_walk_baselines`, `staleness::ensure_fresh`
  and `is_stale` (g-mesh `find_callers`). A file the watcher reindexed keeps
  its old baseline, so the next reconcile of it hashes it and reparses it once
  more (section 4.4).

## 2. Decision 1: how starvation is detected

**Today:** nothing detects it. `WatcherClock` only feeds the F3 hint.

**The problem:** "no event for N s" is true both for a starved stream and for
a project nobody is editing. A detector that only looks at the last event
would sweep an idle project forever.

**Options:**

- **A. Unconditional sweep every N s.** No detection.
  - Benefit: simplest. It also covers any event loss.
  - Risk: it costs 0.3-0.8 s of I/O every N s for as long as the daemon lives
    (section 5), idle or not. It also breaks the acceptance criterion "no sweep
    while events are delivered".
- **B. Last-event time only, with backoff.** Sweep when the consumer has heard
  no signal for N s. Double the interval after each sweep that finds nothing
  (15 s -> 30 s -> ... capped at 5 min). Reset it on any event, or on a sweep
  that found a change.
  - Benefit: no new watch, no new file.
  - Risk: an idle project still sweeps forever, just less often. During a long
    build the sweeps find nothing until the agent edits, so the interval backs
    off. The agent's edit can then wait up to the 5 min cap, which is exactly
    the case F1 exists for.
- **C. Canary in the same FSEvents stream, gated by last-event time
  (Recommended).**
  - Mechanism:
    - The watcher also watches a dedicated directory,
      `<state_dir>/watch-canary/`, non-recursively.
    - While the project has been quiet for 5 s, the watchdog writes
      `<state_dir>/watch-canary/probe` at most every 10 s.
    - The callback diverts canary paths to an `AtomicBool canary_heard`. They
      never reach the channel or `signal_seen`, so they never stamp the F3
      clock or get routed.
    - **Starved** = the project has been quiet for 5 s **and** a probe written
      at least 5 s ago is still unheard.
  - Why the same stream: notify 6.1.1's FSEvents backend
    (`fsevent.rs:283-289`, `run` at `:386-420`) puts every watched path into
    **one** `FSEventStreamCreate`. A canary under a second watcher would be a
    separate stream, and GM-720 saw per-stream bimodal starvation.
  - Benefit:
    - An idle project never sweeps. It costs one tiny write and one delivered
      event per 10 s.
    - A starved stream is told apart from a quiet one directly: the canary
      arrives on a healthy stream within about 1 s (GM-725 ctl: 1.05 s) and
      not at all on a starved one.
  - Risk:
    - It adds a second watched path. `watch()` restarts the FSEvents stream,
      so it must happen at construction, before the walk or the start
      reconcile (section 4.1). A restart later could lose events in the
      gap.
    - It writes a file every 10 s while the project is idle. The file is in the
      daemon's own state dir, not the project.
    - On Linux (inotify) starvation of this kind is not known. The canary works
      there too, and Linux overflows already raise a Rescan.

**Example.** An agent edits `b.rs` during `cargo build`. The build writes
`target/`, but the stream is starved, so nothing arrives. 5 s after the last
signal, the watchdog writes the probe. 5 s later the probe is still unheard, so
the stream is starved and the sweep runs. It finds `b.rs` newer than its
baseline and reindexes it. A by-name query now finds `alpha_new`.

In an idle project the probe is heard after about 1 s, so there is no sweep.

**Fallback when the canary cannot be set up** (the directory cannot be
created, or the watch fails; logged once): use option B's backoff. Without it
the watchdog would be off for that daemon. Owner decision 1b.

**Consequence.** A sweep happens only when the stream itself is proven quiet,
which gives the guarantee in section 6.

## 3. Decision 2: thresholds

| Constant | Value | Why |
|---|---|---|
| `QUIET_BEFORE_PROBE` | 5 s | Same as `WATCHER_QUIET_HINT_AFTER`: a healthy stream delivers an edit in about 1 s (GM-725 ctl1/ctl2: 1.05/1.13 s). While events flow, nothing is written. |
| `CANARY_PERIOD` | 10 s | The most often a probe is written while idle. 0.1 writes/s, negligible. |
| `CANARY_DEADLINE` | 5 s | An unheard probe after 5 s is 5x the healthy delivery time. The 300 ms debounce does not apply to the canary. |
| `STARVED_SWEEP_EVERY` (**N**) | 15 s | During starvation, sweep at most every 15 s, and never sooner than 3x the previous sweep's duration after it ended (`RESCAN_MIN_GAP_FACTOR`, reused). A 0.5 s sweep is about 3% of one core. |

**Options for N:**

- 5 s: freshest, but 10%+ of a core at load 30 on a 50k project.
- **15 s (Recommended):** below GM-725's observed starvations (50-100 s).
- 30 s: cheaper, but a by-name query right after an edit misses for up to
  half a minute.

**Resulting bound.**
- The first sweep of an episode comes 10 s after the last delivered signal
  (5 s quiet plus 5 s deadline).
- Once starved, an edit is seen within N + sweep time, about 15-16 s at 50k
  files.
- The task's acceptance criterion ("within ~N s") is checked against
  N = 15 s plus that detection lag.

**A new probe each sweep.** While starved, each sweep also writes a new probe.
If the first probe was lost rather than delayed, the end of starvation is still
detected.

## 4. The sweep

### 4.1 Where it runs

The sweep runs **on the watcher's consumer thread**, at the end of
`watch_and_route_once`, after the Rescan block (`watch_loop.rs:283-288`).

- Benefit:
  - It is serialized with every watcher-routed apply: there is one consumer
    thread.
  - It shares the `gitignores_since` watermark in `Rescans`, so a Rescan and a
    sweep never both recheck one `.gitignore`.
  - It needs no new thread or lock.
- Risk: while a sweep runs (≤1 s), new events queue in the channel. They are
  routed right after it, as during a Rescan today.

The watchdog state lives in `Rescans` as an `Option<StarvationWatchdog>`:

- `Rescans::new()` stays watchdog-free, so the 14 existing call sites of
  `watch_and_route_once` in tests are unchanged and never sweep.
- `spawn_watch_consumer` builds `Rescans::with_watchdog(...)`.
- The signature of `watch_and_route_once` does not change.

The canary is set up in `ProjectWatcher`'s constructor:
`ProjectWatcher::with_canary(root, canary_dir)` watches the canary directory
before returning. `ProjectWatcher::new(root)`, with no canary, keeps its 20+
test callers. The three production sites switch to `with_canary` with
`registry.state_dir().join("watch-canary")`:

- `startup.rs:424`
- `startup.rs:427`
- `activation.rs:310`

All three construct the watcher before the walk or before the consumer's
start reconcile, so the stream restart caused by the second `watch()` can lose
nothing that a later read of the disk does not cover. The canary directory
must not be under the project root: the state dir is under `G_MESH_HOME`.
If it is under the root anyway, the watchdog falls back to option B and logs
why.

### 4.2 What it does

`PluginRegistry::starvation_sweep(conn, watcher, gitignores_since)` is
`rescan_subtrees` over `[""]` with a new `ReconcileScope::Starved`:

1. `watcher.reload_ignores()`. A `.gitignore` or link edit made during
   starvation was missed too. This is a directory walk (`IgnoreLayers::load`
   -> `walk_dirs`), not a file walk.
2. `gitignores_newer_than(since)`, then `gitignore_changed`, as in
   `rescan_subtrees`.
3. `reconcile_since_index(Plain, Starved)`:
   - It walks for new files with the same bulk walker (`project_files`,
     every-language excludes pruned, `indexing_language` filter). New files are
     the walked paths not in the live set. They are announced
     (`announce_created`) and reindexed.
   - Deletions are live paths absent on disk, routed through
     `route_settled_path`.
   - Changed files go through `ensure_fresh_with` -> `apply_file_change`.

### 4.3 The mtime pre-filter (Decision 3)

**Today:** `reconcile_since_index` calls `ensure_fresh_with` for every walked
file. Each call takes the store lock, prepares `SELECT ... WHERE filePath = ?`
(uncached `query_row`), and stats the file. The live set also costs one
`exists()` per file. The probe in section 5 shows this per-row SQL costs 3-4x
the stat itself.

**Change:**
- Before the loop, read all baselines once with `file_rows::recorded_mtimes`
  (one `conn.with`, about 0.03-0.07 s at 50k).
- Stat each walked file outside the store lock.
- Call `ensure_fresh_with` only when there is no baseline or the mtime
  differs.
- Files with matching mtimes are skipped. `decide` would have returned
  `AlreadyFresh` for them without reading anything, so the outcome is
  identical.

**Options:**
- **(a) Apply it in `reconcile_since_index` for every scope (Recommended).**
  - Benefit: one code path. Start and Rescan reconciles get cheaper too.
  - Risk: it changes the start reconcile, which seeded starts also use. A file
    whose stat fails is handed to `ensure_fresh_with` as before, so the error
    is logged as today.
- (b) Only for `ReconcileScope::Starved`.
  - Benefit: no change to start or Rescan.
  - Risk: two loops to keep equivalent.

### 4.4 Concurrency, lock order, no double apply

- **Lock order is unchanged.** The sweep only calls existing functions in their
  existing order:
  - `conn.with` per step, never held across a stat or a plugin call.
  - `PluginSupervisor::inner` is always taken before the store, never under it
    (`index_store::assert_not_held`, `lifecycle.rs:235`).
  - The pre-filter's bulk read is one `conn.with` with no supervisor lock
    held.
  - No new lock.
- **Sweep vs watcher event.** Both run on the consumer thread, so they cannot
  interleave.
- **Sweep vs a query-time `ensure_file_fresh` on an MCP thread.** Both reach
  `PluginProcess::ensure_fresh` (`plugin.rs:1099`), which re-runs
  `staleness::decide` inside `store.unit` under the supervisor's lock. The
  second caller finds the baseline the first one wrote, gets `AlreadyFresh`,
  and does not apply again.
- **Known redundant reparse (accepted, as today).** The watcher's
  `file_changed` path writes no baseline, so the first sweep of an episode
  reparses once each file the watcher reindexed earlier in the session.
  Likewise, an edit the sweep reindexed during starvation is reparsed again
  when FSEvents finally delivers its late event.
  - The results are identical: `apply_file_change` is idempotent for the same
    content.
  - This already happens for Start and Rescan reconciles.
  - The fix would be a baseline write on the watcher hot path (one more
    stat+hash per edit). Not this task: must-confirm M5 counts it.

### 4.5 Logging

- One line when an episode starts: "watcher starved: no event for 10 s and
  the canary is overdue - sweeping every 15 s".
- One line when it ends: "watcher delivers again after X s: K sweeps,
  M files reindexed".
- The per-sweep `reconciled ...` line is logged only when a sweep changed
  something.
- Benefit: the daemon log does not get a 50k-file line every 15 s during a
  long build.

## 5. Cost budget at 50k files (probe)

The probe is a synthetic tree in the scratchpad, `gm727s1/tree50k`:

- 50,000 one-line `.rs` files in 550 directories under `src/`.
- 20,000 files under a git-ignored `target/` that the walk prunes.
- A `git init`, so `.gitignore` applies.

The walk uses `rg --files`, the same `ignore` crate as `g_mesh_walk`. The stat
and baseline passes are in Python, so they are an **upper bound**: per-file
interpreter overhead dominates. Scripts: `gm727s1/scripts/{mktree,probe,probe_bulk,run_probe}.{py,sh}`.
The machine has 8 cores and 32 GB, and was busy with other work: load 11-36.

| Step | real (s), 3 runs | user / sys (run 1) | `uptime` load (1 min) |
|---|---|---|---|
| walk, `rg --files -j1` (single thread, as `project_files`) | 0.30 / 0.19 / 0.21 | 0.07 / 0.13 | 36.6 / 34.2 / 31.9 |
| walk, `rg --files` (parallel) | 0.10 / 0.09 / 0.08 | 0.13 / 0.16 | same |
| today's per-file path: `exists()` pass + stat + per-row `SELECT` (Python) | 0.49 + 2.15 / 0.46 + 2.55 / 0.46 + 2.59 | 1.90 / 1.54 (whole script) | same |
| pre-filter: one `recorded_mtimes` read + stat + compare (Python) | 0.07 + 0.69 / 0.08 + 0.59 / 0.07 + 0.62 | 0.66 / 0.41 (whole script) | 11.1 |

`user`+`sys` is close to `real` in every row, so the time was spent working,
not waiting.

**Estimate for the Rust sweep at 50k files and load 10-35:**

| Part | Time |
|---|---|
| ignore reload (directory walk) | about 0.1 s |
| file walk | 0.2-0.3 s |
| baseline read | 0.05 s |
| stat pass | 0.1-0.2 s, Rust has no per-call interpreter cost |
| deletion `exists()` pass | 0.1 s; it can reuse the stat pass's misses, see the edit map |
| **Total** | **about 0.5-0.7 s** |

The total is at the top of the GM-725 budget (0.1-0.5 s), so pre-filter
decision (a) matters. Without it, the per-row path adds an estimated 0.5-1 s.
At N = 15 s and the 3x gap, the sweep takes at most about 4% of one core,
during starvation only. The measure slice confirms the real number (M1).

## 6. Guarantee: no sweep while events are delivered

The watchdog sweeps only when all three conditions hold at the decision
instant on the consumer thread:

1. The watchdog's own last-signal stamp (fed by the same `take_signal_seen`
   that stamps `WatcherClock`, ignored paths and Rescans included) is at least
   5 s old.
2. A probe is outstanding and unheard for at least 5 s.
3. `STARVED_SWEEP_EVERY` and the gap have passed.

Any delivered signal resets condition 1 and ends the episode. A heard canary
ends it too. The decision and the sweep run on the thread that receives the
signals, so no signal can be processed "during" the decision.

The one boundary case: a sweep already running when events resume finishes
(≤1 s). The events wait in the channel and are routed right after it. No
further sweep starts until the stream is quiet again for 10 s.

## 7. Test strategy (no flooded setup)

Nothing here uses `churn.py` or real starvation. Starvation is a test input:

- **Fake clock.** The watchdog is a pure state machine in
  `core/src/watcher/starvation.rs`, modelled on `RescanScheduler`: every
  method has an `_at(now)` twin. Its last-signal stamp is its own, set by
  `observe_at(now, signal_seen)`, not `WatcherClock::heard`, which reads the
  real `Instant::now()`. That keeps the test's clock the only clock.
- **Fake canary.** A small trait,
  `trait Canary { fn probe(&self) -> io::Result<()>; fn take_heard(&self) -> bool; }`.
  Production writes the file and reads `ProjectWatcher`'s `canary_heard`
  atomic. A test fake decides "heard" or "never heard". "Never heard" is the
  deterministic starvation trigger.
- **Silent watcher.** Expose the existing `watcher::tests::injected_only`
  (`watcher/mod.rs:398-403`) as `#[cfg(test)] pub(crate) ProjectWatcher::injected_only(root)`.
  It unwatches the root and drains the queue, so only injected signals arrive.
- **Daemon-level tests** reuse GM-616's fixtures in `core/src/daemon/tests.rs`:
  `registry_over`, `walked_index`, `indexed_file`, `edit`, `pump_until`,
  `test_plugin::file_changed_requests`. They drive `watch_and_route_once` with
  a `Rescans::with_watchdog(fake clock, fake canary)` and advance the fake
  clock between iterations.
- **One real-OS canary test**, healthy stream only, like the existing watcher
  tests with their 20 s hang guard: a probe written to the canary directory
  sets `canary_heard`, does not set `signal_seen`, and never comes out of
  `next_change`.

### Behaviours the tests must pin (with the control that tells the arms apart)

| # | Behaviour | Test shape | Control (revert in code) |
|---|---|---|---|
| B1 | Starved (fake canary never heard, no signals, fake clock +20 s): an edit made before is reindexed. Only that file reaches the plugin. | daemon, silent watcher | drop the sweep call in `watch_and_route_once` |
| B2 | Quiet but healthy (canary heard each probe, clock +60 s): **no sweep**, so the same edit is not reindexed. A probe was written. | daemon | make `take_heard` ignored (treat every probe as unheard) |
| B3 | Signals flowing (inject one ignored path per simulated second, canary never heard, clock +60 s): no sweep. | daemon | remove the "quiet ≥ 5 s" gate |
| B4 | While starved, sweeps are at least `STARVED_SWEEP_EVERY` apart and respect 3x the last duration. | pure, `_at` | drop the interval check |
| B5 | A file created and one deleted during starvation are added and removed. | daemon | skip the walk's `created` branch for `Starved` |
| B6 | The pre-filter skips files whose mtime matches and still reindexes a file with no baseline. | daemon, reconcile | pre-filter treats a missing baseline as fresh |
| B7 | A real canary write is heard, is not a signal, and is not routed. | watcher, real OS | forward canary paths like any other path |
| B8 | Episode end: a heard canary or any signal stops further sweeps until a new 10 s quiet. | pure | do not clear `starved_since` on a signal |

That is 8 controls, within the 6-8 rule. B1-B3 and B5 use no real time: the
fake clock decides, and the injected-only watcher receives nothing from the OS.
They are repeatable without the 5x-under-load concern, but the rule still
applies (S4/S5 run them 5x). B7 has a timer (its hang guard) and runs 5x too.

## 8. Edit map for the code slice (S2)

**Read** (do not change):

- `core/src/watcher/rescan.rs` (whole): the `_at` pattern and the gap factor
  to copy.
- `core/src/daemon/registry.rs`:
  - `:1846-1879` `ensure_fresh_with`
  - `:1222` `gitignores_newer_than`
  - `:949-968` `route_settled_path`
- `core/src/daemon/lifecycle.rs:475-520` `PluginSupervisor::ensure_fresh[_with]`
  (lock order).
- `core/src/daemon/plugin.rs:1099-1130` `PluginProcess::ensure_fresh`
  (re-decide under the lock).
- `core/src/ingest/staleness.rs:237-291` `is_stale` and `decide`.
- `core/src/daemon/tests.rs:1003-1125`: GM-616 fixtures and tests.
  `:1360-1440`: `WatcherClock` tests.
- `core/src/daemon/lifecycle.rs:991` `purge_state_dir`: it already removes the
  whole state dir, so `watch-canary/` goes with it. Check this, do not change
  it.

**Change:**

| File:lines | Change |
|---|---|
| new `core/src/watcher/starvation.rs` (+ `pub mod starvation;` in `watcher/mod.rs:1-5`) | `StarvationWatchdog` with the four constants, `observe_at`, `decide_at(now, canary_heard) -> Action { Idle, Probe, Sweep }`, `swept_at(ended, took)`; `Canary` trait; the backoff fallback (option B) when built without a canary. Unit tests B4, B8 in its own `#[cfg(test)] mod tests`. |
| `core/src/watcher/mod.rs:35-57` `ProjectWatcher` | Add `canary: Option<(PathBuf, Arc<AtomicBool>)>`. |
| `core/src/watcher/mod.rs:59-156` `new`/`new_inner` | Add `with_canary(root, canary_dir)`: create the directory, check it is not under `root`, and `watch(canary_dir, NonRecursive)` on the same `RecommendedWatcher` before `watch(root)`. A failure is logged and leaves `canary: None`, not an error. The callback (`:91-99`) splits each event's paths: canary-dir paths set `canary_heard`, the rest go to `forward_event`. Extract the split into a free function so B7's unit half can call it directly. |
| `core/src/watcher/mod.rs` near `:285` | `ProjectWatcher::canary()` returns the production `Canary` impl (writes `probe`, swaps `canary_heard`). Move `injected_only` (`:398-403`) out of `mod tests` as `#[cfg(test)] pub(crate)`. |
| `core/src/daemon/watch_loop.rs:156-174` `Rescans` | Add `watchdog: Option<(StarvationWatchdog, Box<dyn Canary + Send>)>`, add `with_watchdog`, keep `new`/`with_scheduler` watchdog-free. |
| `core/src/daemon/watch_loop.rs:215-289` `watch_and_route_once` | Keep the `take_signal_seen` result in a local (`:221`), and feed both `WatcherClock::heard` and `watchdog.observe_at`. After the Rescan block (`:283-288`), ask `decide_at`: on `Probe`, call `canary.probe()`; on `Sweep`, swap the `gitignores_since` watermark as the Rescan block does, call `registry.starvation_sweep`, then `swept_at`. Episode start and end log lines (4.5). |
| `core/src/daemon/watch_loop.rs:93-151` `spawn_watch_consumer` | Build `Rescans::with_watchdog(StarvationWatchdog::new(), watcher.canary())`. Without a canary, build the fallback watchdog. |
| `core/src/daemon/registry.rs:254-260` `ReconcileScope` | Add `Starved` (whole root, like `Start`, with its own log trigger text). |
| `core/src/daemon/registry.rs:1252-1268` | Add `starvation_sweep` next to `rescan_subtrees`, sharing its body with scope `Starved`. |
| `core/src/daemon/registry.rs:1308-1433` `reconcile_since_index` | The mtime pre-filter (4.3): one `recorded_mtimes` read, stats outside the lock, `ensure_fresh_with` only on mismatch or no baseline. Derive `deleted` from the live paths the stat pass found missing plus live paths not walked whose `exists()` is false, so live files are not statted twice. For `Starved`, log the `reconciled` line only when something changed (4.5). |
| `core/src/daemon/startup.rs:424,427`, `core/src/daemon/activation.rs:310` | `ProjectWatcher::with_canary(root, registry.state_dir().join("watch-canary"))`. |
| `core/src/daemon/tests.rs` (after `:1300`) | B1-B3, B5, B6 (written in S4, not S2). |

**Callers and references this map relies on**, and the g-mesh call that
produced each:

- `apply_file_change`, two declarations (`find_callers` `nameAmbiguous`).
  Callers per declaration (`find_callers` by `symbol_id`):
  - `PluginProcess::apply_file_change` (`plugin.rs:889`):
    `PluginSupervisor::file_changed`, `replay_pending`, plus 4 tests.
  - `ingest::apply::apply_file_change` (`apply.rs:88`):
    `PluginProcess::send_one`, `plugin_check::session::Driver::step`, plus
    tests.
  - The query-time path reaches it through `staleness::ensure_fresh`.
- Watcher reception and dispatch: `get_file_outline core/src/daemon/watch_loop.rs`,
  then reads of `watch_and_route_once`, `next_change` and `forward_event`.
- `ProjectWatcher::new` callers (`find_callers`): 3 production sites
  (`startup.rs` `start_watcher_and_activation`, `activation.rs`
  `ActivationCtx::walk`) and 20 tests. Hence a new constructor, not a changed
  signature.
- `indexed_files` reads:
  - `find_callers lookup_indexed_file`: `staleness::decide`,
    `has_indexed_baseline`.
  - `find_callers known_file_paths`: `registry.rs` only.
  - `find_callers upsert_indexed_file`: walk baselines, `staleness`, tests.
    The watcher path does not write a baseline.
- Periodic tasks: `search_code` "background thread that periodically checks
  idle plugins and memory limits", then `find_callers check_memory_limits_all`,
  which gave `lifecycle::supervise`.

## 9. Must confirm (later slices)

- **M1 (measure, S6).** Sweep cost on a large project with the release binary:
  - the `reconciled ... in X ms` line of a forced `Starved` sweep;
  - repeated 3x;
  - with `uptime` and `/usr/bin/time -p` of the daemon over the window;
  - at 50k files (the `tree50k` generator here, or a real large repo).
  - It must state the number against N = 15 s. If it is above about 1 s,
    revisit N or the gap factor with the owner.
- **M2 (measure, S6).** GM-725's reproduction with the watchdog.
  - Copy `churn.py` from
    `/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/7ee728f3-349d-4163-9680-c24d1b1a2660/scratchpad/gm725s1/`
    (`churn.py`, `driver.py`, `run.sh`, `plugroot/`). It is a scratchpad that
    a reboot clears, so copy it into the S6 scratchpad first.
  - Arms:
    - **churn**: `alpha_new` by name must be found within about 25 s while
      0 `fs_event` lines are logged for `b.rs`, and the log must show the
      episode-start line.
    - **no-churn control**: found at about 1 s, **no** episode line, no
      sweep.
- **M3 (measure).** In the no-churn control, leave the daemon idle for 2 min:
  0 sweeps, about 12 probes heard. This is the "quiet project does not sweep"
  check on the real OS.
- **M4 (verify).** The canary really shares the project's stream on macOS:
  - under churn the canary goes unheard (episode line present);
  - in the control it is heard.

  If the canary is heard while project events are starved, option C is
  unsound and the fallback is option B. That would be an owner decision.
- **M5 (measure).** Count the redundant reparses (4.4) in the churn arm: the
  plugin's `fileChanged` requests per edited file, expected 2 (sweep plus
  late event). If more, find out why before merging.
- **M6 (review, S3).**
  - The pre-filter is semantics-preserving for the `Seeded` start: same
    reparsed set and the same semantic-pending record.
  - A stat failure still goes through `ensure_fresh_with`.

## 10. Owner decisions

1. Detection: **C, a canary in the same stream gated by 5 s quiet
   (Recommended)**, B (last-event time with backoff) or A (unconditional).
   1b: when the canary cannot be set up, fall back to B (Recommended), or
   disable the watchdog for that daemon.
2. N = **15 s (Recommended)**, 5 s or 30 s, with 5 s quiet and a 5 s canary
   deadline.
3. The mtime pre-filter: **in `reconcile_since_index` for every scope
   (Recommended)**, or `Starved` only.
4. Accept the redundant reparse in 4.4 for this task (**Recommended**), or
   also write a baseline on the watcher path, which adds a stat and a hash to
   every edit.
