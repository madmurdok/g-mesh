use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::paths;

/// Deterministic identity for a project root, shared by every call site
/// that needs to agree on it (SQLite storage location, daemon socket/pid
/// file location) - a single function so they can never diverge.
pub fn project_hash(root: &Path) -> Result<String> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    Ok(hash_of_canonical(&canonical))
}

/// The hash of an already-canonical path: sha256 of its lossy string form,
/// first 8 bytes, lowercase hex. Changing it orphans every existing index.
pub(crate) fn hash_of_canonical(canonical: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The canonical project root a state directory belongs to, written beside
/// the `daemon.pid`/`daemon.build` files already there.
///
/// [`project_hash`] is one-way and everything inside the index is stored
/// relative to the root (`indexed_files.filePath` is
/// `src/thing.ts`, never an absolute path), so without this file a state
/// directory cannot say which project it is for. `cli::clean`'s `orphaned`
/// target is the reason that matters: state whose project has been deleted is
/// what actually accumulates, and it cannot be recognized without knowing
/// where the project was.
pub const ROOT_FILE: &str = "project.root";

/// The device id (`st_dev`) of the filesystem the project root was on when
/// [`record_project_root`] last ran, beside [`ROOT_FILE`]. Unix only.
///
/// It is what lets `cli::clean::root_state` tell a deleted directory from an
/// unmounted volume: a deleted root leaves its parent on the root's own
/// device, while an unmounted volume leaves a parent on another one.
pub const DEVICE_FILE: &str = "project.dev";

/// Records `root` as the project `state_dir` holds the state for.
///
/// Idempotent - rewritten every time the state directory is opened, which is
/// also how a directory created before this file existed acquires one.
///
/// The path is canonicalized here rather than trusted from the caller, so
/// that what is written is the same form [`project_hash`] hashed - two
/// spellings of one root (a relative path, a symlink, a trailing slash) must
/// not produce a state directory that disagrees with itself about where its
/// project is.
///
/// On Unix the root's device id is also written, to [`DEVICE_FILE`]. It is a
/// sidecar file and not a second line of `project.root` on purpose: an older
/// g-mesh reads the whole of `project.root` as the path, so a second line
/// would turn the path into something whose parent exists and let that
/// binary delete a live project.
pub fn record_project_root(state_dir: &Path, root: &Path) -> Result<()> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    let path = state_dir.join(ROOT_FILE);
    fs::write(&path, canonical.to_string_lossy().as_bytes())
        .with_context(|| format!("failed to record the project root at {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = fs::metadata(&canonical)
            .with_context(|| format!("failed to stat project root {}", canonical.display()))?
            .dev();
        let dev_path = state_dir.join(DEVICE_FILE);
        fs::write(&dev_path, dev.to_string().as_bytes())
            .with_context(|| format!("failed to record the project device at {}", dev_path.display()))?;
    }
    Ok(())
}

/// The device id [`record_project_root`] wrote, or `None` if there is none
/// (a directory recorded before [`DEVICE_FILE`] existed, a non-Unix platform,
/// an unreadable or unparsable file). `None` is "cannot be judged", the same
/// posture as [`read_project_root`].
pub fn read_root_device(state_dir: &Path) -> Option<u64> {
    fs::read_to_string(state_dir.join(DEVICE_FILE)).ok()?.trim().parse().ok()
}

/// The root [`record_project_root`] wrote, or `None` if this state directory
/// has none.
///
/// `None` is not an error: every directory indexed before `ROOT_FILE` existed
/// is in exactly that state, and callers are expected to treat it as "cannot
/// be judged" rather than to guess. An unreadable or empty file reads the
/// same way, for the same reason - a half-written root is not a fact worth
/// acting on when the action is a delete.
pub fn read_project_root(state_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(state_dir.join(ROOT_FILE)).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// `~/.g-mesh/projects/` (or `$G_MESH_HOME/projects/`), the one directory
/// every project's state lives under. Public because `cli::clean` enumerates
/// it rather than deriving a single project's path from a root the way
/// everything else here does.
pub fn projects_root() -> Result<PathBuf> {
    Ok(projects_root_under(&paths::g_mesh_home()?))
}

/// [`projects_root`] under the g-mesh home `home`.
pub(crate) fn projects_root_under(home: &Path) -> PathBuf {
    home.join("projects")
}

/// `~/.g-mesh/projects/<hash>/` for the given (canonicalized) project root.
/// Uses the same hash as the daemon's own socket/pid file location
/// ([`project_hash`]) so the two can never disagree.
pub fn project_dir(root: &Path) -> Result<PathBuf> {
    project_dir_under(&paths::g_mesh_home()?, root)
}

/// [`project_dir`] under the g-mesh home `home` rather than the one the
/// environment names, so a test can point it at a temp dir without writing
/// `G_MESH_HOME`, which every other thread in the process reads.
pub(crate) fn project_dir_under(home: &Path, root: &Path) -> Result<PathBuf> {
    Ok(projects_root_under(home).join(project_hash(root)?))
}

/// The project's state directory, created if absent and recording which
/// project root it belongs to.
///
/// The one place a state directory comes into existence, so that no path can
/// create one without also leaving `project.root` in it - which is what
/// `cli::clean orphaned` reads to tell state whose project was deleted from
/// state whose project is merely idle. `identity::record_project_root` is
/// idempotent, so an existing directory acquires the file the next time
/// anything opens it.
///
/// That claim was false until GM-255. `daemon::run` and
/// `shim::acquire_bootstrap_lock` both created the directory with a bare
/// `fs::create_dir_all` and no root file, and the shim is the *earliest* of
/// the three - so a project whose first contact was a shim got a state
/// directory with no identity. Nothing recovers it later: `project_hash` is
/// one-way, so `clean orphaned` can only class such a directory `Legacy` and
/// leave it alone. Measured on this machine's own test home, 707 of 775
/// directories were unsweepable for that reason.
///
/// So if a fourth call site ever needs a project directory, it goes through
/// here. A bare `create_dir_all` on this path is not a shortcut, it is a
/// directory that can never be cleaned up.
pub fn ensure_project_dir(root: &Path) -> Result<PathBuf> {
    ensure_project_dir_under(&paths::g_mesh_home()?, root)
}

/// [`ensure_project_dir`] under the g-mesh home `home` (see
/// [`project_dir_under`]).
pub(crate) fn ensure_project_dir_under(home: &Path, root: &Path) -> Result<PathBuf> {
    let dir = project_dir_under(home, root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;
    record_project_root(&dir, root)?;
    Ok(dir)
}

/// The temp-then-rename write behind `daemon::write_pid_file`, also used for the phase file.
pub(crate) fn write_state_file_atomic(path: &Path, contents: &str, what: &str) {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(err) = fs::write(&temporary, contents) {
        crate::log_line!("g-mesh daemon: failed to write {what} {}: {err}", temporary.display());
        return;
    }
    if let Err(err) = fs::rename(&temporary, path) {
        crate::log_line!("g-mesh daemon: failed to put {what} {} in place: {err}", path.display());
        let _ = fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_and_relative_forms_hash_identically_after_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let canonical_hash = project_hash(root).unwrap();
        assert_eq!(canonical_hash, project_hash(root).unwrap(), "the same path hashes identically");

        let with_trailing_slash = root.join("");
        assert_eq!(canonical_hash, project_hash(&with_trailing_slash).unwrap());

        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let via_relative_detour = sub.join("..");
        assert_eq!(canonical_hash, project_hash(&via_relative_detour).unwrap());
    }

    #[test]
    fn a_recorded_root_reads_back_as_the_canonical_path() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();

        record_project_root(state.path(), project.path()).unwrap();

        assert_eq!(read_project_root(state.path()), Some(project.path().canonicalize().unwrap()));
    }

    /// The spellings that [`project_hash`] deliberately collapses have to
    /// collapse here too, or a directory's recorded root would not match the
    /// hash naming it.
    #[test]
    fn a_relative_spelling_of_the_root_is_recorded_canonically() {
        let project = tempfile::tempdir().unwrap();
        let sub = project.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let state = tempfile::tempdir().unwrap();

        record_project_root(state.path(), &sub.join("..")).unwrap();

        assert_eq!(read_project_root(state.path()), Some(project.path().canonicalize().unwrap()));
    }

    /// Every state directory created before this file existed - which is all
    /// of them, on any machine that has run g-mesh before this release.
    #[test]
    fn a_directory_with_no_recorded_root_reads_as_none_rather_than_failing() {
        let state = tempfile::tempdir().unwrap();

        assert_eq!(read_project_root(state.path()), None);
    }

    /// An empty or whitespace-only file is a write that did not finish. It
    /// must read as "unknown", not as the root `""`, which would resolve to
    /// something and be deleted.
    #[test]
    fn a_blank_recorded_root_reads_as_none() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join(ROOT_FILE), "  \n").unwrap();

        assert_eq!(read_project_root(state.path()), None);
    }

    #[test]
    fn distinct_paths_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hashes = std::collections::HashSet::new();
        for i in 0..200 {
            let dir = tmp.path().join(format!("project-{i}"));
            std::fs::create_dir(&dir).unwrap();
            let hash = project_hash(&dir).unwrap();
            assert!(hashes.insert(hash), "hash collision at project-{i}");
        }
    }

    /// Test 6 (GM-565): recording a root records the device it is on, a
    /// later record replaces a stale value, and a missing or unparsable file
    /// reads as `None` rather than failing.
    #[cfg(unix)]
    #[test]
    fn recording_a_root_records_its_device() {
        use std::os::unix::fs::MetadataExt;
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let real = std::fs::metadata(project.path()).unwrap().dev();

        assert_eq!(read_root_device(state.path()), None, "nothing recorded yet");

        record_project_root(state.path(), project.path()).unwrap();
        assert_eq!(read_root_device(state.path()), Some(real));

        std::fs::write(state.path().join(DEVICE_FILE), real.wrapping_add(1).to_string()).unwrap();
        record_project_root(state.path(), project.path()).unwrap();
        assert_eq!(read_root_device(state.path()), Some(real), "a stale device was not replaced");

        std::fs::write(state.path().join(DEVICE_FILE), "not a number").unwrap();
        assert_eq!(read_root_device(state.path()), None);
    }
}
