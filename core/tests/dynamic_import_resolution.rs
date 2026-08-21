//! Acceptance test for computed `import()` specifiers, over the whole real
//! chain: a real MCP client, the real shim, a real daemon, a real cold-start
//! bulk index driven by the real JS/TS plugin, and `get_dependencies`
//! answering off what that produced.
//!
//! Scoped in full in `docs/architecture/g-mesh-v1.md` ("Computed import
//! specifiers") and pinned at the extraction level by
//! `plugins/typescript/test/extract.test.ts` - this test closes the gap those unit
//! fixtures leave open: nothing there goes through a real daemon or a real
//! MCP tool call, so nothing proves the boundary survives contact with the
//! actual query surface an agent uses. Purely structural, like
//! `reexport_linking.rs` - the whole feature is tree-sitter constant folding,
//! no compiler round trip, so no semantic-pass wait loop is needed.
//!
//! Two specifiers, same call shape (`import()` with a non-literal first
//! argument), on opposite sides of the line:
//!
//!  - `import(\`./plugins/${NAME}/index.js\`)` folds, because `NAME` is a
//!    same-file `const` bound to a string literal - arithmetic on this file's
//!    own syntax, no different in kind from a plain relative `import "./x"`.
//!  - `import(getSpecifier(id))` does not, and never will: the specifier is a
//!    function's return value, and nothing about a function's return value is
//!    knowable without running it. This is not a gap to close later, it is a
//!    permanent hard limit - documented in the architecture doc and in
//!    `README.md`, not just a code comment.
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

/// A single-package project whose entry point imports through two computed
/// `import()` specifiers - one this pass can fold, one it deliberately
/// cannot.
const FILES: [(&str, &str); 2] = [
    (
        "src/index.ts",
        r#"const NAME = "alpha";

export async function boot(id: string): Promise<void> {
  // Resolvable: every interpolated part is a same-file constant, known
  // without running anything.
  await import(`./plugins/${NAME}/index.js`);
  // Out of scope, by construction: the specifier is the return value of an
  // arbitrary function call. No amount of static analysis changes that.
  await import(getSpecifier(id));
}

function getSpecifier(id: string): string {
  return `./plugins/${id}/index.js`;
}
"#,
    ),
    ("src/plugins/alpha/index.ts", "export const value = \"alpha\";\n"),
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

/// (filePath or `<qualifiedName>` for a placeholder, kind) per result, sorted
/// for a stable comparison.
fn reached(result: &Value) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = result["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            let what = match row["filePath"].as_str() {
                Some(path) => path.to_string(),
                None => format!("<{}>", row["qualifiedName"].as_str().expect("no qualifiedName")),
            };
            (what, row["kind"].as_str().expect("no kind").to_string())
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn get_dependencies_resolves_a_foldable_computed_specifier_and_none_for_an_unfoldable_one() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    // The daemon binds its socket before its cold-start bulk index and says so
    // per call while the walk runs (task 105), so a completed MCP handshake no
    // longer implies a built index - the walk's own completion marker is what
    // does.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(&root).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_dependencies").with_arguments(
                json!({
                    "file_path": "src/index.ts",
                    "direction": "Outgoing",
                })
                .as_object()
                .cloned()
                .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call failed");
    let result = body(&result);

    // Exactly one outgoing dependency: the folded template specifier resolved
    // to the real file it names. Nothing at all for `getSpecifier(id)` - not
    // a second row, not an unresolved placeholder either. That is the
    // distinguishing point from an ordinary unresolved import (a package name,
    // a dangling relative path), which still gets a placeholder `Module` node
    // to carry the edge: an unfoldable computed specifier produces no edge in
    // the first place, because nothing was ever recorded for it to hang on.
    assert_eq!(
        reached(&result),
        vec![("src/plugins/alpha/index.ts".to_string(), "File".to_string())],
        "only the foldable specifier's target should be reachable, and nothing should stand in for the \
         unfoldable one"
    );
    assert_eq!(result["truncated"], false, "one resolved dependency does not hit any bound");

    client.cancel().await.expect("failed to shut the client down");
}
