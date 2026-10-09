//! The project walk g-mesh's plugins and core share: which entries under a
//! root a walk reaches, and what became of every symlink on the way.
//!
//! The plugin SDK's walk (`g_mesh_plugin_sdk::walk`) and core's walk must
//! agree on which spelling a file is listed under, so both run this one.
//! It depends on `ignore` only. Which directory *names* are excluded is the
//! caller's: the SDK passes its baseline plus a manifest's `exclude_dirs`.
//!
//! # The policy, exactly
//!
//! - **`.gitignore`, layered as git layers it**, from the root down.
//!   `require_git` is off, so a directory that is not a git repository still
//!   honours its own `.gitignore` - which the SDK's conformance kit needs,
//!   since it copies fixtures into a scratch directory with no `.git` in it.
//! - **`.gitignore` above the root is not read** (`parents(false)`). A project
//!   is walked the same way wherever it is checked out.
//! - **The user's global gitignore and `.ignore` files are not read.** Both
//!   are per machine, and an index that depends on them is an index two
//!   developers cannot compare.
//! - **Hidden files are not skipped for being hidden.** `.config/build.ts` is
//!   source. What is skipped is named explicitly, by the caller.
//! - **The caller's `excluded` directory names**, by exact name at any depth.
//! - **Symlinks are followed, behind a guard.** Why, and why these rules, is
//!   [ADR 0025](../../docs/adr/0025-project-walk-follows-symlinks.md).
//! - **Sorted** (`sort_by_file_name`), so "first in walk order" below is the
//!   same order in every walk of the same tree.
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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use ignore::WalkBuilder;

/// What [`walk`] found: the files, and what became of every symlink it met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// One entry per real file, in walk (sorted) order.
    pub files: Vec<WalkedEntry>,
    /// Every symlink the walk judged, in the order it met them.
    ///
    /// A link the walk never judged is absent: one `.gitignore` or a name
    /// exclude skipped, and a file link whose file the caller's filter did
    /// not keep. Two kinds are reported before those rules could run,
    /// because the directory iterator fails on them first: a dangling link,
    /// and a link to one of its own ancestors. A link under an excluded
    /// directory name is never reported: the walk does not enter excluded
    /// directories.
    pub links: Vec<Link>,
}

/// What [`walk_dirs`] found: every directory entered, and the links met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirWalk {
    /// Every directory the walk entered, the root first (`relative` empty),
    /// in walk order. Not deduplicated: a real directory entered under two
    /// spellings (nested links) is listed under both, since each spelling is
    /// a place the walk read entries - and `.gitignore` files - from.
    pub dirs: Vec<WalkedEntry>,
    /// As [`Walk::links`], except that no file link is reported: a directory
    /// walk keeps no file to decide a file link's winner against.
    pub links: Vec<Link>,
}

/// One entry the walk reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedEntry {
    /// The path as reached: the root as given joined with `relative`.
    pub path: PathBuf,
    /// Relative to the root as given, `/`-separated, no leading `./`.
    pub relative: String,
    /// Relative to the root's real path, `/`-separated, when a followed link
    /// is above this entry (or is it), so the two spellings differ; `None`
    /// when the entry was reached without a link.
    pub real_relative: Option<String>,
}

/// One symlink the walk met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The link's own path, relative to the root as given.
    pub at: String,
    /// The target's real (canonical, absolute) path, when it resolves: `None`
    /// only for [`LinkRefusal::Dangling`].
    pub real: Option<PathBuf>,
    /// What the walk did with it.
    pub outcome: LinkOutcome,
}

/// What became of a symlink. Paths are relative to the root as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The target is reached without a link too; the payload is that
    /// spelling, which is the one the target's files are listed under (the
    /// empty path for the root). Whether or not the walk also went through
    /// the link, it contributes no file of its own.
    Aliases(String),
    /// Followed, and the target is reached only through links, this one
    /// first: its files are listed under this link's spelling.
    Followed,
    /// The target is reached only through links, and another one reached it
    /// first; the payload is that spelling.
    Duplicate(String),
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

/// Every file under `root`, one per real file, with the links met.
///
/// `excluded` are exact directory names, matched at any depth; the caller's
/// whole list (this crate adds no baseline of its own).
pub fn walk(root: &Path, excluded: &[String]) -> Walk {
    walk_filtered(root, excluded, |_| true)
}

/// [`walk`], keeping only the files `keep` accepts, by their path relative to
/// `root`. The filter runs *before* the winner pass: a real file is listed
/// under the first kept spelling by the module doc's rule, so a spelling the
/// filter rejects (another extension, say) never makes a kept one an alias.
pub fn walk_filtered(root: &Path, excluded: &[String], keep: impl Fn(&str) -> bool) -> Walk {
    let (builder, guard) = walker(root, excluded);

    let mut walked: Vec<(PathBuf, String)> = Vec::new();
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
        let Some(relative) = relative_to(root, entry.path()) else { continue };
        if keep(&relative) {
            walked.push((entry.into_path(), relative));
        }
    }
    guard.finish(walked)
}

/// Every directory [`walk`] with the same `root` and `excluded` enters, with
/// the directory links met. The same walker, so the two agree on every
/// `.gitignore` rule and every link.
pub fn walk_dirs(root: &Path, excluded: &[String]) -> DirWalk {
    let (builder, guard) = walker(root, excluded);

    let mut dirs = Vec::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                guard.note_error(&err);
                continue;
            }
        };
        if entry.depth() != 0 && !entry.file_type().is_some_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let Some(relative) = relative_to(root, entry.path()) else { continue };
        let real_relative = guard.real_relative(entry.path());
        dirs.push(WalkedEntry { path: entry.into_path(), relative, real_relative });
    }
    let links = guard.links(&[], &HashMap::new());
    DirWalk { dirs, links }
}

/// `path` relative to `root`, `/`-separated with no leading `./`; `None`
/// when `path` is not under `root`.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(normalize(&relative.to_string_lossy()))
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    path.strip_prefix("./").unwrap_or(&path).to_string()
}

/// The walker every walk here runs: the policy in this crate's doc, and
/// nothing else. The guard it returns has seen every entry the builder's walk
/// let through once that walk has run.
fn walker(root: &Path, excluded: &[String]) -> (WalkBuilder, Arc<LinkGuard>) {
    let excluded = excluded.to_vec();
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
        // `excluded` is a list of directory names. Checked before the guard,
        // so a link with an excluded name is never resolved.
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
    /// Not followed; the target's real path when it resolves.
    Refused(LinkRefusal, Option<PathBuf>),
}

impl Verdict {
    fn real(&self) -> Option<&PathBuf> {
        match self {
            Verdict::FollowedDir(real) | Verdict::FollowedFile(real) | Verdict::AlreadyEntered(real) => {
                Some(real)
            }
            Verdict::Refused(_, real) => real.as_ref(),
        }
    }
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

    /// [`WalkedEntry::real_relative`] for `path`, from a real path reached
    /// through a followed link.
    fn real_relative_of(&self, real: &Path, via_link: bool) -> Option<String> {
        if !via_link {
            return None;
        }
        relative_to(&self.root_real, real)
    }

    fn real_relative(&self, path: &Path) -> Option<String> {
        let (real, via_link) = self.real_of(&self.lock(), path);
        self.real_relative_of(&real, via_link)
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
            Verdict::AlreadyEntered(_) | Verdict::Refused(..) => {}
        }
        state.judged.push(Judged { at: path.to_path_buf(), verdict });
        follow
    }

    fn judge(&self, path: &Path, is_dir: bool) -> Verdict {
        let Ok(real) = fs::canonicalize(path) else {
            return Verdict::Refused(LinkRefusal::Dangling, None);
        };
        let Ok(below_root) = real.strip_prefix(&self.root_real) else {
            return Verdict::Refused(LinkRefusal::OutsideRoot, Some(real));
        };
        if below_root
            .components()
            .any(|part| self.excluded.iter().any(|name| part.as_os_str() == name.as_str()))
        {
            return Verdict::Refused(LinkRefusal::ExcludedTarget, Some(real));
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
                    .then(|| (path.to_path_buf(), Verdict::Refused(LinkRefusal::Dangling, None)))
            }
            _ => None,
        };
        let Some((at, verdict)) = verdict_at else { return };
        self.lock().judged.push(Judged { at, verdict });
    }

    /// The winner pass: one spelling per real file, then every judged link's
    /// outcome.
    fn finish(&self, walked: Vec<(PathBuf, String)>) -> Walk {
        // Real path -> index in `walked` of its winning spelling.
        let mut winners: HashMap<PathBuf, (usize, bool)> = HashMap::new();
        let mut reals = Vec::with_capacity(walked.len());
        {
            let state = self.lock();
            for (index, (path, _)) in walked.iter().enumerate() {
                let (real, via_link) = self.real_of(&state, path);
                winners
                    .entry(real.clone())
                    .and_modify(|winner| {
                        if winner.1 && !via_link {
                            *winner = (index, via_link);
                        }
                    })
                    .or_insert((index, via_link));
                reals.push((real, via_link));
            }
        }
        let mut keep = vec![false; walked.len()];
        for (index, _) in winners.values() {
            keep[*index] = true;
        }

        let links = self.links(&walked, &winners);

        let files = walked
            .into_iter()
            .zip(reals)
            .zip(keep)
            .filter_map(|(((path, relative), (real, via_link)), kept)| {
                kept.then(|| WalkedEntry {
                    path,
                    relative,
                    real_relative: self.real_relative_of(&real, via_link),
                })
            })
            .collect();
        Walk { files, links }
    }

    /// Every judged link's outcome, given the files walked and the winner
    /// pass's answer over them (both empty for a directory walk, which then
    /// reports no file link).
    fn links(&self, walked: &[(PathBuf, String)], winners: &HashMap<PathBuf, (usize, bool)>) -> Vec<Link> {
        let state = self.lock();
        let relative = |path: &Path| relative_to(&self.root, path).unwrap_or_default();
        let mut links = Vec::new();
        for judged in &state.judged {
            let outcome = match &judged.verdict {
                Verdict::Refused(reason, _) => LinkOutcome::Refused(*reason),
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
            links.push(Link { at: relative(&judged.at), real: judged.verdict.real().cloned(), outcome });
        }
        links
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
