//! GM-630 (ADR 0038), end to end: a Rust enum variant is a node of its own,
//! so `find_definition` and `find_callers`/`find_references` on it answer
//! across files, and `find_callers` on the enum lists none of its variants'
//! constructions.
//!
//! A real MCP client, the real shim, a real daemon and the real Rust plugin,
//! discovered (alone, through the roots override) from a copy of the
//! checked-in `plugins/rust/plugin.toml` with its three `semantic_*`
//! capabilities turned off, so every edge read here is the structural
//! tier's and core's linker's (`rust_glob_trait_overrides.rs`'s harness).
//!
//! Requires the Rust plugin binary, which `cargo build --workspace` builds.

use std::path::Path;

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::process::Command;

use crate::common;
use crate::common::wait_until_indexed;
use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const FILES: [(&str, &str); 5] = [
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("src/lib.rs", "pub mod bare;\npub mod m;\npub mod user;\n"),
    (
        "src/m.rs",
        "#![allow(non_snake_case)]\n\npub enum E {\n    U,\n    T(u8),\n    S { a: u8 },\n}\n\n\
         pub fn T() {}\n\npub fn local() -> E {\n    E::T(0)\n}\n\n\
         pub enum C {\n    #[cfg(feature = \"x\")]\n    X(u8),\n    #[cfg(not(feature = \"x\"))]\n    X,\n}\n",
    ),
    (
        "src/user.rs",
        "use crate::m::E;\n\npub fn make() -> E {\n    E::T(1)\n}\n\npub fn unit() -> E {\n    E::U\n}\n\n\
         pub fn record() -> E {\n    E::S { a: 2 }\n}\n",
    ),
    ("src/bare.rs", "use crate::m::T;\n\npub fn b() {\n    T();\n}\n"),
];

struct Project {
    dir: tempfile::TempDir,
    plugins: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            plugins: tempfile::tempdir().expect("failed to create a temp plugin root"),
        };
        for (rel, contents) in FILES {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        // The real manifest, structural only: its `${G_MESH_BIN_DIR}` still
        // resolves to this profile's `target/` (`daemon_plugin_bin_dir.rs`).
        let manifest = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust/plugin.toml"),
        )
        .expect("failed to read the Rust manifest");
        let mut structural = manifest.clone();
        for capability in ["semantic_pass", "semantic_sweep", "semantic_prepare"] {
            let on = format!("\n{capability} = true\n");
            assert!(structural.contains(&on), "the shipped manifest no longer says `{}`", on.trim());
            structural = structural.replace(&on, &format!("\n{capability} = false\n"));
        }
        let rust = project.plugins.path().join("rust");
        std::fs::create_dir_all(&rust).expect("failed to create the plugin directory");
        std::fs::write(rust.join("plugin.toml"), structural).expect("failed to write the Rust manifest");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let plugins = self.plugins.path().to_path_buf();
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", &plugins)
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        ().serve(transport).await.expect("MCP initialization failed")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())].into_iter().flatten()
        {
            common::kill_pid_file(&path);
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

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, symbol: &str) -> Value {
    let arguments = json!({ "symbol_name": symbol }).as_object().cloned().expect("arguments are an object");
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .expect("tools/call failed");
    body(&result)
}

/// `tool`'s rows for `symbol`, as sorted `filePath:qualifiedName`s.
async fn rows(client: &RunningService<RoleClient, ()>, tool: &str, symbol: &str) -> Vec<String> {
    let page = call(client, tool, symbol).await;
    let mut rows: Vec<String> = page["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results is not an array: {page}"))
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row["filePath"].as_str().expect("a row has a filePath"),
                row["qualifiedName"].as_str().expect("a row has a qualifiedName")
            )
        })
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_variant_is_defined_and_called_across_files_and_the_enum_rolls_nothing_up() {
    let project = Project::new();
    let client = project.connect().await;
    wait_until_indexed(project.root());

    // find_definition lands on the variant's own node, by full path and by
    // its `E::V` tail, of the kind its shape gives.
    for (symbol, kind) in [("m::E::T", "Function"), ("E::U", "Variable"), ("E::S", "Type")] {
        let definition = call(&client, "find_definition", symbol).await;
        assert_ne!(definition["ambiguous"], json!(true), "{symbol}: {definition}");
        assert_eq!(definition["filePath"], "src/m.rs", "{symbol}: {definition}");
        assert_eq!(
            definition["qualifiedName"],
            format!("m::{}", symbol.trim_start_matches("m::")),
            "{definition}"
        );
        assert_eq!(definition["kind"], kind, "{symbol}: {definition}");
    }
    // Two `cfg` alternatives of one variant are one node, so the name is not
    // ambiguous; the first declared (the tuple one) is it.
    let cfg = call(&client, "find_definition", "m::C::X").await;
    assert_ne!(cfg["ambiguous"], json!(true), "{cfg}");
    assert_eq!(cfg["kind"], "Function", "{cfg}");

    // Construction sites, same file and through a `use` of the enum.
    assert_eq!(
        rows(&client, "find_callers", "m::E::T").await,
        vec!["src/m.rs:m::local", "src/user.rs:user::make"]
    );
    assert!(rows(&client, "find_references", "m::E::U")
        .await
        .contains(&"src/user.rs:user::unit".to_string()));
    assert!(rows(&client, "find_references", "m::E::S")
        .await
        .contains(&"src/user.rs:user::record".to_string()));

    // A bare `T` imported from `m` is the free fn, never the variant.
    assert_eq!(rows(&client, "find_callers", "m::T").await, vec!["src/bare.rs:bare::b"]);

    // No roll-up: the enum's callers are not its variants' constructions.
    assert_eq!(rows(&client, "find_callers", "m::E").await, Vec::<String>::new());

    client.cancel().await.expect("failed to shut the client down");
}
