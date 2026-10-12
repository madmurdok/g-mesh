# GM-524: g-mesh in agent git worktrees - daemon lifecycle, index per worktree

Design slice GM-524/S1 (release 4.7.0). Cold-index time and disk for a fresh
worktree are measured by GM-524/S2 and are not repeated here.

All line numbers are 1-based, against `release-4.7.0` (`1bdd86d`), which is
also what the installed `~/.local/share/g-mesh/bin/g-mesh` (4.6.0, installed
2026-10-12 19:46) was built from for every file cited here (`git diff
c7c66d6 1bdd86d -- core/src` is empty).

## Summary

| Question | Today (4.6.0) | Verdict |
|---|---|---|
| 1. Daemon stops when its worktree is removed | Yes, within one supervisor tick (16 s and 21 s measured). It never recreated the directory in two runs. **But** its index (`~/.g-mesh/projects/<hash>/index.db`, plus lock files) stays forever, and any `cargo`/`rustc` that rust-analyzer started survives a `SIGTERM`/`SIGKILL` of the daemon and keeps writing `<worktree>/target/` | Fix the leftovers: purge the state dir on orphan exit; kill the daemon's process group on exit |
| 2. Killing one project's daemon disconnects the MCP server | No longer: fixed by `ed059df` (2026-10-10, shipped in 4.6.0). Reproduced: kill -9 and SIGTERM both, next call respawned the daemon in 0.6 s / 2.1 s, session stayed up | Add one end-to-end test; polish two messages |
| 3. Index follows a worktree's branch changes | Yes, while its daemon runs (watcher on the worktree's files). Changes made while no daemon ran are caught only for file-anchored tools | Guidance can drop "serves the checkout it was built on" for a selected worktree; optional startup reconcile closes the offline gap |
| 4. Seed a worktree's index from the main checkout | Feasible: the index stores only root-relative paths. Needs a consistent copy plus a reconcile sweep | Proposal; owner decides after S2's cold numbers |

## Repro (real processes, isolated)

Driver: `scratchpad/gm524-s1/repro.py` (not committed). It spawned its own
`g-mesh mcp-shim` over stdio with `CLAUDE_PROJECT_DIR` set to a throwaway
worktree `../g-mesh-wt-gm524-repro` (detached at `release-4.7.0`) and an
isolated `G_MESH_HOME=~/.gm524r` (the scratchpad path is too long for an
AF_UNIX socket: the shim refused it with "socket path is 173 bytes ... at most
103", `shim.rs:221`). Only processes whose command line named the repro
worktree were signalled. Machine load: `uptime` load average 204 at the start
of run 1, 19-37 during run 2 (other agents building), so timings are
indicative only.

Run 1 (kill -9, then remove while rust-analyzer's `cargo check` was starting):

```
call#2 (cold)       41.3 s  answered
kill -9 daemon      plugins gone within 3 s (stdin EOF), nothing left matching the worktree
call#3 after kill    0.6 s  answered, first item: "the daemon serving ... restarted since this
                            session's previous answer ..."; shim alive; new daemon pid
git worktree remove --force   (cargo check running, target/ not yet created)
t+16 s daemon alive, t+21 s daemon gone, 0 processes left; worktree dir absent for 107 s
daemon.log: "the project root ... no longer exists - shutting down"
left behind: ~/.gm524r/projects/<hash>/{index.db,-wal,-shm,daemon.lock,bootstrap.lock,
             daemon.serving,daemon.log,project.root}
call#4 after removal 0.0 s  answered by the shim: "could not reach the daemon serving ...:
                            failed to canonicalize project root ...: No such file or directory";
                            session stays up; directory not recreated
```

Run 2 (SIGTERM, with `target/` being built, then remove while building):

```
target/ 6.5 MB before SIGTERM; daemon gone within 1 s
11 s after SIGTERM: target/ 38 MB and growing; still running, reparented to pid 1, pgid = the
  dead daemon's pid: cargo check --workspace --compile-time-deps --all-targets
  --manifest-path <wt>/Cargo.toml, its rustc children and target/debug/build/ring-*/build-script-build
call#3 2.1 s answered with the restart line; respawned daemon started a second cargo check
git worktree remove --force while both builds ran
t+10 s 7 processes still match the worktree, t+16 s daemon and all builds gone, directory absent
  for 106 s; the same state-dir leftovers as run 1
```

Cleanup: the repro worktree is removed and pruned, no process names it,
`~/.gm524r` is deleted, and no `~/.g-mesh/projects/*/project.root` names it.
Nothing was left behind.

Not reproduced: the directory being *recreated* after removal (the
2026-10-05 gm324 incident). Run 2 shows the mechanism that most likely caused
it: orphaned `cargo`/`rustc` processes started by rust-analyzer outlive the
daemon and write into `<worktree>/target/`. In run 2 they died on their own
within 16 s of the removal, probably because the manifest disappeared. A build
that is further along, or one that `wt-cleanup.sh` TERM-signals one process at
a time, can still be writing when `git worktree remove` finishes. "Survived
SIGTERM" fits the same picture: the daemon dies on SIGTERM (as
`core/tests/daemon_sigterm.rs` asserts), but the `cargo`/`rustc` processes
matched by path are a separate group of processes, and each one has to be
signalled.

Side observation: some `rustc` processes naming the worktree were children of
the shared `sccache` server (ppid 19384, pgid 19383), not of g-mesh.
`wt-cleanup.sh`'s match-by-path kill also hits those workers.

## Q1. Lifecycle: spawn, idle, exit, and what survives removal

**Spawn.** The `g-mesh mcp-shim` process (`shim::run`, `core/src/shim.rs:103-128`)
calls `connect_or_bootstrap` (`shim.rs:212-293`). On a miss it takes the
bootstrap lock and calls `spawn_detached_daemon` (`shim.rs:499-529`). That runs
`g-mesh daemon --project-root <root>` with null stdio and
`process::detach` (`core/src/process.rs:170-174`, `process_group(0)`), so the
daemon leads its own process group. Its plugins, the LSP servers they run as
ordinary children (`plugins/sdk/src/lsp/client.rs:39-56`, "no `setsid`") and
rust-analyzer's `cargo` all inherit that pgid. Run 2 confirms it: the orphaned
cargo had pgid = the daemon's pid.

**Idle / exit.** `daemon::run` (`core/src/daemon/mod.rs:288-522`) ends in
`lifecycle::supervise` (`core/src/daemon/lifecycle.rs:846-901`). Each tick
(≤ 30 s, `MAX_TICK` line 70) it runs:
- `orphan_check` (`lifecycle.rs:784-801`): `ProjectRootGone` when
  `fs::metadata(root)` is `NotFound` (`is_definitely_gone`, `lifecycle.rs:818-823`),
  `ExecutableGone`, or `LifelineGone` (test-only `G_MESH_LIFELINE_PID`).
- plugin idle sleep (1 h), the memory check, then the core idle exit
  (`CoreActivity::idle_beyond`, `lifecycle.rs:738-749`): 24 h *and* no
  connected client. A shim that stays connected keeps the core alive
  indefinitely, so only the orphan check can end a worktree daemon while a
  session is open.

All three exits call `registry.sleep_all_now` and `release_state_files`
(`lifecycle.rs:904-922`). That function removes the pid, build, phase and
progress files and the socket. It does **not** remove `index.db*`,
`daemon.lock`, `bootstrap.lock`, `daemon.serving`, `daemon.log`, `config.toml`
or `project.root`. Today those go only through `g-mesh clean orphaned --force`
(`core/src/cli/clean.rs:285-316`, which classifies with `root_state`,
`clean.rs:268-279`) or a cwd-scoped `g-mesh clean` run *before* removal (what
`wt-cleanup.sh` does). On this machine `~/.g-mesh/projects` holds 3,190 state
directories.

**Why it "survived" and "recreated files".** The daemon itself writes nothing
under the project root: all its state is under `~/.g-mesh/projects/<hash>`
(`storage/connection.rs:27-71`). The shim cannot recreate the root either:
`project_hash` canonicalizes the root first (`daemon/identity.rs:10-18`), and
call#4 above failed on that. What writes under the root is rust-analyzer's
build of compile-time deps (build scripts and proc macros), which
`plugins/rust/plugin.toml:132-160` deliberately leaves on (`checkOnSave =
false` only). Its `cargo` is a grandchild of the daemon. A plugin closing its
stdin ends rust-analyzer gracefully (run 1: nothing left), but a daemon killed
by SIGTERM/SIGKILL leaves `cargo` running (run 2). The SDK's
`kill_live_servers` (`plugins/sdk/src/lsp/client.rs:119`) sends SIGKILL to
rust-analyzer only, not to its children.

**Design.**
1. *Purge on orphan exit* (decision D1): on `ProjectRootGone` the daemon
   deletes its own state directory after the plugins are asleep and the
   singleton lock is released. It does this only when the root's parent still
   exists, the same rule `clean::root_state` uses to tell `Orphaned` from
   `Unreachable` (an unmounted volume). Example: `git worktree remove
   ../g-mesh-wt-gm600` → within 30 s the daemon logs the root gone, stops its
   plugins and removes `~/.g-mesh/projects/<hash>/` (60-140 MB for g-mesh).
   Consequence: an index is never orphaned by a removed worktree. A worktree
   re-created at the same path cold-indexes again, as a new worktree does today
   (different hash anyway once its path differs).
2. *Kill the process group on exit* (decision D2): every exit path the daemon
   controls (orphan, idle, accept-loop failure) ends with `killpg(own pgid,
   SIGTERM)`, a short grace, then `SIGKILL` to the group, skipping itself
   until last. `g-mesh stop` (`cli::stop`) and the shim's
   `retire_outdated_daemon`/`evict_wedged_daemon` signal `-pgid` instead of
   the pid. Example: a SIGTERM'd or orphaned daemon in run 2 would have taken
   its `cargo check` and `rustc` children with it, so `target/` stops growing
   at once. Consequence: nothing g-mesh started can write into the worktree
   after its daemon is gone. A `kill -9 <daemon pid>` from outside still cannot
   run this code, so the guidance and `wt-cleanup.sh` should signal the group:
   `kill -- -<pgid>`.
3. *Never recreate the root*: already true of g-mesh's own code. Pinned by a
   test (below) whose writer is a grandchild process, which is the real
   offender.
4. Detection latency stays one tick (≤ 30 s). A watcher-driven early exit is
   possible (FSEvents reports the root's removal) but not needed: both runs
   exited in ≤ 21 s.

## Q2. MCP front: one project's daemon dying

**Today.** The router (`core/src/shim/router.rs`, module doc lines 47-66)
marks a lost upstream `Slot::Lost`, keeping its root
(`Shared::upstream_ended`, `router.rs:808-865`). The next request routed there
goes through `reach` (`router.rs:579-598`) → `reconnect`
(`router.rs:626-644`) → the `Daemons.connect` closure built in `shim::run`
(`shim.rs:121-124`) → `connect_or_bootstrap`, which spawns a daemon if none is
listening, replays `initialize`, and stamps the first answer with `restarted`
(`router.rs:876-890`). A failed reconnect is answered by the shim
(`unreachable_text`, `router.rs:892-903`) and the session stays up. Unit tests
`a_lost_daemon_is_reconnected_on_the_next_call` (`router.rs:1601`) and
`a_lost_sub_project_reconnects_to_the_selection` (`router.rs:1637`) pin this
with fake daemons.

**Why the server disconnected on 2026-10-05/06.** It ran a build from before
`ed059df` ("fix: keep MCP sessions across a daemon restart", 2026-10-10, in
4.6.0). Before that commit, `serve` "returns when the current upstream's
connection ends" (the removed doc line), so killing the selected sub-project's
daemon ended the shim process, and Claude Code reported the server as
disconnected. This is established from the diff, not reproduced on the old
binary. On 4.6.0 it is reproduced as fixed (runs 1 and 2).

**Remaining design.**
- An end-to-end test with real processes (shim + daemon, kill -9 the daemon,
  next call answers with the restart line), because today only fakes cover it.
- `restarted` names its causes as "g-mesh reindex, init or stop, or a newer
  build". Add "or the daemon exited (its project was removed, it was idle, or
  it was killed)".
- `unreachable_text` for a root that no longer exists currently surfaces
  `failed to canonicalize project root ...: No such file or directory`. Make
  it "the project directory <root> no longer exists; call select_project to
  choose another". Example: an agent whose worktree was cleaned up calls
  `find_callers` → told to re-select instead of retrying forever.

## Q3. Watcher: does the index follow the worktree's branch?

`daemon::run` starts `ProjectWatcher::new(root)` (`daemon/mod.rs:479-489`, or
from activation for a cold project) on the worktree's own root.
`spawn_watch_consumer` (`mod.rs:543-571`) → `watch_and_route_once`
(`mod.rs:574-627`) routes every settled path under it, made root-relative by
`relative_wire_path`. In a worktree `.git` is a *file* pointing into the main
repo's `.git/worktrees/<name>/`, and `BASELINE_EXCLUDED_DIRS` (`.git`,
`.claude`; `watcher/mod.rs:33`) excludes it by name. So:
- `git checkout`/`switch`/`rebase`/`pull` in the worktree change files on
  disk, and those are routed like any edit (a `Cargo.toml` change reindexes
  that language via `[plugin.workspace] watch_files`).
- A commit changes no working files and needs nothing.
- The index therefore serves *the worktree's current files*, whatever branch
  they came from. Selecting the worktree is enough.

Gap: changes made while no daemon ran. Nothing re-walks an already-indexed
project on start (`watcher/staleness.rs:1-25`). `ensure_fresh` repairs only
the three file-anchored tools; `find_callers`/`find_references`/
`find_implementations`/`search_code` can miss them. The only startup sweep is
`.gitignore` (`registry.rs:1270-1297`, `recheck_gitignores_since_index`).
Optional fix (decision D4, shared with seeding): a startup reconcile that
stats every walked file against `indexed_files` (mtime first, hash on a
mismatch) and routes the differences through `route_settled_path`, as the
watcher would. It is cheap: g-mesh has ~800 files.

## Q4. Seeding a worktree's index from the main checkout

**Feasible.** The index stores only root-relative paths: in the main
checkout's `index.db` (110 MB + 25 MB WAL), `.dump` shows 0 rows with
`/Users/` in `edges`, `indexed_files`, `resolution_facts`, `containers`,
`declarations` and `meta`, and 1 in `nodes`, which is doc-comment text. The
project's identity lives outside the db (`project.root`). The generation check
(`schema::ensure_current`, `daemon/mod.rs:337`) passes when both use one
binary.

**Proposal.**
1. At the first bootstrap of a root that is a linked worktree (`git rev-parse
   --git-common-dir` ≠ `--git-dir`) whose main checkout has a completed index
   (`bulkIndexedAt` and `semanticPassAt` set), copy that index with SQLite's
   online backup or `VACUUM INTO` into the new state dir. A plain file copy is
   not safe while the source daemon writes to its WAL.
2. Run the startup reconcile from Q3 before serving. Every file's mtime
   differs in a fresh checkout, so every file is hashed (reads ~800 files,
   well under a second). Only files whose content differs from what the main
   checkout had indexed are reindexed, and rows for files absent in the
   worktree are deleted.
3. Embeddings (`vectors`) and rust-analyzer edges come with the copy. Files
   the reconcile reindexes go through the normal incremental path.

**Cost and risk.** The copy is seconds against a cold walk, plus the semantic
pass and embedding backfill, which S2 measures. The win is largest for the
semantic tier: no whole-project rust-analyzer pass means no `cargo check` build
into the new worktree's `target/` at all. Risks:
- The main checkout's index reflects *its working tree* (uncommitted edits,
  untracked files), not a commit. The reconcile is what makes that safe.
- Gitignored inputs differ. A fresh worktree has no
  `plugins/typescript/node_modules`, so TS edges that the main index resolved
  into it would point at files that do not exist in the worktree until
  `npm ci`. The reconcile sees only tracked/walked files, not resolution
  targets.
- Semantic edges of files the reconcile reindexes depend on how the incremental
  path refreshes them (`semantic_owed_files`): must-confirm.
- More code on the bootstrap path, which today is a single spawn.

## Decisions for the owner

**D1. What happens to a removed worktree's index.**
- Today: the daemon exits but leaves `index.db` and lock files in
  `~/.g-mesh/projects/<hash>/` until someone runs `g-mesh clean orphaned
  --force`.
- (a) **The daemon deletes its own state dir on `ProjectRootGone` when the
  root's parent exists (Recommended).** Benefit: no step to forget, and
  nothing is left behind whatever removed the worktree. Risk: a directory moved
  away and moved back within 30 s loses its index, which costs one cold index.
- (b) Keep the index and rely on `wt-cleanup.sh`/`clean orphaned`. Benefit: no
  code change. Risk: a worktree removed any other way (`git worktree remove`,
  `rm -rf`, the Agent tool's own cleanup) leaks 60-140 MB each, which is how
  3,190 dirs accumulated.
- (c) (a) plus a sweep of orphaned state dirs at shim start. Benefit: cleans
  up after daemons killed with -9 that never ran (a). Risk: shim start does a
  directory scan of thousands of entries. Better done as a later GC task.

**D2. Processes the daemon started that outlive it.**
- Today: SIGTERM/SIGKILL of the daemon leaves rust-analyzer's `cargo`/`rustc`
  writing into the worktree for as long as the build runs.
- (a) **The daemon kills its own process group on every exit it controls, and
  `g-mesh stop`/the shim's retire paths signal the group (Recommended).**
  Benefit: one mechanism that covers every descendant, since all of them
  share the daemon's pgid by construction. Risk: a plugin that ever calls
  `setsid` escapes, and an outside `kill -9 <pid>` still orphans the group,
  so guidance says `kill -- -<pgid>`.
- (b) Have the SDK kill each LSP server's whole tree. Benefit: local to the
  bridge. Risk: it misses any other grandchild, and it does not run when the
  plugin itself is SIGKILLed.

**D3. Where rust-analyzer builds (coordinate with GM-513).**
- Today: it builds compile-time deps into `<worktree>/target/`, the agent's
  own target dir. That competes with the agent's `cargo build` for the build
  lock and adds to the worktree's disk use.
- (a) **Leave it to GM-513 (Recommended).** If GM-513 turns off build scripts
  and proc macros, rust-analyzer builds nothing, and this question goes away.
  Benefit: one owner for `plugins/rust/plugin.toml`. Risk: if GM-513 keeps them
  on after its quality A/B, the problem stays.
- (b) Point `cargo.targetDir` at an absolute path under the project's state
  dir. Benefit: the worktree is never written by g-mesh, there is no lock
  contention, and D1 deletes it. Risk: plugin.toml is static, so the plugin
  must learn its state dir at runtime (must-confirm). It is also a second full
  compile-time-deps build per worktree on disk.

**D4. Seeding and the startup reconcile.**
- (a) **Build the startup reconcile now and decide seeding after S2's numbers
  (Recommended).** Benefit: the reconcile fixes offline-change staleness on
  its own and is the prerequisite for seeding anyway. Risk: one more startup
  pass, ~1 s on g-mesh, more on huge repos.
- (b) Reconcile plus seeding in this task. Benefit: fast worktree starts and
  no rust-analyzer build in new worktrees. Risk: the largest change, with the
  gitignored-input caveat above. Worth it only if S2's cold numbers are minutes.
- (c) Neither. Benefit: no change. Risk: offline changes stay invisible to
  symbol-anchored tools until a file-anchored query or an edit touches them.

## Edit map

To change:

| Function | File:lines | Change |
|---|---|---|
| `supervise` | `core/src/daemon/lifecycle.rs:846-901` | return an exit reason (`Orphaned::ProjectRootGone` vs others) so the caller can purge after releasing the lock (D1); call the group kill on every return path (D2) |
| `release_state_files` | `lifecycle.rs:904-922` | unchanged contract; new sibling `purge_state_dir(state_dir, root)` with the parent-exists rule |
| `orphan_check` / `Orphaned` | `lifecycle.rs:750-801` | no logic change; the reason is carried out of `supervise` |
| `daemon::run` | `core/src/daemon/mod.rs:288-522` (tail at 511-521) | after `drop(singleton)`: on `ProjectRootGone`, drop `conn` and purge; then group kill |
| `front::run` | `core/src/daemon/front.rs:57-108` | same supervise outcome handling (a removed folder) |
| new `process::kill_group` | `core/src/process.rs` (unix `imp`, near 146-174; Windows: job object or no-op) | TERM group, grace, KILL group, excluding self |
| `cli::stop` terminate path | `core/src/cli/stop.rs` (`run` at 115) | signal `-pgid` |
| `retire_outdated_daemon`, `evict_wedged_daemon` | `core/src/shim.rs:334-354`, `402-425` | signal `-pgid` |
| `restarted` | `core/src/shim/router.rs:876-890` | add "or the daemon exited" |
| `unreachable_text` | `router.rs:892-903` | a root that is `NotFound` gets the "no longer exists, select another" text |
| (D4) new `reconcile_since_index` | `core/src/daemon/registry.rs` next to `recheck_gitignores_since_index` (1270-1297), called from `spawn_watch_consumer` (`mod.rs:543-571`) before its loop | stat/hash every walked file and route the differences |
| (D4b) seeding | `daemon::run` before `connection::open` (`mod.rs:331`) | copy via backup API when the state dir is new and the root is a linked worktree |
| (D3b) | `plugins/rust/plugin.toml:159-160`, `plugins/rust/src/semantic.rs` | `cargo.targetDir` |

To read for context: `core/src/storage/connection.rs:27-71` (state dir,
`ensure_project_dir`), `core/src/daemon/identity.rs:10-18` (`project_hash`
canonicalizes), `core/src/cli/clean.rs:268-316` and `401-416`
(`root_state`, `clean_orphaned`, `daemon_is_running`),
`core/src/shim.rs:212-293` and `499-553` (bootstrap, spawn, reaper),
`core/src/shim/router.rs:579-644` and `808-865` (reach, reconnect,
upstream_ended), `plugins/sdk/src/lsp/client.rs:39-56` and `119`
(server as child, `kill_live_servers`), `core/tests/orphaned_daemon.rs`
(fixture that captures state paths before deleting the root),
`core/tests/daemon_sigterm.rs`.

## Behaviours the tests must pin

Each has a control: revert the code (never the test) and the test must fail.

1. A daemon whose root is deleted (parent exists) removes its state dir after it exits (`daemon::run` purge). Test in `core/tests/orphaned_daemon.rs`, extending `assert_released_everything`. Control: skip the purge call → `index.db` still exists.
2. A daemon whose root *and* parent are gone keeps its state dir (purge's parent rule). Control: drop the parent check → dir deleted.
3. On orphan exit, a grandchild in the daemon's process group that keeps writing `<root>/target/x` is killed, and the root stays absent for 3 ticks (`process::kill_group` from `supervise`'s exit). Fixture: a test plugin (`daemon/test_plugin.rs`) that spawns `sh -c 'while :; do mkdir -p $ROOT/target; touch $ROOT/target/x; sleep 0.1; done'`. Control: remove the group kill → the root reappears.
4. `g-mesh stop` ends the same grandchild (`cli::stop` group signal). Control: signal the pid only → grandchild alive.
5. With real processes, kill -9 of the served daemon leaves the shim session up and the next `tools/call` answers with the restart line and a new daemon pid (`router::reconnect`, already in 4.6.0). New `core/tests/` test. Control: in `upstream_ended`, send `Event::Done` when the live upstream ends (the pre-`ed059df` behaviour) → the shim exits.
6. A call to a selection whose directory was deleted is answered with the "no longer exists" text, the session stays up, and the directory is not recreated (`unreachable_text`). Control: restore the old text → assertion on the text fails.
7. (D4) A file edited while no daemon ran is found by `find_references` after restart (`reconcile_since_index`). Control: skip the reconcile call → the old reference is returned.
8. (D4b) A seeded worktree index answers `find_definition` on an unchanged file like a cold index, and on a file changed only in the worktree with the worktree's content (seed + reconcile). Control: seed without the reconcile → the main checkout's version is returned.

The 5x-repeat rule applies to 1-6 (processes and timers). Use
`G_MESH_CORE_IDLE_MS`/`G_MESH_PLUGIN_IDLE_MS` to shorten the tick, as
`orphaned_daemon.rs` already does.

## Must-confirm for the code slice

- Purge ordering: `index.db` must be closed (drop the `IndexStore` Arc and
  all its clones, including the activation thread's) before `remove_dir_all`.
  On macOS an open file can be unlinked anyway, but on Windows it cannot.
  Decide whether Windows skips the purge.
- A new bootstrap racing the purge for the same path is impossible while the
  root is absent (`project_hash` fails). If the root is recreated within the
  tick, the purge must re-check that the root is still absent after taking
  `bootstrap.lock`.
- `killpg` excluding self: send TERM to the group (the daemon has no handler,
  so TERM would kill it first). Either block SIGTERM in the daemon for this
  step, or SIGKILL the listed group members found from the plugin pid files
  and a `ps`/`sysinfo` pgid scan. Pick one and test it on macOS and Linux.
- Windows has no process groups in this sense. `process::detach` uses
  `CREATE_NEW_PROCESS_GROUP`, so a job object would be the equivalent. Scope it
  or document it as Unix-only.
- Whether the front daemon (folder mode) should purge too, when the whole
  folder is removed.
- (D3b) whether the rust plugin knows its state dir at runtime.
- (D4) how `semantic_owed_files` refreshes rust-analyzer/pyright edges for the
  files a reconcile reindexes, and whether the reconcile must defer to an
  owed semantic pass the way the watcher consumer does (`mod.rs:482-488`).
- (D4) the reconcile honours the same excludes as the walk (`[plugin.workspace]
  exclude_dirs`, `.gitignore`), so `target/` and `node_modules/` are never
  hashed.

## Proposed CLAUDE.md "Code search" guidance (text only)

Replace the two worktree bullets with:

> - Working in a `git worktree` (`../<repo>-wt-…`), `select_project` that
>   worktree, not the main checkout. Its index follows that worktree's files,
>   including checkouts and rebases made in it while its daemon runs, so the
>   answers match the code you are editing. The main checkout's index answers
>   only for code the branch has not changed. A worktree's first call builds
>   its index (cold: see GM-524/S2's numbers); later calls are incremental.
> - Removing a worktree stops its g-mesh daemon within ~30 s and deletes its
>   index (from 4.7.0). Nothing to stop by hand. If something still writes into
>   the removed path, stop the daemon's process group (`kill -- -<pgid>`, pgid
>   from `ps -o pgid= -p <daemon pid>`), not processes matched by path, which
>   also hits the shared `sccache` server's workers.
> - If the g-mesh server's daemon for a project dies, the next call respawns it
>   (the answer says "restarted"); the MCP server stays connected (from 4.6.0).

(Wording about deleting the index applies only if D1 (a) or (c) is chosen.
The cold-index sentence takes S2's numbers.)

## g-mesh calls behind the cross-file answers

The session's g-mesh connection is shared, and other agents re-selected
`g-mesh` between my calls, so several answers say "answered from project
g-mesh". The main checkout and this worktree have identical `core/src`, so the
answers hold.

- Who spawns the daemon: `find_callers spawn_detached_daemon` → only
  `shim::connect_or_bootstrap`. `find_callers connect_or_bootstrap` → only
  `shim::run`, through the `Daemons.connect` closure that `reconnect` calls.
- Who runs shutdown/idle: `find_callers supervise` → `daemon::run`,
  `daemon::front::run`. `find_callers orphan_check` → `supervise`.
  `find_callers release_state_files` → `supervise`.
- What references the watcher and its root: `find_references ProjectWatcher`
  → `daemon::spawn_watch_consumer`, `daemon::watch_and_route_once`,
  `daemon::activation::ActivationCtx` (+ `watcher/mod.rs` itself).
- Who handles a daemon connection ending in the MCP front: `find_callers
  Shared::reconnect` → `Shared::reach`, `Shared::reach_for_tools_list`.
  `find_callers Shared::upstream_ended` returned no edge, only
  `untypedReceiverCalls` (1 in `router.rs`, no semantic tier). By reading,
  it is called from `spawn_reader`'s thread when a reader hits EOF.
- `search_code` for an existing startup reconcile returned nothing (embedding
  pass not started), so it was found by grep (`recheck_gitignores_since_index`).

## Owner decisions (2026-10-12)

- D1: "То же + чистка при старте" — (c): the daemon purges its own state dir on `ProjectRootGone` (parent exists), plus a sweep of orphaned state dirs at shim start in this task. The sweep must stay cheap on a store of thousands of dirs (off the request path, rate-limited).
- D2: "Убивать группу процессов (Recommended)" — the daemon kills its process group on every exit it controls; `g-mesh stop` and the shim's retire paths signal the group.
- D3: "Отдать GM-513 (Recommended)" — no `cargo.targetDir` change here; GM-513 owns rust-analyzer build scripts / proc macros.
- D4: "Сверка сейчас, сидинг отдельно (Recommended)" — startup reconcile in this task; seeding is a separate backlog task. S2 cold index: 283-495 s, 114.8 MB (scratch summary; run 1 under load 350-470).
