//! The project walk: which files a plugin's `--bulk-index` mode turns into
//! `File` nodes.
//!
//! # Why the walk is the SDK's and not the extractor's
//!
//! It looks like the most language-specific thing a plugin does and is the
//! least: every language's answer is "the files with my extensions, minus
//! what git ignores, minus the directories my manifest excludes". Getting it
//! wrong is not a small error either - a walk that descends into
//! `node_modules` or `target` indexes an order of magnitude more files than
//! the project has, and one that honours `.gitignore` differently from core's
//! own watcher leaves the two disagreeing about which files exist.
//!
//! # The policy, exactly
//!
//! - **`.gitignore`, layered as git layers it**, from the project root down.
//!   `require_git` is off, so a fixture directory that is not a git
//!   repository still honours its own `.gitignore` - which the conformance
//!   kit needs, since it copies fixtures into a scratch directory with no
//!   `.git` in it.
//! - **`.gitignore` above the project root is not read** (`parents(false)`).
//!   A project is indexed the same way wherever it is checked out; whether
//!   someone's home directory ignores `*.log` is not a fact about it.
//! - **The user's global gitignore and `.ignore` files are not read.** Both
//!   are per machine, and an index that depends on them is an index two
//!   developers cannot compare.
//! - **Hidden files are not skipped for being hidden.** `.config/build.ts` is
//!   source. What is skipped is named explicitly, below.
//! - **[`BASELINE_EXCLUDED_DIRS`] plus the manifest's `exclude_dirs`**, by
//!   exact directory name at any depth. The baseline is the two directories
//!   that are never source in any language; everything else - `node_modules`,
//!   `vendor`, `target` - belongs to the plugin that knows its ecosystem, and
//!   is declared in its `plugin.toml` where core's watcher reads the same
//!   list (`[plugin.workspace] exclude_dirs`).
//! - **Symlinks are not followed.**
//!
//! That last one is a deliberate difference from the TS plugin, which does
//! follow them, behind a guard (`plugins/typescript/src/symlinks.ts`) against
//! cycles, double-indexing the same real directory twice, and links escaping
//! the project root. Following links is worth that machinery for JS, where a
//! workspace's own packages are routinely symlinked into `node_modules` and a
//! plugin that refused links would miss the project's own source. No language
//! this SDK is for has that convention, so the SDK takes the option with no
//! failure modes rather than re-implementing a guard for a case that has not
//! come up. A plugin that needs links followed should say so, and get the
//! guard rather than a flag.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::path::RelPath;

/// Directory names no walk ever descends into, whatever the manifest says.
///
/// Only the two that are never source in any language: git's own object store
/// (walking it is pure cost, and it is never in `.gitignore` because git does
/// not need to ignore itself) and Claude Code's session directory, which
/// holds whole copies of the project made for parallel agent sessions - real
/// source is never there, and a project's `.gitignore` has no reason to list
/// it. This is the same pair core's own baseline excludes, and the same split
/// the architecture doc's Go example makes: `vendor`/`testdata` are the
/// plugin's to declare, `.git` is not.
pub const BASELINE_EXCLUDED_DIRS: [&str; 2] = [".git", ".claude"];

/// Every file under `root` that this plugin claims, in sorted order.
///
/// `extensions` are lowercase and dot-prefixed, as a manifest spells them;
/// matching is case-insensitive, so `A.TS` is claimed by `.ts`.
/// `exclude_dirs` are exact directory names, matched at any depth, on top of
/// [`BASELINE_EXCLUDED_DIRS`].
///
/// Sorted, not merely deterministic-in-practice: the order files are walked
/// in is the order their nodes reach core, and a walk whose order varies
/// between two runs of the same tree makes every "these two runs agree"
/// failure unreproducible.
pub fn walk_project(root: &Path, extensions: &[String], exclude_dirs: &[String]) -> Vec<RelPath> {
    let claimed: Vec<String> = extensions.iter().map(|ext| ext.to_lowercase()).collect();

    let mut files = Vec::new();
    for entry in walker(root, exclude_dirs).build() {
        // An unreadable directory contributes nothing rather than failing the
        // walk: a project with one permission-denied subdirectory still has an
        // index worth having, and the alternative is no index at all.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
            continue;
        }
        let Some(path) = RelPath::relative_to(root, entry.path()) else { continue };
        if path.extension().is_some_and(|extension| claimed.contains(&extension)) {
            files.push(path);
        }
    }
    files
}

/// The most entries [`walk_scope`] returns, [`WalkScope::pruned`] and
/// [`WalkScope::exclude_dirs`] together.
pub const MAX_SCOPE_ENTRIES: usize = 1_000;

/// What [`walk_project`] leaves out of a project, in a form a language server
/// can be told as its own exclude list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkScope {
    /// Directories the walk did not enter because `.gitignore` ignores them,
    /// top-most only: nothing under a listed directory is listed. Shallowest
    /// first, then by path.
    pub pruned: Vec<RelPath>,
    /// The named excludes, [`BASELINE_EXCLUDED_DIRS`] then the caller's, as
    /// bare directory names that apply at any depth.
    pub exclude_dirs: Vec<String>,
    /// How many pruned directories were left out to stay within
    /// [`MAX_SCOPE_ENTRIES`]; the deepest are the ones left out.
    pub dropped: usize,
}

/// The directories [`walk_project`] with the same `root` and `exclude_dirs`
/// declines to enter.
///
/// Built from the same walker as [`walk_project`], so the two agree on every
/// `.gitignore` rule. A symlink is never listed (the walk does not follow
/// one), and neither is a directory skipped by name, which is reported once
/// in `exclude_dirs` instead.
pub fn walk_scope(root: &Path, exclude_dirs: &[String]) -> WalkScope {
    let excluded = excluded_names(exclude_dirs);

    let entered: HashSet<PathBuf> = walker(root, exclude_dirs)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 0 || entry.file_type().is_some_and(|file_type| file_type.is_dir()))
        .map(|entry| entry.into_path())
        .collect();

    let mut pruned = Vec::new();
    for dir in &entered {
        let Ok(children) = fs::read_dir(dir) else { continue };
        for child in children.filter_map(Result::ok) {
            // `DirEntry::file_type` does not follow a symlink, so a link to a
            // directory is not a directory here.
            if !child.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                continue;
            }
            if excluded.iter().any(|name| child.file_name() == name.as_str()) {
                continue;
            }
            let path = child.path();
            if entered.contains(&path) {
                continue;
            }
            if let Some(relative) = RelPath::relative_to(root, &path) {
                pruned.push(relative);
            }
        }
    }
    pruned.sort_by(|a, b| depth(a).cmp(&depth(b)).then_with(|| a.cmp(b)));

    let room = MAX_SCOPE_ENTRIES.saturating_sub(excluded.len());
    let dropped = pruned.len().saturating_sub(room);
    pruned.truncate(room);
    WalkScope { pruned, exclude_dirs: excluded, dropped }
}

fn depth(path: &RelPath) -> usize {
    path.as_str().split('/').count()
}

fn excluded_names(exclude_dirs: &[String]) -> Vec<String> {
    let mut names: Vec<String> = BASELINE_EXCLUDED_DIRS.iter().map(|dir| (*dir).to_string()).collect();
    for dir in exclude_dirs {
        if !names.contains(dir) {
            names.push(dir.clone());
        }
    }
    names
}

/// The walker both [`walk_project`] and [`walk_scope`] run: the policy in this
/// module's doc, and nothing else.
fn walker(root: &Path, exclude_dirs: &[String]) -> WalkBuilder {
    let excluded = excluded_names(exclude_dirs);
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b));
    walker.filter_entry(move |entry| {
        // Directories only: a *file* called `vendor` is a file, and
        // `exclude_dirs` is a list of directory names. `file_type()` is `None`
        // only for the root itself, which is never excluded.
        let is_dir = entry.file_type().is_some_and(|file_type| file_type.is_dir());
        !is_dir || !excluded.iter().any(|name| entry.file_name() == name.as_str())
    });
    walker
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree, removed on drop. `tempfile` is not a dependency of
    /// this crate for the reason `testing`'s own `Scratch` gives.
    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("g-mesh-sdk-walk-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, contents).unwrap();
        }

        fn walk(&self, exclude: &[&str]) -> Vec<String> {
            let extensions = vec![".toy".to_string()];
            let exclude: Vec<String> = exclude.iter().map(|dir| (*dir).to_string()).collect();
            walk_project(&self.0, &extensions, &exclude)
                .into_iter()
                .map(|path| path.as_str().to_string())
                .collect()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn claims_only_its_own_extensions_case_insensitively() {
        let tree = Tree::new("extensions");
        tree.write("a.toy", "");
        tree.write("b.TOY", "");
        tree.write("c.rs", "");
        tree.write("d", "");
        assert_eq!(tree.walk(&[]), vec!["a.toy", "b.TOY"]);
    }

    #[test]
    fn descends_into_hidden_directories_but_never_into_the_baseline_ones() {
        let tree = Tree::new("hidden");
        tree.write(".config/a.toy", "");
        tree.write(".git/b.toy", "");
        tree.write(".claude/worktrees/agent-x/c.toy", "");
        assert_eq!(tree.walk(&[]), vec![".config/a.toy"]);
    }

    #[test]
    fn skips_the_manifests_excluded_directories_at_any_depth() {
        let tree = Tree::new("exclude");
        tree.write("a.toy", "");
        tree.write("vendor/b.toy", "");
        tree.write("src/nested/vendor/c.toy", "");
        tree.write("src/d.toy", "");
        assert_eq!(tree.walk(&["vendor"]), vec!["a.toy", "src/d.toy"]);
        // ...and only when the name is a directory's.
        let tree = Tree::new("exclude-file");
        tree.write("vendor.toy", "");
        assert_eq!(tree.walk(&["vendor.toy"]), vec!["vendor.toy"]);
    }

    /// The fixture case: a directory that is not a git repository still has
    /// its `.gitignore` honoured, layered from the root down, negations and
    /// all.
    #[test]
    fn honours_gitignore_outside_a_git_repository_including_negations() {
        let tree = Tree::new("gitignore");
        tree.write(".gitignore", "generated/\n*.gen.toy\n");
        tree.write("a.toy", "");
        tree.write("a.gen.toy", "");
        tree.write("generated/b.toy", "");
        tree.write("src/.gitignore", "!keep.gen.toy\n");
        tree.write("src/keep.gen.toy", "");
        tree.write("src/drop.gen.toy", "");
        assert_eq!(tree.walk(&[]), vec!["a.toy", "src/keep.gen.toy"]);
    }

    #[test]
    fn is_sorted_and_therefore_reproducible() {
        let tree = Tree::new("order");
        for name in ["z.toy", "a.toy", "m/n.toy", "m/a.toy"] {
            tree.write(name, "");
        }
        let once = tree.walk(&[]);
        assert_eq!(once, vec!["a.toy", "m/a.toy", "m/n.toy", "z.toy"]);
        assert_eq!(once, tree.walk(&[]));
    }

    fn scope(tree: &Tree, exclude: &[&str]) -> WalkScope {
        let exclude: Vec<String> = exclude.iter().map(|dir| (*dir).to_string()).collect();
        walk_scope(&tree.0, &exclude)
    }

    /// Every `.toy` file on disk, relative and `/`-separated, found without
    /// the walker.
    fn every_claimed_file(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for child in fs::read_dir(dir).unwrap().map(Result::unwrap) {
            let path = child.path();
            if child.file_type().unwrap().is_dir() {
                every_claimed_file(root, &path, out);
            } else if path.extension().is_some_and(|extension| extension == "toy") {
                out.push(RelPath::relative_to(root, &path).unwrap().as_str().to_string());
            }
        }
    }

    #[test]
    fn the_scope_lists_exactly_what_the_walk_leaves_out() {
        let tree = Tree::new("scope");
        tree.write(".gitignore", "build/\n");
        tree.write("build/a.toy", "");
        tree.write("build/sub/b.toy", "");
        tree.write("src/.gitignore", "gen/\n");
        tree.write("src/gen/c.toy", "");
        tree.write("src/d.toy", "");
        tree.write("lib/vendor/e.toy", "");
        tree.write("pkg/one/f.toy", "");
        tree.write("pkg/two/g.toy", "");
        tree.write("top.toy", "");
        tree.write(".git/h.toy", "");
        #[cfg(unix)]
        std::os::unix::fs::symlink(tree.0.join("pkg"), tree.0.join("link")).unwrap();

        let scope = scope(&tree, &["vendor"]);
        let pruned: Vec<&str> = scope.pruned.iter().map(RelPath::as_str).collect();
        assert_eq!(pruned, vec!["build", "src/gen"]);
        assert_eq!(scope.exclude_dirs, vec![".git", ".claude", "vendor"]);
        assert_eq!(scope.dropped, 0);

        let walked = tree.walk(&["vendor"]);
        let mut on_disk = Vec::new();
        every_claimed_file(&tree.0, &tree.0, &mut on_disk);
        for file in on_disk {
            let under_pruned = pruned.iter().any(|dir| file.starts_with(&format!("{dir}/")));
            let under_named = file
                .split('/')
                .rev()
                .skip(1)
                .any(|segment| scope.exclude_dirs.iter().any(|name| name == segment));
            let excluded = under_pruned || under_named;
            assert_ne!(walked.contains(&file), excluded, "{file}: walked and excluded must disagree");
        }
    }

    #[test]
    fn the_scope_keeps_the_shallowest_entries_within_the_cap() {
        let tree = Tree::new("scope-cap");
        tree.write(".gitignore", "ign*/\n");
        fs::create_dir_all(tree.0.join("ignroot")).unwrap();
        for index in 0..1_005 {
            fs::create_dir_all(tree.0.join(format!("a/ign{index:04}"))).unwrap();
        }

        let scope = scope(&tree, &[]);
        assert_eq!(scope.pruned.len() + scope.exclude_dirs.len(), MAX_SCOPE_ENTRIES);
        assert_eq!(scope.dropped, 1_006 - (MAX_SCOPE_ENTRIES - BASELINE_EXCLUDED_DIRS.len()));
        assert_eq!(scope.pruned[0].as_str(), "ignroot", "the depth-1 directory is kept, and first");
    }
}
