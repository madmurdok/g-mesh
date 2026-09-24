//! That a plugin does not outlive a daemon that was killed while the plugin
//! was busy (GM-397; design: `docs/architecture/plugin-lifetime.md` §3).
//!
//! # The state that leaks, and how these tests reach it
//!
//! Today a plugin notices its daemon is gone only when it next *writes* (a
//! bulk walk) or next *reads* (a long-lived plugin). A walk that has not
//! written yet, or a `semanticPass` still computing, notices nothing - and
//! nothing ends it. Reaching that state with a real fixture takes a project
//! big enough to keep a plugin busy for seconds, which is slow and
//! machine-dependent. So every plugin carries a test-only knob,
//! `G_MESH_PLUGIN_HOLD_DIR` (`plugins/sdk/src/hold.rs` documents the
//! contract all four share): a plugin at hold point `P` finding
//! `<dir>/P-<language>.hold` records its pid in `<dir>/P-<language>.pid` and
//! then neither reads nor writes while the hold file exists. The pid file is
//! also what proves the hold was actually reached, and it is the only way to
//! learn a bulk child's pid at all - bulk children have no pid file of their
//! own.
//!
//! # What is asserted
//!
//! The daemon is bootstrapped through the shim (so it is detached and a
//! killed daemon cannot linger as this test's zombie - see
//! `daemon_sigterm.rs`), held at one point, then killed with
//! `process::force_stop` - `SIGKILL`, the case in which no daemon code runs
//! at all. The held plugin, reparented to init and so unable to fake "alive"
//! as a zombie, must be gone within [`DEATH_BUDGET`].
//!
//! The positive test guards the other direction: a lifeline must never stop
//! an ordinary walk from finishing.
//!
//! # Cleanup
//!
//! [`Fixture`]'s `Drop` removes every hold file (releasing any held plugin)
//! and then kills every pid any pid file names - the hold dir's, the
//! daemon's and each language's - so a red test does not leak a process.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::process;
use g_mesh::storage::connection::project_dir;
use rusqlite::Connection;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// The plugins' knob - see the module doc. Must match
/// `plugins/sdk/src/hold.rs`, `plugins/go/hold.go` and
/// `plugins/typescript/src/testHold.ts`.
const HOLD_DIR_ENV: &str = "G_MESH_PLUGIN_HOLD_DIR";

/// How long a plugin may survive its killed daemon. The acceptance criterion
/// of GM-397: "within a few seconds".
const DEATH_BUDGET: Duration = Duration::from_secs(5);

/// One source file per bundled language, and the path its `File` node is
/// indexed under.
const SOURCES: &[(&str, &str)] = &[
    ("main.go", "package main\n\nfunc main() {}\n"),
    ("src/lib.rs", "pub fn lib_fn() {}\n"),
    ("a.py", "def py_fn():\n    pass\n"),
    ("a.ts", "export const a = 1;\n"),
];

struct Fixture {
    project: tempfile::TempDir,
    hold_dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = tempfile::tempdir().expect("failed to create a temp project root");
        for (rel, text) in SOURCES {
            let path = project.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        // The Rust plugin refuses to walk without a project model.
        std::fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"gm397\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let hold_dir = tempfile::tempdir().expect("failed to create the hold dir");
        Self { project, hold_dir }
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory")
    }

    fn daemon_pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self, language: &str) -> PathBuf {
        self.state_dir().join(format!("plugin-{language}.pid"))
    }

    fn hold(&self, point: &str, language: &str) {
        std::fs::write(self.hold_dir.path().join(format!("{point}-{language}.hold")), b"")
            .expect("failed to create the hold file");
    }

    fn held_pid_file(&self, point: &str, language: &str) -> PathBuf {
        self.hold_dir.path().join(format!("{point}-{language}.pid"))
    }

    /// Bootstraps a detached daemon through the shim, with the knob set on
    /// the shim (the daemon and its plugins inherit it), asks it to index,
    /// and returns the daemon's pid.
    fn bootstrap(&self) -> u32 {
        let mut shim = Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env(HOLD_DIR_ENV, self.hold_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");
        common::wait_for("the daemon to bind its socket", common::startup_timeout(), || {
            self.daemon_pid_file().exists()
        });
        let _ = shim.kill();
        let _ = shim.wait();
        let core = daemon::read_pid_file(&self.daemon_pid_file()).expect("the daemon pid file holds no pid");
        common::trigger_activation(self.root());
        core
    }

    /// Waits for the plugin to reach its hold and returns its pid.
    fn wait_for_hold(&self, point: &str, language: &str) -> u32 {
        let pid_file = self.held_pid_file(point, language);
        common::wait_for(
            &format!("the {language} plugin to reach hold point {point}"),
            common::startup_timeout(),
            || daemon::read_pid_file(&pid_file).is_some(),
        );
        daemon::read_pid_file(&pid_file).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut pid_files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.hold_dir.path()) {
            for entry in entries.flatten() {
                let path = entry.path();
                match path.extension().and_then(|ext| ext.to_str()) {
                    // Released first, so a held plugin is not parked while
                    // it is being killed.
                    Some("hold") => {
                        let _ = std::fs::remove_file(&path);
                    }
                    Some("pid") => pid_files.push(path),
                    _ => {}
                }
            }
        }
        pid_files.push(self.daemon_pid_file());
        for language in ["go", "rust", "python", "typescript"] {
            pid_files.push(self.plugin_pid_file(language));
        }
        for path in pid_files {
            common::kill_pid_file(&path);
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

/// Holds `language`'s plugin at `point`, kills the daemon, and asserts the
/// plugin goes within [`DEATH_BUDGET`].
fn plugin_dies_with_a_killed_daemon(point: &str, language: &str) {
    let fixture = Fixture::new();
    fixture.hold(point, language);
    let core = fixture.bootstrap();
    let plugin = fixture.wait_for_hold(point, language);
    assert_ne!(core, plugin, "the plugin runs in a process of its own");

    if point == "semantic" {
        // The held process is the daemon's long-lived plugin, not some other
        // process that happened to reach the same code.
        let recorded = fixture.plugin_pid_file(language);
        common::wait_for(&format!("plugin-{language}.pid"), common::startup_timeout(), || {
            daemon::read_pid_file(&recorded).is_some()
        });
        assert_eq!(
            daemon::read_pid_file(&recorded),
            Some(plugin),
            "the plugin held at {point} is the daemon's long-lived {language} plugin"
        );
    }
    assert!(daemon::is_process_alive(core), "the daemon must be up before it is killed");
    assert!(daemon::is_process_alive(plugin), "and so must its held plugin");

    let killed_at = Instant::now();
    process::force_stop(core).expect("failed to kill the daemon");
    common::wait_for("the killed daemon to go", common::startup_timeout(), || {
        !daemon::is_process_alive(core)
    });

    while daemon::is_process_alive(plugin) {
        assert!(
            killed_at.elapsed() < DEATH_BUDGET,
            "the {language} plugin (pid {plugin}, held at {point}) was still alive after {:?} - it \
             outlived its killed daemon",
            DEATH_BUDGET
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

macro_rules! dies_with_daemon {
    ($($name:ident: $point:literal, $language:literal;)*) => {$(
        #[test]
        fn $name() {
            plugin_dies_with_a_killed_daemon($point, $language);
        }
    )*};
}

dies_with_daemon! {
    bulk_go_plugin_dies_with_a_killed_daemon: "bulk", "go";
    bulk_rust_plugin_dies_with_a_killed_daemon: "bulk", "rust";
    bulk_python_plugin_dies_with_a_killed_daemon: "bulk", "python";
    bulk_typescript_plugin_dies_with_a_killed_daemon: "bulk", "typescript";
    long_lived_go_plugin_dies_with_a_killed_daemon: "semantic", "go";
    long_lived_rust_plugin_dies_with_a_killed_daemon: "semantic", "rust";
    long_lived_python_plugin_dies_with_a_killed_daemon: "semantic", "python";
    long_lived_typescript_plugin_dies_with_a_killed_daemon: "semantic", "typescript";
}

/// The other direction: with nothing held, every language's walk still
/// completes and commits its file. A lifeline that ended a healthy walk, or
/// kept a finished plugin from exiting, fails here.
#[test]
fn bulk_walk_still_completes_with_its_lifeline_open() {
    let fixture = Fixture::new();
    fixture.bootstrap();
    common::wait_until_indexed(fixture.root());

    let conn = Connection::open(fixture.state_dir().join("index.db")).expect("failed to open the index");
    let mut stmt = conn.prepare("SELECT filePath FROM nodes WHERE kind = 'File'").unwrap();
    let indexed: BTreeSet<String> =
        stmt.query_map([], |row| row.get::<_, String>(0)).unwrap().map(|row| row.unwrap()).collect();
    let expected: BTreeSet<String> = SOURCES.iter().map(|(rel, _)| rel.to_string()).collect();
    assert_eq!(indexed, expected, "one indexed file node per language");
}
