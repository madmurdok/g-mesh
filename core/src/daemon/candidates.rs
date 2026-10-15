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
/// own that a daemon would keep - rule 2's check, applied to a candidate
/// instead of the root, plus [`current_generation`]. The front uses it to
/// say which of its projects are indexed (D12): an index the daemon will
/// discard on start is not. A state directory that cannot be resolved counts
/// as "not indexed", as in [`detect`].
pub fn has_completed_index(project_root: &Path) -> bool {
    project_dir(project_root).is_ok_and(|state_dir| {
        open_existing_index(&state_dir)
            .is_some_and(|conn| completed_index(&conn) && current_generation(&conn))
    })
}

/// Whether the index was written by this build's schema and core pipeline:
/// `schema_version` equal to [`schema::CURRENT_SCHEMA_VERSION`] and
/// `indexer_version`'s core half equal to [`schema::CURRENT_INDEXER_VERSION`].
/// The plugin half is not compared: that needs plugin discovery, which the
/// front does not do, so a plugin-only rebuild still reads as current here.
/// Any failure to read it counts as "not current". Rule 2 does not use it: an
/// index of the whole folder, of any generation, still says someone chose to
/// treat the folder as one project.
fn current_generation(conn: &Connection) -> bool {
    schema::stored_generation(conn).is_some_and(|(schema_version, indexer_version)| {
        schema_version == schema::CURRENT_SCHEMA_VERSION
            && indexer_version
                .strip_prefix(schema::CURRENT_INDEXER_VERSION)
                .is_some_and(|rest| rest.starts_with('+'))
    })
}

/// Rule 2: `<state dir>/index.db` exists and records a finished walk.
fn completed_index_in(state_dir: &Path) -> bool {
    open_existing_index(state_dir).is_some_and(|conn| completed_index(&conn))
}

/// Opens `<state dir>/index.db` if it exists.
///
/// Opened read-write *without* `CREATE`, as `cli::status::index_status` and
/// `gc::last_used` open it: recovering a WAL an abandoned daemon left behind
/// needs write access, and a missing database must never be conjured into
/// existence by a check. Never through `connection::open`, which creates the
/// file.
fn open_existing_index(state_dir: &Path) -> Option<Connection> {
    let db_path = state_dir.join("index.db");
    if fs::symlink_metadata(&db_path).is_err() {
        return None;
    }
    Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI).ok()
}

/// Whether the index records a finished walk. Any failure to read it (no
/// `meta` table, no row, a corrupt file) counts as "not completed".
fn completed_index(conn: &Connection) -> bool {
    schema::bulk_index_completed(conn).unwrap_or(false)
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

/// Why [`resolve`] found no project at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// The path is not below the root, or climbs out of it (`..`).
    NotBelowRoot,
    /// The path names the root itself.
    IsRoot,
    /// More segments than the walk descends ([`Limits::max_depth`]).
    TooDeep { max_depth: usize },
    /// A segment the walk never enters: a dot-directory or one of the
    /// skipped names (`node_modules`, `target`, ...).
    Skipped(String),
    /// Nothing exists at this path, relative to the root.
    Missing(String),
    /// A symlink or a file, which the walk never takes for a directory.
    NotADirectory(String),
    /// A directory without any of [`MARKERS`].
    NoMarker,
    /// A directory between the root and the path carries a marker, so that
    /// one is the candidate and this path lies inside it.
    InsideCandidate(String),
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unresolved::NotBelowRoot => write!(f, "it is not below the folder"),
            Unresolved::IsRoot => write!(f, "it names the folder itself, not a project below it"),
            Unresolved::TooDeep { max_depth } => {
                write!(f, "projects lie at most {max_depth} levels below the folder")
            }
            Unresolved::Skipped(segment) => {
                write!(f, "'{segment}' is a directory projects are never found in")
            }
            Unresolved::Missing(rel) => write!(f, "'{rel}' does not exist"),
            Unresolved::NotADirectory(rel) => write!(f, "'{rel}' is not a directory (or is a symlink)"),
            Unresolved::NoMarker => write!(f, "it carries none of {}", MARKERS.join(", ")),
            Unresolved::InsideCandidate(rel) => write!(f, "it lies inside the project '{rel}'"),
        }
    }
}

/// `path`'s segments, split on `/` and `\` alike, without empty and `.` ones.
pub(crate) fn segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".").collect()
}

/// `path` below `root`, compared by components. On Windows it is then
/// compared by segments ([`windows_segments_below`]): a verbatim path treats
/// only `\\` as a separator, so `\\?\C:\root/group/c` has no component
/// `group`. `root` is canonical, so a spelling of it that differs otherwise
/// (a symlink, an 8.3 short name, a `..`) is retried canonicalized.
pub(crate) fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    if cfg!(windows) {
        let (root, path_text) = (root.to_string_lossy(), path.to_string_lossy());
        let below = windows_segments_below(&root, &path_text);
        if let Some(below) = below.filter(|below| !below.contains(&"..")) {
            return Some(PathBuf::from(below.join("/")));
        }
    }
    path.canonicalize().ok()?.strip_prefix(root).ok().map(Path::to_path_buf)
}

/// `path`'s segments below `root`'s, both read as Windows paths whatever the
/// host: split on `/` and `\\` alike, with the verbatim prefix (`\\?\`, or
/// `\\?\UNC\` for a share) dropped and drive letters compared ignoring
/// case. A share path matches only a share root, so `\\srv\share` is not
/// `\srv\share` on the current drive. `None` when `root`'s segments do not
/// begin `path`'s.
pub(crate) fn windows_segments_below<'a>(root: &str, path: &'a str) -> Option<Vec<&'a str>> {
    fn parts(path: &str) -> (bool, Vec<&str>) {
        let (unc, rest) = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            (true, rest)
        } else if let Some(rest) = path.strip_prefix(r"\\?\") {
            (false, rest)
        } else {
            let mut chars = path.chars();
            let separator = |c: Option<char>| matches!(c, Some('/' | '\\'));
            (separator(chars.next()) && separator(chars.next()), path)
        };
        (unc, segments(rest))
    }
    let same = |a: &&str, b: &&str| a == b || (a.len() == 2 && a.ends_with(':') && a.eq_ignore_ascii_case(b));
    let (root_unc, root) = parts(root);
    let (path_unc, path) = parts(path);
    let begins =
        root_unc == path_unc && path.len() >= root.len() && root.iter().zip(&path).all(|(a, b)| same(a, b));
    begins.then(|| path[root.len()..].to_vec())
}

/// The candidate `project` names, found by probing that one path rather than
/// walking the folder: fresh, and not bounded by the walk's entry or
/// candidate limits. `project` is a path relative to `root` (`/` or `\`
/// separated) or a rooted path below it. It accepts exactly what [`walk`]
/// would list with the default limits: at most [`Limits::max_depth`]
/// segments, none skipped, each a real directory (not a symlink), the last
/// one marked and none before it.
///
/// `has_root` rather than `is_absolute` picks the branch: on Windows
/// `/root/a` has a root but no drive. A rooted path whose segments below
/// `root` name no candidate (a symlink inside it, `..` in it) is retried
/// canonicalized.
pub fn resolve(root: &Path, project: &str) -> Result<Candidate, Unresolved> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let as_path = Path::new(project);
    if !as_path.has_root() {
        return resolve_segments(&root, &segments(project));
    }
    let Some(rel) = relative_to_root(&root, as_path) else {
        return Err(Unresolved::NotBelowRoot);
    };
    let rel = rel.to_string_lossy().into_owned();
    let first = resolve_segments(&root, &segments(&rel));
    if first.is_ok() {
        return first;
    }
    let canonical =
        as_path.canonicalize().ok().and_then(|path| path.strip_prefix(&root).ok().map(Path::to_path_buf));
    match canonical {
        Some(canonical) if canonical.to_string_lossy() != rel => {
            resolve_segments(&root, &segments(&canonical.to_string_lossy())).or(first)
        }
        _ => first,
    }
}

fn resolve_segments(root: &Path, segments: &[&str]) -> Result<Candidate, Unresolved> {
    let max_depth = Limits::default().max_depth;
    if segments.contains(&"..") {
        return Err(Unresolved::NotBelowRoot);
    }
    if segments.is_empty() {
        return Err(Unresolved::IsRoot);
    }
    if segments.len() > max_depth {
        return Err(Unresolved::TooDeep { max_depth });
    }
    if let Some(skipped) = segments.iter().find(|segment| is_skipped(segment)) {
        return Err(Unresolved::Skipped((*skipped).to_string()));
    }
    let mut dir = root.to_path_buf();
    for (index, segment) in segments.iter().enumerate() {
        dir.push(segment);
        let rel = segments[..=index].join("/");
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(Unresolved::NotADirectory(rel)),
            Err(_) => return Err(Unresolved::Missing(rel)),
        }
        let (markers, is_worktree) = probe(&dir);
        if index + 1 < segments.len() {
            if !markers.is_empty() {
                return Err(Unresolved::InsideCandidate(rel));
            }
            continue;
        }
        if markers.is_empty() {
            return Err(Unresolved::NoMarker);
        }
        return Ok(Candidate { rel_path: rel, abs_path: dir, markers, is_worktree });
    }
    unreachable!("segments is not empty")
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

    /// The generation check behind the front's "(indexed)": this build's
    /// schema and core pipeline, whatever the plugin half says; anything
    /// else, or no generation at all, is not current.
    #[test]
    fn only_this_builds_schema_and_core_pipeline_are_the_current_generation() {
        let state = tempfile::tempdir().unwrap();
        index_with_bulk_indexed_at(state.path(), true);
        let conn = open_existing_index(state.path()).unwrap();
        assert!(current_generation(&conn), "the generation `ensure_current` just wrote");

        let set = |schema_version: &str, indexer_version: &str| {
            conn.execute(
                "UPDATE meta SET schema_version = ?1, indexer_version = ?2 WHERE id = 1",
                [schema_version, indexer_version],
            )
            .unwrap();
        };
        let core = schema::CURRENT_INDEXER_VERSION;
        let schema_now = schema::CURRENT_SCHEMA_VERSION;

        set(schema_now, &format!("{core}+another-plugin-digest"));
        assert!(current_generation(&conn), "the plugin half is not compared");

        for (schema_version, indexer_version) in [
            (schema_now, "stale".to_string()),
            (schema_now, format!("{core}0+digest")),
            (schema_now, core.to_string()),
            (schema_now, String::new()),
            ("0", format!("{core}+digest")),
        ] {
            set(schema_version, &indexer_version);
            assert!(!current_generation(&conn), "schema {schema_version:?}, indexer {indexer_version:?}");
        }

        let empty = tempfile::tempdir().unwrap();
        Connection::open(empty.path().join("index.db")).unwrap();
        assert!(!current_generation(&open_existing_index(empty.path()).unwrap()), "no meta table");
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

    /// A folder holding the projects `a` and `group/c`, canonical.
    fn resolve_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        touch(&root, "a/.git");
        touch(&root, "group/c/go.mod");
        let root = root.canonicalize().unwrap();
        (dir, root)
    }

    fn resolved(root: &Path, project: &str) -> Option<String> {
        resolve(root, project).ok().map(|c| c.rel_path)
    }

    #[test]
    fn a_project_is_found_by_its_relative_path() {
        let (_dir, root) = resolve_fixture();
        let hit = |project: &str| resolved(&root, project);
        assert_eq!(hit("group/c/").as_deref(), Some("group/c"));
        assert_eq!(hit("./a").as_deref(), Some("a"));
        assert_eq!(hit("group\\c").as_deref(), Some("group/c"));
        assert_eq!(hit(".\\group/c\\").as_deref(), Some("group/c"), "mixed separators");
        assert_eq!(resolve(&root, "c"), Err(Unresolved::Missing("c".into())));
        assert_eq!(resolve(&root, "group"), Err(Unresolved::NoMarker));
    }

    /// A path with a root is matched below the served root by its segments.
    /// On Windows `/root/group/c` has a root but no drive, so it is not
    /// absolute: this is the shape an `is_absolute` branch sent down the
    /// relative path.
    #[test]
    fn a_project_is_found_by_a_rooted_path_under_the_root() {
        let (dir, root) = resolve_fixture();
        let under = |rel: &str| format!("{}/{rel}", root.display());
        assert_eq!(resolved(&root, &under("group/c")).as_deref(), Some("group/c"));
        assert_eq!(resolved(&root, &under("a/")).as_deref(), Some("a"));
        assert_eq!(resolved(&root, &under("group\\c")).as_deref(), Some("group/c"), "mixed separators");
        assert_eq!(resolved(&root, &under("c")), None, "whole segments, not a suffix");
        assert_eq!(
            resolve(&root, &under("group")),
            Err(Unresolved::NoMarker),
            "a directory above a candidate"
        );
        let elsewhere = dir.path().join("elsewhere").join("group").join("c");
        assert_eq!(resolve(&root, elsewhere.to_str().unwrap()), Err(Unresolved::NotBelowRoot));
    }

    /// The root is canonical; a project path spelling it differently (here a
    /// symlink) is retried canonicalized.
    #[cfg(unix)]
    #[test]
    fn a_project_through_another_spelling_of_the_root_is_found() {
        let (dir, root) = resolve_fixture();
        std::os::unix::fs::symlink(&root, dir.path().join("link")).unwrap();
        let project = dir.path().join("link").join("group").join("c");
        assert_eq!(resolved(&root, project.to_str().unwrap()).as_deref(), Some("group/c"));
    }

    /// The Windows shape of the test above: the served root carries the
    /// verbatim prefix `canonicalize` adds, the client's path does not.
    #[cfg(windows)]
    #[test]
    fn a_project_without_the_verbatim_prefix_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("root");
        touch(&plain, "group/c/go.mod");
        let root = plain.canonicalize().unwrap();
        let project = plain.join("group").join("c");
        assert_eq!(resolved(&root, project.to_str().unwrap()).as_deref(), Some("group/c"));
    }

    /// `\\?\C:\u\root/group\c` is what a client gets by joining the served
    /// (verbatim) root with a `/` path: below the root by its segments.
    #[test]
    fn a_verbatim_root_takes_a_suffix_in_either_separator() {
        let root = r"\\?\C:\u\root";
        let below = |path: &'static str| windows_segments_below(root, path);
        assert_eq!(below(r"\\?\C:\u\root/group\c"), Some(vec!["group", "c"]));
        assert_eq!(below(r"\\?\C:\u\root/group/c/"), Some(vec!["group", "c"]));
        assert_eq!(below(r"\\?\C:\u\root\a"), Some(vec!["a"]));
        assert_eq!(below(r"\\?\C:\u\root"), Some(vec![]), "the root itself");
    }

    /// The verbatim prefix is a spelling, not a different place.
    #[test]
    fn verbatim_and_plain_spellings_of_a_drive_path_match_each_other() {
        assert_eq!(windows_segments_below(r"\\?\C:\u\root", "C:/u/root/a"), Some(vec!["a"]));
        assert_eq!(windows_segments_below(r"\\?\C:\u\root", r"C:\u\root\group/c"), Some(vec!["group", "c"]));
        assert_eq!(windows_segments_below(r"C:\u\root", r"\\?\C:\u\root\a"), Some(vec!["a"]));
        assert_eq!(windows_segments_below("C:/u/root", r"C:\u\root\a"), Some(vec!["a"]));
    }

    #[test]
    fn drive_letters_match_ignoring_case() {
        assert_eq!(windows_segments_below(r"C:\u\root", r"c:\u\root\a"), Some(vec!["a"]));
        assert_eq!(windows_segments_below(r"c:\u\root", "C:/u/root/a"), Some(vec!["a"]));
        assert_eq!(windows_segments_below(r"\\?\C:\u\root", "c:/u/root/a"), Some(vec!["a"]));
        assert_eq!(windows_segments_below(r"C:\u\root", r"D:\u\root\a"), None, "another drive");
    }

    #[test]
    fn only_whole_segments_below_the_root_match() {
        let root = r"\\?\C:\u\root";
        let below = |path: &'static str| windows_segments_below(root, path);
        assert_eq!(below(r"\\?\C:\u\rootx\a"), None, "a longer last segment");
        assert_eq!(below("C:/u/rootx/a"), None);
        assert_eq!(below(r"C:\u\roo\a"), None, "a shorter last segment");
        assert_eq!(below(r"C:\u"), None, "above the root");
        assert_eq!(below(r"C:\v\root\a"), None, "a differing middle segment");
    }

    /// `\\srv\share` is a network share; `\srv\share` is a folder on the
    /// current drive. Each matches only a root of its own kind.
    #[test]
    fn a_share_path_matches_only_a_share_root() {
        let root = r"\\?\UNC\srv\share\r";
        let below = |path: &'static str| windows_segments_below(root, path);
        assert_eq!(below(r"\\srv\share\r\a"), Some(vec!["a"]));
        assert_eq!(below("//srv/share/r/a"), Some(vec!["a"]));
        assert_eq!(below(r"\\?\UNC\srv\share\r/a"), Some(vec!["a"]));
        assert_eq!(below(r"\srv\share\r\a"), None, "a drive-relative path is not the share");
        assert_eq!(below(r"\\?\srv\share\r\a"), None, "a verbatim drive-style path is not the share");
        assert_eq!(windows_segments_below(r"\srv\share\r", r"\\srv\share\r\a"), None);
        assert_eq!(windows_segments_below(r"\\srv\share\r", r"\\?\UNC\srv\share\r\a"), Some(vec!["a"]));
    }

    /// Segments with `..` are no answer: the path is retried canonicalized,
    /// so `group\..\group\c` below the root is `group\c`, not itself.
    #[cfg(windows)]
    #[test]
    fn a_dotdot_below_a_verbatim_root_is_matched_by_its_canonical_form() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("root"), "group/c/go.mod");
        let root = dir.path().join("root").canonicalize().unwrap();
        let plain = root.to_str().unwrap().strip_prefix(r"\\?\").expect("a canonical root is verbatim");
        let project = Path::new(plain).join("group").join("..").join("group").join("c");
        assert_eq!(relative_to_root(&root, &project), Some(PathBuf::from("group").join("c")));
    }

    /// A rooted path below the root whose segments name no candidate (a
    /// symlink inside the root, a `..`) still resolves to the candidate its
    /// canonical form is.
    #[cfg(unix)]
    #[test]
    fn a_project_whose_segments_differ_is_found_by_its_canonical_path() {
        let (_dir, root) = resolve_fixture();
        std::os::unix::fs::symlink(root.join("group").join("c"), root.join("alias")).unwrap();
        let hit = |project: PathBuf| resolved(&root, project.to_str().unwrap());
        assert_eq!(hit(root.join("alias")).as_deref(), Some("group/c"));
        assert_eq!(hit(root.join("group").join("..").join("group").join("c")).as_deref(), Some("group/c"));
        assert_eq!(resolve(&root, "alias"), Err(Unresolved::NotADirectory("alias".into())));
        assert_eq!(resolve(&root, "../root/a"), Err(Unresolved::NotBelowRoot));
    }

    // ---- GM-571 T6: `resolve` agrees with `walk` ----

    /// A folder with every shape the walk distinguishes: repositories, a
    /// worktree, a project at depth two, a repository with nested packages,
    /// skipped and dot directories with markers inside, a marker at depth
    /// three, an unmarked directory, a file and (on unix) a symlink to a
    /// repository.
    fn agreement_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, "a/.git");
        touch(root, "b/Cargo.toml");
        touch(root, "group/c/go.mod");
        touch(root, "worktrees/wt/.git");
        mkdir(root, "mono/.git");
        touch(root, "mono/y/package.json");
        touch(root, "mono/plugins/x/package.json");
        touch(root, "node_modules/x/package.json");
        touch(root, "target/pyproject.toml");
        touch(root, ".hidden/go.mod");
        touch(root, "x/y/z/go.mod");
        mkdir(root, "plain/dir");
        touch(root, "file.txt");
        let elsewhere = tempfile::tempdir().unwrap();
        mkdir(elsewhere.path(), "repo/.git");
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path().join("repo"), root.join("linked")).unwrap();
        let canonical = root.canonicalize().unwrap();
        (dir, elsewhere, canonical)
    }

    /// Every entry below `root` down to `depth` levels, `/`-separated,
    /// symlinks included but not followed.
    fn entries_below(root: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
        if depth == 0 {
            return;
        }
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            out.push(child.clone());
            if entry.file_type().unwrap().is_dir() {
                entries_below(root, &child, depth - 1, out);
            }
        }
    }

    /// D6 risk: two implementations of "what is a candidate". Every walk
    /// candidate resolves to the very same `Candidate`, and of every entry
    /// in the fixture down to depth three, exactly the walk's candidates
    /// resolve.
    ///
    /// Control C6: in `resolve_segments`, drop the "no marked ancestor"
    /// check (the `InsideCandidate` return): `mono/y` resolves although the
    /// walk never lists it.
    #[test]
    fn resolve_accepts_exactly_the_walks_candidates() {
        let (_dir, _elsewhere, root) = agreement_fixture();
        let walked = walk(&root, Limits::default());
        assert!(!walked.truncated);
        assert_eq!(rels(&walked.candidates), ["a", "b", "group/c", "mono", "worktrees/wt"]);
        for candidate in &walked.candidates {
            assert_eq!(resolve(&root, &candidate.rel_path).as_ref(), Ok(candidate), "{}", candidate.rel_path);
        }

        let mut entries = Vec::new();
        entries_below(&root, "", 3, &mut entries);
        assert!(entries.len() > 20, "the fixture must be walked: {entries:?}");
        let listed: Vec<&str> = rels(&walked.candidates);
        for entry in &entries {
            assert_eq!(
                resolve(&root, entry).is_ok(),
                listed.contains(&entry.as_str()),
                "{entry}: {:?}",
                resolve(&root, entry)
            );
        }
    }

    /// Each kind of path the walk would never list is refused with its own
    /// reason.
    ///
    /// Control C6 (as above): `mono/y` resolves instead of being refused.
    #[test]
    fn resolve_refuses_nested_skipped_symlinked_and_deep_dirs() {
        let (_dir, _elsewhere, root) = agreement_fixture();
        assert_eq!(resolve(&root, "mono/y"), Err(Unresolved::InsideCandidate("mono".into())));
        assert_eq!(resolve(&root, "node_modules/x"), Err(Unresolved::Skipped("node_modules".into())));
        assert_eq!(resolve(&root, "target"), Err(Unresolved::Skipped("target".into())));
        assert_eq!(resolve(&root, ".hidden"), Err(Unresolved::Skipped(".hidden".into())));
        assert_eq!(resolve(&root, "x/y/z"), Err(Unresolved::TooDeep { max_depth: 2 }));
        assert_eq!(resolve(&root, "plain"), Err(Unresolved::NoMarker));
        assert_eq!(resolve(&root, "file.txt"), Err(Unresolved::NotADirectory("file.txt".into())));
        assert_eq!(resolve(&root, "nope"), Err(Unresolved::Missing("nope".into())));
        assert_eq!(resolve(&root, "../a"), Err(Unresolved::NotBelowRoot));
        assert_eq!(resolve(&root, ""), Err(Unresolved::IsRoot));
        #[cfg(unix)]
        assert_eq!(resolve(&root, "linked"), Err(Unresolved::NotADirectory("linked".into())));

        let reason = resolve(&root, "mono/y").unwrap_err().to_string();
        assert_eq!(reason, "it lies inside the project 'mono'");
        let reason = resolve(&root, "x/y/z").unwrap_err().to_string();
        assert_eq!(reason, "projects lie at most 2 levels below the folder");
    }

    /// The walk stops at its candidate limit; `resolve` probes one path and
    /// finds every project, the ones past the limit included.
    #[test]
    fn resolve_finds_a_project_past_the_walk_cap() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..3 {
            mkdir(dir.path(), &format!("r{n}/.git"));
        }
        let root = dir.path().canonicalize().unwrap();
        let walked = walk(&root, Limits { max_candidates: 1, ..Limits::default() });
        assert!(walked.truncated);
        assert_eq!(walked.candidates.len(), 1);
        for n in 0..3 {
            let project = format!("r{n}");
            let found = resolve(&root, &project).unwrap_or_else(|err| panic!("{project}: {err}"));
            assert_eq!(found.abs_path, root.join(&project));
            assert_eq!(found.markers, [".git"]);
        }
    }
}
