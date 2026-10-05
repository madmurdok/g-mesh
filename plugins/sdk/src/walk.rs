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
//! - **Symlinks are followed, behind a guard.** Why, and why these rules,
//!   is [ADR 0025](../../../docs/adr/0025-project-walk-follows-symlinks.md).
//!
//! # The guard's invariants
//!
//! - A link is judged only after `.gitignore` and the name excludes have
//!   let it through: an ignored or excluded link is never followed, and a
//!   gitignored *target* is not a reason to refuse one.
//! - A link is followed only when its target resolves, its real path is
//!   inside the root's real path, and no component of that path below the
//!   root is an excluded directory name. Anything else is refused and
//!   contributes nothing; the walk goes on.
//! - A directory is entered through a link at most once, and never through a
//!   link once it has been entered at all; a directory reached without a
//!   link is always entered. Nested links can still walk one real directory
//!   several times (each through a different entered directory), but every
//!   link is entered once, so the walk ends (walkdir's own ancestor check is
//!   the backstop) and files are deduplicated by real path.
//! - Every file appears once, keyed by its real path. Its spelling is the one
//!   with no followed link above it when the plain walk reaches it, and the
//!   first in walk order otherwise - sibling names never decide the identity
//!   of a file the plain walk reaches.
//! - Paths are relative to the root as given, even when the root itself is
//!   reached through a link; real paths are only ever compared with real
//!   paths.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

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
/// failure unreproducible. The order is component-wise path order.
///
/// A file reachable both directly and through a followed symlink is listed
/// once, under its direct spelling (this module's doc).
pub fn walk_project(root: &Path, extensions: &[String], exclude_dirs: &[String]) -> Vec<RelPath> {
    walk_project_detailed(root, extensions, exclude_dirs).files
}

/// What [`walk_project_detailed`] found: the files, and what became of every
/// symlink it met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedProject {
    /// Exactly [`walk_project`]'s answer.
    pub files: Vec<RelPath>,
    /// Every symlink the walk judged, in the order it met them.
    ///
    /// A link the walk never judged is absent: one `.gitignore` or a name
    /// exclude skipped, and a file link with an unclaimed extension. Two
    /// kinds are reported before those rules could run, because the
    /// directory iterator fails on them first: a dangling link, and a link to
    /// one of its own ancestors. A link under an excluded directory name is
    /// never reported: the walk does not enter excluded directories.
    pub links: Vec<WalkedLink>,
}

/// One symlink the walk met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedLink {
    /// The link's own path.
    pub at: RelPath,
    /// What the walk did with it.
    pub outcome: LinkOutcome,
}

/// What became of a symlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The target is reached without a link too; the payload is that
    /// spelling, which is the one the target's files are listed under (the
    /// empty path for the root). Whether or not the walk also went through
    /// the link, it contributes no file of its own.
    Aliases(RelPath),
    /// Followed, and the target is reached only through links, this one
    /// first: its files are listed under this link's spelling.
    Followed,
    /// The target is reached only through links, and another one reached it
    /// first; the payload is that spelling.
    Duplicate(RelPath),
    /// Not followed.
    Refused(LinkRefusal),
}

/// Why a symlink was not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRefusal {
    /// The target does not exist.
    Dangling,
    /// The target's real path is outside the root's.
    OutsideRoot,
    /// The target's real path passes through an excluded directory name.
    ExcludedTarget,
}

/// [`walk_project`], with the symlinks it met and what became of each.
pub fn walk_project_detailed(root: &Path, extensions: &[String], exclude_dirs: &[String]) -> WalkedProject {
    let claimed: Vec<String> = extensions.iter().map(|ext| ext.to_lowercase()).collect();
    let (builder, guard) = walker(root, exclude_dirs);

    let mut walked: Vec<(PathBuf, RelPath)> = Vec::new();
    for entry in builder.build() {
        // An unreadable directory contributes nothing rather than failing the
        // walk: a project with one permission-denied subdirectory still has an
        // index worth having, and the alternative is no index at all. The
        // links the iterator fails on are recorded first.
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                guard.note_error(&err);
                continue;
            }
        };
        if !entry.file_type().is_some_and(|file_type| file_type.is_file()) {
            continue;
        }
        let Some(path) = RelPath::relative_to(root, entry.path()) else { continue };
        if path.extension().is_some_and(|extension| claimed.contains(&extension)) {
            walked.push((entry.into_path(), path));
        }
    }
    guard.finish(walked)
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
/// `.gitignore` rule. A symlink is never listed, followed or not (listing a
/// directory reports a link as a link, not as a directory); an ignored
/// directory under a followed link is listed under the link's spelling.
/// Neither is a directory skipped by name, which is reported once in
/// `exclude_dirs` instead.
pub fn walk_scope(root: &Path, exclude_dirs: &[String]) -> WalkScope {
    let excluded = excluded_names(exclude_dirs);

    let entered: HashSet<PathBuf> = walker(root, exclude_dirs)
        .0
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
/// module's doc, and nothing else. The guard it returns has seen every entry
/// the builder's walk let through once that walk has run.
fn walker(root: &Path, exclude_dirs: &[String]) -> (WalkBuilder, Arc<LinkGuard>) {
    let excluded = excluded_names(exclude_dirs);
    let guard = Arc::new(LinkGuard::new(root, excluded.clone()));
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(true)
        .sort_by_file_name(|a, b| a.cmp(b));
    let filter_guard = Arc::clone(&guard);
    // `ignore` runs this after `.gitignore` has let the entry through, and
    // never for the root.
    walker.filter_entry(move |entry| {
        // Directories only: a *file* called `vendor` is a file, and
        // `exclude_dirs` is a list of directory names. Checked before the
        // guard, so a link with an excluded name is never resolved.
        let is_dir = entry.file_type().is_some_and(|file_type| file_type.is_dir());
        if is_dir && excluded.iter().any(|name| entry.file_name() == name.as_str()) {
            return false;
        }
        if entry.path_is_symlink() {
            filter_guard.admit_link(entry.path(), is_dir)
        } else {
            if is_dir {
                filter_guard.entered_dir(entry.path());
            }
            true
        }
    });
    (walker, guard)
}

/// The symlink guard's state for one walk. Shared with the walker's
/// `filter_entry`, which must be `Send + Sync + 'static`; the lock is taken
/// only for directories and links.
struct LinkGuard {
    /// The root as given: every path the walk reports starts with it.
    root: PathBuf,
    /// The root's real path, or the root itself if it does not resolve.
    root_real: PathBuf,
    excluded: Vec<String>,
    state: Mutex<GuardState>,
}

#[derive(Default)]
struct GuardState {
    /// Followed links: as-reached path -> real path.
    followed: HashMap<PathBuf, PathBuf>,
    /// Every directory entered, by real path.
    entered: HashMap<PathBuf, Entered>,
    /// Every link judged, in the order met.
    judged: Vec<Judged>,
}

struct Entered {
    /// The spelling it was first entered by.
    first: PathBuf,
    /// The spelling with no followed link above it, once the plain walk has
    /// entered it.
    plain: Option<PathBuf>,
}

struct Judged {
    at: PathBuf,
    verdict: Verdict,
}

enum Verdict {
    FollowedDir(PathBuf),
    FollowedFile(PathBuf),
    /// A directory link whose target was already entered.
    AlreadyEntered(PathBuf),
    Refused(LinkRefusal),
}

impl LinkGuard {
    fn new(root: &Path, excluded: Vec<String>) -> Self {
        let root_real = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut state = GuardState::default();
        state.entered.insert(
            root_real.clone(),
            Entered { first: root.to_path_buf(), plain: Some(root.to_path_buf()) },
        );
        Self { root: root.to_path_buf(), root_real, excluded, state: Mutex::new(state) }
    }

    fn lock(&self) -> MutexGuard<'_, GuardState> {
        // A panic while holding the lock leaves plain maps behind, which are
        // still consistent enough to finish a walk with.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `path`'s real path, and whether a followed link is above it (or is
    /// it). With no link followed yet this is a join, no lookup.
    fn real_of(&self, state: &GuardState, path: &Path) -> (PathBuf, bool) {
        if !state.followed.is_empty() {
            for ancestor in path.ancestors() {
                if ancestor == self.root {
                    break;
                }
                if let Some(real) = state.followed.get(ancestor) {
                    let suffix = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
                    return (real.join(suffix), true);
                }
            }
        }
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        (self.root_real.join(relative), false)
    }

    /// A directory reached without being a link itself: always entered.
    fn entered_dir(&self, path: &Path) {
        let mut state = self.lock();
        let (real, via_link) = self.real_of(&state, path);
        let entered =
            state.entered.entry(real).or_insert_with(|| Entered { first: path.to_path_buf(), plain: None });
        if !via_link && entered.plain.is_none() {
            entered.plain = Some(path.to_path_buf());
        }
    }

    /// Whether to follow the link at `path`.
    fn admit_link(&self, path: &Path, is_dir: bool) -> bool {
        let verdict = self.judge(path, is_dir);
        let follow = matches!(verdict, Verdict::FollowedDir(_) | Verdict::FollowedFile(_));
        let mut state = self.lock();
        match &verdict {
            Verdict::FollowedDir(real) => {
                state.followed.insert(path.to_path_buf(), real.clone());
                state.entered.insert(real.clone(), Entered { first: path.to_path_buf(), plain: None });
            }
            Verdict::FollowedFile(real) => {
                state.followed.insert(path.to_path_buf(), real.clone());
            }
            Verdict::AlreadyEntered(_) | Verdict::Refused(_) => {}
        }
        state.judged.push(Judged { at: path.to_path_buf(), verdict });
        follow
    }

    fn judge(&self, path: &Path, is_dir: bool) -> Verdict {
        let Ok(real) = fs::canonicalize(path) else { return Verdict::Refused(LinkRefusal::Dangling) };
        let Ok(below_root) = real.strip_prefix(&self.root_real) else {
            return Verdict::Refused(LinkRefusal::OutsideRoot);
        };
        if below_root
            .components()
            .any(|part| self.excluded.iter().any(|name| part.as_os_str() == name.as_str()))
        {
            return Verdict::Refused(LinkRefusal::ExcludedTarget);
        }
        if !is_dir {
            return Verdict::FollowedFile(real);
        }
        if self.lock().entered.contains_key(&real) {
            Verdict::AlreadyEntered(real)
        } else {
            Verdict::FollowedDir(real)
        }
    }

    /// Records the two kinds of link the directory iterator fails on before
    /// `filter_entry` sees them: a dangling link and a link to an ancestor.
    fn note_error(&self, err: &ignore::Error) {
        let verdict_at = match innermost(err) {
            (_, Some(ignore::Error::Loop { child, .. })) => {
                fs::canonicalize(child).ok().map(|real| (child.clone(), Verdict::AlreadyEntered(real)))
            }
            (Some(path), _) => {
                let is_link =
                    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
                (is_link && fs::metadata(path).is_err())
                    .then(|| (path.to_path_buf(), Verdict::Refused(LinkRefusal::Dangling)))
            }
            _ => None,
        };
        let Some((at, verdict)) = verdict_at else { return };
        self.lock().judged.push(Judged { at, verdict });
    }

    /// The winner pass: one spelling per real file, then every judged link's
    /// outcome.
    fn finish(&self, walked: Vec<(PathBuf, RelPath)>) -> WalkedProject {
        let state = self.lock();

        // Real path -> index in `walked` of its winning spelling.
        let mut winners: HashMap<PathBuf, (usize, bool)> = HashMap::new();
        for (index, (path, _)) in walked.iter().enumerate() {
            let (real, via_link) = self.real_of(&state, path);
            winners
                .entry(real)
                .and_modify(|winner| {
                    if winner.1 && !via_link {
                        *winner = (index, via_link);
                    }
                })
                .or_insert((index, via_link));
        }
        let mut keep = vec![false; walked.len()];
        for (index, _) in winners.values() {
            keep[*index] = true;
        }

        let relative =
            |path: &Path| RelPath::relative_to(&self.root, path).unwrap_or_else(|| RelPath::new(""));
        let mut links = Vec::new();
        for judged in &state.judged {
            let outcome = match &judged.verdict {
                Verdict::Refused(reason) => LinkOutcome::Refused(*reason),
                Verdict::FollowedDir(real) | Verdict::AlreadyEntered(real) => match state.entered.get(real) {
                    Some(Entered { plain: Some(plain), .. }) => LinkOutcome::Aliases(relative(plain)),
                    Some(Entered { first, .. }) if *first != judged.at => {
                        LinkOutcome::Duplicate(relative(first))
                    }
                    _ => LinkOutcome::Followed,
                },
                Verdict::FollowedFile(real) => {
                    let Some(&(index, via_link)) = winners.get(real) else { continue };
                    let winner = &walked[index].0;
                    if *winner == judged.at {
                        LinkOutcome::Followed
                    } else if via_link {
                        LinkOutcome::Duplicate(relative(winner))
                    } else {
                        LinkOutcome::Aliases(relative(winner))
                    }
                }
            };
            links.push(WalkedLink { at: relative(&judged.at), outcome });
        }

        let files =
            walked.into_iter().zip(keep).filter_map(|((_, path), kept)| kept.then_some(path)).collect();
        WalkedProject { files, links }
    }
}

/// The path an `ignore` error carries, and the error under its wrappers.
fn innermost(err: &ignore::Error) -> (Option<&Path>, Option<&ignore::Error>) {
    let mut path = None;
    let mut current = err;
    loop {
        match current {
            ignore::Error::WithPath { path: at, err } => {
                path.get_or_insert(at.as_path());
                current = err;
            }
            ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => current = err,
            other => return (path, Some(other)),
        }
    }
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

    /// A `.gitignore` below the root binds only its own subtree: a sibling
    /// file with the same name is walked.
    #[test]
    fn a_nested_gitignore_is_scoped_to_its_own_subtree() {
        let tree = Tree::new("gitignore-nested");
        tree.write("pkg/a/.gitignore", "ignored.toy\n");
        tree.write("pkg/a/kept.toy", "");
        tree.write("pkg/a/ignored.toy", "");
        tree.write("pkg/b/ignored.toy", "");
        tree.write("ignored.toy", "");
        assert_eq!(tree.walk(&[]), vec!["ignored.toy", "pkg/a/kept.toy", "pkg/b/ignored.toy"]);
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

    // Symlinks (docs/adr/0025-project-walk-follows-symlinks.md): followed
    // behind a guard, and a file's spelling is its real one whenever the
    // plain walk reaches it. B-numbers are the behaviours in
    // docs/architecture/gm-349-sdk-walk-symlinks.md, section 7; each test
    // names the production change that makes it fail.

    /// What [`walk_project_detailed`] reports, as plain strings: the files,
    /// and every judged link with its outcome, sorted by the link's path.
    fn detailed(root: &Path, exclude: &[&str]) -> (Vec<String>, Vec<(String, LinkOutcome)>) {
        let extensions = vec![".toy".to_string()];
        let exclude: Vec<String> = exclude.iter().map(|dir| (*dir).to_string()).collect();
        let walked = walk_project_detailed(root, &extensions, &exclude);
        let files = walked.files.iter().map(|path| path.as_str().to_string()).collect();
        let mut links: Vec<(String, LinkOutcome)> =
            walked.links.into_iter().map(|link| (link.at.as_str().to_string(), link.outcome)).collect();
        links.sort_by(|a, b| a.0.cmp(&b.0));
        (files, links)
    }

    #[cfg(unix)]
    fn aliases(spelling: &str) -> LinkOutcome {
        LinkOutcome::Aliases(RelPath::new(spelling))
    }

    #[cfg(unix)]
    impl Tree {
        /// A symlink at `at` whose stored target is `target`, verbatim
        /// (relative targets resolve against the link's own directory).
        fn link(&self, target: &str, at: &str) {
            let full = self.0.join(at);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, full).unwrap();
        }
    }

    /// B1: a gitignored directory reached only through a link is walked
    /// under the link's spelling - whichever side of the link it sorts on.
    ///
    /// Controls: `follow_links(false)` in `walker` -> `[]` in both orders;
    /// run the guard before `.gitignore` (claim `real-src` as entered when
    /// the plain walk meets it) -> the `real-src` order returns `[]`.
    #[cfg(unix)]
    #[test]
    fn a_gitignored_target_reached_only_through_a_link_is_walked_under_the_link_spelling() {
        for target in ["real-src", "z-src"] {
            let tree = Tree::new(&format!("b1-{target}"));
            tree.write(".gitignore", &format!("{target}/\n"));
            tree.write(&format!("{target}/pkg.toy"), "");
            tree.link(&format!("../{target}"), "src/linked");

            let (files, links) = detailed(&tree.0, &[]);
            assert_eq!(files, vec!["src/linked/pkg.toy"], "target {target}");
            assert_eq!(links, vec![("src/linked".to_string(), LinkOutcome::Followed)], "target {target}");
        }
    }

    /// B2: a link that
    /// sorts before its plainly walked target adds nothing, and the target's
    /// real spelling is the one listed - for one link and for two.
    ///
    /// Control: make the winner pass in `LinkGuard::finish` keep the first
    /// spelling in walk order (drop the `and_modify` replacement) -> the
    /// `packages/...` spellings are listed instead.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_walked_directory_adds_nothing_and_the_real_spelling_wins() {
        let tree = Tree::new("b2");
        tree.write("vendor/real-lib/index.toy", "");
        tree.link("../vendor/real-lib", "packages/lib");
        tree.write("vendor/shared/thing.toy", "");
        tree.link("../vendor/shared", "packages/a-dup");
        tree.link("../vendor/shared", "packages/dup");

        let (files, links) = detailed(&tree.0, &[]);
        assert_eq!(files, vec!["vendor/real-lib/index.toy", "vendor/shared/thing.toy"]);
        assert_eq!(
            links,
            vec![
                ("packages/a-dup".to_string(), aliases("vendor/shared")),
                ("packages/dup".to_string(), aliases("vendor/shared")),
                ("packages/lib".to_string(), aliases("vendor/real-lib")),
            ]
        );
    }

    /// B3: a file link to a walked file is listed once,
    /// under the real file's spelling, whether the link's name sorts before
    /// or after it.
    ///
    /// Control: skip the winner pass in `LinkGuard::finish` (keep every
    /// entry of `walked`) -> the link's spelling is listed too.
    #[cfg(unix)]
    #[test]
    fn a_file_link_to_a_walked_file_is_indexed_once() {
        for alias in ["alias.toy", "z-alias.toy"] {
            let tree = Tree::new(&format!("b3-{alias}"));
            tree.write("src/index.toy", "");
            tree.link("index.toy", &format!("src/{alias}"));

            let (files, links) = detailed(&tree.0, &[]);
            assert_eq!(files, vec!["src/index.toy"], "alias {alias}");
            assert_eq!(links, vec![(format!("src/{alias}"), aliases("src/index.toy"))], "alias {alias}");
        }
    }

    /// B4: two links into one ignored target, one of them to a subdirectory
    /// of the other's: every file is listed once, under the first link to
    /// reach it, and the nested link is a duplicate of that spelling.
    ///
    /// Control: make `LinkGuard::real_of` ignore `followed` (always
    /// `root_real` + relative path) -> `b` is followed as a fresh target and
    /// `b/x.toy` is listed beside `a/sub/x.toy`.
    #[cfg(unix)]
    #[test]
    fn nested_links_into_one_ignored_target_list_each_file_once() {
        let tree = Tree::new("b4");
        tree.write(".gitignore", "zlib/\n");
        tree.write("zlib/sub/x.toy", "");
        tree.link("zlib", "a");
        tree.link("zlib/sub", "b");

        let (files, links) = detailed(&tree.0, &[]);
        assert_eq!(files, vec!["a/sub/x.toy"]);
        assert_eq!(
            links,
            vec![
                ("a".to_string(), LinkOutcome::Followed),
                ("b".to_string(), LinkOutcome::Duplicate(RelPath::new("a/sub"))),
            ]
        );
    }

    /// B5: links to an ancestor (the containing directory,
    /// the root) and to an already-walked sibling terminate and add nothing.
    /// Run on a thread with a deadline, so a regression fails instead of
    /// hanging the suite.
    ///
    /// Controls: remove the `entered` refusal in `LinkGuard::judge` *and*
    /// the winner pass -> `e/tod/x.toy` is listed; additionally turn off
    /// walkdir's loop check (no ancestor tracking) -> the walk does not end
    /// and the deadline fails the test. Make `note_error` a no-op -> the
    /// ancestor links vanish from `links`.
    #[cfg(unix)]
    #[test]
    fn a_link_to_an_ancestor_terminates_and_adds_nothing() {
        let tree = Tree::new("b5");
        tree.write("cycle/a.toy", "");
        tree.link(".", "cycle/loop");
        tree.write("d/x.toy", "");
        tree.link("..", "d/up");
        tree.write("e/y.toy", "");
        tree.link("../d", "e/tod");
        tree.link(".", "self");

        let root = tree.0.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(detailed(&root, &[]));
        });
        let (files, links) = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a walk over link cycles must terminate");

        assert_eq!(files, vec!["cycle/a.toy", "d/x.toy", "e/y.toy"]);
        assert_eq!(
            links,
            vec![
                ("cycle/loop".to_string(), aliases("cycle")),
                ("d/up".to_string(), aliases("")),
                ("e/tod".to_string(), aliases("d")),
                ("self".to_string(), aliases("")),
            ]
        );
    }

    /// B6: a directory link and a file link whose targets
    /// are outside the root are refused.
    ///
    /// Control: remove the `strip_prefix(&self.root_real)` refusal in
    /// `LinkGuard::judge` -> `out/o.toy` and `o.toy` are listed.
    #[cfg(unix)]
    #[test]
    fn a_link_outside_the_root_is_refused() {
        let outside = Tree::new("b6-outside");
        outside.write("o.toy", "");
        let tree = Tree::new("b6");
        tree.write("a.toy", "");
        tree.link(outside.0.to_str().unwrap(), "out");
        tree.link(outside.0.join("o.toy").to_str().unwrap(), "o.toy");

        let (files, links) = detailed(&tree.0, &[]);
        assert_eq!(files, vec!["a.toy"]);
        assert_eq!(
            links,
            vec![
                ("o.toy".to_string(), LinkOutcome::Refused(LinkRefusal::OutsideRoot)),
                ("out".to_string(), LinkOutcome::Refused(LinkRefusal::OutsideRoot)),
            ]
        );
    }

    /// B7: dangling directory-shaped and file links are
    /// skipped, the walk continues past them, and both are reported - but
    /// not one under an excluded directory name, which is never entered.
    ///
    /// Control: make `LinkGuard::note_error` a no-op -> `links` is empty.
    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_skipped() {
        let tree = Tree::new("b7");
        tree.write("a.toy", "");
        tree.link("nowhere", "0dangling");
        tree.link("nowhere.toy", "0dangling.toy");
        tree.write("z/b.toy", "");
        tree.link("nowhere", "vendor/gone");

        let (files, links) = detailed(&tree.0, &["vendor"]);
        assert_eq!(files, vec!["a.toy", "z/b.toy"]);
        assert_eq!(
            links,
            vec![
                ("0dangling".to_string(), LinkOutcome::Refused(LinkRefusal::Dangling)),
                ("0dangling.toy".to_string(), LinkOutcome::Refused(LinkRefusal::Dangling)),
            ]
        );
    }

    /// B8: a link whose target passes through an excluded directory name
    /// (the manifest's, or a baseline one) is refused, and a link that is
    /// itself named like an excluded directory is skipped by name before it
    /// is ever resolved.
    ///
    /// Controls: remove the excluded-components refusal in
    /// `LinkGuard::judge` -> `cl/c.toy`, `dep/f.toy` and `f.toy` are listed;
    /// move the name check in `walker`'s `filter_entry` after the guard ->
    /// `x/node_modules` appears in `links`.
    #[cfg(unix)]
    #[test]
    fn a_link_into_an_excluded_directory_is_refused_and_one_named_excluded_is_never_resolved() {
        let tree = Tree::new("b8");
        tree.write("node_modules/foo/f.toy", "");
        tree.write(".claude/c.toy", "");
        tree.write("lib/l.toy", "");
        tree.link("node_modules/foo", "dep");
        tree.link(".claude", "cl");
        tree.link("node_modules/foo/f.toy", "f.toy");
        tree.link("../lib", "x/node_modules");

        let (files, links) = detailed(&tree.0, &["node_modules"]);
        assert_eq!(files, vec!["lib/l.toy"]);
        assert_eq!(
            links,
            vec![
                ("cl".to_string(), LinkOutcome::Refused(LinkRefusal::ExcludedTarget)),
                ("dep".to_string(), LinkOutcome::Refused(LinkRefusal::ExcludedTarget)),
                ("f.toy".to_string(), LinkOutcome::Refused(LinkRefusal::ExcludedTarget)),
            ]
        );
    }

    /// B11: a root given through a link (the macOS temp dir is one already;
    /// this makes it explicit on every Unix) keeps B2 and B5, and every path
    /// is relative to the root as given.
    ///
    /// Control: build `LinkGuard::root_real` from `root` without
    /// `canonicalize` -> every in-root link reads as `OutsideRoot`.
    #[cfg(unix)]
    #[test]
    fn a_root_given_through_a_link_keeps_real_wins_and_relative_paths() {
        let base = Tree::new("b11");
        base.write("real-root/z/x.toy", "");
        base.link("z", "real-root/a");
        base.link("..", "real-root/z/up");
        base.link("real-root", "via");

        let (files, links) = detailed(&base.0.join("via"), &[]);
        assert_eq!(files, vec!["z/x.toy"]);
        // `a` sorts first, so the walk goes through it before `z` is entered
        // and meets `a/up` there; the files still carry the real spelling.
        assert_eq!(
            links,
            vec![
                ("a".to_string(), aliases("z")),
                ("a/up".to_string(), aliases("")),
                ("z/up".to_string(), aliases("")),
            ]
        );
    }

    /// B12: `walk_scope` over a tree with followed, aliasing and refused
    /// links lists no link and no refused target; a gitignored directory
    /// reached only through a link is listed at its real spelling, and one
    /// ignored *under* the followed link under the link's spelling (the
    /// pinned output, `walk_scope`'s doc).
    ///
    /// Control: none that the design requires (it pins current output); make
    /// `walk_scope`'s child check follow links (`fs::metadata`) -> the links
    /// `alias`, `src/gen` would be listed.
    #[cfg(unix)]
    #[test]
    fn the_scope_lists_no_link_and_no_refused_target() {
        let outside = Tree::new("b12-outside");
        outside.write("o.toy", "");
        let tree = Tree::new("b12");
        // Anchored: a bare `gen/` would match the link `src/gen` too.
        tree.write(".gitignore", "/gen/\n");
        tree.write("gen/g.toy", "");
        tree.write("gen/.gitignore", "build/\n");
        tree.write("gen/build/b.toy", "");
        tree.link("../gen", "src/gen");
        tree.write("pkg/p.toy", "");
        tree.link("pkg", "alias");
        tree.link(outside.0.to_str().unwrap(), "out");
        tree.write(".claude/c.toy", "");
        tree.link(".claude", "cl");
        tree.link("nowhere", "dangling");

        let scope = scope(&tree, &[]);
        let pruned: Vec<&str> = scope.pruned.iter().map(RelPath::as_str).collect();
        assert_eq!(pruned, vec!["gen", "src/gen/build"]);
        assert_eq!(scope.exclude_dirs, vec![".git", ".claude"]);
        assert_eq!(scope.dropped, 0);
    }

    /// B10 (Windows): a junction to an in-root ignored directory is followed,
    /// and a junction to an ancestor terminates and adds nothing. Junctions
    /// need no privilege, unlike `symlink_dir`.
    ///
    /// Control: `follow_links(false)` in `walker` -> `[]` for the ignored
    /// target.
    #[cfg(windows)]
    #[test]
    fn a_junction_to_an_ignored_directory_is_followed_and_one_to_an_ancestor_terminates() {
        fn junction(target: &Path, at: &Path) {
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(at)
                .arg(target)
                .status()
                .expect("failed to run mklink");
            assert!(status.success(), "mklink /J {} {} failed", at.display(), target.display());
        }

        let tree = Tree::new("b10");
        tree.write(".gitignore", "real-src/\n");
        tree.write("real-src/pkg.toy", "");
        junction(&tree.0.join("real-src"), &tree.0.join("src").join("linked"));
        tree.write("d/x.toy", "");
        junction(&tree.0, &tree.0.join("d").join("up"));

        let root = tree.0.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(detailed(&root, &[]));
        });
        let (files, _) = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a walk over a junction cycle must terminate");
        assert_eq!(files, vec!["d/x.toy", "src/linked/pkg.toy"]);
    }
}
