//! Acceptance test for generic-type references, over the whole real chain: a
//! real MCP client, the real shim, a real daemon, a real cold-start bulk index
//! driven by the real JS/TS plugin, and `find_references` answering off what
//! that produced.
//!
//! Unlike `namespace_import_resolution.rs`/`ambiguous_reexport_linking.rs`,
//! there is no semantic pass to wait on here: the generics-scope task
//! (b25d0f4b) found this was a syntax-visibility gap in the tree-sitter
//! extraction pass itself, not a type-inference problem, and the fix
//! (`plugins/typescript/src/extract.ts`, `// --- generic types ---`) is purely
//! structural. So the new edges are already in the index the moment
//! `wait_until_indexed` returns, exactly like `reexport_linking.rs`.
//!
//! `plugins/typescript/test/extract.test.ts`'s own `// --- generic types ---`
//! section proves these facts at the extractor's unit level; this file proves
//! the same facts are visible through the real MCP tool surface, which is the
//! gap this test closes.
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

/// One file exercising all three generics fixes at once:
///  - `held`/`plain` give `Box` a bare use (already worked) and a generic use
///    (`Box<Widget>`, used to be dropped) side by side.
///  - `WidgetBox extends Box<Widget>` makes `Box` a supertype-only edge and
///    `Widget`, the explicit type argument in that heritage clause, a
///    `REFERENCES` edge - it used to be discarded entirely.
///  - `Holder<T>`'s own type parameter `T` shadows the file-level `interface
///    T`, so `real: T` is the only thing that may reference it; anything
///    inside `Holder`'s body must not.
const FILES: [(&str, &str); 1] = [(
    "src/p.ts",
    r#"export interface Widget {}
export interface T { tag: string }

export class Box<T> {
  get(): T {
    return null!;
  }
}

export class WidgetBox extends Box<Widget> {}

export const plain: Box = null!;
export const held: Box<Widget> = null!;

export class Holder<T> {
  item: T;
  wrap<T>(value: T): T {
    return value;
  }
}

export const real: T = { tag: "" };
"#,
)];

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

/// (qualifiedName, referenceKind) per result, sorted for a stable comparison.
/// `referenceKind` is kept (unlike `reexport_linking.rs`'s `found`) because
/// the generics fixes specifically depend on a heritage clause's head landing
/// as `SUPERTYPE_OF` while its type argument lands as `REFERENCES` - collapsing
/// the two would hide exactly the distinction under test.
fn found(result: &Value) -> Vec<(String, String)> {
    assert_ne!(result["ambiguous"], json!(true), "the name resolved to more than one symbol: {result}");
    let mut rows: Vec<(String, String)> = result["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            (
                row["qualifiedName"].as_str().expect("no qualifiedName").to_string(),
                row["referenceKind"].as_str().expect("no referenceKind").to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn find_references_sees_generic_heads_and_heritage_type_arguments() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    // The daemon binds its socket before its cold-start bulk index and says
    // so per call while the walk runs (task 105), so a completed MCP
    // handshake no longer implies a built index - the walk's own completion
    // marker is what does.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(&root);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    let references = |name: &'static str| {
        let client = &client;
        async move {
            let result = client
                .call_tool(CallToolRequestParams::new("find_references").with_arguments(
                    json!({ "symbol_name": name }).as_object().cloned().expect("arguments are an object"),
                ))
                .await
                .expect("tools/call failed");
            body(&result)
        }
    };

    // `Box` used to be discarded in `held: Box<Widget>` because a generic
    // type's own name lives in the field every other declaration binds
    // through; `plain: Box` (no type argument) already worked and is the
    // control. `WidgetBox`'s heritage clause names `Box` too, but only as a
    // supertype - never doubled up as a `REFERENCES` edge on the same pair.
    assert_eq!(
        found(&references("Box").await),
        vec![
            ("WidgetBox".to_string(), "SUPERTYPE_OF".to_string()),
            ("held".to_string(), "REFERENCES".to_string()),
            ("plain".to_string(), "REFERENCES".to_string()),
        ],
        "a generic type's head must be referenced both bare and instantiated, and only once each"
    );

    // `Widget` never appears as a type by itself in this fixture - only as an
    // explicit type argument, once in a field's type annotation and once in a
    // heritage clause. Both used to be dropped entirely.
    assert_eq!(
        found(&references("Widget").await),
        vec![("WidgetBox".to_string(), "REFERENCES".to_string()), ("held".to_string(), "REFERENCES".to_string())],
        "an explicit type argument must be a reference wherever it's written, including in a heritage clause"
    );

    // The file-level `interface T` must be reached only by `real: T`. `Box<T>`
    // and `Holder<T>` (including its method-level `wrap<T>`) each declare
    // their own `T` as a type parameter that shadows the file-level one -
    // proving the shadow is scoped to each declaration, not a file-wide
    // suppression of the name.
    assert_eq!(
        found(&references("T").await),
        vec![("real".to_string(), "REFERENCES".to_string())],
        "a declaration's own type parameter must shadow a file-level type of the same name"
    );

    client.cancel().await.expect("failed to shut the client down");
}
