//! GM-503 end to end: a path-anchored tool asked about a file whose language
//! is not indexed at all answers with the reason and the fixing command
//! (`docs/architecture/gm-503-absent-language-field.md`), over the real
//! `g-mesh` binary, the real Rust plugin and, for the mid-session test, the
//! real Python plugin.
//!
//! - absent: the plugin root holds only Rust; `tools/gen.py` is refused with
//!   `g-mesh plugins install python`, a Rust miss stays plain text.
//! - failed: the Python plugin's binary was never built
//!   (`common::rust_and_missing_python_plugin_root`); the refusal names
//!   `g-mesh reindex` and the innermost cause.
//! - AC3, mid-session removal: a folder session reselects a project after
//!   its daemon restarted without the Python plugin, and the same session
//!   now gets the absent refusal for a file it had an outline for.
//!
//! The control for the whole file: in `GMeshMcpServer::get_file_outline`,
//! pass `None` instead of `coverage` to `get_file_outline::handle_covered` -
//! every outline refusal is the plain `no file '...' found` text and the
//! JSON parse fails. Each test names its own narrower control too.

use std::path::{Path, PathBuf};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

mod common;

use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const NO_MODEL_DIR: &str = "/nonexistent-g-mesh-test-model-dir";

const RUST_LIB: &str = "pub fn kept_rust_symbol() -> u32 {\n    1\n}\n";
const PYTHON_GEN: &str = "def gen():\n    pass\n";

/// Writes `files` under `root`, creating directories as needed.
fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(path, contents).expect("failed to write a fixture file");
    }
}

/// Kills whatever daemon and plugins serve `dir` and removes its state.
fn clean_up(dir: &Path) {
    let Ok(state) = project_dir(dir) else { return };
    for path in [daemon::pid_path(dir), daemon::plugin_pid_path(dir)].into_iter().flatten() {
        common::kill_pid_file(&path);
    }
    for (_, plugin_pid) in daemon::registry::discovered_pid_files(&state) {
        common::kill_pid_file(&plugin_pid);
    }
    if let Ok(endpoint) = daemon::endpoint(dir) {
        endpoint.clear_stale();
    }
    let _ = std::fs::remove_dir_all(state);
}

/// A small Rust crate with a Python file beside it.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        write_files(
            project.root(),
            &[
                ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
                ("src/lib.rs", RUST_LIB),
                ("tools/gen.py", PYTHON_GEN),
            ],
        );
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        clean_up(self.root());
    }
}

fn shim(dir: &Path, plugins: &Path) -> TokioChildProcess {
    let dir: PathBuf = dir.to_path_buf();
    let plugins = plugins.to_path_buf();
    TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&dir)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by every daemon the shim bootstraps.
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", &plugins)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, NO_MODEL_DIR);
    }))
    .expect("failed to spawn the shim")
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let arguments = args.as_object().cloned().expect("arguments must be an object");
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .unwrap_or_else(|err| panic!("{name} must return a result, not a protocol failure: {err}"))
}

/// The answer's body: its last text block (a switched folder session puts a
/// line naming the project before it).
fn body_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| text.text.clone())
        .next_back()
        .expect("the answer has a text block")
}

/// The JSON body of an answer that must be a not-indexed refusal.
fn refusal(result: &CallToolResult) -> serde_json::Value {
    let text = body_text(result);
    assert_eq!(result.is_error, Some(true), "expected a tool error: {text}");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("a refusal's body must be JSON ({err}): {text}"))
}

fn absent_python() -> serde_json::Value {
    json!({ "language": "python", "reason": "pluginAbsent", "command": "g-mesh plugins install python" })
}

/// Asserts `result` refuses `tools/gen.py` as absent, after `message`.
fn assert_absent_refusal(result: &CallToolResult, message: &str) {
    let body = refusal(result);
    assert_eq!(body["notIndexed"], absent_python(), "{body}");
    let error = body["error"].as_str().expect("the refusal keeps a human `error` string");
    assert!(error.starts_with(message), "{error}");
    assert!(error.contains("`g-mesh plugins install python`"), "{error}");
}

/// Over the wire, absent: the three path-anchored tools refuse the
/// Python file with the install command, a Rust miss stays the plain
/// message, and a `file_paths` filter names the Python entry.
///
/// Control: drop the `registry.path_coverage(..)` calls in the three
/// `GMeshMcpServer` wrappers (pass `None`) - the three refusals are plain
/// text; return an empty list from `filter_coverage` - `notIndexed` is
/// missing from `find_references`.
#[tokio::test]
async fn an_absent_languages_file_is_refused_with_the_install_command_over_the_wire() {
    let project = Project::new();
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    common::add_real_rust_plugin(plugins.path());

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");

    let outline = call(&client, "get_file_outline", json!({ "file_path": "tools/gen.py" })).await;
    assert_absent_refusal(&outline, "g-mesh: no file 'tools/gen.py' found in the index");

    let definition = call(
        &client,
        "find_definition",
        json!({ "file_path": "tools/gen.py", "position": { "line": 0, "col": 0 } }),
    )
    .await;
    assert_absent_refusal(&definition, "g-mesh: no symbol found at tools/gen.py:0:0");

    let dependencies =
        call(&client, "get_dependencies", json!({ "file_path": "tools/gen.py", "direction": "Outgoing" }))
            .await;
    assert_absent_refusal(&dependencies, "g-mesh: no file 'tools/gen.py' found in the index");

    let rust_miss = call(&client, "get_file_outline", json!({ "file_path": "src/nope.rs" })).await;
    let text = body_text(&rust_miss);
    assert_eq!(rust_miss.is_error, Some(true), "{text}");
    assert_eq!(
        text, "g-mesh: no file 'src/nope.rs' found in the index",
        "AC2: no reason for a covered language"
    );

    let rust_hit = call(&client, "get_file_outline", json!({ "file_path": "src/lib.rs" })).await;
    let text = body_text(&rust_hit);
    assert_ne!(rust_hit.is_error, Some(true), "{text}");
    assert!(!text.contains("notIndexed"), "AC2: an indexed file carries no field: {text}");

    let references = call(
        &client,
        "find_references",
        json!({ "symbol_name": "kept_rust_symbol", "file_paths": ["src/lib.rs", "tools/gen.py"] }),
    )
    .await;
    let text = body_text(&references);
    assert_ne!(references.is_error, Some(true), "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).expect("a find answer is JSON");
    let mut expected = absent_python();
    expected["filePaths"] = json!(["tools/gen.py"]);
    assert_eq!(body["notIndexed"], json!([expected]), "{body}");

    client.cancel().await.expect("failed to shut the client down");
}

/// Over the wire, failed: the Python plugin's binary is missing, so the
/// walk failed it; the refusal names `g-mesh reindex` and one line of cause,
/// never the install command.
///
/// Control: make `PluginRegistry::path_coverage` return `None` for a failed
/// language - the outline is the plain no-file text.
#[tokio::test]
async fn a_failed_languages_file_is_refused_with_the_reindex_command_over_the_wire() {
    let project = Project::new();
    let (plugins, _binary) = common::rust_and_missing_python_plugin_root();

    let client =
        ().serve(shim(project.root(), plugins.path())).await.expect("the shim must reach the daemon");
    let outline = call(&client, "get_file_outline", json!({ "file_path": "tools/gen.py" })).await;
    let body = refusal(&outline);
    let field = &body["notIndexed"];
    assert_eq!(field["language"], "python", "{body}");
    assert_eq!(field["reason"], "pluginFailed", "{body}");
    assert_eq!(field["command"], "g-mesh reindex", "{body}");
    let cause = field["error"].as_str().expect("a failed walk records its cause");
    assert!(!cause.is_empty() && !cause.contains('\n'), "the innermost cause, on one line: {cause:?}");

    let error = body["error"].as_str().unwrap();
    assert!(error.contains(&format!("its plugin failed ({cause})")), "{error}");
    assert!(error.contains("`g-mesh reindex`"), "{error}");
    assert!(!error.contains("plugins install"), "{error}");

    client.cancel().await.expect("failed to shut the client down");
}

/// A folder of two projects: `a` (Rust + Python) and `b` (Rust).
struct Folder {
    dir: tempfile::TempDir,
}

impl Folder {
    fn new() -> Self {
        let folder = Self { dir: tempfile::tempdir().expect("failed to create a temp folder") };
        write_files(
            folder.root(),
            &[
                ("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
                ("a/src/lib.rs", RUST_LIB),
                ("a/tools/gen.py", PYTHON_GEN),
                ("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
                ("b/src/lib.rs", "pub fn b() {}\n"),
            ],
        );
        std::fs::create_dir_all(folder.sub("a").join(".git")).unwrap();
        std::fs::create_dir_all(folder.sub("b").join(".git")).unwrap();
        folder
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        for dir in [self.root().to_path_buf(), self.sub("a"), self.sub("b")] {
            clean_up(&dir);
        }
    }
}

/// A plugin root with the real Rust and Python manifests (both resolve
/// their binaries in this profile's `target/`).
fn rust_and_python_plugin_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a plugin root");
    common::add_real_rust_plugin(root.path());
    let python = root.path().join("python");
    std::fs::create_dir_all(&python).expect("failed to create the python plugin directory");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/python/plugin.toml"),
        python.join("plugin.toml"),
    )
    .expect("failed to copy the real Python manifest");
    root
}

async fn select(client: &RunningService<RoleClient, ()>, project: &str) {
    let result = call(client, "select_project", json!({ "project": project })).await;
    assert_ne!(result.is_error, Some(true), "the switch to {project} must succeed: {}", body_text(&result));
}

/// Owner-approved meaning: a folder session's project loses its Python
/// plugin while the session is on another project; its daemon restarts
/// without it, and on reselection the same session's outline of the Python
/// file it had read before is the absent refusal, not a bare "not found".
/// No order between processes is asserted beyond the test's own sequential
/// calls and the stop it waits for.
///
/// Control: pass `None` to `get_file_outline::handle_covered` in
/// `GMeshMcpServer::get_file_outline` - the last outline is the plain
/// no-file text and the JSON parse fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_field_appears_after_a_plugin_is_removed_mid_session() {
    let folder = Folder::new();
    let plugins = rust_and_python_plugin_root();
    let a = folder.sub("a");

    let client = ().serve(shim(folder.root(), plugins.path())).await.expect("the shim must reach the front");

    select(&client, "a").await;
    let before = call(&client, "get_file_outline", json!({ "file_path": "tools/gen.py" })).await;
    let text = body_text(&before);
    assert_ne!(before.is_error, Some(true), "python is indexed before the removal: {text}");
    assert!(text.contains("\"gen\""), "the outline names gen: {text}");

    select(&client, "b").await;
    std::fs::remove_dir_all(plugins.path().join("python")).expect("failed to remove the python plugin");
    // The exit status is not asserted: the daemon a shim bootstrapped stays
    // a zombie child of that shim until reaped, and `g-mesh stop` reports a
    // still-present pid as a failure. What the test needs is that a's daemon
    // no longer serves, which the wait below checks.
    let _ = std::process::Command::new(BIN)
        .arg("stop")
        .current_dir(&a)
        .output()
        .expect("failed to run `g-mesh stop`");
    common::wait_for("a's daemon to stop", common::startup_timeout(), || {
        !daemon::is_listening(&a).unwrap_or(true)
    });

    select(&client, "a").await;
    let after = call(&client, "get_file_outline", json!({ "file_path": "tools/gen.py" })).await;
    assert_absent_refusal(&after, "g-mesh: no file 'tools/gen.py' found in the index");

    client.cancel().await.expect("failed to shut the client down");
}
