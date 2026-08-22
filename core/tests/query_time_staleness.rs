//! Acceptance test for task 117: `watcher::staleness::ensure_fresh` closes a
//! real gap task 111's mutex-serialization argument does not cover, and this
//! is the case that proves it.
//!
//! Task 111 (see `daemon::indexing_status`'s "Why the incremental-edit
//! watcher path does not re-arm this", and this doc's "Ideas surfaced while
//! comparing kungfu" section) reasoned that a query landing while a watcher
//! commit is *in flight* can only ever read stale-but-consistent data, never
//! torn data, because `apply_file_change` holds the same connection mutex
//! every MCP handler locks to answer a query. That argument says nothing
//! about a change the watcher never had a chance to see *at all* - and
//! `an_index_this_indexer_built_is_taken_at_its_word_across_a_restart` in
//! `stale_index_invalidation.rs` already proves the other half of why that
//! happens routinely: an index stamped with the current generation is not
//! re-walked on restart, so a file changed while the daemon was not running
//! is invisible to both the cold-start walk (trusts the existing index) and
//! the watcher (was not running to see the write happen). No mutex is ever
//! held over that gap, because nothing is applying anything - it is not a
//! narrow, self-correcting race, but a staleness that would otherwise persist
//! forever.
//!
//! This drives exactly that sequence against the real binary: index a
//! project, kill the daemon the way a reboot would, edit a file while nothing
//! is running to notice, restart, and query immediately - no artificial delay
//! needed, because nothing in the restarted daemon's own startup would ever
//! catch this on its own. `get_file_outline` must answer with the *current*
//! on-disk content, not the one that was current when it was first indexed.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const FILE: &str = "src/lib.ts";
const ORIGINAL: &str = "export function greet(): string {\n  return \"hello\";\n}\n";
const EDITED_WHILE_DAEMON_WAS_DOWN: &str =
    "export function greet(): string {\n  return \"hello\";\n}\n\nexport function farewell(): string {\n  return \"bye\";\n}\n";

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let path = project.root().join(FILE);
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(&path, ORIGINAL).expect("failed to write the fixture file");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn file_path(&self) -> PathBuf {
        self.root().join(FILE)
    }

    /// Stops the daemon and its plugin the way a reboot or an `-9` would,
    /// leaving the index exactly as it is on disk - matching
    /// `stale_index_invalidation.rs`'s helper of the same shape, which is
    /// what establishes that a restart against it does not re-walk.
    fn stop(&self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            if let Some(pid) = daemon::read_pid_file(&path) {
                let _ = StdCommand::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        // Give the OS a moment to actually reap the killed processes before
        // the next phase starts a fresh daemon against the same state
        // directory - a purely defensive wait, not part of what is under
        // test.
        std::thread::sleep(Duration::from_millis(100));
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

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Connects a fresh shim/daemon pair, waits for the index to be readable,
/// calls `get_file_outline` on [`FILE`], then tears the whole stack back
/// down - so each phase of the test observes a daemon that just started up
/// against whatever was on disk and in the state directory at that moment.
async fn outline_symbol_names(project: &Project) -> Vec<String> {
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");

    // Same marker `stale_index_invalidation.rs` waits on across its own
    // restarts: it reflects a walk that already completed and is not redone
    // for an index stamped with the current generation, so waiting on it here
    // does not give the watcher any extra time to notice the on-disk edit -
    // there is no walk left for it to be part of.
    wait_until_indexed(project.root());

    let result = client
        .call_tool(CallToolRequestParams::new("get_file_outline").with_arguments(
            json!({ "file_path": FILE }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("tools/call failed");
    let page = body(&result);

    client.cancel().await.expect("failed to shut the client down");
    project.stop();

    page["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_file_edited_while_the_daemon_was_down_is_reindexed_before_the_next_query_answers() {
    let project = Project::new();

    assert_eq!(
        outline_symbol_names(&project).await,
        vec!["greet".to_string()],
        "the cold-start index must answer this before anything is done to it"
    );

    // The daemon is fully down - no watcher running, nothing to queue an
    // event for - when the file changes. This is the scenario task 111's
    // mutex-serialization argument does not cover: there is no commit in
    // flight to block a query on, because nothing here is applying anything
    // at all.
    std::fs::write(project.file_path(), EDITED_WHILE_DAEMON_WAS_DOWN)
        .expect("failed to edit the fixture file while the daemon was down");

    // A fresh daemon starts against the same (current-generation) index, so
    // it trusts it rather than re-walking, and its watcher only starts
    // watching from this moment on - it was never running to see the write
    // above happen. Without `ensure_fresh` wired into `get_file_outline`,
    // this would silently answer with the pre-edit outline forever, since
    // nothing else in the system will ever notice this file changed.
    assert_eq!(
        outline_symbol_names(&project).await,
        vec!["greet".to_string(), "farewell".to_string()],
        "a file edited while the daemon was down must be reindexed before the next query answers, \
         not served from a graph that predates the edit"
    );
}
