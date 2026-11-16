# GM-706: control plane out of `cli`, and `StateDir` (R-5, R-4 step 2)

Status: design (S1), for owner review. Base: `release-4.31.0` at `4f2ff074` (GM-705 merged).
Line numbers are 1-based, as of that commit. Sources: `docs/review/4.30/README.md` R-4, R-5;
`docs/design/GM-705-break-core-scc.md` (projection, R-4 step 1).

## 0. Where we start

`python3 scripts/module-cycles.py` on this branch:

```
SCC (5): cli, daemon, mcp, shim, watcher
SCC (3): embedding, graph, storage
```

Edges this task is about (`--edges`):

| edge | sites (use/inline) | where |
|---|---|---|
| daemon->cli | 0/4 | `daemon/lifecycle.rs:1015-1022` (`cli::clean::{LockWait, purge_orphaned_state_dir, Purge}`) |
| shim->cli | 2/0 | `shim.rs:15` (`cli::stop`), `shim.rs:152` (`cli::clean::{sweep_orphaned_if_due, Sweep, ORPHAN_SWEEP_INTERVAL}`) |
| config->storage | **gone** | GM-705 removed it: `config -> paths 1/2` is config's only edge. Nothing to do here; S3's brief drops it. |

Test-only, not counted by the checker: `daemon/semantic.rs:786,793` (inside `#[cfg(test)] mod`
from `:617`) call `crate::cli::status::{semantic_pass_state, semantic_pass_lines}`. GM-708 (R-32)
owns that. The acceptance line "no `crate::cli` import in daemon/" is read as non-test code,
the checker's default (owner decision D5).

## 1. Module layout

```
core/src/control.rs          pub mod lock; pub mod purge; pub mod stop; pub mod sweep;
core/src/control/stop.rs     stop a project's daemon and plugins (from cli/stop.rs)
core/src/control/purge.rs    judge and delete one orphaned state dir (from cli/clean.rs)
core/src/control/sweep.rs    the shim's startup sweep over all state dirs (from cli/clean.rs)
core/src/control/lock.rs     DaemonLock + inspect_daemon_lock (from daemon/mod.rs), S3
core/src/paths/state_dir.rs  + StateDir, OnUnreadable, every state file name, S3
```

`lib.rs`: `pub mod control;` (alphabetical, after `config`).

`control` may import `paths`, `process`, `ipc`, `log` (macro), and nothing else in the crate.
It must never import `daemon`. Without that rule `daemon -> control` (lifecycle's purge) plus
`control -> daemon` (stop needs pid paths, lock probe, endpoint) would put `control` into the
daemon component, which is the same cycle under a new name (projection in section 6).

Why `control/lock.rs`, a fourth file the review did not list: `control::stop` needs
`inspect_daemon_lock` (`cli/stop.rs:139`) and `is_listening`/`endpoint` (`:303-304`). Those
are the "is anything serving this project" probe; they are used by stop, the shim's eviction,
and `cli::status`, never by the daemon itself (only its tests). Moving them is the only way
to keep `control` off `daemon` (decision D3).

Endpoint: `ipc` gains `pub fn endpoint_for(state_dir: &StateDir) -> Option<Endpoint>` (the
body of `daemon::endpoint_in`, `daemon/mod.rs:100-111`, with `SOCKET_FILE` from
`paths::state_dir`). New edge `ipc -> paths`; `paths` stays a leaf.

Shim bootstrap/evict into `control` (the review's optional second step): **not worth it in
GM-706** (D4). It changes no edge (after this task nothing reaches `shim` or `cli` anyway),
`retire_outdated_daemon`/`evict_wedged_daemon` are shim policy (when to replace a daemon), and
the part they share with `control` (the bootstrap lock file and its open-or-create) becomes
`StateDir` methods in S3, so the lock protocol is single-sourced without moving them.

## 2. `StateDir` API (`paths::state_dir`)

`paths` stays a leaf (std, anyhow, sha2, `crate::log_line!`, which it already uses). No
`process`, no `serde_json`, no `ipc`: liveness, JSON snapshots and endpoints stay with their
owners and take a `&StateDir`.

```rust
// ---- file names: the only definitions in the crate ----
pub const PROJECTS_DIR: &str = "projects";            // under the home (today a literal, state_dir.rs:119)
pub const ROOT_FILE: &str = "project.root";            // exists
pub const DEVICE_FILE: &str = "project.dev";           // exists
pub const INDEX_DB_FILE: &str = "index.db";
pub const CONFIG_FILE: &str = "config.toml";           // project state dir and the home
pub const DAEMON_LOG_FILE: &str = "daemon.log";
#[cfg(unix)] pub const SOCKET_FILE: &str = "daemon.sock";
pub const PID_FILE: &str = "daemon.pid";
pub const BUILD_STAMP_FILE: &str = "daemon.build";
pub const BOOTSTRAP_LOCK_FILE: &str = "bootstrap.lock";
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
pub const DAEMON_SERVING_FILE: &str = "daemon.serving";
pub const REBUILD_MARKER_FILE: &str = "reindex.pid";
pub const PHASE_FILE: &str = "index.phase";
pub const PROGRESS_FILE: &str = "index.progress";
pub const RERANK_FILE: &str = "rerank.outcome";
pub const SEED_TMP_FILE: &str = "index.db.seed-tmp";
pub const ORPHAN_SWEEP_STAMP: &str = "orphan-sweep.stamp"; // under the home
const PLUGIN_PREFIX: &str = "plugin-";
const PID_SUFFIX: &str = ".pid";
const SUSPENDED_SUFFIX: &str = ".suspended";
const STAGING_PREFIX: &str = "staging-";
const STAGING_SUFFIX: &str = ".db";

/// What a probe answers when the file it probes cannot be opened or locked for a reason
/// other than "not there". Named at every call site: the two callers that exist want
/// opposite answers on purpose (section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnreadable { Held, Free }

/// One project's state directory (`<home>/projects/<project_hash>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

impl AsRef<Path> for StateDir { .. }                   // no Deref: call sites say .path()

impl StateDir {
    // construction (bodies: project_dir / project_dir_under / ensure_project_dir[_under])
    pub fn of(root: &Path) -> Result<Self>;
    pub(crate) fn of_under(home: &Path, root: &Path) -> Result<Self>;
    pub fn ensure(root: &Path) -> Result<Self>;
    pub(crate) fn ensure_under(home: &Path, root: &Path) -> Result<Self>;
    /// A directory already known (listing `projects/`), whose root may be gone.
    pub fn at(path: impl Into<PathBuf>) -> Self;
    pub fn path(&self) -> &Path;
    /// The final component (the project hash): the Windows pipe name.
    pub fn id(&self) -> Option<&str>;

    // files
    pub fn index_db(&self) -> PathBuf;
    pub fn config_file(&self) -> PathBuf;
    pub fn daemon_log_file(&self) -> PathBuf;
    #[cfg(unix)] pub fn socket_file(&self) -> PathBuf;
    pub fn pid_file(&self) -> PathBuf;
    pub fn plugin_pid_file(&self, language: &str) -> PathBuf;
    pub fn plugin_suspended_marker(&self, language: &str) -> PathBuf;
    pub fn build_stamp_file(&self) -> PathBuf;
    pub fn bootstrap_lock_file(&self) -> PathBuf;
    pub fn daemon_lock_file(&self) -> PathBuf;
    pub fn rebuild_marker_file(&self) -> PathBuf;
    pub fn phase_file(&self) -> PathBuf;
    pub fn progress_file(&self) -> PathBuf;
    pub fn rerank_outcome_file(&self) -> PathBuf;
    pub fn seed_tmp_file(&self) -> PathBuf;
    pub fn staging_db(&self, language: &str) -> PathBuf;
    pub fn is_staging_db(file_name: &str) -> bool;
    fn serving_file(&self) -> PathBuf;                 // private: only the three methods below

    // listings (bodies: registry.rs:137-152, 182-197, 214-240)
    pub fn plugin_pid_files(&self) -> Vec<(String, PathBuf)>;
    pub fn suspended_markers(&self) -> Vec<(String, String)>;
    pub fn clear_suspension_markers(&self);

    // root record (bodies: read_project_root / read_root_device; free fns stay as wrappers)
    pub fn project_root(&self) -> Option<PathBuf>;
    pub fn root_device(&self) -> Option<u64>;

    // probes and records
    /// daemon/mod.rs:971-985 (Free) and cli/clean.rs:516-524 (Held), one body:
    ///   open fails NotFound          -> Ok(false)          (both)
    ///   open fails otherwise         -> Free: Ok(false)    Held: Ok(true)
    ///   try_lock Ok                  -> Ok(false)          (both; the probe's lock drops)
    ///   try_lock WouldBlock          -> Ok(true)           (both)
    ///   try_lock Error               -> Free: Err(..)      Held: Ok(true)
    /// Held therefore never returns Err.
    pub fn daemon_lock_held(&self, on_unreadable: OnUnreadable) -> Result<bool>;
    pub fn serving_owner(&self) -> Option<u32>;                 // daemon/mod.rs:1022-1025
    pub(crate) fn record_serving_owner(&self, pid: u32);        // :993-995, pid passed in
    pub(crate) fn clear_serving_owner(&self);                   // :1011-1018
    pub fn read_phase(&self) -> Option<String>;                 // :165-167
    pub fn rebuild_marker(&self) -> Option<RebuildMarker>;      // parse half of :238-255
    pub(crate) fn write_rebuild_marker(&self, command: &str) -> Result<()>; // :224-228
    pub(crate) fn clear_stale_rebuild_marker(&self);            // :1000-1007
}

pub struct RebuildMarker { pub pid: u32, pub started_unix_secs: u64, pub command: String }

// pid file format (bodies: daemon/mod.rs:257-277), free fns: they also read files outside a StateDir
pub fn read_pid_file(path: &Path) -> Option<u32>;
pub fn read_pid_file_result(path: &Path) -> std::io::Result<Option<u32>>;
pub fn write_pid_file(path: &Path, pid: u32);

// home-level files
pub fn global_config_file(home: &Path) -> PathBuf;   // config/mod.rs:229-231
pub fn orphan_sweep_stamp(home: &Path) -> PathBuf;   // cli/clean.rs:590
```

Lock-file open mode: the two bodies differ today (`write(true).truncate(false)` in daemon,
`read(true).write(true)` in clean). The merged body uses `write(true).truncate(false)`; neither
creates the file, and a write handle is enough for `flock`/`LockFileEx`. Accepted as the one
non-verbatim detail; S4 checks it.

What stays where it is, taking `&StateDir`:

| function | stays in | why |
|---|---|---|
| `read_progress_in` (`daemon/mod.rs:158-161`) | moves to `indexing_status::read_progress(&StateDir)` | `ProgressSnapshot` + serde_json live there; `indexing_status -> paths` exists, `paths -> indexing_status` would be a cycle |
| `is_process_alive` (`:281-283`) | `daemon` (wrapper); `control` calls `crate::process::is_alive` directly | `paths` stays off `process` |
| `rebuild_in_progress[_in]` (`:234-255`) | `daemon`, over `rebuild_marker()` + `process::is_alive` + `daemon_lock_held(Free)` | liveness judgement |
| `acquire_singleton_lock` (`:896-927`) | `daemon`, opening `daemon_lock_file()`, calling `clear_serving_owner()` | only the daemon and `cli::reindex` take it |
| `DaemonLock`, `inspect_daemon_lock[_in]` (`:930-969`) | `control::lock` | section 1 |
| `endpoint`, `endpoint_in`, `is_listening` (`:93-111, 287-289`) | `ipc::endpoint_for` + `control::lock::is_listening(&StateDir)` | section 1 |

Compat: integration tests use the old free functions heavily (`daemon::pid_path` 61 refs,
`endpoint` 35, `plugin_pid_path` 30, `read_pid_file` 27, `is_process_alive` 23; nearly all in
`core/tests/it`). As in GM-705, production moves and the old names stay as one-line wrappers or
`pub use` for this release: `daemon::{endpoint, endpoint_in, socket_path, pid_path,
plugin_pid_path, pid_path_in, plugin_pid_path_in, phase_path_in, rerank_outcome_path_in,
progress_path_in, read_progress_in, read_phase_in, build_stamp_path[_in], lock_path,
rebuild_marker_path_in, read_pid_file[_result], write_pid_file, is_process_alive,
is_listening, inspect_daemon_lock, DaemonLock, serving_owner_in}`,
`registry::{discovered_pid_files, discovered_suspended_markers}`, `shim::DAEMON_LOG_FILE`,
`config::{project_config_path, global_config_path}`. A wrapper builds its path through
`StateDir`, so no file name is spelled outside `paths::state_dir`. `daemon_lock_path` has 0
references (`find_references`): deleted, not wrapped. Migrating the ~60 test files and
deleting the wrappers is a follow-up task (D2).

## 3. Edit map

### 3.1 R-5 (S2): move, logic unchanged

`control/stop.rs` from `cli/stop.rs`:

| item | lines now | note |
|---|---|---|
| module doc | 1-45 | moves whole; `cli/stop.rs` keeps a 2-line doc pointing at `control::stop` |
| `TERMINATION_TIMEOUT`, `PLUGIN_EXIT_TIMEOUT`, `POLL_INTERVAL` | 58-67 | `TERMINATION_TIMEOUT` stays `pub(crate)` |
| `Stopped` + `impl` | 69-86 | `Stopped::pid` becomes `pub(crate)` (cli's `render` calls it) |
| `Outcome` + `impl` | 88-112 | |
| `stop` | 121-176 | `pub` |
| `live_pid`, `terminate`, `terminate_daemon`, `wait_until_group_gone`, `wait_until_gone`, `tidy_state_files` | 177-319 | `terminate_daemon` stays `pub(crate)` |
| tests: `mod signals` + `a_missing_or_unreadable_pid_file_reads_as_nothing_running` | 361-487 | |

Stays in `cli/stop.rs`: `run` (114-119), `render` (320-341), `describe` (343-349), the four
`render` tests (489-547). Imports `crate::control::stop::{self, Outcome, Stopped}`.

`control/purge.rs` from `cli/clean.rs`:

| item | lines now |
|---|---|
| `RootState` (doc from 237) | 237-274 |
| `root_state` | 276-306 |
| `Purge`, `LockWait`, `LOCK_POLL_INTERVAL` | 345-373 |
| `purge_orphaned_state_dir` | 375-473 |
| `delete_root_record_last`, `singleton_lock_is_held` | 475-523 |
| `daemon_is_running` (`pub(crate)`) | 698-723 |
| `delete` (`pub(crate)`) | 725-727 |

`control/sweep.rs` from `cli/clean.rs`: `sweep_orphaned` (525-558), `ORPHAN_SWEEP_INTERVAL`
(560-564), `ORPHAN_SWEEP_MARKER` (566-568, until S3 moves the name), `Sweep` (570-579),
`sweep_orphaned_if_due` (581-619), `validate_id` (621-634, `pub(crate)`: `clean_one` uses it).

Stays in `cli/clean.rs` (command wiring): module doc 1-39, `idle_threshold`, `Target`, `Scope`,
`Outcome`, `Orphan`, `run`, `clean`, `CwdScoped`, `cwd_project_id`, `clean_one`, `clean_many`,
`clean_orphaned`, `Candidate`, `candidates`, `render`. `candidates` stays: it reads
`gc::last_used`, which only the bulk `clean` forms need.

Tests in `cli/clean.rs` (843-1927) split by one rule: **a test that calls `clean`/`clean_in*`/
`render` stays; every other test moves** to the module of the function it calls. Applied:

| stays in cli/clean.rs | moves to control/purge.rs | moves to control/sweep.rs |
|---|---|---|
| everything before 1204; `orphaned_deletes_only_...` (1264), `orphaned_without_force_...` (1286), `orphaned_on_a_root_with_nothing_...` (1326), `the_orphaned_report_...` (1348), `a_single_deletion_...` (1372), `the_sweep_keeps_state_whose_root_is_only_unreadable` (1560: calls both `sweep_orphaned` and `clean_in`) | `root_state_tells_...` (1303), `root_state_keeps_a_root_whose_parent_cannot_be_searched` (1539), `root_state_keeps_a_root_whose_parent_is_on_another_device` (1605), `a_root_recorded_without_a_device_is_not_judged` (1645), the bootstrap-lock and singleton-lock sections (1748-1927) with `hold_bootstrap_lock`, `purge_in_background`, `hold_singleton_lock` | `the_sweep_deletes_only_...` (1393), `the_sweep_keeps_state_whose_daemon_lock_is_held` (1407), `the_sweep_runs_at_most_once_per_interval` (1461), `the_sweep_steps_aside_...` (1482), `the_sweep_keeps_state_of_a_project_on_an_unmounted_volume` (1619), `an_interrupted_purge_is_finished_by_the_next_sweep` (1668), `a_project_on_a_detached_disk_image_is_kept` (1701) |

Shared fixtures (`Projects` 847-892, `orphan_fixture` 1206-1262, `device_of`, `RestoreMode`,
`make_unsearchable`, `record_then_unmount` 1492-1604) move once to
`control/test_fixture.rs` (`#[cfg(test)] pub(crate) mod test_fixture;` in `control.rs`) and
all three test modules import it. Fixtures move verbatim; nothing is duplicated.

Callers to repoint (`find_references`):

| caller | now | after |
|---|---|---|
| `daemon/lifecycle.rs:1015-1022` `purge_state_dir` | `crate::cli::clean::{LockWait, purge_orphaned_state_dir, Purge}` | `crate::control::purge::…`; doc at `:991` too |
| `shim.rs:15` | `use crate::cli::stop;` | `use crate::control::stop;` (`:378` `stop::stop`, `:458` `stop::terminate_daemon`, `stop::TERMINATION_TIMEOUT` unchanged) |
| `shim.rs:152` `spawn_orphan_sweep` | `crate::cli::clean::{sweep_orphaned_if_due, Sweep, ORPHAN_SWEEP_INTERVAL}` | `crate::control::sweep::…`; docs at `:141-142`, `:362` |
| `cli/reindex.rs:51,186` `stop_for_rebuild` | `crate::cli::stop` | `crate::control::stop` (`stop::Outcome` in its return type) |
| `cli/clean.rs` internals | — | `use crate::control::{purge::{self, root_state, RootState}, sweep::validate_id}` |

`gc` docs (same change): `gc/mod.rs:5-7` becomes

```
//! Nothing in this module deletes anything: it maintains and reads the facts a
//! deletion is decided from. An explicit deletion is `g-mesh clean` (`cli::clean`);
//! the unattended one, of state whose project directory is gone, is the daemon's own
//! purge and the shim's startup sweep (`control::purge`, `control::sweep`).
```

and `gc/warning.rs:6-8` ("`cli::clean` is the only place a deletion ever happens, and only
when a human explicitly asks for one") becomes "deletion is `cli::clean` when a human asks, and
`control::{purge, sweep}` for orphaned state; never this module". Doc mentions of
`cli::clean::purge_orphaned_state_dir` / `cli::clean::sweep_*` / `cli::stop::stop` elsewhere
(`daemon/lifecycle.rs:991`, `shim.rs:141-142,362`) are repointed; plain `cli::stop`/`cli::status`
mentions of the commands stay.

### 3.2 R-4 step 2 (S3): `StateDir`, and `control` off `daemon`

Definitions removed (become `StateDir`/`state_dir` bodies or wrappers, section 2):
`daemon/mod.rs:51-84` (10 consts), `:93-289` (helpers), `:930-1025` (lock/serving probes);
`daemon/registry.rs:118-120, 137-152, 156-158, 182-197, 214-240`; `shim.rs:83`;
`config/mod.rs:56, 224-231`; `cli/clean.rs` → `control/sweep.rs` `ORPHAN_SWEEP_MARKER`;
`daemon/seed.rs:30`; `daemon/workspace_reindex.rs:101-102`;
`paths/state_dir.rs:119` (`"projects"`).

Production call sites (production only; grep after the g-mesh `find_references` file lists,
section 8, to get lines):

| file | lines | change |
|---|---|---|
| `daemon/mod.rs` `run` | 299, 303, 310, 316, 417-424, 432-433, 446-447, 453, 464-465 | `let dir = StateDir::ensure(root)?`; `dir.build_stamp_file()`, `ipc::endpoint_for(&dir)`, `dir.clear_suspension_markers()`, `write_pid_file(&dir.pid_file(), ..)`, `dir.phase_file()`, `dir.progress_file()`, `dir.record_serving_owner(std::process::id())`, `dir.rerank_outcome_file()`, `dir.clear_stale_rebuild_marker()` |
| `daemon/mod.rs` `acquire_singleton_lock` | 896-927 | takes `&StateDir`; `dir.daemon_lock_file()`, `dir.clear_serving_owner()` |
| `daemon/front.rs` | 30, 62-69 | same as `run` |
| `daemon/lifecycle.rs` | 262, 371, 475, 516 | `state_dir::write_pid_file` |
| `daemon/lifecycle.rs` `suspended_marker_path` | 663-669 | `StateDir::at(parent).plugin_suspended_marker(lang)` |
| `daemon/lifecycle.rs` `release_state_files` | 1046-1062 | `sd.plugin_pid_files()`, `sd.build_stamp_file()`, `sd.phase_file()`, `sd.progress_file()`, `sd.rerank_outcome_file()`, `sd.pid_file()`, `ipc::endpoint_for(&sd)` |
| `daemon/registry.rs` `pid_file_for` | 1779-1781 | `StateDir::at(&self.state_dir).plugin_pid_file(language)` (or store a `StateDir`) |
| `daemon/semantic.rs` | 156, 534, 549 | `plugin_pid_file`, `write_pid_file` |
| `daemon/plugin.rs` | 1616 | `state_dir::write_pid_file` |
| `daemon/seed.rs` | 197, 234, 239, 300, 338, 346, 358 | `seed_tmp_file()`, `index_db()`; the `-wal/-shm/-journal` suffixes stay local (SQLite's names, appended to `index_db()`) |
| `daemon/workspace_reindex.rs` | 106, 133 | `staging_db(lang)`, `StateDir::is_staging_db(name)` |
| `storage/connection.rs` | 62 | `index_db()` |
| `project_detect.rs` | 216 | `index_db()` |
| `gc/last_used.rs` | 106 | `index_db()` |
| `config/mod.rs` | 54, 224-231, 251 | `StateDir::of(root)?.config_file()`, `global_config_file(&home)`, `StateDir::ensure` |
| `shim.rs` | 168, 247, 346, 437-467, 485, 629-630 | `rebuild_in_progress`, `endpoint_for`, `build_stamp_file`, `control::lock::{DaemonLock, inspect_daemon_lock}`, `StateDir::ensure_under(home, root)?.bootstrap_lock_file()`, `daemon_log_file()` |
| `control/stop.rs` (ex `cli/stop.rs`) | 137-150, 180, 266, 283-315 | `sd.pid_file()`, `control::lock::inspect_daemon_lock`, `sd.plugin_pid_files()`, `state_dir::read_pid_file` + `process::is_alive`, `control::lock::is_listening(&sd)`, `ipc::endpoint_for(&sd)`, `sd.build_stamp_file()`, `sd.clear_suspension_markers()` |
| `control/purge.rs` (ex `cli/clean.rs`) | 405-417, 433, 486-500, 516-524, 709-712 | `sd.project_root()`, `sd.root_device()`, `sd.daemon_lock_held(OnUnreadable::Held)`, `sd.bootstrap_lock_file()`, `ROOT_FILE`/`DEVICE_FILE`, `sd.pid_file()`, `sd.plugin_pid_files()`, `read_pid_file_result`, `process::is_alive` |
| `control/sweep.rs` (ex `cli/clean.rs`) | 590, 618 | `orphan_sweep_stamp(home)`, `projects_root_under` |
| `cli/status.rs` | 356, 405, 430, 468, 545, 551, 556, 575, 588-601 | `rerank_outcome_file`, `read_phase`, `index_db`, `indexing_status::read_progress`, `pid_file`, `control::lock::*`, `build_stamp_file`, `plugin_pid_files`, `suspended_markers` |
| `cli/deps.rs` | 100, 170, 266-267, 337 | `index_db`, `daemon_log_file`, `read_phase`, `read_progress` |
| `cli/reindex.rs` | 180-187 | `StateDir::of`, `sd.write_rebuild_marker(command)` (the `MarkerFile` guard keeps `sd.rebuild_marker_file()`), `acquire_singleton_lock(&sd)` |
| `cli/config_wizard.rs`, `cli/init.rs` | 54, 61; 170 | compat wrappers are enough; repoint for consistency |
| `control/lock.rs` (ex `daemon/mod.rs`) | 930-985 | `inspect_daemon_lock_in(&StateDir, listening)` uses `sd.daemon_lock_held(OnUnreadable::Free)?`, `sd.serving_owner()`, `process::is_alive` |
| `daemon/tests.rs` | 135-251 | `crate::control::lock::inspect_daemon_lock_in`, `sd.record_serving_owner(..)`, `sd.serving_owner()` (test-only `daemon -> control`, fine) |

S3 exit check: `python3 scripts/module-cycles.py --absent 'control->daemon' --absent
'paths->process' --absent 'daemon->cli' --absent 'shim->cli'`, and a grep that the file-name
literals in section 4 appear only in `paths/state_dir.rs` (non-test).

## 4. File names and their single home

All in `core/src/paths/state_dir.rs` after S3. "Now" is every non-test definition today.

| name | defined now | home |
|---|---|---|
| `projects` | `paths/state_dir.rs:119` (literal) | `PROJECTS_DIR` |
| `project.root`, `project.dev` | `paths/state_dir.rs:38,46` | unchanged |
| `index.db` | literals: `storage/connection.rs:62`, `project_detect.rs:216`, `cli/deps.rs:100`, `cli/status.rs:430`, `daemon/seed.rs:234,239,346`, `gc/last_used.rs:106` | `INDEX_DB_FILE` |
| `index.db.seed-tmp` | `daemon/seed.rs:30` | `SEED_TMP_FILE` |
| `staging-<lang>.db` | `daemon/workspace_reindex.rs:101-102` | private consts + `staging_db`/`is_staging_db` |
| `config.toml` | `config/mod.rs:56` | `CONFIG_FILE` |
| `daemon.log` | `shim.rs:83` | `DAEMON_LOG_FILE` (`shim::DAEMON_LOG_FILE` re-exported) |
| `daemon.sock`, `daemon.pid`, `daemon.build`, `bootstrap.lock`, `daemon.lock`, `reindex.pid`, `daemon.serving`, `index.phase`, `index.progress`, `rerank.outcome` | `daemon/mod.rs:52-84` | same names |
| `plugin-<lang>.pid`, `plugin-<lang>.suspended` | `daemon/registry.rs:119,143,157,188,231` | private prefix/suffix consts |
| `orphan-sweep.stamp` | `cli/clean.rs:568` | `ORPHAN_SWEEP_STAMP` |

Not state files, left alone: `plugin.toml` (plugin bundles), `tokenizer.json` (model dir),
the SQLite `-wal/-shm/-journal` suffixes, test fixtures. Including `index.db`, seed-tmp and
staging goes beyond the review's list; the acceptance says "every", so it is in (D1).

## 5. Lock polarity

| call site (after) | now | unreadable reads as | why (quoted) |
|---|---|---|---|
| `control::lock::inspect_daemon_lock_in` → `sd.daemon_lock_held(OnUnreadable::Free)?` | `daemon/mod.rs:960` via `daemon_lock_is_held` `:971-985` | **Free** (open failure); a `try_lock` error propagates | "A lock file that cannot be opened reads as 'not held': eviction must fail towards leaving processes alone." |
| `daemon::rebuild_in_progress_in` → `sd.daemon_lock_held(OnUnreadable::Free)?` | `daemon/mod.rs:251` | **Free**: no rebuild, the shim may bootstrap | same function today; a marker counts only while the lock is held |
| `control::purge` judge → `sd.daemon_lock_held(OnUnreadable::Held).unwrap_or(true)` | `cli/clean.rs:417` via `singleton_lock_is_held` `:516-524` | **Held**: keep the directory | "A lock that cannot be checked counts as held, for the reason `daemon_is_running` gives for an unreadable pid file." |

The `.unwrap_or(true)` is dead (Held never errs) and kept only so the judge closure stays
`Option<Purge>`; it reads as the polarity a second time.

Downstream of the probe, at the `DaemonLock` level (unchanged, listed so S4 can check them):
`shim.rs:465-467` `lock_state`: `inspect_daemon_lock(..).unwrap_or(DaemonLock::Free)`, Free,
"the direction that leaves processes alone"; `cli/status.rs:551` and `control/stop.rs` (now
`cli/stop.rs:139`): `?`, the error is reported.

Pid files have the same split, named by function rather than by enum: `read_pid_file`
(unreadable = nothing recorded: `control/stop.rs` `live_pid`, `cli/status.rs:545,591`) vs
`read_pid_file_result` (unreadable = running, logged: `control::purge::daemon_is_running`,
GM-247). Folding them into one `OnUnreadable` reader would drop `daemon_is_running`'s message;
not proposed.

## 6. Projected cycle-checker result

After S2 only (control still imports daemon):

```
python3 scripts/module-cycles.py --drop 'daemon->cli' --drop 'shim->cli' \
  --add 'daemon->control' --add 'shim->control' --add 'cli->control' \
  --add 'control->daemon' --add 'control->paths' --add 'control->process'
SCC (4): control, daemon, mcp, watcher
SCC (3): embedding, graph, storage
```

After S3 (the task's end state):

```
python3 scripts/module-cycles.py --drop 'daemon->cli' --drop 'shim->cli' \
  --add 'daemon->control' --add 'shim->control' --add 'cli->control' \
  --add 'control->paths' --add 'control->process' --add 'control->ipc' --add 'ipc->paths'
SCC (3): daemon, mcp, watcher
SCC (3): embedding, graph, storage
```

`cli` and `shim` leave the component (nothing reaches them once `daemon->cli` and `shim->cli`
are gone). No new cycle: `control` and `ipc` reach only leaves.

What keeps the two that remain, and who owns each edge:

| edge | sites | owner |
|---|---|---|
| watcher->daemon | 1/0, `watcher/apply.rs:422` (`config_reindex::select_with`) | GM-707 (R-15) |
| mcp->daemon | 10/0: `daemon::registry::PathCoverage` (8 files), `PluginRegistry` (`mcp/mod.rs:27`), `daemon::lifecycle::CoreActivity` (`mcp/mod.rs:26`, `mcp/front.rs:32`) | GM-709 (R-17). Gap: its acceptance names only `PluginRegistry`/`daemon::registry`; `CoreActivity` (`front.rs:32`) would keep `mcp->daemon` alive (D6) |
| daemon->mcp 2/2, daemon->watcher 17/1 | the daemon serving and watching: downward, intended | stay |
| graph<->storage, embedding<->storage | | GM-708 (R-16) |
| `daemon/semantic.rs` tests → `cli::status` (test-only) | | GM-708 (R-32) |

With GM-707 and GM-709 (gap closed) the first component is gone; with GM-708 the second. The
backlog crate-decomposition design (GM-718) depends on GM-706 and re-runs the checker then.

## 7. Slicing check

| slice | model | verdict |
|---|---|---|
| S2 R-5 move | sonnet | Mechanical with this map: ranges in 3.1, the one-line test rule, the shared fixture module, verbatim gc text. Two non-verbatim edits, both named: `Stopped::pid` to `pub(crate)`, imports. Exit adds `module-cycles.py --absent 'daemon->cli' --absent 'shim->cli'`. Run only `cli::clean`, `cli::stop`, `control::` lib tests and `daemon::tests`. |
| S3 StateDir | opus | Larger than the tracker's brief: also `control/lock.rs` (DaemonLock + probes out of daemon) and `ipc::endpoint_for`, required for `control` not to join the SCC (section 6). Drop "config stops importing storage" from its brief (done in GM-705). ~70 production edits in 20 files plus compat wrappers; one opus slice is still right (one design, one diff for S4). |
| S4 review | opus | Unchanged; add: lock open mode (section 2), compat wrappers spell no names, `control` imports only leaves. |
| S5 tests | opus | Polarity test per `OnUnreadable` arm (an unopenable `daemon.lock`: Free → false, Held → true; Unix, skip as root), a source-scan test that the section 4 literals occur only in `paths/state_dir.rs`, `StateDir` paths equal the compat wrappers. Controls: swap the arm at the purge site, swap it at `inspect_daemon_lock_in`, re-add a literal elsewhere. |
| S6 verify | opus | Exit: `--max-scc 3`, `--absent 'X->cli'` for every X, `--absent 'control->daemon'`. Suite: `scripts/test-local.sh --base release-4.31.0` (core sections only; no plugin code changes). |

## 8. g-mesh calls behind this note (project `g-mesh-wt-gm706`)

- `find_references cli::clean::purge_orphaned_state_dir` → `cli/clean.rs` (sweep + 5 tests), `daemon/lifecycle.rs` `purge_state_dir`.
- `find_references cli::clean::sweep_orphaned_if_due` → `shim::spawn_orphan_sweep` only.
- `find_references cli::stop::stop` → `cli::stop::run`, `cli::reindex::stop_for_rebuild`, `shim::retire_outdated_daemon`.
- `find_references cli::stop::terminate_daemon` → `cli::stop::stop`, `shim::evict_wedged_daemon`.
- `find_references` `cli::clean::{root_state, validate_id, daemon_is_running, delete}` → all within `cli/clean.rs` (callers listed in 3.1).
- `find_references ... answer=files` for `daemon::{pid_path, pid_path_in, plugin_pid_path, plugin_pid_path_in, phase_path_in, rerank_outcome_path_in, progress_path_in, read_progress_in, read_phase_in, build_stamp_path, build_stamp_path_in, lock_path, bootstrap_lock_path_in, lock_path_under, daemon_lock_path (0), daemon_lock_path_in, rebuild_marker_path_in, socket_path, endpoint, endpoint_in, read_pid_file, read_pid_file_result, write_pid_file, is_process_alive, is_listening, inspect_daemon_lock, rebuild_in_progress, serving_owner_in, write_rebuild_marker, acquire_singleton_lock}`, `registry::{plugin_pid_file_name, discovered_pid_files, plugin_suspended_marker_file_name, discovered_suspended_markers, clear_stale_suspension_markers}`, `shim::DAEMON_LOG_FILE`, `config::CONFIG_FILE_NAME`: the file lists behind sections 2 and 3.2. Lines then by grep in those files, cut at each file's first `#[cfg(test)]` (`daemon/mod.rs` by hand: its first one is the `test_plugin` module at `:21`).
- `scripts/module-cycles.py --edges` and the two projections in section 6.
