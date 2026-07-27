use std::path::Path;

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
