//! A Rust trait that reaches an impl's module only through a glob
//! `use` is linked by the structural tier alone - the impl's type to the
//! trait, and each impl method to the trait method it implements - so
//! `find_callers` on the impl method carries `overrides` from the first
//! index, with no semantic pass at all.
//!
//! A real MCP client, the real shim, a real daemon and the real Rust plugin,
//! discovered (alone, through the roots override) from a copy of the
//! checked-in `plugins/rust/plugin.toml` with its three `semantic_*`
//! capabilities turned off: rust-analyzer never starts, so every edge read
//! here is the structural tier's and core's linker's.
//!
//! The fixture, one module per case of the design note's behaviour list
//! (`docs/architecture/gm-537-rust-unresolved-trait-overrides.md`, section 5):
//!
//!   - `megaphone`: `use crate::prelude::*`, a glob of a module that only
//!     re-exports `shapes::Loud` (the conformance fixture's `Megaphone`);
//!   - `direct`: `use crate::one::*`, a glob of the declaring module;
//!   - `both`: globs of `one` and `two`, which both declare a `Tr` - Rust
//!     rejects the name as ambiguous, and core must link neither;
//!   - `named`: `use crate::one::Tr;` beside `use crate::two::*;` - the named
//!     import shadows the glob and the impl is `one::Tr`'s.
//!
//! Control: in `plugins/rust/src/extractor/bodies.rs`, make
//! `Bodies::resolve_supertype` call `resolve_path` for every clause (drop the
//! `reaches_only_through_glob` arm) - `megaphone` and `direct` lose both
//! edges and `overrides`, and this fails.
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

mod common;

use common::wait_until_indexed;
use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

const FILES: [(&str, &str); 10] = [
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    (
        "src/lib.rs",
        "pub mod both;\npub mod direct;\npub mod megaphone;\npub mod named;\npub mod one;\n\
         pub mod prelude;\npub mod shapes;\npub mod two;\n",
    ),
    ("src/shapes.rs", "pub trait Loud {\n    fn speak(&self) -> u8;\n}\n"),
    ("src/prelude.rs", "pub use crate::shapes::Loud;\n"),
    (
        "src/megaphone.rs",
        "use crate::prelude::*;\n\npub struct Megaphone;\n\n\
         impl Loud for Megaphone {\n    fn speak(&self) -> u8 {\n        1\n    }\n}\n",
    ),
    ("src/one.rs", "pub trait Tr {\n    fn m(&self);\n}\n"),
    ("src/two.rs", "pub trait Tr {\n    fn m(&self);\n}\n"),
    (
        "src/direct.rs",
        "use crate::one::*;\n\npub struct Direct;\n\nimpl Tr for Direct {\n    fn m(&self) {}\n}\n",
    ),
    (
        "src/both.rs",
        "use crate::one::*;\nuse crate::two::*;\n\npub struct Both;\n\nimpl Tr for Both {\n    fn m(&self) {}\n}\n",
    ),
    (
        "src/named.rs",
        "use crate::one::Tr;\nuse crate::two::*;\n\npub struct Named;\n\nimpl Tr for Named {\n    fn m(&self) {}\n}\n",
    ),
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

/// `find_callers`' `overrides` on the anchor `symbol`, by `qualifiedName`;
/// empty when the field is absent (which is how "none" is spelled).
async fn overrides(client: &RunningService<RoleClient, ()>, symbol: &str) -> Vec<String> {
    let page = call(client, "find_callers", symbol).await;
    assert_eq!(page["anchor"]["qualifiedName"], json!(symbol), "the anchor must be {symbol}: {page}");
    page.get("overrides").map_or_else(Vec::new, |found| {
        found
            .as_array()
            .unwrap_or_else(|| panic!("overrides is not an array: {page}"))
            .iter()
            .map(|member| {
                member["qualifiedName"].as_str().expect("an override has a qualifiedName").to_string()
            })
            .collect()
    })
}

/// `find_implementations`' rows for `symbol`, as sorted `qualifiedName`s.
async fn implementors(client: &RunningService<RoleClient, ()>, symbol: &str) -> Vec<String> {
    let page = call(client, "find_implementations", symbol).await;
    let mut rows: Vec<String> = page["results"]
        .as_array()
        .unwrap_or_else(|| panic!("results is not an array: {page}"))
        .iter()
        .map(|row| row["qualifiedName"].as_str().expect("a row has a qualifiedName").to_string())
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_trait_reached_through_a_glob_gives_overrides_without_a_semantic_pass() {
    let project = Project::new();
    let client = project.connect().await;
    wait_until_indexed(project.root());

    // Behaviour 2 and 3: a glob of a module that only re-exports the trait.
    assert_eq!(
        overrides(&client, "megaphone::<Megaphone as Loud>::speak").await,
        vec!["shapes::Loud::speak"],
        "the impl method must name the trait method it implements, structurally"
    );
    assert_eq!(implementors(&client, "shapes::Loud").await, vec!["megaphone::Megaphone"]);

    // Behaviour 1: a glob of the declaring module.
    assert_eq!(overrides(&client, "direct::<Direct as Tr>::m").await, vec!["one::Tr::m"]);

    // Behaviour 5: the named import shadows `two`'s glob.
    assert_eq!(overrides(&client, "named::<Named as Tr>::m").await, vec!["one::Tr::m"]);

    // Behaviour 4: two globs offering `Tr` link neither - no edge rather
    // than a wrong one.
    assert_eq!(overrides(&client, "both::<Both as Tr>::m").await, Vec::<String>::new());
    assert_eq!(implementors(&client, "one::Tr").await, vec!["direct::Direct", "named::Named"]);
    assert_eq!(implementors(&client, "two::Tr").await, Vec::<String>::new());
}
