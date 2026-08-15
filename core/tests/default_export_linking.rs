//! Acceptance test for a default export used under a different local name,
//! over the whole real chain: a real MCP client, the real shim, a real daemon,
//! a real cold-start bulk index driven by the real JS/TS plugin, a real
//! `tsserver` child answering the semantic pass, and `find_references`
//! answering off what all of that produced.
//!
//! The gap it closes: `export default class Foo {}` publishes the name
//! `default` and declares a node called `Foo`, so an importer writing
//! `import Bar from "./shape"` waits on the address `shape.ts#default` - which
//! that file does not declare. Every layer was individually right and the
//! usages went missing anyway, because the two names are only the same symbol
//! to a type checker. The local name (`Bar`) is not the difficulty and never
//! even reaches the index; the *published* name being different from the
//! *declared* one is.
//!
//! Which is the family `semanticPass.ts`'s second question covers - an
//! unresolved edge whose target file does not declare the name - so nothing
//! here is a mechanism of its own. What this file adds is the end of the
//! thread: that the mechanism really does reach a tool answer for this shape,
//! measured through the same chain a user's query takes, rather than being
//! believed to because the pieces look right.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

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

/// The semantic pass runs *after* the daemon starts serving, on purpose - it
/// makes existing answers better and must never delay the first one - so a
/// test of what it produces has to wait for it rather than for the walk.
/// Generous because the first question asked of a project starts a `tsserver`
/// child (~1.2s measured) before it can be answered.
const PASS_TIMEOUT: Duration = Duration::from_secs(60);

const FILES: [(&str, &str); 4] = [
    ("tsconfig.json", r#"{ "compilerOptions": { "strict": true, "target": "ES2020" }, "include": ["src"] }"#),
    (
        "src/shape.ts",
        r#"export default class Foo {
  area(): number {
    return 1;
  }
}
"#,
    ),
    // Two usages of two different kinds, under a local name the exporting file
    // has never heard of.
    (
        "src/caller.ts",
        r#"import Bar from "./shape";

export class Sub extends Bar {}

export function run(): number {
  return new Bar().area();
}
"#,
    ),
    // A second importer, under a second local name: one alias has to answer
    // both, since neither name is what the placeholder is addressed by.
    (
        "src/other.ts",
        r#"import Widget from "./shape";

export function measure(): number {
  return new Widget().area();
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

    /// `(kind, source, resolved)` of every *usage* edge landing on the `Foo`
    /// class, as the index actually holds them - the half of the claim no tool
    /// response spells out. Structural edges from its own file (`DEFINES`,
    /// `EXPORTS`) are left out: they are same-file facts tree-sitter settled
    /// on its own and has no reason to relabel.
    fn usage_edges_onto_foo(&self) -> Vec<(String, String, bool)> {
        let db = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        let conn = Connection::open(db).expect("failed to open the index");
        let mut stmt = conn
            .prepare(
                "SELECT e.kind, e.source, e.resolved FROM edges e \
                 JOIN nodes n ON n.id = e.toId \
                 WHERE n.name = 'Foo' AND n.filePath = 'src/shape.ts' \
                   AND e.kind IN ('CALLS', 'REFERENCES', 'SUPERTYPE_OF') \
                 ORDER BY e.kind, e.source",
            )
            .expect("failed to prepare the edge query");
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("failed to read the edges")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("failed to collect the edges");
        rows
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
async fn usages_of_a_renamed_default_import_reach_the_class_it_really_names() {
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

    let references = || {
        let client = &client;
        async move {
            let result = client
                .call_tool(CallToolRequestParams::new("find_references").with_arguments(
                    json!({ "symbol_name": "Foo" }).as_object().cloned().expect("arguments are an object"),
                ))
                .await
                .expect("tools/call failed");
            found(&body(&result))
        }
    };

    let expected = vec![
        ("src/caller.ts".to_string(), "Sub".to_string()),
        ("src/caller.ts".to_string(), "run".to_string()),
        ("src/other.ts".to_string(), "measure".to_string()),
    ];

    // Poll rather than assert once: the walk being complete is what
    // `wait_until_indexed` guarantees, and the semantic pass deliberately runs
    // after it. What is being waited for is an *upgrade*, so the intermediate
    // state is a smaller answer, never a wrong one.
    let deadline = Instant::now() + PASS_TIMEOUT;
    let mut last = Vec::new();
    while Instant::now() < deadline {
        last = references().await;
        if last == expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        last, expected,
        "every usage of the default export must reach the class, whatever each importer called it"
    );

    // And the edges say where the answer came from: the compiler, not a name
    // that matched. Two references (`new Bar()`, `new Widget()`) and one
    // supertype clause, all confirmed.
    assert_eq!(
        project.usage_edges_onto_foo(),
        vec![
            ("REFERENCES".to_string(), "ts-compiler".to_string(), true),
            ("REFERENCES".to_string(), "ts-compiler".to_string(), true),
            ("SUPERTYPE_OF".to_string(), "ts-compiler".to_string(), true),
        ],
        "an edge no name matching could have made must record the checker as its source"
    );

    // And the graph is coherent from the other direction too: one definition
    // for `Foo`, in the file that declares it. An upgrade that had invented a
    // second node for the same class - a placeholder promoted, say - would show
    // up here as an ambiguity rather than as a missing edge.
    let definition = client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": "Foo" }).as_object().cloned().expect("arguments are an object"),
        ))
        .await
        .expect("tools/call failed");
    let definition = body(&definition);
    assert_ne!(definition["ambiguous"], json!(true), "only one node declares it: {definition}");
    assert_eq!(definition["filePath"], "src/shape.ts");

    // `default` is a name nothing declares, and no placeholder standing for it
    // may be offered as one - the pending-symbol rows the importers wrote are
    // named exactly that.
    let by_published_name = client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": "default" }).as_object().cloned().expect("arguments are an object"),
        ))
        .await
        .expect("tools/call failed");
    assert_eq!(
        by_published_name.is_error,
        Some(true),
        "a placeholder must never be a definition: {by_published_name:?}"
    );
    match &by_published_name.content[0] {
        ContentBlock::Text(text) => {
            assert!(text.text.contains("no symbol named 'default'"), "{}", text.text)
        }
        other => panic!("expected text content, got {other:?}"),
    }

    client.cancel().await.expect("failed to shut the client down");
}
