//! Acceptance test for import resolution, over the whole real chain: a real
//! MCP client, the real shim, a real daemon, a real cold-start bulk index
//! driven by the real JS/TS plugin, and `get_dependencies` answering off what
//! that produced.
//!
//! Nothing here is stubbed on purpose. The bug this covers was invisible to
//! every layer's own tests - the plugin emitted the placeholder it promised,
//! core stored the edge it was given, and the traversal walked the graph it
//! was handed - and only showed up as `get_dependencies` returning nothing
//! useful when the pieces were put together.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

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

/// A small ESM-TypeScript project, written the way real ones are: specifiers
/// carry the *emitted* `.js` extension, one import addresses a directory by
/// its `index.ts`, one names a package, and one points at a file that is not
/// there.
const FILES: [(&str, &str); 4] = [
    (
        "src/index.ts",
        r#"import { connect } from "./db/connection.js";
import { helpers } from "./util";
import { z } from "zod";

export function start(): number {
  return connect() + helpers.length;
}
"#,
    ),
    (
        "src/db/connection.ts",
        r#"import { POOL_SIZE } from "./pool.js";
import { missing } from "./deleted.js";

export function connect(): number {
  return POOL_SIZE;
}
"#,
    ),
    ("src/db/pool.ts", "export const POOL_SIZE = 8;\n"),
    ("src/util/index.ts", "export const helpers = [1, 2, 3];\n"),
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

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.pid_file()) {
            let _ = StdCommand::new("kill").arg("-9").arg(pid.trim()).status();
        }
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

/// (filePath, depth) per reached dependency, sorted for a stable comparison.
/// A dependency that is not a file of this project has no `filePath` at all,
/// and is listed here under its specifier in angle brackets - so that an
/// unresolved placeholder can never quietly read as one of the real files.
fn reached(result: &Value) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = result["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            let what = match row["filePath"].as_str() {
                Some(path) => path.to_string(),
                None => format!("<{}>", row["qualifiedName"].as_str().expect("no qualifiedName")),
            };
            (what, row["depth"].as_u64().expect("no depth"))
        })
        .collect();
    rows.sort();
    rows
}

fn kinds(result: &Value) -> Vec<String> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn get_dependencies_walks_real_files_in_both_directions_after_a_cold_start() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    // The daemon binds its socket before its cold-start bulk index and says
    // so per call while the walk runs (task 105), so a completed MCP
    // handshake no longer implies a built index - the walk's own completion
    // marker is what does.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let dependencies = |args: Value| {
        let client = &client;
        async move {
            let result = client
                .call_tool(
                    CallToolRequestParams::new("get_dependencies")
                        .with_arguments(args.as_object().cloned().expect("arguments literal is an object")),
                )
                .await
                .expect("tools/call failed");
            body(&result)
        }
    };

    // --- Incoming: who imports this file ---------------------------------
    //
    // The regression this test exists for: this used to be `[]`, because the
    // importer's edge pointed at a per-importer placeholder node rather than
    // at this file, so nothing pointed *at* it at all.
    let importers = dependencies(json!({
        "file_path": "src/db/connection.ts",
        "direction": "Incoming",
    }))
    .await;
    assert_eq!(
        reached(&importers),
        vec![("src/index.ts".to_string(), 1)],
        "the file's real importer must come back, addressed by its own path"
    );
    assert_eq!(kinds(&importers), vec!["File"], "an importer is a file, not a specifier-shaped placeholder");

    // Transitively, from the leaf of the chain: pool <- connection <- index.
    let upstream = dependencies(json!({
        "file_path": "src/db/pool.ts",
        "direction": "Incoming",
    }))
    .await;
    assert_eq!(
        reached(&upstream),
        vec![("src/db/connection.ts".to_string(), 1), ("src/index.ts".to_string(), 2)],
    );

    // --- Outgoing: what this file depends on ------------------------------
    let downstream = dependencies(json!({
        "file_path": "src/index.ts",
        "direction": "Outgoing",
        "max_depth": 2,
    }))
    .await;
    assert_eq!(
        reached(&downstream),
        vec![
            // What resolved to no file of ours, reported as itself rather
            // than as the path of the file that imports it.
            ("<./deleted.js>".to_string(), 2),
            ("<zod>".to_string(), 1),
            // Hop one: a `.js` specifier resolved to its `.ts` source, and a
            // directory resolved to its `index.ts`.
            ("src/db/connection.ts".to_string(), 1),
            // Hop two, only reachable *through* a resolved import - the whole
            // point of resolving them.
            ("src/db/pool.ts".to_string(), 2),
            ("src/util/index.ts".to_string(), 1),
        ],
        "a two-hop walk must pass through the first hop's own imports"
    );
    assert_eq!(downstream["truncated"], false, "depth 2 is enough for this graph");

    // --- what deliberately stays unresolved -------------------------------
    //
    // A package specifier and a dangling relative one still come back as
    // placeholders: they have no file in this project to point at, and
    // pretending otherwise is what the `resolved` flag exists to prevent.
    let unresolved = dependencies(json!({
        "file_path": "src/index.ts",
        "direction": "Outgoing",
        "max_depth": 1,
    }))
    .await;
    let modules: Vec<&str> = unresolved["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "Module")
        .map(|row| row["qualifiedName"].as_str().unwrap())
        .collect();
    assert_eq!(modules, vec!["zod"], "a package has no local file, so it stays a module placeholder");

    let dangling = dependencies(json!({
        "file_path": "src/db/connection.ts",
        "direction": "Outgoing",
        "max_depth": 1,
    }))
    .await;
    let names: Vec<&str> = dangling["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "Module")
        .map(|row| row["qualifiedName"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"./deleted.js"),
        "an import of a file that is not there must survive as a placeholder, not vanish: {names:?}"
    );

    client.cancel().await.expect("failed to shut the client down");
}
