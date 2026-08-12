//! The same guarantee `namespace_import_resolution.rs` proves for a
//! zero-config cold start, over the *other* way a project gets its index:
//! `g-mesh init`.
//!
//! # What this is really about
//!
//! `find_callers` on a function every caller reaches through
//! `import * as ns from "./mod"` is answerable only by the semantic pass - the
//! structural walk never sees the bare name at the use site, so there is not
//! even an unresolved edge for it to leave behind. That pass used to hang off
//! `daemon::run`'s cold-start branch, and `init`'s whole job is to make sure
//! no daemon ever takes that branch: it walks the project itself and records
//! `meta.bulkIndexedAt`, which is precisely the fact every later daemon start
//! reads to conclude it owes no cold start.
//!
//! So an `init`ed project used to keep, permanently, an index the pass had
//! never touched - the namespace call edges simply absent, on a graph that
//! reported itself fully indexed. Measured on the real thing before the fix:
//! `plerp` in excalidraw's `packages/laser-pointer/src/math.ts`, called as
//! `m.plerp(...)` from `state.ts`, had no inbound `CALLS` edge at all, and the
//! whole 27k-edge index carried 10 `ts-compiler` edges (all of them from
//! per-file passes an agent's own queries happened to trigger) where a
//! whole-project pass produces ~4000.
//!
//! What this asserts is therefore not "namespace imports resolve" - the
//! sibling test owns that - but "an index is the same index however it was
//! built". The named-import caller in the same fixture is the control: it is
//! resolved by the structural walk alone, so it was never the thing at risk,
//! and a run where only it comes back is exactly the regression.

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};

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

/// One exported function, reached two ways from one file: through a namespace
/// import (only the checker can attribute this) and by name (the structural
/// walk resolves this on its own). Both live in the same file so that a
/// difference between them cannot be a difference between two files' fates.
const FILES: [(&str, &str); 3] = [
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "target": "ES2020" }, "include": ["src"] }"#,
    ),
    (
        "src/mod.ts",
        r#"export function someExport(): number {
  return 1;
}
"#,
    ),
    (
        "src/app.ts",
        r#"import * as ns from "./mod";
import { someExport } from "./mod";

export function callsThroughNamespace(): number {
  return ns.someExport();
}

export function callsByName(): number {
  return someExport();
}
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
        for path in [self.pid_file(), daemon::plugin_pid_path(self.root()).unwrap()] {
            if let Ok(pid) = std::fs::read_to_string(&path) {
                let _ =
                    StdCommand::new("kill").arg("-9").arg(pid.trim()).stderr(Stdio::null()).status();
            }
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

/// (qualifiedName, resolved) per caller row, sorted for a stable comparison -
/// `resolved` because an edge that is merely present could still be a guess.
fn callers(result: &Value) -> Vec<(String, bool)> {
    assert_ne!(result["ambiguous"], json!(true), "the name resolved to more than one symbol: {result}");
    let mut rows: Vec<(String, bool)> = result["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            (
                row["qualifiedName"].as_str().expect("no qualifiedName").to_string(),
                row["resolved"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_project_prepared_with_init_answers_find_callers_through_a_namespace_import() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    let init = StdCommand::new(BIN)
        .arg("init")
        .current_dir(&root)
        .output()
        .expect("failed to run `g-mesh init`");
    assert!(
        init.status.success(),
        "`g-mesh init` failed with {}: {}",
        init.status,
        String::from_utf8_lossy(&init.stderr)
    );
    let stdout = String::from_utf8(init.stdout).expect("init output is not valid UTF-8");
    assert!(
        stdout.contains("semantic:   pass complete"),
        "init must report the pass it now runs:\n{stdout}"
    );

    // Nothing polls here, and that is the assertion. `init` returned, so by
    // its own contract the index is ready - and the daemon this shim
    // bootstraps will find `meta.bulkIndexedAt` already written and take no
    // cold-start branch at all, so there is no second chance for these edges
    // to appear later. They are in the index `init` built, or they are absent
    // for good.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(&root);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let result = client
        .call_tool(CallToolRequestParams::new("find_callers").with_arguments(
            json!({ "symbol_name": "someExport" }).as_object().cloned().expect("arguments are an object"),
        ))
        .await
        .expect("tools/call failed");

    assert_eq!(
        callers(&body(&result)),
        vec![("callsByName".to_string(), true), ("callsThroughNamespace".to_string(), true)],
        "the namespace-import caller is the one only `init`'s semantic pass can produce; \
         a result carrying only `callsByName` is the regression this test exists for"
    );

    client.cancel().await.expect("failed to shut the client down");
}
