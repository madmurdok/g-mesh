//! Keys for project-relative paths that must match a saved file's path as
//! the watcher reports it, whatever case a plugin spelled them in.
//!
//! On a case-insensitive volume (macOS APFS and Windows NTFS by default) a
//! plugin may declare `Base.json` while the file on disk, or the one created
//! later, is `base.json`; both name the same file, so the keys are
//! lowercased on such a volume, both where they are stored and where a
//! settled path is looked up. On a case-sensitive volume the keys stay
//! exact: `Base.json` and `base.json` are two files there.
//!
//! The volume is probed once per project root per process ([`folds_case`]).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// `path`'s key under `project_root`: lowercased when the project's volume
/// is case-insensitive, unchanged otherwise.
pub(crate) fn key(project_root: &Path, path: &str) -> String {
    fold_key(folds_case(project_root), path)
}

/// [`key`] of each of `paths`, sorted and deduplicated (two spellings of one
/// file on a case-insensitive volume are one key).
pub(crate) fn keys(project_root: &Path, paths: &[String]) -> Vec<String> {
    fold_keys(folds_case(project_root), paths)
}

fn fold_key(fold: bool, path: &str) -> String {
    if fold {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

fn fold_keys(fold: bool, paths: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = paths.iter().map(|path| fold_key(fold, path)).collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Whether `project_root` sits on a case-insensitive volume. Probed on the
/// first call for a root and remembered for the rest of the process; a probe
/// that cannot tell answers `false` (exact keys).
pub(crate) fn folds_case(project_root: &Path) -> bool {
    static PROBED: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let probed = PROBED.get_or_init(Default::default);
    if let Some(&fold) = probed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(project_root) {
        return fold;
    }
    let fold = probe(project_root);
    probed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(project_root.to_path_buf(), fold);
    fold
}

/// Looks up one name whose ASCII letters are case-swapped and asks whether
/// it reaches the very same file. The name is the first entry of the root
/// with an ASCII letter, so the probe runs on the root's own volume; a root
/// with no such entry probes its own last path component instead.
fn probe(project_root: &Path) -> bool {
    let entry = fs::read_dir(project_root).ok().and_then(|entries| {
        entries.filter_map(Result::ok).map(|entry| entry.file_name()).find_map(|name| {
            let name = name.to_str()?.to_string();
            let swapped = swap_ascii_case(&name)?;
            Some((project_root.join(name), project_root.join(swapped)))
        })
    });
    let pair = entry.or_else(|| {
        let root = project_root.canonicalize().ok()?;
        let name = root.file_name()?.to_str()?.to_string();
        let swapped = swap_ascii_case(&name)?;
        Some((root.clone(), root.with_file_name(swapped)))
    });
    match pair {
        Some((original, swapped)) => same_file(&original, &swapped),
        None => false,
    }
}

/// `name` with every ASCII letter's case swapped, `None` when it has none.
fn swap_ascii_case(name: &str) -> Option<String> {
    if !name.bytes().any(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    Some(
        name.chars()
            .map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() })
            .collect(),
    )
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    fs::symlink_metadata(a).is_ok() && fs::symlink_metadata(b).is_ok()
}

#[cfg(test)]
mod tests;
