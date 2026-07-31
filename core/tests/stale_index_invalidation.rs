//! Acceptance test for task 96: an index left behind by an older generation
//! of the indexer must be thrown away and rebuilt, not served.
//!
//! The bug had no failing layer. Extraction, linking and traversal were each
//! correct on the code as it stood; what was wrong was the *data*, produced
//! months of commits earlier and kept forever because nothing about the DDL
//! had changed since. The only way to see it is to do what a real project
//! does - index once, keep the state directory, come back later with a build
//! whose output would differ - so that is what this drives, over the real
//! shim, daemon, plugin and bulk index.
//!
//! Requires `plugins/js-ts/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One file importing another - the smallest graph whose `Incoming` walk is
/// non-empty, and the exact shape that came back empty off the stale index.
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

    fn index_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }

    /// Stops the daemon and its plugin the way a reboot or an `-9` would,
    /// leaving the index exactly as it is on disk - which is the state the
    /// next start has to make a judgement about.
    fn stop(&self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            if let Some(pid) = daemon::read_pid_file(&path) {
                // Stopped twice over on the last phase (here, then again from
                // `Drop`), so "no such process" is the expected second answer
                // and not worth printing into the test output.
                let _ = StdCommand::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        // The socket file outlives the process it belonged to; leaving it
        // would only prove that the shim handles a stale one, which is not
        // this test's subject.
        if let Ok(socket) = daemon::socket_path(self.root()) {
            let _ = std::fs::remove_file(socket);
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

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Runs one `get_dependencies` call against a freshly started shim, then shuts
/// the whole stack down again - so that each phase of the test observes a
/// daemon that had to make its own decision about the index on disk.
async fn importers_of(project: &Project, file_path: &str) -> Vec<String> {
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(project.root());
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    // Waited for *after* the connection is up, which is what makes it correct
    // across the invalidating restarts this file stages: the replacement
    // daemon wipes the index - completion marker and all - before it binds,
    // so a marker still readable here could only be the new walk's own.
    wait_until_indexed(project.root());

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_dependencies").with_arguments(
                json!({ "file_path": file_path, "direction": "Incoming" })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call failed");
    let walk = body(&result);

    client.cancel().await.expect("failed to shut the client down");
    project.stop();

    let mut paths: Vec<String> = walk["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .filter_map(|row| row["filePath"].as_str().map(str::to_string))
        .collect();
    paths.sort();
    paths
}

/// The graph a previous generation of the indexer would have left: the file is
/// still there and still has its symbols, so every symptom is "this query has
/// no answer" rather than "this file is unknown". Deleting the import edges is
/// the cheapest faithful stand-in for the real case, where the plugin of the
/// day resolved no module specifier at all and so never produced them.
fn strip_import_edges(index: &Path) {
    let conn = Connection::open(index).expect("failed to open the project index");
    let removed = conn.execute("DELETE FROM edges WHERE kind = 'IMPORTS'", []).expect("failed to strip edges");
    assert!(removed > 0, "the fixture must have had import edges to strip");
}

fn set_indexer_version(index: &Path, version: &str) {
    let conn = Connection::open(index).expect("failed to open the project index");
    let updated = conn
        .execute("UPDATE meta SET indexer_version = ?1 WHERE id = 1", rusqlite::params![version])
        .expect("failed to restamp the index");
    assert_eq!(updated, 1, "the index must carry exactly one meta row to restamp");
}

#[tokio::test]
async fn an_index_an_older_indexer_built_is_rebuilt_instead_of_answered_from() {
    let project = Project::new();

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "the cold-start index must answer this before anything is done to it"
    );

    // What the daemon comes back to: a complete, current-schema, fully walked
    // index whose graph is what an older pipeline produced.
    strip_import_edges(&project.index_path());
    set_indexer_version(&project.index_path(), "0");

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "an index from an older indexer must be rebuilt from source, not trusted"
    );
}

/// The control that makes the test above about the version stamp and nothing
/// else. Same doctored graph, same restart - but stamped as this generation's
/// work, so the daemon has no reason to doubt it and (correctly) does not
/// re-walk a project whose files have not changed. If this ever starts
/// passing by returning the importer, the test above has stopped proving
/// anything: something else is rebuilding the index.
#[tokio::test]
async fn an_index_this_indexer_built_is_taken_at_its_word_across_a_restart() {
    let project = Project::new();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);

    strip_import_edges(&project.index_path());

    assert!(
        importers_of(&project, "src/db/connection.ts").await.is_empty(),
        "an index stamped with the current generation is not re-walked while its files are unchanged"
    );
}
