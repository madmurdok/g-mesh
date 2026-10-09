//! Core's own walk of a project tree: the one place that says which tree
//! "the project" is when core, not a plugin, has to look at it.
//!
//! Two callers, each with its own per-file filter: `g-mesh status`'s coverage
//! walk (files some discovered plugin would index) and the bulk walk's count
//! of files belonging to absent plugins (`languages::count_absent_files`,
//! [ADR 0021](../../docs/adr/0021-per-language-bulk-outcome.md)). They share
//! this walker so they can never disagree about which directories exist.
//!
//! It mirrors the plugins' walks (`plugins/sdk/src/walk.rs`), which run in
//! their own processes: each directory's own `.gitignore` and nothing else,
//! [`BASELINE_EXCLUDED_DIRS`] plus the caller's directory names pruned at any
//! depth. It diverges only toward doing less (no symlinks followed, unreadable
//! entries skipped). The plugins do follow links, but list a file the plain
//! walk reaches under its direct spelling, so the one difference is files
//! reachable only through a link, which this walk does not see
//! ([ADR 0025](../../docs/adr/0025-project-walk-follows-symlinks.md)).

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::watcher::BASELINE_EXCLUDED_DIRS;

/// One regular file the walk found.
pub struct WalkedFile {
    /// The file's path as the walk reached it (under the root it was given).
    pub path: PathBuf,
    /// Project-relative, forward-slash separated - the same spelling the
    /// `filePath` columns and the wire protocol use.
    pub relative: String,
}

/// Every regular file under `root`, honoring each directory's `.gitignore`,
/// with [`BASELINE_EXCLUDED_DIRS`] and every directory named in `pruned`
/// skipped outright at any depth. Filtering files one by one is the caller's.
pub fn project_files(root: &Path, pruned: &[String]) -> impl Iterator<Item = WalkedFile> {
    walked_files(root, walk_builder(root, pruned, false, None))
}

/// [`project_files`], restricted to the files under `subtrees`
/// (project-relative directories, forward-slash separated; `""` is the whole
/// project). The walk still descends from `root` through each subtree's
/// ancestors, so their `.gitignore` files apply exactly as in the full walk;
/// it only skips every directory that is neither an ancestor of a subtree
/// nor inside one, and the files of the ancestors themselves.
pub fn project_files_under(
    root: &Path,
    pruned: &[String],
    subtrees: &[String],
) -> impl Iterator<Item = WalkedFile> {
    let subtrees: Vec<PathBuf> =
        subtrees.iter().map(|dir| if dir.is_empty() { root.to_path_buf() } else { root.join(dir) }).collect();
    walked_files(root, walk_builder(root, pruned, false, Some(subtrees)))
}

fn walked_files(root: &Path, builder: WalkBuilder) -> impl Iterator<Item = WalkedFile> {
    let walk = builder.build();
    let root = root.to_path_buf();
    walk.filter_map(move |entry| {
        // An unreadable directory costs its subtree, not the walk.
        let entry = entry.ok()?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            return None;
        }
        let relative = relative_wire_path(&root, entry.path())?;
        Some(WalkedFile { path: entry.into_path(), relative })
    })
}

/// Every `.gitignore` the walk would read: one per directory the walk enters
/// (so none inside an ignored or pruned directory), as a [`WalkedFile`].
pub fn gitignore_files(root: &Path, pruned: &[String]) -> impl Iterator<Item = WalkedFile> {
    let walk = project_walk_builder(root, pruned, true).build();
    let root = root.to_path_buf();
    walk.filter_map(move |entry| {
        let entry = entry.ok()?;
        if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
            return None;
        }
        let path = entry.path().join(".gitignore");
        if !path.is_file() {
            return None;
        }
        let relative = relative_wire_path(&root, &path)?;
        Some(WalkedFile { path, relative })
    })
}

/// `absolute` relative to `root`, forward-slash separated; `None` outside
/// `root` or for a non-UTF-8 component.
fn relative_wire_path(root: &Path, absolute: &Path) -> Option<String> {
    let relative = absolute.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        parts.push(component.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// The walker behind [`project_files`], shared with the watcher's
/// [`IgnoreLayers`](crate::watcher::ignore_layers::IgnoreLayers) so the two can
/// never disagree about which `.gitignore` files apply or how. With
/// `directories_only`, files are dropped: the caller wants the directories the
/// walk would enter, not their files.
pub(crate) fn project_walk_builder(root: &Path, pruned: &[String], directories_only: bool) -> WalkBuilder {
    walk_builder(root, pruned, directories_only, None)
}

/// [`project_walk_builder`], optionally kept to `subtrees` (absolute, under
/// `root`): a directory is entered only when it is an ancestor of a subtree
/// or inside one, and a file is kept only inside one. One `filter_entry`
/// holds both rules, as the builder keeps only the last filter it is given.
fn walk_builder(
    root: &Path,
    pruned: &[String],
    directories_only: bool,
    subtrees: Option<Vec<PathBuf>>,
) -> WalkBuilder {
    let pruned: Vec<String> =
        BASELINE_EXCLUDED_DIRS.iter().map(|dir| (*dir).to_string()).chain(pruned.iter().cloned()).collect();
    let mut builder = WalkBuilder::new(root);
    // Matching the plugins' walks, which read each directory's own
    // .gitignore and nothing else: no dotfile skipping, no rules from
    // above the project root, no global/`info/exclude` rules, and rules
    // honored even outside a git repository.
    builder
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let in_scope = |path: &Path| match &subtrees {
                None => true,
                Some(subtrees) => subtrees
                    .iter()
                    .any(|subtree| path.starts_with(subtree) || (is_dir && subtree.starts_with(path))),
            };
            if !is_dir {
                return !directories_only && in_scope(entry.path());
            }
            !entry.file_name().to_str().is_some_and(|name| pruned.iter().any(|dir| dir == name))
                && in_scope(entry.path())
        });
    builder
}
