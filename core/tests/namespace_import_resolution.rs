//! Acceptance test for `import * as ns` over the whole real chain: a real MCP
//! client, the real shim, a real daemon, a real cold-start bulk index driven by
//! the real JS/TS plugin, the real semantic pass that follows it - driving a
//! real `tsserver` child - and `find_callers` answering off what they produced
//! together.
//!
//! What makes this case different from every other cross-file gap: there is no
//! unresolved edge to upgrade. The structural pass emits *nothing* for
//! `ns.someExport()` - it never sees the bare name `someExport` at the use
//! site, only the receiver `ns` and a property access - so `find_callers` used
//! to answer "nothing" for a symbol every one of whose callers reached it
//! through a namespace import. The edge this test looks for is one only the
//! semantic layer can produce.
//!
//! Structured after `reexport_linking.rs`, which proves the same chain for the
//! placeholders tree-sitter *can* emit; requires `plugins/typescript/dist/` to be up
//! to date, which `core/build.rs` keeps so.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

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

/// The semantic pass runs *after* the walk's completion marker, on purpose -
/// the daemon answers off the structural index the moment it has one and
/// improves on it afterwards. So this test waits for its own answer rather
/// than for the index, and generously: the pass pays a `tsserver` child's
/// startup (~1.2s measured) before it can say anything at all.
const SEMANTIC_TIMEOUT: Duration = Duration::from_secs(45);

/// Two shapes in one fixture, both written the way real code writes them.
///
/// `direct.ts` is the plain case: a namespace import of a module that declares
/// the function itself. `viaBarrel.ts` is the case that decides *where* the
/// address has to come from - the declaration is two files away and renamed on
/// the way, so `barrel.someExport` and `realName` are never the same name in
/// any one file, and nothing written at the use site names the declaration.
const FILES: [(&str, &str); 5] = [
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
        "src/impl.ts",
        r#"export function realName(): number {
  return 2;
}
"#,
    ),
    ("src/barrel.ts", "export { realName as aliased } from \"./impl\";\n"),
    (
        "src/app.ts",
        r#"import * as ns from "./mod";
import * as barrel from "./barrel";

export function callsDirectly(): number {
  return ns.someExport();
}

export function callsThroughBarrel(): number {
  return barrel.aliased();
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

/// (qualifiedName, resolved) per caller row, sorted for a stable comparison.
/// `resolved` is what the acceptance criterion is actually about: an edge that
/// is merely *present* could still be a guess.
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
async fn find_callers_reaches_a_caller_that_went_through_a_namespace_import() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let ask = |name: &'static str| {
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

    // Polled rather than asserted once: the index is complete and being served
    // before the semantic pass that improves it has even started its checker.
    let deadline = Instant::now() + SEMANTIC_TIMEOUT;
    let direct = loop {
        let rows = callers(&ask("someExport").await);
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not resolve `ns.someExport()` within {SEMANTIC_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        direct,
        vec![("callsDirectly".to_string(), true)],
        "the caller reached the function through `import * as ns`, and the edge is confirmed"
    );

    // The barrel case, which is why the address is the checker's rather than
    // "the module the namespace names, plus the member name": that would have
    // addressed `src/barrel.ts#aliased`, and only core's own re-export walk
    // would have saved it. The checker names `src/impl.ts#realName` outright.
    let through_barrel = loop {
        let rows = callers(&ask("realName").await);
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not resolve `barrel.aliased()` within {SEMANTIC_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(through_barrel, vec![("callsThroughBarrel".to_string(), true)]);

    client.cancel().await.expect("failed to shut the client down");
}
