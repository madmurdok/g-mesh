//! `g-mesh init` against a fresh project - the acceptance test for task 54.
//!
//! Driven as a subprocess with the project as its cwd, the same way
//! `cli_reindex.rs` and `cli_status.rs` are: what is asserted is the text a
//! human reads, and the state a subsequent `g-mesh status` reports, not an
//! internal struct.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const FILES: [(&str, &str); 2] = [
    (
        "src/index.ts",
        r#"import { connect } from "./db/connection.js";

export function start(): number {
  return connect();
}
"#,
    ),
    ("src/db/connection.ts", "export function connect(): number {\n  return 1;\n}\n"),
];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (rel, contents) in FILES {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory")
    }

    fn config_path(&self) -> PathBuf {
        self.state_dir().join("config.toml")
    }

    fn index_path(&self) -> PathBuf {
        self.state_dir().join("index.db")
    }

    fn init(&self) -> Output {
        Command::new(BIN).arg("init").current_dir(self.root()).output().expect("failed to run `g-mesh init`")
    }

    fn init_with_agents(&self, agents: &str) -> Output {
        Command::new(BIN)
            .arg("init")
            .arg("--agent")
            .arg(agents)
            .current_dir(self.root())
            .output()
            .expect("failed to run `g-mesh init --agent`")
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root().join(rel)).unwrap_or_else(|e| panic!("failed to read {rel}: {e}"))
    }

    fn status(&self) -> String {
        let output = Command::new(BIN)
            .arg("status")
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
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [daemon::pid_path(self.root()).unwrap(), daemon::plugin_pid_path(self.root()).unwrap()] {
            common::kill_pid_file(&path);
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "expected `{needle}` in output:\n{haystack}");
}

/// The acceptance criterion for task 54: running `init` on a fresh project
/// creates the expected directory/config, and a subsequent `status` call
/// shows indexing has completed.
#[test]
fn init_on_a_fresh_project_creates_state_and_a_subsequent_status_shows_it_indexed() {
    let project = Project::new();
    assert!(!project.state_dir().exists(), "the fixture must start with no g-mesh state at all");

    let output = project.init();
    assert!(
        output.status.success(),
        "`g-mesh init` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("init output is not valid UTF-8");
    assert!(!stdout.contains("stopped so init could rebuild"), "{stdout}");
    assert_contains(&stdout, "wrote config.toml with defaults");
    // 2 File nodes + 2 Function nodes (`start`, `connect`) + resolution
    // bookkeeping; 6 edges = 2 DEFINES + 2 EXPORTS (both functions are
    // exported) + 1 IMPORTS + 1 CALLS (`start` calling `connect`) - the real
    // count from this fixture, not a guess (verified live, not asserted
    // blind).
    assert_contains(&stdout, "6 nodes, 6 edges (1 imports linked, 1 symbols linked)");

    // The expected directory/config: state dir, config.toml, and index.db all
    // exist once init has run.
    assert!(project.state_dir().is_dir(), "init must create the project's state directory");
    assert!(project.config_path().is_file(), "init must write config.toml");
    assert!(project.index_path().is_file(), "init must create the index database");

    // A subsequent status call shows indexing has completed - no daemon was
    // ever started, so this is read entirely off what init itself wrote.
    let status = project.status();
    assert_contains(&status, "daemon core:     not running");
    assert!(!status.contains("never fully walked"), "{status}");
    assert_contains(&status, "index coverage:  100.0% (2/2 source files)");
    assert_contains(&status, "dirty files:     0 awaiting reindex");
}

/// Running `init` again on a project it already fully set up must not
/// clobber a hand-edited config or repeat the walk.
#[test]
fn init_run_twice_leaves_an_existing_config_alone_and_skips_the_repeat_walk() {
    let project = Project::new();

    let first = project.init();
    assert!(first.status.success(), "first `g-mesh init` failed: {}", String::from_utf8_lossy(&first.stderr));

    // Hand-edit the config the way a person tuning their settings would.
    let customized = "[plugin]\nidleTimeoutMinutes = 5\n";
    std::fs::write(project.config_path(), customized).expect("failed to hand-edit config.toml");

    let second = project.init();
    assert!(
        second.status.success(),
        "second `g-mesh init` failed with {}: {}",
        second.status,
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8(second.stdout).expect("init output is not valid UTF-8");
    assert_contains(&stdout, "existing config.toml left untouched");
    assert_contains(&stdout, "already fully indexed - nothing to do");

    let contents = std::fs::read_to_string(project.config_path()).expect("failed to read config.toml");
    assert_eq!(contents, customized, "a second init must not overwrite a hand-edited config");
}

/// `init --agent claude` against a fresh project writes both AGENTS.md (the
/// shared cross-tool snippet) and a CLAUDE.md that bridges to it via
/// Claude Code's `@path` import syntax. A second run must not touch either
/// file again.
#[test]
fn init_with_agent_claude_writes_agents_md_and_a_claude_md_bridge_and_is_idempotent() {
    let project = Project::new();

    let first = project.init_with_agents("claude");
    assert!(
        first.status.success(),
        "`g-mesh init --agent claude` failed with {}: {}",
        first.status,
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8(first.stdout).expect("init output is not valid UTF-8");
    assert_contains(&stdout, "AGENTS.md:  wrote the g-mesh code-search snippet");
    assert_contains(&stdout, "CLAUDE.md:  wrote the @AGENTS.md bridge line");

    let agents_md = project.read("AGENTS.md");
    assert!(agents_md.contains("Code search (TypeScript/JavaScript projects)"), "{agents_md}");
    let claude_md = project.read("CLAUDE.md");
    assert!(claude_md.starts_with("@AGENTS.md"), "{claude_md}");

    // Running it again must be a no-op on both files.
    let second = project.init_with_agents("claude");
    assert!(
        second.status.success(),
        "second `g-mesh init --agent claude` failed with {}: {}",
        second.status,
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8(second.stdout).expect("init output is not valid UTF-8");
    assert_contains(&stdout, "AGENTS.md:  already had the g-mesh snippet - left untouched");
    assert_contains(&stdout, "CLAUDE.md:  already bridges to AGENTS.md - left untouched");

    assert_eq!(project.read("AGENTS.md"), agents_md, "a second init must not change AGENTS.md");
    assert_eq!(project.read("CLAUDE.md"), claude_md, "a second init must not change CLAUDE.md");
}

/// `init` without `--agent` must not write any of these files at all - the
/// flag's absence means exactly what it always meant.
#[test]
fn init_without_agent_writes_no_agent_instruction_files() {
    let project = Project::new();

    let output = project.init();
    assert!(output.status.success(), "`g-mesh init` failed: {}", String::from_utf8_lossy(&output.stderr));

    assert!(!project.root().join("AGENTS.md").exists());
    assert!(!project.root().join("CLAUDE.md").exists());
    assert!(!project.root().join("GEMINI.md").exists());
}
