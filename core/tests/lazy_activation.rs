//! Acceptance tests for GM-395's slice 2 (`docs/architecture/lazy-indexing.md`,
//! D2): the daemon builds nothing until a tool call needs it.
//!
//! - Connecting alone - `initialize`, `tools/list` - walks nothing.
//! - The first index-needing call starts the walk and waits for it, then
//!   answers in full.
//! - An index that is already walked is reused, never walked again.
//! - A failed walk is that call's tool error, the session survives it, and
//!   the next call retries the walk.
//!
//! Everything is real: the real binary, a real shim bootstrapping a real
//! detached daemon, and a real `rmcp` client.

use std::path::{Path, PathBuf};
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One TS file with three top-level declarations, so "the full outline" is a
/// list a partial or empty index cannot fake.
const FILES: [(&str, &str); 1] = [(
    "src/index.ts",
    "export function connect(): number {\n  return 1;\n}\n\nexport class Pool {}\n\nexport const size = 3;\n",
)];

/// The names `get_file_outline` must return for `src/index.ts`, in source
/// order.
const FULL_OUTLINE: [&str; 3] = ["connect", "Pool", "size"];

struct Project {
    dir: tempfile::TempDir,
    /// Outside the project root, so writing it cannot feed the watcher.
    log_dir: tempfile::TempDir,
}

impl Project {
    fn empty() -> Self {
        Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            log_dir: tempfile::tempdir().expect("failed to create a temp log directory"),
        }
    }

    fn new() -> Self {
        let project = Self::empty();
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

    fn log(&self) -> PathBuf {
        self.log_dir.path().join("daemon.log")
    }

    fn db(&self) -> Connection {
        let db = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        Connection::open(db).expect("failed to open the project's index")
    }

    fn bulk_indexed(&self) -> bool {
        schema::bulk_index_completed(&self.db()).expect("failed to read bulkIndexedAt")
    }

    /// A client over a real shim, which bootstraps the project's daemon.
    /// `env` is passed to the shim, and so inherited by that daemon.
    async fn connect(&self, env: &[(&str, &std::ffi::OsStr)]) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let log = self.log();
        let env: Vec<(String, std::ffi::OsString)> =
            env.iter().map(|(key, value)| (key.to_string(), value.to_os_string())).collect();
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                // No real model: keeps the embedding backfill pass a no-op, so
                // nothing here depends on this machine's model cache.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
            for (key, value) in &env {
                cmd.env(key, value);
            }
        }))
        .expect("failed to spawn the shim");
        ().serve(transport).await.expect("the shim must reach the daemon")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())].into_iter().flatten()
        {
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

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, arguments: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(arguments.as_object().cloned().expect("arguments literal is an object")),
        )
        .await
        .unwrap_or_else(|err| panic!("{tool} must return a result, not a protocol failure: {err}"))
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {}", text(result));
    serde_json::from_str(&text(result)).expect("tool result is not JSON")
}

/// Every running process whose command line names a `--bulk-index` run for
/// `root` (either spelling of it: the daemon walks the canonical path).
#[cfg(unix)]
fn bulk_index_processes_for(root: &Path) -> Vec<String> {
    let spellings = [root.display().to_string(), root.canonicalize().unwrap().display().to_string()];
    let listing =
        std::process::Command::new("ps").args(["-axo", "pid,command"]).output().expect("failed to run ps");
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|line| {
            line.contains("--bulk-index") && spellings.iter().any(|root| line.contains(root.as_str()))
        })
        .map(str::to_string)
        .collect()
}

/// A1: a session that never calls a tool costs nothing. `initialize` and
/// `tools/list` walk nothing, and nothing is walking for two seconds after.
///
/// The process scan is sampled every 100ms rather than once at the end, so a
/// walk started by the connect cannot finish unseen between samples: a
/// one-shot `--bulk-index` run spawns Node, which alone outlasts a sample
/// interval.
///
/// 2b adds the third assertion (`index.phase` reads `unindexed`).
#[tokio::test]
async fn connecting_alone_indexes_nothing() {
    let project = Project::new();
    let client = project.connect(&[]).await;
    let listed = client.list_tools(None).await.expect("tools/list failed");
    assert!(!listed.tools.is_empty());

    let mut seen_walking: Vec<String> = Vec::new();
    let watch_until = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < watch_until {
        #[cfg(unix)]
        seen_walking.extend(bulk_index_processes_for(project.root()));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(!project.bulk_indexed(), "connecting alone must not walk the project: bulkIndexedAt is set");
    assert!(
        seen_walking.is_empty(),
        "connecting alone must not start a --bulk-index run for the project, saw: {seen_walking:#?}"
    );
    // GM-395 slice 2b (D13): the daemon's own `index.phase` file must agree -
    // a session that never asked for anything leaves the daemon at
    // `Unindexed`, not merely "not yet recorded as walked".
    //
    // *Control:* call `request_activation()` right after bind (in the
    // worktree, inside `daemon::run`, right after `indexing.attach_phase_file`),
    // and this assertion fails alongside the two above: the daemon moves to
    // `Walking` before this test ever gets to read the file.
    let phase = std::fs::read_to_string(daemon::phase_path_in(&project_dir(project.root()).unwrap()))
        .expect("a running daemon must have published its index.phase");
    assert_eq!(phase.trim(), "unindexed", "connecting alone must not move the daemon past Unindexed");

    client.cancel().await.expect("failed to shut the client down");
}

/// A4 (blocking part): the first structural call on an unwalked project
/// starts the walk, waits for it, and answers in full - never "not found"
/// off an index that is not built yet.
#[tokio::test]
async fn first_structural_call_blocks_and_answers_fully() {
    let project = Project::new();
    let client = project.connect(&[]).await;
    assert!(!project.bulk_indexed(), "sanity: nothing may have walked the project before the first call");

    let outline = body(&call(&client, "get_file_outline", json!({ "file_path": "src/index.ts" })).await);
    let names: Vec<&str> = outline["results"]
        .as_array()
        .unwrap_or_else(|| panic!("an outline has a results array: {outline}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("every outline symbol has a name"))
        .collect();
    assert_eq!(names, FULL_OUTLINE, "the first call must answer off the complete index: {outline}");

    assert!(project.bulk_indexed(), "the walk the first call waited for must have been recorded");
    // Also what makes `an_existing_index_is_not_rewalked`'s absent log line
    // mean something: the log channel it reads does carry this line when a
    // walk happens.
    let log = std::fs::read_to_string(project.log()).expect("the daemon log must exist");
    assert!(log.contains("initial index built"), "the walk must be logged:\n{log}");

    client.cancel().await.expect("failed to shut the client down");
}

/// A6 / D9: an index `g-mesh init` already built is reused. The first call
/// answers off it, and nothing walks it again: `bulkIndexedAt` and the node
/// rows are untouched, and the daemon log has no "initial index built" line.
#[tokio::test]
async fn an_existing_index_is_not_rewalked() {
    let project = Project::new();
    let init = std::process::Command::new(BIN)
        .arg("init")
        .current_dir(project.root())
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .output()
        .expect("failed to run `g-mesh init`");
    assert!(init.status.success(), "g-mesh init failed: {}", String::from_utf8_lossy(&init.stderr));

    let snapshot = |project: &Project| -> (Option<String>, i64) {
        let db = project.db();
        let indexed_at = db
            .query_row("SELECT bulkIndexedAt FROM meta WHERE id = 1", [], |row| row.get(0))
            .expect("failed to read bulkIndexedAt");
        let max_rowid = db.query_row("SELECT MAX(rowid) FROM nodes", [], |row| row.get(0)).expect("no nodes");
        (indexed_at, max_rowid)
    };
    let before = snapshot(&project);
    assert!(before.0.is_some(), "sanity: `g-mesh init` must have recorded a walk");

    let client = project.connect(&[]).await;
    let references = call(&client, "find_references", json!({ "symbol_name": "connect" })).await;
    body(&references);
    client.cancel().await.expect("failed to shut the client down");

    assert_eq!(snapshot(&project), before, "an already-walked index must not be walked again");
    let log = std::fs::read_to_string(project.log()).unwrap_or_default();
    assert!(!log.contains("initial index built"), "the daemon walked an already-walked index:\n{log}");
}

/// D2's failure path: a walk that fails - here, a plugin binary that was
/// never built (`common::missing_workspace_binary_plugin_root`) - is that
/// call's tool error carrying the hint, the session survives it, and once
/// the cause is fixed the next call retries the walk and answers.
///
/// The fix is a stand-in plugin written where the missing binary should be:
/// a shell script that answers `--bulk-index` with a fixed NDJSON stream, so
/// Unix only.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_walk_is_a_tool_error_and_is_retried() {
    use g_mesh::protocol::types::{
        EdgeKind, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
    };
    use std::os::unix::fs::PermissionsExt;

    let project = Project::empty();
    std::fs::write(project.root().join("greet.py"), "def greet():\n    return 1\n")
        .expect("failed to write a fixture file");
    let (plugin_root, binary) = common::missing_workspace_binary_plugin_root();
    let client = project.connect(&[("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugin_root.path().as_os_str())]).await;

    let failed = call(&client, "find_definition", json!({ "symbol_name": "greet" })).await;
    assert_eq!(failed.is_error, Some(true), "a failed walk must be a tool error: {}", text(&failed));
    assert!(
        text(&failed).contains("cargo build --workspace")
            && text(&failed).contains(&binary.display().to_string()),
        "the tool error must carry the missing-binary hint: {}",
        text(&failed)
    );

    client.list_tools(None).await.expect("the session must survive a failed walk");

    // The fix: a plugin that walks `greet.py` into one file and one function.
    let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 12 } };
    let node = |id: &str, kind: NodeKind, name: &str| WireNode {
        id: id.to_string(),
        kind,
        name: name.to_string(),
        qualified_name: name.to_string(),
        file_path: "greet.py".to_string(),
        range,
        signature: None,
        visibility: Visibility::Public,
        doc_comment: None,
        language: "python".to_string(),
        native_kind: None,
        has_syntax_errors: false,
        declarations: None,
        container: None,
        container_parent: None,
        target: None,
    };
    let defines = WireEdge {
        id: "greet.py->greet".to_string(),
        from_id: "greet.py".to_string(),
        to_id: "greet.py#greet".to_string(),
        kind: EdgeKind::Defines,
        source: SourceTier::Syntactic,
        engine: "fixture".to_string(),
        resolved: true,
        to_declaration: None,
    };
    let stream = [
        serde_json::to_string(&node("greet.py", NodeKind::File, "greet.py")).unwrap(),
        serde_json::to_string(&node("greet.py#greet", NodeKind::Function, "greet")).unwrap(),
        serde_json::to_string(&defines).unwrap(),
    ]
    .join("\n")
        + "\n";
    let stream_file = plugin_root.path().join("walk.ndjson");
    std::fs::write(&stream_file, stream).expect("failed to write the stand-in walk");
    std::fs::create_dir_all(binary.parent().unwrap()).expect("failed to create the binary's directory");
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\ncase \"$1\" in --bulk-index) cat '{}'; exit 0;; esac\nexit 1\n",
            stream_file.display()
        ),
    )
    .expect("failed to write the stand-in plugin");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("failed to make the stand-in plugin executable");

    let retried = call(&client, "find_definition", json!({ "symbol_name": "greet" })).await;
    let definition = body(&retried);
    assert_eq!(definition["name"], "greet", "the retried walk must answer: {definition}");
    assert!(project.bulk_indexed(), "the retried walk must have been recorded");

    client.cancel().await.expect("failed to shut the client down");
}

/// GM-395 slice 2b: a panic in the embedding backfill pass
/// (`embedding::backfill::run`, forced here by its test-only
/// `PANIC_ENV` knob) is caught by `daemon::activation::ActivationCtx::
/// activate`'s `catch_unwind` around that call and logged, not turned into
/// `Phase::Failed` - structural tools must keep answering even though
/// `search_code` never will for this project, and the daemon must not
/// re-run (or re-panic on) the same pass on a later call once it has moved
/// on to `Phase::Ready`.
///
/// *Control:* remove the `catch_unwind` (and its `AssertUnwindSafe` wrapper)
/// around the `embedding::backfill::run` call in
/// `daemon::activation::ActivationCtx::activate`, propagating the panic as
/// this method's `Err` instead. Both calls below then fail: the first
/// becomes a tool error ("g-mesh could not build this project's index: the
/// index build panicked: ..."), because the panic unwinds through `activate`
/// before `Phase::Ready` is ever reached and `daemon::activation::run` records
/// it as `Phase::Failed`.
#[tokio::test]
async fn a_panicking_embedding_pass_does_not_fail_structural_tools() {
    let project = Project::new();
    let client =
        project.connect(&[(g_mesh::embedding::backfill::PANIC_ENV, std::ffi::OsStr::new("1"))]).await;

    let outline = body(&call(&client, "get_file_outline", json!({ "file_path": "src/index.ts" })).await);
    let names: Vec<&str> = outline["results"]
        .as_array()
        .unwrap_or_else(|| panic!("an outline has a results array: {outline}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("every outline symbol has a name"))
        .collect();
    assert_eq!(
        names, FULL_OUTLINE,
        "a panicking embedding pass must not block or corrupt the structural answer: {outline}"
    );

    // A second call must also answer, not error: the phase must have moved
    // on to `Ready` (never `Failed`), so nothing here is still owed and
    // nothing is retried.
    let again = call(&client, "get_file_outline", json!({ "file_path": "src/index.ts" })).await;
    assert_ne!(
        again.is_error,
        Some(true),
        "a second call must not error either - the panic must not have left the project `Failed`: {}",
        text(&again)
    );

    client.cancel().await.expect("failed to shut the client down");
}
