# GM-707: `watcher` split into events and `ingest`; `daemon::run` in named phases (R-15, R-25)

Status: design (S1), for owner review. Base: `main` at `142af2e9` (`release-4.32.0` differs only by
the version bump). Line numbers are 1-based, as of that commit. Sources: `docs/review/4.30/README.md`
R-15, R-25. Follow-ups that land on this layout: GM-720 (watcher lib tests flaky under load, fixed in
`ingest/`), GM-721 (drop the state-path compat wrappers, `daemon/mod.rs` lines 55-181).

## 0. Where we start

`python3 scripts/module-cycles.py` (non-test code, `use` + inline paths):

```
SCC (3): daemon, mcp, watcher
SCC (3): embedding, graph, storage
watcher -> daemon 1/0, embedding 2/0, project_walk 3/0, protocol 3/3, storage 8/0
cli     -> ... watcher 2/0 ...
```

The single `watcher -> daemon` edge is `watcher/apply.rs:422`
(`use crate::daemon::config_reindex::{select_with, Selection}`). `watcher/` holds two jobs: the FS
event source (`mod.rs` `ProjectWatcher`, `batch`, `burst`, `debounce`, `ignore_layers`, `rescan`) and
the plugin round trip plus diff application (`apply.rs`, `staleness.rs`). Only the second one reaches
`protocol`, `embedding` and `daemon`.

`daemon::run` (`daemon/mod.rs:188-482`) is one 295-line function: claim, front branch, discovery,
index open and its reads, bind and publish, services, watcher, activation, supervise. The watcher
loop (`spawn_watch_consumer`, `Rescans`, `watch_and_route_once` and helpers, 504-715 + 763-773) lives
in the same file.

## 1. Decisions

### D1. New top-level module `ingest/` holds `apply`, `staleness`, `convert`, `select`

- **Today:** `watcher::apply` (round trip + commit, 1419 lines) and `watcher::staleness` (query-time
  freshness) sit beside the FS watcher; anything that needs to apply a diff imports `watcher`.
- **Change:** `git mv` them to `core/src/ingest/apply.rs` (+ `apply/tests.rs`,
  `apply/tests/affected.rs`) and `core/src/ingest/staleness.rs`; split the wire->storage mapping out
  of `apply.rs` into `ingest/convert.rs`; move the selection SQL from `daemon/config_reindex.rs` into
  `ingest/select.rs` (D2). `ingest/mod.rs` declares `pub mod apply; pub(crate) mod convert;
  pub(crate) mod select; pub mod staleness;` and `lib.rs` gains `pub mod ingest;`.
- **Example:** `daemon/bulk_index.rs:30` becomes
  `use crate::ingest::convert::{to_edge_record, to_node_record, PathWarnings};`;
  `core/tests/it/replay_progress.rs:35` becomes `use g_mesh::ingest::apply::HOLD_COMPUTE_FILE_ENV;`.
- **Consequence:** `watcher` keeps only the event source; its edges become `project_walk`, `storage`
  (`batch`, `burst`) and `ingest` (`batch.rs:23` reads `staleness::has_indexed_baseline`).
  `watcher -> protocol`, `watcher -> embedding`, `watcher -> daemon` and `cli -> watcher` disappear.
  `Unit::WatcherApply` and every log string stay as they are.
- **Alternatives:** the name `index_apply/` (Fable's) - `ingest` is shorter and also covers
  `staleness`; keeping `watcher::apply` as a `pub use` re-export of `ingest::apply` - rejected: it keeps
  a `watcher -> ingest` alias for code that no longer belongs there, and two paths for one item.
  No re-exports at all: every importer is updated (section 3.1 lists all 22 sites).
- **Risk:** the public paths `g_mesh::watcher::apply::*` / `g_mesh::watcher::staleness::*` change;
  their only users outside `core/src` are 6 files in `core/tests/it` (import lines only). The embedding
  eval corpus names `watcher::apply::*` qualified names, but `eval/embedding/corpora.toml` pins g-mesh
  at revision `805686ff`, so it is unaffected. `embedding/testdata/*.json` holds the old names as text
  fixtures; not code, left alone.

### D2. `select_with` moves down into `ingest::select` (a move, not a callback)

- **Today:** `ingest`'s re-extract of an `affected` answer (`apply.rs::reextract_affected`, 383-469)
  calls `daemon::config_reindex::select_with`, which is pure SQL over `&Connection` plus wire
  selectors. The daemon also uses it (`config_reindex::selective` via `select`).
- **Change:** move the whole selection layer, unchanged, to `core/src/ingest/select.rs`:
  `FALLBACK_SHARE_PERCENT`, `Selection`, `select`, `select_with`, `select_affected`,
  `indexed_files_of`, `indexed_file_count`, `indexed_importers`, `scan_imports`, the `#[cfg(test)]
  reads` counters, `StoredImport` + `impl`. `config_reindex` imports `select`, `Selection`,
  `FALLBACK_SHARE_PERCENT` from there.
- **Example:** `apply.rs:422` becomes `use crate::ingest::select::{select_with, Selection};`.
- **Consequence:** `daemon -> ingest` (downward) replaces `watcher -> daemon` (upward). The selection
  depends only on `protocol::types` and `rusqlite`.
- **Alternatives:** a callback (`&dyn Fn(&Connection, ...) -> Result<Selection>`) passed into
  `apply_file_change` - rejected (review R-15 picked the move): it threads a parameter through every
  `apply_*` entry point and their 20+ call sites for no behaviour gain. Putting it in `storage` -
  rejected: it takes `protocol` wire selectors, and `storage -> protocol` would widen the
  `embedding, graph, storage` component's surface.
- **Risk:** none at run time (pure move). The selection tests stay in
  `daemon/config_reindex/tests.rs` (D5) and reach the moved items by an explicit import.

### D3. `mtime_millis` moves to a new leaf module `fsutil`

- **Today:** `watcher::staleness::mtime_millis` (418-429) is imported by `daemon/build_stamp.rs:89`
  (the daemon exe's mtime) and `cli/status.rs:60` (counting owed files), and called inline in
  `daemon/tests.rs:1083` and `daemon/plugin/tests.rs:535`.
- **Change:** `core/src/fsutil.rs` (new, `pub mod fsutil;` in `lib.rs`) holds
  `pub(crate) fn mtime_millis(&fs::Metadata) -> Result<i64>`, moved verbatim with its doc. `staleness`
  imports it from there.
- **Example:** `daemon/build_stamp.rs:89` becomes `use crate::fsutil::mtime_millis;`.
- **Consequence:** `build_stamp` (which has nothing to do with ingest) and `cli::status` stop reaching
  into the ingest layer for a metadata helper. `fsutil` has no `crate::` edges.
- **Alternatives:** `paths` (also a leaf) - rejected because `paths` is "where g-mesh keeps its
  state", and a file-metadata reader there would be the first unrelated item; keeping it in
  `ingest::staleness` and importing from there - satisfies the acceptance text literally but makes
  `build_stamp` depend on `ingest` for a two-line helper. **Owner may prefer `paths`** (no new module).
- **Risk:** one more top-level module (21 -> 23 with `ingest`).

### D4. `convert` is the wire->storage mapping, cut at a fixed line range

- **Today:** `apply.rs:1182-1416` maps `FileChangeDiff`/`WireNode`/`WireEdge` to `Diff`/`NodeRecord`/
  `EdgeRecord`; `PathWarnings` logs bad plugin paths once per file. `bulk_index` already reuses it.
- **Change:** that range moves verbatim to `ingest/convert.rs`: `to_storage_diff`, `PathWarnings` +
  `impl`, `checked_paths`, `to_node_record`, `to_storage_visibility`, `to_placeholder_target_record`,
  `to_edge_record`, `edge_kind_wire_value`, `edge_source_tier_wire_value`. `to_storage_diff`,
  `checked_paths`, `to_placeholder_target_record` go from private to `pub(crate)` (apply and its tests
  use them). `method_name` (1165-1180) and `HOLD_COMPUTE_FILE_ENV` stay in `apply` (round trip).
- **Consequence:** "pure" here means no I/O, no locks, no plugin; the only side effect is
  `PathWarnings::warn`'s `log_line!`, which is unchanged.
- **Risk:** none at run time.

### D5. Tests move with their files; no test body changes; import lines may change

- **Today:** `watcher/apply/tests.rs` (3057 lines) and `staleness.rs`'s inline tests use `super::*`.
  The selection tests are in `daemon/config_reindex/tests.rs` (lines 20-540) next to `run`'s end-to-end
  tests and share their fixtures.
- **Change:** the test files move with `git mv`; only `use` lines and inline `crate::watcher::...`
  paths change. The selection tests stay in `config_reindex/tests.rs` with one added import.
- **Consequence:** GM-720 lands its flaky-test fix on `ingest/apply/tests.rs` / `ingest/staleness.rs`
  with unchanged test names (nextest names become `ingest::apply::tests::...`).
- **Alternative:** move the selection tests to `ingest/select/tests.rs` - cleaner, but splits shared
  fixtures; a follow-up if wanted.
- **Risk:** test lists keyed on test names (`scripts/test-plugin-spawns.txt`): see must-confirm C3.

### D6. Moved code keeps its comments verbatim

The project's comment rule (CLAUDE.md) invites cleanup inside a refactor ticket, but S2 is a
mechanical move by a Sonnet agent: a reworded comment hides a real change in a move diff. S2 changes
only module paths inside comments and intra-doc links (`crate::watcher::apply` ->
`crate::ingest::apply`, `config_reindex::select_with` -> `ingest::select::select_with`). Comment
cleanup of `ingest/*` can be a later one-file-at-a-time ticket.

### D7. `daemon::run` keeps its signature and becomes a sequence of named phases in `daemon/startup.rs`

- **Today:** one function, side effects interleaved with decisions.
- **Change:** `run` (stays in `mod.rs`, `pub fn run(root: &Path) -> Result<()>`) reads as:

  ```rust
  pub fn run(root: &Path) -> Result<()> {
      let dir = StateDir::ensure(root)?;
      let Some(singleton) = startup::claim(&dir)? else { return stand_down(root) };
      let detection = candidates::detect_in(root, dir.path(), candidates::Limits::default());
      if detection.mode == candidates::Mode::Multi {
          return front::run(root, &dir, singleton, detection);
      }
      let opened = startup::open_index(root, &dir)?;          // discovery .. IndexStore
      let plan = StartupPlan::decide(&opened.facts);          // pure
      let canonical_root = startup::canonical_root(root)?;
      let published = startup::publish(&dir, &plan)?;         // bind .. record_serving_owner
      let services = startup::start_services(root, &dir, opened, &canonical_root, published.listener, &published.indexing)?;
      startup::start_watcher_and_activation(root, &plan, &services, published, &canonical_root)?;
      startup::supervise_and_tear_down(&canonical_root, &dir, singleton, services)
  }
  ```

  (Phase boundaries and names are the design; exact parameter lists are S3's to settle.) Phases:

  | phase | side effects, in order (section 4 numbers) |
  |---|---|
  | `claim` | 2 lock, 3 rebuild marker, 4 build stamp |
  | (front branch, in `run`) | 5 |
  | `open_index` | 6 discovery, 7 staging/seed tmp, 8 seed, 9 open, 10 generation read, 11 `ensure_current` + log, 12 semantic-pending cleanup, 13 `last_used::touch`, 14 fact reads + seed log, 15 `IndexStore` + link rules |
  | `canonical_root` | 16 |
  | `publish` | 17 stale socket, 18 suspension markers, 19 bind, 20 pid file, 21 phase/progress/activation attach, 22 `record_serving_owner` |
  | `start_services` | 23 config, 24 embedding + rerank file, 25 registry, 26 `seed_failed_languages`, 27 `CoreActivity`, 28 accept thread |
  | `start_watcher_and_activation` | 29 seed gate, 30 watcher (+ consumer), 31 activation spawn |
  | `supervise_and_tear_down` | 32 supervise, 33 drop lock, 34 purge |

- **Consequence:** each ordering constraint sits inside one phase or at a phase boundary, and the
  boundaries follow the constraints (section 4). `run` stays the only entry point (`cli`, `shim`,
  integration tests call `daemon::run`).
- **Alternatives:** phases as private fns in `mod.rs` - fine too, but GM-721 edits `mod.rs` next and
  `mod.rs` would stay ~800 lines; a `Startup` struct with methods - rejected: state moves between
  phases by value (`listener`, `singleton`, `discovered`), which free functions express directly.
- **Risk:** the watcher-start condition and the seed-gate handoff are the intricate part; D8 makes
  them a pure, tested decision.

### D8. The startup decision is a pure `StartupPlan::decide(&IndexFacts)` (section 5)

- **Today:** `cold_cause`, `needs_semantic_pass_retry`, the initial `IndexingStatus`, whether the
  watcher starts now, whether its consumer starts now and with which `ConsumerStart`, and where the
  seed gate goes are computed inline from five reads (`mod.rs:247-296`, `373-433`).
- **Change:** `open_index` gathers `IndexFacts` with the **same conditional reads** as today (the
  `semantic_pass_completed`, `languages_owed_a_retry` and `seed_pending` reads happen only on a walked
  index, in that order, with the same error handling); `StartupPlan::decide` turns them into data;
  phases act on the plan.
- **Example:** a walked, seeded index whose semantic pass never completed:
  `InitialPhase::Walking(Seeded)`, `WatcherPlan::ParkedForActivation`, `seeded: true` -> the gate is
  created and handed to activation, not to a consumer.
- **Consequence:** the 8 cases in section 5 are unit tests that run in microseconds without a daemon.
- **Alternative:** read every fact unconditionally then decide - rejected: `semantic_pass_completed`
  is fatal on error (`?`), so reading it on an unwalked index adds a new startup failure path.
- **Risk:** a plan field that is never read by a phase (dead output). S4 review checks each field has
  one consumer.

### D9. The watcher loop moves to `daemon/watch_loop.rs`

`DEBOUNCE_WINDOW`, `ConsumerStart`, `spawn_watch_consumer`, `Rescans` + `impl`,
`gitignore_watermark`, `unix_now_secs`, `timing_path`, `watch_and_route_once`, `relative_wire_path`
move verbatim. It stays in `daemon` (not `watcher`): it drives `PluginRegistry`, which is daemon's.
`activation.rs` imports `ConsumerStart`/`spawn_watch_consumer` from `crate::daemon::watch_loop`.

## 2. Slicing

The draft holds, with one change: **S2 and S3 can run in parallel** (separate worktrees). Their file
sets overlap only in `daemon/tests.rs`, in different hunks (S2: lines 1083, 1101, 1195; S3: the import
block at 32-34). If parallel worktrees are not wanted, S2 then S3 as drafted.

| slice | model | scope |
|---|---|---|
| S2 R-15 | sonnet | section 3.1, mechanical; build + `cargo clippy` + the moved tests' sections only |
| S3 R-25 | opus | section 3.2, `StartupPlan` with the section-5 tests written alongside (the plan's type is S3's) |
| S4 review | opus | diff of S2+S3 against sections 3-5; check each startup invariant in section 4 |
| S5 tests | sonnet/opus | controls (section 6) and any StartupPlan case S3 left out; no new tests for R-15 (pure move) |
| S6 verify | opus | `scripts/test-local.sh --base release-4.32.0`, controls in a throwaway worktree, checker |

Moving the StartupPlan unit tests into S3 (instead of S5) is the one change I suggest: they pin the
decision S3 extracts, and writing them is how S3 checks its own extraction.

## 3. Edit map

### 3.1 R-15 (S2): moves, logic unchanged

New files and `git mv`s:

| from | to | note |
|---|---|---|
| `core/src/watcher/apply.rs` | `core/src/ingest/apply.rs` | then cut 1182-1416 out (below) |
| `core/src/watcher/apply/tests.rs` | `core/src/ingest/apply/tests.rs` | |
| `core/src/watcher/apply/tests/affected.rs` | `core/src/ingest/apply/tests/affected.rs` | |
| `core/src/watcher/staleness.rs` | `core/src/ingest/staleness.rs` | tests are inline (440-1042), move with it |
| - | `core/src/ingest/mod.rs` | module doc (2-4 lines: "Applies a plugin's answers to the index: the round trip, the wire->storage mapping, the selection of what an edit affects, and query-time freshness.") + 4 `mod` lines (D1) |
| `watcher/apply.rs:1182-1416` | `core/src/ingest/convert.rs` | D4; `to_storage_diff`, `checked_paths`, `to_placeholder_target_record` -> `pub(crate)` |
| `daemon/config_reindex.rs:59-62` + `181-480` | `core/src/ingest/select.rs` | D2; private fns `indexed_files_of`, `indexed_file_count` -> `pub(crate)` (tests use them); `reads` stays `#[cfg(test)] pub(crate) mod` |
| `watcher/staleness.rs:418-429` | `core/src/fsutil.rs` | D3; one-line module doc |
| `lib.rs` | | add `pub mod fsutil;` and `pub mod ingest;` (alphabetical) |
| `watcher/mod.rs:1,7` | | drop `pub mod apply;` and `pub mod staleness;` |

Import and path edits (every site; produced by the g-mesh calls in section 8 plus grep for paths):

| file:line | now | becomes |
|---|---|---|
| `ingest/apply.rs:422` | `use crate::daemon::config_reindex::{select_with, Selection};` | `use crate::ingest::select::{select_with, Selection};` |
| `ingest/apply.rs:385,404` (doc) | `config_reindex::select_with`, link to `crate::daemon::config_reindex::select_with` | `select::select_with`, `crate::ingest::select::select_with` |
| `ingest/apply.rs` top | (convert items were local) | `use crate::ingest::convert::{to_storage_diff, PathWarnings};` (used at 1041); drop imports only convert needed (compiler says which) |
| `ingest/apply/tests.rs:1-11` | `use super::*;` ... | add `use crate::ingest::convert::{checked_paths, to_edge_record, to_node_record, to_placeholder_target_record};` |
| `ingest/staleness.rs:81` | `use crate::watcher::apply::{apply_file_change_in, FileChangeOutcome};` | `use crate::ingest::apply::{...}` ; plus `use crate::fsutil::mtime_millis;` |
| `watcher/batch.rs:23` | `crate::watcher::staleness::has_indexed_baseline` | `crate::ingest::staleness::has_indexed_baseline` |
| `daemon/config_reindex.rs` top | | `use crate::ingest::select::{select, Selection, FALLBACK_SHARE_PERCENT};`; drop now-unused `BTreeSet`, `params`, `Connection`, protocol selector types as the compiler reports; doc links at 19, 35 -> `crate::ingest::select::...` |
| `daemon/config_reindex/tests.rs:11-17` | `use super::*;` | add `use crate::ingest::select::{indexed_file_count, indexed_files_of, reads, select_affected, select_with};` (`super::select` and `Selection` keep resolving through `config_reindex`'s import) |
| `daemon/bulk_index.rs:30-31` | `crate::watcher::apply::{...}`, `crate::watcher::staleness` | `crate::ingest::convert::{to_edge_record, to_node_record, PathWarnings}`, `crate::ingest::staleness` |
| `daemon/plugin.rs:49-53` | `crate::watcher::apply::{...}`, `crate::watcher::staleness::{...}` | `crate::ingest::apply::{...}`, `crate::ingest::staleness::{...}` |
| `daemon/plugin/tests.rs:535` | `staleness::mtime_millis(...)` | `crate::fsutil::mtime_millis(...)` |
| `daemon/lifecycle.rs:43-44` | `crate::watcher::{apply,staleness}::...` | `crate::ingest::...` |
| `daemon/registry.rs:111`, `1576` (doc link) | `crate::watcher::staleness`, `crate::watcher::apply` | `crate::ingest::...` |
| `daemon/semantic.rs:162` | `crate::watcher::apply::SemanticPassOutcome` | `crate::ingest::apply::SemanticPassOutcome` |
| `daemon/workspace_reindex.rs:99-100` | `crate::watcher::{apply,staleness}` | `crate::ingest::...` |
| `daemon/build_stamp.rs:89` | `crate::watcher::staleness::mtime_millis` | `crate::fsutil::mtime_millis` |
| `daemon/tests.rs:1083` | `crate::watcher::staleness::mtime_millis` | `crate::fsutil::mtime_millis` |
| `daemon/tests.rs:1101,1195` | `crate::watcher::staleness::{is_stale, has_indexed_baseline}` | `crate::ingest::staleness::...` |
| `cli/status.rs:60` | `crate::watcher::staleness::mtime_millis` | `crate::fsutil::mtime_millis` (`cli/status/tests.rs:74` keeps working via `super::*`) |
| `cli/plugin_check/session.rs:87` | `crate::watcher::apply::{...}` | `crate::ingest::apply::{...}` |
| `graph/containers/tests.rs:596` | `crate::watcher::apply::to_node_record` | `crate::ingest::convert::to_node_record` |
| `core/tests/it/first_query_after_walk.rs:38` | `g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV` | `g_mesh::ingest::apply::...` |
| `core/tests/it/replay_progress.rs:35`, `superseded_collapsed_call.rs:30`, `incremental_embed_outside_lock.rs:55` | same | same |
| `core/tests/it/protocol_conformance.rs:11` | `g_mesh::watcher::apply::apply_semantic_pass` | `g_mesh::ingest::apply::...` |
| `core/tests/it/module_tree_refresh_rust.rs:226` | `g_mesh::watcher::staleness::StalenessOutcome::{...}` | `g_mesh::ingest::staleness::...` |

Prose mentions of `watcher::apply` / `watcher::staleness` in comments elsewhere (~40, e.g.
`storage/write.rs:37`, `daemon/semantic.rs:9`, `wire/src/lib.rs:593`): update the module path only
(`ingest::apply`), same sentence. `rg -n 'watcher::(apply|staleness)' core wire` must be empty after
S2 except `embedding/testdata/*.json` (data fixtures, left alone).

Read for context (no edits): `watcher/apply.rs:383-469` (`reextract_affected`),
`daemon/config_reindex.rs:122-178` (`selective`, the other `select` caller),
`watcher/batch.rs:40-55`.

Done when: `cargo build -p g-mesh --all-targets` and `cargo clippy -p g-mesh --all-targets -- -D
warnings` are clean; `python3 scripts/module-cycles.py --absent 'watcher->daemon' --absent
'ingest->daemon' --absent 'cli->watcher'` exits 0; `rg -n 'crate::daemon' core/src/ingest
core/src/watcher` prints only `watcher/batch/tests.rs:2` (see C1).

### 3.2 R-25 (S3): `run` in phases, `watch_loop.rs`

`core/src/daemon/watch_loop.rs` (new), verbatim from `daemon/mod.rs`:

| item | lines now | visibility after |
|---|---|---|
| `DEBOUNCE_WINDOW` + doc | 49-53 | `pub(crate)` (daemon tests use it) |
| `ConsumerStart` + doc | 504-517 | `pub(crate)` |
| `spawn_watch_consumer` + doc | 519-581 | `pub(crate)` (run's phase, activation) |
| `Rescans` + `impl` + doc | 582-604 | `pub(crate)` as now |
| `gitignore_watermark`, `unix_now_secs` | 606-618 | private |
| `timing_path` | 620-624 | private |
| `watch_and_route_once` + doc | 625-715 | `pub(crate)` (daemon tests call it) |
| `relative_wire_path` + doc | 763-773 | private (only `timing_path`, `watch_and_route_once` call it) |

Imports that go with them from `mod.rs:30,44-48`: `SystemTime`, `UNIX_EPOCH`, `classify_settled`,
`order_for_routing`, `Debouncer`, `GITIGNORE_FILE_NAME`, `RescanScheduler`, `Change` (drop from `mod.rs`
whatever the compiler then flags as unused).

`core/src/daemon/startup.rs` (new): `IndexFacts`, `WalkFacts`, `StartupPlan`, `InitialPhase`,
`WatcherPlan`, `StartupPlan::decide`, the phase fns of D7, and `#[cfg(test)] mod tests` with the
section-5 cases. Body of `run` (188-482) is cut into the phases with its comments kept next to the
statement they describe.

Stays in `mod.rs`: `run` (now ~20 lines), `stand_down` (484-502), `serve_forever` (716-762),
`acquire_singleton_lock` + constants (775-816), compat wrappers (55-181, GM-721's).

Other edits:

| file:line | change |
|---|---|
| `daemon/mod.rs` | `mod startup; pub(crate) mod watch_loop;` |
| `daemon/activation.rs:50` | `use crate::daemon::watch_loop::{spawn_watch_consumer, ConsumerStart};` |
| `daemon/activation.rs:188` | `super::spawn_watch_consumer(` -> `spawn_watch_consumer(` |
| `daemon/tests.rs:32-34` | add `use super::watch_loop::{watch_and_route_once, Rescans, DEBOUNCE_WINDOW};` and `use crate::watcher::{debounce::Debouncer, ignore_layers::GITIGNORE_FILE_NAME, rescan::RescanScheduler, ProjectWatcher};` and `std::time::{SystemTime, UNIX_EPOCH}` as far as `mod.rs` no longer provides them through `use super::*`. Import lines only; no test body changes. |

Read for context: `daemon/activation.rs:142-200` (`activate`: the consumer start it chooses mirrors
`run`'s), `indexing_status.rs:295-320` (the three constructors), `daemon/seed.rs` `SeedGate::new`.

## 4. Startup-order invariants (`daemon::run`, `mod.rs:188-482`)

C = ordering-constrained. "Pinned by" names tests that fail if it moves; "-" means no test pins it
and S4 must check it by reading.

| # | side effect | line | C? | why | pinned by |
|---|---|---|---|---|---|
| 1 | `StateDir::ensure` (`project.root`) | 191 | C first | a state dir without `project.root` is never cleaned | - (S4: confirm a `cli_clean_sweeping` test covers it) |
| 2 | `acquire_singleton_lock`; lost -> `stand_down` | 195-198 | C before any project file is touched | | `wedged_daemon::a_daemon_started_over_a_wedged_one_fails_fast_and_names_the_pid`, `a_serving_daemon_is_reused_by_a_second_shim_and_never_evicted`; `shim_bootstrap::two_concurrent_shim_bootstraps_produce_exactly_one_daemon` |
| 3 | `clear_stale_rebuild_marker` | 202 | C after 2 | only the lock holder knows no rebuild runs | `cli_reindex::a_crashed_rebuilds_marker_naming_a_live_pid_does_not_turn_shims_away_once_a_daemon_serves` |
| 4 | build stamp written | 206-213 | C after 2, before 19 | never transiently missing while bound | `daemon_build_staleness::*` (5 tests) |
| 5 | project detection; Multi -> `front::run` | 217-220 | C before 6, 9 | the front needs no discovery and never creates `index.db` | `multi_project_front::*` |
| 6 | plugin discovery (fatal) | 225-226 | C before 9-11 and 19 | generation needs it; a bad manifest fails before socket/pid exist | `daemon_plugin_discovery_failure::two_plugins_claiming_the_same_extension_fails_daemon_startup_with_a_clear_error`, `cli_status::a_failed_plugin_discovery_is_reported_by_light_status_and_fatal_to_full` |
| 7 | `remove_stale_staging`, `remove_stale_seed_tmp` | 230-231 | C before 9 and any reindex | leftovers belong to a dead reindex/seed | - |
| 8 | `seed_if_eligible` (+ log) | 233-239 | C before 9 | `connection::open` would create an empty `index.db` | `worktree_seed::*` |
| 9 | `connection::open` | 240 | | | |
| 10 | `stored_generation` read | 243 | C before 11 | `ensure_current`'s reset would hide it | `lazy_activation::a_discarded_index_says_so`, `an_incomplete_index_says_so` |
| 11 | `ensure_current` (+ log) | 246-250 | C before 12, 14 | | `daemon_build_staleness` (older generation) |
| 12 | `remove_stale_semantic_pending` | 258 | C after 11 | tables must exist | - |
| 13 | `last_used::touch` (fatal) | 261 | C before 19 (at open, not serving) | a long cold walk must not read as idle to GC | `last_used::a_daemon_start_and_every_handled_request_advance_last_used_on_disk` |
| 14 | reads: `bulk_index_completed` (fatal), then on a walked index `semantic_pass_completed` (fatal), `languages_owed_a_retry` (logged, -> empty), `seed_pending` (logged, -> false); seed log line | 264-296 | C: conditional and in this order | an unwalked index never reads the last three, so their errors cannot fail its start | `lazy_activation::an_existing_index_is_not_rewalked`, `serving_while_indexing::a_restart_against_an_already_walked_project_never_reports_itself_as_still_indexing`, `worktree_seed::*` |
| 15 | `IndexStore::new(..).with_link_rules` | 297-300 | | | |
| 16 | `canonicalize` (fatal) | 303-305 | keep before 19 | a failure must not leave a bound socket | - |
| 17 | `endpoint`, `clear_stale` | 307-310 | C before 19 | a dead daemon's socket makes bind fail forever | - |
| 18 | `clear_suspension_markers` | 313 | C before 19 | suspension lasts until restart | - |
| 19 | `Listener::bind` | 316-317 | C after 6, before 20 | from here `connect()` succeeds | `shim_bootstrap::shim_bootstraps_a_detached_daemon_when_none_is_running`, `serving_while_indexing::a_walk_that_outlasts_the_bootstrap_timeout_is_waited_out_rather_than_losing_the_client` |
| 20 | `write_pid_file` | 321-322 | C right after 19 | "pid file exists" means "something listens" | `wedged_daemon::stop_clears_a_wedged_daemon_that_no_pid_file_names` (inverse), every `pid_file()` harness |
| 21 | `IndexingStatus` from plan; `attach_phase_file`, `attach_progress_file`, `attach_activation` | 328-339 | C after 20, before 28 | phase published next to the pid file; activation attached before any session can get `indexing` | `lazy_activation::connecting_alone_indexes_nothing`, `index_wait_progress` |
| 22 | `record_serving_owner` | 342 | C after 19 and 21 | the record means "got as far as serving" | `daemon::tests::a_lock_held_by_a_daemon_that_has_not_begun_serving_reads_as_starting`, `a_new_holder_does_not_inherit_its_predecessors_claim_to_be_serving` |
| 23 | `read_project_config` (fatal), timeouts | 345-347 | keep after 19 | moving it earlier changes when a bad config fails (before vs after the socket) | - |
| 24 | `EmbeddingPipeline::load` (no model load), remove + record rerank outcome file | 352-354 | C: no startup thread | a startup thread broke restart budgets | `structural_does_not_wait_for_embeddings` |
| 25 | `discovered.clone()`, `PluginRegistry::new` (spawns nothing) | 358-368 | | | `lazy_activation::connecting_alone_indexes_nothing` |
| 26 | `registry.seed_failed_languages` | 372 | C before 28 and 30 | neither may route a failed language | `one_plugin_binary_missing::a_failed_language_is_not_routed_by_the_watcher_before_the_first_tool_call` |
| 27 | `CoreActivity::new` | 375 | C before 28 | idle clock starts even with no client | `idle_lifecycle` |
| 28 | accept thread (`serve_forever`) | 380-397 | after 21, 26 | | `mcp_e2e`, `shim_bootstrap` |
| 29 | `SeedGate::new` if seeded | 408-413 | | | `worktree_seed::a_tool_call_waits_for_the_seeded_reconcile` |
| 30 | walked: `ProjectWatcher::new` (fatal) and, unless a semantic retry or failed-language retry is owed, `spawn_watch_consumer(Seeded|Reconcile)` | 414-433 | C: consumer only after retries | no incremental pass before the retry; no failed language routed | `one_plugin_binary_missing::...before_the_first_tool_call`, `offline_changes_reconcile`, `worktree_seed` |
| 31 | `activation::spawn` | 439-457 | C after 30 | it takes the parked watcher / gate | `lazy_activation::*` |
| 32 | `lifecycle::supervise` | 461-469 | | | `idle_lifecycle`, `daemon_sigterm` |
| 33 | `drop(singleton)` | 473 | C before 34 | a slow teardown under the lock wedges the project | - |
| 34 | `purge_state_dir` | 477 | C after 33 | judges the dir as `clean` would | `orphaned_daemon`, `worktree_daemon_lifecycle` |

Log-line order: the lines at 8, 11 and 14 keep their relative order (`daemon_log_lines` checks
framing, not order, but humans read them).

## 5. `StartupPlan`

```rust
/// What the index said at open; read by `open_index` in `run`'s order.
pub(crate) struct IndexFacts {
    pub had_generation: bool,      // stored_generation(..).is_some(), before ensure_current
    pub reinitialized: bool,       // ensure_current(..)
    pub walk: WalkFacts,
}
pub(crate) enum WalkFacts {
    /// `bulk_index_completed` is false: nothing else was read.
    Unwalked,
    Walked { semantic_pass_completed: bool, retry_languages: Vec<String>, seeded: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StartupPlan {
    pub initial: InitialPhase,
    pub needs_walk: bool,
    pub needs_semantic_pass_retry: bool,
    pub retry_languages: Vec<String>,
    pub seeded: bool,              // a SeedGate is created; WatcherPlan says who gets it
    pub watcher: WatcherPlan,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InitialPhase {
    Unindexed(ColdCause),          // IndexingStatus::unindexed_because
    Walking(ColdCause),            // only Seeded; IndexingStatus::walking_because
    Structural,                    // IndexingStatus::structural
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WatcherPlan {
    /// Unwalked: activation starts the watcher right before its walk.
    StartedByActivation,
    /// Walked, a retry owed: the watcher starts now, activation starts its consumer (and gets the gate).
    ParkedForActivation,
    /// Walked, nothing owed: watcher and consumer start now (`Seeded` if `seeded`, else `Reconcile`).
    ConsumerNow,
}
impl StartupPlan { pub(crate) fn decide(facts: &IndexFacts) -> StartupPlan { .. } }
```

`cold_cause`: `(false, _) -> Fresh`, `(true, true) -> Discarded`, `(true, false) -> Incomplete`,
exactly `mod.rs:251-255`. `ColdCause` already derives `Debug, Clone, Copy, PartialEq, Eq`.

Unit test cases (`daemon/startup.rs` `mod tests`):

| # | facts | expected |
|---|---|---|
| 1 | no generation, Unwalked | `Unindexed(Fresh)`, `needs_walk`, `StartedByActivation`, no retry, not seeded |
| 2 | generation, reinitialized, Unwalked | `Unindexed(Discarded)` |
| 3 | generation, not reinitialized, Unwalked | `Unindexed(Incomplete)` |
| 4 | Walked, semantic done, no retry langs, not seeded | `Structural`, `ConsumerNow`, `!needs_walk` |
| 5 | Walked, semantic done, seeded | `Walking(Seeded)`, `ConsumerNow`, `seeded` |
| 6 | Walked, semantic not done | `Structural`, `ParkedForActivation`, `needs_semantic_pass_retry` |
| 7 | Walked, semantic done, `retry_languages = ["go"]` | `ParkedForActivation`, `retry_languages == ["go"]` |
| 8 | Walked, semantic not done, seeded | `Walking(Seeded)`, `ParkedForActivation`, `seeded` (gate goes to activation) |

## 6. Controls (for S5/S6)

R-15 is a move; its "control" is the checker: re-add `use crate::daemon::config_reindex::select_with;`
to `ingest/apply.rs` and `module-cycles.py --absent 'ingest->daemon'` must exit 1. R-25 (one test
binary, `--lib`, filter `daemon::startup`): (a) `ParkedForActivation` condition drops the
`retry_languages` term -> case 7 fails; (b) seeded walked index gives `Structural` -> case 5 fails;
(c) `(true, true)` maps to `Incomplete` -> case 2 fails; (d) consumer started regardless of the plan
in the watcher phase -> `one_plugin_binary_missing::a_failed_language_is_not_routed_by_the_watcher_before_the_first_tool_call`
fails (integration, heavier; keep only if S6 has budget). 3-4 controls total.

## 7. Projected cycle-checker result

After R-15 (edges per section 3.1: drop `watcher->{daemon,protocol,embedding}` and `cli->watcher`;
add `watcher->ingest`, `ingest->{storage,protocol,embedding}`, `{daemon,cli}->{ingest,fsutil}`):

```
$ python3 scripts/module-cycles.py --drop 'watcher->daemon' --drop 'watcher->protocol' \
    --drop 'watcher->embedding' --drop 'cli->watcher' --add 'watcher->ingest' \
    --add 'ingest->storage' --add 'ingest->protocol' --add 'ingest->embedding' \
    --add 'daemon->ingest' --add 'cli->ingest' --add 'cli->fsutil' --add 'daemon->fsutil' \
    --absent 'watcher->daemon' --absent 'ingest->daemon' --max-scc 3
SCC (3): embedding, graph, storage
SCC (2): daemon, mcp
ok: edge watcher->daemon absent
ok: edge ingest->daemon absent
exit=0
```

`watcher` leaves the `daemon, mcp, watcher` component; no new component appears. R-25 adds no
cross-module edge (`watch_loop`, `startup` are inside `daemon`).

## 8. Must confirm (code slices)

- **C1** `watcher/batch/tests.rs:2` has `use crate::daemon::test_plugin;` (test-only; the checker
  ignores it). The acceptance line "no `crate::daemon` import in watcher/" holds for non-test code
  only, unless the owner wants `test_plugin` moved (out of scope here).
- **C2** `use super::*` in a child module picks up the parent's private `use` imports
  (`config_reindex/tests.rs` relies on that for `select`, `Selection`); S2 confirms by building.
- **C3** `scripts/test-select.sh` / `test-sections.sh` have no rule keyed on `watcher` (grep, at
  `142af2e9`), so `core/src/ingest/` needs none either; S2 re-checks after the move, and checks
  `scripts/test-plugin-spawns.txt` for `watcher::apply::`/`watcher::staleness::` test names (nextest
  names change to `ingest::...`).
- **C4** after S2, `rg -n 'watcher::(apply|staleness)' core wire scripts docs/adr` - only fixtures
  and dated docs remain.
- **C5** S3: `open_index` keeps reads 10-14 in order and conditional; diff the order of `?` and
  `log_line!` against section 4 rows 6-15.
- **C6** S3: `record_serving_owner` stays after `attach_phase_file` (row 21 -> 22) - both live in
  `publish`; no phase boundary between bind (19) and pid file (20).
- **C7** S3: `ProjectWatcher::new` failure stays fatal and after the accept thread (row 30 after 28):
  today a watcher failure happens with the socket already bound.

## 9. g-mesh calls behind this note (project `g-mesh`, main checkout)

| fact | call |
|---|---|
| `select_with` callers: `config_reindex::select`, `watcher::apply::reextract_affected`, one test | `find_references daemon::config_reindex::select_with` (complete) |
| `select` callers: `selective`, one test | `find_references daemon::config_reindex::select` |
| `select_affected`: `select_with` + 8 tests in `config_reindex/tests.rs` | `find_references daemon::config_reindex::select_affected` |
| `Selection` referenced in `config_reindex.rs` only (plus `apply.rs:422` by import; variant matches in `selective` found by grep) | `find_references daemon::config_reindex::Selection` |
| `mtime_millis`: `staleness::{decide, walk_baseline_for}`, `cli::status::discover_source_files`, `build_stamp::of_running_process`, `cli/status/tests.rs`, `daemon/plugin/tests.rs`, `daemon/tests.rs` | `find_references watcher::staleness::mtime_millis` |
| `spawn_watch_consumer`: `daemon::run`, `ActivationCtx::activate` | `find_references daemon::spawn_watch_consumer` |
| `watch_and_route_once`: `spawn_watch_consumer` + 7 sites in `daemon/tests.rs` | `find_references daemon::watch_and_route_once` |
| `Rescans`: `mod.rs` only (+ `daemon/tests.rs` by `super::*`, grep) | `find_references daemon::Rescans` |
| `ConsumerStart`: `spawn_watch_consumer`, `activation.rs` | `find_references daemon::ConsumerStart` |
| `relative_wire_path`: `timing_path`, `watch_and_route_once` | `find_references daemon::relative_wire_path` |
| item ranges | `get_file_outline` on `watcher/apply.rs`, `watcher/staleness.rs`, `daemon/config_reindex.rs`, `daemon/mod.rs` |
| `ColdCause` derives | `find_definition indexing_status::ColdCause` |

grep was used for: module paths in `use` lines and doc comments (`crate::watcher::`, `g_mesh::watcher::`,
non-code), the convert items' users (private fns, one file each), and the test names in section 4.

## 10. Owner decisions (2026-11-16)

- D3 home of `mtime_millis`: "Новый модуль fsutil (Recommended)".
- `watcher/batch/tests.rs:2` importing `daemon::test_plugin`: "Перенести test_plugin ниже" - the
  test helper moves to a module below `watcher` and `daemon` (S2 picks the home and lists it), so no
  `crate::daemon` path remains anywhere under `watcher/` or `ingest/`, tests included.
- Slicing: "Последовательно, тесты в S5" - S2 then S3, not in parallel; the `StartupPlan` unit tests
  are written in S5 by a different agent, not in S3.
- Design: "Утвердить (Recommended)".
