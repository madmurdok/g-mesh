//! `get_dependencies` Incoming on a Rust module file lists the files that
//! reach it by a block-scoped `use`, an inline module path and a path from
//! its parent module, as well as by a module-level `use`.
//!
//! A real MCP client, the real shim, a real daemon and the real Rust plugin
//! (structural tier only, as in `rust_glob_trait_overrides`). The fixture
//! mirrors `cli/clean.rs`:
//!
//!   - `closure.rs`: `use crate::cli::clean::run;` inside a closure;
//!   - `inline.rs`: `crate::cli::clean::run()` with no `use` at all;
//!   - `cli.rs`: the parent module, calling `clean::run()`;
//!   - `stop.rs`: a module-level `use` (the control that always worked);
//!   - `unrelated.rs`: names nothing of `clean`, and must stay out.
//!
//! Control: in `plugins/rust/src/extractor/bodies.rs`, make
//! `Bodies::visit` skip `block_use_imports` and `Bodies::path_import` return
//! at once - only `stop.rs` is left, and this fails.
//!
//! Requires the Rust plugin binary, which `cargo build --workspace` builds.

use std::path::Path;

use g_mesh::paths::state_dir::StateDir;
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

const FILES: [(&str, &str); 8] = [
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("src/lib.rs", "pub mod cli;\npub mod closure;\npub mod inline;\npub mod stop;\npub mod unrelated;\n"),
    ("src/cli.rs", "pub mod clean;\n\npub fn dispatch() {\n    clean::run();\n}\n"),
    ("src/cli/clean.rs", "pub fn run() {}\n"),
    (
        "src/closure.rs",
        "pub fn go() {\n    let f = || {\n        use crate::cli::clean::run;\n        run();\n    };\n    f();\n}\n",
    ),
    ("src/inline.rs", "pub fn go() {\n    crate::cli::clean::run();\n}\n"),
    ("src/stop.rs", "use crate::cli::clean::run;\n\npub fn go() {\n    run();\n}\n"),
    ("src/unrelated.rs", "pub fn go() {}\n"),
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
        for path in [
            StateDir::of(self.root()).map(|sd| sd.pid_file()),
            StateDir::of(self.root()).map(|sd| sd.plugin_pid_file(g_mesh::daemon::plugin::BUNDLED_LANGUAGE)),
        ]
        .into_iter()
        .flatten()
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

/// The `get_dependencies` Incoming walk's result for `file_path`, as `(filePath, depth)`.
async fn importers(client: &RunningService<RoleClient, ()>, file_path: &str) -> (Vec<(String, u64)>, Value) {
    let arguments = json!({ "file_path": file_path, "direction": "Incoming", "max_depth": 1 })
        .as_object()
        .cloned()
        .expect("arguments are an object");
    let result = client
        .call_tool(CallToolRequestParams::new("get_dependencies").with_arguments(arguments))
        .await
        .expect("tools/call failed");
    let page = body(&result);
    let mut rows: Vec<(String, u64)> = page["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results is not an array: {page}"))
        .iter()
        .map(|row| {
            (
                row["filePath"]
                    .as_str()
                    .unwrap_or_else(|| panic!("a row has no filePath: {row}"))
                    .to_string(),
                row["depth"].as_u64().expect("a row has a depth"),
            )
        })
        .collect();
    rows.sort();
    (rows, page)
}

#[tokio::test]
async fn incoming_lists_block_use_inline_path_and_parent_module_importers() {
    let project = Project::new();
    let client = project.connect().await;
    wait_until_indexed(project.root());

    let (rows, page) = importers(&client, "src/cli/clean.rs").await;
    let paths: Vec<&str> = rows.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["src/cli.rs", "src/closure.rs", "src/inline.rs", "src/stop.rs"],
        "closure `use`, inline path, parent module and module-level `use` must all be importers: {page}"
    );
    assert!(rows.iter().all(|(_, depth)| *depth == 1), "{page}");
}
