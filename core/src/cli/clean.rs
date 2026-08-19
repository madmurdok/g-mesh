//! `g-mesh clean`: delete cached project indexes under `~/.g-mesh/projects/`.
//!
//! Five forms, differing only in what they are scoped to:
//!
//! | invocation | scope |
//! |---|---|
//! | `g-mesh clean` | the current directory's project |
//! | `g-mesh clean <project-id>` | that one project |
//! | `g-mesh clean expired` | every project idle longer than the threshold |
//! | `g-mesh clean orphaned` | every project whose directory was deleted, with `--force` |
//! | `g-mesh clean all` | every project, with `--force` |
//!
//! # Why `all` and `orphaned` ask for confirmation
//!
//! The first three are already scoped by something the caller said or that
//! the data itself established, so a confirmation prompt on top would be the
//! kind that gets typed through without reading. `all`'s blast radius is
//! "everything, including the project you are standing in", so without
//! `--force` it reports the count it *would* have deleted and removes
//! nothing.
//!
//! `orphaned` is data-scoped like `expired`, and still asks, because its
//! criterion is far less conservative: "the project directory is not there"
//! is a fact about this instant, where "idle for 90 days" is a fact about a
//! quarter of a year. The preview is what lets a caller notice that the
//! answer changed because a disk is unmounted - see `RootState` for the rest
//! of that guard.
//!
//! None of this is dangerous by construction, and for the same reason schema
//! versioning can wipe and rebuild: an index is a reproducible local cache,
//! not user data. Deleting one costs a reindex, never work.
//!
//! # Projects with a live daemon
//!
//! Deleting the state directory out from under a running daemon leaves it
//! serving a database that no longer exists. An explicitly named project (the
//! first two forms) is therefore refused, pointing at `g-mesh stop`; a bulk
//! form skips it and says how many it skipped, since silently corrupting a
//! running daemon is not what "delete the idle ones" asked for.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cli::CleanArgs;
use crate::config;
use crate::daemon::{self, identity::project_hash};
use crate::gc::last_used;
use crate::storage::connection::projects_root;

/// Converts a day count from `cleanup.idleThresholdDays` into the [`Duration`]
/// [`Candidate::is_expired`] compares against.
fn idle_threshold(idle_threshold_days: u64) -> Duration {
    Duration::from_secs(idle_threshold_days * 24 * 60 * 60)
}

/// What a `clean` invocation is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// No argument: the project the current directory belongs to.
    Cwd,
    /// One project, named by the `<hash>` directory it lives in.
    Project(String),
    /// Every project idle for longer than `cleanup.idleThresholdDays`
    /// (default 90 - see [`config::CleanupConfig`]).
    Expired,
    /// Every project whose recorded root no longer exists on disk.
    Orphaned,
    /// Every project.
    All,
}

impl Target {
    /// Reads the single positional argument `clean` takes.
    ///
    /// `expired`, `orphaned` and `all` can never be mistaken for a project
    /// id: ids are the 16 hex characters `daemon::identity::project_hash`
    /// produces, which none of those words is.
    pub fn parse(argument: Option<&str>) -> Self {
        match argument {
            None => Target::Cwd,
            Some("expired") => Target::Expired,
            Some("orphaned") => Target::Orphaned,
            Some("all") => Target::All,
            Some(id) => Target::Project(id.to_string()),
        }
    }
}

/// Which set of projects a bulk delete covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Expired,
    All,
}

/// What a `clean` invocation actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// One explicitly scoped project was deleted.
    Deleted { id: String, path: PathBuf },
    /// A bulk form ran. `skipped_running` names the projects it left alone
    /// because a daemon was serving them. `idle_threshold_days` is the
    /// threshold actually applied (only meaningful for `Scope::Expired`, but
    /// carried unconditionally so `render` never has to reconstruct it) -
    /// from `cleanup.idleThresholdDays`, not necessarily the documented
    /// default.
    DeletedMany {
        scope: Scope,
        ids: Vec<String>,
        skipped_running: Vec<String>,
        idle_threshold_days: u64,
    },
    /// `clean all` without `--force`: a count, and nothing touched.
    WouldDelete { count: usize },
    /// `clean orphaned` ran. `deleted` is false when `--force` was not given,
    /// in which case `orphaned` names what *would* have gone and nothing was
    /// touched.
    ///
    /// The three groups are reported separately rather than summed because
    /// they mean different things to whoever reads them: `orphaned` is state
    /// this command handles, while `unreachable` and `legacy` are state it
    /// deliberately refuses to judge and the caller may still want to deal
    /// with by hand.
    Orphaned {
        deleted: bool,
        orphaned: Vec<Orphan>,
        skipped_running: Vec<String>,
        unreachable: Vec<Orphan>,
        legacy: usize,
    },
}

/// A project state directory and the root it says it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub id: String,
    pub root: PathBuf,
}

/// Runs the `clean` the parsed arguments describe.
pub fn run(args: &CleanArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let root = projects_root().context("failed to resolve ~/.g-mesh/projects")?;
    let idle_threshold_days = config::read_global_config()?.cleanup.idle_threshold_days;
    let outcome = clean(
        &Target::parse(args.target.as_deref()),
        args.force,
        &root,
        &cwd,
        idle_threshold_days,
    )?;
    print!("{}", render(&outcome));
    Ok(())
}

/// Deletes whatever `target` names under `projects_root`.
///
/// `projects_root` and `cwd` are parameters rather than read from the
/// environment so that the whole command - including the variants that delete
/// every project there is - can be tested against a directory that is not the
/// developer's own. `idle_threshold_days` is a parameter for the same reason:
/// it is `cleanup.idleThresholdDays` from the global config, but read by the
/// caller (`run`) rather than here, so a test can exercise a shortened
/// threshold without a real `~/.g-mesh/config.toml` in the picture.
pub fn clean(
    target: &Target,
    force: bool,
    projects_root: &Path,
    cwd: &Path,
    idle_threshold_days: u64,
) -> Result<Outcome> {
    match target {
        Target::Cwd => clean_one(&cwd_project_id(cwd)?, projects_root, CwdScoped::Yes),
        Target::Project(id) => clean_one(id, projects_root, CwdScoped::No),
        Target::Expired => clean_many(Scope::Expired, projects_root, idle_threshold_days),
        Target::Orphaned => clean_orphaned(force, projects_root),
        Target::All => {
            if !force {
                return Ok(Outcome::WouldDelete { count: candidates(projects_root)?.len() });
            }
            clean_many(Scope::All, projects_root, idle_threshold_days)
        }
    }
}

/// Only affects the wording of the "not a project" error - which is the whole
/// difference between the two single-project forms.
enum CwdScoped {
    Yes,
    No,
}

fn cwd_project_id(cwd: &Path) -> Result<String> {
    project_hash(cwd).with_context(|| {
        format!("failed to derive a project id for {}", cwd.display())
    })
}

fn clean_one(id: &str, projects_root: &Path, scoped: CwdScoped) -> Result<Outcome> {
    validate_id(id)?;
    let path = projects_root.join(id);

    if !path.is_dir() {
        match scoped {
            // Per the requirements: an unrecognized cwd asks for an explicit
            // id rather than picking something arbitrary to delete.
            CwdScoped::Yes => bail!(
                "the current directory has no g-mesh index (nothing at {}) - \
                 pass an explicit <project-id>, or run `g-mesh clean all` to see what there is",
                path.display()
            ),
            CwdScoped::No => bail!("no such project `{id}` (nothing at {})", path.display()),
        }
    }

    if daemon_is_running(&path) {
        bail!(
            "a daemon is still serving project `{id}` - run `g-mesh stop` in that project first"
        );
    }

    delete(&path)?;
    Ok(Outcome::Deleted { id: id.to_string(), path })
}

fn clean_many(scope: Scope, projects_root: &Path, idle_threshold_days: u64) -> Result<Outcome> {
    let mut ids = Vec::new();
    let mut skipped_running = Vec::new();
    let threshold = idle_threshold(idle_threshold_days);

    for candidate in candidates(projects_root)? {
        if matches!(scope, Scope::Expired) && !candidate.is_expired(threshold) {
            continue;
        }
        if candidate.daemon_running {
            skipped_running.push(candidate.id);
            continue;
        }
        delete(&candidate.path)?;
        ids.push(candidate.id);
    }

    Ok(Outcome::DeletedMany { scope, ids, skipped_running, idle_threshold_days })
}

/// What the recorded project root of a state directory says about it.
///
/// The distinction that carries the whole risk of this command is the last
/// two. A root that is missing *along with the directory that contained it*
/// is exactly what an unmounted external disk looks like: `/Volumes/Ext/proj`
/// and `/Volumes/Ext` both vanish when the disk is ejected, while deleting a
/// project on a filesystem that is still mounted leaves its parent in place.
/// Treating "not present right now" as "gone forever" is the one way this
/// feature could destroy an index someone wanted, so an absent parent is
/// reported and never deleted.
///
/// Deleting a whole tree of projects at once therefore also reads as
/// `Unreachable`. That is the trade accepted knowingly: those are cleaned by
/// id, and being asked to name them is a smaller cost than a delete nobody
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    /// The project is still on disk.
    Live,
    /// The project directory is gone, but the directory that held it is
    /// still there - so the filesystem it lived on is present and it really
    /// was deleted.
    Orphaned,
    /// The root is missing and so is its parent: possibly deleted, possibly
    /// an unmounted volume or a moved tree. Not judged.
    Unreachable,
    /// No `project.root` file - a state directory created before g-mesh
    /// recorded roots. Not judged, and never guessed at: `project_hash` is
    /// one-way, so there is nothing to guess *from*.
    Legacy,
}

fn root_state(root: Option<&Path>) -> RootState {
    let Some(root) = root else {
        return RootState::Legacy;
    };
    if root.exists() {
        return RootState::Live;
    }
    match root.parent() {
        Some(parent) if parent.exists() => RootState::Orphaned,
        _ => RootState::Unreachable,
    }
}

/// `clean orphaned`: deletes state whose project directory was deleted.
///
/// Without `force` this is a preview - the same posture `all` takes, for the
/// reason this module's doc comment gives.
fn clean_orphaned(force: bool, projects_root: &Path) -> Result<Outcome> {
    let mut orphaned = Vec::new();
    let mut unreachable = Vec::new();
    let mut skipped_running = Vec::new();
    let mut legacy = 0;

    for candidate in candidates(projects_root)? {
        let entry = || Orphan {
            id: candidate.id.clone(),
            root: candidate.root.clone().unwrap_or_default(),
        };
        match root_state(candidate.root.as_deref()) {
            RootState::Live => continue,
            RootState::Legacy => legacy += 1,
            RootState::Unreachable => unreachable.push(entry()),
            RootState::Orphaned => {
                // A daemon still serving a project whose directory was
                // deleted is unusual but not impossible - it holds the index
                // open and would be left writing to a path that no longer
                // exists. Skipped for the same reason every other bulk form
                // skips it.
                if candidate.daemon_running {
                    skipped_running.push(candidate.id);
                    continue;
                }
                if force {
                    delete(&candidate.path)?;
                }
                orphaned.push(entry());
            }
        }
    }

    Ok(Outcome::Orphaned { deleted: force, orphaned, skipped_running, unreachable, legacy })
}

/// Rejects anything that is not a bare directory name.
///
/// The id is joined onto `~/.g-mesh/projects/`, so without this a `..` or an
/// absolute path in it would make `clean` delete a directory that has nothing
/// to do with g-mesh. Real ids are hex hashes; alphanumeric is a shade
/// looser, and still admits no separator, no dot, and no traversal.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "`{id}` is not a project id - ids are the directory names under \
             ~/.g-mesh/projects/, not paths"
        );
    }
    Ok(())
}

struct Candidate {
    id: String,
    path: PathBuf,
    /// `None` when the project has no readable `lastUsed` - which is treated
    /// as "unknown", never as "expired". A project whose idle time cannot be
    /// established is exactly the one not to delete on a rule about idle time.
    idle: Option<Duration>,
    daemon_running: bool,
    /// The project root this directory records, or `None` if it records none
    /// - see [`RootState::Legacy`].
    root: Option<PathBuf>,
}

impl Candidate {
    fn is_expired(&self, threshold: Duration) -> bool {
        self.idle.is_some_and(|idle| idle > threshold)
    }
}

/// Every project directory under `projects_root`, sorted by id.
///
/// A missing root is an empty list, not an error: nothing has ever been
/// indexed on this machine, which is a perfectly good answer to "what is
/// there to clean?".
fn candidates(projects_root: &Path) -> Result<Vec<Candidate>> {
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(projects_root)
        .with_context(|| format!("failed to read {}", projects_root.display()))?;

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", projects_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        candidates.push(Candidate {
            // A directory whose index cannot be read at all still counts as a
            // project - it just has no idle time, so only `all` will take it.
            idle: last_used::read_from_project_dir(&path)
                .unwrap_or(None)
                .map(|record| record.idle),
            daemon_running: daemon_is_running(&path),
            root: daemon::identity::read_project_root(&path),
            id,
            path,
        });
    }

    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(candidates)
}

/// Whether anything is still running for this state directory - the core, or
/// any language's plugin that outlived it. Lists every `plugin-<language>.pid`
/// file actually present (`daemon::registry::discovered_pid_files`) rather
/// than assuming one hardcoded plugin - a live Python plugin orphaned by a
/// dead core must stop this project from being cleaned up exactly as much as
/// a live JS/TS one always has.
fn daemon_is_running(state_dir: &Path) -> bool {
    let mut pid_files = vec![daemon::pid_path_in(state_dir)];
    pid_files.extend(daemon::registry::discovered_pid_files(state_dir).into_iter().map(|(_, path)| path));
    pid_files.iter().filter_map(|path| daemon::read_pid_file(path)).any(daemon::is_process_alive)
}

fn delete(path: &Path) -> Result<()> {
    fs::remove_dir_all(path)
        .with_context(|| format!("failed to delete {}", path.display()))
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Deleted { id, path } => {
            format!("g-mesh: deleted project {id} ({})\n", path.display())
        }
        Outcome::WouldDelete { count } => {
            if *count == 0 {
                return "g-mesh: there are no project indexes to delete\n".to_string();
            }
            format!(
                "g-mesh: {count} project index(es) would be deleted - \
                 re-run with --force to confirm. Nothing was deleted.\n"
            )
        }
        Outcome::Orphaned { deleted, orphaned, skipped_running, unreachable, legacy } => {
            let mut out = String::new();
            match (orphaned.len(), deleted) {
                (0, _) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: no project index belongs to a directory that has been deleted"
                    );
                }
                (count, true) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: deleted {count} project index(es) whose project directory is gone"
                    );
                }
                (count, false) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: {count} project index(es) belong to a directory that has been \
                         deleted - re-run with --force to confirm. Nothing was deleted."
                    );
                }
            }
            for orphan in orphaned {
                let _ = writeln!(out, "  {}  {}", orphan.id, orphan.root.display());
            }
            if !skipped_running.is_empty() {
                let _ = writeln!(
                    out,
                    "  skipped {} project(s) with a running daemon - run `g-mesh stop` in each first:",
                    skipped_running.len()
                );
                for id in skipped_running {
                    let _ = writeln!(out, "    {id}");
                }
            }
            if !unreachable.is_empty() {
                let _ = writeln!(
                    out,
                    "  left alone: {} project(s) whose root is missing along with the directory \
                     that held it - an unmounted disk looks exactly like this. Delete one by id \
                     if you are sure:",
                    unreachable.len()
                );
                for orphan in unreachable {
                    let _ = writeln!(out, "    {}  {}", orphan.id, orphan.root.display());
                }
            }
            if *legacy > 0 {
                let _ = writeln!(
                    out,
                    "  left alone: {legacy} project(s) indexed before g-mesh recorded project \
                     roots, so there is nothing to check them against. A project still in use \
                     records its root the next time its daemon starts."
                );
            }
            out
        }
        Outcome::DeletedMany { scope, ids, skipped_running, idle_threshold_days } => {
            let mut out = String::new();
            match (scope, ids.len()) {
                (Scope::Expired, 0) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: no project has been idle for more than {idle_threshold_days} days"
                    );
                }
                (Scope::Expired, count) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: deleted {count} project index(es) idle for more than {idle_threshold_days} days"
                    );
                }
                (Scope::All, 0) => {
                    let _ = writeln!(out, "g-mesh: there were no project indexes to delete");
                }
                (Scope::All, count) => {
                    let _ = writeln!(out, "g-mesh: deleted {count} project index(es)");
                }
            }
            for id in ids {
                let _ = writeln!(out, "  {id}");
            }
            if !skipped_running.is_empty() {
                let _ = writeln!(
                    out,
                    "  skipped {} project(s) with a running daemon - run `g-mesh stop` in each first:",
                    skipped_running.len()
                );
                for id in skipped_running {
                    let _ = writeln!(out, "    {id}");
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A `~/.g-mesh/projects/` of its own, so every test here - including the
    /// ones that delete everything - is confined to a temp directory.
    struct Projects {
        dir: tempfile::TempDir,
    }

    impl Projects {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().unwrap() }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// A project directory with an index whose `lastUsed` is `idle_days`
        /// old, exactly as a real one would record it.
        fn add(&self, id: &str, idle_days: u64) -> PathBuf {
            let path = self.add_empty(id);
            let conn = Connection::open(path.join("index.db")).unwrap();
            crate::storage::schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
            conn.execute(
                "UPDATE meta SET lastUsed = datetime('now', ?1) WHERE id = 1",
                rusqlite::params![format!("-{idle_days} days")],
            )
            .unwrap();
            path
        }

        /// A project directory with no index at all - a bootstrap that never
        /// finished, which has no idle time to judge it by.
        fn add_empty(&self, id: &str) -> PathBuf {
            let path = self.root().join(id);
            fs::create_dir_all(&path).unwrap();
            path
        }

        /// Records `root` as the project directory `id`'s state belongs to,
        /// exactly as `connection::ensure_project_dir` does for a real one.
        fn record_root(&self, id: &str, root: &Path) {
            crate::daemon::identity::record_project_root(&self.root().join(id), root).unwrap();
        }

        /// Marks a project as served by a live daemon, using this test
        /// process's own pid - which is unquestionably alive.
        fn mark_daemon_running(&self, id: &str) {
            fs::write(self.root().join(id).join("daemon.pid"), std::process::id().to_string())
                .unwrap();
        }

        fn exists(&self, id: &str) -> bool {
            self.root().join(id).is_dir()
        }
    }

    /// A cwd that is not any of the fixture's projects.
    fn unrelated_cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The documented default (`cleanup.idleThresholdDays`) - mirrors
    /// `config::CleanupConfig::default()` rather than hard-coding a second
    /// `90` here, so the two can never quietly drift apart.
    fn default_idle_threshold_days() -> u64 {
        config::CleanupConfig::default().idle_threshold_days
    }

    fn clean_in(projects: &Projects, target: Target, force: bool) -> Result<Outcome> {
        clean_in_with_threshold(projects, target, force, default_idle_threshold_days())
    }

    fn clean_in_with_threshold(
        projects: &Projects,
        target: Target,
        force: bool,
        idle_threshold_days: u64,
    ) -> Result<Outcome> {
        let cwd = unrelated_cwd();
        clean(&target, force, projects.root(), cwd.path(), idle_threshold_days)
    }

    /// `expect_err` with the offending input in the panic message.
    trait ExpectRefused {
        fn unwrap_err_or_else(self, id: &str) -> String;
    }

    impl ExpectRefused for Result<Outcome> {
        fn unwrap_err_or_else(self, id: &str) -> String {
            match self {
                Err(err) => err.to_string(),
                Ok(outcome) => panic!("`{id}` must be refused, but it produced {outcome:?}"),
            }
        }
    }

    #[test]
    fn the_positional_argument_selects_the_five_documented_forms() {
        assert_eq!(Target::parse(None), Target::Cwd);
        assert_eq!(Target::parse(Some("expired")), Target::Expired);
        assert_eq!(Target::parse(Some("orphaned")), Target::Orphaned);
        assert_eq!(Target::parse(Some("all")), Target::All);
        assert_eq!(
            Target::parse(Some("a1b2c3d4e5f6a7b8")),
            Target::Project("a1b2c3d4e5f6a7b8".to_string())
        );
    }

    #[test]
    fn cleaning_a_named_project_removes_its_directory_and_nothing_else() {
        let projects = Projects::new();
        projects.add("aaaa1111", 1);
        projects.add("bbbb2222", 1);

        let outcome = clean_in(&projects, Target::Project("aaaa1111".to_string()), false).unwrap();

        assert!(matches!(outcome, Outcome::Deleted { ref id, .. } if id == "aaaa1111"));
        assert!(!projects.exists("aaaa1111"), "the named project must be gone");
        assert!(projects.exists("bbbb2222"), "no other project may be touched");
    }

    #[test]
    fn cleaning_a_project_that_does_not_exist_is_an_error() {
        let projects = Projects::new();

        let err = clean_in(&projects, Target::Project("deadbeef".to_string()), false)
            .expect_err("there is no such project");

        assert!(err.to_string().contains("no such project"), "{err}");
    }

    /// The traversal guard: the id is joined onto the projects root, so
    /// anything path-shaped has to be refused before it gets there.
    #[test]
    fn a_path_shaped_project_id_is_refused_rather_than_joined() {
        let projects = Projects::new();
        let outside = projects.root().parent().unwrap().join("not-a-g-mesh-directory");
        fs::create_dir_all(&outside).unwrap();

        for id in ["..", "../not-a-g-mesh-directory", "/etc", "a/b", "."] {
            let err = clean_in(&projects, Target::Project(id.to_string()), false)
                .unwrap_err_or_else(id);
            assert!(err.contains("is not a project id"), "`{id}` was refused for the wrong reason: {err}");
        }

        assert!(outside.is_dir(), "nothing outside the projects root may be deleted");
        assert!(projects.root().is_dir(), "the projects root itself may not be deleted");
    }

    #[test]
    fn an_unrecognized_cwd_asks_for_an_explicit_id_instead_of_guessing() {
        let projects = Projects::new();
        projects.add("aaaa1111", 1);
        let cwd = unrelated_cwd();

        let err = clean(&Target::Cwd, false, projects.root(), cwd.path(), default_idle_threshold_days())
            .expect_err("this directory has no index");

        assert!(err.to_string().contains("pass an explicit <project-id>"), "{err}");
        assert!(projects.exists("aaaa1111"), "a failed cwd lookup must delete nothing");
    }

    #[test]
    fn cleaning_the_cwds_own_project_removes_it() {
        let projects = Projects::new();
        let cwd = unrelated_cwd();
        let id = project_hash(cwd.path()).unwrap();
        projects.add(&id, 1);

        let outcome =
            clean(&Target::Cwd, false, projects.root(), cwd.path(), default_idle_threshold_days())
                .unwrap();

        assert!(matches!(outcome, Outcome::Deleted { id: ref got, .. } if *got == id));
        assert!(!projects.exists(&id));
    }

    #[test]
    fn expired_deletes_only_what_is_past_the_threshold() {
        let threshold = default_idle_threshold_days();
        let projects = Projects::new();
        projects.add("aaaa1111", threshold + 10);
        projects.add("bbbb2222", threshold - 10);
        projects.add("cccc3333", threshold + 200);

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        match outcome {
            Outcome::DeletedMany { scope, ids, skipped_running, idle_threshold_days } => {
                assert_eq!(scope, Scope::Expired);
                assert_eq!(ids, vec!["aaaa1111", "cccc3333"]);
                assert!(skipped_running.is_empty());
                assert_eq!(idle_threshold_days, threshold);
            }
            other => panic!("expected a bulk delete, got {other:?}"),
        }
        assert!(projects.exists("bbbb2222"), "a project inside the threshold must survive");
    }

    /// The whole point of wiring the threshold to config: a shortened
    /// `cleanup.idleThresholdDays` marks a project stale that the documented
    /// default (90 days) would still consider fresh.
    #[test]
    fn a_shortened_config_threshold_expires_a_project_the_default_would_spare() {
        let projects = Projects::new();
        // Idle 10 days: well inside the 90-day default, so nothing here would
        // move if the threshold were still hard-coded.
        projects.add("aaaa1111", 10);

        let outcome =
            clean_in_with_threshold(&projects, Target::Expired, false, 5).unwrap();

        match outcome {
            Outcome::DeletedMany { ids, idle_threshold_days, .. } => {
                assert_eq!(ids, vec!["aaaa1111"], "a 5-day threshold must treat 10 idle days as stale");
                assert_eq!(idle_threshold_days, 5);
            }
            other => panic!("expected a bulk delete, got {other:?}"),
        }
        assert!(!projects.exists("aaaa1111"));
    }

    /// The same project, judged against the untouched 90-day default, must
    /// survive - the config-driven threshold changes the outcome, not the
    /// default itself.
    #[test]
    fn the_same_idle_time_survives_under_the_unshortened_default() {
        let projects = Projects::new();
        projects.add("aaaa1111", 10);

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        assert!(matches!(outcome, Outcome::DeletedMany { ref ids, .. } if ids.is_empty()));
        assert!(projects.exists("aaaa1111"));
    }

    /// `expired` needs no `--force`: it is already scoped by the data.
    #[test]
    fn expired_needs_no_force() {
        let projects = Projects::new();
        projects.add("aaaa1111", default_idle_threshold_days() + 1);

        clean_in(&projects, Target::Expired, false).unwrap();

        assert!(!projects.exists("aaaa1111"));
    }

    /// An index whose idle time cannot be established is not evidence of
    /// idleness, so `expired` must leave it alone.
    #[test]
    fn a_project_with_no_readable_last_used_is_never_expired() {
        let projects = Projects::new();
        projects.add_empty("aaaa1111");

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        assert!(matches!(outcome, Outcome::DeletedMany { ref ids, .. } if ids.is_empty()));
        assert!(projects.exists("aaaa1111"), "unknown idle time is not expired");
    }

    #[test]
    fn expired_with_nothing_expired_reports_so_and_deletes_nothing() {
        let projects = Projects::new();
        projects.add("aaaa1111", 1);

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        assert!(render(&outcome).contains("no project has been idle"), "{outcome:?}");
        assert!(projects.exists("aaaa1111"));
    }

    /// The acceptance criterion for `all`: without `--force` it prints a
    /// count and deletes nothing at all.
    #[test]
    fn all_without_force_only_counts_and_deletes_nothing() {
        let projects = Projects::new();
        projects.add("aaaa1111", 1);
        projects.add("bbbb2222", 500);
        projects.add_empty("cccc3333");

        let outcome = clean_in(&projects, Target::All, false).unwrap();

        assert_eq!(outcome, Outcome::WouldDelete { count: 3 });
        assert!(render(&outcome).contains("--force"), "the message has to say how to confirm");
        for id in ["aaaa1111", "bbbb2222", "cccc3333"] {
            assert!(projects.exists(id), "`all` without --force must delete nothing");
        }
    }

    #[test]
    fn all_with_force_deletes_every_project_regardless_of_idle_time() {
        let projects = Projects::new();
        projects.add("aaaa1111", 0);
        projects.add("bbbb2222", 500);
        projects.add_empty("cccc3333");

        let outcome = clean_in(&projects, Target::All, true).unwrap();

        match outcome {
            Outcome::DeletedMany { scope, ids, .. } => {
                assert_eq!(scope, Scope::All);
                assert_eq!(ids, vec!["aaaa1111", "bbbb2222", "cccc3333"]);
            }
            other => panic!("expected a bulk delete, got {other:?}"),
        }
        for id in ["aaaa1111", "bbbb2222", "cccc3333"] {
            assert!(!projects.exists(id));
        }
    }

    #[test]
    fn a_project_with_a_live_daemon_is_refused_when_named_explicitly() {
        let projects = Projects::new();
        projects.add("aaaa1111", 500);
        projects.mark_daemon_running("aaaa1111");

        let err = clean_in(&projects, Target::Project("aaaa1111".to_string()), false)
            .expect_err("a served project must not be deleted underneath its daemon");

        assert!(err.to_string().contains("g-mesh stop"), "{err}");
        assert!(projects.exists("aaaa1111"));
    }

    #[test]
    fn a_project_with_a_live_daemon_is_skipped_and_reported_by_the_bulk_forms() {
        let projects = Projects::new();
        projects.add("aaaa1111", 500);
        projects.add("bbbb2222", 500);
        projects.mark_daemon_running("aaaa1111");

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        match &outcome {
            Outcome::DeletedMany { ids, skipped_running, .. } => {
                assert_eq!(ids, &vec!["bbbb2222".to_string()]);
                assert_eq!(skipped_running, &vec!["aaaa1111".to_string()]);
            }
            other => panic!("expected a bulk delete, got {other:?}"),
        }
        assert!(projects.exists("aaaa1111"), "a served project must survive a bulk clean");
        assert!(!projects.exists("bbbb2222"));
        assert!(render(&outcome).contains("running daemon"), "the skip has to be reported");
    }

    /// Nothing indexed on this machine yet is an answer, not a failure.
    #[test]
    fn a_missing_projects_root_is_an_empty_result_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let never_created = dir.path().join("projects");
        let cwd = unrelated_cwd();

        assert_eq!(
            clean(&Target::All, false, &never_created, cwd.path(), default_idle_threshold_days())
                .unwrap(),
            Outcome::WouldDelete { count: 0 }
        );
        let outcome = clean(
            &Target::Expired,
            false,
            &never_created,
            cwd.path(),
            default_idle_threshold_days(),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::DeletedMany { ref ids, .. } if ids.is_empty()));
    }

    // -----------------------------------------------------------------
    // `clean orphaned`
    // -----------------------------------------------------------------

    /// The fixture the whole target has to get right at once: one project
    /// still on disk, one whose directory was deleted, one whose root is
    /// missing along with its parent, one predating recorded roots, and one
    /// orphan a daemon is still serving. Only the second may go.
    struct OrphanFixture {
        projects: Projects,
        /// Held so the live project's directory outlives the assertions.
        _live_root: tempfile::TempDir,
        /// The parent that stays behind when the deleted project goes - what
        /// makes its state `Orphaned` rather than `Unreachable`.
        _surviving_parent: tempfile::TempDir,
    }

    fn orphan_fixture() -> OrphanFixture {
        let projects = Projects::new();

        let live_root = tempfile::tempdir().unwrap();
        projects.add("aaaa1111", 1);
        projects.record_root("aaaa1111", live_root.path());

        // Deleted: created inside a parent that stays, then removed.
        let surviving_parent = tempfile::tempdir().unwrap();
        let deleted = surviving_parent.path().join("gone");
        fs::create_dir(&deleted).unwrap();
        projects.add("bbbb2222", 1);
        projects.record_root("bbbb2222", &deleted);
        fs::remove_dir(&deleted).unwrap();

        // Unreachable: root and its parent both gone, as an ejected disk
        // leaves them. Recorded while both existed, so the path is canonical.
        let volume = tempfile::tempdir().unwrap();
        let mounted = volume.path().join("mount");
        let on_the_volume = mounted.join("project");
        fs::create_dir_all(&on_the_volume).unwrap();
        projects.add("cccc3333", 1);
        projects.record_root("cccc3333", &on_the_volume);
        fs::remove_dir_all(&mounted).unwrap();

        // Legacy: indexed before roots were recorded.
        projects.add("dddd4444", 1);

        // An orphan whose daemon is still running.
        let served = surviving_parent.path().join("served");
        fs::create_dir(&served).unwrap();
        projects.add("eeee5555", 1);
        projects.record_root("eeee5555", &served);
        projects.mark_daemon_running("eeee5555");
        fs::remove_dir(&served).unwrap();

        OrphanFixture {
            projects,
            _live_root: live_root,
            _surviving_parent: surviving_parent,
        }
    }

    /// The acceptance criterion: exactly the state whose project is gone is
    /// deleted, and everything else - including the two kinds this command
    /// refuses to judge - survives.
    #[test]
    fn orphaned_deletes_only_state_whose_project_directory_was_deleted() {
        let fixture = orphan_fixture();

        let outcome = clean_in(&fixture.projects, Target::Orphaned, true).unwrap();

        let Outcome::Orphaned { deleted, orphaned, skipped_running, unreachable, legacy } = outcome
        else {
            panic!("expected an orphaned outcome, got {outcome:?}");
        };
        assert!(deleted);
        assert_eq!(orphaned.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["bbbb2222"]);
        assert_eq!(skipped_running, ["eeee5555"]);
        assert_eq!(unreachable.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["cccc3333"]);
        assert_eq!(legacy, 1);

        assert!(!fixture.projects.exists("bbbb2222"), "the deleted project's state survived");
        for survivor in ["aaaa1111", "cccc3333", "dddd4444", "eeee5555"] {
            assert!(fixture.projects.exists(survivor), "{survivor} must not have been deleted");
        }
    }

    /// Without `--force` it is a report, exactly as `all` is.
    #[test]
    fn orphaned_without_force_names_what_would_go_and_deletes_nothing() {
        let fixture = orphan_fixture();

        let outcome = clean_in(&fixture.projects, Target::Orphaned, false).unwrap();

        let Outcome::Orphaned { deleted, orphaned, .. } = &outcome else {
            panic!("expected an orphaned outcome, got {outcome:?}");
        };
        assert!(!deleted);
        assert_eq!(orphaned.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["bbbb2222"]);
        assert!(fixture.projects.exists("bbbb2222"), "nothing may be deleted without --force");
    }

    /// The classification itself, stated as the four cases rather than only
    /// exercised through a fixture - `root_state` is where the volume guard
    /// lives, and it is worth failing on its own terms.
    #[test]
    fn root_state_tells_a_deleted_project_from_an_unreachable_one() {
        let parent = tempfile::tempdir().unwrap();
        let live = parent.path().join("live");
        fs::create_dir(&live).unwrap();

        assert_eq!(root_state(Some(&live)), RootState::Live);
        assert_eq!(root_state(Some(&parent.path().join("gone"))), RootState::Orphaned);
        assert_eq!(
            root_state(Some(&parent.path().join("unmounted").join("project"))),
            RootState::Unreachable
        );
        assert_eq!(root_state(None), RootState::Legacy);
    }

    #[test]
    fn orphaned_on_a_root_with_nothing_in_it_reports_nothing_to_do() {
        let projects = Projects::new();

        let outcome = clean_in(&projects, Target::Orphaned, true).unwrap();

        assert_eq!(
            outcome,
            Outcome::Orphaned {
                deleted: true,
                orphaned: Vec::new(),
                skipped_running: Vec::new(),
                unreachable: Vec::new(),
                legacy: 0,
            }
        );
        assert!(render(&outcome).contains("no project index belongs to a directory"));
    }

    /// The report has to name the root, not only the id: the id is a hash,
    /// and "which project was that?" is the question a caller about to delete
    /// something actually has.
    #[test]
    fn the_orphaned_report_names_each_root_and_explains_what_it_left_alone() {
        let rendered = render(&Outcome::Orphaned {
            deleted: false,
            orphaned: vec![Orphan {
                id: "a1b2c3d4e5f6a7b8".to_string(),
                root: PathBuf::from("/home/u/work/deleted-project"),
            }],
            skipped_running: Vec::new(),
            unreachable: vec![Orphan {
                id: "b2c3d4e5f6a7b8c9".to_string(),
                root: PathBuf::from("/Volumes/Ext/project"),
            }],
            legacy: 7,
        });

        assert!(rendered.contains("--force"), "{rendered}");
        assert!(rendered.contains("a1b2c3d4e5f6a7b8"), "{rendered}");
        assert!(rendered.contains("/home/u/work/deleted-project"), "{rendered}");
        assert!(rendered.contains("/Volumes/Ext/project"), "{rendered}");
        assert!(rendered.contains("unmounted disk"), "{rendered}");
        assert!(rendered.contains("7 project(s) indexed before"), "{rendered}");
    }

    #[test]
    fn a_single_deletion_names_what_it_deleted() {
        let rendered = render(&Outcome::Deleted {
            id: "a1b2c3d4".to_string(),
            path: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4"),
        });

        assert!(rendered.contains("deleted project a1b2c3d4"), "{rendered}");
        assert!(rendered.contains("/home/u/.g-mesh/projects/a1b2c3d4"), "{rendered}");
    }
}
