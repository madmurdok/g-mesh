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
/// Control: the old abort loop (`docs/adr/0002-bulk-walk.md`, superseded by
/// `docs/adr/0021-per-language-bulk-outcome.md`) - the first call is a tool
/// error naming the missing Python binary (`plugin::missing_plugin_binary_hint`'s
/// message), and this fails on `is_error`.
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

/// Puts the real, built Python plugin where `common::missing_workspace_binary_plugin_root`'s
/// manifest expects its binary, so that language can index from then on.
fn install_python_binary(binary: &Path) {
    let name = format!("g-mesh-plugin-python{}", std::env::consts::EXE_SUFFIX);
    let built = Path::new(BIN).with_file_name(&name);
    let target = PathBuf::from(format!("{}{}", binary.display(), std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(target.parent().unwrap()).expect("failed to create the binary's directory");
    std::fs::copy(&built, &target).unwrap_or_else(|err| {
        panic!("failed to copy {} (run `cargo build --workspace`): {err}", built.display())
    });
}

/// `get_file_outline` on `file`: an index-needing call whose query-time
/// reindex (`PluginRegistry::ensure_fresh`) runs for `file` before it answers.
async fn outline(client: &rmcp::service::RunningService<rmcp::RoleClient, ()>, file: &str) -> String {
    let result = client
        .call_tool(CallToolRequestParams::new("get_file_outline").with_arguments(
            json!({ "file_path": file }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("the call must come back as a tool result");
    text(&result)
}

/// Waits, off the async runtime, until this daemon's activation has finished
/// (`Phase::Ready`), triggering it first.
async fn activated(root: &Path) {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || common::wait_until_phase(&root, "ready"))
        .await
        .expect("the wait must not panic");
}

/// ADR 0021 section 2, through the daemon's own walk: Python fails that walk
/// (binary missing), and once a working binary appears the daemon still does
/// not route Python's files - a query-time reindex of `tools/gen.py`, which
/// has no baseline and so is stale, adds no Python rows. Rust's file, edited
/// after the walk, is reindexed by the same kind of call, so the path itself
/// works here.
///
/// Control: drop the `registry.set_failed_languages(..)` call in
/// `daemon::activation::ActivationCtx::walk` - the outline call spawns the
/// now-present Python plugin and `gen` is written (Python rows > 0).
#[tokio::test]
async fn a_language_the_daemons_walk_failed_is_not_reindexed_once_its_plugin_works() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    activated(project.root()).await;
    assert!(
        matches!(project.outcomes().as_slice(), [(p, LanguageOutcome::Failed { .. }), (r, LanguageOutcome::Indexed { .. })] if p == "python" && r == "rust"),
        "{:?}",
        project.outcomes()
    );

    install_python_binary(&binary);
    std::fs::write(
        project.root().join("src/lib.rs"),
        "pub fn kept_rust_symbol() -> u32 {\n    1\n}\npub fn added_rust_symbol() {}\n",
    )
    .expect("failed to edit the rust file");

    let rust = outline(&client, "src/lib.rs").await;
    assert!(rust.contains("added_rust_symbol"), "a routed language is reindexed at query time: {rust}");
    outline(&client, "tools/gen.py").await;
    assert_eq!(
        project.count("SELECT COUNT(*) FROM nodes WHERE language = 'python'"),
        0,
        "a failed language's file must not be reindexed"
    );
    client.cancel().await.expect("failed to shut the client down");
}

/// An `init`-walked index (Python and Rust both indexed) whose recorded
/// outcome is then made `Failed` for Python, standing in for an earlier walk
/// that failed it.
fn index_recording_python_as_failed(project: &Project, plugins: &Path) {
    let output = project.init(plugins, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        project.count("SELECT COUNT(*) FROM nodes WHERE name = 'gen' AND language = 'python'") > 0,
        "python was indexed"
    );
    let failed = std::collections::BTreeMap::from([
        ("python".to_string(), LanguageOutcome::Failed { error: "recorded by an earlier walk".to_string() }),
        ("rust".to_string(), LanguageOutcome::Indexed { files: 1 }),
    ]);
    schema::record_language_outcomes(&project.index(), &failed).expect("failed to record the outcomes");
}

/// ADR 0021 section 2, across a restart: an index whose `language_outcome`
/// table records Python as failed is served by a daemon that does not walk
/// (the index is complete), yet a Python file edited while no daemon ran is
/// not reindexed by a query, while the Rust file edited alongside it is.
///
/// Control: drop the `registry.seed_failed_languages(&conn)` call in
/// `daemon::run` - nothing else fills the set (activation does not walk this
/// complete index, so `ActivationCtx::walk`'s `set_failed_languages` never
/// runs), the outline call reindexes `tools/gen.py` and `added_python_symbol`
/// is written.
#[tokio::test]
async fn a_failed_language_recorded_in_the_index_is_not_reindexed_after_a_restart() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();
    install_python_binary(&binary);
    index_recording_python_as_failed(&project, plugins.path());

    std::fs::write(
        project.root().join("tools/gen.py"),
        "def gen():\n    pass\n\ndef added_python_symbol():\n    pass\n",
    )
    .expect("failed to edit the python file");
    std::fs::write(
        project.root().join("src/lib.rs"),
        "pub fn kept_rust_symbol() -> u32 {\n    1\n}\npub fn added_rust_symbol() {}\n",
    )
    .expect("failed to edit the rust file");

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    activated(project.root()).await;
    let rust = outline(&client, "src/lib.rs").await;
    assert!(rust.contains("added_rust_symbol"), "a routed language is reindexed at query time: {rust}");
    let python = outline(&client, "tools/gen.py").await;
    assert_eq!(
        project.count("SELECT COUNT(*) FROM nodes WHERE name = 'added_python_symbol'"),
        0,
        "a language recorded as failed must not be reindexed: {python}"
    );
    client.cancel().await.expect("failed to shut the client down");
}

/// `g-mesh daemon` for `project`, started directly rather than through a
/// shim, so that no tool call - and so no activation - happens until the test
/// makes one. Returns once the daemon is listening.
fn start_daemon(project: &Project, plugins: &Path) -> std::process::Child {
    let child = StdCommand::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .current_dir(project.root())
        .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon");
    common::wait_for("the daemon to listen", common::startup_timeout(), || {
        daemon::is_listening(project.root()).unwrap_or(false)
    });
    child
}

/// The rust crate's `src/lib.rs`, declaring `marker` beside the kept symbol.
fn write_rust_marker(project: &Project, marker: &str) {
    std::fs::write(
        project.root().join("src/lib.rs"),
        format!("pub fn kept_rust_symbol() -> u32 {{\n    1\n}}\npub fn {marker}() {{}}\n"),
    )
    .expect("failed to edit the rust file");
}

fn has_node(project: &Project, name: &str) -> bool {
    project.count(&format!("SELECT COUNT(*) FROM nodes WHERE name = '{name}'")) > 0
}

/// Edits the rust file until the watcher writes the edit's marker: proof that
/// the watcher is registered and its consumer is routing. Retried because the
/// watcher is registered after the endpoint is bound, so an edit made right
/// after `start_daemon` may predate it and never be seen.
fn wait_until_the_watcher_routes(project: &Project) {
    let deadline = std::time::Instant::now() + common::startup_timeout();
    for attempt in 0.. {
        let marker = format!("watcher_primed_{attempt}");
        write_rust_marker(project, &marker);
        let retry_at = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < retry_at {
            if has_node(project, &marker) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(std::time::Instant::now() < deadline, "the watcher never routed a rust edit");
    }
}

/// ADR 0021 section 2, across a restart and before any tool call: a daemon
/// over a complete index whose `language_outcome` records Python as failed
/// does not route a Python edit its watcher sees, though no tool call has
/// activated it yet (the watcher's consumer of a walked index starts at
/// startup). The Python plugin works, so a routed edit would be indexed.
///
/// The wait is honest: after the Python edit, two Rust edits in turn are
/// waited for until the watcher writes them. Events arrive in order and the
/// consumer routes on one thread, so the Python change was settled, and
/// routed or skipped, before the second Rust edit was even recorded.
/// The phase is still `structural` at the end: no activation ran.
///
/// Control: drop the `registry.seed_failed_languages(&conn)` call in
/// `daemon::run` (or move it back into activation) - the watcher routes
/// `tools/gen.py` and `added_python_symbol` is written.
#[test]
fn a_failed_language_is_not_routed_by_the_watcher_before_the_first_tool_call() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();
    install_python_binary(&binary);
    index_recording_python_as_failed(&project, plugins.path());

    let mut daemon_process = start_daemon(&project, plugins.path());
    wait_until_the_watcher_routes(&project);

    std::fs::write(
        project.root().join("tools/gen.py"),
        "def gen():\n    pass\n\ndef added_python_symbol():\n    pass\n",
    )
    .expect("failed to edit the python file");
    for marker in ["after_python_first", "after_python_second"] {
        write_rust_marker(&project, marker);
        common::wait_for(&format!("the watcher to index {marker}"), common::startup_timeout(), || {
            has_node(&project, marker)
        });
    }

    assert!(!has_node(&project, "added_python_symbol"), "a language recorded as failed must not be routed");
    let phase = std::fs::read_to_string(daemon::phase_path_in(
        &project_dir(project.root()).expect("failed to resolve the state directory"),
    ))
    .expect("failed to read the phase file");
    assert_eq!(phase.trim(), "structural", "no tool call may have activated the daemon");
    let _ = daemon_process.kill();
    let _ = daemon_process.wait();
}

/// ADR 0021 section 2, on the first tool call after a restart: an outline of
/// a Python file edited while no daemon ran, sent before activation has run
/// (or finished), does not reindex it through `ensure_fresh`. The Rust file
/// edited alongside it is reindexed by the next call, so the path works.
///
/// Control: drop the `registry.seed_failed_languages(&conn)` call in
/// `daemon::run` - the outline reindexes `tools/gen.py` and
/// `added_python_symbol` is written. (Seeding in activation instead races
/// this call, which is why the seed moved.)
#[tokio::test]
async fn a_failed_language_is_not_reindexed_by_the_first_tool_call_after_a_restart() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();
    install_python_binary(&binary);
    index_recording_python_as_failed(&project, plugins.path());

    std::fs::write(
        project.root().join("tools/gen.py"),
        "def gen():\n    pass\n\ndef added_python_symbol():\n    pass\n",
    )
    .expect("failed to edit the python file");
    write_rust_marker(&project, "added_rust_symbol");

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    let python = outline(&client, "tools/gen.py").await;
    assert!(
        !has_node(&project, "added_python_symbol"),
        "the first call must not reindex a language recorded as failed: {python}"
    );
    let rust = outline(&client, "src/lib.rs").await;
    assert!(rust.contains("added_rust_symbol"), "a routed language is reindexed at query time: {rust}");
    assert!(!has_node(&project, "added_python_symbol"), "nor may any later step: {python}");
    client.cancel().await.expect("failed to shut the client down");
}

/// The other half of the seeding rule: a walk in the same activation replaces
/// the set seeded from the index with its own result. The index records
/// Python as failed but is marked as needing a walk; that walk indexes Python,
/// so a Python edit made afterwards is reindexed by a query.
///
/// Control: drop the `registry.set_failed_languages(..)` call in
/// `daemon::activation::ActivationCtx::walk` - the seeded `python` stays in
/// the set and `added_python_symbol` is never written.
#[tokio::test]
async fn a_walk_replaces_the_failed_languages_seeded_from_the_index() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, binary) = common::rust_and_missing_python_plugin_root();
    install_python_binary(&binary);
    index_recording_python_as_failed(&project, plugins.path());
    project
        .index()
        .execute("UPDATE meta SET bulkIndexedAt = NULL WHERE id = 1", [])
        .expect("failed to unmark the walk");

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    activated(project.root()).await;
    assert!(
        matches!(project.outcomes().as_slice(), [(p, LanguageOutcome::Indexed { .. }), _] if p == "python"),
        "the daemon walked and indexed python: {:?}",
        project.outcomes()
    );

    std::fs::write(
        project.root().join("tools/gen.py"),
        "def gen():\n    pass\n\ndef added_python_symbol():\n    pass\n",
    )
    .expect("failed to edit the python file");
    let python = outline(&client, "tools/gen.py").await;
    assert!(python.contains("added_python_symbol"), "the re-walked language is routed again: {python}");
    client.cancel().await.expect("failed to shut the client down");
}

/// GM-330/S12: the daemon's activation log line for a `Failed` language
/// (`daemon::activation::ActivationCtx::walk`) is ONE stderr line carrying
/// the whole stored chain joined by ": " (`languages::error_on_one_line`) -
/// here the walk step and, innermost, the missing-binary hint naming the
/// build command. The daemon is started directly with its stderr in a file,
/// so the line is read as the daemon wrote it (a shim would redirect it to
/// `daemon.log`).
///
/// Control: print the stored error raw in that `eprintln!` (no
/// `error_on_one_line`) - the line ends after the step, the hint lands on a
/// line of its own, and the exact comparison fails.
#[test]
fn the_daemons_log_line_for_a_failed_language_is_its_whole_chain_on_one_line() {
    let project = Project::new(&[("tools/gen.py", "def gen():\n    pass\n")]);
    let (plugins, _binary) = common::rust_and_missing_python_plugin_root();
    let logs = tempfile::tempdir().expect("failed to create a log directory");
    let log = logs.path().join("daemon.stderr");

    let mut child = StdCommand::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .current_dir(project.root())
        .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugins.path())
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).expect("failed to create the stderr file"))
        .spawn()
        .expect("failed to spawn the daemon");
    common::wait_for("the daemon to listen", common::startup_timeout(), || {
        daemon::is_listening(project.root()).unwrap_or(false)
    });
    // The log line is written before the phase leaves `indexing`.
    common::wait_until_phase(project.root(), "ready");
    common::kill_and_wait(child.id());
    let _ = child.wait();

    let stored = match project.outcomes().into_iter().find(|(language, _)| language == "python") {
        Some((_, LanguageOutcome::Failed { error })) => error,
        other => panic!("python must be recorded as Failed, got {other:?}"),
    };
    let causes: Vec<&str> = stored.lines().collect();
    assert_eq!(
        causes.first().copied(),
        Some("failed to spawn the python plugin's bulk index"),
        "the step is the outer cause: {stored}"
    );
    assert!(
        causes.len() >= 2 && causes.last().is_some_and(|hint| hint.contains("cargo build --workspace")),
        "the hint is a separate, innermost cause: {stored}"
    );

    let stderr = std::fs::read_to_string(&log).expect("failed to read the daemon's stderr");
    let lines = lines_starting(&stderr, "g-mesh daemon: python failed");
    assert_eq!(
        lines,
        [format!(
            "g-mesh daemon: python failed to index and is left out of the index until `g-mesh reindex`: {}",
            causes.join(": ")
        )],
        "one line with every cause: {stderr}"
    );
}
