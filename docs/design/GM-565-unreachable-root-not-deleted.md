# GM-565: an unreachable project root is not a deleted one

Status: design (S1). Found by GM-524/S13 verify (findings F2, F3).
Background: `docs/design/GM-524-worktree-daemon-lifecycle.md`, D1 (c).

## Problem

GM-524 D1 (c) made g-mesh delete a project's state directory
(`~/.g-mesh/projects/<hash>/`, index and all) without being asked, whenever
its recorded root reads as deleted. Three paths do that, and all of them reach
one judgement, `cli::clean::root_state` (`core/src/cli/clean.rs:268-280`):

```rust
if root.exists() { return RootState::Live; }
match root.parent() {
    Some(parent) if parent.exists() => RootState::Orphaned,   // -> deleted
    _ => RootState::Unreachable,                              // -> kept
}
```

`Path::exists()` is `false` for *every* `stat` failure, not only "no such
file". Two real situations therefore read as `Orphaned`:

- **F2, permission denied.** `stat(root)` needs search permission on every
  ancestor. If the root's parent loses it (`chmod 000 ~/work`, an ACL change,
  a sandbox), `stat(root)` fails with `EACCES` while `stat(parent)` still
  succeeds. Verified on this machine (macOS 26): `os.stat('p/root')` gives
  errno 13 and `os.path.exists('p')` is `True`. Today: `Orphaned`, and the
  next sweep deletes the index of a project that is still on disk.
- **F3, unmounted volume.** The parent-present rule assumes "parent present
  means the filesystem is mounted". Two layouts break it:
  - macOS, root *is* the volume: `/Volumes/Ext` disappears on eject, its
    parent `/Volumes` stays. Today: `Orphaned`.
  - Linux (and macFUSE/sshfs on macOS), root one level *inside* a mount point:
    `/mnt/ext/proj` disappears, the empty mount-point directory `/mnt/ext`
    stays. Today: `Orphaned`.
  (A Linux root that is itself the mount point, `/mnt/ext`, stays as an empty
  directory and reads `Live`: never deleted, no bug.)

Cost when it fires: a cold reindex of that project (minutes, ~100 MB of work),
not source data loss.

## Every site that decides "root gone" (g-mesh, project `g-mesh`)

| Site | Role | Found by |
|---|---|---|
| `cli::clean::root_state` (`clean.rs:268-280`) | the only classifier of a *recorded* root | definition |
| `cli::clean::clean_orphaned` (`clean.rs:285-319`) | `g-mesh clean orphaned [--force]` | `find_callers root_state` |
| `cli::clean::purge_orphaned_state_dir` (`clean.rs:344-379`) | judge + delete under the bootstrap lock (judged twice) | `find_callers root_state` |
| `cli::clean::sweep_orphaned` (`clean.rs:436-460`) | shim startup sweep, via `sweep_orphaned_if_due` <- `shim::spawn_orphan_sweep` (`shim.rs:141`) | `find_callers purge_orphaned_state_dir`, `find_callers sweep_orphaned_if_due` |
| `daemon::lifecycle::purge_state_dir` (`lifecycle.rs:941-984`) | daemon's own purge on `ProjectRootGone`, called by `daemon::run` (`daemon/mod.rs:294`) and `daemon::front::run` (`front.rs:57`) | `find_callers purge_orphaned_state_dir`, `find_callers purge_state_dir` |
| `daemon::lifecycle::orphan_check` / `is_definitely_gone` (`lifecycle.rs:784-823`) | decides the daemon *exits* (`ProjectRootGone`); already `NotFound`-only; caller `supervise` only | `find_callers orphan_check`, `find_callers is_definitely_gone` |
| `daemon::identity::record_project_root` (`identity.rs:42-49`) | writes `project.root`; called by `storage::connection::ensure_project_dir_under` (`connection.rs:70`) on every open, plus test helpers (`clean.rs:786`, `clean.rs:1324`, `tests/worktree_daemon_lifecycle.rs:485`) | `find_callers record_project_root` |
| `daemon::identity::read_project_root` (`identity.rs:59-66`) | reads it; callers `purge_orphaned_state_dir`, `candidates` (`clean.rs:560`) | `find_callers read_project_root` |

`RootState` is referenced only inside `clean.rs` (`find_references RootState`,
1 file). So the fix lives in `root_state` plus the record it reads; every
deleting path inherits it. `orphan_check` needs no change: exiting the daemon
for an unmounted root is right (it cannot serve), and the purge that follows
goes through `root_state`.

## Decisions

### D1. Which `stat` result counts as "gone" (F2)

- **Today:** `root.exists()` / `parent.exists()`; any error on the root with a
  present parent means deleted.
- **(a) Only `NotFound` on the root, and `Ok` + directory on the parent
  (Recommended).** `fs::metadata(root)`: `Ok` -> `Live`; `Err(NotFound)` ->
  look at the parent; any other error (`PermissionDenied`, `NotADirectory`,
  I/O, a stale NFS handle) -> `Unreachable`. Parent: `fs::metadata(parent)`
  must be `Ok` and `is_dir()`, otherwise `Unreachable`. Same rule
  `is_definitely_gone` already applies to the daemon's exit.
  - Example: `~/work` set to `000`, project `~/work/app` -> `Unreachable`,
    listed by `clean orphaned` under "left alone", never swept.
  - Benefit: closes F2 completely; one function, no format change.
  - Risk: a project that really was deleted *and* sits under a parent that is
    now unsearchable is kept until `clean <id>`/`clean expired`. That is a leak
    of one index, the safe direction.
- (b) Keep `exists()`, add a separate permission probe. Benefit: none over (a).
  Risk: two stats racing each other, and every other error kind still reads as
  deleted.

### D2. Telling an unmounted volume from a deleted directory (F3)

- **Today:** parent present => same filesystem still mounted. False for both
  layouts above.
- **(a) Record the root's device id (`st_dev`) at index time; a missing root
  counts as deleted only if its parent is now on that same device
  (Recommended).** `record_project_root` (already re-run on every open of the
  index) also writes `project.dev` beside `project.root`, holding
  `metadata(canonical_root).dev()`. `root_state` then requires
  `metadata(parent).dev() == recorded_dev` for `Orphaned`, else `Unreachable`.
  - Why one number covers both layouts: a deleted directory leaves its parent
    on the root's own filesystem (dev equal). macOS `/Volumes/Ext`: the root's
    recorded dev is the external disk, `/Volumes` is on the data volume
    (measured here: `/Volumes`, `/`, `$HOME` and `$TMPDIR` all report
    16777223) -> not equal -> `Unreachable`. Linux `/mnt/ext/proj`: recorded
    dev is the volume; after unmount `/mnt/ext` is on the root fs -> not equal
    -> `Unreachable`. A mount point itself cannot be `rmdir`ed while mounted
    (`EBUSY`), so a volume root that vanished was unmounted, never deleted.
  - Example: index `/Volumes/Ext` (dev 16777240), eject; sweep sees root
    `NotFound`, `/Volumes` dev 16777223 != 16777240 -> kept, reported as
    unreachable.
  - Benefit: exact for every mount layout, Linux and macOS, with no path
    lists; costs one extra `stat` per orphan candidate (live roots are decided
    by the first `stat` and never reach it).
  - Risk 1: device ids are not stable across reboots on some filesystems
    (tmpfs, btrfs subvolumes, overlayfs, external disks enumerated in a
    different order). A project deleted after a reboot, and not opened since
    the reboot, then reads `Unreachable` and is kept: a leak, not a loss.
    The main path (the daemon purging its own state right after its root
    goes) always runs in the boot that wrote the dev, because the dev is
    rewritten at every open.
  - Risk 2: a new file in the state dir. Chosen over a second line in
    `project.root` on purpose: an older g-mesh (an installed copy, an older
    worktree build) reads the whole file as the path; `"/a/b\ndev=1"` would
    give parent `/a`, which exists -> `Orphaned` -> an old binary deletes a
    *live* project. A sidecar file is invisible to old readers.
- (b) Mount-path heuristics: treat parents named `/Volumes`, `/media/*`,
  `/run/media/*`, `/mnt` as "maybe unmounted". Benefit: no new state. Risk:
  misses every user-chosen mount point (`~/mnt/x`, sshfs, NFS automounts) and
  keeps real deletions under `/mnt`.
- (c) Judge only at check time: is the parent a mount point (dev(parent) !=
  dev(grandparent))? Benefit: no recorded state. Risk: wrong for both target
  cases: `/Volumes` is not a mount point, and an unmounted `/mnt/ext` no
  longer is one.
- (d) Document the residual case only. Benefit: no code. Risk: acceptance
  criterion 2 unmet; ejecting a disk keeps costing a cold reindex.

### D3. A recorded root with no recorded device

State dirs written by g-mesh 4.7-4.9 have `project.root` but no `project.dev`.
Every one that is opened again gains it (the record is rewritten on open), so
the population is: projects not opened since the upgrade, plus orphans the
6-hourly sweep has not already deleted.

- **Today:** n/a.
- **(a) Unix: no device => `Unreachable`; Windows: the parent rule of D1
  (Recommended).** Std has no stable `st_dev` on Windows, and there a vanished
  volume takes the drive root with it (`D:\proj` -> `D:\` gone ->
  `Unreachable` already), so the parent rule is safe there.
  - Example: a 4.8-era orphan whose root was deleted -> kept, listed under
    "left alone" by `clean orphaned`; `clean <id>` or `clean expired` removes it.
  - Benefit: no path can delete without the evidence D2 relies on; F3 is
    closed for old records too (an external disk indexed by 4.8 and ejected
    before the upgrade is exactly that case).
  - Risk: leftover pre-upgrade orphans are no longer swept automatically; they
    wait for `clean expired`/`clean <id>`. Most were already swept by 4.7-4.9.
- (b) No device => the D1 parent rule (old behaviour minus F2). Benefit:
  pre-upgrade orphans keep being swept. Risk: keeps F3 for every record not
  reopened since the upgrade.

### D4. Deletion order with the new file

`delete_root_record_last` (`clean.rs:390-410`) keeps `project.root` for last
so an interrupted purge is finished by the next sweep. With D3 (a), a dir left
with `project.root` but without `project.dev` would read `Unreachable` and
stay. New order: every file except `project.root` and `project.dev`, then
`project.root`, then `project.dev`, then the directory. An interruption
between the last two leaves a dir with only `project.dev` (a few bytes, index
already gone) that reads `Legacy` and is kept: accepted, documented in the
function. No owner decision needed unless you want it reported by `clean`.

## Edit map

| File | Function / item | Lines | Change |
|---|---|---|---|
| `core/src/daemon/identity.rs` | new `pub const DEVICE_FILE: &str = "project.dev"` | after 30 | |
| `core/src/daemon/identity.rs` | `record_project_root` | 42-49 | after writing the path, `#[cfg(unix)]` write `metadata(&canonical)?.dev()` to `DEVICE_FILE`; doc: why a sidecar (D2 risk 2) |
| `core/src/daemon/identity.rs` | new `read_root_device(state_dir) -> Option<u64>` | after 66 | unreadable/unparsable -> `None` (same posture as `read_project_root`) |
| `core/src/cli/clean.rs` | `RootState` doc | 236-266 | replace the "parent present" rationale with D1+D2; `Unreachable` now also covers "stat failed" and "parent on another device" |
| `core/src/cli/clean.rs` | `root_state` | 268-280 | signature `root_state(root: Option<&Path>, device: Option<u64>)`; D1 (a) error kinds; D2 (a) device compare (unix); D3 (a) missing device |
| `core/src/cli/clean.rs` | `Candidate` + `candidates` | 540-547, 588-592 | add `device: Option<u64>` via `read_root_device` |
| `core/src/cli/clean.rs` | `clean_orphaned` | 293 | pass `candidate.device` |
| `core/src/cli/clean.rs` | `purge_orphaned_state_dir` judge closure | 345-352 | read the device too; `Kept` reason text unchanged |
| `core/src/cli/clean.rs` | `delete_root_record_last` | 390-410 | D4 order |
| `core/src/cli/clean.rs` | `render` (Orphaned arm, `unreachable` explanation) | 625- | wording: "unreachable, unreadable, or on a volume that is not mounted" |
| `core/src/cli/clean.rs` | module doc | ~26 | mention permission/mount cases |
| `core/src/daemon/lifecycle.rs` | `purge_state_dir` doc | 941-953 | "the root's parent must still exist" -> "must read as deleted by `root_state`" ; no code change |
| `core/src/daemon/lifecycle.rs` | `orphan_check`, `is_definitely_gone` | 784-823 | no change (already `NotFound`-only; exiting on unmount is correct) |

## Tests

Unit (`core/src/cli/clean.rs` tests, `core/src/daemon/identity.rs` tests):

1. `root_state_keeps_a_root_whose_parent_cannot_be_searched` (unix): tempdir
   `p/root`, record it, `chmod 000 p`; assert `Unreachable`. A drop guard
   restores `0o755` before the tempdir is removed. If `fs::metadata(p/root)`
   still succeeds after the chmod (running as root, which bypasses
   permissions), the test returns early with a printed note rather than
   asserting nothing silently. Pattern already used in
   `tests/multi_project_front.rs:452-463`.
2. `the_sweep_keeps_state_whose_root_is_only_unreadable`: same setup through
   `sweep_orphaned` -> 0 deleted, state dir present; and through
   `clean orphaned --force` -> listed in `unreachable`.
3. `root_state_keeps_a_root_whose_parent_is_on_another_device` (unix):
   record a root, delete it, overwrite `project.dev` with `real_dev + 1`
   (computed from `metadata(parent).dev()`, never a literal); assert
   `Unreachable`. This is the shared shape of both F3 layouts: "the parent is
   now on a different filesystem than the root was".
4. `the_sweep_keeps_state_of_a_project_on_an_unmounted_volume`: test 3 through
   `sweep_orphaned` and `purge_orphaned_state_dir` (the daemon's path).
5. `a_root_recorded_without_a_device_is_not_judged` (unix): delete
   `project.dev`, root deleted with parent present -> `Unreachable` (D3 a).
6. `recording_a_root_records_its_device` (identity.rs): `read_root_device`
   equals `metadata(root).dev()`; rewritten value replaces a stale one.
7. Existing `root_state_tells_a_deleted_project_from_an_unreachable_one`,
   `orphaned_deletes_only_state_whose_project_directory_was_deleted`,
   `the_sweep_deletes_only_...`, and the integration
   `a_shim_start_sweeps_the_state_of_deleted_projects` stay green: their
   roots are recorded while present, so the device matches and the real
   deletion is still `Orphaned` (proves the fix does not stop cleanup).
8. Interrupted purge (D4): a dir with `project.root` + `project.dev` only
   still reads `Orphaned` and is finished by the sweep.

Unmounting a real volume needs no root on macOS (`hdiutil create -size 2m
-fs APFS`, `hdiutil attach -nobrowse -mountpoint <tmp>/vol`, `hdiutil detach`)
but not on Linux CI, so a real-mount test is optional: an `#[ignore]`d,
`#[cfg(target_os = "macos")]` test, run once by verify by hand. The
device-override fixture (tests 3-4) is the portable proof.

No new processes, threads or timers: no 5x repeat requirement.

## Controls (revert the fix, the named test must fail)

| # | Revert (code only) | Must fail |
|---|---|---|
| C1 | `root_state`: root check back to `root.exists()` | 1, 2 |
| C2 | `root_state`: drop the device comparison (treat as equal) | 3, 4 |
| C3 | `root_state`: missing device -> parent rule instead of `Unreachable` | 5 |
| C4 | `record_project_root`: do not write `project.dev` | 6, and 7 (real deletions turn `Unreachable`) |

No control for test 8: an interruption cannot be placed deterministically
(`read_dir` order is unspecified), so it stays a plain regression test of the
state D4 leaves.

All four are in `core/src/cli/clean.rs` / `core/src/daemon/identity.rs`, so
they build one lib test binary: `cargo test -p g-mesh --lib cli::clean
daemon::identity` per control (plus `--test worktree_daemon_lifecycle` for
C4's integration test).

## Residual cases (documented, not fixed)

- A device id that changed across a reboot keeps a really-deleted project's
  state (leak, safe direction); `clean expired` removes it.
- Windows: no device check; volume removal already reads `Unreachable` via the
  missing drive root. NTFS folder mount points (rare) keep the old behaviour.
- A bind mount or a btrfs subvolume boundary between root and parent: a
  deleted root under it reads `Unreachable` (kept). Safe direction.
