//! ADR 0021 end to end: one discovered plugin whose binary is missing costs
//! only its own language. Every test here runs the real `g-mesh` binary with
//! the real Rust plugin beside a Python plugin whose binary was never built
//! (`common::rust_and_missing_python_plugin_root`), or with one of the two
//! alone.
//!
//! The control for the whole file is the old abort-on-first-failure loop in
//! `daemon::bulk_index::run_with_progress` (`walk_one_language(..)?`): `init`
//! then exits 1 with no Rust symbols written, and the daemon's first tool
//! call is the walk's error rather than an answer. Each test names its own
//! narrower control too.

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};

use g_mesh::daemon;
use g_mesh::daemon::bulk_index::ABSENT_COUNT_OFF_ENV;
use g_mesh::languages::LanguageOutcome;
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::json;
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const NO_MODEL_DIR: &str = "/nonexistent-g-mesh-test-model-dir";

/// A small Rust crate, with `extra` project files beside it.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new(extra: &[(&str, &str)]) -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let crate_files = [
            ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/lib.rs", "pub fn kept_rust_symbol() -> u32 {\n    1\n}\n"),
        ];
        for (rel, contents) in crate_files.iter().chain(extra) {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(path, contents).expect("failed to write a fixture file");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn index(&self) -> Connection {
        let path = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        Connection::open(path).expect("failed to open the index")
    }

    fn init(&self, plugins: &Path, envs: &[(&str, &str)]) -> Output {
        let mut command = StdCommand::new(BIN);
        command
            .arg("init")
            .current_dir(self.root())
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR)
            .env_remove(ABSENT_COUNT_OFF_ENV);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("failed to run `g-mesh init`")
    }

    fn outcomes(&self) -> Vec<(String, LanguageOutcome)> {
        schema::language_outcomes(&self.index()).expect("failed to read the language outcomes")
    }

    fn count(&self, sql: &str) -> i64 {
        self.index().query_row(sql, [], |row| row.get(0)).expect("failed to count")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(path) = daemon::pid_path(self.root()) {
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr_of(output)
    )
}

fn lines_starting(stderr: &str, prefix: &str) -> Vec<String> {
    stderr.lines().filter(|line| line.starts_with(prefix)).map(str::to_string).collect()
}

/// `g-mesh init` with the Python plugin's binary removed: exit 2, Rust
/// indexed and queryable, nothing of Python, one stderr line for the failed
/// Python (with the missing binary) and one for the absent Go (with its file
/// count and install command), and the outcome persisted.
///
/// Controls: the old abort loop (exit 1, no Rust rows); make
/// `report_language_outcomes` return `Ok` for a `Failed` (exit 0); drop the
/// `PluginAbsent` arm from `language_outcome_lines` (no Go line).
#[test]
fn init_with_one_plugin_binary_missing_indexes_the_rest_and_exits_2() {
    let project =
        Project::new(&[("tools/gen.py", "def gen():\n    pass\n"), ("cmd/main.go", "package main\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();

    let output = project.init(plugins.path(), &[]);

    assert_eq!(output.status.code(), Some(2), "a partial failure exits 2: {}", describe(&output));
    let stderr = stderr_of(&output);
    let python = lines_starting(&stderr, "g-mesh: python failed to index");
    assert_eq!(python.len(), 1, "exactly one line for the failed python: {stderr}");
    assert!(
        python[0].contains(&binary.display().to_string()),
        "the line names the missing binary: {}",
        python[0]
    );
    let go = lines_starting(&stderr, "g-mesh: go has no plugin installed");
    assert_eq!(go.len(), 1, "exactly one line for the absent go: {stderr}");
    assert!(go[0].contains("1 file(s)") && go[0].contains("`g-mesh plugins install go`"), "{}", go[0]);
    assert!(
        lines_starting(&stderr, "g-mesh: rust failed").is_empty()
            && lines_starting(&stderr, "g-mesh: rust has no plugin").is_empty(),
        "no outcome line for an indexed language: {stderr}"
    );

    assert_eq!(
        project.count("SELECT COUNT(*) FROM nodes WHERE name = 'kept_rust_symbol'"),
        1,
        "rust is indexed"
    );
    assert_eq!(project.count("SELECT COUNT(*) FROM nodes WHERE language = 'python'"), 0, "nothing of python");
    let outcomes = project.outcomes();
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    assert_eq!(outcomes[0], ("go".to_string(), LanguageOutcome::PluginAbsent { files: Some(1) }));
    assert!(
        matches!(&outcomes[1], (l, LanguageOutcome::Failed { error }) if l == "python" && error.contains("has not been built yet")),
        "{outcomes:?}"
    );
    assert!(
        matches!(&outcomes[2], (l, LanguageOutcome::Indexed { files }) if l == "rust" && *files >= 1),
        "{outcomes:?}"
    );
}

/// Every discovered plugin's binary missing: exit 1, the walk's error names
/// the language, and the outcome rows are still written (the project's
/// `.rs` file, with no Rust plugin discovered here, is an absent language and
/// does not rescue the walk).
///
/// Control: compute `all_failed` as `false` in `run_with_progress` - init
/// exits 2 instead.
#[test]
fn init_with_every_plugin_binary_missing_exits_1_and_records_why() {
    let project = Project::new(&[]);
    let (plugins, _binary) = common::missing_workspace_binary_plugin_root();

    let output = project.init(plugins.path(), &[]);

    assert_eq!(output.status.code(), Some(1), "an all-failed walk exits 1: {}", describe(&output));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("every discovered language failed") && stderr.contains("python"), "{stderr}");
    let outcomes = project.outcomes();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(matches!(&outcomes[0], (l, LanguageOutcome::Failed { .. }) if l == "python"), "{outcomes:?}");
    assert_eq!(outcomes[1], ("rust".to_string(), LanguageOutcome::PluginAbsent { files: Some(1) }));
}

/// Only absent plugins besides an indexed one: exit 0 with one line naming
/// Python's file count - and with the count switched off by
/// `G_MESH_BULK_INDEX_NO_ABSENT_COUNT`, no Python line and no Python row.
///
/// Controls: make `report_language_outcomes` fail on `PluginAbsent` (exit
/// 2); ignore the env var in `absent_count_switched_off` (the second run
/// still prints Python's line).
#[test]
fn init_with_only_absent_plugins_exits_0_and_the_count_can_be_switched_off() {
    let project =
        Project::new(&[("app.py", "x = 1\n"), ("pkg/util.py", "y = 2\n"), (".venv/lib/dep.py", "z = 3\n")]);
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    common::add_real_rust_plugin(plugins.path());

    let counted = project.init(plugins.path(), &[]);
    assert_eq!(counted.status.code(), Some(0), "absent plugins are a valid install: {}", describe(&counted));
    let python = lines_starting(&stderr_of(&counted), "g-mesh: python has no plugin installed");
    assert_eq!(python.len(), 1, "{}", describe(&counted));
    assert!(python[0].contains("2 file(s)"), ".venv is python's own exclude: {}", python[0]);

    // A second `init` skips the walk once the index is complete; `reindex`
    // re-walks, so the switch is exercised through it.
    let reindexed = {
        let mut command = StdCommand::new(BIN);
        command
            .arg("reindex")
            .current_dir(project.root())
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins.path())
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR)
            .env(ABSENT_COUNT_OFF_ENV, "1");
        command.output().expect("failed to run `g-mesh reindex`")
    };
    assert_eq!(reindexed.status.code(), Some(0), "{}", describe(&reindexed));
    assert!(
        lines_starting(&stderr_of(&reindexed), "g-mesh: python").is_empty(),
        "the count is off, so python gets no outcome: {}",
        describe(&reindexed)
    );
    let languages: Vec<String> = project.outcomes().into_iter().map(|(language, _)| language).collect();
    assert_eq!(languages, ["rust"], "the re-walk replaced the rows and recorded no python");
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn shim(root: &Path, plugins: &Path) -> TokioChildProcess {
    let root: PathBuf = root.to_path_buf();
    let plugins = plugins.to_path_buf();
    TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by the daemon the shim bootstraps.
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", &plugins)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR);
    }))
    .expect("failed to spawn the shim")
}

/// The daemon with the Python plugin's binary removed: the first tool call
/// answers for Rust (not the walk's error), the daemon keeps serving, and a
/// later session's handshake - which reads the finished index to write its
/// instructions - succeeds with instructions.
///
/// Control: the old abort loop - the first call is a tool error naming the
/// missing Python binary (GM-316's message), and this fails on `is_error`.
#[tokio::test]
async fn the_daemon_answers_for_the_other_language_when_one_plugin_binary_is_missing() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, _binary) = common::rust_and_missing_python_plugin_root();

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    let result = client
        .call_tool(
            CallToolRequestParams::new("find_definition").with_arguments(
                json!({ "symbol_name": "kept_rust_symbol", "include_source": false })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("the call must come back as a tool result");
    let answer = text(&result);
    assert_ne!(result.is_error, Some(true), "one missing plugin must not fail the call: {answer}");
    assert!(answer.contains("src/lib.rs"), "the rust symbol must be found: {answer}");
    client.cancel().await.expect("failed to shut the first client down");

    assert!(daemon::is_listening(project.root()).unwrap(), "the daemon keeps serving");
    let second = ().serve(shim(project.root(), plugins.path())).await.expect("a second session must connect");
    let instructions = second.peer_info().and_then(|info| info.instructions.clone()).unwrap_or_default();
    assert!(!instructions.is_empty(), "the instructions read path must answer after a partial walk");
    second.cancel().await.expect("failed to shut the second client down");
}
