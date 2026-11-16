//! Fixtures shared by the `control::purge`, `control::sweep` and `cli::clean` tests.
#![allow(clippy::disallowed_macros)]

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// A `~/.g-mesh/projects/` of its own, so every test here - including the
/// ones that delete everything - is confined to a temp directory.
pub(crate) struct Projects {
    pub(crate) dir: tempfile::TempDir,
}

impl Projects {
    pub(crate) fn new() -> Self {
        Self { dir: tempfile::tempdir().unwrap() }
    }

    pub(crate) fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A project directory with an index whose `lastUsed` is `idle_days`
    /// old, exactly as a real one would record it.
    pub(crate) fn add(&self, id: &str, idle_days: u64) -> PathBuf {
        let path = self.add_empty(id);
        let conn = Connection::open(path.join("index.db")).unwrap();
        crate::storage::schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version())
            .unwrap();
        conn.execute(
            "UPDATE meta SET lastUsed = datetime('now', ?1) WHERE id = 1",
            rusqlite::params![format!("-{idle_days} days")],
        )
        .unwrap();
        path
    }

    /// A project directory with no index at all - a bootstrap that never
    /// finished, which has no idle time to judge it by.
    pub(crate) fn add_empty(&self, id: &str) -> PathBuf {
        let path = self.root().join(id);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Records `root` as the project directory `id`'s state belongs to,
    /// exactly as `connection::ensure_project_dir` does for a real one.
    pub(crate) fn record_root(&self, id: &str, root: &Path) {
        crate::paths::state_dir::record_project_root(&self.root().join(id), root).unwrap();
    }

    /// Marks a project as served by a live daemon, using this test
    /// process's own pid - which is unquestionably alive.
    pub(crate) fn mark_daemon_running(&self, id: &str) {
        fs::write(self.root().join(id).join("daemon.pid"), std::process::id().to_string()).unwrap();
    }

    pub(crate) fn exists(&self, id: &str) -> bool {
        self.root().join(id).is_dir()
    }
}

/// The fixture the whole target has to get right at once: one project
/// still on disk, one whose directory was deleted, one whose root is
/// missing along with its parent, one predating recorded roots, and one
/// orphan a daemon is still serving. Only the second may go.
pub(crate) struct OrphanFixture {
    pub(crate) projects: Projects,
    /// Held so the live project's directory outlives the assertions.
    pub(crate) _live_root: tempfile::TempDir,
    /// The parent that stays behind when the deleted project goes - what
    /// makes its state `Orphaned` rather than `Unreachable`.
    pub(crate) _surviving_parent: tempfile::TempDir,
}

pub(crate) fn orphan_fixture() -> OrphanFixture {
    let projects = Projects::new();

    let live_root = tempfile::tempdir().unwrap();
    projects.add("aaaa1111", 1);
    projects.record_root("aaaa1111", live_root.path());

    // Deleted: created inside a parent that stays, then removed.
    let surviving_parent = tempfile::tempdir().unwrap();
    let deleted = surviving_parent.path().join("gone");
    fs::create_dir(&deleted).unwrap();
    projects.add("bbbb2222", 1);
    projects.record_root("bbbb2222", &deleted);
    fs::remove_dir(&deleted).unwrap();

    // Unreachable: root and its parent both gone, as an ejected disk
    // leaves them. Recorded while both existed, so the path is canonical.
    let volume = tempfile::tempdir().unwrap();
    let mounted = volume.path().join("mount");
    let on_the_volume = mounted.join("project");
    fs::create_dir_all(&on_the_volume).unwrap();
    projects.add("cccc3333", 1);
    projects.record_root("cccc3333", &on_the_volume);
    fs::remove_dir_all(&mounted).unwrap();

    // Legacy: indexed before roots were recorded.
    projects.add("dddd4444", 1);

    // An orphan whose daemon is still running.
    let served = surviving_parent.path().join("served");
    fs::create_dir(&served).unwrap();
    projects.add("eeee5555", 1);
    projects.record_root("eeee5555", &served);
    projects.mark_daemon_running("eeee5555");
    fs::remove_dir(&served).unwrap();

    OrphanFixture { projects, _live_root: live_root, _surviving_parent: surviving_parent }
}

/// The device id `path` is on right now.
#[cfg(unix)]
pub(crate) fn device_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().dev()
}

/// Restores a directory's mode on drop, so a panicking assertion cannot
/// leave a `chmod 000` directory the tempdir cleanup cannot remove.
#[cfg(unix)]
pub(crate) struct RestoreMode(PathBuf);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Makes `dir` unsearchable. `None` when the process bypasses
/// permissions (running as root): the caller then has nothing to test
/// and says so instead of asserting nothing silently.
#[cfg(unix)]
pub(crate) fn make_unsearchable(dir: &Path, probe: &Path) -> Option<RestoreMode> {
    use std::os::unix::fs::PermissionsExt;
    let guard = RestoreMode(dir.to_path_buf());
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::metadata(probe).is_ok() {
        eprintln!("note: {} is still readable after chmod 000 (running as root?); skipped", probe.display());
        return None;
    }
    Some(guard)
}

/// Records `root` for `id`, deletes the root (its parent stays), and
/// overwrites the recorded device with one the parent is not on - the
/// shape both unmounted-volume layouts leave (`/Volumes/Ext` and
/// `/mnt/ext/proj`): the parent is now on another filesystem than the root
/// was. The value is computed from the real device, never a literal.
#[cfg(unix)]
pub(crate) fn record_then_unmount(projects: &Projects, id: &str, root: &Path) {
    fs::create_dir(root).unwrap();
    projects.record_root(id, root);
    fs::remove_dir(root).unwrap();
    let other = device_of(root.parent().unwrap()).wrapping_add(1);
    fs::write(projects.root().join(id).join(crate::paths::state_dir::DEVICE_FILE), other.to_string())
        .unwrap();
}

/// A home holding one orphaned project's state, as the sweep sees it:
/// `<home>/projects/<id>/` with a recorded root that was deleted.
pub(crate) struct SweepHome {
    pub(crate) home: tempfile::TempDir,
    pub(crate) parent: tempfile::TempDir,
}

impl SweepHome {
    pub(crate) fn new() -> Self {
        Self { home: tempfile::tempdir().unwrap(), parent: tempfile::tempdir().unwrap() }
    }

    pub(crate) fn add_orphan(&self, id: &str) -> PathBuf {
        let state = crate::paths::state_dir::projects_root_under(self.home.path()).join(id);
        fs::create_dir_all(&state).unwrap();
        let root = self.parent.path().join(id);
        fs::create_dir(&root).unwrap();
        crate::paths::state_dir::record_project_root(&state, &root).unwrap();
        fs::remove_dir(&root).unwrap();
        state
    }
}
