# GM-564: seed a new worktree's index from the main checkout

Status: design (S1). Code base: `main` at `dfc9cac` (4.9.0). Parent:
[GM-524](GM-524-worktree-daemon-lifecycle.md) Q4 and owner decision D4
("Сверка сейчас, сидинг отдельно"): the startup reconcile shipped with GM-524;
seeding is this task.

## Summary

A fresh linked worktree of g-mesh cold-indexes in 283-495 s and 114.8 MB
(GM-524/S2; the Python semantic pass alone ~111 s). Its index is almost the
same as the main checkout's, which stores only root-relative paths (GM-524 Q4).

Proposal: when `daemon::run` starts on a root whose state dir has no
`index.db` yet, and the root lies in a linked git worktree whose main working
tree has a complete index of the same generation, copy that index with
SQLite's online backup API into a temp file in the new state dir, record
where it came from, and rename it into place before `connection::open`. The
first start then runs a *seeded* variant of GM-524's reconcile (deletions by
"indexed but not walked", no `.gitignore` re-check, tool calls held until it
has reparsed, semantic refresh of the reparsed files deferred to right after).
Everything after that start is the ordinary walked-index path.

Expected effect (to be measured, see *Measure protocol*): time-to-first-answer
from minutes to the copy (~1 s for 110 MB) plus hashing every walked file
(~800 files) plus the structural reparse of the files that differ; no
whole-project rust-analyzer/pyright pass and no embedding backfill in the new
worktree.

## What exists today (read for this note)

- `daemon::run` (`core/src/daemon/mod.rs:294-537`): state dir, singleton
  lock, folder-mode check (`candidates::detect_in`, 323-326), plugin discovery
  (330-331), `remove_stale_staging` (336), **`connection::open` (337)**,
  `stored_generation`/`ensure_current` (340-344), `bulk_index_completed`
  (360-361), semantic retry (364-366), initial `IndexingStatus` (411-415:
  `unindexed_because` or `structural`), watcher + `spawn_watch_consumer`
  (485-504).
- `connection::open` (`core/src/storage/connection.rs:119-132`): creates
  `index.db` if absent, WAL, foreign keys off. Production callers (g-mesh
  `find_callers storage::connection::open`): `daemon::run`, `cli::init::init`,
  `cli::reindex::reindex`; the rest are tests.
- `spawn_watch_consumer` (`mod.rs:561-583`): on its thread,
  `recheck_gitignores_since_index` then (if `reconcile`)
  `reconcile_since_index`, then the watcher loop. Callers (g-mesh
  `find_callers spawn_watch_consumer`): `daemon::run` and
  `activation::ActivationCtx::activate` (the latter when a semantic retry or a
  failed-language retry is owed).
- `recheck_gitignores_since_index` (`core/src/daemon/registry.rs:1281-1307`):
  every `.gitignore` with mtime > `bulkIndexedAt` goes through
  `gitignore_changed` (1223-1270), which **reindexes a whole language** when
  the walked file set under those directories differs from the indexed one.
- `reconcile_since_index` (`registry.rs:1339-1394`): walked files vs live
  files; deleted = live and *absent on disk*; created and known go through
  `ensure_fresh` (1646-1665), whose `staleness::is_stale` stat fast path is an
  exact mtime match, else a hash (`core/src/watcher/staleness.rs:235-280`).
  A reindexed file goes through `apply_file_change_in`
  (`core/src/watcher/apply.rs:222-345`), which runs the language's per-file
  semantic pass inline (282-330) when the plugin is semantic-capable.
- `apply_scoped_semantic_pass` (`apply.rs:163-221`): a semantic pass over an
  explicit file list; caller (g-mesh `find_callers`): `PluginProcess::scoped_semantic_pass`
  (`core/src/daemon/plugin.rs:1512`).
- Schema: `meta` is one row with fixed columns (`core/src/storage/schema.rs:524-532`);
  new tables are added with `CREATE TABLE IF NOT EXISTS` and no version bump
  (see `semantic_pending`, 600-611); `wipe` (1940-1964) drops a fixed list and
  is called only by `reset` (g-mesh `find_references wipe`).
- `IndexingStatus` (`core/src/daemon/indexing_status.rs:267-292`): tools wait
  on `Need::Structural` (`core/src/mcp/mod.rs:469-480`) while the phase is
  `Unindexed`/`Walking`.
- Plugins keep no per-project cache of their own (grep of `plugins/*/src` for
  state/cache dirs: only `plugins/sdk/src/lsp/config.rs` passing
  `CARGO_TARGET_DIR` etc. through). Language-server build output lives in the
  worktree (`target/`), not in the state dir, so nothing beside `index.db`
  needs seeding.
- The g-mesh rusqlite dependency (`core/Cargo.toml:112`) has features
  `bundled, hooks`; the backup API needs `backup`.

## Decisions for the owner

Each is self-contained: today, the change, an example, the consequence.

### D1. How the daemon finds the main checkout

**Today.** Nothing in g-mesh knows about git worktrees beyond excluding
`.git` by name. **Change.** A new `daemon::seed::main_checkout(root)`.
**Example.** Root `/P/g-mesh-wt-gm564`: its `.git` is a file
`gitdir: /P/g-mesh/.git/worktrees/g-mesh-wt-gm564`; that dir's `commondir`
file says `../..`, so the common dir is `/P/g-mesh/.git`, whose parent
`/P/g-mesh` is the main working tree.

- (a) **Read the files: walk up from the root to the first `.git`; it must be
  a file whose `gitdir:` points at a directory containing `commondir`; the
  common dir must be named `.git` and be the `.git` *directory* of its parent;
  the project root's path below its worktree top is applied to the main
  working tree (Recommended).** Benefit: no subprocess, no dependency on a
  `git` binary or its env (`GIT_DIR`), unit-testable with plain files; the
  rules reject a submodule (`.git` file, no `commondir`), a bare repo (common
  dir not `.git` under a working tree) and the main checkout itself (`.git`
  is a directory). A worktree of a worktree needs nothing: git links every
  linked worktree to the one common dir. Risk: re-implements a small piece of
  git's layout; a future git layout change (or `extensions.worktreeConfig`
  edge) is not followed, which only loses seeding, never correctness.
- (b) `git rev-parse --git-common-dir --git-dir --show-toplevel` in the
  root. Benefit: git's own answer. Risk: a process spawn on the daemon start
  path (~5-20 ms), fails where `git` is absent or `GIT_*` env leaks in from
  the caller, and still needs (a)'s bare-repo and subdirectory rules on top.

Bare repos (`git clone --bare` + worktrees): no main working tree, so no
seed (D2 says why not a sibling).

### D2. Which index may be a seed source

**Today.** n/a. **Change.** Source = the main working tree's state dir
(`project_dir(main_root)`, same `G_MESH_HOME`). Seed only if all hold:
the worktree's state dir has no `index.db`; the source `index.db` exists;
opened read-only, its `stored_generation` equals
`(CURRENT_SCHEMA_VERSION, indexer_version(&discovered))`; its `bulkIndexedAt`
is set. **Example.** Main checkout mid-cold-walk (no `bulkIndexedAt`): the
worktree cold-walks as today. Main indexed by an older g-mesh: generation
differs, no copy (it would be wiped by `ensure_current` anyway).

- (a) **Main working tree only; semantic state travels with the copy (an
  owed semantic pass or residual is retried in the worktree as it would be in
  main) (Recommended).** Benefit: one well-defined source; checking the
  generation before copying never wastes a 110 MB copy. Risk: a main
  checkout never opened with g-mesh gives no seed; a bare-repo user gets none.
- (b) Also accept the most recently used sibling worktree. Benefit: bare
  repos and never-indexed main checkouts benefit. Risk: picks an index of an
  unrelated branch, larger reconcile; more discovery code (`git worktree list`
  or scanning `worktrees/`).
- (c) Require `semanticPassAt` too. Benefit: never inherits an owed pass.
  Risk: no seed while main's semantic pass is still running or residual.

Opt-out: environment `G_MESH_SEED=0` disables seeding (tests and the measure
slice's cold arm need it; also a user escape hatch). No config key in this
task.

### D3. How the copy is made while the main daemon writes

**Today.** The main daemon holds `index.db` in WAL mode; committed pages can
sit in `index.db-wal` for a long time. A plain copy of `index.db` misses them,
and copying `index.db` + `-wal` without SQLite's locks can tear.
**Change.** Copy into `<state>/index.db.seed-tmp`, record the origin (D6),
`fsync`, rename to `index.db`. A stale `*.seed-tmp` is deleted at start, next
to `remove_stale_staging`. Stale `index.db-wal`/`-shm` without an `index.db`
are deleted before the rename.
**Example.** Main daemon is in its embedding backfill, committing every few
seconds; the worktree daemon starts and copies a snapshot as of one read
transaction.

- (a) **SQLite online backup, one step of all pages (`Backup::step(-1)`),
  source opened `SQLITE_OPEN_READ_ONLY`; enable rusqlite's `backup` feature
  (Recommended).** Benefit: page copy, fastest, one read transaction gives a
  consistent snapshot including WAL-only commits; WAL readers do not block
  the main daemon's writers. Risk: the main daemon cannot checkpoint past the
  snapshot during the copy (WAL grows for ~1 s); with incremental steps the
  backup would restart on every foreign write, hence one step; copies free
  pages too.
- (b) `VACUUM INTO '<tmp>'` from a read-only connection. Benefit: no new
  feature, compacted output. Risk: rebuilds every b-tree, several times
  slower on 110 MB; the measure slice could compare if (a) disappoints.
- (c) File copy of `index.db` (+ `-wal`). Rejected: not consistent.

The rename makes the step atomic: a daemon killed mid-copy leaves only a
`seed-tmp`, never a half `index.db`, and the next start seeds again.

### D4. The first start of a seeded index: deletions, `.gitignore`, gating

**Today.** On a walked index, the consumer thread runs
`recheck_gitignores_since_index` and `reconcile_since_index`, while tools
already answer (phase `Structural`). For a seeded index three things go
wrong: (1) every `.gitignore` in a fresh checkout is newer than
`bulkIndexedAt`, so `gitignore_changed` compares the root subtree, and any
file indexed in main but not walked in the worktree (an untracked scratch
file, a different ignore rule) **reindexes that whole language** - a partial
cold walk; (2) a file indexed in main, present in the worktree but ignored
there, is neither "absent on disk" nor walked, so it stays; (3) the first
tool call races the reconcile and answers with main's content.
**Change.** A seeded start (a `seed_origin` row with `reconciledAt` NULL, D6)
skips the `.gitignore` re-check and runs `reconcile_since_index` with
deleted = live minus walked; it starts with phase `Walking` (cause
`Seeded`), so tools wait on `Need::Structural`, and the consumer sets
`Structural` once the structural reparse is done, then stamps `reconciledAt`.
**Example.** Main has untracked `scratch/try.py` indexed; the worktree does
not. Today's path: `+0, -1` under the root `.gitignore` -> whole Python
reindex (~111 s semantic). Seeded path: `scratch/try.py` routed as a deletion.

- (a) **Seeded reconcile as above (Recommended).** Benefit: no whole-language
  reindex, ignore differences handled per file, first answer reflects the
  worktree. Risk: one more mode in the reconcile; tool calls wait for the
  hash pass (~1 s on g-mesh, longer on huge repos); if the daemon dies before
  `reconciledAt`, the next start repeats the seeded reconcile (intended).
- (b) Plain reconcile, no gating. Benefit: no new code beyond the copy.
  Risk: (1)-(3) above; (1) can cost minutes and erase the gain.

### D5. Semantic edges of the files the seeded reconcile reparses

**Today.** `apply_file_change_in` runs the per-file semantic pass inline for
every reparsed file. In a fresh worktree the language server is not running,
so the first changed `.rs` file starts rust-analyzer, which loads the
workspace and builds into the worktree's `target/` (tens of seconds to
minutes); a `.py` file starts pyright.
**Change.** In the seeded reconcile, reparse structurally only (the existing
`semantic_pass_capable = false` path), mark the reparsed files
semantic-pending (`semantic_pending_files`, so answers carry `staleFiles`
like any fresh edit), publish `Structural`, then run one
`scoped_semantic_pass` per language over its reparsed files before the
watcher loop.
**Example.** Main has an uncommitted edit to `core/src/daemon/mod.rs`; the
worktree has the committed version. First answer after ~1-2 s from the
structural reparse; rust-analyzer's refresh of that one file's edges follows
in the background.

- (a) **Defer the semantic part to a scoped pass after `Structural`
  (Recommended).** Benefit: the first answer never waits on a language
  server; unchanged files keep main's semantic edges. Risk: a window where
  the reparsed files' call edges are syntactic only (flagged by
  `staleFiles`/`provenance`); new code path: a structural-only route through
  `ensure_fresh`, and the pending-files bookkeeping must be cleared by the
  scoped pass (`settle_owed_files`/`clear_semantic_pending_files`, apply.rs
  690-725) - must-confirm.
- (b) Keep the inline per-file semantic pass and gate on the whole
  reconcile. Benefit: no new path. Risk: any changed `.rs`/`.py` file makes
  the first answer wait for a language-server cold start, which is most of
  the cost seeding removes.
- (c) Inline semantic, no gating (D4b). Benefit: least code. Risk: as D4b.

Note: cross-file edges *into* a reparsed file from unchanged files are the
same lazy edges any edit leaves (`graph::imports::link_diff`); seeding adds
no new staleness class there.

### D6. What the seeded index records about its origin

**Today.** Nothing; `meta` is a fixed one-row table, a new column there
would be a schema bump (`schema.rs` version notes).
**Change.** New table, `CREATE TABLE IF NOT EXISTS seed_origin (id INTEGER
PRIMARY KEY CHECK (id = 1), sourceRoot TEXT NOT NULL, sourceBulkIndexedAt
TEXT NOT NULL, seededAt TEXT NOT NULL, reconciledAt TEXT)`, written into the
temp copy before the rename; added to `wipe`'s list so a reset index is no
longer "seeded". Logged at start; `g-mesh status` shows "seeded from
<root> at <time>" (one line, optional in this task).
**Example.** `g-mesh status` in a worktree: `index: seeded from
/P/g-mesh at 2026-10-14T10:02Z, reconciled 2026-10-14T10:02Z`.

- (a) **Table in the index (Recommended).** Benefit: atomic with the copy
  (one rename), survives restarts, drives D4's "seeded start" without a
  second source of truth; no schema bump. Risk: one more table to keep in
  `wipe`'s list (a control pins it).
- (b) A `seeded-from` file in the state dir. Benefit: no schema change.
  Risk: can disagree with the index (crash between copy and file write,
  `g-mesh reindex` leaving it behind).

### D7. Gitignored inputs that differ between main and the worktree

**Today.** The reconcile compares only walked files. A fresh worktree lacks
gitignored inputs main has: `plugins/typescript/node_modules`, a Python
venv, `target/` with build-script output. Edges main's language servers
resolved *into* those (e.g. a TS import resolved to a `node_modules` type)
are copied, though a cold index of the worktree would leave them unresolved
until `npm ci`. Conversely nothing is lost: tracked code is reconciled.
**Change / example.** A TS file importing `@modelcontextprotocol/sdk`:
seeded, `find_references` lists the external target as main resolved it;
cold in a worktree without `node_modules`, it is unresolved.

- (a) **Document it (design note, `docs/` worktree section, the status
  line), no code (Recommended).** Benefit: the seeded answer is the one the
  worktree will have after its own `npm ci`/build, i.e. closer to the
  intended state; no work. Risk: until then the seeded index answers as if
  the dependencies were installed; a dependency *version* differing between
  branches (lockfile change) is only caught when the lockfile's
  `[plugin.workspace] watch_files` triggers a reindex - which the seeded
  reconcile does route (the lockfile is walked and differs).
- (b) Reindex every language with a `watch_files` difference eagerly.
  Benefit: none beyond (a) - that already happens through routing. 
- (c) Skip seeding for languages whose resolution depends on gitignored
  inputs. Risk: partial seeding is not possible with one `index.db`; it
  would mean per-language wipes after the copy. Not recommended.

### Non-decisions (settled by the code)

- Two worktrees seeding at once: each daemon seeds its own state dir under
  its own singleton lock (`acquire_singleton_lock`, `mod.rs:300-304`); both
  read the source as WAL readers. No coordination needed.
- Where the code lives: new `core/src/daemon/seed.rs`. Main-checkout
  discovery is not project discovery and stays out of
  `core/src/daemon/candidates.rs` (GM-571 may change it).
- `g-mesh init` / `g-mesh reindex` never seed: `reindex` wipes anyway, and
  `init` is an explicit cold build.
- Folder (front) mode never seeds: `daemon::run` returns into `front::run`
  (323-326) before the seed point.
- Disk: the seeded `index.db` is the size of main's (~110 MB, vectors
  included), about the cold index's 114.8 MB; no extra.

## Edit map

| Change | Where | What |
|---|---|---|
| new module | `core/src/daemon/seed.rs` (+ `pub(crate) mod seed;` in `mod.rs:3-20`) | `main_checkout(root) -> Option<PathBuf>` (D1a), `seed_if_eligible(root, dir, &discovered) -> Option<SeedOrigin>` (D2a, D3a), `remove_stale_seed_tmp(dir)`, `G_MESH_SEED` check |
| seed call | `daemon::run`, `core/src/daemon/mod.rs:334-337` | after `remove_stale_staging` (336), before `connection::open` (337); log "seeded from …" or the reason not |
| seeded start | `mod.rs:340-366`, `411-415` | after `ensure_current`: `seeded_unreconciled = schema::seed_pending(&conn)`; initial status `IndexingStatus::walking_because(ColdCause::Seeded)` when set |
| consumer | `spawn_watch_consumer`, `mod.rs:561-583` (and its call in `activation.rs` `activate`, from 139) | pass a `ReconcileMode { Plain, Seeded }`; Seeded skips `recheck_gitignores_since_index`, calls the seeded reconcile, sets `Structural`, stamps `reconciledAt`, runs the deferred scoped semantic passes |
| reconcile | `reconcile_since_index`, `core/src/daemon/registry.rs:1339-1394` | mode parameter: Seeded uses deleted = live − walked and a structural-only `ensure_fresh` variant; returns the reparsed files per language |
| structural-only reparse | `ensure_fresh` `registry.rs:1646-1665` → `PluginSupervisor::ensure_fresh` → `apply_file_change_in` `core/src/watcher/apply.rs:222-345` | thread a "defer semantic" flag to the existing `semantic_pass_capable = false` branch (282) and record `semantic_pending_files` |
| schema | `core/src/storage/schema.rs` DDL (after 680) and `wipe` (1940-1964) | `seed_origin` table; helpers `record_seed_origin`, `seed_pending`, `record_seed_reconciled`; add to `wipe` |
| status | `core/src/daemon/indexing_status.rs:105-114, 267-292` | `ColdCause::Seeded`, `walking_because(cause)`; message text for a waiting tool call |
| dependency | `core/Cargo.toml:112` | rusqlite feature `backup` |
| status line (optional) | `core/src/cli/status.rs` | "seeded from" line |
| docs | `docs/` worktree section, CLAUDE.md "Code search" guidance | first-call cost for a seeded worktree; D7 caveat |

Read for context (not edited): `connection::open` (`connection.rs:119-132`),
`schema::ensure_current`/`stored_generation` (`schema.rs:710-779`),
`staleness::decide` (`staleness.rs:261-300`), `gitignore_changed`
(`registry.rs:1223-1270`), `route_settled_path` (`registry.rs:1034-1046`),
`apply_scoped_semantic_pass` (`apply.rs:163-221`) and
`PluginProcess::scoped_semantic_pass` (`plugin.rs:1512`), `activate`
(`activation.rs:139-190`), `mcp::wait_for_index` (`mcp/mod.rs:469-520`),
`core/tests/offline_changes_reconcile.rs` (harness to reuse).

## Must-confirm for the code slice

- Starting at phase `Walking` with `needs_walk = false`: `request_activation`
  and `activate` must not start a walk, and the consumer (not activation)
  must move the phase to `Structural`. When a semantic or failed-language
  retry is owed, the consumer starts from `activate`; the gate must still
  open then.
- A seeded deletion of a file that still exists (ignored in the worktree):
  `route_settled_path` → `file_changed` may reparse an existing file instead
  of deleting it; the seeded reconcile may need the explicit delete path.
- The deferred scoped pass clears exactly the `semantic_pending_files` rows
  it set, and an incomplete pass leaves them owed (`settle_owed_files`).
- `Backup` from a `SQLITE_OPEN_READ_ONLY` connection on a WAL source with no
  `-shm` present (main daemon not running): must succeed, or fall back to no
  seed with one log line.
- The temp copy's `seed_origin` row is written with the copy's own
  connection (foreign keys off, as `connection::open` sets).

## Must-confirm: answers from the code slice

- Gate: a seeded start begins at `Walking` (`ColdCause::Seeded`) with
  `needs_walk = false`. `request_activation` changes the phase only from
  `Unindexed`/`Failed`, and `activate` walks only when `needs_walk`, so
  neither starts a walk. The consumer holds a `seed::SeedGate`; opening it
  (or dropping it, on a panic) moves `Walking` to `Structural`. `activate`
  waits on the gate's receiver before it sets `Embedding`, so the phase
  never skips past or returns to `Structural`. When a semantic or
  failed-language retry is owed, the gate travels in `ActivationCtx` to the
  consumer `activate` starts; the gate then opens after those retries (tool
  calls wait for them too).
- Deletions: a seeded deletion is never routed. Every live file the worktree
  does not walk (on disk or not) is retired by
  `PluginRegistry::retire_file`: an empty diff applied with
  `FileScope::Gone` (nodes, outgoing edges, baseline, semantic rows), no
  plugin, so no reparse of an ignored file and no language-server start for
  a deleted one.
- Pending rows: `record_semantic_pending_files` writes rows for the reparsed
  files of semantic-capable, unsuspended languages; the scoped pass clears
  the requested files' rows when it completes without a list, the settled
  ones when it names `unfinishedFiles` (the rest become owed), and none when
  it is incomplete without a list or fails. Those rows speak (`staleFiles`)
  only while the language's semantic pass is not done; a later start's
  `remove_stale_semantic_pending` clears them for a done language.
- Read-only source: a `SQLITE_OPEN_READ_ONLY` connection cannot open a WAL
  database with no `-wal` (a cleanly exited main daemon), checked with SQLite
  3.51: "unable to open database file". The source is opened
  `SQLITE_OPEN_READ_WRITE` without `CREATE`; it only reads. With `-wal`
  present and `-shm` missing, both modes read the WAL-only rows.
- The copy's `seed_origin` row is written on the copy's own connection after
  `PRAGMA foreign_keys = OFF` (`seed_from`).
- D7's lockfile claim does not hold: the reconcile walks only files a
  language indexes, so a `watch_files` file that differs between branches is
  not routed at start (plain and seeded alike); its next edit is.
- After the seeded start, `recheck_gitignores_since_index` takes the later of
  `bulkIndexedAt` and `seed_origin.reconciledAt`, so the second (plain) start
  does not re-check every fresh `.gitignore`.

## Behaviours the tests must pin

Unit (`core/src/daemon/seed.rs` tests, temp dirs, a real `git worktree add`
where git is needed):
1. `main_checkout`: a linked worktree → main root; a project in a
   subdirectory → the same subdirectory of main; the main checkout itself →
   None; a `.git` file without `commondir` (submodule) → None; a bare repo's
   worktree → None.
2. No seed when the target `index.db` exists (left byte-identical), when the
   source lacks `bulkIndexedAt`, when the generation differs (no file
   created), when `G_MESH_SEED=0`.
3. Consistency: rows committed by a second connection and still only in the
   source's `-wal` (autocheckpoint off) are present in the seed.
4. `seed_origin` is written; `reset` (wipe) removes it; a stale
   `index.db.seed-tmp` is removed at start.

Integration (`core/tests/worktree_seed.rs`, the offline-reconcile harness:
shim + daemon + real TypeScript plugin over a temp git repo):
5. Main indexed; `git worktree add`; in the worktree, one function body
   changed and one file deleted; main has one untracked indexed file. Start
   the worktree daemon: the log says "seeded from"; `find_definition` of an
   unchanged symbol equals main's answer (same file, lines, source);
   `find_definition` of the changed symbol returns the worktree's source; the
   deleted and the untracked file are absent; the log has no "reindexing
   typescript" line.
6. A tool call issued right after connect returns the worktree's content
   (gate), using a test hook that holds the seeded reconcile (precedent:
   `hold_compute_open_for_tests`, `apply.rs:1013`).
7. Restart after a kill between seeding and `reconciledAt`: the seeded
   reconcile runs again; after `reconciledAt`, the next start is a plain one.

New tests with processes or timers (5, 6, 7) run 5x in the tests slice and
5x in verify (once under load).

## Controls (7)

| # | Revert (code, never the test) | Must fail |
|---|---|---|
| C1 | skip the seeded reconcile (seed only) | 5: changed symbol returns main's source (the acceptance's control) |
| C2 | replace the backup with `fs::copy` of `index.db` | 3 |
| C3 | drop the generation check before copying | 2 (mismatch case creates a file) |
| C4 | remove `seed_origin` from `wipe` | 4 |
| C5 | seeded mode uses the plain deleted rule and keeps the `.gitignore` re-check | 5 (untracked file / no-reindex assertion) |
| C6 | `main_checkout` accepts a `.git` file without `commondir` | 1 (submodule case) |
| C7 | start a seeded index at `Structural` (no gate) | 6 |

C2-C4 and C6 share the `--lib` build; C1, C5, C7 share
`--test worktree_seed`: two groups.

## Measure protocol (later slice)

- Machine: record `uptime` before each run and `/usr/bin/time -p` around
  each daemon start; abort a run if load > 2x cores. Main checkout's daemon
  idle and complete first (`g-mesh status`: walked, semantic pass done,
  embeddings ready); no other g-mesh-bench or cargo job running.
- Arms, each on a fresh `git worktree add ../g-mesh-wt-seedNN main`
  (removed with `~/.claude/hooks/wt-cleanup.sh` after the run, relative
  path):
  - **cold**: `G_MESH_SEED=0`;
  - **seeded**: default;
  - **seeded-diverged**: worktree at `main~50` (reconcile reparses many
    files).
  3 runs per arm, interleaved (cold, seeded, diverged, ...). `npm ci` not run
  in any arm (equal footing; note D7).
- Per run: wall time from MCP `initialize` to the first `find_definition`
  answer (symbol `daemon::run`, unanchored); time to phase `Structural` and
  to `Ready` (phase file); the seed's copy ms and the reconcile's log line
  (files, deleted, created, reindexed, ms); `index.db` size; whether
  `target/` was created in the worktree and its size (rust-analyzer build
  avoided?); `user`/`sys` vs `real`.
- A/B sanity: one cheap observation that tells the arms apart - the
  "seeded from" log line present only in seeded arms, and `seed_origin`
  present only there.
- Correctness spot check per seeded run: `find_definition` of three
  unchanged symbols equals the cold arm's answer (file, lines).

## g-mesh calls behind the cross-file answers

The MCP connection is shared with other agents; twice a result came back
"answered from project g-mesh-wt-gm587" (another agent's selection). Those
answers are marked; they concern code GM-587 does not change here as far as
this note relies on (line numbers matched `main`'s files within the
zero-based offset), and the production answers were re-asked after
re-selecting `g-mesh`.

- `find_callers storage::connection::open` (g-mesh): production callers
  `daemon::run`, `cli::init::init`, `cli::reindex::reindex`.
- `find_callers daemon::run` (g-mesh): `cli::dispatch` only (2 unlinked
  usages in `plugins/typescript/tests/semantic_runs_no_project_code.rs`).
- `find_callers ensure_current` (g-mesh): `daemon::run`, `cli::init::init`
  in production; the rest tests.
- `find_callers spawn_watch_consumer` (gm587): `daemon::run`,
  `activation::ActivationCtx::activate`.
- `find_callers reconcile_since_index` / `recheck_gitignores_since_index`
  (gm587): no resolved callers, semantic tier absent; one untyped receiver
  call each in `core/src/daemon/mod.rs` (confirmed by reading
  `spawn_watch_consumer`, `mod.rs:572-577`) and one in
  `registry/tests.rs` for the latter.
- `find_references wipe` (gm587): called only by `schema::reset`.
- `find_callers apply_scoped_semantic_pass` (gm587):
  `PluginProcess::scoped_semantic_pass`.
- `find_callers PluginRegistry::ensure_fresh` (gm587, `answer: files`):
  untyped receiver calls in `daemon/lifecycle.rs`, `mcp/mod.rs`,
  `registry.rs`, tests.
- grep (single known files / non-code): `daemon/mod.rs`, `registry.rs`,
  `connection.rs`, `schema.rs`, `indexing_status.rs`, `staleness.rs`,
  `apply.rs` outlines; `core/Cargo.toml`; plugin state dirs.
