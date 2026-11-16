//! The shim's startup sweep over all project state directories.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::control::purge::{purge_orphaned_state_dir, LockWait, Purge};

/// The shim's startup sweep of orphaned state directories (GM-524 D1, option
/// (c)): what `clean orphaned --force` would delete, deleted without being
/// asked, for the daemons that never got to purge their own state (one killed
/// with `-9`, a machine that slept through the removal, Windows).
///
/// Cheap by construction on a store of thousands of directories: per entry
/// it reads `project.root` and stats the root and its parent; only an
/// orphaned one goes on to the pid files and locks. It skips `lastUsed`,
/// which [`candidates`] reads from each `index.db`. Nothing else is
/// touched: names that are not project ids, files, and every directory whose
/// root is live, unreachable or unrecorded are left exactly as they are.
/// One directory failing to delete does not stop the rest.
pub(crate) fn sweep_orphaned(projects_root: &Path) -> Result<usize> {
    let entries = match fs::read_dir(projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", projects_root.display())),
    };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let is_project_id = entry.file_name().to_str().is_some_and(|id| validate_id(id).is_ok());
        if !is_project_id || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        match purge_orphaned_state_dir(&entry.path(), LockWait::Try) {
            Ok(Purge::Deleted) => deleted += 1,
            Ok(Purge::Kept(_)) => {}
            Err(err) => {
                crate::log_line!("g-mesh: orphan sweep could not delete {}: {err:#}", entry.path().display())
            }
        }
    }
    Ok(deleted)
}

/// How often the startup sweep runs at most. Shims start many times an hour
/// across sessions and agents, the sweep is a backstop for the daemon's own
/// purge rather than the main path, and an orphaned directory costs disk, not
/// correctness - so a few times a day is plenty.
pub(crate) const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// What [`sweep_orphaned_if_due`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sweep {
    /// The last sweep was less than the interval ago.
    NotDue,
    /// Another process is sweeping right now.
    Busy,
    /// Swept; this many directories were deleted.
    Swept(usize),
}

/// [`sweep_orphaned`] over `home`'s projects, at most once per `interval`.
///
/// The marker's mtime is the clock, and the marker's lock makes one sweep at a
/// time: the mtime is bumped *before* the scan, under the lock, so every other
/// shim starting meanwhile reads it as not due or finds the lock taken. A
/// marker that cannot be read (first run, clock skew into the future aside)
/// counts as due.
pub(crate) fn sweep_orphaned_if_due(home: &Path, interval: Duration) -> Result<Sweep> {
    fs::create_dir_all(home).with_context(|| format!("failed to create {}", home.display()))?;
    // In the g-mesh home rather than in `projects/`, where it would be one
    // more entry for every scan.
    let marker_path = crate::paths::state_dir::orphan_sweep_stamp(home);
    let marker = fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&marker_path)
        .with_context(|| format!("failed to open {}", marker_path.display()))?;
    match marker.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(Sweep::Busy),
        Err(fs::TryLockError::Error(err)) => {
            return Err(err).with_context(|| format!("failed to lock {}", marker_path.display()));
        }
    }
    // A marker this call just created has length 0 and was never stamped.
    let stamped = marker.metadata().ok().filter(|meta| meta.len() > 0).and_then(|meta| meta.modified().ok());
    let due = stamped.is_none_or(|at| at.elapsed().map_or(true, |age| age >= interval));
    if !due {
        return Ok(Sweep::NotDue);
    }
    // Written (not just touched) so the length tells a stamped marker from a
    // freshly created one, and the write moves the mtime to now.
    use std::io::Write as _;
    let mut stamp = &marker;
    stamp
        .set_len(0)
        .and_then(|()| stamp.write_all(b"swept\n"))
        .with_context(|| format!("failed to stamp {}", marker_path.display()))?;
    Ok(Sweep::Swept(sweep_orphaned(&crate::paths::state_dir::projects_root_under(home))?))
}

/// Rejects anything that is not a bare directory name.
///
/// The id is joined onto `~/.g-mesh/projects/`, so without this a `..` or an
/// absolute path in it would make `clean` delete a directory that has nothing
/// to do with g-mesh. Real ids are hex hashes; alphanumeric is a shade
/// looser, and still admits no separator, no dot, and no traversal.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "`{id}` is not a project id - ids are the directory names under \
             ~/.g-mesh/projects/, not paths"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::fixture_tests::*;
    use crate::control::purge::{root_state, RootState};
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;
    use std::time::Instant;

    // -----------------------------------------------------------------
    // The shim's startup sweep
    // -----------------------------------------------------------------

    /// The sweep deletes exactly what `clean orphaned --force` would: only
    /// the deleted project's state, never a live, unreachable, unrecorded or
    /// still-served one.
    ///
    /// Control: make `purge_orphaned_state_dir` skip the
    /// `daemon_is_running` check: eeee5555 (served by a live pid) is deleted.
    #[test]
    fn the_sweep_deletes_only_state_whose_project_directory_was_deleted() {
        let fixture = orphan_fixture();

        assert_eq!(sweep_orphaned(fixture.projects.root()).unwrap(), 1);

        assert!(!fixture.projects.exists("bbbb2222"), "the deleted project's state survived");
        for survivor in ["aaaa1111", "cccc3333", "dddd4444", "eeee5555"] {
            assert!(fixture.projects.exists(survivor), "{survivor} must not have been deleted");
        }
    }

    /// A daemon whose pid file is already gone still holds its singleton
    /// lock while it runs; the sweep keeps its state.
    #[test]
    fn the_sweep_keeps_state_whose_daemon_lock_is_held() {
        let fixture = orphan_fixture();
        let state = fixture.projects.root().join("bbbb2222");
        let lock =
            fs::File::create(crate::paths::state_dir::StateDir::at(&state).daemon_lock_file()).unwrap();
        lock.lock().unwrap();

        assert_eq!(sweep_orphaned(fixture.projects.root()).unwrap(), 0);
        assert!(fixture.projects.exists("bbbb2222"), "a locked daemon's state must stay");

        drop(lock);
        // GM-636: a child spawned by another test thread can still hold a
        // copy of the lock fd for a few ms after the drop. The sweep deletes
        // nothing while it sees the lock held, so retrying is safe. A local
        // loop: daemon/tests.rs is a private module of `daemon`.
        let deadline = Instant::now() + Duration::from_secs(5);
        let swept = loop {
            let n = sweep_orphaned(fixture.projects.root()).unwrap();
            if n != 0 || Instant::now() >= deadline {
                break n;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(swept, 1, "and goes once the lock is free");
    }

    /// At most one sweep per interval: a second call within it does nothing,
    /// and leaves an orphan made in between for a later sweep.
    ///
    /// Control: make `sweep_orphaned_if_due` treat every call as due: the
    /// second call sweeps.
    #[test]
    fn the_sweep_runs_at_most_once_per_interval() {
        let fixture = SweepHome::new();
        let first = fixture.add_orphan("aaaa1111");

        assert_eq!(
            sweep_orphaned_if_due(fixture.home.path(), ORPHAN_SWEEP_INTERVAL).unwrap(),
            Sweep::Swept(1)
        );
        assert!(!first.exists());

        let second = fixture.add_orphan("bbbb2222");
        assert_eq!(sweep_orphaned_if_due(fixture.home.path(), ORPHAN_SWEEP_INTERVAL).unwrap(), Sweep::NotDue);
        assert!(second.exists(), "a sweep that is not due deletes nothing");

        assert_eq!(sweep_orphaned_if_due(fixture.home.path(), Duration::ZERO).unwrap(), Sweep::Swept(1));
        assert!(!second.exists(), "once due again, it sweeps");
    }

    /// Another process sweeping (holding the marker's lock) makes this one
    /// step aside rather than sweep beside it.
    #[test]
    fn the_sweep_steps_aside_while_another_one_runs() {
        let fixture = SweepHome::new();
        let orphan = fixture.add_orphan("aaaa1111");
        let marker =
            fs::File::create(crate::paths::state_dir::orphan_sweep_stamp(fixture.home.path())).unwrap();
        marker.lock().unwrap();

        assert_eq!(sweep_orphaned_if_due(fixture.home.path(), Duration::ZERO).unwrap(), Sweep::Busy);
        assert!(orphan.exists());
    }

    /// Test 4: the unmounted-volume shape through the shim's sweep and the
    /// daemon's own purge; a really deleted sibling beside it still goes, so
    /// the sweep is shown to run.
    #[cfg(unix)]
    #[test]
    fn the_sweep_keeps_state_of_a_project_on_an_unmounted_volume() {
        let projects = Projects::new();
        let parent = tempfile::tempdir().unwrap();
        projects.add("aaaa1111", 1);
        record_then_unmount(&projects, "aaaa1111", &parent.path().join("on-volume"));
        let deleted = parent.path().join("deleted");
        fs::create_dir(&deleted).unwrap();
        projects.add("bbbb2222", 1);
        projects.record_root("bbbb2222", &deleted);
        fs::remove_dir(&deleted).unwrap();

        assert!(matches!(
            purge_orphaned_state_dir(&projects.root().join("aaaa1111"), LockWait::Try).unwrap(),
            Purge::Kept(_)
        ));
        assert!(projects.exists("aaaa1111"));

        assert_eq!(sweep_orphaned(projects.root()).unwrap(), 1);
        assert!(projects.exists("aaaa1111"), "the sweep deleted state of a project on an unmounted volume");
        assert!(!projects.exists("bbbb2222"), "the really deleted project's state survived");
    }

    /// Test 8 (D4): a purge cut short after deleting everything but the two
    /// root records leaves a directory that still reads as orphaned and is
    /// finished by the next sweep. One cut between the last two removals
    /// leaves only `project.dev`, which reads `Legacy` and is kept.
    #[test]
    fn an_interrupted_purge_is_finished_by_the_next_sweep() {
        let projects = Projects::new();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("gone");
        fs::create_dir(&root).unwrap();
        projects.add_empty("aaaa1111");
        projects.record_root("aaaa1111", &root);
        fs::remove_dir(&root).unwrap();
        let state = projects.root().join("aaaa1111");

        let recorded = crate::paths::state_dir::read_project_root(&state);
        let device = crate::paths::state_dir::read_root_device(&state);
        assert_eq!(root_state(recorded.as_deref(), device), RootState::Orphaned);

        let only_device = projects.add_empty("bbbb2222");
        fs::write(only_device.join(crate::paths::state_dir::DEVICE_FILE), "1").unwrap();
        assert_eq!(
            root_state(crate::paths::state_dir::read_project_root(&only_device).as_deref(), Some(1)),
            RootState::Legacy
        );

        assert_eq!(sweep_orphaned(projects.root()).unwrap(), 1);
        assert!(!projects.exists("aaaa1111"), "the interrupted purge was not finished");
        assert!(projects.exists("bbbb2222"), "a directory with only project.dev must be kept");
    }

    /// Optional real-volume proof (macOS, needs `hdiutil`, no root): index a
    /// project on a mounted disk image, detach it, and check the sweep keeps
    /// its state. Run by hand:
    /// `cargo test -p g-mesh --lib -- --ignored a_project_on_a_detached_disk_image_is_kept`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "mounts a disk image with hdiutil; run by hand"]
    fn a_project_on_a_detached_disk_image_is_kept() {
        use std::process::Command;

        struct Detach(PathBuf);
        impl Drop for Detach {
            fn drop(&mut self) {
                let _ = Command::new("hdiutil").arg("detach").arg("-force").arg(&self.0).output();
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("vol.dmg");
        let mount = tmp.path().join("vol");
        fs::create_dir(&mount).unwrap();
        let created = Command::new("hdiutil")
            .args(["create", "-size", "2m", "-fs", "APFS", "-volname", "gm565"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
        let attached = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .output()
            .unwrap();
        assert!(attached.status.success(), "{}", String::from_utf8_lossy(&attached.stderr));
        let mut detach = Some(Detach(mount.clone()));

        let root = mount.join("proj");
        fs::create_dir(&root).unwrap();
        let projects = Projects::new();
        projects.add("aaaa1111", 1);
        projects.record_root("aaaa1111", &root);
        assert_ne!(
            crate::paths::state_dir::read_root_device(&projects.root().join("aaaa1111")),
            Some(device_of(tmp.path())),
            "the image did not mount on its own device"
        );

        drop(detach.take());
        assert!(!root.exists(), "the image is still mounted");

        assert_eq!(sweep_orphaned(projects.root()).unwrap(), 0);
        assert!(projects.exists("aaaa1111"), "the sweep deleted state of a project on a detached volume");
    }
}
