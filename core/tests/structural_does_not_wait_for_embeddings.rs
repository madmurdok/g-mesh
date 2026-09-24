//! Acceptance tests for GM-395's slice 1: splitting the embedding backfill
//! pass (`embedding::backfill::run`) out of the cold-start walk must not make
//! a structural tool call (`find_definition`, ...) wait for it, and must
//! still make `search_code` wait for it - `daemon::indexing_status::Need`'s
//! whole reason to distinguish the two.
//!
//! [`g_mesh::embedding::backfill::HOLD_FILE_ENV`] parks the backfill pass
//! open, with the structural walk and its `bulkIndexedAt` marker already
//! committed, at exactly the point a real pass would start its first batch -
//! independent of whether a model is actually available (see that constant's
//! own doc comment), which is what lets both tests below run with no real
//! ONNX weights on disk.
//!
//! Both tests spawn a real daemon through a real `mcp-shim` (as
//! `serving_while_indexing.rs` does), not a directly-spawned daemon process:
//! the point here is what an ordinary MCP client sees, socket and all, not
//! the daemon's internals.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::Path;
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::embedding::backfill::HOLD_FILE_ENV;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const FIXTURE: &str = "export function connect(): number {\n  return 1;\n}\n";

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let path = project.root().join("src/index.ts");
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(&path, FIXTURE).expect("failed to write a fixture file");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Kills the daemon and its plugin the way a reboot would, and waits for
    /// each process to actually die - see `serving_while_indexing.rs`'s
    /// identical `Project::stop` for why this waits rather than merely
    /// signaling.
    fn stop(&self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            if let Some(pid) = daemon::read_pid_file(&path) {
                common::kill_and_wait(pid);
            }
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.stop();
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A client over a real shim, with the embedding backfill pass held open via
/// `hold_file` and the embedding model pointed at a directory that does not
/// exist, so `EmbeddingPipeline::is_available` reads `false` deterministically
/// regardless of what this machine happens to have fetched - see this
/// module's own doc comment on why the hold still applies even so.
async fn connect_with_hold(
    project: &Project,
    hold_file: &Path,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env(HOLD_FILE_ENV, hold_file)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim");

    ().serve(transport).await.expect("the shim must reach the daemon")
}

async fn find_definition(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": name }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

async fn search_code(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    query: &str,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new("search_code").with_arguments(
            json!({ "query": query }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("tools/call must return a result, not a protocol failure")
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

/// The headline case for `Need::Structural`: `find_definition` answers in
/// full while the embedding backfill pass is held open, well before anyone
/// releases it.
///
/// *Control:* in `mcp::mod::GMeshMcpServer::find_definition`, change
/// `self.prepare(Need::Structural)` to `self.prepare(Need::Embeddings)`. The
/// `tokio::time::timeout` below then elapses, because the hold file is never
/// removed by this test until after the assertion runs.
#[tokio::test]
async fn structural_tools_answer_while_the_embedding_backfill_pass_is_held_open() {
    let project = Project::new();
    let hold_file = project.root().join(".g-mesh-hold-the-embedding-pass");
    std::fs::write(&hold_file, b"").expect("failed to plant the embedding-pass hold file");

    let client = connect_with_hold(&project, &hold_file).await;
    // Returns once the structural walk is linked and `bulkIndexedAt` is
    // recorded - well before the embedding backfill pass (held open) could
    // ever finish, which is the whole point being tested below.
    wait_until_indexed(project.root());

    // Deliberately much shorter than `startup_timeout()` (60s), and clearly
    // shorter than `hold_before_first_batch_for_tests`'s own 30s hold cap
    // (`core/src/embedding/backfill.rs`): a `find_definition` that wrongly
    // waited on the hold would still return in ~30-33s (the hold's cap, plus
    // its own immediate-unavailable-model return) - well inside 60s, so that
    // bound alone could not tell a wrongly-waiting call apart from one that
    // never waited. 10s cannot be reached by a wrongly-waiting call, only by
    // one that answers off `Need::Structural` alone.
    const MUST_NOT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

    let result =
        tokio::time::timeout(MUST_NOT_WAIT_TIMEOUT, find_definition(&client, "connect")).await.expect(
            "find_definition must not wait for the embedding backfill pass - it only needs \
             Need::Structural, which the structural walk above already satisfies",
        );

    assert!(
        hold_file.exists(),
        "sanity: the embedding backfill pass must still be held open when find_definition returned, \
         otherwise this proves nothing about waiting"
    );

    let node = body(&result);
    assert_eq!(node["name"], "connect");
    assert_eq!(node["filePath"], "src/index.ts");

    std::fs::remove_file(&hold_file).expect("failed to release the embedding-pass hold");
    client.cancel().await.expect("failed to shut the client down");
}

/// The headline case for `Need::Embeddings`: `search_code`, issued while the
/// embedding backfill pass is held open, has not returned three seconds
/// later - and does return once the hold is released (a "semantic search
/// unavailable" error is the expected outcome with no real weights on disk;
/// the point under test is *when* it returns, not what it says).
///
/// *Control:* in `mcp::mod::GMeshMcpServer::search_code`, change
/// `self.prepare(Need::Embeddings)` to `self.prepare(Need::Structural)`. The
/// `tokio::select!` below then resolves the call arm within the 3s window
/// instead of the timer arm, and the `panic!` fires.
#[tokio::test]
async fn search_code_waits_for_the_embedding_backfill_pass_but_not_forever() {
    let project = Project::new();
    let hold_file = project.root().join(".g-mesh-hold-the-embedding-pass");
    std::fs::write(&hold_file, b"").expect("failed to plant the embedding-pass hold file");

    let client = connect_with_hold(&project, &hold_file).await;
    wait_until_indexed(project.root());

    // Scoped so the pinned `call` future - and the borrow of `client` it
    // holds - ends before `client.cancel()` needs to move it, below.
    {
        let call = search_code(&client, "connect to a database");
        tokio::pin!(call);

        tokio::select! {
            _ = &mut call => panic!(
                "search_code returned before the embedding backfill pass was released - it must wait \
                 for Need::Embeddings, not just Need::Structural"
            ),
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
        assert!(hold_file.exists(), "sanity: the hold must still be in effect at the 3s mark");

        std::fs::remove_file(&hold_file).expect("failed to release the embedding-pass hold");

        // No assertion on the result beyond having received one at all:
        // without real weights on disk, "semantic search unavailable" is the
        // correct, successful-in-the-sense-that-matters-here answer - see
        // this test's own doc comment.
        let _ = tokio::time::timeout(common::startup_timeout(), call)
            .await
            .expect("search_code must return once the embedding backfill pass finishes");
    }

    client.cancel().await.expect("failed to shut the client down");
}
