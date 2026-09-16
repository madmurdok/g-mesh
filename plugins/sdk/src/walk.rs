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

use std::path::Path;

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
    let excluded: Vec<String> = BASELINE_EXCLUDED_DIRS
        .iter()
        .map(|dir| (*dir).to_string())
        .chain(exclude_dirs.iter().cloned())
        .collect();
    let claimed: Vec<String> = extensions.iter().map(|ext| ext.to_lowercase()).collect();

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

    let mut files = Vec::new();
    for entry in walker.build() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
