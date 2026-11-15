//! Judging and deleting one orphaned project state directory: whose project root is
//! gone, which nothing is running for and no lock holds. Shared by `g-mesh clean
//! orphaned`, the daemon's own purge and the shim's startup sweep.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::daemon;

/// What the recorded project root of a state directory says about it.
///
/// The distinction that carries the whole risk of this command is the last
/// two. "Not present right now" is not "gone forever": `stat` fails for a
/// missing directory, but also for a permission change on an ancestor and for
/// a volume that is not mounted. Only these read as deleted (GM-565):
///
/// - `stat(root)` fails with `NotFound` (any other error is `Unreachable`);
/// - the parent is a readable directory; and
/// - on Unix, the parent is on the device the root was recorded on
///   (`project.dev`). A deleted directory leaves its parent on its own
///   filesystem; an unmounted volume leaves a parent on another one
///   (`/Volumes` for `/Volumes/Ext`, the empty mount point for
///   `/mnt/ext/proj`). A root recorded without a device is not judged on
///   Unix.
///
/// Deleting a whole tree of projects at once therefore also reads as
/// `Unreachable`. That is the trade accepted knowingly: those are cleaned by
/// id, and being asked to name them is a smaller cost than a delete nobody
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootState {
    /// The project is still on disk.
    Live,
    /// The project directory is gone (`NotFound`), but the directory that
    /// held it is still there, on the filesystem the root was recorded on -
    /// so it really was deleted.
    Orphaned,
    /// Possibly deleted, possibly not: the root's `stat` failed for a reason
    /// other than `NotFound` (permissions, I/O), its parent is missing or
    /// unreadable, the parent is on another device than the root was (an
    /// unmounted volume), or no device was recorded. Not judged.
    Unreachable,
    /// No `project.root` file - a state directory created before g-mesh
    /// recorded roots. Not judged, and never guessed at: `project_hash` is
    /// one-way, so there is nothing to guess *from*.
    Legacy,
}

/// Judges a recorded root against the disk; see [`RootState`] for the rules.
/// `device` is the recorded `project.dev` ([`crate::paths::state_dir::read_root_device`]).
pub(crate) fn root_state(root: Option<&Path>, device: Option<u64>) -> RootState {
    let Some(root) = root else {
        return RootState::Legacy;
    };
    match fs::metadata(root) {
        Ok(_) => return RootState::Live,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return RootState::Unreachable,
    }
    let Some(parent) = root.parent() else {
        return RootState::Unreachable;
    };
    let Ok(parent_meta) = fs::metadata(parent) else {
        return RootState::Unreachable;
    };
    if !parent_meta.is_dir() {
        return RootState::Unreachable;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if device != Some(parent_meta.dev()) {
            return RootState::Unreachable;
        }
    }
    #[cfg(not(unix))]
    let _ = device;
    RootState::Orphaned
}

/// What [`purge_orphaned_state_dir`] decided for one state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Purge {
    /// The directory was orphaned, idle and unlocked, and is gone.
    Deleted,
    /// The directory was left alone, for the reason given.
    Kept(&'static str),
}

/// How [`purge_orphaned_state_dir`] treats a lock someone else holds: the
/// project's bootstrap lock, or the daemon's singleton lock it probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockWait {
    /// Try once and keep the directory if either lock is busy. The shim's
    /// startup sweep uses this, so it never stalls a shim's start.
    Try,
    /// Poll for both locks for up to this long, one deadline for the two.
    /// The daemon's own purge uses this: a sweeper holding the bootstrap lock
    /// is about to delete the same directory (or died mid-delete and left the
    /// rest), and a sweeper judging the directory holds the singleton lock
    /// for the instant of its probe; giving up at once on either would let
    /// the daemon exit with its state still on disk. A daemon that really is
    /// running holds the singleton lock for good, so that wait ends in
    /// `Kept` at the deadline.
    UpTo(Duration),
}

/// How often [`LockWait::UpTo`] retries a busy bootstrap or singleton lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Deletes one project's state directory, but only under the rules
/// `clean orphaned --force` applies to it: its recorded root is
/// [`RootState::Orphaned`] (gone, while the directory that held it is not -
/// never an unmounted volume, never a directory with no `project.root`), and
/// nothing is running for it ([`daemon_is_running`], plus the daemon's
/// singleton lock, which a daemon whose pid file is already gone still holds).
///
/// Taken under the project's bootstrap lock, then re-judged: a shim can only
/// bootstrap a project whose root exists (`project_hash` canonicalizes it), so
/// a root recreated between the first look and the lock is seen by the second
/// one and the directory kept. `wait` says what a busy lock means: under
/// [`LockWait::Try`] a shim holding the bootstrap lock may be bootstrapping
/// this project, and a held singleton lock may be a running daemon, either of
/// which is reason enough to keep it; under [`LockWait::UpTo`] both are polled
/// until free or the bound runs out, because another process's sweep probes
/// the singleton lock (a brief `try_lock`) and holds the bootstrap lock while
/// it deletes. A live pid file is never waited for. A directory that is gone
/// once the bootstrap lock is taken was deleted by whoever held it, which is
/// the outcome asked for.
///
/// Shared by the daemon's own purge on `ProjectRootGone`
/// (`daemon::lifecycle::purge_state_dir`, GM-524 D1) and the shim's startup
/// sweep ([`sweep_orphaned`]), so the two can never disagree with
/// `clean orphaned` on what is safe to delete.
pub(crate) fn purge_orphaned_state_dir(state_dir: &Path, wait: LockWait) -> Result<Purge> {
    const RUNNING: &str = "a daemon or plugin is still running for it";
    let deadline = match wait {
        LockWait::Try => None,
        LockWait::UpTo(bound) => Some(Instant::now() + bound),
    };
    // `None` when the directory may go; a held singleton lock is polled under
    // `UpTo`, since a sweeper's probe holds it for an instant.
    let judge = || -> Option<Purge> {
        loop {
            let root = crate::paths::state_dir::read_project_root(state_dir);
            let device = crate::paths::state_dir::read_root_device(state_dir);
            if root_state(root.as_deref(), device) != RootState::Orphaned {
                return Some(Purge::Kept("its project root exists, is unreachable, or is not recorded"));
            }
            if daemon_is_running(state_dir) {
                return Some(Purge::Kept(RUNNING));
            }
            if !singleton_lock_is_held(state_dir) {
                return None;
            }
            match deadline {
                None => return Some(Purge::Kept(RUNNING)),
                Some(deadline) if Instant::now() >= deadline => {
                    return Some(Purge::Kept("a daemon or plugin held its singleton lock past the wait"));
                }
                Some(_) => std::thread::sleep(LOCK_POLL_INTERVAL),
            }
        }
    };
    if let Some(kept) = judge() {
        return Ok(kept);
    }

    let lock_path = daemon::bootstrap_lock_path_in(state_dir);
    let lock = match fs::File::options().create(true).write(true).truncate(false).open(&lock_path) {
        Ok(file) => file,
        // The directory went between the judgement and here: someone else
        // purged it, which is the outcome asked for.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Purge::Deleted),
        Err(err) => return Err(err).with_context(|| format!("failed to open {}", lock_path.display())),
    };
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => match deadline {
                None => return Ok(Purge::Kept("a shim is bootstrapping it")),
                Some(deadline) if Instant::now() >= deadline => {
                    return Ok(Purge::Kept("a shim held its bootstrap lock past the wait"));
                }
                Some(_) => std::thread::sleep(LOCK_POLL_INTERVAL),
            },
            Err(fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("failed to lock {}", lock_path.display()));
            }
        }
    }
    // The lock is taken on a file opened before whoever held it deleted the
    // directory, so a gone directory must be told apart here: re-judged, it
    // would read as "not recorded" and be reported as kept.
    match fs::symlink_metadata(state_dir) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Purge::Deleted),
        Err(err) => return Err(err).with_context(|| format!("failed to stat {}", state_dir.display())),
    }
    if let Some(kept) = judge() {
        return Ok(kept);
    }
    // Windows cannot delete a file this process holds open; on Unix the lock
    // is held across the delete, so no bootstrap can slip in between.
    #[cfg(windows)]
    drop(lock);
    delete_root_record_last(state_dir)?;
    Ok(Purge::Deleted)
}

/// [`delete`], with `project.root` and `project.dev` removed only after
/// everything else, in that order. A delete cut short (a shim that exits
/// mid-sweep, a daemon killed mid-purge) then leaves a directory that still
/// reads as orphaned and is finished by the next sweep, never one without its
/// root record, which would be [`RootState::Legacy`] and kept forever. An
/// interruption between the last two removals leaves only `project.dev`: it
/// reads `Legacy` and is kept (a few bytes, accepted).
fn delete_root_record_last(state_dir: &Path) -> Result<()> {
    let entries =
        fs::read_dir(state_dir).with_context(|| format!("failed to read {}", state_dir.display()))?;
    for entry in entries.flatten() {
        if entry.file_name() == crate::paths::state_dir::ROOT_FILE
            || entry.file_name() == crate::paths::state_dir::DEVICE_FILE
        {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("failed to delete {}", path.display())),
        }
    }
    for name in [crate::paths::state_dir::ROOT_FILE, crate::paths::state_dir::DEVICE_FILE] {
        match fs::remove_file(state_dir.join(name)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("failed to delete {name}")),
        }
    }
    delete(state_dir)
}

/// Whether some process holds the daemon's singleton lock in `state_dir`.
/// A lock that cannot be checked counts as held, for the reason
/// [`daemon_is_running`] gives for an unreadable pid file.
fn singleton_lock_is_held(state_dir: &Path) -> bool {
    let path = daemon::daemon_lock_path_in(state_dir);
    let file = match fs::File::options().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(err) => return err.kind() != std::io::ErrorKind::NotFound,
    };
    !matches!(file.try_lock(), Ok(()))
}

/// Whether anything is still running for this state directory - the core, or
/// any language's plugin that outlived it. Lists every `plugin-<language>.pid`
/// file actually present (`daemon::registry::discovered_pid_files`) rather
/// than assuming one hardcoded plugin - a live Python plugin orphaned by a
/// dead core must stop this project from being cleaned up exactly as much as
/// a live JS/TS one always has.
/// A pid file that cannot be read counts as running, and says so. The two
/// answers this function has to give are "delete it" and "leave it alone", and
/// only one of them is reversible - so "I could not tell" belongs with the
/// safe one. Collapsing the error into `None` put it with the other (GM-247).
#[allow(clippy::disallowed_macros)]
pub(crate) fn daemon_is_running(state_dir: &Path) -> bool {
    let mut pid_files = vec![daemon::pid_path_in(state_dir)];
    pid_files.extend(daemon::registry::discovered_pid_files(state_dir).into_iter().map(|(_, path)| path));
    pid_files.iter().any(|path| match daemon::read_pid_file_result(path) {
        Ok(Some(pid)) => daemon::is_process_alive(pid),
        Ok(None) => false,
        Err(err) => {
            eprintln!(
                "g-mesh: cannot read {} ({err}) - treating this project as still running, \
                 because deleting the state of a live one is the mistake that cannot be undone",
                path.display()
            );
            true
        }
    })
}

pub(crate) fn delete(path: &Path) -> Result<()> {
    fs::remove_dir_all(path).with_context(|| format!("failed to delete {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::test_fixture::*;

    /// The classification itself, stated as the four cases rather than only
    /// exercised through a fixture - `root_state` is where the volume guard
    /// lives, and it is worth failing on its own terms.
    #[test]
    fn root_state_tells_a_deleted_project_from_an_unreachable_one() {
        let parent = tempfile::tempdir().unwrap();
        let live = parent.path().join("live");
        fs::create_dir(&live).unwrap();

        #[cfg(unix)]
        let device = {
            use std::os::unix::fs::MetadataExt;
            Some(fs::metadata(parent.path()).unwrap().dev())
        };
        #[cfg(not(unix))]
        let device = None;

        assert_eq!(root_state(Some(&live), device), RootState::Live);
        assert_eq!(root_state(Some(&parent.path().join("gone")), device), RootState::Orphaned);
        assert_eq!(
            root_state(Some(&parent.path().join("unmounted").join("project")), device),
            RootState::Unreachable
        );
        assert_eq!(root_state(None, device), RootState::Legacy);
    }

    /// Test 1: a root whose `stat` fails for a reason other than `NotFound`
    /// (an ancestor lost its search permission) is not judged as deleted,
    /// even though the root's parent itself still stats fine.
    #[cfg(unix)]
    #[test]
    fn root_state_keeps_a_root_whose_parent_cannot_be_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("p");
        let root = parent.join("root");
        fs::create_dir_all(&root).unwrap();
        let state = tempfile::tempdir().unwrap();
        crate::paths::state_dir::record_project_root(state.path(), &root).unwrap();
        let recorded = crate::paths::state_dir::read_project_root(state.path()).unwrap();
        let device = crate::paths::state_dir::read_root_device(state.path());

        let Some(_restore) = make_unsearchable(&parent, &recorded) else {
            return;
        };

        assert_eq!(root_state(Some(&recorded), device), RootState::Unreachable);
    }

    /// Test 3: a missing root whose parent is on another device than the one
    /// recorded is not judged; the same root with the real device is.
    #[cfg(unix)]
    #[test]
    fn root_state_keeps_a_root_whose_parent_is_on_another_device() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("gone");
        let real = device_of(parent.path());

        assert_eq!(root_state(Some(&root), Some(real)), RootState::Orphaned);
        assert_eq!(root_state(Some(&root), Some(real.wrapping_add(1))), RootState::Unreachable);
    }

    /// Test 5 (D3): a root recorded before `project.dev` existed is not
    /// judged on Unix, even when it reads exactly like a deletion.
    #[cfg(unix)]
    #[test]
    fn a_root_recorded_without_a_device_is_not_judged() {
        let projects = Projects::new();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("gone");
        fs::create_dir(&root).unwrap();
        projects.add("aaaa1111", 1);
        projects.record_root("aaaa1111", &root);
        fs::remove_dir(&root).unwrap();
        let state = projects.root().join("aaaa1111");
        fs::remove_file(state.join(crate::paths::state_dir::DEVICE_FILE)).unwrap();

        let recorded = crate::paths::state_dir::read_project_root(&state).unwrap();
        assert_eq!(crate::paths::state_dir::read_root_device(&state), None);
        assert_eq!(root_state(Some(&recorded), None), RootState::Unreachable);
        assert!(matches!(purge_orphaned_state_dir(&state, LockWait::Try).unwrap(), Purge::Kept(_)));
        assert!(projects.exists("aaaa1111"));
    }

    // -----------------------------------------------------------------
    // A purge that finds the bootstrap lock held
    // -----------------------------------------------------------------

    /// Takes `state`'s bootstrap lock on a file of its own, as a
    /// bootstrapping shim or another process's sweep holds it.
    fn hold_bootstrap_lock(state: &Path) -> fs::File {
        let lock = fs::File::create(daemon::bootstrap_lock_path_in(state)).unwrap();
        lock.try_lock().unwrap();
        lock
    }

    /// How long a test lets a purge run against a held lock before it acts:
    /// a purge that does not wait has returned well within it.
    const WATCH: Duration = Duration::from_millis(300);

    /// A bound no test here reaches: the lock is always released first.
    const LONG_WAIT: Duration = Duration::from_secs(60);

    /// Runs the purge on its own thread, so the test can release the lock
    /// (or delete the directory) while it waits.
    fn purge_in_background(state: &Path, wait: LockWait) -> std::thread::JoinHandle<Result<Purge>> {
        let state = state.to_path_buf();
        std::thread::spawn(move || purge_orphaned_state_dir(&state, wait))
    }

    /// The daemon's purge: a lock that is held and then released over a
    /// directory still orphaned is waited for, and the directory deleted.
    ///
    /// Control: make `LockWait::UpTo` give up at once like `Try`: the purge
    /// returns `Kept` while the lock is held.
    #[test]
    fn a_waiting_purge_deletes_the_directory_once_the_lock_is_released() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let lock = hold_bootstrap_lock(&state);

        let purge = purge_in_background(&state, LockWait::UpTo(LONG_WAIT));
        std::thread::sleep(WATCH);
        assert!(!purge.is_finished(), "the purge must wait for the held lock, not give up on it");
        assert!(state.exists(), "nothing may be deleted while the lock is held");

        drop(lock);
        assert_eq!(purge.join().unwrap().unwrap(), Purge::Deleted);
        assert!(!state.exists(), "the directory must go once the lock is free");
    }

    /// A holder that deletes the directory itself (a sweep) and then releases
    /// the lock has done what the purge was for: that is `Deleted`, not a
    /// directory "kept" because its root record went with it.
    ///
    /// Unix only: Windows cannot delete the lock file the purge holds open.
    ///
    /// Control: drop the purge's check for a directory gone once the lock is
    /// taken: the re-judgement reports it kept as not recorded.
    #[cfg(unix)]
    #[test]
    fn a_waiting_purge_whose_directory_the_holder_deleted_reports_it_deleted() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let lock = hold_bootstrap_lock(&state);

        let purge = purge_in_background(&state, LockWait::UpTo(LONG_WAIT));
        // Once the purge has opened the lock file and is waiting on it: a
        // purge that started after the delete would find no directory to
        // open the lock in, which says `Deleted` without the path under test.
        std::thread::sleep(WATCH);
        assert!(!purge.is_finished(), "the purge must still be waiting for the held lock");
        fs::remove_dir_all(&state).unwrap();
        drop(lock);

        assert_eq!(purge.join().unwrap().unwrap(), Purge::Deleted);
    }

    /// A lock held past the bound keeps the directory, saying why, rather
    /// than waiting on.
    ///
    /// Control: make `LockWait::UpTo` give up at once like `Try`: the reason
    /// is the one for a bootstrapping shim instead.
    #[test]
    fn a_purge_whose_lock_stays_held_past_the_bound_keeps_the_directory() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let _lock = hold_bootstrap_lock(&state);
        let bound = Duration::from_millis(200);

        let started = Instant::now();
        let purge = purge_orphaned_state_dir(&state, LockWait::UpTo(bound)).unwrap();

        assert_eq!(purge, Purge::Kept("a shim held its bootstrap lock past the wait"));
        assert!(started.elapsed() >= bound, "the purge must wait out its bound first");
        assert!(state.exists());
    }

    /// The shim's sweep never waits: a busy lock keeps the directory on the
    /// first try.
    ///
    /// Control: make `LockWait::Try` poll like `UpTo`: the purge waits and
    /// reports the lock held past the wait instead.
    #[test]
    fn a_trying_purge_keeps_the_directory_of_a_busy_lock_at_once() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let _lock = hold_bootstrap_lock(&state);

        let purge = purge_orphaned_state_dir(&state, LockWait::Try).unwrap();

        assert_eq!(purge, Purge::Kept("a shim is bootstrapping it"));
        assert!(state.exists());
    }

    // A purge that finds the daemon's singleton lock held
    // -----------------------------------------------------------------

    /// Takes `state`'s singleton lock on a file of its own, as a running
    /// daemon holds it or another process's sweep probes it.
    fn hold_singleton_lock(state: &Path) -> fs::File {
        let lock = fs::File::create(daemon::daemon_lock_path_in(state)).unwrap();
        lock.try_lock().unwrap();
        lock
    }

    /// The daemon's purge: a singleton lock held for a moment (a sweeper's
    /// probe) over a directory still orphaned is waited out, and the
    /// directory deleted.
    ///
    /// Control: return `Kept` at once on a held singleton lock under
    /// `LockWait::UpTo` too: the purge keeps the directory.
    #[test]
    fn a_waiting_purge_deletes_the_directory_once_the_singleton_lock_is_released() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let lock = hold_singleton_lock(&state);

        let purge = purge_in_background(&state, LockWait::UpTo(LONG_WAIT));
        std::thread::sleep(WATCH);
        assert!(!purge.is_finished(), "the purge must wait for the held singleton lock, not give up on it");
        assert!(state.exists(), "nothing may be deleted while the singleton lock is held");

        drop(lock);
        assert_eq!(purge.join().unwrap().unwrap(), Purge::Deleted);
        assert!(!state.exists(), "the directory must go once the singleton lock is free");
    }

    /// A singleton lock held past the bound - a daemon that really runs -
    /// keeps the directory, saying why, once the bound is spent.
    ///
    /// Control: return `Kept` at once on a held singleton lock under
    /// `LockWait::UpTo` too: the reason is the one for a running daemon.
    #[test]
    fn a_purge_whose_singleton_lock_stays_held_past_the_bound_keeps_the_directory() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let _lock = hold_singleton_lock(&state);
        let bound = Duration::from_millis(200);

        let started = Instant::now();
        let purge = purge_orphaned_state_dir(&state, LockWait::UpTo(bound)).unwrap();

        assert_eq!(purge, Purge::Kept("a daemon or plugin held its singleton lock past the wait"));
        assert!(started.elapsed() >= bound, "the purge must wait out its bound first");
        assert!(state.exists());
    }

    /// The shim's sweep never waits: a held singleton lock keeps the
    /// directory on the first try, as a running daemon's.
    #[test]
    fn a_trying_purge_keeps_the_directory_of_a_held_singleton_lock_at_once() {
        let fixture = SweepHome::new();
        let state = fixture.add_orphan("aaaa1111");
        let _lock = hold_singleton_lock(&state);

        let started = Instant::now();
        let purge = purge_orphaned_state_dir(&state, LockWait::Try).unwrap();

        assert_eq!(purge, Purge::Kept("a daemon or plugin is still running for it"));
        assert!(started.elapsed() < WATCH, "a trying purge must not wait on the lock");
        assert!(state.exists());
    }
}
