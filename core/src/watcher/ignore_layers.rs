//! The watcher's `.gitignore` matcher, layered per directory the way the walks
//! that populate the index are (`project_walk::project_files`, the SDK's
//! `walker`): every directory's own `.gitignore`, the deepest decisive rule
//! winning, an ignored directory hiding its whole subtree. Before GM-508 the
//! watcher read only the root `.gitignore`, so a save under a directory a
//! nested `.gitignore` ignores was indexed although no walk would list it.
//!
//! The layers are re-read by [`IgnoreLayers::load`] whenever a `.gitignore`
//! may have changed; the watcher swaps the whole value.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;

use super::BASELINE_EXCLUDED_DIRS;
use crate::project_walk::project_walk_builder;

/// The file name every layer is read from.
pub const GITIGNORE_FILE_NAME: &str = ".gitignore";

/// One `.gitignore` matcher per directory that has one, keyed by that
/// directory's path under the (canonical) project root.
pub struct IgnoreLayers {
    root: PathBuf,
    layers: HashMap<PathBuf, Gitignore>,
}

impl IgnoreLayers {
    /// Reads every `.gitignore` the project walk would read: the walk's own
    /// options and pruning, so a `.gitignore` inside an ignored or baseline
    /// directory is not read here either (the walk never enters it). A file
    /// that fails to parse in part keeps its valid lines, as the walk's
    /// matcher does; the failure is logged.
    pub fn load(root: &Path) -> Self {
        let mut layers = HashMap::new();
        for entry in project_walk_builder(root, &[], true).build() {
            // An unreadable directory costs its subtree, as in the walk.
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                continue;
            }
            let dir = entry.into_path();
            let file = dir.join(GITIGNORE_FILE_NAME);
            if !file.is_file() {
                continue;
            }
            let mut builder = GitignoreBuilder::new(&dir);
            if let Some(err) = builder.add(&file) {
                crate::log_line!("g-mesh: {}: {err}", file.display());
            }
            match builder.build() {
                Ok(matcher) if !matcher.is_empty() => {
                    layers.insert(dir, matcher);
                }
                Ok(_) => {}
                Err(err) => crate::log_line!("g-mesh: {}: {err}", file.display()),
            }
        }
        Self { root: root.to_path_buf(), layers }
    }

    /// Whether the walk would leave `path` out: a [`BASELINE_EXCLUDED_DIRS`]
    /// directory or a `.gitignore`-ignored entry on the way to it, or `path`
    /// itself ignored. A path outside the root is not ignored (nothing here
    /// governs it).
    ///
    /// A path named `.gitignore` is ignored only when its directory is: a
    /// `.gitignore` that lists itself still governs its directory, and its
    /// edits must reach whoever reloads the layers.
    pub fn is_ignored(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let components: Vec<&OsStr> = relative.iter().collect();
        let Some(last) = components.len().checked_sub(1) else {
            return false;
        };
        let is_gitignore = path.file_name() == Some(OsStr::new(GITIGNORE_FILE_NAME));
        let mut current = self.root.clone();
        for (index, component) in components.into_iter().enumerate() {
            current.push(component);
            let is_last = index == last;
            if is_last && is_gitignore {
                break;
            }
            // An ancestor is a directory even when it is already gone (a
            // deleted subtree), the way the walk saw it.
            let is_dir = !is_last || current.is_dir();
            if is_dir && component.to_str().is_some_and(|name| BASELINE_EXCLUDED_DIRS.contains(&name)) {
                return true;
            }
            if self.matched(&current, is_dir) {
                return true;
            }
        }
        false
    }

    /// `path` against the layers of its ancestors, deepest first; the first
    /// layer with an opinion decides, so a nested `!keep.ts` re-includes what
    /// the root ignores and a nested pattern ignores what the root allows
    /// (`ignore::dir::Ignore::matched_ignore`, with `require_git(false)`).
    fn matched(&self, path: &Path, is_dir: bool) -> bool {
        for dir in path.ancestors().skip(1) {
            if let Some(layer) = self.layers.get(dir) {
                match layer.matched(path, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
            if dir == self.root {
                break;
            }
        }
        false
    }
}
