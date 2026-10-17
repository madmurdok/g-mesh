# GM-616: the daemon drops the FSEvents Rescan signal

Status: design (slice S1). No production code changed.

## Summary

- **Drop path: proven.** Reproduced with notify 6.1.1 on this machine: under a
  filesystem-event storm FSEvents sent 85 `MustScanSubDirs | UserDropped` events in
  25 s, every one naming the watched root. g-mesh's watcher forwards only the path
  and loses the flag; the daemon then skips the root path as "the project root
  itself". The edits FSEvents dropped are never reindexed by the watcher.
- **Chosen fix:** carry the rescan as its own signal from the watcher to the
  daemon. The daemon coalesces rescans and then runs the startup reconcile
  (`reconcile_since_index`, Plain mode), limited to the named subtree. It also
  reloads the ignore layers and rechecks `.gitignore` files changed since the
  last reconcile.
- **Open decision for the owner:** the rate-limit numbers (settle 1 s, cap 30 s,
  gap of at least 3x the last run). See "Decision for the owner".

## 1. What happens today

### 1.1 What notify emits

notify 6.1.1 (`Cargo.lock`), `src/fsevent.rs`:

- `translate_flags` (l.93-145): when `MUST_SCAN_SUBDIRS` is set it pushes
  `Event::new(EventKind::Other).set_flag(Flag::Rescan)`, with info
  `"rescan: user dropped"` or `"rescan: kernel dropped"`.
- `callback_impl` (l.494-545) then adds the FSEvents path to it
  (`ev.add_path(path)`).
- Apple's `FSEvents.h`: *"Your application must rescan not just the directory
  given in the event, but all its children, recursively."* When `UserDropped` or
  `KernelDropped` is also set, *"the client must do a full scan of any directories
  ... being monitored by this stream."*

So a Rescan comes in two shapes:

- with a drop flag, it names the watched root;
- after hierarchical coalescing, it names some directory.

Linux behaves the same way. `src/inotify.rs:208-211` turns `IN_Q_OVERFLOW` into a
Rescan event **with no path at all**.

### 1.2 What g-mesh does with it

1. **The watcher callback loses the flag.** `ProjectWatcher::new_inner`
   (`core/src/watcher/mod.rs` l.91-99; the polling fallback at l.125-132 does the
   same) runs `for path in event.paths { tx.send(path) }`. The channel carries a
   bare `PathBuf`, so a Rescan looks exactly like a write to the directory.
   - An inotify overflow has no paths, so nothing is sent at all.
2. **`next_change` (l.232-256) passes the path through.** The root is never
   ignored, so `Some(<root>)` is returned.
3. **`watch_and_route_once` drops it** (`core/src/daemon/mod.rs` l.689-747). The
   root becomes `file_path == ""` and is skipped at l.711-715 ("The project root
   itself (macOS reports it for a write inside it)").
   - A Rescan naming a subdirectory instead gets `classify_settled` and reaches
     `route_settled_path` → `file_changed`. `language_for(<dir>)` is `None`, so the
     path is skipped as unroutable.
   - Either way, nothing is reindexed.

### 1.3 Repro (this slice)

The probe is `scratchpad/gm616/probe`, a standalone crate on `notify = "=6.1.1"`
using `recommended_watcher` with a callback as cheap as g-mesh's (an atomic
increment; it prints `need_rescan()` events). The script is `scratchpad/gm616/run.sh`:

- 12 shell writers each create and delete 200 files in a loop for 25 s, in a
  watched temp dir.
- Load average was 95 at the start and 147 at the end (other agents were running).

| arm | callback | events | Rescan events | Rescan paths |
|---|---|---|---|---|
| fast | no sleep (production shape) | 209 580 | **85** | all = watched root, `rescan: user dropped` |
| slow | 2 ms sleep | 14 171 | 17 | all = watched root, `rescan: user dropped` |

This matches GM-607 arm 2, where `a.python-src`'s change never reached the watcher
in 60 s (`docs/design/GM-607-burst-debounce-flakes.md` l.62-67, 77-89).

The fast arm matters: dropping happens even with a callback that does nothing but
a channel send. A slow consumer is not required, only fseventsd pressure, such as
a full test suite, an `npm install` or a `git checkout` of a big tree.

### 1.4 What partly heals it today, and what does not

- **The query-time safety net.** `PluginSupervisor::ensure_fresh`
  (`core/src/daemon/lifecycle.rs` l.481-484, documented as the net "for a change
  ... dropped by the backend") is called from `mcp/mod.rs` l.403 for the file a
  query **anchors** on. It catches only that one file.
  - Example: `b.rs` calls `foo` in `a.rs`. Someone renames `foo` → `bar` in
    `a.rs`, and FSEvents drops the event.
  - `find_callers foo` anchors on `a.rs`, so `a.rs` is reindexed.
  - `find_references` from `b.rs`, `search_code`, and a `get_file_outline` of any
    other edited file still read the old graph. That lasts until the next daemon
    start (startup reconcile) or until something touches the file again.
- **The startup reconcile** (`reconcile_since_index`) fixes everything, but only
  at the next daemon start.

## 2. Cross-file facts used (g-mesh calls)

| question | answer | call |
|---|---|---|
| who calls `ProjectWatcher::next_change` | only `daemon::watch_and_route_once` (plus 5 untyped calls inside `watcher/mod.rs` tests) | `find_callers ProjectWatcher::next_change` |
| who calls `watch_and_route_once` | only `daemon::spawn_watch_consumer` (`daemon/mod.rs` l.628); daemon tests call it directly (`daemon/tests.rs` l.300-498, grep) | `find_callers watch_and_route_once` |
| what reindexes one path | `PluginRegistry::route_settled_path` (registry.rs l.1050) → `file_changed`; for a stat/hash-gated reindex, `ensure_fresh` (l.1873) / `ensure_fresh_with` (l.1880) | `find_definition route_settled_path`, `find_callers PluginRegistry::ensure_fresh` |
| existing offline-edit scan | `PluginRegistry::reconcile_since_index(conn, ReconcileMode::Plain)` (registry.rs l.1374-1477), called once from `spawn_watch_consumer` (`ConsumerStart::Reconcile`) | `find_definition reconcile_since_index`, `find_callers PluginRegistry::reconcile_since_index` (1 untyped call in `daemon/mod.rs`) |
| subtree walk already available | `project_walk::project_files_under(root, pruned, subtrees)` (project_walk.rs l.72), used only by `gitignore_changed` | `find_callers project_files_under` |
| `.gitignore` recheck | `recheck_gitignores_since_index` (registry.rs l.1297-1331), with the watermark fixed to `bulkIndexedAt` | `find_definition recheck_gitignores_since_index` |

## 3. Options

All three options start the same way: the watcher stops discarding the flag (§4,
step 1). They differ in what the daemon does when a Rescan arrives.

### A. Reuse the startup reconcile, limited to the subtree (chosen)

On a Rescan, run `reconcile_since_index`'s Plain logic over the named subtree:

1. Walk the subtree.
2. Read the live set (`indexed_files` ∪ `File` nodes).
3. Route live files missing on disk as deletions.
4. Announce and reindex walked files that are not live.
5. Call `ensure_fresh` on every known file. Its mtime fast path leaves untouched
   files unread, and its hash check refreshes only the mtime of a touched file.

- **Benefit.** This code is already tested and is what every daemon start runs.
  It covers edits, creations and deletions, and records new baselines.
- **Cost on a large root.** One gitignore-aware walk, one `SELECT` of live paths,
  and per file one `stat` plus one `indexed_files` lookup. Only changed files are
  hashed or reparsed.
  - Proxy measurement (Python walk+stat at load 90): 983 files took
    `real 1.15 / user 0.11 / sys 0.11`. The process was mostly waiting, so that is
    roughly 0.2 ms CPU per file.
  - Extrapolated: about 10 s CPU for a 50k-file root, plus SQLite lookups.
  - Because every observed Rescan named the **root**, limiting to a subtree rarely
    helps. Rate-limiting (C) is what keeps this affordable.
- **Risk.** A storm produces about 3 Rescans/s (85 in 25 s). Without coalescing,
  the reconcile would run back to back on the watcher thread and starve normal
  routing. C is therefore part of A, not an alternative to it.

### B. A dedicated mtime/hash diff (snapshot compare)

Keep an in-memory snapshot of (path, mtime, size) and diff it against a fresh walk.

- **Benefit.** No SQLite read per file.
- **Cost.** A second source of truth beside `indexed_files`, memory per file, and
  a separate code path that needs its own tests for creation, deletion and links.
  `indexed_files` already **is** that snapshot, and `ensure_fresh` already does
  the two-tier mtime/hash compare.
- **Rejected:** it duplicates A's machinery for a saving that does not matter at
  a rate-limited cadence.

### C. Rate-limit / coalesce (combined with A)

A small clock-injected scheduler holds the pending subtrees:

- A nested subtree folds into its ancestor, and a root Rescan absorbs all others.
- It **fires** when no new Rescan has arrived for `RESCAN_SETTLE` (1 s), **or**
  when the oldest pending one is `RESCAN_MAX_DELAY` (30 s) old, so a storm that
  never stops still gets reconciled.
- It never fires sooner than `RESCAN_MIN_GAP_FACTOR` (3) × the previous run's
  duration after that run ended. A 10 s reconcile on a huge root therefore takes
  at most about a quarter of the watcher thread during a storm.
- Rescans that arrive while a run is in progress stay pending, so the trailing
  run catches edits made during the previous one.

A reconcile reads the disk at the moment it runs. An edit after that read either
produces its own event or another Rescan, so coalescing cannot lose an edit. It
only delays one.

## 4. Chosen design: A + C

1. **The watcher separates the two signals.**
   - The channel type becomes `enum WatchSignal { Path(PathBuf), Rescan(PathBuf) }`.
   - One shared function, `forward_event(event, root, tx)`, used by both the
     FSEvents/inotify callback and the poll fallback:
     - `need_rescan()` → `Rescan(p)` for each path, or `Rescan(root)` when the
       event has no paths (inotify overflow);
     - otherwise → `Path(p)` per path, as today.
2. **`next_change` returns `Option<Change>`** (`Change::Path(PathBuf)` |
   `Change::Rescan(PathBuf)`).
   - A Path goes through remap and ignore exactly as today.
   - A Rescan goes through remap only, with no ignore check, because the root is a
     directory the ignore layers do not judge.
     - `Remap::Drop`, or a path outside the root → widened to the root.
     - `Remap::To(p)` → `p`.
3. **`watch_and_route_once` sends a Rescan to the scheduler, not the debouncer.**
   - After routing the batch, it asks the scheduler whether a run is due.
   - If so, it calls `PluginRegistry::rescan_subtrees(conn, &subtrees, watcher)`.
     That function:
     - a. calls `watcher.reload_ignores()`, because a dropped `.gitignore` or link
       edit would otherwise leave the layers stale;
     - b. passes the changed links, plus the `.gitignore` files whose mtime is
       newer than the previous reconcile's start (whole seconds), to
       `gitignore_changed`;
     - c. runs the Plain reconcile over the subtrees.
   - The scheduler and the `.gitignore` watermark live beside the `Debouncer` in
     `spawn_watch_consumer`.
     - The watermark starts at the consumer's start, so the startup reconcile and
       recheck count as the first run.
4. **`reconcile_since_index` gains a subtree parameter.**
   - `&[""]` keeps today's whole-project behaviour, so startup is unchanged.
   - A non-empty subtree list walks with `project_files_under` and filters `live`
     with `under_any`, made `pub(crate)`.
   - Its log line names the trigger, so a Rescan run is visible in `daemon.log`:
     `reconciled N files with the disk after a dropped-event rescan of <subtree>`.

**Example after the fix.** A `git checkout` of a large branch causes FSEvents to
drop, and the root Rescan arrives.

1. Ordinary events keep routing.
2. One second after the storm ends (or at most 30 s into a continuous one), the
   scheduler fires one reconcile.
3. `ensure_fresh` reparses only the files whose mtime/hash changed, and deleted
   files are retired.
4. `find_references` from any file now sees the new graph, with no daemon restart.

**Consequence.** A dropped event is healed within seconds, at the cost of one
stat-level walk of the root per coalesced storm.

## 5. Edit map

| file | function | lines (today) | change |
|---|---|---|---|
| `core/src/watcher/mod.rs` | `ProjectWatcher` struct | l.42-55 | `events: Receiver<WatchSignal>` |
| `core/src/watcher/mod.rs` | `new_inner` | l.74-153 (callbacks l.91-99, l.125-132) | both callbacks call the new `forward_event(event, &root, &tx)` |
| `core/src/watcher/mod.rs` | new `WatchSignal`, `Change`, `forward_event` | next to `next_change` | flag → `Rescan`, empty paths → `Rescan(root)` |
| `core/src/watcher/mod.rs` | `next_change` | l.232-256 | returns `Option<Change>`; Rescan remaps and skips the ignore check |
| `core/src/watcher/mod.rs` | test hook `#[cfg(test)] inject_rescan(&self, path)` | new | keeps a `Sender` clone so tests can inject a Rescan without an FS storm (same pattern as `new_simulating_watch_error`, l.70) |
| `core/src/watcher/rescan.rs` (new) | `RescanScheduler { record_at, due_at, ran(duration) }` | new | clock-injected like `Debouncer::record_at`/`drain_ready_at` (`debounce.rs` l.28-60) |
| `core/src/daemon/mod.rs` | `spawn_watch_consumer` | l.627-674 | owns the scheduler and the `.gitignore` watermark |
| `core/src/daemon/mod.rs` | `watch_and_route_once` | l.689-747 | matches on `Change`; after the batch, runs a due rescan. The `""` skip (l.711-715) stays for Path |
| `core/src/daemon/registry.rs` | `reconcile_since_index` | l.1374-1477 | add `subtrees: &[String]`; walk through `project_files_under`, filter `live` |
| `core/src/daemon/registry.rs` | new `rescan_subtrees` | next to it | reload ignores, recheck `.gitignore` since the watermark, reconcile |
| `core/src/daemon/registry.rs` | `recheck_gitignores_since_index` | l.1297-1331 | extract the mtime filter as `gitignores_newer_than(secs)` for reuse |
| `core/src/project_walk.rs` | `under_any` | l.85-93 | `pub(crate)` |

Callers to update for the `next_change` return type: `watch_and_route_once`
only, plus the tests in `watcher/mod.rs` (`changes_until`/`changes_within`
helpers, l.398-430) and `daemon/tests.rs`.

Read for context: `Debouncer` (`watcher/debounce.rs`),
`PluginRegistry::ensure_fresh_with` (registry.rs l.1880-1912),
`staleness::is_stale` / `Decision` (`watcher/staleness.rs` l.230-270),
`gitignore_changed` (registry.rs l.1238), `project_files_under` (project_walk.rs
l.72-82).

## 6. Behaviours the tests must pin

1. `forward_event` turns a `Flag::Rescan` event with a path into `Rescan(path)`,
   not `Path`.
2. `forward_event` turns a pathless Rescan (the inotify overflow shape) into
   `Rescan(root)`.
3. `next_change` returns a root Rescan as `Change::Rescan`, even though the root
   path alone is skipped as a Path.
4. **Core fix.** An edit whose event was swallowed, followed by an injected root
   Rescan: the edited file is reindexed. Its new symbol is found and the old one
   is gone.
5. A file deleted, and one created, with no event, then a Rescan: the deletion is
   retired and the creation is indexed.
6. A Rescan of a subtree reconciles only that subtree. A swallowed edit outside it
   stays stale until a root Rescan.
7. Scheduler: a burst of Rescans 100 ms apart fires once, after `RESCAN_SETTLE`
   of quiet.
8. Scheduler: Rescans that never stop still fire by `RESCAN_MAX_DELAY`, and not
   sooner than the gap factor after the previous run.
9. A `.gitignore` edit whose event was swallowed, then a Rescan: the newly ignored
   file leaves the index (through `gitignore_changed`).

Tests 1-3 and 7-8 are deterministic unit tests: injected clock, synthetic
`notify::Event`. Tests 4-6 and 9 drive `watch_and_route_once` with
`inject_rescan`. Being timer and thread tests, they run 5 times per the standing
rule. No test needs a real FSEvents storm: §1.3's probe is the evidence for the
OS side.

## 7. Proposed controls (revert code, not tests)

| # | revert | must fail |
|---|---|---|
| C1 | `forward_event` ignores the flag (sends `Path` for every event path) | 1, 4 |
| C2 | the pathless branch sends nothing (today's `for path in event.paths`) | 2 |
| C3 | `watch_and_route_once` treats `Change::Rescan` like a Path (the root is then skipped by the `""` check) | 4 |
| C4 | `reconcile_since_index` ignores `subtrees` (always the whole project) | 6 |
| C5 | the scheduler fires on every Rescan (no settle) | 7 |
| C6 | the scheduler has no max delay (fires only on quiet) | 8 |
| C7 | `rescan_subtrees` skips the `.gitignore` recheck | 9 |

C1/C2/C5/C6 live in the `g-mesh` lib's watcher unit tests and C3/C4/C7 in
`daemon/tests.rs`. They are all in the same lib test binary, so they can share
one build group.

## 8. Decision for the owner

- **The rate-limit numbers: settle 1 s, cap 30 s, gap factor 3.**
  - **Today:** a dropped edit stays stale until a daemon restart.
  - **With these numbers:** stale for at most about 30 s during a storm, and
    about 1 s after it ends. On a 50k-file root the watcher thread spends at most
    about a quarter of its time reconciling during a continuous storm.
  - Lower values heal sooner but cost more CPU under exactly the pressure that
    causes drops. Higher values do the reverse.
- **Out of scope here:** making the reconcile run off the watcher thread. Normal
  routing waits while a reconcile runs, the same as at startup today.
