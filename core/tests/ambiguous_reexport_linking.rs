//! Acceptance test for an ambiguous `export *` barrel over the whole real
//! chain: a real MCP client, the real shim, a real daemon, a real cold-start
//! bulk index driven by the real JS/TS plugin, the real semantic pass that
//! follows it - driving a real `tsserver` child - and `find_callers`
//! answering off what they produced together.
//!
//! `find_callers(symbol_name: "mutate")` alone is the wrong tool for this
//! fixture: there really are two declarations named `mutate` (that is what
//! makes the barrel ambiguous at all), so a bare-name query correctly hits
//! g-mesh's own candidate-list branch (`ambiguous: true`) - a separate,
//! genuine feature, not the bug this test is about. This test follows this
//! project's own documented usage pattern instead (`find_definition` first to
//! resolve the ambiguity to one candidate's `id`, then re-query
//! `find_callers` by that `symbol_id`) - the same pattern
//! `namespace_import_resolution.rs`/`default_export_linking.rs` already use.
//!
//! Complements `plugin_bridge.rs`'s
//! `an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass`, which
//! proves the same fixture's edge flips `source`/`resolved` by reading the
//! index directly. This test proves the same fact through the actual MCP
//! tool surface instead, per this project's acceptance criterion that all
//! three re-export/import gaps be checked "via the real MCP tools", not just
//! at the storage layer.
//!
//! Structured after `namespace_import_resolution.rs`/`reexport_linking.rs`.
//! Requires `plugins/typescript/dist/` to be up to date, which `core/build.rs`
//! keeps so.

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

/// Same generous budget as `namespace_import_resolution.rs`'s
/// `SEMANTIC_TIMEOUT`: the semantic pass runs after the structural index is
/// already being served, and pays a `tsserver` child's own startup first.
const SEMANTIC_TIMEOUT: Duration = Duration::from_secs(45);

/// `a.ts` and `b.ts` both declare `mutate`; `index.ts` re-exports both, so a
/// consumer's `mutate` is ambiguous to a name-only walk. TypeScript's own
/// module resolution isn't ambiguous about it - the first `export *` in
/// source order wins - which is exactly the fact tree-sitter's structural
/// layer cannot see and the semantic pass exists to supply.
const FILES: [(&str, &str); 5] = [
    ("tsconfig.json", r#"{ "compilerOptions": { "strict": true, "target": "ES2020" }, "include": ["."] }"#),
    ("a.ts", "export function mutate(): \"a\" {\n  return \"a\";\n}\n"),
    ("b.ts", "export function mutate(): \"b\" {\n  return \"b\";\n}\n"),
    ("index.ts", "export * from \"./a\";\nexport * from \"./b\";\n"),
    (
        "caller.ts",
        r#"import { mutate } from "./index";

export function run(): void {
  mutate();
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

#[tokio::test]
async fn find_callers_resolves_through_an_ambiguous_export_star_barrel() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true).arg("mcp-shim").current_dir(&root).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    // Step 1: resolve the ambiguity the same way the project's own guidance
    // tells an agent to - find_definition by bare name, then pick a
    // candidate's `id`.
    let definition = client
        .call_tool(
            CallToolRequestParams::new("find_definition")
                .with_arguments(json!({ "symbol_name": "mutate" }).as_object().cloned().unwrap()),
        )
        .await
        .expect("tools/call failed");
    let definition = body(&definition);
    assert_eq!(definition["ambiguous"], json!(true), "expected two same-named declarations: {definition}");
    let candidates = definition["results"].as_array().expect("results is not an array");
    assert_eq!(candidates.len(), 2, "both `mutate` declarations must be indexed: {definition}");
    let a_branch_id =
        candidates.iter().find(|c| c["filePath"].as_str() == Some("a.ts")).expect("no candidate in a.ts")
            ["id"]
            .as_str()
            .expect("candidate has no id")
            .to_string();

    // Step 2: find_callers by that symbol_id, polled until the semantic pass
    // has resolved which branch `caller.ts`'s import actually binds to.
    let ask_callers = || {
        let client = &client;
        let id = a_branch_id.clone();
        async move {
            let result = client
                .call_tool(CallToolRequestParams::new("find_callers").with_arguments(
                    json!({ "symbol_id": id }).as_object().cloned().expect("arguments are an object"),
                ))
                .await
                .expect("tools/call failed");
            body(&result)
        }
    };

    let deadline = Instant::now() + SEMANTIC_TIMEOUT;
    let rows = loop {
        let result = ask_callers().await;
        let rows: Vec<(String, String, bool)> = result["results"]
            .as_array()
            .expect("results is not an array")
            .iter()
            .map(|row| {
                (
                    row["qualifiedName"].as_str().expect("no qualifiedName").to_string(),
                    row["filePath"].as_str().expect("no filePath").to_string(),
                    row["resolved"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        if rows.iter().any(|(_, _, resolved)| *resolved) {
            break rows;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not resolve the call through the ambiguous barrel \
             within {SEMANTIC_TIMEOUT:?}: last answer was {result}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // `export * from "./a"` is written first, and that is the branch the
    // compiler hands a consumer - the second is a TS2308 ambiguity against
    // the barrel, not against this import - so `run` in `caller.ts` must show
    // up as a confirmed caller of the `a.ts` declaration specifically.
    assert_eq!(
        rows,
        vec![("run".to_string(), "caller.ts".to_string(), true)],
        "find_callers must report the caller as confirmed, landed on the branch \
         TypeScript itself resolves to, via the real MCP tool surface"
    );
}
