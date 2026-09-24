//! GM-316: a discovered plugin whose `command` is a cargo-workspace binary
//! that was never built must fail the walk with a message naming the
//! binary, the fact it was never built, and `cargo build --workspace` - not
//! the bare `No such file or directory (os error 2)` `Command::spawn`
//! reports on its own.
//!
//! GM-395 slice 2 changed *where* that message goes. The walk no longer
//! runs at daemon startup but on the first tool call, and a failed walk no
//! longer ends the daemon - that would drop the session of the very call
//! that asked. So the message now comes back as that call's tool error, and
//! the daemon keeps serving (and retries the walk on the next call - see
//! `lazy_activation.rs`).
//!
//! Traced while verifying GM-301: a worktree where `cargo build --workspace`
//! had not been run failed `cargo test -p g-mesh --test cli_stop` with
//! "timed out waiting for the daemon to spawn its plugin within 60s" at load
//! 30, which reads exactly like a regression in the work under review. The
//! real cause was in the daemon log, not the test failure: `daemon::
//! bulk_index::walk_one_language` spawns every *discovered* plugin's one-shot
//! `--bulk-index` binary unconditionally (see that module's own doc comment),
//! `cargo test -p g-mesh` never builds sibling workspace members, and
//! `plugins/python/plugin.toml`'s bundled `command` names a `target/debug/
//! g-mesh-plugin-python` that therefore did not exist. Every test waiting for
//! a plugin pid timed out naming the daemon, because the daemon's cold-start
//! walk had already failed and nothing had spawned.
//!
//! This exercises the real `g-mesh daemon` binary, through a real shim,
//! against a fixture plugin (`common::missing_workspace_binary_plugin_root`)
//! whose `command` points at a `target/debug/` binary that is deliberately
//! never created - the same shape as the real bug, without needing an actual
//! cargo build to reproduce it - the same approach
//! `daemon_plugin_discovery_failure.rs` already takes for a different
//! `discover()`-time failure.

use std::path::Path;

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root") }
    }

    fn root(&self) -> &Path {
        self.dir.path()
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

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_workspace_built_plugin_binary_is_a_tool_error_naming_the_build_command() {
    let project = Project::new();
    let (plugin_root, binary) = common::missing_workspace_binary_plugin_root();

    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by the daemon the shim bootstraps.
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugin_root.path())
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("the shim must reach the daemon");

    let result = client
        .call_tool(
            CallToolRequestParams::new("find_definition").with_arguments(
                json!({ "symbol_name": "anything" })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("a failed walk must come back as a tool result, not end the session");

    assert_eq!(result.is_error, Some(true), "a failed walk must be a tool error: {}", text(&result));
    let message = text(&result);
    assert!(
        message.contains(&binary.display().to_string()),
        "the error must name the missing binary's path: {message}"
    );
    assert!(
        message.contains("has not been built yet"),
        "the error must say the binary was never built, not just that spawn failed: {message}"
    );
    assert!(message.contains("cargo build --workspace"), "the error must name the fix: {message}");
    assert!(
        !message.contains("os error 2"),
        "the raw OS error must not be the only thing surfaced - the hint should replace it, not \
         merely accompany it: {message}"
    );

    // The daemon keeps serving: the same session still answers, and the
    // daemon is still listening for new ones.
    let listed = client.list_tools(None).await.expect("the session must survive a failed walk");
    assert!(!listed.tools.is_empty());
    assert!(
        daemon::is_listening(project.root()).unwrap(),
        "a failed walk must not take the daemon down with it"
    );

    client.cancel().await.expect("failed to shut the client down");
}
