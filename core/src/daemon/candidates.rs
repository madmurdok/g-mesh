//! Multi-project detection (D10 in `docs/architecture/lazy-indexing.md`,
//! GM-399).
//!
//! Decides whether a daemon's root is one project (served normally, lazily)
//! or a folder of several (served by the front, `daemon::front`). The
//! decision is cheap and bounded on purpose: it runs on every daemon start,
//! inside the shim's bootstrap budget, so a normal project pays one `stat`
//! per marker at its root and nothing else, and a folder pays at most
//! [`Limits::max_entries`] directory entries.
//!
//! The rules, in order (a normal project only ever pays the first):
//!  1. the root carries a marker itself: single;
//!  2. the root already has a completed index (`meta.bulkIndexedAt` set):
//!     single - someone chose to index this folder as a whole (Q2, Q7);
//!  3. the bounded walk finds fewer than two candidates: single (Q3).
//!
//! Otherwise it is a folder of projects.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

use crate::storage::connection::project_dir;
use crate::storage::schema;

/// What marks a directory as a project. Each is probed with one
/// `symlink_metadata` of `<dir>/<marker>`, never a listing.
///
/// `.git` is a directory in a repository and a *file* in a worktree or a
/// submodule; both mark a project, and the file form sets
/// [`Candidate::is_worktree`].
pub const MARKERS: [&str; 5] = [".git", "Cargo.toml", "package.json", "go.mod", "pyproject.toml"];

/// Directory names the walk never enters. Every dot-directory is skipped as
/// well (by the leading dot, not by this list); `.git` is only ever probed as
/// a marker.
const SKIPPED_DIRS: [&str; 8] =
    ["node_modules", "target", "dist", "build", "out", "vendor", "venv", "__pycache__"];

/// How few candidates still leave the root a single project (rule 3, Q3):
/// a root with exactly one marked subdirectory is indexed as a whole.
const MULTI_THRESHOLD: usize = 2;

/// The walk's bounds. [`Limits::default`] is what the daemon uses; tests pass
/// smaller ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How many levels below the root are looked at: 2 is the root's
    /// children and grandchildren.
    pub max_depth: usize,
    /// Directory entries read in total, across every listing.
    pub max_entries: usize,
    /// Candidates kept before the walk stops.
    pub max_candidates: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_depth: 2, max_entries: 5_000, max_candidates: 64 }
    }
}

/// One marked directory below the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Relative to the root, `/`-separated.
    pub rel_path: String,
    /// Canonical: the root is canonicalized once and the walk follows no
    /// symlinks, so joining keeps it canonical. The shim compares this
    /// against its own canonical root (D11 step 3).
    pub abs_path: PathBuf,
    /// Which of [`MARKERS`] were found, in [`MARKERS`] order.
    pub markers: Vec<&'static str>,
    /// `.git` is a file (a worktree or a submodule), not a directory.
    pub is_worktree: bool,
}

/// Why a root was judged a single project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleReason {
    /// Rule 1.
    RootMarker,
    /// Rule 2.
    CompletedIndex,
    /// Rule 3.
    FewCandidates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Single(SingleReason),
    Multi,
}

/// What the bounded walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Sorted by `rel_path`.
    pub candidates: Vec<Candidate>,
    pub entries_read: usize,
    pub elapsed: Duration,
    /// A limit ([`Limits::max_entries`] or [`Limits::max_candidates`]) was
    /// hit; `candidates` holds what was found before it.
    pub truncated: bool,
}

/// The mode decision and whatever it had to look at to make it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub mode: Mode,
    /// Empty unless the walk ran (rules 1 and 2 settle it without one).
    pub candidates: Vec<Candidate>,
    pub entries_read: usize,
    /// The whole decision, rules 1-3.
    pub elapsed: Duration,
    pub truncated: bool,
    /// Whether rule 3's walk ran at all.
    pub walked: bool,
}

/// Decides `root`'s mode, reading rule 2 off `root`'s own state directory.
/// A state directory that cannot be resolved counts as "no completed index".
pub fn detect(root: &Path, limits: Limits) -> Detection {
    match project_dir(root) {
        Ok(state_dir) => detect_in(root, &state_dir, limits),
        Err(_) => decide(root, None, limits),
    }
}

/// [`detect`] with the state directory given, which is what the daemon (it
/// has already resolved it) and the unit tests (a tempdir) pass.
pub fn detect_in(root: &Path, state_dir: &Path, limits: Limits) -> Detection {
    decide(root, Some(state_dir), limits)
}

fn decide(root: &Path, state_dir: Option<&Path>, limits: Limits) -> Detection {
    let started = Instant::now();
    let single = |reason| Detection {
        mode: Mode::Single(reason),
        candidates: Vec::new(),
        entries_read: 0,
        elapsed: started.elapsed(),
        truncated: false,
        walked: false,
    };

    if !markers_of(root).is_empty() {
        return single(SingleReason::RootMarker);
    }
    if state_dir.is_some_and(completed_index_in) {
        return single(SingleReason::CompletedIndex);
    }

    let found = walk(root, limits);
    let mode = if found.candidates.len() < MULTI_THRESHOLD {
        Mode::Single(SingleReason::FewCandidates)
    } else {
        Mode::Multi
    };
    Detection {
        mode,
        candidates: found.candidates,
        entries_read: found.entries_read,
        elapsed: started.elapsed(),
        truncated: found.truncated,
        walked: true,
    }
}

/// Whether `project_root` (canonical) already has a completed index of its
/// own - rule 2's check, applied to a candidate instead of the root. The
/// front uses it to say which of its projects are indexed (D12). A state
/// directory that cannot be resolved counts as "not indexed", as in
/// [`detect`].
pub fn has_completed_index(project_root: &Path) -> bool {
    project_dir(project_root).is_ok_and(|state_dir| completed_index_in(&state_dir))
}

/// Rule 2: `<state dir>/index.db` exists and records a finished walk.
///
/// Opened read-write *without* `CREATE`, as `cli::status::index_status` and
/// `gc::last_used` open it: recovering a WAL an abandoned daemon left behind
/// needs write access, and a missing database must never be conjured into
/// existence by a check. Never through `connection::open`, which creates the
/// file. Any failure to read it (no file, no `meta` table, no row, a corrupt
/// file) counts as "not completed".
fn completed_index_in(state_dir: &Path) -> bool {
    let db_path = state_dir.join("index.db");
    if fs::symlink_metadata(&db_path).is_err() {
        return false;
    }
    let Ok(conn) =
        Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI)
    else {
        return false;
    };
    schema::bulk_index_completed(&conn).unwrap_or(false)
}

/// The markers `dir` carries, in [`MARKERS`] order, plus whether `.git` is a
/// file.
fn probe(dir: &Path) -> (Vec<&'static str>, bool) {
    let mut found = Vec::new();
    let mut is_worktree = false;
    for marker in MARKERS {
        if let Ok(metadata) = fs::symlink_metadata(dir.join(marker)) {
            if marker == ".git" && metadata.is_file() {
                is_worktree = true;
            }
            found.push(marker);
        }
    }
    (found, is_worktree)
}

fn markers_of(dir: &Path) -> Vec<&'static str> {
    probe(dir).0
}

fn is_skipped(name: &str) -> bool {
    name.starts_with('.') || SKIPPED_DIRS.contains(&name)
}

/// The bounded breadth-first walk (rule 3), on its own: the daemon reaches it
/// through [`detect`], `select_project` and `g-mesh status` call it directly
/// to re-list a folder already known to be one, and `g-mesh debug-candidates`
/// runs it even where rules 1-2 made it unnecessary, to measure it (M4).
pub fn walk(root: &Path, limits: Limits) -> Walk {
    let started = Instant::now();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut candidates = Vec::new();
    let mut entries_read = 0;
    let mut truncated = false;

    // (directory, its path relative to the root, its depth; the root is 0).
    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::from([(root.clone(), String::new(), 0)]);
    'walk: while let Some((dir, rel, depth)) = queue.pop_front() {
        if depth >= limits.max_depth {
            continue;
        }
        let Ok(listing) = fs::read_dir(&dir) else { continue };
        for entry in listing {
            if entries_read >= limits.max_entries {
                truncated = true;
                break 'walk;
            }
            entries_read += 1;
            let Ok(entry) = entry else { continue };
            // `DirEntry::file_type` does not follow symlinks: a symlink to a
            // repository is a symlink here, never a directory.
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_skipped(name) {
                continue;
            }
            let child = dir.join(name);
            let child_rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
            let (markers, is_worktree) = probe(&child);
            if markers.is_empty() {
                queue.push_back((child, child_rel, depth + 1));
                continue;
            }
            // A candidate: its interior is never listed, which keeps nested
            // repositories and packages out and the walk cheap.
            if candidates.len() >= limits.max_candidates {
                truncated = true;
                break 'walk;
            }
            candidates.push(Candidate { rel_path: child_rel, abs_path: child, markers, is_worktree });
        }
    }

    candidates.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Walk { candidates, entries_read, elapsed: started.elapsed(), truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    fn mkdir(root: &Path, rel: &str) {
        fs::create_dir_all(root.join(rel)).unwrap();
    }

    fn rels(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.rel_path.as_str()).collect()
    }

    /// A state directory with no index in it, so rule 2 never fires.
    fn detect_fresh(root: &Path, limits: Limits) -> Detection {
        let state = tempfile::tempdir().unwrap();
        detect_in(root, state.path(), limits)
    }

    #[test]
    fn three_repos_and_a_worktree_at_depth_two_are_the_four_candidates() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");
        mkdir(root.path(), "group/c/.git");
        touch(root.path(), "worktrees/wt/.git");
        mkdir(root.path(), "plain/dir");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Multi);
        assert_eq!(rels(&detection.candidates), ["a", "b", "group/c", "worktrees/wt"]);
        let worktrees: Vec<&str> =
            detection.candidates.iter().filter(|c| c.is_worktree).map(|c| c.rel_path.as_str()).collect();
        assert_eq!(worktrees, ["worktrees/wt"]);
        let canonical = root.path().canonicalize().unwrap();
        assert_eq!(detection.candidates[0].abs_path, canonical.join("a"));
        assert_eq!(detection.candidates[0].markers, [".git"]);
    }

    #[test]
    fn a_repo_with_nested_packages_is_listed_once() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "mono/.git");
        touch(root.path(), "mono/package.json");
        touch(root.path(), "mono/plugins/x/package.json");
        touch(root.path(), "mono/y/package.json");
        mkdir(root.path(), "other/.git");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(rels(&detection.candidates), ["mono", "other"]);
        assert_eq!(detection.candidates[0].markers, [".git", "package.json"]);
    }

    #[test]
    fn node_modules_packages_are_not_candidates() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "node_modules/x/package.json");
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(rels(&detection.candidates), ["a", "b"]);
    }

    #[test]
    fn a_marker_at_depth_three_is_not_listed() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "x/y/z/go.mod");
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(rels(&detection.candidates), ["a", "b"]);
    }

    #[test]
    fn a_root_with_its_own_marker_is_single_whatever_lies_below() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "package.json");
        touch(root.path(), "plugins/a/package.json");
        touch(root.path(), "plugins/b/package.json");
        mkdir(root.path(), "c/.git");
        mkdir(root.path(), "d/.git");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Single(SingleReason::RootMarker));
        assert!(!detection.walked, "rule 1 must settle it without a walk");
    }

    #[test]
    fn one_candidate_leaves_the_root_single() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "only/.git");
        touch(root.path(), "notes.txt");

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Single(SingleReason::FewCandidates));
        assert_eq!(rels(&detection.candidates), ["only"]);
    }

    fn index_with_bulk_indexed_at(state_dir: &Path, completed: bool) {
        let conn = Connection::open(state_dir.join("index.db")).unwrap();
        schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
        let value: Option<&str> = completed.then_some("2026-09-24 00:00:00");
        conn.execute("UPDATE meta SET bulkIndexedAt = ?1 WHERE id = 1", [value]).unwrap();
    }

    #[test]
    fn a_completed_index_makes_a_folder_single_and_an_unfinished_one_does_not() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");

        let completed = tempfile::tempdir().unwrap();
        index_with_bulk_indexed_at(completed.path(), true);
        let detection = detect_in(root.path(), completed.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Single(SingleReason::CompletedIndex));

        let unfinished = tempfile::tempdir().unwrap();
        index_with_bulk_indexed_at(unfinished.path(), false);
        let detection = detect_in(root.path(), unfinished.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Multi);
    }

    #[test]
    fn the_mode_decision_never_creates_an_index() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");
        let state = tempfile::tempdir().unwrap();

        let detection = detect_in(root.path(), state.path(), Limits::default());
        assert_eq!(detection.mode, Mode::Multi);
        assert!(!state.path().join("index.db").exists(), "rule 2 must not conjure an index.db");
    }

    #[test]
    fn the_entry_limit_truncates_a_wide_folder() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..40 {
            mkdir(root.path(), &format!("d{n:02}"));
        }
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");

        let limits = Limits { max_entries: 10, ..Limits::default() };
        let detection = detect_fresh(root.path(), limits);
        assert!(detection.truncated, "hitting max_entries must set truncated");
        assert!(detection.entries_read <= 10, "read {} entries past a limit of 10", detection.entries_read);
    }

    #[test]
    fn the_candidate_limit_truncates_too() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..5 {
            mkdir(root.path(), &format!("r{n}/.git"));
        }

        let limits = Limits { max_candidates: 3, ..Limits::default() };
        let detection = detect_fresh(root.path(), limits);
        assert!(detection.truncated);
        assert_eq!(detection.candidates.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_repo_is_not_listed() {
        let elsewhere = tempfile::tempdir().unwrap();
        mkdir(elsewhere.path(), "repo/.git");
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "a/.git");
        mkdir(root.path(), "b/.git");
        std::os::unix::fs::symlink(elsewhere.path().join("repo"), root.path().join("linked")).unwrap();

        let detection = detect_fresh(root.path(), Limits::default());
        assert_eq!(rels(&detection.candidates), ["a", "b"]);
    }
}
