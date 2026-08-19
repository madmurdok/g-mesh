use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Deterministic identity for a project root, shared by every call site
/// that needs to agree on it (SQLite storage location, daemon socket/pid
/// file location) - a single function so they can never diverge.
pub fn project_hash(root: &Path) -> Result<String> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
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
pub fn record_project_root(state_dir: &Path, root: &Path) -> Result<()> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;
    let path = state_dir.join(ROOT_FILE);
    fs::write(&path, canonical.to_string_lossy().as_bytes())
        .with_context(|| format!("failed to record the project root at {}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_always_hashes_identically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        assert_eq!(project_hash(root).unwrap(), project_hash(root).unwrap());
    }

    #[test]
    fn trailing_slash_and_relative_forms_hash_identically_after_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let canonical_hash = project_hash(root).unwrap();

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
}
