//! `g-mesh clean`: delete cached project indexes under `~/.g-mesh/projects/`.
//!
//! Four forms, differing only in what they are scoped to:
//!
//! | invocation | scope |
//! |---|---|
//! | `g-mesh clean` | the current directory's project |
//! | `g-mesh clean <project-id>` | that one project |
//! | `g-mesh clean expired` | every project idle longer than the threshold |
//! | `g-mesh clean all` | every project, with `--force` |
//!
//! # Why only `all` asks for confirmation
//!
//! The first three are already scoped by something the caller said or that
//! the data itself established, so a confirmation prompt on top would be the
//! kind that gets typed through without reading. `all` is the only one whose
//! blast radius is "everything, including the project you are standing in",
//! so without `--force` it reports the count it *would* have deleted and
//! removes nothing.
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
use crate::daemon::{self, identity::project_hash};
use crate::gc::last_used;
use crate::storage::connection::projects_root;

/// How long a project must go unused before `clean expired` will delete it.
///
/// The architecture doc puts this in `~/.g-mesh/config.toml` as
/// `cleanup.idleThresholdDays`, but no config file exists yet (its own
/// backlog ticket) - so the documented default is hard-coded here rather than
/// a config system being invented around a single number.
pub const IDLE_THRESHOLD_DAYS: u64 = 90;

const IDLE_THRESHOLD: Duration = Duration::from_secs(IDLE_THRESHOLD_DAYS * 24 * 60 * 60);

/// What a `clean` invocation is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// No argument: the project the current directory belongs to.
    Cwd,
    /// One project, named by the `<hash>` directory it lives in.
    Project(String),
    /// Every project idle for longer than [`IDLE_THRESHOLD_DAYS`].
    Expired,
    /// Every project.
    All,
}

impl Target {
    /// Reads the single positional argument `clean` takes.
    ///
    /// `expired` and `all` can never be mistaken for a project id: ids are
    /// the 16 hex characters `daemon::identity::project_hash` produces, which
    /// neither word is.
    pub fn parse(argument: Option<&str>) -> Self {
        match argument {
            None => Target::Cwd,
            Some("expired") => Target::Expired,
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
    /// because a daemon was serving them.
    DeletedMany { scope: Scope, ids: Vec<String>, skipped_running: Vec<String> },
    /// `clean all` without `--force`: a count, and nothing touched.
    WouldDelete { count: usize },
}

/// Runs the `clean` the parsed arguments describe.
pub fn run(args: &CleanArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let root = projects_root().context("failed to resolve ~/.g-mesh/projects")?;
    let outcome = clean(&Target::parse(args.target.as_deref()), args.force, &root, &cwd)?;
    print!("{}", render(&outcome));
    Ok(())
}

/// Deletes whatever `target` names under `projects_root`.
///
/// `projects_root` and `cwd` are parameters rather than read from the
/// environment so that the whole command - including the variants that delete
/// every project there is - can be tested against a directory that is not the
/// developer's own.
pub fn clean(
    target: &Target,
    force: bool,
    projects_root: &Path,
    cwd: &Path,
) -> Result<Outcome> {
    match target {
        Target::Cwd => clean_one(&cwd_project_id(cwd)?, projects_root, CwdScoped::Yes),
        Target::Project(id) => clean_one(id, projects_root, CwdScoped::No),
        Target::Expired => clean_many(Scope::Expired, projects_root),
        Target::All => {
            if !force {
                return Ok(Outcome::WouldDelete { count: candidates(projects_root)?.len() });
            }
            clean_many(Scope::All, projects_root)
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

fn clean_many(scope: Scope, projects_root: &Path) -> Result<Outcome> {
    let mut ids = Vec::new();
    let mut skipped_running = Vec::new();

    for candidate in candidates(projects_root)? {
        if matches!(scope, Scope::Expired) && !candidate.is_expired() {
            continue;
        }
        if candidate.daemon_running {
            skipped_running.push(candidate.id);
            continue;
        }
        delete(&candidate.path)?;
        ids.push(candidate.id);
    }

    Ok(Outcome::DeletedMany { scope, ids, skipped_running })
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
}

impl Candidate {
    fn is_expired(&self) -> bool {
        self.idle.is_some_and(|idle| idle > IDLE_THRESHOLD)
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
            id,
            path,
        });
    }

    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(candidates)
}

/// Whether anything is still running for this state directory - the core, or
/// a plugin that outlived it.
fn daemon_is_running(state_dir: &Path) -> bool {
    [daemon::pid_path_in(state_dir), daemon::plugin_pid_path_in(state_dir)]
        .iter()
        .filter_map(|path| daemon::read_pid_file(path))
        .any(daemon::is_process_alive)
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
        Outcome::DeletedMany { scope, ids, skipped_running } => {
            let mut out = String::new();
            match (scope, ids.len()) {
                (Scope::Expired, 0) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: no project has been idle for more than {IDLE_THRESHOLD_DAYS} days"
                    );
                }
                (Scope::Expired, count) => {
                    let _ = writeln!(
                        out,
                        "g-mesh: deleted {count} project index(es) idle for more than {IDLE_THRESHOLD_DAYS} days"
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
            crate::storage::schema::ensure_current(&conn, &crate::daemon::plugin::indexer_version()).unwrap();
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

    fn clean_in(projects: &Projects, target: Target, force: bool) -> Result<Outcome> {
        let cwd = unrelated_cwd();
        clean(&target, force, projects.root(), cwd.path())
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
    fn the_positional_argument_selects_the_four_documented_forms() {
        assert_eq!(Target::parse(None), Target::Cwd);
        assert_eq!(Target::parse(Some("expired")), Target::Expired);
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

        let err = clean(&Target::Cwd, false, projects.root(), cwd.path())
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

        let outcome = clean(&Target::Cwd, false, projects.root(), cwd.path()).unwrap();

        assert!(matches!(outcome, Outcome::Deleted { id: ref got, .. } if *got == id));
        assert!(!projects.exists(&id));
    }

    #[test]
    fn expired_deletes_only_what_is_past_the_threshold() {
        let projects = Projects::new();
        projects.add("aaaa1111", IDLE_THRESHOLD_DAYS + 10);
        projects.add("bbbb2222", IDLE_THRESHOLD_DAYS - 10);
        projects.add("cccc3333", IDLE_THRESHOLD_DAYS + 200);

        let outcome = clean_in(&projects, Target::Expired, false).unwrap();

        match outcome {
            Outcome::DeletedMany { scope, ids, skipped_running } => {
                assert_eq!(scope, Scope::Expired);
                assert_eq!(ids, vec!["aaaa1111", "cccc3333"]);
                assert!(skipped_running.is_empty());
            }
            other => panic!("expected a bulk delete, got {other:?}"),
        }
        assert!(projects.exists("bbbb2222"), "a project inside the threshold must survive");
    }

    /// `expired` needs no `--force`: it is already scoped by the data.
    #[test]
    fn expired_needs_no_force() {
        let projects = Projects::new();
        projects.add("aaaa1111", IDLE_THRESHOLD_DAYS + 1);

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
            clean(&Target::All, false, &never_created, cwd.path()).unwrap(),
            Outcome::WouldDelete { count: 0 }
        );
        let outcome = clean(&Target::Expired, false, &never_created, cwd.path()).unwrap();
        assert!(matches!(outcome, Outcome::DeletedMany { ref ids, .. } if ids.is_empty()));
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
