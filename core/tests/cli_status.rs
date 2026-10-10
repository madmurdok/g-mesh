//! `g-mesh status` against a real daemon, in a project whose state the test
//! arranged on purpose: one file with a deliberate syntax error, and later a
//! file the index has never seen.
//!
//! The command is driven as a subprocess with the project as its cwd - the
//! same way a person runs it - so what is asserted is the text a human reads,
//! not an internal struct.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use g_mesh::config::{self, CleanupConfig, GlobalConfig};
use g_mesh::daemon;
use g_mesh::daemon::{manifest, registry};
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rusqlite::Connection;

mod common;

use common::wait_until_indexed;
use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Serializes the two tests below that rewrite the real
/// `~/.g-mesh/config.toml`: with no override hook to point `config::mod` at
/// a scratch directory, two threads flipping `cleanup.enabled` at the same
/// time would race on that one shared file.
static GLOBAL_CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Temporarily replaces `~/.g-mesh/config.toml` with a known value, and
/// restores whatever (if anything) was there before it on drop - so a test
/// exercising `cleanup.enabled` never leaves the developer's real global
/// config mutated.
struct GlobalConfigGuard {
    path: PathBuf,
    original: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl GlobalConfigGuard {
    fn set(cfg: &GlobalConfig) -> Self {
        let lock = GLOBAL_CONFIG_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = config::global_config_path().expect("failed to resolve the global config path");
        let original = std::fs::read_to_string(&path).ok();
        config::write_global_config(cfg).expect("failed to write the global config");
        Self { path, original, _lock: lock }
    }
}

impl Drop for GlobalConfigGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(contents) => {
                let _ = std::fs::write(&self.path, contents);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// A project with a known, deliberately mixed index state.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("good.ts"), b"export function good() { return 1; }\n")
            .expect("failed to write good.ts");
        // Deliberately unparseable: the plugin still emits a File node for it,
        // flagged, which is exactly the state `status` has to surface.
        std::fs::write(dir.path().join("broken.ts"), b"export function broken( {\n")
            .expect("failed to write broken.ts");
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    fn recorded_pid(&self, path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }

    fn status(&self) -> String {
        self.status_with(&[])
    }

    /// `g-mesh status --full`: the view that walks the project for index
    /// coverage and dirty files.
    fn status_full(&self) -> String {
        self.status_with(&["--full"])
    }

    fn status_with(&self, flags: &[&str]) -> String {
        let output = Command::new(BIN)
            .arg("status")
            .args(flags)
            .current_dir(self.root())
            .output()
            .expect("failed to run `g-mesh status`");
        assert!(
            output.status.success(),
            "`g-mesh status` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("status output is not valid UTF-8")
    }

    /// Builds this project's index with no daemon involved, and backdates
    /// `lastUsed` by `idle_days` - the on-disk state a project idle past the
    /// GC warning's threshold would have.
    fn backdate_last_used(&self, idle_days: u64) {
        std::fs::create_dir_all(self.state_dir()).expect("failed to create the state directory");
        let conn = Connection::open(self.state_dir().join("index.db")).expect("failed to open index.db");
        // The same generation a daemon would stamp this index with (see
        // `daemon::registry::indexer_version`), computed the same way from the
        // plugins actually installed - so a daemon started against this
        // project afterwards finds it current and leaves the backdated
        // `lastUsed` this fixture is about alone, instead of wiping it.
        let discovered =
            manifest::discover(&manifest::default_roots()).expect("failed to discover language plugins");
        schema::ensure_current(&conn, &registry::indexer_version(&discovered))
            .expect("failed to initialize the schema");
        conn.execute(
            "UPDATE meta SET lastUsed = datetime('now', ?1) WHERE id = 1",
            rusqlite::params![format!("-{idle_days} days")],
        )
        .expect("failed to backdate lastUsed");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            common::kill_pid_file(&path);
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .lifeline()
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
}

/// GM-301: see `common::wait_for`'s doc comment for why this delegates
/// instead of polling against a file-local timeout constant.
fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "expected `{needle}` in status output:\n{haystack}");
}

#[test]
fn status_reports_the_daemon_plugin_coverage_and_syntax_errors_of_a_live_project() {
    let project = Project::new();
    let mut daemon_process = spawn_daemon(project.root());
    // The daemon's pid file now appears at the socket bind, which is ahead of
    // both the plugin spawn and the cold-start walk (task 105) - and this test
    // asserts on the plugin's pid and on complete coverage, so it has to wait
    // for the walk's own completion marker instead.
    wait_until_indexed(project.root());
    // The registry spawns the bundled (typescript) plugin lazily - here, as a
    // side effect of the post-walk semantic pass `daemon::run` runs once the
    // cold-start walk commits - rather than unconditionally at daemon
    // startup, so its pid file can appear a moment *after* the walk's own
    // completion marker does, not atomically with it.
    wait_for("the typescript plugin to start", || project.plugin_pid_file().exists());

    let core_pid = project.recorded_pid(&project.pid_file());
    let plugin_pid = project.recorded_pid(&project.plugin_pid_file());
    assert_eq!(core_pid, daemon_process.id(), "the pid file must name the daemon we started");
    assert_ne!(plugin_pid, core_pid, "the plugin runs in a process of its own");

    let status = project.status_full();

    assert_contains(&status, &format!("daemon core:     running (pid {core_pid})"));
    assert_contains(&status, &format!("plugin (typescript):     active (pid {plugin_pid})"));
    // Both files were walked, and neither has been edited since.
    assert_contains(&status, "index coverage:  100.0% (2/2 source files)");
    assert_contains(&status, "dirty files:     0 awaiting reindex");
    assert_contains(&status, "syntax errors:   1 file(s)");
    assert_contains(&status, "broken.ts");
    assert!(!status.contains("never fully walked"), "the project has been fully walked:\n{status}");
    // The stamp the daemon wrote at startup, read back off disk by the
    // command rather than asked of the daemon.
    assert_contains(&status, "just now");

    assert!(daemon_process.try_wait().unwrap().is_none(), "the daemon must still be running");
}

#[test]
fn status_reports_a_dead_daemon_and_the_files_its_index_never_saw() {
    let project = Project::new();
    let mut daemon_process = spawn_daemon(project.root());
    // The index this asserts on has to be the finished one - see the wait in
    // the test above.
    wait_until_indexed(project.root());
    // See the test above: the bundled plugin's pid file can appear a moment
    // after the walk's completion marker does.
    wait_for("the typescript plugin to start", || project.plugin_pid_file().exists());
    let plugin_pid = project.recorded_pid(&project.plugin_pid_file());

    // Killed, not stopped, so the pid files are deliberately left behind:
    // status has to see through them rather than trust them.
    daemon_process.kill().expect("failed to kill the daemon");
    daemon_process.wait().expect("failed to reap the daemon");
    // The plugin exits when the daemon's end of its stdin closes, which is
    // what makes "no plugin pid file reads as live" the correct report a
    // moment later.
    wait_for("the plugin to exit with its core", || !daemon::is_process_alive(plugin_pid));

    // A file added while nothing was watching: the index has never seen it.
    std::fs::write(project.root().join("late.ts"), b"export const late = 1;\n")
        .expect("failed to write late.ts");

    let status = project.status_full();

    assert_contains(&status, "daemon core:     not running");
    // The stale plugin-typescript.pid left behind by the kill names a dead
    // process, so `plugin_reports` drops it rather than reporting it -
    // "status has to see through them rather than trust them" applies to
    // this line just as much as to the daemon core's own.
    assert_contains(&status, "plugins:         none active");
    assert_contains(&status, "index coverage:  66.7% (2/3 source files)");
    assert_contains(&status, "dirty files:     1 awaiting reindex");
    // Still true of the index, and still reported with no daemon to ask.
    assert_contains(&status, "syntax errors:   1 file(s)");
    assert_contains(&status, "broken.ts");
}

/// Running it somewhere g-mesh has never indexed must report an empty state,
/// not fail - `status` is the command someone reaches for precisely when they
/// are not sure whether anything is set up.
#[test]
fn status_on_a_project_that_was_never_indexed_reports_an_empty_state() {
    let project = Project::new();

    let status = project.status();

    assert_contains(&status, "daemon core:     not running");
    assert_contains(&status, "plugins:         none active");
    assert_contains(&status, "last used:       never recorded");
    assert_contains(&status, "never fully walked");
    assert_contains(
        &status,
        "index coverage:  not checked - `g-mesh status --full` walks the project for coverage and dirty files",
    );
    assert!(!status.contains("dirty files:"), "the default view does not walk the project:\n{status}");
    assert_contains(&status, "languages:       none recorded - no index yet");
    assert_contains(&status, "syntax errors:   none");

    let full = project.status_full();
    assert_contains(&full, "index coverage:  0.0% (0/2 source files)");
    assert_contains(&full, "dirty files:     2 awaiting reindex");
}

/// A daemon killed before it could clean up leaves its pid, phase and
/// progress files behind; none of them may read as work still under way.
#[test]
fn status_does_not_show_a_dead_daemons_leftover_phase_and_progress_as_live() {
    let project = Project::new();
    let state_dir = project.state_dir();
    std::fs::create_dir_all(&state_dir).expect("failed to create the state directory");
    let mut exited = Command::new(BIN).arg("--version").spawn().expect("failed to run a short-lived process");
    let dead_pid = exited.id();
    exited.wait().expect("failed to wait for the short-lived process");
    daemon::write_pid_file(&project.pid_file(), dead_pid);
    std::fs::write(daemon::phase_path_in(&state_dir), "embedding\n").expect("failed to write index.phase");
    let progress = format!(
        r#"{{"pid":{dead_pid},"updatedAtMs":0,"phase":"embedding","walk":{{"languagesDone":1,"languagesTotal":1,"currentLanguage":null,"items":10}},"semantic":{{"languagesDone":1,"languagesTotal":1,"currentLanguage":null}},"embeddings":{{"done":50,"total":200}}}}"#
    );
    std::fs::write(daemon::progress_path_in(&state_dir), progress).expect("failed to write index.progress");
    assert!(daemon::read_progress_in(&state_dir).is_some(), "the fixture must parse as a progress snapshot");

    let status = project.status();

    assert_contains(&status, "daemon core:     not running");
    assert_contains(&status, "index:           never fully walked - a cold start is still owed");
    assert!(!status.contains("50/200"), "{status}");
    assert!(!status.contains("embeddings being computed"), "{status}");
    assert!(!status.contains("overall:"), "{status}");
}

/// Task #62's acceptance criterion: a project whose `lastUsed` is older
/// than `cleanup.idleThresholdDays` makes `g-mesh status` print the GC
/// warning naming it, when `cleanup.enabled` is on.
#[test]
fn status_warns_about_a_project_idle_past_the_threshold() {
    let _config = GlobalConfigGuard::set(&GlobalConfig {
        cleanup: CleanupConfig { enabled: true, idle_threshold_days: 90 },
        ..GlobalConfig::default()
    });
    let project = Project::new();
    project.backdate_last_used(100);
    let project_id = project.state_dir().file_name().unwrap().to_string_lossy().into_owned();

    let status = project.status();

    assert_contains(&status, "idle for more than 90 days");
    assert_contains(&status, &project_id);
    assert_contains(&status, "g-mesh clean expired");

    // Under `--json` the warning goes to stderr and stdout stays one
    // JSON object (in this test, not its own: the config lock is per process).
    // Control: `print!` instead of `eprint!` in `run`'s JSON branch -> stdout
    // no longer parses.
    let output = status_output(project.root(), &["--json"], &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is not one JSON object ({err}): {stdout}"));
    assert_eq!(report["mode"], "light");
    assert_contains(&String::from_utf8_lossy(&output.stderr), "idle for more than 90 days");
}

/// The other half of the same criterion: `cleanup.enabled = false` prints
/// nothing at all, regardless of how idle a project is.
#[test]
fn status_prints_no_warning_when_cleanup_is_disabled() {
    let _config = GlobalConfigGuard::set(&GlobalConfig {
        cleanup: CleanupConfig { enabled: false, idle_threshold_days: 90 },
        ..GlobalConfig::default()
    });
    let project = Project::new();
    project.backdate_last_used(100);

    let status = project.status();

    assert!(!status.contains("idle for more than"), "{status}");
}

/// GM-399 slice 4 (D11): a folder of projects is served by the front, which
/// has no index. `status` says so in one line and prints no coverage: the
/// whole-folder file walk behind a coverage figure is the cost a front
/// exists to avoid.
#[test]
fn status_in_a_front_served_folder_prints_the_front_line_and_no_coverage() {
    let project = Project::new();
    for repo in ["a/.git", "b/.git"] {
        std::fs::create_dir_all(project.root().join(repo)).expect("failed to create a candidate repo");
    }
    let mut daemon_process = spawn_daemon(project.root());
    let phase_file = daemon::phase_path_in(&project.state_dir());
    wait_for("the front to publish its phase", || {
        std::fs::read_to_string(&phase_file).map(|phase| phase.trim() == "front").unwrap_or(false)
    });

    let status = project.status();

    assert_contains(&status, "index:           folder of 2 projects - no index; a session selects one");
    assert!(!status.contains("index coverage"), "a front has no coverage to report:\n{status}");
    assert!(!status.contains("dirty files"), "a front has no dirty files to report:\n{status}");
    assert!(daemon_process.try_wait().unwrap().is_none(), "the front must still be running");
}

/// `g-mesh status` with `flags` and extra environment, its output unchecked.
fn status_output(root: &Path, flags: &[&str], envs: &[(&str, &Path)]) -> std::process::Output {
    let mut command = Command::new(BIN);
    command.arg("status").args(flags).current_dir(root);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("failed to run `g-mesh status`")
}

/// An index built before per-language outcomes (schema 12, no
/// `language_outcome` table) says so, in text and JSON, and status still
/// exits 0. Control: remove `language_section`'s `sqlite_master` probe ->
/// status fails on the missing table.
#[test]
fn status_on_an_index_that_predates_language_outcomes_says_so() {
    let project = Project::new();
    project.backdate_last_used(0);
    let conn = Connection::open(project.state_dir().join("index.db")).expect("failed to open index.db");
    conn.execute_batch("DROP TABLE language_outcome; UPDATE meta SET schema_version = '12' WHERE id = 1;")
        .expect("failed to make the index a schema-12 one");
    drop(conn);

    let status = project.status();
    assert_contains(
        &status,
        "languages:       not recorded - this index predates per-language outcomes (schema 12); the next \
         daemon start rebuilds it",
    );

    let output = status_output(project.root(), &["--json"], &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON object");
    assert_eq!(report["languages"]["state"], "predates_outcomes", "{report}");
    assert_eq!(report["languages"]["schemaVersion"], "12", "{report}");
    assert_eq!(report["languages"]["outcomes"], serde_json::json!([]), "{report}");
}

/// A plugin discovery that fails (a malformed `plugin.toml`) is
/// named by the light status, which exits 0, and fails `--full`, whose walk
/// needs the plugins. Control: `manifest::discover(..)?` in `collect` ->
/// the light status fails too.
#[test]
fn a_failed_plugin_discovery_is_reported_by_light_status_and_fatal_to_full() {
    let project = Project::new();
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let broken = plugins.path().join("broken");
    std::fs::create_dir_all(&broken).expect("failed to create a plugin directory");
    std::fs::write(broken.join("plugin.toml"), "this is not toml [").expect("failed to write plugin.toml");
    let envs = [("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins.path())];

    let light = status_output(project.root(), &[], &envs);
    assert!(light.status.success(), "{}", String::from_utf8_lossy(&light.stderr));
    assert_contains(&String::from_utf8_lossy(&light.stdout), "plugins:         discovery failed - ");

    let light_json = status_output(project.root(), &["--json"], &envs);
    assert!(light_json.status.success(), "{}", String::from_utf8_lossy(&light_json.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&light_json.stdout).expect("stdout is one JSON object");
    assert!(report["languages"]["pluginDiscoveryError"].is_string(), "{report}");

    let full = status_output(project.root(), &["--full"], &envs);
    assert!(!full.status.success(), "the full walk needs the plugins");
    assert_contains(&String::from_utf8_lossy(&full.stderr), "failed to discover language plugins");
}
