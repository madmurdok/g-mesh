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
//!   link is always entered. A real directory is therefore walked at most
//!   twice, and cycles end (walkdir's own ancestor check is the backstop).
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
    /// one of its own ancestors. Those are left out only when the link sits
    /// under an excluded directory name.
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
        let relative = at.strip_prefix(&self.root).unwrap_or(&at);
        let under_excluded = relative
            .components()
            .any(|part| self.excluded.iter().any(|name| part.as_os_str() == name.as_str()));
        if !under_excluded {
            self.lock().judged.push(Judged { at, verdict });
        }
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
