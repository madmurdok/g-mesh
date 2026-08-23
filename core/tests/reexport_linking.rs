//! Acceptance test for symbols reached through a barrel, over the whole real
//! chain: a real MCP client, the real shim, a real daemon, a real cold-start
//! bulk index driven by the real JS/TS plugin, and `find_callers` answering
//! off what that produced.
//!
//! The fixture is the shape every monorepo has and the one the bug was found
//! in (excalidraw): a package whose entry point declares nothing and only
//! re-exports, imported by its bare workspace name. Each layer was individually
//! right - the plugin resolved `@fixture/element` to the package's `src/index.ts`
//! and hung the usage on a placeholder addressed there, core looked for the
//! name in that file and honestly found nothing - and the caller went missing
//! anyway, because nobody followed the re-export to the file one hop further.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};

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

/// A two-package workspace. `@fixture/element` declares its entry as build
/// output that an unbuilt checkout does not have, so it resolves by the
/// `src/index.*` convention - to a file that is nothing but re-exports, one
/// of them going through a second barrel of its own.
const FILES: [(&str, &str); 7] = [
    ("package.json", r#"{ "name": "fixture-root", "private": true, "workspaces": ["packages/*"] }"#),
    ("packages/element/package.json", r#"{ "name": "@fixture/element", "main": "./dist/prod/index.js" }"#),
    (
        "packages/element/src/index.ts",
        r#"export * from "./mutateElement";
export * from "./shapes";
"#,
    ),
    (
        "packages/element/src/mutateElement.ts",
        r#"export const mutateElement = (element: string): string => {
  return element;
};
"#,
    ),
    (
        "packages/element/src/shapes/index.ts",
        r#"export { newElement as create } from "./factory";
"#,
    ),
    (
        "packages/element/src/shapes/factory.ts",
        r#"export function newElement(): string {
  return "element";
}
"#,
    ),
    (
        "packages/app/src/actions.ts",
        // Written the way the file the bug was found in is: the bare package
        // name, and the call itself inside a callback handed to something.
        r#"import { mutateElement, create } from "@fixture/element";

export const actionFrame = register({
  name: "frame",
  perform: (element) => {
    return mutateElement(create());
  },
});
"#,
    ),
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
        common::kill_pid_file(&self.pid_file());
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

/// (filePath, qualifiedName) per result, sorted for a stable comparison.
fn found(result: &Value) -> Vec<(String, String)> {
    assert_ne!(result["ambiguous"], json!(true), "the name resolved to more than one symbol: {result}");
    let mut rows: Vec<(String, String)> = result["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            (
                row["filePath"].as_str().expect("no filePath").to_string(),
                row["qualifiedName"].as_str().expect("no qualifiedName").to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn find_callers_reaches_a_caller_that_imported_through_a_barrel() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    // The daemon binds its socket before its cold-start bulk index and says
    // so per call while the walk runs (task 105), so a completed MCP
    // handshake no longer implies a built index - the walk's own completion
    // marker is what does.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true).arg("mcp-shim").current_dir(&root).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let callers = |name: &'static str| {
        let client = &client;
        async move {
            let result = client
                .call_tool(CallToolRequestParams::new("find_callers").with_arguments(
                    json!({ "symbol_name": name }).as_object().cloned().expect("arguments are an object"),
                ))
                .await
                .expect("tools/call failed");
            body(&result)
        }
    };

    // One hop, through `export * from "./mutateElement"`. This is the
    // regression: the caller used to be missing entirely, because its edge
    // stopped at a placeholder addressed at the barrel.
    assert_eq!(
        found(&callers("mutateElement").await),
        vec![("packages/app/src/actions.ts".to_string(), "actionFrame".to_string())],
        "the caller reached the function through the package's barrel"
    );

    // Two hops and a rename on the way: `export * from "./shapes"`, then
    // `export { newElement as create } from "./factory"` - so the importer's
    // `create` and the declaration's `newElement` are never the same name in
    // any one file.
    assert_eq!(
        found(&callers("newElement").await),
        vec![("packages/app/src/actions.ts".to_string(), "actionFrame".to_string())],
    );

    // The placeholders themselves must stay out of the answer to "which
    // symbol is this?": the barrel republishes `mutateElement` under that
    // exact name, and offering that row as a definition would make every
    // barrelled symbol in a monorepo ambiguous.
    let definition = client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": "mutateElement" }).as_object().cloned().expect("arguments are an object"),
        ))
        .await
        .expect("tools/call failed");
    let definition = body(&definition);
    assert_ne!(definition["ambiguous"], json!(true), "only one node declares it: {definition}");
    assert_eq!(definition["filePath"], "packages/element/src/mutateElement.ts");

    client.cancel().await.expect("failed to shut the client down");
}
