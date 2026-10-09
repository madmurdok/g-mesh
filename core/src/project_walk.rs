//! Core's own walk of a project tree: the one place that says which tree
//! "the project" is when core, not a plugin, has to look at it.
//!
//! Its callers: `g-mesh status`'s coverage walk (files some discovered plugin
//! would index), the bulk walk's count of files belonging to absent plugins
//! (`languages::count_absent_files`,
//! [ADR 0021](../../docs/adr/0021-per-language-bulk-outcome.md)), the
//! `.gitignore` gate's comparison (`PluginRegistry::gitignore_changed`), and
//! the watcher's layers and link table (`watcher::ignore_layers`).
//!
//! It is the plugins' walk: `g_mesh_walk`, which the SDK runs too, so `.gitignore`
//! layering, the excluded names and the symlink guard are one implementation
//! ([ADR 0025](../../docs/adr/0025-project-walk-follows-symlinks.md)). A file
//! reachable only through a followed link (its target gitignored) is listed
//! under the link's spelling, as the plugins index it; a file reachable both
//! ways under its plain spelling. What differs from a plugin's walk is the
//! excluded names: core prunes [`BASELINE_EXCLUDED_DIRS`] plus the names its
//! caller passes (the ones *every* language excludes), so it can follow a
//! link one language refuses as an excluded target; the per-language check is
//! the caller's, on both spellings ([`WalkedFile::real_relative`]).
//!
//! [`LinkTable`] is what became of every link, directory or file, as the watcher needs
//! it: an event the OS reports under one spelling is turned into the spelling
//! the index holds ([`LinkTable::to_indexed`]).

use std::path::{Path, PathBuf};

use g_mesh_walk::{Link, LinkOutcome, LinkRefusal, WalkedEntry};
use ignore::WalkBuilder;

use crate::watcher::BASELINE_EXCLUDED_DIRS;

/// One regular file the walk found.
pub struct WalkedFile {
    /// The file's path as the walk reached it (under the root it was given).
    pub path: PathBuf,
    /// Project-relative, forward-slash separated - the same spelling the
    /// `filePath` columns and the wire protocol use.
    pub relative: String,
    /// The file's spelling relative to the root's real path, when a followed
    /// link is above it (so the two differ); `None` when the walk reached it
    /// without a link.
    pub real_relative: Option<String>,
}

impl From<WalkedEntry> for WalkedFile {
    fn from(entry: WalkedEntry) -> Self {
        WalkedFile { path: entry.path, relative: entry.relative, real_relative: entry.real_relative }
    }
}

/// Every regular file under `root`, one per real file, honoring each
/// directory's `.gitignore`, with [`BASELINE_EXCLUDED_DIRS`] and every
/// directory named in `pruned` skipped outright at any depth, and links
/// followed behind the guard. Filtering files one by one is the caller's.
pub fn project_files(root: &Path, pruned: &[String]) -> impl Iterator<Item = WalkedFile> {
    g_mesh_walk::walk(root, &excluded_names(pruned)).files.into_iter().map(WalkedFile::from)
}

/// [`project_files`], restricted to the files under `subtrees`
/// (project-relative directories, forward-slash separated; `""` is the whole
/// project), listed under the spellings [`project_files`] lists them.
///
/// Without links in reach this walk descends from `root` through each
/// subtree's ancestors only, so their `.gitignore` files apply exactly as in
/// the full walk; it skips every directory that is neither an ancestor of a
/// subtree nor inside one, and the files of the ancestors themselves. Pruned
/// that way, the guard could miss the plain spelling of a link's target and
/// list an aliased file under the link; so when the pruned walk meets any
/// symlink, the whole project is walked and its files filtered to the
/// subtrees instead.
pub fn project_files_under(
    root: &Path,
    pruned: &[String],
    subtrees: &[String],
) -> impl Iterator<Item = WalkedFile> {
    let files = match plain_files_under(root, pruned, subtrees) {
        Some(files) => files,
        None => project_files(root, pruned).filter(|file| under_any(&file.relative, subtrees)).collect(),
    };
    files.into_iter()
}

/// Whether `relative` lies in one of `subtrees` (`""` is the whole project).
fn under_any(relative: &str, subtrees: &[String]) -> bool {
    subtrees.iter().any(|subtree| {
        subtree.is_empty()
            || relative
                .strip_prefix(subtree.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// The pruned walk behind [`project_files_under`], following no link; `None`
/// as soon as it meets a symlink in reach (inside a subtree, or one of a
/// subtree's ancestors), whose resolution needs the whole tree.
fn plain_files_under(root: &Path, pruned: &[String], subtrees: &[String]) -> Option<Vec<WalkedFile>> {
    let excluded = excluded_names(pruned);
    let subtrees: Vec<PathBuf> =
        subtrees.iter().map(|dir| if dir.is_empty() { root.to_path_buf() } else { root.join(dir) }).collect();
    let mut builder = WalkBuilder::new(root);
    // The policy `g_mesh_walk` documents, minus link following.
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
            if is_dir && entry.file_name().to_str().is_some_and(|name| excluded.iter().any(|dir| dir == name))
            {
                return false;
            }
            // A link may stand for a directory: kept when it is a subtree's
            // ancestor too, so the walk sees it and gives up.
            let may_hold = is_dir || entry.path_is_symlink();
            let path = entry.path();
            subtrees
                .iter()
                .any(|subtree| path.starts_with(subtree) || (may_hold && subtree.starts_with(path)))
        });
    let mut files = Vec::new();
    for entry in builder.build() {
        // An unreadable directory costs its subtree, not the walk.
        let Ok(entry) = entry else { continue };
        if entry.path_is_symlink() {
            return None;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(relative) = relative_wire_path(root, entry.path()) else { continue };
        files.push(WalkedFile { path: entry.into_path(), relative, real_relative: None });
    }
    Some(files)
}

/// Every `.gitignore` the walk would read: one per directory the walk enters
/// (so none inside an ignored or pruned directory, and those under a followed
/// link spelled through it), as a [`WalkedFile`].
pub fn gitignore_files(root: &Path, pruned: &[String]) -> impl Iterator<Item = WalkedFile> {
    g_mesh_walk::walk_dirs(root, &excluded_names(pruned)).dirs.into_iter().filter_map(|dir| {
        let path = dir.path.join(GITIGNORE);
        if !path.is_file() {
            return None;
        }
        Some(WalkedFile {
            path,
            relative: join_wire(&dir.relative, GITIGNORE),
            real_relative: dir.real_relative.map(|real| join_wire(&real, GITIGNORE)),
        })
    })
}

const GITIGNORE: &str = ".gitignore";

fn join_wire(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// [`BASELINE_EXCLUDED_DIRS`] and `pruned`: the names every walk here skips.
pub(crate) fn excluded_names(pruned: &[String]) -> Vec<String> {
    BASELINE_EXCLUDED_DIRS.iter().map(|dir| (*dir).to_string()).chain(pruned.iter().cloned()).collect()
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

/// The absolute path the dangling link at `link` names, as an event for it
/// would spell it once it exists: its parent's real path (when that exists)
/// joined with its name, else the lexical join with `.`/`..` resolved.
fn dangling_target(link: &Path) -> Option<PathBuf> {
    let named = link.parent()?.join(std::fs::read_link(link).ok()?);
    let mut lexical = PathBuf::new();
    for part in named.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    let name = lexical.file_name()?.to_os_string();
    match lexical.parent().and_then(|parent| std::fs::canonicalize(parent).ok()) {
        Some(parent) => Some(parent.join(name)),
        None => Some(lexical),
    }
}

/// What [`LinkTable::to_indexed`] makes of a path an event reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remap {
    /// The path is the spelling the walk lists.
    Keep,
    /// The walk lists the same file (or directory) under this path.
    To(PathBuf),
    /// The path lies under a link the walk refused: the walk lists nothing
    /// there.
    Drop,
}

/// Every link one walk met (`g_mesh_walk::walk_dirs`), directory and file
/// links alike, as the rules that turn a reported path into the spelling the
/// walk lists. Paths are absolute: link spellings under the root the walk was
/// given, targets real. A directory link's rules cover everything under their
/// path; a file link's match its path exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkTable {
    root: PathBuf,
    links: Vec<Link>,
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// A path at or under this prefix is the rule's (only this path itself
    /// when `exact`).
    prefix: PathBuf,
    /// A file link's rule: whole-path match, never a prefix.
    exact: bool,
    /// The prefix is a link's own spelling: the link itself (created,
    /// removed, retargeted) is reported as spelled.
    is_link: bool,
    /// `Keep`, `To(base)` (the prefix replaced by `base`) or `Drop`.
    remap: Remap,
}

impl LinkTable {
    /// The table for `links` met by a walk of `root`.
    pub fn new(root: &Path, links: Vec<Link>) -> Self {
        let mut rules = Vec::new();
        for link in &links {
            let spelled = root.join(&link.at);
            let exact = !link.is_dir;
            let rule = |prefix: PathBuf, is_link: bool, remap: Remap| Rule { prefix, exact, is_link, remap };
            match &link.outcome {
                // The target is reached only through this link: its real
                // spelling (reported by FSEvents, and by inotify for the
                // watch it added last) becomes the link's.
                LinkOutcome::Followed => {
                    if let Some(real) = &link.real {
                        rules.push(rule(real.clone(), false, Remap::To(spelled.clone())));
                    }
                    rules.push(rule(spelled, true, Remap::Keep));
                }
                LinkOutcome::Duplicate(winner) => {
                    rules.push(rule(spelled, true, Remap::To(root.join(winner))));
                }
                LinkOutcome::Aliases(plain) => {
                    let plain = if plain.is_empty() { root.to_path_buf() } else { root.join(plain) };
                    rules.push(rule(spelled, true, Remap::To(plain)));
                }
                // A dangling file link names no file yet: its own path is
                // kept (a deletion through it still reaches the index), and
                // the path it names becomes its spelling, so creating the
                // target reaches the link (and the reload that follows it).
                LinkOutcome::Refused(LinkRefusal::Dangling) if exact => {
                    if let Some(target) = dangling_target(&spelled) {
                        rules.push(rule(target, false, Remap::To(spelled.clone())));
                    }
                    rules.push(rule(spelled, true, Remap::Keep));
                }
                LinkOutcome::Refused(_) => rules.push(rule(spelled, true, Remap::Drop)),
            }
        }
        // Longest prefix first, so the first match is the most specific.
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.prefix.components().count()));
        LinkTable { root: root.to_path_buf(), links, rules }
    }

    /// The links the table was built from, in the order the walk met them.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// The spelling the walk lists for `path` (absolute), by the most
    /// specific rule that covers it: under a followed link's real target ->
    /// the link's spelling; under a duplicate link -> the winning link's;
    /// under an aliasing link -> the plain spelling; under a refused link ->
    /// dropped. A file link's rules match only its target or its own path,
    /// whole. A link's own path is kept as spelled, so its creation or
    /// removal reaches whoever reloads the table; [`Self::file_link_to_indexed`]
    /// then places a file link's own path.
    pub fn to_indexed(&self, path: &Path) -> Remap {
        let Some(rule) = self.rule_for(path) else {
            return Remap::Keep;
        };
        let Ok(suffix) = path.strip_prefix(&rule.prefix) else { return Remap::Keep };
        if rule.is_link && suffix.as_os_str().is_empty() {
            return Remap::Keep;
        }
        match &rule.remap {
            Remap::Keep => Remap::Keep,
            Remap::Drop => Remap::Drop,
            Remap::To(base) => {
                let to = if suffix.as_os_str().is_empty() { base.clone() } else { base.join(suffix) };
                if to == path {
                    Remap::Keep
                } else {
                    Remap::To(to)
                }
            }
        }
    }

    /// The spelling the walk lists for `path` when it is a file link's own
    /// path: a file link *is* the file, so unlike a directory link's path
    /// (which [`Self::to_indexed`] keeps for the reload) it is placed as the
    /// walk places it: kept if followed, the winner's spelling if a duplicate,
    /// the plain spelling if an alias, dropped if refused. `Keep` for any
    /// other path. Applied to a settled batch, after any reload its own
    /// paths triggered.
    pub fn file_link_to_indexed(&self, path: &Path) -> Remap {
        match self.rules.iter().find(|rule| rule.exact && rule.is_link && rule.prefix == path) {
            Some(Rule { remap: Remap::To(to), .. }) if to != path => Remap::To(to.clone()),
            Some(Rule { remap: Remap::Drop, .. }) => Remap::Drop,
            _ => Remap::Keep,
        }
    }

    /// The most specific rule covering `path`.
    fn rule_for(&self, path: &Path) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| if rule.exact { path == rule.prefix } else { path.starts_with(&rule.prefix) })
    }

    /// Whether `path` is a link in this table, still naming what the table
    /// says it names (still a link, to the same real target or still
    /// dangling): an event on it then changes nothing the table holds. One
    /// `canonicalize`.
    pub fn holds(&self, path: &Path) -> bool {
        let Some(link) = self.links.iter().find(|link| self.root.join(&link.at) == path) else {
            return false;
        };
        path.is_symlink() && link.real == std::fs::canonicalize(path).ok()
    }

    /// Whether `path` is a link's spelling in this table or an ancestor of
    /// one (the root excluded): a change there may have changed the table.
    pub fn concerns(&self, path: &Path) -> bool {
        path != self.root && self.links.iter().any(|link| self.root.join(&link.at).starts_with(path))
    }

    /// The spellings (project-relative) of the links whose row differs
    /// between `self` and `newer`: created, removed, retargeted, or judged
    /// differently. Sorted, deduplicated.
    pub fn changed_links(&self, newer: &LinkTable) -> Vec<String> {
        let mut changed: Vec<String> = self
            .links
            .iter()
            .filter(|link| !newer.links.contains(link))
            .chain(newer.links.iter().filter(|link| !self.links.contains(link)))
            .map(|link| link.at.clone())
            .collect();
        changed.sort();
        changed.dedup();
        changed
    }
}

#[cfg(all(test, unix))]
mod tests {
    //! GM-514 (docs/architecture/gm-514-core-symlink-table.md, section 5): the
    //! core walk follows links as the plugins' walk does, and the link table
    //! turns every spelling an OS may report into the one the walk lists.
    //! The OS-independent half of B1-B3, B5, B7, B11, B12, B13 and B14; the
    //! watcher, status and gate tests build on it.

    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A canonical project root (macOS's `/var` is itself a link) and,
    /// outside it, a directory for refused targets.
    struct Tree {
        _root: tempfile::TempDir,
        root: PathBuf,
        _outside: tempfile::TempDir,
        outside: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let root_dir = tempfile::tempdir().unwrap();
            let outside_dir = tempfile::tempdir().unwrap();
            let root = root_dir.path().canonicalize().unwrap();
            let outside = outside_dir.path().canonicalize().unwrap();
            Self { _root: root_dir, root, _outside: outside_dir, outside }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        /// `relative` -> `target`, written as given (relative or absolute).
        fn link(&self, relative: &str, target: impl AsRef<Path>) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(target, path).unwrap();
        }

        fn at(&self, relative: &str) -> PathBuf {
            self.root.join(relative)
        }

        /// The table the watcher builds (`IgnoreLayers::load`'s walk).
        fn table(&self) -> LinkTable {
            LinkTable::new(&self.root, g_mesh_walk::walk_dirs(&self.root, &excluded_names(&[])).links)
        }

        /// `(relative, real_relative)` of every walked `.ts` file, sorted.
        fn ts_files(&self) -> Vec<(String, Option<String>)> {
            let mut files: Vec<_> = project_files(&self.root, &[])
                .filter(|file| file.relative.ends_with(".ts"))
                .map(|file| (file.relative, file.real_relative))
                .collect();
            files.sort();
            files
        }
    }

    /// `gen/` gitignored, reached only through `src/api -> ../gen`.
    fn alias_only() -> Tree {
        let tree = Tree::new();
        tree.write(".gitignore", "gen/\n");
        tree.write("gen/a.ts", "");
        tree.write("src/main.ts", "");
        tree.link("src/api", "../gen");
        tree
    }

    /// B4 (walk half): an alias-only file is listed under the link's
    /// spelling, with its real spelling beside it; nothing under `gen/`.
    ///
    /// Control: `.follow_links(false)` in `g_mesh_walk`'s walker
    /// (walk/src/lib.rs) - `src/api/a.ts` is missing.
    #[test]
    fn an_alias_only_file_is_listed_under_the_link_with_its_real_spelling() {
        let tree = alias_only();
        assert_eq!(
            tree.ts_files(),
            vec![
                ("src/api/a.ts".to_string(), Some("gen/a.ts".to_string())),
                ("src/main.ts".to_string(), None),
            ]
        );
    }

    /// B5 (walk half): a file reachable through a link and plainly is
    /// listed once, under its plain spelling, even when the link comes
    /// first in walk order (`app` sorts before `lib`).
    ///
    /// Control: skip the winner pass in `g_mesh_walk` (`LinkGuard::finish`
    /// keeps every walked entry) - `app/x.ts` is listed too.
    #[test]
    fn a_file_reachable_both_ways_is_listed_once_under_its_plain_spelling() {
        let tree = Tree::new();
        tree.write("lib/x.ts", "");
        tree.link("app", "lib");
        assert_eq!(tree.ts_files(), vec![("lib/x.ts".to_string(), None)]);
    }

    /// B1-B3 (guard half): an event spelled under the followed link's real
    /// target - an existing file (edit, delete) or a new one (create) -
    /// becomes the link's spelling; the link's own spelling and its path
    /// are kept as spelled; an unrelated path is untouched.
    ///
    /// Control: drop the `Followed` real-target rule in `LinkTable::new` -
    /// `gen/a.ts` is kept as spelled (and then dropped as ignored).
    #[test]
    fn a_path_under_a_followed_links_target_maps_to_the_link() {
        let tree = alias_only();
        let table = tree.table();

        assert_eq!(table.to_indexed(&tree.at("gen/a.ts")), Remap::To(tree.at("src/api/a.ts")));
        assert_eq!(
            table.to_indexed(&tree.at("gen/new/b.ts")),
            Remap::To(tree.at("src/api/new/b.ts")),
            "a file that does not exist yet maps too (a create)"
        );
        assert_eq!(table.to_indexed(&tree.at("src/api/a.ts")), Remap::Keep);
        assert_eq!(table.to_indexed(&tree.at("src/api")), Remap::Keep, "the link itself, for the reload");
        assert_eq!(table.to_indexed(&tree.at("src/main.ts")), Remap::Keep);
        assert_eq!(
            table.to_indexed(&tree.at("generated/a.ts")),
            Remap::Keep,
            "a sibling, not a prefix match"
        );
    }

    /// B12 (guard half): a second link to an alias-only target is a
    /// duplicate; its spelling (inotify's, when it added the last watch)
    /// becomes the winner's, and so does the real spelling.
    ///
    /// Control: drop the `Duplicate` rule in `LinkTable::new` -
    /// `src/dup/a.ts` is kept and indexed as a second row.
    #[test]
    fn a_path_under_a_duplicate_link_maps_to_the_winning_link() {
        let tree = alias_only();
        tree.link("src/dup", "../gen");
        let table = tree.table();

        assert_eq!(table.to_indexed(&tree.at("src/dup/a.ts")), Remap::To(tree.at("src/api/a.ts")));
        assert_eq!(table.to_indexed(&tree.at("gen/a.ts")), Remap::To(tree.at("src/api/a.ts")));
        assert_eq!(table.to_indexed(&tree.at("src/api/a.ts")), Remap::Keep);
    }

    /// An aliasing link's spelling maps to the plain one (the remap the SDK
    /// and Go also do, now in core for every language).
    #[test]
    fn a_path_under_an_aliasing_link_maps_to_the_plain_spelling() {
        let tree = Tree::new();
        tree.write("lib/x.ts", "");
        tree.link("app", "lib");
        let table = tree.table();

        assert_eq!(table.to_indexed(&tree.at("app/x.ts")), Remap::To(tree.at("lib/x.ts")));
        assert_eq!(table.to_indexed(&tree.at("lib/x.ts")), Remap::Keep);
    }

    /// B11 / M1 (guard half, every OS): a path spelled through a link the
    /// walk refused (target outside the root) is dropped - inotify reports
    /// such paths on Linux, and the walk indexes nothing there.
    ///
    /// Control: drop the `Refused(_)` -> `Drop` rule in `LinkTable::new` -
    /// `ext/b.ts` is kept and reaches the plugin (the ghost row).
    #[test]
    fn a_path_under_a_refused_link_is_dropped() {
        let tree = Tree::new();
        fs::write(tree.outside.join("b.ts"), "").unwrap();
        tree.link("ext", &tree.outside);
        tree.write("src/main.ts", "");
        let table = tree.table();

        assert_eq!(table.to_indexed(&tree.at("ext/b.ts")), Remap::Drop);
        assert_eq!(table.to_indexed(&tree.at("ext/nested/c.ts")), Remap::Drop);
        assert_eq!(table.to_indexed(&tree.at("src/main.ts")), Remap::Keep);
        assert!(project_files(&tree.root, &[]).all(|file| !file.relative.starts_with("ext/")));
    }

    /// B14 (guard half): an alias-only FILE link's target maps to the link,
    /// whole-path only; a dangling file link's named target maps to it too
    /// (creating it reaches the link); a duplicate file link is placed at
    /// the winner, a refused one dropped.
    ///
    /// Control: make every rule a prefix rule (`exact: false` in
    /// `LinkTable::new`) - `cfg/config.ts.bak` maps to the link.
    #[test]
    fn a_file_links_target_maps_to_the_link_and_its_own_path_is_placed() {
        let tree = Tree::new();
        tree.write(".gitignore", "cfg/\n");
        tree.write("cfg/config.ts", "");
        tree.write("cfg/shared.ts", "");
        fs::write(tree.outside.join("out.ts"), "").unwrap();
        tree.link("src/config.ts", "../cfg/config.ts");
        tree.link("src/a.ts", "../cfg/shared.ts");
        tree.link("src/b.ts", "../cfg/shared.ts");
        tree.link("src/later.ts", "../cfg/later.ts");
        tree.link("src/out.ts", tree.outside.join("out.ts"));
        let table = tree.table();

        assert_eq!(table.to_indexed(&tree.at("cfg/config.ts")), Remap::To(tree.at("src/config.ts")));
        assert_eq!(table.to_indexed(&tree.at("cfg/config.ts.bak")), Remap::Keep, "whole path, not a prefix");
        assert_eq!(table.to_indexed(&tree.at("cfg/shared.ts")), Remap::To(tree.at("src/a.ts")));
        assert_eq!(table.to_indexed(&tree.at("cfg/later.ts")), Remap::To(tree.at("src/later.ts")));

        assert_eq!(table.file_link_to_indexed(&tree.at("src/config.ts")), Remap::Keep);
        assert_eq!(table.file_link_to_indexed(&tree.at("src/b.ts")), Remap::To(tree.at("src/a.ts")));
        assert_eq!(table.file_link_to_indexed(&tree.at("src/a.ts")), Remap::Keep);
        assert_eq!(table.file_link_to_indexed(&tree.at("src/out.ts")), Remap::Drop);

        let mut listed: Vec<String> = project_files(&tree.root, &[])
            .map(|file| file.relative)
            .filter(|relative| relative.ends_with(".ts"))
            .collect();
        listed.sort();
        assert_eq!(listed, vec!["src/a.ts", "src/config.ts"], "the walk lists what the table maps to");
    }

    /// B7 (walk half): the pruned walk of a subtree holding a link to a
    /// non-ignored target outside it lists what the full walk lists there:
    /// the plain spelling wins, so nothing under the link.
    #[test]
    fn a_subtree_walk_lists_an_aliased_file_under_its_plain_spelling_only() {
        let tree = Tree::new();
        tree.write("lib/x.ts", "");
        tree.write("src/a.ts", "");
        tree.link("src/l", "../lib");
        let under: Vec<String> =
            project_files_under(&tree.root, &[], &["src".to_string()]).map(|file| file.relative).collect();
        assert_eq!(under, vec!["src/a.ts"]);
    }

    /// B13 (walk half): the subtree walk of an alias directory (a nested
    /// `.gitignore` edit inside the target is spelled there) lists the
    /// alias-only files under it.
    ///
    /// Control: in `plain_files_under`, `continue` instead of `return None`
    /// on a symlink (no fallback to the full walk) - the list is empty.
    #[test]
    fn a_subtree_walk_through_an_alias_only_link_lists_its_files() {
        let tree = alias_only();
        let under: Vec<(String, Option<String>)> =
            project_files_under(&tree.root, &[], &["src/api".to_string()])
                .map(|file| (file.relative, file.real_relative))
                .collect();
        assert_eq!(under, vec![("src/api/a.ts".to_string(), Some("gen/a.ts".to_string()))]);
    }

    /// B8 (table half): a link created, removed or judged differently is
    /// named by `changed_links`; an unchanged table names nothing.
    #[test]
    fn changed_links_names_created_removed_and_rejudged_links() {
        let tree = Tree::new();
        tree.write(".gitignore", "gen/\n");
        tree.write("gen/a.ts", "");
        let empty = tree.table();
        tree.link("src/api", "../gen");
        let followed = tree.table();
        assert_eq!(empty.changed_links(&followed), vec!["src/api"]);
        assert!(followed.changed_links(&tree.table()).is_empty());

        tree.write(".gitignore", "");
        let aliasing = tree.table();
        assert_eq!(followed.changed_links(&aliasing), vec!["src/api"], "Followed -> Aliases");

        fs::remove_file(tree.at("src/api")).unwrap();
        assert_eq!(aliasing.changed_links(&tree.table()), vec!["src/api"]);
    }
}
